#!/usr/bin/env python3
"""Hot host deploy agent for single-machine installations.

Runs on the VM under systemd. It reads the installation's release pointer,
deploys the signed release it names, delivers secrets as tmpfs files, and
reports status. Operators and workflows reach it only through a fixed set of
commands (see COMMANDS); it never runs arbitrary shell.

    hot-agent run                         # reconcile to the release pointer, forever
    hot-agent command OP [--service S] [--request-id ID]

The standard library only: cloud APIs are called over HTTPS with the VM's
identity, so the host image needs no cloud CLIs.
"""
import argparse
import base64
import contextlib
import datetime
import fcntl
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import re
import secrets as token
import shutil
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

RUNTIME = Path(os.environ.get("HOT_AGENT_RUNTIME", "/opt/hot-host/runtime"))
sys.path.insert(0, str(RUNTIME))
import render  # noqa: E402  (installed beside the agent)

INSTALLATION = Path("/etc/hot/installation.json")
HOT_ROOT = Path("/var/lib/hot")
RELEASES = HOT_ROOT / "releases"
SECRETS_ROOT = Path("/run/hot/secrets")
LOCK = Path("/run/hot/agent.lock")
LIBEXEC = Path("/usr/local/libexec/hot")
DEFAULT_MANIFESTS = "https://github.com/hot-dev/hot/releases/download/v{release}"
POLL_SECONDS = 30
COMMANDS = {"deploy-now", "status", "diagnostics", "backup", "restart-service", "drain-and-stop"}
HOT_SERVICES = ("app", "api", "worker", "task-worker", "scheduler")
# Box storage per size label; the data disk is sized to hold it.
BOX_POOL_GB = {"small": 40, "medium": 100, "large": 200}
# Container users that read their secret files.
SECRET_OWNERS = {"postgres": 999, "valkey": 999}


class AgentError(Exception):
    """A failure that is safe to report: it never contains secret values."""


# --------------------------------------------------------------------------
# Cloud access with the VM's identity
# --------------------------------------------------------------------------

def http(method, url, body=None, headers=None, timeout=30):
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def sigv4_headers(method, host, region, service, target, body, creds, now):
    """AWS Signature Version 4 for a JSON 1.1 API call to `/`."""
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    day = now.strftime("%Y%m%d")
    headers = {"content-type": "application/x-amz-json-1.1", "host": host,
               "x-amz-date": amz_date, "x-amz-target": target}
    if creds.get("Token"):
        headers["x-amz-security-token"] = creds["Token"]
    names = sorted(headers)
    canonical = "\n".join([
        method, "/", "",
        "".join(f"{name}:{headers[name]}\n" for name in names),
        ";".join(names),
        hashlib.sha256(body).hexdigest(),
    ])
    scope = f"{day}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope,
                         hashlib.sha256(canonical.encode()).hexdigest()])
    key = ("AWS4" + creds["SecretAccessKey"]).encode()
    for part in (day, region, service, "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (f"AWS4-HMAC-SHA256 Credential={creds['AccessKeyId']}/{scope}, "
                                f"SignedHeaders={';'.join(names)}, Signature={signature}")
    return headers


