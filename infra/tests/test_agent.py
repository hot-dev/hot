"""The host deploy agent, with fake cloud, process, and download layers."""
import datetime
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import urllib.parse

ROOT = Path(__file__).resolve().parents[1]
os.environ["HOT_AGENT_RUNTIME"] = str(ROOT / "runtime")
spec = importlib.util.spec_from_file_location("hot_agent", ROOT / "agent/hot_agent.py")
agent_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent_module)
render = sys.modules["render"]

GENERATED = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/generated"
OPERATOR = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/operator"
TLS = {"TLS_CERTIFICATE": "-----BEGIN CERTIFICATE-----", "TLS_PRIVATE_KEY": "-----BEGIN PRIVATE KEY-----"}


def installation(**overrides):
    config = json.loads((ROOT / "examples/aws-local.json").read_text())["installation"]
    config.update(data_volume_id="vol-0abc", admin_email="admin@example.com",
                  status_reference="arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/status",
                  secret_references={"generated": GENERATED, "operator": OPERATOR})
    config.update(overrides)
    return config


def manifest(release="3.0.1", latest=22, minimum_source="3.0.0"):
    return {"schema_version": 1, "release": release, "module_tag": "v" + release, "configuration_schema": 1,
            "migrations": {"latest": latest}, "upgrade": {"minimum_source": minimum_source},
            "images": {name: f"registry.example.com/{name}@sha256:" + "a" * 64 for name in render.IMAGES}}


class FakeCloud:
    def __init__(self, pointer="3.0.1", operator=None):
        self.parameters = {"pointer": pointer}
        self.secrets = {OPERATOR: dict(TLS, **(operator or {}))}
        self.statuses = []
        self.metrics = []
        self.journal = []
        self.snapshot = None

    def put_metrics(self, installation, metrics):
        self.metrics.append((installation, metrics))

    def ship_journal(self, installation, entries):
        self.journal.append((installation, entries))

    def latest_snapshot(self, volume, installation):
        return self.snapshot

    def get_parameter(self, reference):
        return self.parameters["pointer"]

    def get_secret(self, reference):
        return self.secrets.get(reference)

    def put_secret_if_absent(self, reference, value):
        return self.secrets.setdefault(reference, value)

    def put_status(self, reference, status):
        self.statuses.append(status)

    def receive_commands(self):
        return []


class Recorder:
    def __init__(self):
        self.commands = []
        self.ps = ""
        self.logs = ""
        self.enddate = "notAfter=Jan 30 12:00:00 2027 GMT\n"
        self.journal = {"agent": [], "host": []}
        self.queue = []

    def __call__(self, command, **kwargs):
        self.commands.append(command)
        if command[0] == "journalctl":
            records = self.journal["agent" if "hot-agent.service" in command else "host"]
            if "--after-cursor" in command:
                after = command[command.index("--after-cursor") + 1]
                records = records[next(i for i, r in enumerate(records) if r["__CURSOR"] == after) + 1:]
            return subprocess.CompletedProcess(command, 0, stdout="".join(json.dumps(r) + "\n" for r in records))
        if "queue" in command:
            pending, processing, deadletter = self.queue.pop(0) if len(self.queue) > 1 else self.queue[0]
            return subprocess.CompletedProcess(command, 0, stdout=(
                "Queue Status:\n  Backend: Redis (redis://hot:***@valkey:6379)\n\nSummary:\n"
                f"  Total pending:    {pending}\n  Total processing: {processing}\n  Total deadletter: {deadletter}\n"))
        stdout = (self.ps if "ps" in command else self.logs if "logs" in command
                  else self.enddate if command[0] == "openssl" else "")
        return subprocess.CompletedProcess(command, 0, stdout=stdout, stderr="")

    def compose_calls(self):
        return [" ".join(c[c.index("compose") + 3:]) for c in self.commands if "compose" in c]


class AgentHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        roots = {name: Path(self.tmp.name) / name for name in ("releases", "secrets", "etc", "host")}
        roots["data"] = Path(self.tmp.name)
        self.config = installation()
        self.cloud = FakeCloud()
        self.run = Recorder()
        self.agent = agent_module.Agent(self.config, self.cloud, run=self.run, sleep=lambda _: None, roots=roots,
                                        chown=lambda *args: None)
        self.agent.wait_ready = lambda: None
        self.manifests = {"3.0.1": manifest()}
        patches = [
            mock.patch.object(agent_module, "http", self.fake_http),
            mock.patch.object(render, "verify_manifest_signature", lambda *args: None),
        ]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        self.addCleanup(self.tmp.cleanup)

    def fake_http(self, method, url, body=None, headers=None, timeout=30):
        release = url.split("/v")[-1].split("/")[0]
        if url.endswith("manifest.json") and release in self.manifests:
            return 200, json.dumps(self.manifests[release]).encode()
        if url.endswith("sigstore.json") and release in self.manifests:
            return 200, b"{}"
        return 404, b""



class Deploy(AgentHarness):
    def test_deploys_in_a_safe_order(self):
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.run.compose_calls()
        order = [next(i for i, c in enumerate(calls) if c.startswith(prefix)) for prefix in (
            "stop scheduler", "up -d --wait postgres valkey", "--profile admin run --rm migrate",
            "--profile admin run --rm provision", "--profile admin run --rm bootstrap-admin",
            "up -d --remove-orphans app api worker task-worker ingress", "up -d scheduler")]
        self.assertEqual(order, sorted(order), calls)
        self.assertIsNone(self.agent.reconcile(), "an unchanged pointer does nothing")

    def test_a_blank_data_disk_is_formatted_only_for_a_new_installation(self):
        self.agent.reconcile()
        disk = [c for c in self.run.commands if c[0].endswith("hot-data-disk")]
        self.assertEqual(disk[0][1], "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol0abc")
        self.assertIn("--allow-format", disk[0])
        # The generated secrets now exist, so a later deploy never formats.
        self.cloud.parameters["pointer"] = "3.0.2"
        self.manifests["3.0.2"] = manifest("3.0.2")
        self.agent.reconcile()
        disk = [c for c in self.run.commands if c[0].endswith("hot-data-disk")]
        self.assertNotIn("--allow-format", disk[-1])

    def test_downgrades_and_mismatched_manifests_are_refused(self):
        self.agent.reconcile()
        self.cloud.parameters["pointer"] = "2.9.0"
        self.manifests["2.9.0"] = manifest("2.9.0", latest=12)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("Hot does not migrate down", status["error"])
        self.assertIn("restore the database", status["error"])
        self.cloud.parameters["pointer"] = "3.0.3"
        self.manifests["3.0.3"] = manifest("3.0.4")
        self.assertIn("names release 3.0.4", self.agent.reconcile()["error"])
        self.cloud.parameters["pointer"] = "latest; rm -rf /"
        self.assertIn("is not a release version", self.agent.reconcile()["error"])

    def test_failures_report_the_command_not_its_output(self):
        def failing(command, **kwargs):
            if "migrate" in command:
                raise subprocess.CalledProcessError(1, command, output="postgres://hot_owner:SECRET@")
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
        self.agent.run_command = failing
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertNotIn("SECRET", json.dumps(status))


