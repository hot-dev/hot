#!/usr/bin/env python3
"""Hot host deploy agent for single-machine installations.

Runs on the VM under systemd. It reads the installation's release pointer,
deploys the signed release it names, delivers secrets as tmpfs files, and
reports status. Operators and workflows reach it only through a fixed set of
commands (see COMMANDS); it never runs arbitrary shell.

    hot-agent run                         # reconcile to the release pointer, forever
    hot-agent command OP [--service S] [--request-id ID]

Every report also goes to the installation's log with its request ID, where
infra/operator/selfhost.py waits for it. Diagnostics and service logs are
redacted before they leave the host.

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
from http.client import HTTPSConnection
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
import xml.etree.ElementTree as ElementTree

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
COMMANDS = {"deploy-now", "status", "diagnostics", "logs", "backup", "restart-service", "drain-and-stop", "roll-back"}
HOT_SERVICES = ("app", "api", "worker", "task-worker", "scheduler")
# Service logs: the newest lines, each shortened, within a size that fits
# one log entry on either cloud.
LOG_LINES = 200
LOG_LINE_CHARS = 1000
LOG_CHARS = 48000
# Reports also go to this stream of the installation's CloudWatch log group.
STATUS_STREAM = "hot-agent-status"
# Report fields too large for the AWS status parameter (4 KB); the log has them.
DETAIL_KEYS = ("diagnostics", "logs")
# Box storage per size label; the data disk is sized to hold it.
BOX_POOL_GB = {"small": 40, "medium": 100, "large": 200}
# Container users that read their secret files.
SECRET_OWNERS = {"postgres": 999, "valkey": 999}
# Deploy records kept in releases/history.json, and pre-upgrade dumps kept.
HISTORY = 20
UPGRADE_DUMPS = 3


# Health metrics the agent publishes every cycle, with their CloudWatch units.
# The stacks alarm on them; see infra/README.md.
METRIC_NAMESPACE = "Hot/SelfHost"
GCP_METRIC_PREFIX = "custom.googleapis.com/hot/"
GCP_METRIC_NAMES = {"AgentHeartbeat": "agent_heartbeat", "UnhealthyServices": "unhealthy_services",
                    "SchedulerRunning": "scheduler_running", "DependenciesReachable": "dependencies_reachable",
                    "Ready": "ready", "DataDiskUsedPercent": "data_disk_used_percent",
                    "BackupAgeHours": "backup_age_hours", "CertificateDaysLeft": "certificate_days_left"}


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


def sigv4_headers(method, host, region, service, target, body, creds, now,
                  content_type="application/x-amz-json-1.1"):
    """AWS Signature Version 4 for an API call to `/`: JSON 1.1 with a
    target, or the Query protocol (form-encoded, no target)."""
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    day = now.strftime("%Y%m%d")
    headers = {"content-type": content_type, "host": host, "x-amz-date": amz_date}
    if target:
        headers["x-amz-target"] = target
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

    def __init__(self, region, log_group=""):
        self.region = region
        self.log_group = log_group

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

    def query(self, service, version, params):
        """A Query-protocol call (EC2, CloudWatch); returns the XML response."""
        host = f"{service}.{self.region}.amazonaws.com"
        action = params.pop("Action")
        body = urllib.parse.urlencode({"Action": action, "Version": version, **params}).encode()
        headers = sigv4_headers("POST", host, self.region, service, None, body, self.credentials(),
                                datetime.datetime.now(datetime.timezone.utc),
                                content_type="application/x-www-form-urlencoded; charset=utf-8")
        status, data = http("POST", f"https://{host}/", body, headers)
        root = ElementTree.fromstring(data or b"<Empty/>")
        if status != 200:
            code = next((e.text for e in root.iter() if e.tag.endswith("Code")), None) or f"HTTP {status}"
            raise AgentError(f"{service} {action} failed: {code}")
        return root

    def put_metrics(self, installation, metrics):
        params = {"Action": "PutMetricData", "Namespace": METRIC_NAMESPACE}
        for index, (name, (value, unit)) in enumerate(sorted(metrics.items()), start=1):
            prefix = f"MetricData.member.{index}."
            params.update({prefix + "MetricName": name, prefix + "Value": repr(float(value)), prefix + "Unit": unit,
                           prefix + "Dimensions.member.1.Name": "Installation",
                           prefix + "Dimensions.member.1.Value": installation})
        self.query("monitoring", "2010-08-01", params)

    def latest_snapshot(self, volume_id, installation):
        """When the newest completed snapshot of the data volume started."""
        root = self.query("ec2", "2016-11-15", {
            "Action": "DescribeSnapshots", "Owner.1": "self",
            "Filter.1.Name": "volume-id", "Filter.1.Value.1": volume_id,
            "Filter.2.Name": "status", "Filter.2.Value.1": "completed"})
        times = [e.text for e in root.iter() if e.tag.endswith("startTime") and e.text]
        return max((parse_time(t) for t in times), default=None)

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
        """The full report goes to the log, where it is found by request ID;
        the status parameter keeps a summary of the latest one."""
        summary = {key: value for key, value in status.items() if key not in DETAIL_KEYS}
        try:
            if self.log_group:
                self.log_status(status)
        finally:
            self.call("ssm", "AmazonSSM.PutParameter", {"Name": self.parameter_name(reference),
                                                        "Value": json.dumps(summary), "Type": "String", "Overwrite": True})

    def log_status(self, status):
        target = {"logGroupName": self.log_group, "logStreamName": STATUS_STREAM}
        event = {**target, "logEvents": [{"timestamp": int(time.time() * 1000), "message": json.dumps(status)}]}
        try:
            self.call("logs", "Logs_20140328.PutLogEvents", event)
        except AgentError as error:
            if "ResourceNotFoundException" not in str(error):
                raise
            try:
                self.call("logs", "Logs_20140328.CreateLogStream", target)
            except AgentError as created:
                if "ResourceAlreadyExistsException" not in str(created):
                    raise
            self.call("logs", "Logs_20140328.PutLogEvents", event)

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

    def __init__(self, project, subscription="", zone=""):
        self.project = project
        self.subscription = subscription
        self.zone = zone
        self.instance_id = None

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

    def put_metrics(self, installation, metrics):
        if self.instance_id is None:
            _, data = http("GET", self.METADATA + "/instance/id", headers={"Metadata-Flavor": "Google"})
            self.instance_id = data.decode()
        now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        resource = {"type": "gce_instance", "labels": {"project_id": self.project, "zone": self.zone,
                                                        "instance_id": self.instance_id}}
        # Per-installation metric types: descriptors are project-wide, and
        # each installation's stack owns its own.
        series = [{"metric": {"type": f"{GCP_METRIC_PREFIX}{installation}/{GCP_METRIC_NAMES[name]}"},
                   "resource": resource, "points": [{"interval": {"endTime": now}, "value": {"doubleValue": float(value)}}]}
                  for name, (value, _) in sorted(metrics.items())]
        self.call("POST", f"https://monitoring.googleapis.com/v3/projects/{self.project}/timeSeries", {"timeSeries": series})

    def latest_snapshot(self, disk_id, installation):
        """When the newest ready snapshot of the data disk was created. The
        snapshot schedule labels its snapshots with the installation."""
        query = urllib.parse.urlencode({"filter": f'labels.hot-installation = "{installation}"'})
        result = self.call("GET", f"https://compute.googleapis.com/compute/v1/projects/{self.project}/global/snapshots?{query}")
        disk = disk_id.rsplit("/", 1)[-1]
        times = [item["creationTimestamp"] for item in (result or {}).get("items", [])
                 if item.get("status") == "READY" and item.get("sourceDisk", "").endswith("/disks/" + disk)]
        return max((parse_time(t) for t in times), default=None)

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
# Connection settings for existing data services; the bundle mounts them itself.
OPERATOR_DATA = {"postgres": ("HOT_DB_URI", "HOT_DB_MIGRATION_URI", "POSTGRES_CA_CERT"),
                 "valkey": ("HOT_REDIS_URI",)}
CA_PATH = "/run/secrets/POSTGRES_CA_CERT"


def operator_routes(operator, config=None):
    """Which Hot services mount each operator secret. Unknown names are
    refused rather than silently ignored."""
    config = config or {}
    data_keys = {key for service, keys in OPERATOR_DATA.items()
                 if render.data_mode(config, service) == "existing" for key in keys}
    routes = {}
    for key in operator:
        if key in OPERATOR_INGRESS or key in data_keys:
            continue
        roles = next((roles for prefix, roles in OPERATOR_ROUTES.items() if key.startswith(prefix)), None)
        if roles is None:
            raise AgentError(f"The operator secret bundle has an unsupported entry {key}")
        for role in roles:
            routes.setdefault(role, []).append(key)
    return routes


def verified_postgres_uri(uri, name, config):
    """Existing PostgreSQL must be reached with full certificate
    verification against the operator's CA, mounted at CA_PATH."""
    parts = urllib.parse.urlsplit(uri)
    if parts.scheme not in ("postgres", "postgresql") or not parts.hostname:
        raise AgentError(f"{name} must be a postgres:// URI")
    query = dict(urllib.parse.parse_qsl(parts.query))
    if query.get("sslmode") != "verify-full":
        raise AgentError(f"{name} must set sslmode=verify-full for an existing database")
    if name == "HOT_DB_URI" and urllib.parse.unquote(parts.username or "") != render.runtime_role(config):
        raise AgentError(f"HOT_DB_URI must log in as data.postgres.runtime_role ({render.runtime_role(config)})")
    query["sslrootcert"] = CA_PATH
    return urllib.parse.urlunsplit(parts._replace(query=urllib.parse.urlencode(query)))


