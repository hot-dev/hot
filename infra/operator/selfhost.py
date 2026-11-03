#!/usr/bin/env python3
"""Operate a self-hosted Hot installation through its host agent.

    selfhost.py installation.tfvars.json command OPERATION [--service S]
    selfhost.py installation.tfvars.json logs SERVICE
    selfhost.py installation.tfvars.json release VERSION
    selfhost.py installation.tfvars.json maintenance start --minutes N
    selfhost.py installation.tfvars.json maintenance end

Uses the `aws` or `gcloud` CLI with the caller's identity, such as the
release role. A command goes to the agent through the SSM command document
(AWS) or the command topic (GCP); the agent's report comes back through the
installation's log, matched by request ID. Nothing here reaches the host over
the network, and nothing needs SSH. The release workflow and the command
workflow run this same script.

Prints the agent's report as JSON (or as Markdown with --summary FILE) and
exits 1 when the agent rejects the command or the deploy fails. On AWS a
scalable installation's command reaches every host, and the helper waits for
each one. On GCP it reaches one host, so the helper sends only status,
diagnostics, and test-alert there, and says what to do instead of the rest. No
command stops a scalable installation, so on either cloud the helper refuses
drain-and-stop there and names the group resize instead, and what to do before
hosts come back after a database restore. Its database and Valkey are not on its
hosts, so it refuses backup, roll-back, and any service its hosts do not run, which
the AWS command document leaves out. A release waits 30
minutes unless --timeout says otherwise, or 60 on a scalable installation, where
--timeout covers the whole release. All of this follows the installation's
preset, so a scalable installation that the preset calls single-machine is
refused. While waiting, a failed read of the log, a group, or a release pointer
is tried again until the wait ends; a failed send or pointer write stops at once.
Release pointers are read before anything moves, so one this identity may not
read, or a sign-in that has lapsed by then, stops at once too. The log and the
groups are read once a command or release is under way, so those reads go on with
a warning whatever the error, even a sign-in that lapses during the wait.

`maintenance` silences the installation's infrastructure alerts during
planned work: on AWS it disables the alarms' actions until `maintenance end`;
on GCP it snoozes the alert policies, which ends by itself after N minutes or
at `maintenance end`. Alerts still change state; nobody is notified. On AWS,
an apply leaves the actions off, so `status` and `diagnostics` warn until
`maintenance end`, which lists the alarms already in ALARM: they notify only
once they recover and breach again.
"""
import argparse
import base64
import datetime
import json
import re
import secrets
import subprocess
import sys
import time

OPERATIONS = ("deploy-now", "status", "diagnostics", "logs", "backup", "restart-service", "drain-and-stop",
              "roll-back", "test-alert")
# Final states per operation; anything else is progress or another request.
FAILED = {"failed", "rejected", "migration-failed", "resume-failed", "expired"}
# The reports that settle a host's deploy of a release. Every report carries
# the host's release, so status, logs, and the rest match it too.
OUTCOMES = ("deployed", "current", "failed")
# A scalable release rolls roles in this order, after migrating; the
# scheduler is replaced last, stop-before-start.
ROLE_ORDER = ("task-worker", "worker", "api", "app", "scheduler")
# What a scalable installation's hosts run, which is all its AWS command document
# lets a command name. The ingress runs beside the app and the API.
HOST_SERVICES = (*ROLE_ORDER, "ingress")
# The release account only views GCP instance groups; replacing an instance takes another identity.
GROUP_IDENTITY = "with an identity that may change instance groups (the release account may not)"
# A host that comes back has no record of a stop or a restore: it deploys its
# role's pointer, and the scheduler's first migrates from the migrate pointer. So
# pointers left on a newer release take a restored database forward again. A release
# cannot put this right with every group at 0: it moves the migrate pointer and waits
# for a report that no host is there to make. So the pointers go first. Then the
# scheduler comes back alone, since only its host checks the database's level: role
# hosts deploy their pointers whatever it is. If the restore did not lower it (the
# wrong time, or hosts still using the old instance), a release refused there leaves
# nothing else serving. Past the migration, the release waits for each other group.
BACK_FROM_A_RESTORE = ("A host that comes back deploys and migrates from its pointers, so after a database restore "
                       "write all six to the earlier release first, which {identity} may: {write}, for each of "
                       f"{', '.join(('migrate', *ROLE_ORDER))}. Then restore only the scheduler's size (an apply would "
                       "restore them all), and run release with that version: only that host checks the database's "
                       "level, and the others would serve whatever it is. Once release says the migration has passed, "
                       "restore the other groups, and it waits for them")
