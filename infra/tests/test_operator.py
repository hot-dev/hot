"""The operator helper against fake aws and gcloud CLIs."""
import base64
import copy
import datetime
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("selfhost", ROOT / "operator/selfhost.py")
selfhost = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selfhost)

os.environ["HOT_AGENT_RUNTIME"] = str(ROOT / "runtime")
agent_spec = importlib.util.spec_from_file_location("hot_agent", ROOT / "agent/hot_agent.py")
hot_agent = importlib.util.module_from_spec(agent_spec)
agent_spec.loader.exec_module(hot_agent)

AWS = {"installation": {"name": "hot-example", "cloud": "aws", "region": "us-east-1"}}
GCP = {"installation": {"name": "hot-example", "cloud": "gcp", "region": "us-central1", "project": "example-project"}}


class FakeCli:
    """Answers with the agent's reports after `delay` polls."""

    def __init__(self, cloud, reports, delay=1):
        self.cloud = cloud
        self.reports = reports
        self.delay = delay
        self.calls = []

    def __call__(self, *args, stdin=None):
        self.calls.append((args, stdin))
        if "filter-log-events" in args:
            self.delay -= 1
            return {"events": [] if self.delay >= 0 else [{"message": json.dumps(r)} for r in self.reports]}
        if args[:3] == ("gcloud", "logging", "read"):
            self.delay -= 1
            entries = [] if self.delay >= 0 else [{"jsonPayload": r} for r in self.reports]
            return entries[::-1] if args[args.index("--order") + 1] == "desc" else entries
        return {}

    def find(self, word):
        return [args for args, _ in self.calls if word in args]


class Clock:
    def __init__(self):
        self.now = 1_800_000_000.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


def installation(document, cli):
    clock = getattr(cli, "clock", None) or Clock()
    return selfhost.Installation(document, cli, clock=clock, sleep=clock.sleep)


def rfc3339(seconds):
    return datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc).isoformat()


class Commands(unittest.TestCase):
    def test_aws_sends_through_the_command_document_and_reads_the_log(self):
        cli = FakeCli("aws", [{"state": "diagnostics", "request_id": "r1", "diagnostics": {}}])
        report = installation(AWS, cli).run("diagnostics", request_id="r1")
        self.assertEqual(report["state"], "diagnostics")
        send = cli.find("send-command")[0]
        self.assertIn("hot-hot-example-command", send)
        self.assertIn("operation=diagnostics,service=none,requestId=r1,expires=1800000600", send,
                      "the agent skips it once the helper has stopped waiting")
        self.assertIn("Key=tag:HotInstallation,Values=hot-example", send)
        self.assertEqual(send[send.index("--timeout-seconds") + 1], "600", "dropped if not started while the helper waits")
        self.assertEqual(send[send.index("--max-errors") + 1], "100%", "a rejection on one host does not stop the rest")
        read = cli.find("filter-log-events")[0]
        self.assertEqual(read[read.index("--log-group-name") + 1], "/hot/hot-example")
        self.assertEqual(read[read.index("--filter-pattern") + 1], '{ $.request_id = "r1" }')
        self.assertEqual(read[read.index("--start-time") + 1], str(int((1_800_000_000 - 120) * 1000)))

    def test_gcp_publishes_to_the_topic_and_reads_the_status_log(self):
        cli = FakeCli("gcp", [{"state": "logs", "service": "api", "request_id": "r2", "logs": ["a"]}])
        report = installation(GCP, cli).run("logs", "api", request_id="r2", timeout=300)
        self.assertEqual(report["logs"], ["a"])
        publish = cli.find("publish")[0]
        self.assertEqual(json.loads(publish[publish.index("--message") + 1]),
                         {"operation": "logs", "service": "api", "request_id": "r2", "expires": 1_800_000_300})
        read = cli.find("read")[0]
        self.assertIn('logName="projects/example-project/logs/hot-hot-example-status"', read[3])
        self.assertIn('jsonPayload.request_id="r2"', read[3])
        self.assertEqual(read[read.index("--order") + 1:read.index("--order") + 4], ("desc", "--limit", "1000"))

    def test_gcp_reports_come_back_oldest_first(self):
        cli = FakeCli("gcp", [{"state": "failed", "request_id": "r3"}, {"state": "deployed", "request_id": "r3"}], delay=0)
        self.assertEqual(installation(GCP, cli).run("deploy-now", request_id="r3")["state"], "deployed")

    def test_a_silent_agent_times_out_with_a_next_step(self):
        cli = FakeCli("aws", [], delay=1000)
        with self.assertRaisesRegex(selfhost.CliError, "did not answer .* within 60 seconds; check the command.*"
                                                       "check status before sending it again"):
            installation(AWS, cli).run("status", timeout=60)

    def test_a_silent_gcp_agent_points_at_the_journal_it_ships(self):
        cli = FakeCli("gcp", [], delay=1000)
        with self.assertRaisesRegex(selfhost.CliError, "check the agent's journal in the hot-hot-example-host log in "
                                                       "Cloud Logging") as caught:
            installation(GCP, cli).run("status", timeout=60)
        self.assertNotIn("serial console", str(caught.exception), "the stacks disable it")

    def test_request_ids_are_generated(self):
        cli = FakeCli("gcp", [{"state": "status"}])
        installation(GCP, cli).run("status")
        message = json.loads(cli.find("publish")[0][-3])
        self.assertRegex(message["request_id"], r"^cli-[0-9a-f]{12}$")

    def test_the_helper_fails_on_every_state_the_agent_fails_on(self):
        self.assertEqual(selfhost.FAILED, hot_agent.FAILED_STATES)


def topic(name):
    return f"arn:aws:sns:us-east-1:123456789012:hot-{name}-alerts"


def condition(text):
    return [{"displayName": "c", "conditionThreshold": {"filter": text}}]


class AlertCli(FakeCli):
    """Lists the installation's alarms or alert policies beside those of
    hot-example-eu, whose name starts with this one's, plus one snooze, keeps
    track of which alarms' actions are off, and answers the rest as FakeCli."""

    job_wait = "OK"
    unreadable = False

    def __init__(self, cloud, reports, delay=1):
        super().__init__(cloud, reports, delay)
        self.actions_off = set()

    def __call__(self, *args, stdin=None):
        if "disable-alarm-actions" in args:
            self.actions_off.update(args[args.index("--alarm-names") + 1:])
        elif "enable-alarm-actions" in args:
            self.actions_off.difference_update(args[args.index("--alarm-names") + 1:])
        answer = self.alerts_answer(args)
        if answer is None:
            return super().__call__(*args, stdin=stdin)
        self.calls.append((args, stdin))
        return answer

    def alerts_answer(self, args):
        if "describe-alarms" in args:
            if self.unreadable:
                raise selfhost.CliError("AccessDenied: not authorized to perform cloudwatch:DescribeAlarms")
            alarms = [
                {"AlarmName": "hot-hot-example-host-down", "StateValue": "OK", "AlarmActions": [topic("hot-example")]},
                {"AlarmName": "hot-hot-example-job-wait", "StateValue": self.job_wait,
                 "AlarmActions": [topic("hot-example")], "OKActions": [topic("hot-example")]},
                {"AlarmName": "hot-hot-example-eu-host-down", "StateValue": "ALARM",
                 "AlarmActions": [topic("hot-example-eu")]}]
            return {"MetricAlarms": [{**a, "ActionsEnabled": a["AlarmName"] not in self.actions_off} for a in alarms]}
        if args[:4] == ("gcloud", "monitoring", "policies", "list"):
            metric = 'metric.type = "custom.googleapis.com/hot/{}/agent_heartbeat"'
            database = 'resource.labels.database_id = "example-project:hot-{}" AND metric.type = "cloudsql.googleapis.com/x"'
            return [{"name": "projects/example-project/alertPolicies/1", "displayName": "hot-hot-example-host-down",
                     "conditions": condition(metric.format("hot-example"))},
                    {"name": "projects/example-project/alertPolicies/2", "displayName": "hot-hot-example-database-connections",
                     "conditions": condition(database.format("hot-example"))},
                    {"name": "projects/example-project/alertPolicies/3", "displayName": "hot-other-host-down",
                     "conditions": condition(metric.format("other"))},
                    {"name": "projects/example-project/alertPolicies/4", "displayName": "hot-hot-example-eu-host-down",
                     "conditions": condition(metric.format("hot-example-eu"))},
                    {"name": "projects/example-project/alertPolicies/5",
                     "displayName": "hot-hot-example-eu-database-connections",
                     "conditions": condition(database.format("hot-example-eu"))}]
        if args[:4] == ("gcloud", "monitoring", "snoozes", "list"):
            return [{"name": "projects/example-project/snoozes/7", "displayName": "hot-hot-example maintenance",
                     "interval": {"endTime": "2027-01-15T09:00:00Z"}},
                    {"name": "projects/example-project/snoozes/6", "displayName": "hot-hot-example maintenance",
                     "interval": {"endTime": "2020-01-01T00:00:00Z"}}]
        return None


class Maintenance(unittest.TestCase):
    def test_aws_disables_the_alarm_actions_until_the_end(self):
        cli = AlertCli("aws", [])
        report = installation(AWS, cli).maintenance("start")
        self.assertEqual(report, {"installation": "hot-example", "state": "maintenance", "alerts": 2,
                                  "until": "until maintenance end"})
        disabled = cli.find("disable-alarm-actions")[0]
        self.assertEqual(disabled[disabled.index("--alarm-names") + 1:], ("hot-hot-example-host-down", "hot-hot-example-job-wait"),
                         "not hot-example-eu's alarms, though their names start the same")
        report = installation(AWS, cli).maintenance("end")
        enabled = cli.find("enable-alarm-actions")[0]
        self.assertEqual(enabled[enabled.index("--alarm-names") + 1:], ("hot-hot-example-host-down", "hot-hot-example-job-wait"))
        self.assertNotIn("alarming", report)

    def test_aws_maintenance_end_names_alarms_that_went_off(self):
        cli = AlertCli("aws", [])
        cli.job_wait = "ALARM"
        report = installation(AWS, cli).maintenance("end")
        self.assertEqual(report["state"], "monitoring")
        self.assertEqual(report["alarming"], ["hot-hot-example-job-wait"])
        self.assertIn("notifies only once an alarm recovers", report["warning"])
        self.assertIn("**Warning:** hot-hot-example-job-wait", selfhost.markdown(report))

    def test_gcp_snoozes_the_installation_policies_for_a_while(self):
        cli = AlertCli("gcp", [])
        report = installation(GCP, cli).maintenance("start", minutes=90)
        created = cli.find("create")[0]
        self.assertEqual(created[created.index("--criteria-policies") + 1],
                         "projects/example-project/alertPolicies/1,projects/example-project/alertPolicies/2",
                         "its metrics and database, not hot-example-eu's")
        self.assertEqual(created[created.index("--end-time") + 1], report["until"])
        self.assertEqual(report["until"], "2027-01-15T09:30:00Z")
        installation(GCP, cli).maintenance("end")
        cancelled = [args[4] for args in cli.find("cancel")]
        self.assertEqual(cancelled, ["projects/example-project/snoozes/7"], "only the snooze still running")

    def test_nothing_to_silence_is_an_error(self):
        with self.assertRaisesRegex(selfhost.CliError, "No alerts found"):
            installation(AWS, FakeCli("aws", [])).maintenance("start")


class SilencedAlarms(unittest.TestCase):
    """The stacks leave alarm actions to `maintenance`, so an apply keeps them
    off and the plan's drift check does not see them. Status and diagnostics
    are where a forgotten `maintenance end` shows."""

    NAMES = ["hot-hot-example-host-down", "hot-hot-example-job-wait"]

    def test_status_and_diagnostics_warn_until_maintenance_end(self):
        for operation in selfhost.REPORTS_SILENCE:
            with self.subTest(operation):
                cli = AlertCli("aws", [{"state": operation}], delay=0)
                self.assertNotIn("warning", installation(AWS, cli).run(operation))
                installation(AWS, cli).maintenance("start")
                report = installation(AWS, cli).run(operation)
                self.assertEqual(report["state"], operation)
                self.assertEqual(report["silenced"], self.NAMES)
                self.assertIn("Alarm actions are off on 2 of 2 alarms", report["warning"])
                self.assertIn("an apply does not turn them back on", report["warning"])
                self.assertIn("installation.tfvars.json maintenance end", report["warning"])
                self.assertIn("**Warning:** Alarm actions are off", selfhost.markdown(report))
                installation(AWS, cli).maintenance("end")
                after = installation(AWS, cli).run(operation)
                self.assertNotIn("warning", after)
                self.assertNotIn("silenced", after)

    def test_one_alarm_left_off_is_enough(self):
        cli = AlertCli("aws", [{"state": "status"}], delay=0)
        cli.actions_off = {"hot-hot-example-job-wait"}
        report = installation(AWS, cli).run("status")
        self.assertEqual(report["silenced"], ["hot-hot-example-job-wait"])
        self.assertIn("off on 1 of 2 alarms", report["warning"])

    def test_another_installations_silenced_alarms_are_not_this_ones(self):
        cli = AlertCli("aws", [{"state": "status"}], delay=0)
        cli.actions_off = {"hot-hot-example-eu-host-down"}
        self.assertNotIn("warning", installation(AWS, cli).run("status"))

    def test_only_reports_on_the_installation_look(self):
        for operation in sorted(set(selfhost.OPERATIONS) - set(selfhost.REPORTS_SILENCE)):
            cli = AlertCli("aws", [{"state": "done"}], delay=0)
            cli.actions_off = set(self.NAMES)
            self.assertNotIn("warning", installation(AWS, cli).run(operation, "app"), operation)
            self.assertEqual(cli.find("describe-alarms"), [], operation)

    def test_a_gcp_snooze_ends_by_itself_so_nothing_looks(self):
        cli = AlertCli("gcp", [{"state": "status"}], delay=0)
        self.assertNotIn("warning", installation(GCP, cli).run("status"))
        self.assertEqual(cli.find("policies"), [])

    def test_alarms_that_cannot_be_read_warn_and_fail_nothing(self):
        cli = AlertCli("aws", [{"state": "status"}], delay=0)
        cli.unreadable = True
        report = installation(AWS, cli).run("status")
        self.assertEqual(report["state"], "status")
        self.assertRegex(report["warning"], "^Could not check whether the alarms' actions are on: AccessDenied")
        self.assertNotIn("silenced", report)

    def test_a_silent_agent_does_not_hide_it(self):
        # A host that does not answer is when it matters that nobody was paged.
        cli = AlertCli("aws", [], delay=1000)
        cli.actions_off = set(self.NAMES)
        with self.assertRaisesRegex(selfhost.CliError, "^The agent did not answer .*Alarm actions are off on 2 of 2 alarms"):
            installation(AWS, cli).run("status", timeout=60)

    def test_every_host_answering_keeps_it(self):
        off = [{"AlarmName": name, "ActionsEnabled": False} for name in self.NAMES]
        with mock.patch.object(selfhost.Installation, "alerts", return_value=off):
            report = installation(SCALABLE, ScalableCli()).run("diagnostics", timeout=60)
            self.assertEqual(report["state"], "diagnostics")
            self.assertEqual(len(report["answers"]), 7)
            self.assertEqual(report["silenced"], self.NAMES)
            cli = ScalableCli()
            cli.silent = {"i-s1"}
            report = installation(SCALABLE, cli).run("status", timeout=60)
            self.assertEqual(report["state"], "failed", "so the helper exits 1")
            self.assertRegex(report["error"], "^i-s1 did not answer")
            self.assertIn("Alarm actions are off", report["warning"])


class Releases(unittest.TestCase):
    def test_the_pointer_is_written_before_deploying(self):
        cli = FakeCli("gcp", [{"state": "deployed", "release": "3.0.2"}])
        report = installation(GCP, cli).release("3.0.2", request_id="gha-1-1")
        self.assertEqual(report["state"], "deployed")
        pointer = next(i for i, (args, _) in enumerate(cli.calls) if "versions" in args and "add" in args)
        publish = next(i for i, (args, _) in enumerate(cli.calls) if "publish" in args)
        self.assertLess(pointer, publish)
        self.assertEqual(json.loads(cli.calls[pointer][1]), {"release": "3.0.2"})

    def test_a_release_deployed_by_the_agent_loop_counts(self):
        cli = FakeCli("aws", [{"state": "current", "release": "3.0.2"}])
        self.assertEqual(installation(AWS, cli).release("3.0.2")["state"], "current")
        self.assertIn("/hot/hot-example/release", cli.find("put-parameter")[0])

    def test_another_release_is_a_failure(self):
        cli = FakeCli("aws", [{"state": "current", "release": "3.0.1"}])
        report = installation(AWS, cli).release("3.0.2")
        self.assertEqual(report["state"], "failed")
        self.assertIn("reports release 3.0.1, not 3.0.2", report["error"])

    def test_resuming_another_release_is_a_failure(self):
        cli = FakeCli("aws", [{"state": "resumed", "release": "3.0.1"}])
        self.assertEqual(installation(AWS, cli).release("3.0.2")["state"], "failed")

    def test_a_release_waits_half_an_hour_by_default(self):
        with self.assertRaisesRegex(selfhost.CliError, "within 1800 seconds"):
            installation(AWS, FakeCli("aws", [], delay=100_000)).release("3.0.2")


