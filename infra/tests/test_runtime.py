import copy
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("render", ROOT / "runtime/render.py")
renderer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(renderer)


def example(cloud="aws", storage="local"):
    return json.loads((ROOT / "examples" / f"{cloud}-{storage}.json").read_text())


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

    def test_user_code_is_denied_the_installation_itself(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        subnet = bundle["networks"]["default"]["ipam"]["config"][0]["subnet"]
        for role in ("worker", "task-worker"):
            deny = bundle["services"][role]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(",")
            self.assertIn(subnet, deny)
            self.assertIn("10.68.0.0/16", deny)
        self.assertNotIn("HOT_NETWORK_DENY_CIDRS", bundle["services"]["app"]["environment"])

    def test_the_example_operator_installation_is_valid(self):
        tfvars = json.loads((ROOT / "github/example-operator-repo/installation/installation.tfvars.json").read_text())
        config = dict(tfvars["installation"], data_volume_id="vol-0abc",
                      release_pointer="arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
                      secret_references={"generated": "arn:aws:secretsmanager:us-east-1:123456789012:secret:g",
                                         "operator": "arn:aws:secretsmanager:us-east-1:123456789012:secret:o"})
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
        self.assertEqual(bundle["services"]["worker"]["environment"]["HOT_EMAIL_PROVIDER"], "smtp")
        self.assertIn("bootstrap-admin", bundle["services"])
        with self.assertRaisesRegex(ValueError, "looks like a secret"):
            renderer.validate(dict(config, settings={"HOT_EMAIL_SMTP_PASSWORD": "x"}))
        with self.assertRaisesRegex(ValueError, "set by the runtime bundle"):
            renderer.render(dict(config, settings={"HOT_ENGINE_ISOLATION": "false"}), manifest(), "/data", "/run/hot/secrets")

    def test_allocations_grow_with_size_and_fit_the_vm(self):
        previous = None
        for size in ("small", "medium", "large"):
            config = dict(renderer.validate(example()), size=size)
            bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
            total = sum(int(s["mem_limit"].rstrip("m")) for s in bundle["services"].values() if "mem_limit" in s)
            self.assertLessEqual(total, renderer.VM_MEMORY_MB[size] - 1024, size)
            task = int(bundle["services"]["task-worker"]["environment"]["HOT_TASK_WORKER_MEMORY_MB"])
            self.assertEqual(f"{task}m", bundle["services"]["task-worker"]["mem_limit"])
            if previous:
                self.assertGreater(total, previous)
            previous = total
            maxmemory = int(renderer.valkey_conf(size).split("maxmemory ")[1].split("mb")[0])
            self.assertLess(maxmemory, int(bundle["services"]["valkey"]["mem_limit"].rstrip("m")))

    def test_postgres_data_uses_the_postgres_18_layout(self):
        postgres = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]["postgres"]
        self.assertEqual(postgres["volumes"][0]["target"], "/var/lib/postgresql")
        upstream = json.loads((ROOT / "release/upstream-images.json").read_text())
        self.assertIn("-pg18@", upstream["postgres"])

    def test_nginx_config_has_safe_defaults_and_streaming(self):
        text = renderer.nginx_config(renderer.validate(example()))
        self.assertIn("ssl_reject_handshake on", text)
        self.assertIn("proxy_buffering off", text)
        self.assertIn("client_max_body_size 300m", text)
        self.assertNotIn("$request_uri", text)
        self.assertNotIn("$http_authorization", text)

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


release_spec = importlib.util.spec_from_file_location("release_manifest", ROOT / "release/release_manifest.py")
releaser = importlib.util.module_from_spec(release_spec)
release_spec.loader.exec_module(releaser)


def fake_cosign(directory, exit_code):
    """A cosign stand-in that records its arguments."""
    script = Path(directory) / "cosign"
    script.write_text(f"#!/bin/sh\necho \"$@\" > {directory}/args\necho 'none of the expected identities matched' >&2\nexit {exit_code}\n")
    script.chmod(0o755)
    return str(script)


class ReleaseManifest(unittest.TestCase):
    def test_release_manifests_pin_every_image_and_render(self):
        upstream = json.loads((ROOT / "release/upstream-images.json").read_text())
        hot = "ghcr.io/hot-dev/hot-runtime@sha256:" + "b" * 64
        built = releaser.build("3.0.1", hot, upstream)
        self.assertEqual(built["images"]["hot"], hot)
        self.assertEqual(built["module_tag"], "v3.0.1")
        self.assertGreater(built["migrations"]["latest"], 0)
        renderer.render(example(), built, "/data", "/run/hot/secrets")
        with self.assertRaises(ValueError):
            releaser.build("3.0.1", "ghcr.io/hot-dev/hot-runtime:latest", upstream)
        with self.assertRaises(ValueError):
            releaser.build("latest", hot, upstream)

    def test_unknown_manifest_fields_are_rejected(self):
        extra = manifest()
        extra["unsigned_override"] = True
        with self.assertRaises(ValueError):
            renderer.render(example(), extra, "/data", "/run/hot/secrets")

    def test_signatures_must_name_the_release_workflow(self):
        with tempfile.TemporaryDirectory() as tmp:
            renderer.verify_manifest_signature("m.json", "m.sigstore.json", cosign=fake_cosign(tmp, 0))
            args = (Path(tmp) / "args").read_text()
            self.assertIn("verify-blob --bundle m.sigstore.json", args)
            self.assertIn(renderer.RELEASE_SIGNER, args)
            self.assertIn(renderer.RELEASE_ISSUER, args)
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "did not verify: none of the expected"):
                renderer.verify_manifest_signature("m.json", "m.sigstore.json", cosign=fake_cosign(tmp, 1))
        signer = re.compile(renderer.RELEASE_SIGNER)
        self.assertTrue(signer.match("https://github.com/hot-dev/hot/.github/workflows/release.yml@refs/tags/v3.0.1"))
        self.assertFalse(signer.match("https://github.com/hot-dev/hot/.github/workflows/release.yml@refs/heads/main"))
        self.assertFalse(signer.match("https://github.com/fork/hot/.github/workflows/release.yml@refs/tags/v3.0.1"))


if __name__ == "__main__":
    unittest.main()
