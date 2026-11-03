#!/usr/bin/env python3
"""Hot host deploy agent for single-machine installations.

Runs on the VM under systemd. It reads the installation's release pointer,
deploys the signed release it names, delivers secrets as tmpfs files, and
reports status. Operators and workflows reach it only through a fixed set of
commands (see COMMANDS); it never runs arbitrary shell.

    hot-agent run                         # reconcile to the release pointer, forever
    hot-agent command OP [--service S] [--request-id ID] [--expires SECONDS]

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
from http.client import HTTPException, HTTPSConnection
import ipaddress
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
# While a deploy, data move, or command holds the agent, a background thread
# publishes AgentHeartbeat this often, so the host-down alarm stays quiet. After
# the limit it stops, so one that hangs (Docker wedged in a deploy) alarms too.
HEARTBEAT_SECONDS = 60
HEARTBEAT_LIMIT_SECONDS = 6 * 3600
# Pub/Sub commands taken per cycle, one message per pull, and how many
# handled request IDs are remembered: delivery is at least once.
COMMANDS_PER_CYCLE = 5
COMMANDS_REMEMBERED = 200
# Report states that mean a command did not do what was asked.
FAILED_STATES = {"rejected", "failed", "migration-failed", "resume-failed", "expired"}
# A failed command's report ends with this much of its redacted output,
# small enough for the AWS status parameter.
ERROR_LINES = 3
ERROR_CHARS = 400
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
# The ClientRequestToken of the generated secret's first version on AWS:
# fixed, and 32 to 64 characters as Secrets Manager requires.
GENERATED_SECRET_TOKEN = "hot-generated-secrets-first-version-0001"
# Report fields too large for the AWS status parameter (4 KB); the log has them.
DETAIL_KEYS = ("diagnostics", "logs")
# Box storage per size label; the data disk is sized to hold it.
BOX_POOL_GB = {"small": 40, "medium": 100, "large": 200}
# Container users that read their secret files.
SECRET_OWNERS = {"postgres": 999, "valkey": 999}
# The user Hot's services and the bundle's one-off jobs run as (see the
# renderer).
JOB_USER = 10001
# Deploy records kept in releases/history.json, and pre-upgrade dumps kept;
# as many data-move dumps are kept, newest first.
HISTORY = 20
UPGRADE_DUMPS = 3
# How long queued work may take to drain before a data move gives up.
DRAIN_SECONDS = 900
# How long a deploy waits for a grant to take effect, and how far apart it tries.
# The GCP stacks tell the VM about a bucket or a mirror only once the host's grant
# exists, but Google's IAM can take minutes to apply it.
GRANT_WAIT_SECONDS = 600
GRANT_RETRY_SECONDS = 15
# What Docker's errors say, in a registry's own words, when it refuses this identity.
REGISTRY_REFUSED = re.compile(r"denied|unauthorized|forbidden", re.IGNORECASE)
# The setting that lists objects a storage move leaves behind, which Hot's
# `hot db move-storage` reads; how many failed ids a report names, which must
# fit the AWS status parameter beside the rest of the failure; and how many
# diagnostics names, more than that setting can hold on AWS, in a report that
# goes to the log only.
MOVE_SKIP_SETTING = "HOT_MOVE_STORAGE_SKIP"
STORAGE_FAILED_LISTED = 8
STORAGE_FAILED_DIAGNOSED = 500
OBJECT_ID = re.compile(r"[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}")
# The setting that leaves behind everything the last report said did not move,
# by how many, however many that is: the agent lists them from the move's
# manifest, in a file beside it. The flag tells the move's job where it has
# that file; Hot refuses a move whose file it cannot read. Hot's variable for it
# would not do: its settings read any HOT_*_FILE as the file form of a setting,
# and a release from before it would ignore it and fail the same objects again,
# where a flag it does not know stops it at its arguments.
MOVE_SKIP_FAILED_SETTING = "HOT_MOVE_STORAGE_SKIP_FAILED"
MOVE_SKIP_FILE_FLAG = "--skip-file"
MOVE_SKIP_FILE_IN_JOB = "/run/secrets/move-storage-skip"
# Free space a data move leaves on the data disk beside the staged copy of
# the database dump, and a database dump beside the data.
STAGING_RESERVE_BYTES = 256 * 2**20
# After a resume that failed, how long before the loop tries it again.
RESUME_RETRY_SECONDS = 300
# How long the addresses of managed and existing data services are kept in
# memory before the operator secret bundle is read for them again.
DEPENDENCIES_SECONDS = 300
# After Compose starts services, how many times and how far apart the agent
# looks at their containers, and the container states that mean a service is
# down: Compose says "started" whatever the service does next.
SETTLE_LOOKS = 6
SETTLE_SECONDS = 3
DOWN_STATES = {"restarting", "exited", "dead"}
# What a production scheduler logs as it exits because another database session
# holds its singleton lock, and the line each of its starts logs first. Docker's
# restart brings it up once the lock frees.
SCHEDULER_LOCK_HELD = "already holds the scheduler singleton lock"
SCHEDULER_STARTING = "SCHEDULER starting"
# The label Compose gives a one-off job's container (`compose run`).
ONE_OFF_LABEL = "com.docker.compose.oneoff=True"
# The bundle's job that says how far the database has migrated, how long it may
# take, which covers pulling its image onto a replaced VM, and what a release
# whose Hot has no such command answers.
MIGRATION_STATUS_JOB = "migration-status"
MIGRATION_STATUS_SECONDS = 600
UNKNOWN_COMMAND = ("unexpected argument", "unrecognized subcommand")
# Where a release's runtime image holds its hotbox, and the stopped container
# the agent copies it out of (see install_hotbox).
HOTBOX_IN_IMAGE = "/usr/share/hot/bin/hotbox-linux-x86_64"
HOTBOX_CONTAINER = "hot-agent-hotbox"


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
# How long a question to a service's container may take: docker exec can hang,
# and the agent has one loop for deploys, commands, and every metric.
EXEC_SECONDS = 120
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


class UnreadableLevel(AgentError):
    """A migration status job that ran, and printed no migration level."""


# How reading a database's level with a migration status job can fail.
LEVEL_READ_FAILURES = (subprocess.CalledProcessError, subprocess.TimeoutExpired, UnreadableLevel)


class NoSkipFileFlag(AgentError):
    """A storage move's release that refused the flag naming its skip file, so
    what settings.HOT_MOVE_STORAGE_SKIP_FAILED says cannot happen on it."""


class ApiError(AgentError):
    """A cloud API's error response. The message names only the call and
    its status; `body` keeps the response for callers that inspect it."""

    def __init__(self, message, status, body=b""):
        super().__init__(message)
        self.status = status
        self.body = body


class CommandFailed(AgentError):
    """A command that failed after it changed something; `details` say how
    far it got and what to do next."""

    def __init__(self, message, **details):
        super().__init__(message)
        self.details = details


# --------------------------------------------------------------------------
# Cloud access with the VM's identity
# --------------------------------------------------------------------------

def http(method, url, body=None, headers=None, timeout=30):
    """(status, body). A request that gets no response (no connection, a
    timeout, a broken read) is an AgentError naming the host."""
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()
    except (OSError, HTTPException) as error:
        reason = getattr(error, "reason", None) or error
        raise AgentError(f"{method} {urllib.parse.urlsplit(url).hostname} failed: {reason}") from error


def secret_json(data, reference):
    """A secret's value, which holds JSON. One that does not is an
    AgentError naming the secret, never its content."""
    try:
        return json.loads(data)
    except (TypeError, ValueError):
        raise AgentError(f"The secret {reference.rsplit('/', 1)[-1].rsplit(':', 1)[-1]} does not hold valid JSON; "
                         "store its value again as a JSON object") from None


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
        try:
            result = json.loads(data or b"{}")
        except ValueError:  # an error page from something in between
            result = {}
        if status != 200 or not isinstance(result, dict):
            kind = (result.get("__type", "") if isinstance(result, dict) else "").split("#")[-1] or f"HTTP {status}"
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
        try:
            root = ElementTree.fromstring(data or b"<Empty/>")
        except ElementTree.ParseError as error:  # an error page from something in between
            raise AgentError(f"{service} {action} failed: HTTP {status} without an XML response") from error
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

    def installation(self, reference=""):
        """The installation configuration in its SSM parameter, which
        OpenTofu updates in place; None where the stack sets none."""
        value = self.get_parameter(reference) if reference else None
        return json.loads(value) if value else None

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
        """(milliseconds, message) pairs to a stream of the installation's
        log group, created on first use. Split into batches CloudWatch
        accepts: in time order (a clock stepped back leaves the journal out
        of it), under 1 MB, and spanning under a day."""
        target = {"logGroupName": self.log_group, "logStreamName": stream}
        events = sorted(events, key=lambda event: event[0])
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
        return secret_json(result.get("SecretString"), reference)

    def put_secret_if_absent(self, reference, value):
        existing = self.get_secret(reference)
        if existing is not None:
            return existing
        try:
            # A fixed token makes the first write idempotent: hosts writing
            # at once all keep the one version written first, since the same
            # token with other content is refused.
            self.call("secretsmanager", "secretsmanager.PutSecretValue",
                      {"SecretId": reference, "SecretString": json.dumps(value),
                       "ClientRequestToken": GENERATED_SECRET_TOKEN})
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
        self.instance_zone = None

    def metadata(self, path):
        status, data = http("GET", f"{self.METADATA}/{path}", headers={"Metadata-Flavor": "Google"})
        if status != 200:
            raise AgentError(f"The metadata server answered HTTP {status} for {path}")
        return data.decode()

    def token(self):
        return json.loads(self.metadata("instance/service-accounts/default/token"))["access_token"]

    def call(self, method, url, payload=None, allow=()):
        body = json.dumps(payload).encode() if payload is not None else None
        status, data = http(method, url, body, {"Authorization": "Bearer " + self.token(),
                                                "Content-Type": "application/json"}, timeout=90)
        if status in allow:
            return None
        if status >= 300:
            raise ApiError(f"{urllib.parse.urlparse(url).path} failed: HTTP {status}", status, data)
        return json.loads(data or b"{}")

    def get_secret(self, reference, version="latest"):
        result = self.call("GET", f"https://secretmanager.googleapis.com/v1/{reference}/versions/{version}:access",
                           allow=(404,))
        if result is None:
            return None
        try:
            data = base64.b64decode(result["payload"]["data"])
        except (KeyError, TypeError, ValueError):
            raise AgentError(f"The secret {reference.rsplit('/', 1)[-1]} has no readable value") from None
        return secret_json(data, reference)

    def get_parameter(self, reference):
        """The release a pointer names. Its versions hold {"release": ...},
        as the release workflow writes them."""
        value = self.get_secret(reference)
        if value is None:
            return None
        release = value.get("release") if isinstance(value, dict) else None
        if not isinstance(release, str):
            raise AgentError(f"The release pointer {reference.rsplit('/', 1)[-1]} must hold a JSON object such as "
                             '{"release": "3.0.2"}')
        return release

    def add_version(self, reference, value):
        return self.call("POST", f"https://secretmanager.googleapis.com/v1/{reference}:addVersion",
                         {"payload": {"data": base64.b64encode(json.dumps(value).encode()).decode()}})

    def put_secret_if_absent(self, reference, value):
        """Secret Manager has no create-if-absent. The stacks let one host add
        a version: the single machine, or the scalable scheduler's host, which
        deploys first. Any other host that finds the secret missing, outside
        that order, gets a permission error from add_version. Should two
        writers ever start at once, each adds a version: every host keeps
        version 1, the first one written, and a host whose own version came
        later adds version 1's value again, so the latest version, which later
        deliveries read, holds it too."""
        existing = self.get_secret(reference)
        if existing is not None:
            return existing
        added = self.add_version(reference, value) or {}
        if added.get("name", "").endswith("/versions/1"):
            return value
        first = self.get_secret(reference, "1")
        if first is not None and first != value:
            self.add_version(reference, first)
            return first
        return self.get_secret(reference)

    def put_status(self, reference, status):
        self.call("POST", "https://logging.googleapis.com/v2/entries:write", {
            "logName": f"projects/{self.project}/logs/{reference or 'hot-agent-status'}",
            "resource": {"type": "global"}, "entries": [{"jsonPayload": status, "severity": "INFO"}]})

    def registry_password(self, host):
        """An access token, which Artifact Registry accepts as a password."""
        return self.token()

    def check_storage_write(self, bucket, prefix):
        """Write and delete a small object under a storage role's prefix, where
        the host's grant applies. It goes under .uploads/, where Hot keeps nothing
        committed and lists only its own uploads, and where a provisioned bucket's
        lifecycle rule removes one whose delete never came. Raises what the
        service answered."""
        name = urllib.parse.quote(f"{prefix.strip('/')}/.uploads/hot-agent-write-check", safe="")
        self.call("POST", f"https://storage.googleapis.com/upload/storage/v1/b/{bucket}/o?uploadType=media&name={name}",
                  {"check": "hot-agent"})
        self.call("DELETE", f"https://storage.googleapis.com/storage/v1/b/{bucket}/o/{name}", allow=(404,))

    def installation(self, reference=""):
        """The installation configuration in the instance's metadata, which
        OpenTofu updates in place; None where the stack sets none."""
        status, data = http("GET", self.METADATA + "/instance/attributes/hot-installation",
                            headers={"Metadata-Flavor": "Google"})
        return json.loads(base64.b64decode(data)) if status == 200 else None

    def instance_name(self):
        """The instance name, which the managed instance group lists."""
        return self.metadata("instance/name")

    def own_zone(self):
        """This VM's zone. A regional group spreads its instances across the
        region's zones, so it is not always the installation's zone."""
        if self.instance_zone is None:
            self.instance_zone = self.metadata("instance/zone").rsplit("/", 1)[-1]
        return self.instance_zone

    def instance(self):
        if self.instance_id is None:
            self.instance_id = self.metadata("instance/id")
        return {"type": "gce_instance", "labels": {"project_id": self.project, "zone": self.own_zone(),
                                                    "instance_id": self.instance_id}}

    def ship_journal(self, installation, entries):
        severities = {0: "EMERGENCY", 1: "ALERT", 2: "CRITICAL", 3: "ERROR", 4: "WARNING", 5: "NOTICE", 6: "INFO", 7: "DEBUG"}
        written = [{"jsonPayload": entry["fields"], "severity": severities.get(entry["priority"], "DEFAULT"),
                    "timestamp": datetime.datetime.fromtimestamp(entry["at_ms"] / 1000, datetime.timezone.utc).isoformat()}
                   for entry in entries]
        sized = [(0, json.dumps(entry)) for entry in written]
        for chunk in log_batches(sized, max_bytes=5_000_000):
            # With partialSuccess Cloud Logging writes every entry it accepts
            # and answers with the error of one it refused, such as an entry
            # older than the log's retention. Those are refused for good, so
            # the batch counts as shipped rather than being sent again.
            try:
                self.call("POST", "https://logging.googleapis.com/v2/entries:write", {
                    "logName": f"projects/{self.project}/logs/hot-{installation}-host", "resource": self.instance(),
                    "partialSuccess": True, "entries": [json.loads(text) for _, text in chunk]})
            except ApiError as error:
                refused = partial_write_errors(error.body)
                if refused is None:
                    raise
                print(json.dumps({"journal_entries_refused": refused}), file=sys.stderr, flush=True)

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
        """At most one message, as (ack ID, request): pulling more would hold
        the others' acknowledgement deadlines while the first one runs, and
        Pub/Sub would deliver them again. The request is None for a message
        that is not JSON, so it can still be acknowledged. Its `sent` is when
        Pub/Sub received it."""
        if not self.subscription:
            return []
        result = self.call("POST", f"https://pubsub.googleapis.com/v1/{self.subscription}:pull", {"maxMessages": 1})
        received = []
        for message in (result or {}).get("receivedMessages", []):
            try:
                request = json.loads(base64.b64decode(message["message"].get("data", "") or b"e30="))
            except ValueError:
                request = None
            if isinstance(request, dict):
                request["sent"] = message["message"].get("publishTime", "")
            received.append((message["ackId"], request))
        return received

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


S3_KEYS = ("ACCESS_KEY_ID", "SECRET_ACCESS_KEY", "SESSION_TOKEN")
# Operator secrets by name prefix, and the Hot services that read them.
OPERATOR_ROUTES = {
    "HOT_EMAIL_": ("worker",),
    "HOT_GOOGLE_CLIENT_": ("app",),
    "HOT_GITHUB_CLIENT_": ("app",),
}
# Exact names, and the services that read them: the egress proxy carries
# user code's HTTP, which runs in workers and task workers, and its address
# may hold the proxy's credentials. Storage takes S3 credentials and nothing
# else, since a bucket or a region in the bundle would never reach Hot.
OPERATOR_NAMES = {"HOT_NETWORK_EGRESS_PROXY": ("worker", "task-worker"),
                  **{f"HOT_{role}_AWS_{suffix}": HOT_SERVICES for role in ("BUILD", "FILE") for suffix in S3_KEYS}}
# The app reads the other HOT_EMAIL_* settings too (the provider decides
# whether it sends invitations, and the sender and links go into each queued
# message), so only what the worker alone sends with goes in the bundle, which
# only the worker mounts: credentials, and a private relay's CA certificate.
# Anything with a secret's name counts as a credential.
EMAIL_WORKER_ONLY = ("HOT_EMAIL_SMTP_USERNAME", "HOT_EMAIL_SMTP_CA_CERT")
OPERATOR_INGRESS = ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY")
# Entries only the agent reads; no service mounts them.
OPERATOR_AGENT = ("HOT_SYNTHETIC_API_KEY",)
# Connection settings for existing data services; the bundle mounts them itself.
OPERATOR_DATA = {"postgres": ("HOT_DB_URI", "HOT_DB_MIGRATION_URI", "POSTGRES_CA_CERT"),
                 "valkey": ("HOT_REDIS_URI",)}
CA_PATH = "/run/secrets/POSTGRES_CA_CERT"


def operator_roles(key):
    """The Hot services that mount an operator secret, or None."""
    return OPERATOR_NAMES.get(key) or next(
        (roles for prefix, roles in OPERATOR_ROUTES.items() if key.startswith(prefix)), None)


def operator_entry_problem(key, data_keys):
    """Why a render refuses an operator secret bundle entry, or None."""
    if key in OPERATOR_INGRESS or key in OPERATOR_AGENT or key in data_keys:
        return None
    if key.endswith("_FILE"):
        return f"The operator secret bundle entry {key} names a file; put the value itself under {key[:-5]}"
    if (key.startswith("HOT_EMAIL_") and key not in EMAIL_WORKER_ONLY
            and not any(marker in key for marker in render.SECRET_MARKERS)):
        return (f"{key} is not a credential; put it in settings, which every Hot service reads, "
                "instead of the operator secret bundle")
    if operator_roles(key) is None:
        return f"The operator secret bundle has an unsupported entry {key}"
    return None


def operator_readers(key, data_keys):
    """What mounts an operator secret bundle entry: Hot services, by name or as
    an existing data service's connection, or the ingress. Nothing mounts one
    only the agent reads."""
    if key in OPERATOR_INGRESS:
        return ("ingress",)
    return HOT_SERVICES if key in data_keys else operator_roles(key) or ()


def reads_as_empty(value):
    """Whether Hot takes a secret file holding this for an empty one, which stops
    the service that mounts it (resolve_one, in crates/hot/src/environment.rs): it
    drops a byte order mark and one line ending, and nothing else."""
    return value.removeprefix("\ufeff") in ("", "\n", "\r\n")


def empty_entry(key):
    """The refusal of an entry that reads as empty, which never repeats a value."""
    return AgentError(f"The operator secret bundle entry {key} is empty, and the service that mounts it would "
                      "exit on an empty file. Give it a value, or remove the entry")


def bad_egress_proxy(key):
    """The refusal of an egress proxy address Hot would refuse, which never
    repeats the value: it may hold the proxy's password."""
    return AgentError(f"The operator secret bundle entry {key} must be an http:// or https:// proxy address: "
                      "a host, an optional port, and no path. A / ? # or \\ in its credentials must be "
                      "percent-encoded")


def existing_data_keys(config):
    """The operator secret bundle entries `config`'s existing data services read."""
    return {key for service, keys in OPERATOR_DATA.items()
            if render.data_mode(config, service) == "existing" for key in keys}


def operator_routes(operator, config=None, rendering=True):
    """Which Hot services mount each operator secret. A render refuses unknown
    names rather than silently ignoring them, and empty values where something
    here would mount them, before anything stops. Delivering secrets for a
    bundle already rendered (`rendering` False) refuses none of that: an entry
    the bundle does not mount must not keep a reboot, a restart, or a failed
    data move's way back from starting the installed release. A name a render
    would refuse goes to no service, and an existing data service's entries,
    which `config` may not use yet, are read by name (role_secrets)."""
    config = config or {}
    data_keys = existing_data_keys(config)
    settings = config.get("settings") or {}
    here = {*render.roles(config), *(("ingress",) if render.web_roles(config) else ())}
    routes = {}
    for key, value in operator.items():
        if not isinstance(value, str):
            raise AgentError(f"The operator secret bundle entry {key} must be a string")
        if rendering and key in settings:
            raise AgentError(f"{key} is set in both settings and the operator secret bundle; keep one")
        problem = operator_entry_problem(key, data_keys)
        if problem and rendering:
            raise AgentError(problem)
        # Every other check reads a blank entry as unset, but the bundle still
        # mounts its file. A host with nothing to mount it stays out of this, so
        # it still comes up for a scale-out or a replacement.
        if rendering and reads_as_empty(value) and here.intersection(operator_readers(key, data_keys)):
            raise empty_entry(key)
        # Only the agent sees this copy, the only one that may hold credentials
        # (render.validate checks the one in settings). The worker and task
        # worker exit on one Hot refuses, and the message never repeats it.
        # Delivering still hands it over: the installed bundle mounts that file.
        if rendering and key == "HOT_NETWORK_EGRESS_PROXY" and not render.is_egress_proxy(value):
            raise bad_egress_proxy(key)
        if not problem:
            for role in operator_roles(key) or ():
                routes.setdefault(role, []).append(key)
    return routes


def check_smtp(settings, operator):
    """What the worker's SMTP provider refuses and the app never sees, since
    it holds no credentials: it would tell people email works while the worker
    drops every message. Only the agent reads settings and the bundle together,
    so a render refuses them, before anything stops. Delivering secrets does
    not: a mail setting must not keep a reboot or a restart from starting the
    release that is installed. The rules are Hot's (SmtpSettings::from_conf),
    and the messages name settings, never values."""
    def setting(key):
        return str({**settings, **operator}.get(key) or "").strip()

    if "smtp" not in {setting(key).lower() for key in ("HOT_EMAIL_PROVIDER", "HOT_EMAIL_ALERTS_PROVIDER")}:
        return
    plaintext = setting("HOT_EMAIL_SMTP_SECURITY").lower() == "none"
    username, password = setting("HOT_EMAIL_SMTP_USERNAME"), operator.get("HOT_EMAIL_SMTP_PASSWORD", "")
    if bool(username) != bool(password):
        raise AgentError("Set both HOT_EMAIL_SMTP_USERNAME and HOT_EMAIL_SMTP_PASSWORD, or neither")
    if username and plaintext:
        raise AgentError("HOT_EMAIL_SMTP_SECURITY none would send the SMTP password in plaintext; use starttls or tls")
    certificate = setting("HOT_EMAIL_SMTP_CA_CERT")
    if certificate and "-----BEGIN CERTIFICATE-----" not in certificate:
        raise AgentError("HOT_EMAIL_SMTP_CA_CERT must hold PEM certificates")
    if certificate and plaintext:
        raise AgentError("HOT_EMAIL_SMTP_CA_CERT needs HOT_EMAIL_SMTP_SECURITY starttls or tls")


