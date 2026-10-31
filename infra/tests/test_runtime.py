import copy
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("render", ROOT / "runtime/render.py")
renderer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(renderer)


def example(cloud="aws", storage="local"):
    return json.loads((ROOT / "examples" / f"{cloud}-{storage}.json").read_text())


# The container tests run only against images you name, so a test run never
# pulls an image by surprise. The nightly workflow sets both.
VALKEY_IMAGE = os.environ.get("HOT_INFRA_TEST_VALKEY_IMAGE")
NGINX_IMAGE = os.environ.get("HOT_INFRA_TEST_NGINX_IMAGE")


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], capture_output=True, text=True, **kwargs)


def manifest():
    return {"schema_version": 1, "release": "development-fixture",
            "images": {name: f"registry.example.com/{name}@sha256:" + "a" * 64
                       for name in renderer.IMAGES}}


class RuntimeContract(unittest.TestCase):
    def test_both_clouds_accept_local_and_existing_s3(self):
        for cloud in ("aws", "gcp"):
            for storage in ("local", "existing-s3"):
                with self.subTest(cloud=cloud, storage=storage):
                    config = renderer.validate(example(cloud, storage))
                    bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
                    self.assertTrue(set(renderer.SERVICES).issubset(bundle["services"]))
                    for name in renderer.SERVICES:
                        self.assertEqual(bundle["services"][name]["image"], manifest()["images"]["hot"])

    def test_no_plaintext_secrets_data_ports_or_docker_socket(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        for role in renderer.SERVICES:
            service = bundle["services"][role]
            self.assertEqual(service["environment"]["HOT_ENV"], "production")
            self.assertEqual(service["environment"]["HOT_ENGINE_ISOLATION"], "true")
            self.assertEqual(service["environment"]["HOT_NETWORK_ALLOW_PRIVATE"], "false")
            self.assertNotIn("HOT_DB_URI", service["environment"])
            self.assertNotIn("HOT_DB_MIGRATION_URI_FILE", service["environment"])
            self.assertNotIn("ports", service)
        self.assertEqual(bundle["services"]["ingress"]["ports"], ["443:8443"])
        for role in ("postgres", "valkey"):
            self.assertNotIn("ports", bundle["services"][role])
        self.assertNotIn("docker.sock", json.dumps(bundle))
        self.assertIn("HOT_DB_MIGRATION_URI_FILE", bundle["services"]["migrate"]["environment"])
        self.assertTrue(bundle["services"]["task-worker"]["privileged"])
        self.assertFalse(bundle["services"]["worker"].get("privileged", False))

    def test_mixed_storage_and_gcp_s3_file_credentials(self):
        config = example("gcp", "existing-s3")
        config["installation"]["storage"]["build"] = {"backend": "local"}
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets", "files")
        env = bundle["services"]["worker"]["environment"]
        self.assertEqual(env["HOT_BUILD_STORAGE_TYPE"], "local")
        self.assertEqual(env["HOT_FILE_STORAGE_TYPE"], "s3")
        self.assertIn("HOT_FILE_AWS_SECRET_ACCESS_KEY_FILE", env)
        self.assertNotIn("HOT_BUILD_AWS_SECRET_ACCESS_KEY_FILE", env)
        self.assertNotIn("GCS", json.dumps(env))
        temporary = renderer.render(config, manifest(), "/data", "/run/hot/secrets", "files", True)
        self.assertIn("HOT_FILE_AWS_SESSION_TOKEN_FILE", temporary["services"]["worker"]["environment"])

    def test_invalid_settings_fail_before_rendering(self):
        cases = [
            lambda c: c.update(password="do-not-print"),
            lambda c: c.update(app_exposure="public"),
            lambda c: c["storage"]["file"].update(backend="gcs"),
            lambda c: c["storage"]["file"].update(backend="s3"),
            lambda c: c.update(network={"client_cidrs": ["0.0.0.0/0"]}),
            lambda c: c.update(secret_references={"app": "plaintext-value"}),
            lambda c: c.update(api_hostname="invalid; nginx injection"),
        ]
        for modify in cases:
            config = copy.deepcopy(example()["installation"])
            modify(config)
            with self.assertRaises(ValueError):
                renderer.validate(config)
        unpinned = manifest()
        unpinned["images"]["hot"] = "registry.example.com/hot:latest"
        with self.assertRaises(ValueError):
            renderer.render(example(), unpinned, "/data", "/run/hot/secrets")

    def test_nginx_config_has_safe_defaults_and_streaming(self):
        text = renderer.nginx_config(renderer.validate(example()))
        self.assertIn("ssl_reject_handshake on", text)
        self.assertIn("proxy_buffering off", text)
        self.assertIn("client_max_body_size 300m", text)
        self.assertNotIn("$request_uri", text)
        self.assertNotIn("$http_authorization", text)

    def test_nginx_keeps_every_temp_path_on_the_tmpfs(self):
        text = renderer.nginx_config(renderer.validate(example()))
        for kind in ("client_body", "proxy", "fastcgi", "uwsgi", "scgi"):
            self.assertRegex(text, rf"\n  {kind}_temp_path /tmp/{kind}\b")
        self.assertIn("pid /tmp/nginx.pid", text)

    def test_valkey_healthcheck_sends_a_command(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        test = bundle["services"]["valkey"]["healthcheck"]["test"]
        self.assertEqual(test[0], "CMD-SHELL")
        self.assertIn("--askpass PING <", test[1])

    def test_services_run_in_the_foreground_and_name_their_hosts(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        config = renderer.validate(example())
        for role in renderer.SERVICES:
            env = bundle["services"][role]["environment"]
            # HOT_DAEMON re-spawns the process and exits, so the container stops.
            self.assertNotIn("HOT_DAEMON", env)
            self.assertEqual(env["HOT_APP_URL"], "https://" + config["app_hostname"])
            self.assertEqual(env["HOT_API_URL"], "https://" + config["api_hostname"])

    @unittest.skipUnless(shutil.which("docker") and VALKEY_IMAGE, "set HOT_INFRA_TEST_VALKEY_IMAGE to run Valkey")
    def test_valkey_healthcheck_passes_in_a_container(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        healthcheck = bundle["services"]["valkey"]["healthcheck"]["test"][1]
        with tempfile.TemporaryDirectory() as tmp:
            files = {
                "valkey.conf": renderer.VALKEY_CONF,
                "VALKEY_ACL": "user default on >healthcheck-test ~* &* +@all\n",
                "VALKEY_PASSWORD": "healthcheck-test\n",
            }
            for name, text in files.items():
                (Path(tmp) / name).write_text(text)
                (Path(tmp) / name).chmod(0o644)
            started = docker(
                "run", "-d", "--rm",
                "-v", f"{tmp}/valkey.conf:/etc/valkey/valkey.conf:ro",
                "-v", f"{tmp}/VALKEY_ACL:/run/secrets/VALKEY_ACL:ro",
                "-v", f"{tmp}/VALKEY_PASSWORD:/run/secrets/VALKEY_PASSWORD:ro",
                VALKEY_IMAGE, "valkey-server", "/etc/valkey/valkey.conf",
            )
            self.assertEqual(started.returncode, 0, started.stderr)
            container = started.stdout.strip()
            try:
                result = None
                for _ in range(40):
                    result = docker("exec", container, "sh", "-c", healthcheck)
                    if result.returncode == 0:
                        break
                    time.sleep(0.25)
                self.assertEqual(result.returncode, 0, docker("logs", container).stdout)
                # The command before the fix sent nothing, so it never matched.
                unfixed = healthcheck.replace(" PING <", " <")
                self.assertNotEqual(docker("exec", container, "sh", "-c", unfixed).returncode, 0)
            finally:
                docker("rm", "-f", container)

    @unittest.skipUnless(shutil.which("docker") and shutil.which("openssl") and NGINX_IMAGE,
                         "set HOT_INFRA_TEST_NGINX_IMAGE to run nginx -t")
    def test_nginx_config_loads_read_only_and_unprivileged(self):
        ingress = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]["ingress"]
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "nginx.conf").write_text(renderer.nginx_config(renderer.validate(example())))
            made = subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                 "-subj", "/CN=app.hot.internal", "-keyout", f"{tmp}/TLS_PRIVATE_KEY",
                 "-out", f"{tmp}/TLS_CERTIFICATE"],
                capture_output=True, text=True,
            )
            self.assertEqual(made.returncode, 0, made.stderr)
            for name in ("nginx.conf", "TLS_PRIVATE_KEY", "TLS_CERTIFICATE"):
                (Path(tmp) / name).chmod(0o644)
            # The same user, read-only root and tmpfs the bundle gives the ingress.
            result = docker(
                "run", "--rm", "--read-only", "--user", ingress["user"], "--cap-drop", "ALL",
                "--tmpfs", ingress["tmpfs"][0].replace("size=16777216,", ""),
                "--add-host", "app:127.0.0.1", "--add-host", "api:127.0.0.1",
                "-v", f"{tmp}/nginx.conf:/etc/nginx/nginx.conf:ro",
                "-v", f"{tmp}/TLS_CERTIFICATE:/run/secrets/TLS_CERTIFICATE:ro",
                "-v", f"{tmp}/TLS_PRIVATE_KEY:/run/secrets/TLS_PRIVATE_KEY:ro",
                "--entrypoint", "nginx", NGINX_IMAGE, "-t",
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(shutil.which("docker"), "Docker CLI is not installed")
    def test_compose_accepts_the_complete_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "compose.json"
            path.write_text(json.dumps(renderer.render(example(), manifest(), "/data", "/run/hot/secrets")))
            result = subprocess.run(["docker", "compose", "-f", str(path), "--profile", "admin", "config", "--format", "json"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            parsed = json.loads(result.stdout)
            self.assertEqual(len(parsed["services"]), 10)
            self.assertEqual(parsed["services"]["task-worker"]["pid"], "host")
            self.assertEqual(parsed["services"]["scheduler"]["environment"]["HOT_LOG_FORMAT"], "json")


if __name__ == "__main__":
    unittest.main()