class Secrets(AgentHarness):
    def test_each_role_receives_only_its_own_files(self):
        self.cloud.secrets[OPERATOR].update(HOT_EMAIL_SMTP_PASSWORD="smtp", HOT_GOOGLE_CLIENT_SECRET="oauth")
        self.agent.reconcile()
        root = Path(self.tmp.name) / "secrets"
        names = {role.name: sorted(p.name for p in role.iterdir()) for role in root.iterdir()}
        self.assertIn("HOT_APP_SESSION_SECRET", names["app"])
        self.assertNotIn("HOT_APP_SESSION_SECRET", names["worker"])
        self.assertNotIn("HOT_ENCRYPTION_KEY", names["scheduler"])
        self.assertIn("HOT_EMAIL_SMTP_PASSWORD", names["worker"])
        self.assertNotIn("HOT_EMAIL_SMTP_PASSWORD", names["app"])
        self.assertIn("HOT_GOOGLE_CLIENT_SECRET", names["app"])
        self.assertEqual(names["migrate"], ["HOT_DB_MIGRATION_URI"])
        self.assertNotIn("HOT_DB_MIGRATION_URI", names["app"])
        mode = (root / "app/HOT_APP_SESSION_SECRET").stat().st_mode & 0o777
        self.assertEqual(mode, 0o400)
        # The rendered bundle mounts what was written.
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertIn("HOT_EMAIL_SMTP_PASSWORD_FILE", compose["services"]["worker"]["environment"])
        self.assertNotIn("HOT_EMAIL_SMTP_PASSWORD_FILE", compose["services"]["app"]["environment"])

    def test_generated_secrets_are_created_once(self):
        self.agent.reconcile()
        first = dict(self.cloud.secrets[GENERATED])
        self.agent.command("restart-service", service="app")
        self.assertEqual(self.cloud.secrets[GENERATED], first)
        self.assertEqual(len(first["HOT_ENCRYPTION_KEY"]), 64)

    def test_operator_bundles_are_checked(self):
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = "postgres://elsewhere"
        self.assertIn("unsupported entry HOT_DB_URI", self.agent.reconcile()["error"])
        del self.cloud.secrets[OPERATOR]["HOT_DB_URI"]
        del self.cloud.secrets[OPERATOR]["TLS_PRIVATE_KEY"]
        self.assertIn("TLS_PRIVATE_KEY", self.agent.reconcile(retry=True)["error"])

    def test_status_never_carries_secret_values(self):
        self.agent.reconcile()
        values = list(self.cloud.secrets[GENERATED].values())
        for status in self.cloud.statuses:
            text = json.dumps(status)
            self.assertFalse(any(value in text for value in values))


class ExistingData(AgentHarness):
    URI = "postgres://hot_runtime:pw@db.internal.example:5432/hot?sslmode=verify-full"
    OWNER = "postgres://hot_owner:pw@db.internal.example:5432/hot?sslmode=verify-full"

    def setUp(self):
        super().setUp()
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR].update(HOT_DB_URI=self.URI, HOT_DB_MIGRATION_URI=self.OWNER,
                                            POSTGRES_CA_CERT="-----BEGIN CERTIFICATE-----")
        self.agent.resolve = lambda hosts: ["10.20.30.40/32"] if hosts else []

    def test_existing_databases_are_checked_then_migrated(self):
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.run.compose_calls()
        self.assertIn("up -d --wait valkey", calls)
        self.assertLess(calls.index("--profile admin run --rm check-postgres"),
                        calls.index("--profile admin run --rm migrate"))
        root = Path(self.tmp.name) / "secrets"
        uri = (root / "app/HOT_DB_URI").read_text()
        self.assertIn("sslrootcert=%2Frun%2Fsecrets%2FPOSTGRES_CA_CERT", uri)
        self.assertTrue((root / "migrate/POSTGRES_CA_CERT").exists())
        self.assertFalse((root / "postgres").exists())
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertIn("10.20.30.40/32", compose["services"]["task-worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"])

    def test_unverified_or_mismatched_connections_are_refused(self):
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("verify-full", "require")
        self.assertIn("sslmode=verify-full", self.agent.reconcile()["error"])
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("hot_runtime", "postgres")
        self.assertIn("runtime_role", self.agent.reconcile(retry=True)["error"])
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI
        self.config["data"]["valkey"] = {"mode": "existing"}
        self.cloud.secrets[OPERATOR]["HOT_REDIS_URI"] = "redis://cache.internal:6379"
        self.assertIn("rediss://", self.agent.reconcile(retry=True)["error"])

    def test_database_settings_are_refused_for_local_data(self):
        self.config["data"] = {}
        self.assertIn("unsupported entry", self.agent.reconcile()["error"])


class UpgradeHarness(AgentHarness):
    def setUp(self):
        super().setUp()
        self.failing = None
        recorder = self.run

        def run(command, **kwargs):
            if self.failing and self.failing in command:
                recorder.commands.append(command)
                raise subprocess.CalledProcessError(1, command)
            if "pg_dump" in command:
                dump = Path(self.tmp.name) / "data/postgres/backup.dump"
                dump.parent.mkdir(parents=True, exist_ok=True)
                dump.write_text("dump")
            return recorder(command, **kwargs)
        self.agent.run_command = run
        self.agent.reconcile()

    def release(self, version, **fields):
        self.manifests[version] = manifest(version, **fields)
        self.cloud.parameters["pointer"] = version