def check_oauth(settings, operator):
    """A sign-in provider needs its client ID and secret together. With one
    alone, or a blank one, the app refuses to start. So a render on a host with
    the app (Agent.check_sign_in), a restart of the app, and a migration host's
    new release (check_every_role) refuse them first, naming what is missing and
    never a value. Delivering secrets does not: an entry must not keep a reboot
    from starting the release that is installed. An ID may also be a setting; a
    secret never is."""
    values = {**settings, **operator}
    for provider, label in (("GOOGLE", "Google"), ("GITHUB", "GitHub")):
        names = (f"HOT_{provider}_CLIENT_ID", f"HOT_{provider}_CLIENT_SECRET")
        missing = [name for name in names if not str(values.get(name) or "").strip()]
        if missing and any(name in values for name in names):
            raise AgentError(f"Sign-in with {label} needs both {names[0]} and {names[1]}, and "
                             f"{' and '.join(missing)} {'has' if len(missing) == 1 else 'have'} no value. Set "
                             f"{'it' if len(missing) == 1 else 'them'} in the operator secret bundle, or remove what "
                             "is there")


def check_every_role(config, operator):
    """What some role's render refuses, whichever host this is. A scalable host
    leaves what only another role reads alone, so that it still comes up, and a
    release would find it partway: the database migrated and some roles rolled."""
    check_oauth(config.get("settings") or {}, operator)
    operator_routes(operator, {key: value for key, value in config.items() if key != "roles"})


def connection_parts(uri, name):
    """A connection URI's parts. urllib's errors can quote it, and part of a
    password can look like a port, so the message names the connection and
    nothing of its text."""
    try:
        parts = urllib.parse.urlsplit(uri)
        parts.port  # read here, since reading it is what fails
    except ValueError:
        raise AgentError(f"{name} is not a usable connection URI: its port must be a number, and a / ? # or [ in its "
                         "password must be percent-encoded") from None
    return parts


def verified_postgres_uri(uri, name, config):
    """Existing PostgreSQL must be reached with full certificate
    verification against the operator's CA, mounted at CA_PATH. Exactly one
    mode is accepted: Hot's driver reads the last one and takes ssl-mode for
    sslmode, and HOT_DB_URI reaches it without passing through a service file,
    so sslmode=verify-full&ssl-mode=disable must not read as verify-full."""
    parts = connection_parts(uri, name)
    if parts.scheme not in ("postgres", "postgresql") or not parts.hostname:
        raise AgentError(f"{name} must be a postgres:// URI")
    modes = [pair for pair in urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
             if pair[0] in ("sslmode", "ssl-mode")]
    if modes != [("sslmode", "verify-full")]:
        raise AgentError(f"{name} must set sslmode=verify-full, once, for an existing database")
    if name == "HOT_DB_URI" and urllib.parse.unquote(parts.username or "") != render.runtime_role(config):
        raise AgentError(f"HOT_DB_URI must log in as data.postgres.runtime_role ({render.runtime_role(config)})")
    # The operator's parameters stay as written: libpq, which the psql jobs
    # use, decodes only %XX, so re-encoding (a space as +) would change them.
    pairs = [pair for pair in parts.query.split("&")
             if pair and urllib.parse.unquote(pair.split("=", 1)[0]) != "sslrootcert"]
    pairs.append("sslrootcert=" + urllib.parse.quote(CA_PATH, safe=""))
    return urllib.parse.urlunsplit(parts._replace(query="&".join(pairs)))


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
            uri = operator.get(key)
            if not uri or not isinstance(uri, str):
                raise AgentError(f"An existing database needs {key} in the operator secret bundle"
                                 if service == "postgres" else
                                 "An existing Valkey needs a rediss:// HOT_REDIS_URI in the operator secret bundle")
            parts = connection_parts(uri, key)
            if parts.hostname:
                services.append((parts.hostname, parts.port or port))
    return services


def data_endpoints(config, operator):
    """Hosts of managed and existing data services, which user code must
    never reach."""
    return [host for host, _ in data_services(config, operator)]