# On GCP a scalable installation's hosts share one command subscription, so a
# command reaches whichever host takes it first. Any host answers status,
# diagnostics, and test-alert, and its report names it. These would act on, or
# ask about, that host alone, so they are refused with what to do instead. On AWS
# the command document leaves out backup and roll-back, so they are refused there
# too, with the same words.
ONE_HOST_INSTEAD = {
    "deploy-now": "To retry a failed role, replace its group's instances; to retry a failed migration, recreate the "
                  f"scheduler's instance; do either {GROUP_IDENTITY}; then run release again. See Upgrading in "
                  "infra/README.md",
    "restart-service": "Replace the role's instances, which deliver the current secrets as they deploy, "
                       f"{GROUP_IDENTITY}. See Rotating secrets and certificates in infra/README.md",
    "logs": "Read the containers' logs in Cloud Logging, which records each line's instance and hot.role label. With "
            "logging.destination = local they stay on each host, out of reach of any command: set it to cloud, which "
            "replaces the hosts",
    "drain-and-stop": "It would stop that host and leave the others serving and writing. Nothing stops every host at "
                      f"once here: {GROUP_IDENTITY}, resize each group to 0 with gcloud, with its autoscaling off "
                      "first. " + BACK_FROM_A_RESTORE.format(
                          identity="the release account",
                          write="printf '{\"release\": \"VERSION\"}' | gcloud secrets versions add "
                                "hot-NAME-release-POINTER --data-file=-") + ". See Upgrading in infra/README.md",
    "roll-back": "Scalable hosts have no roll-back: fix forward, or restore the database to a point in time. See "
                 "Upgrading in infra/README.md",
    "backup": "A scalable installation's database is not on its hosts: back it up with its provider",
}
# On AWS a scalable installation's app and API groups replace a host that fails
# the load balancer's health check, as a stopped one does. Its replacement has
# no record of the stop, so it deploys its role's release. So no command stops
# the installation, and only resizing its groups does. The scheduler goes first:
# it runs the migrations and measures the jobs that workers scale on.
RESIZE_TO_STOP = ("The app and API groups replace a host that fails the load balancer's health check, as a stopped "
                  "one does, and the replacement deploys its role's release and serves again within minutes. To stop "
                  "Hot, resize each group to 0 with an identity that may (the release role may not), "
                  "hot-{name}-scheduler first, then hot-{name}-worker and hot-{name}-task-worker, then hot-{name}-api "
                  "and hot-{name}-app: aws autoscaling update-auto-scaling-group --auto-scaling-group-name GROUP "
                  "--min-size 0 --desired-capacity 0. An autoscaled task-worker group protects its hosts from "
                  "scale-in, so terminate those too. An apply restores the sizes, so apply nothing while the stop "
                  "must hold. " + BACK_FROM_A_RESTORE.format(
                      identity="the release role",
                      write="aws ssm put-parameter --name /hot/{name}/release/POINTER --value VERSION --type String "
                            "--overwrite") + ". See Operating an installation in infra/README.md")
# The stacks leave alarm actions to `maintenance`, so an apply keeps them off and
# the plan's drift check does not see them. These reports are where a forgotten
# `maintenance end` shows.
REPORTS_SILENCE = ("status", "diagnostics")
ALARMS_SILENCED = ("Alarm actions are off on {off} of {total} alarms, so nobody is notified when they go off, and an "
                   "apply does not turn them back on. Once the planned work is over, run `python3 "
                   "infra/operator/selfhost.py installation.tfvars.json maintenance end`, which does")
# A preset left at single-machine, as in a copied workflow, would not wait for
# every host or refuse what reaches only one.
WRONG_PRESET = ("this installation is scalable; set preset to scalable, in the tfvars file or on the workflow's "
                "caller. As single-machine the helper takes one host's answer for the whole installation")
STATUS_STREAM = "hot-agent-status"
# Log timestamps come from the host, so a read of its reports allows for this
# many seconds of clock skew.
CLOCK_SKEW = 120
# How a read of a pointer says it has never been written: SSM's
# ParameterNotFound, or Secret Manager's NOT_FOUND for a secret with no version.
NOT_WRITTEN = re.compile(r"not[ _]?found", re.IGNORECASE)
# How a read says this identity may not make it, which no number of tries changes.
DENIED = ("AccessDenied", "PERMISSION_DENIED", "does not have permission")
# How a read says there is no valid sign-in to make it with, which no number of
# tries changes either: the AWS CLI's errors, a profile or config file it cannot
# find among them, then gcloud's.
SIGNED_OUT = ("ExpiredToken", "UnrecognizedClient", "InvalidClientTokenId", "InvalidSignature",
              "Unable to locate credentials", "Partial credentials", "could not be found", "MFA token required",
              "Token has expired", "SSO session", "SSO Token", "aws login", "login session", "login credentials",
              "problem refreshing", "problem reauthenticating", "active account", "gcloud auth login", "Please run:",
              "Reauthentication", "UNAUTHENTICATED", "Failed to load credential file", "Failed to impersonate")
# A host answers a pointer by acting on it. One that had already acted on this
# release when its pointer was last written, as when the pointer went away and
# came back before the host saw it, has nothing new to say.
REPOINTED = (". A host that had already acted on this release when its pointer was last written reports nothing new, "
             "so releasing again will not end this wait: see Upgrading in infra/README.md")
# With no migration host, a release has moved only the migrate pointer. A new host
# reports as it boots, so releasing again does end this wait.
NO_MIGRATION_HOST = ("the scheduler's group has no instance in service, so no host has answered the migrate pointer. "
                     "Restore its size, then release again. A host that comes back deploys its role's pointer, so "
                     "after a database restore first write every role's pointer to {version}: see Upgrading in "
                     "infra/README.md")
# What a role's wait names while its group has no host to wait for. A new host
# reports as it boots, so unlike REPOINTED, releasing again does end this wait.
NO_ROLE_HOST = "an instance in service"
NO_ROLE_HOST_STEP = ". Its group is empty, and a new host reports as it boots: restore its size, then release again"
# Cloud Logging returns the newest this many entries per read: enough for
# each host's latest reports about a release.
LOG_ENTRIES = 1000
# CloudWatch takes up to 100 alarm names per call; a snooze names up to 16
# alert policies.
ALARMS_PER_CALL = 100
POLICIES_PER_SNOOZE = 16


class CliError(Exception):
    pass


class TimedOut(CliError):
    """A wait that ended. `unread` is the error of its last read, if that failed."""

    def __init__(self, message, unread=None):
        super().__init__(message)
        self.unread = unread