# Managed PostgreSQL CA certificates: RDS publishes its regional bundle;
# a Cloud SQL instance's own CA comes from the stack.
RDS_CA_BUNDLE = "https://truststore.pki.rds.amazonaws.com/{region}/{region}-bundle.pem"


def postgres_uri(user, password, endpoint, database, sslmode):
    query = urllib.parse.urlencode({"sslmode": sslmode, "sslrootcert": CA_PATH})
    credentials = urllib.parse.quote(user, safe="") + ":" + urllib.parse.quote(password, safe="")
    return f"postgres://{credentials}@{endpoint['host']}:{int(endpoint['port'])}/{database}?{query}"


def managed_sslmode(config):
    """RDS certificates name the instance's endpoint, so the hostname is
    verified too. A Cloud SQL instance's server certificate is signed by a CA
    that belongs to that instance alone and names no address, so verifying
    the CA identifies the instance."""
    return "verify-full" if config["cloud"] == "aws" else "verify-ca"


def data_services(config, operator):
    """(host, port) of managed and existing data services."""
    services = []
    for service, key, port in (("postgres", "HOT_DB_URI", 5432), ("valkey", "HOT_REDIS_URI", 6379)):
        mode = render.data_mode(config, service)
        if mode == "managed":
            endpoint = config["data"][service]["endpoint"]
            services.append((endpoint["host"], int(endpoint["port"])))
        elif mode == "existing":
            parts = urllib.parse.urlsplit(operator[key])
            if parts.hostname:
                services.append((parts.hostname, parts.port or port))
    return services