class Aws:
    """SSM Parameter Store for the pointer and status, Secrets Manager for secrets.
    Commands arrive through SSM Run Command, which invokes `hot-agent command`."""

    IMDS = "http://169.254.169.254"

    def __init__(self, region):
        self.region = region

    def credentials(self):
        _, imds_token = http("PUT", self.IMDS + "/latest/api/token",
                             headers={"X-aws-ec2-metadata-token-ttl-seconds": "300"})
        auth = {"X-aws-ec2-metadata-token": imds_token.decode()}
        _, role = http("GET", self.IMDS + "/latest/meta-data/iam/security-credentials/", headers=auth)
        _, creds = http("GET", self.IMDS + "/latest/meta-data/iam/security-credentials/" + role.decode().strip(),
                        headers=auth)
        return json.loads(creds)

    def call(self, service, target, payload):
        host = f"{service}.{self.region}.amazonaws.com"
        body = json.dumps(payload).encode()
        headers = sigv4_headers("POST", host, self.region, service, target, body,
                                self.credentials(), datetime.datetime.now(datetime.timezone.utc))
        status, data = http("POST", f"https://{host}/", body, headers)
        result = json.loads(data or b"{}")
        if status != 200:
            kind = result.get("__type", "").split("#")[-1] or f"HTTP {status}"
            raise AgentError(f"{target} failed: {kind}")
        return result

    @staticmethod
    def parameter_name(reference):
        # arn:aws:ssm:REGION:ACCOUNT:parameter/NAME -> /NAME
        return "/" + reference.split(":parameter/", 1)[1] if ":parameter/" in reference else reference

    def get_parameter(self, reference):
        try:
            result = self.call("ssm", "AmazonSSM.GetParameter", {"Name": self.parameter_name(reference)})
        except AgentError as error:
            if "ParameterNotFound" in str(error):
                return None
            raise
        return result["Parameter"]["Value"]

    def put_status(self, reference, status):
        self.call("ssm", "AmazonSSM.PutParameter", {"Name": self.parameter_name(reference), "Value": json.dumps(status),
                                                    "Type": "String", "Overwrite": True})

    def get_secret(self, reference):
        try:
            result = self.call("secretsmanager", "secretsmanager.GetSecretValue", {"SecretId": reference})
        except AgentError as error:
            if "ResourceNotFoundException" in str(error):
                return None
            raise
        return json.loads(result["SecretString"])

    def put_secret_if_absent(self, reference, value):
        existing = self.get_secret(reference)
        if existing is not None:
            return existing
        try:
            # A fixed token makes a retried first write idempotent.
            self.call("secretsmanager", "secretsmanager.PutSecretValue",
                      {"SecretId": reference, "SecretString": json.dumps(value),
                       "ClientRequestToken": "hot-generated-secrets-0001"})
        except AgentError as error:
            if "ResourceExistsException" not in str(error):
                raise
        return self.get_secret(reference)

    def receive_commands(self):
        return []

    def ack(self, ack_id):
        pass


class Gcp:
    """Secret Manager for the pointer and secrets, Cloud Logging for status,
    and a Pub/Sub pull subscription for commands."""

    METADATA = "http://metadata.google.internal/computeMetadata/v1"

    def __init__(self, project, subscription=""):
        self.project = project
        self.subscription = subscription

    def token(self):
        _, data = http("GET", self.METADATA + "/instance/service-accounts/default/token",
                       headers={"Metadata-Flavor": "Google"})
        return json.loads(data)["access_token"]

    def call(self, method, url, payload=None, allow=()):
        body = json.dumps(payload).encode() if payload is not None else None
        status, data = http(method, url, body, {"Authorization": "Bearer " + self.token(),
                                                "Content-Type": "application/json"}, timeout=90)
        if status in allow:
            return None
        if status >= 300:
            raise AgentError(f"{urllib.parse.urlparse(url).path} failed: HTTP {status}")
        return json.loads(data or b"{}")

    def get_secret(self, reference):
        result = self.call("GET", f"https://secretmanager.googleapis.com/v1/{reference}/versions/latest:access",
                           allow=(404,))
        if result is None:
            return None
        return json.loads(base64.b64decode(result["payload"]["data"]))

    def get_parameter(self, reference):
        value = self.get_secret(reference)
        return value if value is None else value.get("release")

    def put_secret_if_absent(self, reference, value):
        existing = self.get_secret(reference)
        if existing is not None:
            return existing
        self.call("POST", f"https://secretmanager.googleapis.com/v1/{reference}:addVersion",
                  {"payload": {"data": base64.b64encode(json.dumps(value).encode()).decode()}})
        return self.get_secret(reference)

    def put_status(self, reference, status):
        self.call("POST", "https://logging.googleapis.com/v2/entries:write", {
            "logName": f"projects/{self.project}/logs/{reference or 'hot-agent-status'}",
            "resource": {"type": "global"}, "entries": [{"jsonPayload": status, "severity": "INFO"}]})

    def receive_commands(self):
        if not self.subscription:
            return []
        result = self.call("POST", f"https://pubsub.googleapis.com/v1/{self.subscription}:pull", {"maxMessages": 5})
        return [(m["ackId"], json.loads(base64.b64decode(m["message"].get("data", "") or b"e30=")))
                for m in (result or {}).get("receivedMessages", [])]

    def ack(self, ack_id):
        self.call("POST", f"https://pubsub.googleapis.com/v1/{self.subscription}:acknowledge", {"ackIds": [ack_id]})