class Upgrades(UpgradeHarness):
    def test_a_failed_release_waits_for_deploy_now(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("Migrations may be partly applied", status["recovery"])
        self.assertIn("restore the database from ", status["recovery"])
        calls = len(self.run.commands)
        self.assertIsNone(self.agent.reconcile(), "a failed release is not retried every poll")
        self.assertEqual(len(self.run.commands), calls)
        point = self.agent.state()["failed"]["restore_point"]
        self.failing = None
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="retry-1")
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.2"))
        self.assertNotIn("failed", self.agent.state())
        self.assertFalse(any("pg_dump" in c for c in self.run.commands), "a retry keeps the first restore point")
        self.assertEqual(self.agent.history()[-1]["restore_point"], point)

    def test_a_failure_before_migrating_keeps_the_previous_scheduler(self):
        self.release("3.0.2")
        self.failing = "--wait"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("The database was not changed and 3.0.1 keeps running", status["recovery"])
        self.assertIn("previous-scheduler-restarted", status["steps"])
        restart = self.run.commands[-1]
        self.assertIn(str(Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json"), restart)
        self.assertEqual(restart[-3:], ["up", "-d", "scheduler"])

    def test_an_interrupted_deploy_resumes(self):
        self.release("3.0.2")
        self.agent.save_state({**self.agent.state(), "deploying": {"release": "3.0.2", "steps": ["started", "pulled"]}})
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        record = self.agent.history()[-1]
        self.assertEqual(record["resumed_after"], "pulled")
        self.assertNotIn("deploying", self.agent.state())

    def test_schema_changes_take_a_restore_point_first(self):
        self.assertNotIn("restore_point", self.agent.history()[-1], "a first install has nothing to restore")
        for number, version in enumerate(["3.0.2", "3.0.3", "3.0.4", "3.0.5"]):
            self.release(version, latest=23 + number)
            self.run.commands.clear()
            self.assertEqual(self.agent.reconcile()["state"], "deployed")
            calls = [" ".join(c) for c in self.run.commands]
            dump = next(i for i, c in enumerate(calls) if "pg_dump" in c)
            self.assertLess(dump, next(i for i, c in enumerate(calls) if c.endswith("run --rm migrate")))
        point = self.agent.history()[-1]["restore_point"]
        self.assertEqual(point["kind"], "dump")
        self.assertTrue(Path(point["path"]).exists())
        self.assertEqual(len(list((Path(self.tmp.name) / "backups").glob("pre-*"))), agent_module.UPGRADE_DUMPS)
        self.release("3.0.6", latest=26)
        self.agent.reconcile()
        self.assertNotIn("restore_point", self.agent.history()[-1], "no schema change, no dump")

    def test_unsupported_jumps_are_refused(self):
        self.release("3.2.0", minimum_source="3.1.0")
        status = self.agent.reconcile()
        self.assertIn("upgrades from 3.1.0 or later, and this installation runs 3.0.1", status["error"])
        self.assertIn("The database was not changed", status["recovery"])
        self.assertLess(agent_module.version_key("3.1.0-rc.1"), agent_module.version_key("3.1.0"))
        self.assertLess(agent_module.version_key("3.9.0"), agent_module.version_key("3.10.0"))

    def test_each_deploy_is_recorded_without_secrets(self):
        (Path(self.tmp.name) / "host").mkdir()
        (Path(self.tmp.name) / "host/versions.json").write_text(json.dumps({"kata": "3.21.0"}))
        self.release("3.0.2", latest=23)
        self.agent.reconcile()
        record = self.agent.history()[-1]
        self.assertEqual({k: record[k] for k in ("release", "previous", "migrations_before", "migrations",
                                                 "configuration_schema")},
                         {"release": "3.0.2", "previous": "3.0.1", "migrations_before": 22, "migrations": 23,
                          "configuration_schema": 1})
        self.assertEqual(record["images"]["hot"], "sha256:aaaaaaaaaaaa")
        self.assertEqual(record["host"], {"kata": "3.21.0"})
        self.assertIn("finished", record)
        text = (Path(self.tmp.name) / "releases/history.json").read_text()
        for value in self.cloud.secrets[GENERATED].values():
            self.assertNotIn(value, text)


class DataMoves(UpgradeHarness):
    ADMIN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef"
    HOST = "hot-example.c0123.us-east-1.rds.amazonaws.com"
    CACHE = "master.hot-example.abc123.use1.cache.amazonaws.com"
    TOKEN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/valkey-AbCdEf"

    def setUp(self):
        super().setUp()
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "p@ss/word"}
        self.cloud.secrets[self.TOKEN] = {"password": "token"}
        self.agent.resolve = lambda hosts: []
        self.run.queue = [(0, 0, 0)]

    def fake_http(self, method, url, body=None, headers=None, timeout=30):
        if url.startswith("https://truststore.pki.rds.amazonaws.com/"):
            return 200, b"-----BEGIN CERTIFICATE-----\nRDS\n-----END CERTIFICATE-----\n"
        return super().fake_http(method, url, body, headers, timeout)

    def move(self, postgres=True, valkey=False):
        data = {}
        if postgres:
            data["postgres"] = {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
                "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}
        if valkey:
            data["valkey"] = {"mode": "managed", "endpoint": {"host": self.CACHE, "port": 6379, "auth_secret": self.TOKEN}}
        self.config["data"] = data
        self.release("3.0.2")
        self.run.commands.clear()
        return self.agent.reconcile()

    def calls(self):
        return [" ".join(c) for c in self.run.commands]

    def test_a_local_database_moves_to_the_managed_one(self):
        status = self.move()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        order = [next(i for i, c in enumerate(calls) if marker in c) for marker in (
            "releases/3.0.1/bundle/compose.json stop scheduler", "releases/3.0.1/bundle/compose.json stop app api ingress",
            "stop --timeout 120 worker task-worker", "exec -T postgres pg_dump", "run --rm prepare-postgres",
            "run --rm check-postgres", "run --rm restore-postgres", "run --rm migrate", "up -d scheduler")]
        self.assertEqual(order, sorted(order), calls)
        self.assertFalse(any("queue status" in c for c in calls), "the queue stays put, so it is not drained")
        record = self.agent.history()[-1]
        self.assertEqual(record["moves"], {"postgres": "local -> managed"})
        self.assertTrue(Path(record["restore_point"]["path"]).exists(), "the dump is kept as the recovery point")
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "managed", "valkey": "local"})
        self.assertFalse((Path(self.tmp.name) / "data/moves/postgres.dump").exists(), "the staged copy is removed")
        self.assertIn("restore_postgres.sh", [p.name for p in (Path(self.tmp.name) / "releases/3.0.2/bundle").iterdir()])

    def test_a_moving_queue_drains_first(self):
        self.run.queue = [(3, 1, 2), (0, 1, 2), (0, 0, 2)]
        status = self.move(postgres=False, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        checks = [i for i, c in enumerate(calls) if "exec -T worker /usr/bin/hot queue status" in c]
        self.assertEqual(len(checks), 3)
        self.assertLess(calls.index(next(c for c in calls if c.endswith("stop app api ingress"))), checks[0])
        self.assertLess(checks[-1], next(i for i, c in enumerate(calls) if "stop --timeout 120 worker task-worker" in c))
        self.assertFalse(any("pg_dump" in c for c in calls), "the database stays put")
        self.assertEqual(self.agent.history()[-1]["deadletter"], 2)

    def test_work_that_will_not_drain_stops_the_move_and_restarts_hot(self):
        self.run.queue = [(5, 0, 0)]
        status = self.move(valkey=True)
        self.assertEqual(status["state"], "failed")
        self.assertIn("did not finish within 15 minutes", status["error"])
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertFalse(any("pg_dump" in c for c in self.calls()))
        self.assertEqual(self.calls()[-1].split("compose.json ")[1], "up -d scheduler")
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "local", "valkey": "local"})

    def test_a_failed_restore_returns_to_the_local_data(self):
        self.failing = "restore-postgres"
        status = self.move()
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("The database was not changed", status["recovery"])
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "local")

    def test_after_the_data_moves_failures_are_fixed_forward(self):
        self.failing = "migrate"
        status = self.move()
        self.assertIn("data now lives in the new database", status["recovery"])
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")
        with self.assertRaisesRegex(agent_module.AgentError, "already moved"):
            self.agent.command("roll-back")

    def test_moving_a_database_back_to_the_host_is_refused(self):
        self.agent.save_state({**self.agent.state(), "data_modes": {"postgres": "managed", "valkey": "local"}})
        self.release("3.0.2")
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("is not automated", status["error"])
        self.assertFalse(any("stop" in c for c in self.calls()), "nothing stops")

    def test_older_installations_are_read_from_the_data_disk(self):
        state = self.agent.state()
        state.pop("data_modes")
        (Path(self.tmp.name) / "data/postgres").mkdir(parents=True, exist_ok=True)
        (Path(self.tmp.name) / "data/postgres/PG_VERSION").write_text("18")
        self.agent.save_state(state)
        self.assertEqual(self.agent.data_moves(self.agent.state()), {})
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.assertEqual(self.agent.data_moves(self.agent.state()), {"postgres": ("local", "existing")})