def data_endpoints(config, operator):
    """Hosts of managed and existing data services, which user code must
    never reach."""
    return [host for host, _ in data_services(config, operator)]


def role_secrets(config, generated, operator, managed=None):
    """Every file the rendered bundle mounts, keyed by role, then by name.
    Each role receives only what it needs. `managed` carries a managed
    database's administrator login and CA certificate, and a managed
    Valkey's AUTH token."""
    g = generated
    managed = managed or {}
    pg_mode = render.data_mode(config, "postgres")
    pg_local = pg_mode == "local"
    valkey_local = render.data_mode(config, "valkey") == "local"
    if pg_local:
        runtime_uri = f"postgres://hot_runtime:{g['POSTGRES_RUNTIME_PASSWORD']}@postgres:5432/hot?sslmode=disable"
        owner_uri = f"postgres://hot_owner:{g['POSTGRES_PASSWORD']}@postgres:5432/hot?sslmode=disable"
    elif pg_mode == "managed":
        admin = managed.get("postgres")
        if not admin:
            raise AgentError("Managed PostgreSQL needs its administrator login and CA certificate")
        endpoint, sslmode = config["data"]["postgres"]["endpoint"], managed_sslmode(config)
        runtime_uri = postgres_uri(render.runtime_role(config), g["POSTGRES_RUNTIME_PASSWORD"], endpoint, "hot", sslmode)
        owner_uri = postgres_uri("hot_owner", g["POSTGRES_PASSWORD"], endpoint, "hot", sslmode)
        admin_uri = {database: postgres_uri(admin["username"], admin["password"], endpoint, database, sslmode)
                     for database in ("postgres", "hot")}
    else:
        for key in OPERATOR_DATA["postgres"]:
            if key not in operator:
                raise AgentError(f"An existing database needs {key} in the operator secret bundle")
        runtime_uri = verified_postgres_uri(operator["HOT_DB_URI"], "HOT_DB_URI", config)
        owner_uri = verified_postgres_uri(operator["HOT_DB_MIGRATION_URI"], "HOT_DB_MIGRATION_URI", config)
    if valkey_local:
        redis_uri = f"redis://hot:{g['VALKEY_PASSWORD']}@valkey:6379"
    elif render.data_mode(config, "valkey") == "managed":
        token = (managed.get("valkey") or {}).get("password")
        if not token:
            raise AgentError("The managed Valkey AUTH token secret has no token yet")
        endpoint = config["data"]["valkey"]["endpoint"]
        redis_uri = f"rediss://:{urllib.parse.quote(token, safe='')}@{endpoint['host']}:{int(endpoint['port'])}"
    else:
        redis_uri = operator.get("HOT_REDIS_URI", "")
        if not redis_uri.startswith("rediss://"):
            raise AgentError("An existing Valkey needs a rediss:// HOT_REDIS_URI in the operator secret bundle")
    ca = ({} if pg_local else {"POSTGRES_CA_CERT": managed["postgres"]["ca"]} if pg_mode == "managed"
          else {"POSTGRES_CA_CERT": operator["POSTGRES_CA_CERT"]})
    files = {}
    for role in HOT_SERVICES:
        files[role] = {"HOT_DB_URI": runtime_uri, "HOT_REDIS_URI": redis_uri, **ca}
        if role != "scheduler":
            files[role]["HOT_ENCRYPTION_KEY"] = g["HOT_ENCRYPTION_KEY"]
    for role, keys in operator_routes(operator, config).items():
        for key in keys:
            files[role][key] = operator[key]
    files["app"]["HOT_APP_SESSION_SECRET"] = g["HOT_APP_SESSION_SECRET"]
    if pg_local:
        files["postgres"] = {"POSTGRES_PASSWORD": g["POSTGRES_PASSWORD"],
                             "POSTGRES_RUNTIME_PASSWORD": g["POSTGRES_RUNTIME_PASSWORD"]}
    if valkey_local:
        files["valkey"] = {"VALKEY_PASSWORD": g["VALKEY_PASSWORD"],
                           "VALKEY_ACL": f"user default off\nuser hot on >{g['VALKEY_PASSWORD']} ~* &* +@all\n"}
    for key in ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY"):
        if key not in operator:
            raise AgentError(f"The operator secret bundle needs {key} for HTTPS ingress")
    files["ingress"] = {"TLS_CERTIFICATE": operator["TLS_CERTIFICATE"], "TLS_PRIVATE_KEY": operator["TLS_PRIVATE_KEY"]}
    files["migrate"] = {"HOT_DB_MIGRATION_URI": owner_uri, **ca}
    if pg_local:
        files["provision"] = {"PGPASSFILE": f"postgres:5432:hot:hot_owner:{g['POSTGRES_PASSWORD']}\n"}
    else:
        files["provision"] = {"HOT_DB_MIGRATION_URI": owner_uri, **ca}
        files["check-postgres"] = {"HOT_DB_MIGRATION_URI": owner_uri, **ca}
    if pg_mode == "managed":
        files["prepare-postgres"] = {"POSTGRES_ADMIN_URI": admin_uri["postgres"], "POSTGRES_ADMIN_HOT_URI": admin_uri["hot"],
                                     "POSTGRES_PASSWORD": g["POSTGRES_PASSWORD"],
                                     "POSTGRES_RUNTIME_PASSWORD": g["POSTGRES_RUNTIME_PASSWORD"], **ca}
    if config.get("admin_email"):
        files["bootstrap-admin"] = {"HOT_DB_URI": runtime_uri, "ADMIN_PASSWORD": g["ADMIN_PASSWORD"], **ca}
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
# Redaction: what diagnostics and logs may carry off the host
# --------------------------------------------------------------------------

