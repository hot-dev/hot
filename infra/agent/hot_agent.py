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
import threading
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
COMMANDS = {"deploy-now", "status", "diagnostics", "logs", "backup", "restart-service", "drain-and-stop", "roll-back",
            "test-alert"}
# After a test alert, how long the agent reports AlertTest = 0, which
# resolves the alert on clouds that keep an incident open without data.
ALERT_TEST_RESOLVE_SECONDS = (120, 900)
HOT_SERVICES = ("app", "api", "worker", "task-worker", "scheduler")
# Service logs: the newest lines, each shortened, within a size that fits
# one log entry on either cloud.
LOG_LINES = 200
LOG_LINE_CHARS = 1000
LOG_CHARS = 48000
# Reports also go to this stream of the installation's CloudWatch log group.
STATUS_STREAM = "hot-agent-status"
# The host journal goes to this stream (AWS) or log (GCP): every agent line,
# and warnings and errors from the rest of the host, such as the kernel's OOM
# kills, Docker, and failed units. Each cycle ships at most JOURNAL_BATCH
# entries per source and continues from a saved cursor.
JOURNAL_STREAM = "host-journal"
JOURNAL_SOURCES = {"agent": ["-u", "hot-agent.service"], "host": ["--priority", "warning"]}
JOURNAL_BATCH = 500
# Report fields too large for the AWS status parameter (4 KB); the log has them.
DETAIL_KEYS = ("diagnostics", "logs")
# Box storage per size label; the data disk is sized to hold it.
BOX_POOL_GB = {"small": 40, "medium": 100, "large": 200}
# Container users that read their secret files.
SECRET_OWNERS = {"postgres": 999, "valkey": 999}
# The user the bundle's one-off jobs run as (see the renderer).
JOB_USER = 10001
# Deploy records kept in releases/history.json, and pre-upgrade dumps kept.
HISTORY = 20
UPGRADE_DUMPS = 3
# How long queued work may take to drain before a data move gives up.
DRAIN_SECONDS = 900


# Health metrics the agent publishes every cycle, with their CloudWatch units.
# The stacks alarm on them; see infra/README.md.
METRIC_NAMESPACE = "Hot/SelfHost"
GCP_METRIC_PREFIX = "custom.googleapis.com/hot/"
GCP_METRIC_NAMES = {"AgentHeartbeat": "agent_heartbeat", "UnhealthyServices": "unhealthy_services",
                    "SchedulerRunning": "scheduler_running", "DependenciesReachable": "dependencies_reachable",
                    "Ready": "ready", "DataDiskUsedPercent": "data_disk_used_percent",
                    "BackupAgeHours": "backup_age_hours", "CertificateDaysLeft": "certificate_days_left",
                    "QueueBacklog": "queue_backlog", "QueueDeadLetters": "queue_dead_letters",
                    "NewDeadLetters": "new_dead_letters", "JobsWaiting": "jobs_waiting",
                    "JobWaitSeconds": "job_wait_seconds", "TasksWaiting": "tasks_waiting",
                    "TaskWaitSeconds": "task_wait_seconds", "TaskSlotsUsedPercent": "task_slots_used_percent",
                    "AlertTest": "alert_test", "SyntheticOk": "synthetic_ok", "SyntheticSeconds": "synthetic_seconds",
                    "MemoryUsedPercent": "memory_used_percent"}
# Installation-wide measurements of Hot's queue, published by the one host
# that runs the scheduler. On GCP they are written for the project rather
# than an instance, so autoscalers can use them as per-group metrics.
QUEUE_METRICS = {"QueueBacklog", "QueueDeadLetters", "NewDeadLetters", "JobsWaiting", "JobWaitSeconds",
                 "TasksWaiting", "TaskWaitSeconds"}
