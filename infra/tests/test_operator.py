"""The operator helper against fake aws and gcloud CLIs."""
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("selfhost", ROOT / "operator/selfhost.py")
selfhost = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selfhost)

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
            return [] if self.delay >= 0 else [{"jsonPayload": r} for r in self.reports]
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
    clock = Clock()
    return selfhost.Installation(document, cli, clock=clock, sleep=clock.sleep)


class Commands(unittest.TestCase):
    def test_aws_sends_through_the_command_document_and_reads_the_log(self):
        cli = FakeCli("aws", [{"state": "diagnostics", "request_id": "r1", "diagnostics": {}}])
        report = installation(AWS, cli).run("diagnostics", request_id="r1")
        self.assertEqual(report["state"], "diagnostics")
        send = cli.find("send-command")[0]
        self.assertIn("hot-hot-example-command", send)
        self.assertIn("operation=diagnostics,service=none,requestId=r1", send)
        self.assertIn("Key=tag:HotInstallation,Values=hot-example", send)
        read = cli.find("filter-log-events")[0]
        self.assertEqual(read[read.index("--log-group-name") + 1], "/hot/hot-example")
        self.assertEqual(read[read.index("--filter-pattern") + 1], '{ $.request_id = "r1" }')
        self.assertEqual(read[read.index("--start-time") + 1], str(int((1_800_000_000 - 120) * 1000)))

    def test_gcp_publishes_to_the_topic_and_reads_the_status_log(self):
        cli = FakeCli("gcp", [{"state": "logs", "service": "api", "request_id": "r2", "logs": ["a"]}])
        report = installation(GCP, cli).run("logs", "api", request_id="r2")
        self.assertEqual(report["logs"], ["a"])
        publish = cli.find("publish")[0]
        self.assertEqual(json.loads(publish[publish.index("--message") + 1]),
                         {"operation": "logs", "service": "api", "request_id": "r2"})
        query = cli.find("read")[0][3]
        self.assertIn('logName="projects/example-project/logs/hot-hot-example-status"', query)
        self.assertIn('jsonPayload.request_id="r2"', query)

    def test_a_silent_agent_times_out_with_a_next_step(self):
        cli = FakeCli("aws", [], delay=1000)
        with self.assertRaisesRegex(selfhost.CliError, "did not answer .* within 60 seconds; check the command"):
            installation(AWS, cli).run("status", timeout=60)

    def test_request_ids_are_generated(self):
        cli = FakeCli("gcp", [{"state": "status"}])
        installation(GCP, cli).run("status")
        message = json.loads(cli.find("publish")[0][-3])
        self.assertRegex(message["request_id"], r"^cli-[0-9a-f]{12}$")


class Releases(unittest.TestCase):
    def test_the_pointer_is_written_before_deploying(self):
        cli = FakeCli("gcp", [{"state": "deployed", "release": "3.0.2"}])
        report = installation(GCP, cli).release("3.0.2", request_id="gha-1-1")
        self.assertEqual(report["state"], "deployed")
        pointer = next(i for i, (args, _) in enumerate(cli.calls) if "versions" in args)
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


class ScalableCli:
    """A scalable AWS installation whose agents answer once their pointer
    changes: the migration host first, then each role's instances."""

    GROUPS = {"task-worker": ["i-t1"], "worker": ["i-w1", "i-w2"], "api": ["i-a1"], "app": ["i-p1", "i-p2"],
              "scheduler": ["i-s1"]}

    def __init__(self, fail_role=None, migration="migrated"):
        self.pointers = {}
        self.events = []
        self.calls = []
        self.fail_role = fail_role
        self.migration = migration

    def __call__(self, *args, stdin=None):
        self.calls.append(args)
        if "put-parameter" in args:
            name, value = args[args.index("--name") + 1], args[args.index("--value") + 1]
            self.pointers[name] = value
            role = name.rsplit("/", 1)[-1]
            if role == "migrate":
                self.events.append({"state": self.migration, "release": value, "instance": "i-s1", "roles": ["scheduler"]})
            else:
                for instance in self.GROUPS[role]:
                    state = "failed" if role == self.fail_role else "deployed"
                    self.events.append({"state": state, "release": value, "instance": instance, "roles": [role]})
            return {}
        if "get-parameter" in args:
            name = args[args.index("--name") + 1]
            if name not in self.pointers:
                raise selfhost.CliError("ParameterNotFound")
            return {"Parameter": {"Value": self.pointers[name]}}
        if "describe-auto-scaling-groups" in args:
            role = args[-1].split("hot-example-", 1)[1]
            return {"AutoScalingGroups": [{"Instances": [{"InstanceId": i, "LifecycleState": "InService"}
                                                         for i in self.GROUPS[role]]}]}
        if "filter-log-events" in args:
            return {"events": [{"message": json.dumps(e)} for e in self.events]}
        return {}

    def order(self):
        return [a[a.index("--name") + 1].rsplit("/", 1)[-1] for a in self.calls if "put-parameter" in a]


SCALABLE = {"installation": dict(AWS["installation"], preset="scalable")}


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
        self.assertIn("deploy-now on the scheduler host", report["error"])
        self.assertEqual(cli.order(), ["migrate"])

    def test_a_failed_role_stops_the_roll(self):
        cli = ScalableCli(fail_role="api")
        report = installation(SCALABLE, cli).release("3.0.2")
        self.assertEqual((report["state"], report["role"]), ("failed", "api"))
        self.assertEqual(cli.order(), ["migrate", "task-worker", "worker", "api"])
        self.assertEqual(set(report["rolled"]), {"task-worker", "worker"})


class Main(unittest.TestCase):
    def run_main(self, argv, reports, document=AWS):
        with tempfile.TemporaryDirectory() as tmp:
            tfvars = Path(tmp) / "installation.tfvars.json"
            tfvars.write_text(json.dumps(document))
            summary = Path(tmp) / "summary.md"
            cli = FakeCli(document["installation"]["cloud"], reports, delay=0)
            with mock.patch("sys.stdout", io.StringIO()):
                code = selfhost.main([str(tfvars), "--summary", str(summary), *argv], cli=cli)
            return code, summary.read_text() if summary.exists() else ""

    def test_rejections_exit_nonzero(self):
        code, summary = self.run_main(["command", "restart-service", "--service", "db"],
                                      [{"state": "rejected", "error": "Unknown service 'db'"}])
        self.assertEqual(code, 1)
        self.assertIn("**Error:** Unknown service 'db'", summary)

    def test_logs_are_summarized_in_a_code_block(self):
        code, summary = self.run_main(["logs", "app"], [{"state": "logs", "service": "app", "logs": ["one", "```"]}])
        self.assertEqual(code, 0)
        self.assertIn("Last 2 lines of `app`:\n\n````\none\n```\n````", summary)
        self.assertNotIn('"logs": [', summary)


if __name__ == "__main__":
    unittest.main()