class ScalableCli:
    """A scalable installation whose agents answer once their pointer
    changes: the migration host first, then each role's instances. As the agent
    does, they say nothing new about a release they have already acted on, so
    writing one again logs nothing. Reports reach the log `delay` seconds later,
    and reads filter them by field and time, order, and limit as each cloud's
    log does."""

    GROUPS = {"task-worker": ["i-t1"], "worker": ["i-w1", "i-w2"], "api": ["i-a1"], "app": ["i-p1", "i-p2"],
              "scheduler": ["i-s1"]}

    def __init__(self, fail_role=None, migration="migrated", cloud="aws", groups=None, delay=0):
        self.cloud = cloud
        self.clock = Clock()
        self.groups = copy.deepcopy(groups or self.GROUPS)
        self.pointers = {}
        self.written = {}
        self.acted = {}
        self.written_unreadable = ()
        self.events = []
        self.calls = []
        self.fail_role = fail_role
        self.migration = migration
        self.delay = delay
        self.silent = set()
        self.answer_delay = {}
        self.later = []
        self.on_write = None
        self.started = {}
        self.zones = {}
        self.failure = {}

    def zone_of(self, instance):
        return self.zones.get(instance, "us-central1-a")

    def log(self, report, delay=0):
        self.events.append((self.clock.now + delay, {"at": rfc3339(self.clock.now + delay), **report}))

    def at(self, delay, action):
        self.later.append((self.clock.now + delay, action))

    def write(self, role, value):
        self.pointers[role] = value
        self.written[role] = self.clock.now
        if self.on_write:
            self.on_write(role, value)
        if self.acted.get(role) == value:
            return
        self.acted[role] = value
        if role == "migrate":
            instance = (self.groups["scheduler"] or ["i-s1"])[0]
            if instance not in self.silent:
                self.log({"state": self.migration, "release": value, "instance": instance, "roles": ["scheduler"],
                          **(self.failure if self.migration == "migration-failed" else {})}, self.delay)
            return
        for instance in self.groups[role]:
            if instance not in self.silent:
                state = "failed" if role == self.fail_role else "deployed"
                self.log({"state": state, "release": value, "instance": instance, "roles": [role],
                          **(self.failure if state == "failed" else {})}, self.delay)

    def answer(self, instance, role, operation, service, request_id):
        report = {"instance": instance, "roles": [role], "request_id": request_id, "release": "3.0.1"}
        service = "" if service == "none" else service  # as the agent reads the document's default
        if operation in ("restart-service", "logs"):
            if service != role and not (service == "ingress" and role in ("app", "api")):
                return {**report, "state": "rejected", "error": f"Unknown service {service!r}"}
            if operation == "logs":
                return {**report, "state": "logs", "service": service, "logs": [f"{instance} line"]}
            return {**report, "state": "restarted", "service": service}
        if operation == "deploy-now":
            return {**report, "state": "migrated" if role == "scheduler" else "current"}
        return {**report, "state": operation}

    def visible(self, field, value, start):
        return [r for at, r in sorted(self.events, key=lambda e: e[0])
                if start <= at <= self.clock.now and r.get(field) == value]

    def __call__(self, *args, stdin=None):
        self.calls.append(args)
        for due in [e for e in self.later if e[0] <= self.clock.now]:
            self.later.remove(due)
            due[1]()
        if "put-parameter" in args:
            self.write(args[args.index("--name") + 1].rsplit("/", 1)[-1], args[args.index("--value") + 1])
            return {}
        if args[:4] == ("gcloud", "secrets", "versions", "add"):
            self.write(args[4].split("-release-", 1)[1], json.loads(stdin)["release"])
            return {}
        if "get-parameter" in args or args[:4] == ("gcloud", "secrets", "versions", "access"):
            name = args[args.index("--name") + 1] if "--name" in args else args[args.index("--secret") + 1]
            role = re.split(r"/release/|-release-", name)[1]
            if role not in self.pointers:
                raise selfhost.CliError("An error occurred (ParameterNotFound) when calling the GetParameter operation: "
                                        if self.cloud == "aws" else "ERROR: (gcloud.secrets.versions.access) NOT_FOUND: "
                                        f"Secret [projects/p/secrets/{name}] not found or has no versions.")
            if self.cloud == "aws":
                return {"Parameter": {"Value": self.pointers[role], "LastModifiedDate": rfc3339(self.written.get(role, 0))}}
            return {"payload": {"data": base64.b64encode(json.dumps({"release": self.pointers[role]}).encode()).decode()}}
        if args[:4] == ("gcloud", "secrets", "versions", "describe"):
            role = args[args.index("--secret") + 1].split("-release-", 1)[1]
            if args[4] != "latest" or role in self.written_unreadable:
                raise selfhost.CliError("PERMISSION_DENIED: Permission 'secretmanager.versions.get' denied")
            return {"name": args[4], "createTime": rfc3339(self.written.get(role, 0))}
        if "describe-auto-scaling-groups" in args:
            names = args[args.index("--auto-scaling-group-names") + 1:]
            return {"AutoScalingGroups": [{"Instances": [{"InstanceId": i, "LifecycleState": "InService"}
                                                         for i in self.groups[name.split("hot-example-", 1)[1]]]}
                                          for name in names]}
        if args[:4] == ("gcloud", "compute", "instances", "describe"):
            if args[args.index("--zone") + 1] != self.zone_of(args[4]):
                raise selfhost.CliError(f"The resource '{args[4]}' was not found")
            return {"name": args[4], "lastStartTimestamp": rfc3339(self.started.get(args[4], 0))}
        if "list-instances" in args:
            role = args[args.index("list-instances") + 1].split("hot-example-", 1)[1]
            return [{"instance": f"https://www.googleapis.com/compute/v1/projects/p/zones/{self.zone_of(i)}/instances/{i}",
                     "instanceStatus": "RUNNING"} for i in self.groups[role]]
        if "send-command" in args:
            parameters = dict(p.split("=", 1) for p in args[args.index("--parameters") + 1].split(","))
            for role, instances in self.groups.items():
                for instance in instances:
                    if instance not in self.silent:
                        self.log(self.answer(instance, role, parameters["operation"], parameters["service"],
                                             parameters["requestId"]), self.answer_delay.get(instance, 0))
            return {"Command": {"CommandId": "c-1"}}
        if "filter-log-events" in args:
            field, value = re.fullmatch(r'\{ \$\.(\w+) = "([^"]*)" \}', args[args.index("--filter-pattern") + 1]).groups()
            start = int(args[args.index("--start-time") + 1]) / 1000
            return {"events": [{"message": json.dumps(r)} for r in self.visible(field, value, start)]}
        if args[:3] == ("gcloud", "logging", "read"):
            field, value = re.search(r'jsonPayload\.(\w+)="([^"]*)"', args[3]).groups()
            found = self.visible(field, value, selfhost.parse_time(re.search(r'timestamp>="([^"]+)"', args[3]).group(1)))
            if args[args.index("--order") + 1] == "desc":
                found = found[::-1]
            return [{"jsonPayload": r} for r in found[:int(args[args.index("--limit") + 1])]]
        return {}

    def order(self):
        return [a[a.index("--name") + 1].rsplit("/", 1)[-1] if "--name" in a else a[4].split("-release-", 1)[1]
                for a in self.calls if "put-parameter" in a or a[:4] == ("gcloud", "secrets", "versions", "add")]

    def listed(self, flag):
        return [a[a.index(flag) + 1] for a in self.calls if "list-instances" in a and flag in a]


SCALABLE = {"installation": dict(AWS["installation"], preset="scalable")}
GCP_SCALABLE = {"installation": {"name": "hot-example", "cloud": "gcp", "project": "example-project",
                                 "zone": "us-central1-a", "preset": "scalable"}}


def command_document(stack):
    """The allowed values in a stack's command document."""
    main = (ROOT / f"opentofu/stacks/aws/{stack}/main.tf").read_text()
    return {name: re.findall(r'"([^"]+)"', re.search(rf"(?m)^ +{name} += \{{.*allowedValues = \[([^\]]*)\]", main).group(1))
            for name in ("operation", "service")}


class DocumentCli(ScalableCli):
    """A scalable AWS installation whose Systems Manager holds a command to the
    allowed values in the stack's command document, as the real one does."""

    ALLOWED = command_document("scalable")

    def __call__(self, *args, stdin=None):
        if "send-command" in args:
            parameters = dict(p.split("=", 1) for p in args[args.index("--parameters") + 1].split(","))
            if any(parameters[name] not in allowed for name, allowed in self.ALLOWED.items()):
                raise selfhost.CliError("An error occurred (InvalidParameters) when calling the SendCommand operation")
        return super().__call__(*args, stdin=stdin)


