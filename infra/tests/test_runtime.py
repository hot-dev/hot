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


def contract_output(tfvars):
    """The installation as the stacks hand it to the host: the contract fills
    in every optional field, so the renderer sees the full shape."""
    tofu = shutil.which("tofu")
    if not tofu:
        raise unittest.SkipTest("tofu is not installed")
    result = subprocess.run(
        [tofu, "-chdir=" + str(ROOT / "opentofu/modules/single-machine-contract"), "console",
         "-var-file=" + str(tfvars)],
        input="jsonencode(var.installation)\n", capture_output=True, text=True, check=True)
    return json.loads(json.loads(result.stdout.strip().splitlines()[-1]))


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
            lambda c: c.update(app_exposure="internet"),
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

    def test_the_contract_output_renders(self):
        sources = [*(ROOT / "examples" / f"{name}.json" for name in ("aws-local", "aws-existing-s3", "gcp-local", "gcp-existing-s3")),
                   ROOT / "github/example-operator-repo/installation/installation.tfvars.json"]
        for source in sources:
            with self.subTest(source=source.name):
                config = contract_output(source)
                stack = {"aws": {"data_volume_id": "vol-0abc",
                                 "release_pointer": "arn:aws:ssm:us-east-1:123456789012:parameter/hot/x/release",
                                 "secret_references": {"generated": "arn:aws:secretsmanager:us-east-1:123456789012:secret:g",
                                                       "operator": "arn:aws:secretsmanager:us-east-1:123456789012:secret:o"}},
                         "gcp": {"data_volume_id": "projects/p/zones/z/disks/d", "release_pointer": "projects/p/secrets/r",
                                 "command_subscription": "projects/p/subscriptions/c",
                                 "secret_references": {"generated": "projects/p/secrets/g", "operator": "projects/p/secrets/o"}}}
                config.update(stack[config["cloud"]], status_reference="status")
                renderer.render(config, manifest(), "/data", "/run/hot/secrets")

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

    def test_existing_data_services_leave_out_their_containers(self):
        config = dict(renderer.validate(example()), data={"postgres": {"mode": "existing", "runtime_role": "hot_app"},
                                                          "valkey": {"mode": "existing"}})
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets", extra_deny=["10.9.8.7/32"])
        services = bundle["services"]
        self.assertNotIn("postgres", services)
        self.assertNotIn("valkey", services)
        self.assertEqual(services["app"]["depends_on"], {})
        self.assertIn("check-postgres", services)
        # Connection strings stay in files: never an argument or environment value.
        for name in ("provision", "check-postgres"):
            self.assertIn("$(cat /run/secrets/HOT_DB_MIGRATION_URI)", services[name]["entrypoint"][2])
            self.assertNotIn("environment", services[name])
        self.assertIn("--set=runtime_role=hot_app", services["provision"]["command"])
        self.assertIn("app-POSTGRES_CA_CERT", [s["source"] for s in services["app"]["secrets"]])
        self.assertNotIn("POSTGRES_CA_CERT_FILE", services["app"]["environment"])
        self.assertIn("10.9.8.7/32", services["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"])
        with self.assertRaises(ValueError):
            renderer.validate(dict(config, data={"postgres": {"mode": "managed"}}))
        for role in ("x; drop", "hot_owner"):
            with self.assertRaises(ValueError):
                renderer.validate(dict(config, data={"postgres": {"runtime_role": role}}))

    def test_managed_postgres_is_prepared_by_its_administrator(self):
        endpoint = {"host": "hot-example.c0123.us-east-1.rds.amazonaws.com", "port": 5432, "admin_user": "hot_admin",
                    "admin_secret": "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123"}
        config = dict(renderer.validate(example()), data={"postgres": {"mode": "managed", "endpoint": endpoint}})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertNotIn("postgres", services)
        self.assertIn("valkey", services)
        prepare = services["prepare-postgres"]
        self.assertIn("$(cat /run/secrets/POSTGRES_ADMIN_URI)", prepare["entrypoint"][2])
        self.assertNotIn("environment", prepare)
        self.assertEqual({s["target"] for s in prepare["secrets"]},
                         {"POSTGRES_ADMIN_URI", "POSTGRES_ADMIN_HOT_URI", "POSTGRES_PASSWORD",
                          "POSTGRES_RUNTIME_PASSWORD", "POSTGRES_CA_CERT"})
        # Only the prepare job holds the administrator login.
        for name, service in services.items():
            if name != "prepare-postgres":
                self.assertNotIn("POSTGRES_ADMIN_URI", {s["target"] for s in service.get("secrets", [])})
        self.assertIn("check-postgres", services)
        self.assertIn("app-POSTGRES_CA_CERT", [s["source"] for s in services["app"]["secrets"]])
        # A Cloud SQL endpoint also carries its instance CA.
        gcp = dict(renderer.validate(example("gcp")), data={"postgres": {"mode": "managed", "endpoint": dict(
            endpoint, admin_secret="projects/p/secrets/s")}})
        with self.assertRaisesRegex(ValueError, "ca_certificate"):
            renderer.validate(gcp)
        with self.assertRaisesRegex(ValueError, "Managed Valkey needs the endpoint"):
            renderer.validate(dict(config, data={"valkey": {"mode": "managed"}}))
        cache = {"mode": "managed", "endpoint": {"host": "cache.example", "port": 6379, "auth_secret": "projects/p/secrets/v"}}
        with self.assertRaisesRegex(ValueError, "AWS only"):
            renderer.validate(dict(renderer.validate(example("gcp")), data={"valkey": cache}))

    @unittest.skipUnless(shutil.which("docker"), "Docker CLI is not installed")
    def test_compose_accepts_existing_and_managed_data_bundles(self):
        managed = {"mode": "managed", "endpoint": {"host": "db.example", "port": 5432, "admin_user": "hot_admin",
                                                   "admin_secret": "arn:aws:secretsmanager:us-east-1:123456789012:secret:a"}}
        for data in ({"postgres": {"mode": "existing"}, "valkey": {"mode": "existing"}}, {"postgres": managed}):
            config = dict(renderer.validate(example()), data=data)
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "compose.json"
                path.write_text(json.dumps(renderer.render(config, manifest(), "/data", "/run/hot/secrets")))
                result = subprocess.run(["docker", "compose", "-f", str(path), "--profile", "admin", "config", "--quiet"],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_gcs_storage_uses_the_vm_identity_on_gcp(self):
        config = renderer.validate(example("gcp"))
        config["storage"] = {"build": {"backend": "gcs", "bucket": "acme-hot", "prefix": "builds"},
                             "file": {"backend": "local"}}
        env = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]["worker"]["environment"]
        self.assertEqual(env["HOT_BUILD_STORAGE_TYPE"], "gcs")
        self.assertEqual((env["HOT_BUILD_GCS_BUCKET"], env["HOT_BUILD_GCS_PREFIX"]), ("acme-hot", "builds"))
        self.assertFalse(any("GCS" in key and key.endswith("_FILE") for key in env))
        with self.assertRaisesRegex(ValueError, "needs cloud=gcp"):
            renderer.validate(dict(renderer.validate(example("aws")), storage=config["storage"]))

    def test_public_endpoints_get_a_load_balancer_listener(self):
        private = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        self.assertEqual(private["services"]["ingress"]["ports"], ["443:8443"])
        self.assertEqual(private["services"]["api"]["environment"]["HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES"],
                         renderer.RUNTIME_SUBNET)
        config = dict(renderer.validate(example("gcp")), api_exposure="public")
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
        self.assertIn("9443:9443", bundle["services"]["ingress"]["ports"])
        self.assertIn("130.211.0.0/22", bundle["services"]["api"]["environment"]["HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES"])
        text = renderer.nginx_config(config)
        lb = text.split("listen 9443 ssl;")
        self.assertEqual(len(lb), 2, "only the public endpoint is served to the load balancer")
        self.assertIn(config["api_hostname"], lb[1].split("}")[0])
        self.assertIn("return 444", text)
        with self.assertRaises(ValueError):
            renderer.validate(dict(config, app_exposure="internet"))
        with self.assertRaisesRegex(ValueError, "set by the runtime bundle"):
            renderer.render(dict(config, settings={"HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES": "0.0.0.0/0"}),
                            manifest(), "/data", "/run/hot/secrets")

    def test_postgres_data_uses_the_postgres_18_layout(self):
        postgres = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]["postgres"]
        self.assertEqual(postgres["volumes"][0]["target"], "/var/lib/postgresql")
        upstream = json.loads((ROOT / "release/upstream-images.json").read_text())
        self.assertIn("-pg18@", upstream["postgres"])

    def test_services_run_in_the_foreground(self):
        # daemon = true makes the CLI start a background copy and exit, which
        # a container's supervisor sees as the service stopping.
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        for role in renderer.SERVICES:
            self.assertNotIn("HOT_DAEMON", bundle["services"][role]["environment"], role)

    def test_services_know_the_installation_hostnames(self):
        config = example("aws", "local")
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
        for role in renderer.SERVICES:
            env = bundle["services"][role]["environment"]
            self.assertEqual(env["HOT_APP_URL"], "https://" + config["installation"]["app_hostname"])
            self.assertEqual(env["HOT_API_URL"], "https://" + config["installation"]["api_hostname"])
            self.assertEqual(env["HOT_OAUTH_REDIRECT_BASE_URL"], env["HOT_APP_URL"])
            self.assertEqual(env["HOT_COOKIE_DOMAIN"], config["installation"]["app_hostname"])
        with self.assertRaisesRegex(ValueError, "set by the runtime bundle"):
            renderer.render(dict(config["installation"], settings={"HOT_APP_URL": "https://elsewhere.example"}),
                            manifest(), "/data", "/run/hot/secrets")

    def test_logs_ship_to_the_cloud_without_blocking(self):
        aws = renderer.render(example("aws"), manifest(), "/data", "/run/hot/secrets")["services"]
        gcp = renderer.render(example("gcp"), manifest(), "/data", "/run/hot/secrets")["services"]
        for services in (aws, gcp):
            for name, service in services.items():
                options = service["logging"]["options"]
                self.assertEqual(options["mode"], "non-blocking", name)
                self.assertEqual((options["cache-max-size"], options["cache-max-file"]), ("10m", "3"), name)
        self.assertEqual(aws["worker"]["logging"]["driver"], "awslogs")
        self.assertEqual(aws["worker"]["logging"]["options"]["awslogs-group"], "/hot/hot-example")
        self.assertEqual(gcp["migrate"]["logging"]["driver"], "gcplogs")
        local = dict(renderer.validate(example("aws")), logging={"destination": "local"})
        services = renderer.render(local, manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertEqual({s["logging"]["driver"] for s in services.values()}, {"local"})
        with self.assertRaisesRegex(ValueError, "logging.destination"):
            renderer.validate(dict(local, logging={"destination": "syslog"}))

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