class RollBack(UpgradeHarness):
    def fail_upgrade(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.run.commands.clear()

    def test_a_failed_upgrade_is_restored_and_the_installed_release_restarted(self):
        self.fail_upgrade()
        dump = self.agent.state()["failed"]["restore_point"]["path"]
        report = self.agent.command("roll-back", request_id="rb-1")
        self.assertEqual((report["state"], report["release"], report["rolled_back_from"]),
                         ("rolled-back", "3.0.1", "3.0.2"))
        calls = self.run.compose_calls()
        restore = next(i for i, c in enumerate(calls) if c.startswith("exec -T postgres pg_restore"))
        self.assertIn("--clean --if-exists --create --exit-on-error /var/lib/postgresql/restore.dump", calls[restore])
        self.assertEqual(calls[0], "stop scheduler")
        self.assertLess(calls.index("stop app api ingress"), restore)
        self.assertLess(restore, calls.index("--profile admin run --rm migrate"))
        self.assertEqual(calls[-1], "up -d scheduler")
        self.assertTrue(all("releases/3.0.1/bundle" in " ".join(c) for c in self.run.commands if "compose" in c))
        self.assertTrue(Path(dump).exists(), "the restore point is kept")
        self.assertFalse((Path(self.tmp.name) / "data/postgres/restore.dump").exists())
        self.assertIsNone(self.agent.reconcile(), "the failed release is still not retried")
        self.assertEqual(self.agent.history()[-1]["rolled_back_from"], "3.0.2")

    def test_a_failure_before_migrating_needs_no_restore(self):
        self.release("3.0.2")
        self.failing = "--wait"
        self.agent.reconcile()
        self.failing = None
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertEqual(report["steps"], ["started"])
        self.assertFalse(any("pg_restore" in c for c in self.run.commands))

    def test_only_a_failed_upgrade_with_a_local_dump_is_restored(self):
        with self.assertRaisesRegex(agent_module.AgentError, "the last deploy did not fail"):
            self.agent.command("roll-back")
        self.fail_upgrade()
        failed = self.agent.state()["failed"]
        failed["restore_point"] = {"kind": "point-in-time", "time": "2026-10-29T12:00:00+00:00"}
        self.agent.save_state({**self.agent.state(), "failed": failed})
        with self.assertRaisesRegex(agent_module.AgentError, "restore it to 2026-10-29T12:00:00.*then deploy 3.0.1"):
            self.agent.command("roll-back")
        self.assertEqual(self.run.commands, [], "nothing is stopped")


def journal_record(number, message, unit="hot-agent.service", priority=6):
    return {"__CURSOR": f"s=1;i={number}", "__REALTIME_TIMESTAMP": str(1_800_000_000_000_000 + number * 1_000_000),
            "_SYSTEMD_UNIT": unit, "PRIORITY": str(priority), "MESSAGE": message}


class HostJournal(AgentHarness):
    def shipped(self):
        return [entry["fields"] for _, entries in self.cloud.journal for entry in entries]

    def test_entries_ship_once_from_saved_cursors(self):
        self.run.journal["agent"] = [journal_record(1, '{"state": "deployed"}')]
        self.run.journal["host"] = [journal_record(1, "Out of memory: Killed process 4242 (hot)", "", 3)]
        self.agent.ship_journal()
        calls = [c for c in self.run.commands if c[0] == "journalctl"]
        self.assertEqual([c[-2:] for c in calls], [["--lines", "100"]] * 2)
        self.assertIn(["--priority", "warning"], [c[3:5] for c in calls])
        self.assertEqual([(f["source"], f["priority"]) for f in self.shipped()], [("agent", 6), ("host", 3)])
        self.run.journal["agent"].append(journal_record(2, "next"))
        self.agent.ship_journal()
        self.assertEqual([f["message"] for f in self.shipped()][-1], "next")
        self.assertEqual(len(self.shipped()), 3, "nothing ships twice")

    def test_messages_are_redacted(self):
        self.agent.reconcile()
        secret = self.cloud.secrets[GENERATED]["VALKEY_PASSWORD"]
        self.run.journal["agent"] = [journal_record(1, f"password {secret} refused"),
                                     journal_record(2, list(b"caf\xe9 bytes"))]
        self.agent.ship_journal()
        messages = [f["message"] for f in self.shipped()]
        self.assertEqual(messages[0], "password [secret] refused")
        self.assertEqual(messages[1], "caf\ufffd bytes")

    def test_a_failed_source_keeps_its_cursor(self):
        self.run.journal["agent"] = [journal_record(1, "a")]
        self.run.journal["host"] = [journal_record(1, "b", "docker.service", 4)]
        shipped = self.cloud.ship_journal

        def flaky(installation, entries):
            if entries[0]["fields"]["source"] == "agent":
                raise agent_module.AgentError("PutLogEvents failed: ThrottlingException")
            shipped(installation, entries)
        self.cloud.ship_journal = flaky
        self.agent.ship_journal()
        cursors = json.loads((Path(self.tmp.name) / "releases/journal.json").read_text())
        self.assertEqual(cursors, {"host": "s=1;i=1"})
        self.cloud.ship_journal = shipped
        self.agent.ship_journal()
        self.assertEqual([f["message"] for f in self.shipped()], ["b", "a"])

    def test_each_cycle_is_bounded(self):
        self.run.journal["agent"] = [journal_record(n, str(n)) for n in range(1, 701)]
        self.agent.ship_journal()
        self.assertEqual(len(self.shipped()), agent_module.JOURNAL_BATCH)
        self.agent.ship_journal()
        self.assertEqual(len(self.shipped()), 700)

    def test_local_logging_keeps_the_journal_on_the_host(self):
        self.config["logging"] = {"destination": "local"}
        self.agent.ship_journal()
        self.assertFalse(any(c[0] == "journalctl" for c in self.run.commands))

    def test_batches_fit_cloudwatch(self):
        day = 24 * 3600 * 1000
        events = [(0, "a"), (1000, "b"), (day, "c"), (day + 1, "d" * 600)]
        self.assertEqual([[m for _, m in b] for b in agent_module.log_batches(events, 10_000, 23 * 3600 * 1000)],
                         [["a", "b"], ["c", "d" * 600]])
        self.assertEqual(len(list(agent_module.log_batches(events, 700))), 2)

    def test_cloud_logging_entries_carry_severity_and_the_instance(self):
        sent = []
        gcp = agent_module.Gcp("example-project", zone="us-central1-a")
        gcp.instance_id = "1234567890"
        gcp.call = lambda method, url, payload=None, allow=(): sent.append(payload)
        entry = agent_module.journal_entry(journal_record(1, "oom", "", 3), agent_module.Redactor(), "host")
        gcp.ship_journal("hot-example", [entry])
        self.assertEqual(sent[0]["logName"], "projects/example-project/logs/hot-hot-example-host")
        self.assertTrue(sent[0]["partialSuccess"])
        self.assertEqual(sent[0]["entries"][0]["severity"], "ERROR")
        self.assertEqual(sent[0]["resource"]["labels"]["instance_id"], "1234567890")


class Readiness(unittest.TestCase):
    def test_an_unreachable_ingress_is_a_connection_error(self):
        # The agent's http() helper must not shadow the http.client module.
        with mock.patch.object(agent_module.socket, "create_connection", side_effect=ConnectionRefusedError):
            with self.assertRaises(OSError):
                agent_module.https_status("app.hot.internal", "/signin")


class ManagedData(AgentHarness):
    ADMIN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef"
    HOST = "hot-example.c0123.us-east-1.rds.amazonaws.com"

    def setUp(self):
        super().setUp()
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "p@ss/word"}
        self.agent.resolve = lambda hosts: ["10.68.4.10/32"] if hosts == [self.HOST] else []
        self.ca_requests, self.ca_status = [], 200

    def fake_http(self, method, url, body=None, headers=None, timeout=30):
        if url.startswith("https://truststore.pki.rds.amazonaws.com/"):
            self.ca_requests.append(url)
            return self.ca_status, b"-----BEGIN CERTIFICATE-----\nRDS\n-----END CERTIFICATE-----\n"
        return super().fake_http(method, url, body, headers, timeout)

    def test_the_database_is_prepared_checked_then_migrated(self):
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.run.compose_calls()
        order = [calls.index(f"--profile admin run --rm {job}") for job in ("prepare-postgres", "check-postgres", "migrate")]
        self.assertEqual(order, sorted(order))
        self.assertNotIn("up -d --wait postgres", " ".join(calls))
        self.assertEqual(self.ca_requests, ["https://truststore.pki.rds.amazonaws.com/us-east-1/us-east-1-bundle.pem"])
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertNotIn("postgres", compose["services"])
        self.assertIn("10.68.4.10/32", compose["services"]["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"])
        self.assertTrue((Path(self.tmp.name) / "releases/3.0.1/bundle/prepare_postgres.sql").exists())

    def test_only_the_prepare_job_gets_the_administrator(self):
        self.agent.reconcile()
        root = Path(self.tmp.name) / "secrets"
        admin = (root / "prepare-postgres/POSTGRES_ADMIN_URI").read_text()
        self.assertTrue(admin.startswith("postgres://hot_admin:p%40ss%2Fword@" + self.HOST + ":5432/postgres?"))
        self.assertIn("/hot?", (root / "prepare-postgres/POSTGRES_ADMIN_HOT_URI").read_text())
        runtime = (root / "app/HOT_DB_URI").read_text()
        self.assertTrue(runtime.startswith("postgres://hot_runtime:"), runtime)
        self.assertIn("sslmode=verify-full", runtime)
        self.assertIn("RDS", (root / "app/POSTGRES_CA_CERT").read_text())
        for path in root.rglob("*"):
            if path.is_file() and path.parent.name != "prepare-postgres":
                self.assertNotIn("p@ss/word", path.read_text())
                self.assertNotIn("p%40ss%2Fword", path.read_text())

    def test_cloud_sql_verifies_its_own_ca(self):
        self.config.update(cloud="gcp", project="example-project")
        self.config["data"]["postgres"]["endpoint"] = {
            "host": "10.100.0.3", "port": 5432, "admin_user": "postgres",
            "admin_secret": "projects/example-project/secrets/hot-example-postgres-admin",
            "ca_certificate": "-----BEGIN CERTIFICATE-----\nSQL\n-----END CERTIFICATE-----\n"}
        self.cloud.secrets["projects/example-project/secrets/hot-example-postgres-admin"] = {"username": "postgres", "password": "x"}
        generated = {k: "g" for k in ("POSTGRES_PASSWORD", "POSTGRES_RUNTIME_PASSWORD", "VALKEY_PASSWORD",
                                     "HOT_ENCRYPTION_KEY", "HOT_APP_SESSION_SECRET", "ADMIN_PASSWORD")}
        files = agent_module.role_secrets(self.config, generated, self.cloud.secrets[OPERATOR], self.agent.managed_data())
        self.assertIn("sslmode=verify-ca", files["worker"]["HOT_DB_URI"])
        self.assertIn("SQL", files["worker"]["POSTGRES_CA_CERT"])
        self.assertEqual(self.ca_requests, [])

    def test_a_missing_administrator_or_ca_stops_the_deploy(self):
        del self.cloud.secrets[self.ADMIN]
        self.assertIn("administrator secret", self.agent.reconcile()["error"])
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "x"}
        self.ca_status = 503
        self.assertIn("CA certificate", self.agent.reconcile(retry=True)["error"])

    def test_operator_database_settings_are_refused(self):
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = "postgres://x"
        self.assertIn("unsupported entry", self.agent.reconcile()["error"])

    def test_managed_valkey_uses_tls_and_its_token(self):
        cache = "master.hot-example.abc123.use1.cache.amazonaws.com"
        token_secret = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/valkey-AbCdEf"
        self.config["data"]["valkey"] = {"mode": "managed", "endpoint": {"host": cache, "port": 6379, "auth_secret": token_secret}}
        self.assertIn("AUTH token", self.agent.reconcile()["error"])
        self.cloud.secrets[token_secret] = {"password": "tok+en/1"}
        self.agent.resolve = lambda hosts: [f"10.68.5.{i}/32" for i, _ in enumerate(hosts)]
        status = self.agent.reconcile(retry=True)
        self.assertEqual(status["state"], "deployed", status)
        root = Path(self.tmp.name) / "secrets"
        self.assertEqual((root / "worker/HOT_REDIS_URI").read_text(), f"rediss://:tok%2Ben%2F1@{cache}:6379")
        self.assertFalse((root / "valkey").exists())
        calls = self.run.compose_calls()
        self.assertFalse(any(call.startswith("up -d --wait") for call in calls), calls)
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertNotIn("valkey", compose["services"])
        self.assertIn("10.68.5.1/32", compose["services"]["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"])