# --------------------------------------------------------------------------
# Secrets: one generated bundle, one operator bundle, per-role files
# --------------------------------------------------------------------------

def generate_secrets():
    """Values the installation creates for itself on first boot. They are
    stored once (create-if-absent), so they never enter OpenTofu state."""
    return {key: token.token_hex(32) for key in
            ("POSTGRES_PASSWORD", "POSTGRES_RUNTIME_PASSWORD", "VALKEY_PASSWORD",
             "HOT_ENCRYPTION_KEY", "HOT_APP_SESSION_SECRET", "ADMIN_PASSWORD")}


# Operator secrets by name prefix, and the Hot services that read them.
OPERATOR_ROUTES = {
    "HOT_EMAIL_": ("worker",),
    "HOT_GOOGLE_CLIENT_": ("app",),
    "HOT_GITHUB_CLIENT_": ("app",),
    "HOT_BUILD_AWS_": HOT_SERVICES,
    "HOT_FILE_AWS_": HOT_SERVICES,
}
OPERATOR_INGRESS = ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY")


def operator_routes(operator):
    """Which Hot services mount each operator secret. Unknown names are
    refused rather than silently ignored."""
    routes = {}
    for key in operator:
        if key in OPERATOR_INGRESS:
            continue
        roles = next((roles for prefix, roles in OPERATOR_ROUTES.items() if key.startswith(prefix)), None)
        if roles is None:
            raise AgentError(f"The operator secret bundle has an unsupported entry {key}")
        for role in roles:
            routes.setdefault(role, []).append(key)
    return routes


def role_secrets(config, generated, operator):
    """Every file the rendered bundle mounts, keyed by role, then by name.
    Each role receives only what it needs."""
    g = generated
    runtime_uri = f"postgres://hot_runtime:{g['POSTGRES_RUNTIME_PASSWORD']}@postgres:5432/hot?sslmode=disable"
    owner_uri = f"postgres://hot_owner:{g['POSTGRES_PASSWORD']}@postgres:5432/hot?sslmode=disable"
    redis_uri = f"redis://hot:{g['VALKEY_PASSWORD']}@valkey:6379"
    files = {}
    for role in HOT_SERVICES:
        files[role] = {"HOT_DB_URI": runtime_uri, "HOT_REDIS_URI": redis_uri}
        if role != "scheduler":
            files[role]["HOT_ENCRYPTION_KEY"] = g["HOT_ENCRYPTION_KEY"]
    for role, keys in operator_routes(operator).items():
        for key in keys:
            files[role][key] = operator[key]
    files["app"]["HOT_APP_SESSION_SECRET"] = g["HOT_APP_SESSION_SECRET"]
    files["postgres"] = {"POSTGRES_PASSWORD": g["POSTGRES_PASSWORD"],
                         "POSTGRES_RUNTIME_PASSWORD": g["POSTGRES_RUNTIME_PASSWORD"]}
    files["valkey"] = {"VALKEY_PASSWORD": g["VALKEY_PASSWORD"],
                       "VALKEY_ACL": f"user default off\nuser hot on >{g['VALKEY_PASSWORD']} ~* &* +@all\n"}
    for key in ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY"):
        if key not in operator:
            raise AgentError(f"The operator secret bundle needs {key} for HTTPS ingress")
    files["ingress"] = {"TLS_CERTIFICATE": operator["TLS_CERTIFICATE"], "TLS_PRIVATE_KEY": operator["TLS_PRIVATE_KEY"]}
    files["migrate"] = {"HOT_DB_MIGRATION_URI": owner_uri}
    files["provision"] = {"PGPASSFILE": f"postgres:5432:hot:hot_owner:{g['POSTGRES_PASSWORD']}\n"}
    if config.get("admin_email"):
        files["bootstrap-admin"] = {"HOT_DB_URI": runtime_uri, "ADMIN_PASSWORD": g["ADMIN_PASSWORD"]}
    return files