class Redactor:
    """Removes secrets from text the agent reports: every value in the
    host's secret files (and passwords inside connection URIs), then common
    credential shapes. Email addresses keep only their domain, and AWS
    account IDs only their last four digits."""

    PATTERNS = [
        (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)", re.S), "[private key]"),
        (re.compile(r"\b([a-z][a-z0-9+.-]*://)[^/\s:@]*:[^@\s/]*@", re.I), r"\1[credentials]@"),
        (re.compile(r"\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+", re.I), r"\1 [redacted]"),
        (re.compile(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*"), "[token]"),
        (re.compile(r"\bhot_[0-9a-f]{32}_[0-9a-f]+\b"), "[api key]"),
        (re.compile(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"), "[access key]"),
        (re.compile(r"(?i)\b((?:\w+[_-])?(?:password|passwd|secret|token|api[_-]?key|access[_-]?key))"
                    r"(\\?[\"']?\s*[:=]\s*\\?[\"']?)[^\s\"'\\,;&}]+"), r"\1\2[redacted]"),
        (re.compile(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)"), r"[email]@\1"),
        (re.compile(r"(arn:aws[a-z-]*:[a-z0-9-]*:[a-z0-9-]*:)\d{8}(\d{4})"), r"\1****\2"),
    ]
    # Shorter values are too common to replace safely; the patterns cover
    # passwords inside URIs and settings.
    MINIMUM = 8

    def __init__(self, values=()):
        known = set()
        for value in values:
            value = value.strip()
            known.add(value)
            for word in value.split():
                if "://" in word:
                    password = urllib.parse.urlsplit(word).password or ""
                    known.update({password, urllib.parse.unquote(password)})
        self.values = sorted((v for v in known if len(v) >= self.MINIMUM), key=len, reverse=True)

    @classmethod
    def from_files(cls, root):
        values = []
        for path in sorted(Path(root).glob("*/*")):
            with contextlib.suppress(OSError, UnicodeDecodeError):
                values.append(path.read_text())
        return cls(values)

    def __call__(self, text):
        for value in self.values:
            text = text.replace(value, "[secret]")
        for pattern, replacement in self.PATTERNS:
            text = pattern.sub(replacement, text)
        return text

    def tree(self, value):
        if isinstance(value, dict):
            return {key: self.tree(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.tree(item) for item in value]
        return self(value) if isinstance(value, str) else value


# Configuration keys that name where secrets and channels live. Diagnostics
# show that they are set, not what they name.
REFERENCE_KEYS = {"secret_references", "release_pointer", "status_reference", "command_subscription"}


def effective_configuration(config, redact):
    """The installation as the host sees it, without references, private
    certificates, or anything the redactor removes."""
    def clean(key, value):
        if key in REFERENCE_KEYS or key.endswith("_secret"):
            return "[reference]" if value else ""
        if key == "ca_certificate":
            return "[certificate]" if value else ""
        if isinstance(value, dict):
            return {k: clean(k, v) for k, v in value.items()}
        if isinstance(value, list):
            return [clean(key, v) for v in value]
        return redact(value) if isinstance(value, str) else value
    return {key: clean(key, value) for key, value in config.items()}


def bounded_lines(text, redact, lines=LOG_LINES, line_chars=LOG_LINE_CHARS, total=LOG_CHARS):
    """The newest redacted lines that fit within `total` characters."""
    kept, size = [], 0
    for line in reversed(text.splitlines()[-lines:]):
        line = redact(line)
        if len(line) > line_chars:
            line = line[:line_chars] + " [shortened]"
        if size + len(line) > total:
            break
        kept.append(line)
        size += len(line) + 1
    return kept[::-1]


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
        self.data_root = Path(roots.get("data", HOT_ROOT))
        self.secrets_root = Path(roots.get("secrets", SECRETS_ROOT))
        self.etc = Path(roots.get("etc", "/etc/hot"))
        self.host = Path(roots.get("host", "/opt/hot-host"))
        self.backups = self.data_root / "backups"
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
        routes = operator_routes(operator, self.config)
        # S3 keys in the bundle mean file credentials; otherwise VM identity.
        s3_files = any(key.startswith(("HOT_BUILD_AWS_", "HOT_FILE_AWS_")) for key in operator)
        sessions = any(key.endswith("_AWS_SESSION_TOKEN") for key in operator)
        compose = render.render(self.config, manifest, str(HOT_ROOT / "data"), str(self.secrets_root),
                                s3_auth="files" if s3_files else "identity", s3_session_tokens=sessions,
                                operator_secrets=routes,
                                extra_deny=self.resolve(data_endpoints(self.config, operator)))
        (bundle / "compose.json").write_text(json.dumps(compose, indent=2))
        (bundle / "nginx.conf").write_text(render.nginx_config(render.validate(self.config)))
        (bundle / "init.sql").write_text(render.INIT_SQL)
        (bundle / "valkey.conf").write_text(render.valkey_conf(self.config.get("size", "small")))
        (bundle / "check_postgres.sql").write_text(render.CHECK_POSTGRES_SQL)
        (bundle / "prepare_postgres.sql").write_text(render.PREPARE_POSTGRES_SQL)
        shutil.copy(provision_sql(), bundle / "provision_runtime.sql")
        return bundle

    @staticmethod
    def resolve(hosts):
        """Addresses of existing data services, as /32 or /128 deny entries."""
        addresses = set()
        for host in hosts:
            try:
                for info in socket.getaddrinfo(host, None):
                    address = info[4][0]
                    addresses.add(address + ("/128" if ":" in address else "/32"))
            except OSError as error:
                raise AgentError(f"Cannot resolve the existing data service {host}: {error}") from error
        return sorted(addresses)

    def compose(self, bundle, *args):
        return self.run_command(["docker", "compose", "-f", str(bundle / "compose.json"), *args], cwd=bundle)

    def operator_secrets(self):
        return self.cloud.get_secret(self.references["operator"]) or {}

    def managed_data(self):
        """What the agent needs from managed data services: the database's
        administrator login and CA certificate, and the cache's AUTH token."""
        managed = {"postgres": self.managed_postgres()}
        if render.data_mode(self.config, "valkey") == "managed":
            managed["valkey"] = self.cloud.get_secret(self.config["data"]["valkey"]["endpoint"]["auth_secret"])
        return managed

    def managed_postgres(self):
        """A managed database's administrator login, from the secret the
        stack names, and the CA certificate its server presents."""
        if render.data_mode(self.config, "postgres") != "managed":
            return None
        endpoint = self.config["data"]["postgres"]["endpoint"]
        login = self.cloud.get_secret(endpoint["admin_secret"])
        if not isinstance(login, dict) or not login.get("username") or not login.get("password"):
            raise AgentError("The managed PostgreSQL administrator secret has no login yet")
        if self.config["cloud"] == "aws":
            status, data = http("GET", RDS_CA_BUNDLE.format(region=self.config["region"]), timeout=60)
            ca = data.decode() if status == 200 else ""
        else:
            ca = endpoint.get("ca_certificate", "")
        if "-----BEGIN CERTIFICATE-----" not in ca:
            raise AgentError("The managed PostgreSQL CA certificate is unavailable")
        return {"username": login["username"], "password": login["password"], "ca": ca}

    def deliver_secrets(self):
        generated = self.cloud.put_secret_if_absent(self.references["generated"], generate_secrets())
        write_secret_files(self.secrets_root,
                           role_secrets(self.config, generated, self.operator_secrets(), self.managed_data()),
                           chown=self.chown)

    def deploy(self, release, previous):
        """Deploy one release. Order matters: the old scheduler stops before
        anything else changes, migrations finish before services start, and
        the new scheduler starts last. Progress is saved after every step, so
        an interrupted deploy is resumed and a failed one is explained."""
        state = self.state()
        interrupted = state.get("deploying") or {}
        record = {"release": release, "previous": previous, "started": utc_now(), "steps": [],
                  "migrations_before": state.get("migrations")}
        if interrupted.get("release") == release:
            record["resumed_after"] = (interrupted.get("steps") or ["start"])[-1]
        # A retry keeps the first attempt's restore point: by now the
        # database may hold some of this release's migrations.
        for earlier in (interrupted, state.get("failed") or {}):
            if earlier.get("release") == release and earlier.get("restore_point"):
                record["restore_point"] = earlier["restore_point"]

        def step(name):
            record["steps"].append(name)
            self.save_state({**self.state(), "deploying": record})

        step("started")
        manifest = self.fetch_release(release)
        check_upgrade(manifest, previous, state.get("migrations"))
        record.update(images={name: image.rsplit("@", 1)[-1][:19] for name, image in manifest["images"].items()},
                      migrations=manifest["migrations"]["latest"],
                      configuration_schema=manifest.get("configuration_schema", 1), host=self.host_versions())
        step("verified")
        bundle = self.render_bundle(release, manifest, self.operator_secrets())
        self.deliver_secrets()
        step("secrets")
        self.compose(bundle, "--profile", "admin", "pull", "--quiet")
        step("pulled")
        self.compose(bundle, "stop", "scheduler")
        step("scheduler-stopped")
        local_data = [s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local"]
        if local_data:
            self.compose(bundle, "up", "-d", "--wait", *local_data)
        if render.data_mode(self.config, "postgres") == "managed":
            self.compose(bundle, "--profile", "admin", "run", "--rm", "prepare-postgres")
        if render.data_mode(self.config, "postgres") != "local":
            self.compose(bundle, "--profile", "admin", "run", "--rm", "check-postgres")
        step("data-ready")
        if state.get("migrations") is not None and record["migrations"] > state["migrations"]:
            # Hot never migrates down: undoing these migrations means a restore.
            if not record.get("restore_point"):
                record["restore_point"] = self.restore_point(bundle, release)
            step("restore-point")
        step("migrating")
        self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
        self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
        step("migrated")
        if self.config.get("admin_email"):
            self.compose(bundle, "--profile", "admin", "run", "--rm", "bootstrap-admin")
            step("admin")
        self.start_services(bundle)
        step("ready")
        record["finished"] = utc_now()
        self.save_state({"release": release, "previous": previous, "migrations": record["migrations"],
                         "dependencies": data_services(self.config, self.operator_secrets())})
        self.remember(record)
        return record["steps"]

    def recover(self, release, previous, error):
        """After a failed deploy: keep the previous release scheduling when
        the database was not touched, record the failure, and say what
        recovery means. The failed release is not retried until asked."""
        state = self.state()
        record = state.get("deploying") or {}
        if record.get("release") != release:
            record = {"release": release, "previous": previous, "steps": []}
        steps = record["steps"]
        if "migrating" in steps:
            point = record.get("restore_point")
            where = f" from {point['path']}" if point and point.get("path") else (
                f" to {point['time']}" if point else "")
            recovery = "Migrations may be partly applied, and Hot does not migrate down. Deploy a fixed release"
            recovery += f", or restore the database{where} and deploy {previous} again." if previous else "."
        else:
            if previous and "scheduler-stopped" in steps:
                with contextlib.suppress(AgentError, subprocess.CalledProcessError):
                    self.compose(self.releases / previous / "bundle", "up", "-d", "scheduler")
                    steps.append("previous-scheduler-restarted")
            recovery = (f"The database was not changed and {previous} keeps running. Fix the cause, then run "
                        "deploy-now, or point the installation at another release." if previous else
                        "Nothing was installed. Fix the cause, then run deploy-now.")
        record.update(error=describe(error), recovery=recovery, finished=utc_now())
        failed = {key: record[key] for key in ("release", "steps", "error", "recovery", "restore_point") if key in record}
        self.save_state({**{k: v for k, v in state.items() if k != "deploying"}, "failed": failed})
        self.remember(record)
        return failed

    def restore_point(self, bundle, release):
        """Where to recover the database from if this release's migrations
        must be undone: a dump of a local database, or the time to restore a
        managed or existing one to."""
        mode = render.data_mode(self.config, "postgres")
        if mode != "local":
            return {"kind": "point-in-time" if mode == "managed" else "operator", "time": utc_now()}
        target = self.dump_postgres(bundle, f"pre-{release}")
        dumps = sorted(self.backups.glob("pre-*"), key=lambda path: path.stat().st_mtime)
        for old in dumps[:-UPGRADE_DUMPS]:
            shutil.rmtree(old)
        return {"kind": "dump", "path": str(target / "postgres.dump"), "time": utc_now()}

    def dump_postgres(self, bundle, label):
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        target = self.backups / f"{label}-{stamp}"
        target.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.compose(bundle, "exec", "-T", "postgres", "pg_dump", "-U", "hot_owner", "-d", "hot",
                     "--format=custom", "--file=/var/lib/postgresql/backup.dump")
        shutil.move(str(self.data_root / "data/postgres/backup.dump"), str(target / "postgres.dump"))
        return target

    def host_versions(self):
        try:
            return json.loads((self.host / "versions.json").read_text())
        except (OSError, ValueError):
            return {}

    def history(self):
        try:
            return json.loads((self.releases / "history.json").read_text())
        except (OSError, ValueError):
            return []

    def remember(self, record):
        (self.releases / "history.json").write_text(json.dumps((self.history() + [record])[-HISTORY:], indent=1))

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
        self.releases.mkdir(parents=True, exist_ok=True)
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

    # -- monitoring ---------------------------------------------------------------

    def containers(self, bundle):
        """`docker compose ps` entries: a JSON array, or JSON lines from
        newer Compose releases."""
        output = self.compose(bundle, "ps", "--all", "--format", "json").stdout.strip()
        return json.loads(output) if output.startswith("[") else [json.loads(line) for line in output.splitlines()]

    def service_states(self, bundle):
        """Each service's container state and health check result."""
        return {e.get("Service", "?"): (e.get("State", ""), e.get("Health", "")) for e in self.containers(bundle)}

    def certificate_days_left(self, now=None):
        """Days until the ingress certificate expires, or None before it is
        delivered."""
        path = self.secrets_root / "ingress" / "TLS_CERTIFICATE"
        if not path.exists():
            return None
        output = self.run_command(["openssl", "x509", "-noout", "-enddate", "-in", str(path)]).stdout.strip()
        expires = datetime.datetime.strptime(output.split("=", 1)[1], "%b %d %H:%M:%S %Y %Z")
        now = now or datetime.datetime.now(datetime.timezone.utc)
        return round((expires.replace(tzinfo=datetime.timezone.utc) - now).total_seconds() / 86400, 1)

    def observe(self, probe=None, now=None):
        """The health metrics the stacks alarm on. Ones that need a deployed
        release are left out before the first deploy."""
        now = now or datetime.datetime.now(datetime.timezone.utc)
        metrics = {"AgentHeartbeat": (1, "Count")}
        release = self.state().get("release")
        if release:
            local_data = [s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local"]
            states = self.service_states(self.releases / release / "bundle")
            healthy = {name for name, (state, health) in states.items()
                       if state == "running" and health in ("", "healthy")}
            expected = [*HOT_SERVICES, "ingress", *local_data]
            metrics["UnhealthyServices"] = (sum(name not in healthy for name in expected), "Count")
            metrics["SchedulerRunning"] = (int("scheduler" in healthy), "Count")
            reachable = (all(name in healthy for name in local_data)
                         and all(tcp_reachable(host, port) for host, port in self.state().get("dependencies", [])))
            metrics["DependenciesReachable"] = (int(reachable), "Count")
            probe = probe or https_status
            targets = [(self.config["app_hostname"], "/signin"), (self.config["api_hostname"], "/status")]
            try:
                ready = all(probe(host, path) < 400 for host, path in targets)
            except OSError:
                ready = False
            metrics["Ready"] = (int(ready), "Count")
            try:
                days = self.certificate_days_left(now)
            except (ValueError, IndexError, subprocess.CalledProcessError):
                days = 0  # unreadable: alarm rather than pass
            if days is not None:
                metrics["CertificateDaysLeft"] = (days, "None")
        usage = os.statvfs(self.data_root)
        used = max(1 - usage.f_bavail / max(usage.f_blocks, 1), 1 - usage.f_favail / max(usage.f_files, 1))
        metrics["DataDiskUsedPercent"] = (round(100 * used, 1), "Percent")
        # Until the first snapshot, backup age counts from when monitoring began.
        baseline = self.releases / "monitoring.json"
        try:
            since = parse_time(json.loads(baseline.read_text())["since"])
        except (OSError, ValueError, KeyError):
            since = now
            baseline.parent.mkdir(parents=True, exist_ok=True)
            baseline.write_text(json.dumps({"since": now.isoformat()}))
        try:
            latest = self.cloud.latest_snapshot(self.config["data_volume_id"], self.config["name"]) or since
        except AgentError as error:
            # Left out, so the backup alarm reports the status as unknown.
            print(json.dumps({"backup_status_failed": str(error)}), file=sys.stderr, flush=True)
        else:
            metrics["BackupAgeHours"] = (round((now - latest).total_seconds() / 3600, 2), "None")
        return metrics

    def monitor(self):
        """Publish health metrics. Failures are logged, never raised: the
        missing heartbeat is what alarms."""
        try:
            self.cloud.put_metrics(self.config["name"], self.observe())
        except (AgentError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
            print(json.dumps({"monitoring_failed": describe(error)}), file=sys.stderr, flush=True)

    # -- reconcile and commands ------------------------------------------------

    def reconcile(self, request_id="", retry=False):
        """Deploy the release pointer if it changed. A release that failed
        waits for deploy-now (`retry`) or a new pointer; an interrupted one
        is resumed."""
        desired = self.cloud.get_parameter(self.config["release_pointer"])
        state = self.state()
        if not desired or desired == state.get("release"):
            return None
        if not retry and (state.get("failed") or {}).get("release") == desired:
            return None
        first_install = self.cloud.get_secret(self.references["generated"]) is None and not state.get("release")
        try:
            self.prepare_host(first_install)
            steps = self.deploy(desired, state.get("release"))
            return self.report(state="deployed", release=desired, steps=steps, request_id=request_id)
        except (AgentError, ValueError, subprocess.CalledProcessError) as error:
            failed = self.recover(desired, state.get("release"), error)
            return self.report(state="failed", release=desired, error=describe(error), request_id=request_id,
                               steps=failed["steps"], recovery=failed["recovery"])

    def command(self, operation, service="", request_id=""):
        if operation not in COMMANDS:
            raise AgentError(f"Unknown command {operation!r}")
        service = "" if service == "none" else service  # the SSM document's default
        release = self.state().get("release")
        bundle = self.releases / release / "bundle" if release else None
        if operation == "deploy-now":
            return self.reconcile(request_id, retry=True) or self.report(state="current", request_id=request_id)
        if operation == "status":
            return self.report(state="status", request_id=request_id)
        if operation == "diagnostics":
            return self.report(state="diagnostics", request_id=request_id, diagnostics=self.diagnostics(bundle))
        if bundle is None:
            raise AgentError("Nothing is deployed yet")
        if operation == "logs":
            local_data = tuple(s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local")
            if service not in HOT_SERVICES + ("ingress",) + local_data:
                raise AgentError(f"Unknown service {service!r}")
            output = self.compose(bundle, "logs", "--no-color", "--timestamps", "--tail", str(LOG_LINES), service).stdout
            lines = bounded_lines(output, Redactor.from_files(self.secrets_root))
            return self.report(state="logs", service=service, request_id=request_id, logs=lines)
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
            self.stop_services(bundle)
            return self.report(state="stopped", request_id=request_id)
        if operation == "roll-back":
            return self.report(state="rolled-back", request_id=request_id, **self.roll_back(bundle))
        if operation == "backup":
            # An extra logical dump beside the scheduled data-disk snapshots.
            # A managed database is backed up by its cloud service, an existing
            # one by its operator.
            if render.data_mode(self.config, "postgres") != "local":
                raise AgentError("The database is not on this host; back it up with its provider")
            target = self.dump_postgres(bundle, "manual")
            return self.report(state="backed-up", backup=str(target / "postgres.dump"), request_id=request_id)
        raise AgentError(f"Unhandled command {operation!r}")

    def stop_services(self, bundle):
        """Stop Hot in order: the scheduler, then workers with time to
        finish, then the app, API, and ingress."""
        self.compose(bundle, "stop", "scheduler")
        self.compose(bundle, "stop", "--timeout", "120", "worker", "task-worker")
        self.compose(bundle, "stop", "app", "api", "ingress")

    def start_services(self, bundle):
        self.compose(bundle, "up", "-d", "--remove-orphans", "app", "api", "worker", "task-worker", "ingress")
        self.wait_ready()
        self.compose(bundle, "up", "-d", "scheduler")

    def roll_back(self, bundle):
        """Undo a failed upgrade: restore a local database from the dump
        taken before the release's migrations, then run the installed
        release again. The failed release stays failed, so it is not
        retried; point the installation back at the installed release."""
        state = self.state()
        failed = state.get("failed") or {}
        if not failed:
            raise AgentError("Nothing to roll back: the last deploy did not fail")
        steps = []
        if "migrating" in failed.get("steps", []):
            point = failed.get("restore_point") or {}
            if point.get("kind") != "dump":
                when = f" to {point['time']}" if point.get("time") else ""
                raise AgentError(f"The database is not on this host: restore it{when} with its provider's "
                                 f"point-in-time recovery, then deploy {state.get('release')}")
            dump = Path(point["path"])
            if not dump.exists():
                raise AgentError(f"The restore point {dump} is missing")
            self.stop_services(bundle)
            steps.append("stopped")
            staged = self.data_root / "data/postgres/restore.dump"
            shutil.copy(dump, staged)
            try:
                self.compose(bundle, "up", "-d", "--wait", "postgres")
                self.compose(bundle, "exec", "-T", "postgres", "pg_restore", "-U", "hot_owner", "-d", "postgres",
                             "--clean", "--if-exists", "--create", "--exit-on-error",
                             "/var/lib/postgresql/restore.dump")
            finally:
                staged.unlink(missing_ok=True)
            steps.append("restored")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
            steps.append("checked")
        self.start_services(bundle)
        steps.append("started")
        failed = {**failed, "rolled_back": utc_now()}
        self.save_state({**state, "failed": failed})
        self.remember({"release": state.get("release"), "rolled_back_from": failed["release"],
                       "restore_point": failed.get("restore_point"), "steps": steps, "finished": utc_now()})
        return {"release": state.get("release"), "rolled_back_from": failed["release"], "steps": steps}

    def diagnostics(self, bundle, probe=None):
        """What support needs to understand an installation, redacted:
        versions, effective configuration, services, dependencies, health
        metrics, and the certificate. Logs are a separate command."""
        redact = Redactor.from_files(self.secrets_root)
        state = self.state()
        result = {
            "versions": {"release": state.get("release"), "previous": state.get("previous"),
                         "migrations": state.get("migrations"), "host": self.host_versions()},
            "deploys": self.history()[-5:],
            "configuration": effective_configuration(self.config, redact),
            "dependencies": [{"service": service, "mode": render.data_mode(self.config, service)}
                             for service in ("postgres", "valkey")],
        }
        for entry, (host_name, port) in zip(
                [d for d in result["dependencies"] if d["mode"] != "local"], state.get("dependencies", [])):
            entry.update(endpoint=f"{host_name}:{port}", reachable=tcp_reachable(host_name, port))
        if bundle is not None:
            result["services"] = {
                e.get("Service", "?"): {"state": e.get("State", ""), "health": e.get("Health", ""),
                                        "status": e.get("Status", ""), "exit_code": e.get("ExitCode", 0),
                                        "image": e.get("Image", "").rsplit("@", 1)[-1][:19]}
                for e in self.containers(bundle)}
            compose = json.loads((bundle / "compose.json").read_text())
            result["environment"] = {name: dict(sorted(compose["services"][name].get("environment", {}).items()))
                                     for name in HOT_SERVICES if name in compose["services"]}
        try:
            metrics = self.observe(probe)
        except (AgentError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
            result["health_error"] = describe(error)
        else:
            result["health"] = {name: value for name, (value, _) in metrics.items()}
        return redact.tree(result)


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
    connection = HTTPSConnection(host, 443, timeout=5, context=context)
    connection.sock = context.wrap_socket(socket.create_connection((address, 443), timeout=5),
                                          server_hostname=host)
    try:
        connection.request("GET", path, headers={"Host": host})
        return connection.getresponse().status
    finally:
        connection.close()


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def version_key(version):
    """Orders release versions; a prerelease sorts before its release."""
    core, _, pre = version.partition("-")
    return tuple(int(part) for part in core.split(".")), pre == "", pre


def check_upgrade(manifest, installed, migrations):
    """The upgrade rules every deploy enforces. Hot never migrates a
    database down, so a release older than the installed schema needs a
    restore, and a release names the oldest release it upgrades from."""
    latest = (manifest.get("migrations") or {}).get("latest")
    if not isinstance(latest, int):
        raise AgentError(f"The manifest for {manifest.get('release')} names no migration level")
    if migrations is not None and latest < migrations:
        raise AgentError(f"Release {manifest['release']} knows migrations up to {latest}, but the database has "
                         f"{migrations}; Hot does not migrate down. Deploy a newer release, or restore the "
                         "database from before the upgrade")
    minimum = (manifest.get("upgrade") or {}).get("minimum_source")
    if installed and minimum and version_key(installed) < version_key(minimum):
        raise AgentError(f"Release {manifest['release']} upgrades from {minimum} or later, and this installation "
                         f"runs {installed}; deploy a release in between first")


def parse_time(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


def tcp_reachable(host, port, timeout=3):
    try:
        socket.create_connection((host, int(port)), timeout=timeout).close()
        return True
    except OSError:
        return False


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
        return Aws(config["region"], f"/hot/{config['name']}")
    return Gcp(config["project"], config.get("command_subscription", ""), config["zone"])


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
            agent.monitor()
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
