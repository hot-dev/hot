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


def manifest(release="3.0.1", latest=22):
    return {"schema_version": 1, "release": release, "module_tag": "v" + release, "configuration_schema": 1,
            "migrations": {"latest": latest},
            "images": {name: f"registry.example.com/{name}@sha256:" + "a" * 64 for name in render.IMAGES}}


class FakeCloud:
    def __init__(self, pointer="3.0.1", operator=None):
        self.parameters = {"pointer": pointer}
        self.secrets = {OPERATOR: dict(TLS, **(operator or {}))}
        self.statuses = []

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

    def __call__(self, command, **kwargs):
        self.commands.append(command)
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

    def compose_calls(self):
        return [" ".join(c[c.index("compose") + 3:]) for c in self.commands if "compose" in c]


class AgentHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        roots = {name: Path(self.tmp.name) / name for name in ("releases", "secrets", "etc")}
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
        self.assertIn("downgrades need a restore", status["error"])
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
        self.assertIn("TLS_PRIVATE_KEY", self.agent.reconcile()["error"])

    def test_status_never_carries_secret_values(self):
        self.agent.reconcile()
        values = list(self.cloud.secrets[GENERATED].values())
        for status in self.cloud.statuses:
            text = json.dumps(status)
            self.assertFalse(any(value in text for value in values))


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


class AwsSigning(unittest.TestCase):
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