class ScalableReleases(unittest.TestCase):
    def test_migrations_run_first_and_roles_roll_in_order(self):
        cli = ScalableCli()
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api", "app", "scheduler"])
        self.assertEqual(report["rolled"]["app"], ["i-p1", "i-p2"])

    def test_a_failed_migration_rolls_nothing(self):
        cli = ScalableCli(migration="migration-failed")
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "failed")
        self.assertIn("run command deploy-now (it reaches every host; only the scheduler's retries", report["error"])
        self.assertNotIn("on the scheduler host", report["error"], "no command targets one host")
        self.assertEqual(cli.order(), ["migrate"])

    def test_a_failed_gcp_migration_names_the_retry_the_helper_allows(self):
        cli = ScalableCli(cloud="gcp", migration="migration-failed")
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "failed")
        self.assertIn("recreate the scheduler's instance", report["error"])
        self.assertNotIn("deploy-now", report["error"], "the helper refuses it there")
        self.assertIn("with an identity that may change instance groups (the release account may not)", report["error"])

    def test_a_failed_role_stops_the_roll(self):
        cli = ScalableCli(fail_role="api")
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"]), ("failed", "api"))
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api"])
        self.assertEqual(set(report["rolled"]), {"task-worker", "worker"})

    def test_a_failed_aws_role_keeps_the_hosts_recovery(self):
        # deploy-now reaches every host there.
        said = "Nothing was installed. Fix the cause, then run deploy-now."
        cli = ScalableCli(fail_role="api")
        cli.failure = {"error": "boom", "recovery": said}
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"], report["recovery"]), ("failed", "api", said))

    def test_a_failed_gcp_role_names_the_retry_the_helper_allows(self):
        # What the agent itself says after a failed deploy, on a scalable host.
        agent = mock.MagicMock(config={"preset": "scalable"})
        agent.restore_checked = hot_agent.Agent.restore_checked
        agent.stops.return_value = {"starting"}
        agent.start_services.side_effect = hot_agent.AgentError("the old one is stuck")
        agent.explain.return_value = "the old one is stuck"
        restart_failed = hot_agent.Agent.return_to_previous(agent, {"release": "3.0.2", "previous": "3.0.1", "steps": []}, {})
        agent = mock.MagicMock(config={"preset": "scalable"})
        # The real check, not a truthy mock: none of these states holds a
        # roll-back that left part of Hot down. Boot's start is over.
        agent.restore_checked = hot_agent.Agent.restore_checked
        agent.boot_settled.return_value = True
        agent.stops.return_value = set()
        retry = f"Fix the cause. {selfhost.ONE_HOST_INSTEAD['deploy-now']}"
        another = "Or point the installation at another release."
        recoveries = (
            (hot_agent.Agent.return_to_previous(agent, {"release": "3.0.2", "steps": []}, {}),
             f"Nothing was installed. {retry}"),
            (hot_agent.Agent.return_to_previous(agent, {"release": "3.0.2", "previous": "3.0.1", "steps": []}, {}),
             f"The database was not changed and 3.0.1 keeps running. {retry}. {another}"),
            (hot_agent.Agent.return_to_previous(agent, {"release": "3.0.2", "previous": "3.0.1", "steps": []},
                                                {"stopped": True}),
             "The database was not changed, and Hot stays stopped, as it was before this deploy. "
             f"{retry}. Or point the installation back at 3.0.1 to start it."),
            (hot_agent.Agent.return_to_previous(agent, {"release": "3.0.2", "previous": "3.0.1", "steps": []},
                                                {"resume_failed": {"error": "stuck"}}),
             "The database was not changed, but 3.0.1 has not started again: stuck. That start is tried again every 5 "
             f"minutes. {retry}. {another}"),
            (restart_failed,
             "The database was not changed, but 3.0.1 did not start again: the old one is stuck. That start is tried "
             f"again every 5 minutes. {retry}"),
        )
        for said, told in recoveries:
            with self.subTest(said):
                self.assertIn("run deploy-now", said)
                cli = ScalableCli(cloud="gcp", fail_role="api")
                # As the agent reports it: every report carries the host's state.
                cli.failure = {"error": "boom", "recovery": said, "failed": {"release": "3.0.2", "recovery": said}}
                report = installation(GCP_SCALABLE, cli).release("3.0.2")
                self.assertEqual((report["state"], report["role"]), ("failed", "api"))
                self.assertEqual(report["recovery"], told)
                self.assertEqual(report["failed"]["recovery"], told)
                self.assertNotIn("deploy-now", selfhost.markdown(report), "the helper refuses it there")
                self.assertIn("replace its group's instances", report["recovery"])
                self.assertIn("with an identity that may change instance groups", report["recovery"])

    def test_a_failed_gcp_role_says_nothing_of_deploy_now_however_the_host_put_it(self):
        for said in (None, "", "Run deploy-now once fixed.", "Fix the cause, then run deploy-now.",
                     "Fix the cause, then run roll-back to start 3.0.1 at once, or deploy-now to retry 3.0.2."):
            with self.subTest(said):
                self.assertEqual(selfhost.gcp_recovery(said), f"Fix the cause. {selfhost.ONE_HOST_INSTEAD['deploy-now']}")

    def test_a_failed_gcp_migration_says_nothing_of_deploy_now_from_an_earlier_failure(self):
        # The scheduler's host failed a deploy before, and its report still carries that.
        said = "The database was not changed and 3.0.1 keeps running. Fix the cause, then run deploy-now to retry 3.0.2."
        cli = ScalableCli(cloud="gcp", migration="migration-failed")
        cli.failure = {"error": "boom", "failed": {"release": "3.0.2", "recovery": said}}
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "failed")
        self.assertEqual(report["failed"]["recovery"], selfhost.gcp_recovery(said))
        self.assertNotIn("deploy-now", selfhost.markdown(report), "the helper refuses it there")

    def test_any_gcp_scalable_answer_says_nothing_of_deploy_now_from_an_earlier_failure(self):
        said = "The database was not changed and 3.0.1 keeps running. Fix the cause, then run deploy-now to retry 3.0.2."
        for operation, extra in (("status", {}),
                                 ("diagnostics", {"diagnostics": {"deploys": [{"release": "3.0.2", "recovery": said}]}})):
            with self.subTest(operation):
                cli = FakeCli("gcp", [{"state": operation, "instance": "i-a1", "roles": ["api"],
                                       "failed": {"release": "3.0.2", "recovery": said}, **extra}])
                report = installation(GCP_SCALABLE, cli).run(operation)
                self.assertEqual(report["failed"]["recovery"], selfhost.gcp_recovery(said))
                self.assertIn("replace its group's instances", report["failed"]["recovery"])
                self.assertNotIn("deploy-now", json.dumps(report), "the helper refuses it there")

    def test_a_recovery_that_names_no_deploy_now_is_left_as_the_host_said_it(self):
        said = "3.0.1 was started again by pointing back at it, so this attempt is over."
        cli = FakeCli("gcp", [{"state": "status", "instance": "i-a1", "roles": ["api"],
                               "failed": {"release": "3.0.2", "recovery": said}}])
        self.assertEqual(installation(GCP_SCALABLE, cli).run("status")["failed"]["recovery"], said)

    def test_deploy_now_stays_in_the_recovery_where_the_helper_sends_it(self):
        said = "The database was not changed and 3.0.1 keeps running. Fix the cause, then run deploy-now to retry 3.0.2."
        report = {"state": "status", "failed": {"release": "3.0.2", "recovery": said}}
        cli = FakeCli("gcp", [report])
        self.assertEqual(installation(GCP, cli).run("status")["failed"]["recovery"], said)
        scalable = ScalableCli(fail_role="api")
        scalable.failure = {"error": "boom", "recovery": said, "failed": {"release": "3.0.2", "recovery": said}}
        released = installation(SCALABLE, scalable).release("3.0.2")
        self.assertEqual((released["recovery"], released["failed"]["recovery"]), (said, said))

    def test_a_role_with_no_instance_in_service_is_still_pointed(self):
        cli = ScalableCli(groups={**ScalableCli.GROUPS, "scheduler": []})

        def replaced(role, value):
            # The scheduler's replacement boots from the pointer.
            if role == "scheduler":
                cli.at(30, lambda: (cli.groups["scheduler"].append("i-s2"), cli.log(
                    {"state": "deployed", "release": value, "instance": "i-s2", "roles": ["scheduler"]})))
        cli.on_write = replaced
        with mock.patch("sys.stderr", io.StringIO()) as stderr:
            report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertIn("waits for an instance in service in scheduler's group", stderr.getvalue())
        self.assertEqual(cli.pointers["scheduler"], "3.0.2")
        self.assertEqual(report["rolled"]["scheduler"], ["i-s2"])

    def test_an_instance_that_leaves_its_group_is_not_waited_for(self):
        cli = ScalableCli(groups={**ScalableCli.GROUPS, "api": ["i-a1", "i-a2", "i-a3"]})
        cli.silent = {"i-a3"}

        def scaled_in(role, value):
            if role == "api":
                cli.at(20, lambda: cli.groups["api"].remove("i-a3"))
        cli.on_write = scaled_in
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(report["rolled"]["api"], ["i-a1", "i-a2"])

    def test_a_host_replaced_mid_deploy_gives_way_to_its_replacement(self):
        cli = ScalableCli(groups={**ScalableCli.GROUPS, "api": ["i-a1", "i-a2"]})
        cli.silent = {"i-a2"}
        pointed = {}

        def replaced(role, value):
            pointed[role] = cli.clock.now
            if role == "api":
                # The group replaces i-a2 before it reports. The replacement
                # boots from the pointer and reports a while later.
                cli.at(20, lambda: cli.groups["api"].__setitem__(1, "i-a3"))
                cli.at(60, lambda: cli.log({"state": "deployed", "release": value, "instance": "i-a3", "roles": ["api"]}))
        cli.on_write = replaced
        report = installation(SCALABLE, cli).release("3.0.2", timeout=300)
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(report["rolled"]["api"], ["i-a1", "i-a3"])
        self.assertGreaterEqual(pointed["app"] - pointed["api"], 60, "the role waits for the replacement")

    def test_a_scalable_release_waits_an_hour_by_default(self):
        report = installation(SCALABLE, ScalableCli(delay=100_000)).release("3.0.2")
        self.assertIn("did not report within 3600 seconds", report["error"])

    def test_the_timeout_covers_the_whole_release(self):
        cli = ScalableCli(delay=30)
        report = installation(SCALABLE, cli).release("3.0.2", timeout=100)
        self.assertEqual((report["state"], report["role"]), ("failed", "api"), report)
        self.assertEqual(set(report["rolled"]), {"task-worker", "worker"})
        self.assertIn("within the release's 100 seconds; still waiting for i-a1", report["error"])
        self.assertLessEqual(cli.clock.now - 1_800_000_000, 100)

    def test_a_migration_that_never_reports_is_a_failure(self):
        cli = ScalableCli(delay=1000)
        report = installation(SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual(report["state"], "failed")
        self.assertIn("migration host did not report within 60 seconds", report["error"])
        self.assertEqual(cli.order(), ["migrate"])

    def test_gcp_lists_regional_groups_in_the_zone_region_and_reads_every_report(self):
        groups = {"task-worker": [f"t{i}" for i in range(6)], "worker": [f"w{i}" for i in range(8)],
                  "api": [f"a{i}" for i in range(4)], "app": [f"p{i}" for i in range(4)], "scheduler": ["s0"]}
        cli = ScalableCli(cloud="gcp", groups=groups)
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(report["rolled"]["app"], ["p0", "p1", "p2", "p3"], "after more than 20 reports")
        self.assertEqual(set(cli.listed("--region")), {"us-central1"})
        self.assertEqual(set(cli.listed("--zone")), {"us-central1-a"})

    def test_gcp_needs_a_zone_before_anything_moves(self):
        document = {"installation": dict(GCP_SCALABLE["installation"], zone="")}
        cli = ScalableCli(cloud="gcp")
        with self.assertRaisesRegex(selfhost.CliError, "needs the installation's region and zone"):
            installation(document, cli).release("3.0.2")
        self.assertEqual(cli.order(), [])


class ScalableRetries(unittest.TestCase):
    def test_running_the_release_again_reports_a_failed_role_until_it_deploys(self):
        cli = ScalableCli(fail_role="api")
        installation(SCALABLE, cli).release("3.0.2")
        again = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual((again["state"], again["role"]), ("failed", "api"), "not skipped because its pointer is set")
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api"], "nothing is pointed twice")
        # deploy-now retried the API hosts, and they deployed.
        cli.log({"state": "deployed", "release": "3.0.2", "instance": "i-a1", "roles": ["api"]})
        cli.fail_role = None
        third = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual(third["state"], "deployed", third)
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api", "app", "scheduler"])

    def test_later_reports_from_a_host_do_not_hide_its_deploy(self):
        cli = ScalableCli()
        installation(SCALABLE, cli).release("3.0.2")
        cli.clock.now += 600
        # `command status` reached every host; each report carries the release.
        for role, instances in cli.groups.items():
            for instance in instances:
                cli.log({"state": "status", "release": "3.0.2", "instance": instance, "roles": [role]})
        # A failed restart is no failed deploy.
        cli.log({"state": "failed", "command": "restart-service", "release": "3.0.2", "instance": "i-w1",
                 "roles": ["worker"], "error": "docker compose up exited 1"})
        again = installation(SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual(again["state"], "deployed", again)

    def test_a_rerun_counts_deploys_from_before_it(self):
        cli = ScalableCli(groups={**ScalableCli.GROUPS, "app": ["i-p1"]})
        installation(SCALABLE, cli).release("3.0.2")
        cli.clock.now += 600
        # The app group scaled out; its new instance boots the release.
        cli.groups["app"].append("i-p2")
        cli.log({"state": "deployed", "release": "3.0.2", "instance": "i-p2", "roles": ["app"]}, delay=30)
        again = installation(SCALABLE, cli).release("3.0.2", timeout=120)
        self.assertEqual(again["state"], "deployed", again)
        self.assertEqual(again["rolled"]["app"], ["i-p1", "i-p2"])


class StaleFailures(unittest.TestCase):
    """The retry for a failed migration on GCP is to recreate the scheduler's
    instance and release again, and a release may start before the new host
    has reported. Acting on the old failure would send the operator to recreate
    an instance that is migrating. Any role's group can do the same to a host."""

    CLOUDS = (("aws", SCALABLE), ("gcp", GCP_SCALABLE))

    def after_failure(self, cloud, scheduler, instance="old-s", state="migration-failed"):
        """A release whose migration pointer is set, whose host `instance`
        reported `state`, ten minutes ago, and whose scheduler is now `scheduler`."""
        cli = ScalableCli(cloud=cloud, groups={**ScalableCli.GROUPS, "scheduler": [scheduler]})
        cli.pointers["migrate"] = "3.0.2"
        cli.log({"state": state, "release": "3.0.2", "instance": instance, "roles": ["scheduler"], "error": "boom"})
        cli.clock.now += 600
        return cli

    def report_later(self, cli, instance, state="migrated", delay=30):
        error = {"error": "boom again"} if state == "migration-failed" else {}
        cli.at(delay, lambda: cli.log({"state": state, "release": "3.0.2", "instance": instance,
                                       "roles": ["scheduler"], **error}))

    def test_a_replaced_hosts_failure_waits_for_the_new_host(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.after_failure(cloud, "new-s")
                before = cli.clock.now
                self.report_later(cli, "new-s")
                report = installation(document, cli).release("3.0.2")
                self.assertEqual(report["state"], "deployed", report)
                self.assertGreaterEqual(cli.clock.now - before, 30, "it waited for the new host")
                self.assertEqual(cli.order(), ["task-worker", "worker", "api", "app", "scheduler"])

    def test_a_recreated_instance_keeps_its_name_but_not_its_old_failure(self):
        # GCP's recreate keeps the instance's name, so only its start time tells.
        cli = self.after_failure("gcp", "i-s1", instance="i-s1")
        cli.started["i-s1"] = cli.clock.now - 60
        before = cli.clock.now
        self.report_later(cli, "i-s1")
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertGreaterEqual(cli.clock.now - before, 30, "it waited for the recreated instance")

    def test_a_new_host_that_never_reports_times_out(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.after_failure(cloud, "new-s")
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual(report["state"], "failed")
                self.assertIn("migration host did not report within 60 seconds", report["error"])
                self.assertNotIn("recreate", report["error"], "it may be migrating")
                self.assertNotIn("old-s", report["error"], "a host the group replaced says nothing")
                self.assertEqual(cli.order(), [])

    def test_a_scheduler_restarted_in_place_keeps_its_failed_migration_when_the_wait_times_out(self):
        # A restart moves the start time, but the agent keeps its state, and
        # reports nothing new about a release that failed: this is the answer.
        cli = self.after_failure("gcp", "i-s1", instance="i-s1")
        failed_at = rfc3339(cli.clock.now - 600)
        cli.started["i-s1"] = cli.clock.now - 60
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual(report["state"], "failed")
        self.assertIn("migration host did not report within 60 seconds; it may still be migrating", report["error"])
        self.assertIn(f"i-s1 last reported migration-failed at {failed_at}: boom, and has restarted since", report["error"])
        self.assertIn("recreate the scheduler's instance", report["error"])
        self.assertEqual(cli.order(), [])

    def test_the_new_hosts_own_failure_is_the_answer(self):
        cli = self.after_failure("gcp", "new-s")
        self.report_later(cli, "new-s", state="migration-failed")
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["instance"]), ("failed", "new-s"))
        self.assertIn("boom again", report["error"])
        self.assertEqual(cli.order(), [])

    def test_a_failure_from_the_host_as_it_runs_now_is_answered_at_once(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.after_failure(cloud, "i-s1", instance="i-s1")
                cli.started["i-s1"] = cli.clock.now - 3600
                before = cli.clock.now
                report = installation(document, cli).release("3.0.2")
                self.assertEqual((report["state"], report["instance"]), ("failed", "i-s1"))
                self.assertIn("boom", report["error"])
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual(cli.order(), [])

    def test_a_failure_stands_when_the_instance_gives_no_start_time(self):
        gcp = installation(GCP_SCALABLE, FakeCli("gcp", []))
        with mock.patch.object(gcp, "gcp_instances", return_value={"i-s1": "us-central1-a"}):
            self.assertTrue(gcp.failure_stands({"instance": "i-s1", "at": rfc3339(0)}, "scheduler"))

    def test_a_failure_from_an_instance_no_longer_running_does_not_stand(self):
        gcp = installation(GCP_SCALABLE, ScalableCli(cloud="gcp"))
        self.assertFalse(gcp.failure_stands({"instance": "i-gone", "at": rfc3339(0)}, "app"))

    def test_report_and_start_times_compare_across_their_formats(self):
        # The agent writes UTC with microseconds; Compute Engine, an offset with milliseconds.
        self.assertLess(selfhost.parse_time("2026-11-02T15:12:33.123456+00:00"),
                        selfhost.parse_time("2026-11-02T08:12:34.000-07:00"))
        # Secret Manager may write nanoseconds, which Python before 3.11 refuses.
        self.assertEqual(selfhost.parse_time("2026-11-02T15:12:33.123456789Z"),
                         selfhost.parse_time("2026-11-02T15:12:33.123456+00:00"))

    def test_a_replaced_hosts_migration_still_counts(self):
        # The database did migrate, whichever host did it.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.after_failure(cloud, "new-s", state="migrated")
                before = cli.clock.now
                report = installation(document, cli).release("3.0.2")
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(cli.clock.now, before)

    def test_a_stale_failure_does_not_hide_an_earlier_migration(self):
        cli = self.after_failure("gcp", "new-s", state="migrated")
        cli.log({"state": "migration-failed", "release": "3.0.2", "instance": "old-s", "roles": ["scheduler"]})
        cli.clock.now += 600
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["state"], "deployed")

    def test_a_recreated_schedulers_old_deploy_failure_waits_for_its_new_report(self):
        # Replacing the scheduler's group recreates its instance, under the same name.
        cli = ScalableCli(cloud="gcp", fail_role="scheduler")
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], "scheduler")
        cli.clock.now += 600
        cli.started["i-s1"] = cli.clock.now - 60
        before = cli.clock.now
        self.report_later(cli, "i-s1", state="deployed")
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertGreaterEqual(cli.clock.now - before, 30)

    def test_a_scheduler_deploy_failure_from_the_host_as_it_runs_now_stands(self):
        cli = ScalableCli(cloud="gcp", fail_role="scheduler")
        installation(GCP_SCALABLE, cli).release("3.0.2")
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual((report["state"], report["role"]), ("failed", "scheduler"))

    def test_any_roles_recreated_host_waits_for_its_new_report(self):
        # Autohealing recreates a web host under its name, and any group repairs
        # a dead VM the same way. Regional groups spread hosts across zones.
        one_each = {"task-worker": ["i-t1"], "worker": ["i-w1"], "api": ["i-a1"], "app": ["i-p1"], "scheduler": ["i-s1"]}
        for role, instances in one_each.items():
            with self.subTest(role):
                instance = instances[0]
                cli = ScalableCli(cloud="gcp", fail_role=role, groups=one_each)
                cli.zones[instance] = "us-central1-a" if role == "scheduler" else "us-central1-c"
                self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], role)
                cli.fail_role = None
                cli.clock.now += 600
                cli.started[instance] = cli.clock.now - 60
                before = cli.clock.now
                cli.at(30, lambda: cli.log({"state": "deployed", "release": "3.0.2", "instance": instance, "roles": [role]}))
                report = installation(GCP_SCALABLE, cli).release("3.0.2")
                self.assertEqual(report["state"], "deployed", report)
                self.assertGreaterEqual(cli.clock.now - before, 30, "it waited for the recreated host")
                described = [a for a in cli.calls if a[:4] == ("gcloud", "compute", "instances", "describe")]
                self.assertEqual({a[a.index("--zone") + 1] for a in described}, {cli.zones[instance]},
                                 "in the instance's own zone")

    def test_another_hosts_failure_in_the_role_still_stands(self):
        cli = ScalableCli(cloud="gcp", fail_role="app")
        cli.zones.update({"i-p1": "us-central1-b", "i-p2": "us-central1-c"})
        installation(GCP_SCALABLE, cli).release("3.0.2")
        cli.clock.now += 600
        cli.started["i-p1"] = cli.clock.now - 60
        before = cli.clock.now
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"], report["instance"]), ("failed", "app", "i-p2"))
        self.assertEqual(cli.clock.now, before, "nothing to wait for")

    def test_a_failure_from_a_host_as_it_runs_now_stands_in_any_role(self):
        cli = ScalableCli(cloud="gcp", fail_role="api")
        cli.zones["i-a1"] = "us-central1-f"
        installation(GCP_SCALABLE, cli).release("3.0.2")
        cli.clock.now += 600
        cli.started["i-a1"] = cli.clock.now - 3600 * 24
        before = cli.clock.now
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"], report["instance"]), ("failed", "api", "i-a1"))
        self.assertEqual(cli.clock.now, before)

    def restarted_after_a_failed_deploy(self):
        """A GCP release whose app hosts both failed, ten minutes ago, and
        have both started since, as a restart in place does."""
        cli = ScalableCli(cloud="gcp", fail_role="app")
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], "app")
        cli.log({"state": "failed", "release": "3.0.2", "instance": "i-p1", "roles": ["app"], "error": "compose up exited 1"})
        failed_at = rfc3339(cli.clock.now)
        cli.clock.now += 600
        cli.started.update({"i-p1": cli.clock.now - 60, "i-p2": cli.clock.now - 60})
        return cli, failed_at

    def test_a_host_restarted_in_place_keeps_its_failed_deploy_when_the_wait_times_out(self):
        # A restart moves the start time, but the agent keeps its state, and
        # reports nothing new about a release that failed: this is the answer.
        cli, failed_at = self.restarted_after_a_failed_deploy()
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=3600)
        self.assertEqual((report["state"], report["role"]), ("failed", "app"))
        self.assertEqual(sorted(report["rolled"]), ["api", "task-worker", "worker"])
        self.assertIn("app did not finish within the release's 3600 seconds; still waiting for i-p1, i-p2", report["error"])
        self.assertIn(f"i-p1 last reported failed at {failed_at}: compose up exited 1, and has restarted since", report["error"])
        self.assertIn(f"i-p2 last reported failed at {failed_at}: no error given, and has restarted since", report["error"])
        self.assertIn("replace its group's instances", report["error"])

    def test_a_timeout_names_only_the_set_aside_failures_still_unanswered(self):
        cli, _ = self.restarted_after_a_failed_deploy()
        cli.at(30, lambda: cli.log({"state": "deployed", "release": "3.0.2", "instance": "i-p2", "roles": ["app"]}))
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=3600)
        self.assertIn("still waiting for i-p1.", report["error"])
        self.assertIn("i-p1 last reported failed", report["error"])
        self.assertNotIn("i-p2", report["error"], "its new host deployed")

    def test_a_timeout_with_no_failure_set_aside_says_nothing_of_one(self):
        cli = ScalableCli(cloud="gcp")
        cli.silent.add("i-p1")
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual(report["error"], "app did not finish within the release's 60 seconds; still waiting for i-p1")