class Memorystore(AgentHarness):
    CA = "-----BEGIN CERTIFICATE-----\nMEMORYSTORE\n-----END CERTIFICATE-----\n"

    def test_hot_connects_with_iam_tokens_and_the_instance_ca(self):
        config = installation(cloud="gcp", project="example-project", zone="us-central1-a", region="us-central1")
        config["data"] = {"valkey": {"mode": "managed", "endpoint": {
            "host": "10.68.0.40", "port": 6379, "auth": "gcp-iam", "ca_certificate": self.CA}}}
        files = agent_module.role_secrets(config, self.cloud.put_secret_if_absent(GENERATED, agent_module.generate_secrets()),
                                          dict(TLS), {})
        for role in agent_module.HOT_SERVICES:
            self.assertEqual(files[role]["HOT_REDIS_URI"], "rediss://10.68.0.40:6379", "no password in the URI")
            self.assertEqual(files[role]["HOT_REDIS_CA_CERT"], self.CA)
        self.assertNotIn("valkey", files)
        self.assertEqual(agent_module.data_services(config, {}), [("10.68.0.40", 6379)])


class Monitoring(AgentHarness):
    NOW = datetime.datetime(2026, 10, 29, 12, 0, tzinfo=datetime.timezone.utc)

    def ps(self, **overrides):
        services = {name: ("running", "") for name in (*agent_module.HOT_SERVICES, "ingress")}
        services.update(postgres=("running", "healthy"), valkey=("running", "healthy"))
        services.update(overrides)
        self.run.ps = "\n".join(json.dumps({"Service": name, "State": state, "Health": health})
                                 for name, (state, health) in services.items())

    def test_before_the_first_deploy_only_the_host_is_reported(self):
        metrics = self.agent.observe(now=self.NOW)
        self.assertEqual(set(metrics), {"AgentHeartbeat", "DataDiskUsedPercent", "BackupAgeHours"})
        self.assertEqual(metrics["BackupAgeHours"][0], 0)
        later = self.agent.observe(now=self.NOW + datetime.timedelta(hours=31))
        self.assertEqual(later["BackupAgeHours"][0], 31, "without a snapshot, age counts from when monitoring began")

    def test_a_healthy_deployment(self):
        self.agent.reconcile()
        self.ps()
        self.cloud.snapshot = self.NOW - datetime.timedelta(hours=5)
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual({k: v[0] for k, v in metrics.items() if k != "DataDiskUsedPercent"}, {
            "AgentHeartbeat": 1, "UnhealthyServices": 0, "SchedulerRunning": 1, "DependenciesReachable": 1,
            "Ready": 1, "BackupAgeHours": 5, "CertificateDaysLeft": 93})
        self.assertTrue(0 <= metrics["DataDiskUsedPercent"][0] <= 100)

    def test_failures_show_up_in_the_metrics(self):
        self.agent.reconcile()
        self.ps(scheduler=("restarting", ""), postgres=("running", "unhealthy"), worker=("exited", ""))
        metrics = self.agent.observe(probe=lambda host, path: 502, now=self.NOW)
        self.assertEqual(metrics["UnhealthyServices"][0], 3)
        self.assertEqual(metrics["SchedulerRunning"][0], 0)
        self.assertEqual(metrics["DependenciesReachable"][0], 0)
        self.assertEqual(metrics["Ready"][0], 0)

    def test_an_unknown_backup_status_is_left_out(self):
        def denied(volume, installation):
            raise agent_module.AgentError("ec2 DescribeSnapshots failed: UnauthorizedOperation")
        self.cloud.latest_snapshot = denied
        metrics = self.agent.observe(now=self.NOW)
        self.assertNotIn("BackupAgeHours", metrics)
        self.assertEqual(metrics["AgentHeartbeat"][0], 1)

    def test_publishing_failures_never_stop_the_agent(self):
        def broken(installation, metrics):
            raise agent_module.AgentError("PutMetricData failed: Throttling")
        self.cloud.put_metrics = broken
        self.agent.monitor()
        self.cloud.put_metrics = FakeCloud.put_metrics.__get__(self.cloud)
        self.agent.monitor()
        self.assertEqual(self.cloud.metrics[-1][0], "hot-example")

    def test_cloudwatch_and_ec2_use_signed_query_requests(self):
        requests = []

        def fake(method, url, body=None, headers=None, timeout=30):
            requests.append((url, urllib.parse.parse_qs(body.decode()), headers))
            if "ec2." in url:
                return 200, (b'<DescribeSnapshotsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><snapshotSet>'
                             b"<item><startTime>2026-10-28T03:00:12.000Z</startTime></item>"
                             b"<item><startTime>2026-10-29T03:00:09.000Z</startTime></item></snapshotSet></DescribeSnapshotsResponse>")
            return 200, b"<PutMetricDataResponse/>"
        aws = agent_module.Aws("us-east-1")
        aws.credentials = lambda: {"AccessKeyId": "AKID", "SecretAccessKey": "secret", "Token": "t"}
        with mock.patch.object(agent_module, "http", fake):
            aws.put_metrics("hot-example", {"Ready": (1, "Count"), "DataDiskUsedPercent": (42.5, "Percent")})
            latest = aws.latest_snapshot("vol-0abc", "hot-example")
        url, params, headers = requests[0]
        self.assertEqual(url, "https://monitoring.us-east-1.amazonaws.com/")
        self.assertEqual(params["Action"], ["PutMetricData"])
        self.assertEqual(params["Namespace"], ["Hot/SelfHost"])
        self.assertEqual(params["MetricData.member.2.MetricName"], ["Ready"])
        self.assertEqual(params["MetricData.member.1.Dimensions.member.1.Value"], ["hot-example"])
        self.assertNotIn("x-amz-target", headers)
        self.assertEqual(requests[1][1]["Filter.1.Value.1"], ["vol-0abc"])
        self.assertEqual(latest, datetime.datetime(2026, 10, 29, 3, 0, 9, tzinfo=datetime.timezone.utc))

    def test_cloud_monitoring_series_carry_the_instance(self):
        sent = []
        gcp = agent_module.Gcp("example-project", zone="us-central1-a")
        gcp.instance_id = "1234567890"
        gcp.call = lambda method, url, payload=None, allow=(): sent.append((url, payload))
        gcp.put_metrics("hot-example", {"AgentHeartbeat": (1, "Count")})
        url, payload = sent[0]
        self.assertTrue(url.endswith("/projects/example-project/timeSeries"))
        series = payload["timeSeries"][0]
        self.assertEqual(series["metric"], {"type": "custom.googleapis.com/hot/hot-example/agent_heartbeat"})
        self.assertEqual(series["resource"]["labels"], {"project_id": "example-project", "zone": "us-central1-a",
                                                        "instance_id": "1234567890"})

    def test_a_database_off_the_host_is_not_dumped(self):
        self.agent.reconcile()
        self.config["data"] = {"postgres": {"mode": "existing"}}
        with self.assertRaisesRegex(agent_module.AgentError, "back it up with its provider"):
            self.agent.command("backup")