# Box guests never reach these, whatever the operator allows (the task
# worker's ALWAYS_BLOCKED). The host baseline opens resolvers on port 53
# before its own deny entries, so a resolver here is refused.
GUEST_ALWAYS_BLOCKED = ("0.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "10.88.0.0/16", "224.0.0.0/4", "240.0.0.0/4")
# The host's bridges for the bundle's containers and box guests, not addresses
# of its own. docker0 is not one: host images run Docker with no default bridge,
# and the deny list names no 172.17.0.0/16, so one added to a host is denied as
# the host's own address.
BRIDGE_INTERFACES = re.compile(r"br-.*|veth.*|kata-br0")


# IPv6 prefixes that carry an IPv4 address, as Hot reads them
# (outbound::canonical_net).
EMBEDDED_IPV4 = tuple(ipaddress.ip_network(prefix) for prefix in ("::ffff:0:0/96", "::/96", "64:ff9b::/96"))


def ipv4_form(network):
    """An IPv6 range inside EMBEDDED_IPV4 as the IPv4 range it carries. Wider
    ranges and other networks stay as they are."""
    if network.version == 6 and network.prefixlen >= 96 and any(network.subnet_of(prefix) for prefix in EMBEDDED_IPV4):
        return ipaddress.IPv4Network((int(network.network_address) & 0xffffffff, network.prefixlen - 96))
    return network


def ipv4_networks(entries, name):
    """The IPv4 entries of an address list as networks, each once, as Hot
    reads them: IPv4 written in IPv6 counts. Other IPv6 entries are left out:
    box guests have no IPv6."""
    networks = []
    for entry in entries:
        entry = entry.strip()
        if not entry:
            continue
        try:
            network = ipv4_form(ipaddress.ip_network(entry, strict=False))
        except ValueError:
            raise AgentError(f"{name} entry {entry!r} is not an IP address or CIDR") from None
        if network.version == 4 and str(network) not in networks:
            networks.append(str(network))
    return networks


def outside_networks(addresses, networks):
    """The `addresses` in none of the `networks`, as Hot reads them: IPv4 written
    in IPv6 counts as IPv4."""
    networks = [ipv4_form(ipaddress.ip_network(network, strict=False)) for network in networks]

    def inside(address):
        address = ipv4_form(ipaddress.ip_network(address, strict=False))
        return any(address.version == network.version and address.subnet_of(network) for network in networks)
    return [address for address in addresses if not inside(address)]


def guest_network_env(config, extra_deny=(), proxy=()):
    """/etc/hot/guest-network.env for the host's box guest baseline, which
    runs before the task worker's per-task rules: the same deny list the task
    worker gets (render.user_code_deny), network.allow-cidrs, box.guest-dns
    resolvers, and the egress proxy as IPv4:port, which is then guests' only
    way out. Without them the baseline would drop every private destination,
    resolver, and proxy those rules allow. The file is sourced by a shell, so
    it holds only addresses parsed here and in proxy_addresses."""
    settings = config.get("settings") or {}
    deny = ipv4_networks(render.user_code_deny(config, extra_deny), "The guest deny list")
    allow = ipv4_networks(settings.get("HOT_NETWORK_ALLOW_CIDRS", "").split(","), "settings HOT_NETWORK_ALLOW_CIDRS")
    lines = [f"HOT_GUEST_DENY_CIDRS={','.join(deny)}", f"HOT_GUEST_ALLOW_CIDRS={','.join(allow)}"]
    resolvers = [entry.strip() for entry in settings.get("HOT_BOX_GUEST_DNS", "").split(",") if entry.strip()]
    if resolvers:
        blocked = [ipaddress.ip_network(network) for network in GUEST_ALWAYS_BLOCKED]
        for entry in resolvers:
            try:
                address = ipaddress.IPv4Address(entry)
            except ValueError:
                raise AgentError(f"settings HOT_BOX_GUEST_DNS entry {entry!r} is not an IPv4 address") from None
            if any(address in network for network in blocked):
                raise AgentError(f"settings HOT_BOX_GUEST_DNS entry {entry!r} is in a range box guests can never "
                                 "reach, such as loopback, link-local, or the guest network")
        lines.append("HOT_GUEST_DNS=" + ",".join(str(ipaddress.IPv4Address(entry)) for entry in resolvers))
    if proxy:
        lines.append("HOT_GUEST_PROXY=" + ",".join(proxy))
    return "".join(line + "\n" for line in lines)


def s3_authentication(config, operator):
    """How Hot's services reach S3 storage, as the renderer's (s3_auth,
    s3_session_tokens): the VM's identity, or access keys from the operator
    secret bundle. The renderer makes one choice for every S3-backed storage
    role, so every such role has its keys (and every one a session token, or
    none does) or none has: a mixed bundle would mount key files that are
    never written. Only an AWS VM has an identity S3 knows; elsewhere nothing
    would touch a bucket until a build or a file failed, long after the deploy
    reported success."""
    roles = [role for role, storage in config["storage"].items() if storage["backend"] == "s3"]
    held = {role: {suffix for suffix in S3_KEYS if f"HOT_{role.upper()}_AWS_{suffix}" in operator} for role in roles}
    if not any(held.values()):
        if roles and config["cloud"] != "aws":
            lacking = [f"HOT_{role.upper()}_AWS_{suffix}" for role in roles for suffix in S3_KEYS[:2]]
            raise AgentError(f"S3 storage needs access keys on {config['cloud']}, whose VMs have no AWS identity, and "
                             "the operator secret bundle lacks " + ", ".join(lacking))
        return "identity", False
    tokens = any("SESSION_TOKEN" in keys for keys in held.values())
    wanted = S3_KEYS if tokens else S3_KEYS[:2]
    missing = [f"HOT_{role.upper()}_AWS_{suffix}" for role in roles for suffix in wanted if suffix not in held[role]]
    if missing:
        raise AgentError("S3 storage uses access keys for every S3 storage role or for none, and the operator "
                         "secret bundle lacks " + ", ".join(missing))
    return "files", tokens


def service_file(uri, name):
    """A connection as the libpq service file the bundle's database jobs read
    (see render.pg_service). The message names the connection and nothing of
    its text: part of a password can look like a port."""
    try:
        return render.pg_service(uri)
    except ValueError:
        raise AgentError(f"{name} cannot be written as a libpq service file: it must be a postgres:// URI with a "
                         "numeric port, no service parameter, no repeated or blank query parameters, and no line "
                         "breaks or surrounding spaces in its values") from None


def role_secrets(config, generated, operator, managed=None):
    """Every file the rendered bundle mounts, keyed by role, then by name.
    Each role receives only what it needs. `managed` carries a managed
    database's administrator login and CA certificate, and a managed
    Valkey's AUTH token."""
    g = generated
    managed = managed or {}
    admin_uri = None
    pg_mode = render.data_mode(config, "postgres")
    pg_local = pg_mode == "local"
    valkey_local = render.data_mode(config, "valkey") == "local"
    if pg_local:
        runtime_uri = f"postgres://hot_runtime:{g['POSTGRES_RUNTIME_PASSWORD']}@postgres:5432/hot?sslmode=disable"
        owner_uri = f"postgres://hot_owner:{g['POSTGRES_PASSWORD']}@postgres:5432/hot?sslmode=disable"
    elif pg_mode == "managed":
        admin = managed.get("postgres")
        if not admin:
            raise AgentError("Managed PostgreSQL needs its CA certificate")
        endpoint, sslmode = config["data"]["postgres"]["endpoint"], managed_sslmode(config)
        runtime_uri = postgres_uri(render.runtime_role(config), g["POSTGRES_RUNTIME_PASSWORD"], endpoint, "hot", sslmode)
        owner_uri = postgres_uri("hot_owner", g["POSTGRES_PASSWORD"], endpoint, "hot", sslmode)
        # Only the host that migrates has the administrator login.
        if admin.get("username"):
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
    for role, keys in operator_routes(operator, config, rendering=False).items():
        for key in keys:
            files[role][key] = operator[key]
    files["app"]["HOT_APP_SESSION_SECRET"] = g["HOT_APP_SESSION_SECRET"]
    if pg_local:
        files["postgres"] = {"POSTGRES_PASSWORD": g["POSTGRES_PASSWORD"],
                             "POSTGRES_RUNTIME_PASSWORD": g["POSTGRES_RUNTIME_PASSWORD"]}
    if valkey_local:
        files["valkey"] = {"VALKEY_PASSWORD": g["VALKEY_PASSWORD"],
                           "VALKEY_ACL": render.valkey_acl(g["VALKEY_PASSWORD"])}
    for key in ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY"):
        if key not in operator:
            raise AgentError(f"The operator secret bundle needs {key} for HTTPS ingress")
    files["ingress"] = {"TLS_CERTIFICATE": operator["TLS_CERTIFICATE"], "TLS_PRIVATE_KEY": operator["TLS_PRIVATE_KEY"]}
    files["migrate"] = {"HOT_DB_MIGRATION_URI": owner_uri, **ca}
    if pg_local:
        files["provision"] = {"PGPASSFILE": f"postgres:5432:hot:hot_owner:{g['POSTGRES_PASSWORD']}\n"}
    else:
        owner_service = service_file(owner_uri, "HOT_DB_MIGRATION_URI")
        files["provision"] = {"HOT_DB_MIGRATION_SERVICE": owner_service, **ca}
        # Also what restore-postgres reads.
        files["check-postgres"] = {"HOT_DB_MIGRATION_SERVICE": owner_service, **ca}
    if admin_uri:
        admin_service = service_file(admin_uri["postgres"], "The managed PostgreSQL administrator login")
        files["prepare-postgres"] = {"POSTGRES_ADMIN_SERVICE": admin_service, "POSTGRES_ADMIN_HOT_URI": admin_uri["hot"],
                                     "POSTGRES_PASSWORD": g["POSTGRES_PASSWORD"],
                                     "POSTGRES_RUNTIME_PASSWORD": g["POSTGRES_RUNTIME_PASSWORD"], **ca}
    if config.get("admin_email"):
        files["bootstrap-admin"] = {"HOT_DB_URI": runtime_uri, "ADMIN_PASSWORD": g["ADMIN_PASSWORD"], **ca}
    return files


def write_secret_files(root, files, chown=os.chown, keep_others=False):
    """Write secrets to tmpfs: one 0400 file per value, owned by the reading
    container's user. The new tree is written beside the delivered one and
    then replaces it whole, so a failure partway leaves the delivered
    secrets as they were. Stale files from other releases go with the old
    tree. With `keep_others`, roles not in `files` go on to the new tree as
    they were delivered (hard links, on the same tmpfs)."""
    root = Path(root)
    for role, values in files.items():
        for key, value in values.items():
            if not isinstance(value, str):
                raise AgentError(f"The secret {key} for {role} is not text")
    staging, old = root.with_name(root.name + ".new"), root.with_name(root.name + ".old")
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(mode=0o711, parents=True)
    try:
        for role, values in files.items():
            directory = staging / role
            directory.mkdir(mode=0o711)
            owner = SECRET_OWNERS.get(role, 0 if role == "task-worker" else 10001)
            for key, value in values.items():
                path = directory / key
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
                with os.fdopen(descriptor, "w") as handle:
                    handle.write(value)
                chown(path, owner, owner)
        if keep_others and root.is_dir():
            for directory in root.iterdir():
                if directory.name not in files:
                    shutil.copytree(directory, staging / directory.name, symlinks=True, copy_function=os.link)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    shutil.rmtree(old, ignore_errors=True)
    if root.exists():
        os.rename(root, old)
    os.rename(staging, root)
    shutil.rmtree(old, ignore_errors=True)


def write_atomic(path, text, mode=0o600):
    """Replace a file whole: the new content is written and flushed beside
    it, then renamed over it. A full disk, a kill, or a power loss leaves
    the old file, never an empty or partial one."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.new")
    try:
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
        with os.fdopen(descriptor, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    except BaseException:
        with contextlib.suppress(OSError):
            temporary.unlink()
        raise
    # The rename itself survives a power loss once its directory is synced.
    with contextlib.suppress(OSError):
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)


# --------------------------------------------------------------------------
# Redaction: what diagnostics and logs may carry off the host
# --------------------------------------------------------------------------

class Redactor:
    """Removes secrets from text the agent reports: every value in the
    host's secret files (and passwords inside connection URIs and libpq
    service files), then common credential shapes. Email addresses keep only
    their domain, and AWS account IDs only their last four digits."""

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
            if "PRIVATE KEY-----" in value:
                # Logs and the journal print a key one line at a time.
                known.update(line.strip() for line in value.splitlines() if not line.startswith("-----"))
            for word in value.split():
                if "://" in word:
                    try:
                        password = urllib.parse.urlsplit(word).password or ""
                    except ValueError:  # not a URL after all, such as abc://[def
                        known.add(word)
                        continue
                    known.update({password, urllib.parse.unquote(password)})
            for line in value.splitlines():
                if line.startswith("password="):  # a service file, as render.pg_service writes it
                    known.add(line[len("password="):])
        # As JSON log lines carry them: quotes, backslashes, and newlines escaped.
        known |= {json.dumps(value, ensure_ascii=False)[1:-1] for value in known}
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
    """The newest redacted lines that fit within `total` characters. They
    are redacted together, so a value printed across lines, such as a
    private key, is found whole."""
    kept, size = [], 0
    for line in reversed(redact("\n".join(text.splitlines()[-lines:])).splitlines()):
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
        # Restarts this process finished and could not take off the state, as
        # {service: request ID}: see finish_restarts.
        self._restarted = {}

    @staticmethod
    def _run(command, **kwargs):
        """Run a command and keep its output. Given `stdout`, a file of the
        caller's, what it prints goes straight there, however much that is, and
        only its errors are kept."""
        streams = {"stderr": subprocess.PIPE} if "stdout" in kwargs else {"capture_output": True}
        return subprocess.run(command, check=True, text=True, **streams, **kwargs)

    # -- host preparation ---------------------------------------------------

    def data_device(self):
        if self.config.get("data_disk") == "scratch" and self.config["cloud"] == "aws":
            return "auto"  # an Auto Scaling group's volume IDs are not known in advance
        if self.config["cloud"] == "aws":
            return "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_" + self.config["data_volume_id"].replace("-", "")
        return "/dev/disk/by-id/google-hot-data"

    def mount_data(self):
        """Mount an initialized data disk before anything reads state from
        it: a replaced VM boots without the mount, and its state, deploy
        history, and data are on the disk. Its boot skipped the host services
        that wait for the disk, so they start once it is mounted. Until
        configure_host has finished (its marker is on the boot disk, so a
        restart does not forget) it runs again each cycle. A blank disk is left
        for a first install's prepare_host to format."""
        if self.config.get("data_disk") == "scratch":
            return
        if not os.path.ismount(self.data_root):
            try:
                self.run_command([str(LIBEXEC / "hot-data-disk"), self.data_device()])
            except (subprocess.CalledProcessError, OSError):
                return
            (self.etc / "host-configured").unlink(missing_ok=True)
        if not (self.etc / "host-configured").exists():
            self.configure_host()

    def grow_data_disk(self):
        """Grow the filesystem of a data disk enlarged since the last deploy,
        for a command about to check its room. Only hot-data-disk does that, and
        otherwise a mounted disk meets it only in a deploy. It changes nothing
        on an unchanged disk, and one it cannot reach is the room check's to
        refuse."""
        try:
            self.run_command([str(LIBEXEC / "hot-data-disk"), self.data_device()])
        except (subprocess.CalledProcessError, OSError) as error:
            print(json.dumps({"data_disk_not_grown": self.explain(error)}), file=sys.stderr, flush=True)

    def prepare_scratch(self):
        """Format and mount a scalable host's scratch disk if this process has
        not yet, before anything writes the agent's state on it: written
        first, it would land on the boot disk, under the mount."""
        if (self.config.get("data_disk") == "scratch" and not getattr(self, "_scratch_ready", False)
                and not os.path.ismount(self.data_root)):
            self.prepare_host(first_install=False)

    def prepare_host(self, first_install):
        """Mount the data disk and write host settings. A blank disk is
        formatted only for a brand-new installation."""
        command = [str(LIBEXEC / "hot-data-disk"), self.data_device()]
        # A scalable host's disk is scratch: it holds nothing that outlives
        # the instance, so a blank one is always formatted.
        if first_install or self.config.get("data_disk") == "scratch":
            command.append("--allow-format")
        self.run_command(command)
        self._scratch_ready = True
        data = self.data_root / "data"
        # Before anything writes the generated secret or starts PostgreSQL,
        # whose image names this only in its own log. Today's layout keeps
        # PG_VERSION under <major>/docker.
        if render.data_mode(self.config, "postgres") == "local" and (data / "postgres/PG_VERSION").exists():
            raise AgentError("This data disk holds a database in the layout of a v3.0.1 bundle (data/postgres/"
                             "PG_VERSION), which is not supported: this release's PostgreSQL image refuses to start on "
                             "it. Nothing on the disk has changed, and this deploy wrote no secrets. Create a new "
                             "installation, and dump the data from the old database and restore it there by hand")
        # The bundle bind-mounts these, and Docker does not create them.
        # PostgreSQL and Valkey own theirs from their own entrypoints; Hot's
        # services write local builds and files as JOB_USER.
        for service in ("postgres", "valkey"):
            if render.data_mode(self.config, service) == "local":
                (data / service).mkdir(parents=True, exist_ok=True)
        for role, storage in self.config["storage"].items():
            if storage["backend"] == "local":
                (data / role).mkdir(parents=True, exist_ok=True)
                self.chown(data / role, JOB_USER, JOB_USER)
        # Downloaded embedding models, for the services that load them: the
        # task worker, which runs as root, keeps its own.
        for role in render.MODEL_ROLES:
            if role in self.roles():
                models = data / render.MODEL_DIRECTORIES[role]
                models.mkdir(parents=True, exist_ok=True)
                owner = 0 if role == "task-worker" else JOB_USER
                self.chown(models, owner, owner)
        # Extracted bundles, the same way, and for their service's eyes alone:
        # they hold deployed code, which a tmpfs would keep inside the container.
        for role, directory in render.EXTRACTION_DIRECTORIES.items():
            if role in self.roles():
                extractions = data / directory
                extractions.mkdir(mode=0o700, parents=True, exist_ok=True)
                owner = 0 if role == "task-worker" else JOB_USER
                self.chown(extractions, owner, owner)
        # Each box's /data, also for the task worker's eyes alone.
        if "task-worker" in self.roles():
            box_data = data / render.BOX_DATA_DIRECTORY
            box_data.mkdir(mode=0o700, parents=True, exist_ok=True)
            self.chown(box_data, 0, 0)
        self.configure_host()

    def configure_host(self):
        """Write the host settings, then start what reads them: the box
        pool's size, the box guests' network baseline (see
        guest_network_env), and Kata's containerd, which waits for the data
        disk."""
        operator = self.operator_secrets()
        guest = self.guest_network(operator)
        self.etc.mkdir(parents=True, exist_ok=True)
        marker = self.etc / "host-configured"
        marker.unlink(missing_ok=True)
        write_atomic(self.etc / "host.env", f"HOT_BOX_POOL_GB={BOX_POOL_GB[self.config.get('size', 'small')]}\n",
                     mode=0o644)
        write_atomic(self.etc / "guest-network.env", guest, mode=0o644)
        self.run_command(["systemctl", "restart", "hot-guest-firewall.service"])
        self.run_command(["systemctl", "start", "kata-containerd.service"])
        marker.write_text(utc_now())

    def guest_network(self, operator):
        """/etc/hot/guest-network.env as it should be now. An egress proxy whose
        name does not resolve at the moment keeps the addresses in the file, so
        guests stay limited to the proxy through a DNS outage, and it is looked
        up again next cycle."""
        proxy = self.proxy_addresses(operator)
        if not proxy and self.egress_proxy(operator):
            self._guest_network_checked = None
            proxy = self.written_proxy()
        return guest_network_env(self.config, self.extra_deny(operator), proxy)

    def written_proxy(self):
        """The proxy entries guest-network.env holds."""
        try:
            lines = (self.etc / "guest-network.env").read_text().splitlines()
        except OSError:
            return []
        return next((line.split("=", 1)[1].split(",") for line in lines if line.startswith("HOT_GUEST_PROXY=")), [])

    def refresh_guest_network(self):
        """Keep the box guests' baseline in step with addresses that change
        between deploys: the egress proxy's, which the task worker resolves for
        every task, and the data services'. They are looked up every
        DEPENDENCIES_SECONDS, and the baseline restarts only when they changed,
        which leaves containerd and running boxes alone. A restart that fails
        puts the file back, so the next check tries it again."""
        path = self.etc / "guest-network.env"
        checked = getattr(self, "_guest_network_checked", None)
        if not path.exists() or (checked is not None and time.monotonic() - checked < DEPENDENCIES_SECONDS):
            return
        self._guest_network_checked = time.monotonic()
        wanted, written = self.guest_network(self.operator_secrets()), path.read_text()
        if wanted == written:
            return
        write_atomic(path, wanted, mode=0o644)
        try:
            self.run_command(["systemctl", "restart", "hot-guest-firewall.service"])
        except Exception:
            write_atomic(path, written, mode=0o644)
            raise

    def egress_proxy(self, operator):
        """The egress proxy's URL, from settings or the operator bundle."""
        proxy = (self.config.get("settings") or {}).get("HOT_NETWORK_EGRESS_PROXY") or operator.get(
            "HOT_NETWORK_EGRESS_PROXY")
        return proxy if isinstance(proxy, str) else ""

    def proxy_addresses(self, operator):
        """The egress proxy as IPv4:port entries, the only way box guests reach
        the internet when it is set; its credentials stay in its URL. None
        when its name does not resolve now: the task worker then refuses
        guests' networks itself."""
        proxy = self.egress_proxy(operator)
        try:
            parts = urllib.parse.urlsplit(proxy.strip())
            port = parts.port or {"http": 80, "https": 443}.get(parts.scheme)
            infos = socket.getaddrinfo(parts.hostname, None, socket.AF_INET) if parts.hostname and port else []
        except (OSError, ValueError, UnicodeError):
            return []
        return sorted({f"{info[4][0]}:{port}" for info in infos})

    def host_addresses(self):
        """This host's own IPv4 addresses, as /32 deny entries: user code that
        reaches one reaches the ingress from the bundle's network, whose
        forwarded headers the app believes. The bridges are left out, since
        the deny list names their whole networks. Addresses that cannot be
        read stop whatever needs them, so no bundle or firewall is written
        without them."""
        try:
            interfaces = json.loads(self.run_command(["ip", "-json", "-4", "address", "show", "scope", "global"]).stdout)
            addresses = {f"{ipaddress.IPv4Address(entry['local'])}/32" for interface in interfaces
                         if not BRIDGE_INTERFACES.fullmatch(interface.get("ifname", ""))
                         for entry in interface.get("addr_info", [])}
        except (OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as error:
            raise AgentError(f"Cannot read this host's addresses: {describe(error)}") from error
        if not addresses:
            raise AgentError("Cannot read this host's addresses: no interface has a global IPv4 address")
        return sorted(addresses)

    def extra_deny(self, operator):
        """What user code may never reach beyond what the configuration names
        (render.user_code_deny): the addresses of data services and of this
        host. The bundle and the box guests' baseline get the same list."""
        return [*self.data_addresses(operator), *self.host_addresses()]

    def data_addresses(self, operator):
        """The data services' addresses, with those denied so far: a failover
        can bring an old one back, and a name that answers with addresses in
        turn settles once all of them are listed."""
        return sorted({*self.resolve(data_endpoints(self.config, operator)), *self.state().get("data_addresses", [])})

    def readdressed(self, state):
        """Whether this VM or a data service has an address its bundle does not
        deny. A replaced VM keeps the data disk with the bundle, and on an
        existing network it gets an address the bundle does not name; so does
        a managed database or cache after a failover, inside whatever range an
        allowlist opens. A new network's whole range is denied already, which
        takes in its hosts, and so are the networks the stack says a service moves
        within (render.data_deny), so a failover inside them costs no redeploy:
        that would restart the workers as the service recovers, and gain nothing.
        An existing service on a new network can move outside all of them."""
        recorded = state.get("host_addresses")
        if recorded is None:
            return False
        network = self.config.get("network", {})
        new_network = network.get("mode", "new") == "new"
        if not new_network:
            with contextlib.suppress(AgentError):  # a deploy would stop on it too; this cycle leaves things as they are
                if recorded != self.host_addresses():
                    return True
        denied = state.get("data_addresses")
        if denied is None:
            return False
        with contextlib.suppress(AgentError, KeyError, TypeError, ValueError):
            moved = set(self.resolve([host for host, _ in self.dependencies()])) - set(denied)
            whole = [network.get("cidr", "10.68.0.0/16")] if new_network else []  # as render.user_code_deny does
            return bool(outside_networks(moved, [*whole, *render.data_deny(self.config)]))
        return False

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

    def render_bundle(self, release, manifest, operator, install=True):
        """Render a release's bundle beside its installed one, which a
        failure leaves as it was. With `install` it then takes that one's
        place (install_bundle)."""
        staging = self.releases / release / "bundle.new"
        shutil.rmtree(staging, ignore_errors=True)
        staging.mkdir(mode=0o700, parents=True)
        try:
            self.check_sign_in(operator)
            routes = operator_routes(operator, self.config)
            check_smtp(self.config.get("settings") or {}, operator)
            s3_auth, sessions = s3_authentication(self.config, operator)
            compose = render.render(self.config, manifest, str(HOT_ROOT / "data"), str(self.secrets_root),
                                    s3_auth=s3_auth, s3_session_tokens=sessions, operator_secrets=routes,
                                    extra_deny=self.extra_deny(operator),
                                    instance=self.instance_name())
            (staging / "compose.json").write_text(json.dumps(compose, indent=2))
            (staging / "nginx.conf").write_text(render.nginx_config(render.validate(self.config)))
            (staging / "init.sql").write_text(render.INIT_SQL)
            (staging / "valkey.conf").write_text(render.valkey_conf(self.config.get("size", "small")))
            (staging / "check_postgres.sql").write_text(render.CHECK_POSTGRES_SQL)
            (staging / "prepare_postgres.sql").write_text(render.PREPARE_POSTGRES_SQL)
            (staging / "restore_postgres.sh").write_text(render.RESTORE_POSTGRES_SH)
            shutil.copy(provision_sql(), staging / "provision_runtime.sql")
        except BaseException:
            shutil.rmtree(staging, ignore_errors=True)
            raise
        return self.install_bundle(release) if install else staging

    def install_bundle(self, release, keep=False):
        """Put a rendered bundle in place. Containers bind its files by path,
        so they are always created from the installed path. With `keep`, in a
        deploy of the release already running, that release's own bundle
        stays beside it as bundle-running: the deploy drains and dumps through
        it, and a failure before the point of no return puts it back
        (restore_installed_bundle)."""
        bundle = self.releases / release / "bundle"
        aside = bundle.with_name("bundle-running" if keep else "bundle.old")
        shutil.rmtree(aside, ignore_errors=True)
        if bundle.exists():
            os.rename(bundle, aside)
        os.rename(bundle.with_name("bundle.new"), bundle)
        if not keep:
            shutil.rmtree(aside, ignore_errors=True)
        return bundle

    def restore_installed_bundle(self, record):
        """After a deploy of the running release rendered a new bundle and
        then failed or was interrupted before its point of no return (see
        committed), put the release's own bundle back: it runs again as it
        was installed, and a retry drains and dumps through it. True when it
        did."""
        release = record.get("release")
        if (not release or release != record.get("previous") or "rendered" not in record.get("steps", [])
                or self.committed(record)):
            return False
        bundle = self.releases / release / "bundle"
        running = bundle.with_name("bundle-running")
        if not (running / "compose.json").exists():
            return False
        replaced = bundle.with_name("bundle.old")
        shutil.rmtree(replaced, ignore_errors=True)
        if bundle.exists():
            os.rename(bundle, replaced)
        os.rename(running, bundle)
        shutil.rmtree(replaced, ignore_errors=True)
        return True

    @staticmethod
    def committed(record):
        """Whether a deploy got past the point where the installed release
        can run as before: its data or storage moved."""
        return ("data-moved" in record.get("steps", []) or bool(record.get("data_moved"))
                or Agent.storage_progress(record) == "moved")

    @staticmethod
    def storage_progress(record):
        """How far a deploy, or an attempt before it, moved storage: "moving"
        while some records may point at the new storage, "moved" once they all
        do, or None. This attempt's steps come first."""
        steps = record.get("steps", [])
        if "storage-moved" in steps:
            return "moved"
        return "moving" if "storage-moving" in steps else record.get("storage_progress")

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
        elif self.artifact_registry_mirror():
            user, password = "oauth2accesstoken", self.cloud.registry_password(host)
        else:
            return
        self.run_command(["docker", "login", "--username", user, "--password-stdin", f"https://{host}"], input=password)

    def artifact_registry_mirror(self):
        """Whether image_registry is an Artifact Registry repository the VM's
        identity signs in to."""
        host = self.config.get("image_registry", "").split("/", 1)[0]
        return self.config["cloud"] == "gcp" and host.endswith("-docker.pkg.dev")

    def wait_for_grant(self, attempt, refused, what, timeout, clock):
        """Run `attempt`, and again while `refused` says its failure may only mean
        a new grant has not taken effect, up to `timeout`: a deploy that fails is
        not tried again until asked. Returns its result, or raises its last
        failure, or any failure `refused` does not recognize."""
        deadline, said = clock() + timeout, False
        while True:
            try:
                return attempt()
            except (AgentError, subprocess.CalledProcessError) as error:
                if not refused(error) or clock() >= deadline:
                    raise
                if not said:
                    print(json.dumps({"waiting_for_grant": what, "refused": self.explain(error)}),
                          file=sys.stderr, flush=True)
                    said = True
            self.sleep(GRANT_RETRY_SECONDS)

    def pull_images(self, bundle, timeout=GRANT_WAIT_SECONDS, clock=time.monotonic):
        """Sign in to a private mirror and pull a bundle's images. An Artifact
        Registry mirror's refusal is tried again with a fresh sign-in, since the
        host's grant may be new. Any other failure, such as an image the mirror
        does not hold, is reported at once."""
        def pull():
            self.registry_login()
            self.compose(bundle, "--profile", "admin", "pull", "--quiet")

        def refused(error):
            return (isinstance(error, subprocess.CalledProcessError) and self.artifact_registry_mirror()
                    and bool(REGISTRY_REFUSED.search(error.stderr or error.output or "")))
        self.wait_for_grant(pull, refused, "image_registry", timeout, clock)

    def compose(self, bundle, *args, **kwargs):
        return self.run_command(["docker", "compose", "-f", str(bundle / "compose.json"), *args], cwd=bundle, **kwargs)

    def data_disk_mounted(self):
        return os.path.ismount(self.data_root)

    def copy_hotbox(self, bundle):
        """The hotbox of the release in `bundle`, copied out of its digest-pinned
        runtime image to a file beside the installed one, ready to take its
        place. None on a host with no task worker. A copy that fails leaves the
        installed one alone and is an AgentError, since the baked fallback, like
        any other release's hotbox, may not speak this release's protocol. So is
        a data disk that is not mounted: the copy would land on the boot disk,
        under the mount, where the task worker never finds it once the disk is back."""
        if "task-worker" not in self.roles():
            return None
        directory = self.data_root / "kata"
        copy = directory / ".hotbox.new"
        try:
            if not self.data_disk_mounted():
                raise AgentError("the data disk is not mounted")
            image = json.loads((bundle / "compose.json").read_text())["services"]["task-worker"]["image"]
            directory.mkdir(mode=0o755, parents=True, exist_ok=True)
            copy.unlink(missing_ok=True)
            # A kill can leave the container behind, and its name would be taken.
            self.remove_hotbox_container()
            self.run_command(["docker", "create", "--name", HOTBOX_CONTAINER, image])
            try:
                self.run_command(["docker", "cp", f"{HOTBOX_CONTAINER}:{HOTBOX_IN_IMAGE}", str(copy)])
            finally:
                self.remove_hotbox_container()
            if not copy.stat().st_size:
                raise AgentError("the copy is empty")
            self.chown(copy, 0, 0)
            os.chmod(copy, 0o755)
            with open(copy, "rb+") as handle:
                os.fsync(handle.fileno())
        except (AgentError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
            with contextlib.suppress(OSError):
                copy.unlink(missing_ok=True)
            raise AgentError(f"Could not copy hotbox out of the runtime image of {bundle.parent.name}: "
                             f"{self.explain(error)}") from error
        return copy

    def remove_hotbox_container(self):
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            self.run_command(["docker", "rm", "--force", HOTBOX_CONTAINER])

    def install_hotbox(self, bundle):
        """Before a release's task worker starts, whatever starts it: put that
        release's own hotbox where the worker has Kata's shim bind-mount it into
        each box, replacing whatever release ran last whole. The file lives on
        the data disk, so a replaced VM finds the last one, and this replaces it
        like any other. The host image's baked copy is only the worker's fallback
        when no file is there."""
        copy = self.copy_hotbox(bundle)
        if copy:
            os.replace(copy, copy.with_name("hotbox"))
            with contextlib.suppress(OSError):
                directory = os.open(copy.parent, os.O_RDONLY)
                try:
                    os.fsync(directory)
                finally:
                    os.close(directory)

    def operator_secrets(self):
        operator = self.cloud.get_secret(self.references["operator"]) or {}
        if not isinstance(operator, dict):
            raise AgentError("The operator secret bundle must be a JSON object of names and values")
        return operator

    def check_sign_in(self, operator):
        """What a render does about a half-set sign-in provider (check_oauth),
        before operator_routes, whose refusal of a blank entry says less. Only the
        app reads the pair, so only its host refuses. A scalable host with no app
        must still come up for a scale-out or a replacement, and only logs it; a
        release stops at the migration host, before it changes anything
        (check_every_role)."""
        try:
            check_oauth(self.config.get("settings") or {}, operator)
        except AgentError as error:
            if "app" in self.roles():
                raise
            print(json.dumps({"sign_in_not_usable": str(error)}), file=sys.stderr, flush=True)

    def unmounted_secrets(self, bundle, roles=None, rendering=True):
        """Hot services whose operator secret bundle entries differ from what
        the rendered `bundle` mounts, as {service: (added, removed)}. A value
        rotated in place needs only delivering; an entry added or removed
        needs a render. Empty when no bundle is rendered there. An entry a
        render refuses is refused here too, unless the caller only delivers
        (`rendering`, as in operator_routes)."""
        try:
            services = json.loads((bundle / "compose.json").read_text())["services"]
        except (OSError, ValueError, KeyError, TypeError):
            return {}
        operator = self.operator_secrets()
        if rendering:
            self.check_sign_in(operator)
        routes = operator_routes(operator, self.config, rendering)
        prefixes = tuple(OPERATOR_ROUTES)
        changes = {}
        for role in HOT_SERVICES:
            if role not in services or (roles is not None and role not in roles):
                continue
            mounted = {entry["target"] for entry in services[role].get("secrets", [])
                       if entry["target"] in OPERATOR_NAMES or entry["target"].startswith(prefixes)}
            wanted = set(routes.get(role, []))
            if mounted != wanted:
                changes[role] = (sorted(wanted - mounted), sorted(mounted - wanted))
        return changes

    def managed_data(self, config=None):
        """What the agent needs from managed data services: the database's
        administrator login and CA certificate, and the cache's AUTH token."""
        config = config or self.config
        managed = {"postgres": self.managed_postgres(config)}
        if render.data_mode(config, "valkey") == "managed" and config["cloud"] == "aws":
            managed["valkey"] = self.cloud.get_secret(config["data"]["valkey"]["endpoint"]["auth_secret"])
        return managed

    def managed_postgres(self, config=None):
        """The CA certificate a managed database's server presents and, on
        the host that migrates, the administrator login from the secret the
        stack names. Only that host may read the login."""
        config = config or self.config
        if render.data_mode(config, "postgres") != "managed":
            return None
        endpoint = config["data"]["postgres"]["endpoint"]
        if config["cloud"] == "aws":
            status, data = http("GET", RDS_CA_BUNDLE.format(region=config["region"]), timeout=60)
            ca = data.decode() if status == 200 else ""
        else:
            ca = endpoint.get("ca_certificate", "")
        if "-----BEGIN CERTIFICATE-----" not in ca:
            raise AgentError("The managed PostgreSQL CA certificate is unavailable")
        if not render.runs_migrations(config):
            return {"ca": ca}
        login = self.cloud.get_secret(endpoint["admin_secret"])
        if not isinstance(login, dict) or not login.get("username") or not login.get("password"):
            raise AgentError("The managed PostgreSQL administrator secret has no login yet")
        return {"username": login["username"], "password": login["password"], "ca": ca}

    def installed_config(self, data_modes=None):
        """The configuration with the data services the installed release
        was deployed with (`data_modes`, as the state records them): until a
        data move finishes, the old ones. A move from a managed service
        cannot be described again once the configuration has dropped it."""
        if not data_modes or data_modes == self.configured_modes():
            return self.config
        config = json.loads(json.dumps(self.config))
        data = config.setdefault("data", {})
        for service, mode in data_modes.items():
            if render.data_mode(config, service) == mode:
                continue
            if mode == "managed":
                raise AgentError(f"The installed release uses a managed {service}, which the configuration no longer "
                                 f"describes, so its connection cannot be delivered. Describe it in data.{service} "
                                 "again and run deploy-now to start Hot, then change the mode")
            data[service] = {"mode": mode}
        return config

    def deliver_secrets(self, data_modes=None, only=None):
        """Write every secret this host's services and jobs read. With
        `data_modes`, for the data services the installed release uses (see
        installed_config) rather than the configured ones. With `only`, just
        those roles' files change, and the others stay as delivered."""
        config = self.installed_config(data_modes)
        generated = self.cloud.put_secret_if_absent(self.references["generated"], generate_secrets())
        files = role_secrets(config, generated, self.operator_secrets(), self.managed_data(config))
        # A host receives only what the services and jobs it runs read.
        present = {*self.roles(), "postgres", "valkey"} | ({"ingress"} if render.web_roles(self.config) else set())
        if render.runs_migrations(self.config):
            present |= {"migrate", "provision", "bootstrap-admin", "check-postgres", "prepare-postgres"}
        delivering = {role: f for role, f in files.items() if role in present and (only is None or role in only)}
        write_secret_files(self.secrets_root, delivering, chown=self.chown, keep_others=only is not None)

    def deliver_level_secrets(self):
        """Deliver what the migration status job mounts, migrate's files, and
        nothing else. A deploy may yet be refused after that read, and the
        installed release goes on: a service reads its files again at its next
        restart, even one Docker does, so they change only at the deploy's
        `secrets` step. Only jobs read migrate's, each after a delivery of its own."""
        self.deliver_secrets(only=("migrate",))

    def kept_secrets(self):
        return self.secrets_root.with_name(self.secrets_root.name + ".installed")

    def keep_secrets(self):
        """Before a data move delivers its new secrets, keep the delivered
        ones beside them (hard links, on the same tmpfs), so a failed move can
        give the installed release its own back."""
        kept = self.kept_secrets()
        shutil.rmtree(kept, ignore_errors=True)
        if self.secrets_root.is_dir() and any(self.secrets_root.iterdir()):
            shutil.copytree(self.secrets_root, kept, symlinks=True, copy_function=os.link)

    def restore_installed_secrets(self, data_modes):
        """Give the installed release its own secrets back after a data move
        delivered new ones: the copy keep_secrets made, or, when a reboot
        emptied the tmpfs, its data services' secrets delivered again."""
        kept, root = self.kept_secrets(), self.secrets_root
        if kept.is_dir():
            old = root.with_name(root.name + ".old")
            shutil.rmtree(old, ignore_errors=True)
            if root.exists():
                os.rename(root, old)
            os.rename(kept, root)
            shutil.rmtree(old, ignore_errors=True)
        elif data_modes and data_modes != self.configured_modes():
            self.deliver_secrets(data_modes=data_modes)

    def deploy(self, release, previous):
        """Deploy one release. Order matters: the old scheduler stops before
        anything else changes, migrations finish before services start, and
        the new scheduler starts last. Progress is saved after every step, so
        an interrupted deploy is resumed and a failed one is explained. Returns
        its record."""
        state = self.state()
        interrupted = state.get("deploying") or {}
        # An interrupted deploy that had not reached its point of no return
        # starts again from the installed release's own bundle and secrets.
        self.restore_installed_bundle(interrupted)
        if interrupted and self.committed(interrupted):
            shutil.rmtree(self.kept_secrets(), ignore_errors=True)
        elif interrupted and self.kept_secrets().is_dir():
            self.restore_installed_secrets(None)
        record = {"release": release, "previous": previous, "started": utc_now(), "steps": [],
                  "migrations_before": state.get("migrations")}
        if interrupted.get("release") == release:
            record["resumed_after"] = (interrupted.get("steps") or ["start"])[-1]
        record.update(self.carried_over(state, interrupted))

        def step(name):
            record["steps"].append(name)
            self.save_state({**self.state(), "deploying": record})

        step("started")
        moves = self.data_moves(state)
        # A move that finished, or one left partway whose storage was put back,
        # leaves no storage_moves for the rule below to see, and Hot is stopped
        # until its deploy finishes. A data move on top of that, if it failed, would
        # leave a bundle and delivered logins that disagree, and every deploy-now
        # would then stop on the database's level.
        progress = self.storage_progress(record)
        if moves and progress:
            settings = " and ".join(f"data.{service}" for service in moves)
            where, finish = (("a storage move finished, and the deploy it belongs to has not", " to finish that deploy")
                             if progress == "moved" else ("a storage move has not finished, and a data move must wait "
                                                          "for it", ""))
            raise AgentError(f"Move storage and data services in separate deploys: {where}. Put {settings} back, "
                             f"apply, and run deploy-now{finish}, then move the data in a deploy of its own")
        manifest = self.fetch_release(release)
        # Rendered first, beside the installed bundle: a configuration the
        # renderer or the operator bundle's checks refuse stops the deploy
        # before anything stops or is delivered. Any refusal up to the `secrets`
        # step leaves the services' files as they were (deliver_level_secrets).
        operator = self.operator_secrets()
        record["data_addresses"] = self.data_addresses(operator)
        staged = self.render_bundle(release, manifest, operator, install=False)
        level = self.database_level(previous, state, record, staged)
        check_upgrade(manifest, previous, level)
        record.update(images={name: image.rsplit("@", 1)[-1][:19] for name, image in manifest["images"].items()},
                      migrations=manifest["migrations"]["latest"], migrations_before=level,
                      configuration_schema=manifest.get("configuration_schema", 1), host=self.host_versions(),
                      host_addresses=self.host_addresses())
        step("verified")
        # Storage moves run through the installed bundle, before the new one
        # takes its place.
        storage_moves = self.storage_moves(previous) if previous else {}
        # A move drains Hot and may dump its database, which must be one the
        # installed release can run on: a dump of a partly migrated one would
        # take the place of the restore point from before those migrations, the
        # only one roll-back can use. A retry after the data moved has no moves.
        changing = [f"data.{service}.mode" for service in moves] + [f"storage.{role}" for role in storage_moves]
        if changing and record.get("partly_migrated"):
            # roll_back refuses once records may have moved.
            ways = ("put the setting back and deploy a fixed release" if self.storage_progress(record)
                    else "run roll-back, or put the setting back and deploy a fixed release")
            raise AgentError(f"The failed deploy of {record['partly_migrated']} may have partly applied its "
                             f"migrations, so {' and '.join(changing)} cannot change yet. Finish that upgrade first: "
                             f"{ways}. Then make this change in a deploy of its own")
        if storage_moves:
            if release != previous:
                raise AgentError("Change storage in a deploy of the release already running, then upgrade")
            if moves:
                raise AgentError("Move storage and data services in separate deploys")
            # A retry cut short before its bundle went in moves the same roles
            # again. Another role would take over the record of the move that
            # finished, and with it what that one left behind, which no report
            # would say then.
            joining = sorted(set(storage_moves) - set(record.get("storage_manifests", {})))
            if progress == "moved" and joining:
                settings = " and ".join(f"storage.{role}" for role in joining)
                raise AgentError(f"Another storage role must wait: a storage move finished, and the deploy it belongs "
                                 f"to has not. Put {settings} back, apply, and run deploy-now to finish that deploy, "
                                 "then move that storage in a deploy of its own")
            # Before anything is delivered or stops: a move that cannot write once
            # it has drained Hot, or whose list of what to leave behind it would
            # refuse, leaves it stopped, waiting for deploy-now.
            skip = self.storage_move_skip()
            try:
                skip_files = self.write_skip_files(record, storage_moves)
                self.wait_for_storage_write(storage_moves)
                self.start_installed_data(self.releases / previous / "bundle", state)
                # The move's worker reads the target bucket's keys, which may be
                # new in the operator bundle.
                self.deliver_secrets()
                self.move_storage(self.releases / previous / "bundle", storage_moves, record, step, operator, skip,
                                  skip_files)
            finally:
                self.remove_skip_files()
        # In a deploy of the running release, its own bundle stays beside the
        # new one: the Compose project is named after the installation, so
        # both address the same containers.
        bundle = self.install_bundle(release, keep=release == previous)
        step("rendered")
        installed = None
        if previous:
            installed = self.releases / previous / ("bundle-running" if release == previous else "bundle")
        self.pull_images(bundle)
        step("pulled")
        # A release whose image cannot give its hotbox stops before anything does.
        spare = self.copy_hotbox(bundle)
        if spare:
            spare.unlink()
        # A release refused for its images leaves the services' files as they
        # were too: the installed one reads them at its next restart.
        if not moves:
            self.deliver_secrets()
            step("secrets")
        else:
            # Hot stops completely, so nothing is written after the copy.
            record["moves"] = {service: f"{old} -> {new}" for service, (old, new) in moves.items()}
            step("draining")
            self.start_installed_data(installed, state)
            record["deadletter"] = self.drain(installed, queue="valkey" in moves)
            step("drained")
            if "postgres" in moves:
                dump = self.dump_postgres(installed, f"move-{release}") / "postgres.dump"
                self.prune_dumps("move-")
                record["restore_point"] = {"kind": "dump", "path": str(dump), "time": utc_now()}
                step("dumped")
            # Only now do the secrets change to the new data services. Until
            # the restore, a failure gives the installed release its own back.
            self.keep_secrets()
            self.deliver_secrets()
            step("secrets")
        if "scheduler" in self.roles():
            # Said first: the stop can take its whole grace period, and the agent
            # may die in it, or Compose fail after it took effect. Starting a
            # scheduler that never stopped does no harm.
            step("scheduler-stopping")
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
            # The schema changes when the release adds migrations, or when the
            # installed level is unknown. At the same level, migrate only
            # checks the database.
            schema_changes = level is None or record["migrations"] > level
            if previous and schema_changes:
                # Hot never migrates down: undoing these migrations means a restore.
                if not record.get("restore_point"):
                    record["restore_point"] = self.restore_point(bundle, release)
                step("restore-point")
            if schema_changes or record.get("partly_migrated"):
                # From here the database may hold some of these migrations.
                step("migrating")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
            step("migrated")
            if self.config.get("admin_email"):
                self.compose(bundle, "--profile", "admin", "run", "--rm", "bootstrap-admin")
                step("admin")
        # From here this release's services replace the installed release's.
        step("starting")
        self.start_services(bundle)
        step("ready")
        record["finished"] = utc_now()
        warnings = [self.storage_warning(record) if self.abandoned_storage(record) else "",
                    self.left_behind_warning(record)]
        if any(warnings):
            record["warning"] = " ".join(filter(None, warnings))
        migrated = {k: v for k, v in self.state().items() if k == "migrated_release"}
        # The new release serves now; data services' addresses are read again
        # when monitoring next needs them, so no failure here undoes that.
        self._dependencies = None
        self.save_state({"release": release, "previous": previous, "migrations": record["migrations"],
                         "data_modes": self.configured_modes(), "configuration": self.configuration_digest(),
                         "host_addresses": record["host_addresses"], "data_addresses": record["data_addresses"],
                         **migrated})
        self.settle_boot()
        shutil.rmtree(self.kept_secrets(), ignore_errors=True)
        # A kill leaves these, and a move put back never comes to remove them.
        self.remove_skip_files()
        self.remove_left_behind_files()
        self.remember(record)
        return record

    @staticmethod
    def carried_over(state, interrupted):
        """What this deploy inherits from attempts since the last successful
        deploy, whatever release they were for, so its recovery and roll-back
        see the data as those attempts left it: data or storage they moved, or
        migrations they may have partly applied, with the restore point from
        before them. Only those keep their restore point; any other database
        has taken writes since, and gets a new one. An attempt rolled back, or
        pointed back from, is over, so nothing carries over from it; nor from
        one whose roll-back got its database restored and checked. An attempt
        cut short also passes on what it stopped (stops), since nothing else
        will say so."""
        carried = {}
        settled = False
        for earlier in (interrupted, state.get("failed") or {}):
            if not earlier or earlier.get("rolled_back") or Agent.restore_checked(earlier):
                continue
            steps = earlier.get("steps", [])
            if "data-moved" in steps or earlier.get("data_moved"):
                carried["data_moved"] = True
            if "migrating" in steps or earlier.get("partly_migrated"):
                carried.setdefault("partly_migrated", earlier.get("partly_migrated") or earlier.get("release"))
                if earlier.get("restore_point"):
                    carried.setdefault("restore_point", earlier["restore_point"])
            progress = Agent.storage_progress(earlier)
            if progress:
                # An attempt whose move finished, or that began from one that had,
                # has the last word on what it failed or left behind: with nothing
                # to say, it took that off its record, and a failed record from
                # before it does not put that back. Not one cut short before its
                # bundle went in, though: the retry moves again, by the same report,
                # and when that move finishes it says for itself what is left.
                keys = ["storage_manifests"]
                if not settled:
                    keys += ["storage_failed", "storage_left_behind"]
                carried.setdefault("storage_progress", progress)
                for key in keys:
                    if earlier.get(key):
                        carried.setdefault(key, earlier[key])
                settled = progress == "moved" and ("storage-moved" not in steps or "rendered" in steps)
        left = Agent.stops(interrupted)
        if left:
            carried["cut_short"] = sorted(left)
        return carried

    @staticmethod
    def stops(record):
        """What a deploy, and any attempts cut short before it, did to the
        installed release's services: "scheduler-stopped", which counts from
        when it began to stop; "draining", a data move stopped all of Hot;
        "starting", this release's services replaced some of them; or "rebooted",
        a reboot left only the data services running (see resume). A killed agent
        leaves them as they were."""
        done = {*record.get("steps", []), *record.get("cut_short", [])}
        if "scheduler-stopping" in done:
            done.add("scheduler-stopped")
        if record.get("rebooted"):
            done.add("rebooted")
        return done & {"scheduler-stopped", "draining", "starting", "rebooted"}

    def database_level(self, installed, state, record=None, staged=None):
        """The migration level the upgrade rules go by: what the database's own
        ledger says, read with the installed bundle's job, not the level the state
        recorded. No restore lowers that, and nothing tells it that another Hot
        migrated further. The job leaves everything as it is.

        A host that does not migrate has no job and no migration login, and its
        recorded level is exactly what a restore leaves wrong, so it goes by none
        (None), and check_upgrade asks only for the oldest release this one upgrades
        from. The migration host compared this release with the ledger before the
        release helper moved any role's pointer. A role pointer set by hand to an
        earlier release is therefore no longer refused on these hosts.

        Without the job there is only the recorded level, which can be wrong, and
        the new release's migrate then refuses a database newer than it knows, once
        Hot has stopped:
        - nothing is installed yet, so nothing can stop;
        - the installed bundle or its Hot predates the job;
        - the data moved, and the installed bundle describes the copy it left.
        A job that fails is left to level_without_the_job, with the deploy's rendered
        bundle (`staged`). A database with no migrations where the state recorded some
        stops the deploy here, before anything else does: it is empty, or not this
        installation's, and migrating it would pass it off as this installation's
        data."""
        if not render.runs_migrations(self.config):
            return None
        recorded = state.get("migrations")
        if not installed or (record or {}).get("data_moved"):
            return recorded
        bundle = self.releases / installed / "bundle"
        try:
            if MIGRATION_STATUS_JOB not in json.loads((bundle / "compose.json").read_text())["services"]:
                return recorded
        except (OSError, ValueError, KeyError, TypeError):
            return recorded
        try:
            # A reboot leaves a stopped installation with no secrets or local database.
            self.start_installed_data(bundle, state)
            # It reads the database this deploy's jobs will, with the login the operator
            # secret bundle holds now, unless a data move still has to deliver those.
            if self.installed_modes(state) == self.configured_modes():
                self.deliver_level_secrets()
            level = self.read_migration_level(bundle)
        except LEVEL_READ_FAILURES as error:
            if isinstance(getattr(error, "stderr", None), str) and any(word in error.stderr for word in UNKNOWN_COMMAND):
                print(json.dumps({"migration_level_recorded": f"The Hot of {installed} cannot say its database's level"}),
                      file=sys.stderr, flush=True)
                return recorded
            level = self.level_without_the_job(error, state, record, staged)
        if recorded and not level:
            raise AgentError("The database has no migrations, though this installation's last deploy left it at "
                             f"{recorded}, so it is empty or not this installation's, and this deploy stopped before "
                             "changing anything. Check its connection, or restore it")
        return level

    def read_migration_level(self, bundle):
        """What a bundle's migration status job says the database's level is."""
        status = self.compose(bundle, "--profile", "admin", "run", "--rm", "--no-deps", "-T", MIGRATION_STATUS_JOB,
                              timeout=MIGRATION_STATUS_SECONDS)
        return migration_level(status.stdout)

    def level_without_the_job(self, failure, state, record, staged):
        """The level when the installed release's job failed. That need not be the
        database's doing: a private mirror may no longer hold the release's image,
        its command may be broken, or its local database may not start under its
        bundle. A deploy may be what fixes those, and these hosts have no shell, so
        two other sources count, while the data services are where the configuration
        says:
        - the job of the release being deployed, whose image is being pulled anyway:
          any Hot reads the ledger, whichever Hot wrote it;
        - the recorded level of a local database, the one place it cannot be wrong:
          the state is on the database's disk, so no restore parts them. Only a failed
          deploy that may have partly migrated it (carried_over) can.
        Otherwise the database cannot be read, or Hot cannot read it. That is no state
        to upgrade from, so the deploy stops here, before anything else does."""
        recorded = state.get("migrations")
        in_place = self.installed_modes(state) == self.configured_modes()
        reasons = [self.explain(failure)]
        if staged and in_place:
            try:
                # What failed may have come before the delivery, and its image is
                # not on this host until a current sign-in pulls it.
                self.deliver_level_secrets()
                self.registry_login()
                return self.read_migration_level(staged)
            except LEVEL_READ_FAILURES as error:
                reasons.append(f"the job of the release being deployed failed too: {self.explain(error)}")
        if (in_place and recorded is not None and render.data_mode(self.config, "postgres") == "local"
                and not (record or {}).get("partly_migrated")):
            print(json.dumps({"migration_level_recorded": "; ".join(reasons)}), file=sys.stderr, flush=True)
            return recorded
        raise AgentError("Could not read the database's migration level, so this deploy stopped before changing "
                         f"anything: {'; '.join(reasons)}") from failure

    # -- data moves --------------------------------------------------------------

    def configuration_digest(self):
        return hashlib.sha256(json.dumps(self.config, sort_keys=True).encode()).hexdigest()[:16]

    def configured_modes(self):
        return {service: render.data_mode(self.config, service) for service in ("postgres", "valkey")}

    def installed_modes(self, state):
        """Where the installed release's data services are: until a data move
        finishes, the old places, whatever the configuration asks for."""
        return {**self.configured_modes(), **(state.get("data_modes") or {})}

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
        {role: target}. Moves into S3 or GCS are automated on a single machine.
        Moving back onto the host, between buckets of one backend, or out of GCS
        is refused before anything stops, and so is any change of backend, bucket,
        or prefix on a scalable installation, whose hosts do not move storage."""
        bundle = self.releases / previous / "bundle"
        if not (bundle / "compose.json").exists():
            return {}
        deployed, configured = self.deployed_storage(bundle), self.configured_storage()
        moves = {role: configured[role] for role in configured if configured[role] != deployed[role]}
        if render.scalable(self.config):
            for role, target in moves.items():
                if target["backend"] != deployed[role]["backend"]:
                    raise AgentError(f"A scalable installation does not move storage, and {role} storage changed from "
                                     f"{deployed[role]['backend']} to {target['backend']}: Hot would start on it with "
                                     f"its records still on the old backend. Put storage.{role} back")
                # As the stacks' plans compare it: a prefix reads the same with or
                # without a trailing slash. A bucket has one region, so a new region
                # only corrects the setting.
                was, now = (f"{side['bucket']}/{side['prefix'].strip('/')}" for side in (deployed[role], target))
                if was != now:
                    raise AgentError(f"A scalable installation does not move storage, and {role} storage changed from "
                                     f"{was} to {now}: Hot would start on it with its records still on the old one. "
                                     f"Put storage.{role} back")
            return {}
        for role, target in moves.items():
            if target["backend"] == "local":
                raise AgentError(f"Moving {role} storage back onto the host is not automated, and no agent command "
                                 f"runs hot db move-storage for it. Put storage.{role} back")
            if target["backend"] == deployed[role]["backend"]:
                raise AgentError(f"Moving {role} storage to another {target['backend']} bucket or prefix is not "
                                 "automated: records name their bucket, and hot db move-storage refuses it, so copying "
                                 f"the objects is not enough. Put storage.{role} back")
            # The stack grants this host an existing bucket only while a role uses
            # it, and removes that before the VM sees the change, so the move would
            # stop Hot and then be refused. A check could still pass while that
            # takes effect. A provisioned bucket's plan fails instead.
            if deployed[role]["backend"] == "gcs":
                raise AgentError(f"Moving {role} storage out of GCS is not automated: the apply that changed it also "
                                 f"removes this host's access to gs://{deployed[role]['bucket']}, so the move could not "
                                 f"read the objects. Hot cannot read them either until you put storage.{role} back "
                                 "and apply, which restores that access")
        return moves

    def start_installed_data(self, bundle, state):
        """Before a data or storage move: a reboot while Hot was stopped left
        the tmpfs empty and the local data services down, since resume leaves a
        stopped installation alone, and the move needs them. So do as resume
        does, from the installed bundle. Nothing is recreated, and a running
        installation needs none of this. Neither has one whose resume has yet to
        run this boot, though a command may have delivered its secrets."""
        modes = state.get("data_modes") or self.configured_modes()
        delivered = self.secrets_root.is_dir() and any(self.secrets_root.iterdir())
        if not delivered:
            self.deliver_secrets(data_modes=modes)
        if delivered and self.boot_settled() and not state.get("stopped"):
            return
        self.registry_login()
        local_data = [service for service in ("postgres", "valkey") if modes.get(service) == "local"]
        if local_data:
            self.compose(bundle, "up", "-d", "--wait", "--no-recreate", *local_data)

    def wait_for_storage_write(self, moves, timeout=GRANT_WAIT_SECONDS, clock=time.monotonic):
        """Before a move into GCS, wait until this host can write under each
        target's prefix, with the VM identity the move's worker uses. Raises
        AgentError if it still cannot."""
        for role, target in moves.items():
            if target["backend"] != "gcs":
                continue
            bucket, prefix = target["bucket"], target["prefix"]
            try:
                self.wait_for_grant(lambda: self.cloud.check_storage_write(bucket, prefix),
                                    lambda error: isinstance(error, AgentError), f"storage.{role}", timeout, clock)
            except AgentError as error:
                raise AgentError(f"This host could not write to gs://{bucket}/{prefix} within {timeout // 60} minutes, "
                                 f"so {role} storage did not start to move: {self.explain(error)}. "
                                 "Check the host's grant on that bucket") from error

    def move_storage(self, bundle, moves, record, step, operator, skip, skip_files):
        """Stop Hot, then move each role's objects with hot db move-storage in
        the installed release's worker, which still has the source settings.
        A bucket reached with access keys gets the operator bundle's keys for
        its role, which that worker may not mount yet; they are delivered
        first. Each object's record changes only after its copy checks out,
        so the move resumes where it stopped. Each object, moved or failed, goes
        to the manifest on the data disk as it finishes, so a move that fails or
        is cut short keeps the only record of what it rewrote; the failed record
        names objects that did not move. Those settings.HOT_MOVE_STORAGE_SKIP
        lists (`skip`), which can never move, stay behind, and so do those in a
        role's file in `skip_files` (write_skip_files), which only the job's
        user reads, and which a release without the flag refuses."""
        skip = ",".join(skip)
        record["storage_moves"] = {role: target["backend"] for role, target in moves.items()}
        step("storage-moving")
        self.drain(bundle, queue=False)
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.backups.mkdir(parents=True, exist_ok=True)
        worker = json.loads((bundle / "compose.json").read_text())["services"]["worker"]
        mounted = {secret["target"] for secret in worker.get("secrets", [])}
        for role, target in moves.items():
            command = ["run", "--rm", "--no-deps"]
            if target["backend"] == "s3":
                for key in (f"HOT_{role.upper()}_AWS_{suffix}" for suffix in S3_KEYS):
                    if key in operator and key not in mounted:
                        command += ["-e", f"{key}_FILE=/run/secrets/{key}",
                                    "-v", f"{self.secrets_root / 'worker' / key}:/run/secrets/{key}:ro"]
            # The installed bundle has the list its last render saw, or none.
            if skip or MOVE_SKIP_SETTING in worker.get("environment", {}):
                command += ["-e", f"{MOVE_SKIP_SETTING}={skip}"]
            if role in skip_files:
                command += ["-v", f"{skip_files[role]}:{MOVE_SKIP_FILE_IN_JOB}:ro"]
            command += ["worker", "db", "move-storage", "--role", role, "--to", target["backend"],
                        "--bucket", target["bucket"], "--prefix", target["prefix"], "--manifest", "-"]
            if target["region"]:
                command += ["--region", target["region"]]
            if role in skip_files:
                command += [MOVE_SKIP_FILE_FLAG, MOVE_SKIP_FILE_IN_JOB]
            manifest = self.backups / f"storage-move-{role}-{stamp}.jsonl"
            record.setdefault("storage_manifests", {})[role] = str(manifest)
            self.save_state({**self.state(), "deploying": record})
            try:
                with open(manifest, "a", encoding="utf-8") as output:
                    self.compose(bundle, *command, stdout=output)
            except subprocess.CalledProcessError as error:
                # A partly failed move exits 1 after listing every object. One
                # that refused its skip file lists none, and the report stands.
                failed = self.failed_objects(manifest, role)
                if failed:
                    # The next attempt starts a manifest of its own, which may
                    # never finish: this one goes with what the report says.
                    record["storage_failed"] = {**failed, "manifest": str(manifest)}
                    self.keep_left_behind(record, skip_files)
                self.save_state({**self.state(), "deploying": record})
                # Arguments it does not know exit 2. Hot's own failures exit 1,
                # and the one that says objects did not move names the flag too.
                if role in skip_files and error.returncode == 2 and MOVE_SKIP_FILE_FLAG in (error.stderr or ""):
                    raise NoSkipFileFlag(f"Release {record['release']} has no `hot db move-storage {MOVE_SKIP_FILE_FLAG}`, "
                                         f"so settings.{MOVE_SKIP_FAILED_SETTING} cannot leave objects behind on it, "
                                         f"and its {role} objects did not move. Remove that setting, then list their "
                                         f"ids in settings.{MOVE_SKIP_SETTING} or fix the cause. Then apply and run "
                                         "deploy-now") from error
                raise
        record.pop("storage_failed", None)
        self.count_left_behind(record, moves)
        step("storage-moved")

    @staticmethod
    def abandoned_storage(record):
        """The roles an attempt before this deploy began to move and this one,
        now that it finished, did not: their storage was put back, whatever else
        changed with it. A move that finished earlier leaves nothing behind."""
        if record.get("storage_progress") != "moving":
            return []
        finished = record.get("storage_moves", {}) if "storage-moved" in record.get("steps", []) else {}
        return sorted(set(record.get("storage_manifests", {})) - set(finished))

    @staticmethod
    def storage_warning(record):
        """What it costs to run without finishing a storage move, for the report
        of whatever started Hot that way: the operator's choice, and not free."""
        manifests = ", ".join(record.get("storage_manifests", {}).values())
        return ("Records already moved stay on the new storage, and Hot cannot read them until the move finishes: set "
                "that storage again, apply, and run deploy-now, which resumes where it stopped."
                + (f" Each object is listed in {manifests}." if manifests else ""))

    @staticmethod
    def left_behind_warning(record):
        """What a finished move left on the old storage, for the report of the
        deploy that finishes it: however it was asked for, this says how much."""
        left = record.get("storage_left_behind") or {}
        if not left or Agent.storage_progress(record) != "moved":
            return ""
        manifests = record.get("storage_manifests", {})
        listed = ", ".join(manifests[role] for role in sorted(left) if role in manifests)
        return (f"{sum(left.values())} object(s) stayed behind on the old storage, and Hot cannot read them: a file left "
                "there can only be deleted, and a build must be deployed again."
                + (f" Each is listed as skipped in {listed}." if listed else ""))

    def storage_move_skip(self):
        """The ids settings.HOT_MOVE_STORAGE_SKIP lists. A setting is what an
        operator can reach, since these hosts have no shell. One that is not an
        id is refused here, before a move stops anything: the worker would
        refuse it only after that."""
        listed = (self.config.get("settings") or {}).get(MOVE_SKIP_SETTING, "")
        ids = [entry.strip() for entry in listed.split(",") if entry.strip()]
        for entry in ids:
            if not OBJECT_ID.fullmatch(entry):
                raise AgentError(f"settings.{MOVE_SKIP_SETTING} must list object ids separated by commas, "
                                 f"and {entry[:40]!r} is not one")
        return ids

    def write_skip_files(self, record, moves):
        """What settings.HOT_MOVE_STORAGE_SKIP_FAILED leaves behind, as {role:
        file of ids, one to a line}, for a job that mounts it: every object the
        last report said did not move, when the setting names that report's count,
        listed from that move's manifest. An outage or bad credentials can fail
        every object, so a setting that is stale or careless, or that no report
        stands behind, is refused here, before a move stops anything, and leaves
        nothing behind. What this setting left behind in attempts since then
        (keep_left_behind) goes in too, for as long as it is there: with only the
        newest report's, each report's objects would fail again, the other role's
        or the last round's, and the move would never finish. Taking it out tries
        them all again. A manifest's `skipped` is not the list: it also holds what
        settings.HOT_MOVE_STORAGE_SKIP left behind, which goes by that setting, so
        an id taken off it moves."""
        self.remove_skip_files()
        settings = self.config.get("settings") or {}
        if MOVE_SKIP_FAILED_SETTING not in settings:
            record.pop("storage_left_behind", None)
            self.remove_left_behind_files()
            return {}
        name, wanted = f"settings.{MOVE_SKIP_FAILED_SETTING}", str(settings[MOVE_SKIP_FAILED_SETTING])
        if not re.fullmatch(r"[1-9][0-9]*", wanted):
            raise AgentError(f"{name} must be a whole number of 1 or more, the count of objects the last storage move "
                             "reported did not move")
        # The configuration takes a count of any length, and a report has only
        # so much room. It is compared as text: int() refuses a long enough one.
        told = wanted if len(wanted) <= 40 else f"{wanted[:40]}..."
        report = record.get("storage_failed")
        if not report:
            raise AgentError(f"{name} is {told}, and no storage move has reported objects that did not move, so it "
                             "has nothing to leave behind. Remove it, apply, then run deploy-now")
        role, count = report["role"], report["count"]
        left = {other: many for other, many in self.left_behind(record).items() if other in moves}
        if wanted != str(count):
            raise AgentError(f"{name} is {told}, and the last report said {count} {role} objects did not move, so "
                             f"nothing was left behind. To leave them all behind, set it to {count}."
                             f"{self.left_behind_in_all(left, count)} Otherwise remove it and fix the cause. Then "
                             "apply and run deploy-now")
        if role not in moves:
            raise AgentError(f"{name} is {wanted}, and the {count} objects the last report named are {role} objects, "
                             "whose storage is no longer moving. Remove it, apply, then run deploy-now")
        manifests = record.get("storage_manifests") or {}
        manifest = report.get("manifest") or manifests.get(role, "")
        again = (f"list their ids in settings.{MOVE_SKIP_SETTING}, or remove {name}, apply, and run deploy-now, whose "
                 "report lists them again")
        try:
            failed = self.write_skip_file(role, manifest, left)
        except OSError as error:
            raise AgentError(f"{name} is {wanted}, and the objects could not be listed from that move's manifest: "
                             f"{describe(error)}. To leave them behind without it, {again}") from error
        if failed != count:
            raise AgentError(f"{name} is {wanted}, as the last report said, and that move's manifest {manifest} lists "
                             f"{failed}, so nothing was left behind. To leave them behind, {again}")
        files = {role: self.skip_file(role)}
        for other in moves:
            if other != role and other in left:
                with contextlib.suppress(OSError):
                    self.write_skip_file(other, None, left)
                    files[other] = self.skip_file(other)
        return files

    def skip_file(self, role):
        return self.backups / f"storage-skip-{role}.txt"

    def write_skip_file(self, role, manifest, left):
        """List the ids a manifest, if any, says failed, then those this role
        has in `left`, in a file only the job's user can read: how many failed."""
        self.backups.mkdir(parents=True, exist_ok=True)
        path = self.skip_file(role)
        path.touch(mode=0o600)
        os.chmod(path, 0o600)
        self.chown(path, JOB_USER, JOB_USER)
        failed = 0
        with open(path, "w", encoding="utf-8") as output:
            for entry in self.manifest_entries(manifest) if manifest else ():
                if entry.get("status") != "failed":
                    continue
                # The job would refuse it, with Hot stopped by then.
                identifier = str(entry.get("id") or "")
                if not OBJECT_ID.fullmatch(identifier):
                    raise AgentError(f"The storage move manifest {manifest} names {identifier[:40]!r}, which is not an "
                                     "object id, so its objects cannot be left behind by count")
                output.write(identifier + "\n")
                failed += 1
            if role in left:
                with open(self.left_behind_file(role), encoding="utf-8") as earlier:
                    shutil.copyfileobj(earlier, output)
        return failed

    def remove_skip_files(self):
        for path in self.backups.glob("storage-skip-*.txt"):
            path.unlink(missing_ok=True)

    def left_behind_file(self, role):
        return self.backups / f"storage-left-behind-{role}.txt"

    def keep_left_behind(self, record, skip_files):
        """A new report takes the place of the one `skip_files` were written from,
        so the next files start from it. What these listed goes on staying behind,
        in a file of its own, and the record says how many, {role: count}. One that
        cannot be kept only fails again, and a report names it."""
        left = {}
        for role, path in skip_files.items():
            with contextlib.suppress(OSError):
                with open(path, encoding="utf-8") as lines:
                    count = sum(1 for _ in lines)
                os.replace(path, self.left_behind_file(role))
                left[role] = count
        if left:
            record["storage_left_behind"] = left

    def left_behind(self, record):
        """What keep_left_behind kept and is still there: an attempt that took the
        setting out removes the files, and a kill can leave the record naming them."""
        return {role: count for role, count in (record.get("storage_left_behind") or {}).items()
                if self.left_behind_file(role).exists()}

    def remove_left_behind_files(self):
        for path in self.backups.glob("storage-left-behind-*.txt"):
            path.unlink(missing_ok=True)

    @staticmethod
    def left_behind_in_all(left, count):
        """How many stay behind in all if `count` more do, where that is not `count`."""
        earlier = sum(left.values())
        if not earlier:
            return ""
        return f" {earlier + count} then stay behind in all; taking the setting out tries them all again."

    def count_left_behind(self, record, moves):
        """What a finished move left on the old storage, each role's as its manifest
        says, whichever setting asked for it. A manifest that cannot be read costs
        the warning only, since these records have all changed. What a move finished
        before this one left stands."""
        left = dict(record.get("storage_left_behind") or {}) if record.get("storage_progress") == "moved" else {}
        for role in moves:
            left.pop(role, None)
            with contextlib.suppress(OSError):
                entries = self.manifest_entries(record["storage_manifests"][role])
                count = sum(entry.get("status") == "skipped" for entry in entries)
                if count:
                    left[role] = count
        record.pop("storage_left_behind", None)
        if left:
            record["storage_left_behind"] = left

    @staticmethod
    def manifest_entries(manifest):
        """A move's manifest, a line at a time, since it can hold millions of
        objects. A last line cut short is passed over."""
        with open(manifest, encoding="utf-8", errors="replace") as lines:
            for line in lines:
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                if isinstance(entry, dict):
                    yield entry

    @staticmethod
    def failed_objects(manifest, role, listed=STORAGE_FAILED_LISTED):
        """How many objects a move's manifest lists as failed, and the first
        `listed` ids, as the failed record carries them, or None."""
        count, ids = 0, []
        for entry in Agent.manifest_entries(manifest):
            if entry.get("status") == "failed":
                count += 1
                if len(ids) < listed and entry.get("id"):
                    ids.append(str(entry["id"]))
        return {"role": role, "count": count, "ids": ids} if count else None

    def drain(self, bundle, queue, timeout=DRAIN_SECONDS, clock=time.monotonic):
        """Stop admissions (scheduler, app, API, ingress), let workers finish
        queued work when the queue is moving, then stop the workers. Returns
        the dead-lettered items left behind in the old queue. Workers that
        are stopped, as after drain-and-stop, start to finish that work; nothing
        running is recreated, whichever of the release's bundles this is."""
        self.compose(bundle, "stop", "scheduler")
        self.compose(bundle, "stop", "app", "api", "ingress")
        deadletter = 0
        if queue:
            self.install_hotbox(bundle)
            self.compose(bundle, "up", "-d", "--no-recreate", "worker", "task-worker")
            waited, unread = 0, ""
            while True:
                asked = clock()
                try:
                    backlog, deadletter = self.queue_backlog(bundle)
                except subprocess.TimeoutExpired as error:
                    # No answer counts toward the wait; the next may answer.
                    backlog, unread = None, self.explain(error)
                    waited += int(error.timeout)
                except (AgentError, subprocess.CalledProcessError) as error:
                    # So does an answer with no totals, as from a Valkey that
                    # stalled or is failing over, however long that took.
                    backlog, unread = None, self.explain(error)
                    waited += int(clock() - asked)
                if backlog == 0:
                    break
                if waited >= timeout:
                    items = "Queued items" if backlog is None else f"{backlog} queued items"
                    reason = f". The queue's status could not be read: {unread}" if backlog is None else ""
                    raise AgentError(f"{items} did not finish within {timeout // 60} minutes; "
                                     f"the data move was not started{reason}")
                self.sleep(10)
                waited += 10
        self.compose(bundle, "stop", "--timeout", "120", "worker", "task-worker")
        return deadletter

    def queue_status(self, bundle, service="worker"):
        """Hot's queue from `hot queue status` in a running service: per
        queue, what waits (pending), what runs (processing), what was
        dead-lettered, and how long the oldest waiting item has waited. Raises
        TimeoutExpired when the service does not answer in EXEC_SECONDS, and
        AgentError when it answers without the totals, as when it cannot read
        the queue."""
        command = ("exec", "-T", service, "/usr/bin/hot", "queue", "status")
        try:
            return json.loads(self.compose(bundle, *command, "--json", timeout=EXEC_SECONDS).stdout)
        except (subprocess.CalledProcessError, ValueError):
            pass
        # Releases before --json: only the totals, from the text report.
        output = self.compose(bundle, *command, timeout=EXEC_SECONDS).stdout
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
        # A copy a reboot or kill left behind, which no `finally` removed, would
        # count against the room for this one.
        copy.unlink(missing_ok=True)
        try:
            self.check_staging_room(dump, staged, "deploy-now")
            copy.touch(mode=0o600)
            os.chmod(copy, 0o600)
            self.chown(copy, JOB_USER, JOB_USER)
            shutil.copyfile(dump, copy)
            self.compose(bundle, "--profile", "admin", "run", "--rm", "restore-postgres")
        finally:
            copy.unlink(missing_ok=True)

    @staticmethod
    def check_staging_room(dump, directory, retry):
        """A dump's staged copy must fit on the data disk beside the database,
        with room to spare; `retry` is the command to run once it does. These
        hosts have no shell to free space with, so the way out is a larger disk."""
        size, free = dump.stat().st_size, shutil.disk_usage(directory).free
        if free < size + STAGING_RESERVE_BYTES:
            raise AgentError(f"Staging the database dump for the restore needs {size // 2**20} MB, and the data disk "
                             f"has {free // 2**20} MB free; grow the disk with a larger size, then run {retry}")

    def recover(self, release, previous, error):
        """After a failed deploy: return to the previous release when the
        database was not touched, record the failure, and say what recovery
        means. Hot stays stopped, across reboots too, when its data or
        storage moved partway or completely, when drain-and-stop had stopped
        it, and when a data move or a reboot had stopped it and migrations may
        be partly applied. The failed release is not retried until asked."""
        state = self.state()
        record = state.get("deploying") or {}
        if record.get("release") != release:
            # It failed before deploy() began its record, as in prepare_host;
            # what earlier attempts left still stands.
            self.restore_installed_bundle(state.get("deploying") or {})
            record = {"release": release, "previous": previous, "steps": [],
                      **self.carried_over(state, state.get("deploying") or {})}
        steps = record["steps"]
        stopped = bool(state.get("stopped"))
        manifests = ", ".join(record.get("storage_manifests", {}).values())
        listed = f" Each object moved or failed is listed in {manifests}." if manifests else ""
        progress = self.storage_progress(record)
        if progress == "moving":
            named = record.get("storage_failed")
            recovery = ("Some objects may already be in the new storage, so Hot stays stopped." + listed
                        + (f" {named['count']} did not move: storage_failed in the failed record names the first "
                           f"{len(named['ids'])}, and diagnostics up to {STORAGE_FAILED_DIAGNOSED}." if named else "")
                        + " Fix the cause, then run deploy-now: the move skips objects already moved."
                        f" An object that can never move can be left behind: add its id to settings.{MOVE_SKIP_SETTING}"
                        ", separated by commas, apply that, then run deploy-now."
                        + (f" To leave all {named['count']} behind, set settings.{MOVE_SKIP_FAILED_SETTING} to "
                           f"{named['count']} instead." + self.left_behind_in_all(self.left_behind(record), named["count"])
                           if named and not isinstance(error, NoSkipFileFlag) else "")
                        + " Hot can't read what stays behind: "
                        "a file left there can only be deleted, and a build must be deployed again."
                        + (" Migrations may be partly applied too." if self.partly_migrated(record) else ""))
            stopped = True
        elif progress == "moved":
            recovery = ("The objects moved to the new storage, and their records point there, so Hot stays "
                        "stopped until a deploy with that storage finishes. Fix the cause, then run deploy-now.")
            stopped = True
        elif "data-moved" in steps or record.get("data_moved"):
            recovery = ("The data now lives in the new database, and the copy on this host is no longer used"
                        + ("; migrations may be partly applied" if self.partly_migrated(record) else "")
                        + ". Hot stays stopped. Fix the cause, then run deploy-now.")
            stopped = True
        elif self.partly_migrated(record):
            point = record.get("restore_point") or {}
            recovery = "Migrations may be partly applied, and Hot does not migrate down. Deploy a fixed release"
            if not previous:
                recovery += "."
            elif point.get("kind") == "dump":
                recovery += (f", or run roll-back: it restores the database from {point['path']} and starts "
                             f"{previous}. Then point the installation back at {previous}.")
            else:
                recovery += f", or {self.restore_steps(point, previous)}."
            # A data move or a reboot had stopped all of Hot, and only these two
            # ways start it on this database, with its own secrets back.
            stopped = stopped or bool(previous and {"draining", "rebooted"} & self.stops(record))
            if stopped:
                recovery = "Hot stays stopped. " + recovery
                with contextlib.suppress(AgentError, OSError, subprocess.SubprocessError):
                    self.restore_installed_secrets(state.get("data_modes"))
        else:
            recovery = self.return_to_previous(record, state)
        if stopped and previous:
            # Whatever this attempt or one cut short started stops again: workers
            # started to drain the queue, or services a failed start left up.
            with contextlib.suppress(AgentError, OSError, subprocess.SubprocessError):
                self.stop_services(self.releases / previous / "bundle")
        # The installed release's own secrets are back, or no longer needed.
        shutil.rmtree(self.kept_secrets(), ignore_errors=True)
        record.update(error=self.explain(error), recovery=recovery, finished=utc_now())
        failed = {key: record[key] for key in ("release", "steps", "error", "recovery", "restore_point",
                                               "partly_migrated", "data_moved", "storage_progress",
                                               "storage_manifests", "storage_failed", "storage_left_behind")
                  if key in record}
        failed["at"] = record["finished"]
        kept = {k: v for k, v in state.items() if k not in ("deploying", "stopped")}
        self.save_state({**kept, **({"stopped": True} if stopped else {}), "failed": failed})
        self.remember(record)
        return failed

    @staticmethod
    def partly_migrated(record):
        """Whether a deploy, or an attempt before it, began this release's
        migrations: the database may hold some of them."""
        return "migrating" in record.get("steps", []) or bool(record.get("partly_migrated"))

    @staticmethod
    def fix_forward(failed):
        """The way on from migrations that may be partly applied when records may
        have moved too, so roll-back refuses. A move left partway goes back to the
        old storage first, since a move waits for that upgrade; one that finished
        cannot go back, and the installed bundle already reads the new storage."""
        storage = "the storage setting put back" if Agent.storage_progress(failed) == "moving" else "that storage kept"
        return f"point the installation at a fixed release, with {storage}"

    @staticmethod
    def migrated_by(failed):
        """The release whose deploy may have partly migrated the database: a
        later attempt refused before it ran anything fails under its own name."""
        return failed.get("partly_migrated") or failed.get("release")

    @staticmethod
    def restore_checked(failed):
        """Whether a roll-back of this failed deploy restored the database and
        saw it pass the installed release's checks, whatever came of starting
        Hot on it."""
        return "checked" in (failed.get("roll_back_failed") or {}).get("steps", [])

    @staticmethod
    def database_restore(point):
        """How an operator restores a database roll-back cannot, in a few words."""
        when = f" to {point['time']}" if point.get("time") else ""
        return f"restore the database{when} with its provider's point-in-time recovery or your own backups"

    @staticmethod
    def restore_steps(point, release):
        """The way back from migrations on such a database. Hot stops first, so
        nothing writes to the database as it is restored, and only a stopped Hot
        starts whole when `release` points back at it (settle_pointed_back)."""
        return f"run drain-and-stop, {Agent.database_restore(point)}, then release {release}"

    def return_to_previous(self, record, state):
        """For a deploy that failed before the database changed: run the
        installed release as before, with its own bundle and, after a data
        move, its own secrets, or keep Hot stopped as it was before the
        deploy (drain-and-stop, or an earlier failed move). A failed roll-back
        whose database passed its checks may have started only part of Hot, and
        this failure takes the place of its record, and of the note that said
        to run it again, so Hot starts whole here. Returns the recovery text."""
        release, previous, steps = record["release"], record.get("previous"), record["steps"]
        if not previous:
            return "Nothing was installed. Fix the cause, then run deploy-now."
        stops = self.stops(record)
        installed = self.releases / previous / "bundle"
        try:
            self.restore_installed_bundle(record)
            if state.get("stopped"):
                # What a move delivered and nothing else: its local data services
                # restart from these files, and a retry reads the database's level
                # with them. One that delivered nothing has its files as they were.
                self.restore_installed_secrets(None)
                return (f"The database was not changed, and Hot stays stopped, as it was before this deploy. Fix "
                        f"the cause, then run deploy-now, or point the installation back at {previous} to start it.")
            if {"draining", "starting", "rebooted"} & stops or self.restore_checked(state.get("failed") or {}):
                # A data move stopped all of Hot, this release's services
                # replaced some of the installed one's, a reboot left only
                # the data services running, or a roll-back did not finish its start.
                self.restore_installed_secrets(state.get("data_modes"))
                self.start_services(installed)
                steps.append("previous-release-restarted")
                state.pop("restarting", None)
            else:
                waiting = state.get("restarting") or {}
                if "scheduler-stopped" in stops and "scheduler" in self.roles() and "scheduler" not in waiting:
                    self.compose(installed, "up", "-d", "scheduler")
                    steps.append("previous-scheduler-restarted")
                # What a restart cut short left down: it keeps running only once
                # it runs again. A start after a boot has that to do, if one is due.
                if self.boot_settled():
                    for service, request_id in sorted(waiting.items()):
                        self.restart(installed, service, request_id)
                        steps.append(f"previous-{service}-restarted")
                    state.pop("restarting", None)
        except (AgentError, OSError, subprocess.CalledProcessError) as restart:
            explained = self.explain(restart)
            # Saved with the failure: the loop tries again, and a later failure
            # does not say this release keeps running.
            state["resume_failed"] = {"release": previous, "error": explained, "at": utc_now()}
            # Scalable hosts have no roll-back: the AWS command document leaves it
            # out, and the GCP helper refuses it.
            then = (f"run deploy-now to retry {release}" if render.scalable(self.config)
                    else f"run roll-back to start {previous} at once, or deploy-now to retry {release}")
            return (f"The database was not changed, but {previous} did not start again: {explained}. That start is "
                    f"tried again every {RESUME_RETRY_SECONDS // 60} minutes. Fix the cause, then {then}.")
        failure = state.get("resume_failed")
        if failure and "previous-release-restarted" not in steps:
            # A deploy that failed before it touched anything leaves that as it was.
            return (f"The database was not changed, but {previous} has not started again: "
                    f"{failure.get('error')}. That start is tried again every {RESUME_RETRY_SECONDS // 60} minutes. "
                    "Fix the cause, then run deploy-now, or point the installation at another release.")
        if "previous-release-restarted" not in steps and not self.boot_settled():
            # On AWS a command is its own process, which can get here before the
            # loop's first cycle after a boot.
            return (f"The database was not changed, and {previous} has yet to start after this boot: the next cycle "
                    "starts it. Fix the cause, then run deploy-now, or point the installation at another release.")
        return (f"The database was not changed and {previous} keeps running. Fix the cause, then run deploy-now, "
                "or point the installation at another release.")

    def restore_point(self, bundle, release):
        """Where to recover the database from if this release's migrations
        must be undone: a dump of a local database, or the time to restore a
        managed or existing one to."""
        mode = render.data_mode(self.config, "postgres")
        if mode != "local":
            return {"kind": "point-in-time" if mode == "managed" else "operator", "time": utc_now()}
        target = self.dump_postgres(bundle, f"pre-{release}")
        self.prune_dumps("pre-")
        return {"kind": "dump", "path": str(target / "postgres.dump"), "time": utc_now()}

    def prune_dumps(self, prefix):
        """Keep the newest UPGRADE_DUMPS dumps whose names start with
        `prefix`: pre-upgrade dumps, or the dumps of data-move attempts."""
        dumps = sorted(self.backups.glob(prefix + "*"), key=lambda path: path.stat().st_mtime)
        for old in dumps[:-UPGRADE_DUMPS]:
            shutil.rmtree(old)

    def dump_postgres(self, bundle, label):
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.backups.mkdir(parents=True, exist_ok=True)
        # The dump goes on the data disk beside the database, so one that
        # would fill it is refused before it starts: the disk must hold
        # another like the newest dump, with room to spare. What a reboot or
        # kill left of the last one does not count against that.
        (self.data_root / "data/postgres/backup.dump").unlink(missing_ok=True)
        dumps = sorted(self.backups.glob("*/postgres.dump"), key=lambda path: path.stat().st_mtime)
        expected = dumps[-1].stat().st_size if dumps else 0
        free = shutil.disk_usage(self.backups).free
        if free < expected + STAGING_RESERVE_BYTES:
            raise AgentError(f"A database dump needs about {(expected + STAGING_RESERVE_BYTES) // 2**20} MB, and the "
                             f"data disk has {free // 2**20} MB free; grow the disk with a larger size, then try again")
        target = self.backups / f"{label}-{stamp}"
        target.mkdir(parents=True, exist_ok=True, mode=0o700)
        try:
            self.compose(bundle, "exec", "-T", "postgres", "pg_dump", "-U", "hot_owner", "-d", "hot",
                         "--format=custom", "--file=/var/lib/postgresql/backup.dump")
            shutil.move(str(self.data_root / "data/postgres/backup.dump"), str(target / "postgres.dump"))
        except BaseException:
            # No half-made backup is left to be mistaken for a good one.
            shutil.rmtree(target, ignore_errors=True)
            (self.data_root / "data/postgres/backup.dump").unlink(missing_ok=True)
            raise
        return target

    def host_versions(self):
        try:
            return json.loads((self.host / "versions.json").read_text())
        except (OSError, ValueError):
            return {}

    def hotbox_version(self):
        """Which hotbox this host's boxes get, for diagnostics: the release's own,
        which a start of the task worker installed, or with no such file the host
        image's fallback, the only one versions.json's hotbox_from describes. None
        on a host with no task worker."""
        if "task-worker" not in self.roles():
            return None
        fallback_from = self.host_versions().get("hotbox_from")
        try:
            digest = hashlib.sha256((self.data_root / "kata/hotbox").read_bytes()).hexdigest()
        except OSError:
            return {"in_use": "fallback", "fallback_from": fallback_from}
        return {"in_use": "installed", "installed_sha256": digest, "fallback_from": fallback_from}

    def history(self):
        try:
            return json.loads((self.releases / "history.json").read_text())
        except (OSError, ValueError):
            return []

    def remember(self, record):
        write_atomic(self.releases / "history.json", json.dumps((self.history() + [record])[-HISTORY:], indent=1))

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

    def refresh_configuration(self):
        """Use a configuration the stack changed in place (an SSM parameter
        on AWS, instance metadata on GCP): the next reconcile sees a new
        configuration digest and redeploys with it, without a reboot or a
        new VM."""
        try:
            current = getattr(self.cloud, "installation", lambda reference: None)(
                self.config.get("installation_parameter", ""))
        except (AgentError, OSError, ValueError):
            return False
        if not current or current == self.config:
            return False
        # The next boot reads it, so it is never left half written.
        write_atomic(self.etc / "installation.json", json.dumps(current))
        self.config = current
        self.references = current["secret_references"]
        self._dependencies = None
        return True

    def dependencies(self):
        """(host, port) of the managed and existing data services the installed
        release uses, which a failed data move's target is not. Kept in
        memory, not in the state file: existing services' addresses come
        from the operator's connection URIs. They are read again every
        DEPENDENCIES_SECONDS, so a URI changed in the operator secret bundle
        is probed from then on, also when another process restarted Hot."""
        read = getattr(self, "_dependencies_read", None)
        if (getattr(self, "_dependencies", None) is None or read is None
                or time.monotonic() - read >= DEPENDENCIES_SECONDS):
            installed = self.installed_config(self.state().get("data_modes"))
            self._dependencies = data_services(installed, self.operator_secrets())
            self._dependencies_read = time.monotonic()
        return self._dependencies

    def dependencies_reachable(self):
        """Whether every managed and existing data service accepts
        connections; False when the operator secret cannot be read."""
        try:
            return all(tcp_reachable(host, port) for host, port in self.dependencies())
        except (AgentError, KeyError, TypeError, ValueError):  # unreadable, or a malformed URI
            return False

    def save_state(self, state):
        # Deploy history, never secret values; still only root's to read. The
        # agent's only record of what is deployed, so it is replaced whole.
        write_atomic(self.state_path(), json.dumps(state))

    def report(self, **status):
        """Print a report and send it to the status reference. Sending it
        never fails the caller: what was done stays done, and the printed
        report reaches the installation's log with the journal."""
        status = {"installation": self.config["name"], "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  **self.state(), **status}
        if render.scalable(self.config):
            status.update(instance=self.instance_name(), roles=list(self.roles()))
        print(json.dumps(status), flush=True)
        reference = self.config.get("status_reference", "")
        if reference:
            try:
                self.cloud.put_status(reference, status)
            except Exception as error:  # a timeout, a bad response, anything
                print(json.dumps({"status_report_failed": describe(error)}), file=sys.stderr, flush=True)
        return status

    def explain(self, error):
        """A failure as reports and the deploy record carry it: redacted like
        diagnostics, with the end of a failed command's output."""
        return describe(error, Redactor.from_files(self.secrets_root))

    # -- monitoring ---------------------------------------------------------------

    def containers(self, bundle):
        """`docker compose ps` entries: a JSON array, or JSON lines from
        newer Compose releases. Raises TimeoutExpired when Docker does not
        answer in EXEC_SECONDS. A one-off job a reboot left behind, which --all
        lists under its service's name, does not count as that service."""
        output = self.compose(bundle, "ps", "--all", "--format", "json", timeout=EXEC_SECONDS).stdout.strip()
        entries = json.loads(output) if output.startswith("[") else [json.loads(line) for line in output.splitlines()]
        return [entry for entry in entries if ONE_OFF_LABEL not in str(entry.get("Labels", ""))]

    def service_states(self, bundle):
        """Each service's container state and health check result."""
        return {e.get("Service", "?"): (e.get("State", ""), e.get("Health", "")) for e in self.containers(bundle)}

    def certificate_days_left(self, now=None):
        """Days until the ingress certificate expires, or None before it is
        delivered."""
        path = self.secrets_root / "ingress" / "TLS_CERTIFICATE"
        if not path.exists():
            return None
        output = self.run_command(["openssl", "x509", "-noout", "-enddate", "-in", str(path)],
                                  timeout=EXEC_SECONDS).stdout.strip()
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
        except (AgentError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
            return {}
        dead = status["total_deadletter"]
        # The previous count is kept on the data disk too, so dead letters
        # added while the agent restarted are still new.
        previous = last[1]["QueueDeadLetters"][0] if last else self.dead_letters_measured()
        added = max(0, dead - previous) if previous is not None else 0
        if previous != dead:
            with contextlib.suppress(OSError):
                write_atomic(self.releases / "queue.json", json.dumps({"dead_letters": dead}))
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

    def dead_letters_measured(self):
        """The dead-letter count of the last measurement before the agent
        started, or None before the first one."""
        try:
            count = json.loads((self.releases / "queue.json").read_text())["dead_letters"]
        except (OSError, ValueError, KeyError, TypeError):
            return None
        return count if isinstance(count, int) else None

    def task_slots_used(self, bundle, now):
        """How full the task worker is, in percent: the busiest of its code
        slots, container slots, and container memory, from the status file
        it rewrites. None when the file is missing or stale. Keeps the file's
        last_task_at for retire_if_idle."""
        try:
            status = json.loads(self.compose(bundle, "exec", "-T", "task-worker", "cat", render.TASK_STATUS,
                                             timeout=EXEC_SECONDS).stdout)
            if now.timestamp() - status["time"] > TASK_STATUS_STALE_SECONDS:
                return None
            self._last_task_at = status.get("last_task_at")
            shares = [pool["busy"] / pool["slots"] for pool in (status["code"], status["containers"]) if pool["slots"]]
            if status.get("memory_mb"):
                shares.append(status["memory_used_mb"] / status["memory_mb"])
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError, KeyError, TypeError):
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
            ok, seconds = self.synthetic_check()
        except (AgentError, OSError, ValueError, KeyError, TypeError) as error:
            print(json.dumps({"synthetic_check_failed": describe(error)}), file=sys.stderr, flush=True)
            ok, seconds = False, 0
        # A new object for every check, so monitor() can tell it from the
        # one it published.
        self._synthetic_result = {"ok": bool(ok), "seconds": seconds}

    def synthetic_metrics(self, now):
        """The newest synthetic check's result, until monitor() has published
        it: each check is one data point, so one failed run is one failed
        point rather than a failure repeated until the next check. On hosts
        that serve the API, a new check starts in the background every
        SYNTHETIC_SECONDS, so it never holds up the agent's loop."""
        if not (self.config.get("monitoring", {}).get("synthetic_event") and "api" in self.roles()):
            return {}
        started = getattr(self, "_synthetic_started", None)
        running = getattr(self, "_synthetic_thread", None)
        if (started is None or (now - started).total_seconds() >= SYNTHETIC_SECONDS) and not (running and running.is_alive()):
            self._synthetic_started = now
            self._synthetic_thread = self.background(self.run_synthetic)
        result = getattr(self, "_synthetic_result", None)
        if result is None or result is getattr(self, "_synthetic_published", None):
            return {}
        self._synthetic_observed = result
        return {"SyntheticOk": (int(result["ok"]), "Count"),
                "SyntheticSeconds": (round(result["seconds"], 1), "Seconds")}

    @staticmethod
    def background(function):
        thread = threading.Thread(target=function, daemon=True)
        thread.start()
        return thread

    def retire_if_idle(self, metrics, now):
        """In a scalable AWS installation, a task host that has run nothing for
        scaling.task_worker.retire_idle_minutes leaves its group, which
        shrinks by one. Groups only scale task hosts out, since they cannot
        tell which hosts are running tasks; this is how they shrink. GCP has
        no such grant: its IAM conditions cannot name one instance group, and
        a task host runs untrusted boxes, so it must not hold one over them
        all. Those groups stay at their peak until an operator shrinks them,
        so nothing here tries, and no worker stops for it. The
        task worker stops first, so tasks claimed in the meantime go back
        to the queue. The group keeps its minimum, and each host waits up to
        five minutes longer than the others, by its name, so idle hosts do
        not all leave at once. A reading shows only what runs as the worker
        writes it, so a short task can fall between readings; the wait also
        counts from the worker's last_task_at. An image without that field,
        or a worker that has run nothing since it started (0), adds nothing."""
        scaling = self.config.get("scaling", {}).get("task_worker", {})
        minutes = scaling.get("retire_idle_minutes", 0)
        if not (self.config["cloud"] == "aws" and render.scalable(self.config) and "task-worker" in self.roles()
                and minutes > 0 and scaling.get("max", 1) > scaling.get("min", 1)):
            return
        used = metrics.get("TaskSlotsUsedPercent", (None,))[0]
        if used is None or used > 0:
            self._idle_since = None
            return
        self._idle_since = getattr(self, "_idle_since", None) or now
        stagger = int(hashlib.sha256(self.instance_name().encode()).hexdigest(), 16) % 300
        idle_from = self._idle_since.timestamp()
        last_task = getattr(self, "_last_task_at", None)
        if isinstance(last_task, (int, float)) and last_task > 0:
            idle_from = max(idle_from, last_task)
        if now.timestamp() - idle_from < minutes * 60 + stagger:
            return
        self._idle_since = now
        group = self.config.get("instance_group")
        if not group:
            return
        bundle = self.releases / self.state()["release"] / "bundle"
        try:
            if self.cloud.group_size(group) <= scaling["min"]:
                return
            self.compose(bundle, "stop", "--timeout", "120", "task-worker", timeout=2 * EXEC_SECONDS)
            try:
                self.report(state="retiring", reason=f"No tasks for {minutes} minutes; leaving {group}")
                self.cloud.leave_group(group)
            except (AgentError, OSError):
                self.compose(bundle, "up", "-d", "task-worker", timeout=EXEC_SECONDS)
                raise
        except (AgentError, OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
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
        state = self.state()
        release = state.get("release")
        if release:
            modes = self.installed_modes(state)
            local_data = [s for s in ("postgres", "valkey") if modes[s] == "local"]
            states = self.service_states(self.releases / release / "bundle")
            healthy = {name for name, (state, health) in states.items()
                       if state == "running" and health in ("", "healthy")}
            expected = [*self.roles(), *(["ingress"] if render.web_roles(self.config) else []), *local_data]
            metrics["UnhealthyServices"] = (sum(name not in healthy for name in expected), "Count")
            if "scheduler" in self.roles():
                metrics["SchedulerRunning"] = (int("scheduler" in healthy), "Count")
            reachable = (all(name in healthy for name in local_data)
                         and self.dependencies_reachable())
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
            except (ValueError, IndexError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
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
        # The host image has no data root until something mounts or writes it,
        # and on GCP that waits for the first release. The heartbeat must not.
        with contextlib.suppress(FileNotFoundError):
            usage = os.statvfs(self.data_root)
            used = max(1 - usage.f_bavail / max(usage.f_blocks, 1), 1 - usage.f_favail / max(usage.f_files, 1))
            metrics["DataDiskUsedPercent"] = (round(100 * used, 1), "Percent")
        memory = memory_used_percent(self.meminfo)
        if memory is not None:
            metrics["MemoryUsedPercent"] = (memory, "Percent")
        if self.config.get("data_disk") == "scratch":
            # A scalable host's disk holds nothing to back up: its data
            # services and buckets keep their own backups.
            return metrics
        volume = self.config.get("data_volume_id")
        if not volume:
            # Left out, so the backup alarm reports the status as unknown.
            print(json.dumps({"backup_status_failed": "The configuration names no data_volume_id"}),
                  file=sys.stderr, flush=True)
            return metrics
        # Until the first snapshot, backup age counts from when monitoring began.
        baseline = self.releases / "monitoring.json"
        try:
            since = parse_time(json.loads(baseline.read_text())["since"])
        except (OSError, ValueError, KeyError):
            since = now
            baseline.parent.mkdir(parents=True, exist_ok=True)
            baseline.write_text(json.dumps({"since": now.isoformat()}))
        try:
            latest = self.cloud.latest_snapshot(volume, self.config["name"]) or since
        except AgentError as error:
            # Left out, so the backup alarm reports the status as unknown.
            print(json.dumps({"backup_status_failed": str(error)}), file=sys.stderr, flush=True)
        else:
            metrics["BackupAgeHours"] = (round((now - latest).total_seconds() / 3600, 2), "None")
        return metrics

    def monitor(self, now=None):
        """Publish health metrics, then retire this host if it is an idle
        task host. Failures are logged, never raised: the missing heartbeat
        is what alarms, as when Docker no longer answers."""
        now = now or datetime.datetime.now(datetime.timezone.utc)
        try:
            metrics = self.observe(now=now)
            beat = getattr(self, "_beat_at", None)
            if beat is not None and time.monotonic() - beat < 10:
                # Just sent by heartbeat(); Cloud Monitoring refuses a second
                # point of one series this soon.
                metrics.pop("AgentHeartbeat", None)
            self.cloud.put_metrics(self.config["name"], metrics)
        except (AgentError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            print(json.dumps({"monitoring_failed": describe(error)}), file=sys.stderr, flush=True)
            return
        if "SyntheticOk" in metrics:
            # Published once; the next check's result comes next.
            self._synthetic_published = getattr(self, "_synthetic_observed", None)
        self.retire_if_idle(metrics, now)

    # -- host journal -----------------------------------------------------------

    def journal_records(self, source, cursor):
        """Up to JOURNAL_BATCH records for one source after `cursor`, oldest
        first. Without a cursor, the newest 100. Only that many are read, so
        a backlog (shipping that failed for days, a flood of warnings) costs
        one batch per cycle, not the whole backlog."""
        command = ["journalctl", "--output=json", "--no-pager", *JOURNAL_SOURCES[source]]
        command += ["--after-cursor", cursor] if cursor else ["--lines", "100"]
        records = []
        for line in self.read_lines(command, JOURNAL_BATCH):
            with contextlib.suppress(ValueError):
                records.append(json.loads(line))
        return records

    @staticmethod
    def read_lines(command, limit, timeout=EXEC_SECONDS):
        """The first `limit` lines a command writes, read as it writes them.
        The command is stopped once they are read, and one still writing after
        `timeout` seconds is killed and counts as failed."""
        lines, stopped = [], False
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                              encoding="utf-8", errors="replace") as process:
            watchdog = threading.Timer(timeout, process.kill)
            watchdog.start()
            try:
                for line in process.stdout:
                    lines.append(line)
                    if len(lines) >= limit:
                        process.kill()
                        stopped = True
                        break
            finally:
                watchdog.cancel()
        if process.returncode and not stopped:
            raise subprocess.CalledProcessError(process.returncode, command)
        return lines

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
        # Every host of a scalable AWS installation writes the same stream.
        instance = self.instance_name()
        for source in JOURNAL_SOURCES:
            try:
                records = self.journal_records(source, cursors.get(source))
                if records:
                    self.cloud.ship_journal(self.config["name"],
                                            [journal_entry(r, redact, source, instance) for r in records])
                    cursors[source] = records[-1]["__CURSOR"]
            except (AgentError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
                print(json.dumps({"journal_shipping_failed": describe(error), "source": source}),
                      file=sys.stderr, flush=True)
        write_atomic(path, json.dumps(cursors))

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
        if not desired:
            return None
        # A new host's scratch disk is mounted before the migration writes its
        # release files and state, which the mount would otherwise hide.
        self.prepare_scratch()
        state = self.state()
        # A failed migration of a later release leaves this release's mark, while
        # the database may have moved on: when the pointer comes back, compare it
        # with the ledger again, and report, which is what the release helper waits for.
        if desired == state.get("migrated_release") and not state.get("migration_failed"):
            # A pointer that went to a later release and back before this host
            # acted leaves nothing reported since it was written, and the helper
            # waits on that: deploy-now says it again. Without a request_id, since
            # it does not answer the command, and None, so the command carries on.
            if retry:
                self.report(state="migrated", release=desired)
            return None
        if not retry and (state.get("migration_failed") or {}).get("release") == desired:
            return None
        try:
            manifest = self.fetch_release(desired)
            installed = state.get("migrated_release") or state.get("release")
            operator = self.operator_secrets()
            # A new release stops here on what any role would refuse. A replacement
            # catching up, with no mark yet, or the pointer back at this host's
            # mark, is about a release the database already has, and only logs it.
            if state.get("migrated_release") not in (None, desired):
                check_every_role(self.config, operator)
            # Rendered first, as in deploy: what it refuses is never delivered.
            staged = self.render_bundle(desired, manifest, operator, install=False)
            level = self.database_level(installed, state, staged=staged)
            check_upgrade(manifest, installed, level)
            bundle = self.install_bundle(desired)
            self.deliver_secrets()
            # The jobs run the new release's images, which a private mirror
            # serves only with a current sign-in.
            self.registry_login()
            if render.data_mode(self.config, "postgres") == "managed":
                self.compose(bundle, "--profile", "admin", "run", "--rm", "prepare-postgres")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "check-postgres")
            restore_point = {"kind": "point-in-time" if render.data_mode(self.config, "postgres") == "managed"
                             else "operator", "time": utc_now()}
            self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
            if self.config.get("admin_email"):
                self.compose(bundle, "--profile", "admin", "run", "--rm", "bootstrap-admin")
        except Exception as error:  # any failure is recorded, so it waits for deploy-now
            explained = self.explain(error)
            self.save_state({**self.state(), "migration_failed": {"release": desired, "error": explained,
                                                                  "at": utc_now()}})
            return self.report(state="migration-failed", release=desired, error=explained, request_id=request_id)
        latest = manifest["migrations"]["latest"]
        self.save_state({**{k: v for k, v in self.state().items() if k != "migration_failed"},
                         "migrated_release": desired, "migrations": max(latest, level or 0)})
        self.remember({"release": desired, "migrated": latest, "restore_point": restore_point, "finished": utc_now()})
        return self.report(state="migrated", release=desired, restore_point=restore_point, request_id=request_id)

    def reconcile(self, request_id="", retry=False):
        """Deploy the release pointer if it changed. A release that failed
        waits for deploy-now (`retry`) or a new pointer; an interrupted one
        is resumed. Nothing deploys onto a database that only roll-back can put
        right (restore_pending), and deploy-now is told so. A scalable migration
        host migrates first, from its own pointer, and a release whose migration
        failed there does not deploy until deploy-now gets it past that."""
        migrated = self.reconcile_migrations(request_id, retry)
        desired = self.cloud.get_parameter(self.config["release_pointer"])
        state = self.state()
        if state.get("release") and "configuration" not in state:
            # Deployed before configurations were recorded: assume the one it runs.
            self.save_state({**state, "configuration": self.configuration_digest()})
            state = self.state()
        # A configuration change (settings, data modes, storage) redeploys the
        # same release, so the change takes effect; so does a new address,
        # which the bundle must deny user code. An address alone does not start
        # an installation that is stopped, whoever stopped it and for whatever
        # work on its data: that waits for deploy-now, or for the deploy one
        # began to resume. Box guests' baseline keeps up until then
        # (refresh_guest_network).
        changed = state.get("configuration") not in (None, self.configuration_digest()) or (
            (retry or not state.get("stopped") or bool(state.get("deploying"))) and self.readdressed(state))
        if retry and not changed and desired and desired == state.get("release"):
            # deploy-now also renders the bundle again when the operator
            # secret bundle gained or lost an entry, which only a render
            # mounts or unmounts.
            changed = bool(self.unmounted_secrets(self.releases / desired / "bundle"))
        interrupted = state.get("deploying") or {}
        if not desired or (desired == state.get("release") and not changed):
            if desired and interrupted:
                # A deploy was interrupted, and the pointer and configuration
                # went back meanwhile: it ends as a failure, which returns to
                # the installed release.
                failed = self.recover(interrupted.get("release"), state.get("release"), AgentError(
                    "The deploy was interrupted, and the release pointer and configuration no longer ask for it"))
                return self.report(state="failed", release=interrupted.get("release"), error=failed["error"],
                                   request_id=request_id, steps=failed["steps"], recovery=failed["recovery"])
            return migrated or (None if retry else self.report_pointer_returned(state, desired))
        # A failed release waits for deploy-now; an interrupted one resumes.
        failed_release = (state.get("failed") or {}).get("release")
        if not retry and failed_release == desired and interrupted.get("release") != desired:
            if "pointer_returned" in state:
                # It left the release this host runs: its next return counts too.
                self.save_state({key: value for key, value in state.items() if key != "pointer_returned"})
            return None
        pending = self.restore_pending(state, desired)
        if pending and self.partly_migrated(interrupted):
            # Nothing can take this attempt up, and roll-back waits for it to end,
            # so it ends as a failure, which roll-back can undo.
            failed = self.recover(interrupted.get("release"), state.get("release"), AgentError(pending))
            return self.report(state="failed", release=interrupted.get("release"), error=failed["error"],
                               request_id=request_id, steps=failed["steps"], recovery=failed["recovery"])
        if pending:
            if retry:
                raise AgentError(pending)
            return migrated
        # This release's migration failed on this host, so it does not start here, in
        # this cycle or a later one, until a retry (deploy-now, or a new host) gets past
        # that. With nothing installed, a new host has no level to compare
        # (database_level), so the job's refusal is all that keeps its scheduler off a
        # database a restore left past this release, and restoring again needs Hot
        # stopped (Going back in infra/README.md). A replaced host at the database's
        # level passes the job and goes on. One whose job fails for another reason waits
        # too, as a failed deploy does, with the scheduler alarm firing: it cannot tell
        # the two apart. A role pointer at another release still deploys. An interrupted
        # deploy waits as well: its failure would start the installed release, older still.
        if (state.get("migration_failed") or {}).get("release") == desired:
            return migrated
        first_install = self.cloud.get_secret(self.references["generated"]) is None and not state.get("release")
        try:
            self.prepare_host(first_install)
            if self.state() == state:
                record = self.deploy(desired, state.get("release"))
                return self.report(state="deployed", release=desired, steps=record["steps"], request_id=request_id,
                                   **{key: record[key] for key in ("warning",) if key in record})
        except Exception as error:  # any failure is recorded and explained, never left as "deploying"
            # With no release in `state`, the disk may have mounted before the
            # failure, and it knows better.
            failed = self.recover(desired, state.get("release") or self.state().get("release"), error)
            return self.report(state="failed", release=desired, error=failed["error"], request_id=request_id,
                               steps=failed["steps"], recovery=failed["recovery"])
        # The disk was not ready when this began, and mounted only now, as with
        # a slow attach. What it holds decides, so nothing upgrades as a first
        # install: start what it says is installed, as this cycle's resume could
        # not, then look again.
        self.resume()
        return self.reconcile(request_id, retry)

    def report_pointer_returned(self, state, desired):
        """A scalable host's report when its pointer comes back to the release
        it runs, from one that failed here: it has nothing to do, and the release
        helper waits for a report from since it wrote the pointer, or times out with
        this host on the right release. Once for each return, without a request_id,
        since no command asks for it. A host that is not running says nothing."""
        failed = state.get("failed") or {}
        if (not desired or not render.scalable(self.config) or failed.get("release") in (None, desired)
                or state.get("stopped") or state.get("resume_failed") or state.get("restarting")
                or state.get("pointer_returned") == failed.get("at")):
            return None
        self.save_state({**state, "pointer_returned": failed.get("at")})
        return self.report(state="current", release=desired)

    def failure_since(self, state, sent, request_id):
        """The report of a failed deploy or migration of the release asked
        for now, if it failed after a deploy-now was `sent`: that command
        waited behind this very attempt, so nothing has been fixed since. It
        gets that failure as its answer, not a second attempt. A failure from
        before it was sent is what deploy-now retries, once the cause is
        fixed."""
        failed, migration = state.get("failed") or {}, state.get("migration_failed") or {}
        if (recorded_since(failed.get("at"), sent)
                and failed.get("release") == self.cloud.get_parameter(self.config["release_pointer"])):
            return self.report(state="failed", release=failed["release"], error=failed.get("error"),
                               steps=failed.get("steps", []), recovery=failed.get("recovery"), request_id=request_id)
        pointer = self.config.get("migration_pointer")
        if (pointer and recorded_since(migration.get("at"), sent)
                and migration.get("release") == self.cloud.get_parameter(pointer)):
            return self.report(state="migration-failed", release=migration["release"], error=migration.get("error"),
                               request_id=request_id)
        return None

    def command(self, operation, service="", request_id="", sent=""):
        """Run one command. `sent` is when it was sent, as far as known."""
        if operation not in COMMANDS:
            raise AgentError(f"Unknown command {operation!r}")
        service = "" if service == "none" else service  # the SSM document's default
        release = self.state().get("release")
        bundle = self.releases / release / "bundle" if release else None
        if operation == "deploy-now":
            state = self.state()
            settled = self.failure_since(state, sent, request_id)
            if settled:
                return settled
            # Deploy a new pointer or configuration. Otherwise, after
            # drain-and-stop, or a start after a boot that failed or has yet to
            # run, start the installed release again, unless its database needs
            # roll-back first or fails its checks. A deploy that fails keeps Hot
            # stopped.
            report = self.reconcile(request_id, retry=True)
            if report:
                return report
            state = self.state()
            if state.get("stopped") or state.get("resume_failed") or (state.get("release") and not self.boot_settled()):
                pending = self.restore_pending(state, self.cloud.get_parameter(self.config["release_pointer"]))
                if pending:
                    raise AgentError(pending)
                return self.resume(force=True, request_id=request_id, **self.settle_pointed_back(state))
            self.finish_restarts()
            return self.report(state="current", request_id=request_id, **self.scheduler_held(state))
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
            modes = self.installed_modes(self.state())
            local_data = tuple(s for s in ("postgres", "valkey") if modes[s] == "local")
            if service not in self.roles() + (("ingress",) if render.web_roles(self.config) else ()) + local_data:
                raise AgentError(f"Unknown service {service!r}")
            output = self.compose(bundle, "logs", "--no-color", "--timestamps", "--tail", str(LOG_LINES), service,
                                  timeout=EXEC_SECONDS).stdout
            lines = bounded_lines(output, Redactor.from_files(self.secrets_root))
            return self.report(state="logs", service=service, request_id=request_id, logs=lines)
        if operation == "restart-service":
            if service not in self.roles() + (("ingress",) if render.web_roles(self.config) else ()):
                raise AgentError(f"Unknown service {service!r}")
            # The next cycle finishes a drain-and-stop cut short or failed, and
            # stops this service again with the rest (finish_stop).
            if self.state().get("stopping"):
                raise AgentError("drain-and-stop has yet to finish: the next cycle stops the rest of Hot. Run "
                                 "restart-service once it has, or run deploy-now to start all of Hot")
            pending = self.restore_pending(self.state(), release)
            if pending:
                raise AgentError(pending)
            # The installed bundle cannot read records a move has rewritten, and
            # would put new uploads on the old storage. A move cut short counts
            # too, until the next cycle takes it up.
            failed = self.state().get("failed") or {}
            if (self.storage_progress(self.state().get("deploying") or {}) == "moving"
                    or (self.storage_progress(failed) == "moving" and not failed.get("rolled_back"))):
                raise AgentError("Hot stays stopped: some objects may already be in the new storage, which the "
                                 "installed bundle cannot read. Fix the cause and run deploy-now, or put the storage "
                                 "setting back, apply, and run deploy-now")
            # Nor does it describe a database the data has moved to, in a deploy
            # that upgraded too. Secrets follow the state, so nothing would write
            # to the old copy, but the service could not connect.
            if self.bundle_left_behind(bundle, self.state()):
                raise AgentError("Hot stays stopped: the data moved to the new database, and the installed bundle "
                                 "still describes the copy it left. Fix the cause and run deploy-now")
            # The scheduler stays off a schema that may be partly migrated until
            # what its note names clears that.
            held = self.scheduler_held(self.state()) if service == "scheduler" else {}
            if held:
                raise AgentError(f"The scheduler is {held['scheduler']}")
            if service == "app":
                check_oauth(self.config.get("settings") or {}, self.operator_secrets())
            # The service is recreated from the installed bundle, which mounts
            # the operator secret bundle's entries as they were rendered: a new
            # entry would never reach it, and a removed one would leave it
            # mounting a file that is gone.
            changes = self.unmounted_secrets(bundle, roles=[service], rendering=False)
            if changes:
                added, removed = changes[service]
                parts = ([f"has {', '.join(added)}, which the installed bundle does not mount"] if added else []) + (
                    [f"no longer has {', '.join(removed)}, which the installed bundle mounts"] if removed else [])
                raise AgentError(f"For {service}, the operator secret bundle {' and '.join(parts)}; run deploy-now, "
                                 "which renders the bundle again with them")
            # What it recreates would exit on an empty file or an egress proxy
            # Hot refuses, where now it runs on the value it read. A reboot
            # delivers regardless: nothing is running.
            data_keys = existing_data_keys(self.installed_config(self.state().get("data_modes")))
            recreated = {service, *(("ingress",) if service in render.web_roles(self.config) else ())}
            for key, value in self.operator_secrets().items():
                if not (isinstance(value, str) and recreated.intersection(operator_readers(key, data_keys))):
                    continue
                if reads_as_empty(value):
                    raise empty_entry(key)
                if key == "HOT_NETWORK_EGRESS_PROXY" and not render.is_egress_proxy(value):
                    raise bad_egress_proxy(key)
            # Secrets are rendered again, which is how rotation takes effect,
            # for the data services the installed bundle uses.
            self.deliver_secrets(data_modes=self.state().get("data_modes"))
            # A rotated connection URI may name another host to probe.
            self._dependencies = None
            try:
                self.restart(bundle, service, request_id)
            except (OSError, subprocess.SubprocessError) as error:
                raise CommandFailed(self.explain(error), recovery=self.restart_recovery(service)) from error
            return self.report(state="restarted", service=service, request_id=request_id)
        if operation == "drain-and-stop":
            # Kept stopped across reboots until the next deploy. Said first, as a
            # deploy says what it stops: the stop can take minutes, and the agent
            # may die in it, or Compose fail after part of it took effect. Until it
            # is over, `stopping` in the state says so, with the request that asked
            # (finish_stop), and deploy-now starts them all. A disk too full to say
            # so does not keep Hot from stopping; the save after it reports that.
            stopped = {**{key: value for key, value in self.state().items() if key not in ("restarting", "stopping")},
                       "stopped": True}
            with contextlib.suppress(OSError):
                self.save_state({**stopped, "stopping": {"request_id": request_id}})
            try:
                self.stop_services(bundle)
            except (OSError, subprocess.SubprocessError) as error:
                raise CommandFailed(self.explain(error), recovery=self.stop_recovery()) from error
            self.save_state(stopped)
            return self.report(state="stopped", request_id=request_id)
        if operation == "roll-back":
            return self.report(state="rolled-back", request_id=request_id, **self.roll_back(bundle))
        if operation == "backup":
            # An extra logical dump beside the scheduled data-disk snapshots.
            # A managed database is backed up by its cloud service, an existing
            # one by its operator. Until a data move finishes, the database the
            # installed release runs on is still here.
            state = self.state()
            if self.installed_modes(state)["postgres"] != "local":
                raise AgentError("The database is not on this host; back it up with its provider")
            self.grow_data_disk()
            # A reboot leaves a stopped installation's database down too, and
            # only the data services start here: Hot stays stopped for the dump.
            self.start_installed_data(bundle, state)
            target = self.dump_postgres(bundle, "manual")
            return self.report(state="backed-up", backup=str(target / "postgres.dump"), request_id=request_id)
        raise AgentError(f"Unhandled command {operation!r}")

    def restart(self, bundle, service, request_id):
        """Recreate one service from `bundle`. Said first, as a deploy says what it
        stops: the agent may die in a stop or a recreate, or Compose fail after it
        took effect, and nothing else would know the service is down. Until it
        is up, `restarting` in the state names it, with the request that asked
        (finish_restarts). Doing this again recreates once more what never went
        down, as that request asked. A hotbox that cannot be copied refuses the
        restart first, with the service still running."""
        if service == "task-worker":
            # A failed deploy that stopped Hot may have left its own.
            self.install_hotbox(bundle)
        self.note_restarts(started={service: request_id})
        if service == "scheduler":
            self.compose(bundle, "stop", "scheduler")
        self.compose(bundle, "up", "-d", "--force-recreate", service)
        recreated = [service]
        if service in render.web_roles(self.config):
            # The service is up, so an ingress that fails does not recreate it again.
            self.note_restarts(started={"ingress": request_id}, done=[service])
            self.refresh_ingress(bundle)
            recreated.append("ingress")
        if not self.note_restarts(done=recreated):
            self._restarted.update(dict.fromkeys(recreated, request_id))

    def restart_recovery(self, service):
        """What to do after a restart of `service` failed, going by what the state
        says now, which is all the next cycle goes by: it may have no word of the
        restart, or be stopped, and then nothing starts the service again."""
        state = self.state()
        waiting = state.get("restarting") or {}
        if state.get("stopping"):
            return (f"{service} may be down, and a drain-and-stop has yet to finish: the next cycle stops the rest of "
                    f"Hot. Run restart-service {service} again once it has, or run deploy-now to start all of Hot.")
        if state.get("stopped"):
            return (f"{service} may be down, and Hot is stopped, so nothing will start it again. Fix the cause and run "
                    f"restart-service {service} again, or run deploy-now to start all of Hot.")
        if service in waiting:
            down = service
        elif service in render.web_roles(self.config) and "ingress" in waiting:
            down = "ingress"
        else:
            return (f"{service} may be down, and nothing will start it again. Fix the cause and run restart-service "
                    f"{service} again.")
        return (f"{down} may be down. The next cycle starts it again, and goes on until a start works. Fix the cause, "
                f"or run restart-service {service} again.")

    def note_restarts(self, started=None, done=()):
        """Keep in the state what restarts have yet to bring up, {service: request
        ID}, and return whether that was saved. On a disk too full to say so the
        restart still runs. A word that cannot be taken back costs one more
        recreate if another process finds it: on AWS, a command is its own
        process. The process that finished the restart remembers (finish_restarts)."""
        state = self.state()
        waiting = {service: request for service, request in (state.pop("restarting", None) or {}).items()
                   if service not in done}
        waiting.update(started or {})
        try:
            self.save_state({**state, **({"restarting": waiting} if waiting else {})})
        except OSError as error:
            print(json.dumps({"restart_not_recorded": describe(error)}), file=sys.stderr, flush=True)
            return False
        return True

    def finish_restarts(self):
        """Do again the restarts that were cut short or failed, and say so to whoever
        asked. A stopped installation stays stopped, whatever stopped it after they
        began, and a scheduler held since keeps to what its note names. Nothing runs
        while another start has this to do: an interrupted deploy, which starts
        everything or, failing, returns to the installed release (return_to_previous),
        or a start after a boot, which starts everything too (resume). A restart this
        process finished, whose word the disk would not let it take back, is not done
        again: only taking that back is, on a healthy service that this would stop."""
        state = self.state()
        waiting = state.get("restarting") or {}
        self._restarted = {service: request for service, request in self._restarted.items()
                           if waiting.get(service) == request}
        if not waiting or state.get("deploying") or state.get("resume_failed"):
            return
        if state.get("stopped"):
            self.note_restarts(done=list(waiting))
            return
        if not self.boot_settled():
            return
        bundle = self.releases / state["release"] / "bundle"
        held = self.scheduler_held(state)
        for service, request_id in sorted(waiting.items()):
            if (service == "scheduler" and held) or self._restarted.get(service) == request_id:
                self.note_restarts(done=[service])
                continue
            self.restart(bundle, service, request_id)
            self.report(state="restarted", service=service, request_id=request_id)

    def stop_recovery(self):
        """What to do after a drain-and-stop failed, going by what the state says
        now, as restart_recovery does: on a disk too full to say so, nothing finishes it."""
        if self.state().get("stopping"):
            return ("Hot counts as stopped from here, also across reboots, though some of it may still run. The next "
                    "cycle stops the rest, and goes on until that works. Fix the cause, or run deploy-now to start it all.")
        return "Some of Hot may still run, and nothing will stop the rest. Fix the cause and run drain-and-stop again."

    def finish_stop(self):
        """Finish a drain-and-stop that was cut short or failed, and say so to whoever
        asked. Until then `stopping` in the state and in status says some of Hot may
        still run: a restore of its database, say, must wait for that. Stopping what
        already stopped changes nothing, so a stop that a failed deploy or a reboot
        finished is only answered here. An interrupted deploy has this to do: it
        starts everything or, failing, stops it again (recover). A deploy that
        finishes, a roll-back that does, and deploy-now's start end the stop."""
        state = self.state()
        stopping = state.get("stopping")
        if not stopping or state.get("deploying"):
            return
        self.stop_services(self.releases / state["release"] / "bundle")
        self.save_state({key: value for key, value in self.state().items() if key != "stopping"})
        self.report(state="stopped", request_id=stopping.get("request_id", ""))

    @staticmethod
    def bundle_left_behind(bundle, state):
        """Whether the installed `bundle` still runs a data service on this host
        that the state says left it. Only a deploy of another release can leave
        this, by failing or being cut short after its move (see committed): a
        deploy of the release already running has its new bundle in place by then."""
        try:
            services = json.loads((bundle / "compose.json").read_text())["services"]
        except (OSError, ValueError, KeyError, TypeError):
            return False
        return any(mode != "local" and service in services for service, mode in (state.get("data_modes") or {}).items())

    def restore_pending(self, state, starting):
        """Why Hot must wait for roll-back before the release `starting` starts
        or a deploy runs, or None. Both cases are a local database, which only
        the agent can restore, since these hosts have no shell:
        - A roll-back stopped partway. The database may be partly restored,
          whatever the pointer names, so nothing deploys onto it either.
        - Hot is stopped, the installed release is named again, and a failed
          deploy may have partly migrated the database. Nothing is writing to it,
          so the dump loses nothing, and a fixed release may still go forward
          from it. roll_back refuses once data moved, so that is left out, and
          once storage moved, when only that fixed release is left. If a roll-back
          already restored it, all that is left to do is start Hot, which roll-back
          does without restoring it again."""
        failed = state.get("failed") or {}
        point = failed.get("restore_point") or {}
        attempt = (failed.get("roll_back_failed") or {}).get("steps", [])
        if "stopped" in attempt and "checked" not in attempt:
            return ("Hot stays stopped: a roll-back did not finish, and the database may be partly restored or not "
                    f"fit to run on. Fix the cause, then run roll-back again: it restores {point.get('path')} from "
                    "the start.")
        if (state.get("stopped") and starting and starting == state.get("release") and point.get("kind") == "dump"
                and self.partly_migrated(failed) and not failed.get("rolled_back")
                and not failed.get("data_moved") and "data-moved" not in failed.get("steps", [])):
            if self.storage_progress(failed):
                return (f"Hot stays stopped: the failed deploy of {self.migrated_by(failed)} may have partly applied "
                        f"its migrations, so {starting} cannot start on this database, and records may already point "
                        f"at the new storage, so roll-back cannot restore it. To go on, {self.fix_forward(failed)}.")
            if self.restore_checked(failed):
                return (f"Hot stays stopped: a roll-back restored the database, but did not start {starting}. Run "
                        "roll-back again: it only starts Hot.")
            return (f"Hot stays stopped: the failed deploy of {self.migrated_by(failed)} may have partly applied its "
                    f"migrations, so {starting} cannot start on this database. Run roll-back: it restores "
                    f"{point.get('path')} and starts {starting}. Or point the installation at a fixed release.")
        return None

    def settle_pointed_back(self, state):
        """Hot was stopped, and the pointer and configuration name the installed
        release again, not a failed attempt that may have left migrations or
        storage partway: the operator's way back from what the agent cannot undo
        itself, a database off this host, which they restored (roll-back
        refuses it), or a storage setting they put back. A local database goes
        through roll-back instead (restore_pending). That attempt is over: the
        release starts whole, and neither its restore point nor its steps carry
        into later deploys. A move whose data already went cannot go back, so
        that record stays, and neither can storage whose records all moved: the
        installed bundle already reads the new storage, which the setting no
        longer names, so that is refused. After migrations, the release's own
        checks must pass first (check_pointed_back). Returns what the report
        should add."""
        failed = state.get("failed") or {}
        migrated, progress = self.partly_migrated(failed), self.storage_progress(failed)
        if (not state.get("stopped") or failed.get("rolled_back") or failed.get("data_moved")
                or "data-moved" in failed.get("steps", [])
                or (migrated and (failed.get("restore_point") or {}).get("kind") == "dump")
                or not (migrated or progress)
                or state.get("release") != self.cloud.get_parameter(self.config["release_pointer"])):
            return {}
        if progress == "moved":
            raise AgentError("Hot stays stopped: the objects moved to the new storage, and their records point there. "
                             "Moving them back is not automated, so Hot cannot start on the storage the configuration "
                             "now names. Set that storage again, apply, and run deploy-now")
        if migrated:
            self.check_pointed_back(state, failed)
        notes = {}
        recovery = f"{state.get('release')} was started again by pointing back at it, so this attempt is over."
        if progress:
            notes["warning"] = self.storage_warning(failed)
            recovery += " " + notes["warning"]
        self.save_state({**state, "failed": {**failed, "rolled_back": utc_now(), "pointed_back": True,
                                             "recovery": recovery}})
        return notes

    def check_pointed_back(self, state, failed):
        """Before a release pointed back at starts on a database the agent did
        not restore: the checks roll_back runs on one it did, the release's own
        migrate and provision. That migrate refuses a database holding migrations
        newer than the release's, so a restore that was missed stops here; a real
        restore passes, and so do migrations that never applied. A refusal
        changes nothing: Hot stays stopped, and the failed record stands."""
        release = state.get("release")
        bundle = self.releases / release / "bundle"
        try:
            self.deliver_secrets(data_modes=state.get("data_modes"))
            self.start_installed_data(bundle, state)
            self.compose(bundle, "--profile", "admin", "run", "--rm", "migrate")
            self.compose(bundle, "--profile", "admin", "run", "--rm", "provision")
        except Exception as error:  # whatever stopped the checks, nothing starts
            raise CommandFailed(
                f"Hot stays stopped: {release} did not pass its checks on this database: {self.explain(error)}",
                release=release, recovery=(
                    f"If the database still holds migrations from the failed deploy of {self.migrated_by(failed)}, "
                    f"{self.database_restore(failed.get('restore_point') or {})}, then run deploy-now again. Otherwise "
                    "fix the cause and run deploy-now.")) from error

    def answer(self, operation, service="", request_id="", sent="", expires=None):
        """Run one command and always report on it. A command refused before
        it changed anything is `rejected`; one that failed partway is
        `failed`, with what it did and what to do next. One past `expires`
        (seconds since the epoch, when the helper stopped waiting) is `expired`
        and does not run: its sender was told it did not answer, and may have
        sent it again. Older helpers send no `expires`."""
        deadline = expiry(expires)
        if deadline is not None and deadline < time.time():
            at = datetime.datetime.fromtimestamp(deadline, datetime.timezone.utc).isoformat(timespec="seconds")
            return self.report(state="expired", command=operation, request_id=request_id,
                               error=f"The helper stopped waiting for this command at {at}; send it again if it is "
                                     "still needed")
        try:
            return self.command(operation, service, request_id, sent)
        except CommandFailed as error:
            return self.report(state="failed", command=operation, error=str(error), request_id=request_id,
                               **error.details)
        except AgentError as error:
            return self.report(state="rejected", error=str(error), request_id=request_id)
        except Exception as error:  # a failed docker compose, a full disk: still answered
            return self.report(state="failed", command=operation, error=self.explain(error), request_id=request_id)

    def first_delivery(self, request_id):
        """Record a command's request ID; False when it was handled before.
        Pub/Sub delivers at least once, so a copy of a command may come
        again, and a second roll-back or drain-and-stop must not run."""
        path = self.releases / "commands.json"
        try:
            seen = json.loads(path.read_text())
        except (OSError, ValueError):
            seen = []
        if request_id in seen:
            return False
        # A list lost to a half-written file would run commands again. With a
        # full disk, the command still runs and reports: it may be the one
        # asked to find out why.
        try:
            write_atomic(path, json.dumps((seen + [request_id])[-COMMANDS_REMEMBERED:]))
        except OSError as error:
            print(json.dumps({"command_not_recorded": describe(error), "request_id": request_id}),
                  file=sys.stderr, flush=True)
        return True

    def take_commands(self, limit=COMMANDS_PER_CYCLE):
        """Commands from the pull subscription (GCP), one message at a time:
        each is acknowledged before it runs, so nothing waits past its
        acknowledgement deadline behind a long command, and none runs twice.
        Each command first reads the stack's configuration again, as `hot-agent
        command` does on AWS: an apply that rewrote the instance metadata after
        this cycle's refresh, followed at once by deploy-now, must not be
        answered on the configuration from before it."""
        for _ in range(limit):
            received = self.cloud.receive_commands()
            if not received:
                return
            for ack_id, request in received:
                self.cloud.ack(ack_id)
                if not isinstance(request, dict):
                    print(json.dumps({"command_ignored": "The message is not a JSON command"}),
                          file=sys.stderr, flush=True)
                    continue
                request_id = str(request.get("request_id", ""))
                if request_id and not self.first_delivery(request_id):
                    print(json.dumps({"command_ignored": "Already handled", "request_id": request_id}),
                          file=sys.stderr, flush=True)
                    continue
                try:
                    self.refresh_configuration()
                except Exception as error:  # still answered, on the configuration in the file
                    print(json.dumps({"configuration_not_refreshed": describe(error)}), file=sys.stderr, flush=True)
                self.answer(str(request.get("operation", "")), str(request.get("service", "")), request_id,
                            str(request.get("sent", "")), request.get("expires"))

    def run_once(self):
        """One cycle of `hot-agent run`. A step that fails is logged and the
        next one still runs, so a cloud API outage or a broken deploy never
        stops the heartbeat, the journal, or commands. Only the steps that may
        rightly take long beat while they run: a host whose monitoring hangs,
        as when Docker wedges, goes silent, and host-down fires."""
        for step, long_running in ((self.refresh_configuration, False), (self.mount_data, False), (self.resume, True),
                                   (self.finish_stop, True), (self.finish_restarts, True), (self.reconcile, True),
                                   (self.refresh_guest_network, False), (self.monitor, False),
                                   (self.ship_journal, False), (self.take_commands, True)):
            try:
                with self.heartbeat() if long_running else contextlib.nullcontext():
                    step()
            except Exception as error:  # the loop outlives any one step
                print(json.dumps({"step_failed": step.__name__, "error": self.explain(error)}),
                      file=sys.stderr, flush=True)

    @contextlib.contextmanager
    def heartbeat(self, interval=HEARTBEAT_SECONDS, limit=HEARTBEAT_LIMIT_SECONDS):
        """While a long step holds the agent (a deploy, a data move, a
        command), publish AgentHeartbeat every `interval` seconds from a
        thread: the agent is working, not down. After `limit` seconds it stops
        and says so, so a step that hangs alarms in the end."""
        done = threading.Event()
        started = time.monotonic()

        def beat():
            while not done.wait(interval):
                if time.monotonic() - started >= limit:
                    print(json.dumps({"heartbeat_stopped": f"One step has run for over {limit // 60:g} minutes"}),
                          file=sys.stderr, flush=True)
                    return
                try:
                    self.cloud.put_metrics(self.config["name"], {"AgentHeartbeat": (1, "Count")})
                    self._beat_at = time.monotonic()
                except Exception as error:  # logged; the next beat tries again
                    print(json.dumps({"heartbeat_failed": describe(error)}), file=sys.stderr, flush=True)
        thread = threading.Thread(target=beat, daemon=True)
        thread.start()
        try:
            yield
        finally:
            done.set()
            thread.join(timeout=5)

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

    def resume(self, force=False, request_id="", **notes):
        """After a boot, start the deployed release again. Secrets live on
        tmpfs, so they are gone, and Docker's own restart of the containers
        failed without them. They are delivered for the data services the
        installed release uses, which differ during a data move. An
        installation stopped with drain-and-stop stays stopped. After a boot
        that interrupted a deploy, only the installed release's data services
        start, from its own bundle: the deploy resumes and starts the rest, or
        its failure returns to that release, or keeps Hot stopped if migrations
        may be partly applied (recover). After a failed deploy that
        may have partly migrated the database, the installed release's
        scheduler stays stopped, as the failure left it. A start that fails
        is tried again every RESUME_RETRY_SECONDS, and by deploy-now. `notes`
        go in its report."""
        state = self.state()
        release = state.get("release")
        delivered = self.secrets_root.is_dir() and any(self.secrets_root.iterdir())
        interrupted = state.get("deploying")
        if not release:
            return None
        if not force and state.get("stopped") and not interrupted:
            self.settle_boot()
            return None
        failure = state.get("resume_failed")
        if not force and delivered and (not self.resume_due(failure) if failure else self.boot_settled()):
            return None
        if force and state.get("stopped"):
            state = {k: v for k, v in state.items() if k not in ("stopped", "stopping")}
            self.save_state(state)
        held = self.scheduler_held(state)
        try:
            # A start that was killed must not pass for one that finished, so it
            # counts as failed until it does.
            state = {**state, "resume_failed": {"release": release, "error": "The start did not finish", "at": utc_now()}}
            self.save_state(state)
            if interrupted:
                self.restore_installed_bundle(interrupted)
                self.save_state({**state, "deploying": {**interrupted, "rebooted": True}})
            bundle = self.releases / release / "bundle"
            modes = state.get("data_modes") or self.configured_modes()
            self.deliver_secrets(data_modes=modes)
            # A replaced VM pulls the installed release's images again.
            self.registry_login()
            local_data = [s for s in ("postgres", "valkey") if modes.get(s) == "local"]
            if local_data:
                self.compose(bundle, "up", "-d", "--wait", *local_data)
            if not interrupted:
                self.start_services(bundle, scheduler=not held)
        except Exception as error:  # any failure is reported, and tried again
            explained = self.explain(error)
            with contextlib.suppress(OSError):
                self.save_state({**self.state(), "resume_failed": {"release": release, "error": explained,
                                                                   "at": utc_now()}})
            return self.report(state="resume-failed", release=release, error=explained, request_id=request_id, **notes)
        current = self.state()
        # What it started takes in any restart cut short, unless a deploy still has that to do.
        finished = {"resume_failed", *(() if interrupted else ("restarting",))}
        if finished & current.keys():
            self.save_state({k: v for k, v in current.items() if k not in finished})
        self.settle_boot()
        if interrupted:
            return self.report(state="resumed", release=release, request_id=request_id, started="data services",
                               interrupted=interrupted.get("release"))
        return self.report(state="resumed", release=release, request_id=request_id, **held, **notes)

    def boot_settled(self):
        """Whether Hot's start after this boot is over: a resume finished, or left
        a stopped installation alone, or a deploy or roll-back started Hot. The
        mark is on tmpfs beside the secrets, so a boot removes it, and outside their
        tree, which each delivery replaces. Delivered secrets cannot say this. On AWS
        a command is its own process, which can take the lock first at boot and
        deliver them for one service while the rest stay down."""
        return self.secrets_root.with_name("resumed").exists()

    def settle_boot(self):
        """Mark Hot's start after this boot as over (boot_settled). A mark that
        cannot be written costs one more start of what is already running."""
        marker = self.secrets_root.with_name("resumed")
        with contextlib.suppress(OSError):
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.touch()

    def scheduler_held(self, state):
        """For a report: why the installed release's scheduler stays stopped,
        after a deploy cut short or failed that may have partly migrated the
        database and, if failed, was not rolled back or pointed back from, and
        what starts it."""
        interrupted = state.get("deploying") or {}
        if self.partly_migrated(interrupted) and "scheduler" in self.roles():
            return {"scheduler": f"stopped: {self.interrupted_migrations(interrupted)}"}
        failed = state.get("failed") or {}
        if not self.partly_migrated(failed) or failed.get("rolled_back") or "scheduler" not in self.roles():
            return {}
        if self.restore_checked(failed):
            return {"scheduler": "stopped: a roll-back restored the database, but did not start Hot. To start it, run "
                                 "roll-back again: it only starts Hot"}
        release, point = state.get("release"), failed.get("restore_point") or {}
        if point.get("kind") != "dump":
            step = self.restore_steps(point, release)
        elif self.storage_progress(failed):
            # roll_back refuses once records may have moved.
            step = self.fix_forward(failed)
        else:
            step = f"run roll-back: it restores {point.get('path')} and starts {release}"
        return {"scheduler": f"stopped: the failed deploy of {self.migrated_by(failed)} may have partly applied its "
                             f"migrations. To start it, {step}"}

    @staticmethod
    def interrupted_migrations(interrupted):
        """Why the installed release must wait after a deploy cut short that may
        have partly migrated the database. That deploy is what ends this: it goes
        on, or fails and says what comes next."""
        return (f"the interrupted deploy of {interrupted.get('release')} may have partly applied migrations. "
                "The next cycle or deploy-now takes that deploy up")

    @staticmethod
    def resume_due(failure, now=None):
        """Whether a resume that failed is due to be tried again."""
        if not failure:
            return False
        now = now or datetime.datetime.now(datetime.timezone.utc)
        try:
            return (now - parse_time(failure["at"])).total_seconds() >= RESUME_RETRY_SECONDS
        except (KeyError, TypeError, ValueError):
            return True

    def start_services(self, bundle, scheduler=True):
        """Start this host's services, the task worker with its release's hotbox,
        the ingress after what it proxies to, and the scheduler last. It raises if
        the app and API do not become ready or any service does not stay up, so a
        deploy or a resume that left one failing says so."""
        roles = self.roles()
        services = [r for r in roles if r != "scheduler"]
        if services:
            self.install_hotbox(bundle)
            self.compose(bundle, "up", "-d", "--remove-orphans", *services)
            self.refresh_ingress(bundle)
            self.wait_ready()
            self.check_stays_up(bundle, services + (["ingress"] if render.web_roles(self.config) else []))
        if scheduler and "scheduler" in roles:
            self.compose(bundle, "up", "-d", "scheduler")
            self.check_stays_up(bundle, ["scheduler"])

    def refresh_ingress(self, bundle):
        """Recreate the ingress on hosts that have one. Compose leaves an ingress
        whose own definition did not change alone, also when its bind-mounted
        nginx.conf did: in a redeploy of the same release it would go on serving
        the old hostnames. nginx looks up the app's and API's addresses while
        serving, cached for 10 seconds, so starting it fresh also skips up to 10
        seconds of a stale address after the recreate this agent just ran."""
        if render.web_roles(self.config):
            self.compose(bundle, "up", "-d", "--force-recreate", "--no-deps", "ingress")

    def check_stays_up(self, bundle, services):
        """Raise unless `services` stay up for a while. Compose says "started"
        whatever a service does next, and the readiness probes ask only the app
        and API, so a worker that exits on a setting it refuses would otherwise
        pass for deployed. A service seen down twice is failing; once may be one
        restart. The message ends with its log, since a failed deploy's recovery
        replaces the container and these hosts have no shell to read it from.
        A Docker that does not answer says nothing about the services, and the
        health metrics report that, so it fails nothing here. Nor does a
        scheduler waiting for another session's lock (scheduler_waits_for_lock):
        Docker brings it up once the lock frees, and the SchedulerRunning alarm
        reports a lock that never does."""
        down = set()
        for look in range(SETTLE_LOOKS):
            if look:
                self.sleep(SETTLE_SECONDS)
            try:
                states = self.service_states(bundle)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                print(json.dumps({"services_not_checked": describe(error)}), file=sys.stderr, flush=True)
                return
            seen = {name for name in services if states.get(name, ("",))[0] in DOWN_STATES}
            failing = sorted(seen & down)
            if failing == ["scheduler"] and states["scheduler"][0] == "restarting" and (
                    self.scheduler_waits_for_lock(bundle)):
                print(json.dumps({"scheduler_waiting": "Another session holds the scheduler's lock; Docker restarts "
                                                       "it until that session ends"}), file=sys.stderr, flush=True)
                return
            if failing:
                names = ", ".join(f"{name} ({states[name][0]})" for name in failing)
                raise AgentError(f"These services did not stay running: {names}{self.log_ending(bundle, failing[0])}")
            down |= seen

    def scheduler_waits_for_lock(self, bundle):
        """Whether the scheduler's last words were that another session holds
        its singleton lock, as a dead host's session does until the server drops
        it. Only its last words count, so a crash after an earlier wait still
        fails; the start of its next attempt says nothing yet. This reads the
        log whole, since log_ending shortens each line, and in the JSON log the
        message comes after other fields."""
        try:
            output = self.compose(bundle, "logs", "--no-color", "--tail", str(4 * ERROR_LINES), "scheduler",
                                  timeout=EXEC_SECONDS).stdout
        except (OSError, subprocess.SubprocessError):
            return False
        lines = [line for line in output.splitlines() if line.strip()]
        while lines and SCHEDULER_STARTING in lines[-1]:
            lines.pop()
        return bool(lines) and SCHEDULER_LOCK_HELD in lines[-1]

    def log_ending(self, bundle, service):
        """The end of a service's redacted log for a failure's message, or
        nothing when it cannot be read."""
        try:
            output = self.compose(bundle, "logs", "--no-color", "--tail", str(4 * ERROR_LINES), service,
                                  timeout=EXEC_SECONDS).stdout
        except (OSError, subprocess.SubprocessError):
            return ""
        lines = bounded_lines(output, Redactor.from_files(self.secrets_root), lines=ERROR_LINES,
                              line_chars=ERROR_CHARS // ERROR_LINES, total=ERROR_CHARS)
        return f". The end of the {service} log: " + " | ".join(line.strip() for line in lines) if lines else ""

    def roll_back(self, bundle):
        """Undo a failed upgrade: restore a local database from the dump
        taken before the release's migrations, then run the installed
        release again. The failed release stays failed, so it is not
        retried; point the installation back at the installed release. A retry
        after the restored database passed its checks only starts Hot: what came
        up since may have written to it."""
        state = self.state()
        interrupted = state.get("deploying") or {}
        if self.partly_migrated(interrupted):
            # An older failed record says nothing of what this attempt did to the
            # database, and the attempt would go on from whatever this restored.
            raise AgentError(f"{state.get('release')} cannot start yet: {self.interrupted_migrations(interrupted)}. "
                             "If it fails, run roll-back then")
        failed = state.get("failed") or {}
        if not failed:
            raise AgentError("Nothing to roll back: the last deploy did not fail")
        failed_steps = failed.get("steps", [])
        if "data-moved" in failed_steps or failed.get("data_moved"):
            raise AgentError("The data already moved to the new database; rolling back would return to the copy "
                             "on this host from before the move. Fix the cause and run deploy-now instead")
        if self.storage_progress(failed):
            raise AgentError("Records may already point at the new storage, so the installed release cannot run "
                             "on the old one. Fix the cause and run deploy-now instead")
        migrated = self.partly_migrated(failed)
        if failed.get("pointed_back") and migrated:
            raise AgentError(f"{state.get('release')} was started again at {failed['rolled_back']} by pointing back at "
                             "it after a restore this host does not do. If the database still holds the failed "
                             "release's migrations, stop Hot and restore it again yourself")
        if failed.get("rolled_back") and migrated:
            raise AgentError(f"Already rolled back at {failed['rolled_back']}, and {state.get('release')} runs "
                             "again; restoring the dump again would lose what was written since")
        restored = self.restore_checked(failed)
        steps = list(failed["roll_back_failed"]["steps"]) if restored else []
        dump = None
        if migrated:
            point = failed.get("restore_point") or {}
            if point.get("kind") != "dump":
                if not point and render.data_mode(self.config, "postgres") == "local":
                    raise AgentError("No dump of this host's database was taken before these migrations: restore "
                                     f"the data disk from a snapshot taken before the deploy, then deploy "
                                     f"{state.get('release')}")
                raise AgentError(f"The database is not on this host: {self.restore_steps(point, state.get('release'))}")
            dump = Path(point["path"])
            if not restored and not dump.exists():
                raise AgentError(f"The restore point {dump} is missing: restore the data disk from a snapshot taken "
                                 "before the deploy")

        def progress(**fields):
            """How far this got, in case a reboot or a killed agent records
            nothing more: only finishing clears it, or roll_back_failed says how
            it failed."""
            self.save_state({**self.state(), **fields, "failed": {**failed, "roll_back_failed": {
                "steps": list(steps), "error": "The roll-back did not finish", "at": utc_now()}}})

        # After a reboot that kept Hot stopped, the tmpfs holds no secrets.
        # They are for the data services the installed release uses.
        self.deliver_secrets(data_modes=state.get("data_modes"))
        try:
            # A replaced VM pulls the installed release's images again.
            self.registry_login()
            if dump and not restored:
                staged = self.data_root / "data/postgres/restore.dump"
                # A copy a reboot or kill left behind, which no `finally`
                # removed, would count against the room for this one.
                staged.unlink(missing_ok=True)
                self.grow_data_disk()
                # Refused while Hot still runs.
                self.check_staging_room(dump, staged.parent, "roll-back")
                self.stop_services(bundle)
                steps.append("stopped")
                # From here the database may not be fit to run on, and a reboot
                # or a killed agent must not start Hot on it.
                progress(stopped=True)
                try:
                    shutil.copy(dump, staged)
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
                # From here a retry must not restore: what starts may write.
                progress()
            self.start_services(bundle)
            steps.append("started")
        except Exception as error:
            self.roll_back_failed(state, failed, steps, dump, error)
        failed = {**{k: v for k, v in failed.items() if k != "roll_back_failed"}, "rolled_back": utc_now()}
        # Hot runs again, so a stop kept by a failed roll-back, a failed start
        # after a boot, or a restart or stop cut short ends here.
        self.save_state({**{k: v for k, v in state.items()
                            if k not in ("stopped", "stopping", "resume_failed", "restarting")}, "failed": failed})
        self.settle_boot()
        self.remember({"release": state.get("release"), "rolled_back_from": failed["release"],
                       "restore_point": failed.get("restore_point"), "steps": steps, "finished": utc_now()})
        return {"release": state.get("release"), "rolled_back_from": failed["release"], "steps": steps}

    def roll_back_failed(self, state, failed, steps, dump, error):
        """Record a roll-back that stopped partway, then raise CommandFailed.
        If it stopped before the restored database passed the installed
        release's checks, Hot stays stopped, across reboots too: the database
        may be partly restored, or not fit to run on. Running roll-back again
        restores the dump from the start; after the checks passed, it only
        starts Hot."""
        explained = self.explain(error)
        # A retry that failed before it stopped anything left the database as an
        # earlier attempt did, and that attempt's steps are the only record of it.
        attempt = (failed.get("roll_back_failed") or {}).get("steps", [])
        reached = attempt if "stopped" in attempt and "checked" not in attempt and "stopped" not in steps else steps
        restoring = "stopped" in reached and "checked" not in reached
        recovery = ""
        if restoring:
            recovery = ("Hot stays stopped: the database was restored, but the installed release's checks on it failed. "
                        if "restored" in reached else "Hot stays stopped, and the database may be partly restored. ")
        recovery += (("The database is restored and passed the installed release's checks, but Hot did not finish "
                      "starting, and what did start runs without the scheduler. Fix the cause, then run roll-back "
                      "again: it only starts Hot, and does not restore the database again.") if "checked" in steps
                     else f"Fix the cause, then run roll-back again: it restores {dump} from the start." if dump
                     else "Fix the cause, then run roll-back again.")
        failed = {**failed, "roll_back_failed": {"steps": reached, "error": explained, "at": utc_now()}}
        # Hot stays as it was before this roll-back, once the database is fit.
        kept = {k: v for k, v in self.state().items() if k != "stopped"}
        self.save_state({**kept, "failed": failed, **({"stopped": True} if restoring or state.get("stopped") else {})})
        self.remember({"release": state.get("release"), "roll_back_failed_from": failed["release"],
                       "steps": steps, "error": explained, "recovery": recovery, "finished": utc_now()})
        raise CommandFailed(f"Rolling back failed: {explained}", release=state.get("release"),
                            rolled_back_from=failed["release"], steps=steps, recovery=recovery) from error

    def diagnostics(self, bundle, probe=None):
        """What support needs to understand an installation, redacted:
        versions, effective configuration, services, dependencies, health
        metrics, and the certificate. Logs are a separate command."""
        redact = Redactor.from_files(self.secrets_root)
        state = self.state()
        result = {
            "versions": {"release": state.get("release"), "previous": state.get("previous"),
                         "migrations": state.get("migrations"), "host": self.host_versions(),
                         "hotbox": self.hotbox_version()},
            "deploys": self.history()[-5:],
            "configuration": effective_configuration(self.config, redact),
        }
        # As the installed release uses them, and what the configuration asks
        # for where a data move has not finished.
        installed, configured = self.installed_modes(state), self.configured_modes()
        result["dependencies"] = [{"service": service, "mode": installed[service],
                                   **({"configured_mode": configured[service]} if configured[service] != installed[service]
                                      else {})}
                                  for service in ("postgres", "valkey")]
        # Only services off the host have addresses, which may need the
        # operator secret bundle; reading it can fail without failing the rest.
        remote = [d for d in result["dependencies"] if d["mode"] != "local"]
        try:
            for entry, (host_name, port) in zip(remote, self.dependencies() if remote else []):
                entry.update(endpoint=f"{host_name}:{port}", reachable=tcp_reachable(host_name, port))
        except (AgentError, KeyError, TypeError, ValueError) as error:  # an incomplete operator secret: say so
            result["dependencies_error"] = describe(error)
        if bundle is not None:
            try:
                result["services"] = {
                    e.get("Service", "?"): {"state": e.get("State", ""), "health": e.get("Health", ""),
                                            "status": e.get("Status", ""), "exit_code": e.get("ExitCode", 0),
                                            "image": e.get("Image", "").rsplit("@", 1)[-1][:19]}
                    for e in self.containers(bundle)}
            except (subprocess.SubprocessError, OSError, ValueError) as error:  # Docker down: what this report is for
                result["services_error"] = self.explain(error)
            try:
                compose = json.loads((bundle / "compose.json").read_text())
                # As the services see them: Compose's escaping doubles every $.
                result["environment"] = {name: {key: value.replace("$$", "$") if isinstance(value, str) else value
                                                for key, value in sorted(compose["services"][name].get("environment", {}).items())}
                                         for name in self.roles() if name in compose["services"]}
            except (OSError, ValueError, KeyError, AttributeError) as error:  # a bundle not all there: say so
                result["environment_error"] = describe(error)
        try:
            metrics = self.observe(probe)
        except (AgentError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            result["health_error"] = self.explain(error)
        else:
            result["health"] = {name: value for name, (value, _) in metrics.items()}
        # Past what the failed record names: the operator has no shell to read
        # the manifest with, and leaving these behind takes their ids.
        failed = state.get("failed") or {}
        named = failed.get("storage_failed")
        if named:
            try:
                manifest = named.get("manifest") or failed["storage_manifests"][named["role"]]
                result["storage_failed"] = self.failed_objects(manifest, named["role"], listed=STORAGE_FAILED_DIAGNOSED)
            except (OSError, KeyError) as error:
                result["storage_failed_error"] = describe(error)
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


def partial_write_errors(body):
    """How many entries Cloud Logging refused in a partialSuccess write
    whose other entries it wrote, or None for an error that refused the
    whole request."""
    try:
        details = json.loads(body or b"{}").get("error", {}).get("details", [])
    except (ValueError, AttributeError):
        return None
    for detail in details if isinstance(details, list) else []:
        if isinstance(detail, dict) and str(detail.get("@type", "")).endswith("WriteLogEntriesPartialErrors"):
            return len(detail.get("logEntryErrors") or {})
    return None


def journal_entry(record, redact, source, instance=""):
    """One `journalctl --output=json` record as shipped: its source, unit,
    priority, redacted message, and the instance that logged it."""
    message = record.get("MESSAGE") or ""
    if isinstance(message, list):  # journald sends non-UTF-8 messages as bytes
        message = bytes(message).decode("utf-8", "replace")
    priority = int(record.get("PRIORITY", 6))
    fields = {"source": source, "unit": record.get("_SYSTEMD_UNIT") or record.get("SYSLOG_IDENTIFIER", ""),
              "priority": priority, "message": redact(str(message))}
    if instance:
        fields["instance"] = instance
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
    restore, and a release names the oldest release it upgrades from. A host
    with no level to go by (`migrations` None) checks only the second."""
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


def migration_level(output):
    """The level in what the migration status job printed, one JSON line."""
    lines = (output or "").strip().splitlines()
    try:
        level = json.loads(lines[-1])["level"]
    except (IndexError, ValueError, KeyError, TypeError):
        level = None
    if not isinstance(level, int) or isinstance(level, bool) or level < 0:
        raise UnreadableLevel("the migration status job's output names no level")
    return level


def parse_time(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


def expiry(value):
    """When a command expires, in seconds since the epoch, from a number or
    the text of one. None for no expiry: nothing sent, or anything else."""
    try:
        seconds = None if isinstance(value, bool) else float(value)
    except (TypeError, ValueError):
        return None
    return seconds if seconds is not None and seconds > 0 else None


def recorded_since(recorded, sent):
    """Whether a time the agent recorded, to the second, is at or after when a
    command was sent. Unknown times (older records, no send time) are not."""
    try:
        # Pub/Sub's times have up to nine fractional digits.
        return parse_time(recorded) >= parse_time(re.sub(r"\.\d+", "", sent))
    except (AttributeError, TypeError, ValueError):
        return False


def tcp_reachable(host, port, timeout=3):
    try:
        socket.create_connection((host, int(port)), timeout=timeout).close()
        return True
    except OSError:
        return False


def describe(error, redact=None):
    """A failure in a few words. For a command: what ran, without the
    bundle's path, so the job (migrate, provision, check-postgres, ...) is
    named, and how it ended. Given a redactor, the end of its error output
    (or of its output, when it wrote no errors) follows, redacted. Errors
    other than AgentError and ValueError also name their type."""
    if isinstance(error, (subprocess.CalledProcessError, subprocess.TimeoutExpired)):
        parts = [str(part) for part in (error.cmd if isinstance(error.cmd, (list, tuple)) else [error.cmd])]
        if parts[:2] == ["docker", "compose"] and "-f" in parts:
            del parts[parts.index("-f"):parts.index("-f") + 2]
        ended = (f"timed out after {error.timeout:g} seconds" if isinstance(error, subprocess.TimeoutExpired)
                 else f"exited {error.returncode}")
        text = f"{' '.join(parts[:14])} {ended}"
        if redact is None:
            return text
        output = ""
        for stream in (error.stderr, error.output):
            output = stream.decode("utf-8", "replace") if isinstance(stream, bytes) else stream or ""
            if output.strip():
                break
        # Redacted whole, then shortened, so no secret is cut in two.
        tail = redact("\n".join(output.splitlines()[-4 * ERROR_LINES:])).splitlines()
        tail = [line.strip() for line in tail if line.strip()][-ERROR_LINES:]
        return redact(text) + (": " + " | ".join(tail)[-ERROR_CHARS:] if tail else "")
    text = str(error) if isinstance(error, (AgentError, ValueError)) else f"{type(error).__name__}: {error}"
    return redact(text) if redact else text


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


def new_agent():
    """An agent on the configuration in the file, as the loop last refreshed it."""
    config = json.loads(INSTALLATION.read_text())
    return Agent(config, cloud_for(config))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("run")
    command = sub.add_parser("command")
    command.add_argument("operation", choices=sorted(COMMANDS))
    command.add_argument("--service", default="")
    command.add_argument("--request-id", default="")
    command.add_argument("--expires", default="")
    args = parser.parse_args(argv)
    if args.mode == "command":
        # Systems Manager starts this at once, and it may then wait for the
        # lock through a whole deploy; by then the helper may have stopped
        # waiting for it (`expires`), and the loop may have taken up and
        # deployed a new configuration. The file is read only with the lock
        # held, or deploy-now would see the one from before the wait as a
        # change and deploy that. The loop writes the file only between its
        # sleeps, so an apply just before this may not have reached it, and a
        # failed release is never tried again by the loop: this reads the stack's
        # own. The loop's next cycle then finds its own copy out of date, and
        # goes on from there.
        sent = utc_now()
        with locked():
            agent = new_agent()
            try:
                agent.refresh_configuration()
            except Exception as error:  # still answered, on the configuration in the file
                print(json.dumps({"configuration_not_refreshed": describe(error)}), file=sys.stderr, flush=True)
            with agent.heartbeat():
                report = agent.answer(args.operation, args.service, args.request_id, sent, args.expires)
        return 1 if report.get("state") in FAILED_STATES else 0
    agent = new_agent()
    while True:
        with locked():
            agent.run_once()
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    sys.exit(main())