class EarlierReleases(unittest.TestCase):
    """Going back to a version writes its pointers again, and the log still
    holds `migrated` and `deployed` from when that version was first released.
    Role hosts deploy on the migration host's word, and the release reports on
    its hosts' word, so only a report from since a pointer was written may give it."""

    CLOUDS = StaleFailures.CLOUDS
    WEEK = 7 * 86400
    ROLES = selfhost.ROLE_ORDER
    # Every role but the scheduler, which is last.
    OTHERS = ROLES[:-1]

    def refused(self, cloud, document):
        """3.0.1 was released, then 3.0.2 took the database further, and now
        `release 3.0.1` has been refused by the migration host."""
        cli = ScalableCli(cloud=cloud)
        self.assertEqual(installation(document, cli).release("3.0.1")["state"], "deployed")
        cli.clock.now += self.WEEK
        self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
        cli.clock.now += self.WEEK
        cli.migration = "migration-failed"
        self.assertEqual(installation(document, cli).release("3.0.1")["state"], "failed")
        self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"})
        cli.clock.now += 600
        return cli

    def replace_scheduler(self, cli):
        """The instance the failed migration's error says to recreate. GCP's
        keeps its name, and AWS's group launches a new one."""
        if cli.cloud == "gcp":
            cli.started["i-s1"] = cli.clock.now - 60
            return "i-s1"
        cli.groups["scheduler"] = ["new-s"]
        return "new-s"

    def report_later(self, cli, instance, state, delay=30):
        error = {"error": "Release 3.0.1 knows migrations up to 22"} if state == "migration-failed" else {}
        cli.at(delay, lambda: cli.log({"state": state, "release": "3.0.1", "instance": instance,
                                       "roles": ["scheduler"], **error}))

    def test_a_replaced_scheduler_waits_for_its_new_host(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.refused(cloud, document)
                self.replace_scheduler(cli)
                report = installation(document, cli).release("3.0.1", timeout=60)
                self.assertEqual(report["state"], "failed", report)
                self.assertIn("migration host did not report within 60 seconds", report["error"])
                self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"},
                                 "the old migrated report gives no role a go-ahead")

    def test_the_new_hosts_refusal_is_the_answer(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.refused(cloud, document)
                instance = self.replace_scheduler(cli)
                self.report_later(cli, instance, "migration-failed")
                report = installation(document, cli).release("3.0.1")
                self.assertEqual((report["state"], report["instance"]), ("failed", instance))
                self.assertIn("knows migrations up to 22", report["error"])
                self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"})

    def test_the_new_hosts_migration_lets_the_roles_roll(self):
        # The database was restored meanwhile, so this release now fits it.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.refused(cloud, document)
                before = cli.clock.now
                self.report_later(cli, self.replace_scheduler(cli), "migrated")
                report = installation(document, cli).release("3.0.1")
                self.assertEqual(report["state"], "deployed", report)
                self.assertGreaterEqual(cli.clock.now - before, 30, "it waited for the new host")
                self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.1"})

    def test_a_refusal_from_the_host_as_it_runs_now_is_answered_at_once(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.refused(cloud, document)
                before = cli.clock.now
                report = installation(document, cli).release("3.0.1")
                self.assertEqual(report["state"], "failed")
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"})

    def test_a_report_counts_from_when_the_pointer_was_written(self):
        # Within the allowance for a host's clock, as with any other read.
        for cloud, document in self.CLOUDS:
            for age, state in ((30, "deployed"), (selfhost.CLOCK_SKEW + 1, "failed")):
                with self.subTest(cloud=cloud, age=age):
                    cli = ScalableCli(cloud=cloud)
                    cli.pointers["migrate"] = "3.0.1"
                    cli.log({"state": "migrated", "release": "3.0.1", "instance": "i-s1", "roles": ["scheduler"]})
                    cli.written["migrate"] = cli.clock.now + age
                    cli.clock.now += age + 600
                    self.assertEqual(installation(document, cli).release("3.0.1", timeout=60)["state"], state)

    def test_a_pointer_whose_time_cannot_be_read_moves_nothing(self):
        # A stack from before the release account could read it, on GCP.
        cli = ScalableCli(cloud="gcp")
        cli.pointers["migrate"] = "3.0.1"
        cli.log({"state": "migrated", "release": "3.0.1", "instance": "i-s1", "roles": ["scheduler"]})
        cli.written_unreadable = ("migrate",)
        with self.assertRaisesRegex(selfhost.CliError, "Could not tell when the migrate pointer was last written.*"
                                                       "PERMISSION_DENIED.*apply it, then release again"):
            installation(GCP_SCALABLE, cli).release("3.0.1", timeout=60)
        self.assertEqual(cli.order(), [])

    def test_a_role_pointers_time_that_may_not_be_read_moves_nothing(self):
        # A stack that lets the release account read the migration pointer's time only.
        cli = ScalableCli(cloud="gcp", fail_role="api")
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], "api")
        before = cli.clock.now
        cli.written_unreadable = ("worker",)
        with self.assertRaisesRegex(selfhost.CliError, "Could not tell when the worker pointer was last written.*"
                                                       "PERMISSION_DENIED.*apply it, then release again"):
            installation(GCP_SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api"], "nothing is written")
        self.assertEqual(cli.clock.now, before, "no wait gets past it")

    def stopped(self, cloud, document):
        """3.0.1 was released, then 3.0.2, and every group was resized to 0 to
        restore the database to before 3.0.2. Returns the groups as they were."""
        cli = ScalableCli(cloud=cloud)
        self.assertEqual(installation(document, cli).release("3.0.1")["state"], "deployed")
        cli.clock.now += self.WEEK
        self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
        cli.clock.now += self.WEEK
        sizes, cli.groups = cli.groups, {role: [] for role in cli.groups}
        cli.silent = {"i-s1"}
        return cli, sizes

    def come_back(self, cli, sizes, roles=("scheduler", *OTHERS), migration="migrated"):
        """These groups' sizes are restored, and each new host acts on its
        pointers as it boots: after a refused migration too, as the agent does."""
        cli.silent = set()
        for role in roles:
            cli.groups[role] = sizes[role]
            if role == "scheduler":
                cli.log({"state": migration, "release": cli.pointers["migrate"], "instance": sizes[role][0], "roles": [role]})
            for instance in sizes[role]:
                cli.log({"state": "deployed", "release": cli.pointers[role], "instance": instance, "roles": [role]})

    def pointers_first(self, cloud, document):
        """Every group is at 0 and the database is restored to before 3.0.2, and
        all six pointers now name 3.0.1, as the refusal of drain-and-stop says."""
        cli, sizes = self.stopped(cloud, document)
        found = installation(document, cli)
        for pointer in ("migrate", *self.ROLES):
            found.point_at("3.0.1", pointer)
        cli.clock.now += 600
        return cli, sizes, found

    def test_with_every_group_at_0_a_release_says_no_host_can_answer(self):
        # Not that a host may still be migrating. Its role pointers stay on 3.0.2,
        # which hosts brought back next would deploy on the restored database.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli, _ = self.stopped(cloud, document)
                for run in ("the run that writes the pointer", "the run after it"):
                    report = installation(document, cli).release("3.0.1", timeout=60)
                    self.assertEqual(report["state"], "failed", run)
                    self.assertRegex(report["error"], "^The migration host did not report within 60 seconds; the "
                                                      "scheduler's group has no instance in service")
                    self.assertIn("after a database restore first write every role's pointer to 3.0.1", report["error"])
                    for wrong in ("may still be migrating", "check its journal", "will not end this wait"):
                        self.assertNotIn(wrong, report["error"], run)
                    self.assertEqual(cli.pointers["migrate"], "3.0.1")
                    self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"})

    def test_a_group_that_cannot_be_read_leaves_the_report_as_it_was(self):
        for cloud, document, read in (("aws", SCALABLE, "describe-auto-scaling-groups"),
                                      ("gcp", GCP_SCALABLE, "list-instances")):
            with self.subTest(cloud):
                cli = FlakyScalable(read, cloud=cloud, groups={**ScalableCli.GROUPS, "scheduler": []}, times=10_000)
                cli.silent = {"i-s1"}
                report = installation(document, cli).release("3.0.1", timeout=60)
                self.assertEqual(report["state"], "failed", report)
                self.assertIn("migration host did not report within 60 seconds; it may still be migrating", report["error"])
                self.assertNotIn("no instance in service", report["error"])
                self.assertEqual(cli.failed, 1, "read once, for this report")

    def test_a_migration_host_in_service_may_still_be_migrating(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                report = installation(document, ScalableCli(cloud=cloud, delay=1000)).release("3.0.2", timeout=60)
                self.assertIn("it may still be migrating: check its journal, then release again", report["error"])
                self.assertNotIn("no instance in service", report["error"])

    def test_a_release_with_the_scheduler_back_alone_refused_leaves_the_others_down(self):
        # The restore did not lower the level, and role hosts do not check it. They
        # stay at 0, and nothing tells the operator to bring them back.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli, sizes, found = self.pointers_first(cloud, document)
                self.come_back(cli, sizes, ["scheduler"], migration="migration-failed")
                with mock.patch("sys.stderr", io.StringIO()) as stderr:
                    report = found.release("3.0.1", timeout=600)
                self.assertEqual((report["state"], report["instance"]), ("failed", sizes["scheduler"][0]), report)
                self.assertEqual(stderr.getvalue(), "")
                self.assertEqual({role: cli.groups[role] for role in self.OTHERS}, {role: [] for role in self.OTHERS})

    def test_the_others_come_back_once_the_release_says_the_migration_has_passed(self):
        # The order the refusal of drain-and-stop gives: the scheduler alone, then
        # the rest while the same release waits.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli, sizes, found = self.pointers_first(cloud, document)
                self.come_back(cli, sizes, ["scheduler"])
                cli.at(120, lambda: self.come_back(cli, sizes, self.OTHERS))
                before, written = cli.clock.now, dict(cli.written)
                with mock.patch("sys.stderr", io.StringIO()) as stderr:
                    report = found.release("3.0.1", timeout=600)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(report["rolled"], {role: sizes[role] for role in self.ROLES})
                self.assertLess(cli.clock.now - before, 600, "it goes on as they report")
                self.assertEqual(cli.written, written, "no pointer moves, which would lose the reports")
                self.assertEqual(stderr.getvalue(), "warning: the migration has passed, and the release now waits for an "
                                                    "instance in service in task-worker's group: if it is at 0, restore "
                                                    "its size\n",
                                 "the cue, once however many polls")

    def test_a_group_not_back_yet_says_releasing_again_ends_the_wait(self):
        # A new host reports as it boots, so this is not a host with nothing new to say.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli, sizes, found = self.pointers_first(cloud, document)
                self.come_back(cli, sizes, ["scheduler"])
                with mock.patch("sys.stderr", io.StringIO()):
                    report = found.release("3.0.1", timeout=60)
                self.assertEqual((report["state"], report["role"], report["rolled"]), ("failed", "task-worker", {}), report)
                self.assertEqual(report["error"], "task-worker did not finish within the release's 60 seconds; still "
                                                  "waiting for an instance in service. Its group is empty, and a new "
                                                  "host reports as it boots: restore its size, then release again")
                self.come_back(cli, sizes, self.OTHERS)
                self.assertEqual(found.release("3.0.1", timeout=60)["state"], "deployed")

    def test_a_group_with_a_silent_host_still_says_releasing_again_may_not(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.back_with_a_silent_host(cloud, document)
                installation(document, cli).release("3.0.1", timeout=60)
                report = installation(document, cli).release("3.0.1", timeout=60)
                self.assertIn("still waiting for i-t1. A host that had already acted on this release", report["error"])
                self.assertNotIn("restore its size", report["error"])

    def test_pointers_written_first_bring_the_hosts_back_on_the_earlier_release(self):
        # With every group back before the release, as an apply brings them, it only checks.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli, sizes, found = self.pointers_first(cloud, document)
                self.come_back(cli, sizes)
                cli.clock.now += 300
                before, written = cli.clock.now, dict(cli.written)
                report = found.release("3.0.1", timeout=60)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(report["rolled"], {role: sizes[role] for role in self.ROLES})
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual(cli.written, written, "no pointer moves, which would lose the reports")

    def back_with_a_silent_host(self, cloud, document):
        """3.0.1 was released, then 3.0.2, and now `release 3.0.1`: its migration
        ran, and i-t1 has yet to answer its pointer."""
        cli = ScalableCli(cloud=cloud)
        self.assertEqual(installation(document, cli).release("3.0.1")["state"], "deployed")
        cli.clock.now += self.WEEK
        self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
        cli.clock.now += self.WEEK
        cli.silent.add("i-t1")
        return cli

    def test_a_host_silent_on_a_role_pointer_is_not_answered_by_its_old_deploy(self):
        # It may still be on 3.0.2, against a database restored to 3.0.1's schema.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.back_with_a_silent_host(cloud, document)
                for run in ("the run that writes the pointer", "the run after it"):
                    report = installation(document, cli).release("3.0.1", timeout=60)
                    self.assertEqual((report["state"], report["role"], report["rolled"]), ("failed", "task-worker", {}), run)
                    self.assertIn("still waiting for i-t1", report["error"], run)
                    self.assertEqual(cli.pointers["worker"], "3.0.2", "the next role stays where it is")

    def test_a_host_that_answers_a_role_pointer_on_a_later_run_counts(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.back_with_a_silent_host(cloud, document)
                self.assertEqual(installation(document, cli).release("3.0.1", timeout=60)["state"], "failed")
                cli.log({"state": "deployed", "release": "3.0.1", "instance": "i-t1", "roles": ["task-worker"]})
                report = installation(document, cli).release("3.0.1", timeout=60)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(report["rolled"]["task-worker"], ["i-t1"])

    def test_a_role_that_never_moved_still_counts_its_deploy(self):
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = ScalableCli(cloud=cloud)
                self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                cli.clock.now += self.WEEK
                before = cli.clock.now
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual(cli.order(), ["migrate", *self.ROLES], "nothing is written again")

    def test_a_rerun_going_back_stops_at_the_role_still_to_answer(self):
        # Not only the first role: the ones before it have answered this
        # pointer, and the worker hosts have yet to.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.back_with_a_silent_host(cloud, document)
                cli.silent = {"i-w1", "i-w2"}
                for run in ("the run that writes the pointer", "the run after it"):
                    report = installation(document, cli).release("3.0.1", timeout=60)
                    self.assertEqual((report["state"], report["role"], list(report["rolled"])),
                                     ("failed", "worker", ["task-worker"]), run)
                    self.assertIn("still waiting for i-w1, i-w2", report["error"], run)
                    self.assertEqual(cli.pointers["api"], "3.0.2", "the next role stays where it is")
                cli.log({"state": "failed", "release": "3.0.1", "instance": "i-w1", "roles": ["worker"], "error": "boom"})
                report = installation(document, cli).release("3.0.1", timeout=60)
                self.assertEqual((report["state"], report["role"], report["instance"]), ("failed", "worker", "i-w1"),
                                 "a failure after the re-runs is still reported")

    def test_going_back_to_the_release_the_roles_never_left_counts_their_deploys(self):
        # 3.0.3 was refused, so no role moved. Their deploys of 3.0.2 are still
        # true, and no host has anything new to say about it.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = ScalableCli(cloud=cloud)
                self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                cli.clock.now += self.WEEK
                cli.migration = "migration-failed"
                self.assertEqual(installation(document, cli).release("3.0.3")["state"], "failed")
                self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.2"})
                cli.clock.now += self.WEEK
                cli.migration = "migrated"
                before = cli.clock.now
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual(cli.order(), ["migrate", *self.ROLES, "migrate", "migrate"], "no role's pointer moves")

    def test_a_role_report_counts_from_when_its_pointer_was_written(self):
        # Within the allowance for a host's clock, as with the migration's.
        for cloud, document in self.CLOUDS:
            for age, state in ((30, "deployed"), (selfhost.CLOCK_SKEW + 1, "failed")):
                with self.subTest(cloud=cloud, age=age):
                    cli = ScalableCli(cloud=cloud)
                    for pointer in ("migrate", "task-worker"):
                        cli.pointers[pointer] = "3.0.1"
                    cli.log({"state": "migrated", "release": "3.0.1", "instance": "i-s1", "roles": ["scheduler"]})
                    cli.log({"state": "deployed", "release": "3.0.1", "instance": "i-t1", "roles": ["task-worker"]})
                    cli.written["task-worker"] = cli.clock.now + age
                    cli.clock.now += age + 600
                    self.assertEqual(installation(document, cli).release("3.0.1", timeout=60)["state"], state)

    def test_a_pointer_back_on_a_release_its_host_never_left_says_a_rerun_will_not_help(self):
        # 3.0.2's run stopped before the hosts saw its pointers. They are still
        # on 3.0.1, and the agent reports nothing about what it has acted on.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = ScalableCli(cloud=cloud)
                self.assertEqual(installation(document, cli).release("3.0.1")["state"], "deployed")
                cli.clock.now += self.WEEK
                cli.pointers["migrate"] = "3.0.2"
                first = installation(document, cli).release("3.0.1", timeout=60)
                self.assertIn("migration host did not report within 60 seconds", first["error"])
                self.assertNotIn("reports nothing new", first["error"], "it may simply be migrating")
                again = installation(document, cli).release("3.0.1", timeout=60)
                self.assertIn("migration host did not report within 60 seconds", again["error"])
                self.assertIn("reports nothing new, so releasing again will not end this wait: see Upgrading", again["error"])

    def test_a_host_that_answers_a_written_back_pointer_ends_the_wait(self):
        # deploy-now on AWS: the migration host says it again, and role hosts
        # say current.
        cli = ScalableCli()
        self.assertEqual(installation(SCALABLE, cli).release("3.0.1")["state"], "deployed")
        cli.clock.now += self.WEEK
        for pointer in ("migrate", "task-worker"):
            cli.pointers[pointer] = "3.0.2"
        self.assertEqual(installation(SCALABLE, cli).release("3.0.1", timeout=60)["state"], "failed")
        cli.clock.now += 60
        installation(SCALABLE, cli).run("deploy-now", timeout=60)
        report = installation(SCALABLE, cli).release("3.0.1", timeout=60)
        self.assertEqual(report["state"], "deployed", report)

    def failed_and_stayed(self, cloud, document, stayed=("i-w1", "i-w2")):
        """3.0.1 was released, then 3.0.2 failed on the `stayed` worker hosts,
        which went on running 3.0.1, and the release stopped at their role. As
        the agent does, they say so once, 30 seconds after the pointer comes
        back: `current`, naming no command or request, since none asked, with
        the failure still in their state, which every report carries."""
        cli = ScalableCli(cloud=cloud)
        self.assertEqual(installation(document, cli).release("3.0.1")["state"], "deployed")
        cli.clock.now += self.WEEK
        cli.silent = set(stayed)
        failed = {"release": "3.0.2", "error": "the mirror lags", "at": rfc3339(cli.clock.now)}

        def answer(role, release):
            for instance in stayed if role == "worker" else ():
                host = {"instance": instance, "roles": [role]}
                if release == "3.0.2":
                    cli.log({**failed, **host, "state": "failed"})
                else:
                    cli.log({**host, "state": "current", "release": release, "failed": failed,
                             "pointer_returned": failed["at"]}, 30)
        cli.on_write = answer
        report = installation(document, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"]), ("failed", "worker"))
        self.assertEqual(cli.pointers["api"], "3.0.1", "the roles after it never moved")
        cli.clock.now += self.WEEK
        return cli

    def test_a_host_that_stayed_on_a_release_answers_its_pointer_coming_back(self):
        # It has nothing to deploy, and its report of that counts, beside a host
        # in the role that did go on to 3.0.2 and deploys 3.0.1 again.
        for cloud, document in self.CLOUDS:
            for stayed in (("i-w1", "i-w2"), ("i-w1",)):
                with self.subTest(cloud=cloud, stayed=stayed):
                    cli = self.failed_and_stayed(cloud, document, stayed)
                    before = cli.clock.now
                    report = installation(document, cli).release("3.0.1")
                    self.assertEqual(report["state"], "deployed", report)
                    self.assertEqual(report["rolled"]["worker"], ["i-w1", "i-w2"])
                    self.assertEqual(list(report["rolled"]), list(self.ROLES))
                    self.assertEqual(cli.clock.now - before, 30, "the wait ends when they answer, not at its timeout")
                    self.assertEqual({cli.pointers[role] for role in self.ROLES}, {"3.0.1"})

    def test_a_rerun_finds_what_a_host_said_when_its_pointer_came_back(self):
        # The agent says it once, so a run that ended before it did, and any run
        # after that, must find it by when the pointer was written.
        for cloud, document in self.CLOUDS:
            with self.subTest(cloud):
                cli = self.failed_and_stayed(cloud, document)
                first = installation(document, cli).release("3.0.1", timeout=20)
                self.assertEqual((first["state"], first["role"]), ("failed", "worker"))
                self.assertIn("still waiting for i-w1, i-w2", first["error"])
                self.assertEqual(cli.pointers["api"], "3.0.1")
                cli.clock.now += self.WEEK
                before, writes = cli.clock.now, cli.order()
                for run in ("the run after they answered", "the run after that"):
                    report = installation(document, cli).release("3.0.1", timeout=20)
                    self.assertEqual(report["state"], "deployed", (run, report))
                    self.assertEqual(report["rolled"]["worker"], ["i-w1", "i-w2"], run)
                self.assertEqual(cli.clock.now, before, "nothing to wait for")
                self.assertEqual(cli.order(), writes, "no pointer is written again, which would lose the report")

    def test_the_time_is_read_as_each_cli_prints_it(self):
        def written(document, answer):
            return selfhost.Installation(document, lambda *args, stdin=None: answer).pointer_written("migrate")
        aws = lambda value: {"Parameter": {"Value": "3.0.1", "LastModifiedDate": value}}  # noqa: E731
        expected = selfhost.parse_time("2026-11-02T15:12:33.500+00:00")
        self.assertEqual(written(SCALABLE, aws("2026-11-02T08:12:33.500000-07:00")), expected, "AWS CLI v2")
        self.assertEqual(written(SCALABLE, aws(expected)), expected, "AWS CLI v1")
        self.assertEqual(written(GCP_SCALABLE, {"createTime": "2026-11-02T15:12:33.500000000Z"}), expected)
        for document, answer in ((SCALABLE, {"Parameter": {"Value": "3.0.1"}}), (GCP_SCALABLE, {"name": "versions/1"}),
                                 (GCP_SCALABLE, None), (GCP_SCALABLE, {"createTime": "soon"})):
            with self.subTest(answer), self.assertRaisesRegex(selfhost.CliError, "Could not tell when the migrate pointer"):
                written(document, answer)


class ScalableCommands(unittest.TestCase):
    def test_a_service_restart_waits_for_every_host_and_ignores_hosts_without_it(self):
        cli = ScalableCli()
        cli.answer_delay = {"i-w2": 20}
        report = installation(SCALABLE, cli).run("restart-service", "worker", request_id="r1")
        self.assertEqual(report["state"], "restarted", report)
        self.assertEqual(report["answers"]["i-w2"], "restarted")
        self.assertEqual(report["answers"]["i-a1"], "rejected")
        self.assertEqual(len(report["answers"]), 7)

    def rejecting(self, cli, instance, error):
        answer = cli.answer
        cli.answer = lambda host, *rest: ({**answer(host, *rest), "state": "rejected", "error": error}
                                          if host == instance else answer(host, *rest))

    def test_a_host_without_the_service_and_nothing_deployed_is_not_the_answer(self):
        # A new task-worker whose first deploy failed stays in service.
        cli = ScalableCli()
        self.rejecting(cli, "i-t1", "Nothing is deployed yet")
        report = installation(SCALABLE, cli).run("logs", "worker", request_id="n1")
        self.assertEqual(report["state"], "logs", report)
        self.assertEqual(report["logs"], ["i-w2 line"])
        self.assertNotIn("error", report)
        self.assertEqual(report["answers"]["i-t1"], "rejected")

        cli = ScalableCli()
        self.rejecting(cli, "i-t1", "Nothing is deployed yet")
        report = installation(SCALABLE, cli).run("restart-service", "worker", request_id="n2")
        self.assertEqual(report["state"], "restarted", report)

    def test_a_host_with_the_service_and_nothing_deployed_is_the_answer(self):
        cli = ScalableCli()
        self.rejecting(cli, "i-w1", "Nothing is deployed yet")
        report = installation(SCALABLE, cli).run("restart-service", "worker", request_id="n3")
        self.assertEqual((report["state"], report["instance"]), ("rejected", "i-w1"))

    def test_the_ingress_is_on_the_web_hosts_though_their_roles_do_not_list_it(self):
        cli = ScalableCli()
        report = installation(SCALABLE, cli).run("restart-service", "ingress", request_id="n4")
        self.assertEqual(report["state"], "restarted", report)
        self.assertEqual(report["answers"]["i-w1"], "rejected")

        cli = ScalableCli()
        self.rejecting(cli, "i-p1", "Nothing is deployed yet")
        report = installation(SCALABLE, cli).run("restart-service", "ingress", request_id="n5")
        self.assertEqual((report["state"], report["instance"]), ("rejected", "i-p1"))

    def test_a_command_that_names_no_service_keeps_every_rejection(self):
        # The command workflow passes its service input with any operation.
        cli = ScalableCli()
        self.rejecting(cli, "i-s1", "The database needs a restore first")
        report = installation(SCALABLE, cli).run("deploy-now", "worker", request_id="n6")
        self.assertEqual((report["state"], report["instance"]), ("rejected", "i-s1"))

    def test_deploy_now_waits_for_the_migration_host(self):
        cli = ScalableCli()
        cli.answer_delay = {"i-s1": 30}
        report = installation(SCALABLE, cli).run("deploy-now", request_id="r2")
        self.assertEqual(report["answers"]["i-s1"], "migrated")
        self.assertGreaterEqual(cli.clock.now - 1_800_000_000, 30)

    def test_naming_no_service_is_a_rejection_everywhere(self):
        cli = ScalableCli()
        report = installation(SCALABLE, cli).run("restart-service", request_id="r3")
        self.assertEqual(report["state"], "rejected")
        self.assertEqual(report["error"], "Unknown service ''")

    def test_a_failure_on_one_host_is_the_answer(self):
        cli = ScalableCli()
        answer = cli.answer
        cli.answer = lambda instance, *rest: ({**answer(instance, *rest), "state": "failed", "error": "boom"}
                                             if instance == "i-w1" else answer(instance, *rest))
        report = installation(SCALABLE, cli).run("deploy-now", request_id="r4")
        self.assertEqual((report["state"], report["instance"]), ("failed", "i-w1"))

    def test_a_silent_host_is_named_and_the_answers_survive(self):
        cli = ScalableCli()
        cli.silent = {"i-s1"}
        report = installation(SCALABLE, cli).run("logs", "worker", request_id="r5", timeout=60)
        self.assertEqual(report["state"], "failed", "so the helper exits 1")
        self.assertRegex(report["error"], "^i-s1 did not answer r5 within 60 seconds")
        self.assertEqual(report["logs"], ["i-w2 line"])
        self.assertEqual(report["answers"]["i-w1"], "logs")
        self.assertNotIn("i-s1", report["answers"])
        self.assertIn("i-w2 line", selfhost.markdown(report))

    def test_the_only_host_with_the_service_being_silent_leads_the_error(self):
        cli = ScalableCli(groups=dict(ScalableCli.GROUPS, worker=["i-w1"]))
        cli.silent = {"i-w1"}
        report = installation(SCALABLE, cli).run("logs", "worker", request_id="r9", timeout=60)
        self.assertEqual(report["state"], "failed", "so the helper exits 1")
        self.assertRegex(report["error"], "^i-w1 did not answer r9 within 60 seconds")
        self.assertNotIn("Unknown service", report["error"])
        for unrelated in ("instance", "roles", "release"):
            self.assertNotIn(unrelated, report)
        self.assertEqual(report["answers"]["i-t1"], "rejected")
        self.assertIn("i-w1 did not answer", selfhost.markdown(report))

    def test_naming_no_service_stays_the_answer_beside_a_silent_host(self):
        cli = ScalableCli()
        cli.silent = {"i-s1"}
        report = installation(SCALABLE, cli).run("restart-service", request_id="r10", timeout=60)
        self.assertEqual(report["state"], "failed")
        self.assertRegex(report["error"], "^Unknown service ''; i-s1 did not answer r10")

    def test_a_silent_host_does_not_hide_a_failure(self):
        cli = ScalableCli()
        cli.silent = {"i-p2"}
        answer = cli.answer
        cli.answer = lambda instance, *rest: ({**answer(instance, *rest), "state": "failed", "error": "boom"}
                                             if instance == "i-w1" else answer(instance, *rest))
        report = installation(SCALABLE, cli).run("deploy-now", request_id="r6", timeout=60)
        self.assertEqual((report["state"], report["instance"]), ("failed", "i-w1"))
        self.assertRegex(report["error"], "^boom; i-p2 did not answer r6")

    def test_no_answer_from_any_host_raises(self):
        cli = ScalableCli()
        cli.silent = {i for instances in cli.groups.values() for i in instances}
        with self.assertRaisesRegex(selfhost.CliError, "^The agent did not answer r7 within 60 seconds"):
            installation(SCALABLE, cli).run("status", request_id="r7", timeout=60)

    def test_an_expired_host_is_the_answer(self):
        cli = ScalableCli()
        answer = cli.answer
        cli.answer = lambda instance, *rest: ({**answer(instance, *rest), "state": "expired", "error": "too late"}
                                             if instance == "i-w1" else answer(instance, *rest))
        report = installation(SCALABLE, cli).run("status", request_id="r8")
        self.assertEqual((report["state"], report["instance"]), ("expired", "i-w1"))

    def test_drain_and_stop_is_refused_and_names_the_group_resize(self):
        # Its groups replace hosts that fail the load balancer's health check,
        # and the new hosts deploy and serve, so a report of `stopped` would be wrong.
        cli = ScalableCli()
        with self.assertRaises(selfhost.CliError) as caught:
            installation(SCALABLE, cli).run("drain-and-stop", request_id="w1")
        self.assertEqual(cli.calls, [], "no host, and nothing else, is touched")
        said = str(caught.exception)
        self.assertRegex(said, "^Not sent: on a scalable AWS installation drain-and-stop would not hold")
        self.assertIn("The app and API groups replace a host", said)
        self.assertRegex(said, "hot-hot-example-scheduler first, then hot-hot-example-worker and "
                               "hot-hot-example-task-worker, then hot-hot-example-api and hot-hot-example-app")
        self.assertIn("update-auto-scaling-group --auto-scaling-group-name GROUP --min-size 0 --desired-capacity 0", said)
        self.assertIn("(the release role may not)", said)
        self.assertIn("protects its hosts from scale-in", said)
        self.assertIn("apply nothing while the stop must hold", said)

    def test_the_stop_says_what_comes_first_when_hosts_return_after_a_restore(self):
        # A host that comes back acts on its pointers, and a release needs a host
        # to report before it moves the role pointers. Only the scheduler's checks
        # the database's level, so it comes back alone.
        for document, cloud, identity, write in (
                (SCALABLE, "aws", "the release role", "aws ssm put-parameter --name /hot/hot-example/release/POINTER "
                                                      "--value VERSION --type String --overwrite"),
                (GCP_SCALABLE, "gcp", "the release account", """printf '{"release": "VERSION"}' | gcloud secrets versions """
                                                             "add hot-NAME-release-POINTER --data-file=-")):
            with self.subTest(cloud):
                cli = ScalableCli(cloud=cloud)
                with self.assertRaises(selfhost.CliError) as caught:
                    installation(document, cli).run("drain-and-stop", request_id="w2")
                self.assertEqual(cli.calls, [])
                said = str(caught.exception)
                self.assertIn("A host that comes back deploys and migrates from its pointers, so after a database "
                              f"restore write all six to the earlier release first, which {identity} may: {write}, for "
                              "each of migrate, task-worker, worker, api, app, scheduler. Then restore only the "
                              "scheduler's size (an apply would restore them all), and run release with that version: "
                              "only that host checks the database's level, and the others would serve whatever it is. "
                              "Once release says the migration has passed, restore the other groups, and it waits for "
                              "them", said)
                # What it names is what a release writes, which that identity may.
                installation(document, cli).point_at("3.0.1", "migrate")
                args = cli.calls[-1]
                self.assertIn("/hot/hot-example/release/migrate" if cloud == "aws" else "hot-hot-example-release-migrate", args)
                for flag in re.findall(r"--\S+", write):
                    self.assertIn(flag, args)

    def test_a_single_machine_still_stops(self):
        for document, cloud in ((AWS, "aws"), (GCP, "gcp")):
            with self.subTest(cloud):
                report = installation(document, FakeCli(cloud, [{"state": "stopped"}])).run("drain-and-stop")
                self.assertEqual(report["state"], "stopped")
                self.assertNotIn("warning", report)

    def test_backup_and_roll_back_are_refused_with_what_to_do_instead(self):
        for operation, instead in (("backup", "back it up with its provider"),
                                   ("roll-back", "fix forward, or restore the database to a point in time")):
            with self.subTest(operation):
                cli = ScalableCli()
                with self.assertRaises(selfhost.CliError) as caught:
                    installation(SCALABLE, cli).run(operation, request_id="w3")
                self.assertEqual(cli.calls, [], "no host, and nothing else, is touched")
                said = str(caught.exception)
                self.assertRegex(said, f"^Not sent: {operation} is for single-machine installations")
                self.assertIn(instead, said)
                self.assertNotIn("reaches only the one host", said, "on AWS a command reaches them all")

    def test_a_service_not_on_the_hosts_is_refused_on_either_cloud(self):
        for document, cloud in ((SCALABLE, "aws"), (GCP_SCALABLE, "gcp")):
            for operation in ("logs", "restart-service"):
                for service in ("postgres", "valkey", "db"):
                    with self.subTest(cloud=cloud, operation=operation, service=service):
                        cli = ScalableCli(cloud=cloud)
                        with self.assertRaises(selfhost.CliError) as caught:
                            installation(document, cli).run(operation, service, request_id="w4")
                        self.assertEqual(cli.calls, [], "no host, and nothing else, is touched")
                        said = str(caught.exception)
                        self.assertRegex(said, f"^Not sent: a scalable installation's hosts do not run {service}\\.")
                        self.assertIn("task-worker, worker, api, app, scheduler, ingress", said)
                        self.assertIn("Its database and Valkey are with their provider", said)

    def test_a_service_passed_with_another_operation_is_not_refused(self):
        # The command workflow passes its service input with any operation.
        cli = ScalableCli()
        report = installation(SCALABLE, cli).run("status", "postgres", request_id="w5")
        self.assertEqual(report["state"], "status")
        self.assertIn("operation=status,service=none,requestId=w5", next(a for a in cli.calls if "send-command" in a)[-3])

    def test_a_single_machine_still_reaches_its_data_services(self):
        for document, cloud in ((AWS, "aws"), (GCP, "gcp")):
            for service in ("postgres", "valkey"):
                with self.subTest(cloud=cloud, service=service):
                    cli = FakeCli(cloud, [{"state": "logs", "service": service, "logs": ["a"]}])
                    self.assertEqual(installation(document, cli).run("logs", service)["logs"], ["a"])

    def test_nothing_goes_out_that_the_command_document_refuses(self):
        # What the example caller offers on any installation, whatever its preset.
        example = (ROOT / "github/example-operator-repo/.github/workflows/command.yml").read_text()
        operations, services = (re.search(rf"(?m)^      {name}:\n(?:        .*\n)*?        options: \[([a-z, -]+)\]",
                                          example).group(1).split(", ") for name in ("operation", "service"))
        self.assertEqual(set(operations), set(selfhost.OPERATIONS))
        self.assertLess(set(DocumentCli.ALLOWED["service"]), set(services))
        sent = set()
        for operation in operations:
            for service in (*services, "", "db"):
                with self.subTest(operation=operation, service=service):
                    cli = DocumentCli()
                    try:
                        report = installation(SCALABLE, cli).run(operation, service, request_id="w6", timeout=60)
                    except selfhost.CliError as refused:
                        self.assertRegex(str(refused), "^Not sent: ", "AWS's raw error names no next step")
                        self.assertEqual(cli.calls, [])
                    else:
                        self.assertTrue(report)
                        sent.add(operation)
        self.assertEqual(sent, set(DocumentCli.ALLOWED["operation"]), "everything else it allows still goes out")

    def test_the_helper_and_the_command_document_agree(self):
        self.assertEqual(set(selfhost.HOST_SERVICES) | {"none"}, set(DocumentCli.ALLOWED["service"]))
        self.assertEqual(set(selfhost.OPERATIONS) - set(DocumentCli.ALLOWED["operation"]),
                         {"backup", "drain-and-stop", "roll-back"})


class GcpScalableCommands(unittest.TestCase):
    """All roles share one subscription, so a command reaches one host."""

    def test_what_one_host_cannot_answer_is_refused_before_sending(self):
        instead = {"drain-and-stop": "resize each group to 0", "restart-service": "Replace the role's instances",
                   "logs": "Cloud Logging", "deploy-now": "recreate the scheduler's instance",
                   "roll-back": "no roll-back", "backup": "its provider"}
        self.assertEqual(set(instead), set(selfhost.ONE_HOST_INSTEAD))
        self.assertLess(set(instead), set(selfhost.OPERATIONS))
        for operation, alternative in instead.items():
            cli = FakeCli("gcp", [{"state": "stopped", "instance": "p0"}])
            with self.assertRaisesRegex(selfhost.CliError, f"reaches only the one host.*{alternative}"):
                installation(GCP_SCALABLE, cli).run(operation, "app")
            self.assertEqual(cli.calls, [], f"{operation} must not reach any host")

    def test_the_refusals_that_touch_instance_groups_say_who_may(self):
        # The release account only views them.
        for operation in ("deploy-now", "restart-service", "drain-and-stop"):
            with self.assertRaisesRegex(selfhost.CliError, r"with an identity that may change instance groups "
                                                           r"\(the release account may not\)"):
                installation(GCP_SCALABLE, FakeCli("gcp", [])).run(operation, "app")

    def test_the_logs_refusal_says_what_happens_with_local_logging(self):
        # Cloud Logging holds no container lines then, and nothing else can reach them.
        with self.assertRaisesRegex(selfhost.CliError, "logging.destination = local.*set it to cloud"):
            installation(GCP_SCALABLE, FakeCli("gcp", [])).run("logs", "app")

    def test_any_host_answers_the_rest_and_its_report_names_it(self):
        for operation in sorted(set(selfhost.OPERATIONS) - set(selfhost.ONE_HOST_INSTEAD)):
            cli = FakeCli("gcp", [{"state": operation, "instance": "p0", "roles": ["app"]}])
            report = installation(GCP_SCALABLE, cli).run(operation)
            self.assertEqual((report["state"], report["instance"]), (operation, "p0"))
            self.assertEqual(json.loads(cli.find("publish")[0][-3])["operation"], operation)

    def test_a_single_machine_and_a_scalable_aws_installation_still_send_them(self):
        for operation in selfhost.ONE_HOST_INSTEAD:
            cli = FakeCli("gcp", [{"state": "done"}])
            self.assertEqual(installation(GCP, cli).run(operation, "app")["state"], "done")
            cli = FakeCli("aws", [{"state": "done"}])
            self.assertEqual(installation(AWS, cli).run(operation, "app")["state"], "done")
            if operation in ("drain-and-stop", "backup", "roll-back"):
                continue  # Refused there too: see test_nothing_goes_out_that_the_command_document_refuses.
            cli = ScalableCli()
            installation(SCALABLE, cli).run(operation, "app", timeout=60)
            self.assertTrue([a for a in cli.calls if "send-command" in a], operation)

    def test_a_release_is_not_a_command(self):
        cli = ScalableCli(cloud="gcp")
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["state"], "deployed")


def released_fleet(cloud):
    """A scalable installation: AWS creates each role's pointer as "none", and
    on GCP it has a version once the role has been released."""
    cli = ScalableCli(cloud=cloud)
    cli.pointers["scheduler"] = "none" if cloud == "aws" else "3.0.1"
    return cli


class WrongPreset(unittest.TestCase):
    """A preset left at single-machine on a scalable installation."""

    REFUSED = "^Not sent: this installation is scalable; set preset to scalable"

    def test_a_command_is_refused_before_it_is_sent(self):
        for document, cloud, sends in ((AWS, "aws", "send-command"), (GCP, "gcp", "publish")):
            for operation in selfhost.OPERATIONS:
                cli = released_fleet(cloud)
                with self.assertRaisesRegex(selfhost.CliError, self.REFUSED):
                    installation(document, cli).run(operation, "worker")
                self.assertFalse([a for a in cli.calls if sends in a], f"{cloud} {operation} must not reach any host")

    def test_a_release_is_refused_before_the_pointer_moves(self):
        for document, cloud, sends in ((AWS, "aws", "send-command"), (GCP, "gcp", "publish")):
            cli = released_fleet(cloud)
            with self.assertRaisesRegex(selfhost.CliError, self.REFUSED):
                installation(document, cli).release("3.0.2")
            self.assertEqual(cli.order(), [])
            self.assertFalse([a for a in cli.calls if sends in a], cloud)

    def test_a_single_machine_whose_role_pointer_cannot_be_read_still_sends(self):
        # Through the real Cli: its release role is denied, and an identity
        # that may read finds no such pointer.
        unreadable = ((AWS, "get-parameter", "send-command", 254,
                       "An error occurred (AccessDeniedException) when calling the GetParameter operation"),
                      (AWS, "get-parameter", "send-command", 254,
                       "An error occurred (ParameterNotFound) when calling the GetParameter operation"),
                      (GCP, "access", "publish", 1, "ERROR: (gcloud.secrets.versions.access) PERMISSION_DENIED"),
                      (GCP, "access", "publish", 1, "ERROR: (gcloud.secrets.versions.access) NOT_FOUND"))
        answer = {"state": "status"}
        for document, reads, sends, code, stderr in unreadable:
            def run(args, **_):
                if reads in args:
                    return subprocess.CompletedProcess(args, code, "", stderr)
                if "filter-log-events" in args:
                    return subprocess.CompletedProcess(args, 0, json.dumps({"events": [{"message": json.dumps(answer)}]}), "")
                if "logging" in args:
                    return subprocess.CompletedProcess(args, 0, json.dumps([{"jsonPayload": answer}]), "")
                return subprocess.CompletedProcess(args, 0, "{}", "")
            with mock.patch.object(selfhost.subprocess, "run", side_effect=run) as called:
                self.assertEqual(installation(document, selfhost.Cli()).run("status")["state"], "status", stderr)
            self.assertTrue([c for c in called.call_args_list if sends in c.args[0]], stderr)

    def test_a_scalable_agents_answer_fails_when_its_pointer_said_nothing(self):
        # A GCP role's pointer has no version until its first release.
        cli = FakeCli("gcp", [{"state": "current", "instance": "p0", "roles": ["app"], "release": "3.0.2"}])
        report = installation(GCP, cli).run("deploy-now", request_id="r1")
        self.assertEqual(report["state"], "failed")
        self.assertRegex(report["error"], "^Sent, but this installation is scalable; set preset to scalable")
        self.assertEqual(report["answers"], {"p0": "current"}, "what the host said survives")
        self.assertEqual(selfhost.markdown(report).count("**Error:**"), 1)

    def test_a_scalable_agents_rejection_stays_in_the_error(self):
        cli = FakeCli("gcp", [{"state": "rejected", "instance": "p0", "roles": ["app"], "error": "Unknown service 'db'"}])
        report = installation(GCP, cli).run("restart-service", "db")
        self.assertEqual(report["state"], "failed")
        self.assertRegex(report["error"], "^Unknown service 'db'; Sent, but this installation is scalable")

    def test_a_release_answered_by_a_scalable_agent_fails(self):
        cli = FakeCli("gcp", [{"state": "current", "instance": "p0", "roles": ["app"], "release": "3.0.2"}])
        report = installation(GCP, cli).release("3.0.2")
        self.assertEqual(report["state"], "failed")
        self.assertIn("this installation is scalable", report["error"])

    def test_the_right_preset_reads_no_extra_pointer(self):
        cli = released_fleet("aws")
        self.assertEqual(installation(SCALABLE, cli).run("status")["state"], "status")
        self.assertFalse([a for a in cli.calls if "get-parameter" in a])


THROTTLED = "An error occurred (ThrottlingException) when calling the FilterLogEvents operation: Rate exceeded"


class Flaky:
    """A fake CLI whose calls containing `word`, or all of several words, fail:
    `times` of them, after letting `after` through, with `error`. `failed`
    counts them."""

    def __init__(self, word, *args, times=1, after=0, error=THROTTLED, **kwargs):
        super().__init__(*args, **kwargs)
        self.words = (word,) if isinstance(word, str) else word
        self.times, self.after, self.failed, self.error = times, after, 0, error

    def __call__(self, *args, stdin=None):
        if all(word in args for word in self.words):
            if self.after:
                self.after -= 1
            elif self.times:
                self.times -= 1
                self.failed += 1
                raise selfhost.CliError(self.error)
        return super().__call__(*args, stdin=stdin)


class FlakyFake(Flaky, FakeCli):
    pass


class FlakyScalable(Flaky, ScalableCli):
    pass


@mock.patch("sys.stderr", new_callable=io.StringIO)
class FailedReads(unittest.TestCase):
    """A scalable release makes about 1,000 CLI calls an hour. One that fails
    says nothing about the release, and the reports wait in the log. A send or
    a pointer write is an action, which must not be taken for done."""

    SINGLE = ((AWS, "filter-log-events", "send-command"), (GCP, "read", "publish"))
    # Each read a scalable wait makes.
    READS = ((SCALABLE, "aws", "filter-log-events"), (SCALABLE, "aws", "describe-auto-scaling-groups"),
             (GCP_SCALABLE, "gcp", "read"), (GCP_SCALABLE, "gcp", "list-instances"),
             (SCALABLE, "aws", "get-parameter"), (GCP_SCALABLE, "gcp", ("secrets", "versions", "access")))
    # What a run reads of a pointer that already says its release.
    POINTER_READS = ((SCALABLE, "aws", "get-parameter", 12), (GCP_SCALABLE, "gcp", ("secrets", "versions", "access"), 6),
                     (GCP_SCALABLE, "gcp", ("secrets", "versions", "describe"), 6))
    # What a poll reads of a GCP host whose failure may be stale.
    DESCRIBE = ("compute", "instances", "describe")
    LAST_READ = f". The last read failed, so an answer may have gone unseen: {THROTTLED}"

    def test_a_command_waits_through_a_failed_read(self, stderr):
        for document, read, _ in self.SINGLE:
            with self.subTest(read):
                cli = FlakyFake(read, document["installation"]["cloud"], [{"state": "status", "request_id": "r1"}], delay=0)
                self.assertEqual(installation(document, cli).run("status", request_id="r1")["state"], "status")
                self.assertEqual(cli.failed, 1)
        self.assertIn(f"warning: a read failed, still waiting: {THROTTLED}", stderr.getvalue())

    def test_a_read_that_keeps_failing_is_named_when_the_wait_ends(self, stderr):
        for document, read, _ in self.SINGLE:
            with self.subTest(read):
                cli = FlakyFake(read, document["installation"]["cloud"], [{"state": "status"}], times=10_000, delay=0)
                with self.assertRaises(selfhost.CliError) as caught:
                    installation(document, cli).run("status", timeout=60)
                self.assertRegex(str(caught.exception), "^The agent did not answer .* within 60 seconds; check .*")
                self.assertTrue(str(caught.exception).endswith(f"check status before sending it again{self.LAST_READ}"), caught.exception)
                self.assertEqual(cli.failed, 7, "until the deadline")
        self.assertEqual(stderr.getvalue().count("warning: a read failed"), 2, "once a wait, however many polls")

    def test_a_good_read_since_clears_the_failure(self, stderr):
        cli = FlakyFake("filter-log-events", "aws", [], delay=10_000)
        with self.assertRaises(selfhost.CliError) as caught:
            installation(AWS, cli).run("status", timeout=60)
        self.assertEqual(cli.failed, 1)
        self.assertNotIn("The last read failed", str(caught.exception), "the log kept what that read would have shown")

    def test_a_failed_send_stops_at_once(self, stderr):
        for document, read, send in self.SINGLE:
            with self.subTest(send):
                cli = FlakyFake(send, document["installation"]["cloud"], [{"state": "status"}], delay=0)
                with self.assertRaisesRegex(selfhost.CliError, "ThrottlingException"):
                    installation(document, cli).run("status")
                self.assertFalse(cli.find(read), "nothing was sent, so nothing can answer")
        self.assertEqual(stderr.getvalue(), "")

    def test_a_failed_listing_before_the_send_sends_nothing(self, stderr):
        cli = FlakyScalable("describe-auto-scaling-groups")
        with self.assertRaisesRegex(selfhost.CliError, "ThrottlingException"):
            installation(SCALABLE, cli).run("status")
        self.assertFalse([a for a in cli.calls if "send-command" in a], "the hosts to wait for are not known")

    def test_a_scalable_command_waits_through_a_failed_read(self, stderr):
        for read in ("filter-log-events", "describe-auto-scaling-groups"):
            with self.subTest(read):
                cli = FlakyScalable(read, after=1 if read != "filter-log-events" else 0)
                report = installation(SCALABLE, cli).run("status")
                self.assertEqual(cli.failed, 1)
                self.assertEqual(sorted(report["answers"]), sorted(i for g in ScalableCli.GROUPS.values() for i in g))

    def test_a_scalable_command_keeps_the_answers_it_read(self, stderr):
        cli = FlakyScalable("filter-log-events", after=1, times=10_000)
        cli.silent.add("i-w1")
        report = installation(SCALABLE, cli).run("status", timeout=60)
        self.assertEqual(report["state"], "failed")
        self.assertEqual(report["answers"]["i-t1"], "status")
        self.assertNotIn("i-w1", report["answers"])
        self.assertIn("i-w1 did not answer", report["error"])
        self.assertTrue(report["error"].endswith(self.LAST_READ), report["error"])

    def test_a_failed_read_does_not_stop_a_roll(self, stderr):
        for document, cloud, read in self.READS:
            for after in range(5):
                with self.subTest(read=read, after=after):
                    cli = FlakyScalable(read, cloud=cloud, after=after)
                    report = installation(document, cli).release("3.0.2")
                    self.assertEqual(report["state"], "deployed", report)
                    self.assertEqual(cli.failed, 1, "the read did fail")
                    self.assertEqual(cli.order(), ["migrate", *selfhost.ROLE_ORDER], "each once")
                    self.assertEqual(set(report["rolled"]), set(selfhost.ROLE_ORDER))

    def test_a_read_that_keeps_failing_ends_the_wait_with_its_error(self, stderr):
        for document, cloud, read in (self.READS[0], self.READS[2]):
            with self.subTest(read):
                cli = FlakyScalable(read, cloud=cloud, times=10_000)
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual((report["state"], report["rolled"]), ("failed", {}), report)
                self.assertRegex(report["error"], "^The migration host did not report within 60 seconds")
                self.assertTrue(report["error"].endswith(self.LAST_READ), report["error"])
                self.assertEqual(cli.order(), ["migrate"])
                cli = FlakyScalable(read, cloud=cloud, times=10_000, after=1)
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual((report["state"], report["role"], report["rolled"]), ("failed", "task-worker", {}), report)
                self.assertEqual(report["error"], "task-worker did not finish within the release's 60 seconds; "
                                                  f"still waiting for its instances{self.LAST_READ}")

    def test_a_sign_in_that_lapses_during_a_wait_does_not_stop_it(self, stderr):
        # Unlike a pointer read before anything moves: the command is out or the
        # release under way, and the log keeps its answers for the next read.
        lapsed = {"aws": "An error occurred (ExpiredTokenException) when calling the FilterLogEvents operation: The "
                         "security token included in the request is expired",
                  "gcp": "ERROR: (gcloud.logging.read) There was a problem refreshing your current auth tokens: "
                         "('invalid_grant: Token has been expired or revoked.')"}
        for document, read, _ in self.SINGLE:
            cloud = document["installation"]["cloud"]
            with self.subTest(command=read):
                cli = FlakyFake(read, cloud, [{"state": "status", "request_id": "r1"}], delay=0, error=lapsed[cloud])
                self.assertEqual(installation(document, cli).run("status", request_id="r1")["state"], "status")
                self.assertEqual(cli.failed, 1)
                self.assertIn(f"warning: a read failed, still waiting: {lapsed[cloud]}", stderr.getvalue())
                cli = FlakyFake(read, cloud, [{"state": "status"}], times=10_000, delay=0, error=lapsed[cloud])
                with self.assertRaisesRegex(selfhost.CliError, "The last read failed, so an answer may have gone unseen"):
                    installation(document, cli).run("status", timeout=60)
                self.assertEqual(cli.failed, 7, "until the deadline")
        for document, cloud, read in self.READS[:4]:
            with self.subTest(release=read):
                cli = FlakyScalable(read, cloud=cloud, error=lapsed[cloud])
                report = installation(document, cli).release("3.0.2")
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(cli.failed, 1, "the read did fail")
                self.assertEqual(cli.order(), ["migrate", *selfhost.ROLE_ORDER], "each once")
        for document, cloud, read in (self.READS[0], self.READS[2]):
            with self.subTest(release_ends=read):
                cli = FlakyScalable(read, cloud=cloud, error=lapsed[cloud], times=10_000)
                before = cli.clock.now
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual((report["state"], report["rolled"]), ("failed", {}), report)
                self.assertTrue(report["error"].endswith(lapsed[cloud]), report["error"])
                self.assertGreaterEqual(cli.clock.now - before, 60, "until the deadline")

    def test_a_timeout_keeps_who_the_last_good_read_was_waiting_for(self, stderr):
        cli = FlakyScalable(("describe-auto-scaling-groups", "hot-hot-example-app"), after=1, times=10_000)
        cli.silent.add("i-p1")
        report = installation(SCALABLE, cli).release("3.0.2", timeout=120)
        self.assertEqual((report["state"], report["role"]), ("failed", "app"), report)
        self.assertEqual(report["error"], f"app did not finish within the release's 120 seconds; still waiting for i-p1{self.LAST_READ}")

    def test_a_timeout_keeps_the_failures_the_last_good_read_set_aside(self, stderr):
        # A GCP release whose app hosts failed and have restarted since. Each
        # poll lists the group, then again per failure, and the second poll
        # fails partway through looking at them.
        cli = FlakyScalable(("list-instances", "hot-hot-example-app"), cloud="gcp", fail_role="app", times=0)
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], "app")
        cli.clock.now += 600
        cli.started.update({"i-p1": cli.clock.now - 60, "i-p2": cli.clock.now - 60})
        cli.after, cli.times = 4, 10_000
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=120)
        self.assertEqual((report["state"], report["role"]), ("failed", "app"), report)
        self.assertIn("still waiting for i-p1, i-p2. i-p1 last reported failed", report["error"])
        self.assertTrue(report["error"].endswith(self.LAST_READ), report["error"])

    def test_a_failed_pointer_write_stops_at_once(self, stderr):
        for document, cloud, write in ((SCALABLE, "aws", "put-parameter"), (GCP_SCALABLE, "gcp", "add")):
            with self.subTest(write):
                cli = FlakyScalable(write, cloud=cloud, after=2)
                with self.assertRaisesRegex(selfhost.CliError, "ThrottlingException"):
                    installation(document, cli).release("3.0.2")
                self.assertEqual(cli.order(), ["migrate", "task-worker"], "a role still to come stays where it is")
        cli = FlakyFake("put-parameter", "aws", [{"state": "deployed", "release": "3.0.2"}], delay=0)
        with self.assertRaisesRegex(selfhost.CliError, "ThrottlingException"):
            installation(AWS, cli).release("3.0.2")
        self.assertFalse(cli.find("send-command"), "the agent would deploy what the pointer still says")

    def test_a_failed_read_of_a_pointer_does_not_write_it_again(self, stderr):
        # A host says nothing new about a release it has acted on, and reports
        # from before a write no longer count: writing it again would time out
        # this run and every later one, though the release is in place.
        for document, cloud, read, reads in self.POINTER_READS:
            for after in range(reads):
                with self.subTest(read=read, after=after):
                    cli = FlakyScalable(read, cloud=cloud, times=0)
                    self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                    cli.clock.now += 3600
                    written, order = dict(cli.written), cli.order()
                    cli.after, cli.times = after, 1
                    report = installation(document, cli).release("3.0.2", timeout=120)
                    self.assertEqual(report["state"], "deployed", report)
                    self.assertEqual(cli.failed, 1, "the read did fail")
                    self.assertEqual((cli.written, cli.order()), (written, order), "no pointer moved")
                    again = installation(document, cli).release("3.0.2", timeout=120)
                    self.assertEqual(again["state"], "deployed", again)

    def test_a_pointer_that_keeps_failing_to_be_read_moves_nothing(self, stderr):
        for document, cloud, read, _ in self.POINTER_READS:
            with self.subTest(read):
                cli = FlakyScalable(read, cloud=cloud, times=10_000)
                cli.pointers["migrate"] = "3.0.2"
                report = installation(document, cli).release("3.0.2", timeout=60)
                self.assertEqual((report["state"], report["rolled"]), ("failed", {}), report)
                self.assertIn("Could not read the migrate pointer in time, so nothing moved", report["error"])
                self.assertIn(THROTTLED, report["error"])
                self.assertEqual(cli.order(), [])

    def test_a_pointer_this_identity_may_not_read_stops_at_once(self, stderr):
        denied = ((SCALABLE, "aws", "get-parameter", "An error occurred (AccessDeniedException) when calling the "
                                                     "GetParameter operation: not authorized"),
                  (GCP_SCALABLE, "gcp", "access", "ERROR: (gcloud.secrets.versions.access) PERMISSION_DENIED: "
                                                  "Permission 'secretmanager.versions.access' denied"))
        for document, cloud, read, error in denied:
            with self.subTest(read):
                cli = FlakyScalable(read, cloud=cloud, after=3, error=error)
                before = cli.clock.now
                with self.assertRaisesRegex(selfhost.CliError, "Denied|DENIED"):
                    installation(document, cli).release("3.0.2")
                self.assertEqual(cli.order(), [], "not taken for a pointer to write, here or for the next role")
                self.assertEqual(cli.clock.now, before)

    def test_a_lapsed_sign_in_stops_at_once(self, stderr):
        # Whichever read of the pointer meets it: nothing brings it back while
        # someone waits, and nothing has moved.
        lapsed = {"aws": ("An error occurred (ExpiredTokenException) when calling the GetParameter operation: The security "
                          "token included in the request is expired",
                          "An error occurred (ExpiredToken) when calling the GetParameter operation: expired",
                          "An error occurred (UnrecognizedClientException) when calling the GetParameter operation: The "
                          "security token included in the request is invalid.",
                          "An error occurred (InvalidClientTokenId) when calling the GetParameter operation: invalid",
                          "An error occurred (InvalidSignatureException) when calling the GetParameter operation: Signature expired",
                          "Unable to locate credentials. You can configure credentials by running \"aws configure\".",
                          "Error when retrieving token from sso: Token has expired and refresh failed",
                          "The SSO session associated with this profile has expired or is otherwise invalid.",
                          "Error loading SSO Token: Token for my-session does not exist",
                          "Your session has expired. Please reauthenticate using 'aws login'.",
                          "aws: [ERROR]: The config profile (nope) could not be found",
                          "aws: [ERROR]: The specified config file (/x) could not be found.",
                          "Error loading login session token: Failed to refresh an access token.",
                          "An unspecified error happened when resolving AWS credentials or refreshing a login session profile.",
                          "Unable to create or refresh login credentials due to insufficient permissions.",
                          "Partial credentials found in env, missing: AWS_SECRET_ACCESS_KEY",
                          "Cannot refresh credentials: MFA token required."),
                  "gcp": ("ERROR: (gcloud.secrets.versions.access) There was a problem refreshing your current auth tokens: "
                          "('invalid_grant: Token has been expired or revoked.')",
                          "ERROR: (gcloud.secrets.versions.access) You do not currently have an active account selected.",
                          "ERROR: (gcloud.secrets.versions.access) Your current active account [a@b.c] does not have any "
                          "valid credentials",
                          "ERROR: (gcloud.secrets.versions.access) Reauthentication failed. cannot prompt during "
                          "non-interactive execution.",
                          "ERROR: (gcloud.secrets.versions.access) UNAUTHENTICATED: Request had invalid authentication credentials.",
                          "ERROR: (gcloud.secrets.versions.access) There was a problem reauthenticating while refreshing "
                          "your current auth tokens: ('invalid_rapt')",
                          "ERROR: (gcloud.secrets.versions.access) Please run:",
                          "ERROR: (gcloud.secrets.versions.access) Failed to load credential file: [/x].  Expecting value",
                          "ERROR: (gcloud.secrets.versions.access) Failed to load credential file: [/x].  File /x was not found.",
                          "ERROR: (gcloud.secrets.versions.access) Failed to impersonate [a@b.c]. Make sure the account "
                          "that's trying to impersonate it has access to the service account itself")}
        for document, cloud, read, _ in self.POINTER_READS:
            for error in lapsed[cloud]:
                with self.subTest(read=read, error=error):
                    cli = FlakyScalable(read, cloud=cloud, error=error, times=0)
                    self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                    cli.clock.now += 3600
                    before, written = cli.clock.now, dict(cli.written)
                    cli.times = 1
                    with self.assertRaises(selfhost.CliError) as caught:
                        installation(document, cli).release("3.0.2", timeout=3600)
                    self.assertIn(error, str(caught.exception))
                    self.assertEqual(cli.clock.now, before, "no wait for what cannot clear")
                    self.assertEqual(cli.written, written, "not taken for a pointer to write")
        self.assertEqual(stderr.getvalue(), "")

    def test_what_may_clear_is_tried_again(self, stderr):
        passing = {"aws": ("An error occurred (ThrottlingException) when calling the GetParameter operation: Rate exceeded",
                           "An error occurred (InternalServerError) when calling the GetParameter operation (reached max "
                           "retries: 2): Internal Server Error",
                           "An error occurred (ServiceUnavailable) when calling the GetParameter operation: 503",
                           "Could not connect to the endpoint URL: \"https://ssm.us-east-1.amazonaws.com/\""),
                   "gcp": ("ERROR: (gcloud.secrets.versions.access) UNAVAILABLE: The service is currently unavailable.",
                           "ERROR: (gcloud.secrets.versions.access) INTERNAL: Internal error encountered.",
                           "ERROR: (gcloud.secrets.versions.access) RESOURCE_EXHAUSTED: Quota exceeded")}
        for document, cloud, read, _ in self.POINTER_READS:
            for error in passing[cloud]:
                with self.subTest(read=read, error=error):
                    cli = FlakyScalable(read, cloud=cloud, error=error, times=0)
                    self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                    cli.clock.now += 3600
                    written = dict(cli.written)
                    cli.times = 1
                    self.assertEqual(installation(document, cli).release("3.0.2", timeout=120)["state"], "deployed")
                    self.assertEqual(cli.failed, 1, "the read did fail")
                    self.assertEqual(cli.written, written)

    def test_a_pointer_never_written_is_written(self, stderr):
        for document, cloud, _, _ in self.POINTER_READS[:2]:
            with self.subTest(cloud):
                cli = ScalableCli(cloud=cloud)
                self.assertEqual(installation(document, cli).release("3.0.2")["state"], "deployed")
                self.assertEqual(cli.order(), ["migrate", *selfhost.ROLE_ORDER])
        self.assertEqual(stderr.getvalue(), "", "not a failed read")

    def recreated_after_a_failed_deploy(self):
        """A GCP release whose workers failed, ten minutes ago, and have started
        since, with one look at those hosts set to fail."""
        cli = FlakyScalable(self.DESCRIBE, cloud="gcp", fail_role="worker", times=0)
        self.assertEqual(installation(GCP_SCALABLE, cli).release("3.0.2")["role"], "worker")
        cli.fail_role = None
        cli.clock.now += 600
        cli.started.update({"i-w1": cli.clock.now - 60, "i-w2": cli.clock.now - 60})
        cli.times = 1
        return cli

    def test_a_failed_look_at_a_recreated_host_does_not_stop_a_roll(self, stderr):
        cli = self.recreated_after_a_failed_deploy()
        for instance in ("i-w1", "i-w2"):
            cli.at(30, lambda instance=instance: cli.log({"state": "deployed", "release": "3.0.2", "instance": instance,
                                                          "roles": ["worker"]}))
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(cli.failed, 1, "the read did fail")
        self.assertEqual(cli.order(), ["migrate", *selfhost.ROLE_ORDER], "the rest of the roles moved")

    def test_a_host_gone_since_its_listing_drops_out_of_the_next_poll(self, stderr):
        cli = self.recreated_after_a_failed_deploy()
        cli.started.pop("i-w2")
        cli.log({"state": "deployed", "release": "3.0.2", "instance": "i-w2", "roles": ["worker"]})
        cli.at(5, lambda: cli.groups["worker"].remove("i-w1"))
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(report["rolled"]["worker"], ["i-w2"])

    def test_a_failed_look_at_a_recreated_scheduler_does_not_stop_the_migration_wait(self, stderr):
        cli = FlakyScalable(self.DESCRIBE, cloud="gcp")
        cli.pointers["migrate"] = "3.0.2"
        cli.log({"state": "migration-failed", "release": "3.0.2", "instance": "i-s1", "roles": ["scheduler"], "error": "old"})
        cli.clock.now += 600
        cli.started["i-s1"] = cli.clock.now - 60
        cli.at(30, lambda: cli.log({"state": "migrated", "release": "3.0.2", "instance": "i-s1", "roles": ["scheduler"]}))
        report = installation(GCP_SCALABLE, cli).release("3.0.2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(cli.failed, 1)

    def test_a_host_that_keeps_failing_to_be_read_ends_the_wait_with_its_error(self, stderr):
        cli = self.recreated_after_a_failed_deploy()
        cli.times = 10_000
        report = installation(GCP_SCALABLE, cli).release("3.0.2", timeout=60)
        self.assertEqual((report["state"], report["role"], sorted(report["rolled"])), ("failed", "worker", ["task-worker"]), report)
        self.assertRegex(report["error"], "^worker did not finish within the release's 60 seconds; still waiting for its instances")
        self.assertTrue(report["error"].endswith(self.LAST_READ), report["error"])
        self.assertEqual(stderr.getvalue().count(f"warning: a read failed, still waiting: {THROTTLED}"), 1)

    def test_a_release_run_again_reads_again_what_a_failed_read_missed(self, stderr):
        # The migration reported long before this run, so only a read from when
        # the pointer was written can see it. Otherwise the release times out.
        for document, cloud, read in (self.READS[0], self.READS[2]):
            with self.subTest(read):
                cli = FlakyScalable(read, cloud=cloud)
                cli.pointers["migrate"] = "3.0.2"
                cli.written["migrate"] = cli.clock.now
                cli.log({"state": "migrated", "release": "3.0.2", "instance": "i-s1", "roles": ["scheduler"]})
                cli.clock.now += 3600
                report = installation(document, cli).release("3.0.2", timeout=120)
                self.assertEqual(report["state"], "deployed", report)
                self.assertEqual(cli.failed, 1)
                self.assertEqual(cli.order(), list(selfhost.ROLE_ORDER), "the pointer stays as it was written")


class RealCli(unittest.TestCase):
    def failing(self, stderr):
        done = subprocess.CompletedProcess(("gcloud",), 1, stdout="", stderr=stderr)
        with mock.patch.object(selfhost.subprocess, "run", return_value=done), self.assertRaises(selfhost.CliError) as caught:
            selfhost.Cli()("gcloud", "secrets")
        return str(caught.exception)

    def test_an_error_is_its_last_line(self):
        self.assertEqual(self.failing("\nAn error occurred (ParameterNotFound) when calling the GetParameter operation: \n"),
                         "An error occurred (ParameterNotFound) when calling the GetParameter operation:")

    def test_a_gcloud_error_is_not_lost_behind_its_details(self):
        # What tells a pointer never written from a read that failed.
        error = "ERROR: (gcloud.secrets.versions.access) NOT_FOUND: Secret [s] not found or has no versions."
        self.assertEqual(self.failing(f"{error}\n- '@type': type.googleapis.com/google.rpc.ErrorInfo\n  reason: SOMETHING\n"), error)

    def test_a_compute_error_keeps_its_reason_below_the_header(self):
        # What a wait's warning and its timeout say about a read that failed.
        for command, reason in (("instances.describe", "Required 'compute.instances.get' permission for "
                                                       "'projects/p/zones/z/instances/i'"),
                                ("instance-groups.managed.list-instances", "The resource 'projects/p/regions/r/"
                                                                           "instanceGroupManagers/g' was not found")):
            header = f"ERROR: (gcloud.compute.{command}) Could not fetch resource:"
            self.assertEqual(self.failing(f"{header}\n - {reason}\n\n"), f"{header} - {reason}")

    def test_every_reason_stays_and_nothing_after_them(self):
        header = "ERROR: (gcloud.compute.instances.describe) Some requests did not succeed:"
        self.assertEqual(self.failing(f"{header}\n - one\n - two\n\nWARNING: later\n - not a reason\n"), f"{header} - one - two")

    def test_a_header_followed_only_by_details_drops_them(self):
        header = "ERROR: (gcloud.secrets.versions.access) NOT_FOUND: Secret [s] not found:"
        self.assertEqual(self.failing(f"{header}\n- '@type': type.googleapis.com/google.rpc.ErrorInfo\n - indented\n"), header)

    def test_the_cut_comes_after_the_reason_joins_its_header(self):
        header = "ERROR: (gcloud.compute.instances.describe) Could not fetch resource:"
        reason = "Required 'compute.instances.get' permission for " + "x" * 300
        said = self.failing(f"{header}\n - {reason}\n")
        self.assertEqual(said, f"{header} - {reason}"[:300])
        self.assertEqual(len(said), 300)

    def test_a_sign_in_needing_a_browser_keeps_its_command_below_a_blank_line(self):
        header = "ERROR: (gcloud.secrets.versions.access) Please run:"
        self.assertEqual(self.failing(f"{header}\n\n  $ gcloud auth login\n\nto complete reauthentication.\n"),
                         f"{header} $ gcloud auth login")
        self.assertEqual(self.failing(f"{header}\n\nsomething else\n\n  $ later\n"), header)

    def test_a_lapsed_sign_in_stops_at_once_as_each_cli_prints_it(self):
        # Through what_failed, which decides what the read's error says.
        gcloud = "ERROR: (gcloud.secrets.versions.access)"
        for document, stderr, said in (
                (SCALABLE, "\nError loading SSO Token: Token for my-session does not exist\n",
                 "Error loading SSO Token: Token for my-session does not exist"),
                (SCALABLE, "\nYour session has expired. Please reauthenticate using 'aws login'.\n",
                 "Your session has expired. Please reauthenticate using 'aws login'."),
                (SCALABLE, "\naws: [ERROR]: The config profile (nope) could not be found\n",
                 "aws: [ERROR]: The config profile (nope) could not be found"),
                (SCALABLE, "\naws: [ERROR]: Partial credentials found in env, missing: AWS_SECRET_ACCESS_KEY\n",
                 "aws: [ERROR]: Partial credentials found in env, missing: AWS_SECRET_ACCESS_KEY"),
                (GCP_SCALABLE, f"{gcloud} Failed to load credential file: [/x].  File /x was not found.\n",
                 f"{gcloud} Failed to load credential file: [/x].  File /x was not found."),
                (GCP_SCALABLE, f"{gcloud} There was a problem reauthenticating while refreshing your current auth tokens: "
                               "('invalid_rapt')\nPlease run:\n\n  $ gcloud auth login\n\nto obtain new credentials.\n\n"
                               "If you have already logged in with a different account, run:\n\n"
                               "  $ gcloud config set account ACCOUNT\n\nto select an already authenticated account.\n",
                 f"{gcloud} There was a problem reauthenticating while refreshing your current auth tokens: ('invalid_rapt')"),
                (GCP_SCALABLE, f"{gcloud} Please run:\n\n  $ gcloud auth login\n\nto complete reauthentication.\n",
                 f"{gcloud} Please run: $ gcloud auth login")):
            with self.subTest(said):
                clock = Clock()
                before = clock.now
                found = selfhost.Installation(document, selfhost.Cli(), clock=clock, sleep=clock.sleep)
                done = subprocess.CompletedProcess(("cli",), 1, stdout="", stderr=stderr)
                with mock.patch.object(selfhost.subprocess, "run", return_value=done) as run, \
                        self.assertRaises(selfhost.CliError) as caught:
                    found.pointed_since("migrate", "3.0.2", before + 3600)
                self.assertEqual(str(caught.exception), said)
                self.assertEqual(run.call_count, 1, "no second try at what cannot clear")
                self.assertEqual(clock.now, before)

    def test_nothing_on_stderr_names_the_command(self):
        self.assertEqual(self.failing(""), "gcloud exited 1")


class Main(unittest.TestCase):
    def run_main(self, argv, reports, document=AWS, cli=None):
        with tempfile.TemporaryDirectory() as tmp:
            tfvars = Path(tmp) / "installation.tfvars.json"
            tfvars.write_text(json.dumps(document))
            summary = Path(tmp) / "summary.md"
            cli = cli or FakeCli(document["installation"]["cloud"], reports, delay=0)
            with mock.patch("sys.stdout", io.StringIO()):
                code = selfhost.main([str(tfvars), "--summary", str(summary), *argv], cli=cli)
            return code, summary.read_text() if summary.exists() else ""

    def test_rejections_exit_nonzero(self):
        code, summary = self.run_main(["command", "restart-service", "--service", "db"],
                                      [{"state": "rejected", "error": "Unknown service 'db'"}])
        self.assertEqual(code, 1)
        self.assertIn("**Error:** Unknown service 'db'", summary)

    def test_a_failed_read_while_waiting_still_reaches_the_summary(self):
        clock = Clock()
        real = selfhost.Installation
        fake_time = lambda document, runner: real(document, runner, clock=clock, sleep=clock.sleep)  # noqa: E731
        cli = FlakyFake("filter-log-events", "aws", [{"state": "status"}], delay=0)
        with mock.patch("sys.stderr", io.StringIO()) as stderr, mock.patch.object(selfhost, "Installation", fake_time):
            code, summary = self.run_main(["command", "status"], [], cli=cli)
        self.assertEqual((code, cli.failed), (0, 1))
        self.assertIn("### Hot: status", summary)
        self.assertIn("warning: a read failed, still waiting", stderr.getvalue())
        cli = FlakyScalable("describe-auto-scaling-groups", after=4)
        with mock.patch("sys.stderr", io.StringIO()), mock.patch.object(selfhost, "Installation", fake_time):
            code, summary = self.run_main(["release", "3.0.2"], [], SCALABLE, cli=cli)
        self.assertEqual((code, cli.failed), (0, 1))
        self.assertIn("### hot-example: deployed", summary)

    def test_a_refused_command_exits_nonzero_and_says_why(self):
        for argv in (["command", "drain-and-stop"], ["logs", "app"]):
            with mock.patch("sys.stderr", io.StringIO()) as stderr:
                code, _ = self.run_main(argv, [{"state": "stopped"}], GCP_SCALABLE)
            self.assertEqual(code, 1)
            self.assertIn("reaches only the one host", stderr.getvalue())
        with mock.patch("sys.stderr", io.StringIO()) as stderr:
            code, _ = self.run_main(["command", "drain-and-stop"], [{"state": "stopped"}], SCALABLE)
        self.assertEqual(code, 1)
        self.assertIn("would not hold", stderr.getvalue())
        self.assertIn("resize each group to 0", stderr.getvalue())

    def test_a_wrong_preset_exits_nonzero_and_says_why(self):
        for argv in (["command", "status"], ["logs", "app"], ["release", "3.0.2"]):
            with mock.patch("sys.stderr", io.StringIO()) as stderr:
                code, _ = self.run_main(argv, [], AWS, cli=released_fleet("aws"))
            self.assertEqual(code, 1)
            self.assertIn("this installation is scalable; set preset to scalable", stderr.getvalue())

    def test_a_scalable_agents_answer_exits_nonzero_and_keeps_the_answer_in_the_summary(self):
        code, summary = self.run_main(["command", "status"], [{"state": "status", "instance": "p0", "roles": ["app"]}], GCP)
        self.assertEqual(code, 1)
        self.assertIn("**Error:** Sent, but this installation is scalable", summary)
        self.assertIn('"p0": "status"', summary)

    def test_silenced_alarms_warn_in_the_terminal_and_the_summary_without_failing(self):
        cli = AlertCli("aws", [{"state": "status"}], delay=0)
        cli.actions_off = {"hot-hot-example-host-down", "hot-hot-example-job-wait"}
        with mock.patch("sys.stderr", io.StringIO()) as stderr:
            code, summary = self.run_main(["command", "status"], [], AWS, cli=cli)
        self.assertEqual(code, 0)
        self.assertIn("warning: Alarm actions are off on 2 of 2 alarms", stderr.getvalue())
        self.assertIn("**Warning:** Alarm actions are off on 2 of 2 alarms", summary)
        self.assertIn('"silenced": [', summary)

    def test_hot_failing_to_start_again_exits_nonzero(self):
        failed = [{"state": "resume-failed", "release": "3.0.2", "error": "did not become ready"}]
        self.assertEqual(self.run_main(["command", "deploy-now"], failed)[0], 1)
        self.assertEqual(self.run_main(["release", "3.0.2"], failed)[0], 1)

    def test_a_command_the_agent_skipped_exits_nonzero(self):
        expired = [{"state": "expired", "command": "drain-and-stop", "error": "The helper stopped waiting"}]
        self.assertEqual(self.run_main(["command", "drain-and-stop"], expired)[0], 1)
        self.assertEqual(self.run_main(["release", "3.0.2"], expired)[0], 1)

    def test_a_silent_host_exits_nonzero_and_keeps_the_answers_in_the_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            tfvars = Path(tmp) / "installation.tfvars.json"
            tfvars.write_text(json.dumps(SCALABLE))
            summary = Path(tmp) / "summary.md"
            cli = ScalableCli()
            cli.silent = {"i-s1"}
            real = selfhost.Installation
            fake_time = lambda document, runner: real(document, runner, clock=cli.clock, sleep=cli.clock.sleep)  # noqa: E731
            with mock.patch("sys.stdout", io.StringIO()), mock.patch.object(selfhost, "Installation", fake_time):
                code = selfhost.main([str(tfvars), "--summary", str(summary), "--timeout", "60", "logs", "worker"], cli=cli)
            self.assertEqual(code, 1)
            self.assertIn("i-w2 line", summary.read_text())
            self.assertIn("i-s1 did not answer", summary.read_text())

    def test_a_release_takes_its_default_wait_from_the_preset(self):
        for document, seconds, method in ((AWS, 1800, "run"), (SCALABLE, 3600, "scalable_release")):
            with mock.patch.object(selfhost.Installation, method, return_value={"state": "deployed", "release": "3.0.2"}) as called:
                self.assertEqual(self.run_main(["release", "3.0.2"], [], document)[0], 0)
            self.assertIn(seconds, [*called.call_args.args, *called.call_args.kwargs.values()])

    def test_logs_are_summarized_in_a_code_block(self):
        code, summary = self.run_main(["logs", "app"], [{"state": "logs", "service": "app", "logs": ["one", "```"]}])
        self.assertEqual(code, 0)
        self.assertIn("Last 2 lines of `app`:\n\n````\none\n```\n````", summary)
        self.assertNotIn('"logs": [', summary)


if __name__ == "__main__":
    unittest.main()