class Commands(AgentHarness):
    def test_only_allowlisted_commands_and_services(self):
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown command"):
            self.agent.command("sh -c id")
        self.agent.reconcile()
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("restart-service", service="postgres; id")

    def test_scheduler_restarts_stop_before_start(self):
        self.agent.reconcile()
        self.run.commands.clear()
        self.agent.command("restart-service", service="scheduler")
        self.assertEqual(self.run.compose_calls(), ["stop scheduler", "up -d --force-recreate scheduler"])

    def test_drain_stops_the_scheduler_first(self):
        self.agent.reconcile()
        self.run.commands.clear()
        self.agent.command("drain-and-stop")
        self.assertEqual(self.run.compose_calls()[0], "stop scheduler")


class Diagnostics(AgentHarness):
    def setUp(self):
        super().setUp()
        self.agent.reconcile()
        self.run.ps = "\n".join(json.dumps({
            "Service": name, "State": "running", "Health": "", "Status": "Up 2 hours", "ExitCode": 0,
            "Image": "registry.example.com/hot@sha256:" + "a" * 64}) for name in (*agent_module.HOT_SERVICES, "ingress"))
        (Path(self.tmp.name) / "host").mkdir()
        (Path(self.tmp.name) / "host/versions.json").write_text(json.dumps({"kata": "3.21.0", "image": "hot-host-3.0.1"}))
        self.generated = self.cloud.secrets[GENERATED]

    def assert_no_secrets(self, report):
        text = json.dumps(report)
        for value in [*self.generated.values(), *TLS.values()]:
            self.assertNotIn(value, text)

    def test_the_bundle_describes_the_installation_without_secrets(self):
        self.config["public_certificate_arn"] = "arn:aws:acm:us-east-1:123456789012:certificate/0123"
        report = self.agent.command("diagnostics", request_id="req-1")
        bundle = report["diagnostics"]
        self.assertEqual(report["request_id"], "req-1")
        self.assertEqual(bundle["versions"]["release"], "3.0.1")
        self.assertEqual(bundle["versions"]["host"]["kata"], "3.21.0")
        self.assertEqual(bundle["services"]["app"]["image"], "sha256:aaaaaaaaaaaa")
        self.assertEqual(bundle["configuration"]["secret_references"], "[reference]")
        self.assertEqual(bundle["configuration"]["admin_email"], "[email]@example.com")
        self.assertEqual(bundle["configuration"]["status_reference"], "[reference]")
        self.assertEqual(bundle["configuration"]["public_certificate_arn"], "arn:aws:acm:us-east-1:****9012:certificate/0123")
        self.assertEqual(bundle["configuration"]["app_hostname"], self.config["app_hostname"])
        self.assertEqual(bundle["environment"]["app"]["HOT_ENV"], "production")
        self.assertGreater(bundle["health"]["CertificateDaysLeft"], 0)
        self.assertEqual([d["mode"] for d in bundle["dependencies"]], ["local", "local"])
        self.assert_no_secrets(report)

    def test_the_bundle_works_before_the_first_deploy(self):
        self.agent.save_state({})
        bundle = self.agent.command("diagnostics")["diagnostics"]
        self.assertNotIn("services", bundle)
        self.assertEqual(bundle["health"]["AgentHeartbeat"], 1)

    def test_logs_are_redacted_and_bounded(self):
        secret = self.generated["HOT_APP_SESSION_SECRET"]
        noisy = [f"app-1  | 2026-10-29T12:00:00Z line {i} " + "x" * 400 for i in range(400)]
        self.run.logs = "\n".join(noisy + [
            f"app-1  | session {secret}",
            "app-1  | connecting to postgres://hot_runtime:hunter2hunter2@db.internal:5432/hot",
            "app-1  | Authorization: Bearer abc.def.ghi from ada@example.com",
            'app-1  | {"api_key":"hot_0123456789abcdef0123456789abcdef_abcdef","password":"pw"}',
        ])
        report = self.agent.command("logs", service="app")
        lines = report["logs"]
        self.assertEqual(self.run.compose_calls()[-1], "logs --no-color --timestamps --tail 200 app")
        self.assertLessEqual(len(lines), agent_module.LOG_LINES)
        self.assertLessEqual(sum(len(line) for line in lines), agent_module.LOG_CHARS)
        tail = "\n".join(lines[-4:])
        for leaked in (secret, "hunter2hunter2", "abc.def.ghi", "ada@", "0123456789abcdef", '"pw"'):
            self.assertNotIn(leaked, tail)
        self.assertIn("postgres://[credentials]@db.internal", tail)

    def test_logs_only_for_known_services(self):
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("logs", service="app; id")
        self.config["data"] = {"postgres": {"mode": "existing"}}
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("logs", service="postgres")
        self.agent.command("logs", service="valkey")

    def test_an_unreadable_certificate_alarms(self):
        self.run.enddate = "garbage"
        self.assertEqual(self.agent.observe(probe=lambda host, path: 200)["CertificateDaysLeft"][0], 0)


