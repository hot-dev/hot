"""The host deploy agent, with fake cloud, process, and download layers."""
import base64
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import urllib.error
import urllib.parse
import xml.etree.ElementTree as ElementTree

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
        return self.parameters.get(reference, self.parameters["pointer"])

    def get_secret(self, reference):
        return self.secrets.get(reference)

    def put_secret_if_absent(self, reference, value):
        return self.secrets.setdefault(reference, value)

    def put_status(self, reference, status):
        self.statuses.append(status)

    def receive_commands(self):
        return []


def interfaces(*own):
    """`ip -json -4 address show scope global` on a host with these addresses
    of its own, and the bridges every host has."""
    bridges = {"br-0a1b2c3d4e5f": "172.30.0.1", "kata-br0": "10.88.0.1"}
    links = [{"ifname": f"ens{index}", "addr_info": [{"family": "inet", "local": address, "scope": "global"}]}
             for index, address in enumerate(own, start=4)]
    links += [{"ifname": name, "addr_info": [{"family": "inet", "local": address, "scope": "global"}]}
              for name, address in bridges.items()]
    # Links with no such address, as some iproute2 releases list them.
    return links + [{}, {"ifname": "veth1234abc", "addr_info": []}]


def queue_status(queues):
    """`hot queue status --json` for {name: (pending, processing, deadletter,
    oldest_pending_secs)}."""
    rows = [{"name": name, "pending": p, "processing": r, "deadletter": d, "oldest_pending_secs": w}
            for name, (p, r, d, w) in queues.items()]
    return {"queues": rows, "total_pending": sum(r["pending"] for r in rows),
            "total_processing": sum(r["processing"] for r in rows),
            "total_deadletter": sum(r["deadletter"] for r in rows),
            "oldest_pending_secs": max((r["oldest_pending_secs"] or 0 for r in rows), default=None), "type": "redis"}


class Recorder:
    def __init__(self):
        self.commands = []
        self.ps = ""
        self.logs = ""
        self.enddate = "notAfter=Jan 30 12:00:00 2027 GMT\n"
        self.journal = {"agent": [], "host": []}
        self.queue = []
        # A release whose hot has no `queue status --json`.
        self.text_queue_status = False
        self.task_status = None
        self.interfaces = interfaces("10.68.1.20")
        # What the migration status job finds in the database: the level a
        # harness says the state records, unless a test says otherwise.
        self.migration_level = lambda: 0
        # A runtime image with no hotbox at the path the agent copies.
        self.no_hotbox = False
        self.hotbox_image = ""
        # Kata's box pool runs, as kata-devpool-activate started it held, and
        # its table shows it so. A test of a pool that is not set up in
        # device-mapper yet says so, and the room it finds on the disk.
        self.pool_active = True
        self.pool_table = "0 83886080 thin-pool 7:1 7:0 128 32768 1 no_discard_passdown\n"

    def __call__(self, command, **kwargs):
        self.commands.append(command)
        if command[0] == "ip":
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps(self.interfaces))
        if command[0] == "journalctl":
            records = self.journal["agent" if "hot-agent.service" in command else "host"]
            if "--after-cursor" in command:
                after = command[command.index("--after-cursor") + 1]
                records = records[next(i for i, r in enumerate(records) if r["__CURSOR"] == after) + 1:]
            return subprocess.CompletedProcess(command, 0, stdout="".join(json.dumps(r) + "\n" for r in records))
        if command[:2] == ["docker", "create"]:
            self.hotbox_image = command[-1]
            return subprocess.CompletedProcess(command, 0, stdout="0123abcd\n", stderr="")
        if command[:2] == ["docker", "cp"]:
            if self.no_hotbox:
                raise subprocess.CalledProcessError(1, command, stderr="Could not find the file in container")
            Path(command[-1]).write_text("hotbox of " + self.hotbox_image)
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
        if agent_module.MIGRATION_STATUS_JOB in command:
            # Compose's own lines go to its error stream.
            return subprocess.CompletedProcess(command, 0, stderr="Container hot-example-migration-status-run Creating\n",
                                               stdout=json.dumps({"level": self.migration_level(), "latest": 0, "pending": [],
                                                                  "modified": [], "unknown": []}) + "\n")
        if "move-storage" in command:
            kwargs["stdout"].write('{"kind":"file","status":"moved"}\n')
            return subprocess.CompletedProcess(command, 0, stderr="")
        if "queue" in command:
            if "--json" in command and self.text_queue_status:
                raise subprocess.CalledProcessError(2, command, stderr="unexpected argument '--json'")
            queue = self.queue or [(0, 0, 0)]
            entry = queue.pop(0) if len(queue) > 1 else queue[0]
            if isinstance(entry, dict):
                return subprocess.CompletedProcess(command, 0, stdout=json.dumps(entry))
            pending, processing, deadletter = entry
            if "--json" in command:
                return subprocess.CompletedProcess(command, 0, stdout=json.dumps(queue_status(
                    {"{hot:event}": (pending, processing, deadletter, 30 if pending else None)})))
            return subprocess.CompletedProcess(command, 0, stdout=(
                "Queue Status:\n  Backend: Redis (redis://hot:***@valkey:6379)\n\nSummary:\n"
                f"  Total pending:    {pending}\n  Total processing: {processing}\n  Total deadletter: {deadletter}\n"))
        if command[0] == "dmsetup" and not self.pool_active:
            raise subprocess.CalledProcessError(1, command, stderr="Device does not exist.")
        if command[:2] == ["dmsetup", "table"]:
            return subprocess.CompletedProcess(command, 0, stdout=self.pool_table, stderr="")
        if command[-1] == render.TASK_STATUS:
            if self.task_status is None:
                raise subprocess.CalledProcessError(1, command)
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps(self.task_status))
        stdout = (self.ps if "ps" in command else self.logs if "logs" in command
                  else self.enddate if command[0] == "openssl" else "")
        return subprocess.CompletedProcess(command, 0, stdout=stdout, stderr="")

    def compose_calls(self):
        """What Compose was told to do, without the looks at its containers or
        at the database's migration level, which change nothing."""
        calls = [" ".join(c[c.index("compose") + 3:]) for c in self.commands if "compose" in c]
        return [call for call in calls if not call.startswith("ps ") and agent_module.MIGRATION_STATUS_JOB not in call]


def undelivered_secrets(tmp, release=None):
    """The secret files a deployed bundle names that were not delivered:
    Compose refuses to start a service without its files. With more than one
    release on the host, `release` names the bundle: directory order differs
    between filesystems, so the first one found is not always the same."""
    bundles = sorted(Path(tmp, "releases").glob(f"{release or '*'}/bundle/compose.json"))
    assert len(bundles) == 1, f"name the release to check among {[b.parent.parent.name for b in bundles]}"
    compose = json.loads(bundles[0].read_text())
    assert compose["secrets"]
    return sorted(secret["file"] for secret in compose["secrets"].values() if not Path(secret["file"]).is_file())


def free_space(megabytes):
    """The data disk as having `megabytes` free to a non-root writer."""
    return mock.patch.object(agent_module.os, "statvfs", lambda path: mock.Mock(
        f_bavail=megabytes * 256, f_frsize=4096, f_blocks=1 << 30, f_files=1 << 20, f_favail=1 << 20))


class AgentHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        roots = {name: Path(self.tmp.name) / name for name in ("releases", "secrets", "etc", "host")}
        roots["data"] = Path(self.tmp.name)
        roots["meminfo"] = Path(self.tmp.name) / "meminfo"  # absent: no memory metric
        self.roots = roots
        self.config = installation()
        self.cloud = FakeCloud()
        self.run = Recorder()
        self.agent = agent_module.Agent(self.config, self.cloud, run=self.run, sleep=lambda _: None, roots=roots,
                                        chown=lambda *args: None)
        self.agent.wait_ready = lambda: None
        self.run.migration_level = lambda: self.agent.state().get("migrations") or 0
        # journalctl's output comes from the recorder, as far as read_lines reads.
        self.agent.read_lines = lambda command, limit: self.run(command).stdout.splitlines(True)[:limit]
        self.manifests = {"3.0.1": manifest()}
        patches = [
            mock.patch.object(agent_module, "http", self.fake_http),
            mock.patch.object(render, "verify_manifest_signature", lambda *args: None),
            # The temporary directory stands in for the mounted data disk.
            mock.patch.object(agent_module.Agent, "data_disk_mounted", lambda agent: True),
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
            "up -d --remove-orphans app api worker task-worker", "up -d scheduler")]
        self.assertEqual(order, sorted(order), calls)
        self.assertIsNone(self.agent.reconcile(), "an unchanged pointer does nothing")

    def test_a_redeploy_of_the_same_release_recreates_the_ingress(self):
        # nginx resolves the app and API once, when it starts, and Compose sees
        # nothing to change in an ingress of the same release and path.
        self.agent.reconcile()
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        self.run.commands.clear()
        at_readiness = []
        self.agent.wait_ready = lambda: at_readiness.extend(self.run.compose_calls())
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        calls = self.run.compose_calls()
        started = calls.index("up -d --remove-orphans app api worker task-worker")
        ingress = calls.index("up -d --force-recreate --no-deps ingress")
        self.assertLess(started, ingress)
        self.assertLess(ingress, calls.index("up -d scheduler"))
        self.assertIn("up -d --force-recreate --no-deps ingress", at_readiness, "before the probes go through it")

    def test_a_restart_of_the_app_or_api_also_recreates_the_ingress(self):
        self.agent.reconcile()
        for service, ingress in (("app", True), ("api", True), ("worker", False), ("task-worker", False)):
            with self.subTest(service=service):
                self.run.commands.clear()
                self.agent.command("restart-service", service=service)
                self.assertEqual(self.run.compose_calls(), [
                    f"up -d --force-recreate {service}", *(["up -d --force-recreate --no-deps ingress"] if ingress else [])])

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

    def test_a_private_mirror_is_signed_in_to_with_the_vm_identity(self):
        self.cloud.registry_password = lambda host: "token-for-" + host
        logins = []
        recorder = self.run

        def run(command, **kwargs):
            if command[:2] == ["docker", "login"]:
                logins.append((command, kwargs.get("input")))
            return recorder(command, **kwargs)
        self.agent.run_command = run
        for cloud, registry, user in (("aws", "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot", "AWS"),
                                      ("gcp", "us-docker.pkg.dev/acme/hot", "oauth2accesstoken"),
                                      ("aws", "mirror.example.com/hot", None)):
            with self.subTest(registry=registry):
                logins.clear()
                self.config["cloud"] = cloud
                self.config["image_registry"] = registry
                self.agent.registry_login()
                host = registry.split("/")[0]
                if user is None:
                    self.assertEqual(logins, [], "other mirrors are pulled anonymously")
                    continue
                self.assertEqual(logins, [(["docker", "login", "--username", user, "--password-stdin",
                                            f"https://{host}"], "token-for-" + host)])
        # The sign-in comes before the pull, and the pulled images are the mirror's.
        self.config["cloud"] = "aws"
        self.config["image_registry"] = "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot"
        self.agent.reconcile()
        commands = [" ".join(c) for c in self.run.commands]
        login = next(i for i, c in enumerate(commands) if c.startswith("docker login"))
        pull = next(i for i, c in enumerate(commands) if c.endswith("pull --quiet"))
        self.assertLess(login, pull)
        bundle = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertTrue(bundle["services"]["app"]["image"].startswith("123456789012.dkr.ecr.us-east-1.amazonaws.com/hot/hot@"),
                        bundle["services"]["app"])

    def test_failures_report_the_command_not_its_output(self):
        def failing(command, **kwargs):
            if "migrate" in command:
                raise subprocess.CalledProcessError(1, command, output="postgres://hot_owner:SECRET@")
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
        self.agent.run_command = failing
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertNotIn("SECRET", json.dumps(status))

    def test_failures_name_the_job_and_the_end_of_its_redacted_error(self):
        def failing(command, **kwargs):
            if "migrate" in command:
                password = self.cloud.secrets[GENERATED]["POSTGRES_PASSWORD"]
                raise subprocess.CalledProcessError(1, command, output="", stderr=(
                    "Container hot-example-migrate-run Creating\n"
                    f"connecting with {password}\nError: migration 23 failed: relation \"runs\" already exists\n"))
            return self.run(command, **kwargs)
        self.agent.run_command = failing
        status = self.agent.reconcile()
        self.assertIn("docker compose --profile admin run --rm migrate exited 1: ", status["error"])
        self.assertIn('migration 23 failed: relation "runs" already exists', status["error"])
        self.assertNotIn("compose.json", status["error"])
        self.assertNotIn(self.cloud.secrets[GENERATED]["POSTGRES_PASSWORD"], json.dumps(status))
        self.assertEqual(self.agent.state()["failed"]["error"], status["error"])

    def test_any_failure_is_recorded_rather_than_left_deploying(self):
        def full(release):
            raise OSError(28, "No space left on device")
        self.agent.fetch_release = full
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("No space left on device", status["error"])
        self.assertNotIn("deploying", self.agent.state())
        self.assertIn("Nothing was installed", status["recovery"])


class Http(unittest.TestCase):
    def test_a_request_without_a_response_is_an_agent_error(self):
        unreachable = urllib.error.URLError(socket.gaierror(-2, "Name or service not known"))
        with mock.patch.object(agent_module.urllib.request, "urlopen", side_effect=unreachable):
            with self.assertRaisesRegex(agent_module.AgentError, "GET github.com failed: .*Name or service"):
                agent_module.http("GET", "https://github.com/hot-dev/hot/releases/download/v3.0.2/x.json")
        with mock.patch.object(agent_module.urllib.request, "urlopen", side_effect=TimeoutError("timed out")):
            with self.assertRaisesRegex(agent_module.AgentError, "POST ssm.us-east-1.amazonaws.com failed: timed out"):
                agent_module.http("POST", "https://ssm.us-east-1.amazonaws.com/", b"{}")


class DataDisk(AgentHarness):
    def test_an_initialized_disk_is_mounted_before_state_is_read(self):
        self.agent.mount_data()
        self.assertEqual(self.run.commands[0][-1], "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol0abc")
        self.assertNotIn("--allow-format", self.run.commands[0], "never formats here")

    def test_a_replaced_vm_starts_the_host_services_once_the_disk_is_mounted(self):
        # The new boot disk skipped Kata's containerd, which waits for the
        # data disk, and has no host settings yet.
        self.agent.mount_data()
        self.assertEqual([c for c in self.run.commands[1:] if c[0] != "ip"],
                         [["systemctl", "restart", "hot-guest-firewall.service"],
                          ["systemctl", "start", "kata-containerd.service"]])
        etc = Path(self.tmp.name) / "etc"
        self.assertEqual((etc / "host.env").read_text(), "HOT_BOX_POOL_GB=40\nHOT_BOX_POOL_PREALLOCATE=1\n")
        self.assertIn("HOT_GUEST_DENY_CIDRS=", (etc / "guest-network.env").read_text())
        # A failure is retried next cycle, though the disk is mounted by then.
        self.run.commands.clear()
        get_secret = self.cloud.get_secret

        def throttled(reference):
            raise agent_module.AgentError("GetSecretValue failed: ThrottlingException")
        self.cloud.get_secret = throttled
        with self.assertRaisesRegex(agent_module.AgentError, "Throttling"):
            self.agent.mount_data()
        self.cloud.get_secret = get_secret
        with mock.patch.object(agent_module.os.path, "ismount", return_value=True):
            self.agent.mount_data()
            self.assertEqual(self.run.commands[-1], ["systemctl", "start", "kata-containerd.service"])
            self.run.commands.clear()
            self.agent.mount_data()
        self.assertEqual(self.run.commands, [], "a mounted disk with its services started needs nothing")

    def new_agent(self):
        """The agent after a restart: what it knows is on the disks."""
        return agent_module.Agent(self.config, self.cloud, run=self.run, sleep=lambda _: None, roots=self.roots,
                                  chown=lambda *args: None)

    def test_a_host_configuration_that_failed_is_retried_after_a_restart(self):
        etc = Path(self.tmp.name) / "etc"
        get_secret = self.cloud.get_secret

        def throttled(reference):
            raise agent_module.AgentError("GetSecretValue failed: ThrottlingException")
        self.cloud.get_secret = throttled
        with self.assertRaisesRegex(agent_module.AgentError, "Throttling"):
            self.agent.mount_data()
        self.assertFalse((etc / "host.env").exists())
        self.cloud.get_secret = get_secret
        self.run.commands.clear()
        # By now the disk is mounted, and the new process knows nothing else.
        with mock.patch.object(agent_module.os.path, "ismount", return_value=True):
            self.new_agent().mount_data()
            self.assertEqual([c for c in self.run.commands if c[0] != "ip"],
                             [["systemctl", "restart", "hot-guest-firewall.service"],
                              ["systemctl", "start", "kata-containerd.service"]])
            self.assertEqual((etc / "host.env").read_text(), "HOT_BOX_POOL_GB=40\nHOT_BOX_POOL_PREALLOCATE=1\n")
            self.assertIn("HOT_GUEST_DENY_CIDRS=", (etc / "guest-network.env").read_text())
            self.run.commands.clear()
            self.new_agent().mount_data()
            self.assertEqual(self.run.commands, [], "done once it finished")

    def test_a_host_service_that_did_not_start_is_retried_after_a_restart(self):
        recorder = self.run

        def refused(command, **kwargs):
            if "kata-containerd.service" in command:
                raise subprocess.CalledProcessError(1, command)
            return recorder(command, **kwargs)
        self.agent.run_command = refused
        with self.assertRaises(subprocess.CalledProcessError):
            self.agent.mount_data()
        self.assertTrue((Path(self.tmp.name) / "etc/host.env").exists(), "written, but kata-containerd did not start")
        recorder.commands.clear()
        with mock.patch.object(agent_module.os.path, "ismount", return_value=True):
            self.new_agent().mount_data()
            self.assertEqual(recorder.commands[-1], ["systemctl", "start", "kata-containerd.service"])
            # A deploy configures the host again, and may fail the same way.
            with self.assertRaises(subprocess.CalledProcessError):
                self.agent.configure_host()
            recorder.commands.clear()
            self.new_agent().mount_data()
            self.assertEqual(recorder.commands[-1], ["systemctl", "start", "kata-containerd.service"])

    def test_a_blank_disk_is_left_for_the_first_install(self):
        def blank(command, **kwargs):
            raise subprocess.CalledProcessError(1, command, stderr="Data disk has no filesystem")
        self.agent.run_command = blank
        self.agent.mount_data()  # no error

    def test_scratch_disks_wait_for_the_deploy(self):
        self.config["data_disk"] = "scratch"
        self.agent.mount_data()
        self.assertEqual(self.run.commands, [])


class BoxPool(AgentHarness):
    """Kata's box pool holds its data file's space where a local database
    shares the data disk (kata-devpool-activate reads the flag). It takes that
    up only when it starts; a pool that started held keeps holding through
    hot-box-pool-hold.timer, and one that started sparse waits for a restart."""

    SPARSE_TABLE = "0 83886080 thin-pool 7:1 7:0 128 32768\n"

    MANAGED = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
                   "host": "db.example", "port": 5432, "admin_user": "hot_admin", "admin_secret": "admin"}},
               "valkey": {"mode": "managed", "endpoint": {"host": "cache.example", "port": 6379, "auth_secret": "token"}}}

    def setUp(self):
        super().setUp()
        self.agent.resolve = lambda hosts: []
        self.pool = Path(self.tmp.name) / "kata/devmapper/data"

    def host_env(self):
        return (Path(self.tmp.name) / "etc/host.env").read_text()

    def sparse_pool(self):
        self.pool.parent.mkdir(parents=True, exist_ok=True)
        with open(self.pool, "wb") as pool:
            pool.truncate(64 << 20)
        if os.stat(self.pool).st_blocks * 512 >= 64 << 20:
            self.skipTest("this filesystem does not keep sparse files")

    def held_pool(self):
        self.pool.parent.mkdir(parents=True, exist_ok=True)
        self.pool.write_bytes(b"\1" * (1 << 20))

    def test_the_pool_holds_its_space_only_where_a_local_database_shares_the_disk(self):
        self.agent.configure_host()
        self.assertEqual(self.host_env(), "HOT_BOX_POOL_GB=40\nHOT_BOX_POOL_PREALLOCATE=1\n")
        # Valkey alone on the disk is enough: a box filling it stops Valkey.
        self.config["data"] = {"postgres": self.MANAGED["postgres"], "valkey": {"mode": "local"}}
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=1\n", self.host_env())
        self.config.update(data=self.MANAGED, size="medium")
        self.agent.configure_host()
        self.assertEqual(self.host_env(), "HOT_BOX_POOL_GB=100\nHOT_BOX_POOL_PREALLOCATE=0\n")
        # A scalable task host's disk holds no database; other roles run no boxes.
        self.config.update(preset="scalable", roles=["task-worker"])
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=0\n", self.host_env())
        self.config.update(preset="single-machine", roles=["app"], data={})
        self.assertFalse(self.agent.holds_box_pool())

    def test_a_pool_that_started_sparse_under_a_local_database_is_a_deploy_warning(self):
        # As on a host an earlier release deployed: the pool runs, sparse and
        # passing discards down, and activation holds it only when it next
        # starts.
        self.sparse_pool()
        self.run.pool_active = True
        self.run.pool_table = self.SPARSE_TABLE
        with self.free(4096):
            status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed")
        self.assertIn("Kata's box pool started sparse, before it was to hold its space", status["warning"])
        self.assertIn("64 MB of its data file is not held now", status["warning"])
        self.assertIn("reboot the VM", status["warning"])
        self.assertIn(["dmsetup", "table", "hot-devpool"], self.run.commands)
        # A disk without room for the rest says a reboot would stop box
        # tasks, and to grow the disk first.
        with self.free(512):
            warning = self.agent.box_pool_warning()
        self.assertIn("The disk has 512 MB free, too little for that and 1,024 MB to spare", warning)
        self.assertIn("after a reboot, or on a replaced VM, no box task would run on this host, though code tasks "
                      "would", warning)
        self.assertNotIn("task worker stopped", warning)
        # The size change stops and starts the VM, which ends the sparse
        # pool, so no second reboot is asked for.
        self.assertIn("Grow the disk with a larger size and apply, which stops and starts the VM, then run "
                      "deploy-now: the pool holds its space as it starts once the disk has room for it", warning)
        self.assertNotIn("reboot the VM", warning)
        # Fully written, it still punches holes as boxes discard, and says so.
        self.held_pool()
        with self.free(0):
            self.assertIn("reboot the VM, which stops Hot", self.agent.box_pool_warning())
            self.assertNotIn("not held now", self.agent.box_pool_warning())
        # Once it started held (after that reboot), nothing to say.
        self.run.pool_table = "0 83886080 thin-pool 7:1 7:0 128 32768 1 no_discard_passdown\n"
        self.assertEqual(self.agent.box_pool_warning(), "")
        # A pool that is not active where the disk has room for it did not
        # start for another reason, which is no talk of holding.
        self.run.pool_active = False
        self.run.pool_table = self.SPARSE_TABLE
        with self.free(4096):
            warning = self.agent.box_pool_warning()
        self.assertTrue(warning.startswith("Kata's box pool is not active, so neither is its containerd, so box "
                                           "tasks do not run on this host"), warning)
        self.assertIn("Run deploy-now, which starts it again and says why it does not start", warning)
        self.assertNotIn("started sparse", warning)
        self.assertNotIn("Grow the disk", warning)

    def test_a_warning_counts_the_margin_activation_leaves(self):
        # A pool that started sparse has no "held" file, since its sparse
        # start removed it, and activation would leave 1 GB; the agent counts
        # the same, and would count 64 MB for a data file that held its space.
        self.sparse_pool()
        self.run.pool_table = self.SPARSE_TABLE
        held = self.pool.with_name("held")
        with self.free(512):
            self.assertIn("too little for that and 1,024 MB to spare", self.agent.box_pool_warning())
            held.touch()
            self.assertIn("The pool holds it when it next starts", self.agent.box_pool_warning())
        with self.free(100):
            self.assertIn("The disk has 100 MB free, too little for that and 64 MB to spare",
                          self.agent.box_pool_warning())

    def test_a_pool_that_started_held_is_no_warning_for_the_holes_boxes_punch(self):
        # Block zeroing punches holes between hot-box-pool-hold.timer's runs,
        # which fills them again; a reboot would not keep them away.
        self.sparse_pool()
        self.run.pool_active = True
        self.assertEqual(self.agent.box_pool_warning(), "")
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed")
        self.assertNotIn("box pool", status.get("warning", ""))
        # dmsetup missing reads as no pool.
        recorder = self.agent.run_command

        def no_dmsetup(command, **kwargs):
            if command[0] == "dmsetup":
                raise FileNotFoundError(command[0])
            return recorder(command, **kwargs)
        self.agent.run_command = no_dmsetup
        self.run.pool_table = self.SPARSE_TABLE
        self.assertEqual(self.agent.box_pool_warning(), "")

    def test_a_data_move_changes_the_flag_and_the_warning_in_both_directions(self):
        # Valkey moving onto the disk sets the flag, and the running pool,
        # started sparse, is reported until a reboot holds it.
        self.config["data"] = self.MANAGED
        self.sparse_pool()
        self.run.pool_active = True
        self.run.pool_table = self.SPARSE_TABLE
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=0\n", self.host_env())
        self.assertEqual(self.agent.box_pool_warning(), "", "a sparse pool is right with no database on the disk")
        self.config["data"] = {"postgres": self.MANAGED["postgres"], "valkey": {"mode": "local"}}
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=1\n", self.host_env())
        self.assertIn("started sparse", self.agent.box_pool_warning())
        # Moving the databases off clears the flag; a held pool keeps its
        # space, which is only unused room, and nothing is reported.
        self.held_pool()
        self.run.pool_table = "0 83886080 thin-pool 7:1 7:0 128 32768 1 no_discard_passdown\n"
        self.config["data"] = self.MANAGED
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=0\n", self.host_env())
        self.assertEqual(self.agent.box_pool_warning(), "")

    def test_a_database_the_installed_release_runs_on_still_holds_the_pool(self):
        # During a move off the disk, and after one that failed before the
        # data moved, the local database is still there.
        self.agent.save_state({"release": "3.0.1", "data_modes": {"postgres": "local", "valkey": "managed"}})
        self.config["data"] = self.MANAGED
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=1\n", self.host_env())
        self.agent.save_state({"release": "3.0.1", "data_modes": {"postgres": "managed", "valkey": "managed"}})
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=0\n", self.host_env())
        # And the other way, the configuration's local database counts first.
        self.config["data"] = {"postgres": self.MANAGED["postgres"], "valkey": {"mode": "local"}}
        self.assertTrue(self.agent.holds_box_pool())

    def refuse_containerd(self):
        recorder = self.run

        def refused(command, **kwargs):
            if "kata-containerd.service" in command:
                raise subprocess.CalledProcessError(1, command)
            return recorder(command, **kwargs)
        self.agent.run_command = refused

    def free(self, megabytes):
        return free_space(megabytes)

    def test_a_disk_without_room_for_the_pool_says_so_when_containerd_does_not_start(self):
        # A replaced VM, or a reboot, on a disk the database has filled since.
        self.sparse_pool()
        self.run.pool_active = False
        self.refuse_containerd()
        with self.free(512), self.assertRaisesRegex(
                agent_module.AgentError, "Kata's containerd did not start: its box pool holds its space on this "
                "data disk, which a local database shares, and needs 64 MB more and 1,024 MB to spare, where 512 "
                "MB is free. Box tasks do not run on this host: they wait in the queue, and none fail, until it "
                "fits. Code tasks still run, and Hot's other services, the scheduler too.* Grow the "
                "disk with a larger size"):
            self.agent.configure_host()
        self.assertFalse((Path(self.tmp.name) / "etc/host-configured").exists(), "tried again next cycle")
        # Support sees it in diagnostics, and a deploy or a resume in its
        # warning (box_pool_warning), since the task worker runs.
        with self.free(512):
            self.assertIn("Kata's containerd did not start: its box pool holds its space",
                          self.agent.diagnostics(None)["box_pool"])
            self.assertIn("they wait in the queue, and none fail, until it fits.", self.agent.box_pool_warning())
        # With room, the refusal is something else, and still said.
        with self.free(2048):
            pool = self.agent.diagnostics(None)["box_pool"]
        self.assertTrue(pool.startswith("Kata's containerd is not running (not active), so box tasks do not run"),
                        pool)
        self.assertNotIn("Grow the disk", pool)
        # With room, the failure is something else, and says what it said.
        with self.free(2048), self.assertRaises(subprocess.CalledProcessError):
            self.agent.configure_host()
        # A pool with no data file yet needs all of its size.
        self.pool.unlink()
        with self.free(2048), self.assertRaisesRegex(
                agent_module.AgentError, "needs 40960 MB more and 1,024 MB to spare, where 2048 MB is free"):
            self.agent.configure_host()
        with self.free(42 * 1024), self.assertRaises(subprocess.CalledProcessError):
            self.agent.configure_host()
        # A pool that does not hold its space was not refused for room.
        self.config["data"] = self.MANAGED
        with self.free(0), self.assertRaises(subprocess.CalledProcessError):
            self.agent.configure_host()

    def test_a_data_file_that_held_its_space_before_needs_only_a_small_margin(self):
        # Activation marks a data file that took its full size ("held"),
        # until the pool starts sparse: its holes are only what boxes punched,
        # and it leaves 64 MB beside them, not 1 GB.
        self.sparse_pool()
        self.run.pool_active = False
        self.refuse_containerd()
        held = self.pool.with_name("held")
        held.touch()
        self.assertEqual(self.agent.box_pool_margin(), 64 << 20)
        with self.free(512), self.assertRaises(subprocess.CalledProcessError):
            self.agent.configure_host()
        with self.free(100), self.assertRaisesRegex(
                agent_module.AgentError, "needs 64 MB more and 64 MB to spare, where 100 MB is free"):
            self.agent.configure_host()
        # Without the mark, the same disk is refused with the full margin.
        held.unlink()
        self.assertEqual(self.agent.box_pool_margin(), 1 << 30)
        with self.free(512), self.assertRaisesRegex(agent_module.AgentError, "needs 64 MB more and 1,024 MB to spare"):
            self.agent.configure_host()
        # A mark with no data file is a new pool's, which needs 1 GB too.
        held.touch()
        self.pool.unlink()
        self.assertEqual(self.agent.box_pool_margin(), 1 << 30)
        with self.free(2048), self.assertRaisesRegex(
                agent_module.AgentError, "needs 40960 MB more and 1,024 MB to spare"):
            self.agent.configure_host()

    def test_an_active_pool_is_not_blamed_when_containerd_does_not_start(self):
        # A host an earlier release deployed, not rebooted since: its pool
        # runs sparse, and activation, which finds it active, checks no room.
        self.sparse_pool()
        self.run.pool_active = True
        self.refuse_containerd()
        with self.free(512), self.assertRaises(subprocess.CalledProcessError) as raised:
            self.agent.configure_host()
        self.assertEqual(raised.exception.cmd, ["systemctl", "start", "kata-containerd.service"])
        self.assertIn(["dmsetup", "info", "hot-devpool"], self.run.commands)
        # One that is not active is blamed, and dmsetup missing counts as not active.
        self.run.pool_active = False
        with self.free(512), self.assertRaisesRegex(agent_module.AgentError, "needs 64 MB more"):
            self.agent.configure_host()
        recorder = self.agent.run_command

        def no_dmsetup(command, **kwargs):
            if command[0] == "dmsetup":
                raise FileNotFoundError(command[0])
            return recorder(command, **kwargs)
        self.agent.run_command = no_dmsetup
        with self.free(512), self.assertRaisesRegex(agent_module.AgentError, "needs 64 MB more"):
            self.agent.configure_host()


class OlderDatabaseLayout(AgentHarness):
    """A v3.0.1 bundle kept a local database at the root of data/postgres, which
    this release's PostgreSQL image refuses, and it says so only in its log."""

    def old_disk(self):
        directory = Path(self.tmp.name) / "data/postgres"
        directory.mkdir(parents=True)
        (directory / "PG_VERSION").write_text("17\n")

    def test_it_is_refused_before_a_secret_is_written_or_anything_starts(self):
        self.old_disk()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("in the layout of a v3.0.1 bundle", status["error"])
        self.assertIn("which is not supported", status["error"])
        self.assertIn("Create a new installation, and dump the data from the old database and restore it there by hand",
                      status["error"])
        self.assertIn("wrote no secrets", status["error"])
        self.assertNotIn(GENERATED, self.cloud.secrets)
        self.assertEqual(self.run.compose_calls(), [])
        self.assertEqual((Path(self.tmp.name) / "data/postgres/PG_VERSION").read_text(), "17\n", "left as it was")
        self.assertFalse(any("--allow-format" in c and "PG_VERSION" in c for c in self.run.commands))

    def test_it_waits_for_deploy_now_and_is_refused_again(self):
        self.old_disk()
        self.agent.reconcile()
        self.assertIsNone(self.agent.reconcile())
        self.assertIn("in the layout of a v3.0.1 bundle", self.agent.command("deploy-now", request_id="d1")["error"])

    def test_todays_layout_passes(self):
        directory = Path(self.tmp.name) / "data/postgres/18/docker"
        directory.mkdir(parents=True)
        (directory / "PG_VERSION").write_text("18\n")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")

    def test_a_copy_hot_no_longer_uses_is_no_obstacle(self):
        self.old_disk()
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = "postgres://hot_runtime:pw@db.internal.example/hot"
        self.agent.resolve = lambda hosts: []
        self.agent.prepare_host(False)


class DataDirectories(AgentHarness):
    def test_the_first_deploy_creates_what_the_bundle_mounts(self):
        owners = []
        self.agent.chown = lambda path, uid, gid: owners.append((Path(path).name, uid))
        self.agent.reconcile()
        data = Path(self.tmp.name) / "data"
        mounted = {Path(volume["source"]).name for service in self.compose()["services"].values()
                   for volume in service.get("volumes", [])
                   if volume.get("type") == "bind" and volume["source"].startswith("/var/lib/hot/data/")
                   and volume["target"] != "/move"}
        self.assertEqual(mounted, {"postgres", "valkey", "build", "file", "models", "models-task-worker",
                                   "extractions", "extractions-task-worker", render.BOX_DATA_DIRECTORY})
        self.assertTrue(all((data / name).is_dir() for name in mounted))
        self.assertIn(("build", 10001), owners)
        self.assertIn(("file", 10001), owners)
        self.assertIn(("models", 10001), owners)
        self.assertIn(("models-task-worker", 0), owners, "the task worker runs as root")
        self.assertIn(("extractions", 10001), owners)
        self.assertIn(("extractions-task-worker", 0), owners)
        self.assertIn((render.BOX_DATA_DIRECTORY, 0), owners, "the task worker loop-mounts each box's /data there")
        for name in ("extractions", "extractions-task-worker", render.BOX_DATA_DIRECTORY):
            self.assertEqual((data / name).stat().st_mode & 0o777, 0o700, name)

    def compose(self):
        return json.loads(next(Path(self.tmp.name, "releases").glob("*/bundle/compose.json")).read_text())


class GuestNetwork(AgentHarness):
    def guest_env(self):
        text = (Path(self.tmp.name) / "etc/guest-network.env").read_text()
        return dict(line.split("=", 1) for line in text.splitlines())

    def test_the_guest_baseline_opens_what_the_task_workers_rules_allow(self):
        # The baseline runs before the task worker's per-task rules, so it
        # must open the operator's allowlist, resolvers, and egress proxy.
        self.config["settings"] = {"HOT_NETWORK_ALLOW_CIDRS": "10.20.0.0/16, 2600:1f18::/64,192.168.5.10",
                                   "HOT_BOX_GUEST_DNS": "10.68.0.2", "HOT_NETWORK_DENY_CIDRS": "10.20.0.5"}
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://hot:proxy-password@10.30.0.7:3128"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        env = self.guest_env()
        self.assertEqual(env["HOT_GUEST_ALLOW_CIDRS"], "10.20.0.0/16,192.168.5.10/32",
                         "IPv4 only: guests have no IPv6")
        self.assertEqual(env["HOT_GUEST_DNS"], "10.68.0.2")
        self.assertEqual(env["HOT_GUEST_PROXY"], "10.30.0.7:3128")
        deny = env["HOT_GUEST_DENY_CIDRS"].split(",")
        self.assertLessEqual({"172.30.0.0/24", "10.88.0.0/16", "10.68.0.0/16", "10.20.0.5/32", "10.68.1.20/32"}, set(deny))
        # The task worker gets the same deny list; data services' addresses
        # are in both.
        task_worker = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())[
            "services"]["task-worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(",")
        self.assertEqual(deny, agent_module.ipv4_networks(task_worker, "deny"))
        self.assertIn("10.68.4.10/32", agent_module.guest_network_env(self.config, ["10.68.4.10/32", "2600::1/128"]))
        self.assertNotIn("proxy-password", (Path(self.tmp.name) / "etc/guest-network.env").read_text())

    def test_without_settings_the_baseline_keeps_its_defaults(self):
        self.agent.reconcile()
        env = self.guest_env()
        self.assertEqual(env["HOT_GUEST_ALLOW_CIDRS"], "")
        self.assertNotIn("HOT_GUEST_DNS", env, "the baseline's default resolvers are the task worker's")
        self.assertNotIn("HOT_GUEST_PROXY", env)
        self.assertEqual(env["HOT_GUEST_DENY_CIDRS"],
                         "172.30.0.0/24,10.88.0.0/16,10.68.0.0/16,10.68.1.20/32")

    def test_the_egress_proxy_is_written_as_addresses_and_ports(self):
        resolved = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("10.30.0.8", 0)),
                    (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("10.30.0.7", 0))]
        with mock.patch.object(agent_module.socket, "getaddrinfo", return_value=resolved) as lookup:
            for proxy, expected in (("http://hot:proxy-password@proxy.internal:3128", ["10.30.0.7:3128", "10.30.0.8:3128"]),
                                    ("http://proxy.internal", ["10.30.0.7:80", "10.30.0.8:80"]),
                                    ("https://proxy.internal/", ["10.30.0.7:443", "10.30.0.8:443"]),
                                    (" HTTPS://proxy.internal:8443 ", ["10.30.0.7:8443", "10.30.0.8:8443"])):
                with self.subTest(proxy=proxy):
                    self.assertEqual(self.agent.proxy_addresses({"HOT_NETWORK_EGRESS_PROXY": proxy}), expected)
            self.assertEqual(lookup.call_args.args[:3], ("proxy.internal", None, socket.AF_INET))
            for proxy in ("socks5://proxy.internal", "not a proxy", "http://proxy.internal:port", "", None):
                with self.subTest(proxy=proxy):
                    self.assertEqual(self.agent.proxy_addresses({"HOT_NETWORK_EGRESS_PROXY": proxy}), [])
            self.config["settings"] = {"HOT_NETWORK_EGRESS_PROXY": "http://proxy.internal:3128"}
            self.assertEqual(self.agent.proxy_addresses({})[0], "10.30.0.7:3128", "or from settings")
        with mock.patch.object(agent_module.socket, "getaddrinfo", side_effect=socket.gaierror("no such host")):
            self.assertEqual(self.agent.proxy_addresses({}), [], "the task worker then refuses guests' networks")

    def test_a_proxy_that_moves_reaches_the_baseline_without_a_deploy(self):
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://hot:proxy-password@proxy.internal:3128"
        answers, clock = [["10.30.0.7"]], [1000.0]

        def getaddrinfo(host, port, family=0):
            if answers[0] is None:
                raise socket.gaierror("no such host")
            return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", (address, 0)) for address in answers[0]]

        def restarts():
            return self.run.commands.count(["systemctl", "restart", "hot-guest-firewall.service"])
        with mock.patch.object(agent_module.socket, "getaddrinfo", getaddrinfo), \
                mock.patch.object(agent_module.time, "monotonic", lambda: clock[0]):
            self.assertEqual(self.agent.reconcile()["state"], "deployed")
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.7:3128")
            self.run.commands.clear()
            self.agent.refresh_guest_network()
            self.assertEqual(restarts(), 0, "the same addresses change nothing")
            # Its load balancer's addresses rotate.
            answers[0] = ["10.30.0.8", "10.30.0.9"]
            self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.7:3128", "looked up every few minutes")
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.8:3128,10.30.0.9:3128")
            self.assertEqual(restarts(), 1)
            self.assertNotIn("proxy-password", (Path(self.tmp.name) / "etc/guest-network.env").read_text())
            # A lookup that fails keeps guests limited to the proxy, and is
            # tried again next cycle.
            answers[0] = None
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.8:3128,10.30.0.9:3128")
            self.assertEqual(restarts(), 1)
            answers[0] = ["10.30.0.10"]
            self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.10:3128")
            # A restart that fails leaves the file for the next check to apply.
            recorder, answers[0] = self.run, ["10.30.0.11"]

            def refused(command, **kwargs):
                if "hot-guest-firewall.service" in command:
                    raise subprocess.CalledProcessError(1, command)
                return recorder(command, **kwargs)
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            run = self.agent.run_command
            self.agent.run_command = refused
            with self.assertRaises(subprocess.CalledProcessError):
                self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.10:3128")
            self.agent.run_command = run
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            self.agent.refresh_guest_network()
            self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.11:3128")
            # Once the proxy is gone, so is the line.
            del self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"]
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            self.agent.refresh_guest_network()
            self.assertNotIn("HOT_GUEST_PROXY", self.guest_env())

    def test_a_proxy_that_did_not_resolve_at_the_deploy_reaches_the_baseline_next_cycle(self):
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://proxy.internal:3128"
        with mock.patch.object(agent_module.socket, "getaddrinfo", side_effect=socket.gaierror("no such host")):
            self.assertEqual(self.agent.reconcile()["state"], "deployed")
            self.assertNotIn("HOT_GUEST_PROXY", self.guest_env())
            self.agent.refresh_guest_network()
        resolved = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("10.30.0.7", 0))]
        with mock.patch.object(agent_module.socket, "getaddrinfo", return_value=resolved):
            self.agent.refresh_guest_network()
        self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.7:3128")

    def test_a_deploy_during_a_dns_outage_keeps_guests_on_the_proxy(self):
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://proxy.internal:3128"
        resolved = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("10.30.0.7", 0))]
        with mock.patch.object(agent_module.socket, "getaddrinfo", return_value=resolved):
            self.agent.reconcile()
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        with mock.patch.object(agent_module.socket, "getaddrinfo", side_effect=socket.gaierror("no such host")):
            self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.guest_env()["HOT_GUEST_PROXY"], "10.30.0.7:3128")

    def test_nothing_is_kept_up_before_the_host_settings_are_written(self):
        self.agent.refresh_guest_network()
        self.assertEqual(self.run.commands, [])

    def test_ipv4_written_in_ipv6_counts_as_hot_reads_it(self):
        self.assertEqual(agent_module.ipv4_networks(
            ["::ffff:10.20.0.0/112", "::ffff:192.168.5.10", "64:ff9b::10.30.0.0/112", "::10.40.0.1/128", "10.20.0.0/16",
             "2600:1f18::/64", "::ffff:0:0/90", "64:ff9b::/90", "::1:0:0:0/96"], "list"),
            ["10.20.0.0/16", "192.168.5.10/32", "10.30.0.0/16", "10.40.0.1/32"])
        self.config["settings"] = {"HOT_NETWORK_ALLOW_CIDRS": "::ffff:10.20.0.0/112",
                                   "HOT_NETWORK_DENY_CIDRS": "::ffff:10.20.0.5"}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        env = self.guest_env()
        self.assertEqual(env["HOT_GUEST_ALLOW_CIDRS"], "10.20.0.0/16", "the baseline opens what the task worker's rules do")
        self.assertIn("10.20.0.5/32", env["HOT_GUEST_DENY_CIDRS"].split(","))

    def test_the_hosts_own_addresses_are_denied_on_every_kind_of_network(self):
        for network in ({"mode": "new"}, {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}):
            with self.subTest(network=network):
                self.config["network"] = network
                self.run.interfaces = interfaces("10.20.0.9", "192.168.7.4")
                self.assertEqual(self.agent.reconcile(retry=True)["state"], "deployed")
                bundle = next(Path(self.tmp.name, "releases").glob("*/bundle/compose.json"))
                services = json.loads(bundle.read_text())["services"]
                guest = self.guest_env()["HOT_GUEST_DENY_CIDRS"].split(",")
                for role in ("worker", "task-worker"):
                    self.assertEqual(services[role]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(","), guest)
                self.assertLessEqual({"10.20.0.9/32", "192.168.7.4/32"}, set(guest))
                for bridge in ("172.30.0.1/32", "10.88.0.1/32"):
                    self.assertNotIn(bridge, guest, "the bridges' whole networks are listed")

    def test_a_docker0_added_to_a_host_is_denied_as_its_own_address(self):
        # Host images have no docker0, so the deny list names no 172.17.0.0/16.
        self.run.interfaces = interfaces("10.68.1.20") + [
            {"ifname": "docker0", "addr_info": [{"family": "inet", "local": "172.17.0.1", "scope": "global"}]}]
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        guest = self.guest_env()["HOT_GUEST_DENY_CIDRS"].split(",")
        self.assertIn("172.17.0.1/32", guest)
        self.assertNotIn("172.17.0.0/16", guest, "an ordinary range an operator may allowlist")
        bundle = next(Path(self.tmp.name, "releases").glob("*/bundle/compose.json"))
        worker = json.loads(bundle.read_text())["services"]["worker"]
        self.assertIn("172.17.0.1/32", worker["environment"]["HOT_NETWORK_DENY_CIDRS"].split(","))

    def test_nothing_is_written_without_the_hosts_addresses(self):
        unreadable = self.run

        def run(command, **kwargs):
            if command[0] == "ip":
                raise subprocess.CalledProcessError(1, command)
            return unreadable(command, **kwargs)
        self.agent.run_command = run
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Cannot read this host's addresses", status["error"])
        self.assertFalse((Path(self.tmp.name) / "etc/guest-network.env").exists())
        self.assertEqual(list(Path(self.tmp.name, "releases").glob("*/bundle")), [])
        self.agent.run_command = self.run
        self.run.interfaces = interfaces()
        self.assertIn("no interface has a global IPv4 address", self.agent.reconcile(retry=True)["error"])
        self.run.interfaces = interfaces("not an address")
        self.assertIn("Cannot read this host's addresses", self.agent.reconcile(retry=True)["error"])

    def test_a_replaced_vm_on_an_existing_network_renders_its_new_address(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        self.run.interfaces = interfaces("10.20.0.9")
        self.agent.reconcile()
        self.assertIsNone(self.agent.reconcile(), "the same address needs nothing")
        self.run.interfaces = interfaces("10.20.0.33")
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        for deny in (self.guest_env()["HOT_GUEST_DENY_CIDRS"],
                     json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())[
                         "services"]["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"]):
            self.assertIn("10.20.0.33/32", deny.split(","))
            self.assertNotIn("10.20.0.9/32", deny.split(","))
        self.assertIsNone(self.agent.reconcile())
        self.run.interfaces = []
        self.assertIsNone(self.agent.reconcile(), "addresses it cannot read now change nothing")

    def test_a_replaced_vm_does_not_start_a_stopped_installation(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        self.run.interfaces = interfaces("10.20.0.9")
        self.agent.reconcile()
        self.agent.command("drain-and-stop", request_id="s1")
        self.run.interfaces = interfaces("10.20.0.33")
        self.run.commands.clear()
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.run.compose_calls(), [])
        self.assertTrue(self.agent.state()["stopped"])
        # Box guests' baseline does not wait for the next deploy.
        self.agent.refresh_guest_network()
        self.assertIn("10.20.0.33/32", self.guest_env()["HOT_GUEST_DENY_CIDRS"].split(","))
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        self.assertNotIn("stopped", self.agent.state())
        worker = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())["services"]["worker"]
        self.assertIn("10.20.0.33/32", worker["environment"]["HOT_NETWORK_DENY_CIDRS"].split(","))

    def test_a_new_network_already_covers_a_replaced_vm(self):
        self.agent.reconcile()
        self.run.interfaces = interfaces("10.68.1.77")
        self.assertIsNone(self.agent.reconcile())

    def test_the_file_the_host_shell_reads_holds_only_addresses(self):
        for settings, message in (
                ({"HOT_NETWORK_ALLOW_CIDRS": "10.0.0.0/8; touch /tmp/owned"}, "is not an IP address or CIDR"),
                ({"HOT_BOX_GUEST_DNS": "$(id)"}, "is not an IPv4 address"),
                ({"HOT_BOX_GUEST_DNS": "2001:4860:4860::8888"}, "is not an IPv4 address"),
                ({"HOT_BOX_GUEST_DNS": "169.254.169.253"}, "can never reach")):
            with self.subTest(settings=settings), self.assertRaisesRegex(agent_module.AgentError, message):
                agent_module.guest_network_env({**self.config, "settings": settings})


class Secrets(AgentHarness):
    def test_each_role_receives_only_its_own_files(self):
        self.cloud.secrets[OPERATOR].update(HOT_EMAIL_SMTP_PASSWORD="smtp", HOT_GOOGLE_CLIENT_ID="google-id",
                                            HOT_GOOGLE_CLIENT_SECRET="oauth")
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
        self.assertEqual((root / "valkey/VALKEY_ACL").read_text(),
                         render.valkey_acl(self.cloud.secrets[GENERATED]["VALKEY_PASSWORD"]))
        self.assertEqual(undelivered_secrets(self.tmp.name), [])
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

    def test_email_credentials_go_in_the_bundle_and_settings_in_settings(self):
        self.cloud.secrets[OPERATOR].update(HOT_EMAIL_SMTP_USERNAME="mailer", HOT_EMAIL_SMTP_PASSWORD="smtp-password")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        worker = {p.name for p in (Path(self.tmp.name) / "secrets/worker").iterdir()}
        self.assertLessEqual({"HOT_EMAIL_SMTP_USERNAME", "HOT_EMAIL_SMTP_PASSWORD"}, worker)
        # The app reads the provider and sender too, so in the bundle (the
        # worker's alone) they would be half applied.
        for key in ("HOT_EMAIL_PROVIDER", "HOT_EMAIL_FROM", "HOT_EMAIL_FROM_NAME", "HOT_EMAIL_SMTP_HOST"):
            with self.subTest(key=key), self.assertRaisesRegex(agent_module.AgentError, f"{key} is not a credential"):
                agent_module.operator_routes({key: "x"}, self.config)

    def test_a_private_relays_ca_certificate_goes_only_to_the_worker(self):
        pem = "-----BEGIN CERTIFICATE-----\nrelay\n-----END CERTIFICATE-----\n"
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_CA_CERT"] = pem
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        root = Path(self.tmp.name) / "secrets"
        self.assertEqual((root / "worker/HOT_EMAIL_SMTP_CA_CERT").read_text(), pem)
        self.assertFalse((root / "app/HOT_EMAIL_SMTP_CA_CERT").exists())
        services = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())["services"]
        self.assertEqual(services["worker"]["environment"]["HOT_EMAIL_SMTP_CA_CERT_FILE"],
                         "/run/secrets/HOT_EMAIL_SMTP_CA_CERT")
        self.assertNotIn("HOT_EMAIL_SMTP_CA_CERT_FILE", services["app"]["environment"])
        self.assertEqual(undelivered_secrets(self.tmp.name), [])

    def test_the_egress_proxy_goes_only_to_the_services_that_run_user_code(self):
        proxy = "http://hot:proxy-password@proxy.internal:3128"
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = proxy
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        root = Path(self.tmp.name) / "secrets"
        self.assertEqual((root / "worker/HOT_NETWORK_EGRESS_PROXY").read_text(), proxy)
        self.assertEqual((root / "task-worker/HOT_NETWORK_EGRESS_PROXY").read_text(), proxy)
        self.assertFalse((root / "app/HOT_NETWORK_EGRESS_PROXY").exists())
        text = (Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text()
        compose = json.loads(text)
        for role in ("worker", "task-worker"):
            self.assertEqual(compose["services"][role]["environment"]["HOT_NETWORK_EGRESS_PROXY_FILE"],
                             "/run/secrets/HOT_NETWORK_EGRESS_PROXY")
        self.assertNotIn("proxy-password", text)
        with self.assertRaisesRegex(agent_module.AgentError, "in both settings and the operator secret bundle"):
            agent_module.operator_routes({"HOT_NETWORK_EGRESS_PROXY": proxy},
                                         {"settings": {"HOT_NETWORK_EGRESS_PROXY": "http://other:3128"}})
        with self.assertRaisesRegex(agent_module.AgentError, "names a file; put the value itself under HOT_NETWORK"):
            agent_module.operator_routes({"HOT_NETWORK_EGRESS_PROXY_FILE": "/run/proxy"})
        with self.assertRaisesRegex(agent_module.AgentError, "unsupported entry HOT_NETWORK_DENY_CIDRS"):
            agent_module.operator_routes({"HOT_NETWORK_DENY_CIDRS": "10.0.0.0/8"})

    def test_smtp_settings_the_worker_would_refuse_stop_the_deploy(self):
        smtp = {"HOT_EMAIL_PROVIDER": "smtp", "HOT_EMAIL_FROM": "hot@example.com", "HOT_EMAIL_SMTP_HOST": "smtp.example.com"}
        login = {"HOT_EMAIL_SMTP_USERNAME": "hot", "HOT_EMAIL_SMTP_PASSWORD": "smtp-password"}
        pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
        for settings, operator, message in (
                ({"HOT_EMAIL_SMTP_SECURITY": "none"}, login, "in plaintext"),
                ({"HOT_EMAIL_SMTP_SECURITY": " NONE "}, login, "in plaintext"),
                ({}, {"HOT_EMAIL_SMTP_USERNAME": "hot"}, "Set both"),
                ({}, {"HOT_EMAIL_SMTP_PASSWORD": "smtp-password"}, "Set both"),
                ({"HOT_EMAIL_SMTP_USERNAME": "hot"}, {}, "Set both"),
                ({"HOT_EMAIL_SMTP_SECURITY": "none"}, {"HOT_EMAIL_SMTP_CA_CERT": pem}, "starttls or tls"),
                ({}, {"HOT_EMAIL_SMTP_CA_CERT": "not a certificate"}, "PEM certificates")):
            with self.subTest(settings=settings, operator=operator):
                with self.assertRaisesRegex(agent_module.AgentError, message) as caught:
                    agent_module.check_smtp({**smtp, **settings}, operator)
                self.assertNotIn("smtp-password", str(caught.exception))
        for settings, operator in (({}, login), ({}, {}), ({"HOT_EMAIL_SMTP_SECURITY": "none"}, {}),
                                   ({"HOT_EMAIL_SMTP_SECURITY": "tls"}, {**login, "HOT_EMAIL_SMTP_CA_CERT": pem}),
                                   ({"HOT_EMAIL_SMTP_USERNAME": "hot"}, {"HOT_EMAIL_SMTP_PASSWORD": "smtp-password"})):
            with self.subTest(settings=settings, operator=operator):
                agent_module.check_smtp({**smtp, **settings}, operator)
        # Another provider does not use them.
        agent_module.check_smtp({"HOT_EMAIL_PROVIDER": "none"}, {"HOT_EMAIL_SMTP_PASSWORD": "smtp-password"})
        # Alerts may be the only email that goes through SMTP.
        with self.assertRaisesRegex(agent_module.AgentError, "Set both"):
            agent_module.check_smtp({"HOT_EMAIL_ALERTS_PROVIDER": "SMTP"}, {"HOT_EMAIL_SMTP_PASSWORD": "smtp-password"})
        # Refused before anything stops.
        self.agent.reconcile()
        self.config["settings"] = smtp
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = "smtp-password"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Set both", status["error"])
        self.assertFalse(any(c.startswith("stop") for c in self.run.compose_calls()[:-1]))

    def test_smtp_settings_do_not_keep_the_installed_release_from_starting(self):
        smtp = {"HOT_EMAIL_PROVIDER": "smtp", "HOT_EMAIL_FROM": "hot@example.com", "HOT_EMAIL_SMTP_HOST": "smtp.example.com",
                "HOT_EMAIL_SMTP_USERNAME": "hot"}
        self.config["settings"] = smtp
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = "smtp-password"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        # A later change a render would refuse, and then a reboot: the deploy
        # leaves the release running, and the reboot starts it.
        self.config["settings"] = {**smtp, "HOT_EMAIL_SMTP_CA_CERT": "not a certificate"}
        status = self.agent.reconcile()
        self.assertIn("PEM certificates", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        report = self.agent.resume()
        self.assertEqual(report["state"], "resumed", report)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertEqual(self.agent.command("restart-service", "worker", "r1")["state"], "restarted")
        # A credential gone from the bundle reaches Compose too, which starts
        # the data services and refuses only the service that mounts it, and
        # deploy-now says what is wrong.
        del self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"]
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertIn("up -d --wait postgres valkey", self.run.compose_calls())
        self.assertEqual([Path(file).parent.name + "/" + Path(file).name for file in undelivered_secrets(self.tmp.name)],
                         ["worker/HOT_EMAIL_SMTP_PASSWORD"])
        self.config["settings"] = smtp
        self.assertIn("Set both", self.agent.command("deploy-now", request_id="d1")["error"])

    def test_an_entry_added_or_removed_takes_deploy_now_not_a_restart(self):
        self.agent.reconcile()
        compose = Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json"
        self.cloud.secrets[OPERATOR].update(HOT_GOOGLE_CLIENT_ID="google-id", HOT_GOOGLE_CLIENT_SECRET="google-secret")
        self.run.commands.clear()
        report = self.agent.answer("restart-service", service="app", request_id="r1")
        self.assertEqual(report["state"], "rejected")
        self.assertIn("For app, the operator secret bundle has HOT_GOOGLE_CLIENT_ID, HOT_GOOGLE_CLIENT_SECRET, which "
                      "the installed bundle does not mount; run deploy-now", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing restarts")
        self.assertEqual(self.agent.answer("restart-service", service="worker")["state"], "restarted",
                         "the worker mounts none of them")
        # deploy-now renders the same release and configuration again, with them.
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.1"), report)
        self.assertIn("HOT_GOOGLE_CLIENT_ID_FILE", json.loads(compose.read_text())["services"]["app"]["environment"])
        self.assertEqual(self.agent.answer("restart-service", service="app")["state"], "restarted")
        # A value rotated in place needs only the restart.
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_SECRET"] = "rotated-secret"
        self.assertEqual(self.agent.answer("restart-service", service="app")["state"], "restarted")
        self.assertEqual(self.agent.command("deploy-now")["state"], "current")
        # Removed, the bundle would mount files that are gone.
        for key in ("HOT_GOOGLE_CLIENT_ID", "HOT_GOOGLE_CLIENT_SECRET"):
            del self.cloud.secrets[OPERATOR][key]
        report = self.agent.answer("restart-service", service="app")
        self.assertIn("no longer has HOT_GOOGLE_CLIENT_ID, HOT_GOOGLE_CLIENT_SECRET, which the installed bundle mounts",
                      report["error"])
        self.assertEqual(self.agent.command("deploy-now")["state"], "deployed")
        self.assertNotIn("HOT_GOOGLE_CLIENT_ID_FILE", json.loads(compose.read_text())["services"]["app"]["environment"])

    def test_a_bad_value_never_leaves_part_of_the_secrets(self):
        self.agent.reconcile()
        root = Path(self.tmp.name) / "secrets"

        def tree():
            return sorted(p.relative_to(root).as_posix() for p in root.rglob("*"))
        before = tree()
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = 12345678
        report = self.agent.answer("restart-service", service="worker", request_id="r1")
        self.assertEqual(report["state"], "rejected")
        self.assertIn("HOT_EMAIL_SMTP_PASSWORD must be a string", report["error"])
        self.assertEqual(tree(), before)
        # A write that fails partway leaves the delivered files as they were.
        written = []

        def chown(path, uid, gid):
            written.append(path)
            if len(written) == 3:
                raise OSError(28, "No space left on device")
        with self.assertRaises(OSError):
            agent_module.write_secret_files(root, {"app": {"A": "1", "B": "2"}, "worker": {"C": "3"}}, chown=chown)
        self.assertEqual(tree(), before)
        self.assertFalse(root.with_name("secrets.new").exists())
        self.assertIsNone(self.agent.resume(), "the delivered secrets are complete")

    def test_a_delivery_of_some_roles_leaves_the_others_as_they_were(self):
        self.agent.reconcile()
        root = Path(self.tmp.name) / "secrets"
        worker = root / "worker/HOT_DB_URI"
        before = worker.stat()
        migrate = root / "migrate/HOT_DB_MIGRATION_URI"
        stale = root / "migrate/STALE"
        descriptor = os.open(stale, os.O_WRONLY | os.O_CREAT, 0o400)
        os.close(descriptor)
        self.cloud.secrets[self.agent.references["generated"]]["POSTGRES_PASSWORD"] = "changed"
        self.cloud.secrets[self.agent.references["generated"]]["POSTGRES_RUNTIME_PASSWORD"] = "changed-too"
        self.agent.deliver_level_secrets()
        self.assertIn("changed", migrate.read_text())
        self.assertFalse(stale.exists(), "the role delivered is replaced whole")
        self.assertNotIn("changed-too", worker.read_text())
        after = worker.stat()
        self.assertEqual((after.st_ino, after.st_mode, after.st_uid), (before.st_ino, before.st_mode, before.st_uid))
        for name in ("secrets.new", "secrets.old"):
            self.assertFalse(root.with_name(name).exists())
        self.assertEqual(undelivered_secrets(self.tmp.name), [])
        # A failure partway leaves all of it as it was.
        with self.assertRaises(OSError), mock.patch.object(agent_module.shutil, "copytree",
                                                           side_effect=OSError(28, "No space left on device")):
            self.agent.deliver_level_secrets()
        self.assertEqual(worker.stat().st_ino, before.st_ino)
        self.assertFalse(root.with_name("secrets.new").exists())


class BundleEntries(AgentHarness):
    """What a render refuses in the operator secret bundle, and what that must
    not stop: a reboot or a restart of what is already installed."""

    NOT_CREDENTIALS = ("HOT_FILE_AWS_S3_BUCKET", "HOT_BUILD_AWS_S3_REGION", "HOT_BUILD_AWS_SECRET_KEY", "HOT_FILE_AWS_")

    def test_only_s3_credentials_go_under_the_storage_prefixes(self):
        for key in self.NOT_CREDENTIALS:
            with self.subTest(key=key), self.assertRaisesRegex(agent_module.AgentError, f"unsupported entry {key}$"):
                agent_module.operator_routes({key: "x"}, self.config)
        for role in ("BUILD", "FILE"):
            for suffix in agent_module.S3_KEYS:
                key = f"HOT_{role}_AWS_{suffix}"
                with self.subTest(key=key):
                    routes = agent_module.operator_routes({key: "x"}, self.config)
                    self.assertEqual(routes, {service: [key] for service in agent_module.HOT_SERVICES})

    OAUTH = (("Google", "HOT_GOOGLE_CLIENT_ID", "HOT_GOOGLE_CLIENT_SECRET"),
             ("GitHub", "HOT_GITHUB_CLIENT_ID", "HOT_GITHUB_CLIENT_SECRET"))

    def test_a_half_set_sign_in_stops_a_deploy_before_anything_stops_and_names_what_is_missing(self):
        self.agent.reconcile()
        for label, client_id, secret in self.OAUTH:
            for present, missing in ((client_id, secret), (secret, client_id)):
                with self.subTest(present=present):
                    self.cloud.secrets[OPERATOR][present] = "the-value"
                    self.config["settings"] = {"HOT_EXAMPLE_SETTING": present}
                    self.run.commands.clear()
                    status = self.agent.reconcile(retry=True)
                    self.assertEqual(status["state"], "failed", status)
                    self.assertIn(f"Sign-in with {label} needs both", status["error"])
                    self.assertIn(f"{missing} has no value", status["error"])
                    self.assertNotIn("the-value", status["error"])
                    self.assertEqual(self.run.compose_calls(), [], "nothing stops")
                    self.assertIn("3.0.1 keeps running", status["recovery"])
                    del self.cloud.secrets[OPERATOR][present]

    def test_deploy_now_refuses_a_half_set_sign_in_too(self):
        self.agent.reconcile()
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"] = "google-id"
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("HOT_GOOGLE_CLIENT_SECRET has no value", report["error"])
        self.assertEqual(self.run.compose_calls(), [])

    def test_a_blank_sign_in_entry_counts_as_missing(self):
        # Mounted, its empty file would keep the app from starting.
        self.agent.reconcile()
        for blank in ("", "  \n"):
            with self.subTest(blank=blank):
                self.cloud.secrets[OPERATOR].update(HOT_GITHUB_CLIENT_ID="github-id", HOT_GITHUB_CLIENT_SECRET=blank)
                report = self.agent.answer("deploy-now", request_id="d" + str(len(blank)))
                self.assertIn("HOT_GITHUB_CLIENT_SECRET has no value", report["error"])
                self.cloud.secrets[OPERATOR].update(HOT_GITHUB_CLIENT_ID=blank, HOT_GITHUB_CLIENT_SECRET=blank)
                with self.assertRaisesRegex(agent_module.AgentError, "HOT_GITHUB_CLIENT_ID and HOT_GITHUB_CLIENT_SECRET have no value"):
                    agent_module.check_oauth({}, self.cloud.secrets[OPERATOR])

    def test_a_whole_sign_in_or_none_goes_through(self):
        agent_module.check_oauth({}, {})
        agent_module.check_oauth({}, {"HOT_GOOGLE_CLIENT_ID": "id", "HOT_GOOGLE_CLIENT_SECRET": "secret"})
        agent_module.check_oauth({"HOT_GOOGLE_CLIENT_ID": "id"}, {"HOT_GOOGLE_CLIENT_SECRET": "secret"})
        with self.assertRaisesRegex(agent_module.AgentError, "HOT_GOOGLE_CLIENT_SECRET has no value"):
            agent_module.check_oauth({"HOT_GOOGLE_CLIENT_ID": "id"}, {})
        self.cloud.secrets[OPERATOR].update(HOT_GOOGLE_CLIENT_ID="google-id", HOT_GOOGLE_CLIENT_SECRET="google-secret")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")

    def test_a_restart_of_the_app_refuses_a_half_set_sign_in_and_others_go_on(self):
        self.cloud.secrets[OPERATOR].update(HOT_GOOGLE_CLIENT_ID="google-id", HOT_GOOGLE_CLIENT_SECRET="google-secret")
        self.agent.reconcile()
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_SECRET"] = ""
        self.run.commands.clear()
        report = self.agent.answer("restart-service", "app", request_id="r1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("HOT_GOOGLE_CLIENT_SECRET has no value", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "the app keeps running on what it read")
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="r2")["state"], "restarted")
        # An entry added alone says so too, rather than only asking for a render.
        del self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_SECRET"]
        report = self.agent.answer("restart-service", "app", request_id="r3")
        self.assertIn("HOT_GOOGLE_CLIENT_SECRET has no value", report["error"])

    def test_a_half_set_sign_in_does_not_keep_a_reboot_from_starting_the_installed_release(self):
        self.agent.reconcile()
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"] = "google-id"
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        report = self.agent.resume()
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")

    def test_an_entry_a_render_refuses_stops_a_deploy_before_anything_stops(self):
        self.agent.reconcile()
        self.cloud.secrets[OPERATOR]["HOT_FILE_AWS_SECRET_KEY"] = "secret-value"
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "rejected")
        self.assertIn("unsupported entry HOT_FILE_AWS_SECRET_KEY", report["error"])
        self.assertEqual(self.run.compose_calls(), [])
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("unsupported entry HOT_FILE_AWS_SECRET_KEY", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), [], "rendered first, so nothing stopped")

    EMPTY = ("", "\n", "\r\n", "\ufeff", "\ufeff\n", "\ufeff\r\n")

    def test_an_entry_hot_would_read_as_empty_is_refused_by_name(self):
        # Each is mounted as a file, and Hot exits on an empty one (resolve_one).
        for key in ("HOT_NETWORK_EGRESS_PROXY", "HOT_EMAIL_SMTP_USERNAME", "HOT_EMAIL_SMTP_PASSWORD",
                    "HOT_EMAIL_SMTP_CA_CERT", "HOT_FILE_AWS_ACCESS_KEY_ID", "HOT_BUILD_AWS_SESSION_TOKEN",
                    "TLS_CERTIFICATE", "TLS_PRIVATE_KEY"):
            for empty in self.EMPTY:
                with self.subTest(key=key, empty=empty), self.assertRaises(agent_module.AgentError) as caught:
                    agent_module.operator_routes({key: empty}, self.config)
                self.assertIn(f"entry {key} is empty", str(caught.exception))
                self.assertIn("remove the entry", str(caught.exception))
        existing = dict(self.config, data={"postgres": {"mode": "existing"}})
        with self.assertRaisesRegex(agent_module.AgentError, "entry POSTGRES_CA_CERT is empty"):
            agent_module.operator_routes({"POSTGRES_CA_CERT": ""}, existing)
        # Hot reads these as values: it drops one line ending and nothing else.
        for value in ("hot", "hot\n", "\n\n", " ", "\t\n"):
            with self.subTest(value=value):
                agent_module.operator_routes({"HOT_EMAIL_SMTP_USERNAME": value}, self.config)
        agent_module.operator_routes({agent_module.SYNTHETIC_KEY: ""}, self.config)

    def test_an_empty_entry_stops_a_deploy_before_anything_stops(self):
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://proxy.internal:3128"
        self.agent.reconcile()
        before = self.delivered()
        for key, value in (("HOT_NETWORK_EGRESS_PROXY", ""), ("HOT_EMAIL_SMTP_USERNAME", ""),
                           ("HOT_FILE_AWS_SECRET_ACCESS_KEY", "\n"), ("TLS_PRIVATE_KEY", "")):
            with self.subTest(key=key):
                operator = dict(self.cloud.secrets[OPERATOR])
                self.cloud.secrets[OPERATOR][key] = value
                self.config["settings"] = {"HOT_EXAMPLE_SETTING": key}  # so each attempt renders
                self.run.commands.clear()
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn(f"entry {key} is empty", status["error"])
                self.assertIn("3.0.1 keeps running", status["recovery"])
                self.assertEqual(self.run.compose_calls(), [], "nothing stops")
                self.assertEqual(self.delivered(), before)
                self.cloud.secrets[OPERATOR] = operator
        # deploy-now, whose render would mount the same file, says so first.
        self.config["settings"] = {}
        self.assertEqual(self.agent.answer("deploy-now", request_id="d1")["state"], "deployed")
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = ""
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("entry HOT_NETWORK_EGRESS_PROXY is empty", report["error"])
        self.assertEqual(self.run.compose_calls(), [])

    def test_an_empty_entry_nothing_here_mounts_does_not_stop_a_host(self):
        worker_entry, ingress_entry = {"HOT_EMAIL_SMTP_PASSWORD": ""}, {"TLS_PRIVATE_KEY": ""}
        sign_in = {"HOT_GOOGLE_CLIENT_SECRET": ""}
        for roles, refused in ((["worker"], (worker_entry,)), (["task-worker"], ()),
                               (["app"], (ingress_entry, sign_in)), (["api"], (ingress_entry,)), (["scheduler"], ())):
            config = dict(self.config, roles=roles)
            for entry in (worker_entry, ingress_entry, sign_in):
                with self.subTest(roles=roles, entry=entry):
                    if entry in refused:
                        with self.assertRaisesRegex(agent_module.AgentError, "is empty"):
                            agent_module.operator_routes(entry, config)
                    else:
                        agent_module.operator_routes(entry, config)

    def test_an_empty_entry_does_not_stop_a_reboot(self):
        # The installed bundle still mounts that file, so it is delivered as it is.
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = "smtp-password"
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_USERNAME"] = "hot"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = ""
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(self.delivered()["worker/HOT_EMAIL_SMTP_PASSWORD"], "")
        self.assertEqual(undelivered_secrets(self.tmp.name), [])

    def test_a_restart_refuses_an_egress_proxy_hot_would_refuse(self):
        # The worker and task worker exit on one, where now they run on the value
        # they read, and the service would restart until the alert fires.
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://hot:proxy-password@proxy.internal:3128"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        before = self.delivered()
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "hot:proxy-password@proxy.internal:3128"
        for service in ("worker", "task-worker"):
            with self.subTest(service=service):
                self.run.commands.clear()
                report = self.agent.answer("restart-service", service, request_id=f"r-{service}")
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("HOT_NETWORK_EGRESS_PROXY must be an http:// or https:// proxy address", report["error"])
                self.assertNotIn("proxy-password", report["error"])
                self.assertEqual(self.run.compose_calls(), [], "it keeps running on what it read")
                self.assertEqual(self.delivered(), before)
        # A service that does not mount it goes on, and a reboot delivers regardless.
        self.assertEqual(self.agent.answer("restart-service", "app", request_id="r-app")["state"], "restarted")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(self.delivered()["worker/HOT_NETWORK_EGRESS_PROXY"], "hot:proxy-password@proxy.internal:3128")
        # A good value is still all a rotation needs.
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://hot:rotated@proxy.internal:3128"
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="r-good")["state"], "restarted")

    def test_a_restart_refuses_an_empty_entry_the_service_mounts(self):
        # It would exit on the file, where now it runs on the value it read.
        self.cloud.secrets[OPERATOR].update(HOT_NETWORK_EGRESS_PROXY="http://proxy.internal:3128",
                                            HOT_EMAIL_SMTP_USERNAME="hot", HOT_EMAIL_SMTP_PASSWORD="smtp-password",
                                            HOT_FILE_AWS_ACCESS_KEY_ID="key", HOT_FILE_AWS_SECRET_ACCESS_KEY="secret")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        before = self.delivered()
        everything = ("app", "api", "worker", "task-worker", "scheduler")
        for key, refused in (("HOT_NETWORK_EGRESS_PROXY", ("worker", "task-worker")),
                             ("HOT_EMAIL_SMTP_PASSWORD", ("worker",)),
                             ("HOT_FILE_AWS_SECRET_ACCESS_KEY", everything),
                             # Restarting the app or the API recreates the ingress too.
                             ("TLS_PRIVATE_KEY", ("ingress", "app", "api"))):
            for empty in ("", "\ufeff\r\n"):
                operator = dict(self.cloud.secrets[OPERATOR])
                self.cloud.secrets[OPERATOR][key] = empty
                for service in refused:
                    with self.subTest(key=key, service=service, empty=empty):
                        self.run.commands.clear()
                        report = self.agent.answer("restart-service", service, request_id=f"r-{key}-{service}")
                        self.assertEqual(report["state"], "rejected", report)
                        self.assertIn(f"entry {key} is empty", report["error"])
                        self.assertEqual(self.run.compose_calls(), [], "it keeps running on what it read")
                        self.assertEqual(self.delivered(), before)
                self.cloud.secrets[OPERATOR] = operator
        # What does not mount it goes on, and a value is still all a rotation needs.
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = ""
        self.assertEqual(self.agent.answer("restart-service", "api", request_id="r-api")["state"], "restarted")
        self.cloud.secrets[OPERATOR]["HOT_EMAIL_SMTP_PASSWORD"] = "rotated"
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="r-worker")["state"], "restarted")
        self.assertEqual(self.delivered()["worker/HOT_EMAIL_SMTP_PASSWORD"], "rotated")

    def delivered(self):
        root = Path(self.tmp.name) / "secrets"
        return {str(path.relative_to(root)): path.read_text() for path in root.rglob("*") if path.is_file()}

    def test_a_refused_deploy_leaves_the_delivered_secrets_as_they_were(self):
        # The installed release reads them at its next restart, and Hot errors on
        # an empty file: this deploy said nothing changed.
        self.cloud.secrets[OPERATOR].update(HOT_GOOGLE_CLIENT_ID="google-id", HOT_GOOGLE_CLIENT_SECRET="google-secret")
        self.agent.reconcile()
        before = self.delivered()
        self.assertEqual(before["app/HOT_GOOGLE_CLIENT_SECRET"], "google-secret")
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters["pointer"] = "3.0.2"
        for entry, settings, refusal in (({"HOT_GOOGLE_CLIENT_SECRET": ""}, {}, "Sign-in with Google needs both"),
                                         ({"HOT_NETWORK_EGRESS_PROXY": "ftp://proxy.internal"}, {}, "must be an http://"),
                                         ({"HOT_EMAIL_SMTP_PASSWORD": "pw"}, {"HOT_EMAIL_PROVIDER": "smtp"}, "Set both")):
            with self.subTest(entry=entry):
                operator = dict(self.cloud.secrets[OPERATOR])
                self.cloud.secrets[OPERATOR].update(entry)
                self.config["settings"] = settings
                self.run.commands.clear()
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn(refusal, status["error"])
                self.assertIn("3.0.1 keeps running", status["recovery"])
                self.assertEqual(self.delivered(), before)
                self.assertEqual(self.run.compose_calls(), [])
                self.assertFalse(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands))
                self.cloud.secrets[OPERATOR] = operator
                self.config["settings"] = {}

    def test_a_deploy_refused_after_reading_the_level_leaves_the_delivered_secrets_too(self):
        # The installed bundle goes on mounting the pair, and Docker's next restart
        # of the app cannot mount a file that is gone.
        self.cloud.secrets[OPERATOR].update(HOT_GOOGLE_CLIENT_ID="google-id", HOT_GOOGLE_CLIENT_SECRET="google-secret")
        self.agent.reconcile()
        before = self.delivered()
        del self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"], self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_SECRET"]
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.manifests["3.2.0"] = manifest("3.2.0", latest=30, minimum_source="3.1.0")
        installed = self.config["storage"]
        s3 = {"backend": "s3", "bucket": "acme-files", "region": "us-east-1", "prefix": "files"}
        for release, level, storage, refusal in (
                ("3.2.0", 22, installed, "upgrades from 3.1.0 or later"),
                ("3.0.2", 24, installed, "knows migrations up to 23, but the database has 24"),
                ("3.0.2", 0, installed, "has no migrations"),
                ("3.0.2", 22, dict(installed, file=s3), "Change storage in a deploy of the release already running")):
            with self.subTest(refusal=refusal):
                self.cloud.parameters["pointer"] = release
                self.config["storage"] = storage
                self.run.migration_level = lambda level=level: level
                self.run.commands.clear()
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn(refusal, status["error"])
                self.assertIn("3.0.1 keeps running", status["recovery"])
                self.assertTrue(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands), "it read the level")
                self.assertEqual(self.run.compose_calls(), [])
                self.assertEqual(self.delivered(), before)
                self.assertEqual(undelivered_secrets(self.tmp.name), [])
        # A deploy that goes on delivers them all.
        self.config["storage"] = installed
        self.run.migration_level = lambda: 22
        self.manifests["3.0.2"] = manifest("3.0.2")
        self.cloud.parameters["pointer"] = "3.0.2"
        status = self.agent.reconcile(retry=True)
        self.assertEqual(status["state"], "deployed", status)
        self.assertNotIn("app/HOT_GOOGLE_CLIENT_ID", self.delivered())

    def test_an_entry_a_render_refuses_does_not_stop_a_reboot_or_a_restart(self):
        self.agent.reconcile()
        secrets = Path(self.tmp.name) / "secrets"
        for entry in ({"HOT_FILE_AWS_S3_BUCKET": "acme"}, {"HOT_FILE_AWS_SECRET_KEY": "secret-value"},
                      {"HOT_UNKNOWN_SETTING": "x"}, {"HOT_EMAIL_FROM": "hot@example.com"},
                      {"HOT_EMAIL_SMTP_PASSWORD_FILE": "/run/password"}, {"HOT_DB_URI": "postgres://elsewhere/hot"}):
            with self.subTest(entry=entry):
                self.cloud.secrets[OPERATOR] = {**TLS, **entry}
                shutil.rmtree(secrets)
                self.assertEqual(self.agent.resume()["state"], "resumed")
                self.assertEqual(self.agent.answer("restart-service", service="worker")["state"], "restarted")
                delivered = "".join(path.read_text() for path in secrets.glob("*/*"))
                self.assertFalse(any(value in delivered for value in entry.values()), "it goes to no service")
                self.assertEqual(undelivered_secrets(self.tmp.name), [])
        # A setting that is also in the bundle keeps the file the installed
        # bundle mounts, until a render says to keep one.
        self.cloud.secrets[OPERATOR] = {**TLS, "HOT_NETWORK_EGRESS_PROXY": "http://proxy.internal:3128"}
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.config["settings"] = {"HOT_NETWORK_EGRESS_PROXY": "http://proxy.internal:3128"}
        self.assertIn("in both settings", self.agent.reconcile()["error"])
        shutil.rmtree(secrets)
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(undelivered_secrets(self.tmp.name), [])

    BAD_PROXY = "The operator secret bundle entry HOT_NETWORK_EGRESS_PROXY must be an http:// or https:// proxy address"

    def test_a_proxy_hot_would_exit_on_stops_a_deploy_without_repeating_it(self):
        self.agent.reconcile()
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}  # so each attempt renders
        with mock.patch.object(agent_module.socket, "getaddrinfo", side_effect=OSError("no such host")):
            for proxy in ("user:proxy-password@proxy.internal:3128", "proxy-password.internal:3128",
                          "socks5://user:proxy-password@proxy.internal", "http://proxy-password.internal/path",
                          "http://user:proxy/password@proxy.internal:3128", "http://proxy-password.internal:port"):
                with self.subTest(proxy=proxy):
                    self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = proxy
                    self.run.commands.clear()
                    status = self.agent.reconcile(retry=True)
                    self.assertEqual(status["state"], "failed", status)
                    self.assertIn(self.BAD_PROXY, status["error"])
                    self.assertIn("keeps running", status["recovery"])
                    self.assertEqual(self.run.compose_calls(), [], "nothing stops")
                    for carried in (status, self.agent.state(), self.agent.history()):
                        self.assertNotIn("proxy-password", json.dumps(carried))
            self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://user:proxy-password@proxy.internal:3128"
            self.assertEqual(self.agent.reconcile(retry=True)["state"], "deployed")

    def test_deploy_now_refuses_a_proxy_rotated_to_one_hot_would_exit_on(self):
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "http://user:proxy-password@proxy.internal:3128"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        # The entry's name is still the one mounted, so nothing else asks for a render.
        rotated = "user:proxy-password@proxy.internal:3128"
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = rotated
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn(self.BAD_PROXY, report["error"])
        self.assertNotIn("proxy-password", json.dumps(report))
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")
        # Delivering refuses nothing, so a reboot still starts what is installed.
        secrets = Path(self.tmp.name) / "secrets"
        shutil.rmtree(secrets)
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual((secrets / "worker/HOT_NETWORK_EGRESS_PROXY").read_text(), rotated)

    def test_a_proxy_in_settings_is_refused_by_that_name(self):
        self.agent.reconcile()
        self.run.commands.clear()
        self.config["settings"] = {"HOT_NETWORK_EGRESS_PROXY": "proxy-password.internal:3128"}
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("settings entry HOT_NETWORK_EGRESS_PROXY must be an http:// or https:// proxy address",
                      status["error"])
        self.assertNotIn("proxy-password", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")


class S3Keys(AgentHarness):
    BUCKET = {"backend": "s3", "bucket": "acme", "region": "us-east-1"}

    def keys(self, role, token=False):
        prefix = f"HOT_{role.upper()}_AWS_"
        return {prefix + "ACCESS_KEY_ID": "AKIAEXAMPLEEXAMPLE00", prefix + "SECRET_ACCESS_KEY": "secret-" + role,
                **({prefix + "SESSION_TOKEN": "token-" + role} if token else {})}

    def test_keys_are_for_every_s3_role_or_for_none(self):
        both = {"build": dict(self.BUCKET, prefix="builds"), "file": dict(self.BUCKET, prefix="files")}
        config = {**self.config, "storage": both}
        self.assertEqual(agent_module.s3_authentication(config, {}), ("identity", False))
        self.assertEqual(agent_module.s3_authentication(config, {**self.keys("build"), **self.keys("file")}),
                         ("files", False))
        self.assertEqual(agent_module.s3_authentication(
            config, {**self.keys("build", token=True), **self.keys("file", token=True)}), ("files", True))
        with self.assertRaisesRegex(agent_module.AgentError,
                                    "lacks HOT_FILE_AWS_ACCESS_KEY_ID, HOT_FILE_AWS_SECRET_ACCESS_KEY"):
            agent_module.s3_authentication(config, self.keys("build"))
        with self.assertRaisesRegex(agent_module.AgentError, "lacks HOT_BUILD_AWS_SESSION_TOKEN$"):
            agent_module.s3_authentication(config, {**self.keys("build"), **self.keys("file", token=True)})
        # Keys for a role on the data disk leave the S3 role on the VM identity.
        mixed = {**self.config, "storage": {"build": {"backend": "local"}, "file": dict(self.BUCKET, prefix="files")}}
        self.assertEqual(agent_module.s3_authentication(mixed, self.keys("build")), ("identity", False))

    def test_s3_elsewhere_than_aws_takes_keys_since_the_vm_has_no_aws_identity(self):
        example = json.loads((ROOT / "examples/gcp-existing-s3.json").read_text())["installation"]
        with self.assertRaisesRegex(agent_module.AgentError, (
                "needs access keys on gcp, .* lacks HOT_BUILD_AWS_ACCESS_KEY_ID, HOT_BUILD_AWS_SECRET_ACCESS_KEY, "
                "HOT_FILE_AWS_ACCESS_KEY_ID, HOT_FILE_AWS_SECRET_ACCESS_KEY$")):
            agent_module.s3_authentication(example, {})
        self.assertEqual(agent_module.s3_authentication(example, {**self.keys("build"), **self.keys("file")}),
                         ("files", False))
        # Only the role on S3 is named, and other storage is none of this.
        one = {**example, "storage": {"build": {"backend": "local"}, "file": dict(self.BUCKET, prefix="files")}}
        with self.assertRaisesRegex(agent_module.AgentError, "lacks HOT_FILE_AWS_ACCESS_KEY_ID, "
                                                             "HOT_FILE_AWS_SECRET_ACCESS_KEY$"):
            agent_module.s3_authentication(one, {})
        gcs = {"backend": "gcs", "bucket": "acme", "prefix": "files"}
        self.assertEqual(agent_module.s3_authentication({**example, "storage": {"build": {"backend": "local"}, "file": gcs}}, {}),
                         ("identity", False))

    def test_s3_elsewhere_than_aws_without_keys_is_refused_before_anything_is_rendered(self):
        self.config.update(cloud="gcp", project="example-project",
                           storage={"build": dict(self.BUCKET, prefix="builds"), "file": dict(self.BUCKET, prefix="files")})
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("needs access keys on gcp", status["error"])
        self.assertEqual(list((Path(self.tmp.name) / "releases/3.0.1").glob("bundle*")), [])
        self.assertEqual(self.run.compose_calls(), [])

    def test_a_mixed_bundle_is_refused_before_anything_is_rendered(self):
        self.config["storage"] = {"build": dict(self.BUCKET, prefix="builds"),
                                  "file": dict(self.BUCKET, prefix="files")}
        self.cloud.secrets[OPERATOR].update(self.keys("file"))
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("lacks HOT_BUILD_AWS_ACCESS_KEY_ID", status["error"])
        self.assertEqual(list((Path(self.tmp.name) / "releases/3.0.1").glob("bundle*")), [])
        self.cloud.secrets[OPERATOR].update(self.keys("build"))
        self.assertEqual(self.agent.reconcile(retry=True)["state"], "deployed")
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        mounted = {secret["target"] for secret in compose["services"]["app"]["secrets"]}
        self.assertLessEqual({*self.keys("build"), *self.keys("file")}, mounted)
        for key in mounted:
            self.assertTrue((Path(self.tmp.name) / "secrets/app" / key).exists(), key)


class ExistingData(AgentHarness):
    URI = "postgres://hot_runtime:pw@db.internal.example:5432/hot?sslmode=verify-full"
    OWNER = "postgres://hot_owner:pw@db.internal.example:5432/hot?sslmode=verify-full"

    def setUp(self):
        super().setUp()
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR].update(HOT_DB_URI=self.URI, HOT_DB_MIGRATION_URI=self.OWNER,
                                            POSTGRES_CA_CERT="-----BEGIN CERTIFICATE-----")
        self.agent.resolve = lambda hosts: ["10.20.30.40/32"] if hosts else []

    def test_a_restart_refuses_an_empty_certificate_for_an_existing_database(self):
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.cloud.secrets[OPERATOR]["POSTGRES_CA_CERT"] = ""
        self.run.commands.clear()
        report = self.agent.answer("restart-service", "scheduler", request_id="r1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("entry POSTGRES_CA_CERT is empty", report["error"])
        self.assertEqual(self.run.compose_calls(), [])

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

    def test_every_secret_the_bundle_names_is_delivered(self):
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(undelivered_secrets(self.tmp.name), [])

    def test_the_level_is_read_with_the_login_the_operator_bundle_holds_now(self):
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        rotated = self.OWNER.replace(":pw@", ":rotated@")
        self.cloud.secrets[OPERATOR]["HOT_DB_MIGRATION_URI"] = rotated
        delivered = Path(self.tmp.name) / "secrets/migrate/HOT_DB_MIGRATION_URI"
        logins = []

        def level():
            logins.append(delivered.read_text())
            return 22
        self.run.migration_level = level
        self.cloud.parameters["pointer"] = "3.0.2"
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(len(logins), 1)
        self.assertIn(":rotated@", logins[0])

    def test_database_jobs_read_a_service_file_with_a_decoded_password(self):
        self.cloud.secrets[OPERATOR]["HOT_DB_MIGRATION_URI"] = self.OWNER.replace(":pw@", ":p%40ss%20word@")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        root = Path(self.tmp.name) / "secrets"
        for role in ("provision", "check-postgres"):
            self.assertEqual((root / role / "HOT_DB_MIGRATION_SERVICE").read_text(), (
                "[hot]\nhost=db.internal.example\nport=5432\ndbname=hot\nuser=hot_owner\npassword=p@ss word\n"
                "sslmode=verify-full\nsslrootcert=/run/secrets/POSTGRES_CA_CERT\n"))
            self.assertTrue((root / role / "POSTGRES_CA_CERT").exists())
            self.assertFalse((root / role / "HOT_DB_MIGRATION_URI").exists())
        self.assertIn("p%40ss%20word", (root / "migrate/HOT_DB_MIGRATION_URI").read_text(), "hot migrate keeps the URI")

    def test_a_connection_a_service_file_cannot_hold_is_refused_without_its_text(self):
        for owner in (self.OWNER + "&service=elsewhere",
                      "postgres://hot_owner:hidden/rest@db.internal.example:5432/hot?sslmode=verify-full",
                      self.OWNER.replace(":pw@", ":hidden%0Arest@")):
            with self.subTest(owner=owner):
                self.cloud.secrets[OPERATOR]["HOT_DB_MIGRATION_URI"] = owner
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertRegex(status["error"], "HOT_DB_MIGRATION_URI (cannot be written as a libpq service file|"
                                                  "is not a usable connection URI)")
                self.assertNotIn("hidden", status["error"])
                self.assertNotIn("elsewhere", status["error"])

    def test_a_repeated_or_blank_parameter_is_named_in_the_refusal(self):
        # The message is all the operator sees of why the file cannot be written.
        for owner in (self.OWNER + "&application_name=", self.OWNER + "&application_name=a&application_name=b"):
            with self.subTest(owner=owner):
                self.cloud.secrets[OPERATOR]["HOT_DB_MIGRATION_URI"] = owner
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn("HOT_DB_MIGRATION_URI cannot be written as a libpq service file", status["error"])
                self.assertIn("no repeated or blank query parameters", status["error"])

    def test_a_password_that_looks_like_a_port_is_refused_without_its_text(self):
        for name, uri in (("HOT_DB_URI", "postgres://hot_runtime:Sup3rS3cr/etPass@db.internal.example/hot?sslmode=verify-full"),
                          ("HOT_DB_URI", "postgres://hot_runtime:Sup3rS3cr[etPass@db.internal.example/hot?sslmode=verify-full"),
                          ("HOT_REDIS_URI", "rediss://:Sup3rS3cr/etPass@cache.internal.example")):
            with self.subTest(name=name, uri=uri):
                self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI
                self.config["data"].pop("valkey", None)
                if name == "HOT_REDIS_URI":
                    self.config["data"]["valkey"] = {"mode": "existing"}
                self.cloud.secrets[OPERATOR][name] = uri
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn(f"{name} is not a usable connection URI", status["error"])
                for carried in (status, self.agent.state(), self.agent.history(), self.cloud.statuses):
                    self.assertNotIn("Sup3rS3cr", json.dumps(carried))
                self.assertFalse(self.agent.dependencies_reachable())
                diagnostics = self.agent.command("diagnostics")["diagnostics"]
                self.assertIn(f"{name} is not a usable connection URI", diagnostics["dependencies_error"])
                self.assertNotIn("Sup3rS3cr", json.dumps(diagnostics))
                self.agent._dependencies = None

    def test_a_data_service_that_moves_is_denied_by_a_new_deploy(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertIsNone(self.agent.reconcile(), "the same address needs nothing")
        addresses[:] = ["10.20.30.41/32"]  # a failover
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        services = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())["services"]
        guest = dict(line.split("=", 1) for line in (Path(self.tmp.name) / "etc/guest-network.env").read_text().splitlines())
        for deny in (services["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"],
                     services["task-worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"], guest["HOT_GUEST_DENY_CIDRS"]):
            self.assertLessEqual({"10.20.30.40/32", "10.20.30.41/32"}, set(deny.split(",")))
        self.assertIsNone(self.agent.reconcile())
        addresses[:] = ["10.20.30.40/32"]  # and back, or a name that answers in turn
        self.assertIsNone(self.agent.reconcile(), "both are denied already")

        def unresolved(hosts):
            raise agent_module.AgentError("Cannot resolve the existing data service")
        self.agent.resolve = unresolved
        self.assertIsNone(self.agent.reconcile(), "a name that does not resolve now changes nothing")

    def test_a_data_service_that_moves_inside_its_denied_networks_needs_no_deploy(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        self.config["data"]["postgres"]["endpoint"] = {"deny_cidrs": ["10.20.30.0/24", "2001:db8:1::/48"]}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        compose = Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json"
        rendered = compose.read_text()
        for moved in ("10.20.30.41/32", "2001:db8:1::5/128", "::ffff:10.20.30.42/128"):  # a failover, or a promotion
            with self.subTest(moved=moved):
                addresses[:] = [moved]
                self.run.commands.clear()
                self.assertIsNone(self.agent.reconcile())
                self.assertEqual(self.run.compose_calls(), [], "no restart as the service recovers")
                self.assertEqual(compose.read_text(), rendered)
        # One address outside them is enough, whichever family it is.
        for moved in ("10.20.31.7/32", "2001:db8:2::5/128"):
            with self.subTest(moved=moved):
                addresses[:] = ["10.20.30.41/32", moved]
                status = self.agent.reconcile()
                self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
                services = json.loads(compose.read_text())["services"]
                self.assertIn(moved, services["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(","))
                self.assertIsNone(self.agent.reconcile())

    def test_a_new_host_address_still_redeploys_whatever_the_denied_networks(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        self.config["data"]["postgres"]["endpoint"] = {"deny_cidrs": ["10.68.0.0/16"]}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.run.interfaces = interfaces("10.68.1.21")  # inside them, and a replaced VM's own
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)

    def test_an_existing_service_that_moves_outside_a_new_network_is_denied_by_a_new_deploy(self):
        # In a peered network, say, which an allowlist can cover.
        self.config["network"] = {"mode": "new", "cidr": "10.70.0.0/16"}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertIsNone(self.agent.reconcile(), "the same address needs nothing")
        # The default range does not count on a network that has its own.
        addresses[:] = ["10.68.0.5/32"]
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        addresses[:] = ["10.20.30.41/32"]  # a failover
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        services = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())["services"]
        guest = dict(line.split("=", 1) for line in (Path(self.tmp.name) / "etc/guest-network.env").read_text().splitlines())
        for deny in (services["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"],
                     services["task-worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"], guest["HOT_GUEST_DENY_CIDRS"]):
            self.assertLessEqual({"10.20.30.40/32", "10.68.0.5/32", "10.20.30.41/32"}, set(deny.split(",")))
        self.assertIsNone(self.agent.reconcile())
        addresses[:] = ["10.20.30.40/32"]  # and back, or a name that answers in turn
        self.assertIsNone(self.agent.reconcile(), "both are denied already")

        def unresolved(hosts):
            raise agent_module.AgentError("Cannot resolve the existing data service")
        self.agent.resolve = unresolved
        self.assertIsNone(self.agent.reconcile(), "a name that does not resolve now changes nothing")

    def test_a_new_networks_host_address_needs_no_deploy_while_its_services_are_followed(self):
        # The host check alone stays off: the range holds the host, not the service.
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.run.interfaces = interfaces("10.68.1.21")
        self.run.commands.clear()
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.run.compose_calls(), [])
        addresses[:] = ["10.20.30.41/32"]
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)

    def test_a_data_service_that_moves_does_not_start_a_stopped_installation(self):
        # The operator stopped Hot to work on that database, say.
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.agent.command("drain-and-stop", request_id="s1")
        addresses[:] = ["10.20.30.41/32"]
        self.run.commands.clear()
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.run.compose_calls(), [], "no migrate, and nothing starts")
        self.assertTrue(self.agent.state()["stopped"])
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        self.assertNotIn("stopped", self.agent.state())
        self.assertIn("10.20.30.41/32", self.agent.state()["data_addresses"])

    def test_a_deploy_now_cut_short_after_an_address_changed_resumes(self):
        self.config["network"] = {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.agent.reconcile()
        self.agent.command("drain-and-stop", request_id="s1")
        addresses[:] = ["10.20.30.41/32"]
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "pull" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d1")
        self.agent.run_command = run
        self.assertEqual(self.agent.reconcile()["state"], "deployed", "the deploy asked for, not a new one")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_new_network_already_covers_a_data_service_that_moves(self):
        addresses = ["10.68.4.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        for moved in ("10.68.4.41/32", "::ffff:10.68.4.42/128"):
            with self.subTest(moved=moved):
                addresses[:] = [moved]
                self.run.commands.clear()
                self.assertIsNone(self.agent.reconcile())
                self.assertEqual(self.run.compose_calls(), [], "no restart as the service recovers")

    def test_a_new_network_and_a_services_denied_networks_both_count(self):
        self.config["data"]["postgres"]["endpoint"] = {"deny_cidrs": ["10.20.30.0/24"]}
        addresses = ["10.20.30.40/32"]
        self.agent.resolve = lambda hosts: list(addresses) if hosts else []
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        for moved in ("10.20.30.41/32", "10.68.4.40/32"):
            with self.subTest(moved=moved):
                addresses[:] = [moved]
                self.assertIsNone(self.agent.reconcile())
        addresses[:] = ["10.20.31.7/32"]
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)

    def test_a_deploy_from_before_data_addresses_were_recorded_is_left_alone(self):
        for network in ({"mode": "new"}, {"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"}):
            with self.subTest(network=network):
                self.config["network"] = network
                self.agent.reconcile()
                self.agent.save_state({k: v for k, v in self.agent.state().items() if k != "data_addresses"})
                self.assertIsNone(self.agent.reconcile())

    def test_unverified_or_mismatched_connections_are_refused(self):
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("verify-full", "require")
        self.assertIn("sslmode=verify-full", self.agent.reconcile()["error"])
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("hot_runtime", "postgres")
        self.assertIn("runtime_role", self.agent.reconcile(retry=True)["error"])
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI
        self.config["data"]["valkey"] = {"mode": "existing"}
        self.cloud.secrets[OPERATOR]["HOT_REDIS_URI"] = "redis://cache.internal:6379"
        self.assertIn("rediss://", self.agent.reconcile(retry=True)["error"])

    def test_every_ssl_mode_in_a_connection_is_read(self):
        # HOT_DB_URI goes straight to Hot's driver, which reads the last mode,
        # refuses a blank one only when it connects, and takes ssl-mode for sslmode.
        for query in ("sslmode=verify-full&sslmode=", "sslmode=verify-full&ssl%6Dode=", "sslmode=verify-full&sslmode",
                      "sslmode=verify-full&ssl-mode=disable", "sslmode=verify-full&ssl%2Dmode=disable",
                      "sslmode=disable&sslmode=verify-full", "ssl-mode=verify-full"):
            with self.subTest(query=query):
                self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("sslmode=verify-full", query)
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn("HOT_DB_URI must set sslmode=verify-full", status["error"])

    def test_database_settings_are_refused_for_local_data(self):
        self.config["data"] = {}
        self.assertIn("unsupported entry", self.agent.reconcile()["error"])

    def test_a_missing_connection_is_named_and_recorded(self):
        del self.cloud.secrets[OPERATOR]["HOT_DB_URI"]
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("An existing database needs HOT_DB_URI", status["error"])
        self.assertNotIn("deploying", self.agent.state())
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI
        self.config["data"]["valkey"] = {"mode": "existing"}
        self.assertIn("needs a rediss:// HOT_REDIS_URI", self.agent.reconcile(retry=True)["error"])
        diagnostics = self.agent.command("diagnostics")["diagnostics"]
        self.assertIn("HOT_REDIS_URI", diagnostics["dependencies_error"])

    def test_a_database_moved_to_another_host_is_probed_there(self):
        self.agent.reconcile()
        probed, clock = [], [1000.0]
        with mock.patch.object(agent_module, "tcp_reachable", lambda host, port: probed.append(host) or True), \
                mock.patch.object(agent_module.time, "monotonic", lambda: clock[0]):
            self.agent.dependencies_reachable()
            # The operator points HOT_DB_URI at a new host and restarts Hot from
            # another process, as AWS runs commands.
            self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("db.internal", "db2.internal")
            clock[0] += 60
            self.agent.dependencies_reachable()
            clock[0] += agent_module.DEPENDENCIES_SECONDS
            self.agent.dependencies_reachable()
            # A restart in this process reads it at once.
            self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = self.URI.replace("db.internal", "db3.internal")
            self.agent.command("restart-service", service="app")
            self.agent.dependencies_reachable()
        self.assertEqual(probed, ["db.internal.example", "db.internal.example", "db2.internal.example",
                                  "db3.internal.example"])

    def test_the_operator_parameters_are_kept_as_written(self):
        uri = agent_module.verified_postgres_uri(
            self.OWNER + "&options=-c%20search_path%3Dhot&application_name=Hot%20Migrate&sslrootcert=/etc/ca.pem",
            "HOT_DB_MIGRATION_URI", self.config)
        # libpq decodes only %XX: a space written as + would reach the server as +.
        self.assertIn("&options=-c%20search_path%3Dhot&application_name=Hot%20Migrate&", uri)
        self.assertNotIn("+", uri)
        self.assertTrue(uri.endswith("&sslrootcert=%2Frun%2Fsecrets%2FPOSTGRES_CA_CERT"), uri)
        self.assertNotIn("/etc/ca.pem", uri)


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

    def stopped_and_rebooted(self):
        """drain-and-stop, then a reboot: the tmpfs is empty, and the local data
        services are down, since resume leaves a stopped installation alone.
        From here Compose refuses what it cannot start."""
        self.agent.command("drain-and-stop", request_id="s1")
        secrets = Path(self.tmp.name) / "secrets"
        shutil.rmtree(secrets)
        (Path(self.tmp.name) / "resumed").unlink()
        self.assertIsNone(self.agent.resume())
        self.assertTrue(self.agent.boot_settled(), "nothing more is owed to this boot")
        run, started = self.agent.run_command, set()

        def strict(command, **kwargs):
            if "compose" in command:
                args = command[command.index("compose") + 3:]
                if args[0] in ("up", "run") and not (secrets / "worker/HOT_DB_URI").exists():
                    raise subprocess.CalledProcessError(1, command, stderr="bind source path does not exist")
                if args[0] == "up":
                    started.update(service for service in ("postgres", "valkey") if service in args)
                if ("exec" in args and "postgres" in args or "move-storage" in args) and "postgres" not in started:
                    raise subprocess.CalledProcessError(1, command, stderr="the database is not running")
            return run(command, **kwargs)
        self.agent.run_command = strict
        self.run.commands.clear()


# What a production scheduler logs, as Compose prints it, when a start finds
# another session holding its singleton lock.
SCHEDULER_STARTS = ('scheduler-1  | {"timestamp":"2026-11-02T12:00:00Z","level":"INFO","fields":{"message":"hot.dev: '
                    'SCHEDULER starting, version: 3.0.1 (abc1234)"},"target":"hot_cli::command::scheduler"}\n')
SCHEDULER_LOCK_HELD = ('scheduler-1  | {"timestamp":"2026-11-02T12:00:01Z","level":"ERROR","fields":{"message":"hot.dev: '
                       'Scheduler error: another hot_scheduler instance already holds the scheduler singleton lock"},'
                       '"target":"hot_cli::command::scheduler"}\n')


class ServicesStayUp(UpgradeHarness):
    """Compose says "started", and readiness probes only the app and API."""

    def states(self, compose_file=None, **services):
        """`ps` from now on, for one bundle or for any: these services' states,
        and every other service running. A value may list states to go through."""
        run, looks = self.agent.run_command, {name: list(states) if isinstance(states, list) else [states]
                                               for name, states in services.items()}

        def watching(command, **kwargs):
            if "ps" not in command or (compose_file and compose_file not in " ".join(command)):
                return run(command, **kwargs)
            run(command, **kwargs)
            rows = {name: "running" for name in (*agent_module.HOT_SERVICES, "ingress")}
            for name, states in looks.items():
                rows[name] = states.pop(0) if len(states) > 1 else states[0]
            return subprocess.CompletedProcess(command, 0, stdout="\n".join(
                json.dumps({"Service": name, "State": state, "Health": ""}) for name, state in rows.items()))
        self.agent.run_command = watching

    def test_a_worker_that_keeps_restarting_fails_the_deploy_and_returns_to_the_installed_release(self):
        self.release("3.0.2")
        self.states("releases/3.0.2/", worker="restarting")
        self.run.logs = "worker-1  | starting\nworker-1  | Error: network.egress-proxy is not a URL\n"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("did not stay running: worker (restarting)", status["error"])
        self.assertIn("network.egress-proxy is not a URL", status["error"], "its container is about to be replaced")
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.agent.state()["release"], "3.0.1")
        calls = [" ".join(c) for c in self.run.commands if "up" in c and "scheduler" in c]
        self.assertTrue(all("releases/3.0.1/" in call for call in calls), "not this release's scheduler")

    def test_every_service_counts(self):
        for service in ("task-worker", "app", "api", "ingress", "scheduler"):
            for state in ("restarting", "exited", "dead"):
                with self.subTest(service=service, state=state):
                    run = self.agent.run_command
                    self.states(**{service: state})
                    with self.assertRaisesRegex(agent_module.AgentError, f"did not stay running: {service} .{state}."):
                        self.agent.start_services(Path(self.tmp.name) / "releases/3.0.1/bundle")
                    self.agent.run_command = run

    def test_a_box_pool_that_does_not_fit_is_a_warning_while_all_of_hot_runs(self):
        # A reboot or a replaced VM whose disk has too little room for the
        # pool to hold its space: Kata's containerd refuses to start, and the
        # task worker runs code tasks and leaves box tasks queued. The resume
        # finishes, and its warning says why box tasks do not run.
        self.run.pool_active = False
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        with free_space(512):
            report = self.agent.resume()
        self.assertEqual(report["state"], "resumed", report)
        self.assertIn("Kata's containerd did not start: its box pool holds its space", report["warning"])
        self.assertIn("Box tasks do not run on this host: they wait in the queue, and none fail, until it fits",
                      report["warning"])
        self.assertIn("Code tasks still run", report["warning"])
        self.assertIn("up -d scheduler", self.run.compose_calls())
        self.assertNotIn("resume_failed", self.agent.state())
        # A deploy goes on too, where its own start of containerd is refused.
        self.release("3.0.2")
        self.failing = "kata-containerd.service"
        with free_space(512):
            status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertIn("Kata's containerd did not start: its box pool holds its space", status["warning"])
        self.assertEqual(self.agent.state()["release"], "3.0.2")
        self.assertFalse((Path(self.tmp.name) / "etc/host-configured").exists(), "mount_data tries it again")
        # With room, containerd that does not start is still said; once it
        # runs, nothing is.
        with free_space(64 * 1024):
            self.assertIn("Kata's containerd is not running", self.agent.box_pool_warning())
        self.failing = None
        self.run.pool_active = True
        with free_space(64 * 1024):
            self.assertEqual(self.agent.box_pool_warning(), "")
        # A task worker that does not stay up fails the start, pool or not:
        # it no longer needs containerd to run.
        self.states(**{"task-worker": "restarting"})
        with free_space(512), self.assertRaisesRegex(agent_module.AgentError,
                                                     r"did not stay running: task-worker \(restarting\)"):
            self.agent.start_services(Path(self.tmp.name) / "releases/3.0.2/bundle")

    def test_a_start_that_failed_is_not_followed_by_the_scheduler(self):
        self.states(worker="restarting")
        self.run.commands.clear()
        with self.assertRaises(agent_module.AgentError):
            self.agent.start_services(Path(self.tmp.name) / "releases/3.0.1/bundle")
        self.assertNotIn("up -d scheduler", self.run.compose_calls())

    def test_a_reboot_that_finds_a_service_failing_reports_it_and_tries_again(self):
        self.states(worker="restarting")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        report = self.agent.resume()
        self.assertEqual(report["state"], "resume-failed")
        self.assertIn("did not stay running: worker", report["error"])
        self.assertIn("resume_failed", self.agent.state())

    def test_one_restart_is_not_a_failure(self):
        self.states(worker=["running", "restarting", "running"])
        self.agent.start_services(Path(self.tmp.name) / "releases/3.0.1/bundle")

    def test_a_service_that_goes_down_late_in_the_wait_is_seen(self):
        looks = ["running"] * (agent_module.SETTLE_LOOKS - 2) + ["restarting"]
        self.states(worker=looks)
        with self.assertRaisesRegex(agent_module.AgentError, "worker"):
            self.agent.check_stays_up(Path(self.tmp.name) / "releases/3.0.1/bundle", ["worker"])

    def test_the_wait_lasts_while_the_services_look_fine(self):
        slept = []
        self.agent.sleep = slept.append
        self.run.commands.clear()
        self.agent.check_stays_up(Path(self.tmp.name) / "releases/3.0.1/bundle", ["worker"])
        self.assertEqual(sum("ps" in c for c in self.run.commands), agent_module.SETTLE_LOOKS)
        self.assertEqual(sum(slept), (agent_module.SETTLE_LOOKS - 1) * agent_module.SETTLE_SECONDS)

    def test_a_one_off_job_a_reboot_left_behind_is_not_its_service(self):
        # Exited, under the service's name; `ps --all` sorts it after the real one.
        rows = [{"Service": "worker", "State": "running", "Health": "", "Name": "hot-example-worker-1",
                 "Labels": "com.docker.compose.oneoff=False,com.docker.compose.service=worker"},
                {"Service": "worker", "State": "exited", "Health": "", "Name": "hot-example-worker-run-0123",
                 "Labels": "com.docker.compose.oneoff=True,com.docker.compose.service=worker"}]
        bundle = Path(self.tmp.name) / "releases/3.0.1/bundle"
        for output in ("\n".join(json.dumps(row) for row in rows), json.dumps(rows)):
            with self.subTest(output=output):
                self.run.ps = output
                self.assertEqual(self.agent.service_states(bundle), {"worker": ("running", "")})
                self.agent.check_stays_up(bundle, ["worker"])

    def test_a_docker_that_does_not_answer_fails_nothing_here(self):
        # The recovery path runs this too, and must finish whatever happens.
        run = self.agent.run_command
        for error in (subprocess.TimeoutExpired(["ps"], 120), subprocess.CalledProcessError(1, ["ps"]), OSError("gone")):
            with self.subTest(error=error):
                def silent(command, error=error, **kwargs):
                    if "ps" in command:
                        raise error
                    return run(command, **kwargs)
                self.agent.run_command = silent
                self.run.commands.clear()
                with mock.patch("sys.stderr") as stderr:
                    self.agent.start_services(Path(self.tmp.name) / "releases/3.0.1/bundle")
                self.assertIn("services_not_checked", "".join(str(c) for c in stderr.write.call_args_list))
                self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")

    def test_a_log_that_cannot_be_read_leaves_the_failure(self):
        self.states(worker="restarting")
        run = self.agent.run_command

        def hanging(command, **kwargs):
            if "logs" in command:
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return run(command, **kwargs)
        self.agent.run_command = hanging
        with self.assertRaisesRegex(agent_module.AgentError, r"did not stay running: worker \(restarting\)$"):
            self.agent.check_stays_up(Path(self.tmp.name) / "releases/3.0.1/bundle", ["worker"])

    def test_the_log_in_a_failure_is_redacted_and_bounded(self):
        self.states(worker="restarting")
        password = self.cloud.secrets[GENERATED]["POSTGRES_PASSWORD"]
        self.run.logs = (f"worker-1  | connecting with {password}\n" + "worker-1  | " + "x" * 5000 + "\n"
                         "worker-1  | postgres://hot_owner:hunter2hunter2@db/hot refused\n")
        with self.assertRaises(agent_module.AgentError) as caught:
            self.agent.check_stays_up(Path(self.tmp.name) / "releases/3.0.1/bundle", ["worker"])
        message = str(caught.exception)
        self.assertNotIn(password, message)
        self.assertNotIn("hunter2hunter2", message)
        self.assertLess(len(message), 2 * agent_module.ERROR_CHARS)


    def bundle(self):
        return Path(self.tmp.name) / "releases/3.0.1/bundle"

    def test_a_scheduler_waiting_for_another_sessions_lock_is_not_failing(self):
        self.states(scheduler="restarting")
        self.run.logs = (SCHEDULER_STARTS + SCHEDULER_LOCK_HELD) * 3
        with mock.patch("sys.stderr") as stderr:
            self.agent.start_services(self.bundle())
        self.assertIn("scheduler_waiting", "".join(str(c) for c in stderr.write.call_args_list))

    def test_a_scheduler_caught_as_its_next_start_begins_is_still_waiting(self):
        self.states(scheduler="restarting")
        self.run.logs = SCHEDULER_STARTS + SCHEDULER_LOCK_HELD + SCHEDULER_STARTS
        self.agent.check_stays_up(self.bundle(), ["scheduler"])

    def test_the_lock_message_matches_what_the_scheduler_says(self):
        crates = ROOT.parent / "crates"
        self.assertIn(agent_module.SCHEDULER_LOCK_HELD, (crates / "hot_scheduler/src/server.rs").read_text())
        self.assertIn(agent_module.SCHEDULER_STARTING, (crates / "hot_cli/src/command/scheduler.rs").read_text())
        self.assertIn(agent_module.SCHEDULER_LOCK_HELD, SCHEDULER_LOCK_HELD)

    def test_a_reboot_with_a_dead_sessions_lock_still_held_resumes(self):
        # On a managed or existing database, the crash that rebooted the host
        # left its session holding the lock until the server drops it.
        self.states(scheduler="restarting")
        self.run.logs = SCHEDULER_STARTS + SCHEDULER_LOCK_HELD
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertNotIn("resume_failed", self.agent.state())
        self.run.commands.clear()
        self.assertIsNone(self.agent.resume())
        self.assertEqual(self.run.compose_calls(), [], "no retry that recreates the ingress")

    def test_a_scheduler_that_crashes_for_another_reason_still_fails(self):
        crash = "scheduler-1  | Error: the configuration is not valid\n"
        for logs in (crash, SCHEDULER_LOCK_HELD + SCHEDULER_STARTS + crash, "", SCHEDULER_STARTS):
            with self.subTest(logs=logs):
                run = self.agent.run_command
                self.states(scheduler="restarting")
                self.run.logs = logs
                with self.assertRaisesRegex(agent_module.AgentError, r"did not stay running: scheduler \(restarting\)"):
                    self.agent.check_stays_up(self.bundle(), ["scheduler"])
                self.agent.run_command = run

    def test_a_scheduler_docker_will_not_restart_still_fails(self):
        self.states(scheduler="exited")
        self.run.logs = SCHEDULER_LOCK_HELD
        with self.assertRaisesRegex(agent_module.AgentError, r"did not stay running: scheduler \(exited\)"):
            self.agent.check_stays_up(self.bundle(), ["scheduler"])

    def test_only_the_scheduler_waits_for_a_lock(self):
        self.states(worker="restarting")
        self.run.logs = SCHEDULER_LOCK_HELD
        with self.assertRaisesRegex(agent_module.AgentError, r"did not stay running: worker"):
            self.agent.check_stays_up(self.bundle(), ["worker"])

    def test_a_log_that_cannot_be_read_says_nothing_of_a_lock(self):
        self.states(scheduler="restarting")
        run = self.agent.run_command

        def hanging(command, **kwargs):
            if "logs" in command:
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return run(command, **kwargs)
        self.agent.run_command = hanging
        with self.assertRaisesRegex(agent_module.AgentError, r"did not stay running: scheduler"):
            self.agent.check_stays_up(self.bundle(), ["scheduler"])


class Upgrades(UpgradeHarness):
    def killed_at(self, word):
        """The agent dies as Compose is told `word`, and systemd restarts it. The
        host stays up, so the tmpfs and every container stay as they were."""
        run = self.agent.run_command

        def crash(command, **kwargs):
            if word in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run

    def test_a_kill_after_the_scheduler_stopped_then_an_early_failure_restarts_it(self):
        self.release("3.0.2")
        self.killed_at("--wait")
        self.assertIn("scheduler-stopped", self.agent.state()["deploying"]["steps"])
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("HTTP 404", status["error"])
        self.assertIn("previous-scheduler-restarted", status["steps"])
        restart = self.run.commands[-1]
        self.assertIn(str(Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json"), restart)
        self.assertEqual(restart[-3:], ["up", "-d", "scheduler"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_kill_inside_the_schedulers_stop_then_an_early_failure_restarts_it(self):
        # The stop may have taken effect, and no step after it was saved.
        self.release("3.0.2")
        self.killed_at("stop")
        steps = self.agent.state()["deploying"]["steps"]
        self.assertEqual(steps[-1], "scheduler-stopping", steps)
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("previous-scheduler-restarted", status["steps"])
        self.assertEqual(self.run.commands[-1][-3:], ["up", "-d", "scheduler"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_schedulers_stop_that_failed_after_taking_effect_restarts_it(self):
        self.release("3.0.2")
        self.failing = "stop"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("previous-scheduler-restarted", status["steps"])
        self.assertEqual(self.run.commands[-1][-3:], ["up", "-d", "scheduler"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def start_due_again(self):
        """Time passes until a failed start of the installed release is due."""
        state = self.agent.state()
        state["resume_failed"]["at"] = "2026-10-29T12:00:00+00:00"
        self.agent.save_state(state)

    def test_a_restart_that_failed_is_tried_again(self):
        self.release("3.0.2")
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("3.0.1 did not start again", status["recovery"])
        self.assertIn("tried again every 5 minutes", status["recovery"])
        self.assertIn("run roll-back to start 3.0.1", status["recovery"])
        failure = self.agent.state()["resume_failed"]
        self.assertEqual(failure["release"], "3.0.1")
        self.assertIn("did not become ready", failure["error"])
        # Too soon, and then due, while the cause lasts.
        self.run.commands.clear()
        self.assertIsNone(self.agent.resume())
        self.assertEqual(self.run.compose_calls(), [])
        self.start_due_again()
        self.assertEqual(self.agent.resume()["state"], "resume-failed")
        # Another failure does not say 3.0.1 runs.
        self.cloud.parameters["pointer"] = "3.0.3"  # no such manifest
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertNotIn("keeps running", status["recovery"])
        self.assertIn("3.0.1 has not started again", status["recovery"])
        # Once the cause clears, the loop starts it whole.
        self.agent.wait_ready = lambda: None
        self.start_due_again()
        self.run.commands.clear()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("resume_failed", self.agent.state())

    def test_a_restart_that_failed_takes_deploy_now_at_once(self):
        self.release("3.0.2")
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        self.agent.reconcile()
        self.agent.wait_ready = lambda: None
        self.release("3.0.1")
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        self.assertNotIn("resume_failed", self.agent.state())

    def test_a_start_after_a_boot_that_was_killed_is_tried_again(self):
        secrets = Path(self.tmp.name) / "secrets"
        shutil.rmtree(secrets)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "--remove-orphans" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.resume()
        self.agent.run_command = run
        self.assertTrue(any(secrets.iterdir()), "the next cycle finds the secrets delivered")
        self.assertEqual(self.agent.state()["resume_failed"]["error"], "The start did not finish")
        # So a failure in the meantime does not say it runs, and the start is due later.
        self.cloud.parameters["pointer"] = "3.0.3"  # no such manifest
        self.assertNotIn("keeps running", self.agent.reconcile()["recovery"])
        self.assertIsNone(self.agent.resume())
        self.start_due_again()
        self.run.commands.clear()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("resume_failed", self.agent.state())

    def test_a_reboot_mid_deploy_then_a_failed_restart_is_tried_again(self):
        self.release("3.0.2")
        self.killed_at("--wait")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["started"], "data services")
        del self.manifests["3.0.2"]
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("3.0.1 did not start again", status["recovery"])
        self.assertIn("resume_failed", self.agent.state())
        self.agent.wait_ready = lambda: None
        self.start_due_again()
        self.run.commands.clear()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        calls = self.run.compose_calls()
        self.assertIn("up -d --remove-orphans app api worker task-worker", calls)
        self.assertEqual(calls[-1], "up -d scheduler")

    def test_a_kill_before_anything_stopped_then_an_early_failure_restarts_nothing(self):
        self.release("3.0.2")
        self.killed_at("pull")
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual(self.run.compose_calls(), [])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_second_kill_still_knows_what_the_first_stopped(self):
        self.release("3.0.2")
        self.killed_at("--wait")
        self.killed_at("pull")
        self.assertEqual(self.agent.state()["deploying"]["cut_short"], ["scheduler-stopped"])
        del self.manifests["3.0.2"]
        status = self.agent.reconcile()
        self.assertIn("previous-scheduler-restarted", status["steps"])

    def test_a_kill_of_the_new_services_then_an_early_failure_restarts_the_installed_ones(self):
        self.release("3.0.2")
        self.killed_at("--remove-orphans")
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("up -d --remove-orphans app api worker task-worker", self.run.compose_calls())
        self.assertIn("releases/3.0.1/bundle/compose.json",
                      " ".join(next(c for c in self.run.commands if "--remove-orphans" in c)))
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_kill_during_migrations_then_a_failed_retry_says_nothing_of_stopping(self):
        # Only the scheduler stopped, as in any migration that fails.
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        self.failing = "migrate"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertTrue(status["recovery"].startswith("Migrations may be partly applied"), status["recovery"])
        self.assertNotIn("stopped", self.agent.state())
        self.assertFalse(any(c.startswith("stop --timeout") for c in self.run.compose_calls()))

    def test_a_reboot_during_migrations_then_a_failed_retry_keeps_hot_stopped_and_says_so(self):
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        point = self.agent.state()["deploying"]["restore_point"]
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["started"], "data services")
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertTrue(status["recovery"].startswith("Hot stays stopped. Migrations may be partly applied"), status)
        self.assertIn(point["path"], status["recovery"])
        self.assertTrue(self.agent.state()["stopped"])
        self.assertIsNone(self.agent.resume(), "another boot does not start it either")
        self.failing = None
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertIn("restored", report["steps"])
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_reboot_during_migrations_no_longer_asked_for_keeps_hot_stopped_too(self):
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.agent.resume()
        self.release("3.0.1")
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertTrue(status["recovery"].startswith("Hot stays stopped. Migrations may be partly applied"), status)
        self.assertTrue(self.agent.state()["stopped"])
        self.assertNotIn("deploying", self.agent.state())
        self.assertIn("restored", self.agent.command("roll-back")["steps"])

    def test_a_failed_start_after_a_reboot_during_migrations_stops_what_it_started(self):
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.agent.resume()
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("starting", status["steps"])
        self.assertTrue(status["recovery"].startswith("Hot stays stopped."), status["recovery"])
        calls = self.run.compose_calls()
        self.assertEqual(calls[-1], "stop app api ingress")
        self.assertIn("stop --timeout 120 worker task-worker", calls)

    def test_a_deploy_cut_short_then_a_failure_before_the_next_begins_puts_its_bundle_back(self):
        bundle = Path(self.tmp.name) / "releases/3.0.1/bundle"
        installed = (bundle / "compose.json").read_text()
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.killed_at("pull")
        self.assertNotEqual((bundle / "compose.json").read_text(), installed)
        self.release("3.0.2")
        self.agent.prepare_host = mock.Mock(side_effect=agent_module.AgentError("GetSecretValue failed: Throttling"))
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual((bundle / "compose.json").read_text(), installed)
        self.assertFalse(bundle.with_name("bundle-running").exists())

    def failed_resume(self):
        """A boot's start of the installed release failed, and will be tried again."""
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.failing = "--remove-orphans"
        self.assertEqual(self.agent.resume()["state"], "resume-failed")
        self.failing = None

    def test_a_failure_while_a_start_after_a_boot_is_failing_does_not_say_it_runs(self):
        self.failed_resume()
        self.cloud.parameters["pointer"] = "3.0.2"  # no such manifest
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertNotIn("keeps running", status["recovery"])
        self.assertIn("3.0.1 has not started again", status["recovery"])
        self.assertIn("tried again every 5 minutes", status["recovery"])
        self.assertIn("resume_failed", self.agent.state(), "the retry goes on")

    def test_a_restart_that_worked_says_it_runs_though_the_boots_start_failed(self):
        self.failed_resume()
        self.release("3.0.2")  # the same migration level
        checks = []

        def ready():
            checks.append(1)
            if len(checks) == 1:
                raise agent_module.AgentError("The app and API did not become ready")
        self.agent.wait_ready = ready
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_backup_after_a_reboot_while_stopped_starts_only_the_data_services(self):
        self.stopped_and_rebooted()
        report = self.agent.answer("backup", request_id="b1")
        self.assertEqual(report["state"], "backed-up", report)
        calls = self.run.compose_calls()
        self.assertLess(calls.index("up -d --wait --no-recreate postgres valkey"),
                        next(i for i, c in enumerate(calls) if "pg_dump" in c))
        self.assertFalse(any(c.startswith("up -d") and "--wait" not in c for c in calls), calls)
        self.assertTrue(self.agent.state()["stopped"], "Hot stays stopped for the backup")

    def mounts_late(self):
        """The disk was not ready when this cycle read the state, and mounts when
        the deploy asks for it, holding the state of what is installed."""
        path = self.agent.state_path()
        on_disk = path.read_text()
        path.unlink()
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        run = self.agent.run_command

        def mounting(command, **kwargs):
            if command[0].endswith("hot-data-disk"):
                path.write_text(on_disk)
            return run(command, **kwargs)
        self.agent.run_command = mounting
        self.run.commands.clear()

    def test_an_upgrade_finding_the_disk_mounts_late_still_upgrades_from_what_is_installed(self):
        self.release("3.0.2", latest=23)
        self.mounts_late()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.2"), status)
        record = self.agent.history()[-1]
        self.assertEqual(record["previous"], "3.0.1")
        self.assertEqual(record["restore_point"]["kind"], "dump")
        self.assertIn("restore-point", record["steps"])
        self.assertEqual(self.agent.state()["previous"], "3.0.1")
        self.assertLess(next(i for i, c in enumerate(self.run.compose_calls()) if c == "up -d scheduler"),
                        next(i for i, c in enumerate(self.run.compose_calls()) if "pg_dump" in c),
                        "the installed release came back first, as a reboot brings it back")

    def test_a_disk_that_mounts_late_still_enforces_the_upgrade_rules(self):
        self.release("3.2.0", minimum_source="3.1.0")
        self.mounts_late()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("upgrades from 3.1.0 or later, and this installation runs 3.0.1", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_disk_that_mounts_late_on_the_release_asked_for_deploys_nothing(self):
        self.mounts_late()
        self.assertIsNone(self.agent.reconcile())
        calls = self.run.compose_calls()
        self.assertEqual(calls[-1], "up -d scheduler", "the installed release started")
        self.assertFalse(any("migrate" in c for c in calls), calls)
        self.assertEqual(self.agent.state()["release"], "3.0.1")

    def test_a_failure_as_the_disk_mounts_late_knows_what_is_installed(self):
        self.mounts_late()
        self.release("3.0.2")
        configure = self.agent.configure_host

        def refused():
            raise agent_module.AgentError("GetSecretValue failed: ThrottlingException")
        self.agent.configure_host = refused
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertNotIn("Nothing was installed", status["recovery"])
        self.agent.configure_host = configure

    def test_a_failed_release_waits_for_deploy_now(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("Migrations may be partly applied", status["recovery"])
        self.assertIn("run roll-back: it restores the database from ", status["recovery"])
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

    def test_a_deploy_now_sent_before_a_failure_answers_with_that_failure(self):
        # The loop deployed the new pointer first, and the command waited
        # behind that attempt, so nothing was fixed in between.
        sent = agent_module.utc_now()
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        first = self.agent.reconcile()
        self.assertEqual(first["state"], "failed")
        self.failing = None
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="release-1", sent=sent)
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("failed", "3.0.2", "release-1"))
        self.assertEqual((report["error"], report["steps"], report["recovery"]),
                         (first["error"], first["steps"], first["recovery"]))
        self.assertEqual(self.run.commands, [], "nothing runs again before the cause is fixed")
        self.assertEqual(self.cloud.statuses[-1]["request_id"], "release-1")
        # As Pub/Sub writes its times.
        published = sent.replace("+00:00", ".123456789Z")
        self.assertEqual(self.agent.answer("deploy-now", sent=published)["state"], "failed")
        self.assertEqual(self.run.commands, [])
        # Sent after the failure: the documented way to retry.
        recorded = agent_module.parse_time(self.agent.state()["failed"]["at"])
        later = (recorded + datetime.timedelta(seconds=1)).isoformat()
        self.assertEqual(self.agent.answer("deploy-now", sent=later)["state"], "deployed")

    def test_a_deploy_now_from_before_a_failure_still_deploys_a_newer_pointer(self):
        sent = agent_module.utc_now()
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.release("3.0.3", latest=23)
        report = self.agent.answer("deploy-now", sent=sent)
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.3"), report)

    def test_a_failure_recorded_without_a_time_is_retried(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        state = self.agent.state()
        self.assertIn("at", state["failed"])
        del state["failed"]["at"]
        self.agent.save_state(state)
        self.assertEqual(self.agent.answer("deploy-now", sent=agent_module.utc_now())["state"], "deployed")

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

    def test_a_failure_after_drain_and_stop_keeps_hot_stopped(self):
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.2")
        self.failing = "pull"
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed")
        self.assertIn("Hot stays stopped, as it was before this deploy", status["recovery"])
        self.assertNotIn("keeps running", status["recovery"])
        # Reading the database's level may start its data services, and only those.
        self.assertEqual([c for c in self.run.compose_calls() if c.startswith("up") and "--no-recreate postgres valkey" not in c], [])
        self.assertTrue(self.agent.state()["stopped"])
        # Pointing back at the installed release starts it again.
        self.failing = None
        self.release("3.0.1")
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"))
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_release_without_migrations_that_fails_restarts_the_installed_one(self):
        self.release("3.0.2")  # the same migration level
        checks = []

        def ready():
            checks.append(1)
            if len(checks) == 1:
                raise agent_module.AgentError("The app and API did not become ready")
        self.agent.wait_ready = ready
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertNotIn("migrating", status["steps"], "the database was only checked")
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("The database was not changed and 3.0.1 keeps running", status["recovery"])
        restart = [c for c in self.run.commands if "--remove-orphans" in c][-1]
        self.assertIn(str(Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json"), restart)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertEqual(report["steps"], ["started"])
        self.assertFalse(any("pg_restore" in c for c in self.run.commands))

    def test_a_retry_that_fails_early_still_knows_the_migrations_started(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        point = self.agent.state()["failed"]["restore_point"]
        self.failing = "pull"
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertNotIn("migrating", status["steps"])
        self.assertIn("Migrations may be partly applied", status["recovery"])
        self.assertIn(point["path"], status["recovery"])
        self.assertEqual(self.agent.state()["failed"]["partly_migrated"], "3.0.2")
        self.failing = None
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertIn("restored", report["steps"])
        restore = next(c for c in self.run.commands if "pg_restore" in c)
        self.assertIn("releases/3.0.1/bundle", " ".join(restore))

    def test_a_retry_that_fails_before_its_deploy_begins_still_knows_the_migrations_started(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        point = self.agent.state()["failed"]["restore_point"]
        self.failing = None
        prepare = self.agent.prepare_host
        self.agent.prepare_host = mock.Mock(side_effect=agent_module.AgentError("GetSecretValue failed: Throttling"))
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((status["state"], status["steps"]), ("failed", []))
        self.assertIn("Migrations may be partly applied", status["recovery"])
        self.assertNotIn("not changed", status["recovery"])
        failed = self.agent.state()["failed"]
        self.assertEqual((failed["partly_migrated"], failed["restore_point"]), ("3.0.2", point))
        self.agent.prepare_host = prepare
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertIn("restored", report["steps"])
        self.assertTrue(any("pg_restore" in c for c in self.run.commands))

    def test_a_deploy_of_another_release_that_fails_first_keeps_what_the_interrupted_one_left(self):
        self.release("3.0.2", latest=23)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "migrate" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        point = self.agent.state()["deploying"]["restore_point"]
        self.release("3.0.3", latest=24)
        self.agent.prepare_host = mock.Mock(side_effect=agent_module.AgentError("GetSecretValue failed: Throttling"))
        status = self.agent.reconcile()
        self.assertIn("Migrations may be partly applied", status["recovery"])
        failed = self.agent.state()["failed"]
        self.assertEqual((failed["release"], failed["partly_migrated"], failed["restore_point"]), ("3.0.3", "3.0.2", point))
        self.assertNotIn("deploying", self.agent.state())

    def test_a_reboot_during_a_deploy_of_another_release_still_restarts_the_installed_one(self):
        self.release("3.0.2")
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "pull" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.agent.resume()
        self.release("3.0.3")
        self.agent.prepare_host = mock.Mock(side_effect=agent_module.AgentError("GetSecretValue failed: Throttling"))
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"], "only the data services ran again")
        self.assertIn("up -d --remove-orphans app api worker task-worker", self.run.compose_calls())

    def test_a_fix_forward_keeps_the_restore_point_from_before_the_first_attempt(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        point = self.agent.state()["failed"]["restore_point"]
        self.release("3.0.3", latest=24)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertFalse(any("pg_dump" in c for c in self.run.commands), "the database is already partly migrated")
        self.assertIn(f"run roll-back: it restores the database from {point['path']} and starts 3.0.1", status["recovery"])
        self.assertEqual(self.agent.state()["failed"]["restore_point"], point)

    def test_a_serving_release_is_recorded_though_the_operator_secret_is_unreadable(self):
        self.release("3.0.2", latest=23)
        start = self.agent.start_services

        def start_then_throttle(bundle):
            start(bundle)

            def throttled(reference):
                raise agent_module.AgentError("secretsmanager GetSecretValue failed: ThrottlingException")
            self.cloud.get_secret = throttled
        self.agent.start_services = start_then_throttle
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.agent.state()["release"], "3.0.2")
        self.assertNotIn("failed", self.agent.state())

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

    def test_a_dump_that_would_fill_the_data_disk_is_refused(self):
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        with mock.patch.object(agent_module.shutil, "disk_usage", return_value=mock.Mock(free=2**20)):
            status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("A database dump needs about 256 MB, and the data disk has 1 MB free", status["error"])
        self.assertFalse(any("pg_dump" in c for c in self.run.commands), "nothing is dumped")
        self.assertIn("The database was not changed and 3.0.1 keeps running", status["recovery"])

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


class Hotbox(UpgradeHarness):
    """Each release's task worker gets that release's hotbox (F166)."""

    IMAGE = "registry.example.com/hot@sha256:"

    def setUp(self):
        super().setUp()
        self.file = Path(self.tmp.name) / "kata/hotbox"

    def own(self, character="a"):
        return f"hotbox of {self.IMAGE}{character * 64}"

    def release_with_its_own(self, version, character, **fields):
        self.release(version, **fields)
        self.manifests[version]["images"]["hot"] = self.IMAGE + character * 64

    def task_worker_starts(self):
        """Run compose as before, and note the hotbox in place at each start of a task worker."""
        run, seen = self.agent.run_command, []

        def noting(command, **kwargs):
            if "compose" in command and "up" in command and "task-worker" in command:
                seen.append(self.file.read_text() if self.file.exists() else None)
            return run(command, **kwargs)
        self.agent.run_command = noting
        return seen

    def test_a_first_deploy_installs_it_executable(self):
        self.assertEqual(self.file.read_text(), self.own())
        self.assertEqual(self.file.stat().st_mode & 0o777, 0o755)
        self.assertEqual([p.name for p in self.file.parent.iterdir()], ["hotbox"], "no copy is left beside it")

    def test_an_upgrade_replaces_it_only_as_the_new_task_worker_starts(self):
        self.release_with_its_own("3.0.2", "b", latest=23)
        seen, run = self.task_worker_starts(), self.agent.run_command
        at_migration = []

        def noting(command, **kwargs):
            if "migrate" in command:
                at_migration.append(self.file.read_text())
            return run(command, **kwargs)
        self.agent.run_command = noting
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(at_migration, [self.own()], "the old task worker still runs")
        self.assertEqual(seen, [self.own("b")])
        self.assertEqual(self.file.read_text(), self.own("b"))

    def test_it_comes_from_the_container_and_leaves_none_behind(self):
        self.release_with_its_own("3.0.2", "b")
        self.run.commands.clear()
        self.agent.reconcile()
        docker = [c[1:] for c in self.run.commands if c[0] == "docker" and c[1] != "compose"]
        name = agent_module.HOTBOX_CONTAINER
        # Checked before anything stops, then installed as the services start.
        self.assertEqual(docker[-3:], [["create", "--name", name, self.IMAGE + "b" * 64],
                                       ["cp", f"{name}:{agent_module.HOTBOX_IN_IMAGE}", str(self.file.with_name(".hotbox.new"))],
                                       ["rm", "--force", name]])
        self.assertEqual(docker[-4], ["rm", "--force", name], "one a kill left would hold the name")

    def test_an_image_without_it_stops_the_deploy_before_anything_stops(self):
        self.release_with_its_own("3.0.2", "b", latest=23)
        self.run.no_hotbox = True
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not copy hotbox out of the runtime image of 3.0.2", status["error"])
        self.assertIn("Could not find the file", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), ["--profile admin pull --quiet"], "nothing stops")
        self.assertEqual(self.file.read_text(), self.own())
        self.assertFalse(self.file.with_name(".hotbox.new").exists())

    def test_a_data_disk_that_is_not_mounted_stops_the_deploy_before_anything_lands_under_it(self):
        self.release_with_its_own("3.0.2", "b", latest=23)
        self.run.commands.clear()
        with mock.patch.object(agent_module.Agent, "data_disk_mounted", lambda agent: False):
            status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not copy hotbox out of the runtime image of 3.0.2: the data disk is not mounted", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertFalse(any(c[:2] == ["docker", "create"] for c in self.run.commands))
        self.assertEqual(self.run.compose_calls(), ["--profile admin pull --quiet"], "nothing stops")
        self.assertEqual(self.file.read_text(), self.own())

    def test_an_empty_copy_is_no_copy(self):
        self.release_with_its_own("3.0.2", "b")
        run = self.agent.run_command

        def empty(command, **kwargs):
            if command[:2] == ["docker", "cp"]:
                self.run.commands.append(command)
                Path(command[-1]).write_text("")
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
            return run(command, **kwargs)
        self.agent.run_command = empty
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("the copy is empty", status["error"])
        self.assertEqual(self.file.read_text(), self.own())

    def test_a_copy_that_fails_as_services_start_starts_none_of_that_release(self):
        self.release_with_its_own("3.0.2", "b")
        run, copies = self.agent.run_command, []

        def failing_second(command, **kwargs):
            if command[:2] == ["docker", "cp"] and self.run.hotbox_image.endswith("b" * 64):
                copies.append(command)
                if len(copies) == 2:
                    raise subprocess.CalledProcessError(1, command, stderr="no space left on device")
            return run(command, **kwargs)
        self.agent.run_command = failing_second
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not copy hotbox", status["error"])
        self.assertIn("no space left on device", status["error"])
        self.assertFalse(any("releases/3.0.2/bundle" in " ".join(c) and "--remove-orphans" in c for c in self.run.commands),
                         "no service of 3.0.2 starts")
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertEqual(self.file.read_text(), self.own(), "3.0.1 runs with its own")

    def test_a_failed_start_puts_the_installed_releases_back(self):
        self.release_with_its_own("3.0.2", "b")
        checks = []

        def ready():
            checks.append(self.file.read_text())
            if len(checks) == 1:
                raise agent_module.AgentError("The app and API did not become ready")
        self.agent.wait_ready = ready
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertEqual(checks, [self.own("b"), self.own()])
        self.assertEqual(self.file.read_text(), self.own())

    def test_pointing_at_an_earlier_release_of_the_same_level_puts_its_back(self):
        self.release_with_its_own("3.0.2", "b")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.file.read_text(), self.own("b"))
        self.release("3.0.1")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.file.read_text(), self.own())

    def wrong_one_in_place(self):
        self.file.write_text("hotbox of another release")
        self.run.commands.clear()
        return self.task_worker_starts()

    def test_a_reboot_starts_with_the_installed_releases(self):
        seen = self.wrong_one_in_place()
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(seen, [self.own()])

    def test_a_replaced_vm_starts_with_the_installed_releases(self):
        # The data disk holds it, and only a restored or older disk would not.
        seen = self.wrong_one_in_place()
        self.file.unlink()
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(seen, [self.own()])

    def test_a_copy_that_fails_after_a_reboot_starts_nothing_and_is_tried_again(self):
        self.file.write_text("hotbox of another release")
        self.run.no_hotbox = True
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        report = self.agent.resume()
        self.assertEqual(report["state"], "resume-failed", report)
        self.assertIn("Could not copy hotbox", report["error"])
        self.assertFalse(any("task-worker" in c for c in self.run.compose_calls()))
        self.assertIn("resume_failed", self.agent.state())

    def test_starting_a_stopped_installation_uses_the_installed_releases(self):
        self.agent.command("drain-and-stop", request_id="s1")
        seen = self.wrong_one_in_place()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "resumed", report)
        self.assertEqual(seen, [self.own()])

    def test_a_roll_back_uses_the_installed_releases(self):
        self.release_with_its_own("3.0.2", "b", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        seen = self.wrong_one_in_place()
        self.assertEqual(self.agent.command("roll-back", request_id="rb-1")["state"], "rolled-back")
        self.assertEqual(seen, [self.own()])

    def test_a_restart_of_the_task_worker_uses_the_installed_releases(self):
        seen = self.wrong_one_in_place()
        report = self.agent.answer("restart-service", "task-worker", request_id="r1")
        self.assertEqual(report["state"], "restarted", report)
        self.assertEqual(seen, [self.own()])
        self.file.write_text("hotbox of another release")
        self.agent.answer("restart-service", "worker", request_id="r2")
        self.assertEqual(self.file.read_text(), "hotbox of another release", "only its own restart touches it")

    def test_a_copy_that_fails_refuses_the_restart(self):
        self.run.no_hotbox = True
        self.run.commands.clear()
        report = self.agent.answer("restart-service", "task-worker", request_id="r1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("Could not copy hotbox", report["error"])
        self.assertEqual(self.run.compose_calls(), [])
        self.assertNotIn("restarting", self.agent.state(), "nothing stopped, so nothing is left to start")

    def test_workers_started_to_drain_the_queue_use_the_installed_releases(self):
        seen = self.wrong_one_in_place()
        self.agent.drain(self.roots["releases"] / "3.0.1/bundle", queue=True)
        self.assertEqual(seen, [self.own()])


class DataDiskMount(unittest.TestCase):
    def test_it_is_the_data_root_that_must_be_a_mount(self):
        agent = agent_module.Agent(installation(), FakeCloud(), roots={"data": "/mnt/hot"})
        for mounted in (True, False):
            with mock.patch.object(agent_module.os.path, "ismount", return_value=mounted) as ismount:
                self.assertEqual(agent.data_disk_mounted(), mounted)
            ismount.assert_called_once_with(Path("/mnt/hot"))


class MigrationLevel(UpgradeHarness):
    """The upgrade rules go by the database's own ledger (F111)."""

    def to_302(self):
        self.release("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.agent.state()["migrations"], 23)

    def level_jobs(self):
        return [c for c in self.run.commands if agent_module.MIGRATION_STATUS_JOB in c]

    def database_off_the_host(self):
        """A database whose level only its own ledger can tell, already in use."""
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR].update(
            HOT_DB_URI="postgres://hot_runtime:pw@db.internal.example:5432/hot?sslmode=verify-full",
            HOT_DB_MIGRATION_URI="postgres://hot_owner:pw@db.internal.example:5432/hot?sslmode=verify-full",
            POSTGRES_CA_CERT="-----BEGIN CERTIFICATE-----")
        self.agent.resolve = lambda hosts: ["10.20.30.40/32"] if hosts else []
        self.agent.save_state({**self.agent.state(), "data_modes": self.agent.configured_modes()})

    def installed_job_fails(self, **fields):
        """Only the installed release's job fails, or answers with `fields`."""
        run = self.agent.run_command

        def failing(command, **kwargs):
            if agent_module.MIGRATION_STATUS_JOB in command and "releases/3.0.1/bundle/" in " ".join(command):
                self.run.commands.append(command)
                if "stdout" in fields:
                    return subprocess.CompletedProcess(command, 0, stderr="", **fields)
                raise subprocess.CalledProcessError(1, command, stderr="manifest unknown: not in the mirror\n")
            return run(command, **kwargs)
        self.agent.run_command = failing

    def test_a_database_restored_below_the_recorded_level_takes_the_earlier_release(self):
        self.to_302()
        self.run.migration_level = lambda: 22  # the operator restored it
        self.release("3.0.1")
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        self.assertEqual(self.agent.state()["migrations"], 22)
        self.assertEqual(self.agent.history()[-1]["migrations_before"], 22)
        job = self.level_jobs()[0]
        self.assertIn("releases/3.0.2/bundle/compose.json", " ".join(job), "the installed release's job")
        self.assertEqual(job[-5:], ["run", "--rm", "--no-deps", "-T", agent_module.MIGRATION_STATUS_JOB])
        self.assertLess(self.run.commands.index(job), next(i for i, c in enumerate(self.run.commands) if "stop" in c))

    def test_without_the_restore_the_earlier_release_is_still_refused(self):
        self.to_302()
        self.release("3.0.1")
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("knows migrations up to 22, but the database has 23", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")

    def test_a_database_above_the_recorded_level_is_refused_before_anything_stops(self):
        self.run.migration_level = lambda: 24  # a newer Hot migrated it, or a later backup came back
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("knows migrations up to 23, but the database has 24", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), [])
        self.assertEqual(self.agent.state()["migrations"], 22, "the record of the last deploy stands")

    def test_a_lower_database_counts_as_a_schema_change(self):
        # The same release again: its migrations must apply, with a restore point.
        self.to_302()
        self.run.migration_level = lambda: 22
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertIn("restore-point", status["steps"])
        self.assertIn("migrating", status["steps"])

    def test_a_database_with_no_migrations_is_refused_before_anything_stops(self):
        # Empty, or another database: nothing about it says it may start over.
        self.run.migration_level = lambda: 0
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("has no migrations", status["error"])
        self.assertIn("left it at 22", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), [])
        self.assertEqual(self.agent.state()["migrations"], 22, "the record of the last deploy stands")

    def test_a_state_with_no_recorded_level_takes_the_databases_word_for_none(self):
        self.agent.save_state({key: value for key, value in self.agent.state().items() if key != "migrations"})
        self.run.migration_level = lambda: 0
        self.release("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")

    def test_a_higher_database_of_this_release_counts_as_no_schema_change(self):
        # 3.0.2's migrations already ran, whatever the record says.
        self.run.migration_level = lambda: 23
        self.release("3.0.2", latest=23)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertNotIn("restore-point", status["steps"])

    def test_a_database_that_cannot_be_read_stops_the_deploy_before_anything_stops(self):
        self.database_off_the_host()
        self.release("3.0.2", latest=23)
        self.failing = agent_module.MIGRATION_STATUS_JOB
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not read the database's migration level", status["error"])
        self.assertIn("the job of the release being deployed failed too", status["error"])
        self.assertEqual(len(self.level_jobs()), 2, "the installed release's, then the one being deployed")
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), [])
        self.assertNotIn("stopped", self.agent.state())
        # It waits for deploy-now, and then goes ahead.
        self.failing = None
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")

    def test_output_with_no_level_stops_the_deploy_too(self):
        self.database_off_the_host()
        run = self.agent.run_command
        for output in ("", "not json\n", '{"latest": 23}\n', '{"level": "23"}\n', '{"level": true}\n', "[23]\n"):
            with self.subTest(output=output):
                def answering(command, **kwargs):
                    if agent_module.MIGRATION_STATUS_JOB in command:
                        return subprocess.CompletedProcess(command, 0, stdout=output, stderr="")
                    return run(command, **kwargs)
                self.agent.run_command = answering
                self.release("3.0.2", latest=23)
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn("Could not read the database's migration level", status["error"])
                self.assertIn("output names no level", status["error"])

    def test_the_last_line_is_the_answer(self):
        self.assertEqual(agent_module.migration_level('a line of something\n{"level": 7}\n'), 7)
        self.assertEqual(agent_module.migration_level('{"level": 0, "latest": 24}'), 0)

    def test_a_first_deploy_has_nothing_to_read_with(self):
        fresh = agent_module.Agent(self.config, self.cloud, run=self.run, sleep=lambda _: None,
                                   roots={**self.roots, "releases": Path(self.tmp.name) / "fresh"}, chown=lambda *args: None)
        fresh.wait_ready = lambda: None
        self.run.commands.clear()
        self.assertEqual(fresh.reconcile()["state"], "deployed")
        self.assertEqual(self.level_jobs(), [])

    def drop_the_job(self):
        path = self.roots["releases"] / "3.0.1/bundle/compose.json"
        compose = json.loads(path.read_text())
        del compose["services"][agent_module.MIGRATION_STATUS_JOB]
        path.write_text(json.dumps(compose))

    def test_a_bundle_from_before_the_job_goes_by_the_recorded_level(self):
        self.drop_the_job()
        self.run.migration_level = lambda: self.fail("no such job to run")
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.level_jobs(), [])

    def test_a_bundle_from_before_the_job_still_refuses_what_the_record_refuses(self):
        self.drop_the_job()
        self.release("3.0.0", latest=21)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("knows migrations up to 21, but the database has 22", status["error"])

    def test_a_hot_from_before_the_command_goes_by_the_recorded_level(self):
        run = self.agent.run_command

        def older(command, **kwargs):
            if agent_module.MIGRATION_STATUS_JOB in command:
                raise subprocess.CalledProcessError(2, command, stderr="error: unexpected argument '--status' found\n")
            return run(command, **kwargs)
        self.agent.run_command = older
        self.release("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.release("3.0.1")
        self.assertIn("knows migrations up to 22, but the database has 23", self.agent.reconcile()["error"])

    def test_a_stopped_installation_after_a_reboot_reads_it_with_what_the_job_needs(self):
        self.stopped_and_rebooted()
        self.release("3.0.2", latest=23)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        job = self.run.commands.index(self.level_jobs()[0])
        started = next(i for i, c in enumerate(self.run.commands) if "--no-recreate" in c and "postgres" in c)
        self.assertLess(started, job)

    def test_a_timeout_stops_the_deploy_too(self):
        self.database_off_the_host()
        run = self.agent.run_command

        def stuck(command, **kwargs):
            if agent_module.MIGRATION_STATUS_JOB in command:
                self.assertEqual(kwargs["timeout"], agent_module.MIGRATION_STATUS_SECONDS)
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return run(command, **kwargs)
        self.agent.run_command = stuck
        self.release("3.0.2", latest=23)
        status = self.agent.reconcile()
        self.assertIn("timed out after 600 seconds", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_failing_installed_job_gives_way_to_the_job_of_the_release_being_deployed(self):
        # A mirror that no longer holds its image, or a broken command: no shell
        # reaches these hosts, and a deploy may be the fix.
        for fields in ({}, {"stdout": "not json\n"}):
            with self.subTest(fields=fields):
                self.database_off_the_host()
                run = self.agent.run_command
                self.installed_job_fails(**fields)
                self.run.migration_level = lambda: 22
                self.release("3.0.2", latest=23)
                self.run.commands.clear()
                status = self.agent.reconcile()
                self.assertEqual(status["state"], "deployed", status)
                installed, target = self.level_jobs()
                self.assertIn("releases/3.0.1/bundle/compose.json", " ".join(installed))
                self.assertIn("releases/3.0.2/bundle.new/compose.json", " ".join(target))
                self.assertEqual(target[-5:], ["run", "--rm", "--no-deps", "-T", agent_module.MIGRATION_STATUS_JOB])
                self.assertEqual(self.agent.history()[-1]["migrations_before"], 22)
                self.assertIn("restore-point", status["steps"])
                # Back where the next round starts.
                self.agent.run_command = run
                self.run.migration_level = lambda: 22
                self.agent.save_state({"release": "3.0.1", "migrations": 22, "data_modes": self.agent.configured_modes(),
                                       "configuration": self.agent.configuration_digest()})

    def delivered(self):
        root = Path(self.tmp.name) / "secrets"
        return {str(path.relative_to(root)): path.read_text() for path in root.rglob("*") if path.is_file()}

    def existing_database_in_use(self):
        """An installation on a database off the host, deployed, whose services read
        files delivered for it. Returns those files."""
        self.database_off_the_host()
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        return self.delivered()

    def test_a_connection_the_level_refuses_reaches_only_the_job(self):
        # The refusal says nothing changed, and the running release's next restart
        # reads the services' files.
        before = self.existing_database_in_use()
        self.cloud.secrets[OPERATOR].update(
            HOT_DB_URI="postgres://hot_runtime:pw@other.internal.example:5432/hot?sslmode=verify-full",
            HOT_DB_MIGRATION_URI="postgres://hot_owner:pw@other.internal.example:5432/hot?sslmode=verify-full")
        self.agent.resolve = lambda hosts: ["10.20.30.41/32"] if hosts else []
        self.run.migration_level = lambda: 0
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("stopped before changing anything", status["error"])
        self.assertEqual(self.run.compose_calls(), [])
        after = self.delivered()
        self.assertIn("other.internal.example", after["migrate/HOT_DB_MIGRATION_URI"], "what the bundle holds now")
        self.assertEqual({k: v for k, v in after.items() if not k.startswith("migrate/")},
                         {k: v for k, v in before.items() if not k.startswith("migrate/")})

    def test_a_refusal_after_the_other_job_read_the_level_reaches_only_the_job_too(self):
        before = self.existing_database_in_use()
        self.installed_job_fails()
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = (
            "postgres://hot_runtime:rotated@db.internal.example:5432/hot?sslmode=verify-full")
        self.run.migration_level = lambda: 24
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("knows migrations up to 23, but the database has 24", status["error"])
        self.assertEqual(len(self.level_jobs()), 2, "the installed release's, then the one being deployed")
        self.assertEqual(self.delivered(), before)

    def test_that_job_is_held_to_what_it_reads(self):
        self.database_off_the_host()
        self.installed_job_fails()
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        self.run.migration_level = lambda: 24
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("knows migrations up to 23, but the database has 24", status["error"])
        self.run.migration_level = lambda: 0
        status = self.agent.reconcile(retry=True)
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("has no migrations", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")

    def test_that_job_signs_in_to_a_private_mirror_and_has_its_secrets_first(self):
        self.database_off_the_host()
        self.config["image_registry"] = "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot"
        self.cloud.registry_password = lambda host: "token"
        self.installed_job_fails()
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        run, read = self.agent.run_command, []

        def checking(command, **kwargs):
            if agent_module.MIGRATION_STATUS_JOB in command and "bundle.new" in " ".join(command):
                compose = json.loads((kwargs["cwd"] / "compose.json").read_text())
                read.extend(compose["secrets"][entry["source"]]["file"]
                            for entry in compose["services"][agent_module.MIGRATION_STATUS_JOB]["secrets"])
                self.assertTrue(read and all(Path(file).is_file() for file in read), read)
            return run(command, **kwargs)
        self.agent.run_command = checking
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertTrue(read)
        commands = [" ".join(c) for c in self.run.commands]
        login = next(i for i, c in enumerate(commands) if c.startswith("docker login"))
        self.assertLess(login, commands.index(" ".join(self.level_jobs()[1])))

    def test_a_local_database_that_cannot_be_read_goes_by_the_recorded_level(self):
        # Crash-looping under the installed bundle, say, whose image or memory cap
        # this deploy replaces. The state is on its disk, so no restore parts them.
        self.failing = agent_module.MIGRATION_STATUS_JOB
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(len(self.level_jobs()), 2, "the installed release's, then the one being deployed")
        self.assertIn("up -d --wait postgres valkey", self.run.compose_calls())
        self.assertEqual(self.agent.history()[-1]["migrations_before"], 22)
        # What the record refuses stays refused.
        self.release("3.0.0", latest=21)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("knows migrations up to 21, but the database has 23", status["error"])

    def test_a_local_database_a_failed_deploy_may_have_migrated_stops_the_deploy(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.failing = agent_module.MIGRATION_STATUS_JOB
        self.run.commands.clear()
        status = self.agent.reconcile(retry=True)
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not read the database's migration level", status["error"])
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()), "nothing changes")

    def test_a_data_move_still_to_come_has_no_other_way_to_read_it(self):
        # The release being deployed describes a database the data has yet to reach.
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR].update(
            HOT_DB_URI="postgres://hot_runtime:pw@db.internal.example:5432/hot?sslmode=verify-full",
            HOT_DB_MIGRATION_URI="postgres://hot_owner:pw@db.internal.example:5432/hot?sslmode=verify-full",
            POSTGRES_CA_CERT="-----BEGIN CERTIFICATE-----")
        self.agent.resolve = lambda hosts: ["10.20.30.40/32"] if hosts else []
        self.failing = agent_module.MIGRATION_STATUS_JOB
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Could not read the database's migration level", status["error"])
        self.assertEqual(len(self.level_jobs()), 1)
        self.assertIn("releases/3.0.1/bundle/compose.json", " ".join(self.level_jobs()[0]))

    def test_data_that_will_not_start_under_the_installed_bundle_is_no_dead_end(self):
        # Stopped and rebooted, with an image the mirror no longer holds.
        self.stopped_and_rebooted()
        run = self.agent.run_command

        def missing(command, **kwargs):
            if "--no-recreate" in command:
                self.run.commands.append(command)
                raise subprocess.CalledProcessError(1, command, stderr="manifest unknown\n")
            return run(command, **kwargs)
        self.agent.run_command = missing
        self.release("3.0.2", latest=23)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertIn("up -d --wait postgres valkey", self.run.compose_calls())


class BundleRendering(UpgradeHarness):
    def test_a_refused_configuration_leaves_the_running_bundle(self):
        bundle = Path(self.tmp.name) / "releases/3.0.1/bundle"
        before = {path.name: path.read_text() for path in bundle.iterdir()}
        for change in ({"settings": {"HOT_LOG_LEVEL": "debug"}}, {"operator": {"HOT_SMTP_PASSWORD": "x"}}):
            with self.subTest(change=change):
                self.config["settings"] = change.get("settings", {})
                self.cloud.secrets[OPERATOR].update(change.get("operator", {}))
                self.run.commands.clear()
                status = self.agent.command("deploy-now", request_id="r")
                self.assertEqual(status["state"], "failed")
                self.assertIn("3.0.1 keeps running", status["recovery"])
                self.assertEqual({path.name: path.read_text() for path in bundle.iterdir()}, before)
                self.assertFalse(bundle.with_name("bundle.new").exists())
                self.assertFalse(any(" stop " in f" {c} " for c in self.run.compose_calls()), "nothing stopped")
                self.cloud.secrets[OPERATOR].pop("HOT_SMTP_PASSWORD", None)
        self.config["settings"] = {}
        self.run.ps = json.dumps({"Service": "app", "State": "running", "Health": ""})
        self.assertIn("UnhealthyServices", self.agent.observe(), "monitoring still reads the bundle")


class Scalable(AgentHarness):
    ADMIN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef"
    TOKEN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/valkey"

    def host(self, roles, migrations=False):
        self.config.update(preset="scalable", roles=roles, migrations=migrations,
                           storage={"build": {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "builds"},
                                    "file": {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"}},
                           data={"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
                                     "host": "db.example", "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}},
                                 "valkey": {"mode": "managed", "endpoint": {
                                     "host": "cache.example", "port": 6379, "auth_secret": self.TOKEN}}})
        if migrations:
            self.config["migration_pointer"] = "migrate-pointer"
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "admin"}
        self.cloud.secrets[self.TOKEN] = {"password": "token"}
        self.agent.resolve = lambda hosts: []

    def fake_http(self, method, url, body=None, headers=None, timeout=30):
        if url.startswith("https://truststore.pki.rds.amazonaws.com/"):
            return 200, b"-----BEGIN CERTIFICATE-----\nRDS\n-----END CERTIFICATE-----\n"
        return super().fake_http(method, url, body, headers, timeout)

    def task_host(self, idle_minutes=30, size=3, minimum=1):
        self.host(["task-worker"])
        self.config.update(instance_group="hot-example-task-worker", scaling={
            "task_worker": {"min": minimum, "max": 5, "retire_idle_minutes": idle_minutes}})
        self.cloud.group = size
        self.cloud.left = []
        self.cloud.group_size = lambda group: self.cloud.group
        self.cloud.leave_group = self.cloud.left.append
        self.agent._instance_name = "i-0123456789abcdef0"
        self.agent.reconcile()
        self.run.ps = json.dumps({"Service": "task-worker", "State": "running", "Health": ""})
        self.run.commands.clear()

    IDLE_START = datetime.datetime(2026, 10, 29, 12, 0, tzinfo=datetime.timezone.utc)

    def idle_for(self, minutes, busy=0, last_task_at=None):
        for minute in range(0, minutes + 1):
            now = self.IDLE_START + datetime.timedelta(minutes=minute)
            self.run.task_status = {"time": now.timestamp(), "code": {"slots": 500, "busy": busy},
                                    "containers": {"slots": 8, "busy": 0}, "memory_mb": 8192, "memory_used_mb": 0}
            if last_task_at:
                self.run.task_status["last_task_at"] = last_task_at(now)
            self.agent.monitor(now=now)

    def test_an_idle_task_host_leaves_its_group(self):
        self.task_host()
        self.idle_for(20)
        self.assertEqual(self.cloud.left, [], "not idle long enough")
        self.idle_for(36)
        self.assertEqual(self.cloud.left, ["hot-example-task-worker"])
        stop = next(i for i, c in enumerate(self.run.commands) if c[-1] == "task-worker" and "stop" in c)
        self.assertEqual(self.run.commands[stop][-3:], ["--timeout", "120", "task-worker"])
        self.assertEqual(self.cloud.statuses[-1]["state"], "retiring")

    def test_a_task_between_readings_keeps_the_host_in_its_group(self):
        self.task_host()
        self.idle_for(120, last_task_at=lambda now: now.timestamp() - 60)
        self.assertEqual(self.cloud.left, [], "every reading is 0, but tasks keep running between them")

    def test_the_idle_time_counts_from_the_last_task(self):
        self.task_host()
        finished = (self.IDLE_START + datetime.timedelta(minutes=10)).timestamp()
        self.idle_for(39, last_task_at=lambda now: finished)
        self.assertEqual(self.cloud.left, [], "29 minutes since the last task, 39 of zero readings")
        self.idle_for(46, last_task_at=lambda now: finished)
        self.assertEqual(self.cloud.left, ["hot-example-task-worker"])

    def test_a_last_task_time_that_says_nothing_changes_nothing(self):
        for last_task_at in (0, None, "soon", float("nan")):
            with self.subTest(last_task_at=last_task_at):
                self.agent._idle_since = None
                self.task_host()
                self.idle_for(36, last_task_at=lambda now: last_task_at)
                self.assertEqual(self.cloud.left, ["hot-example-task-worker"])

    def test_a_task_host_stays_while_busy_or_at_the_minimum(self):
        self.task_host()
        self.idle_for(60, busy=1)
        self.assertEqual(self.cloud.left, [])
        self.task_host(size=1)
        self.idle_for(60)
        self.assertEqual(self.cloud.left, [], "the group keeps its minimum")
        self.task_host(idle_minutes=0)
        self.idle_for(60)
        self.assertEqual(self.cloud.left, [], "retire_idle_minutes = 0 turns retirement off")

    def test_only_aws_task_hosts_retire(self):
        # Google's IAM conditions cannot name one instance group, so a GCP task
        # host has no grant: it tries nothing, stops nothing, and logs nothing.
        for cloud, left in (("aws", ["hot-example-task-worker"]), ("gcp", [])):
            with self.subTest(cloud=cloud):
                self.agent._idle_since = None
                self.task_host()
                self.config["cloud"] = cloud
                if cloud == "gcp":
                    del self.config["instance_group"]  # as the GCP stacks' configurations have none
                asked = []
                self.cloud.group_size = lambda group: asked.append(group) or self.cloud.group
                self.cloud.statuses.clear()
                with mock.patch("sys.stderr") as stderr:
                    self.idle_for(36)
                self.assertEqual(self.cloud.left, left)
                self.assertEqual(bool(asked), bool(left), "the group's size is read only to leave it")
                stopped = [c for c in self.run.commands if "stop" in c and c[-1] == "task-worker"]
                self.assertEqual(bool(stopped), bool(left), "the worker stops only to leave")
                self.assertEqual(any(s["state"] == "retiring" for s in self.cloud.statuses), bool(left))
                self.assertNotIn("retire_failed", "".join(str(c) for c in stderr.write.call_args_list))
                self.assertEqual(self.agent._idle_since is not None, bool(left), "no idle wait where nothing can leave")
                self.config["cloud"] = "aws"

    def test_a_failed_departure_restarts_the_task_worker(self):
        self.task_host()
        def refused(group):
            raise agent_module.AgentError("TerminateInstanceInAutoScalingGroup failed: AccessDenied")
        self.cloud.leave_group = refused
        self.idle_for(36)
        calls = [" ".join(c) for c in self.run.commands]
        stop = max(i for i, c in enumerate(calls) if c.endswith("stop --timeout 120 task-worker"))
        self.assertTrue(any(c.endswith("up -d task-worker") for c in calls[stop:]), calls[stop:])

    def test_a_task_worker_that_will_not_stop_keeps_the_host_in_its_group(self):
        self.task_host()
        recorder, asked = self.run, []

        def hanging(command, **kwargs):
            if "stop" in command:
                asked.append(kwargs["timeout"])
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        with mock.patch("sys.stderr") as stderr:
            self.idle_for(36)
        self.assertTrue(asked)
        self.assertGreater(asked[0], 120, "past the task worker's own grace")
        self.assertEqual(self.cloud.left, [])
        self.assertIn("retire_failed", "".join(str(c) for c in stderr.write.call_args_list))

    def test_a_task_host_without_a_group_stays(self):
        self.task_host()
        del self.config["instance_group"]
        self.idle_for(36)
        self.assertEqual(self.cloud.left, [])
        self.assertFalse(any("stop" in c for c in self.run.commands))

    def failed_to_deploy_a_later_release(self):
        """A host on 3.0.1 that failed to deploy 3.0.2 and went on running 3.0.1."""
        self.config["status_reference"] = "status"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.manifests["3.0.2"] = manifest("3.0.2")
        self.cloud.parameters["pointer"] = "3.0.2"
        pull = self.agent.pull_images
        self.agent.pull_images = mock.Mock(side_effect=agent_module.AgentError("the mirror lags"))
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.agent.pull_images = pull
        state = self.agent.state()
        self.assertEqual((state["release"], state["failed"]["release"]), ("3.0.1", "3.0.2"))
        self.cloud.statuses.clear()
        self.run.commands.clear()

    def test_a_host_reports_once_when_its_pointer_returns_after_a_failed_deploy(self):
        # The release helper waits for a report from since it wrote the pointer.
        self.host(["worker"])
        self.failed_to_deploy_a_later_release()
        failed = self.agent.state()["failed"]
        self.cloud.parameters["pointer"] = "3.0.1"
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"], status.get("command")), ("current", "3.0.1", None), status)
        self.assertFalse(status.get("request_id"), "no command asked for it")
        self.assertEqual([(s["state"], s["release"]) for s in self.cloud.statuses], [("current", "3.0.1")])
        self.assertEqual(self.agent.state()["failed"], failed, "the failed record keeps its other uses")
        self.assertEqual(self.run.compose_calls(), [], "nothing restarts")
        self.assertIsNone(self.agent.reconcile(), "later cycles stay quiet")
        self.assertEqual(len(self.cloud.statuses), 1)

    def test_each_return_of_the_pointer_is_reported(self):
        self.host(["worker"])
        self.failed_to_deploy_a_later_release()
        self.cloud.parameters["pointer"] = "3.0.1"
        self.assertEqual(self.agent.reconcile()["state"], "current")
        self.cloud.parameters["pointer"] = "3.0.2"
        self.assertIsNone(self.agent.reconcile(), "the failed release still waits for deploy-now")
        self.assertNotIn("pointer_returned", self.agent.state())
        self.cloud.parameters["pointer"] = "3.0.1"
        self.assertEqual(self.agent.reconcile()["state"], "current")

    def test_a_host_that_is_not_running_says_nothing_of_a_returned_pointer(self):
        self.host(["worker"])
        self.failed_to_deploy_a_later_release()
        self.cloud.parameters["pointer"] = "3.0.1"
        state = self.agent.state()
        for note in ({"stopped": True}, {"resume_failed": {"release": "3.0.1", "error": "no", "at": "then"}},
                     {"restarting": {"worker": "r1"}}):
            with self.subTest(note=note):
                self.agent.save_state({**state, **note})
                self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.cloud.statuses, [])

    def scheduler_restart_killed(self):
        """A scheduler host whose restart died between its stop and its start."""
        self.host(["scheduler"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        recorder = self.agent.run_command

        def killed(command, **kwargs):
            if "up" in command:
                raise KeyboardInterrupt
            return recorder(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.answer("restart-service", "scheduler", request_id="r1")
        self.agent.run_command = recorder
        self.run.commands.clear()
        self.assertEqual(self.agent.state()["restarting"], {"scheduler": "r1"})

    def test_the_next_cycle_starts_a_scheduler_a_killed_restart_left_stopped(self):
        self.scheduler_restart_killed()
        self.agent.run_once()
        calls = self.run.compose_calls()
        self.assertLess(calls.index("stop scheduler"), calls.index("up -d --force-recreate scheduler"))
        self.assertNotIn("restarting", self.agent.state())
        self.assertEqual((self.cloud.statuses[-1]["state"], self.cloud.statuses[-1]["request_id"]), ("restarted", "r1"))

    def test_a_later_failed_deploy_starts_the_scheduler_it_would_say_keeps_running(self):
        self.scheduler_restart_killed()
        self.manifests["3.2.0"] = manifest("3.2.0", minimum_source="3.1.0")
        self.cloud.parameters["pointer"] = "3.2.0"
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("up -d --force-recreate scheduler", self.run.compose_calls())
        self.assertIn("previous-scheduler-restarted", report["steps"])
        self.assertIn("keeps running", report["recovery"])
        self.assertNotIn("restarting", self.agent.state())

    def test_deploy_now_answers_a_returned_pointer_with_its_own_report(self):
        self.host(["worker"])
        self.failed_to_deploy_a_later_release()
        self.cloud.parameters["pointer"] = "3.0.1"
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["request_id"]), ("current", "d1"), report)
        self.assertEqual(len(self.cloud.statuses), 1, "one report, which answers the command")

    def test_a_single_machine_says_nothing_of_a_returned_pointer(self):
        self.failed_to_deploy_a_later_release()
        self.cloud.parameters["pointer"] = "3.0.1"
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.cloud.statuses, [])

    def test_only_the_migrating_host_reads_the_database_administrator_login(self):
        read = []
        get_secret = self.cloud.get_secret
        self.cloud.get_secret = lambda reference: read.append(reference) or get_secret(reference)
        self.host(["app"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertNotIn(self.ADMIN, read, "an app host's role may not read the administrator secret")
        self.assertFalse((Path(self.tmp.name) / "secrets/prepare-postgres").exists())
        self.assertTrue((Path(self.tmp.name) / "secrets/app/POSTGRES_CA_CERT").exists())

    def test_only_a_host_with_a_task_worker_keeps_box_data(self):
        box_data = Path(self.tmp.name) / "data" / render.BOX_DATA_DIRECTORY
        self.host(["worker"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertFalse(box_data.exists())
        self.agent.save_state({})
        self.host(["task-worker"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(box_data.stat().st_mode & 0o777, 0o700)

    def test_a_half_set_sign_in_does_not_stop_a_host_with_no_app(self):
        # A scale-out or a replacement, while the operator waits for the client secret.
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"] = "google-id"
        for role in ("task-worker", "worker", "api", "scheduler"):
            with self.subTest(role=role):
                self.agent.save_state({})
                self.host([role])
                with mock.patch("sys.stderr") as stderr:
                    status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "deployed", status)
                logged = "".join(call.args[0] for call in stderr.write.call_args_list)
                self.assertIn("HOT_GOOGLE_CLIENT_SECRET has no value", logged)
                self.assertNotIn("google-id", logged)
                with mock.patch("sys.stderr"):
                    self.assertEqual(self.agent.answer("deploy-now", request_id="d-" + role)["state"], "current")

    def test_a_half_set_sign_in_stops_the_host_with_the_app(self):
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"] = "google-id"
        self.host(["app"])
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("HOT_GOOGLE_CLIENT_SECRET has no value", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing started")

    def test_a_blank_sign_in_entry_does_not_stop_a_host_with_no_app(self):
        self.cloud.secrets[OPERATOR].update(HOT_GITHUB_CLIENT_ID="github-id", HOT_GITHUB_CLIENT_SECRET="")
        self.host(["task-worker"])
        with mock.patch("sys.stderr"):
            self.assertEqual(self.agent.reconcile()["state"], "deployed")

    def test_a_host_with_no_web_roles_fails_when_its_worker_keeps_restarting(self):
        self.host(["worker"])
        self.run.ps = json.dumps({"Service": "worker", "State": "restarting", "Health": ""})
        self.run.logs = "worker-1  | Error: network.egress-proxy is not a URL\n"
        # Nothing to probe, so readiness passes at once.
        del self.agent.wait_ready
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("did not stay running: worker (restarting)", status["error"])
        self.assertIn("network.egress-proxy is not a URL", status["error"])
        self.assertNotIn("release", self.agent.state())
        self.run.ps = json.dumps({"Service": "worker", "State": "running", "Health": ""})
        self.assertEqual(self.agent.reconcile(retry=True)["state"], "deployed")
        self.assertFalse(any("ingress" in c for c in self.run.compose_calls()), "this host has none")

    def test_a_restart_that_failed_offers_no_roll_back(self):
        # The AWS command document leaves roll-back out here, and the GCP helper refuses it.
        self.host(["app"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.manifests["3.0.2"] = manifest("3.0.2")
        self.cloud.parameters["pointer"] = "3.0.2"
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("3.0.1 did not start again", status["recovery"])
        self.assertIn("tried again every 5 minutes", status["recovery"])
        self.assertIn("run deploy-now to retry 3.0.2", status["recovery"])
        self.assertNotIn("roll-back", status["recovery"])

    def test_a_replacement_scheduler_host_deploys_while_a_dead_hosts_lock_is_held(self):
        self.host(["scheduler"])
        self.run.ps = json.dumps({"Service": "scheduler", "State": "restarting", "Health": ""})
        self.run.logs = SCHEDULER_STARTS + SCHEDULER_LOCK_HELD
        del self.agent.wait_ready
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(self.agent.state()["release"], "3.0.1")
        self.assertNotIn("failed", self.agent.state())
        # It publishes its metrics, and a lock that never frees reaches the alarm.
        self.assertEqual(self.agent.observe()["SchedulerRunning"], (0, "Count"))

    def test_a_kill_as_the_scheduler_step_passes_on_a_host_without_one_starts_none(self):
        self.host(["worker"])
        self.run.ps = json.dumps({"Service": "worker", "State": "running", "Health": ""})
        del self.agent.wait_ready
        self.agent.reconcile()
        # Nothing runs between that step and the start on a scalable host, so
        # the record is all a kill there leaves.
        self.agent.save_state({**self.agent.state(), "deploying": {
            "release": "3.0.2", "previous": "3.0.1", "steps": ["started", "scheduler-stopped"]}})
        self.cloud.parameters["pointer"] = "3.0.2"  # no such manifest
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual(self.run.compose_calls(), [])
        self.assertIn("3.0.1 keeps running", status["recovery"])

    def test_a_scheduler_host_whose_scheduler_crashes_for_another_reason_still_fails(self):
        self.host(["scheduler"])
        self.run.ps = json.dumps({"Service": "scheduler", "State": "restarting", "Health": ""})
        self.run.logs = SCHEDULER_LOCK_HELD + SCHEDULER_STARTS + "scheduler-1  | Error: bad configuration\n"
        del self.agent.wait_ready
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("did not stay running: scheduler (restarting)", status["error"])
        self.assertNotIn("release", self.agent.state())

    def test_an_app_host_deploys_only_its_role(self):
        self.host(["app"])
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(status["roles"], ["app"])
        self.assertIn("instance", status)
        calls = self.run.compose_calls()
        self.assertFalse(any("migrate" in c or "scheduler" in c for c in calls), calls)
        self.assertIn("up -d --remove-orphans app", calls)
        self.assertIn("up -d --force-recreate --no-deps ingress", calls)
        roles = {p.name for p in (Path(self.tmp.name) / "secrets").iterdir()}
        self.assertEqual(roles, {"app", "ingress"}, "a host receives only its roles' secrets")

    def test_a_storage_backend_change_is_refused_before_anything_stops(self):
        self.host(["worker"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.config["storage"] = dict(self.config["storage"], file={"backend": "gcs", "bucket": "acme", "prefix": "files"})
        # A render takes GCS only on GCP, whose stack this one's settings are not.
        render_bundle, self.agent.render_bundle = self.agent.render_bundle, lambda *args, **kwargs: None
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.agent.render_bundle = render_bundle
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("A scalable installation does not move storage, and file storage changed from s3 to gcs",
                      status["error"])
        self.assertIn("Put storage.file back", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")
        self.assertIn("keeps running", status["recovery"])
        self.assertFalse(any("move-storage" in c for c in self.run.commands))
        # With it put back the host redeploys nothing, and is on the old backend.
        self.config["storage"] = dict(self.config["storage"], file={
            "backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"})
        self.assertIsNone(self.agent.reconcile())

    def test_no_role_sees_a_backend_change_where_none_was_made(self):
        for role in ("app", "api", "worker", "task-worker", "scheduler"):
            with self.subTest(role=role):
                self.host([role])
                self.assertEqual(self.agent.reconcile(retry=True)["state"], "deployed")
                self.config["settings"] = {"HOT_EXAMPLE_SETTING": role}
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "deployed", status)

    def test_another_bucket_or_prefix_is_refused_before_anything_stops(self):
        self.host(["worker"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        installed = self.config["storage"]
        for change, spelled in (({"bucket": "other"}, "acme/files to other/files"),
                                ({"prefix": "elsewhere"}, "acme/files to acme/elsewhere")):
            with self.subTest(change=change):
                self.config["storage"] = dict(installed, file={**installed["file"], **change})
                self.run.commands.clear()
                status = self.agent.reconcile(retry=True)
                self.assertEqual(status["state"], "failed", status)
                self.assertIn(f"A scalable installation does not move storage, and file storage changed from {spelled}",
                              status["error"])
                self.assertIn("Put storage.file back", status["error"])
                self.assertEqual(self.run.compose_calls(), [], "nothing stops")
                self.assertIn("keeps running", status["recovery"])
                self.assertFalse(any("move-storage" in c for c in self.run.commands))
        # With it put back the host redeploys nothing, and is where it was.
        self.config["storage"] = installed
        self.assertIsNone(self.agent.reconcile())

    def test_what_the_plans_do_not_call_a_new_location_is_not_refused(self):
        self.host(["worker"])
        self.agent.reconcile()
        installed = self.config["storage"]
        for same in ({"prefix": "files/"}, {"region": "us-west-2"}):
            with self.subTest(same=same):
                self.config["storage"] = dict(installed, file={**installed["file"], **same})
                status = self.agent.reconcile()
                self.assertEqual(status["state"], "deployed", status)

    def test_the_migration_host_goes_by_the_databases_level(self):
        self.host(["scheduler"], migrations=True)
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters.update({"migrate-pointer": "3.0.2", "pointer": None})
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        self.assertEqual(self.agent.state()["migrations"], 23)
        # Restored to before 3.0.2, the earlier release can migrate again.
        self.run.migration_level = lambda: 22
        self.cloud.parameters["migrate-pointer"] = "3.0.1"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.1"), status)
        self.assertEqual(self.agent.state()["migrations"], 22)
        job = next(c for c in self.run.commands if agent_module.MIGRATION_STATUS_JOB in c)
        self.assertIn("releases/3.0.2/bundle/compose.json", " ".join(job))
        # And a database above what a release knows is refused, whatever the record says.
        self.run.migration_level = lambda: 30
        self.manifests["3.0.3"] = manifest("3.0.3", latest=24)
        self.cloud.parameters["migrate-pointer"] = "3.0.3"
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "migration-failed", status)
        self.assertIn("knows migrations up to 24, but the database has 30", status["error"])
        # And one with no migrations is not this installation's.
        self.run.migration_level = lambda: 0
        status = self.agent.reconcile(retry=True)
        self.assertEqual(status["state"], "migration-failed", status)
        self.assertIn("has no migrations", status["error"])
        self.assertEqual(self.agent.state()["migrations"], 22)

    def test_a_host_that_does_not_migrate_follows_the_release_back_after_a_restore(self):
        # It has no migration login to read the database with, and its recorded level
        # is what the restore left wrong. The migration host, which read the database,
        # reported `migrated` before the release helper moved this role's pointer.
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        for role in ("app", "api", "worker", "task-worker"):
            with self.subTest(role=role):
                directory = Path(self.tmp.name) / role
                self.agent = agent_module.Agent(self.config, self.cloud, run=self.run, sleep=lambda _: None, chown=lambda *args: None,
                                                roots={**self.roots, "releases": directory / "releases", "data": directory,
                                                       "secrets": directory / "secrets"})
                self.agent.wait_ready = lambda: None
                self.host([role])
                self.cloud.parameters["pointer"] = "3.0.1"
                self.assertEqual(self.agent.reconcile()["state"], "deployed")
                self.cloud.parameters["pointer"] = "3.0.2"
                self.assertEqual(self.agent.reconcile()["state"], "deployed")
                self.assertEqual(self.agent.state()["migrations"], 23)
                self.cloud.parameters["pointer"] = "3.0.1"
                self.run.commands.clear()
                status = self.agent.reconcile()
                self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
                self.assertEqual(self.agent.state()["migrations"], 22)
                self.assertIsNone(self.agent.history()[-1]["migrations_before"])
                self.assertFalse(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands))

    def test_a_host_that_does_not_migrate_still_asks_for_the_oldest_release_upgraded_from(self):
        self.host(["app"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.manifests["3.1.0"] = manifest("3.1.0", latest=30, minimum_source="3.0.5")
        self.cloud.parameters["pointer"] = "3.1.0"
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("upgrades from 3.0.5 or later", status["error"])

    def test_the_migration_host_delivers_nothing_a_render_refuses(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        root = Path(self.tmp.name) / "secrets"
        before = {str(path.relative_to(root)): path.read_text() for path in root.rglob("*") if path.is_file()}
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters["migrate-pointer"] = "3.0.2"
        self.cloud.secrets[OPERATOR]["HOT_NETWORK_EGRESS_PROXY"] = "ftp://proxy.internal"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "migration-failed", status)
        self.assertIn("must be an http://", status["error"])
        self.assertEqual({str(path.relative_to(root)): path.read_text() for path in root.rglob("*") if path.is_file()}, before)
        self.assertFalse(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands))
        self.assertTrue((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").exists())

    def test_the_migration_host_reads_with_the_release_it_migrates_to_when_its_own_job_fails(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        run = self.agent.run_command

        def failing(command, **kwargs):
            if agent_module.MIGRATION_STATUS_JOB in command and "releases/3.0.1/bundle/" in " ".join(command):
                raise subprocess.CalledProcessError(1, command, stderr="manifest unknown\n")
            return run(command, **kwargs)
        self.agent.run_command = failing
        self.run.migration_level = lambda: 22
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters["migrate-pointer"] = "3.0.2"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.2"), status)
        job = next(c for c in self.run.commands if agent_module.MIGRATION_STATUS_JOB in c)
        self.assertIn("releases/3.0.2/bundle.new/compose.json", " ".join(job))
        self.assertIn("--rm migrate", " ".join(self.run.compose_calls()))

    def test_only_a_host_with_a_task_worker_takes_the_hotbox(self):
        self.host(["app"])
        self.agent.reconcile()
        self.assertFalse(any(c[:2] == ["docker", "create"] for c in self.run.commands))
        self.assertFalse((Path(self.tmp.name) / "kata").exists())
        self.assertIsNone(self.agent.hotbox_version())

    def test_a_task_host_takes_the_hotbox_of_its_release(self):
        self.host(["task-worker"])
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        hotbox = Path(self.tmp.name) / "kata/hotbox"
        self.assertTrue(hotbox.read_text().startswith("hotbox of registry.example.com/hot@sha256:"))

    def test_the_migration_host_migrates_from_its_own_pointer(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.1"))
        calls = self.run.compose_calls()
        order = [calls.index(f"--profile admin run --rm {job}") for job in ("prepare-postgres", "check-postgres", "migrate", "provision")]
        self.assertEqual(order, sorted(order))
        self.assertFalse(any(c.startswith(("stop", "up")) for c in calls), "migrating restarts nothing")
        self.assertIsNone(self.agent.reconcile(), "a release migrates once")
        # The scheduler role then rolls without migrating again.
        self.cloud.parameters["pointer"] = "3.0.1"
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertFalse(any("run --rm migrate" in c for c in self.run.compose_calls()))
        self.assertEqual(self.agent.state()["migrated_release"], "3.0.1")

    def test_a_failed_migration_waits_for_deploy_now(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        failing = self.run

        def run(command, **kwargs):
            if "migrate" in command:
                failing.commands.append(command)
                raise subprocess.CalledProcessError(1, command)
            return failing(command, **kwargs)
        self.agent.run_command = run
        self.assertEqual(self.agent.reconcile()["state"], "migration-failed")
        self.assertIsNone(self.agent.reconcile())
        self.agent.run_command = failing
        self.assertEqual(self.agent.command("deploy-now", request_id="r")["state"], "migrated")

    def test_a_deploy_now_sent_before_a_failed_migration_answers_with_it(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        sent = agent_module.utc_now()
        failing = self.run

        def run(command, **kwargs):
            if "migrate" in command:
                failing.commands.append(command)
                raise subprocess.CalledProcessError(1, command)
            return failing(command, **kwargs)
        self.agent.run_command = run
        self.assertEqual(self.agent.reconcile()["state"], "migration-failed")
        self.agent.run_command = failing
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="r", sent=sent)
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("migration-failed", "3.0.1", "r"))
        self.assertEqual(self.run.commands, [], "not migrated again before the cause is fixed")
        recorded = agent_module.parse_time(self.agent.state()["migration_failed"]["at"])
        later = (recorded + datetime.timedelta(seconds=1)).isoformat()
        self.assertEqual(self.agent.command("deploy-now", sent=later)["state"], "migrated")

    def a_new_host_on_pointers(self, migrate, role):
        """A scheduler host with nothing installed, as a group brings up after going
        back or replaces one. Its migrate job refuses, as Hot does a database past
        what the release knows, until the test gives it self.run back."""
        self.host(["scheduler"], migrations=True)
        self.config["status_reference"] = "status"
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters.update({"migrate-pointer": migrate, "pointer": role})
        self.a_migrate_job_that_refuses()

    def a_migrate_job_that_refuses(self):
        recorder = self.run

        def run(command, **kwargs):
            if "migrate" in command:
                recorder.commands.append(command)
                raise subprocess.CalledProcessError(1, command)
            return recorder(command, **kwargs)
        self.agent.run_command = run

    def test_a_new_host_does_not_start_a_release_whose_migration_failed(self):
        # Going back writes every pointer first, and with nothing installed this host
        # has no level to compare (database_level): the job's refusal is all that says
        # the restore did not go far enough, and restoring again needs Hot stopped.
        self.a_new_host_on_pointers("3.0.1", "3.0.1")
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migration-failed", "3.0.1"), status)
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.1")])
        state = self.agent.state()
        self.assertEqual(state["migration_failed"]["release"], "3.0.1")
        self.assertNotIn("release", state)
        self.assertNotIn("deploying", state)
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()), "nothing starts")
        for _ in range(2):
            self.run.commands.clear()
            self.assertIsNone(self.agent.reconcile(), "it waits for deploy-now, not only in this cycle")
            self.assertEqual(self.run.commands, [])
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.1")])
        self.assertEqual(self.agent.state()["migration_failed"]["release"], "3.0.1")

    def test_deploy_now_retries_the_migration_and_only_then_starts_the_release(self):
        self.a_new_host_on_pointers("3.0.1", "3.0.1")
        self.agent.reconcile()
        report = self.agent.command("deploy-now", request_id="r")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("migration-failed", "3.0.1", "r"))
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()), "still nothing starts")
        self.assertEqual(self.agent.state()["migration_failed"]["release"], "3.0.1")
        # Restored far enough back.
        self.agent.run_command = self.run
        self.cloud.statuses.clear()
        report = self.agent.command("deploy-now", request_id="r2")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("deployed", "3.0.1", "r2"))
        self.assertEqual(self.migration_reports(), [("migrated", "3.0.1"), ("deployed", "3.0.1")])
        self.assertIn("up -d scheduler", self.run.compose_calls())
        state = self.agent.state()
        self.assertNotIn("migration_failed", state)
        self.assertEqual((state["release"], state["migrated_release"]), ("3.0.1", "3.0.1"))

    def test_a_replaced_host_catching_up_on_the_databases_release_starts_it(self):
        # What a group's replacement does all the time: at the database's level the
        # job only checks it, so the failed migration above is the only thing held.
        self.a_new_host_on_pointers("3.0.1", "3.0.1")
        self.agent.run_command = self.run
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        self.assertEqual(self.migration_reports(), [("migrated", "3.0.1"), ("deployed", "3.0.1")])
        self.assertIn("up -d scheduler", self.run.compose_calls())

    def test_a_new_host_runs_its_roles_release_when_a_later_one_failed_to_migrate(self):
        # A replacement in the middle of an upgrade: the scheduler rolls last, so its
        # pointer names the release every role still runs, which the database has.
        self.a_new_host_on_pointers("3.0.2", "3.0.1")
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.2"), ("deployed", "3.0.1")])
        self.assertIn("up -d scheduler", self.run.compose_calls())

    def test_a_running_host_keeps_its_release_when_the_one_named_failed_to_migrate(self):
        # Both pointers written by hand, ahead of the migration: the running release
        # goes on, and the new one never starts on a database it could not migrate.
        self.a_new_host_on_pointers("3.0.1", "3.0.1")
        self.agent.run_command = self.run
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.cloud.parameters.update({"migrate-pointer": "3.0.2", "pointer": "3.0.2"})
        self.a_migrate_job_that_refuses()
        self.cloud.statuses.clear()
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migration-failed", "3.0.2"), status)
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.2")])
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()), "the scheduler goes on")
        state = self.agent.state()
        self.assertEqual((state["release"], state["migration_failed"]["release"]), ("3.0.1", "3.0.2"))
        self.assertNotIn("deploying", state)
        self.assertIsNone(self.agent.reconcile())

    def test_an_interrupted_deploy_waits_with_the_release_whose_migration_failed(self):
        # Its failure would start the installed release, which is older still.
        self.a_new_host_on_pointers("3.0.1", "3.0.1")
        self.agent.run_command = self.run
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        interrupted = {"release": "3.0.2", "previous": "3.0.1", "steps": ["started", "scheduler-stopped"]}
        self.agent.save_state({**self.agent.state(), "deploying": interrupted})
        self.cloud.parameters.update({"migrate-pointer": "3.0.2", "pointer": "3.0.2"})
        self.a_migrate_job_that_refuses()
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "migration-failed")
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()))
        self.assertEqual(self.agent.state()["deploying"]["release"], "3.0.2")
        # The attempt goes on once the migration passes.
        self.agent.run_command = self.run
        self.assertEqual(self.agent.command("deploy-now", request_id="r")["state"], "deployed")
        state = self.agent.state()
        self.assertEqual(state["release"], "3.0.2")
        self.assertNotIn("deploying", state)
        self.assertNotIn("migration_failed", state)

    def a_later_release_failed_to_migrate(self):
        """A scheduler host running 3.0.1, whose migration to 3.0.2 failed, and
        the pointer back at 3.0.1. Its last release report says nothing of
        what the failed attempt did to the database."""
        self.host(["scheduler"], migrations=True)
        self.config["status_reference"] = "status"
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": "3.0.1"})
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters["migrate-pointer"] = "3.0.2"
        recorder = self.run

        def run(command, **kwargs):
            if "migrate" in command:
                recorder.commands.append(command)
                raise subprocess.CalledProcessError(1, command)
            return recorder(command, **kwargs)
        self.agent.run_command = run
        self.assertEqual(self.agent.reconcile()["state"], "migration-failed")
        self.agent.run_command = recorder
        state = self.agent.state()
        self.assertEqual((state["migrated_release"], state["migration_failed"]["release"]), ("3.0.1", "3.0.2"))
        self.cloud.parameters["migrate-pointer"] = "3.0.1"
        self.cloud.statuses.clear()
        self.run.commands.clear()

    def migration_reports(self):
        return [(s["state"], s["release"]) for s in self.cloud.statuses]

    def test_a_host_reports_when_the_pointer_returns_after_a_failed_migration(self):
        # The release helper waits for a report from since it moved the pointer.
        self.a_later_release_failed_to_migrate()
        self.run.migration_level = lambda: 22
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.1"), status)
        self.assertEqual(self.migration_reports(), [("migrated", "3.0.1")])
        state = self.agent.state()
        self.assertNotIn("migration_failed", state)
        self.assertEqual((state["migrated_release"], state["migrations"], state["release"]), ("3.0.1", 22, "3.0.1"))
        self.assertTrue(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands), "it read the ledger")
        self.assertFalse(any(c.startswith(("stop", "up")) for c in self.run.compose_calls()), "migrating restarts nothing")
        self.run.commands.clear()
        self.assertIsNone(self.agent.reconcile(), "a release reports once")
        self.assertEqual(self.migration_reports(), [("migrated", "3.0.1")])
        self.assertEqual(self.run.commands, [])

    def a_pointer_that_went_and_came_back(self):
        """A scheduler host on 3.0.1 whose migrate pointer went to 3.0.2 and
        back before it acted: nothing to do, and nothing said since it moved."""
        self.host(["scheduler"], migrations=True)
        self.config["status_reference"] = "status"
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": "3.0.1"})
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.cloud.statuses.clear()
        self.run.commands.clear()

    def test_deploy_now_says_again_what_a_pointer_that_came_back_is_waiting_for(self):
        self.a_pointer_that_went_and_came_back()
        self.assertIsNone(self.agent.reconcile(), "only deploy-now says it again")
        self.assertEqual(self.cloud.statuses, [])
        report = self.agent.command("deploy-now", request_id="r")
        # The command's answer stays its own, and is the last the helper reads.
        self.assertEqual((report["state"], report["request_id"]), ("current", "r"), report)
        migrated, answer = self.cloud.statuses
        self.assertEqual((migrated["state"], migrated["release"]), ("migrated", "3.0.1"), migrated)
        self.assertNotIn("request_id", migrated, "run() must not take this for the host's answer")
        self.assertEqual(answer["request_id"], "r")
        self.assertFalse(any("--rm" in c for c in self.run.compose_calls()), "the release stands, so no job runs")
        self.assertEqual(self.agent.state()["migrated_release"], "3.0.1")

    def test_saying_it_again_does_not_stop_deploy_now_resuming_a_stopped_host(self):
        self.a_pointer_that_went_and_came_back()
        self.agent.command("drain-and-stop", request_id="s1")
        self.cloud.statuses.clear()
        report = self.agent.command("deploy-now", request_id="r")
        self.assertEqual((report["state"], report["request_id"]), ("resumed", "r"), report)
        self.assertNotIn("stopped", self.agent.state())
        self.assertEqual([s["state"] for s in self.cloud.statuses], ["migrated", "resumed"])
        self.assertNotIn("request_id", self.cloud.statuses[0])

    def test_a_host_refuses_once_when_the_pointer_returns_to_a_database_past_it(self):
        # The failed attempt left the database above what 3.0.1 knows, so the
        # helper hears that, once, and not that the old mark still stands.
        self.a_later_release_failed_to_migrate()
        self.run.migration_level = lambda: 23
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migration-failed", "3.0.1"), status)
        self.assertIn("knows migrations up to 22, but the database has 23", status["error"])
        self.assertFalse(any("run --rm migrate" in c for c in self.run.compose_calls()))
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.1")])
        self.assertEqual(self.agent.state()["migration_failed"]["release"], "3.0.1")
        for _ in range(2):
            self.run.commands.clear()
            self.assertIsNone(self.agent.reconcile(), "a refused release waits for deploy-now")
            self.assertEqual(self.run.commands, [])
        self.assertEqual(self.migration_reports(), [("migration-failed", "3.0.1")])
        # Still above it, deploy-now gets the refusal; restored, the release stands.
        report = self.agent.command("deploy-now", request_id="r")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("migration-failed", "3.0.1", "r"))
        self.run.migration_level = lambda: 22
        report = self.agent.command("deploy-now", request_id="r2")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("migrated", "3.0.1", "r2"))
        self.assertNotIn("migration_failed", self.agent.state())
        self.assertIsNone(self.agent.reconcile())

    def about_to_migrate(self):
        """A migration host on 3.0.1, with the pointer at a new release."""
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        self.manifests["3.0.2"] = manifest("3.0.2", latest=23)
        self.cloud.parameters["migrate-pointer"] = "3.0.2"
        self.run.commands.clear()

    # What only some roles' hosts refuse, and what each says.
    REFUSED_BY_OTHER_ROLES = (({"HOT_GOOGLE_CLIENT_ID": "the-value"}, "HOT_GOOGLE_CLIENT_SECRET has no value"),
                              ({"HOT_GITHUB_CLIENT_SECRET": ""}, "HOT_GITHUB_CLIENT_ID and HOT_GITHUB_CLIENT_SECRET have no value"),
                              ({"HOT_EMAIL_SMTP_PASSWORD": ""}, "entry HOT_EMAIL_SMTP_PASSWORD is empty"),
                              ({"HOT_NETWORK_EGRESS_PROXY": "\n"}, "entry HOT_NETWORK_EGRESS_PROXY is empty"),
                              ({"TLS_PRIVATE_KEY": ""}, "entry TLS_PRIVATE_KEY is empty"))

    def test_the_migration_host_refuses_a_new_release_any_role_would_refuse(self):
        # The database stays as it is and no role rolls, so fixing it costs nothing.
        for entry, refusal in self.REFUSED_BY_OTHER_ROLES:
            with self.subTest(entry=entry):
                self.about_to_migrate()
                self.cloud.statuses.clear()
                operator = dict(self.cloud.secrets[OPERATOR])
                self.cloud.secrets[OPERATOR].update(entry)
                status = self.agent.reconcile()
                self.assertEqual((status["state"], status["release"]), ("migration-failed", "3.0.2"), status)
                self.assertIn(refusal, status["error"])
                self.assertNotIn("the-value", status["error"])
                self.assertEqual(self.run.commands, [], "nothing ran, not even the job that reads the level")
                state = self.agent.state()
                self.assertEqual((state["migrated_release"], state["migrations"]), ("3.0.1", 22))
                self.assertFalse((Path(self.tmp.name) / "releases/3.0.2/bundle").exists())
                self.assertEqual([s["state"] for s in self.cloud.statuses], ["migration-failed"])
                self.assertIsNone(self.agent.reconcile(), "it waits for deploy-now")
                self.cloud.secrets[OPERATOR] = operator
                report = self.agent.command("deploy-now", request_id="r")
                self.assertEqual((report["state"], report["release"]), ("migrated", "3.0.2"), report)
                self.assertNotIn("migration_failed", self.agent.state())
                self.agent.save_state({})
                shutil.rmtree(self.agent.releases / "3.0.2")

    def test_a_replaced_migration_host_catches_up_whatever_another_role_would_refuse(self):
        # The database has this release, and a refusal here would record that it
        # does not, for a host that must come up.
        for entry, refusal in self.REFUSED_BY_OTHER_ROLES:
            with self.subTest(entry=entry):
                self.agent.save_state({})
                self.host(["scheduler"], migrations=True)
                self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
                operator = dict(self.cloud.secrets[OPERATOR])
                self.cloud.secrets[OPERATOR].update(entry)
                with mock.patch("sys.stderr"):
                    status = self.agent.reconcile()
                self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.1"), status)
                self.assertNotIn("migration_failed", self.agent.state())
                self.cloud.secrets[OPERATOR] = operator

    def test_the_pointer_back_at_this_hosts_mark_does_not_stop_on_another_roles_refusal(self):
        # The way back from a refused release, with the cause still there.
        self.a_later_release_failed_to_migrate()
        self.run.migration_level = lambda: 22
        self.cloud.secrets[OPERATOR]["HOT_GOOGLE_CLIENT_ID"] = "google-id"
        with mock.patch("sys.stderr"):
            status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("migrated", "3.0.1"), status)

    def test_every_secret_a_role_host_names_is_delivered(self):
        self.host(["scheduler"], migrations=True)
        self.cloud.parameters["migrate-pointer"] = "3.0.1"
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(undelivered_secrets(self.tmp.name), [])
        self.assertTrue((Path(self.tmp.name) / "secrets/check-postgres/HOT_DB_MIGRATION_SERVICE").exists())

    def test_a_scratch_host_publishes_its_metrics_without_a_data_volume(self):
        # As the scalable stacks configure their hosts: no data volume to back up.
        del self.config["data_volume_id"]
        self.config["data_disk"] = "scratch"
        self.task_host()
        self.idle_for(36)
        published = [metrics for _, metrics in self.cloud.metrics]
        self.assertTrue(published)
        self.assertTrue(all("AgentHeartbeat" in metrics for metrics in published))
        self.assertIn("TaskSlotsUsedPercent", published[-1])
        self.assertFalse(any("BackupAgeHours" in metrics for metrics in published))
        self.assertEqual(self.cloud.left, ["hot-example-task-worker"], "an idle host still retires")

    def test_a_new_scheduler_host_mounts_its_scratch_disk_before_migrating(self):
        self.host(["scheduler"], migrations=True)
        self.config["data_disk"] = "scratch"
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        written = []
        recorder = self.run

        def run(command, **kwargs):
            if command[0].endswith("hot-data-disk"):
                # What the migration writes must land on the mounted disk.
                written.append(self.agent.state_path().exists() or (Path(self.tmp.name) / "releases/3.0.1").exists())
            return recorder(command, **kwargs)
        self.agent.run_command = run
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        self.assertEqual(written[:1], [False], "the disk is mounted before anything is written to it")
        disk = next(c for c in self.run.commands if c[0].endswith("hot-data-disk"))
        self.assertIn("--allow-format", disk)
        self.run.commands.clear()
        with mock.patch.object(agent_module.os.path, "ismount", return_value=True):
            self.assertIsNone(self.agent.reconcile(), "the migration is remembered on the mounted disk")
        self.assertFalse(any(c[0].endswith("hot-data-disk") for c in self.run.commands))

    def test_the_migration_host_signs_in_to_a_private_mirror_first(self):
        self.host(["scheduler"], migrations=True)
        self.config["image_registry"] = "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot"
        self.cloud.registry_password = lambda host: "token"
        self.cloud.parameters.update({"migrate-pointer": "3.0.1", "pointer": None})
        self.assertEqual(self.agent.reconcile()["state"], "migrated")
        commands = [" ".join(c) for c in self.run.commands]
        login = next(i for i, c in enumerate(commands) if c.startswith("docker login"))
        self.assertLess(login, next(i for i, c in enumerate(commands) if "run --rm prepare-postgres" in c))


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

    def managed(self, postgres=True, valkey=False):
        data = {}
        if postgres:
            data["postgres"] = {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
                "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}
        if valkey:
            data["valkey"] = {"mode": "managed", "endpoint": {"host": self.CACHE, "port": 6379, "auth_secret": self.TOKEN}}
        return data

    def move(self, postgres=True, valkey=False):
        self.config["data"] = self.managed(postgres, valkey)
        self.release("3.0.2")
        self.run.commands.clear()
        return self.agent.reconcile()

    def calls(self):
        return [" ".join(c) for c in self.run.commands]

    def test_a_data_move_waits_for_a_storage_move_put_back_partway(self):
        # Put back in the apply that moves the data, the storage shows no move for
        # the rule about moving both, and a failed restore would leave the new
        # bundle and the new logins on a state that says the data is still local.
        installed = self.config["storage"]
        self.failing = "move-storage"
        self.config["storage"] = dict(installed, file=StorageMoves.S3)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual(self.agent.storage_progress(self.agent.state()["failed"]), "moving")
        bundle = (Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text()
        secrets = Path(self.tmp.name) / "secrets"
        before = {path: path.read_text() for path in secrets.rglob("*") if path.is_file()}
        self.config["storage"] = installed
        self.config["data"] = self.managed()
        self.failing = "restore-postgres"
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["started"]), report)
        self.assertIn("Move storage and data services in separate deploys: a storage move has not finished, and a data "
                      "move must wait for it. Put data.postgres back, apply, and run deploy-now, then move the data in "
                      "a deploy of its own", report["error"])
        # Only recovery's stop of what is already stopped: nothing drains, dumps or is delivered.
        self.assertEqual([c for c in self.run.compose_calls() if not c.startswith("stop ")], [])
        self.assertEqual((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text(), bundle)
        self.assertEqual({path: path.read_text() for path in secrets.rglob("*") if path.is_file()}, before)
        state = self.agent.state()
        self.assertEqual(state["data_modes"]["postgres"], "local")
        self.assertEqual(self.agent.storage_progress(state["failed"]), "moving")
        self.assertTrue(state["stopped"])
        # The way the refusal gives ends that move on the old storage, and the data can then move.
        del self.config["data"]
        self.failing = None
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "resumed", report)
        self.assertIn("Records already moved stay on the new storage", report["warning"])
        self.assertNotIn("stopped", self.agent.state())
        self.config["data"] = self.managed()
        report = self.agent.command("deploy-now", request_id="d3")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_the_box_pool_holds_its_space_until_the_databases_moved_off_the_disk(self):
        # host.env is written before the move: a move that fails before the
        # data moved keeps Hot on the local database, so the pool goes on
        # holding (hot-box-pool-hold reads the flag) and holds after a reboot.
        host_env = Path(self.tmp.name) / "etc/host.env"
        self.failing = "restore-postgres"
        status = self.move(valkey=True)
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "local")
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=1\n", host_env.read_text())
        self.agent.configure_host()
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=1\n", host_env.read_text())
        # Once the move finished, nothing on the disk needs it held.
        self.failing = None
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "managed", "valkey": "managed"})
        self.assertIn("HOT_BOX_POOL_PREALLOCATE=0\n", host_env.read_text())

    def test_a_reboot_whose_box_pool_does_not_start_is_a_warning_with_the_data_off_the_disk(self):
        # A sparse pool, with no database on the disk: nothing is held, and
        # a refusal is never for room, but box tasks still wait.
        status = self.move(postgres=True, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        self.assertFalse(self.agent.holds_box_pool())
        self.run.pool_active = False  # dm-thin refused the pool, say
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        report = self.agent.resume()
        self.assertEqual(report["state"], "resumed", report)
        self.assertIn("Kata's box pool is not active, so neither is its containerd, so box tasks do not run on "
                      "this host: they wait in the queue, and none fail, until it starts. Code tasks still run",
                      report["warning"])
        self.assertIn("Run deploy-now", report["warning"])
        self.assertIn("Kata's box pool is not active", self.agent.diagnostics(None)["box_pool"])
        # Kata's containerd down with the pool up (hot-guest-firewall not
        # active, say) is said with what systemd says of it.
        self.run.pool_active = True
        run = self.agent.run_command

        def restarting(command, **kwargs):
            if command == ["systemctl", "is-active", "kata-containerd.service"]:
                raise subprocess.CalledProcessError(3, command, output="activating\n")
            return run(command, **kwargs)
        self.agent.run_command = restarting
        self.assertTrue(self.agent.box_pool_warning().startswith(
            "Kata's containerd is not running (activating), so box tasks do not run on this host"))
        self.assertNotIn("another task host", self.agent.box_pool_warning())
        # A scalable task host's box tasks may run on another task host.
        before = {key: self.config.get(key) for key in ("preset", "roles")}
        self.config.update(preset="scalable", roles=["task-worker"])
        self.assertIn("they wait in the queue, and none fail, until it starts, or another task host runs them. "
                      "Code tasks still run", self.agent.box_pool_warning())
        self.config.update(before)
        # Both running, nothing; nor on a host that runs no task worker.
        self.agent.run_command = run
        self.assertEqual(self.agent.box_pool_warning(), "")
        self.agent.run_command = restarting
        self.config.update(preset="scalable", roles=["app"])
        self.assertEqual(self.agent.box_pool_warning(), "")

    def test_a_local_database_moves_to_the_managed_one(self):
        owners = []
        self.agent.chown = lambda path, uid, gid: owners.append(
            (Path(path).name, uid, gid, Path(path).stat().st_mode & 0o777))
        status = self.move()
        self.assertEqual(status["state"], "deployed", status)
        self.assertIn(("postgres.dump", 10001, 10001, 0o600), owners, "only the restore job's user reads the dump")
        self.assertEqual(self.agent.state_path().stat().st_mode & 0o777, 0o600)
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
        self.assertNotIn("dependencies", self.agent.state(), "service addresses stay in memory")
        self.assertFalse((Path(self.tmp.name) / "data/moves/postgres.dump").exists(), "the staged copy is removed")
        self.assertIn("restore_postgres.sh", [p.name for p in (Path(self.tmp.name) / "releases/3.0.2/bundle").iterdir()])

    def test_a_move_within_the_running_release_dumps_through_the_running_bundle(self):
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"), status)
        dump = next(c for c in self.run.commands if "pg_dump" in c)
        compose = Path(dump[dump.index("-f") + 1])
        self.assertEqual(compose.parent.name, "bundle-running")
        self.assertIn("postgres", json.loads(compose.read_text())["services"], "the running bundle, not the new one")

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
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "local", "valkey": "local"})

    def hang_queue_status(self, times=None):
        """`hot queue status` does not answer, `times` times or for good."""
        run, self.asked, left = self.agent.run_command, [], [times]

        def hanging(command, **kwargs):
            if "queue" in command and left[0] != 0:
                self.asked.append(kwargs["timeout"])
                left[0] = None if left[0] is None else left[0] - 1
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return run(command, **kwargs)
        self.agent.run_command = hanging

    def test_a_queue_status_that_hangs_counts_toward_the_drain(self):
        self.hang_queue_status()
        status = self.move(valkey=True)
        self.assertEqual(status["state"], "failed")
        self.assertIn("did not finish within 15 minutes", status["error"])
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertFalse(any("pg_dump" in c for c in self.calls()))
        self.assertEqual(self.asked, [agent_module.EXEC_SECONDS] * 7, "each one uses up its time from the wait")

    def test_a_queue_status_that_hangs_once_is_asked_again(self):
        self.hang_queue_status(times=2)
        status = self.move(postgres=False, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(len(self.asked), 2)

    def stall_queue_status(self, times=None):
        """A Valkey that stalls or fails over, `times` times or for good: `hot
        queue status --json` gives up and exits 1, and the text form says why
        and exits 0."""
        run, self.stalled, left = self.agent.run_command, [], [times]

        def stalled(command, **kwargs):
            if "queue" not in command or left[0] == 0:
                return run(command, **kwargs)
            self.stalled.append(command)
            if "--json" in command:
                raise subprocess.CalledProcessError(1, command, stderr="Error: connection timed out")
            left[0] = None if left[0] is None else left[0] - 1
            return subprocess.CompletedProcess(command, 0, stdout=(
                "Queue Status:\n  Type: redis\n  Backend: Redis (redis://hot:***@valkey:6379)\n\n"
                "  Query failed: connection timed out\n"))
        self.agent.run_command = stalled

    def test_a_queue_that_stalls_once_is_asked_again(self):
        self.stall_queue_status(times=1)
        status = self.move(postgres=False, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(len(self.stalled), 2, "both forms, once")

    def test_a_queue_that_stays_stalled_gives_up_when_the_wait_ends(self):
        self.stall_queue_status()
        status = self.move(valkey=True)
        self.assertEqual(status["state"], "failed")
        self.assertIn("did not finish within 15 minutes", status["error"])
        self.assertIn("Could not read the queue's status from the worker", status["error"])
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertFalse(any("pg_dump" in c for c in self.calls()))

    def test_the_time_a_stalled_status_takes_counts_toward_the_wait(self):
        self.stall_queue_status()
        now = [0]

        def clock():  # both forms take about 80 seconds together
            now[0] += 80
            return now[0]
        with self.assertRaisesRegex(agent_module.AgentError, "did not finish within 15 minutes"):
            self.agent.drain(Path(self.tmp.name) / "releases/3.0.1/bundle", queue=True, clock=clock)
        self.assertEqual(len(self.stalled), 2 * 11, "90 seconds a round, not 10")

    def test_a_status_the_worker_cannot_run_counts_toward_the_wait(self):
        run, asked = self.agent.run_command, []

        def failing(command, **kwargs):
            if "queue" in command and len(asked) < 2:
                asked.append(command)
                raise subprocess.CalledProcessError(1, command, stderr="service worker is restarting")
            return run(command, **kwargs)
        self.agent.run_command = failing
        status = self.move(postgres=False, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(len(asked), 2)

    def test_a_failed_restore_returns_to_the_local_data(self):
        self.failing = "restore-postgres"
        status = self.move()
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("The database was not changed", status["recovery"])
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "local")
        # The installed release runs with its own secrets, not the new database's.
        secrets = Path(self.tmp.name) / "secrets"
        self.assertIn("@postgres:5432/", (secrets / "worker/HOT_DB_URI").read_text())
        self.assertFalse((secrets / "worker/POSTGRES_CA_CERT").exists())
        self.assertTrue((secrets / "postgres/POSTGRES_PASSWORD").exists())
        self.assertFalse(secrets.with_name("secrets.installed").exists())

    def assert_installed_secrets(self):
        """What the installed release, on this host's data services, reads."""
        secrets = Path(self.tmp.name) / "secrets"
        self.assertIn("@postgres:5432/", (secrets / "worker/HOT_DB_URI").read_text())
        self.assertIn("@valkey:6379", (secrets / "worker/HOT_REDIS_URI").read_text())
        self.assertFalse((secrets / "worker/POSTGRES_CA_CERT").exists())
        for name in ("postgres/POSTGRES_PASSWORD", "postgres/POSTGRES_RUNTIME_PASSWORD", "provision/PGPASSFILE",
                     "valkey/VALKEY_PASSWORD"):
            self.assertTrue((secrets / name).exists(), name)
        self.assertIn("@postgres:5432/", (secrets / "migrate/HOT_DB_MIGRATION_URI").read_text())
        self.assertFalse(secrets.with_name("secrets.installed").exists())

    def assert_a_failed_move_from_a_stopped_start_gives_the_secrets_back(self, failing, **moving):
        self.agent.command("drain-and-stop", request_id="s1")
        installed = self.agent.state()["release"]
        self.failing = failing
        status = self.move(**moving)
        self.assertEqual(status["state"], "failed", status)
        self.assertNotIn("data-moved", status["steps"])
        self.assertIn("Hot stays stopped, as it was before this deploy", status["recovery"])
        self.assertTrue(self.agent.state()["stopped"])
        self.assert_installed_secrets()
        # The release that failed to move stays on the host beside it; the
        # installed one is what a start runs.
        self.assertEqual(undelivered_secrets(self.tmp.name, installed), [])

    def test_a_failed_database_move_from_a_stopped_start_gives_the_secrets_back(self):
        self.assert_a_failed_move_from_a_stopped_start_gives_the_secrets_back("restore-postgres")

    def test_a_failed_queue_move_from_a_stopped_start_gives_the_secrets_back(self):
        self.assert_a_failed_move_from_a_stopped_start_gives_the_secrets_back("migrate", postgres=False, valkey=True)

    def test_a_failed_move_of_both_from_a_stopped_start_gives_the_secrets_back(self):
        self.assert_a_failed_move_from_a_stopped_start_gives_the_secrets_back("restore-postgres", valkey=True)

    def test_a_retry_of_a_move_from_a_stopped_start_reads_the_level_it_left(self):
        self.agent.command("drain-and-stop", request_id="s1")
        self.failing = "restore-postgres"
        self.assertEqual(self.move()["state"], "failed")
        delivered = Path(self.tmp.name) / "secrets/migrate/HOT_DB_MIGRATION_URI"
        logins = []

        def level():
            logins.append(delivered.read_text())
            return 22
        self.run.migration_level = level
        self.failing = None
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(len(logins), 1)
        self.assertIn("@postgres:5432/", logins[0], "the installed bundle's job, with the login it knows")
        self.assertNotIn("stopped", self.agent.state())
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_a_move_from_a_stopped_start_cut_short_and_put_back_gives_the_secrets_back(self):
        self.agent.command("drain-and-stop", request_id="s1")
        self.config["data"] = self.managed()
        self.release("3.0.2")
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "restore-postgres" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        self.config.pop("data")
        self.release("3.0.1")
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Hot stays stopped", status["recovery"])
        self.assert_installed_secrets()

    def test_a_failure_from_a_stopped_start_before_the_move_delivers_nothing(self):
        # Its files stay as they were, as with any refusal up to the secrets step.
        self.agent.command("drain-and-stop", request_id="s1")
        secrets = Path(self.tmp.name) / "secrets"
        before = {path: path.stat().st_ino for path in secrets.rglob("*") if path.is_file()}
        self.failing = "pull"
        status = self.move()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Hot stays stopped, as it was before this deploy", status["recovery"])
        self.assertNotIn("resume_failed", self.agent.state())
        self.assertEqual({path: path.stat().st_ino for path in secrets.rglob("*") if path.is_file()}, before)

    def test_a_restart_is_refused_once_an_upgrade_moved_the_data(self):
        self.config["data"] = self.managed(postgres=True)
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertIn("data-moved", status["steps"])
        self.assertTrue(self.agent.state()["stopped"])
        self.failing = None
        self.run.commands.clear()
        for service in ("app", "worker", "task-worker", "scheduler", "ingress"):
            with self.subTest(service=service):
                report = self.agent.answer("restart-service", service=service, request_id="r-" + service)
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("the installed bundle still describes the copy it left", report["error"])
                self.assertIn("deploy-now", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing recreated")
        self.assertIn("@" + self.HOST, (Path(self.tmp.name) / "secrets/worker/HOT_DB_URI").read_text())
        # The way on, after which the bundle and the data agree.
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.agent.answer("restart-service", service="app", request_id="r1")["state"], "restarted")

    def test_a_restart_is_refused_while_an_upgrade_that_moved_the_data_is_cut_short(self):
        self.config["data"] = self.managed(postgres=True)
        self.release("3.0.2", latest=23)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "migrate" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        self.assertIn("data-moved", self.agent.state()["deploying"]["steps"])
        report = self.agent.answer("restart-service", service="worker", request_id="r1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("deploy-now", report["error"])

    def test_a_restart_goes_on_when_the_new_bundle_is_in_place(self):
        # A move within the running release: the bundle installed is the new one.
        self.config["data"] = self.managed(postgres=True)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertIn("data-moved", status["steps"])
        self.failing = None
        report = self.agent.answer("restart-service", service="worker", request_id="r1")
        self.assertEqual(report["state"], "restarted", report)

    def test_a_failed_move_within_the_running_release_returns_to_its_own_bundle(self):
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        bundle = Path(self.tmp.name) / "releases/3.0.1/bundle"
        self.failing = "restore-postgres"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("previous-release-restarted", status["steps"])
        restart = next(c for c in reversed(self.run.commands) if "--remove-orphans" in c)
        self.assertEqual(restart[restart.index("-f") + 1], str(bundle / "compose.json"))
        self.assertIn("postgres", json.loads((bundle / "compose.json").read_text())["services"],
                      "the installed bundle, so the local database is no orphan")
        self.assertFalse(bundle.with_name("bundle-running").exists())
        self.assertIn("@postgres:5432/", (Path(self.tmp.name) / "secrets/worker/HOT_DB_URI").read_text())
        # A retry drains and dumps through that bundle again.
        self.failing = None
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        dump = next(c for c in self.run.commands if "pg_dump" in c)
        self.assertIn("postgres", json.loads(Path(dump[dump.index("-f") + 1]).read_text())["services"])
        self.assertNotIn("postgres", json.loads((bundle / "compose.json").read_text())["services"])

    def test_a_reboot_during_a_move_starts_only_the_installed_data_services(self):
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "check-postgres" in command:
                raise KeyboardInterrupt  # the host goes down mid-move
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.assertIn("secrets", self.agent.state()["deploying"]["steps"])
        # The boot empties the tmpfs.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        shutil.rmtree(Path(self.tmp.name) / "secrets.installed", ignore_errors=True)
        self.agent.run_command = run
        self.run.commands.clear()
        report = self.agent.resume()
        self.assertEqual((report["state"], report["interrupted"]), ("resumed", "3.0.1"), report)
        self.assertEqual(self.run.compose_calls(), ["up -d --wait postgres valkey"], "Hot itself waits for the deploy")
        self.assertIn("releases/3.0.1/bundle/compose.json", " ".join(self.run.commands[0]))
        self.assertIn("@postgres:5432/", (Path(self.tmp.name) / "secrets/worker/HOT_DB_URI").read_text())
        # The deploy then resumes on the local data, and finishes the move.
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_a_reboot_before_a_move_runs_starts_the_installed_release_on_its_own_data(self):
        # The configuration asks for the managed database and cache, and the
        # move has not run (or failed) when the host reboots.
        self.config["data"] = {
            "postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
                "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}},
            "valkey": {"mode": "managed", "endpoint": {"host": self.CACHE, "port": 6379, "auth_secret": self.TOKEN}}}
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertIn("up -d --wait postgres valkey", self.run.compose_calls())
        secrets = Path(self.tmp.name) / "secrets"
        self.assertTrue((secrets / "postgres/POSTGRES_PASSWORD").exists())
        self.assertTrue((secrets / "valkey/VALKEY_ACL").exists())
        self.assertIn("@postgres:5432/", (secrets / "worker/HOT_DB_URI").read_text())
        self.assertIn("@valkey:6379", (secrets / "worker/HOT_REDIS_URI").read_text(), "the old queue, not the new one")
        self.assertNotIn(self.HOST, (secrets / "app/HOT_DB_URI").read_text())

    def test_an_interrupted_move_no_longer_asked_for_returns_to_the_installed_release(self):
        installed = json.loads(json.dumps(self.config))
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "check-postgres" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.config.clear()
        self.config.update(installed)  # the operator reverts the move
        self.agent.resume()
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertNotIn("deploying", self.agent.state())
        self.assertIn("up -d --remove-orphans app api worker task-worker", self.run.compose_calls())
        self.assertIsNone(self.agent.reconcile(), "nothing is left to deploy")

    def test_a_queue_moving_in_a_maintenance_window_is_drained_by_its_workers(self):
        self.agent.command("drain-and-stop", request_id="s1")
        self.run.queue = [(5, 0, 0)]
        self.config["data"] = {"valkey": {"mode": "managed", "endpoint": {
            "host": self.CACHE, "port": 6379, "auth_secret": self.TOKEN}}}
        self.release("3.0.2")
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed")
        calls = self.calls()
        start = next(i for i, c in enumerate(calls) if c.endswith("up -d --no-recreate worker task-worker"))
        self.assertLess(start, next(i for i, c in enumerate(calls) if "queue status" in c))
        self.assertIn("Hot stays stopped, as it was before this deploy", status["recovery"])
        self.assertTrue(calls[-1].endswith("stop app api ingress"), "the workers started to drain stop again")
        self.assertFalse(any(c.endswith("up -d scheduler") for c in calls))
        self.assertTrue(self.agent.state()["stopped"])

    def test_a_moving_queue_after_a_reboot_while_stopped_finds_its_data_and_workers(self):
        self.stopped_and_rebooted()
        self.run.queue = [(2, 0, 0), (0, 0, 0)]
        status = self.move(postgres=False, valkey=True)
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        data = next(i for i, c in enumerate(calls) if c.endswith("up -d --wait --no-recreate postgres valkey"))
        workers = next(i for i, c in enumerate(calls) if c.endswith("up -d --no-recreate worker task-worker"))
        self.assertLess(data, workers)
        self.assertLess(workers, next(i for i, c in enumerate(calls) if "queue status" in c))
        self.assertIn("releases/3.0.1/bundle/compose.json", calls[data], "the installed release's own")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_moving_database_after_a_reboot_while_stopped_is_dumped_from_its_own(self):
        self.stopped_and_rebooted()
        status = self.move()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        data = next(i for i, c in enumerate(calls) if c.endswith("up -d --wait --no-recreate postgres valkey"))
        self.assertLess(data, next(i for i, c in enumerate(calls) if "exec -T postgres pg_dump" in c))
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_a_move_from_a_managed_cache_after_a_reboot_says_how_to_start_it(self):
        self.assertEqual(self.move(postgres=False, valkey=True)["state"], "deployed")
        managed = self.config["data"]
        self.agent.command("drain-and-stop", request_id="s1")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.config["data"] = {}
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Describe it in data.valkey again and run deploy-now", status["error"])
        self.assertTrue(self.agent.state()["stopped"])
        self.config["data"] = managed
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.2"), report)

    def test_staging_the_dump_needs_room_and_leaves_nothing_behind(self):
        # Room for the dump itself, not for its staged copy.
        def usage(path):
            return mock.Mock(free=2**20 if Path(path).name == "moves" else 2**40)
        with mock.patch.object(agent_module.shutil, "disk_usage", usage):
            status = self.move()
        self.assertEqual(status["state"], "failed")
        self.assertIn("Staging the database dump for the restore needs", status["error"])
        self.assertFalse(any("restore-postgres" in c for c in self.calls()))
        self.assertIn("previous-release-restarted", status["steps"])
        staged = Path(self.tmp.name) / "data/moves/postgres.dump"

        copyfile = shutil.copyfile

        def partial(source, target, **kwargs):
            if Path(target) != staged:
                return copyfile(source, target, **kwargs)
            Path(target).write_text("part of the dump")
            raise OSError(28, "No space left on device")
        self.failing = None
        with mock.patch.object(agent_module.shutil, "copyfile", partial):
            status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed")
        self.assertIn("No space left on device", status["error"])
        self.assertFalse(staged.exists(), "no partial copy is left on the disk")
        self.assertNotIn("deploying", self.agent.state())

    def test_a_staged_copy_a_kill_left_behind_does_not_count_against_the_retry(self):
        staged = Path(self.tmp.name) / "data/moves/postgres.dump"
        staged.parent.mkdir(parents=True)
        staged.write_text("part of the dump")
        room = agent_module.STAGING_RESERVE_BYTES + len("dump")

        def usage(path):
            if Path(path).name != "moves":
                return mock.Mock(free=2**40)
            return mock.Mock(free=room - (staged.stat().st_size if staged.exists() else 0))
        with mock.patch.object(agent_module.shutil, "disk_usage", usage):
            status = self.move()
        self.assertEqual(status["state"], "deployed", status)
        self.assertFalse(staged.exists())

    def test_after_the_data_moves_failures_are_fixed_forward(self):
        self.failing = "migrate"
        status = self.move()
        self.assertIn("data now lives in the new database", status["recovery"])
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")
        with self.assertRaisesRegex(agent_module.AgentError, "already moved"):
            self.agent.command("roll-back")

    def fix_forward_with_a_move(self, **moving):
        """3.0.2's migrations fail partway on the local database. The operator
        then asks for a fixed release and a move at once. Returns the refusal and
        the restore point from before 3.0.2."""
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        point = self.agent.state()["failed"]["restore_point"]
        self.assertIn("/pre-3.0.2-", point["path"])
        self.config["data"] = self.managed(**moving)
        self.release("3.0.3", latest=24)
        self.run.commands.clear()
        return self.agent.reconcile(), point

    def check_nothing_stopped(self, status, point, setting):
        self.assertEqual(status["state"], "failed", status)
        self.assertIn(f"{setting} cannot change yet", status["error"])
        self.assertIn(f"run roll-back: it restores the database from {point['path']}", status["recovery"])
        self.assertNotIn("Hot stays stopped", status["recovery"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops, drains, or dumps")
        state = self.agent.state()
        self.assertEqual((state["failed"]["restore_point"], state["failed"]["partly_migrated"]), (point, "3.0.2"))
        # 3.0.3 never ran, whatever the failed record's own release says.
        self.assertEqual(state["failed"]["release"], "3.0.3")
        self.assertIn("the failed deploy of 3.0.2 may have", self.agent.scheduler_held(state)["scheduler"])
        refused = self.agent.answer("restart-service", "scheduler", request_id="r-scheduler")
        self.assertIn("the failed deploy of 3.0.2 may have", refused["error"])
        self.assertEqual(state["data_modes"], {"postgres": "local", "valkey": "local"})
        self.assertNotIn("stopped", state)
        secrets = Path(self.tmp.name) / "secrets"
        self.assertIn("@postgres:5432/", (secrets / "worker/HOT_DB_URI").read_text())
        self.assertIn("@valkey:6379", (secrets / "worker/HOT_REDIS_URI").read_text())

    def test_a_fix_forward_that_also_moves_the_database_stops_nothing(self):
        status, point = self.fix_forward_with_a_move(postgres=True)
        self.check_nothing_stopped(status, point, "data.postgres.mode")

    def test_a_fix_forward_that_also_moves_the_queue_stops_nothing(self):
        status, point = self.fix_forward_with_a_move(postgres=False, valkey=True)
        self.check_nothing_stopped(status, point, "data.valkey.mode")

    def test_a_fix_forward_that_also_moves_both_names_both(self):
        status, point = self.fix_forward_with_a_move(postgres=True, valkey=True)
        self.check_nothing_stopped(status, point, "data.postgres.mode and data.valkey.mode")

    def test_a_refusal_on_the_installed_release_names_the_release_that_migrated(self):
        installed = json.loads(json.dumps(self.config))
        self.fix_forward_with_a_move(postgres=True)
        self.release("3.0.1")
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        state = self.agent.state()
        self.assertEqual((state["failed"]["release"], state["failed"]["partly_migrated"]), ("3.0.1", "3.0.2"))
        self.assertIn("the failed deploy of 3.0.2 may have", self.agent.scheduler_held(state)["scheduler"])
        self.config.pop("data", None)
        self.config.update(installed)
        self.assertIn("the failed deploy of 3.0.2 may have", self.agent.command("deploy-now", request_id="d1")["scheduler"])
        self.agent.command("drain-and-stop", request_id="s1")
        self.assertIn("the failed deploy of 3.0.2 may have", self.agent.answer("deploy-now", request_id="d2")["error"])

    def test_roll_back_uses_the_dump_from_before_the_refused_fix_forward(self):
        _, point = self.fix_forward_with_a_move(postgres=True)
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertIn("restored", report["steps"])
        restore = next(c for c in self.run.commands if "pg_restore" in c)
        self.assertIn("releases/3.0.1/bundle", " ".join(restore))
        self.assertEqual(self.agent.state()["failed"]["restore_point"], point)
        # The upgrade is over, so the move goes ahead.
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_a_fixed_release_with_the_mode_put_back_finishes_the_upgrade_then_the_move_runs(self):
        self.fix_forward_with_a_move(postgres=True)
        self.config["data"] = {}
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.3"), report)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "local")
        self.config["data"] = self.managed(postgres=True)
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "managed")

    def test_a_retry_after_the_data_moved_is_no_new_move(self):
        self.config["data"] = self.managed(postgres=True)
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.assertIn("data-moved", status["steps"])
        self.failing = None
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)

    def test_a_move_reads_the_level_of_the_database_it_leaves(self):
        delivered = Path(self.tmp.name) / "secrets/migrate/HOT_DB_MIGRATION_URI"
        logins = []

        def level():
            logins.append(delivered.read_text())
            return 22
        self.run.migration_level = level
        self.config["data"] = self.managed(postgres=True)
        self.release("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(len(logins), 1)
        self.assertIn("@postgres:5432/", logins[0], "its secrets stay until the move delivers new ones")

    def test_a_retry_after_the_data_moved_goes_by_the_recorded_level(self):
        # The installed bundle still describes the copy the data left behind.
        self.config["data"] = self.managed(postgres=True)
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.run.commands.clear()
        self.run.migration_level = lambda: self.fail("the job would read the wrong database")
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.assertFalse(any(agent_module.MIGRATION_STATUS_JOB in c for c in self.run.commands))

    def queue_move_fails_at(self, job):
        """A queue move with an upgrade: Hot drained, then `job` fails."""
        self.config["data"] = self.managed(postgres=False, valkey=True)
        self.release("3.0.2", latest=23)
        self.failing = job
        status = self.agent.reconcile()
        self.failing = None
        return status

    def check_queue_move_left_hot_stopped(self, status):
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("drained", status["steps"])
        self.assertTrue(status["recovery"].startswith("Hot stays stopped. Migrations may be partly applied"), status)
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertEqual(state["data_modes"]["valkey"], "local")
        secrets = Path(self.tmp.name) / "secrets"
        self.assertIn("@valkey:6379", (secrets / "worker/HOT_REDIS_URI").read_text(), "its own queue back")
        self.assertFalse((Path(self.tmp.name) / "secrets.installed").exists())

    def test_a_queue_move_whose_migrations_fail_keeps_hot_stopped_and_says_so(self):
        self.check_queue_move_left_hot_stopped(self.queue_move_fails_at("migrate"))

    def test_a_queue_move_whose_provisioning_fails_keeps_hot_stopped_and_says_so(self):
        self.check_queue_move_left_hot_stopped(self.queue_move_fails_at("provision"))

    def test_a_queue_move_whose_new_services_fail_stops_them_again(self):
        checks = []

        def ready():
            checks.append(1)
            raise agent_module.AgentError("The app and API did not become ready")
        self.agent.wait_ready = ready
        self.run.commands.clear()
        status = self.queue_move_fails_at(None)
        self.assertIn("starting", status["steps"])
        self.check_queue_move_left_hot_stopped(status)
        self.assertEqual(self.run.compose_calls()[-1], "stop app api ingress")
        self.assertEqual(len(checks), 1, "nothing tries to start it again")

    def test_after_a_queue_move_left_hot_stopped_the_ways_out_work(self):
        status = self.queue_move_fails_at("migrate")
        point = self.agent.state()["failed"]["restore_point"]
        # A reboot does not start it on this database.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        # Nor does a retry that would drain through it: it says what to do.
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("data.valkey.mode cannot change yet", report["error"])
        self.assertTrue(report["recovery"].startswith("Hot stays stopped."), status)
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertEqual(state["failed"]["restore_point"], point)
        # Roll-back restores the dump, and starts 3.0.1 on its own queue.
        self.run.commands.clear()
        report = self.agent.command("roll-back")
        self.assertIn("restored", report["steps"])
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("stopped", self.agent.state())
        self.assertIn("@valkey:6379", (Path(self.tmp.name) / "secrets/worker/HOT_REDIS_URI").read_text())

    def test_a_queue_move_cut_short_during_migrations_stays_off_this_database(self):
        self.config["data"] = self.managed(postgres=False, valkey=True)
        self.release("3.0.2", latest=23)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "migrate" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("data.valkey.mode cannot change yet", status["error"])
        self.assertTrue(status["recovery"].startswith("Hot stays stopped."), status["recovery"])
        self.assertTrue(self.agent.state()["stopped"])
        self.assertIn("@valkey:6379", (Path(self.tmp.name) / "secrets/worker/HOT_REDIS_URI").read_text())

    def killed_at(self, word):
        """A move is under way when the agent dies; the host stays up."""
        self.config["data"] = self.managed()
        self.release("3.0.2")
        run = self.agent.run_command

        def crash(command, **kwargs):
            if word in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run

    def check_installed_release_restarted(self):
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("HTTP 404", status["error"])
        self.assertIn("previous-release-restarted", status["steps"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        calls = self.run.compose_calls()
        self.assertIn("up -d --remove-orphans app api worker task-worker", calls)
        self.assertEqual(calls[-1], "up -d scheduler")
        self.assertTrue(all("releases/3.0.1/" in c for c in self.calls() if " up " in c))
        self.assertIn("@postgres:5432/", (Path(self.tmp.name) / "secrets/worker/HOT_DB_URI").read_text())
        self.assertNotIn("stopped", self.agent.state())

    def test_a_kill_after_the_drain_then_an_early_failure_restarts_the_installed_release(self):
        self.killed_at("pg_dump")
        self.assertIn("drained", self.agent.state()["deploying"]["steps"])
        self.check_installed_release_restarted()

    def test_a_kill_with_the_new_secrets_delivered_then_an_early_failure_restarts_the_installed_release(self):
        self.killed_at("check-postgres")
        self.assertIn(self.HOST, (Path(self.tmp.name) / "secrets/worker/HOT_DB_URI").read_text())
        self.check_installed_release_restarted()

    def test_a_failed_start_after_the_data_moved_stops_what_it_started(self):
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        status = self.move()
        self.assertIn("data-moved", status["steps"])
        self.assertIn("Hot stays stopped", status["recovery"])
        self.assertTrue(self.agent.state()["stopped"])
        calls = self.run.compose_calls()
        self.assertEqual(calls[-1], "stop app api ingress", "as the recovery says")
        self.assertIn("stop --timeout 120 worker task-worker", calls)

    def test_a_kill_as_services_start_after_the_data_moved_then_an_early_failure_stops_them(self):
        self.config["data"] = self.managed()
        self.release("3.0.2")
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "--remove-orphans" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("Hot stays stopped", status["recovery"])
        self.assertEqual(self.run.compose_calls()[-1], "stop app api ingress")

    def test_a_kill_while_workers_drain_a_stopped_hot_stops_them_again(self):
        self.agent.command("drain-and-stop", request_id="s1")
        self.config["data"] = self.managed(postgres=False, valkey=True)
        self.release("3.0.2")
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "queue" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d1")
        self.agent.run_command = run
        del self.manifests["3.0.2"]
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Hot stays stopped, as it was before this deploy", status["recovery"])
        self.assertIn("stop --timeout 120 worker task-worker", self.run.compose_calls())
        self.assertFalse(any(c.startswith("up") for c in self.run.compose_calls()))
        self.assertTrue(self.agent.state()["stopped"])

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


class MovesToExistingData(UpgradeHarness):
    """The bundle holds an existing service's connection before the installed
    release, on local data, stops needing it."""

    ENTRIES = {"postgres": {"HOT_DB_URI": "postgres://hot_runtime:pw@db.internal.example:5432/hot?sslmode=verify-full",
                            "HOT_DB_MIGRATION_URI": "postgres://hot_owner:pw@db.internal.example:5432/hot?sslmode=verify-full",
                            "POSTGRES_CA_CERT": "-----BEGIN CERTIFICATE-----"},
               "valkey": {"HOT_REDIS_URI": "rediss://:pw@cache.internal.example:6379"}}

    def setUp(self):
        super().setUp()
        self.agent.resolve = lambda hosts: ["10.20.30.40/32"] if hosts else []
        self.run.queue = [(0, 0, 0)]

    def add_entries(self, *services):
        for service in services:
            self.cloud.secrets[OPERATOR].update(self.ENTRIES[service])

    def change_mode(self, *services):
        self.config["data"] = {service: {"mode": "existing"} for service in services}

    def on_local_data(self):
        """The installed release's secrets name its own data services."""
        secrets = Path(self.tmp.name) / "secrets"
        self.assertIn("@postgres:5432/", (secrets / "worker/HOT_DB_URI").read_text())
        self.assertIn("@valkey:6379", (secrets / "worker/HOT_REDIS_URI").read_text())
        self.assertTrue((secrets / "postgres/POSTGRES_PASSWORD").exists())
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "local", "valkey": "local"})

    def rebooted(self):
        """The same disks with empty tmpfs directories, and a new process."""
        for name in ("secrets", "secrets.installed", "secrets.new", "secrets.old"):
            shutil.rmtree(Path(self.tmp.name) / name, ignore_errors=True)
        (Path(self.tmp.name) / "resumed").unlink(missing_ok=True)
        agent = agent_module.Agent(self.config, self.cloud, run=self.agent.run_command, sleep=lambda _: None,
                                   roots=self.roots, chown=lambda *args: None)
        agent.wait_ready, agent.resolve = self.agent.wait_ready, self.agent.resolve
        self.agent = agent

    def test_entries_added_before_the_mode_changes_keep_hot_running(self):
        self.add_entries("postgres", "valkey")
        self.rebooted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.on_local_data()
        self.assertEqual(self.agent.answer("restart-service", service="worker")["state"], "restarted")
        self.assertIsNone(self.agent.reconcile(), "nothing to deploy yet")
        self.assertEqual(self.agent.observe(probe=lambda host, path: 200)["AgentHeartbeat"][0], 1)
        # A deploy that renders refuses them until the mode changes, and leaves
        # Hot as it was.
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        status = self.agent.reconcile()
        self.assertIn("unsupported entry", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.rebooted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.change_mode("postgres", "valkey")
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "deployed", status)
        self.assertEqual(self.agent.state()["data_modes"], {"postgres": "existing", "valkey": "existing"})

    def test_a_failure_before_the_new_secrets_restarts_the_installed_release(self):
        for service, failing in (("postgres", "pg_dump"), ("valkey", "queue")):
            with self.subTest(service=service):
                self.doCleanups()
                self.setUp()
                self.add_entries(service)
                self.change_mode(service)
                self.failing = failing
                status = self.agent.reconcile()
                self.assertEqual(status["state"], "failed", status)
                self.assertNotIn("did not start again", status["recovery"])
                self.assertIn("previous-release-restarted", status["steps"])
                self.on_local_data()

    def test_after_a_failed_move_hot_can_restart_reboot_and_roll_back(self):
        self.add_entries("postgres")
        self.change_mode("postgres")
        self.failing = "check-postgres"
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"])
        self.on_local_data()
        self.failing = None
        self.assertEqual(self.agent.answer("restart-service", service="worker")["state"], "restarted")
        self.on_local_data()
        self.rebooted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.on_local_data()
        self.assertEqual(self.agent.answer("backup", request_id="b1")["state"], "backed-up")
        self.rebooted()
        report = self.agent.answer("roll-back", request_id="r1")
        self.assertEqual(report["state"], "rolled-back", report)
        self.on_local_data()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.agent.state()["data_modes"]["postgres"], "existing")

    def test_a_reboot_while_a_mode_change_waits_for_its_move_leaves_it_retryable(self):
        self.add_entries("postgres")
        self.change_mode("postgres")
        self.failing = "pg_dump"
        self.agent.reconcile()
        self.failing = None
        self.rebooted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)

    def faults(self, services, release):
        """One trial per command of the move and per kind of fault: it fails
        once, or the host goes down there. Yields the fault and whether it hit."""
        for kind in ("fails", "reboots"):
            for at in range(500):
                self.doCleanups()
                self.setUp()
                self.add_entries(*services)
                self.change_mode(*services)
                self.release(release)
                run, count, hit = self.agent.run_command, [0], [False]

                def faulty(command, run=run, count=count, hit=hit, at=at, kind=kind, **kwargs):
                    count[0] += 1
                    if count[0] - 1 == at:
                        hit[0] = True
                        if kind == "reboots":
                            raise KeyboardInterrupt
                        raise subprocess.CalledProcessError(1, command, stderr="it failed")
                    return run(command, **kwargs)
                self.agent.run_command = faulty
                yield kind, at, hit
                if not hit[0]:
                    break
            else:
                self.fail("the move never finished")

    def check_faults(self, services, release="3.0.1"):
        trials = 0
        for kind, at, hit in self.faults(services, release):
            with self.subTest(services=services, kind=kind, at=at):
                try:
                    status = self.agent.reconcile()
                except KeyboardInterrupt:
                    status = None
                if not hit[0]:
                    self.assertEqual(status["state"], "deployed", status)
                    continue
                trials += 1
                if status is None:
                    # Hot comes back as far as it may, and the deploy resumes.
                    self.rebooted()
                    report = self.agent.resume()
                    if report is not None:
                        self.assertEqual(report["state"], "resumed", report)
                    status = self.agent.reconcile()
                    self.assertEqual((status["state"], self.agent.state()["data_modes"]["postgres"]),
                                     ("deployed", "existing" if "postgres" in services else "local"), status)
                    self.assertEqual(self.agent.state()["release"], release)
                    continue
                if status["state"] == "deployed":  # a failure the deploy gets past
                    continue
                self.assertEqual(status["state"], "failed", status)
                self.assertNotIn("did not start again", status["recovery"])
                moved = self.agent.state()["data_modes"]["postgres"] == "existing" and "postgres" in services
                if not moved:
                    self.on_local_data()
                    self.assertEqual(self.agent.answer("restart-service", service="worker")["state"], "restarted")
                self.rebooted()
                report = self.agent.resume()
                if report is not None:
                    self.assertEqual(report["state"], "resumed", report)
                report = self.agent.command("deploy-now", request_id="retry")
                self.assertEqual((report["state"], self.agent.state().get("release")), ("deployed", release), report)
        self.assertGreater(trials, 20)

    def test_no_single_fault_in_a_database_move_leaves_it_stuck(self):
        self.check_faults(("postgres",))

    def test_no_single_fault_in_a_queue_move_leaves_it_stuck(self):
        self.check_faults(("valkey",))

    def test_no_single_fault_in_a_move_of_both_leaves_it_stuck(self):
        self.check_faults(("postgres", "valkey"))

    def test_no_single_fault_in_a_move_with_an_upgrade_leaves_it_stuck(self):
        self.check_faults(("postgres", "valkey"), release="3.0.2")


class InstalledDataModes(UpgradeHarness):
    """After a failed move to a managed database, Hot runs on the local one, so
    that is the one to back up, read logs from, and watch."""

    ADMIN = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef"
    HOST = "hot-example.c0123.us-east-1.rds.amazonaws.com"

    def setUp(self):
        super().setUp()
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "p@ss/word"}
        self.agent.resolve = lambda hosts: []
        self.config["data"] = {"postgres": {"mode": "managed", "runtime_role": "hot_runtime", "endpoint": {
            "host": self.HOST, "port": 5432, "admin_user": "hot_admin", "admin_secret": self.ADMIN}}}
        self.failing = "check-postgres"
        status = self.agent.reconcile()
        self.assertIn("previous-release-restarted", status["steps"])
        self.failing = None
        self.run.commands.clear()

    def fake_http(self, method, url, body=None, headers=None, timeout=30):
        if url.startswith("https://truststore.pki.rds.amazonaws.com/"):
            return 200, b"-----BEGIN CERTIFICATE-----\nRDS\n-----END CERTIFICATE-----\n"
        return super().fake_http(method, url, body, headers, timeout)

    def ps(self, **overrides):
        services = {name: ("running", "") for name in (*agent_module.HOT_SERVICES, "ingress")}
        services.update(postgres=("running", "healthy"), valkey=("running", "healthy"))
        services.update(overrides)
        self.run.ps = "\n".join(json.dumps({"Service": name, "State": state, "Health": health})
                                 for name, (state, health) in services.items())

    def test_backup_dumps_the_database_hot_runs_on(self):
        report = self.agent.answer("backup", request_id="b1")
        self.assertEqual(report["state"], "backed-up", report)
        self.assertTrue(Path(report["backup"]).exists())

    def test_its_log_can_be_read(self):
        self.run.logs = "postgres-1  | ready\n"
        self.assertEqual(self.agent.command("logs", service="postgres")["logs"], ["postgres-1  | ready"])

    def test_monitoring_watches_it_and_not_the_target(self):
        probed = []
        with mock.patch.object(agent_module, "tcp_reachable", lambda host, port: probed.append(host) or True):
            self.ps()
            metrics = self.agent.observe(probe=lambda host, path: 200)
            self.assertEqual((metrics["UnhealthyServices"][0], metrics["DependenciesReachable"][0]), (0, 1))
            self.ps(postgres=("exited", ""))
            metrics = self.agent.observe(probe=lambda host, path: 200)
            self.assertEqual((metrics["UnhealthyServices"][0], metrics["DependenciesReachable"][0]), (1, 0))
        self.assertEqual(probed, [], "the release does not use the managed endpoint")

    def test_diagnostics_say_where_it_is_and_what_is_asked_for(self):
        dependencies = self.agent.command("diagnostics")["diagnostics"]["dependencies"]
        self.assertEqual(dependencies, [{"service": "postgres", "mode": "local", "configured_mode": "managed"},
                                        {"service": "valkey", "mode": "local"}])

    def test_once_the_data_moved_the_new_place_counts(self):
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        with self.assertRaisesRegex(agent_module.AgentError, "back it up with its provider"):
            self.agent.command("backup")
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("logs", service="postgres")
        probed = []
        self.agent._dependencies = None
        with mock.patch.object(agent_module, "tcp_reachable", lambda host, port: probed.append(host) or True):
            self.agent.dependencies_reachable()
        self.assertEqual(probed, [self.HOST])


class ImagesBeforeSecrets(UpgradeHarness):
    """A service reads its files again at its next restart, even one Docker does,
    so a release refused for its images must leave them as they were."""
    PAIR = {"HOT_GOOGLE_CLIENT_ID": "client-id", "HOT_GOOGLE_CLIENT_SECRET": "client-secret"}

    def app_secret(self, name):
        return Path(self.tmp.name) / "secrets/app" / name

    def deployed_with_a_pair(self):
        self.cloud.secrets[OPERATOR].update(self.PAIR)
        self.release("3.0.2", latest=23)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        for name in self.PAIR:
            self.cloud.secrets[OPERATOR].pop(name)
        self.release("3.0.3", latest=24)
        self.run.commands.clear()

    def bundle_mounts(self, release):
        compose = json.loads((Path(self.tmp.name) / f"releases/{release}/bundle/compose.json").read_text())
        return {secret["target"] for secret in compose["services"]["app"]["secrets"]}

    def assert_refused_with_the_files_as_they_were(self, last_step):
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertEqual(status["steps"][-1], last_step)
        self.assertNotIn("secrets", status["steps"])
        self.assertIn("3.0.2 keeps running", status["recovery"])
        self.assertEqual(self.run.compose_calls(), ["--profile admin pull --quiet"], "nothing stops")
        self.assertTrue(set(self.PAIR) <= self.bundle_mounts("3.0.2"), "the installed release still mounts them")
        for name, value in self.PAIR.items():
            self.assertEqual(self.app_secret(name).read_text(), value, "its next restart still finds them")

    def test_a_pull_that_fails_leaves_the_delivered_files(self):
        self.deployed_with_a_pair()
        self.failing = "pull"
        self.assert_refused_with_the_files_as_they_were("rendered")

    def test_an_image_without_a_hotbox_leaves_the_delivered_files(self):
        self.deployed_with_a_pair()
        self.run.no_hotbox = True
        self.assert_refused_with_the_files_as_they_were("pulled")

    def test_a_rotated_value_waits_for_images_that_pull(self):
        self.deployed_with_a_pair()
        self.cloud.secrets[OPERATOR].update(self.PAIR, HOT_GOOGLE_CLIENT_SECRET="not-for-the-running-release")
        self.failing = "pull"
        self.agent.reconcile()
        self.assertEqual(self.app_secret("HOT_GOOGLE_CLIENT_SECRET").read_text(), "client-secret")

    def test_a_release_that_passes_delivers_after_its_images(self):
        self.deployed_with_a_pair()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        self.assertLess(status["steps"].index("pulled"), status["steps"].index("secrets"))
        self.assertLess(status["steps"].index("secrets"), status["steps"].index("scheduler-stopping"))
        for name in self.PAIR:
            self.assertFalse(self.app_secret(name).exists())


class StorageMoves(UpgradeHarness):
    S3 = {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"}

    def calls(self):
        return [" ".join(c) for c in self.run.commands]

    def worker_env(self):
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        return compose["services"]["worker"]["environment"]

    def test_a_configuration_change_redeploys_the_same_release(self):
        self.assertIsNone(self.agent.reconcile())
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.1"))
        self.assertEqual(self.worker_env()["HOT_LOG_LEVEL_FILTER"], "warn")
        self.assertIsNone(self.agent.reconcile())

    def test_files_move_to_a_bucket_before_the_new_storage_starts(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        move = next(i for i, c in enumerate(calls) if "move-storage" in c)
        self.assertIn("run --rm --no-deps worker db move-storage --role file --to s3 --bucket acme --prefix files "
                      "--manifest - --region us-east-1", calls[move])
        self.assertLess(next(i for i, c in enumerate(calls) if c.endswith("stop --timeout 120 worker task-worker")), move)
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3", "the new bundle uses the bucket")
        record = self.agent.history()[-1]
        self.assertEqual(Path(record["storage_manifests"]["file"]).read_text(), '{"kind":"file","status":"moved"}\n')

    def test_storage_moves_with_the_running_release_only(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.release("3.0.2")
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("release already running", status["error"])
        self.assertFalse(any(" stop " in f" {c} " for c in self.calls()), "nothing stops")

    def test_moving_back_onto_the_host_is_refused(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.agent.reconcile()
        self.config["storage"] = dict(self.config["storage"], file={"backend": "local"})
        error = self.agent.reconcile()["error"]
        self.assertIn("back onto the host is not automated", error)
        self.assertIn("Put storage.file back", error)
        self.assertNotIn("copy the objects", error, "a copy alone would not work")
        self.assertNotIn("yourself", error, "these hosts have no shell to do it from")

    def test_moving_to_another_bucket_is_refused_without_suggesting_a_copy_works(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.agent.reconcile()
        self.config["storage"] = dict(self.config["storage"], file=dict(self.S3, bucket="other"))
        error = self.agent.reconcile()["error"]
        self.assertIn("another s3 bucket or prefix is not automated", error)
        self.assertIn("copying the objects is not enough", error)
        self.assertNotIn("Copy the objects", error)

    def test_a_failed_move_keeps_hot_stopped(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "move-storage"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("Hot stays stopped", status["recovery"])
        after = self.calls()[next(i for i, c in enumerate(self.calls()) if "move-storage" in c):]
        self.assertFalse(any(" up " in f" {c} " for c in after), "nothing restarts")
        # A reboot does not start Hot on partly moved records either.
        self.assertTrue(self.agent.state()["stopped"])
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        with self.assertRaisesRegex(agent_module.AgentError, "deploy-now instead"):
            self.agent.command("roll-back")

    def test_a_restart_does_not_get_around_a_partly_failed_move(self):
        installed = self.config["storage"]
        self.config["storage"] = dict(installed, file=self.S3)
        self.failing = "move-storage"
        self.agent.reconcile()
        self.failing = None
        self.run.commands.clear()
        for service in ("app", "worker", "ingress", "scheduler"):
            with self.subTest(service=service):
                report = self.agent.answer("restart-service", service, request_id=f"x-{service}")
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("Fix the cause and run deploy-now, or put the storage setting back, apply, and run deploy-now",
                              report["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops or starts")
        self.assertTrue(self.agent.state()["stopped"])
        # Putting the setting back ends that attempt, and restarts work again.
        self.config["storage"] = installed
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "resumed")
        self.assertEqual(self.agent.answer("restart-service", "app", request_id="x-after")["state"], "restarted")

    def test_a_restart_does_not_get_around_a_move_cut_short_either(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "move-storage" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        self.run.commands.clear()
        report = self.agent.answer("restart-service", "app", request_id="x1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertEqual(self.run.compose_calls(), [])

    def test_a_drain_that_failed_partway_still_stops_what_it_left_running(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        run, failed = self.agent.run_command, []

        def failing_once(command, **kwargs):
            if "ingress" in command and "stop" in command and not failed:
                failed.append(command)
                raise subprocess.CalledProcessError(1, command, stderr="it failed")
            return run(command, **kwargs)
        self.agent.run_command = failing_once
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertIn("Hot stays stopped", status["recovery"])
        self.assertEqual(self.run.compose_calls()[-1], "stop app api ingress")

    def test_a_restart_after_every_object_moved_uses_the_new_bundle(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "pull"
        self.agent.reconcile()
        self.failing = None
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="x1")["state"], "restarted")

    def test_a_move_waits_for_a_failed_upgrade_to_be_settled(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        point = self.agent.state()["failed"]["restore_point"]
        self.release("3.0.1")
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("storage.file cannot change yet", status["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing drains or moves")
        self.assertEqual(self.agent.state()["failed"]["restore_point"], point)
        self.assertNotIn("storage_progress", self.agent.state()["failed"])
        self.assertIn("restored", self.agent.command("roll-back")["steps"])

    def failing_move(self, listed, **fields):
        """A move that writes `listed` to its output, then exits 1, unless the
        command lists what is in `skip`, as a worker that leaves those behind."""
        recorder, skip = self.run, fields.get("skip", "-")

        def run(command, **kwargs):
            if "move-storage" in command and f"HOT_MOVE_STORAGE_SKIP={skip}" not in command:
                recorder.commands.append(command)
                kwargs["stdout"].write(listed)
                raise subprocess.CalledProcessError(1, command, stderr=fields.get(
                    "stderr", "1 moved, 1 failed\nError: 1 object(s) did not move"))
            return recorder(command, **kwargs)
        self.agent.run_command = run

    def test_a_partly_failed_move_keeps_its_manifest(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        stuck = "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a"
        listed = ('{"kind":"file","id":"0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5b","status":"moved"}\n'
                  '{"kind":"file","id":"' + stuck + '","status":"failed","error":"object is missing from the source"}\n')
        self.failing_move(listed)
        status = self.agent.reconcile()
        manifest = Path(self.agent.state()["failed"]["storage_manifests"]["file"])
        self.assertEqual(manifest.read_text(), listed)
        self.assertIn(str(manifest), status["recovery"])
        self.assertIn("1 moved, 1 failed", status["error"])
        self.assertEqual(status["failed"]["storage_failed"],
                         {"role": "file", "count": 1, "ids": [stuck], "manifest": str(manifest)})
        self.assertIn("1 did not move", status["recovery"])

    def test_a_report_names_only_so_many_objects_that_did_not_move(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        ids = [f"0190a6e4-7c11-7b3e-9a52-{n:012x}" for n in range(agent_module.STORAGE_FAILED_LISTED + 5)]
        listed = "".join(json.dumps({"kind": "file", "id": entry, "status": "failed", "error": "bytes are gone"}) + "\n"
                         for entry in ids)
        self.failing_move(listed + '{"kind":"file","id":"0190a6', stderr="x" * 5000)  # the last line is cut short
        status = self.agent.reconcile()
        self.assertEqual({key: status["failed"]["storage_failed"][key] for key in ("role", "count", "ids")},
                         {"role": "file", "count": len(ids), "ids": ids[:agent_module.STORAGE_FAILED_LISTED]})
        # The AWS status parameter holds 4 KB.
        self.assertLess(len(json.dumps({k: v for k, v in status.items() if k not in agent_module.DETAIL_KEYS})), 4096)

    def test_the_manifest_is_written_as_the_move_runs(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        recorder, line = self.run, '{"kind":"file","status":"moved"}\n'

        def run(command, **kwargs):
            if "move-storage" in command:
                self.assertNotIn("capture_output", kwargs)
                kwargs["stdout"].write(line)
                kwargs["stdout"].flush()
                raise KeyboardInterrupt  # a reboot, or the agent killed
            return recorder(command, **kwargs)
        self.agent.run_command = run
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        manifest = self.agent.state()["deploying"]["storage_manifests"]["file"]
        self.assertEqual(Path(manifest).read_text(), line, "records were rewritten: this is their only record")

    def test_a_command_given_a_file_writes_there_and_keeps_its_errors(self):
        script = "echo moved; echo 'one failed' >&2; exit {}"
        with tempfile.TemporaryFile("w+") as output:
            done = agent_module.Agent._run(["sh", "-c", script.format(0)], stdout=output)
            self.assertEqual((done.stdout, done.stderr), (None, "one failed\n"))
            with self.assertRaises(subprocess.CalledProcessError) as caught:
                agent_module.Agent._run(["sh", "-c", script.format(1)], stdout=output)
            self.assertEqual(caught.exception.stderr, "one failed\n")
            output.seek(0)
            self.assertEqual(output.read(), "moved\nmoved\n")
        done = agent_module.Agent._run(["sh", "-c", script.format(0)])
        self.assertEqual((done.stdout, done.stderr), ("moved\n", "one failed\n"))

    def test_diagnostics_names_the_objects_a_report_has_no_room_for(self):
        # The operator has no shell to read the manifest with.
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        ids = [f"0190a6e4-7c11-7b3e-9a52-{n:012x}" for n in range(agent_module.STORAGE_FAILED_DIAGNOSED + 5)]
        self.failing_move("".join(json.dumps({"kind": "file", "id": entry, "status": "failed"}) + "\n" for entry in ids))
        status = self.agent.reconcile()
        self.assertIn(f"and diagnostics up to {agent_module.STORAGE_FAILED_DIAGNOSED}", status["recovery"])
        self.assertEqual(len(status["failed"]["storage_failed"]["ids"]), agent_module.STORAGE_FAILED_LISTED)
        named = self.agent.command("diagnostics", request_id="d1")["diagnostics"]["storage_failed"]
        self.assertEqual(named, {"role": "file", "count": len(ids), "ids": ids[:agent_module.STORAGE_FAILED_DIAGNOSED]})
        # A manifest that went missing costs the bundle nothing else.
        Path(self.agent.state()["failed"]["storage_manifests"]["file"]).unlink()
        bundle = self.agent.command("diagnostics", request_id="d2")["diagnostics"]
        self.assertIn("storage_failed_error", bundle)
        self.assertIn("configuration", bundle)

    def test_the_recovery_says_how_to_leave_an_object_behind(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "move-storage"
        recovery = self.agent.reconcile()["recovery"]
        self.assertIn("add its id to settings.HOT_MOVE_STORAGE_SKIP", recovery)
        self.assertIn("then run deploy-now", recovery)
        self.assertNotIn("-skip", recovery, "these hosts have no shell to write a file with")

    def test_objects_that_can_never_move_can_be_left_behind(self):
        first, second = "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a", "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5b"
        self.config["settings"] = {"HOT_MOVE_STORAGE_SKIP": f"{first}, {second}"}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        move = next(c for c in self.run.commands if "move-storage" in c)
        flags = move[:move.index("worker")]
        self.assertEqual(flags[flags.index("-e") + 1], f"HOT_MOVE_STORAGE_SKIP={first},{second}")

    def test_the_way_out_of_an_object_that_can_never_move(self):
        # What an operator can do with no shell: read the report, set the setting.
        stuck = "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a"
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing_move(json.dumps({"kind": "file", "id": stuck, "status": "failed", "error": "bytes are gone"}) + "\n",
                          skip=stuck)
        status = self.agent.reconcile()
        self.assertEqual(status["failed"]["storage_failed"]["ids"], [stuck])
        self.config["settings"] = {"HOT_MOVE_STORAGE_SKIP": stuck}
        self.assertIsNone(self.agent.reconcile(), "the failed deploy still waits for deploy-now")
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.assertNotIn("failed", self.agent.state())

    def test_a_list_that_is_not_ids_is_refused_before_anything_stops(self):
        self.config["settings"] = {"HOT_MOVE_STORAGE_SKIP": "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a,not-an-id"}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("'not-an-id' is not one", status["error"])
        self.assertIn("keeps running", status["recovery"])
        self.assertFalse(any(" stop " in f" {c} " for c in self.calls()), "nothing stops")
        self.assertFalse(any("move-storage" in c for c in self.calls()))

    def test_a_list_taken_out_of_settings_no_longer_applies(self):
        # The installed bundle's worker has the list its render saw.
        self.config["settings"] = {"HOT_MOVE_STORAGE_SKIP": "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a"}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertIn("HOT_MOVE_STORAGE_SKIP", self.worker_env())
        self.config["settings"] = {}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        move = next(c for c in self.run.commands if "move-storage" in c)
        self.assertIn("HOT_MOVE_STORAGE_SKIP=", move)

    def test_a_move_with_nothing_listed_leaves_nothing_behind(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        move = next(c for c in self.run.commands if "move-storage" in c)
        self.assertFalse(any("HOT_MOVE_STORAGE_SKIP" in part for part in move))

    def test_after_the_objects_move_a_later_failure_keeps_hot_stopped(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "pull"
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed")
        self.assertIn("storage-moved", status["steps"])
        self.assertIn("records point there, so Hot stays stopped", status["recovery"])
        self.assertNotIn("keeps running", status["recovery"])
        self.assertFalse(any(" up " in f" {c} " for c in self.calls()), "drained, and nothing restarts")
        self.assertTrue(self.agent.state()["stopped"])
        # deploy-now finishes the deploy on the bucket without moving again.
        self.failing = None
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertFalse(any("move-storage" in c for c in self.calls()))
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_data_move_waits_for_the_deploy_of_a_finished_storage_move(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "bootstrap-admin"
        status = self.agent.reconcile()
        self.assertIn("records point there, so Hot stays stopped", status["recovery"])
        self.failing = None
        bundle = (Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text()
        secrets = Path(self.tmp.name) / "secrets"
        before = {path: path.read_text() for path in secrets.rglob("*") if path.is_file()}
        installed = self.config.get("data")
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = "postgres://hot_runtime:pw@db.internal.example/hot"
        self.agent.resolve = lambda hosts: []
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["started"]), report)
        self.assertIn("Move storage and data services in separate deploys", report["error"])
        self.assertIn("Put data.postgres back, apply, and run deploy-now", report["error"])
        # Only recovery's stop of what is already stopped: no job, and nothing starts.
        self.assertEqual([c for c in self.run.compose_calls() if not c.startswith("stop ")], [])
        self.assertEqual((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text(), bundle)
        self.assertEqual({path: path.read_text() for path in secrets.rglob("*") if path.is_file()}, before)
        state = self.agent.state()
        self.assertEqual((state["data_modes"]["postgres"], state["failed"]["storage_progress"]), ("local", "moved"))
        self.assertTrue(state["stopped"])
        # The way the refusal gives finishes that deploy, and the data can then move.
        if installed is None:
            del self.config["data"]
        else:
            self.config["data"] = installed
        del self.cloud.secrets[OPERATOR]["HOT_DB_URI"]
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_data_move_waits_for_a_storage_move_that_has_not_finished(self):
        # Its storage still set, so both would move: the same rule, sooner.
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "move-storage"
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.failing = None
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.cloud.secrets[OPERATOR]["HOT_DB_URI"] = "postgres://hot_runtime:pw@db.internal.example/hot"
        self.agent.resolve = lambda hosts: []
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["started"]), report)
        self.assertIn("a storage move has not finished, and a data move must wait for it", report["error"])
        self.assertEqual([c for c in self.run.compose_calls() if not c.startswith("stop ")], [])

    # -- leaving behind what a report counted (HOT_MOVE_STORAGE_SKIP_FAILED) -----

    SKIP = agent_module.MOVE_SKIP_FAILED_SETTING
    STUCK = [f"0190a6e4-7c11-7b3e-9a52-{n:012x}" for n in range(3)]

    def moving_both(self):
        self.config["storage"] = {"build": dict(self.S3, prefix="builds"), "file": self.S3}

    def worker_with_stuck_objects(self, stuck, noise=""):
        """A worker whose move lists, for each role in `stuck`, those objects as
        failed, or as skipped when its skip file or HOT_MOVE_STORAGE_SKIP names
        them, and exits 1 if any failed. `self.jobs` keeps what each job's file
        said, as it ran."""
        recorder, self.jobs = self.run, []

        def run(command, **kwargs):
            if "move-storage" not in command:
                return recorder(command, **kwargs)
            recorder.commands.append(command)
            role, skipped = command[command.index("--role") + 1], set()
            if "--skip-file" in command:
                inside = command[command.index("--skip-file") + 1]
                self.assertEqual(inside, agent_module.MOVE_SKIP_FILE_IN_JOB)
                self.assertGreater(command.index("--skip-file"), command.index("move-storage"), "Hot's flag, not Compose's")
                mounts = [part for part in command[:command.index("worker")] if part.endswith(f":{inside}:ro")]
                self.assertEqual(len(mounts), 1, "mounted read-only on the job")
                host = mounts[0].split(":")[0]
                self.assertEqual(stat.S_IMODE(os.stat(host).st_mode), 0o600, "the job's user alone reads it")
                skipped = set(Path(host).read_text().splitlines())
            self.jobs.append((role, skipped))
            listed = next((set(filter(None, part.split("=", 1)[1].split(","))) for part in command
                           if part.startswith(f"{agent_module.MOVE_SKIP_SETTING}=")), set())
            objects = stuck.get(role, [])
            # Attempts in one second would share a manifest, as they cannot on a host.
            kwargs["stdout"].truncate(0)
            for entry in objects:
                status = "skipped" if entry in skipped | listed else "failed"
                kwargs["stdout"].write(json.dumps({"kind": "file", "id": entry, "status": status}) + "\n")
            if set(objects) - skipped - listed:
                # Hot's own words, which name the flag.
                raise subprocess.CalledProcessError(1, command, stderr=(
                    f"{noise}Database command failed: {len(set(objects) - skipped - listed)} object(s) did not move, and "
                    "their records did not change. To leave behind an object that can never move, list its id in "
                    "HOT_MOVE_STORAGE_SKIP, or on its own line in a file named by --skip-file"))
            return subprocess.CompletedProcess(command, 0, stderr="")
        self.agent.run_command = run

    def skip_files(self):
        return sorted((Path(self.tmp.name) / "backups").glob("storage-skip-*"))

    def left_behind_files(self):
        return sorted((Path(self.tmp.name) / "backups").glob("storage-left-behind-*"))

    def move_that_failed(self, stuck):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects({"file": stuck})
        status = self.agent.reconcile()
        self.assertEqual(status["failed"]["storage_failed"]["count"], len(stuck), status)
        return status

    def test_what_a_report_counted_can_be_left_behind(self):
        recovery = self.move_that_failed(self.STUCK)["recovery"]
        self.assertIn(f"To leave all 3 behind, set settings.{self.SKIP} to 3 instead", recovery)
        self.config["settings"] = {self.SKIP: "3"}
        self.assertIsNone(self.agent.reconcile(), "the setting alone does not retry")
        self.assertEqual(self.jobs, [("file", set())], "only the move that failed has run")
        self.jobs.clear()
        self.run.commands.clear()
        owners = []
        self.agent.chown = lambda path, uid, gid: owners.append((Path(path).name, uid, gid))
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK))])
        self.assertIn(("storage-skip-file.txt", agent_module.JOB_USER, agent_module.JOB_USER), owners)
        move = next(c for c in self.run.commands if "move-storage" in c)
        flags = move[:move.index("worker")]
        listed = Path(self.tmp.name) / "backups/storage-skip-file.txt"
        self.assertIn(f"{listed}:{agent_module.MOVE_SKIP_FILE_IN_JOB}:ro", flags)
        self.assertEqual(move[move.index("--skip-file") + 1], agent_module.MOVE_SKIP_FILE_IN_JOB)
        # Beside HOT_MOVE_STORAGE_SKIP, Hot's settings would warn about this
        # variable, and alone would load the whole list into its configuration.
        self.assertNotIn("HOT_MOVE_STORAGE_SKIP_FILE", " ".join(move))
        self.assertEqual(self.skip_files(), [], "cleaned up")
        self.assertNotIn("failed", self.agent.state())
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")
        self.assertNotIn("HOT_MOVE_STORAGE_SKIP=", " ".join(move))

    def test_a_count_that_is_not_the_reports_is_refused(self):
        self.move_that_failed(self.STUCK)
        for wrong in ("2", "4", "30"):
            with self.subTest(wrong=wrong):
                self.config["settings"] = {self.SKIP: wrong}
                self.jobs.clear()
                report = self.agent.command("deploy-now", request_id=f"d{wrong}")
                self.assertEqual(report["state"], "failed", report)
                self.assertIn(f"{self.SKIP} is {wrong}, and the last report said 3 file objects did not move",
                              report["error"])
                self.assertIn("nothing was left behind", report["error"])
                self.assertIn("set it to 3", report["error"])
                self.assertEqual(self.jobs, [], "nothing moved")
                self.assertEqual(self.skip_files(), [])
                self.assertEqual(self.agent.state()["failed"]["storage_failed"]["count"], 3, "the report stands")
                self.assertTrue(self.agent.state()["stopped"])
        self.config["settings"] = {self.SKIP: "3"}
        self.assertEqual(self.agent.command("deploy-now", request_id="d3")["state"], "deployed")

    def test_a_count_too_long_for_a_whole_number_is_only_a_wrong_count(self):
        # The configuration has no upper limit, and int() refuses this many digits.
        self.move_that_failed(self.STUCK)
        wrong = "9" * 5000
        with self.assertRaises(ValueError):
            int(wrong)
        self.config["settings"] = {self.SKIP: wrong}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("and the last report said 3 file objects did not move", report["error"])
        self.assertLess(len(report["error"]), 500, "a report has only so much room")
        self.assertEqual(self.jobs, [])
        self.assertEqual(self.agent.state()["failed"]["storage_failed"]["count"], 3)

    def test_a_count_no_report_stands_behind_is_refused_before_anything_stops(self):
        self.config["settings"] = {self.SKIP: "3"}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects({"file": self.STUCK})
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("no storage move has reported objects that did not move", status["error"])
        self.assertIn("keeps running", status["recovery"])
        self.assertEqual(self.jobs, [])
        self.assertFalse(any(" stop " in f" {c} " for c in self.calls()), "nothing stops")

    def test_a_count_left_set_after_its_move_means_nothing_until_the_next_move(self):
        self.move_that_failed(self.STUCK)
        self.config["settings"] = {self.SKIP: "3"}
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        # An ordinary deploy with it set goes on.
        self.config["settings"] = {self.SKIP: "3", "HOT_LOG_LEVEL_FILTER": "warn"}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        # The next move's objects have not been reported, so it abandons none of them.
        self.config["storage"] = dict(self.config["storage"], build=dict(self.S3, prefix="builds"))
        self.jobs.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("no storage move has reported", status["error"])
        self.assertEqual(self.jobs, [])

    def test_a_count_that_is_not_a_whole_number_is_refused(self):
        self.move_that_failed(self.STUCK)
        for wrong in ("0", "-3", "3.0", "03", "three", "", " 3", "٣"):
            with self.subTest(wrong=wrong):
                self.config["settings"] = {self.SKIP: wrong}
                self.jobs.clear()
                report = self.agent.command("deploy-now", request_id=f"d{wrong}")
                self.assertEqual(report["state"], "failed", report)
                self.assertIn(f"{self.SKIP} must be a whole number of 1 or more", report["error"])
                self.assertEqual(self.jobs, [])

    def test_a_count_whose_manifest_is_gone_is_refused(self):
        self.move_that_failed(self.STUCK)
        Path(self.agent.state()["failed"]["storage_manifests"]["file"]).unlink()
        self.config["settings"] = {self.SKIP: "3"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("could not be listed from that move's manifest", report["error"])
        self.assertIn(f"settings.{agent_module.MOVE_SKIP_SETTING}", report["error"], "by id still works")
        self.assertEqual(self.jobs, [])
        self.assertEqual(self.skip_files(), [])
        self.assertTrue(self.agent.state()["stopped"])

    def test_a_manifest_that_lists_other_than_the_report_said_is_refused(self):
        self.move_that_failed(self.STUCK)
        manifest = Path(self.agent.state()["failed"]["storage_manifests"]["file"])
        manifest.write_text(json.dumps({"kind": "file", "id": self.STUCK[0], "status": "failed"}) + "\n")
        self.config["settings"] = {self.SKIP: "3"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn(f"as the last report said, and that move's manifest {manifest} lists 1", report["error"])
        self.assertEqual(self.jobs, [])
        self.assertEqual(self.skip_files(), [])

    def test_a_manifest_that_names_no_id_is_refused(self):
        self.move_that_failed(self.STUCK[:1])
        manifest = Path(self.agent.state()["failed"]["storage_manifests"]["file"])
        manifest.write_text(json.dumps({"kind": "file", "id": "not-an-id", "status": "failed"}) + "\n")
        self.config["settings"] = {self.SKIP: "1"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("'not-an-id', which is not an object id", report["error"])
        self.assertEqual(self.jobs, [])
        self.assertEqual(self.skip_files(), [])

    def test_more_objects_than_the_id_list_could_hold_can_be_left_behind(self):
        # settings.HOT_MOVE_STORAGE_SKIP has about 8 KB on AWS, and one argument 128 KB.
        stuck = [f"0190a6e4-7c11-7b3e-9a52-{n:012x}" for n in range(5000)]
        self.assertGreater(len(",".join(stuck)), 128 * 1024)
        self.move_that_failed(stuck)
        self.config["settings"] = {self.SKIP: "5000"}
        self.jobs.clear()
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(stuck))])
        move = next(c for c in self.run.commands if "move-storage" in c)
        self.assertLess(max(len(part) for part in move), 1024)
        self.assertEqual(self.skip_files(), [])

    def test_each_role_keeps_what_it_left_behind(self):
        # A move stops at the first role that fails, so reports come a role at a
        # time. The setting then names only the last, and the first role must not
        # fail again on what it left behind, or the two would take turns for ever.
        build, files = self.STUCK[:1], self.STUCK[1:]
        self.moving_both()
        self.worker_with_stuck_objects({"build": build, "file": files})
        status = self.agent.reconcile()
        self.assertEqual((status["failed"]["storage_failed"]["role"], status["failed"]["storage_failed"]["count"]), ("build", 1))
        self.config["settings"] = {self.SKIP: "1"}
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["failed"]["storage_failed"]["role"], report["failed"]["storage_failed"]["count"]), ("file", 2))
        self.assertEqual(report["failed"]["storage_left_behind"], {"build": 1})
        self.assertIn("instead. 3 then stay behind in all", report["recovery"])
        self.config["settings"] = {self.SKIP: "2"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("build", set(build)), ("file", set(files))])
        self.assertEqual(self.skip_files(), [])
        self.assertEqual(self.left_behind_files(), [], "cleaned up")
        self.assertIn("3 object(s) stayed behind", report["warning"])
        for manifest in self.agent.history()[-1]["storage_manifests"].values():
            self.assertIn(manifest, report["warning"])

    def test_a_role_not_reached_keeps_what_its_count_accepted(self):
        # The count named the second role, and the first role's new failure stopped
        # the move before the second ran: that count is not asked for twice.
        build, files = self.STUCK[:1], self.STUCK[1:]
        stuck = {"file": files}
        self.moving_both()
        self.worker_with_stuck_objects(stuck)
        self.assertEqual(self.agent.reconcile()["failed"]["storage_failed"]["role"], "file")
        stuck["build"] = build
        self.config["settings"] = {self.SKIP: "2"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["failed"]["storage_failed"]["ids"], build)
        self.assertEqual([role for role, _ in self.jobs], ["build"], "the file role did not run")
        self.config["settings"] = {self.SKIP: "1"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("build", set(build)), ("file", set(files))])

    def test_new_failures_are_listed_beside_what_was_left_behind(self):
        stuck = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(stuck)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        stuck["file"] = self.STUCK[:2]  # another fails as the retry runs
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["failed"]["storage_failed"]["ids"], [self.STUCK[1]])
        self.assertIn("did not move", report["error"], "Hot's words name the flag, and the flag was not refused")
        self.assertIn(f"To leave all 1 behind, set settings.{self.SKIP}", report["recovery"])
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK[:2]))])

    def test_an_id_taken_off_the_list_moves_though_a_count_is_set(self):
        # A manifest's `skipped` also has what the list left behind, and a count
        # that went by that would leave this id behind with nothing said.
        repaired, stuck = self.STUCK[:2]
        objects = {"file": [repaired, stuck]}
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: repaired}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        status = self.agent.reconcile()
        self.assertEqual(status["failed"]["storage_failed"]["ids"], [stuck])
        self.assertNotIn("storage_left_behind", status["failed"], "the list goes by its own setting")
        self.assertNotIn("then stay behind in all", status["recovery"])
        manifest = Path(status["failed"]["storage_manifests"]["file"]).read_text()
        self.assertIn(json.dumps({"kind": "file", "id": repaired, "status": "skipped"}), manifest)
        # Its bytes are back, it comes off the list, and the other is given up on.
        objects["file"] = [stuck]
        self.config["settings"] = {self.SKIP: "1"}
        self.jobs.clear()
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", {stuck})])
        self.assertNotIn(repaired, " ".join(next(c for c in self.run.commands if "move-storage" in c)))
        self.assertIn("1 object(s) stayed behind", report["warning"])

    def test_an_id_kept_on_the_list_stays_behind_beside_the_count(self):
        listed, stuck = self.STUCK[:2]
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: listed}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects({"file": [listed, stuck]})
        self.agent.reconcile()
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: listed, self.SKIP: "1"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", {stuck})], "the file holds what the count covers")
        self.assertIn("2 object(s) stayed behind", report["warning"])
        self.assertIn(self.agent.history()[-1]["storage_manifests"]["file"], report["warning"])

    def test_a_finished_deploy_reports_what_the_list_left_behind(self):
        listed = self.STUCK[0]
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: listed}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects({"file": [listed]})
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertIn("1 object(s) stayed behind on the old storage", report["warning"])
        self.assertIn(self.agent.history()[-1]["storage_manifests"]["file"], report["warning"])
        # The next deploy has nothing to say about it.
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: listed, "HOT_LOG_LEVEL_FILTER": "warn"}
        self.assertNotIn("warning", self.agent.reconcile())

    def test_a_failure_after_the_move_keeps_what_it_left_behind_for_the_report(self):
        self.move_that_failed(self.STUCK)
        self.config["settings"] = {self.SKIP: "3"}
        worker = self.agent.run_command

        def admin_fails(command, **kwargs):
            if "bootstrap-admin" in command:
                raise subprocess.CalledProcessError(1, command)
            return worker(command, **kwargs)
        self.agent.run_command = admin_fails
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("records point there, so Hot stays stopped", status["recovery"])
        self.assertEqual(status["failed"]["storage_left_behind"], {"file": 3})
        self.agent.run_command = worker
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [], "it does not move again")
        self.assertIn("3 object(s) stayed behind", report["warning"])

    def test_a_deploy_with_nothing_left_behind_has_no_warning(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects({})
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertNotIn("warning", report)

    def test_each_round_says_how_many_stay_behind_in_all(self):
        objects = {"file": self.STUCK[:2]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.assertNotIn("then stay behind in all", self.agent.reconcile()["recovery"], "nothing was left behind yet")
        self.config["settings"] = {self.SKIP: "2"}
        objects["file"] = self.STUCK  # another fails as the retry runs
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["failed"]["storage_failed"]["ids"], self.STUCK[2:])
        self.assertEqual(report["failed"]["storage_left_behind"], {"file": 2})
        self.assertIn(f"To leave all 1 behind, set settings.{self.SKIP} to 1 instead. 3 then stay behind in all; taking "
                      "the setting out tries them all again.", report["recovery"])
        self.assertEqual(self.skip_files(), [])
        # A count refused says so too.
        self.config["settings"] = {self.SKIP: "3"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertIn(f"{self.SKIP} is 3, and the last report said 1 file objects did not move", report["error"])
        self.assertIn("set it to 1. 3 then stay behind in all; taking the setting out tries them all again. Otherwise",
                      report["error"])
        self.assertEqual(self.jobs, [])
        self.assertEqual(report["failed"]["storage_left_behind"], {"file": 2}, "a refusal keeps them")
        self.config["settings"] = {self.SKIP: "1"}
        report = self.agent.command("deploy-now", request_id="d3")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK))])
        self.assertIn("3 object(s) stayed behind", report["warning"])
        self.assertEqual(self.left_behind_files(), [])

    def test_a_retry_that_was_cut_short_keeps_what_a_round_left_behind(self):
        objects = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        objects["file"] = self.STUCK[:2]
        self.agent.command("deploy-now", request_id="d1")
        worker = self.agent.run_command

        def killed(command, **kwargs):
            if "move-storage" in command:
                raise KeyboardInterrupt
            return worker(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(self.skip_files(), [])
        self.assertEqual(len(self.left_behind_files()), 1, "what the round before left behind stays")
        self.agent.run_command = worker
        self.jobs.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.jobs, [("file", set(self.STUCK[:2]))])

    def finish_the_move_then_kill_the_pull(self):
        """Runs a deploy whose move finishes and whose pull is killed, and puts
        the worker back for whatever comes next."""
        worker = self.agent.run_command

        def killed(command, **kwargs):
            if "pull" in command:
                raise KeyboardInterrupt
            return worker(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d-killed")
        self.agent.run_command = worker
        self.jobs.clear()
        deploying = self.agent.state()["deploying"]
        self.assertIn("storage-moved", deploying["steps"])
        return deploying

    def a_round_left_behind_then_all_were_repaired(self):
        objects = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        objects["file"] = self.STUCK[:2]
        failed = self.agent.command("deploy-now", request_id="d1")["failed"]
        self.assertEqual(failed["storage_left_behind"], {"file": 1})
        self.assertEqual(failed["storage_failed"]["ids"], self.STUCK[1:2])
        self.config["settings"] = {}
        objects["file"] = []

    def test_a_move_that_finished_with_nothing_left_behind_stays_so_after_a_kill(self):
        # The failed record still counts what a round left behind. The attempt
        # that finished took that off its own record, and it does not come back.
        self.a_round_left_behind_then_all_were_repaired()
        deploying = self.finish_the_move_then_kill_the_pull()
        self.assertNotIn("storage_left_behind", deploying)
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [], "it does not move again")
        self.assertNotIn("warning", report)

    def finish_the_move_then_kill_its_install(self):
        """Runs a deploy whose move finishes and that dies before its bundle goes
        in, so the installed bundle still reads the old storage."""
        install_bundle = self.agent.install_bundle

        def killed(*args, **kwargs):
            self.agent.install_bundle = install_bundle
            raise KeyboardInterrupt
        self.agent.install_bundle = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d-killed")
        self.jobs.clear()
        deploying = self.agent.state()["deploying"]
        self.assertIn("storage-moved", deploying["steps"])
        self.assertNotIn("rendered", deploying["steps"])
        return deploying

    def test_a_kill_before_the_bundle_goes_in_moves_again_by_the_same_report(self):
        self.move_that_failed(self.STUCK)
        # Attempts in one second would share a manifest, as they cannot on a host.
        state = self.agent.state()
        report = state["failed"]["storage_failed"]
        reported = Path(report["manifest"]).with_name("storage-move-file-reported.jsonl")
        shutil.copy(report["manifest"], reported)
        report["manifest"] = str(reported)
        self.agent.save_state(state)
        self.config["settings"] = {self.SKIP: "3"}
        self.finish_the_move_then_kill_its_install()
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK))])
        self.assertIn("3 object(s) stayed behind", report["warning"])
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")

    def test_a_kill_before_the_bundle_goes_in_keeps_a_move_with_nothing_left_behind_so(self):
        # The failed record's count is gone by the end of the move that runs again.
        self.a_round_left_behind_then_all_were_repaired()
        self.assertNotIn("storage_left_behind", self.finish_the_move_then_kill_its_install())
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set())])
        self.assertNotIn("warning", report)

    def test_a_kill_before_the_bundle_goes_in_moves_both_roles_again(self):
        self.moving_both()
        self.worker_with_stuck_objects({"build": [], "file": []})
        worker = self.agent.run_command
        self.finish_the_move_then_kill_its_install()
        self.agent.run_command = worker
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual([role for role, _ in self.jobs], ["build", "file"])

    def build_moved_then_its_pull_failed(self, settings):
        """A build move finished, leaving one object behind, and the deploy it
        belongs to failed: the bundle is in place, and Hot stopped."""
        self.config["storage"] = dict(self.config["storage"], build=dict(self.S3, prefix="builds"))
        self.config["settings"] = settings
        self.worker_with_stuck_objects({"build": self.STUCK[:1]})
        worker = self.agent.run_command

        def pull_fails(command, **kwargs):
            if "pull" in command:
                raise subprocess.CalledProcessError(1, command)
            return worker(command, **kwargs)
        self.agent.run_command = pull_fails
        status = self.agent.reconcile()
        self.agent.run_command = worker
        self.assertEqual(agent_module.Agent.storage_progress(status["failed"]), "moved", status)
        self.assertEqual(status["failed"]["storage_left_behind"], {"build": 1})
        self.jobs.clear()
        self.run.commands.clear()

    def test_another_role_waits_for_the_deploy_of_a_finished_move(self):
        self.build_moved_then_its_pull_failed({agent_module.MOVE_SKIP_SETTING: self.STUCK[0]})
        installed = self.config["storage"]
        self.config["storage"] = dict(installed, file=self.S3)
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["steps"][-1]), ("failed", "verified"), report)
        self.assertIn("Put storage.file back, apply, and run deploy-now to finish that deploy", report["error"])
        self.assertEqual(self.jobs, [], "no job runs")
        self.assertEqual([c for c in self.run.compose_calls() if not c.startswith("stop ")],
                         ["up -d --wait --no-recreate postgres valkey"], "only what the database's level is read from")
        self.assertEqual(report["failed"]["storage_left_behind"], {"build": 1}, "its count stands")
        self.assertTrue(self.agent.state()["stopped"])
        # The way the refusal gives finishes that deploy, which says what stayed
        # behind, and the other role can then move.
        self.config["storage"] = installed
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [], "the build move does not run again")
        self.assertIn("1 object(s) stayed behind", report["warning"])
        self.config["storage"] = dict(installed, file=self.S3)
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set())])

    def test_another_role_waits_however_the_objects_were_left_behind(self):
        self.build_moved_then_its_pull_failed({agent_module.MOVE_SKIP_SETTING: self.STUCK[0]})
        self.config["settings"] = {self.SKIP: "1"}
        installed = self.config["storage"]
        self.config["storage"] = dict(installed, file=self.S3)
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("Put storage.file back", report["error"])
        self.assertEqual(report["failed"]["storage_left_behind"], {"build": 1})

    def test_a_failure_after_that_kill_reports_the_move_as_a_failure_would_have(self):
        self.a_round_left_behind_then_all_were_repaired()
        self.finish_the_move_then_kill_the_pull()
        worker = self.agent.run_command

        def pull_fails(command, **kwargs):
            if "pull" in command:
                raise subprocess.CalledProcessError(1, command)
            return worker(command, **kwargs)
        self.agent.run_command = pull_fails
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("records point there, so Hot stays stopped", report["recovery"])
        self.assertEqual(report["failed"]["storage_progress"], "moved")
        self.assertNotIn("storage_left_behind", report["failed"])
        self.assertNotIn("storage_failed", report["failed"], "those objects moved")

    def test_what_a_finished_move_left_behind_outlasts_a_kill(self):
        self.move_that_failed(self.STUCK)
        self.config["settings"] = {self.SKIP: "3"}
        self.assertEqual(self.finish_the_move_then_kill_the_pull()["storage_left_behind"], {"file": 3})
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [])
        self.assertIn("3 object(s) stayed behind", report["warning"])

    def test_what_a_finished_move_left_behind_outlasts_a_failure_and_then_a_kill(self):
        # The attempt that was killed did not move anything: it carried the count
        # from the failed record, and that is the one the next attempt reads.
        self.move_that_failed(self.STUCK)
        self.config["settings"] = {self.SKIP: "3"}
        worker = self.agent.run_command

        def admin_fails(command, **kwargs):
            if "bootstrap-admin" in command:
                raise subprocess.CalledProcessError(1, command)
            return worker(command, **kwargs)
        self.agent.run_command = admin_fails
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["failed"]["storage_left_behind"], {"file": 3})
        self.agent.run_command = worker

        def killed(command, **kwargs):
            if "bootstrap-admin" in command:
                raise KeyboardInterrupt
            return worker(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(self.agent.state()["deploying"]["storage_left_behind"], {"file": 3})
        self.agent.run_command = worker
        self.jobs.clear()
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [])
        self.assertIn("3 object(s) stayed behind", report["warning"])

    def test_a_job_that_stopped_without_a_report_keeps_the_report_and_what_was_left_behind(self):
        objects = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        objects["file"] = self.STUCK[:2]
        self.agent.command("deploy-now", request_id="d1")
        # Attempts in one second would share a manifest, as they cannot on a host.
        state = self.agent.state()
        reported = state["failed"]["storage_failed"]
        earlier = Path(reported["manifest"]).with_name("storage-move-file-earlier.jsonl")
        Path(reported["manifest"]).rename(earlier)
        reported["manifest"] = str(earlier)
        self.agent.save_state(state)
        worker = self.agent.run_command

        def outage(command, **kwargs):
            if "move-storage" in command:
                raise subprocess.CalledProcessError(1, command, stderr="the database is not answering")
            return worker(command, **kwargs)
        self.agent.run_command = outage
        status = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(status["failed"]["storage_failed"], reported)
        self.assertEqual(status["failed"]["storage_left_behind"], {"file": 1})
        self.agent.run_command = worker
        self.jobs.clear()
        self.assertEqual(self.agent.command("deploy-now", request_id="d3")["state"], "deployed")
        self.assertEqual(self.jobs, [("file", set(self.STUCK[:2]))])

    def test_a_report_beside_what_was_left_behind_still_fits_the_status_parameter(self):
        # Paths as on a host. The AWS status parameter holds 4 KB.
        ids = [f"0190a6e4-7c11-7b3e-9a52-{n:012x}" for n in range(1200)]
        self.moving_both()
        self.worker_with_stuck_objects({"build": ids[:600], "file": ids[600:]}, noise="x" * 5000)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "600"}
        report = self.agent.command("deploy-now", request_id="0" * 32)
        self.assertEqual(report["failed"]["storage_left_behind"], {"build": 600})
        self.assertIn("1200 then stay behind in all", report["recovery"])
        summary = json.dumps({k: v for k, v in report.items() if k not in agent_module.DETAIL_KEYS})
        self.assertLess(len(summary.replace(self.tmp.name, "/var/lib/hot")), 4096)

    def test_taking_the_count_out_tries_again_what_earlier_rounds_left_behind(self):
        objects = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        objects["file"] = self.STUCK[:2]
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["failed"]["storage_left_behind"], {"file": 1})
        # The first is repaired, and the other still fails.
        self.config["settings"] = {}
        objects["file"] = self.STUCK[1:2]
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual(self.jobs, [("file", set())])
        self.assertEqual(report["failed"]["storage_failed"]["ids"], self.STUCK[1:2])
        self.assertNotIn("storage_left_behind", report["failed"])
        self.assertNotIn("then stay behind in all", report["recovery"])
        self.assertEqual(self.left_behind_files(), [])
        # It can then be left behind on its own.
        self.config["settings"] = {self.SKIP: "1"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d3")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK[1:2]))])
        self.assertIn("1 object(s) stayed behind", report["warning"])

    def test_a_retry_cut_short_with_the_count_out_tries_again_what_it_had_left_behind(self):
        # The record from before names what was left behind, and its file is gone:
        # those objects get another try, and the next report says how they did.
        objects = {"file": self.STUCK[:1]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "1"}
        objects["file"] = self.STUCK[:2]
        self.agent.command("deploy-now", request_id="d1")
        self.config["settings"] = {}
        worker = self.agent.run_command

        def killed(command, **kwargs):
            if "move-storage" in command:
                raise KeyboardInterrupt
            return worker(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d2")
        self.agent.run_command = worker
        self.config["settings"] = {self.SKIP: "1"}
        self.jobs.clear()
        report = self.agent.reconcile()
        self.assertEqual(self.jobs, [("file", set(self.STUCK[1:2]))])
        self.assertEqual(report["failed"]["storage_failed"]["ids"], self.STUCK[:1])
        self.assertEqual(report["failed"]["storage_left_behind"], {"file": 1})
        self.assertIn(f"To leave all 1 behind, set settings.{self.SKIP} to 1 instead. 2 then stay behind in all",
                      report["recovery"])

    def test_taking_the_count_out_retries_everything(self):
        stuck = {"file": self.STUCK[:2]}
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.worker_with_stuck_objects(stuck)
        self.agent.reconcile()
        self.config["settings"] = {}
        stuck["file"] = []  # the cause was fixed
        self.jobs.clear()
        self.run.commands.clear()
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.assertEqual(self.jobs, [("file", set())])
        self.assertFalse(any("--skip-file" in c for c in self.run.commands))

    def test_a_retry_that_was_cut_short_still_goes_by_the_report(self):
        self.move_that_failed(self.STUCK)
        reported = self.agent.state()["failed"]["storage_failed"]["manifest"]
        worker = self.agent.run_command

        def killed(command, **kwargs):
            if "move-storage" in command:
                raise KeyboardInterrupt  # a reboot, or the agent killed
            return worker(command, **kwargs)
        self.agent.run_command = killed
        self.config["settings"] = {self.SKIP: "3"}
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(self.skip_files(), [], "removed on the way out")
        # The attempt began a manifest of its own, which lists nothing.
        state = self.agent.state()
        self.assertEqual(state["deploying"]["storage_failed"]["manifest"], reported)
        later = Path(self.tmp.name) / "backups/storage-move-file-later.jsonl"
        later.write_text("")
        state["deploying"]["storage_manifests"]["file"] = str(later)
        self.agent.save_state(state)
        self.agent.run_command = worker
        self.jobs.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(self.jobs, [("file", set(self.STUCK))])

    def test_diagnostics_reads_the_manifest_the_report_came_from(self):
        self.move_that_failed(self.STUCK)
        failed = self.agent.state()["failed"]
        later = Path(self.tmp.name) / "backups/storage-move-file-later.jsonl"
        later.write_text("")
        failed["storage_manifests"]["file"] = str(later)
        self.agent.save_state({**self.agent.state(), "failed": failed})
        named = self.agent.command("diagnostics", request_id="d1")["diagnostics"]["storage_failed"]
        self.assertEqual(named, {"role": "file", "count": 3, "ids": self.STUCK})

    def test_a_job_that_refuses_its_skip_file_moves_nothing_and_says_so(self):
        # Hot refuses before it reaches the database, so the manifest gets no
        # lines, and that is not objects failing.
        self.move_that_failed(self.STUCK)
        reported = self.agent.state()["failed"]["storage_failed"]
        recorder = self.agent.run_command
        inside = agent_module.MOVE_SKIP_FILE_IN_JOB
        self.config["settings"] = {self.SKIP: "3"}
        for number, refusal in enumerate((
                f"Cannot read the skip file {inside}: Permission denied (os error 13)",
                f"Line 2 of the skip file {inside} is not an object id. The file lists one id per line")):
            with self.subTest(refusal=refusal):
                def refusing(command, **kwargs):
                    if "--skip-file" in command:
                        raise subprocess.CalledProcessError(1, command, stderr=f"Database command failed: {refusal}")
                    return recorder(command, **kwargs)
                self.agent.run_command = refusing
                report = self.agent.command("deploy-now", request_id=f"d{number}")
                self.assertEqual(report["state"], "failed", report)
                self.assertIn(refusal.split(". ")[0], report["error"])
                self.assertEqual(self.skip_files(), [])
                state = self.agent.state()
                self.assertEqual(state["failed"]["storage_failed"], reported, "the report stands")
                self.assertTrue(state["stopped"])
        self.agent.run_command = recorder
        self.assertEqual(self.agent.command("deploy-now", request_id="d9")["state"], "deployed", "the report still serves")

    def test_a_release_without_the_flag_says_so_and_nothing_else_changes(self):
        # A move runs in the installed release, which can be older than this
        # agent. It stops at its arguments, with nothing in the manifest.
        self.move_that_failed(self.STUCK)
        reported = self.agent.state()["failed"]["storage_failed"]
        recorder = self.agent.run_command

        def older(command, **kwargs):
            if "--skip-file" in command:
                raise subprocess.CalledProcessError(2, command, stderr=(
                    "error: unexpected argument '--skip-file' found\n\n"
                    "  tip: to pass '--skip-file' as a value, use '-- --skip-file'\n\n"
                    "Usage: hot db move-storage [OPTIONS] --role <ROLE> --to <TO>\n\n"
                    "For more information, try '--help'.\n"))
            return recorder(command, **kwargs)
        self.agent.run_command = older
        self.config["settings"] = {self.SKIP: "3"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn(f"Release 3.0.1 has no `hot db move-storage --skip-file`, so settings.{self.SKIP} cannot leave",
                      report["error"])
        self.assertIn(f"list their ids in settings.{agent_module.MOVE_SKIP_SETTING}", report["error"])
        self.assertNotIn("To leave all", report["recovery"], "that setting is what failed")
        self.assertIn(f"add its id to settings.{agent_module.MOVE_SKIP_SETTING}", report["recovery"])
        self.assertIn("3 did not move", report["recovery"])
        self.assertEqual(self.jobs, [])
        self.assertEqual(self.skip_files(), [])
        state = self.agent.state()
        self.assertEqual(state["failed"]["storage_failed"], reported, "the report stands")
        self.assertTrue(state["stopped"])
        # By id, or with the cause fixed, that release can still finish the move.
        self.config["settings"] = {agent_module.MOVE_SKIP_SETTING: ",".join(self.STUCK)}
        self.jobs.clear()
        self.run.commands.clear()
        self.worker_with_stuck_objects({"file": []})
        self.assertEqual(self.agent.command("deploy-now", request_id="d2")["state"], "deployed")
        move = next(c for c in self.run.commands if "move-storage" in c)
        self.assertNotIn("--skip-file", move)
        self.assertIn(f"{agent_module.MOVE_SKIP_SETTING}={','.join(self.STUCK)}", move)

    def test_only_a_role_with_something_to_leave_behind_gets_the_flag(self):
        self.moving_both()
        self.worker_with_stuck_objects({"file": self.STUCK})
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "3"}
        self.run.commands.clear()
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        moves = {c[c.index("--role") + 1]: c for c in self.run.commands if "move-storage" in c}
        self.assertNotIn("--skip-file", moves["build"], "a release without the flag can still move this role")
        self.assertIn("--skip-file", moves["file"])

    def test_a_count_for_a_role_no_longer_moving_is_refused(self):
        installed = self.config["storage"]
        self.moving_both()
        self.worker_with_stuck_objects({"file": self.STUCK})
        self.agent.reconcile()
        self.config["settings"] = {self.SKIP: "3"}
        self.config["storage"] = dict(self.config["storage"], file=installed["file"])
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("are file objects, whose storage is no longer moving", report["error"])
        self.assertEqual(self.jobs, [])

    def test_a_count_with_the_storage_put_back_has_nothing_to_do(self):
        installed = self.config["storage"]
        self.move_that_failed(self.STUCK)
        self.config["storage"] = installed
        self.config["settings"] = {self.SKIP: "3"}
        self.jobs.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.jobs, [])
        self.assertIn("warning", report)
        self.assertNotIn("stayed behind", report["warning"], "the move is over, and what it did goes by that warning")

    def test_what_a_kill_leaves_is_removed(self):
        installed = self.config["storage"]
        self.move_that_failed(self.STUCK)
        leftovers = [Path(self.tmp.name) / f"backups/storage-{name}-build.txt" for name in ("skip", "left-behind")]
        for leftover in leftovers:
            leftover.write_text(self.STUCK[0] + "\n")
        self.config["storage"] = installed
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        for leftover in leftovers:
            self.assertFalse(leftover.exists())

    def test_a_file_from_an_earlier_move_means_nothing_to_this_one(self):
        # Only the record makes a file what was left behind: a kill can get
        # between a finished deploy's record and removing the file.
        stale = Path(self.tmp.name) / "backups/storage-left-behind-file.txt"
        stale.parent.mkdir(parents=True, exist_ok=True)
        stale.write_text(self.STUCK[0] + "\n")
        self.move_that_failed(self.STUCK[1:])
        self.config["settings"] = {self.SKIP: "2"}
        self.jobs.clear()
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.assertEqual(self.jobs, [("file", set(self.STUCK[1:]))])

    def test_a_retry_that_fails_early_still_knows_the_move_began(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        stuck = "0190a6e4-7c11-7b3e-9a52-0d1f2e3c4b5a"
        recorder = self.run
        self.failing_move('{"kind":"file","id":"' + stuck + '","status":"failed"}\n', stderr="1 object(s) did not move")
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        manifest = self.agent.state()["failed"]["storage_manifests"]["file"]
        self.agent.run_command = recorder
        fetch = self.agent.fetch_release
        self.agent.fetch_release = mock.Mock(side_effect=agent_module.AgentError("the manifest is unavailable"))
        for attempt in ("d1", "d2"):
            status = self.agent.command("deploy-now", request_id=attempt)
            self.assertEqual((status["state"], status["steps"]), ("failed", ["started"]))
            self.assertIn("Some objects may already be in the new storage, so Hot stays stopped", status["recovery"])
            self.assertIn(manifest, status["recovery"])
            self.assertEqual(status["failed"]["storage_failed"]["ids"], [stuck], "the report still names it")
            self.assertNotIn("point the installation back", status["recovery"])
            self.assertTrue(self.agent.state()["stopped"])
            with self.assertRaisesRegex(agent_module.AgentError, "deploy-now instead"):
                self.agent.command("roll-back")
        # A retry before the deploy begins knows it too.
        self.agent.fetch_release = fetch
        prepare = self.agent.prepare_host
        self.agent.prepare_host = mock.Mock(side_effect=agent_module.AgentError("GetSecretValue failed: Throttling"))
        status = self.agent.command("deploy-now", request_id="d3")
        self.assertIn("Some objects may already be in the new storage", status["recovery"])
        self.agent.prepare_host = prepare
        # The move finishes, and the deploy with it.
        report = self.agent.command("deploy-now", request_id="d4")
        self.assertEqual(report["state"], "deployed", report)
        self.assertNotIn("failed", self.agent.state())

    def test_a_retry_that_fails_early_still_knows_the_objects_moved(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "pull"
        self.agent.reconcile()
        self.failing = None
        fetch = self.agent.fetch_release
        self.agent.fetch_release = mock.Mock(side_effect=agent_module.AgentError("the manifest is unavailable"))
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["steps"], ["started"])
        self.assertIn("records point there, so Hot stays stopped", status["recovery"])
        with self.assertRaisesRegex(agent_module.AgentError, "deploy-now instead"):
            self.agent.command("roll-back")
        self.agent.fetch_release = fetch
        self.run.commands.clear()
        self.assertEqual(self.agent.command("deploy-now", request_id="d2")["state"], "deployed")
        self.assertFalse(any("move-storage" in c for c in self.calls()))

    def test_reverting_a_partly_failed_move_starts_hot_and_ends_that_attempt(self):
        installed = self.config["storage"]
        self.config["storage"] = dict(installed, file=self.S3)
        self.failing = "move-storage"
        self.agent.reconcile()
        self.failing = None
        manifest = self.agent.state()["failed"]["storage_manifests"]["file"]
        self.config["storage"] = installed
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("stopped", self.agent.state())
        # It starts, and the report says what that costs.
        self.assertIn("Hot cannot read them until the move finishes", report["warning"])
        self.assertIn("set that storage again, apply, and run deploy-now", report["warning"])
        self.assertIn(manifest, report["warning"])
        failed = self.agent.state()["failed"]
        self.assertIn(report["warning"], failed["recovery"])
        self.assertNotIn("Hot stays stopped", failed["recovery"])
        # A later upgrade that fails is about that upgrade.
        self.release("3.0.2")
        self.failing = "pull"
        status = self.agent.reconcile()
        self.assertIn("The database was not changed and 3.0.1 keeps running", status["recovery"])
        self.assertNotIn("stopped", self.agent.state())

    def partly_failed_move(self):
        """A move of file storage that failed partway; returns its manifest."""
        self.installed = self.config["storage"]
        self.config["storage"] = dict(self.installed, file=self.S3)
        self.failing = "move-storage"
        self.agent.reconcile()
        self.failing = None
        return self.agent.state()["failed"]["storage_manifests"]["file"]

    def assert_warned(self, report, manifest):
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.1"), report)
        self.assertIn("Hot cannot read them until the move finishes", report["warning"])
        self.assertIn(manifest, report["warning"])
        # The failed record is gone now: this is what is left of it.
        self.assertNotIn("failed", self.agent.state())
        self.assertEqual(self.agent.history()[-1]["warning"], report["warning"])
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "local")

    def test_reverting_a_partly_failed_move_with_its_keys_left_warns_too(self):
        # The keys in the operator bundle make deploy-now render again, so this
        # is a deploy rather than a start.
        manifest = self.partly_failed_move()
        self.config["storage"] = self.installed
        self.cloud.secrets[OPERATOR].update(HOT_FILE_AWS_ACCESS_KEY_ID="AKIAEXAMPLEEXAMPLE00",
                                            HOT_FILE_AWS_SECRET_ACCESS_KEY="partner-secret")
        self.run.commands.clear()
        self.assert_warned(self.agent.command("deploy-now", request_id="d1"), manifest)
        self.assertFalse(any("move-storage" in c for c in self.calls()))
        # A later deploy has no more to say of it.
        self.release("3.0.2")
        self.assertNotIn("warning", self.agent.reconcile())

    def test_reverting_a_partly_failed_move_along_with_a_setting_warns_too(self):
        manifest = self.partly_failed_move()
        self.config["storage"] = self.installed
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.assert_warned(self.agent.command("deploy-now", request_id="d1"), manifest)

    def test_reverting_one_role_of_two_warns_though_the_other_moved(self):
        manifest = self.partly_failed_move()
        self.config["storage"] = dict(self.installed, build=dict(self.S3, prefix="builds"))
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertIn("storage-moved", report["steps"])
        self.assertIn(manifest, report["warning"])

    def test_reverting_a_move_that_finished_is_refused_and_keeps_hot_stopped(self):
        installed = self.config["storage"]
        self.config["storage"] = dict(installed, file=self.S3)
        self.failing = "pull"
        status = self.agent.reconcile()
        self.failing = None
        self.assertIn("storage-moved", status["steps"])
        self.assertTrue(self.agent.state()["stopped"])
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3", "the installed bundle is the new render")
        # Only the storage put back, so nothing would render or deploy.
        self.config["storage"] = installed
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("Set that storage again, apply, and run deploy-now", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "Hot does not start on storage the configuration no longer names")
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertNotIn("rolled_back", state["failed"])
        self.assertIsNone(self.agent.resume(), "nor does a reboot")
        # With another setting too it goes through a deploy, which says the same.
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        status = self.agent.reconcile(retry=True)
        self.assertIn("back onto the host is not automated", status["error"])
        self.assertTrue(self.agent.state()["stopped"])
        # The way out is the one it names.
        del self.config["settings"]
        self.config["storage"] = dict(installed, file=self.S3)
        report = self.agent.command("deploy-now", request_id="d2")
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.1"), report)
        self.assertNotIn("stopped", self.agent.state())

    def failed_upgrade_on_a_reverted_move(self):
        """A move that failed partway, its storage put back, then an upgrade whose
        migrations fail: Hot stays stopped, on a partly migrated database, with
        records that may be on the new storage and a dump that predates the failure."""
        manifest = self.partly_failed_move()
        self.config["storage"] = self.installed
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        status = self.agent.reconcile()
        self.failing = None
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("Migrations may be partly applied too", status["recovery"])
        failed = self.agent.state()["failed"]
        self.assertEqual((self.agent.migrated_by(failed), failed["restore_point"]["kind"], failed["storage_progress"]),
                         ("3.0.2", "dump", "moving"))
        self.assertTrue(self.agent.state()["stopped"])
        return manifest

    def test_pointing_back_after_a_failed_upgrade_on_a_reverted_move_keeps_hot_stopped(self):
        self.failed_upgrade_on_a_reverted_move()
        self.release("3.0.1")
        self.assertIsNone(self.agent.reconcile())
        self.run.commands.clear()
        for operation, service in (("deploy-now", ""), ("restart-service", "app")):
            report = self.agent.answer(operation, service, request_id="r" + operation)
            self.assertEqual(report["state"], "rejected", report)
            self.assertIn("3.0.2 may have partly applied its migrations", report["error"])
            self.assertIn("roll-back cannot restore it", report["error"])
            self.assertIn("a fixed release, with the storage setting put back", report["error"])
        self.assertIsNone(self.agent.resume(), "nor does a reboot start it")
        self.assertEqual(self.run.compose_calls(), [])
        self.assertTrue(self.agent.state()["stopped"])

    def test_after_a_failed_upgrade_on_a_reverted_move_a_fixed_release_goes_forward(self):
        manifest = self.failed_upgrade_on_a_reverted_move()
        self.release("3.0.3", latest=24)
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("deployed", "3.0.3"), report)
        self.assertIn("Hot cannot read them until the move finishes", report["warning"])
        self.assertIn(manifest, report["warning"])

    def test_no_note_or_refusal_names_a_roll_back_that_would_refuse(self):
        self.failed_upgrade_on_a_reverted_move()
        self.assertIn("Records may already point at the new storage", self.agent.answer("roll-back", request_id="rb")["error"])
        held = self.agent.scheduler_held(self.agent.state())["scheduler"]
        self.assertIn("fixed release", held)
        self.assertNotIn("roll-back", held)
        # Setting the storage again would be refused too, naming the way that works.
        self.config["storage"] = dict(self.installed, file=self.S3)
        error = self.agent.answer("deploy-now", request_id="d1")["error"]
        self.assertIn("put the setting back and deploy a fixed release", error)
        self.assertNotIn("run roll-back", error)

    def test_a_failed_upgrade_on_a_finished_move_goes_forward_with_that_storage(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "pull"
        self.agent.reconcile()
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.failing = None
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        # Finished, so nothing carries over: a later failure stands alone.
        self.release("3.0.2", latest=23)
        self.failing = "pull"
        self.agent.reconcile()
        self.failing = None
        failed = dict(self.agent.state()["failed"], storage_progress="moved", partly_migrated="3.0.2",
                      restore_point={"kind": "dump", "path": "/backups/pre-3.0.2/postgres.dump"})
        self.agent.save_state({**self.agent.state(), "failed": failed, "stopped": True})
        self.release("3.0.1")
        report = self.agent.answer("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("a fixed release, with that storage kept", report["error"])

    def test_finishing_a_move_warns_of_nothing(self):
        self.partly_failed_move()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertNotIn("warning", report)
        self.assertNotIn("warning", self.agent.history()[-1])

    def test_a_move_that_finished_before_a_later_failure_warns_of_nothing(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.failing = "pull"
        self.agent.reconcile()
        self.failing = None
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertNotIn("warning", report)

    def test_a_drain_that_failed_moved_nothing_to_warn_of(self):
        self.installed = self.config["storage"]
        self.config["storage"] = dict(self.installed, file=self.S3)
        self.failing = "stop"
        self.agent.reconcile()
        self.failing = None
        self.assertNotIn("storage_manifests", self.agent.state()["failed"])
        self.config["storage"] = self.installed
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "deployed", report)
        self.assertNotIn("warning", report)

    def test_a_move_after_a_reboot_while_stopped_has_its_database(self):
        self.stopped_and_rebooted()
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        data = next(i for i, c in enumerate(calls) if c.endswith("up -d --wait --no-recreate postgres valkey"))
        self.assertLess(data, next(i for i, c in enumerate(calls) if "move-storage" in c))
        self.assertIn("releases/3.0.1/bundle/compose.json", calls[data])

    def test_a_reboot_after_the_objects_move_keeps_the_new_bundle(self):
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "pull" in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.agent.run_command = run
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertEqual(self.agent.resume()["started"], "data services")
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3", "the old bundle's storage is behind it")
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertFalse(any("move-storage" in c for c in self.calls()))

    def test_a_bucket_reached_with_new_keys_gets_them_for_the_move(self):
        # The installed worker mounts no key files; the move's worker gets
        # the operator bundle's, delivered first.
        self.cloud.secrets[OPERATOR].update(HOT_FILE_AWS_ACCESS_KEY_ID="AKIAEXAMPLEEXAMPLE00",
                                            HOT_FILE_AWS_SECRET_ACCESS_KEY="partner-secret")
        self.config["storage"] = dict(self.config["storage"], file=self.S3)
        self.run.commands.clear()
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        move = next(c for c in self.run.commands if "move-storage" in c)
        secrets = Path(self.tmp.name) / "secrets/worker"
        for key in ("HOT_FILE_AWS_ACCESS_KEY_ID", "HOT_FILE_AWS_SECRET_ACCESS_KEY"):
            flags = move[:move.index("worker")]
            self.assertIn(f"{key}_FILE=/run/secrets/{key}", flags)
            self.assertIn(f"{secrets / key}:/run/secrets/{key}:ro", flags)
            self.assertTrue((secrets / key).exists())
        self.assertNotIn("partner-secret", " ".join(move))


class NewGrants(UpgradeHarness):
    """The GCP stacks tell the VM about a bucket or a mirror once the host's grant
    exists, and Google's IAM can take minutes more to apply it."""
    GCS = {"backend": "gcs", "bucket": "acme", "prefix": "files"}
    MIRROR = "us-docker.pkg.dev/acme/hot"
    DENIED = ('Error response from daemon: Head "https://us-docker.pkg.dev/v2/acme/hot/hot/manifests/sha256:a": denied: '
              'Permission "artifactregistry.repositories.downloadArtifacts" denied on resource')
    ALWAYS = 10 ** 6
    # The first try, and one after each wait until the limit.
    ATTEMPTS = agent_module.GRANT_WAIT_SECONDS // agent_module.GRANT_RETRY_SECONDS + 1

    def setUp(self):
        super().setUp()
        self.config.update(cloud="gcp", project="example-project")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.refused_writes = 0
        self.cloud.check_storage_write = self.check_storage_write
        self.cloud.registry_password = lambda host: "token"
        self.waits = []
        self.agent.sleep = lambda seconds: self.waits.append(seconds) if seconds == agent_module.GRANT_RETRY_SECONDS else None

    def check_storage_write(self, bucket, prefix):
        self.run.commands.append(["write-check", bucket, prefix])
        if self.refused_writes:
            self.refused_writes -= 1
            raise agent_module.ApiError("/upload/storage/v1/b/acme/o failed: HTTP 403", 403)

    def calls(self):
        return [" ".join(c) for c in self.run.commands]

    def moving_to_gcs(self, refused):
        self.config["storage"] = dict(self.config["storage"], file=self.GCS)
        self.refused_writes = refused
        self.run.commands.clear()

    def no_wait_for_writes(self):
        wait = self.agent.wait_for_storage_write
        self.agent.wait_for_storage_write = lambda moves: wait(moves, timeout=0)
        return wait

    def fake_clock(self):
        """A clock each wait moves on."""
        now = [0]
        self.agent.sleep = lambda seconds: now.__setitem__(0, now[0] + seconds)
        return lambda: now[0]

    def refusing_pulls(self, times, stderr):
        run, left = self.agent.run_command, [times]

        def refusing(command, **kwargs):
            if "pull" in command and left[0]:
                left[0] -= 1
                self.run.commands.append(command)
                raise subprocess.CalledProcessError(1, command, stderr=stderr)
            return run(command, **kwargs)
        self.agent.run_command = refusing
        self.run.commands.clear()

    def bundle(self):
        return Path(self.tmp.name) / "releases/3.0.1/bundle"

    def test_a_move_into_gcs_waits_until_the_host_can_write(self):
        self.moving_to_gcs(refused=3)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        self.assertEqual(calls.count("write-check acme files"), 4)
        self.assertEqual(self.waits, [agent_module.GRANT_RETRY_SECONDS] * 3)
        checked = max(i for i, c in enumerate(calls) if c == "write-check acme files")
        self.assertLess(checked, next(i for i, c in enumerate(calls) if c.endswith("stop scheduler")), "before anything stops")
        self.assertLess(checked, next(i for i, c in enumerate(calls) if "move-storage" in c))

    def test_only_a_move_into_gcs_waits(self):
        self.agent.wait_for_storage_write({"file": StorageMoves.S3})
        self.assertEqual(self.calls().count("write-check acme files"), 0)

    def test_a_move_out_of_gcs_is_refused_before_anything_stops(self):
        # The apply that sets this removes the host's grant on the old bucket, so
        # the move's worker could not read it once Hot had stopped.
        self.moving_to_gcs(refused=0)
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.cloud.secrets[OPERATOR].update(HOT_FILE_AWS_ACCESS_KEY_ID="key", HOT_FILE_AWS_SECRET_ACCESS_KEY="secret")
        self.config["storage"] = dict(self.config["storage"], file=StorageMoves.S3)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("file storage out of GCS is not automated", status["error"])
        self.assertIn("removes this host's access to gs://acme", status["error"])
        self.assertIn("put storage.file back and apply", status["error"])
        self.assertIn("keeps running", status["recovery"])
        self.assertNotIn("stopped", self.agent.state())
        self.assertNotIn("storage-moving", status["steps"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops")
        self.assertFalse(any("move-storage" in c for c in self.calls()))
        # With it put back, the host redeploys nothing.
        del self.cloud.secrets[OPERATOR]["HOT_FILE_AWS_ACCESS_KEY_ID"], self.cloud.secrets[OPERATOR]["HOT_FILE_AWS_SECRET_ACCESS_KEY"]
        self.config["storage"] = dict(self.config["storage"], file=self.GCS)
        self.assertIsNone(self.agent.reconcile())

    def test_a_grant_that_does_not_take_effect_leaves_hot_running(self):
        self.moving_to_gcs(refused=self.ALWAYS)
        wait = self.no_wait_for_writes()
        with mock.patch.object(self.agent, "deliver_secrets", wraps=self.agent.deliver_secrets) as delivered:
            status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("could not write to gs://acme/files", status["error"])
        self.assertIn("file storage did not start to move", status["error"])
        self.assertIn("HTTP 403", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertNotIn("stopped", self.agent.state())
        self.assertNotIn("storage-moving", status["steps"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops or starts")
        self.assertEqual(delivered.call_args_list, [mock.call(only=("migrate",))], "the services' files stay as they were")
        # It waits for deploy-now, and restarts still work meanwhile.
        self.refused_writes = 0
        self.agent.wait_for_storage_write = wait
        self.assertIsNone(self.agent.reconcile())
        self.assertEqual(self.agent.answer("restart-service", "app", request_id="x1")["state"], "restarted")
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")
        self.assertTrue(any("move-storage" in c for c in self.calls()))

    def test_the_wait_for_a_write_has_a_limit(self):
        self.moving_to_gcs(refused=self.ALWAYS)
        clock = self.fake_clock()
        with self.assertRaisesRegex(agent_module.AgentError, "within 10 minutes"):
            self.agent.wait_for_storage_write({"file": self.GCS}, clock=clock)
        self.assertEqual(self.calls().count("write-check acme files"), self.ATTEMPTS)
        self.assertEqual(clock(), agent_module.GRANT_WAIT_SECONDS)

    def test_a_move_already_under_way_stays_stopped_if_the_host_cannot_write(self):
        self.moving_to_gcs(refused=0)
        self.failing = "move-storage"
        self.assertIn("Hot stays stopped", self.agent.reconcile()["recovery"])
        self.failing = None
        self.refused_writes = self.ALWAYS
        self.no_wait_for_writes()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("could not write to gs://acme/files", report["error"])
        self.assertIn("Some objects may already be in the new storage", report["recovery"])
        self.assertTrue(self.agent.state()["stopped"])

    def test_a_refusal_by_a_new_mirror_is_tried_again_with_a_fresh_sign_in(self):
        self.config["image_registry"] = self.MIRROR
        self.refusing_pulls(2, self.DENIED)
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "deployed", status)
        calls = self.calls()
        self.assertEqual(sum(c.startswith("docker login") for c in calls), 3)
        self.assertEqual(sum(c.endswith("pull --quiet") for c in calls), 3)
        self.assertEqual(self.waits, [agent_module.GRANT_RETRY_SECONDS] * 2)

    def test_a_sign_in_the_mirror_refuses_is_tried_again_too(self):
        self.config["image_registry"] = self.MIRROR
        run, left = self.agent.run_command, [1]

        def refusing(command, **kwargs):
            if command[:2] == ["docker", "login"] and left[0]:
                left[0] -= 1
                raise subprocess.CalledProcessError(1, command, stderr="Error response from daemon: unauthorized")
            return run(command, **kwargs)
        self.agent.run_command = refusing
        self.agent.pull_images(self.bundle())
        self.assertEqual(self.waits, [agent_module.GRANT_RETRY_SECONDS])

    def test_the_wait_for_a_mirror_has_a_limit_and_hot_keeps_running(self):
        self.config["image_registry"] = self.MIRROR
        self.refusing_pulls(self.ALWAYS, self.DENIED)
        clock = self.fake_clock()
        with self.assertRaises(subprocess.CalledProcessError):
            self.agent.pull_images(self.bundle(), clock=clock)
        self.assertEqual(sum(c.endswith("pull --quiet") for c in self.calls()), self.ATTEMPTS)
        self.assertEqual(clock(), agent_module.GRANT_WAIT_SECONDS)
        pull = self.agent.pull_images
        self.agent.pull_images = lambda bundle: pull(bundle, timeout=0)
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("pull --quiet exited 1", status["error"])
        self.assertIn("denied", status["error"])
        self.assertIn("3.0.1 keeps running", status["recovery"])
        self.assertFalse(any(c.startswith("stop ") for c in self.run.compose_calls()), "nothing stops")

    def test_other_pull_failures_are_reported_at_once(self):
        ecr = "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot"
        for cloud, registry, stderr in (("gcp", self.MIRROR, "manifest unknown: the mirror does not hold it"),
                                        ("gcp", self.MIRROR, None),
                                        ("gcp", "mirror.example.com/hot", self.DENIED),
                                        ("aws", ecr, self.DENIED)):
            with self.subTest(cloud=cloud, registry=registry, stderr=stderr):
                self.config.update(cloud=cloud, image_registry=registry)
                self.refusing_pulls(self.ALWAYS, stderr)
                with self.assertRaises(subprocess.CalledProcessError):
                    self.agent.pull_images(self.bundle())
                self.assertEqual(sum(c.endswith("pull --quiet") for c in self.calls()), 1)
                self.assertEqual(self.waits, [])


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

    def killed_at(self, word, **reconcile):
        """The agent dies as Compose is told `word`, and the command that comes
        next takes the lock before the loop's next cycle."""
        run = self.agent.run_command

        def crash(command, **kwargs):
            if word in command:
                raise KeyboardInterrupt
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile(**reconcile)
        self.agent.run_command = run
        self.run.commands.clear()

    def rebooted(self):
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        (Path(self.tmp.name) / "resumed").unlink()

    def test_a_restart_keeps_the_scheduler_off_a_deploy_killed_in_its_migrations(self):
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        for rebooted in (False, True):
            with self.subTest(rebooted=rebooted):
                if rebooted:
                    self.rebooted()
                report = self.agent.answer("restart-service", "scheduler", request_id="r1")
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("The scheduler is stopped: the interrupted deploy of 3.0.2", report["error"])
                self.assertIn("next cycle or deploy-now takes that deploy up", report["error"])
                self.assertEqual(self.run.compose_calls(), [])
        self.assertFalse((Path(self.tmp.name) / "secrets").exists(), "a refusal delivers nothing")
        # The boot's start goes on as if the command never came, then the deploy.
        self.assertEqual(self.agent.resume()["started"], "data services")
        self.assertNotIn("up -d scheduler", self.run.compose_calls())
        self.run.commands.clear()
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("deployed", "3.0.2"), status)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertIn("releases/3.0.2/bundle", " ".join([c for c in self.run.commands if "compose" in c][-1]))
        # Past that deploy, restarts are as before.
        self.assertEqual(self.agent.answer("restart-service", "scheduler", request_id="r2")["state"], "restarted")

    def test_other_services_restart_after_a_deploy_killed_in_its_migrations(self):
        # As after a failed one: only the scheduler is held.
        self.release("3.0.2", latest=23)
        self.killed_at("migrate")
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="r1")["state"], "restarted")

    def test_a_restart_goes_ahead_after_a_deploy_killed_before_any_migrations(self):
        self.release("3.0.2", latest=23)
        self.killed_at("--wait")
        self.assertNotIn("migrating", self.agent.state()["deploying"]["steps"])
        self.assertEqual(self.agent.answer("restart-service", "scheduler", request_id="r1")["state"], "restarted")

    def test_a_retry_killed_early_still_holds_the_scheduler_for_the_attempt_before_it(self):
        self.fail_upgrade()
        self.killed_at("--wait", retry=True)
        deploying = self.agent.state()["deploying"]
        self.assertNotIn("migrating", deploying["steps"])
        self.assertEqual(deploying["partly_migrated"], "3.0.2")
        for command, service in (("restart-service", "scheduler"), ("roll-back", "")):
            with self.subTest(command=command):
                report = self.agent.answer(command, service, request_id="r1")
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("the interrupted deploy of 3.0.2", report["error"])
        self.assertEqual(self.run.compose_calls(), [])
        self.assertEqual(self.agent.state()["failed"]["steps"][-1], "migrating", "the record of the failure stands")

    def test_a_roll_back_refuses_a_deploy_killed_in_its_migrations_past_an_older_failure(self):
        # The older failure touched no migrations, so nothing would be restored,
        # and all of Hot would start from the installed bundle on this schema.
        self.release("3.0.2")
        self.failing = "--wait"
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.failing = None
        self.release("3.0.3", latest=23)
        self.killed_at("migrate")
        for rebooted in (False, True):
            with self.subTest(rebooted=rebooted):
                if rebooted:
                    self.rebooted()
                report = self.agent.answer("roll-back", request_id="r1")
                self.assertEqual(report["state"], "rejected", report)
                self.assertIn("3.0.1 cannot start yet: the interrupted deploy of 3.0.3", report["error"])
                self.assertIn("If it fails, run roll-back then", report["error"])
                self.assertEqual(self.run.compose_calls(), [])
        self.assertEqual(self.agent.state()["failed"]["release"], "3.0.2", "nothing is recorded as rolled back")
        self.assertNotIn("rolled_back", self.agent.state()["failed"])
        self.agent.resume()
        # Should the deploy fail after all, roll-back undoes what it did.
        self.failing = "migrate"
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.failing = None
        self.run.commands.clear()
        report = self.agent.answer("roll-back", request_id="r2")
        self.assertEqual((report["state"], report["rolled_back_from"]), ("rolled-back", "3.0.3"), report)
        self.assertTrue(any("pg_restore" in c for c in self.run.commands))
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")

    def test_a_deploy_nothing_can_take_up_ends_as_a_failure_so_roll_back_can_run(self):
        # Hot was stopped when 3.0.2 failed in its migrations. The fix was killed in
        # its own, and then the pointer went back at the installed release with a
        # changed configuration: that deploy waits for roll-back, which would wait
        # for this one.
        self.agent.command("drain-and-stop", request_id="s1")
        self.fail_upgrade()
        self.assertTrue(self.agent.state()["stopped"])
        dump = self.agent.state()["failed"]["restore_point"]["path"]
        self.release("3.0.3", latest=24)
        self.killed_at("migrate")
        self.cloud.parameters["pointer"] = "3.0.1"
        self.config["settings"] = {"HOT_EXAMPLE_SETTING": "changed"}
        status = self.agent.reconcile()
        self.assertEqual((status["state"], status["release"]), ("failed", "3.0.3"), status)
        self.assertIn("Run roll-back", status["error"])
        state = self.agent.state()
        self.assertNotIn("deploying", state)
        self.assertTrue(state["stopped"])
        self.assertEqual(state["failed"]["restore_point"]["path"], dump, "from before all of those migrations")
        self.run.commands.clear()
        report = self.agent.answer("roll-back", request_id="r1")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertTrue(any("pg_restore" in c for c in self.run.commands))

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
        with self.assertRaisesRegex(agent_module.AgentError,
                                    "run drain-and-stop, restore the database to 2026-10-29T12:00:00.*then release 3.0.1"):
            self.agent.command("roll-back")
        self.assertEqual(self.run.commands, [], "nothing is stopped")

    def test_a_dump_is_restored_once(self):
        self.fail_upgrade()
        self.agent.command("roll-back")
        self.run.commands.clear()
        with self.assertRaisesRegex(agent_module.AgentError, "Already rolled back"):
            self.agent.command("roll-back")
        self.assertEqual(self.run.commands, [], "nothing is stopped or restored again")

    def test_a_reboot_after_a_failed_migration_keeps_the_scheduler_stopped(self):
        self.fail_upgrade()
        secrets = Path(self.tmp.name) / "secrets"
        shutil.rmtree(secrets)
        report = self.agent.resume()
        self.assertEqual(report["state"], "resumed", report)
        self.assertIn("may have partly applied its migrations", report["scheduler"])
        calls = self.run.compose_calls()
        self.assertIn("up -d --remove-orphans app api worker task-worker", calls)
        self.assertNotIn("up -d scheduler", calls, "as the failure left it: no schedules on a partly migrated schema")
        # Once rolled back, a reboot starts the installed release whole.
        self.agent.command("roll-back")
        shutil.rmtree(secrets)
        self.run.commands.clear()
        self.assertNotIn("scheduler", self.agent.resume())
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")

    def test_a_retry_after_a_roll_back_takes_a_new_restore_point(self):
        self.fail_upgrade()
        self.agent.command("roll-back")
        # The database has taken writes since the first dump.
        self.failing = "migrate"
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(status["state"], "failed")
        calls = [" ".join(c) for c in self.run.commands]
        dump = next(i for i, c in enumerate(calls) if "pg_dump" in c)
        self.assertLess(dump, next(i for i, c in enumerate(calls) if c.endswith("run --rm migrate")))
        self.assertNotIn("rolled_back", self.agent.state()["failed"])
        self.failing = None
        self.assertEqual(self.agent.command("roll-back")["release"], "3.0.1", "the new failure can be rolled back")

    def test_a_failed_restore_is_reported_and_keeps_hot_stopped(self):
        self.fail_upgrade()
        self.failing = "pg_restore"
        report = self.agent.answer("roll-back", request_id="rb-9")
        self.assertEqual((report["state"], report["command"], report["request_id"]), ("failed", "roll-back", "rb-9"))
        self.assertIn("pg_restore", report["error"])
        self.assertEqual(report["steps"], ["stopped"])
        self.assertIn("Hot stays stopped, and the database may be partly restored", report["recovery"])
        self.assertEqual(self.cloud.statuses[-1]["request_id"], "rb-9")
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertEqual(state["failed"]["roll_back_failed"]["steps"], ["stopped"])
        self.assertNotIn("rolled_back", state["failed"])
        self.assertIn("roll_back_failed_from", self.agent.history()[-1])
        # A reboot does not start Hot on a partly restored database.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        # Rolling back again restores from the start, with the secrets delivered again.
        self.failing = None
        report = self.agent.answer("roll-back", request_id="rb-10")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertTrue((Path(self.tmp.name) / "secrets/app/HOT_APP_SESSION_SECRET").exists())
        self.assertNotIn("stopped", self.agent.state())
        self.assertNotIn("roll_back_failed", self.agent.state()["failed"])

    def test_a_restore_that_fails_its_checks_keeps_hot_stopped(self):
        self.fail_upgrade()
        self.failing = "migrate"
        report = self.agent.answer("roll-back", request_id="rb-9")
        self.assertEqual(report["state"], "failed", report)
        self.assertEqual(report["steps"], ["stopped", "restored"])
        self.assertIn("Hot stays stopped: the database was restored", report["recovery"])
        self.assertTrue(self.agent.state()["stopped"])
        # A reboot does not start Hot on a database that failed its checks.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        self.failing = None
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-10")["state"], "rolled-back")
        self.assertNotIn("stopped", self.agent.state())

    def test_a_failure_after_the_checks_says_nothing_of_stopping(self):
        self.fail_upgrade()
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        report = self.agent.answer("roll-back", request_id="rb-9")
        self.assertEqual(report["steps"], ["stopped", "restored", "checked"])
        self.assertNotIn("stays stopped", report["recovery"])
        self.assertNotIn("stopped", self.agent.state())

    def not_ready(self):
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))

    def only_starts_hot(self, report, *before):
        """A roll-back that found the database restored and checked: it started
        Hot, whole, and touched nothing else."""
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertEqual(report["steps"], ["stopped", "restored", "checked", "started"])
        self.assertEqual(self.run.compose_calls(), [*before, "up -d --remove-orphans app api worker task-worker",
                                                    "up -d --force-recreate --no-deps ingress", "up -d scheduler"])
        failed = self.agent.state()["failed"]
        self.assertIn("rolled_back", failed)
        self.assertNotIn("roll_back_failed", failed)
        self.assertNotIn("stopped", self.agent.state())

    def test_a_retry_after_the_checks_only_starts_hot(self):
        # What came up since may have written to the database.
        self.fail_upgrade()
        self.not_ready()
        report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertIn("The database is restored", report["recovery"])
        self.assertIn("run roll-back again: it only starts Hot, and does not restore", report["recovery"])
        self.assertNotIn("from the start", report["recovery"])
        # A retry that fails the same way says the same.
        self.run.commands.clear()
        report = self.agent.answer("roll-back", request_id="rb-2")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["stopped", "restored", "checked"]), report)
        self.assertIn("it only starts Hot", report["recovery"])
        self.assertFalse(any("pg_restore" in c for c in self.run.commands))
        # The dump is no longer needed, and the deploy-now in between says what to do.
        self.release("3.0.1")
        note = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(note["state"], "current", note)
        self.assertIn("a roll-back restored the database", note["scheduler"])
        self.assertIn("it only starts Hot", note["scheduler"])
        self.assertNotIn("partly applied", note["scheduler"])
        Path(self.agent.state()["failed"]["restore_point"]["path"]).unlink()
        self.agent.wait_ready = lambda: None
        self.run.commands.clear()
        self.only_starts_hot(self.agent.answer("roll-back", request_id="rb-3"))
        with self.assertRaisesRegex(agent_module.AgentError, "Already rolled back"):
            self.agent.command("roll-back")

    def test_a_stop_after_the_checks_leaves_a_roll_back_that_only_starts_hot(self):
        self.fail_upgrade()
        self.not_ready()
        self.agent.answer("roll-back", request_id="rb-1")
        self.agent.answer("drain-and-stop", request_id="s1")
        self.release("3.0.1")
        self.run.commands.clear()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("a roll-back restored the database", report["error"])
        self.assertIn("Run roll-back again: it only starts Hot", report["error"])
        self.assertNotIn("restores", report["error"])
        self.agent.wait_ready = lambda: None
        self.run.commands.clear()
        self.only_starts_hot(self.agent.answer("roll-back", request_id="rb-2"))

    def test_a_kill_after_the_checks_does_not_restore_again(self):
        self.fail_upgrade()
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "--remove-orphans" in command:
                raise KeyboardInterrupt  # a reboot, or the agent killed
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("roll-back", request_id="rb-1")
        self.agent.run_command = run
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume(), "it stays stopped")
        self.run.commands.clear()
        self.only_starts_hot(self.agent.answer("roll-back", request_id="rb-2"))
        self.assertTrue((Path(self.tmp.name) / "secrets/app/HOT_APP_SESSION_SECRET").exists())

    def test_a_fixed_release_after_the_checks_takes_a_new_restore_point(self):
        # The restored database has taken writes, so the old dump would lose them.
        self.fail_upgrade()
        old = self.agent.state()["failed"]["restore_point"]["path"]
        self.not_ready()
        self.agent.answer("roll-back", request_id="rb-1")
        self.agent.wait_ready = lambda: None
        self.release("3.0.3", latest=24)
        self.failing = "migrate"
        self.run.commands.clear()
        self.assertEqual(self.agent.answer("deploy-now", request_id="d1")["state"], "failed")
        self.assertTrue(any("pg_dump" in c for c in self.run.commands))
        failed = self.agent.state()["failed"]
        self.assertNotEqual(failed["restore_point"]["path"], old)
        self.assertNotIn("roll_back_failed", failed)

    def test_a_failure_after_the_checks_leaves_a_stop_from_before_it(self):
        self.fail_upgrade()
        self.agent.command("drain-and-stop", request_id="s1")
        self.agent.wait_ready = mock.Mock(side_effect=agent_module.AgentError("The app and API did not become ready"))
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-9")["state"], "failed")
        self.assertTrue(self.agent.state()["stopped"])

    def only_roll_back_goes_next(self, *commands):
        """Every way to start or deploy is refused, naming roll-back, and leaves
        Hot stopped and the roll-back available."""
        self.run.commands.clear()
        for operation, service in commands:
            report = self.agent.answer(operation, service, request_id="r1")
            self.assertEqual(report["state"], "rejected", report)
            self.assertIn("roll-back", report["error"])
        self.assertIsNone(self.agent.resume())
        self.assertEqual(self.run.compose_calls(), [], "nothing runs on this database")
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertNotIn("rolled_back", state["failed"])

    def test_nothing_starts_on_a_database_that_may_be_partly_restored(self):
        self.fail_upgrade()
        dump = self.agent.state()["failed"]["restore_point"]["path"]
        self.failing = "pg_restore"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-1")["state"], "failed")
        self.failing = None
        starts = (("deploy-now", ""), ("restart-service", "scheduler"))
        # With the pointer on the failed release.
        self.only_roll_back_goes_next(*starts)
        self.assertIn(dump, self.agent.answer("deploy-now", request_id="d1")["error"])
        # On the installed one, as the README's sequence has it.
        self.release("3.0.1")
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(*starts)
        # On a new one, and with a new configuration, which the loop would deploy.
        self.release("3.0.3", latest=24)
        self.run.commands.clear()
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(*starts)
        self.release("3.0.1")
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(*starts)
        # The way out still works, and everything else works after it.
        del self.config["settings"]
        report = self.agent.answer("roll-back", request_id="rb-2")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertEqual(report["steps"], ["stopped", "restored", "checked", "started"])
        self.assertNotIn("stopped", self.agent.state())
        self.assertEqual(self.agent.answer("deploy-now", request_id="d2")["state"], "current")

    def test_nothing_starts_on_a_restored_database_that_failed_its_checks(self):
        self.fail_upgrade()
        self.failing = "migrate"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-1")["state"], "failed")
        self.failing = None
        self.only_roll_back_goes_next(("deploy-now", ""))
        self.release("3.0.1")
        self.only_roll_back_goes_next(("deploy-now", ""))
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-2")["state"], "rolled-back")

    def test_a_roll_back_cut_short_keeps_hot_off_the_database_too(self):
        self.fail_upgrade()
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "pg_restore" in command:
                raise KeyboardInterrupt  # a reboot, or the agent killed
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("roll-back", request_id="rb-1")
        self.agent.run_command = run
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.only_roll_back_goes_next(("deploy-now", ""))
        report = self.agent.answer("roll-back", request_id="rb-2")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertNotIn("stopped", self.agent.state())
        self.assertNotIn("roll_back_failed", self.agent.state()["failed"])

    def test_a_roll_back_that_stopped_nothing_leaves_deploys_alone(self):
        self.fail_upgrade()
        with mock.patch.object(agent_module.shutil, "disk_usage", lambda path: mock.Mock(free=2**20)):
            report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertEqual((report["state"], report["steps"]), ("failed", []), report)
        self.assertEqual(self.agent.answer("deploy-now", request_id="d1")["state"], "deployed")

    def retry_refused_for_room(self):
        low = mock.patch.object(agent_module.shutil, "disk_usage", lambda path: mock.Mock(free=2**20))
        with low:
            return self.agent.answer("roll-back", request_id="rb-2")

    def check_only_roll_back_goes_next_after_a_retry(self, report, kept):
        self.assertEqual((report["state"], report["steps"]), ("failed", []), report)
        self.assertIn("Staging the database dump", report["error"])
        self.assertTrue(report["recovery"].startswith("Hot stays stopped"), report)
        failed = self.agent.state()["failed"]
        self.assertEqual(failed["roll_back_failed"]["steps"], kept, "the record that a restore began")
        starts = (("deploy-now", ""), ("restart-service", "scheduler"))
        self.only_roll_back_goes_next(*starts)
        # On the installed release, and on a new one, which the loop would deploy.
        self.release("3.0.1")
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(*starts)
        self.release("3.0.3", latest=24)
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(*starts)
        # The restore still works, from the start, when room is made.
        report = self.agent.answer("roll-back", request_id="rb-3")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertIn("restored", report["steps"])
        self.assertNotIn("stopped", self.agent.state())
        self.assertNotIn("roll_back_failed", self.agent.state()["failed"])

    def test_a_retry_refused_for_room_does_not_forget_a_restore_that_failed(self):
        self.fail_upgrade()
        self.failing = "pg_restore"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-1")["steps"], ["stopped"])
        self.failing = None
        report = self.retry_refused_for_room()
        self.assertIn("may be partly restored", report["recovery"])
        self.check_only_roll_back_goes_next_after_a_retry(report, ["stopped"])

    def test_a_retry_refused_for_room_does_not_forget_a_restore_cut_short(self):
        self.fail_upgrade()
        run = self.agent.run_command

        def crash(command, **kwargs):
            if "pg_restore" in command:
                raise KeyboardInterrupt  # a reboot, or the agent killed
            return run(command, **kwargs)
        self.agent.run_command = crash
        with self.assertRaises(KeyboardInterrupt):
            self.agent.command("roll-back", request_id="rb-1")
        self.agent.run_command = run
        self.check_only_roll_back_goes_next_after_a_retry(self.retry_refused_for_room(), ["stopped"])

    def test_a_retry_refused_for_room_does_not_forget_checks_that_failed(self):
        self.fail_upgrade()
        self.failing = "migrate"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-1")["steps"], ["stopped", "restored"])
        self.failing = None
        report = self.retry_refused_for_room()
        self.assertIn("the installed release's checks on it failed", report["recovery"])
        self.check_only_roll_back_goes_next_after_a_retry(report, ["stopped", "restored"])

    def test_a_retry_that_fails_stopping_does_not_forget_a_restore_that_failed(self):
        self.fail_upgrade()
        self.failing = "pg_restore"
        self.agent.answer("roll-back", request_id="rb-1")
        self.failing = "stop"
        report = self.agent.answer("roll-back", request_id="rb-2")
        self.failing = None
        self.assertEqual((report["state"], report["steps"]), ("failed", []), report)
        self.assertEqual(self.agent.state()["failed"]["roll_back_failed"]["steps"], ["stopped"])
        self.only_roll_back_goes_next(("deploy-now", ""))

    def test_a_retry_that_stopped_again_says_how_far_it_got(self):
        self.fail_upgrade()
        self.failing = "migrate"
        self.agent.answer("roll-back", request_id="rb-1")
        self.failing = "pg_restore"
        self.agent.answer("roll-back", request_id="rb-2")
        self.failing = None
        self.assertEqual(self.agent.state()["failed"]["roll_back_failed"]["steps"], ["stopped"])

    def test_a_restore_that_will_not_fit_leaves_hot_running_and_nothing_behind(self):
        self.fail_upgrade()
        staged = Path(self.tmp.name) / "data/postgres/restore.dump"
        with mock.patch.object(agent_module.shutil, "disk_usage", lambda path: mock.Mock(free=2**20)):
            report = self.agent.answer("roll-back", request_id="rb-9")
        self.assertEqual((report["state"], report["steps"]), ("failed", []), report)
        self.assertIn("Staging the database dump for the restore needs", report["error"])
        self.assertIn("then run roll-back", report["error"])
        self.assertFalse(any(c.startswith("stop") for c in self.run.compose_calls()), "refused before anything stops")
        self.assertNotIn("stopped", self.agent.state())

        def partial(source, target, **kwargs):
            Path(target).write_text("part of the dump")
            raise OSError(28, "No space left on device")
        with mock.patch.object(agent_module.shutil, "copy", partial):
            report = self.agent.answer("roll-back", request_id="rb-10")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["stopped"]), report)
        self.assertIn("No space left on device", report["error"])
        self.assertFalse(staged.exists(), "no partial copy is left beside the database")
        self.assertTrue(self.agent.state()["stopped"])

    def test_a_copy_a_kill_left_behind_does_not_count_against_the_retry(self):
        # A reboot or power loss runs no `finally`, and these hosts have no
        # shell to delete the file with.
        self.fail_upgrade()
        dump = Path(self.agent.state()["failed"]["restore_point"]["path"])
        staged = Path(self.tmp.name) / "data/postgres/restore.dump"
        staged.write_text(dump.read_text())
        room = dump.stat().st_size + agent_module.STAGING_RESERVE_BYTES

        def usage(path):
            return mock.Mock(free=room - (staged.stat().st_size if staged.exists() else 0))
        with mock.patch.object(agent_module.shutil, "disk_usage", usage):
            report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertFalse(staged.exists())

    def test_a_disk_grown_for_the_restore_is_seen_by_its_room_check(self):
        # On a mounted disk only hot-data-disk grows the filesystem, and
        # restore_pending keeps a deploy, which would run it, from happening.
        self.fail_upgrade()
        run, grown = self.agent.run_command, []

        def grow(command, **kwargs):
            if command[0].endswith("hot-data-disk"):
                grown.append(command)
            return run(command, **kwargs)
        self.agent.run_command = grow
        usage = lambda path: mock.Mock(free=2**40 if grown else 2**20)  # noqa: E731
        with mock.patch.object(agent_module.shutil, "disk_usage", usage):
            report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertEqual(grown, [[str(agent_module.LIBEXEC / "hot-data-disk"),
                                  "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol0abc"]], "never formats")

    def test_a_disk_that_cannot_be_grown_leaves_it_to_the_room_check(self):
        self.fail_upgrade()
        run = self.agent.run_command

        def refuse(command, **kwargs):
            if command[0].endswith("hot-data-disk"):
                raise subprocess.CalledProcessError(1, command, stderr="Data disk is not attached")
            return run(command, **kwargs)
        self.agent.run_command = refuse
        with mock.patch.object(agent_module.shutil, "disk_usage", lambda path: mock.Mock(free=2**20)):
            report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertEqual((report["state"], report["steps"]), ("failed", []), report)
        self.assertIn("grow the disk with a larger size, then run roll-back", report["error"])
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-2")["state"], "rolled-back", "there is room")

    def restore_off_this_host(self):
        """A failed upgrade as a managed or existing database leaves it:
        roll-back names the time to restore to."""
        self.fail_upgrade()
        failed = self.agent.state()["failed"]
        failed["restore_point"] = {"kind": "point-in-time", "time": "2026-10-29T12:00:00+00:00"}
        self.agent.save_state({**self.agent.state(), "failed": failed})
        with self.assertRaisesRegex(agent_module.AgentError, "restore the database to 2026-10-29T12:00:00"):
            self.agent.command("roll-back")

    def test_pointing_back_after_a_provider_restore_starts_the_release_whole(self):
        # As the README has it: drain-and-stop, restore with the provider, then
        # `release` the installed version.
        self.restore_off_this_host()
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.1")
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        self.assertNotIn("scheduler", report)
        calls = self.run.compose_calls()
        self.assertEqual(calls[-1], "up -d scheduler")
        # The release's own checks come before anything starts on that database.
        self.assertLess(calls.index("--profile admin run --rm migrate"), calls.index("--profile admin run --rm provision"))
        self.assertLess(calls.index("--profile admin run --rm provision"),
                        calls.index("up -d --remove-orphans app api worker task-worker"))
        self.assertTrue(all("releases/3.0.1/bundle" in " ".join(c) for c in self.run.commands if "compose" in c))
        state = self.agent.state()
        self.assertNotIn("stopped", state)
        self.assertIn("rolled_back", state["failed"])
        self.assertIn("started again by pointing back", state["failed"]["recovery"])
        self.assertNotIn("Migrations may be partly applied", state["failed"]["recovery"])
        with self.assertRaisesRegex(agent_module.AgentError, "by pointing back at it.*restore it again yourself"):
            self.agent.command("roll-back")
        # A reboot starts it whole too.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.run.commands.clear()
        self.assertNotIn("scheduler", self.agent.resume())
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        # A later upgrade that fails starts over from the database as it is.
        self.release("3.0.2", latest=23)
        self.failing = "pull"
        status = self.agent.command("deploy-now", request_id="d2")
        self.assertIn("The database was not changed and 3.0.1 keeps running", status["recovery"])
        self.assertNotIn("partly_migrated", self.agent.state()["failed"])
        self.failing = "migrate"
        self.run.commands.clear()
        status = self.agent.command("deploy-now", request_id="d3")
        self.assertTrue(any("pg_dump" in c for c in self.run.commands), "a new restore point, not the old one")
        self.assertNotIn("2026-10-29", json.dumps(self.agent.state()["failed"]["restore_point"]))

    def test_pointing_back_without_the_restore_keeps_hot_stopped(self):
        # The installed release's migrate refuses a database holding newer
        # migrations, which is how a restore that was missed shows.
        self.restore_off_this_host()
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.1")
        self.failing = "migrate"
        self.run.commands.clear()
        before = self.agent.state()["failed"]
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["command"]), ("failed", "deploy-now"), report)
        self.assertIn("Hot stays stopped: 3.0.1 did not pass its checks", report["error"])
        self.assertIn("run --rm migrate", report["error"])
        self.assertIn("restore the database to 2026-10-29T12:00:00", report["recovery"])
        self.assertIn("then run deploy-now again", report["recovery"])
        self.assertEqual([c for c in self.run.compose_calls() if " scheduler" in c or "--remove-orphans" in c], [],
                         "nothing starts on that database")
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertEqual(state["failed"], before, "the attempt is not over, and roll-back still says what to do")
        # A reboot does not start it either.
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        # Once restored, the same command starts it, with its secrets delivered again.
        self.failing = None
        report = self.agent.answer("deploy-now", request_id="d2")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertTrue(self.agent.state()["failed"]["pointed_back"])

    def test_a_check_that_cannot_run_keeps_hot_stopped_too(self):
        self.restore_off_this_host()
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.1")
        self.failing = "provision"
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("run --rm provision", report["error"])
        self.assertTrue(self.agent.state()["stopped"])
        self.assertNotIn("rolled_back", self.agent.state()["failed"])
        self.assertEqual(self.agent.answer("deploy-now", request_id="d2")["state"], "failed")
        self.failing = None
        self.assertEqual(self.agent.answer("deploy-now", request_id="d3")["state"], "resumed")

    def test_pointing_back_after_a_reboot_has_what_the_checks_need(self):
        self.restore_off_this_host()
        self.stopped_and_rebooted()
        self.release("3.0.1")
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["release"]), ("resumed", "3.0.1"), report)
        calls = self.run.compose_calls()
        self.assertLess(calls.index("up -d --wait --no-recreate postgres valkey"),
                        calls.index("--profile admin run --rm migrate"))

    def test_a_recovery_off_this_host_says_to_stop_hot_first(self):
        # Restoring under a running Hot loses what it writes, and only a stopped
        # Hot starts whole when pointed back at.
        self.agent.restore_point = lambda bundle, release: {"kind": "point-in-time", "time": "2026-10-29T12:00:00+00:00"}
        self.fail_upgrade()
        recovery = self.agent.state()["failed"]["recovery"]
        self.assertIn("Deploy a fixed release, or run drain-and-stop, restore the database to 2026-10-29T12:00:00", recovery)
        self.assertIn("then release 3.0.1.", recovery)
        self.assertNotIn("deploy 3.0.1", recovery)
        # And the note on the scheduler says what clears it, which the steps do.
        self.release("3.0.1")
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "current", report)
        self.assertIn("may have partly applied its migrations", report["scheduler"])
        self.assertIn("To start it, run drain-and-stop, restore the database to 2026-10-29T12:00:00", report["scheduler"])
        self.assertIn("then release 3.0.1", report["scheduler"])
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s1")["state"], "stopped")
        report = self.agent.answer("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "resumed", report)
        self.assertNotIn("scheduler", report)

    def test_pointing_back_at_a_partly_migrated_local_database_takes_roll_back_first(self):
        # Hot was stopped for the upgrade, which failed in migrate.
        self.agent.command("drain-and-stop", request_id="s1")
        self.fail_upgrade()
        dump = self.agent.state()["failed"]["restore_point"]["path"]
        self.assertTrue(self.agent.state()["stopped"])
        self.release("3.0.1")
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(("deploy-now", ""), ("restart-service", "worker"))
        error = self.agent.answer("deploy-now", request_id="d1")["error"]
        self.assertIn(f"Run roll-back: it restores {dump} and starts 3.0.1", error)
        # The restore is still there for it, and the README's sequence ends well.
        report = self.agent.answer("roll-back", request_id="rb-1")
        self.assertEqual(report["state"], "rolled-back", report)
        self.assertIn("restored", report["steps"])
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("stopped", self.agent.state())
        report = self.agent.answer("deploy-now", request_id="d2")
        self.assertEqual(report["state"], "current")
        self.assertNotIn("scheduler", report)

    def test_a_stop_after_the_failure_takes_roll_back_first_too(self):
        self.fail_upgrade()
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.1")
        self.only_roll_back_goes_next(("deploy-now", ""))
        self.config["settings"] = {"HOT_LOG_LEVEL_FILTER": "warn"}
        self.assertIsNone(self.agent.reconcile())
        self.only_roll_back_goes_next(("deploy-now", ""))
        self.assertIn("restored", self.agent.answer("roll-back", request_id="rb-1")["steps"])

    def test_restarting_the_scheduler_does_not_get_around_its_hold(self):
        self.fail_upgrade()
        report = self.agent.answer("restart-service", "scheduler", request_id="x1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("may have partly applied its migrations", report["error"])
        self.assertIn("To start it, run roll-back", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stops or starts")
        # Only the scheduler is held, and only until the hold clears.
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="x2")["state"], "restarted")
        self.assertEqual(self.agent.answer("roll-back", request_id="rb-1")["state"], "rolled-back")
        self.assertEqual(self.agent.answer("restart-service", "scheduler", request_id="x3")["state"], "restarted")

    def test_a_stopped_partly_migrated_database_may_retry_the_failed_release(self):
        self.fail_upgrade()
        self.agent.command("drain-and-stop", request_id="s1")
        self.assertEqual(self.agent.answer("deploy-now", request_id="d1")["state"], "deployed")

    def test_a_stopped_partly_migrated_database_may_go_forward_to_a_fixed_release(self):
        self.fail_upgrade()
        self.agent.command("drain-and-stop", request_id="s1")
        self.release("3.0.3", latest=24)
        self.assertEqual(self.agent.answer("deploy-now", request_id="d1")["state"], "deployed")
        self.assertFalse(any("pg_restore" in c for c in self.run.commands))

    def test_a_start_retried_without_stopping_is_not_pointing_back(self):
        self.fail_upgrade()
        self.agent.save_state({**self.agent.state(), "resume_failed": {"release": "3.0.1", "error": "x", "at": "2026"}})
        self.release("3.0.1")
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "resumed")
        self.assertIn("may have partly applied its migrations", report["scheduler"])
        self.assertNotIn("rolled_back", self.agent.state()["failed"])

    def test_a_refused_pointing_back_names_the_release_that_migrated(self):
        failed = {"release": "3.0.1", "partly_migrated": "3.0.2", "restore_point": {"kind": "operator"}}
        self.failing = "migrate"
        with self.assertRaises(agent_module.CommandFailed) as refused:
            self.agent.check_pointed_back(self.agent.state(), failed)
        self.assertIn("the failed deploy of 3.0.2", refused.exception.details["recovery"])

    def test_pointing_back_without_stopping_leaves_the_scheduler_for_roll_back(self):
        # Nothing could have been restored while Hot ran.
        self.fail_upgrade()
        self.release("3.0.1")
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "current")
        self.assertIn("may have partly applied its migrations", report["scheduler"], "and the report says why")
        self.assertIn("To start it, run roll-back", report["scheduler"], "and what starts it")
        self.assertNotIn("rolled_back", self.agent.state()["failed"])
        self.assertNotIn("up -d scheduler", self.run.compose_calls())
        self.assertIn("restored", self.agent.command("roll-back")["steps"])

    def test_a_release_pointer_with_nothing_in_it_is_not_pointing_back(self):
        self.restore_off_this_host()
        self.agent.command("drain-and-stop", request_id="s1")
        self.cloud.parameters["pointer"] = None
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "resumed")
        self.assertIn("may have partly applied its migrations", report["scheduler"])
        self.assertNotIn("rolled_back", self.agent.state()["failed"])

    def test_a_deploy_now_with_the_failed_release_still_named_retries_it(self):
        self.restore_off_this_host()
        self.agent.command("drain-and-stop", request_id="s1")
        self.assertEqual(self.agent.command("deploy-now", request_id="d1")["state"], "deployed")


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

    def test_a_backlog_is_read_only_one_batch_deep(self):
        # journalctl after an old cursor writes the whole backlog; only a
        # batch of it is read, and the command is stopped.
        backlog = [sys.executable, "-c", "for n in range(10**7): print(n)"]
        started = time.monotonic()
        lines = agent_module.Agent.read_lines(backlog, 5)
        self.assertEqual([line.strip() for line in lines], ["0", "1", "2", "3", "4"])
        self.assertLess(time.monotonic() - started, 5, "not read to the end")
        with self.assertRaises(subprocess.CalledProcessError) as failed:
            agent_module.Agent.read_lines([sys.executable, "-c", "print('partial'); raise SystemExit(3)"], 5)
        self.assertEqual(failed.exception.returncode, 3)

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
        gcp.instance_id, gcp.instance_zone = "1234567890", "us-central1-b"
        gcp.call = lambda method, url, payload=None, allow=(): sent.append(payload)
        entry = agent_module.journal_entry(journal_record(1, "oom", "", 3), agent_module.Redactor(), "host")
        gcp.ship_journal("hot-example", [entry])
        self.assertEqual(sent[0]["logName"], "projects/example-project/logs/hot-hot-example-host")
        self.assertTrue(sent[0]["partialSuccess"])
        self.assertEqual(sent[0]["entries"][0]["severity"], "ERROR")
        self.assertEqual(sent[0]["resource"]["labels"]["instance_id"], "1234567890")
        self.assertEqual(sent[0]["resource"]["labels"]["zone"], "us-central1-b")

    def test_entries_cloud_logging_refuses_are_not_sent_again(self):
        sent = []
        gcp = agent_module.Gcp("example-project", zone="us-central1-a")
        gcp.instance_id, gcp.instance_zone = "1234567890", "us-central1-a"
        partial = json.dumps({"error": {"code": 400, "details": [{
            "@type": "type.googleapis.com/google.logging.v2.WriteLogEntriesPartialErrors",
            "logEntryErrors": {"0": {"code": 3, "message": "timestamp too old"}}}]}}).encode()

        def call(method, url, payload=None, allow=()):
            sent.append(payload)
            raise agent_module.ApiError("/v2/entries:write failed: HTTP 400", 400, self.body)
        gcp.call = call
        entries = [agent_module.journal_entry(journal_record(n, "old"), agent_module.Redactor(), "host") for n in (1, 2)]
        self.body = partial
        gcp.ship_journal("hot-example", entries)  # the other entries were written
        self.assertEqual(len(sent), 1)
        self.body = json.dumps({"error": {"code": 403, "status": "PERMISSION_DENIED"}}).encode()
        with self.assertRaises(agent_module.ApiError, msg="a refused request is still a failure"):
            gcp.ship_journal("hot-example", entries)

    def test_every_entry_names_its_instance(self):
        self.agent._instance_name = "i-0123456789abcdef0"
        self.run.journal["host"] = [journal_record(1, "Out of memory: Killed process 4242 (hot)", "", 3)]
        self.agent.ship_journal()
        self.assertEqual({f["instance"] for f in self.shipped()}, {"i-0123456789abcdef0"})

    def test_cloudwatch_receives_events_in_time_order(self):
        calls = []
        aws = agent_module.Aws("us-east-1", "/hot/hot-example")
        aws.call = lambda service, target, payload: calls.append(payload) or {}
        # A clock stepped back: journal order is no longer time order.
        aws.ship_journal("hot-example", [{"at_ms": at, "fields": {"message": str(at)}} for at in (3000, 1000, 2000)])
        self.assertEqual([e["timestamp"] for e in calls[0]["logEvents"]], [1000, 2000, 3000])


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
        admin = (root / "prepare-postgres/POSTGRES_ADMIN_SERVICE").read_text()
        self.assertEqual(admin, (f"[hot]\nhost={self.HOST}\nport=5432\ndbname=postgres\nuser=hot_admin\n"
                                 "password=p@ss/word\nsslmode=verify-full\n"
                                 "sslrootcert=/run/secrets/POSTGRES_CA_CERT\n"))
        self.assertFalse((root / "prepare-postgres/POSTGRES_ADMIN_URI").exists())
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
        # Every file the bundle names is among those written.
        compose = render.render(self.config, manifest(), "/var/lib/hot/data", "/run/hot/secrets",
                                operator_secrets=agent_module.operator_routes(self.cloud.secrets[OPERATOR], self.config))
        for secret in compose["secrets"].values():
            role, key = Path(secret["file"]).parts[-2:]
            self.assertIn(key, files.get(role, {}), secret["file"])

    def test_every_secret_the_bundle_names_is_delivered(self):
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertEqual(undelivered_secrets(self.tmp.name), [])

    def test_an_administrator_login_a_service_file_cannot_hold_is_refused_without_its_text(self):
        self.cloud.secrets[self.ADMIN] = {"username": "hot_admin", "password": "hidden\nrest"}
        status = self.agent.reconcile()
        self.assertEqual(status["state"], "failed", status)
        self.assertIn("administrator login cannot be written as a libpq service file", status["error"])
        self.assertNotIn("hidden", status["error"])

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

    def test_a_host_with_no_data_root_yet_still_beats(self):
        # On GCP the disk waits for the first release, and with logs kept on
        # the host nothing else creates the directory.
        self.config["logging"] = {"destination": "local"}
        self.agent.data_root = Path(self.tmp.name) / "not-mounted-yet"
        for now in (self.NOW, self.NOW + datetime.timedelta(minutes=1)):
            self.agent.monitor(now=now)
            published = self.cloud.metrics[-1][1]
            self.assertEqual(published["AgentHeartbeat"][0], 1)
            self.assertNotIn("DataDiskUsedPercent", published)
        self.assertEqual(len(self.cloud.metrics), 2)

    def test_a_healthy_deployment(self):
        self.agent.reconcile()
        self.ps()
        self.cloud.snapshot = self.NOW - datetime.timedelta(hours=5)
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual({k: v[0] for k, v in metrics.items() if k != "DataDiskUsedPercent"}, {
            "AgentHeartbeat": 1, "UnhealthyServices": 0, "SchedulerRunning": 1, "DependenciesReachable": 1,
            "Ready": 1, "BackupAgeHours": 5, "CertificateDaysLeft": 93, "QueueBacklog": 0, "QueueDeadLetters": 0,
            "NewDeadLetters": 0, "JobsWaiting": 0, "JobWaitSeconds": 0, "TasksWaiting": 0, "TaskWaitSeconds": 0})
        self.assertTrue(0 <= metrics["DataDiskUsedPercent"][0] <= 100)

    def test_memory_in_use_counts_what_the_kernel_cannot_give_back(self):
        (Path(self.tmp.name) / "meminfo").write_text(
            "MemTotal:        8000000 kB\nMemFree:          500000 kB\nMemAvailable:    2000000 kB\n")
        self.assertEqual(self.agent.observe(now=self.NOW)["MemoryUsedPercent"], (75.0, "Percent"))
        (Path(self.tmp.name) / "meminfo").write_text("MemTotal: 8000000 kB\n")
        self.assertNotIn("MemoryUsedPercent", self.agent.observe(now=self.NOW))

    def test_failures_show_up_in_the_metrics(self):
        self.agent.reconcile()
        self.ps(scheduler=("restarting", ""), postgres=("running", "unhealthy"), worker=("exited", ""))
        metrics = self.agent.observe(probe=lambda host, path: 502, now=self.NOW)
        self.assertEqual(metrics["UnhealthyServices"][0], 3)
        self.assertEqual(metrics["SchedulerRunning"][0], 0)
        self.assertEqual(metrics["DependenciesReachable"][0], 0)
        self.assertEqual(metrics["Ready"][0], 0)

    def test_the_scheduler_host_measures_the_queue_every_minute(self):
        self.agent.reconcile()
        self.ps()
        self.run.queue = [(40, 2, 1)]
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual((metrics["QueueBacklog"][0], metrics["QueueDeadLetters"][0]), (42, 1))
        self.assertIn("exec -T scheduler /usr/bin/hot queue status --json", self.run.compose_calls())
        self.assertEqual(metrics["NewDeadLetters"][0], 0, "the first measurement has nothing to compare with")
        self.run.queue = [(0, 0, 4)]
        later = self.agent.observe(probe=lambda host, path: 200, now=self.NOW + datetime.timedelta(seconds=30))
        self.assertEqual(later["QueueBacklog"][0], 42, "not measured again within a minute")
        later = self.agent.observe(probe=lambda host, path: 200, now=self.NOW + datetime.timedelta(minutes=2))
        self.assertEqual((later["QueueBacklog"][0], later["NewDeadLetters"][0]), (0, 3))
        self.run.queue = [(0, 0, 0)]
        later = self.agent.observe(probe=lambda host, path: 200, now=self.NOW + datetime.timedelta(minutes=4))
        self.assertEqual(later["NewDeadLetters"][0], 0, "clearing dead letters is not new ones")
        self.ps(scheduler=("exited", ""))
        self.agent._queue_measured = None
        self.assertNotIn("QueueBacklog", self.agent.observe(probe=lambda host, path: 200, now=self.NOW))

    def test_a_question_to_a_container_that_hangs_skips_its_metrics(self):
        self.agent.reconcile()
        self.ps()
        recorder, asked = self.run, []
        self.run.task_status = {"time": self.NOW.timestamp(), "code": {"slots": 4, "busy": 1},
                                "containers": {"slots": 4, "busy": 0}, "memory_mb": 0, "memory_used_mb": 0}

        def hanging(command, **kwargs):
            if "queue" in command or command[-1] == render.TASK_STATUS:
                asked.append(kwargs.get("timeout"))
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual(asked, [agent_module.EXEC_SECONDS] * 2)
        self.assertEqual(metrics["AgentHeartbeat"][0], 1, "the other metrics still go out")
        self.assertEqual(metrics["UnhealthyServices"][0], 0)
        self.assertNotIn("QueueBacklog", metrics)
        self.assertNotIn("TaskSlotsUsedPercent", metrics)
        self.agent.run_command = recorder
        later = self.agent.observe(probe=lambda host, path: 200, now=self.NOW + datetime.timedelta(seconds=30))
        self.assertIn("QueueBacklog", later, "the next cycle asks again")

    def test_docker_that_no_longer_answers_silences_the_heartbeat(self):
        self.agent.reconcile()
        self.ps()
        recorder, asked = self.run, []

        def hanging(command, **kwargs):
            if "ps" in command:
                asked.append(kwargs.get("timeout"))
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        published = len(self.cloud.metrics)
        with mock.patch("sys.stderr") as stderr:
            self.agent.monitor()
        self.assertEqual(asked, [agent_module.EXEC_SECONDS])
        self.assertEqual(len(self.cloud.metrics), published, "no heartbeat, so host-down fires")
        written = "".join(str(c) for c in stderr.write.call_args_list)
        self.assertIn("monitoring_failed", written)
        self.assertIn("timed out after 120 seconds", written)

    def test_a_certificate_check_that_hangs_alarms(self):
        self.agent.reconcile()
        self.ps()
        recorder, asked = self.run, []

        def hanging(command, **kwargs):
            if command[0] == "openssl":
                asked.append(kwargs.get("timeout"))
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual(asked, [agent_module.EXEC_SECONDS])
        self.assertEqual(metrics["CertificateDaysLeft"][0], 0)

    def test_a_journal_read_that_hangs_is_stopped(self):
        with self.assertRaises(subprocess.CalledProcessError):
            agent_module.Agent.read_lines(["sleep", "30"], 10, timeout=0.05)

    def test_a_process_is_stopped_at_its_timeout(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            agent_module.Agent._run(["sleep", "30"], timeout=0.05)

    def test_jobs_and_tasks_are_measured_apart(self):
        self.agent.reconcile()
        self.ps()
        self.run.queue = [queue_status({"{hot:event}": (12, 3, 0, 40), "{hot:email}": (2, 0, 1, 95),
                                        "{hot:task}": (7, 4, 0, 300), "{hot:deploy}": (0, 0, 0, None)})]
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual({k: metrics[k][0] for k in ("QueueBacklog", "JobsWaiting", "JobWaitSeconds",
                                                     "TasksWaiting", "TaskWaitSeconds")},
                         {"QueueBacklog": 28, "JobsWaiting": 14, "JobWaitSeconds": 95,
                          "TasksWaiting": 7, "TaskWaitSeconds": 300})

    def test_a_release_without_json_queue_status_reports_totals(self):
        self.agent.reconcile()
        self.ps()
        self.run.text_queue_status = True
        self.run.queue = [(5, 1, 0)]
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual(metrics["QueueBacklog"][0], 6)
        self.assertNotIn("JobsWaiting", metrics)

    def test_the_task_worker_reports_its_busiest_resource(self):
        self.agent.reconcile()
        self.ps()
        now = self.NOW.timestamp()
        self.run.task_status = {"time": now - 10, "code": {"slots": 500, "busy": 50},
                                "containers": {"slots": 8, "busy": 6}, "memory_mb": 8192, "memory_used_mb": 4096}
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertEqual(metrics["TaskSlotsUsedPercent"], (75.0, "Percent"))
        self.run.task_status["time"] = now - 600
        stale = self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        self.assertNotIn("TaskSlotsUsedPercent", stale, "a stopped worker's file is not current")
        self.run.task_status = None
        self.assertNotIn("TaskSlotsUsedPercent", self.agent.observe(probe=lambda host, path: 200, now=self.NOW))

    def test_dead_letters_added_while_the_agent_restarted_are_new(self):
        self.agent.reconcile()
        self.ps()
        self.run.queue = [(0, 0, 10)]
        self.agent.observe(probe=lambda host, path: 200, now=self.NOW)
        # A new agent process starts with nothing in memory; one more job was
        # dead-lettered meanwhile.
        self.agent._queue_measured = None
        self.run.queue = [(0, 0, 11)]
        metrics = self.agent.observe(probe=lambda host, path: 200, now=self.NOW + datetime.timedelta(minutes=2))
        self.assertEqual(metrics["NewDeadLetters"][0], 1)

    def test_a_host_without_a_data_volume_reports_its_backup_as_unknown(self):
        del self.config["data_volume_id"]
        with mock.patch("sys.stderr"):
            metrics = self.agent.observe(now=self.NOW)
        self.assertNotIn("BackupAgeHours", metrics)
        self.assertEqual(metrics["AgentHeartbeat"][0], 1)

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
        # A regional group placed this VM in another zone than the installation's.
        gcp = agent_module.Gcp("example-project", zone="us-central1-a")
        gcp.call = lambda method, url, payload=None, allow=(): sent.append((url, payload))
        metadata = {"instance/id": b"1234567890", "instance/zone": b"projects/123/zones/us-central1-c"}
        with mock.patch.object(agent_module, "http", lambda method, url, **kwargs: (
                200, metadata[url.split("/v1/", 1)[1]])):
            gcp.put_metrics("hot-example", {"AgentHeartbeat": (1, "Count")})
        url, payload = sent[0]
        self.assertTrue(url.endswith("/projects/example-project/timeSeries"))
        series = payload["timeSeries"][0]
        self.assertEqual(series["metric"], {"type": "custom.googleapis.com/hot/hot-example/agent_heartbeat"})
        self.assertEqual(series["resource"]["labels"], {"project_id": "example-project", "zone": "us-central1-c",
                                                        "instance_id": "1234567890"})

    def test_a_backup_sees_a_grown_disk_and_not_what_a_kill_left(self):
        self.agent.reconcile()
        left = Path(self.tmp.name) / "data/postgres/backup.dump"
        left.parent.mkdir(parents=True, exist_ok=True)
        left.write_text("part of a dump")
        room = agent_module.STAGING_RESERVE_BYTES

        def usage(path):
            return mock.Mock(free=room - (left.stat().st_size if left.exists() else 0))
        run, grown = self.agent.run_command, []

        def dump(command, **kwargs):
            if command[0].endswith("hot-data-disk"):
                grown.append(command)
            if "pg_dump" in command:
                left.write_text("dump")
            return run(command, **kwargs)
        self.agent.run_command = dump
        with mock.patch.object(agent_module.shutil, "disk_usage", usage):
            report = self.agent.answer("backup", request_id="b1")
        self.assertEqual(report["state"], "backed-up", report)
        self.assertEqual(len(grown), 1)
        self.assertNotIn("--allow-format", grown[0])

    def test_a_database_off_the_host_is_not_dumped(self):
        self.agent.reconcile()
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.agent.save_state({**self.agent.state(), "data_modes": self.agent.configured_modes()})
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

    def test_a_command_that_fails_partway_is_answered(self):
        self.agent.reconcile()
        recorder = self.run

        def failing(command, **kwargs):
            if "pg_dump" in command:
                raise subprocess.CalledProcessError(1, command, stderr="pg_dump: error: No space left on device")
            return recorder(command, **kwargs)
        self.agent.run_command = failing
        report = self.agent.answer("backup", request_id="b1")
        self.assertEqual((report["state"], report["command"], report["request_id"]), ("failed", "backup", "b1"))
        self.assertIn("exec -T postgres pg_dump", report["error"])
        self.assertIn("No space left on device", report["error"])
        self.assertEqual(list((Path(self.tmp.name) / "backups").glob("manual-*")), [], "no partial backup is left")
        self.assertEqual(self.agent.answer("sh -c id", request_id="x")["state"], "rejected")


class CommandsCutShort(UpgradeHarness):
    """restart-service and drain-and-stop killed at each point, or failing there,
    with only the state left to say what they did. The next cycle, a deploy-now, a
    reboot, or a later deploy that fails must act on that, and report Hot as it
    is. `self.running` is what Docker would have up."""

    HOT = {"scheduler", "worker", "task-worker", "app", "api", "ingress"}
    DATA = {"postgres", "valkey"}
    OLD_ENOUGH = "2026-10-29T12:00:00+00:00"

    def setUp(self):
        super().setUp()
        self.running = self.HOT | self.DATA
        self.kill_at, self.changes = None, 0
        self.cloud.statuses.clear()
        outer = self.agent.run_command

        def docker(command, **kwargs):
            args = command[command.index("compose") + 3:] if "compose" in command else [""]
            if args[0] not in ("stop", "up"):
                return outer(command, **kwargs)
            services = {arg for arg in args[1:] if not arg.startswith("-") and not arg.isdigit()}
            # A stop takes effect as it goes, and a recreate takes the old container away.
            going = services if args[0] == "stop" or "--force-recreate" in args else set()
            point, self.changes = self.changes, self.changes + 1
            moment = self.kill_at[1] if self.kill_at and self.kill_at[0] == point else None
            if moment == "before":
                raise KeyboardInterrupt
            if moment == "during":
                self.running = self.running - going
                raise KeyboardInterrupt
            try:
                result = outer(command, **kwargs)
            except subprocess.CalledProcessError:
                self.running = self.running - going
                raise
            self.running = self.running - services if args[0] == "stop" else self.running | services
            if moment == "after":
                raise KeyboardInterrupt
            return result
        self.agent.run_command = docker

    def start_over(self):
        self.failing = None
        self.running = self.HOT | self.DATA
        self.cloud.parameters["pointer"] = "3.0.1"
        self.cloud.statuses.clear()
        self.run.commands.clear()
        self.agent.save_state({key: value for key, value in self.agent.state().items()
                               if key not in ("stopped", "stopping", "failed", "restarting", "resume_failed")})

    def killed(self, operation, service="", point=0, moment="after", request_id="c1"):
        """Run a command that dies at its `point`th Compose call that changes
        something, which got as far as `moment`. False if it never got there."""
        self.kill_at, self.changes = (point, moment), 0
        try:
            self.agent.answer(operation, service, request_id=request_id)
        except KeyboardInterrupt:
            return True
        finally:
            self.kill_at = None
        return False

    def after_each_kill(self, operation, service, then):
        """`then`, after `operation` is killed at each point of each Compose call."""
        point = 0
        while True:
            for moment in ("before", "during", "after"):
                if not self.killed(operation, service, point, moment):
                    self.assertGreater(point, 0, "no call was killed")
                    self.start_over()
                    return
                with self.subTest(operation=operation, service=service, point=point, moment=moment):
                    then()
                self.start_over()
            point += 1

    def after_each_killed_restart(self, then, services=None):
        def checked():
            self.assertTrue(self.agent.state()["restarting"], "it says what it has yet to bring up")
            then()
        for service in sorted(services or self.HOT):
            self.after_each_kill("restart-service", service, checked)

    def test_the_next_cycle_finishes_a_restart_cut_short(self):
        def then():
            self.agent.run_once()
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertNotIn("restarting", self.agent.state())
            self.assertEqual([status["request_id"] for status in self.cloud.statuses if status["state"] == "restarted"],
                             ["c1"], "whoever asked gets an answer")
        self.after_each_killed_restart(then)

    def test_deploy_now_finishes_a_restart_cut_short(self):
        def then():
            report = self.agent.answer("deploy-now", request_id="d1")
            self.assertEqual((report["state"], report["request_id"]), ("current", "d1"), report)
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertNotIn("restarting", report)
        self.after_each_killed_restart(then)

    def test_a_reboot_starts_what_a_restart_left_down(self):
        def then():
            shutil.rmtree(Path(self.tmp.name) / "secrets")
            (Path(self.tmp.name) / "resumed").unlink()
            self.running = set()
            self.run.commands.clear()
            self.agent.run_once()
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertNotIn("restarting", self.agent.state())
            self.assertEqual(self.cloud.statuses[0]["state"], "resumed")
            self.assertEqual([call for call in self.run.compose_calls()
                              if call.startswith("up -d --force-recreate") and "--no-deps" not in call], [],
                             "started once, by the boot's start")
        self.after_each_killed_restart(then)

    def test_a_later_failed_deploy_says_a_release_keeps_running_only_once_it_does(self):
        def then():
            self.release("3.2.0", minimum_source="3.1.0")
            report = self.agent.reconcile()
            self.assertEqual(report["state"], "failed", report)
            self.assertIn("3.0.1 keeps running", report["recovery"])
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertNotIn("restarting", self.agent.state())
        self.after_each_killed_restart(then)

    def test_a_later_failed_deploy_says_so_when_what_a_restart_left_down_does_not_start(self):
        def then():
            self.release("3.2.0", minimum_source="3.1.0")
            self.failing = "--force-recreate"
            report = self.agent.reconcile()
            self.assertEqual(report["state"], "failed", report)
            self.assertNotIn("keeps running", report["recovery"])
            self.assertIn("did not start again", report["recovery"])
            state = self.agent.state()
            self.assertIn("restarting", state)
            self.assertEqual(state["resume_failed"]["release"], "3.0.1")
            self.failing = None
            self.cloud.parameters["pointer"] = "3.0.1"
            state["resume_failed"]["at"] = self.OLD_ENOUGH
            self.agent.save_state(state)
            self.agent.run_once()
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertFalse({"restarting", "resume_failed"} & self.agent.state().keys())
        with mock.patch("sys.stderr"):
            self.after_each_killed_restart(then, ("scheduler", "app"))

    def test_a_failed_deploy_ahead_of_a_boots_start_does_not_say_a_release_keeps_running(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        (Path(self.tmp.name) / "resumed").unlink()
        self.running = set()
        self.release("3.2.0", minimum_source="3.1.0")
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("keeps running", report["recovery"])
        self.assertIn("3.0.1 has yet to start after this boot: the next cycle starts it", report["recovery"])
        self.assertEqual(self.agent.state()["restarting"], {"scheduler": "c1"}, "that start has this to do")
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertFalse({"restarting", "resume_failed"} & self.agent.state().keys())

    def test_a_deploy_cut_short_takes_over_from_a_restart_cut_short(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        self.release("3.0.2")
        with mock.patch.object(self.agent, "pull_images", side_effect=KeyboardInterrupt), \
                self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.run.commands.clear()
        self.agent.finish_restarts()
        self.assertEqual(self.run.compose_calls(), [], "that deploy has this to do")
        self.assertEqual(self.agent.state()["restarting"], {"scheduler": "c1"})
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())

    def test_a_deploy_cut_short_and_then_given_up_returns_to_what_runs(self):
        self.assertTrue(self.killed("restart-service", "worker", 0, "during"))
        self.assertNotIn("worker", self.running)
        self.release("3.0.2")
        with mock.patch.object(self.agent, "pull_images", side_effect=KeyboardInterrupt), \
                self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.cloud.parameters["pointer"] = "3.0.1"
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("previous-worker-restarted", report["steps"])
        self.assertIn("3.0.1 keeps running", report["recovery"])
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())

    def test_a_scheduler_held_since_stays_stopped(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        report = self.agent.reconcile()
        self.assertIn("Migrations may be partly applied", report["recovery"])
        self.assertNotIn("keeps running", report["recovery"])
        self.failing = None
        self.cloud.statuses.clear()
        self.agent.run_once()
        self.assertEqual(self.running, (self.HOT | self.DATA) - {"scheduler"}, "only roll-back or a fixed release starts it")
        self.assertNotIn("restarting", self.agent.state())
        self.assertFalse([status for status in self.cloud.statuses if status["state"] == "restarted"])

    def test_another_service_starts_under_a_scheduler_held_since(self):
        self.assertTrue(self.killed("restart-service", "worker", 0, "during"))
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.assertNotIn("worker", self.running)
        self.agent.run_once()
        self.assertEqual(self.running, (self.HOT | self.DATA) - {"scheduler"})
        self.assertNotIn("restarting", self.agent.state())

    def test_a_roll_back_starts_what_a_restart_left_down(self):
        self.assertTrue(self.killed("restart-service", "worker", 0, "during"))
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.assertEqual(self.agent.command("roll-back", request_id="rb")["state"], "rolled-back")
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())

    def test_a_good_deploy_starts_what_a_restart_left_down(self):
        self.assertTrue(self.killed("restart-service", "worker", 0, "during"))
        self.release("3.0.2")
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())

    def test_a_stop_after_a_restart_cut_short_keeps_it_down(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s1")["state"], "stopped")
        state = self.agent.state()
        self.assertTrue(state["stopped"])
        self.assertNotIn("restarting", state)
        self.run.commands.clear()
        self.agent.run_once()
        self.assertEqual(self.running, self.DATA)
        self.assertEqual(self.run.compose_calls(), [])

    def test_a_stop_that_came_some_other_way_takes_back_a_restart_cut_short(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        self.agent.save_state({**self.agent.state(), "stopped": True})
        self.run.commands.clear()
        self.agent.finish_restarts()
        self.assertEqual(self.run.compose_calls(), [])
        self.assertNotIn("restarting", self.agent.state())

    def test_each_restart_cut_short_is_finished(self):
        self.assertTrue(self.killed("restart-service", "scheduler"))
        self.assertTrue(self.killed("restart-service", "task-worker", 0, "during", request_id="c2"))
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="c3")["state"], "restarted")
        self.assertEqual(self.agent.state()["restarting"], {"scheduler": "c1", "task-worker": "c2"})
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())
        self.assertEqual({(status["service"], status["request_id"]) for status in self.cloud.statuses
                          if status["state"] == "restarted"}, {("scheduler", "c1"), ("task-worker", "c2"), ("worker", "c3")})

    def test_a_restart_killed_in_its_hotbox_copy_stopped_nothing(self):
        outer = self.agent.run_command

        def killed(command, **kwargs):
            if command[:2] == ["docker", "cp"]:
                raise KeyboardInterrupt
            return outer(command, **kwargs)
        self.agent.run_command = killed
        with self.assertRaises(KeyboardInterrupt):
            self.agent.answer("restart-service", "task-worker", request_id="c1")
        self.agent.run_command = outer
        self.assertNotIn("restarting", self.agent.state())
        self.assertEqual(self.running, self.HOT | self.DATA)

    def test_a_restart_that_fails_is_answered_and_tried_until_it_works(self):
        self.failing = "--force-recreate"
        report = self.agent.answer("restart-service", "scheduler", request_id="c1")
        self.assertEqual((report["state"], report["command"]), ("failed", "restart-service"), report)
        self.assertIn("--force-recreate scheduler", report["error"])
        self.assertIn("The next cycle starts it again", report["recovery"])
        self.assertEqual(report["restarting"], {"scheduler": "c1"})
        self.assertNotIn("scheduler", self.running)
        with mock.patch("sys.stderr") as stderr:
            self.agent.run_once()
        self.assertIn("finish_restarts", "".join(str(call) for call in stderr.write.call_args_list))
        self.assertEqual(self.agent.state()["restarting"], {"scheduler": "c1"})
        self.failing = None
        self.agent.run_once()
        self.assertIn("scheduler", self.running)
        self.assertNotIn("restarting", self.agent.state())

    def test_an_ingress_that_fails_does_not_recreate_the_app_again(self):
        self.failing = "ingress"
        report = self.agent.answer("restart-service", "app", request_id="c1")
        self.assertEqual(report["state"], "failed", report)
        self.assertEqual(report["restarting"], {"ingress": "c1"})
        self.assertIn("ingress may be down. The next cycle starts it again", report["recovery"])
        self.run.commands.clear()
        with mock.patch("sys.stderr"):
            self.agent.run_once()
        calls = self.run.compose_calls()
        self.assertIn("up -d --force-recreate ingress", calls)
        self.assertNotIn("up -d --force-recreate app", calls)
        self.failing = None
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("restarting", self.agent.state())

    def test_the_next_cycle_finishes_a_drain_cut_short(self):
        def then():
            state = self.agent.state()
            self.assertTrue(state["stopped"], "said before anything stops")
            self.assertEqual(state["stopping"], {"request_id": "c1"}, "and that it has not finished")
            self.assertEqual(self.agent.answer("status", request_id="q1")["stopping"], {"request_id": "c1"},
                             "status does not read as a stop that finished")
            self.run.commands.clear()
            self.agent.run_once()
            self.assertEqual(self.running, self.DATA)
            calls = self.run.compose_calls()
            self.assertTrue(calls)
            self.assertEqual([call for call in calls if not call.startswith("stop ")], [], "it starts nothing")
            state = self.agent.state()
            self.assertTrue(state["stopped"])
            self.assertNotIn("stopping", state)
            self.assertEqual([status["request_id"] for status in self.cloud.statuses if status["state"] == "stopped"],
                             ["c1"], "whoever asked gets an answer")
            self.run.commands.clear()
            self.agent.run_once()
            self.assertEqual(self.run.compose_calls(), [], "that is the end of it")
            self.assertEqual(self.running, self.DATA)
        self.after_each_kill("drain-and-stop", "", then)

    def test_a_drain_cut_short_stays_stopped_across_a_reboot(self):
        def then():
            shutil.rmtree(Path(self.tmp.name) / "secrets")
            (Path(self.tmp.name) / "resumed").unlink()
            self.running = set()
            self.agent.run_once()
            self.assertEqual(self.running, set())
            self.assertTrue(self.agent.state()["stopped"])
            self.assertNotIn("stopping", self.agent.state())
            self.assertTrue(self.agent.boot_settled())
            self.agent.deliver_secrets()  # for the next round; nothing here starts on them
        self.after_each_kill("drain-and-stop", "", then)

    def test_a_later_failed_deploy_stops_what_a_drain_cut_short_left_running(self):
        def then():
            self.release("3.2.0", minimum_source="3.1.0")
            report = self.agent.reconcile()
            self.assertEqual(report["state"], "failed", report)
            self.assertIn("Hot stays stopped, as it was before this deploy", report["recovery"])
            self.assertNotIn("keeps running", report["recovery"])
            self.assertEqual(self.running, self.DATA)
            self.assertTrue(self.agent.state()["stopped"])
            self.cloud.statuses.clear()
            self.agent.run_once()
            self.assertEqual([status["request_id"] for status in self.cloud.statuses if status["state"] == "stopped"],
                             ["c1"], "whoever asked gets an answer")
            self.assertNotIn("stopping", self.agent.state())
        self.after_each_kill("drain-and-stop", "", then)

    def test_deploy_now_starts_everything_after_a_drain_cut_short(self):
        def then():
            report = self.agent.answer("deploy-now", request_id="d1")
            self.assertEqual((report["state"], report["request_id"]), ("resumed", "d1"), report)
            self.assertEqual(self.running, self.HOT | self.DATA)
            self.assertFalse({"stopped", "stopping"} & self.agent.state().keys())
            self.agent.run_once()
            self.assertEqual(self.running, self.HOT | self.DATA, "and nothing stops it again")
        self.after_each_kill("drain-and-stop", "", then)

    def test_a_drain_sent_again_stops_the_rest(self):
        def then():
            report = self.agent.answer("drain-and-stop", request_id="s2")
            self.assertEqual(report["state"], "stopped")
            self.assertNotIn("stopping", report)
            self.assertEqual(self.running, self.DATA)
        self.after_each_kill("drain-and-stop", "", then)

    def test_a_restart_waits_for_a_drain_cut_short_to_finish(self):
        def then():
            up, state = set(self.running), self.agent.state()
            self.run.commands.clear()
            report = self.agent.answer("restart-service", "worker", request_id="r1")
            self.assertEqual(report["state"], "rejected", report)
            self.assertIn("drain-and-stop has yet to finish", report["error"])
            self.assertEqual(self.run.compose_calls(), [], "before anything changes")
            self.assertEqual((self.running, self.agent.state()), (up, state))
            self.cloud.statuses.clear()
            self.agent.run_once()
            self.assertEqual(self.running, self.DATA)
            self.assertEqual([(status["state"], status["request_id"]) for status in self.cloud.statuses
                              if status["state"] in ("stopped", "restarted")], [("stopped", "c1")])
            self.assertEqual(self.agent.answer("restart-service", "worker", request_id="r2")["state"], "restarted",
                             "once it has")
            self.agent.run_once()
            self.assertEqual(self.running, self.DATA | {"worker"})
        self.after_each_kill("drain-and-stop", "", then)

    def test_a_restart_waits_for_a_drain_that_failed_to_finish(self):
        self.failing = "task-worker"
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s1")["state"], "failed")
        self.failing = None
        report = self.agent.answer("restart-service", "app", request_id="r1")
        self.assertEqual(report["state"], "rejected", report)
        self.assertIn("deploy-now", report["error"])
        self.agent.run_once()
        self.assertEqual(self.running, self.DATA)
        self.assertNotIn("stopping", self.agent.state())

    def test_a_deploy_cut_short_takes_over_from_a_drain_cut_short(self):
        self.assertTrue(self.killed("drain-and-stop"))
        self.release("3.0.2")
        with mock.patch.object(self.agent, "pull_images", side_effect=KeyboardInterrupt), \
                self.assertRaises(KeyboardInterrupt):
            self.agent.reconcile()
        self.run.commands.clear()
        self.agent.finish_stop()
        self.assertEqual(self.run.compose_calls(), [], "that deploy has this to do")
        self.assertEqual(self.agent.state()["stopping"], {"request_id": "c1"})
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "deployed", report)
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertFalse({"stopped", "stopping"} & self.agent.state().keys())

    def test_a_good_deploy_ends_a_drain_cut_short(self):
        self.assertTrue(self.killed("drain-and-stop"))
        self.release("3.0.2")
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        self.assertFalse({"stopped", "stopping"} & self.agent.state().keys())
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)

    def migrated_then_drain_cut_short(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.assertTrue(self.killed("drain-and-stop"))

    def test_a_roll_back_ends_a_drain_cut_short(self):
        self.migrated_then_drain_cut_short()
        self.assertEqual(self.agent.command("roll-back", request_id="rb")["state"], "rolled-back")
        self.assertFalse({"stopped", "stopping"} & self.agent.state().keys())
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)

    def test_a_failed_roll_back_leaves_a_drain_cut_short_to_finish(self):
        self.migrated_then_drain_cut_short()
        self.failing = "--remove-orphans"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb")["state"], "failed")
        self.failing = None
        self.assertEqual(self.agent.state()["stopping"], {"request_id": "c1"})
        self.running |= {"worker", "api"}  # what a start that failed partway left up
        self.agent.run_once()
        self.assertEqual(self.running, self.DATA)
        self.assertNotIn("stopping", self.agent.state())
        self.assertTrue(self.agent.state()["stopped"])

    def test_a_drain_that_fails_is_tried_until_it_works(self):
        self.failing = "task-worker"
        report = self.agent.answer("drain-and-stop", request_id="s1")
        self.assertEqual(report["stopping"], {"request_id": "s1"})
        self.assertIn("The next cycle stops the rest, and goes on until that works", report["recovery"])
        self.cloud.statuses.clear()
        with mock.patch("sys.stderr") as stderr:
            self.agent.run_once()
        self.assertIn("finish_stop", "".join(str(call) for call in stderr.write.call_args_list))
        self.assertEqual(self.agent.state()["stopping"], {"request_id": "s1"})
        self.assertEqual([status for status in self.cloud.statuses if status["state"] == "stopped"], [])
        self.failing = None
        self.agent.run_once()
        self.assertEqual(self.running, self.DATA)
        self.assertNotIn("stopping", self.agent.state())
        self.assertEqual([status["request_id"] for status in self.cloud.statuses if status["state"] == "stopped"], ["s1"])

    def test_a_drain_that_fails_partway_counts_as_stopped_and_says_so(self):
        self.failing = "task-worker"
        report = self.agent.answer("drain-and-stop", request_id="s1")
        self.assertEqual((report["state"], report["command"]), ("failed", "drain-and-stop"), report)
        self.assertIn("stop --timeout 120 worker task-worker", report["error"])
        self.assertIn("Hot counts as stopped", report["recovery"])
        self.assertIs(report["stopped"], True)
        self.assertNotIn("scheduler", self.running)
        self.assertTrue({"app", "api", "ingress"} <= self.running, "it did not get that far")
        self.failing = None
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s2")["state"], "stopped")
        self.assertEqual(self.running, self.DATA)

    def test_a_full_disk_does_not_keep_a_restart_or_a_stop_from_running(self):
        full = OSError(28, "No space left on device")
        with mock.patch.object(self.agent, "save_state", side_effect=full), mock.patch("sys.stderr") as stderr:
            report = self.agent.answer("restart-service", "scheduler", request_id="c1")
            self.assertEqual(report["state"], "restarted", report)
            self.assertIn("restart_not_recorded", "".join(str(call) for call in stderr.write.call_args_list))
            report = self.agent.answer("drain-and-stop", request_id="s1")
        self.assertEqual((report["state"], report["command"]), ("failed", "drain-and-stop"), report)
        self.assertIn("No space left on device", report["error"])
        self.assertEqual(self.running, self.DATA, "Hot stopped all the same")

    def disk_full_after(self, saves):
        """A disk that takes `saves` more writes of the state, then is full."""
        save, left = agent_module.Agent.save_state, [saves]

        def saving(agent, state):
            if left[0] <= 0:
                raise OSError(28, "No space left on device")
            left[0] -= 1
            save(agent, state)
        return mock.patch.object(agent_module.Agent, "save_state", saving)

    def recreates(self):
        return [call for call in self.run.compose_calls() if call.startswith("up -d --force-recreate")]

    def restarted_reports(self):
        return [status["request_id"] for status in self.cloud.statuses if status["state"] == "restarted"]

    def test_a_word_that_cannot_be_taken_back_recreates_nothing_more_in_the_loop(self):
        with self.disk_full_after(1), mock.patch("sys.stderr"):
            report = self.agent.answer("restart-service", "worker", request_id="c1")
            self.assertEqual(report["state"], "restarted", report)
            self.assertEqual(report["restarting"], {"worker": "c1"})
            self.run.commands.clear()
            for _ in range(4):
                self.agent.run_once()
            self.assertEqual(self.recreates(), [], "it did not go down, so nothing does that again")
        self.assertEqual(self.restarted_reports(), ["c1"])
        self.assertEqual(self.agent.state()["restarting"], {"worker": "c1"})
        self.agent.run_once()
        self.assertEqual(self.recreates(), [])
        self.assertNotIn("restarting", self.agent.state(), "the word goes once the disk takes it")
        self.assertIn("worker", self.running)

    def test_a_word_a_command_process_could_not_take_back_costs_one_more_recreate(self):
        process = agent_module.Agent(self.config, self.cloud, run=self.agent.run_command, sleep=lambda _: None,
                                     roots=self.roots, chown=lambda *args: None)
        with self.disk_full_after(1), mock.patch("sys.stderr"):
            self.assertEqual(process.answer("restart-service", "worker", request_id="c1")["state"], "restarted")
            self.run.commands.clear()
            for _ in range(4):
                self.agent.run_once()
            self.assertEqual(self.recreates(), ["up -d --force-recreate worker"])
        self.assertEqual(self.restarted_reports(), ["c1", "c1"])
        self.agent.run_once()
        self.assertEqual(self.recreates(), ["up -d --force-recreate worker"])
        self.assertNotIn("restarting", self.agent.state())

    def test_a_restart_tried_again_on_a_full_disk_recreates_once(self):
        self.failing = "--force-recreate"
        self.assertEqual(self.agent.answer("restart-service", "worker", request_id="c1")["state"], "failed")
        self.failing = None
        self.run.commands.clear()
        with self.disk_full_after(0), mock.patch("sys.stderr"):
            for _ in range(4):
                self.agent.run_once()
        self.assertEqual(self.recreates(), ["up -d --force-recreate worker"])
        self.assertEqual(self.restarted_reports(), ["c1"])
        self.assertIn("worker", self.running)

    def test_a_word_that_cannot_be_taken_back_recreates_neither_the_app_nor_the_ingress(self):
        for saves in (1, 2):
            with self.subTest(saves=saves), self.disk_full_after(saves), mock.patch("sys.stderr"):
                self.assertEqual(self.agent.answer("restart-service", "app", request_id="c1")["state"], "restarted")
                self.assertIn(set(self.agent.state()["restarting"]), ({"app"}, {"ingress"}))
                self.run.commands.clear()
                for _ in range(3):
                    self.agent.run_once()
                self.assertEqual(self.recreates(), [])
            self.agent.run_once()
            self.assertEqual(self.recreates(), [])
            self.assertNotIn("restarting", self.agent.state())

    def test_a_later_request_is_not_taken_for_the_one_that_finished(self):
        with self.disk_full_after(1), mock.patch("sys.stderr"):
            self.agent.answer("restart-service", "worker", request_id="c1")
        self.running -= {"worker"}
        self.agent.save_state({**self.agent.state(), "restarting": {"worker": "c2"}})  # a command process, killed
        self.cloud.statuses.clear()
        self.agent.run_once()
        self.assertIn("worker", self.running)
        self.assertEqual(self.restarted_reports(), ["c2"])
        self.assertNotIn("restarting", self.agent.state())

    def test_a_failed_restart_on_a_stopped_installation_says_nothing_starts_it(self):
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s1")["state"], "stopped")
        self.failing = "--force-recreate"
        report = self.agent.answer("restart-service", "worker", request_id="c1")
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("next cycle", report["recovery"])
        self.assertIn("worker may be down, and Hot is stopped, so nothing will start it again", report["recovery"])
        self.assertIn("restart-service worker", report["recovery"])
        self.assertIn("deploy-now", report["recovery"])
        self.failing = None
        self.run.commands.clear()
        self.agent.run_once()
        self.assertEqual(self.run.compose_calls(), [], "as it says")
        self.assertNotIn("worker", self.running)

    def test_a_failed_restart_that_could_not_say_so_says_nothing_starts_it(self):
        self.failing = "--force-recreate"
        with self.disk_full_after(0), mock.patch("sys.stderr"):
            report = self.agent.answer("restart-service", "worker", request_id="c1")
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("next cycle", report["recovery"])
        self.assertIn("worker may be down, and nothing will start it again", report["recovery"])
        self.assertIn("restart-service worker", report["recovery"])
        self.failing = None
        self.run.commands.clear()
        self.agent.run_once()
        self.assertEqual(self.recreates(), [], "as it says")
        self.assertNotIn("worker", self.running)

    def test_a_hotbox_that_cannot_be_put_in_place_says_nothing_starts_it(self):
        with mock.patch.object(self.agent, "install_hotbox", side_effect=OSError(5, "Input/output error")):
            report = self.agent.answer("restart-service", "task-worker", request_id="c1")
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("next cycle", report["recovery"])
        self.assertIn("nothing will start it again", report["recovery"])
        self.assertNotIn("restarting", self.agent.state())
        self.assertEqual(self.running, self.HOT | self.DATA)

    def test_a_failed_stop_that_could_not_say_so_says_nothing_stops_the_rest(self):
        self.failing = "task-worker"
        with self.disk_full_after(0):
            report = self.agent.answer("drain-and-stop", request_id="s1")
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("counts as stopped", report["recovery"])
        self.assertNotIn("next cycle", report["recovery"])
        self.assertIn("nothing will stop the rest", report["recovery"])
        self.assertIn("run drain-and-stop again", report["recovery"])
        self.assertFalse({"stopped", "stopping"} & self.agent.state().keys())

    def test_a_stop_it_could_not_say_had_finished_is_said_once_the_disk_takes_it(self):
        with self.disk_full_after(1), mock.patch("sys.stderr"):
            report = self.agent.answer("drain-and-stop", request_id="s1")
            self.assertEqual(report["state"], "failed", report)
            self.assertIn("No space left on device", report["error"])
            self.assertEqual(report["stopping"], {"request_id": "s1"})
            self.cloud.statuses.clear()
            self.agent.run_once()
            self.assertEqual([status for status in self.cloud.statuses if status["state"] == "stopped"], [])
        self.agent.run_once()
        self.assertNotIn("stopping", self.agent.state())
        self.assertEqual([status["request_id"] for status in self.cloud.statuses if status["state"] == "stopped"], ["s1"])

    def roll_back_that_did_not_start_hot(self):
        """A migration failed, and a roll-back restored the database and saw it
        pass its checks, then failed to start Hot: only the data services run."""
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = "--remove-orphans"
        report = self.agent.answer("roll-back", request_id="rb")
        self.assertEqual((report["state"], report["steps"]), ("failed", ["stopped", "restored", "checked"]), report)
        self.assertNotIn("stopped", self.agent.state())
        self.failing = None
        self.assertEqual(self.running, self.DATA)
        self.release("3.2.0", minimum_source="3.1.0")

    def test_a_later_failed_deploy_starts_what_a_failed_roll_back_left_down(self):
        self.roll_back_that_did_not_start_hot()
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("previous-release-restarted", report["steps"])
        self.assertIn("3.0.1 keeps running", report["recovery"])
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertEqual(report.get("resume_failed"), None)
        self.assertEqual(self.agent.scheduler_held(self.agent.state()), {})

    def test_a_later_failed_deploy_says_so_when_what_a_failed_roll_back_left_down_does_not_start(self):
        self.roll_back_that_did_not_start_hot()
        self.failing = "--remove-orphans"
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertNotIn("keeps running", report["recovery"])
        self.assertIn("3.0.1 did not start again", report["recovery"])
        state = self.agent.state()
        self.assertEqual(state["resume_failed"]["release"], "3.0.1")
        self.failing = None
        state["resume_failed"]["at"] = self.OLD_ENOUGH
        self.agent.save_state(state)
        self.agent.run_once()
        self.assertEqual(self.running, self.HOT | self.DATA)
        self.assertNotIn("resume_failed", self.agent.state())

    def test_a_later_failed_deploy_keeps_a_stopped_installation_stopped_after_a_failed_roll_back(self):
        self.release("3.0.2", latest=23)
        self.failing = "migrate"
        self.agent.reconcile()
        self.failing = None
        self.assertEqual(self.agent.answer("drain-and-stop", request_id="s1")["state"], "stopped")
        self.failing = "--remove-orphans"
        self.assertEqual(self.agent.answer("roll-back", request_id="rb")["state"], "failed")
        self.failing = None
        self.release("3.2.0", minimum_source="3.1.0")
        report = self.agent.reconcile()
        self.assertEqual(report["state"], "failed", report)
        self.assertIn("Hot stays stopped", report["recovery"])
        self.assertEqual(self.running, self.DATA)
        self.assertTrue(self.agent.state()["stopped"])


class CommandMode(unittest.TestCase):
    def test_a_command_takes_its_send_time_before_it_waits_for_the_lock(self):
        order = []

        @agent_module.contextlib.contextmanager
        def locked():
            order.append("locked")
            yield

        def utc_now():
            order.append("timed")
            return "2026-11-01T12:00:00+00:00"
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(agent_module, "Agent") as agent, \
                mock.patch.object(agent_module, "cloud_for"), mock.patch.object(agent_module, "locked", locked), \
                mock.patch.object(agent_module, "utc_now", utc_now), \
                mock.patch.object(agent_module, "INSTALLATION", Path(tmp) / "installation.json"):
            agent_module.INSTALLATION.write_text("{}")
            agent.return_value.answer.return_value = {"state": "failed"}
            self.assertEqual(agent_module.main(["command", "deploy-now", "--request-id", "r1"]), 1)
            agent.return_value.answer.assert_called_once_with("deploy-now", "", "r1", "2026-11-01T12:00:00+00:00", "")
        self.assertEqual(order, ["timed", "locked"])

    def test_a_command_passes_on_when_it_expires(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(agent_module, "Agent") as agent, \
                mock.patch.object(agent_module, "cloud_for"), \
                mock.patch.object(agent_module, "locked", agent_module.contextlib.nullcontext), \
                mock.patch.object(agent_module, "INSTALLATION", Path(tmp) / "installation.json"):
            agent_module.INSTALLATION.write_text("{}")
            agent.return_value.answer.return_value = {"state": "status"}
            self.assertEqual(agent_module.main(["command", "status", "--expires", "1793750400"]), 0)
            self.assertEqual(agent.return_value.answer.call_args.args[-1], "1793750400")

    def test_a_command_reads_the_configuration_once_it_holds_the_lock(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(agent_module, "Agent") as agent, \
                mock.patch.object(agent_module, "cloud_for") as cloud_for, \
                mock.patch.object(agent_module, "INSTALLATION", Path(tmp) / "installation.json"):
            agent_module.INSTALLATION.write_text('{"name": "before the wait"}')

            @agent_module.contextlib.contextmanager
            def locked():
                agent_module.INSTALLATION.write_text('{"name": "after the wait"}')
                yield
            agent.return_value.answer.return_value = {"state": "status"}
            with mock.patch.object(agent_module, "locked", locked):
                self.assertEqual(agent_module.main(["command", "status"]), 0)
        cloud_for.assert_called_once_with({"name": "after the wait"})
        agent.assert_called_once_with({"name": "after the wait"}, cloud_for.return_value)

    def test_the_loop_reads_the_configuration_when_it_starts(self):
        class Stopped(Exception):
            pass
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(agent_module, "Agent") as agent, \
                mock.patch.object(agent_module, "cloud_for") as cloud_for, \
                mock.patch.object(agent_module, "locked", agent_module.contextlib.nullcontext), \
                mock.patch.object(agent_module.time, "sleep", side_effect=Stopped), \
                mock.patch.object(agent_module, "INSTALLATION", Path(tmp) / "installation.json"):
            agent_module.INSTALLATION.write_text('{"name": "hot-example"}')
            with self.assertRaises(Stopped):
                agent_module.main(["run"])
        agent.assert_called_once_with({"name": "hot-example"}, cloud_for.return_value)
        agent.return_value.run_once.assert_called_once_with()


class CommandQueue:
    """A Pub/Sub pull subscription: one message per pull."""

    def __init__(self, cloud, messages, events):
        self.messages, self.events = list(messages), events
        cloud.receive_commands, cloud.ack = self.receive_commands, self.ack

    def receive_commands(self):
        return [self.messages.pop(0)] if self.messages else []

    def ack(self, ack_id):
        self.events.append(("ack", ack_id))


class RunLoop(AgentHarness):
    def setUp(self):
        super().setUp()
        self.events = []
        put_status = self.cloud.put_status
        self.cloud.put_status = lambda reference, status: (
            self.events.append(("report", status.get("request_id"))), put_status(reference, status))

    def test_a_failing_step_does_not_stop_the_cycle(self):
        self.agent.reconcile()

        def throttled(reference):
            raise agent_module.AgentError("AmazonSSM.GetParameter failed: ThrottlingException")
        self.cloud.get_parameter = throttled
        self.run.journal["agent"] = [journal_record(1, "hello")]
        CommandQueue(self.cloud, [("a1", {"operation": "status", "request_id": "s1"})], self.events)
        with mock.patch("sys.stderr") as stderr:
            self.agent.run_once()
        self.assertIn("ThrottlingException", "".join(str(c) for c in stderr.write.call_args_list))
        self.assertEqual(self.cloud.metrics[-1][1]["AgentHeartbeat"], (1, "Count"), "the heartbeat still goes out")
        self.assertTrue(self.cloud.journal, "and the journal")
        self.assertEqual(self.events[-2:], [("ack", "a1"), ("report", "s1")], "and commands still run")

    def test_commands_are_acknowledged_before_they_run_and_run_once(self):
        self.agent.reconcile()
        self.events.clear()
        CommandQueue(self.cloud, [("a1", {"operation": "status", "request_id": "r1"}),
                                  ("a2", None),  # not JSON
                                  ("a3", {"operation": "status", "request_id": "r1"}),  # delivered again
                                  ("a4", {"operation": "roll-back", "request_id": "r2"})], self.events)
        self.agent.take_commands()
        self.assertEqual(self.events, [("ack", "a1"), ("report", "r1"), ("ack", "a2"), ("ack", "a3"),
                                       ("ack", "a4"), ("report", "r2")])
        self.assertEqual(self.cloud.statuses[-1]["state"], "rejected")

    def test_a_command_past_its_expiry_is_reported_and_not_run(self):
        self.agent.reconcile()
        self.events.clear()
        self.run.commands.clear()
        now = int(time.time())
        CommandQueue(self.cloud, [("a1", {"operation": "drain-and-stop", "request_id": "r1", "expires": now - 5}),
                                  ("a2", {"operation": "drain-and-stop", "request_id": "r1", "expires": now - 5}),  # again
                                  ("a3", {"operation": "status", "request_id": "r2", "expires": now + 600}),
                                  ("a4", {"operation": "status", "request_id": "r3"}),  # from an older helper
                                  ("a5", {"operation": "status", "request_id": "r4", "expires": "soon"})], self.events)
        self.agent.take_commands()
        self.assertEqual([event for event in self.events if event[0] == "report"],
                         [("report", "r1"), ("report", "r2"), ("report", "r3"), ("report", "r4")])
        self.assertEqual([event for event in self.events if event[0] == "ack"],
                         [("ack", "a1"), ("ack", "a2"), ("ack", "a3"), ("ack", "a4"), ("ack", "a5")])
        expired = next(s for s in self.cloud.statuses if s.get("request_id") == "r1")
        self.assertEqual((expired["state"], expired["command"]), ("expired", "drain-and-stop"))
        self.assertIn("stopped waiting for this command at ", expired["error"])
        self.assertIn("expired", agent_module.FAILED_STATES)
        self.assertNotIn("stopped", self.agent.state())
        self.assertEqual(self.run.compose_calls(), [], "nothing stopped")
        self.assertEqual(self.agent.state().get("release"), "3.0.1")

    def test_a_command_that_waited_for_the_lock_past_its_expiry_is_reported_and_not_run(self):
        # Systems Manager starts the agent's command at once, and it waits for
        # the lock behind whatever runs; the helper meanwhile told its sender
        # there was no answer.
        self.agent.reconcile()
        self.run.commands.clear()
        order = []

        @agent_module.contextlib.contextmanager
        def locked():
            order.append("locked")
            yield
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(agent_module, "Agent", return_value=self.agent), \
                mock.patch.object(agent_module, "cloud_for"), mock.patch.object(agent_module, "locked", locked), \
                mock.patch.object(agent_module, "INSTALLATION", Path(tmp) / "installation.json"):
            agent_module.INSTALLATION.write_text("{}")
            code = agent_module.main(["command", "drain-and-stop", "--request-id", "r1",
                                      "--expires", str(int(time.time()) - 5)])
        self.assertEqual(code, 1)
        self.assertEqual(order, ["locked"])
        report = self.cloud.statuses[-1]
        self.assertEqual((report["state"], report["command"], report["request_id"]), ("expired", "drain-and-stop", "r1"))
        self.assertIn("stopped waiting for this command at ", report["error"])
        self.assertEqual(self.run.compose_calls(), [], "nothing stopped")
        self.assertNotIn("stopped", self.agent.state())

    def test_only_a_time_in_the_past_expires_a_command(self):
        self.agent.reconcile()
        now = int(time.time())
        for expires in (None, "", "none", "0", 0, -1, True, "soon", float("nan"), now + 600, str(now + 600)):
            with self.subTest(expires=expires):
                self.assertEqual(self.agent.answer("status", expires=expires)["state"], "status")
        for expires in (now - 5, str(now - 5), float(now - 5)):
            with self.subTest(expires=expires):
                self.assertEqual(self.agent.answer("status", expires=expires)["state"], "expired")

    def test_a_full_disk_does_not_lose_a_command(self):
        # The disk-pressure alert fired, and the operator asks what is wrong.
        self.agent.reconcile()
        self.events.clear()
        CommandQueue(self.cloud, [("a1", {"operation": "status", "request_id": "r1"}),
                                  ("a2", {"operation": "status", "request_id": "r2"})], self.events)
        with mock.patch.object(agent_module.os, "fsync", side_effect=OSError(28, "No space left on device")), \
                mock.patch("sys.stderr") as stderr:
            self.agent.take_commands()
        self.assertEqual(self.events, [("ack", "a1"), ("report", "r1"), ("ack", "a2"), ("report", "r2")])
        self.assertIn("command_not_recorded", "".join(str(c) for c in stderr.write.call_args_list))

    def test_a_secret_that_only_looks_like_a_url_does_not_stop_the_loop(self):
        self.agent.reconcile()
        (Path(self.tmp.name) / "secrets/worker/HOT_EMAIL_SMTP_PASSWORD").write_text("abc://[def-password")

        def throttled(reference):
            raise agent_module.AgentError("AmazonSSM.GetParameter failed: ThrottlingException")
        self.cloud.get_parameter = throttled
        CommandQueue(self.cloud, [("a1", {"operation": "roll-back", "request_id": "r1"})], self.events)
        self.agent.run_once()
        self.assertEqual(self.events[-2:], [("ack", "a1"), ("report", "r1")])

    def test_a_deploy_now_from_the_subscription_knows_when_it_was_sent(self):
        self.manifests["3.0.2"] = manifest("3.0.2")
        self.cloud.parameters["pointer"] = "3.0.2"
        self.agent.reconcile()
        self.events.clear()
        self.agent.fetch_release = mock.Mock(side_effect=agent_module.AgentError("the manifest is not signed"))
        self.cloud.parameters["pointer"] = "3.0.3"
        sent = agent_module.utc_now()
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        CommandQueue(self.cloud, [("a1", {"operation": "deploy-now", "request_id": "r1", "sent": sent})], self.events)
        self.agent.fetch_release.reset_mock()
        self.agent.take_commands()
        self.agent.fetch_release.assert_not_called()
        self.assertEqual((self.cloud.statuses[-1]["state"], self.cloud.statuses[-1]["request_id"]), ("failed", "r1"))

    def test_a_pull_takes_one_message_and_survives_one_that_is_not_json(self):
        sent = []
        gcp = agent_module.Gcp("acme", "projects/acme/subscriptions/hot-example-commands")
        published = "2026-11-01T12:00:00.123456789Z"
        messages = [{"ackId": "a1", "message": {"data": base64.b64encode(b"deploy-now").decode()}},
                    {"ackId": "a2", "message": {"data": base64.b64encode(b'{"operation": "status", "sent": "1999"}').decode(),
                                                "publishTime": published}}]
        gcp.call = lambda method, url, payload=None, allow=(): sent.append(payload) or {
            "receivedMessages": [messages.pop(0)]}
        self.assertEqual(gcp.receive_commands(), [("a1", None)])
        self.assertEqual(gcp.receive_commands(), [("a2", {"operation": "status", "sent": published})],
                         "the time is Pub/Sub's, not what the message says")
        self.assertEqual(sent, [{"maxMessages": 1}] * 2, "a long command holds no other message's deadline")

    def test_the_heartbeat_goes_on_while_a_long_step_runs(self):
        with self.agent.heartbeat(interval=0.01):
            time.sleep(0.1)
        beats = [m for _, m in self.cloud.metrics if m == {"AgentHeartbeat": (1, "Count")}]
        self.assertGreater(len(beats), 1)
        count = len(self.cloud.metrics)
        time.sleep(0.05)
        self.assertEqual(len(self.cloud.metrics), count, "it stops with the step")
        self.agent.monitor()
        self.assertNotIn("AgentHeartbeat", self.cloud.metrics[-1][1], "no second point seconds after a beat")

    def test_only_the_steps_that_may_take_long_beat_while_they_run(self):
        beat = self.agent.heartbeat
        self.agent.heartbeat = lambda: beat(interval=0.01)
        steps = ("refresh_configuration", "mount_data", "resume", "reconcile", "refresh_guest_network", "monitor",
                 "ship_journal", "take_commands")
        for blocking in steps:
            with self.subTest(step=blocking):
                called = []
                for name in steps:
                    setattr(self.agent, name, lambda name=name: called.append(name))
                setattr(self.agent, blocking, lambda: (called.append(blocking), time.sleep(0.1)))
                self.cloud.metrics.clear()
                self.agent.run_once()
                self.assertEqual(called, list(steps))
                beats = [m for _, m in self.cloud.metrics if m == {"AgentHeartbeat": (1, "Count")}]
                if blocking in ("resume", "reconcile", "take_commands"):
                    self.assertGreater(len(beats), 1, "working, not down")
                else:
                    self.assertEqual(beats, [], "a step that hangs here goes silent, and host-down fires")

    def test_a_step_that_hangs_for_hours_stops_the_heartbeat_too(self):
        with mock.patch("sys.stderr") as stderr, self.agent.heartbeat(interval=0.01, limit=0.03):
            time.sleep(0.1)
            count = len(self.cloud.metrics)
            self.assertGreater(count, 0)
            time.sleep(0.05)
            self.assertEqual(len(self.cloud.metrics), count, "silent past the limit, while the step still runs")
        self.assertIn("heartbeat_stopped", "".join(str(c) for c in stderr.write.call_args_list))


class Synthetic(AgentHarness):
    NOW = datetime.datetime(2026, 10, 29, 12, 0, tzinfo=datetime.timezone.utc)

    def setUp(self):
        super().setUp()
        self.config["monitoring"] = {"synthetic_event": "hot-check:ping"}
        self.cloud.secrets[OPERATOR]["HOT_SYNTHETIC_API_KEY"] = "hot_synthetic_key"
        self.agent.background = lambda function: function()
        self.requests = []
        self.runs = ["running", "succeeded"]

    def request(self, host, method, path, body=None, headers=None):
        self.requests.append((host, method, path, json.loads(body) if body else None, headers))
        if method == "POST":
            return 201, json.dumps({"data": {"event_id": "e1"}}).encode()
        status = self.runs.pop(0) if len(self.runs) > 1 else self.runs[0]
        return 200, json.dumps({"data": [{"run_id": "r1", "status": status}]}).encode()

    def clock(self):
        self.ticks = getattr(self, "ticks", 0) + 2
        return self.ticks

    def test_an_event_runs_through_the_api_and_a_worker(self):
        ok, seconds = self.agent.synthetic_check(request=self.request, clock=self.clock)
        self.assertTrue(ok)
        host, method, path, body, headers = self.requests[0]
        self.assertEqual((host, method, path), ("api.hot.internal", "POST", "/v1/events"))
        self.assertEqual(body["event_type"], "hot-check:ping")
        self.assertEqual(headers["Authorization"], "Bearer hot_synthetic_key")
        self.assertEqual([r[2] for r in self.requests[1:]], ["/v1/events/e1/runs"] * 2)

    def test_a_failed_or_slow_run_fails_the_check(self):
        self.runs = ["failed"]
        self.assertFalse(self.agent.synthetic_check(request=self.request, clock=self.clock)[0])
        self.runs = ["running"]
        ok, seconds = self.agent.synthetic_check(request=self.request, clock=self.clock)
        self.assertFalse(ok)
        self.assertGreaterEqual(seconds, agent_module.SYNTHETIC_TIMEOUT)

    def test_the_api_key_is_for_the_agent_alone(self):
        self.assertEqual(self.agent.reconcile()["state"], "deployed", "the key is an accepted operator entry")
        mounted = [path.name for path in (Path(self.tmp.name) / "secrets").rglob("*") if path.is_file()]
        self.assertNotIn("HOT_SYNTHETIC_API_KEY", mounted, "no service receives it")

    def test_the_result_is_published_and_rechecked_every_few_minutes(self):
        self.agent.synthetic_check = lambda: (True, 3.25)
        self.assertEqual(self.agent.synthetic_metrics(self.NOW),
                         {"SyntheticOk": (1, "Count"), "SyntheticSeconds": (3.2, "Seconds")})
        self.agent.synthetic_check = lambda: (False, 60)
        soon = self.agent.synthetic_metrics(self.NOW + datetime.timedelta(minutes=1))
        self.assertEqual(soon["SyntheticOk"][0], 1, "not run again within five minutes")
        later = self.agent.synthetic_metrics(self.NOW + datetime.timedelta(minutes=6))
        self.assertEqual(later["SyntheticOk"][0], 0)

    def test_a_missing_key_fails_the_check(self):
        del self.cloud.secrets[OPERATOR]["HOT_SYNTHETIC_API_KEY"]
        self.assertEqual(self.agent.synthetic_metrics(self.NOW)["SyntheticOk"][0], 0)

    def test_each_result_is_published_once(self):
        self.agent.reconcile()
        self.run.ps = "\n".join(json.dumps({"Service": name, "State": "running", "Health": ""})
                                for name in (*agent_module.HOT_SERVICES, "ingress", "postgres", "valkey"))
        self.agent.probe = lambda host, path: 200

        def points():
            return [metrics["SyntheticOk"][0] for _, metrics in self.cloud.metrics if "SyntheticOk" in metrics]
        self.agent.synthetic_check = lambda: (False, 60)
        for minute in range(5):  # within one check's interval
            self.agent.monitor(now=self.NOW + datetime.timedelta(minutes=minute))
        self.assertEqual(points(), [0], "one failed run is one failed point, not five minutes of them")
        self.agent.synthetic_check = lambda: (True, 2)
        self.agent.monitor(now=self.NOW + datetime.timedelta(minutes=6))
        self.agent.monitor(now=self.NOW + datetime.timedelta(minutes=7))
        self.assertEqual(points(), [0, 1])
        # The same result again is a new point all the same.
        self.agent.synthetic_check = lambda: (True, 2)
        self.agent.monitor(now=self.NOW + datetime.timedelta(minutes=12))
        self.assertEqual(points(), [0, 1, 1])

    def test_only_api_hosts_with_an_event_run_it(self):
        self.config["roles"] = ["worker"]
        self.assertEqual(self.agent.synthetic_metrics(self.NOW), {})
        self.config["roles"] = None
        self.config["monitoring"] = {}
        self.assertEqual(self.agent.synthetic_metrics(self.NOW), {})


class ConfigurationRefresh(AgentHarness):
    def test_a_configuration_changed_in_place_is_used_and_redeploys(self):
        self.agent.reconcile()
        changed = json.loads(json.dumps(self.config))
        changed["settings"] = {"HOT_APP_SIGNUP": "open"}
        self.cloud.installation = lambda reference: changed
        self.assertTrue(self.agent.refresh_configuration())
        self.assertEqual(json.loads((Path(self.tmp.name) / "etc/installation.json").read_text()), changed)
        self.assertEqual(self.agent.config["settings"], {"HOT_APP_SIGNUP": "open"})
        self.assertEqual(self.agent.reconcile()["state"], "deployed", "a new configuration digest redeploys")
        self.assertFalse(self.agent.refresh_configuration(), "unchanged: nothing to do")

    def test_clouds_without_one_keep_the_boot_configuration(self):
        self.assertFalse(self.agent.refresh_configuration())

    def test_aws_reads_it_from_the_installation_parameter(self):
        aws = agent_module.Aws("us-east-1")
        with mock.patch.object(agent_module.Aws, "get_parameter", return_value='{"name": "hot-example"}') as read:
            self.assertEqual(aws.installation("/hot/hot-example/installation"), {"name": "hot-example"})
        self.assertEqual(read.call_args.args[0], "/hot/hot-example/installation")
        self.assertIsNone(aws.installation(""), "a host from before the parameter keeps its boot configuration")

    def test_gcp_reads_it_from_the_instance_metadata(self):
        encoded = base64.b64encode(json.dumps({"name": "hot-example"}).encode())
        with mock.patch.object(agent_module, "http", return_value=(200, encoded)) as fetched:
            self.assertEqual(agent_module.Gcp("acme").installation(), {"name": "hot-example"})
        self.assertTrue(fetched.call_args.args[1].endswith("/instance/attributes/hot-installation"))
        with mock.patch.object(agent_module, "http", return_value=(404, b"")):
            self.assertIsNone(agent_module.Gcp("acme").installation())


class CommandBehindTheLoop(UpgradeHarness):
    """On AWS a command is a process of its own, which may wait for the lock
    behind the loop's cycle. The cycle may take up a new configuration, and the
    command must then act on that one, not on the one from before its wait. Nor
    may it act on the file when the loop has yet to wake and read the stack's."""

    S3 = {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"}
    SKIP, STUCK = StorageMoves.SKIP, StorageMoves.STUCK
    worker_with_stuck_objects = StorageMoves.worker_with_stuck_objects
    move_that_failed = StorageMoves.move_that_failed

    def installation_file(self):
        return Path(self.tmp.name) / "etc/installation.json"

    def run_own_process(self, installation, *command, locked=agent_module.contextlib.nullcontext):
        """Run `hot-agent command` with the file as it was deployed, and the
        stack's parameter reading as `installation` says."""
        self.installation_file().parent.mkdir(parents=True, exist_ok=True)
        self.installation_file().write_text(json.dumps(self.config))
        self.cloud.installation = installation
        harness = self

        class OwnProcess(agent_module.Agent):
            def __init__(self, config, cloud):
                super().__init__(config, harness.cloud, run=harness.agent.run_command, sleep=lambda _: None,
                                 roots=harness.roots, chown=lambda *args: None)
                self.wait_ready = lambda: None
        with mock.patch.object(agent_module, "Agent", OwnProcess), mock.patch.object(agent_module, "cloud_for"), \
                mock.patch.object(agent_module, "locked", locked), \
                mock.patch.object(agent_module, "INSTALLATION", self.installation_file()):
            return agent_module.main(["command", *command])

    def run_behind_the_loop(self, changes, *command):
        """While the command waits, the stack's parameter changes, and the loop,
        ahead of it at the lock, refreshes the file and deploys that."""
        changed = {**json.loads(json.dumps(self.config)), **changes}

        @agent_module.contextlib.contextmanager
        def locked():
            self.assertTrue(self.agent.refresh_configuration())
            self.assertEqual(self.agent.reconcile()["state"], "deployed")
            self.run.commands.clear()
            yield
        return self.run_own_process(lambda reference: changed, *command, locked=locked)

    def run_ahead_of_the_loop(self, parameter, *command):
        """`tofu apply && selfhost.py command`, with the loop asleep: the stack's
        parameter is new, and the file is not. The failure it retries is older."""
        state = self.agent.state()
        state["failed"]["at"] = "2026-10-29T12:00:00+00:00"
        self.agent.save_state(state)
        return self.run_own_process(lambda reference: parameter, *command)

    def test_a_count_applied_just_before_deploy_now_is_the_one_it_goes_by(self):
        self.move_that_failed(self.STUCK)
        applied = {**json.loads(json.dumps(self.config)), "settings": {self.SKIP: "3"}}
        self.jobs.clear()
        code = self.run_ahead_of_the_loop(applied, "deploy-now", "--request-id", "d1")
        report = self.cloud.statuses[-1]
        self.assertEqual((code, report["state"]), (0, "deployed"), report)
        self.assertEqual(self.jobs, [("file", set(self.STUCK))], "the first deploy-now uses the setting")
        self.assertIn("3 object(s) stayed behind", report["warning"])
        self.assertEqual(json.loads(self.installation_file().read_text()), applied)
        # The loop catches up, and has nothing to deploy.
        self.assertTrue(self.agent.refresh_configuration())
        self.assertIsNone(self.agent.reconcile())

    def test_storage_put_back_just_before_deploy_now_does_not_run_the_move(self):
        installed = self.config["storage"]
        objects = {"file": self.STUCK}
        self.config["storage"] = dict(installed, file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        put_back = {**json.loads(json.dumps(self.config)), "storage": installed}
        objects["file"] = []  # the failure has cleared, so the move would finish
        self.jobs.clear()
        code = self.run_ahead_of_the_loop(put_back, "deploy-now", "--request-id", "d1")
        report = self.cloud.statuses[-1]
        self.assertEqual((code, report["state"]), (0, "resumed"), report)
        self.assertEqual(self.jobs, [], "the move does not run")
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "local")
        self.assertTrue(self.agent.refresh_configuration())
        self.assertIsNone(self.agent.reconcile())

    def test_storage_put_back_just_before_a_pulled_deploy_now_does_not_run_the_move(self):
        # GCP: the cycle refreshed before the apply rewrote the metadata, and
        # its take_commands pulls the deploy-now sent right after the apply.
        installed = self.config["storage"]
        objects = {"file": self.STUCK}
        self.config["storage"] = dict(installed, file=self.S3)
        self.worker_with_stuck_objects(objects)
        self.assertEqual(self.agent.reconcile()["state"], "failed")
        self.assertFalse(self.agent.refresh_configuration(), "the cycle's refresh came before the apply")
        put_back = {**json.loads(json.dumps(self.config)), "storage": installed}
        self.cloud.installation = lambda reference: put_back
        objects["file"] = []
        self.jobs.clear()
        events = []
        CommandQueue(self.cloud, [("a1", {"operation": "deploy-now", "request_id": "d1"})], events)
        self.agent.take_commands()
        report = self.cloud.statuses[-1]
        self.assertEqual((report["state"], report["request_id"]), ("resumed", "d1"), report)
        self.assertEqual(self.jobs, [], "the move does not run")
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "local")
        self.assertEqual(json.loads(self.installation_file().read_text()), put_back)
        self.assertFalse(self.agent.refresh_configuration())
        self.assertIsNone(self.agent.reconcile())

    def test_metadata_that_cannot_be_read_leaves_a_pulled_command_on_the_file(self):
        for trouble in (agent_module.AgentError("metadata unreachable"), KeyError("secret_references")):
            with self.subTest(trouble=trouble), mock.patch("sys.stderr") as stderr:
                self.cloud.installation = mock.Mock(side_effect=trouble)
                CommandQueue(self.cloud, [("a1", {"operation": "status", "request_id": f"s-{trouble}"})], [])
                self.agent.take_commands()
                self.assertEqual(self.cloud.statuses[-1]["request_id"], f"s-{trouble}", "still answered")
                self.assertEqual(self.agent.config["storage"], self.config["storage"])
        self.assertIn("configuration_not_refreshed", "".join(str(c) for c in stderr.write.call_args_list))

    def test_a_parameter_that_cannot_be_read_leaves_the_command_on_the_file(self):
        for trouble in (agent_module.AgentError("AccessDenied"), OSError("unreachable"), ValueError("not JSON"),
                        KeyError("anything")):
            with self.subTest(trouble=trouble), mock.patch("sys.stderr"):
                code = self.run_own_process(mock.Mock(side_effect=trouble), "status", "--request-id", "s1")
                self.assertEqual((code, self.cloud.statuses[-1]["request_id"]), (0, "s1"), "still answered")
                self.assertEqual(json.loads(self.installation_file().read_text()), self.config)

    def test_a_status_reads_the_stacks_configuration_and_deploys_nothing(self):
        applied = {**json.loads(json.dumps(self.config)), "settings": {"HOT_LOG_LEVEL_FILTER": "warn"}}
        self.run.commands.clear()
        code = self.run_own_process(lambda reference: applied, "status", "--request-id", "s1")
        self.assertEqual((code, self.cloud.statuses[-1]["state"]), (0, "status"))
        self.assertEqual(json.loads(self.installation_file().read_text()), applied)
        self.assertEqual(self.run.compose_calls(), [])

    def worker_env(self):
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        return compose["services"]["worker"]["environment"]

    def test_deploy_now_does_not_bring_back_the_settings_from_before_its_wait(self):
        code = self.run_behind_the_loop({"settings": {"HOT_LOG_LEVEL_FILTER": "warn"}},
                                        "deploy-now", "--request-id", "d1")
        report = self.cloud.statuses[-1]
        self.assertEqual((code, report["state"], report["request_id"]), (0, "current", "d1"))
        self.assertEqual(self.run.compose_calls(), [], "no second restart")
        self.assertEqual(self.worker_env()["HOT_LOG_LEVEL_FILTER"], "warn")
        self.assertIsNone(self.agent.reconcile(), "the loop has nothing to deploy again")

    def test_deploy_now_does_not_undo_a_move_the_loop_finished_during_its_wait(self):
        storage = dict(self.config["storage"], file=self.S3)
        code = self.run_behind_the_loop({"storage": storage}, "deploy-now", "--request-id", "d1")
        report = self.cloud.statuses[-1]
        self.assertEqual((code, report["state"], report["request_id"]), (0, "current", "d1"))
        self.assertNotIn("failed", self.agent.state(), "no failure is left on record")
        self.assertEqual(self.worker_env()["HOT_FILE_STORAGE_TYPE"], "s3")


class Resume(AgentHarness):
    def booted(self):
        """What a reboot leaves: the deployed release, and an empty tmpfs."""
        self.agent.reconcile()
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        (Path(self.tmp.name) / "resumed").unlink()
        self.run.commands.clear()

    def test_a_reboot_delivers_secrets_and_starts_the_release_again(self):
        self.booted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertTrue(any((Path(self.tmp.name) / "secrets").iterdir()))
        calls = self.run.compose_calls()
        self.assertIn("up -d --wait postgres valkey", calls)
        self.assertEqual(calls[-1], "up -d scheduler", "the scheduler starts last")
        self.assertIsNone(self.agent.resume(), "nothing to do once secrets are back")

    def answer_a_backup(self):
        """A backup on these fakes: the dump lands, and the disk has room."""
        run = self.agent.run_command

        def dumping(command, **kwargs):
            if "pg_dump" in command:
                dump = Path(self.tmp.name) / "data/postgres/backup.dump"
                dump.parent.mkdir(parents=True, exist_ok=True)
                dump.write_text("dump")
            return run(command, **kwargs)
        self.agent.run_command = dumping
        room = mock.Mock(free=agent_module.STAGING_RESERVE_BYTES)
        try:
            with mock.patch.object(agent_module.shutil, "disk_usage", lambda path: room):
                return self.agent.answer("backup", request_id="b1")
        finally:
            self.agent.run_command = run

    def test_a_command_that_wins_the_lock_at_boot_leaves_the_start_to_resume(self):
        # On AWS a command is its own process, and a command sent during a reboot
        # may take the lock before the loop does. It delivers secrets for itself.
        for operation, answer in (("restart-service", lambda: self.agent.answer("restart-service", "worker", "r1")),
                                  ("backup", self.answer_a_backup)):
            with self.subTest(operation=operation):
                self.booted()
                self.assertIn(answer()["state"], ("restarted", "backed-up"))
                self.assertTrue(any((Path(self.tmp.name) / "secrets").iterdir()))
                self.assertFalse(self.agent.boot_settled())
                self.run.commands.clear()
                self.assertEqual(self.agent.resume()["state"], "resumed")
                calls = self.run.compose_calls()
                self.assertIn("up -d --wait postgres valkey", calls)
                self.assertIn("up -d --remove-orphans app api worker task-worker", calls)
                self.assertEqual(calls[-1], "up -d scheduler")
                self.assertIsNone(self.agent.resume(), "and only once")

    def test_a_delivery_does_not_replace_the_mark_that_a_boot_started_hot(self):
        self.agent.reconcile()
        self.assertTrue(self.agent.boot_settled())
        self.agent.deliver_secrets()
        self.run.commands.clear()
        self.assertTrue(self.agent.boot_settled())
        self.assertIsNone(self.agent.resume())
        self.assertEqual(self.run.commands, [])

    def test_a_second_command_before_resume_still_finds_the_local_data_down(self):
        self.booted()
        self.assertEqual(self.agent.answer("restart-service", "worker", "r1")["state"], "restarted")
        self.run.commands.clear()
        self.assertEqual(self.answer_a_backup()["state"], "backed-up")
        calls = self.run.compose_calls()
        self.assertLess(calls.index("up -d --wait --no-recreate postgres valkey"),
                        next(i for i, c in enumerate(calls) if "pg_dump" in c))

    def test_deploy_now_that_wins_the_lock_at_boot_starts_hot(self):
        self.booted()
        report = self.agent.answer("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["request_id"]), ("resumed", "d1"), report)
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertEqual(self.agent.answer("deploy-now", request_id="d2")["state"], "current")

    def test_a_stopped_installation_stays_stopped_until_deploy_now(self):
        self.agent.reconcile()
        self.agent.command("drain-and-stop", request_id="s1")
        shutil.rmtree(Path(self.tmp.name) / "secrets")
        self.assertIsNone(self.agent.resume())
        self.run.commands.clear()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["request_id"]), ("resumed", "d1"))
        self.assertIn("up -d scheduler", self.run.compose_calls())
        self.assertNotIn("stopped", self.agent.state())

    def test_a_failed_resume_is_reported(self):
        self.booted()
        def missing(reference):
            raise agent_module.AgentError("secretsmanager GetSecretValue failed: AccessDeniedException")
        self.cloud.get_secret = missing
        self.assertEqual(self.agent.resume()["state"], "resume-failed")

    def test_a_replaced_vm_signs_in_to_a_private_mirror_before_starting(self):
        self.config["image_registry"] = "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot"
        self.cloud.registry_password = lambda host: "token"
        self.booted()
        self.assertEqual(self.agent.resume()["state"], "resumed")
        commands = [" ".join(c) for c in self.run.commands]
        login = next(i for i, c in enumerate(commands) if c.startswith("docker login"))
        self.assertLess(login, next(i for i, c in enumerate(commands) if " up -d " in c))

    def slow_start(self):
        """A boot whose first start does not become ready in time."""
        self.booted()
        checks = []

        def ready():
            checks.append(1)
            if len(checks) == 1:
                raise agent_module.AgentError("The app and API did not become ready")
        self.agent.wait_ready = ready
        report = self.agent.resume()
        self.assertEqual(report["state"], "resume-failed")
        self.assertNotIn("up -d scheduler", self.run.compose_calls())
        self.assertFalse(self.agent.boot_settled(), "a start that failed is not over")
        self.run.commands.clear()

    def test_a_failed_resume_is_tried_again(self):
        self.slow_start()
        self.assertIsNone(self.agent.resume(), "not every cycle")
        self.assertEqual(self.run.commands, [])
        state = self.agent.state()
        state["resume_failed"]["at"] = "2026-10-29T12:00:00+00:00"  # RESUME_RETRY_SECONDS ago, or more
        self.agent.save_state(state)
        self.assertEqual(self.agent.resume()["state"], "resumed")
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertNotIn("resume_failed", self.agent.state())
        self.assertIsNone(self.agent.resume())

    def test_deploy_now_starts_a_release_whose_resume_failed(self):
        self.slow_start()
        report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["request_id"]), ("resumed", "d1"))
        self.assertEqual(self.run.compose_calls()[-1], "up -d scheduler")
        self.assertEqual(self.agent.command("deploy-now", request_id="d2")["state"], "current")


class AlertTests(AgentHarness):
    def test_a_test_alert_fires_then_resolves(self):
        report = self.agent.command("test-alert", request_id="t1")
        self.assertEqual(report["state"], "alert-sent")
        self.assertEqual(self.cloud.metrics[-1], ("hot-example", {"AlertTest": (1, "Count")}))
        sent = agent_module.parse_time((Path(self.tmp.name) / "releases/alert-test").read_text())
        soon = self.agent.observe(now=sent + datetime.timedelta(seconds=30))
        self.assertNotIn("AlertTest", soon, "the alarm sees the test first")
        later = self.agent.observe(now=sent + datetime.timedelta(minutes=5))
        self.assertEqual(later["AlertTest"], (0, "Count"), "then a zero resolves it")
        self.assertNotIn("AlertTest", self.agent.observe(now=sent + datetime.timedelta(hours=1)))


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

    def test_the_hotbox_in_use_is_told_from_the_fallback_the_host_image_baked(self):
        baked = "registry.example.com/hot@sha256:" + "f" * 64
        (Path(self.tmp.name) / "host/versions.json").write_text(json.dumps({"hotbox_from": baked}))
        installed = Path(self.tmp.name) / "kata/hotbox"
        self.assertEqual(self.agent.command("diagnostics")["diagnostics"]["versions"]["hotbox"],
                         {"in_use": "installed", "installed_sha256": hashlib.sha256(installed.read_bytes()).hexdigest(),
                          "fallback_from": baked})
        installed.unlink()
        self.assertEqual(self.agent.command("diagnostics")["diagnostics"]["versions"]["hotbox"],
                         {"in_use": "fallback", "fallback_from": baked})

    def test_the_environment_is_reported_as_the_services_see_it(self):
        self.config["settings"] = {"HOT_EXAMPLE_PATTERN": "^$5 and $$6$"}
        self.assertEqual(self.agent.reconcile()["state"], "deployed")
        compose = json.loads((Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").read_text())
        self.assertEqual(compose["services"]["app"]["environment"]["HOT_EXAMPLE_PATTERN"], "^$$5 and $$$$6$$")
        bundle = self.agent.command("diagnostics")["diagnostics"]
        self.assertEqual(bundle["environment"]["app"]["HOT_EXAMPLE_PATTERN"], "^$5 and $$6$")

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

    def test_logs_that_hang_are_answered(self):
        recorder = self.run

        def hanging(command, **kwargs):
            if "logs" in command:
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        report = self.agent.answer("logs", service="app", request_id="l1")
        self.assertEqual((report["state"], report["request_id"]), ("failed", "l1"))
        self.assertIn("logs --no-color --timestamps --tail 200 app timed out after 120 seconds", report["error"])
        self.assertNotIn("compose.json", report["error"])

    def test_logs_only_for_known_services(self):
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("logs", service="app; id")
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.agent.save_state({**self.agent.state(), "data_modes": self.agent.configured_modes()})
        with self.assertRaisesRegex(agent_module.AgentError, "Unknown service"):
            self.agent.command("logs", service="postgres")
        self.agent.command("logs", service="valkey")

    def test_an_unreadable_operator_secret_leaves_the_rest_of_the_bundle(self):
        get_secret = self.cloud.get_secret

        def denied(reference):
            if reference == OPERATOR:
                raise agent_module.AgentError("secretsmanager.GetSecretValue failed: AccessDeniedException")
            return get_secret(reference)
        self.cloud.get_secret = denied
        bundle = self.agent.command("diagnostics", request_id="d1")["diagnostics"]
        self.assertEqual(bundle["versions"]["release"], "3.0.1")
        self.assertIn("services", bundle)
        self.assertIn("health", bundle)
        self.assertNotIn("dependencies_error", bundle, "all data is local: no address to read")
        self.config["data"] = {"postgres": {"mode": "existing"}}
        self.agent.save_state({**self.agent.state(), "data_modes": self.agent.configured_modes()})
        bundle = self.agent.command("diagnostics")["diagnostics"]
        self.assertIn("AccessDeniedException", bundle["dependencies_error"])
        self.assertIn("environment", bundle)

    def test_docker_that_no_longer_answers_leaves_the_rest_of_the_bundle(self):
        recorder = self.run

        def hanging(command, **kwargs):
            if "ps" in command:
                raise subprocess.TimeoutExpired(command, kwargs["timeout"])
            return recorder(command, **kwargs)
        self.agent.run_command = hanging
        diagnostics = self.agent.command("diagnostics", request_id="d1")["diagnostics"]
        self.assertIn("ps --all --format json timed out after 120 seconds", diagnostics["services_error"])
        self.assertIn("timed out after 120 seconds", diagnostics["health_error"])
        self.assertIn("configuration", diagnostics)
        self.assertIn("environment", diagnostics)

    def assert_the_rest_of_the_bundle_stays(self, report, error):
        self.assertEqual(report["state"], "diagnostics", report)
        diagnostics = report["diagnostics"]
        self.assertIn(error, diagnostics["services_error"])
        self.assertNotIn("services", diagnostics)
        for part in ("versions", "deploys", "configuration", "dependencies", "environment"):
            self.assertIn(part, diagnostics)

    def test_docker_that_is_down_leaves_the_rest_of_the_bundle(self):
        recorder = self.run

        def down(command, **kwargs):
            if "ps" in command:
                raise subprocess.CalledProcessError(1, command, stderr="Cannot connect to the Docker daemon")
            return recorder(command, **kwargs)
        self.agent.run_command = down
        self.assert_the_rest_of_the_bundle_stays(self.agent.answer("diagnostics", request_id="d1"),
                                                 "Cannot connect to the Docker daemon")

    def test_docker_that_is_not_there_leaves_the_rest_of_the_bundle(self):
        recorder = self.run

        def missing(command, **kwargs):
            if "ps" in command:
                raise FileNotFoundError(2, "No such file or directory", "docker")
            return recorder(command, **kwargs)
        self.agent.run_command = missing
        self.assert_the_rest_of_the_bundle_stays(self.agent.answer("diagnostics", request_id="d1"),
                                                 "No such file or directory")

    def test_compose_output_that_is_not_json_leaves_the_rest_of_the_bundle(self):
        self.run.ps = "WARN[0000] something is off\n"
        self.assert_the_rest_of_the_bundle_stays(self.agent.answer("diagnostics", request_id="d1"), "Expecting value")

    def test_a_bundle_not_all_there_leaves_the_rest_of_the_report(self):
        (Path(self.tmp.name) / "releases/3.0.1/bundle/compose.json").unlink()
        report = self.agent.answer("diagnostics", request_id="d1")
        self.assertEqual(report["state"], "diagnostics", report)
        self.assertIn("compose.json", report["diagnostics"]["environment_error"])
        self.assertIn("configuration", report["diagnostics"])

    def test_an_unreadable_certificate_alarms(self):
        self.run.enddate = "garbage"
        self.assertEqual(self.agent.observe(probe=lambda host, path: 200)["CertificateDaysLeft"][0], 0)


class Reports(AgentHarness):
    def test_deploys_answer_their_request(self):
        report = self.agent.command("deploy-now", request_id="gha-1-1")
        self.assertEqual((report["state"], report["request_id"]), ("deployed", "gha-1-1"))
        report = self.agent.command("deploy-now", service="none", request_id="gha-2-1")
        self.assertEqual((report["state"], report["release"], report["request_id"]), ("current", "3.0.1", "gha-2-1"))

    def test_a_report_that_cannot_be_sent_never_undoes_what_was_done(self):
        def timed_out(reference, status):
            raise TimeoutError("The read operation timed out")
        self.cloud.put_status = timed_out
        with mock.patch("sys.stderr") as stderr:
            report = self.agent.command("deploy-now", request_id="d1")
        self.assertEqual((report["state"], report["request_id"]), ("deployed", "d1"))
        self.assertEqual(self.agent.state()["release"], "3.0.1")
        self.assertIn("status_report_failed", "".join(str(c) for c in stderr.write.call_args_list))

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


class Registries(unittest.TestCase):
    def test_an_ecr_password_comes_from_the_instance_role(self):
        token = base64.b64encode(b"AWS:the-password").decode()
        with mock.patch.object(agent_module.Aws, "call",
                               return_value={"authorizationData": [{"authorizationToken": token}]}) as call:
            password = agent_module.Aws("eu-west-1").registry_password("123456789012.dkr.ecr.us-east-1.amazonaws.com")
        self.assertEqual(password, "the-password")
        self.assertEqual(call.call_args.args[0], "ecr", "ECR signs as ecr at the api.ecr endpoint")
        self.assertEqual(call.call_args.kwargs, {"endpoint": "api.ecr"})


class Groups(unittest.TestCase):
    def test_aws_reads_the_desired_capacity_and_terminates_itself(self):
        xml = ElementTree.fromstring(
            "<R xmlns='http://autoscaling.amazonaws.com/doc/2011-01-01/'><DesiredCapacity>3</DesiredCapacity></R>")
        aws = agent_module.Aws("us-east-1")
        with mock.patch.object(agent_module.Aws, "query", return_value=xml) as query, \
                mock.patch.object(agent_module.Aws, "instance_name", return_value="i-0123"):
            self.assertEqual(aws.group_size("hot-example-task-worker"), 3)
            aws.leave_group("hot-example-task-worker")
        self.assertEqual(query.call_args_list[0].args[2]["AutoScalingGroupNames.member.1"], "hot-example-task-worker")
        self.assertEqual(query.call_args_list[1].args[2], {
            "Action": "TerminateInstanceInAutoScalingGroup", "InstanceId": "i-0123",
            "ShouldDecrementDesiredCapacity": "true"})

    def test_gcp_has_no_way_to_change_its_group(self):
        # The stack grants a task host nothing on instance groups.
        for method in ("group_size", "group_url", "leave_group"):
            self.assertFalse(hasattr(agent_module.Gcp, method), method)

    def test_gcp_queue_metrics_are_written_for_the_project(self):
        gcp = agent_module.Gcp("acme", zone="us-central1-a")
        gcp.instance_id, gcp.instance_zone = "42", "us-central1-a"
        sent = []
        gcp.call = lambda method, url, payload=None, allow=(): sent.append(payload)
        gcp.put_metrics("hot-example", {"JobsWaiting": (3, "Count"), "TaskSlotsUsedPercent": (50, "Percent")})
        resources = {s["metric"]["type"].rsplit("/", 1)[-1]: s["resource"]["type"] for s in sent[0]["timeSeries"]}
        self.assertEqual(resources, {"jobs_waiting": "global", "task_slots_used_percent": "gce_instance"})


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

    def test_a_service_files_password_is_found_alone(self):
        redact = agent_module.Redactor(["[hot]\nhost=db.example\nuser=hot_admin\npassword=p@ss word/0001\nsslmode=verify-full\n"])
        self.assertEqual(redact("auth failed: p@ss word/0001 was refused"), "auth failed: [secret] was refused")
        self.assertEqual(redact("db.example verify-full"), "db.example verify-full", "the rest of the file is not secret")

    def test_a_value_that_only_looks_like_a_url_is_still_removed(self):
        redact = agent_module.Redactor(["abc://[def-password", "one two://[three-password"])
        self.assertEqual(redact("x abc://[def-password y"), "x [secret] y")
        self.assertEqual(redact("x two://[three-password y"), "x [secret] y")

    def test_short_values_are_left_to_the_patterns(self):
        self.assertEqual(agent_module.Redactor(["abc"]).values, [])

    def test_a_private_key_is_removed_however_it_is_printed(self):
        body = ["MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKL"]
        key = "-----BEGIN PRIVATE KEY-----\n" + "\n".join(body) + "\n-----END PRIVATE KEY-----\n"
        redact = agent_module.Redactor([key])
        log = "2026 app | loaded key\n" + "".join(f"2026 app | {line}\n" for line in key.splitlines())
        lines = agent_module.bounded_lines(log, redact)
        self.assertEqual(lines, ["2026 app | loaded key", "2026 app | [private key]"])
        # The journal has one record per line.
        for line in body:
            self.assertEqual(redact(f"app | {line}"), "app | [secret]")

    def test_values_are_found_as_json_escapes_them(self):
        redact = agent_module.Redactor(['p@ss"w\\rd/with%chars'])
        self.assertEqual(redact('{"err": "auth failed for p@ss\\"w\\\\rd/with%chars"}'), '{"err": "auth failed for [secret]"}')


class GeneratedSecrets(unittest.TestCase):
    """Hosts that start together each try to create the generated secret."""

    def test_aws_keeps_the_first_write(self):
        stored, tokens, stale = {}, [], []

        def call(service, target, payload, endpoint=""):
            if target.endswith("GetSecretValue"):
                if stale or "value" not in stored:
                    stale.clear()
                    raise agent_module.AgentError(f"{target} failed: ResourceNotFoundException")
                return {"SecretString": stored["value"]}
            tokens.append(payload["ClientRequestToken"])
            if not 32 <= len(payload["ClientRequestToken"]) <= 64:
                raise agent_module.AgentError(f"{target} failed: ValidationException")
            if "value" in stored and stored["value"] != payload["SecretString"]:
                raise agent_module.AgentError(f"{target} failed: ResourceExistsException")
            stored["value"] = payload["SecretString"]
            return {}
        first, second = agent_module.Aws("us-east-1"), agent_module.Aws("us-east-1")
        first.call = second.call = call
        self.assertEqual(first.put_secret_if_absent(GENERATED, {"KEY": "a"}), {"KEY": "a"})
        # The second host read before the first one's write landed.
        stale.append(True)
        self.assertEqual(second.put_secret_if_absent(GENERATED, {"KEY": "b"}), {"KEY": "a"})
        self.assertEqual(len(set(tokens)), 1)
        self.assertTrue(32 <= len(tokens[0]) <= 64, "Secrets Manager's length for ClientRequestToken")

    def test_gcp_keeps_version_one(self):
        versions, stale = [], []

        def call(method, url, payload=None, allow=()):
            if url.endswith(":addVersion"):
                versions.append(json.loads(base64.b64decode(payload["payload"]["data"])))
                return {"name": f"projects/123/secrets/hot-example-generated/versions/{len(versions)}"}
            version = url.split("/versions/", 1)[1].split(":", 1)[0]
            if not versions or (version == "latest" and stale):
                stale.clear()
                return None  # 404
            value = versions[-1] if version == "latest" else versions[int(version) - 1]
            return {"payload": {"data": base64.b64encode(json.dumps(value).encode()).decode()}}
        reference = "projects/acme/secrets/hot-example-generated"
        first, second = agent_module.Gcp("acme"), agent_module.Gcp("acme")
        first.call = second.call = call
        self.assertEqual(first.put_secret_if_absent(reference, {"KEY": "a"}), {"KEY": "a"})
        # The second host read before the first one's version landed, and adds its own.
        stale.append(True)
        self.assertEqual(second.put_secret_if_absent(reference, {"KEY": "b"}), {"KEY": "a"})
        self.assertEqual(first.get_secret(reference), {"KEY": "a"}, "later deliveries read the same values")
        self.assertEqual(versions, [{"KEY": "a"}, {"KEY": "b"}, {"KEY": "a"}])


class CloudValues(unittest.TestCase):
    """Values an operator may store by hand in the wrong shape."""

    def test_a_gcp_release_pointer_must_name_a_release(self):
        gcp = agent_module.Gcp("acme")
        reference = "projects/acme/secrets/hot-example-release"
        for stored, expected in ((b"3.0.2", "hot-example-release does not hold valid JSON"),
                                 (b'"3.0.2"', 'hot-example-release must hold a JSON object such as {"release": "3.0.2"}'),
                                 (b"3.0", "must hold a JSON object"),
                                 (b'{"release": "3.0.2"}', None)):
            with self.subTest(stored=stored):
                gcp.call = lambda method, url, payload=None, allow=(), data=stored: {
                    "payload": {"data": base64.b64encode(data).decode()}}
                if expected is None:
                    self.assertEqual(gcp.get_parameter(reference), "3.0.2")
                    continue
                with self.assertRaises(agent_module.AgentError) as refused:
                    gcp.get_parameter(reference)
                self.assertIn(expected, str(refused.exception))

    def test_a_secret_that_is_not_json_is_named_without_its_value(self):
        aws = agent_module.Aws("us-east-1")
        aws.call = lambda service, target, payload, endpoint="": {"SecretString": "HOT_DB_URI=postgres://u:hunter2@db"}
        with self.assertRaises(agent_module.AgentError) as refused:
            aws.get_secret(OPERATOR)
        self.assertIn("The secret operator does not hold valid JSON", str(refused.exception))
        self.assertNotIn("hunter2", str(refused.exception))
        cloud = FakeCloud()
        cloud.secrets[OPERATOR] = ["TLS_CERTIFICATE"]
        agent = agent_module.Agent(installation(), cloud)
        with self.assertRaisesRegex(agent_module.AgentError, "must be a JSON object"):
            agent.operator_secrets()

    def test_a_write_check_writes_and_deletes_under_the_prefix_its_grant_covers(self):
        gcp = agent_module.Gcp("acme")
        gcp.token = lambda: "token"
        name = "files%2F.uploads%2Fhot-agent-write-check"
        upload = f"https://storage.googleapis.com/upload/storage/v1/b/acme-hot/o?uploadType=media&name={name}"
        delete = f"https://storage.googleapis.com/storage/v1/b/acme-hot/o/{name}"
        for answers, expected, refused in (({"POST": (200, b'{"name": "x"}'), "DELETE": (204, b"")}, [upload, delete], False),
                                           ({"POST": (200, b'{"name": "x"}'), "DELETE": (404, b"")}, [upload, delete], False),
                                           ({"POST": (403, b"{}")}, [upload], True),
                                           ({"POST": (200, b'{"name": "x"}'), "DELETE": (403, b"{}")}, [upload, delete], True)):
            with self.subTest(answers=answers):
                calls = []

                def http(method, url, body=None, headers=None, timeout=30):
                    calls.append(url)
                    self.assertEqual(headers["Authorization"], "Bearer token")
                    return answers[method]
                with mock.patch.object(agent_module, "http", http):
                    if refused:
                        with self.assertRaisesRegex(agent_module.ApiError, "HTTP 403"):
                            gcp.check_storage_write("acme-hot", "files/")
                    else:
                        gcp.check_storage_write("acme-hot", "files/")
                self.assertEqual(calls, expected)


class StateFiles(AgentHarness):
    def test_a_failed_write_leaves_the_state_as_it_was(self):
        self.agent.reconcile()
        before = self.agent.state()
        with mock.patch.object(agent_module.os, "fsync", side_effect=OSError(28, "No space left on device")):
            with self.assertRaises(OSError):
                self.agent.save_state({**before, "deploying": {"release": "3.0.2", "steps": ["started"]}})
        self.assertEqual(self.agent.state(), before, "never an empty or partial state")
        releases = Path(self.tmp.name) / "releases"
        self.assertEqual([p.name for p in releases.iterdir() if p.name.startswith(".")], [], "nothing half written")
        self.assertEqual(self.agent.state_path().stat().st_mode & 0o777, 0o600)

    def test_the_agent_files_are_replaced_whole(self):
        written = []
        replace = os.replace
        with mock.patch.object(agent_module.os, "replace", lambda source, target: written.append(Path(target).name)
                               or replace(source, target)):
            self.agent.reconcile()
            self.agent.first_delivery("r1")
            self.config["settings"] = {"HOT_APP_SIGNUP": "open"}
            self.cloud.installation = lambda reference: dict(self.config, settings={"HOT_APP_SIGNUP": "closed"})
            self.agent.refresh_configuration()
            self.agent.ship_journal()
        self.assertLessEqual({"state.json", "history.json", "commands.json", "installation.json", "journal.json"},
                             set(written))


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