def write_secret_files(root, files, chown=os.chown):
    """Write secrets to tmpfs: one 0400 file per value, owned by the reading
    container's user. Stale files from other releases are removed."""
    root = Path(root)
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(mode=0o711, parents=True)
    for role, values in files.items():
        directory = root / role
        directory.mkdir(mode=0o711)
        owner = SECRET_OWNERS.get(role, 0 if role == "task-worker" else 10001)
        for key, value in values.items():
            path = directory / key
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
            with os.fdopen(descriptor, "w") as handle:
                handle.write(value)
            chown(path, owner, owner)


# --------------------------------------------------------------------------
# The agent
# --------------------------------------------------------------------------

class Agent:
    def __init__(self, config, cloud, run=None, sleep=time.sleep, roots=None, chown=os.chown):
        self.config = config
        self.cloud = cloud
        self.run_command = run or self._run
        self.sleep = sleep
        self.chown = chown
        roots = roots or {}
        self.releases = Path(roots.get("releases", RELEASES))
        self.secrets_root = Path(roots.get("secrets", SECRETS_ROOT))
        self.etc = Path(roots.get("etc", "/etc/hot"))
        self.references = config["secret_references"]

    @staticmethod
    def _run(command, **kwargs):
        return subprocess.run(command, check=True, text=True, capture_output=True, **kwargs)

    # -- host preparation ---------------------------------------------------

    def data_device(self):
        if self.config["cloud"] == "aws":
            return "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_" + self.config["data_volume_id"].replace("-", "")
        return "/dev/disk/by-id/google-hot-data"

    def prepare_host(self, first_install):
        """Mount the data disk and write host settings. A blank disk is
        formatted only for a brand-new installation."""
        command = [str(LIBEXEC / "hot-data-disk"), self.data_device()]
        if first_install:
            command.append("--allow-format")
        self.run_command(command)
        self.etc.mkdir(parents=True, exist_ok=True)
        (self.etc / "host.env").write_text(f"HOT_BOX_POOL_GB={BOX_POOL_GB[self.config.get('size', 'small')]}\n")
        deny = [render.RUNTIME_SUBNET]
        if self.config.get("network", {}).get("mode", "new") == "new":
            deny.append(self.config.get("network", {}).get("cidr", "10.68.0.0/16"))
        (self.etc / "guest-network.env").write_text(f"HOT_GUEST_DENY_CIDRS={','.join(deny)}\n")
        self.run_command(["systemctl", "restart", "hot-guest-firewall.service"])
        self.run_command(["systemctl", "start", "kata-containerd.service"])

    # -- release delivery -----------------------------------------------------

    def fetch_release(self, release):
        """Download the manifest and its signature, then verify them."""
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", release):
            raise AgentError(f"Release pointer value {release!r} is not a release version")
        directory = self.releases / release
        directory.mkdir(parents=True, exist_ok=True)
        base = (self.config.get("manifest_base_url") or DEFAULT_MANIFESTS).format(release=release)
        for name in ("hot-selfhost-manifest.json", "hot-selfhost-manifest.sigstore.json"):
            status, data = http("GET", f"{base}/{name}", timeout=60)
            if status != 200:
                raise AgentError(f"Release {release}: {name} is unavailable (HTTP {status})")
            (directory / name).write_bytes(data)
        render.verify_manifest_signature(directory / "hot-selfhost-manifest.json",
                                         directory / "hot-selfhost-manifest.sigstore.json")
        manifest = json.loads((directory / "hot-selfhost-manifest.json").read_text())
        if manifest.get("release") != release:
            raise AgentError(f"The manifest for {release} names release {manifest.get('release')}")
        return manifest

    def render_bundle(self, release, manifest, operator):
        bundle = self.releases / release / "bundle"
        if bundle.exists():
            shutil.rmtree(bundle)
        bundle.mkdir(mode=0o700)
        routes = operator_routes(operator)
        # S3 keys in the bundle mean file credentials; otherwise VM identity.
        s3_files = any(key.startswith(("HOT_BUILD_AWS_", "HOT_FILE_AWS_")) for key in operator)
        sessions = any(key.endswith("_AWS_SESSION_TOKEN") for key in operator)
        compose = render.render(self.config, manifest, str(HOT_ROOT / "data"), str(self.secrets_root),
                                s3_auth="files" if s3_files else "identity", s3_session_tokens=sessions,
                                operator_secrets=routes)
        (bundle / "compose.json").write_text(json.dumps(compose, indent=2))
        (bundle / "nginx.conf").write_text(render.nginx_config(render.validate(self.config)))
        (bundle / "init.sql").write_text(render.INIT_SQL)
        (bundle / "valkey.conf").write_text(render.VALKEY_CONF)
        shutil.copy(provision_sql(), bundle / "provision_runtime.sql")
        return bundle

    def compose(self, bundle, *args):
        return self.run_command(["docker", "compose", "-f", str(bundle / "compose.json"), *args], cwd=bundle)

    def operator_secrets(self):
        return self.cloud.get_secret(self.references["operator"]) or {}

    def deliver_secrets(self):
        generated = self.cloud.put_secret_if_absent(self.references["generated"], generate_secrets())
        write_secret_files(self.secrets_root, role_secrets(self.config, generated, self.operator_secrets()),
                           chown=self.chown)

    def deploy(self, release, previous):
        """Deploy one release. Order matters: the old scheduler stops before
        anything else changes, migrations finish before services start, and
        the new scheduler starts last."""
        steps = []
        manifest = self.fetch_release(release)
        steps.append("verified")
        current = self.state().get("migrations")
        latest = manifest.get("migrations", {}).get("latest", 0)
        if current is not None and latest < current:
            raise AgentError(f"Release {release} expects migration {latest}, older than the installed {current}; "
                             "downgrades need a restore")
        bundle = self.render_bundle(release, manifest, self.operator_secrets())
        self.deliver_secrets()
        steps.append("secrets")
        self.compose(bundle, "--profile", "admin", "pull", "--quiet")
        steps.append("pulled")
        self.compose(bundle, "stop", "scheduler")
        steps.append("scheduler-stopped")
        self.compose(bundle, "up", "-d", "--wait", "postgres", "valkey")
        self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
        self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
        steps.append("migrated")
        if self.config.get("admin_email"):
            self.compose(bundle, "--profile", "admin", "run", "--rm", "bootstrap-admin")
            steps.append("admin")
        self.compose(bundle, "up", "-d", "--remove-orphans", "app", "api", "worker", "task-worker", "ingress")
        self.wait_ready()
        self.compose(bundle, "up", "-d", "scheduler")
        steps.append("started")
        self.save_state({"release": release, "previous": previous, "migrations": latest})
        return steps

    def wait_ready(self, attempts=40, probe=None):
        """Check the app and API through the ingress from the host itself, so
        private installations are checked too."""
        probe = probe or https_status
        targets = [(self.config["app_hostname"], "/signin"), (self.config["api_hostname"], "/status")]
        for _ in range(attempts):
            try:
                if all(probe(host, path) < 400 for host, path in targets):
                    return
            except OSError:
                pass
            self.sleep(3)
        raise AgentError("The app and API did not become ready")

    # -- state and status ------------------------------------------------------

    def state_path(self):
        return self.releases / "state.json"

    def state(self):
        try:
            return json.loads(self.state_path().read_text())
        except (OSError, ValueError):
            return {}

    def save_state(self, state):
        self.state_path().write_text(json.dumps(state))

    def report(self, **status):
        status = {"installation": self.config["name"], "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  **self.state(), **status}
        print(json.dumps(status), flush=True)
        reference = self.config.get("status_reference", "")
        if reference:
            try:
                self.cloud.put_status(reference, status)
            except AgentError as error:
                print(json.dumps({"status_report_failed": str(error)}), file=sys.stderr, flush=True)
        return status

    # -- reconcile and commands ------------------------------------------------

    def reconcile(self):
        desired = self.cloud.get_parameter(self.config["release_pointer"])
        state = self.state()
        if not desired or desired == state.get("release"):
            return None
        first_install = self.cloud.get_secret(self.references["generated"]) is None and not state
        try:
            self.prepare_host(first_install)
            steps = self.deploy(desired, state.get("release"))
            return self.report(state="deployed", release=desired, steps=steps)
        except (AgentError, ValueError, subprocess.CalledProcessError) as error:
            return self.report(state="failed", release=desired, error=describe(error))

    def command(self, operation, service="", request_id=""):
        if operation not in COMMANDS:
            raise AgentError(f"Unknown command {operation!r}")
        release = self.state().get("release")
        bundle = self.releases / release / "bundle" if release else None
        if operation == "deploy-now":
            return self.reconcile() or self.report(state="current", request_id=request_id)
        if operation == "status":
            return self.report(state="status", request_id=request_id)
        if bundle is None:
            raise AgentError("Nothing is deployed yet")
        if operation == "restart-service":
            if service not in HOT_SERVICES + ("ingress",):
                raise AgentError(f"Unknown service {service!r}")
            # Secrets are rendered again, which is how rotation takes effect.
            self.deliver_secrets()
            if service == "scheduler":
                self.compose(bundle, "stop", "scheduler")
            self.compose(bundle, "up", "-d", "--force-recreate", service)
            return self.report(state="restarted", service=service, request_id=request_id)
        if operation == "drain-and-stop":
            self.compose(bundle, "stop", "scheduler")
            self.compose(bundle, "stop", "--timeout", "120", "worker", "task-worker")
            self.compose(bundle, "stop", "app", "api", "ingress")
            return self.report(state="stopped", request_id=request_id)
        if operation == "backup":
            stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
            target = HOT_ROOT / "backups" / stamp
            target.mkdir(parents=True, mode=0o700)
            self.compose(bundle, "exec", "-T", "postgres", "pg_dump", "-U", "hot_owner", "-d", "hot",
                         "--format=custom", "--file=/var/lib/postgresql/backup.dump")
            shutil.move(str(HOT_ROOT / "data/postgres/backup.dump"), str(target / "postgres.dump"))
            return self.report(state="backed-up", backup=str(target), request_id=request_id)
        # diagnostics: service state and versions, never configuration values.
        services = {}
        for line in self.compose(bundle, "ps", "--all", "--format", "json").stdout.splitlines():
            entry = json.loads(line)
            services[entry.get("Service", "?")] = entry.get("State", "?")
        host = json.loads(Path("/opt/hot-host/versions.json").read_text())
        return self.report(state="diagnostics", request_id=request_id, services=services,
                           kata=host.get("kata"), guest_rootfs=host.get("guest_rootfs_sha256", "")[:12])


def provision_sql():
    """The runtime grant script: installed beside the renderer on hosts, and
    in resources/ in a source checkout."""
    beside = RUNTIME / "provision_runtime.sql"
    if beside.exists():
        return beside
    return Path(render.__file__).resolve().parents[2] / "resources/db/postgres/provision_runtime.sql"


def https_status(host, path, address="127.0.0.1"):
    """GET through the local ingress, sending the hostname as SNI: the
    ingress refuses handshakes that name no configured host."""
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # loopback only; certificates name the public hostnames
    connection = http.client.HTTPSConnection(host, 443, timeout=5, context=context)
    connection.sock = context.wrap_socket(socket.create_connection((address, 443), timeout=5),
                                          server_hostname=host)
    try:
        connection.request("GET", path, headers={"Host": host})
        return connection.getresponse().status
    finally:
        connection.close()


def describe(error):
    if isinstance(error, subprocess.CalledProcessError):
        command = " ".join(str(part) for part in error.cmd[:6])
        return f"{command} exited {error.returncode}"
    return str(error)


@contextlib.contextmanager
def locked():
    LOCK.parent.mkdir(parents=True, exist_ok=True)
    with open(LOCK, "w") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def cloud_for(config):
    if config["cloud"] == "aws":
        return Aws(config["region"])
    return Gcp(config["project"], config.get("command_subscription", ""))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("run")
    command = sub.add_parser("command")
    command.add_argument("operation", choices=sorted(COMMANDS))
    command.add_argument("--service", default="")
    command.add_argument("--request-id", default="")
    args = parser.parse_args(argv)
    config = json.loads(INSTALLATION.read_text())
    agent = Agent(config, cloud_for(config))
    if args.mode == "command":
        with locked():
            try:
                agent.command(args.operation, args.service, args.request_id)
            except AgentError as error:
                agent.report(state="rejected", error=str(error), request_id=args.request_id)
                return 1
        return 0
    while True:
        with locked():
            agent.reconcile()
            for ack_id, request in agent.cloud.receive_commands():
                agent.cloud.ack(ack_id)
                try:
                    agent.command(str(request.get("operation", "")), str(request.get("service", "")),
                                  str(request.get("request_id", "")))
                except AgentError as error:
                    agent.report(state="rejected", error=str(error), request_id=str(request.get("request_id", "")))
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    sys.exit(main())