class Reports(AgentHarness):
    def test_deploys_answer_their_request(self):
        report = self.agent.command("deploy-now", request_id="gha-1-1")
        self.assertEqual((report["state"], report["request_id"]), ("deployed", "gha-1-1"))
        report = self.agent.command("deploy-now", service="none", request_id="gha-2-1")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("current", "3.0.1", "gha-2-1"))

    def test_aws_logs_the_full_report_and_keeps_a_summary(self):
        calls = []
        streams = set()

        def call(service, target, payload):
            calls.append((target, payload))
            if target.endswith("PutLogEvents") and payload["logStreamName"] not in streams:
                raise agent_module.AgentError(f"{target} failed: ResourceNotFoundException")
            if target.endswith("CreateLogStream"):
                streams.add(payload["logStreamName"])
            return {}
        aws = agent_module.Aws("us-east-1", "/hot/hot-example")
        aws.call = call
        aws.put_status("arn:aws:ssm:us-east-1:1:parameter/hot/hot-example/status",
                       {"state": "logs", "request_id": "r1", "logs": ["line"] * 500})
        targets = [target for target, _ in calls]
        self.assertEqual(targets, ["Logs_20140328.PutLogEvents", "Logs_20140328.CreateLogStream",
                                   "Logs_20140328.PutLogEvents", "AmazonSSM.PutParameter"])
        logged = json.loads(calls[2][1]["logEvents"][0]["message"])
        self.assertEqual(len(logged["logs"]), 500)
        self.assertEqual(calls[2][1]["logGroupName"], "/hot/hot-example")
        summary = json.loads(calls[3][1]["Value"])
        self.assertEqual(summary, {"state": "logs", "request_id": "r1"})