class Cli:
    """Runs a cloud CLI and returns its parsed JSON output."""

    def __call__(self, *args, stdin=None):
        result = subprocess.run(list(args), input=stdin, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            said = what_failed((result.stderr or result.stdout).strip().splitlines())
            raise CliError((said or f"{args[0]} exited {result.returncode}")[:300])
        return json.loads(result.stdout or "null")


class Installation:
    def __init__(self, document, cli, clock=time.time, sleep=time.sleep):
        config = document.get("installation", document)
        self.name = config["name"]
        self.cloud = config["cloud"]
        self.project = config.get("project", "")
        self.zone = config.get("zone", "")
        # A GCP zone names its region: us-central1-a is in us-central1.
        self.region = config.get("region") or (self.zone.rsplit("-", 1)[0] if self.cloud == "gcp" else "")
        self.scalable = config.get("preset", "single-machine") == "scalable"
        self.cli = cli
        self.clock = clock
        self.sleep = sleep

    # -- sending ---------------------------------------------------------------

    def send(self, operation, service, request_id, timeout=600):
        """Send one command. It expires when the helper stops waiting for
        it, so a command reported as unanswered does not run later: past the
        `expires` time (seconds since the epoch) the agent reports `expired`
        instead of running it. Systems Manager also starts the agent's command
        at once, where it may wait for the lock, and drops one that has not
        started by then. Hosts without the service exit 1 on a rejection, and
        Systems Manager would otherwise stop sending to the rest."""
        expires = int(self.clock() + timeout)
        if self.cloud == "aws":
            parameters = f"operation={operation},service={service or 'none'},requestId={request_id},expires={expires}"
            self.cli("aws", "ssm", "send-command", "--region", self.region, "--output", "json",
                     "--document-name", f"hot-{self.name}-command",
                     "--targets", f"Key=tag:HotInstallation,Values={self.name}",
                     "--max-errors", "100%", "--timeout-seconds", str(max(30, int(timeout))),
                     "--parameters", parameters, "--comment", f"Hot {operation}")
        else:
            message = json.dumps({"operation": operation, "service": service, "request_id": request_id,
                                  "expires": expires})
            self.cli("gcloud", "pubsub", "topics", "publish", f"hot-{self.name}-commands",
                     "--project", self.project, "--message", message, "--format", "json")

    def point_at(self, release, role=""):
        """Write the release pointer: the installation's, or on a scalable
        installation one role's (or "migrate", for the migration host)."""
        suffix = f"/{role}" if role else ""
        if self.cloud == "aws":
            self.cli("aws", "ssm", "put-parameter", "--region", self.region, "--output", "json",
                     "--name", f"/hot/{self.name}/release{suffix}", "--value", release, "--type", "String", "--overwrite")
        else:
            self.cli("gcloud", "secrets", "versions", "add", f"hot-{self.name}-release{suffix.replace('/', '-')}",
                     "--project", self.project, "--data-file=-", "--format", "json",
                     stdin=json.dumps({"release": release}))

    def pointer(self, role):
        """What a role's pointer says, or None however it could not be read."""
        try:
            return self.read_pointer(role)
        except (CliError, ValueError):
            return None

    def read_pointer(self, role):
        """What a role's pointer says, or None if it has never been written or
        names no release. A read that fails raises: it says neither."""
        try:
            if self.cloud == "aws":
                found = self.cli("aws", "ssm", "get-parameter", "--region", self.region, "--output", "json",
                                 "--name", f"/hot/{self.name}/release/{role}")
            else:
                found = self.cli("gcloud", "secrets", "versions", "access", "latest", "--secret",
                                 f"hot-{self.name}-release-{role}", "--project", self.project, "--format", "json")
        except CliError as error:
            # gcloud says a credential file it cannot find was not found, too.
            if NOT_WRITTEN.search(str(error)) and not any(word in str(error) for word in SIGNED_OUT):
                return None
            raise
        try:
            if self.cloud == "aws":
                return found["Parameter"]["Value"]
            return json.loads(base64.b64decode(found["payload"]["data"]))["release"]
        except (KeyError, TypeError, ValueError):
            return None

    def pointer_written(self, role):
        """When a role's pointer was last written, by the cloud's clock, in
        seconds since the epoch. Not knowing stops the release: this decides
        whether a report is about this release at all."""
        try:
            if self.cloud == "aws":
                found = self.cli("aws", "ssm", "get-parameter", "--region", self.region, "--output", "json",
                                 "--name", f"/hot/{self.name}/release/{role}")
                written = found["Parameter"]["LastModifiedDate"]
                # AWS CLI v2 prints a time, and v1 seconds since the epoch.
                return written if isinstance(written, (int, float)) else parse_time(written)
            found = self.cli("gcloud", "secrets", "versions", "describe", "latest", "--secret",
                             f"hot-{self.name}-release-{role}", "--project", self.project, "--format", "json")
            return parse_time(found["createTime"])
        except (CliError, KeyError, TypeError, ValueError) as error:
            raise CliError(f"Could not tell when the {role} pointer was last written, so its reports may be about an "
                           f"earlier release of this version: {error}. The stack lets the release identity read this; "
                           "apply it, then release again") from error

    def pointed_since(self, role, version, deadline, poll=10):
        """When a role's pointer was written, if it already says `version`; None
        if it says anything else or nothing, so it needs writing. A failed read
        says neither, and must not be taken for a pointer to write: that moves
        the write time, a host that has already acted on the release reports
        nothing new, and reports from before count for nothing, so this run and
        every later one would time out. So it is tried again, as in a wait,
        unless no try can change it: this identity may not make it, or its
        sign-in has lapsed. Nothing has moved yet, so stopping costs nothing."""
        unread = None
        while True:
            try:
                if self.read_pointer(role) != version:
                    return None
                return self.pointer_written(role)
            except CliError as error:
                if any(word in str(error) for word in (*DENIED, *SIGNED_OUT)):
                    raise
                unread = note_unread(error, unread)
            if self.clock() >= deadline:
                raise TimedOut(f"Could not read the {role} pointer in time, so nothing moved: {unread}", unread)
            self.sleep(poll)

    def check_preset(self):
        """Refuse, before anything moves, a scalable installation that the
        preset calls single-machine. Only a scalable installation has a role's
        pointer: on a single machine it does not exist and the release role
        may not read it, and pointer() answers None to both. A scalable GCP
        pointer has no version until its first release, which reads the same,
        so run() also looks at who answered."""
        if not self.scalable and self.pointer("scheduler") is not None:
            raise CliError(f"Not sent: {WRONG_PRESET}")

    def instances(self, *roles):
        """The running instances in the roles' groups."""
        groups = [f"hot-{self.name}-{role}" for role in roles]
        if self.cloud == "aws":
            found = self.cli("aws", "autoscaling", "describe-auto-scaling-groups", "--region", self.region,
                             "--output", "json", "--auto-scaling-group-names", *groups)
            return sorted(i["InstanceId"] for g in found.get("AutoScalingGroups", []) for i in g.get("Instances", [])
                          if i.get("LifecycleState") == "InService")
        return sorted(name for role in roles for name in self.gcp_instances(role))

    def group_is_empty(self, role):
        """Whether a read finds no instance of a role in service. A read that
        fails says neither, and only explains what a report already says, so it
        does not replace that report."""
        try:
            return not self.instances(role)
        except (CliError, AttributeError, KeyError, TypeError, ValueError):
            return False

    def gcp_instances(self, role):
        """A role's running instances on GCP, each with its zone. The
        scheduler's group is zonal; the others are regional and spread across
        zones, so each instance's zone comes from its URL."""
        location = ["--zone", self.zone] if role == "scheduler" else ["--region", self.region]
        found = self.cli("gcloud", "compute", "instance-groups", "managed", "list-instances", f"hot-{self.name}-{role}",
                         *location, "--project", self.project, "--format", "json") or []
        urls = [i["instance"].split("/") for i in found if i.get("instanceStatus") == "RUNNING"]
        return {url[-1]: url[-3] for url in urls}

    # -- maintenance ---------------------------------------------------------------

    def alerts(self):
        """The installation's alarms (AWS) or alert policies (GCP). Another
        installation's name may start with this one's (prod and prod-eu), so
        the name prefix only narrows the list: an alarm is this
        installation's when it notifies the installation's topic, and a
        policy when it watches the installation's metrics or database."""
        prefix = f"hot-{self.name}-"
        if self.cloud == "aws":
            topic = f":hot-{self.name}-alerts"
            found = self.cli("aws", "cloudwatch", "describe-alarms", "--region", self.region, "--output", "json",
                             "--alarm-name-prefix", prefix)
            return sorted((a for a in (found or {}).get("MetricAlarms", [])
                           if any(arn.endswith(topic) for arn in a.get("AlarmActions", []) + a.get("OKActions", []))),
                          key=lambda a: a["AlarmName"])
        markers = (f"custom.googleapis.com/hot/{self.name}/", f':hot-{self.name}"')
        found = self.cli("gcloud", "monitoring", "policies", "list", "--project", self.project, "--format", "json") or []
        return sorted((p for p in found if p.get("displayName", "").startswith(prefix)
                       and any(m in text for text in strings(p.get("conditions", [])) for m in markers)),
                      key=lambda p: p["name"])

    def maintenance(self, action, minutes=60):
        """Silence the installation's alerts, or stop silencing them."""
        listed = self.alerts()
        if not listed:
            raise CliError(f"No alerts found for hot-{self.name}; has the stack been applied?")
        alerts = [a["AlarmName"] if self.cloud == "aws" else a["name"] for a in listed]
        snooze = f"hot-{self.name} maintenance"
        alarming = []
        if self.cloud == "aws":
            # CloudWatch notifies on a change of state only, so an alarm that
            # went off while silenced stays quiet until it recovers.
            alarming = [a["AlarmName"] for a in listed if action == "end" and a.get("StateValue") == "ALARM"]
            verb = "disable-alarm-actions" if action == "start" else "enable-alarm-actions"
            for start in range(0, len(alerts), ALARMS_PER_CALL):
                self.cli("aws", "cloudwatch", verb, "--region", self.region, "--output", "json",
                         "--alarm-names", *alerts[start:start + ALARMS_PER_CALL])
            until = "until maintenance end" if action == "start" else ""
        elif action == "start":
            now = datetime.datetime.fromtimestamp(self.clock(), datetime.timezone.utc)
            until = (now + datetime.timedelta(minutes=minutes)).strftime("%Y-%m-%dT%H:%M:%SZ")
            for start in range(0, len(alerts), POLICIES_PER_SNOOZE):
                self.cli("gcloud", "monitoring", "snoozes", "create", "--project", self.project, "--format", "json",
                         "--display-name", snooze, "--criteria-policies", ",".join(alerts[start:start + POLICIES_PER_SNOOZE]),
                         "--start-time", now.strftime("%Y-%m-%dT%H:%M:%SZ"), "--end-time", until)
        else:
            now = self.clock()
            for found in self.cli("gcloud", "monitoring", "snoozes", "list", "--project", self.project,
                                  "--format", "json") or []:
                ends = found.get("interval", {}).get("endTime", "")
                if found.get("displayName") == snooze and ends and parse_time(ends) > now:
                    self.cli("gcloud", "monitoring", "snoozes", "cancel", found["name"], "--project", self.project,
                             "--format", "json")
            until = ""
        state = "maintenance" if action == "start" else "monitoring"
        warning = (f"{', '.join(alarming)} went into ALARM while silenced; CloudWatch notifies only once an alarm "
                   "recovers and breaches again, so check them now") if alarming else ""
        return {"installation": self.name, "state": state, "alerts": len(alerts),
                **({"until": until} if until else {}),
                **({"alarming": alarming, "warning": warning} if alarming else {})}

    def silenced(self):
        """What a report adds while alarm actions are off on AWS. Not being
        able to look fails nothing, but says so: nothing else would show it."""
        try:
            listed = self.alerts()
        except CliError as error:
            return {"warning": f"Could not check whether the alarms' actions are on: {error}"}
        off = [a["AlarmName"] for a in listed if a.get("ActionsEnabled") is False]
        return {"silenced": off, "warning": ALARMS_SILENCED.format(off=len(off), total=len(listed))} if off else {}

    # -- reports -----------------------------------------------------------------

    def reports(self, request_id, since, field="request_id"):
        """The agent's reports for one request (or, with field="release",
        about one release), oldest first."""
        if self.cloud == "aws":
            result = self.cli("aws", "logs", "filter-log-events", "--region", self.region, "--output", "json",
                              "--log-group-name", f"/hot/{self.name}", "--log-stream-names", STATUS_STREAM,
                              "--start-time", str(int(since * 1000)),
                              "--filter-pattern", f'{{ $.{field} = "{request_id}" }}')
            return [json.loads(event["message"]) for event in (result or {}).get("events", [])]
        started = datetime.datetime.fromtimestamp(since, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        query = (f'logName="projects/{self.project}/logs/hot-{self.name}-status" '
                 f'AND jsonPayload.{field}="{request_id}" AND timestamp>="{started}"')
        # The newest entries, so a large release or a busy log cannot push the
        # latest reports out of the read; returned oldest first, as on AWS.
        entries = self.cli("gcloud", "logging", "read", query, "--project", self.project,
                           "--format", "json", "--order", "desc", "--limit", str(LOG_ENTRIES)) or []
        return [entry["jsonPayload"] for entry in reversed(entries)]

    def run(self, operation, service="", request_id="", timeout=600, poll=10):
        """Send one command and wait for the agent's answer to it. On AWS a
        scalable installation's command reaches every host, so wait for each
        host that was running when it was sent and is still running. If some
        never answer, the report fails but keeps what the others said."""
        if operation not in OPERATIONS:
            raise CliError(f"Unknown operation {operation}")
        # The command workflow passes its service input with any operation.
        service = service if operation in ("logs", "restart-service") else ""
        if self.scalable and service not in (*HOST_SERVICES, "", "none"):
            raise CliError(f"Not sent: a scalable installation's hosts do not run {service}. They run "
                           f"{', '.join(HOST_SERVICES)}. Its database and Valkey are with their provider: read their "
                           "logs and restart them there")
        if self.cloud == "gcp" and self.scalable and operation in ONE_HOST_INSTEAD:
            raise CliError(f"Not sent: on a scalable GCP installation {operation} reaches only the one host that takes "
                           f"it. {ONE_HOST_INSTEAD[operation]}")
        if self.scalable and operation == "drain-and-stop":
            raise CliError(f"Not sent: on a scalable AWS installation {operation} would not hold. "
                           f"{RESIZE_TO_STOP.format(name=self.name)}")
        if self.scalable and operation in ("backup", "roll-back"):
            raise CliError(f"Not sent: {operation} is for single-machine installations. {ONE_HOST_INSTEAD[operation]}")
        self.check_preset()
        request_id = request_id or f"cli-{secrets.token_hex(6)}"
        since = self.clock() - CLOCK_SKEW
        targets = set(self.instances(*ROLE_ORDER)) if self.cloud == "aws" and self.scalable else None
        notes = self.silenced() if self.cloud == "aws" and operation in REPORTS_SILENCE else {}
        self.send(operation, service, request_id, timeout)
        deadline = self.clock() + timeout
        reports, answered, unread = [], {}, None
        while True:
            try:
                reports = self.reports(request_id, since)
                if targets is not None:
                    answered = latest_by_instance(reports)
                    targets &= set(self.instances(*ROLE_ORDER))
            except CliError as error:
                # Nothing new: the command is out, and the log keeps its
                # answers for the next read.
                unread = note_unread(error, unread)
            else:
                unread = None
                if targets is None:
                    if reports:
                        if self.scalable or not reports[-1].get("roles"):
                            return {**self.steps_it_allows(reports[-1]), **notes}
                        # Only a scalable agent names its roles. check_preset()
                        # could not tell, so this command is already out.
                        report = combined(latest_by_instance(reports), service)
                        error = (f"Sent, but {WRONG_PRESET}. Hosts that have not answered may still be acting on it: "
                                 "check status before sending it again")
                        return {**report, **notes, "state": "failed",
                                "error": "; ".join(filter(None, (report.get("error"), error)))}
                elif answered and targets <= set(answered):
                    return {**combined(answered, service), **notes}
            if self.clock() >= deadline:
                where = ("the command's invocation in Systems Manager" if self.cloud == "aws"
                         else f"the agent's journal in the hot-{self.name}-host log in Cloud Logging "
                              "(on the host itself with logging.destination = local)")
                silent = sorted(targets - set(answered)) if targets is not None and reports else []
                who = f"{', '.join(silent)} did not" if silent else "The agent did not"
                error = (f"{who} answer {request_id} within {timeout} seconds; check {where}. The command "
                         "may still be waiting for the agent or running: check status before sending it again"
                         f"{read_failed(unread)}")
                if not silent:
                    raise CliError("; ".join(filter(None, (error, notes.get("warning")))))
                report = combined(answered, service)
                if elsewhere(report, service):
                    # Only hosts without the service answered, and none of it
                    # is about the silent hosts that may have it.
                    report = {"installation": self.name, "service": service, "request_id": request_id,
                              "answers": report["answers"]}
                return {**report, **notes, "state": "failed",
                        "error": "; ".join(filter(None, (report.get("error"), error)))}
            self.sleep(poll)

    def steps_it_allows(self, report):
        """A host's report, with the steps this installation's helper will send."""
        return gcp_recoveries(report) if self.cloud == "gcp" and self.scalable else report

    def release(self, version, request_id="", timeout=0):
        """Point the installation at a release and deploy it now. By default
        wait as long as the release workflow does: 30 minutes, or 60 for the
        whole of a scalable release."""
        timeout = timeout or (3600 if self.scalable else 1800)
        if self.scalable:
            return self.steps_it_allows(self.scalable_release(version, timeout))
        self.check_preset()
        self.point_at(version)
        report = self.run("deploy-now", request_id=request_id, timeout=timeout)
        if report.get("state") in ("deployed", "current", "resumed") and report.get("release") != version:
            report = {**report, "state": "failed",
                      "error": f"The agent reports release {report.get('release')}, not {version}"}
        return report

    def wait_for(self, release, since, deadline, done, poll=10, first_since=None):
        """Poll the reports about one release until `done(reports)` returns a
        report, or the deadline passes. Reads start at `since`, and until one
        succeeds at `first_since` if that is given. A failed read is nothing
        new, since the log keeps its reports for the next one; a scalable
        release makes about 1,000 an hour, and stopping on one would leave the
        roles it has not reached on the old release."""
        unread = None
        while True:
            try:
                result = done(self.reports(release, since if first_since is None else first_since, field="release"))
                first_since, unread = None, None
            except CliError as error:
                result, unread = None, note_unread(error, unread)
            if result:
                return result
            if self.clock() >= deadline:
                raise TimedOut(f"Release {release} did not finish in time", unread)
            self.sleep(poll)

    def failure_stands(self, report, role):
        """Whether a failure a role's host reported still stands. One from an
        instance the group has replaced says nothing about its replacement,
        which migrates or deploys as it boots, so the release waits for that.
        On GCP any group can recreate an instance, by autohealing it or
        repairing a dead VM, and it keeps its name and perhaps its creation
        time, so there the failure must be newer than the instance's last
        start. A restart in place moves that time too, and its host reports
        nothing new, so a wait that times out still says what it set aside
        (restarted_since). A read that fails raises, and the wait's next poll
        looks again: an instance the group has taken away drops out of its
        listing, and a read this identity may not make shows in the wait's
        warning and its timeout."""
        instance = report.get("instance")
        if self.cloud != "gcp":
            return instance in self.instances(role)
        zone = self.gcp_instances(role).get(instance)
        if not zone:
            return False
        started = (self.cli("gcloud", "compute", "instances", "describe", instance, "--zone", zone,
                            "--project", self.project, "--format", "json") or {}).get("lastStartTimestamp")
        return not started or not report.get("at") or parse_time(report["at"]) >= parse_time(started)

    def scalable_release(self, version, timeout):
        """Migrate once, then roll each role in order, waiting for every
        instance in its group. The scheduler is last. `timeout` covers the
        whole release, so a workflow job's limit can hold all of it."""
        needed = (("region", self.region),) + ((("project", self.project), ("zone", self.zone))
                                              if self.cloud == "gcp" else ())
        missing = [name for name, value in needed if not value]
        if missing:
            # Before any pointer moves, not after the migration.
            raise CliError(f"A scalable release needs the installation's {' and '.join(missing)}")
        since = self.clock() - CLOCK_SKEW
        deadline = self.clock() + timeout
        restarted = {}
        try:
            # Read before anything moves, so a pointer that cannot be read
            # leaves no role behind the others.
            written = {role: self.pointed_since(role, version, deadline) for role in ("migrate", *ROLE_ORDER)}
        except TimedOut as ended:
            return {"installation": self.name, "state": "failed", "release": version, "rolled": {}, "error": str(ended)}

        def migrated(reports):
            # A migration stands whichever host ran it: the database migrated.
            for r in reversed(reports):
                if r.get("state") == "migrated":
                    return r
                if r.get("state") == "migration-failed":
                    if self.failure_stands(r, "scheduler"):
                        return r
                    # A host the group replaced says nothing; its replacement
                    # migrates. Kept across polls, which read only recent reports.
                    if r.get("instance") in self.instances("scheduler"):
                        restarted.setdefault(r.get("instance"), r)
            return None
        try:
            if written["migrate"] is not None:
                # Already released: rely on what the migration host reported
                # since this pointer was written. A report from before then is
                # about an earlier release of this version, and the database
                # may have moved on: role hosts deploy on the migration host's
                # word, so an old `migrated` must not outlast this release's
                # refusal when the host that refused is replaced. The new host
                # reports as it boots.
                result = self.wait_for(version, since, deadline, migrated, first_since=written["migrate"] - CLOCK_SKEW)
            else:
                self.point_at(version, "migrate")
                result = self.wait_for(version, since, deadline, migrated)
        except TimedOut as ended:
            why = (NO_MIGRATION_HOST.format(version=version) if self.group_is_empty("scheduler")
                   else f"it may still be migrating: check its journal, then release again{restarted_since(restarted)}"
                        f"{'' if written['migrate'] is None else REPOINTED}")
            return {"installation": self.name, "state": "failed", "release": version, "rolled": {},
                    "error": f"The migration host did not report within {timeout} seconds; {why}"
                             f"{read_failed(ended.unread)}"}
        if result["state"] != "migrated":
            retry = (f"{GROUP_IDENTITY}, recreate the scheduler's instance, which migrates from the pointer"
                     if self.cloud == "gcp"
                     else "run command deploy-now (it reaches every host; only the scheduler's retries the migration)")
            return {**result, "state": "failed",
                    "error": f"{result.get('error', 'Migrations failed')}; after fixing the cause, {retry}, "
                             "then release again"}
        rolled = {}
        for role in ROLE_ORDER:
            if written[role] is None:
                # Even while the group has no instance in service, so the
                # instances it launches next boot this release.
                self.point_at(version, role)
            pending, restarted = ["its instances"], {}

            def deployed(reports, role=role, pending=pending, restarted=restarted):
                # Listed each time: an instance that left the group (scaled
                # in, replaced, or retired) never reports, and one it launched
                # must deploy too.
                instances = self.instances(role)
                # Each instance's latest deploy report: a retry supersedes a
                # failure, and a later status or logs report, or another
                # command's failure, changes nothing.
                latest = latest_by_instance(r for r in reports if role in (r.get("roles") or [])
                                            and r.get("state") in OUTCOMES and r.get("command") in (None, "deploy-now")
                                            and r.get("instance") in instances)
                if self.cloud == "gcp":
                    # A recreated instance keeps its name: its new host reports
                    # next. Worked out first, so a failed read keeps the last answer.
                    aside = {i: r for i, r in latest.items() if r["state"] == "failed" and not self.failure_stands(r, role)}
                    restarted.clear()
                    restarted.update(aside)
                    latest = {i: r for i, r in latest.items() if i not in restarted}
                failed = next((r for r in latest.values() if r["state"] == "failed"), None)
                if failed:
                    return failed
                if not instances and pending != [NO_ROLE_HOST]:
                    # Once a wait: after a restore, the cue to bring back the other groups.
                    print(f"warning: the migration has passed, and the release now waits for {NO_ROLE_HOST} in {role}'s "
                          "group: if it is at 0, restore its size", file=sys.stderr)
                pending[:] = sorted(set(instances) - set(latest)) or ([] if instances else [NO_ROLE_HOST])
                return None if pending else {"state": "deployed", "instances": instances}
            try:
                # A role pointed by an earlier run counts the reports since that
                # write: each instance must have deployed it, so a role that
                # failed then is reported again, not skipped. A deploy from before
                # is about an earlier release of this version, and a host that
                # has not answered since may be on the release in between.
                start = since if written[role] is None else written[role] - CLOCK_SKEW
                result = self.wait_for(version, start, deadline, deployed)
            except TimedOut as ended:
                step = NO_ROLE_HOST_STEP if pending == [NO_ROLE_HOST] else "" if written[role] is None else REPOINTED
                return {"installation": self.name, "state": "failed", "release": version, "role": role,
                        "rolled": rolled, "error": f"{role} did not finish within the release's {timeout} seconds; "
                                                   f"still waiting for {', '.join(pending)}{restarted_since(restarted)}"
                                                   f"{step}{read_failed(ended.unread)}"}
            if result["state"] == "failed":
                recovery = {"recovery": gcp_recovery(result.get("recovery"))} if self.cloud == "gcp" else {}
                return {**result, **recovery, "role": role, "rolled": rolled}
            rolled[role] = result["instances"]
        return {"installation": self.name, "state": "deployed", "release": version, "rolled": rolled}


def what_failed(lines):
    """What a failed CLI said: its last line, or gcloud's last error. gcloud may
    follow that with the response's details, as `- '@type'` lines, which only
    hide it. On a compute command it is a header ending in a colon, and the
    reason is on the ` - ` lines below. On a sign-in that needs a browser it is
    `Please run:`, and the reason is the command after the blank line below."""
    errors = [at for at, line in enumerate(lines) if line.startswith("ERROR:")]
    if not errors:
        return lines[-1] if lines else ""
    said = [lines[errors[-1]].rstrip()]
    if said[0].endswith(":"):
        below = lines[errors[-1] + 1:]
        for line in below:
            if not line.startswith(" - "):
                break
            said.append(line.strip())
        said += [line.strip() for line in below[:2] if line.strip().startswith("$ ")]
    return " ".join(said)


def note_unread(error, last):
    """Keep the error of a failed read during a wait, and say so as it happens,
    once per error: a read that will never work, such as one this identity may
    not make, otherwise looks like a silent agent until the wait ends."""
    if str(error) != str(last):
        print(f"warning: a read failed, still waiting: {error}", file=sys.stderr)
    return error


def read_failed(error):
    """What a timed-out wait adds when its last read failed."""
    return f". The last read failed, so an answer may have gone unseen: {error}" if error else ""


def restarted_since(failures):
    """What a timed-out wait adds about the failures it set aside, by instance,
    because their hosts have started since. A recreated host deploys as it
    boots, but one restarted in place keeps the agent's state, and the agent
    neither retries nor reports a failed release again without deploy-now, which
    a scalable GCP installation cannot send. So this may be the only answer."""
    if not failures:
        return ""
    last = "; ".join(f"{i} last reported {r.get('state')} at {r.get('at')}: {r.get('error') or 'no error given'}, "
                     "and has restarted since" for i, r in sorted(failures.items()))
    return (f". {last}. A host restarted in place does not try a failed release again, so check its journal for a "
            f"new attempt. {ONE_HOST_INSTEAD['deploy-now']}")


def gcp_recovery(said):
    """A host's recovery text for a role that failed on a scalable GCP installation.
    What it says happened stays: that the earlier release keeps running, or did not
    start again. The step that names deploy-now, as `run deploy-now` or however
    another agent release puts it, gives way to the retry that works, since the
    helper does not send it there, and whatever follows that step goes too. The other
    way on that the step offers stays: another release needs no identity that may
    change instance groups."""
    sentences = re.split(r"(?<=\.) ", said or "")
    at = next((i for i, sentence in enumerate(sentences) if "deploy-now" in sentence), len(sentences))
    other = re.search(r"deploy-now[^,]*, or (.+)", "".join(sentences[at:at + 1]))
    step = f"Fix the cause. {ONE_HOST_INSTEAD['deploy-now']}"
    return " ".join(filter(None, (*sentences[:at], f"{step}. Or {other.group(1)}" if other else step)))


def gcp_recoveries(value):
    """A scalable GCP host's report with each recovery text that names deploy-now
    as gcp_recovery tells it. Every report carries the host's state, so its failed
    record says it too, whatever the report is about, and so do the past deploys in
    diagnostics."""
    if isinstance(value, list):
        return [gcp_recoveries(item) for item in value]
    if not isinstance(value, dict):
        return value
    return {key: gcp_recovery(item) if key == "recovery" and isinstance(item, str) and "deploy-now" in item
            else gcp_recoveries(item) for key, item in value.items()}


def latest_by_instance(reports):
    """Each instance's newest report, in the order they arrived."""
    latest = {}
    for report in reports:
        latest.pop(report.get("instance"), None)
        latest[report.get("instance")] = report
    return latest


def elsewhere(report, service):
    """Whether a host that does not run the service turned it down. Whatever it
    says, an unknown service or nothing deployed yet, is about that host. The
    ingress runs beside the app and the API, and a service no role runs is on
    no host, so turning that down is the answer."""
    homes = ("app", "api") if service == "ingress" else (service,) if service in ROLE_ORDER else ()
    roles = report.get("roles") or []
    return report.get("state") == "rejected" and bool(homes and roles) and not set(homes) & set(roles)


def combined(answered, service):
    """One report for a command every host answered: the first failure, or
    else the newest answer, with each host's state. A host without the
    service rejects it, which is no failure while another host answered."""
    reports = list(answered.values())
    relevant = [r for r in reports if not elsewhere(r, service)] or reports
    chosen = next((r for r in relevant if r.get("state") in FAILED), relevant[-1])
    return {**chosen, "answers": {str(i): r.get("state") for i, r in sorted(answered.items(), key=lambda a: str(a[0]))}}


def strings(value):
    """Every string inside a parsed JSON value."""
    if isinstance(value, dict):
        for item in value.values():
            yield from strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)
    elif isinstance(value, str):
        yield value


def parse_time(value):
    """Seconds since the epoch, from an RFC 3339 time. Google's may carry
    nanoseconds, which Python before 3.11 refuses."""
    return datetime.datetime.fromisoformat(re.sub(r"(\.\d{6})\d+", r"\1", value).replace("Z", "+00:00")).timestamp()


def markdown(report):
    """A job summary for a report. Logs go in a code block; everything else
    is shown as JSON."""
    lines = [f"### {report.get('installation', 'Hot')}: {report.get('state', '?')}", ""]
    if report.get("error"):
        lines += [f"**Error:** {report['error']}", ""]
    if report.get("warning"):
        lines += [f"**Warning:** {report['warning']}", ""]
    logs = report.get("logs")
    body = {key: value for key, value in report.items() if key != "logs"}
    lines += ["```json", json.dumps(body, indent=2), "```"]
    if logs is not None:
        fence = "````" if any("```" in line for line in logs) else "```"
        lines += ["", f"Last {len(logs)} lines of `{report.get('service')}`:", "", fence, *logs, fence]
    return "\n".join(lines) + "\n"


def main(argv=None, cli=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tfvars", help="the installation's tfvars JSON")
    parser.add_argument("--request-id", default="")
    parser.add_argument("--timeout", type=int, default=0,
                        help="seconds to wait for the agent; for a scalable release, for the whole release "
                             "(a release defaults to 1800, or 3600 on scalable)")
    parser.add_argument("--summary", help="also append a Markdown report to this file")
    sub = parser.add_subparsers(dest="action", required=True)
    command = sub.add_parser("command")
    command.add_argument("operation", choices=OPERATIONS)
    command.add_argument("--service", default="")
    logs = sub.add_parser("logs")
    logs.add_argument("service")
    release = sub.add_parser("release")
    release.add_argument("version")
    maintenance = sub.add_parser("maintenance")
    maintenance.add_argument("when", choices=("start", "end"))
    maintenance.add_argument("--minutes", type=int, default=60, help="GCP: how long the snooze lasts")
    args = parser.parse_args(argv)
    with open(args.tfvars) as handle:
        installation = Installation(json.load(handle), cli or Cli())
    try:
        if args.action == "release":
            report = installation.release(args.version, args.request_id, args.timeout)
        elif args.action == "maintenance":
            report = installation.maintenance(args.when, args.minutes)
        elif args.action == "logs":
            report = installation.run("logs", args.service, args.request_id, args.timeout or 300)
        else:
            report = installation.run(args.operation, args.service, args.request_id, args.timeout or 600)
    except CliError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2))
    if report.get("warning"):
        print(f"warning: {report['warning']}", file=sys.stderr)
    if args.summary:
        with open(args.summary, "a") as handle:
            handle.write(markdown(report))
    return 1 if report.get("state") in FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