# How often that host measures the queue: hot queue status starts a
# process, so not every cycle.
QUEUE_MEASURE_SECONDS = 60
TASK_QUEUE = "{hot:task}"
# The task worker rewrites its status file every 15 seconds; an older one
# is a stopped worker's.
TASK_STATUS_STALE_SECONDS = 120
# The synthetic check (monitoring.synthetic_event): how often API hosts run
# it, how long its run may take, and the operator secret key holding its API
# key.
SYNTHETIC_SECONDS = 300
SYNTHETIC_TIMEOUT = 60
SYNTHETIC_KEY = "HOT_SYNTHETIC_API_KEY"


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

    def registry_password(self, host):
        """An ECR password for Docker, from the instance role."""
        region = host.split(".")[3]
        found = Aws(region).call("ecr", "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken", {},
                                 endpoint="api.ecr")
        return base64.b64decode(found["authorizationData"][0]["authorizationToken"]).decode().split(":", 1)[1]

    def instance_name(self):
        """The EC2 instance ID, which the Auto Scaling group lists."""
        _, imds_token = http("PUT", self.IMDS + "/latest/api/token",
                             headers={"X-aws-ec2-metadata-token-ttl-seconds": "300"})
        _, data = http("GET", self.IMDS + "/latest/meta-data/instance-id",
                       headers={"X-aws-ec2-metadata-token": imds_token.decode()})
        return data.decode()

    def credentials(self):
        _, imds_token = http("PUT", self.IMDS + "/latest/api/token",
                             headers={"X-aws-ec2-metadata-token-ttl-seconds": "300"})
        auth = {"X-aws-ec2-metadata-token": imds_token.decode()}
        _, role = http("GET", self.IMDS + "/latest/meta-data/iam/security-credentials/", headers=auth)
        _, creds = http("GET", self.IMDS + "/latest/meta-data/iam/security-credentials/" + role.decode().strip(),
                        headers=auth)
        return json.loads(creds)

    def call(self, service, target, payload, endpoint=""):
        host = f"{endpoint or service}.{self.region}.amazonaws.com"
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

    def group_size(self, group):
        """The Auto Scaling group's desired capacity."""
        root = self.query("autoscaling", "2011-01-01", {
            "Action": "DescribeAutoScalingGroups", "AutoScalingGroupNames.member.1": group})
        sizes = [e.text for e in root.iter() if e.tag.endswith("DesiredCapacity") and e.text]
        if not sizes:
            raise AgentError(f"Auto Scaling group {group} was not found")
        return int(sizes[0])

    def leave_group(self, group):
        """Terminate this instance, shrinking its Auto Scaling group by one."""
        self.query("autoscaling", "2011-01-01", {
            "Action": "TerminateInstanceInAutoScalingGroup", "InstanceId": self.instance_name(),
            "ShouldDecrementDesiredCapacity": "true"})

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
        self.put_log_events(STATUS_STREAM, [(int(time.time() * 1000), json.dumps(status))])

    def put_log_events(self, stream, events):
        """(milliseconds, message) pairs, oldest first, to a stream of the
        installation's log group, created on first use. Split into batches
        CloudWatch accepts: under 1 MB, and spanning under a day."""
        target = {"logGroupName": self.log_group, "logStreamName": stream}
        for chunk in log_batches(events, max_bytes=900_000, max_span_ms=23 * 3600 * 1000):
            batch = {**target, "logEvents": [{"timestamp": at, "message": message} for at, message in chunk]}
            try:
                self.call("logs", "Logs_20140328.PutLogEvents", batch)
            except AgentError as error:
                if "ResourceNotFoundException" not in str(error):
                    raise
                try:
                    self.call("logs", "Logs_20140328.CreateLogStream", target)
                except AgentError as created:
                    if "ResourceAlreadyExistsException" not in str(created):
                        raise
                self.call("logs", "Logs_20140328.PutLogEvents", batch)

    def ship_journal(self, installation, entries):
        self.put_log_events(JOURNAL_STREAM, [(entry["at_ms"], json.dumps(entry["fields"])) for entry in entries])

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

    def registry_password(self, host):
        """An access token, which Artifact Registry accepts as a password."""
        return self.token()

    def instance_name(self):
        """The instance name, which the managed instance group lists."""
        _, data = http("GET", self.METADATA + "/instance/name", headers={"Metadata-Flavor": "Google"})
        return data.decode()

    def instance(self):
        if self.instance_id is None:
            _, data = http("GET", self.METADATA + "/instance/id", headers={"Metadata-Flavor": "Google"})
            self.instance_id = data.decode()
        return {"type": "gce_instance", "labels": {"project_id": self.project, "zone": self.zone,
                                                    "instance_id": self.instance_id}}

    def ship_journal(self, installation, entries):
        severities = {0: "EMERGENCY", 1: "ALERT", 2: "CRITICAL", 3: "ERROR", 4: "WARNING", 5: "NOTICE", 6: "INFO", 7: "DEBUG"}
        written = [{"jsonPayload": entry["fields"], "severity": severities.get(entry["priority"], "DEFAULT"),
                    "timestamp": datetime.datetime.fromtimestamp(entry["at_ms"] / 1000, datetime.timezone.utc).isoformat()}
                   for entry in entries]
        sized = [(0, json.dumps(entry)) for entry in written]
        for chunk in log_batches(sized, max_bytes=5_000_000):
            # Entries too old for Cloud Logging are dropped, not the batch.
            self.call("POST", "https://logging.googleapis.com/v2/entries:write", {
                "logName": f"projects/{self.project}/logs/hot-{installation}-host", "resource": self.instance(),
                "partialSuccess": True, "entries": [json.loads(text) for _, text in chunk]})

    def put_metrics(self, installation, metrics):
        instance = self.instance()
        project = {"type": "global", "labels": {"project_id": self.project}}
        now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        # Per-installation metric types: descriptors are project-wide, and
        # each installation's stack owns its own.
        series = [{"metric": {"type": f"{GCP_METRIC_PREFIX}{installation}/{GCP_METRIC_NAMES[name]}"},
                   "resource": project if name in QUEUE_METRICS else instance,
                   "points": [{"interval": {"endTime": now}, "value": {"doubleValue": float(value)}}]}
                  for name, (value, _) in sorted(metrics.items())]
        self.call("POST", f"https://monitoring.googleapis.com/v3/projects/{self.project}/timeSeries", {"timeSeries": series})

    def group_url(self, group):
        """A regional managed instance group in the installation's region."""
        region = self.zone.rsplit("-", 1)[0]
        return f"https://compute.googleapis.com/compute/v1/projects/{self.project}/regions/{region}/instanceGroupManagers/{group}"

    def group_size(self, group):
        """The managed instance group's target size."""
        return int(self.call("GET", self.group_url(group))["targetSize"])

    def leave_group(self, group):
        """Delete this instance from its group, which shrinks by one."""
        _, zone = http("GET", self.METADATA + "/instance/zone", headers={"Metadata-Flavor": "Google"})
        instance = f"zones/{zone.decode().rsplit('/', 1)[-1]}/instances/{self.instance_name()}"
        self.call("POST", self.group_url(group) + "/deleteInstances", {"instances": [instance]})

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
    elif render.data_mode(config, "valkey") == "managed" and config["cloud"] == "gcp":
        # Memorystore: Hot authenticates with the VM's IAM tokens.
        endpoint = config["data"]["valkey"]["endpoint"]
        redis_uri = f"rediss://{endpoint['host']}:{int(endpoint['port'])}"
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
    valkey_ca = ({"HOT_REDIS_CA_CERT": config["data"]["valkey"]["endpoint"]["ca_certificate"]}
                 if render.data_mode(config, "valkey") == "managed" and config["cloud"] == "gcp" else {})
    files = {}
    for role in HOT_SERVICES:
        files[role] = {"HOT_DB_URI": runtime_uri, "HOT_REDIS_URI": redis_uri, **ca, **valkey_ca}
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
        self.meminfo = Path(roots.get("meminfo", "/proc/meminfo"))
        self.backups = self.data_root / "backups"
        self.references = config["secret_references"]

    @staticmethod
    def _run(command, **kwargs):
        return subprocess.run(command, check=True, text=True, capture_output=True, **kwargs)

    # -- host preparation ---------------------------------------------------

    def data_device(self):
        if self.config.get("data_disk") == "scratch" and self.config["cloud"] == "aws":
            return "auto"  # an Auto Scaling group's volume IDs are not known in advance
        if self.config["cloud"] == "aws":
            return "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_" + self.config["data_volume_id"].replace("-", "")
        return "/dev/disk/by-id/google-hot-data"

    def prepare_host(self, first_install):
        """Mount the data disk and write host settings. A blank disk is
        formatted only for a brand-new installation."""
        command = [str(LIBEXEC / "hot-data-disk"), self.data_device()]
        # A scalable host's disk is scratch: it holds nothing that outlives
        # the instance, so a blank one is always formatted.
        if first_install or self.config.get("data_disk") == "scratch":
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
                                extra_deny=self.resolve(data_endpoints(self.config, operator)),
                                instance=self.instance_name())
        (bundle / "compose.json").write_text(json.dumps(compose, indent=2))
        (bundle / "nginx.conf").write_text(render.nginx_config(render.validate(self.config)))
        (bundle / "init.sql").write_text(render.INIT_SQL)
        (bundle / "valkey.conf").write_text(render.valkey_conf(self.config.get("size", "small")))
        (bundle / "check_postgres.sql").write_text(render.CHECK_POSTGRES_SQL)
        (bundle / "prepare_postgres.sql").write_text(render.PREPARE_POSTGRES_SQL)
        (bundle / "restore_postgres.sh").write_text(render.RESTORE_POSTGRES_SH)
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

    def registry_login(self):
        """Sign Docker in to a private mirror (image_registry) with the VM's
        identity: ECR on AWS, Artifact Registry on GCP (the contract refuses
        the other cloud's). Other mirrors are pulled anonymously."""
        registry = self.config.get("image_registry", "")
        host = registry.split("/", 1)[0]
        if not host:
            return
        cloud = self.config["cloud"]
        if cloud == "aws" and re.fullmatch(r"[0-9]{12}\.dkr\.ecr\.[a-z0-9-]+\.amazonaws\.com", host):
            user, password = "AWS", self.cloud.registry_password(host)
        elif cloud == "gcp" and host.endswith("-docker.pkg.dev"):
            user, password = "oauth2accesstoken", self.cloud.registry_password(host)
        else:
            return
        self.run_command(["docker", "login", "--username", user, "--password-stdin", f"https://{host}"], input=password)

    def compose(self, bundle, *args):
        return self.run_command(["docker", "compose", "-f", str(bundle / "compose.json"), *args], cwd=bundle)

    def operator_secrets(self):
        return self.cloud.get_secret(self.references["operator"]) or {}

    def managed_data(self):
        """What the agent needs from managed data services: the database's
        administrator login and CA certificate, and the cache's AUTH token."""
        managed = {"postgres": self.managed_postgres()}
        if render.data_mode(self.config, "valkey") == "managed" and self.config["cloud"] == "aws":
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
        files = role_secrets(self.config, generated, self.operator_secrets(), self.managed_data())
        # A host receives only what the services and jobs it runs read.
        present = {*self.roles(), "postgres", "valkey"} | ({"ingress"} if render.web_roles(self.config) else set())
        if render.runs_migrations(self.config):
            present |= {"migrate", "provision", "bootstrap-admin", "check-postgres", "prepare-postgres"}
        write_secret_files(self.secrets_root, {role: f for role, f in files.items() if role in present},
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
        moves = self.data_moves(state)
        manifest = self.fetch_release(release)
        check_upgrade(manifest, previous, state.get("migrations"))
        record.update(images={name: image.rsplit("@", 1)[-1][:19] for name, image in manifest["images"].items()},
                      migrations=manifest["migrations"]["latest"],
                      configuration_schema=manifest.get("configuration_schema", 1), host=self.host_versions())
        step("verified")
        # Storage moves run through the installed bundle, before it is
        # rendered again with the new storage.
        storage_moves = self.storage_moves(previous) if previous and not render.scalable(self.config) else {}
        if storage_moves:
            if release != previous:
                raise AgentError("Change storage in a deploy of the release already running, then upgrade")
            if moves:
                raise AgentError("Move storage and data services in separate deploys")
            self.move_storage(self.releases / previous / "bundle", storage_moves, record, step)
        bundle = self.render_bundle(release, manifest, self.operator_secrets())
        self.deliver_secrets()
        step("secrets")
        self.registry_login()
        self.compose(bundle, "--profile", "admin", "pull", "--quiet")
        step("pulled")
        if moves:
            # Hot stops completely, so nothing is written after the copy.
            record["moves"] = {service: f"{old} -> {new}" for service, (old, new) in moves.items()}
            step("draining")
            record["deadletter"] = self.drain(self.releases / previous / "bundle", queue="valkey" in moves)
            step("drained")
            if "postgres" in moves:
                dump = self.dump_postgres(self.releases / previous / "bundle", f"move-{release}") / "postgres.dump"
                record["restore_point"] = {"kind": "dump", "path": str(dump), "time": utc_now()}
                step("dumped")
        if "scheduler" in self.roles():
            self.compose(bundle, "stop", "scheduler")
        step("scheduler-stopped")
        if render.scalable(self.config):
            # A scalable installation migrated from its migration pointer
            # before any role rolled; see reconcile_migrations.
            step("data-ready")
        else:
            local_data = [s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local"]
            if local_data:
                self.compose(bundle, "up", "-d", "--wait", *local_data)
            if render.data_mode(self.config, "postgres") == "managed":
                self.compose(bundle, "--profile", "admin", "run", "--rm", "prepare-postgres")
            if render.data_mode(self.config, "postgres") != "local":
                self.compose(bundle, "--profile", "admin", "run", "--rm", "check-postgres")
            if "postgres" in moves:
                self.restore_moved_database(bundle, Path(record["restore_point"]["path"]))
                # From here the data lives in the new database: a later failure
                # is fixed forward, never by returning to the local copy.
                self.save_state({**self.state(), "data_modes": self.configured_modes()})
                step("data-moved")
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
        migrated = {k: v for k, v in self.state().items() if k == "migrated_release"}
        self.save_state({"release": release, "previous": previous, "migrations": record["migrations"],
                         "dependencies": data_services(self.config, self.operator_secrets()),
                         "data_modes": self.configured_modes(), "configuration": self.configuration_digest(),
                         **migrated})
        self.remember(record)
        return record["steps"]

    # -- data moves --------------------------------------------------------------

    def configuration_digest(self):
        return hashlib.sha256(json.dumps(self.config, sort_keys=True).encode()).hexdigest()[:16]

    def configured_modes(self):
        return {service: render.data_mode(self.config, service) for service in ("postgres", "valkey")}

    def data_moves(self, state):
        """Data services whose mode changed since the last deploy, as
        {service: (old, new)}. PostgreSQL moves from the host to a managed
        or existing database; Valkey may move anywhere, since its work is
        drained rather than copied. Other moves are refused before anything
        stops."""
        if not state.get("release"):
            return {}
        recorded = state.get("data_modes") or {
            # Deployed before modes were recorded: what the data disk holds.
            service: "local" if any((self.data_root / "data" / service).glob("*")) else render.data_mode(self.config, service)
            for service in ("postgres", "valkey")}
        moves = {service: (recorded[service], mode) for service, mode in self.configured_modes().items()
                 if recorded.get(service, mode) != mode}
        old_postgres = moves.get("postgres", ("local",))[0]
        if old_postgres != "local":
            raise AgentError(f"Moving PostgreSQL from {old_postgres} to {moves['postgres'][1]} is not automated: "
                             "copy the database with pg_dump and pg_restore, then change data.postgres.mode")
        return moves

    def deployed_storage(self, bundle):
        """Each storage role as a deployed bundle configured it."""
        services = json.loads((bundle / "compose.json").read_text())["services"]
        env = next(services[role]["environment"] for role in render.SERVICES if role in services)
        storage = {}
        for role in ("build", "file"):
            name = "HOT_" + role.upper()
            backend = env.get(name + "_STORAGE_TYPE", "local")
            storage[role] = {"backend": backend,
                             "bucket": env.get(name + "_AWS_S3_BUCKET") or env.get(name + "_GCS_BUCKET") or "",
                             "prefix": env.get(name + "_AWS_S3_PREFIX") or env.get(name + "_GCS_PREFIX") or "",
                             "region": env.get(name + "_AWS_S3_REGION", "")}
        return storage

    def configured_storage(self):
        storage = {}
        for role in ("build", "file"):
            settings = self.config["storage"][role]
            backend = settings["backend"]
            storage[role] = {"backend": backend,
                             "bucket": settings.get("bucket", "") if backend != "local" else "",
                             "prefix": settings.get("prefix", role + "s") if backend != "local" else "",
                             "region": settings.get("region", "") if backend == "s3" else ""}
        return storage

    def storage_moves(self, previous):
        """Storage roles whose backend changed since the installed bundle, as
        {role: target}. Moves into S3 or GCS are automated; moving back onto
        the host, or between buckets of one backend, is refused before
        anything stops."""
        bundle = self.releases / previous / "bundle"
        if not (bundle / "compose.json").exists():
            return {}
        deployed, configured = self.deployed_storage(bundle), self.configured_storage()
        moves = {role: configured[role] for role in configured if configured[role] != deployed[role]}
        for role, target in moves.items():
            if target["backend"] == "local":
                raise AgentError(f"Moving {role} storage back onto the host is not automated; copy the objects "
                                 "and run hot db move-storage yourself")
            if target["backend"] == deployed[role]["backend"]:
                raise AgentError(f"Moving {role} storage to another {target['backend']} bucket or prefix is not "
                                 "automated: records keep their bucket. Copy the objects with your provider's tools")
        return moves

    def move_storage(self, bundle, moves, record, step):
        """Stop Hot, then move each role's objects with hot db move-storage in
        the installed release's worker, which still has the source settings.
        Each object's record changes only after its copy checks out, so the
        move resumes where it stopped; the manifests record what moved."""
        record["storage_moves"] = {role: target["backend"] for role, target in moves.items()}
        step("storage-moving")
        self.drain(bundle, queue=False)
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.backups.mkdir(parents=True, exist_ok=True)
        for role, target in moves.items():
            command = ["run", "--rm", "--no-deps", "worker", "db", "move-storage", "--role", role,
                       "--to", target["backend"], "--bucket", target["bucket"], "--prefix", target["prefix"],
                       "--manifest", "-"]
            if target["region"]:
                command += ["--region", target["region"]]
            moved = self.compose(bundle, *command)
            manifest = self.backups / f"storage-move-{role}-{stamp}.jsonl"
            manifest.write_text(moved.stdout)
            record.setdefault("storage_manifests", {})[role] = str(manifest)
        step("storage-moved")

    def drain(self, bundle, queue, timeout=DRAIN_SECONDS):
        """Stop admissions (scheduler, app, API, ingress), let workers finish
        queued work when the queue is moving, then stop the workers. Returns
        the dead-lettered items left behind in the old queue."""
        self.compose(bundle, "stop", "scheduler")
        self.compose(bundle, "stop", "app", "api", "ingress")
        deadletter = 0
        if queue:
            waited = 0
            while True:
                backlog, deadletter = self.queue_backlog(bundle)
                if backlog == 0:
                    break
                if waited >= timeout:
                    raise AgentError(f"{backlog} queued items did not finish within {timeout // 60} minutes; "
                                     "the data move was not started")
                self.sleep(10)
                waited += 10
        self.compose(bundle, "stop", "--timeout", "120", "worker", "task-worker")
        return deadletter

    def queue_status(self, bundle, service="worker"):
        """Hot's queue from `hot queue status` in a running service: per
        queue, what waits (pending), what runs (processing), what was
        dead-lettered, and how long the oldest waiting item has waited."""
        command = ("exec", "-T", service, "/usr/bin/hot", "queue", "status")
        try:
            return json.loads(self.compose(bundle, *command, "--json").stdout)
        except (subprocess.CalledProcessError, ValueError):
            pass
        # Releases before --json: only the totals, from the text report.
        output = self.compose(bundle, *command).stdout
        counts = dict(re.findall(r"Total (pending|processing|deadletter):\s+(-?\d+)", output))
        if {"pending", "processing"} - set(counts):
            raise AgentError(f"Could not read the queue's status from the {service}")
        return {"queues": [], "total_pending": int(counts["pending"]),
                "total_processing": int(counts["processing"]), "total_deadletter": int(counts.get("deadletter", 0))}

    def queue_backlog(self, bundle):
        """(pending + in progress, dead-lettered) items in Hot's queue."""
        status = self.queue_status(bundle)
        return status["total_pending"] + status["total_processing"], status["total_deadletter"]

    def restore_moved_database(self, bundle, dump):
        """Restore the local database's dump into the new one, which must not
        hold Hot's tables yet."""
        staged = self.data_root / "data" / "moves"
        staged.mkdir(parents=True, exist_ok=True)
        # Readable only by the restore job's user: the dump is the database.
        copy = staged / "postgres.dump"
        copy.touch(mode=0o600)
        os.chmod(copy, 0o600)
        self.chown(copy, JOB_USER, JOB_USER)
        shutil.copyfile(dump, copy)
        try:
            self.compose(bundle, "--profile", "admin", "run", "--rm", "restore-postgres")
        finally:
            (staged / "postgres.dump").unlink(missing_ok=True)

    def recover(self, release, previous, error):
        """After a failed deploy: keep the previous release scheduling when
        the database was not touched, record the failure, and say what
        recovery means. The failed release is not retried until asked."""
        state = self.state()
        record = state.get("deploying") or {}
        if record.get("release") != release:
            record = {"release": release, "previous": previous, "steps": []}
        steps = record["steps"]
        if "storage-moving" in steps and "storage-moved" not in steps:
            recovery = ("Some objects may already be in the new storage, so Hot stays stopped. Fix the cause, then "
                        "run deploy-now: the move skips objects already moved.")
        elif "data-moved" in steps:
            recovery = ("The data now lives in the new database, and the copy on this host is no longer used"
                        + ("; migrations may be partly applied" if "migrating" in steps else "")
                        + ". Fix the cause, then run deploy-now.")
        elif "migrating" in steps:
            point = record.get("restore_point")
            where = f" from {point['path']}" if point and point.get("path") else (
                f" to {point['time']}" if point else "")
            recovery = "Migrations may be partly applied, and Hot does not migrate down. Deploy a fixed release"
            recovery += f", or restore the database{where} and deploy {previous} again." if previous else "."
        else:
            if previous and "draining" in steps:
                # A data move stopped all of Hot; the local data is untouched.
                with contextlib.suppress(AgentError, subprocess.CalledProcessError):
                    self.start_services(self.releases / previous / "bundle")
                    steps.append("previous-release-restarted")
            elif previous and "scheduler-stopped" in steps:
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

    def roles(self):
        return render.roles(self.config)

    def instance_name(self):
        """How the instance group names this host, for the release workflow."""
        if not getattr(self, "_instance_name", None):
            try:
                self._instance_name = self.cloud.instance_name()
            except (AgentError, OSError, AttributeError):
                return socket.gethostname()
        return self._instance_name

    def readiness_targets(self):
        """The hostnames and paths this host serves, checked through its own
        ingress: port 443 on a single machine, the load balancer's port on a
        scalable host."""
        paths = {"app": "/signin", "api": "/status"}
        return [(self.config[role + "_hostname"], paths[role]) for role in render.web_roles(self.config)]

    def probe(self, host, path):
        return https_status(host, path)

    def wait_ready(self, attempts=40, probe=None):
        """Check the app and API through the ingress from the host itself, so
        private installations are checked too."""
        probe = probe or self.probe
        targets = self.readiness_targets()
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
        # Deploy history, never secret values; still only root's to read.
        self.releases.mkdir(parents=True, exist_ok=True)
        path = self.state_path()
        path.touch(mode=0o600)
        os.chmod(path, 0o600)
        path.write_text(json.dumps(state))

    def report(self, **status):
        status = {"installation": self.config["name"], "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  **self.state(), **status}
        if render.scalable(self.config):
            status.update(instance=self.instance_name(), roles=list(self.roles()))
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

    def queue_metrics(self, bundle, now):
        """Work waiting or in progress in Hot's queue, dead-lettered work, and
        how much of that is new since the last measurement; then, apart for
        workers' jobs and task workers' tasks, how many wait and how long the
        oldest has waited. Measured at most every QUEUE_MEASURE_SECONDS, for
        the queue alerts and for scaling workers."""
        last = getattr(self, "_queue_measured", None)
        if last and (now - last[0]).total_seconds() < QUEUE_MEASURE_SECONDS:
            return last[1]
        try:
            status = self.queue_status(bundle, "scheduler")
        except (AgentError, subprocess.CalledProcessError):
            return {}
        dead = status["total_deadletter"]
        added = max(0, dead - last[1]["QueueDeadLetters"][0]) if last else 0
        measured = {"QueueBacklog": (status["total_pending"] + status["total_processing"], "Count"),
                    "QueueDeadLetters": (dead, "Count"), "NewDeadLetters": (added, "Count")}
        if status["queues"]:
            for kind, queues in (("Job", [q for q in status["queues"] if q["name"] != TASK_QUEUE]),
                                 ("Task", [q for q in status["queues"] if q["name"] == TASK_QUEUE])):
                measured[kind + "sWaiting"] = (sum(q["pending"] for q in queues), "Count")
                measured[kind + "WaitSeconds"] = (max((q["oldest_pending_secs"] or 0 for q in queues), default=0),
                                                  "Seconds")
        self._queue_measured = (now, measured)
        return measured

    def task_slots_used(self, bundle, now):
        """How full the task worker is, in percent: the busiest of its code
        slots, container slots, and container memory, from the status file
        it rewrites. None when the file is missing or stale."""
        try:
            status = json.loads(self.compose(bundle, "exec", "-T", "task-worker", "cat", render.TASK_STATUS).stdout)
            if now.timestamp() - status["time"] > TASK_STATUS_STALE_SECONDS:
                return None
            shares = [pool["busy"] / pool["slots"] for pool in (status["code"], status["containers"]) if pool["slots"]]
            if status.get("memory_mb"):
                shares.append(status["memory_used_mb"] / status["memory_mb"])
        except (subprocess.CalledProcessError, ValueError, KeyError, TypeError):
            return None
        return round(100 * max(shares, default=0), 1)

    def synthetic_check(self, request=None, clock=time.monotonic):
        """Publish monitoring.synthetic_event through this host's ingress
        with the operator's synthetic API key, and wait for a run it starts
        to finish: (succeeded, seconds). The operator deploys a handler for
        the event; the check covers the API, the queue, a worker, and the
        database together."""
        request = request or https_request
        key = self.operator_secrets().get(SYNTHETIC_KEY, "")
        if not key:
            raise AgentError(f"The operator secret has no {SYNTHETIC_KEY} for the synthetic check")
        host = self.config["api_hostname"]
        headers = {"Authorization": "Bearer " + key, "Content-Type": "application/json"}
        started = clock()
        event = {"event_type": self.config["monitoring"]["synthetic_event"], "event_data": {"synthetic": True}}
        status, body = request(host, "POST", "/v1/events", json.dumps(event).encode(), headers)
        if status >= 300:
            raise AgentError(f"Publishing the synthetic event answered HTTP {status}")
        event_id = json.loads(body)["data"]["event_id"]
        while clock() - started < SYNTHETIC_TIMEOUT:
            status, body = request(host, "GET", f"/v1/events/{event_id}/runs", None, headers)
            states = {run.get("status") for run in (json.loads(body).get("data", []) if status == 200 else [])}
            if "succeeded" in states:
                return True, clock() - started
            if states & {"failed", "cancelled"}:
                return False, clock() - started
            self.sleep(2)
        return False, clock() - started

    def run_synthetic(self):
        try:
            self._synthetic_result = self.synthetic_check()
        except (AgentError, OSError, ValueError, KeyError, TypeError) as error:
            print(json.dumps({"synthetic_check_failed": describe(error)}), file=sys.stderr, flush=True)
            self._synthetic_result = (False, 0)

    def synthetic_metrics(self, now):
        """The latest synthetic check's result. On hosts that serve the API,
        a new check starts in the background every SYNTHETIC_SECONDS, so it
        never holds up the agent's loop."""
        if not (self.config.get("monitoring", {}).get("synthetic_event") and "api" in self.roles()):
            return {}
        started = getattr(self, "_synthetic_started", None)
        running = getattr(self, "_synthetic_thread", None)
        if (started is None or (now - started).total_seconds() >= SYNTHETIC_SECONDS) and not (running and running.is_alive()):
            self._synthetic_started = now
            self._synthetic_thread = self.background(self.run_synthetic)
        result = getattr(self, "_synthetic_result", None)
        if result is None:
            return {}
        return {"SyntheticOk": (int(result[0]), "Count"), "SyntheticSeconds": (round(result[1], 1), "Seconds")}

    @staticmethod
    def background(function):
        thread = threading.Thread(target=function, daemon=True)
        thread.start()
        return thread

    def retire_if_idle(self, metrics, now):
        """In a scalable installation, a task host that has run nothing for
        scaling.task_worker.retire_idle_minutes leaves its group, which
        shrinks by one. Groups only scale task hosts out, since they cannot
        tell which hosts are running tasks; this is how they shrink. The
        task worker stops first, so tasks claimed in the meantime go back
        to the queue. The group keeps its minimum, and each host waits up to
        five minutes longer than the others, by its name, so idle hosts do
        not all leave at once."""
        scaling = self.config.get("scaling", {}).get("task_worker", {})
        minutes = scaling.get("retire_idle_minutes", 0)
        if not (render.scalable(self.config) and "task-worker" in self.roles() and minutes > 0
                and scaling.get("max", 1) > scaling.get("min", 1)):
            return
        used = metrics.get("TaskSlotsUsedPercent", (None,))[0]
        if used is None or used > 0:
            self._idle_since = None
            return
        self._idle_since = getattr(self, "_idle_since", None) or now
        stagger = int(hashlib.sha256(self.instance_name().encode()).hexdigest(), 16) % 300
        if (now - self._idle_since).total_seconds() < minutes * 60 + stagger:
            return
        self._idle_since = now
        group = self.config["instance_group"]
        bundle = self.releases / self.state()["release"] / "bundle"
        try:
            if self.cloud.group_size(group) <= scaling["min"]:
                return
            self.compose(bundle, "stop", "--timeout", "120", "task-worker")
            try:
                self.report(state="retiring", reason=f"No tasks for {minutes} minutes; leaving {group}")
                self.cloud.leave_group(group)
            except (AgentError, OSError):
                self.compose(bundle, "up", "-d", "task-worker")
                raise
        except (AgentError, OSError, subprocess.CalledProcessError) as error:
            print(json.dumps({"retire_failed": describe(error)}), file=sys.stderr, flush=True)

    def observe(self, probe=None, now=None):
        """The health metrics the stacks alarm on. Ones that need a deployed
        release are left out before the first deploy."""
        now = now or datetime.datetime.now(datetime.timezone.utc)
        metrics = {"AgentHeartbeat": (1, "Count")}
        try:
            tested = parse_time((self.releases / "alert-test").read_text())
        except (OSError, ValueError):
            tested = None
        if tested and ALERT_TEST_RESOLVE_SECONDS[0] <= (now - tested).total_seconds() <= ALERT_TEST_RESOLVE_SECONDS[1]:
            metrics["AlertTest"] = (0, "Count")
        release = self.state().get("release")
        if release:
            local_data = [s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local"]
            states = self.service_states(self.releases / release / "bundle")
            healthy = {name for name, (state, health) in states.items()
                       if state == "running" and health in ("", "healthy")}
            expected = [*self.roles(), *(["ingress"] if render.web_roles(self.config) else []), *local_data]
            metrics["UnhealthyServices"] = (sum(name not in healthy for name in expected), "Count")
            if "scheduler" in self.roles():
                metrics["SchedulerRunning"] = (int("scheduler" in healthy), "Count")
            reachable = (all(name in healthy for name in local_data)
                         and all(tcp_reachable(host, port) for host, port in self.state().get("dependencies", [])))
            metrics["DependenciesReachable"] = (int(reachable), "Count")
            probe = probe or self.probe
            targets = self.readiness_targets()
            try:
                ready = all(probe(host, path) < 400 for host, path in targets)
            except OSError:
                ready = False
            if targets:
                metrics["Ready"] = (int(ready), "Count")
            try:
                days = self.certificate_days_left(now)
            except (ValueError, IndexError, subprocess.CalledProcessError):
                days = 0  # unreadable: alarm rather than pass
            if days is not None:
                metrics["CertificateDaysLeft"] = (days, "None")
            if "scheduler" in self.roles() and "scheduler" in healthy:
                metrics.update(self.queue_metrics(self.releases / release / "bundle", now))
            if "api" in healthy:
                metrics.update(self.synthetic_metrics(now))
            if "task-worker" in self.roles() and "task-worker" in healthy:
                used = self.task_slots_used(self.releases / release / "bundle", now)
                if used is not None:
                    metrics["TaskSlotsUsedPercent"] = (used, "Percent")
        usage = os.statvfs(self.data_root)
        used = max(1 - usage.f_bavail / max(usage.f_blocks, 1), 1 - usage.f_favail / max(usage.f_files, 1))
        metrics["DataDiskUsedPercent"] = (round(100 * used, 1), "Percent")
        memory = memory_used_percent(self.meminfo)
        if memory is not None:
            metrics["MemoryUsedPercent"] = (memory, "Percent")
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

    def monitor(self, now=None):
        """Publish health metrics, then retire this host if it is an idle
        task host. Failures are logged, never raised: the missing heartbeat
        is what alarms."""
        now = now or datetime.datetime.now(datetime.timezone.utc)
        try:
            metrics = self.observe(now=now)
            self.cloud.put_metrics(self.config["name"], metrics)
        except (AgentError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
            print(json.dumps({"monitoring_failed": describe(error)}), file=sys.stderr, flush=True)
            return
        self.retire_if_idle(metrics, now)

    # -- host journal -----------------------------------------------------------

    def journal_records(self, source, cursor):
        """Up to JOURNAL_BATCH records for one source after `cursor`, oldest
        first. Without a cursor, the newest 100."""
        command = ["journalctl", "--output=json", "--no-pager", *JOURNAL_SOURCES[source]]
        command += ["--after-cursor", cursor] if cursor else ["--lines", "100"]
        records = []
        for line in self.run_command(command).stdout.splitlines():
            if len(records) == JOURNAL_BATCH:
                break
            with contextlib.suppress(ValueError):
                records.append(json.loads(line))
        return records

    def ship_journal(self):
        """Ship new host journal entries to the installation's log. A source
        whose shipping fails is retried from the same cursor next cycle; a
        failure is logged and never stops the agent. With logs kept on the
        host (logging.destination = local), the journal stays there too."""
        if (self.config.get("logging") or {}).get("destination", "cloud") != "cloud":
            return
        path = self.releases / "journal.json"
        try:
            cursors = json.loads(path.read_text())
        except (OSError, ValueError):
            cursors = {}
        redact = Redactor.from_files(self.secrets_root)
        for source in JOURNAL_SOURCES:
            try:
                records = self.journal_records(source, cursors.get(source))
                if records:
                    self.cloud.ship_journal(self.config["name"], [journal_entry(r, redact, source) for r in records])
                    cursors[source] = records[-1]["__CURSOR"]
            except (AgentError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
                print(json.dumps({"journal_shipping_failed": describe(error), "source": source}),
                      file=sys.stderr, flush=True)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(cursors))

    # -- reconcile and commands ------------------------------------------------

    def reconcile_migrations(self, request_id="", retry=False):
        """On a scalable installation's migration host: migrate the database
        to the release the migration pointer names, without restarting
        anything. The release workflow sets this pointer and waits for the
        report before it rolls any role; a failure waits for deploy-now."""
        pointer = self.config.get("migration_pointer")
        if not pointer:
            return None
        desired = self.cloud.get_parameter(pointer)
        state = self.state()
        if not desired or desired == state.get("migrated_release"):
            return None
        if not retry and (state.get("migration_failed") or {}).get("release") == desired:
            return None
        try:
            manifest = self.fetch_release(desired)
            check_upgrade(manifest, state.get("migrated_release") or state.get("release"), state.get("migrations"))
            bundle = self.render_bundle(desired, manifest, self.operator_secrets())
            self.deliver_secrets()
            if render.data_mode(self.config, "postgres") == "managed":
                self.compose(bundle, "--profile", "admin", "run", "--rm", "prepare-postgres")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "check-postgres")
            restore_point = {"kind": "point-in-time" if render.data_mode(self.config, "postgres") == "managed"
                             else "operator", "time": utc_now()}
            self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
            if self.config.get("admin_email"):
                self.compose(bundle, "--profile", "admin", "run", "--rm", "bootstrap-admin")
        except (AgentError, ValueError, subprocess.CalledProcessError) as error:
            self.save_state({**self.state(), "migration_failed": {"release": desired, "error": describe(error)}})
            return self.report(state="migration-failed", release=desired, error=describe(error), request_id=request_id)
        latest = manifest["migrations"]["latest"]
        self.save_state({**{k: v for k, v in self.state().items() if k != "migration_failed"},
                         "migrated_release": desired, "migrations": max(latest, state.get("migrations") or 0)})
        self.remember({"release": desired, "migrated": latest, "restore_point": restore_point, "finished": utc_now()})
        return self.report(state="migrated", release=desired, restore_point=restore_point, request_id=request_id)

    def reconcile(self, request_id="", retry=False):
        """Deploy the release pointer if it changed. A release that failed
        waits for deploy-now (`retry`) or a new pointer; an interrupted one
        is resumed. A scalable migration host migrates first, from its own
        pointer."""
        migrated = self.reconcile_migrations(request_id, retry)
        desired = self.cloud.get_parameter(self.config["release_pointer"])
        state = self.state()
        if state.get("release") and "configuration" not in state:
            # Deployed before configurations were recorded: assume the one it runs.
            self.save_state({**state, "configuration": self.configuration_digest()})
            state = self.state()
        # A configuration change (settings, data modes, storage) redeploys the
        # same release, so the change takes effect.
        changed = state.get("configuration") not in (None, self.configuration_digest())
        if not desired or (desired == state.get("release") and not changed):
            return migrated
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
        if operation == "test-alert":
            # The alert-test alarm fires on this, then resolves: the whole
            # path from the agent to the operator's inbox, without touching
            # Hot.
            self.cloud.put_metrics(self.config["name"], {"AlertTest": (1, "Count")})
            # On AWS a command runs in its own process; the loop reads this.
            self.releases.mkdir(parents=True, exist_ok=True)
            (self.releases / "alert-test").write_text(utc_now())
            return self.report(state="alert-sent", request_id=request_id)
        if bundle is None:
            raise AgentError("Nothing is deployed yet")
        if operation == "logs":
            local_data = tuple(s for s in ("postgres", "valkey") if render.data_mode(self.config, s) == "local")
            if service not in self.roles() + (("ingress",) if render.web_roles(self.config) else ()) + local_data:
                raise AgentError(f"Unknown service {service!r}")
            output = self.compose(bundle, "logs", "--no-color", "--timestamps", "--tail", str(LOG_LINES), service).stdout
            lines = bounded_lines(output, Redactor.from_files(self.secrets_root))
            return self.report(state="logs", service=service, request_id=request_id, logs=lines)
        if operation == "restart-service":
            if service not in self.roles() + (("ingress",) if render.web_roles(self.config) else ()):
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
        finish, then the app, API, and ingress. A scalable host stops only
        its own roles."""
        roles = self.roles()
        if "scheduler" in roles:
            self.compose(bundle, "stop", "scheduler")
        workers = [r for r in ("worker", "task-worker") if r in roles]
        if workers:
            self.compose(bundle, "stop", "--timeout", "120", *workers)
        web = list(render.web_roles(self.config))
        if web:
            self.compose(bundle, "stop", *web, "ingress")

    def start_services(self, bundle):
        roles = self.roles()
        services = [r for r in roles if r != "scheduler"] + (["ingress"] if render.web_roles(self.config) else [])
        if services:
            self.compose(bundle, "up", "-d", "--remove-orphans", *services)
            self.wait_ready()
        if "scheduler" in roles:
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
        if "data-moved" in failed.get("steps", []):
            raise AgentError("The data already moved to the new database; rolling back would return to the copy "
                             "on this host from before the move. Fix the cause and run deploy-now instead")
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
                                     for name in self.roles() if name in compose["services"]}
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


def memory_used_percent(meminfo):
    """The share of the host's memory in use: all of it except what the
    kernel reports as available without swapping. None where unreadable."""
    try:
        fields = dict(line.split(":", 1) for line in meminfo.read_text().splitlines() if ":" in line)
        total, available = (int(fields[key].split()[0]) for key in ("MemTotal", "MemAvailable"))
    except (OSError, KeyError, ValueError):
        return None
    return round(100 * (1 - available / max(total, 1)), 1)


def https_request(host, method, path, body=None, headers=None, address="127.0.0.1", port=443, timeout=10):
    """A request through the local ingress, sending the hostname as SNI: the
    ingress refuses handshakes that name no configured host. Returns
    (status, body)."""
    context = ssl.create_default_context()
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # loopback only; certificates name the public hostnames
    connection = HTTPSConnection(host, port, timeout=timeout, context=context)
    connection.sock = context.wrap_socket(socket.create_connection((address, port), timeout=timeout),
                                          server_hostname=host)
    try:
        connection.request(method, path, body=body, headers={"Host": host, **(headers or {})})
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def https_status(host, path, address="127.0.0.1", port=443):
    """GET through the local ingress; the response's status."""
    return https_request(host, "GET", path, address=address, port=port, timeout=5)[0]


def log_batches(events, max_bytes, max_span_ms=None):
    """Split (milliseconds, message) pairs, oldest first, into consecutive
    batches under `max_bytes` that span less than `max_span_ms`."""
    batch, size = [], 0
    for at, message in events:
        length = len(message.encode()) + 26
        if batch and (size + length > max_bytes or (max_span_ms and at - batch[0][0] >= max_span_ms)):
            yield batch
            batch, size = [], 0
        batch.append((at, message))
        size += length
    if batch:
        yield batch


def journal_entry(record, redact, source):
    """One `journalctl --output=json` record as shipped: its source, unit,
    priority, and redacted message."""
    message = record.get("MESSAGE") or ""
    if isinstance(message, list):  # journald sends non-UTF-8 messages as bytes
        message = bytes(message).decode("utf-8", "replace")
    priority = int(record.get("PRIORITY", 6))
    fields = {"source": source, "unit": record.get("_SYSTEMD_UNIT") or record.get("SYSLOG_IDENTIFIER", ""),
              "priority": priority, "message": redact(str(message))}
    return {"at_ms": int(record["__REALTIME_TIMESTAMP"]) // 1000, "priority": priority, "fields": fields}


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
            agent.ship_journal()
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