class Redaction(unittest.TestCase):
    def test_known_values_and_credential_shapes(self):
        redact = agent_module.Redactor(["s3cr3t-value-0001", "rediss://:t%2Fok3n-value@cache:6379"])
        cases = {
            "x s3cr3t-value-0001 y": "x [secret] y",
            "token t/ok3n-value": "token [secret]",
            "postgres://u:p@h/db": "postgres://[credentials]@h/db",
            "AKIAABCDEFGHIJKLMNOP": "[access key]",
            "SMTP_PASSWORD=abc HOT_DB_URI_FILE=/run/secrets/HOT_DB_URI":
                "SMTP_PASSWORD=[redacted] HOT_DB_URI_FILE=/run/secrets/HOT_DB_URI",
            "arn:aws:iam::123456789012:role/x": "arn:aws:iam::****9012:role/x",
            "-----BEGIN RSA PRIVATE KEY-----\nabc": "[private key]",
        }
        for text, expected in cases.items():
            self.assertEqual(redact(text), expected, text)

    def test_short_values_are_left_to_the_patterns(self):
        self.assertEqual(agent_module.Redactor(["abc"]).values, [])


class AwsSigning(unittest.TestCase):
    def test_query_signature_matches_botocore(self):
        body = b"Action=DescribeSnapshots&Version=2016-11-15&Owner.1=self&Filter.1.Name=volume-id&Filter.1.Value.1=vol-0abc"
        headers = agent_module.sigv4_headers(
            "POST", "ec2.us-east-2.amazonaws.com", "us-east-2", "ec2", None, body,
            {"AccessKeyId": "AKIDEXAMPLE", "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
             "Token": "SESSIONTOKEN"},
            datetime.datetime(2026, 10, 29, 12, 0, 0, tzinfo=datetime.timezone.utc),
            content_type="application/x-www-form-urlencoded; charset=utf-8")
        # Reference value computed with botocore 1.40.0 for the same inputs.
        self.assertTrue(headers["authorization"].endswith(
            "Signature=23bcc4832d3d58fc357b9af748806e53fe9a62ade885712e16be6c27276228ad"))

    def test_signature_matches_botocore(self):
        body = json.dumps({"Name": "/hot/demo/release"}).encode()
        headers = agent_module.sigv4_headers(
            "POST", "ssm.us-east-2.amazonaws.com", "us-east-2", "ssm", "AmazonSSM.GetParameter", body,
            {"AccessKeyId": "AKIDEXAMPLE", "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
             "Token": "SESSIONTOKEN"},
            datetime.datetime(2026, 10, 29, 12, 0, 0, tzinfo=datetime.timezone.utc))
        # Reference value computed with botocore 1.40.0 for the same inputs.
        self.assertTrue(headers["authorization"].endswith(
            "Signature=b821ec55cf27ef65907f92a710b8615e0db0fc1c1a2c9fa040d78d353e3ff00b"))

    def test_parameter_arns_become_names(self):
        self.assertEqual(agent_module.Aws.parameter_name("arn:aws:ssm:us-east-1:1:parameter/hot/x/release"),
                         "/hot/x/release")


if __name__ == "__main__":
    unittest.main()
