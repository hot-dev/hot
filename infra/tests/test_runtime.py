import copy
import importlib.util
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import unittest
from unittest import mock

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
# A PostgreSQL image's psql, and a database it can reach from a container.
POSTGRES_IMAGE = os.environ.get("HOT_INFRA_TEST_POSTGRES_IMAGE")
POSTGRES_URI = os.environ.get("HOT_INFRA_TEST_POSTGRES_URI")


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], capture_output=True, text=True, **kwargs)


# Line breaks and all, as an operator's private certificate authority reads.
SMTP_CA = "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUTest\n-----END CERTIFICATE-----\n"


def manifest():
    return {"schema_version": 1, "release": "development-fixture",
            "images": {name: f"registry.example.com/{name}@sha256:" + "a" * 64
                       for name in renderer.IMAGES}}


def ip_net(cidr):
    """A trusted-proxy entry as every Hot release parses it (the API panics at
    startup on one it cannot). Current Hot also reads a bare address as a
    single host, but older releases require a prefix length, so the renderer
    still writes one. Python would also take a netmask, which Hot refuses."""
    if not re.fullmatch(r"[0-9A-Fa-f:.]+/[0-9]{1,3}", cidr):
        raise ValueError(f"Hot refuses the trusted proxy {cidr!r}")
    return ipaddress.ip_network(cidr, strict=False)


def resolve_client(forwarded, peer, trusted):
    """Hot's client address (crates/hot_api/src/client_ip.rs): the
    rightmost untrusted address of X-Forwarded-For and the peer."""
    networks = [ip_net(cidr) for cidr in trusted]
    chain = [ipaddress.ip_address(ip) for ip in [*forwarded, peer]]
    if not any(chain[-1] in network for network in networks):
        return str(chain[-1])
    return str(next((ip for ip in reversed(chain) if not any(ip in n for n in networks)), chain[0]))


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
            lambda c: c.update(network={"client_cidrs": ["10.0.0.0/255.0.0.0"]}),
            lambda c: c.update(network={"cidr": "10.68.0.0/255.255.0.0"}),
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

    def test_settings_cannot_change_what_the_bundle_sets(self):
        for key in ("HOT_LOG_LEVEL", "HOT_NETWORK_ALLOW_PRIVATE", "HOT_FILE_STORAGE_TYPE", "HOT_BUILD_STORAGE_PATH",
                    "HOT_FILE_AWS_S3_BUCKET", "HOT_BUILD_GCS_PREFIX", "HOT_NETWORK_CLIENT_IP_HEADER",
                    "HOT_STORE_MODELS_PATH", "HOT_API_KNOWN_HOSTS", "HOT_APP_PORT", "HOT_API_PORT", "HOT_DB_SCHEMA",
                    "HOT_DB_RLS_ENFORCE", "HOT_RLS_ENFORCE", "HOT_STORE_TYPE", "HOT_TASK_DATA_VOLUME_DIR",
                    "HOT_TASK_CONTAINER_RESERVED_MEMORY_MB", "HOT_TASK_CODE_MEMORY_PERCENT"):
            with self.assertRaisesRegex(ValueError, f"{key} is set by the runtime bundle"):
                renderer.validate(dict(example()["installation"], settings={key: "x"}))
        # Hot parses JSON for a name whose section is a map, and replaces the whole
        # section, so these would undo what the names above are refused to protect:
        # only what the bundle sets comes back after them.
        conf = (ROOT.parent / "crates/hot_cli/src/conf.rs").read_text()
        self.assertIn("Some(Val::Map(_)) => match serde_json::from_str::<Val>(&value)", conf)
        for key, section in (("HOT_DB", '{"rls":{"enforce":false},"schema":"x"}'), ("HOT_APP", '{"port":9000}'),
                             ("HOT_API", '{"port":9000}')):
            self.assertIn(f'"{key[4:].lower()}": {key[4:].lower()}_conf', conf)
            with self.assertRaisesRegex(ValueError, f"{key} is set by the runtime bundle"):
                renderer.validate(dict(example()["installation"], settings={key: section}))
        # A scalable installation sizes each instance's pool for the database;
        # a single machine leaves it to the operator.
        pool = {"HOT_WORKER_DB_MAX_CONNECTIONS": "30"}
        env = renderer.render(dict(renderer.validate(example()), settings=pool), manifest(), "/data", "/run/hot/secrets")[
            "services"]["worker"]["environment"]
        self.assertEqual(env["HOT_WORKER_DB_MAX_CONNECTIONS"], "30")
        with self.assertRaisesRegex(ValueError, "HOT_WORKER_DB_MAX_CONNECTIONS is set by the runtime bundle"):
            renderer.validate(dict(self.scalable(["worker"])[0], settings=pool))

    def test_settings_cannot_name_a_file(self):
        # Nothing can mount one, so Hot would stop at startup: a missing file
        # for the first, and "set only one of" beside the bundle's HOT_LOG_LEVEL.
        for key, value in (("HOT_NETWORK_EGRESS_PROXY_FILE", "HOT_NETWORK_EGRESS_PROXY"), ("HOT_LOG_LEVEL_FILE", "HOT_LOG_LEVEL"),
                           ("HOT_APP_SIGNUP_FILE", "HOT_APP_SIGNUP")):
            with self.assertRaisesRegex(ValueError, f"{key} names a file no container has; set {value} here"):
                renderer.validate(dict(example()["installation"], settings={key: "/run/secrets/x"}))
        self.assertTrue(renderer.reserved_setting(renderer.validate(example()), "HOT_APP_SIGNUP_FILE"))

    def test_a_storage_moves_skip_count_is_what_the_contract_takes(self):
        # How many failed objects to leave behind, which only the agent reads.
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        self.assertIn('regex("^[1-9][0-9]*$", lookup(var.installation.settings, "HOT_MOVE_STORAGE_SKIP_FAILED", "1"))', contract)
        config = renderer.validate(example())
        for count in ("1", "120000"):
            settings = {"HOT_MOVE_STORAGE_SKIP_FAILED": count}
            self.assertEqual(renderer.validate(dict(config, settings=settings))["settings"], settings)
        for count in ("0", "-1", "1.5", "012", "0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d21", "", "   ", " 3", "3 ", "3\n", "٣", "all", "leak:hunter2"):
            with self.subTest(count=count), self.assertRaisesRegex(ValueError, "HOT_MOVE_STORAGE_SKIP_FAILED must be a whole number") as refused:
                renderer.validate(dict(config, settings={"HOT_MOVE_STORAGE_SKIP_FAILED": count}))
            self.assertNotIn("hunter2", str(refused.exception))
        # Settings are strings before this check sees them.
        for count in (3, None):
            with self.subTest(count=count), self.assertRaisesRegex(ValueError, "must be a HOT_\\* name with a string value"):
                renderer.validate(dict(config, settings={"HOT_MOVE_STORAGE_SKIP_FAILED": count}))

    def test_a_storage_moves_skip_file_comes_with_its_job_and_not_the_bundle(self):
        # The move runs in the installed bundle, which an earlier render wrote, so
        # nothing a render adds reaches it: the agent mounts the file for that one
        # job, as it does a bucket's keys. A worker runs as JOB_USER with a read-only
        # root, so that file has to be readable as that user and is not written there.
        self.assertIn('self.move_storage(self.releases / previous / "bundle",', (ROOT / "agent/hot_agent.py").read_text())
        config = dict(renderer.validate(example()), settings={"HOT_MOVE_STORAGE_SKIP_FAILED": "3"})
        worker = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]["worker"]
        self.assertEqual((worker["user"], worker["read_only"]), ("10001:10001", True))
        self.assertNotIn("HOT_MOVE_STORAGE_SKIP_FILE", worker["environment"])
        self.assertNotIn("skip", json.dumps(worker["volumes"]))
        # Nor can a setting name one: Hot's move would read a file no container has.
        with self.assertRaisesRegex(ValueError, "HOT_MOVE_STORAGE_SKIP_FILE names a file no container has"):
            renderer.validate(dict(example()["installation"], settings={"HOT_MOVE_STORAGE_SKIP_FILE": "/run/secrets/x"}))
        self.assertTrue(renderer.reserved_setting(renderer.validate(example()), "HOT_MOVE_STORAGE_SKIP_FILE"))

    def test_settings_hot_refuses_at_startup_stop_the_render(self):
        # What Hot's parsers exit or panic on, and allow-host entries that never match.
        config = renderer.validate(example())
        for key, refused, accepted in (
                ("HOT_NETWORK_ALLOW_HOSTS",
                 ("https://x.internal:443", "x.internal:443", "x.internal/path", "*", "*.", "*.*.x.internal", "a*.x.internal",
                  "api.corp.example,x.internal:443", "x.internal.", "a b.internal"),
                 ("", "api.corp.example", " *.svc.internal , x.internal,,")),
                ("HOT_NETWORK_EGRESS_PROXY",
                 ("proxy.internal:3128", "ftp://proxy.internal:21", "socks5://proxy.internal:1080", "http://",
                  "http://proxy.internal:3128/path", "http://proxy.internal:99999", "http://proxy.internal:port",
                  "http://a b:3128", "http://256.256.256.256:3128", "http://proxy.1:3128", "http://[nope]:3128",
                  "http://[10.0.0.5]:3128"),
                 ("", "  ", "http://proxy.internal:3128", "https://proxy.internal:3128/", "HTTP://proxy.internal",
                  " http://proxy.internal:3128 ", "http://10.0.0.5:3128", "http://[2001:db8::1]:3128")),
                ("HOT_APP_SIGNUP", ("closed", "", "invited"), ("open", "invite", " INVITE ")),
                ("HOT_REDIS_AUTH", ("iam", "PASSWORD", "none"), ("", " password ")),
                ("HOT_REDIS_CA_CERT", ("not a certificate",), ("", " ", SMTP_CA))):
            for value in refused:
                with self.subTest(key=key, value=value), self.assertRaisesRegex(ValueError, key):
                    renderer.validate(dict(config, settings={key: value}))
            for value in accepted:
                with self.subTest(key=key, value=value):
                    renderer.validate(dict(config, settings={key: value}))

    def test_refused_settings_are_not_repeated(self):
        # A mistaken entry may hold what belongs in the operator secret bundle.
        config = renderer.validate(example())
        for key, value in (("HOT_NETWORK_ALLOW_HOSTS", "leak:hunter2"), ("HOT_NETWORK_EGRESS_PROXY", "leak:hunter2"),
                           ("HOT_APP_SIGNUP", "leak:hunter2"), ("HOT_REDIS_AUTH", "leak:hunter2"),
                           ("HOT_REDIS_CA_CERT", "leak:hunter2")):
            with self.assertRaisesRegex(ValueError, key) as refused:
                renderer.validate(dict(config, settings={key: value}))
            self.assertNotIn("hunter2", str(refused.exception))

    def test_the_egress_proxy_check_takes_a_bundled_address(self):
        # The operator secret bundle's copy may carry credentials, as Hot reads them.
        self.assertTrue(renderer.is_egress_proxy("http://hot:pa%40ss@proxy.internal:3128"))
        self.assertFalse(renderer.is_egress_proxy("hot:pass@proxy.internal:3128"))
        self.assertFalse(renderer.is_egress_proxy("http://hot:pa/ss@proxy.internal:3128"))

    def test_redis_settings_fit_the_valkey_in_use(self):
        gcp = renderer.validate(example("gcp"))
        existing = {"valkey": {"mode": "existing"}}
        iam = {"HOT_REDIS_AUTH": "gcp-iam"}
        # A local Valkey is plaintext, and only Google's metadata server has tokens.
        for config in (gcp, renderer.validate(example("aws")), dict(renderer.validate(example("aws")), data=existing)):
            with self.assertRaisesRegex(ValueError, "HOT_REDIS_AUTH can be gcp-iam only"):
                renderer.validate(dict(config, settings=iam))
        renderer.validate(dict(gcp, data=existing, settings=iam))
        # Hot's gcp-iam and CA certificate support standalone servers only.
        renderer.validate(dict(gcp, data=existing, settings={"HOT_REDIS_CLUSTER": "true"}))
        for settings in ({**iam, "HOT_REDIS_CLUSTER": "true"}, {"HOT_REDIS_CA_CERT": SMTP_CA, "HOT_REDIS_CLUSTER": "ON"}):
            with self.assertRaisesRegex(ValueError, "HOT_REDIS_CLUSTER"):
                renderer.validate(dict(gcp, data=existing, settings=settings))
        renderer.validate(dict(gcp, data=existing, settings={**iam, "HOT_REDIS_CLUSTER": "false"}))

    def test_the_contract_refuses_what_the_bundle_sets(self):
        text = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        block = text[text.index("The runtime bundle sets these itself"):]
        names, rules = block[:block.index("error_message")].split("], key)")
        single = set(renderer.common_environment(renderer.validate(example())))
        scalable = set(renderer.common_environment(self.scalable(["worker"])[0]))
        self.assertEqual(set(re.findall(r'"(HOT_[A-Z0-9_]+)"', names)), single | set(renderer.RESERVED_SETTINGS))
        for function, expected in (("endswith", renderer.RESERVED_SUFFIXES), ("strcontains", renderer.RESERVED_PARTS),
                                   ("startswith", renderer.RESERVED_PREFIXES)):
            self.assertEqual(set(re.findall(function + r'\(key, "([A-Z0-9_]+)"\)', rules)), set(expected), function)
        # Reserved only where the bundle sets it.
        self.assertEqual(scalable - single, {"HOT_WORKER_DB_MAX_CONNECTIONS"})
        self.assertIn('(var.installation.preset != "scalable" || key != "HOT_WORKER_DB_MAX_CONNECTIONS")', rules)
        self.assertEqual(set(re.findall(r'key != "(HOT_[A-Z0-9_]+)"', rules)), scalable - single)

    def test_installation_names_are_usable_in_cloud_resource_names(self):
        for name in ("abc", "a-1", "hot-example", "a" + "-" * 22 + "z"):
            self.assertEqual(renderer.validate(dict(example()["installation"], name=name))["name"], name)
        for name in ("abc-", "ab", "1bc", "Abc", "a" * 25, "a_c", ""):
            with self.assertRaisesRegex(ValueError, "Installation name", msg=name):
                renderer.validate(dict(example()["installation"], name=name))

    def test_bucket_prefixes_cannot_widen_the_hosts_grant(self):
        def configured(cloud, backend, prefix):
            config = copy.deepcopy(example(cloud)["installation"])
            config["storage"]["file"] = {"backend": backend, "bucket": "acme", "region": "us-east-1", "prefix": prefix}
            return config

        for cloud, backend in (("aws", "s3"), ("gcp", "s3"), ("gcp", "gcs")):
            for prefix in ("files", "hot-example/files", "a.b_c-d/e/"):
                self.assertEqual(renderer.validate(configured(cloud, backend, prefix))["storage"]["file"]["prefix"], prefix)
            for prefix in ("*", "files/*", "fi?les", 'files") || true || ("', "a b", "a//b"):
                with self.assertRaisesRegex(ValueError, "storage prefix", msg=(backend, prefix)):
                    renderer.validate(configured(cloud, backend, prefix))
        # A local backend has no bucket, and takes no grant.
        local = copy.deepcopy(example()["installation"])
        local["storage"]["file"] = {"backend": "local", "prefix": "any thing"}
        renderer.validate(local)

    def test_call_retention_must_be_a_value_hot_honors(self):
        for days in ("-1", "1", "30", "36500"):
            renderer.validate(dict(example()["installation"], settings={"HOT_DB_RETENTION_CALL_DAYS": days}))
        # Hot would keep calls forever on these, saying so only in the worker's log.
        for days in ("", " 30", "30 ", "30\n", "30d", "0", "-2", "36501", "40000", "030", "+30", "1.5", "3_0", "٣٠"):
            with self.assertRaisesRegex(ValueError, "HOT_DB_RETENTION_CALL_DAYS must be", msg=repr(days)):
                renderer.validate(dict(example()["installation"], settings={"HOT_DB_RETENTION_CALL_DAYS": days}))

    def test_a_proxys_credentials_stay_out_of_settings(self):
        plain = "http://proxy.internal:3128"
        env = renderer.render(dict(renderer.validate(example()), settings={"HOT_NETWORK_EGRESS_PROXY": plain}),
                              manifest(), "/data", "/run/hot/secrets")["services"]["worker"]["environment"]
        self.assertEqual(env["HOT_NETWORK_EGRESS_PROXY"], plain)
        for proxy in ("http://hot:s3cr3t@proxy.internal:3128", "https://hot@proxy.internal", "hot:s3cr3t@proxy.internal:3128"):
            with self.assertRaisesRegex(ValueError, "HOT_NETWORK_EGRESS_PROXY.*operator secret bundle") as refused:
                renderer.validate(dict(example()["installation"], settings={"HOT_NETWORK_EGRESS_PROXY": proxy}))
            self.assertNotIn("s3cr3t", str(refused.exception))
        # From the bundle, only the services that run user code mount it.
        routes = {"worker": ["HOT_NETWORK_EGRESS_PROXY"], "task-worker": ["HOT_NETWORK_EGRESS_PROXY"]}
        services = renderer.render(example(), manifest(), "/data", "/run/hot/secrets", operator_secrets=routes)["services"]
        for role in renderer.SERVICES:
            env, mounted = services[role]["environment"], [s["target"] for s in services[role]["secrets"]]
            self.assertNotIn("HOT_NETWORK_EGRESS_PROXY", env, role)
            if role in routes:
                self.assertEqual(env["HOT_NETWORK_EGRESS_PROXY_FILE"], "/run/secrets/HOT_NETWORK_EGRESS_PROXY", role)
                self.assertIn("HOT_NETWORK_EGRESS_PROXY", mounted, role)
            else:
                self.assertNotIn("HOT_NETWORK_EGRESS_PROXY_FILE", env, role)
                self.assertNotIn("HOT_NETWORK_EGRESS_PROXY", mounted, role)

    def test_a_private_smtp_ca_reaches_the_services_intact(self):
        # Not a credential, so it stays in settings, where every Hot service
        # reads the rest of the email configuration.
        config = dict(renderer.validate(example()), settings={"HOT_EMAIL_SMTP_CA_CERT": SMTP_CA})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        for role in renderer.SERVICES:
            self.assertEqual(services[role]["environment"]["HOT_EMAIL_SMTP_CA_CERT"], SMTP_CA, role)

    def test_user_code_is_denied_the_installation_itself(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        subnet = bundle["networks"]["default"]["ipam"]["config"][0]["subnet"]
        for role in ("worker", "task-worker"):
            deny = bundle["services"][role]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(",")
            self.assertIn(subnet, deny)
            self.assertIn("10.68.0.0/16", deny)
            # The host's Kata bridge address (10.88.0.1). Host images have no
            # Docker bridge, so 172.17.0.0/16 is an ordinary range.
            self.assertIn("10.88.0.0/16", deny)
            self.assertNotIn("172.17.0.0/16", deny)
        self.assertNotIn("HOT_NETWORK_DENY_CIDRS", bundle["services"]["app"]["environment"])
        # An existing network adds no range of its own; the host's own address
        # comes from the agent with the data services'.
        existing = dict(renderer.validate(example()), network={"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"})
        deny = renderer.render(existing, manifest(), "/data", "/run/hot/secrets", extra_deny=["10.1.2.3/32"])[
            "services"]["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(",")
        self.assertNotIn("10.68.0.0/16", deny)
        self.assertEqual(deny, renderer.user_code_deny(existing, ["10.1.2.3/32"]))
        self.assertIn("10.1.2.3/32", deny)
        self.assertIn("10.88.0.0/16", deny)

    def test_operator_deny_entries_still_apply_to_user_code(self):
        settings = {"HOT_NETWORK_ALLOW_CIDRS": "10.0.0.0/8", "HOT_NETWORK_DENY_CIDRS": "10.50.0.10/32, 10.20.5.0/24"}
        config = dict(renderer.validate(example()), settings=settings)
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets", extra_deny=["10.9.8.7/32"])["services"]
        for role in ("worker", "task-worker"):
            env = services[role]["environment"]
            deny = env["HOT_NETWORK_DENY_CIDRS"].split(",")
            self.assertEqual(env["HOT_NETWORK_ALLOW_CIDRS"], "10.0.0.0/8")
            for cidr in ("10.50.0.10/32", "10.20.5.0/24", "10.9.8.7/32", renderer.RUNTIME_SUBNET, "10.68.0.0/16"):
                self.assertIn(cidr, deny, role)
        self.assertEqual(services["app"]["environment"]["HOT_NETWORK_DENY_CIDRS"], settings["HOT_NETWORK_DENY_CIDRS"])
        for bad in ("vault.internal", "10.0.0.0/255.0.0.0", "10.0.0.0/0.255.255.255", "fe80::1%eth0", "10.0.0.0/33"):
            with self.assertRaisesRegex(ValueError, "HOT_NETWORK_DENY_CIDRS entries", msg=bad):
                renderer.validate(dict(config, settings={"HOT_NETWORK_DENY_CIDRS": f"10.50.0.10/32,{bad}"}))
        # Hot takes a bare address here, and host bits.
        renderer.validate(dict(config, settings={"HOT_NETWORK_DENY_CIDRS": "10.50.0.10,10.50.0.11/24,2600::1"}))

    def data_networks(self, postgres, valkey):
        """An existing network, and managed data services that name `postgres`
        and `valkey` as their networks, under an allowlist covering them all."""
        return dict(renderer.validate(example()), settings={"HOT_NETWORK_ALLOW_CIDRS": "10.0.0.0/8"},
                    network={"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1"},
                    data={"postgres": {"mode": "managed", "endpoint": {
                              "host": "db.example", "port": 5432, "admin_user": "hot_admin",
                              "admin_secret": "arn:aws:secretsmanager:us-east-1:1:secret:a", "deny_cidrs": postgres}},
                          "valkey": {"mode": "managed", "endpoint": {
                              "host": "cache.example", "port": 6379,
                              "auth_secret": "arn:aws:secretsmanager:us-east-1:1:secret:v", "deny_cidrs": valkey}}})

    def test_data_networks_are_denied_where_a_failover_can_move_a_service(self):
        config = self.data_networks(["10.1.2.0/24", "10.1.3.0/24"], ["10.1.3.0/24", "10.1.4.0/24", "fd00:1::/48"])
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets", extra_deny=["10.1.2.10/32"])["services"]
        for role in ("worker", "task-worker"):
            deny = services[role]["environment"]["HOT_NETWORK_DENY_CIDRS"].split(",")
            self.assertEqual(deny, renderer.user_code_deny(config, ["10.1.2.10/32"]))
            self.assertEqual(len(deny), len(set(deny)), role)
            # The endpoint's address stays, beside the networks.
            for cidr in ("10.1.2.0/24", "10.1.3.0/24", "10.1.4.0/24", "fd00:1::/48", "10.1.2.10/32"):
                self.assertIn(cidr, deny, role)
            # A standby and a replica the bundle never resolved, inside the
            # allowlist: Hot's deny list wins over it, and it parses all of these.
            networks = [ip_net(cidr) for cidr in deny]
            for address in ("10.1.2.11", "10.1.4.20"):
                self.assertTrue(any(ipaddress.ip_address(address) in network for network in networks), address)
        for role in ("app", "api", "scheduler"):
            self.assertNotIn("HOT_NETWORK_DENY_CIDRS", services[role]["environment"], role)
        # An existing service's networks count too, and a scalable host's.
        existing = dict(renderer.validate(example()), data={"valkey": {"mode": "existing", "endpoint": {"deny_cidrs": ["10.1.5.0/24"]}}})
        self.assertIn("10.1.5.0/24", renderer.user_code_deny(renderer.validate(existing)))
        scalable, _ = self.scalable(["worker"])
        scalable["data"]["postgres"]["endpoint"]["deny_cidrs"] = ["10.1.6.0/24"]
        worker = renderer.render(scalable, manifest(), "/data", "/run/hot/secrets")["services"]["worker"]
        self.assertIn("10.1.6.0/24", worker["environment"]["HOT_NETWORK_DENY_CIDRS"].split(","))

    def test_data_networks_reach_box_guests(self):
        with mock.patch.dict(os.environ, {"HOT_AGENT_RUNTIME": str(ROOT / "runtime")}):
            spec = importlib.util.spec_from_file_location("hot_agent", ROOT / "agent/hot_agent.py")
            agent = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(agent)
        config = self.data_networks(["10.1.2.0/24"], ["10.1.3.0/24", "fd00:1::/48"])
        lines = dict(line.split("=", 1) for line in agent.guest_network_env(config, ["10.1.2.10/32"]).splitlines())
        deny = lines["HOT_GUEST_DENY_CIDRS"].split(",")
        for cidr in ("10.1.2.0/24", "10.1.3.0/24", "10.1.2.10/32"):
            self.assertIn(cidr, deny)
        # Guests have no IPv6.
        self.assertNotIn("fd00:1::/48", deny)

    def test_data_networks_are_checked_as_the_contract_checks_them(self):
        config = renderer.validate(example())

        def with_networks(networks, mode="existing"):
            return dict(config, data={"postgres": {"mode": mode, "endpoint": {"deny_cidrs": networks}}})
        for good in ("10.1.2.0/24", "10.1.4.5/32", "fd00:1::/48", "FD00:1::/48", "2600:1f18:0:1::1/128",
                     "::/95", "::1:0:0/96", "64:ff9b:1::/96", "fd00::a00:0/104"):
            self.assertEqual(renderer.validate(with_networks([good]))["data"]["postgres"]["endpoint"]["deny_cidrs"], [good])
        for bad in ("db.internal/24", "10.1.2.10", "10.1.2.10/24", "0.0.0.0/0", "::/0", "10.0.0.0/255.0.0.0", "10.0.0.0/33",
                    "fe80::1%eth0/64", "fd00:0001::/48", "::ffff:10.0.0.0/104", "", 10, None,
                    # IPv4 written in IPv6: Hot reads ::/96 as 0.0.0.0/0.
                    "::/96", "::/97", "64:ff9b::/96", "::a00:0/104", "64:FF9B::a14:105/128", "::ffff:a00:0/104"):
            with self.assertRaisesRegex(ValueError, "deny_cidrs must list", msg=bad):
                renderer.validate(with_networks(["10.1.2.0/24", bad]))
        for bad in ("10.1.2.0/24", {"10.1.2.0/24": True}, None):
            with self.assertRaisesRegex(ValueError, "deny_cidrs must list", msg=bad):
                renderer.validate(with_networks(bad))
        # The contract fills in an empty list for a local service, and refuses
        # anything else: its address is on the bundle's own network.
        self.assertEqual(renderer.validate(with_networks([], "local"))["data"]["postgres"]["endpoint"], {"deny_cidrs": []})
        with self.assertRaisesRegex(ValueError, "not local ones"):
            renderer.validate(with_networks(["10.1.2.0/24"], "local"))
        # A stack writes null for a local service's endpoint.
        local = renderer.validate(dict(config, data={"postgres": {"mode": "local", "endpoint": None}}))
        self.assertEqual(renderer.user_code_deny(local), renderer.user_code_deny(config))
        # A key nothing reads, on a service the stack does not provision: as a
        # typo it would leave the network open without a word.
        for mode in ("local", "existing"):
            for endpoint in ({"deny_cidr": ["10.1.2.0/24"]}, {"host": "db.example", "deny_cidrs": []}):
                with self.assertRaisesRegex(ValueError, "takes only deny_cidrs", msg=(mode, endpoint)):
                    renderer.validate(dict(config, data={"postgres": {"mode": mode, "endpoint": endpoint}}))
        # What a managed endpoint takes has only grown by this key.
        managed = self.data_networks(["10.1.2.0/24"], ["10.1.3.0/24"])
        for service in ("postgres", "valkey"):
            data = copy.deepcopy(managed["data"])
            data[service]["endpoint"]["extra"] = "x"
            with self.assertRaisesRegex(ValueError, "needs the endpoint", msg=service):
                renderer.validate(dict(managed, data=data))
        with self.assertRaisesRegex(ValueError, "endpoint must be an object"):
            renderer.validate(dict(config, data={"postgres": {"mode": "existing", "endpoint": "db.example"}}))

    def test_allowlisted_networks_are_ones_hot_can_parse(self):
        config = renderer.validate(example())
        for bad in ("nope", "vault.internal", "10.0.0.0/255.0.0.0", "10.0.0.0/0.255.255.255", "fe80::1%eth0", "10.0.0.0/33"):
            with self.assertRaisesRegex(ValueError, "HOT_NETWORK_ALLOW_CIDRS entries", msg=bad):
                renderer.validate(dict(config, settings={"HOT_NETWORK_ALLOW_CIDRS": f"10.50.0.0/16,{bad}"}))
        # As conf_cidrs reads them: a bare address, host bits, IPv6, spaces, and
        # empty entries.
        renderer.validate(dict(config, settings={"HOT_NETWORK_ALLOW_CIDRS": "10.50.0.10, 10.50.0.11/24,2600::1,,"}))

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
            # Box guests run outside every container's limit, so each container
            # slot's default box counts against the VM beside the caps.
            slots = int(bundle["services"]["task-worker"]["environment"]["HOT_TASK_CONTAINER_MAX_CONCURRENT"])
            self.assertLessEqual(total + slots * renderer.BOX_HOST_MB, renderer.VM_MEMORY_MB[size] - renderer.HOST_RESERVE_MB, size)
            task = int(bundle["services"]["task-worker"]["environment"]["HOT_TASK_WORKER_MEMORY_MB"])
            self.assertEqual(f"{task}m", bundle["services"]["task-worker"]["mem_limit"])
            # A single machine's worker shares the VM with everything else.
            worker = bundle["services"]["worker"]["environment"]
            self.assertEqual(worker["HOT_WORKER_THREADS"], "1")
            self.assertEqual(worker["HOT_ENGINE_THREADS"], str(renderer.SIZES[size]["engine_threads"]))
            if previous:
                self.assertGreater(total, previous)
            previous = total
            maxmemory = int(renderer.valkey_conf(size).split("maxmemory ")[1].split("mb")[0])
            self.assertLess(maxmemory, int(bundle["services"]["valkey"]["mem_limit"].rstrip("m")))

    def test_the_box_allowance_follows_kata_and_the_task_worker(self):
        host = (ROOT / "images/host/setup-host.sh").read_text()
        limits = (ROOT.parent / "crates/hot_task_worker/src/box_limits.rs").read_text()
        executor = (ROOT.parent / "crates/hot_task_worker/src/executor.rs").read_text()
        self.assertEqual(int(re.search(r"^default_memory = (\d+)$", host, re.M)[1]), renderer.KATA_GUEST_BASE_MB)
        self.assertEqual(int(re.search(r"DEFAULT_MEMORY_MB: u64 = (\d+);", limits)[1]), renderer.BOX_MEMORY_MB)
        # A guest's RAM is default_memory plus the box's limit, and /tmp is a
        # tmpfs in that RAM, so the allowance has no separate room for it.
        self.assertIn("static_sandbox_resource_mgmt = true", host)
        self.assertRegex(executor, r'"destination": "/tmp",\s*"type": "tmpfs",\s*"source": "tmpfs"')

    def test_hot_admits_the_boxes_each_size_leaves_room_for(self):
        # Hot (runtime_budget.rs) holds back the process's memory and a reserve
        # from the task worker's, and splits the rest. A default box counts its
        # memory and /tmp against the boxes' share and fails at once if it could
        # never fit; a code VM counts 256 MB against the other, with at least one.
        limits = (ROOT.parent / "crates/hot_task_worker/src/box_limits.rs").read_text()
        tmp = int(re.search(r"DEFAULT_TMP_SIZE_MB: u64 = (\d+);", limits)[1])
        budget = (ROOT.parent / "crates/hot/src/runtime_budget.rs").read_text()
        process = int(re.search(r"TASK_PROCESS_RESERVE_MB: u64 = (\d+);", budget)[1])
        # The environment finds these two only through the defaults' keys.
        conf = (ROOT.parent / "crates/hot_cli/src/conf.rs").read_text()
        self.assertIn('"code-memory-percent": 50i64', conf)
        self.assertIn('"container-reserved-memory-mb": 512i64', conf)

        def shares(config, size):
            env = renderer.render(dict(config, size=size), manifest(), "/data", "/run/hot/secrets")["services"]["task-worker"]["environment"]
            usable = int(env["HOT_TASK_WORKER_MEMORY_MB"]) - process - int(env["HOT_TASK_CONTAINER_RESERVED_MEMORY_MB"])
            code = usable * int(env["HOT_TASK_CODE_MEMORY_PERCENT"]) // 100
            return usable - code, max(code // 256, 1), env

        single, (task_host, _) = renderer.validate(example()), self.scalable(["task-worker"])
        for size in renderer.SIZES:
            for config in (single, task_host):
                with self.subTest(size=size, preset=config.get("preset", "single-machine")):
                    boxes, vms, env = shares(config, size)
                    self.assertGreaterEqual(boxes, int(env["HOT_TASK_CONTAINER_MAX_CONCURRENT"]) * (renderer.BOX_MEMORY_MB + tmp))
                    if config is not single or size != "small":
                        self.assertGreaterEqual(vms, int(env["HOT_TASK_CODE_MAX_CONCURRENT"]))

        # Small gives its one box everything the cap leaves: a default box and the
        # profiles up to medium run, large is refused, and one code VM runs.
        boxes, vms, _ = shares(single, "small")
        self.assertEqual((boxes, vms), (1024, 1))

        def profile(name):
            found = re.search(rf"Self::{name} => SizeProfile \{{\s*memory_mb: ([\d_]+),\s*cpu_quota: [\d_]+,\s*tmp_size_mb: ([\d_]+),", limits)
            return int(found[1].replace("_", "")) + int(found[2].replace("_", ""))

        self.assertLessEqual(profile("Medium"), boxes)
        self.assertGreater(profile("Large"), boxes)

    def test_hot_admits_each_sizes_container_slots_on_disk(self):
        # Hot refuses a box at once, whatever its memory, when it counts for more
        # than the disk budget: the task worker's disk less a recovery reserve. A
        # box counts its /data, a quarter more, and image and snapshot overhead.
        # /data is on the data disk beside the pool, so both have to fit there.
        # Where a local PostgreSQL or Valkey shares that disk, the pool's data file
        # is allocated in full (setup-host.sh), so the pool, the budget, and the
        # pool's sparse metadata at its full size (a tenth of the pool, plus 1 GB)
        # stay under the disk alarm and leave the database room. A scalable task
        # host's disk holds no database, and its pool stays sparse.
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        alarms = [re.findall(r'"(?:DataDiskUsedPercent|data_disk_used_percent)".*threshold = (\d+)',
                             (ROOT / f"opentofu/stacks/{stack}/single-machine/monitoring.tf").read_text())
                  for stack in ("aws", "gcp")]
        # One alarm per stack, and both the same, so a pattern that stops matching fails.
        self.assertTrue(all(len(found) == 1 for found in alarms), alarms)
        self.assertEqual(alarms[0], alarms[1])
        alarm = int(alarms[0][0])
        # The agent reports 1 - f_bavail / f_blocks. setup-host.sh runs mkfs.ext4
        # with its defaults, so ext4 reserves 5% of the disk, which counts as used,
        # and f_blocks leaves out the inode tables (256 bytes per 16 KiB) and the
        # journal (at most 1 GiB). Files can fill this much before the alarm fires.
        self.assertIn('mkfs.ext4 -q -L hot-data "$real"', (ROOT / "images/host/setup-host.sh").read_text())
        def alarm_mb(data_gb):
            total = data_gb * 1024
            return (total - total * 256 // 16384 - 1024) * alarm // 100 - total * 5 // 100
        source = ROOT.parent / "crates/hot_task_worker/src"
        data = int(re.search(r"DEFAULT_DISK_SIZE_MB: u64 = ([\d_]+);", (source / "box_limits.rs").read_text())[1].replace("_", ""))
        overhead = int(re.search(r"OVERHEAD_MB: u64 = ([\d_]+);", (source / "resource_budget.rs").read_text())[1].replace("_", ""))
        template = (ROOT.parent / "resources/init/hot.hot.task-worker.template").read_text()
        reserve = int(re.search(r'"HOT_TASK_CONTAINER_RESERVED_DISK_MB", "(\d+)"', template)[1])
        pools = json.loads(re.search(r"^BOX_POOL_GB = (\{.*\})$", (ROOT / "agent/hot_agent.py").read_text(), re.M)[1])
        single, task_host = renderer.validate(example()), self.scalable(["task-worker"])[0]
        self.assertTrue(all(renderer.data_mode(single, service) == "local" for service in ("postgres", "valkey")))
        self.assertTrue(all(renderer.data_mode(task_host, service) != "local" for service in ("postgres", "valkey")))
        for size, pool_gb in pools.items():
            for config in (single, task_host):
                with self.subTest(size=size, preset=config.get("preset", "single-machine")):
                    bundle = renderer.render(dict(config, size=size), manifest(), "/data", "/run/hot/secrets")
                    env = bundle["services"]["task-worker"]["environment"]
                    disk = int(env["HOT_TASK_WORKER_DISK_MB"])
                    slots = int(env["HOT_TASK_CONTAINER_MAX_CONCURRENT"])
                    self.assertGreaterEqual(disk - reserve, slots * (data + data // 4 + overhead))
                    self.assertLessEqual(disk, pool_gb * 1024)
                    data_gb = int(re.search(rf"{size}\s*=\s*\{{[^}}]*data_gb = (\d+)", contract)[1])
                    self.assertLessEqual(disk + pool_gb * 1024, data_gb * 1024)
                    if config is single:
                        meta_gb = pool_gb // 10 + 1
                        self.assertLess(disk + (pool_gb + meta_gb) * 1024, alarm_mb(data_gb))

    def test_box_data_is_on_the_data_disk_where_kata_can_see_it(self):
        # The data root the agent renders with.
        root = "/var/lib/hot/data"
        self.assertTrue('render.render(self.config, manifest, str(HOT_ROOT / "data")' in (ROOT / "agent/hot_agent.py").read_text())
        for name, config in (("single-machine", example()), ("scalable", self.scalable(["task-worker"])[0])):
            task = renderer.render(config, manifest(), root, "/run/hot/secrets")["services"]["task-worker"]
            path = f"{root}/{renderer.BOX_DATA_DIRECTORY}"
            self.assertEqual(task["environment"]["HOT_TASK_DATA_VOLUME_DIR"], path, name)
            # Kata's shim resolves the same path on the host, and the worker's loop
            # mounts reach it only with shared propagation.
            self.assertIn({"type": "bind", "source": path, "target": path, "read_only": False,
                           "bind": {"create_host_path": False, "propagation": "rshared"}}, task["volumes"], name)
            # Not under a tmpfs, which would hold it in the container's memory.
            self.assertFalse(any(path.startswith(entry.split(":")[0] + "/") for entry in task["tmpfs"]), name)
            # Those mounts each take a loop device's node, which may be newer than
            # the container, so its /dev is the host's. No propagation key: only the
            # box data directory's mounts have to reach the host.
            self.assertIn({"type": "bind", "source": "/dev", "target": "/dev", "read_only": False,
                           "bind": {"create_host_path": False}}, task["volumes"], name)
        # The other roles make no boxes, and hold no mount that reaches the host.
        for role, service in renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"].items():
            if role != "task-worker":
                self.assertNotIn("HOT_TASK_DATA_VOLUME_DIR", service.get("environment", {}), role)
                self.assertNotIn("rshared", json.dumps(service.get("volumes", [])), role)
                self.assertNotIn('"/dev"', json.dumps(service.get("volumes", [])), role)
        # What Hot's configuration and the host each need for this to take effect:
        # the environment name's key, the key the worker reads, and the directory,
        # since a bind's source must exist.
        self.assertRegex((ROOT.parent / "crates/hot_cli/src/conf.rs").read_text(),
                         r'\("HOT_TASK_DATA_VOLUME_DIR",\s*"task\.data-volume-dir"\)')
        self.assertIn('t.get("data-volume-dir")', (ROOT.parent / "crates/hot_cli/src/command/worker.rs").read_text())
        self.assertTrue("render.BOX_DATA_DIRECTORY" in (ROOT / "agent/hot_agent.py").read_text(),
                        "the agent makes the directory before the task worker binds it")

    def test_every_service_uses_the_store_its_database_can_hold(self):
        # Hot's default is SQLite, whose store refuses a PostgreSQL pool, and the
        # bundle only ever hands out postgres:// URIs.
        self.assertIn('std::env::var("HOT_STORE_TYPE")', (ROOT.parent / "crates/hot/src/store/mod.rs").read_text())
        _, scalable = self.scalable(["worker", "task-worker"])
        for bundle, role_names in ((renderer.render(example(), manifest(), "/data", "/run/hot/secrets"), renderer.SERVICES),
                                   (scalable, ("worker", "task-worker"))):
            for role in role_names:
                self.assertEqual(bundle["services"][role]["environment"]["HOT_STORE_TYPE"], "postgres", role)

    def test_vm_sizes_match_the_contract(self):
        text = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        for size in renderer.VM_VCPUS:
            sized = re.search(rf"{size}\s*=\s*\{{[^}}]*memory_mb = (\d+), cpus = (\d+)", text)
            self.assertEqual((int(sized[1]), int(sized[2])), (renderer.VM_MEMORY_MB[size], renderer.VM_VCPUS[size]), size)

    def test_existing_data_services_leave_out_their_containers(self):
        config = dict(renderer.validate(example()), data={"postgres": {"mode": "existing", "runtime_role": "hot_app"},
                                                          "valkey": {"mode": "existing"}})
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets", extra_deny=["10.9.8.7/32"])
        services = bundle["services"]
        self.assertNotIn("postgres", services)
        self.assertNotIn("valkey", services)
        self.assertEqual(services["app"]["depends_on"], {})
        self.assertIn("check-postgres", services)
        # Connections come from service files: never an argument, which any
        # process on the host can read, or an environment value.
        for name in ("provision", "check-postgres", "restore-postgres"):
            service = services[name]
            self.assertEqual(service["environment"], {"PGSERVICEFILE": "/run/secrets/HOT_DB_MIGRATION_SERVICE"}, name)
            self.assertIn("HOT_DB_MIGRATION_SERVICE", {s["target"] for s in service["secrets"]}, name)
            self.assertNotIn("HOT_DB_MIGRATION_URI", {s["target"] for s in service["secrets"]}, name)
            self.assertNotIn("$(cat", json.dumps(service), name)
        for name in ("provision", "check-postgres"):
            self.assertEqual(services[name]["entrypoint"], ["psql", "--dbname=service=hot"])
        self.assertNotIn("$uri", renderer.RESTORE_POSTGRES_SH)
        self.assertIn("--dbname=service=hot /move/postgres.dump", renderer.RESTORE_POSTGRES_SH)
        self.assertIn("--set=runtime_role=hot_app", services["provision"]["command"])
        self.assertIn("app-POSTGRES_CA_CERT", [s["source"] for s in services["app"]["secrets"]])
        self.assertNotIn("POSTGRES_CA_CERT_FILE", services["app"]["environment"])
        self.assertIn("10.9.8.7/32", services["worker"]["environment"]["HOT_NETWORK_DENY_CIDRS"])
        with self.assertRaises(ValueError):
            renderer.validate(dict(config, data={"postgres": {"mode": "managed"}}))
        for role in ("x; drop", "hot_owner"):
            with self.assertRaises(ValueError):
                renderer.validate(dict(config, data={"postgres": {"mode": "existing", "runtime_role": role}}))
        # A local database always creates and logs in as hot_runtime.
        with self.assertRaisesRegex(ValueError, "local database's runtime login is hot_runtime"):
            renderer.validate(dict(config, data={"postgres": {"mode": "local", "runtime_role": "hot_app"}}))
        renderer.validate(dict(config, data={"postgres": {"mode": "local", "runtime_role": "hot_runtime"}}))

    def test_service_files_carry_the_whole_connection(self):
        uri = ("postgres://hot%40owner:p%40ss%3Aw%2Fo%23rd@db.example:6543/hot"
               "?sslmode=verify-full&sslrootcert=%2Frun%2Fsecrets%2FPOSTGRES_CA_CERT")
        self.assertEqual(renderer.pg_service(uri).splitlines(), [
            "[hot]", "host=db.example", "port=6543", "dbname=hot", "user=hot@owner", "password=p@ss:w/o#rd",
            "sslmode=verify-full", "sslrootcert=/run/secrets/POSTGRES_CA_CERT"])
        self.assertIn("host=10.0.0.5\n", renderer.pg_service("postgresql://u:p@10.0.0.5/postgres"))
        # libpq decodes only %XX in a URI, so a + is a + and not a space.
        plus = renderer.pg_service("postgres://u:p+q@db/hot?application_name=a+b&password=c+d&options=-c%20x%3D1")
        self.assertEqual(plus.splitlines(), ["[hot]", "host=db", "dbname=hot", "user=u", "password=c+d",
                                             "application_name=a+b", "options=-c x=1"])
        self.assertIn("password=p+q\n", renderer.pg_service("postgres://u:p+q@db/hot"))
        # A trailing & is no parameter.
        self.assertIn("sslmode=verify-full\n", renderer.pg_service("postgres://u:p@db/hot?sslmode=verify-full&"))
        for bad in ("mysql://u:p@db/hot", "postgres://u:p%0Aq@db/hot", "postgres://u:%20p@db/hot",
                    "postgres://u:p@db/hot?service=other", "postgres://u:p%00q@db/hot", "postgres://u:%FF@db/hot",
                    "postgres://u:p@db/hot?=value",
                    # The agent's check reads verify-full in the first three, and
                    # a file without sslmode connects with libpq's default.
                    "postgres://u:p@db/hot?sslmode=verify-full&sslmode=",
                    "postgres://u:p@db/hot?sslmode=verify-full&ssl%6Dode=",
                    "postgres://u:p@db/hot?sslmode=disable&sslmode=verify-full",
                    "postgres://u:p@db/hot?sslmode", "postgres://u:p@db/hot?host=", "postgres://u:p@db/hot?password="):
            with self.assertRaises(ValueError, msg=bad):
                renderer.pg_service(bad)

    @unittest.skipUnless(shutil.which("docker") and POSTGRES_IMAGE and POSTGRES_URI,
                         "set HOT_INFRA_TEST_POSTGRES_IMAGE and HOT_INFRA_TEST_POSTGRES_URI to run psql")
    def test_psql_connects_through_a_service_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            service = Path(tmp) / "HOT_DB_MIGRATION_SERVICE"
            service.write_text(renderer.pg_service(POSTGRES_URI))
            service.chmod(0o644)
            result = docker("run", "--rm", "--pull", "never", "--add-host", "host.docker.internal:host-gateway",
                            "-v", f"{service}:/run/secrets/HOT_DB_MIGRATION_SERVICE:ro",
                            "-e", "PGSERVICEFILE=/run/secrets/HOT_DB_MIGRATION_SERVICE",
                            "--entrypoint", "psql", POSTGRES_IMAGE, "--dbname=service=hot", "--no-psqlrc",
                            "--tuples-only", "--no-align", "--command=SELECT 1")
            self.assertEqual((result.returncode, result.stdout.strip()), (0, "1"), result.stderr)

    def test_the_migration_status_job_reads_with_migrates_login(self):
        managed = self.data_networks([], [])
        for name, config in (("local", example()), ("managed", managed)):
            bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
            job, migrate = bundle["services"]["migration-status"], bundle["services"]["migrate"]
            self.assertEqual(job["command"], ["db", "migrate", "--status"], name)
            self.assertEqual(job["profiles"], ["admin"], name)
            # The same login from the same files, so nothing else is delivered,
            # and for a local database it starts PostgreSQL the same way.
            self.assertEqual(job["secrets"], migrate["secrets"], name)
            self.assertEqual(bundle["secrets"]["migrate-HOT_DB_MIGRATION_URI"]["file"],
                             "/run/hot/secrets/migrate/HOT_DB_MIGRATION_URI", name)
            for key in ("image", "platform", "user", "read_only", "tmpfs", "cap_drop", "security_opt", "depends_on"):
                self.assertEqual(job[key], migrate[key], (name, key))
            # Hot reads its URI and the ledger through this login alone.
            env = job["environment"]
            self.assertEqual(env["HOT_DB_URI_FILE"], "/run/secrets/HOT_DB_MIGRATION_URI", name)
            self.assertEqual(env["HOT_DB_MIGRATION_URI_FILE"], "/run/secrets/HOT_DB_MIGRATION_URI", name)
            # The agent reads stdout, which holds only the status with no logs.
            self.assertEqual(env["HOT_LOG_TARGET"], "none", name)
            self.assertNotIn("HOT_LOG_TARGET", migrate["environment"], name)
            self.assertEqual(migrate["command"], ["db", "migrate"], name)
            self.assertEqual(job["labels"]["hot.role"], "migration-status", name)

    def test_managed_postgres_is_prepared_by_its_administrator(self):
        endpoint = {"host": "hot-example.c0123.us-east-1.rds.amazonaws.com", "port": 5432, "admin_user": "hot_admin",
                    "admin_secret": "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123"}
        config = dict(renderer.validate(example()), data={"postgres": {"mode": "managed", "endpoint": endpoint}})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertNotIn("postgres", services)
        self.assertIn("valkey", services)
        prepare = services["prepare-postgres"]
        self.assertEqual(prepare["entrypoint"], ["psql", "--dbname=service=hot"])
        self.assertEqual(prepare["environment"], {"PGSERVICEFILE": "/run/secrets/POSTGRES_ADMIN_SERVICE"})
        self.assertEqual({s["target"] for s in prepare["secrets"]},
                         {"POSTGRES_ADMIN_SERVICE", "POSTGRES_ADMIN_HOT_URI", "POSTGRES_PASSWORD",
                          "POSTGRES_RUNTIME_PASSWORD", "POSTGRES_CA_CERT"})
        # Its second connection is psql's own (\connect), never an argument.
        self.assertIn("\\connect :hot_uri", renderer.PREPARE_POSTGRES_SQL)
        # Only the prepare job holds the administrator login.
        for name, service in services.items():
            if name != "prepare-postgres":
                self.assertFalse({"POSTGRES_ADMIN_SERVICE", "POSTGRES_ADMIN_HOT_URI"}
                                 & {s["target"] for s in service.get("secrets", [])}, name)
        self.assertIn("check-postgres", services)
        self.assertIn("app-POSTGRES_CA_CERT", [s["source"] for s in services["app"]["secrets"]])
        # A Cloud SQL endpoint also carries its instance CA.
        gcp = dict(renderer.validate(example("gcp")), data={"postgres": {"mode": "managed", "endpoint": dict(
            endpoint, admin_secret="projects/p/secrets/s")}})
        with self.assertRaisesRegex(ValueError, "ca_certificate"):
            renderer.validate(gcp)
        with self.assertRaisesRegex(ValueError, "Managed Valkey needs the endpoint"):
            renderer.validate(dict(config, data={"valkey": {"mode": "managed"}}))
        # Memorystore needs its CA and IAM tokens, not an AUTH secret.
        cache = {"mode": "managed", "endpoint": {"host": "cache.example", "port": 6379, "auth_secret": "projects/p/secrets/v"}}
        with self.assertRaisesRegex(ValueError, "auth, ca_certificate, host, port"):
            renderer.validate(dict(renderer.validate(example("gcp")), data={"valkey": cache}))

    def test_memorystore_uses_iam_tokens_and_its_ca(self):
        endpoint = {"host": "10.68.0.40", "port": 6379, "auth": "gcp-iam",
                    "ca_certificate": "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n"}
        config = dict(renderer.validate(example("gcp")), data={"valkey": {"mode": "managed", "endpoint": endpoint}})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertNotIn("valkey", services)
        for role in ("app", "api", "worker", "task-worker", "scheduler"):
            env = services[role]["environment"]
            self.assertEqual(env["HOT_REDIS_AUTH"], "gcp-iam", role)
            self.assertEqual(env["HOT_REDIS_CA_CERT_FILE"], "/run/secrets/HOT_REDIS_CA_CERT", role)
            self.assertIn(f"{role}-HOT_REDIS_CA_CERT", [s["source"] for s in services[role]["secrets"]])
            # Hot exits on a setting beside its _FILE.
            self.assertFalse({key for key in env if key + "_FILE" in env}, role)
        with self.assertRaisesRegex(ValueError, "gcp-iam"):
            renderer.validate(dict(config, data={"valkey": {"mode": "managed", "endpoint": dict(endpoint, auth="password")}}))
        # The bundle sets both, and Hot exits on the CA setting beside its _FILE.
        for key, value in (("HOT_REDIS_CA_CERT", endpoint["ca_certificate"]), ("HOT_REDIS_AUTH", "password")):
            with self.assertRaisesRegex(ValueError, f"{key} is set by the runtime bundle"):
                renderer.validate(dict(config, settings={key: value}))
        with self.assertRaisesRegex(ValueError, "HOT_REDIS_CLUSTER"):
            renderer.validate(dict(config, settings={"HOT_REDIS_CLUSTER": "true"}))
        # Elsewhere the CA is the operator's to set.
        for other in (dict(config, data={"valkey": {"mode": "existing"}}), renderer.validate(example("aws"))):
            renderer.validate(dict(other, settings={"HOT_REDIS_CA_CERT": endpoint["ca_certificate"]}))

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
        config = dict(renderer.validate(example("gcp")), api_exposure="public", load_balancer_addresses=["34.120.5.9"])
        bundle = renderer.render(config, manifest(), "/data", "/run/hot/secrets")
        self.assertIn("9443:9443", bundle["services"]["ingress"]["ports"])
        proxies = bundle["services"]["api"]["environment"]["HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES"].split(",")
        self.assertIn("130.211.0.0/22", proxies)
        # Google's load balancer appends its own address after the client's.
        self.assertIn("34.120.5.9/32", proxies)
        self.assertNotIn("34.120.5.9", proxies)
        self.assertEqual(resolve_client(["203.0.113.7", "34.120.5.9", "35.191.10.20"], "172.30.0.5", proxies), "203.0.113.7")
        self.assertIn("2600:1901:0:1::5/128", renderer.trusted_proxies(dict(config, load_balancer_addresses=["2600:1901:0:1::5"])))
        for bad in ("34.120.5.0/24", "34.120.5.9/32", "fe80::1%eth0", 1):
            with self.assertRaisesRegex(ValueError, "load_balancer_addresses", msg=bad):
                renderer.validate(dict(config, load_balancer_addresses=[bad]))
        text = renderer.nginx_config(config)
        lb = text.split("listen 9443 ssl;")
        self.assertEqual(len(lb), 2, "only the public endpoint is served to the load balancer")
        self.assertIn(config["api_hostname"], lb[1].split("}")[0])
        self.assertIn("proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;", lb[1].split("}")[0])
        self.assertIn("return 444", text)
        # Only the load balancer may connect to its port: user code calling
        # the host's own address arrives from the bundle's network.
        for block in text.split("server {")[1:]:
            listen = "listen 9443" in block
            self.assertEqual(f"deny {renderer.RUNTIME_SUBNET};" in block, listen, block)
        with self.assertRaises(ValueError):
            renderer.validate(dict(config, app_exposure="internet"))
        with self.assertRaisesRegex(ValueError, "set by the runtime bundle"):
            renderer.render(dict(config, settings={"HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES": "0.0.0.0/0"}),
                            manifest(), "/data", "/run/hot/secrets")

    def test_an_aws_load_balancer_is_trusted_from_its_own_subnets(self):
        subnets = ["172.31.0.0/20", "172.31.16.0/20"]
        config = dict(renderer.validate(example()), app_exposure="public", load_balancer_cidrs=subnets,
                      network={"mode": "existing", "network_id": "vpc-1", "subnet_id": "subnet-1",
                               "public_subnet_ids": ["subnet-2", "subnet-3"]})
        proxies = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]["app"][
            "environment"]["HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES"]
        # An existing VPC's range is not network.cidr, which is left at its default.
        self.assertEqual(proxies, "172.30.0.0/24,172.31.0.0/20,172.31.16.0/20")
        self.assertEqual(resolve_client(["203.0.113.7", "172.31.5.5"], "172.30.0.5", proxies.split(",")), "203.0.113.7")
        for bad in (["172.31.0.0"], ["0.0.0.0/0"], ["172.31.0.1/20"], ["fd00::/48"], ["172.31.0.0/255.255.240.0"], [1], [],
                    "172.31.0.0/20", None):
            with self.assertRaisesRegex(ValueError, "load_balancer_cidrs", msg=bad):
                renderer.validate(dict(config, load_balancer_cidrs=bad))
        # A configuration from before the stack passed it keeps network.cidr.
        older = {key: value for key, value in config.items() if key != "load_balancer_cidrs"}
        self.assertEqual(renderer.trusted_proxies(older), [renderer.RUNTIME_SUBNET, "10.68.0.0/16"])
        # Google's ranges are its own; a scalable installation trusts no hop.
        gcp = dict(renderer.validate(example("gcp")), app_exposure="public", load_balancer_cidrs=subnets)
        self.assertEqual(renderer.load_balancer_sources(gcp), list(renderer.GCP_LOAD_BALANCER_RANGES))
        self.assertEqual(renderer.trusted_proxies(dict(config, preset="scalable")), [renderer.RUNTIME_SUBNET])

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

    def test_the_task_worker_writes_its_slots_where_the_agent_reads_them(self):
        services = renderer.render(example("aws", "local"), manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertEqual(services["task-worker"]["environment"]["HOT_TASK_STATUS_PATH"], renderer.TASK_STATUS)
        self.assertTrue(renderer.TASK_STATUS.startswith("/tmp/"), "the worker's own writable /tmp")

    def test_the_task_worker_looks_for_the_releases_hotbox_on_the_host(self):
        # Kata's shim resolves a box's hotbox on the host, and the worker looks in
        # the same place through PID 1's mounts. A bind for it would be worse: its
        # source must exist before the worker can start (create_host_path false),
        # and a single-file bind keeps the old file after the agent replaces it.
        executor = (ROOT.parent / "crates/hot_task_worker/src/executor.rs").read_text()
        self.assertIn('const IN_HOST_MOUNTS: &[&str] = &["nsenter", "-t", "1", "-m", "--"];', executor)
        self.assertIn('const INSTALLED_HOTBOX: &str = "/var/lib/hot/kata/hotbox";', executor)
        _, scalable = self.scalable(["worker", "task-worker"])
        for name, bundle in (("single-machine", renderer.render(example(), manifest(), "/data", "/run/hot/secrets")),
                             ("scalable", scalable)):
            task = bundle["services"]["task-worker"]
            self.assertEqual(task["pid"], "host", name)
            self.assertTrue(task["privileged"], name)
            for volume in task["volumes"]:
                for path in (volume["source"], volume["target"]):
                    self.assertNotRegex(path, r"hotbox|/kata(/|$)", name)

    def test_a_stalled_scheduler_is_unhealthy(self):
        services = renderer.render(example("aws", "local"), manifest(), "/data", "/run/hot/secrets")["services"]
        scheduler = services["scheduler"]
        self.assertEqual(scheduler["environment"]["HOT_SCHEDULER_HEARTBEAT_PATH"], "/tmp/hot-scheduler-heartbeat")
        self.assertIn("/tmp:", scheduler["tmpfs"][0])
        check = scheduler["healthcheck"]["test"][1]
        self.assertIn("$$(cat /tmp/hot-scheduler-heartbeat", check)
        self.assertNotRegex(check.replace("$$", ""), r"\$", "every $ is escaped for Compose")
        self.assertNotIn("healthcheck", services["worker"])
        # The check itself: fresh passes, stale and missing fail.
        shell = check.replace("$$", "$")
        with tempfile.TemporaryDirectory() as tmp:
            beat = Path(tmp) / "beat"
            run = lambda: subprocess.run(["sh", "-c", shell.replace("/tmp/hot-scheduler-heartbeat", str(beat))]).returncode
            self.assertNotEqual(run(), 0)
            beat.write_text(str(int(time.time()) - 300))
            self.assertNotEqual(run(), 0)
            beat.write_text(str(int(time.time())))
            self.assertEqual(run(), 0)

    def scalable(self, roles, migrations=False):
        config = dict(renderer.validate(example("aws")), preset="scalable", roles=roles, migrations=migrations,
                      storage={"build": {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "builds"},
                               "file": {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"}},
                      data={"postgres": {"mode": "managed", "endpoint": {"host": "db.example", "port": 5432,
                                         "admin_user": "hot_admin", "admin_secret": "arn:aws:secretsmanager:us-east-1:1:secret:a"}},
                            "valkey": {"mode": "managed", "endpoint": {"host": "cache.example", "port": 6379,
                                       "auth_secret": "arn:aws:secretsmanager:us-east-1:1:secret:v"}}})
        return config, renderer.render(config, manifest(), "/data", "/run/hot/secrets")

    def test_a_scalable_host_runs_only_its_roles(self):
        config, bundle = self.scalable(["app"])
        self.assertEqual(set(bundle["services"]), {"app", "ingress"})
        self.assertEqual(bundle["services"]["ingress"]["ports"], [f"443:{renderer.LOAD_BALANCER_PORT}"])
        self.assertEqual(bundle["services"]["app"]["environment"]["HOT_WORKER_DB_MAX_CONNECTIONS"], "20")
        self.assertEqual(bundle["services"]["ingress"]["depends_on"], ["app"])
        self.assertTrue(all(name.startswith(("app-", "ingress-")) for name in bundle["secrets"]), bundle["secrets"])
        nginx = renderer.nginx_config(config)
        self.assertIn("set $hot_health_upstream http://app:4680/signin;", nginx)
        self.assertNotIn("http://api:4681", nginx)
        self.assertNotIn("listen 8443 ssl;", nginx)

    def test_scalable_clients_cannot_name_their_own_address(self):
        # Passthrough load balancers keep the client's address as Nginx's
        # peer, so no forwarded header is believed, public or not.
        for cloud in ("aws", "gcp"):
            config, bundle = self.scalable(["api"])
            config = dict(config, cloud=cloud, project="p", api_exposure="public")
            if cloud == "gcp":
                config["data"] = {"postgres": {"mode": "existing"}, "valkey": {"mode": "existing"}}
                config["storage"] = {"build": {"backend": "gcs", "bucket": "acme", "prefix": "builds"},
                                     "file": {"backend": "gcs", "bucket": "acme", "prefix": "files"}}
            api = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]["api"]
            proxies = api["environment"]["HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES"].split(",")
            self.assertEqual(proxies, [renderer.RUNTIME_SUBNET], cloud)
            nginx = renderer.nginx_config(config)
            self.assertIn("proxy_set_header X-Forwarded-For $remote_addr;", nginx)
            self.assertNotIn("$proxy_add_x_forwarded_for", nginx)
            # The agent checks readiness through 127.0.0.1:443, which is this port.
            self.assertNotIn("deny ", nginx)
            # nginx replaced the claim, so the header holds only the client.
            self.assertEqual(resolve_client(["10.68.5.5"], "172.30.0.4", proxies), "10.68.5.5")

    def test_scalable_roles_have_their_host_to_themselves(self):
        for size in ("small", "medium", "large"):
            for role in renderer.SERVICES:
                with self.subTest(size=size, role=role):
                    config, _ = self.scalable([role], migrations=role == "scheduler")
                    bundle = renderer.render(dict(config, size=size), manifest(), "/data", "/run/hot/secrets")
                    service = bundle["services"][role]
                    # No CPU cap below the VM, and a worker loop per vCPU, so
                    # host CPU can reach the groups' 60% scaling targets.
                    self.assertNotIn("cpus", service)
                    env = service["environment"]
                    vcpus = {"small": "2", "medium": "4", "large": "8"}[size]
                    self.assertEqual(env["HOT_WORKER_THREADS"], vcpus)
                    self.assertEqual(env["HOT_ENGINE_THREADS"], vcpus)
                    # Each loop's connection, and Hot's 4 reserved, fit the pool.
                    self.assertLessEqual(int(vcpus) + 4, int(env["HOT_WORKER_DB_MAX_CONNECTIONS"]))
                    caps = sum(int(s["mem_limit"].rstrip("m")) for s in bundle["services"].values() if "mem_limit" in s)
                    available = renderer.VM_MEMORY_MB[size] - renderer.HOST_RESERVE_MB
                    if role == "task-worker":
                        budget = int(env["HOT_TASK_WORKER_MEMORY_MB"])
                        self.assertEqual(f"{budget}m", service["mem_limit"])
                        slots = int(env["HOT_TASK_CONTAINER_MAX_CONCURRENT"])
                        self.assertGreater(budget, renderer.SIZES[size]["memory"]["task-worker"])
                        self.assertGreater(slots, renderer.SIZES[size]["task"]["container"])
                        # Box guests run outside the container, so each container
                        # slot's default box counts against the VM beside the cap.
                        caps += slots * renderer.BOX_HOST_MB
                        if size == "small":
                            # Past the plan in the worst case, keeping 512 MB
                            # rather than 1 GB, which is accepted (SCALABLE_TASK).
                            self.assertEqual(caps, available + 512)
                            continue
                    else:
                        self.assertGreaterEqual(caps, available - 256)
                    self.assertLessEqual(caps, available)

    def test_only_the_migration_host_carries_admin_jobs(self):
        _, scheduler = self.scalable(["scheduler"], migrations=True)
        self.assertEqual(set(scheduler["services"]) - {"migrate", "migration-status", "provision", "check-postgres", "prepare-postgres",
                                                      "restore-postgres", "bootstrap-admin"}, {"scheduler"})
        self.assertIn("migration-status", scheduler["services"])
        self.assertIn("healthcheck", scheduler["services"]["scheduler"])
        _, worker = self.scalable(["worker", "task-worker"])
        self.assertEqual(set(worker["services"]), {"worker", "task-worker"})
        self.assertTrue(worker["services"]["task-worker"]["privileged"])

    def test_scalable_installations_need_shared_data_and_storage(self):
        config, _ = self.scalable(["app"])
        with self.assertRaisesRegex(ValueError, "managed or existing"):
            renderer.validate(dict(config, data={}))
        with self.assertRaisesRegex(ValueError, "S3 or GCS"):
            renderer.validate(dict(config, storage={"build": {"backend": "local"}, "file": {"backend": "local"}}))
        with self.assertRaisesRegex(ValueError, "roles must name"):
            renderer.validate(dict(config, roles=["ingress"]))

    def test_sign_up_is_by_invitation_until_the_operator_opens_it(self):
        config = example("aws", "local")
        app = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]["app"]
        self.assertEqual(app["environment"]["HOT_APP_SIGNUP"], "invite")
        opened = renderer.render(dict(config["installation"], settings={"HOT_APP_SIGNUP": "open"}),
                                 manifest(), "/data", "/run/hot/secrets")["services"]["app"]
        self.assertEqual(opened["environment"]["HOT_APP_SIGNUP"], "open")

    def test_embedding_models_are_kept_on_the_data_disk(self):
        services = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]
        for role in renderer.SERVICES:
            env, volumes = services[role]["environment"], services[role]["volumes"]
            if role in renderer.MODEL_ROLES:
                self.assertEqual(env["HOT_STORE_MODELS_PATH"], renderer.MODELS_PATH, role)
                # The root task worker shares no directory with uid 10001, and
                # its own is not inside theirs.
                source = "/data/models-task-worker" if role == "task-worker" else "/data/models"
                self.assertEqual(services[role]["user"].split(":")[0] == "0", role == "task-worker", role)
                self.assertIn({"type": "bind", "source": source, "target": renderer.MODELS_PATH,
                               "read_only": False, "bind": {"create_host_path": False}}, volumes, role)
                # Not under the memory-backed .hot tmpfs.
                self.assertFalse(any(t.startswith(renderer.MODELS_PATH) for t in services[role]["tmpfs"]), role)
            else:
                self.assertNotIn("HOT_STORE_MODELS_PATH", env, role)
        with self.assertRaisesRegex(ValueError, "set by the runtime bundle"):
            renderer.render(dict(renderer.validate(example()), settings={"HOT_STORE_MODELS_PATH": "/tmp"}),
                            manifest(), "/data", "/run/hot/secrets")

    def test_operators_can_choose_the_embedding_model(self):
        # A smaller model is what a small install can use today if its worker
        # cannot hold the default one, so every role that embeds must get it.
        config = dict(renderer.validate(example()), settings={"HOT_STORE_EMBEDDING_MODEL": "bge-small-en-v1.5"})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        for role in renderer.MODEL_ROLES:
            self.assertEqual(services[role]["environment"]["HOT_STORE_EMBEDDING_MODEL"], "bge-small-en-v1.5", role)

    def test_extracted_bundles_are_kept_on_the_data_disk(self):
        services = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]
        for role, root in renderer.EXTRACTION_ROOTS.items():
            self.assertIn({"type": "bind", "source": "/data/" + renderer.EXTRACTION_DIRECTORIES[role],
                           "target": services[role]["working_dir"] + "/" + root,
                           "read_only": False, "bind": {"create_host_path": False}}, services[role]["volumes"], role)
        # The root task worker shares no directory with uid 10001.
        self.assertEqual(len(set(renderer.EXTRACTION_DIRECTORIES.values())), 2)
        self.assertEqual(set(renderer.EXTRACTION_DIRECTORIES), set(renderer.EXTRACTION_ROOTS))
        for role in set(renderer.SERVICES) - set(renderer.EXTRACTION_ROOTS):
            self.assertNotIn("entrypoint", services[role], role)
        # Where Hot extracts them.
        for path, root in (("crates/hot_worker/src/server.rs", ".hot/run"), ("crates/hot_task_worker/src/lib.rs", ".hot/task-worker")):
            self.assertIn(f'BundleExtractions::new("{root}")', (ROOT.parent / path).read_text(), path)

    def test_a_worker_starts_with_nothing_extracted(self):
        services = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertIn('ENTRYPOINT ["/usr/bin/hot"]', (ROOT / "images/containers/runtime.Dockerfile").read_text())
        for role, root in renderer.EXTRACTION_ROOTS.items():
            # As Compose reads it, with something that can run here for hot.
            command = [part.replace("$$", "$").replace("/usr/bin/hot", "echo") for part in services[role]["entrypoint"]]
            with tempfile.TemporaryDirectory() as tmp:
                extractions = Path(tmp) / root
                (extractions / "build-1/.hot/cache").mkdir(parents=True)
                (extractions / "build-1/manifest.hot").write_text("{}")
                (extractions / "build-1.lease").write_text("")
                (extractions / ".evicting-2").mkdir()
                # A one-off command, which can run beside the service.
                started = subprocess.run([*command, "db", "move-storage"], cwd=tmp, capture_output=True, text=True, check=True)
                self.assertEqual(started.stdout.strip(), "db move-storage", role)
                self.assertEqual(len(list(extractions.iterdir())), 3, role)
                started = subprocess.run([*command, *services[role]["command"]], cwd=tmp, capture_output=True, text=True, check=True)
                self.assertEqual(started.stdout.strip(), role)
                self.assertEqual(list(extractions.iterdir()), [], role)

    def test_operator_values_are_never_interpolated_by_compose(self):
        config = dict(renderer.validate(example()), admin_email="ops$team@example.com",
                      settings={"HOT_EMAIL_FROM_NAME": "Ops $USER ${HOME Team"})
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        self.assertEqual(services["api"]["environment"]["HOT_EMAIL_FROM_NAME"], "Ops $$USER $${HOME Team")
        self.assertIn("ops$$team@example.com", services["bootstrap-admin"]["command"])
        # The scheduler's check is shell for the container, not for Compose.
        self.assertIn("$$(cat", services["scheduler"]["healthcheck"]["test"][1])

    def test_storage_values_the_bundle_carries_have_no_dollar_to_escape(self):
        # The agent compares these with the deployed bundle's, where a $ would
        # be doubled and read as a storage move.
        s3 = {"backend": "s3", "bucket": "acme", "region": "us-east-1", "prefix": "files"}
        config = renderer.validate(example())
        for bad, message in (({"prefix": "$files"}, "storage prefix takes"), ({"region": "us$east"}, "S3 requires"),
                             ({"region": "US-EAST-1"}, "S3 requires"), ({"bucket": "ac$me"}, "S3 requires")):
            with self.assertRaisesRegex(ValueError, message, msg=bad):
                renderer.validate(dict(config, storage={"build": {"backend": "local"}, "file": {**s3, **bad}}))
        # A local backend's fields reach nothing, and a GCS backend reads no
        # region, so the contract's acceptance of these stands.
        ignored = dict(config, storage={"build": {"backend": "local", "prefix": "a$b", "region": "us$east"}, "file": s3})
        env = renderer.render(ignored, manifest(), "/data", "/run/hot/secrets")["services"]["worker"]["environment"]
        self.assertNotIn("$", "".join(value for key, value in env.items() if "_STORAGE_" in key or "_AWS_S3_" in key))
        gcp = renderer.validate(example("gcp"))
        renderer.validate(dict(gcp, storage={"build": {"backend": "local"}, "file": {
            "backend": "gcs", "bucket": "acme", "prefix": "files", "region": "us$east"}}))

    def test_a_release_needing_a_newer_renderer_is_refused(self):
        renderer.render(example(), dict(manifest(), minimum_renderer=renderer.RENDERER_VERSION), "/data", "/run/hot/secrets")
        for needed in (renderer.RENDERER_VERSION + 1, "1", True):
            with self.assertRaisesRegex(ValueError, "build a host image from the release's tag"):
                renderer.render(example(), dict(manifest(), minimum_renderer=needed), "/data", "/run/hot/secrets")

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

    def test_log_lines_name_their_host_role_and_release(self):
        aws = renderer.render(example("aws"), manifest(), "/data", "/run/hot/secrets", instance="i-0abc")["services"]
        gcp = renderer.render(example("gcp"), manifest(), "/data", "/run/hot/secrets", instance="hot-example-x1")["services"]
        self.assertEqual(aws["worker"]["logging"]["options"]["tag"], "i-0abc/{{.Name}}")
        self.assertEqual(gcp["worker"]["logging"]["options"]["labels"], "hot.installation,hot.role,hot.release")
        self.assertEqual(aws["worker"]["labels"], {"hot.installation": "hot-example", "hot.role": "worker", "hot.release": manifest()["release"]})

    def test_a_mirror_keeps_each_images_digest(self):
        config = dict(renderer.validate(example()), image_registry="123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror/")
        services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
        for role, service in (("hot", "worker"), ("postgres", "postgres"), ("ingress", "ingress")):
            self.assertEqual(services[service]["image"], f"123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror/{role}@sha256:" + "a" * 64)

    def test_nginx_config_has_safe_defaults_and_streaming(self):
        text = renderer.nginx_config(renderer.validate(example()))
        self.assertIn("ssl_reject_handshake on", text)
        self.assertIn("proxy_buffering off", text)
        self.assertIn("client_max_body_size 300m", text)
        self.assertNotIn("$request_uri", text)
        self.assertNotIn("$http_authorization", text)
        # A caller's short request ID reaches Hot and the access log; Nginx
        # makes one only for a request without a token Hot would keep.
        self.assertIn('"~^[A-Za-z0-9._-]{1,64}$" $http_x_request_id;', text)
        self.assertIn("default $request_id;", text)
        self.assertNotIn("X-Request-ID $request_id", text)
        self.assertIn("proxy_set_header X-Request-ID $hot_request_id;", text)
        self.assertIn('"request_id":"$hot_request_id"', text)

    def test_the_api_knows_its_own_hostname(self):
        # Hot's API takes any other Host for a custom domain and answers 404,
        # on /status too, so neither readiness nor a request would ever pass.
        self.assertIn('"HOT_API_KNOWN_HOSTS"', (ROOT.parent / "crates/hot_api/src/domain_resolver.rs").read_text())
        for cloud in ("aws", "gcp"):
            config = renderer.validate(example(cloud))
            services = renderer.render(config, manifest(), "/data", "/run/hot/secrets")["services"]
            self.assertEqual(services["api"]["environment"]["HOT_API_KNOWN_HOSTS"], config["api_hostname"], cloud)
            for role in ("app", "worker", "scheduler", "task-worker"):
                self.assertNotIn("HOT_API_KNOWN_HOSTS", services[role]["environment"], role)
        config, bundle = self.scalable(["api"])
        self.assertEqual(bundle["services"]["api"]["environment"]["HOT_API_KNOWN_HOSTS"], config["api_hostname"])

    def test_the_load_balancer_health_check_arrives_as_a_real_request(self):
        def health(config):
            return re.search(r"location = /hot-lb-health \{(.*?)\}", renderer.nginx_config(config), re.DOTALL).group(1)

        # Without a Host, the API would see api:4681, which it doesn't know.
        public = dict(renderer.validate(example()), api_exposure="public")
        for config in (public, dict(public, api_exposure="private", app_exposure="public"), self.scalable(["api"])[0]):
            self.assertIn("set $hot_health_upstream http://api:4681/status;", health(config))
            self.assertIn("proxy_pass $hot_health_upstream;", health(config))
            self.assertIn(f"proxy_set_header Host {config['api_hostname']};", health(config))
        config = self.scalable(["app"])[0]
        self.assertIn("set $hot_health_upstream http://app:4680/signin;", health(config))
        self.assertIn("proxy_pass $hot_health_upstream;", health(config))
        self.assertIn(f"proxy_set_header Host {config['app_hostname']};", health(config))

    def test_nginx_looks_up_upstreams_while_serving(self):
        # A crash restart can move the app or API, and the agent only recreates
        # the ingress on a start or restart it runs itself.
        public = dict(renderer.validate(example()), app_exposure="public", api_exposure="public")
        for config in (renderer.validate(example()), public, self.scalable(["api"])[0]):
            text = renderer.nginx_config(config)
            self.assertIn("resolver 127.0.0.11 valid=10s ipv6=off;", text)
            targets = re.findall(r"proxy_pass (\S+);", text)
            self.assertTrue(targets)
            self.assertLessEqual(set(targets), {"$hot_upstream", "$hot_health_upstream"})
        # Each hostname still goes to its own role.
        text = renderer.nginx_config(renderer.validate(example()))
        for role, port in (("app", 4680), ("api", 4681)):
            self.assertIn(f"set $hot_upstream http://{role}:{port};\n    proxy_pass $hot_upstream;", text)

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
        # The ACL turns the default user off, so the check logs in as Hot does.
        self.assertIn(f"--user {renderer.VALKEY_USER} ", test[1])
        self.assertIn("user default off\n", renderer.valkey_acl("x"))
        self.assertIn(f"\nuser {renderer.VALKEY_USER} on >x ", renderer.valkey_acl("x"))

    @unittest.skipUnless(shutil.which("docker") and VALKEY_IMAGE, "set HOT_INFRA_TEST_VALKEY_IMAGE to run Valkey")
    def test_valkey_healthcheck_passes_in_a_container(self):
        bundle = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")
        healthcheck = bundle["services"]["valkey"]["healthcheck"]["test"][1]
        with tempfile.TemporaryDirectory() as tmp:
            # As the agent writes them: the ACL, and the password without a newline.
            files = {
                "valkey.conf": renderer.VALKEY_CONF,
                "VALKEY_ACL": renderer.valkey_acl("healthcheck-test"),
                "VALKEY_PASSWORD": "healthcheck-test",
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
                # Earlier checks sent nothing, or logged in as the disabled
                # default user, so they never matched.
                for unfixed in (healthcheck.replace(" PING <", " <"), healthcheck.replace(f"--user {renderer.VALKEY_USER} ", "")):
                    self.assertNotEqual(docker("exec", container, "sh", "-c", unfixed).returncode, 0, unfixed)
            finally:
                docker("rm", "-f", container)

    @unittest.skipUnless(shutil.which("docker") and shutil.which("openssl") and NGINX_IMAGE,
                         "set HOT_INFRA_TEST_NGINX_IMAGE to run nginx -t")
    def test_nginx_config_loads_read_only_and_unprivileged(self):
        ingress = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]["ingress"]
        public = dict(renderer.validate(example("gcp")), app_exposure="public", api_exposure="public")
        for config in (renderer.validate(example()), public, self.scalable(["app"])[0]):
            with self.subTest(public=renderer.public_endpoints(config), scalable=renderer.scalable(config)):
                self.nginx_test(renderer.nginx_config(config), ingress)

    def nginx_test(self, nginx, ingress):
        with tempfile.TemporaryDirectory() as tmp:
            # The app and API are absent: Nginx must load without their names.
            result = docker("run", "--rm", *self.ingress_args(tmp, nginx, ingress),
                            "--entrypoint", "nginx", NGINX_IMAGE, "-t")
            self.assertEqual(result.returncode, 0, result.stderr)

    def ingress_args(self, tmp, nginx, ingress):
        (Path(tmp) / "nginx.conf").write_text(nginx)
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
        return ["--read-only", "--user", ingress["user"], "--cap-drop", "ALL",
                "--tmpfs", ingress["tmpfs"][0].replace("size=16777216,", ""),
                "-v", f"{tmp}/nginx.conf:/etc/nginx/nginx.conf:ro",
                "-v", f"{tmp}/TLS_CERTIFICATE:/run/secrets/TLS_CERTIFICATE:ro",
                "-v", f"{tmp}/TLS_PRIVATE_KEY:/run/secrets/TLS_PRIVATE_KEY:ro"]

    @unittest.skipUnless(shutil.which("docker") and shutil.which("openssl") and NGINX_IMAGE,
                         "set HOT_INFRA_TEST_NGINX_IMAGE to run Nginx")
    def test_nginx_follows_an_app_that_came_back_at_another_address(self):
        # What Docker's restart policy can do without the agent: the app moves
        # and the ingress goes on running.
        config = renderer.validate(example())
        ingress = renderer.render(example(), manifest(), "/data", "/run/hot/secrets")["services"]["ingress"]
        host = config["app_hostname"]
        name = f"hot-ingress-test-{os.getpid()}"
        created = docker("network", "create", "--subnet", "10.213.77.0/24", name)
        self.assertEqual(created.returncode, 0, created.stderr)
        self.addCleanup(docker, "network", "rm", name)
        self.addCleanup(docker, "rm", "-f", f"{name}-app", f"{name}-ingress")

        def start(tmp, address):
            started = docker("run", "-d", "--name", f"{name}-app", "--network", name, "--network-alias", "app",
                             "--ip", address, "-v", f"{tmp}/app.conf:/etc/nginx/nginx.conf:ro",
                             "--entrypoint", "nginx", NGINX_IMAGE, "-g", "daemon off;")
            self.assertEqual(started.returncode, 0, started.stderr)

        def wait_for_reply_from(address):
            reply = None
            deadline = time.monotonic() + 30
            while reply != address and time.monotonic() < deadline:
                time.sleep(1)
                reply = docker("exec", f"{name}-ingress", "curl", "-sk", "--max-time", "3", "--resolve",
                               f"{host}:8443:127.0.0.1", f"https://{host}:8443/").stdout
            self.assertEqual(reply, address)

        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "app.conf").write_text('''pid /tmp/nginx.pid;
events { worker_connections 16; }
http {
  access_log off;
  client_body_temp_path /tmp/client_body;
  proxy_temp_path /tmp/proxy;
  fastcgi_temp_path /tmp/fastcgi;
  uwsgi_temp_path /tmp/uwsgi;
  scgi_temp_path /tmp/scgi;
  server { listen 4680; location / { return 200 "$server_addr"; } }
}
''')
            (Path(tmp) / "app.conf").chmod(0o644)
            start(tmp, "10.213.77.3")
            started = docker("run", "-d", "--name", f"{name}-ingress", "--network", name, "--ip", "10.213.77.10",
                             *self.ingress_args(tmp, renderer.nginx_config(config), ingress),
                             "--entrypoint", "nginx", NGINX_IMAGE, "-g", "daemon off;")
            self.assertEqual(started.returncode, 0, started.stderr)
            wait_for_reply_from("10.213.77.3")
            removed = docker("rm", "-f", f"{name}-app")
            self.assertEqual(removed.returncode, 0, removed.stderr)
            start(tmp, "10.213.77.2")
            wait_for_reply_from("10.213.77.2")

    @unittest.skipUnless(shutil.which("docker"), "Docker CLI is not installed")
    def test_compose_accepts_the_complete_bundle(self):
        settings = {"HOT_EMAIL_FROM_NAME": "Ops $USER ${HOME Team", "HOT_EMAIL_SMTP_CA_CERT": SMTP_CA}
        config = dict(renderer.validate(example()), settings=settings)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "compose.json"
            path.write_text(json.dumps(renderer.render(config, manifest(), "/data", "/run/hot/secrets")))
            result = subprocess.run(["docker", "compose", "-f", str(path), "--profile", "admin", "config", "--format", "json"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            parsed = json.loads(result.stdout)
            self.assertEqual(len(parsed["services"]), 11)
            self.assertEqual(parsed["services"]["task-worker"]["pid"], "host")
            self.assertEqual(parsed["services"]["scheduler"]["environment"]["HOT_LOG_FORMAT"], "json")
            # Nothing was substituted: `config` prints each $ escaped again.
            self.assertEqual(parsed["services"]["worker"]["environment"]["HOT_EMAIL_FROM_NAME"], "Ops $$USER $${HOME Team")
            self.assertEqual(parsed["services"]["worker"]["environment"]["HOT_EMAIL_SMTP_CA_CERT"], SMTP_CA)
            self.assertIn("$$(cat /tmp/hot-scheduler-heartbeat", parsed["services"]["scheduler"]["healthcheck"]["test"][1])
            self.assertIn('exec /usr/bin/hot "$$@"', parsed["services"]["worker"]["entrypoint"][2])


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
        self.assertEqual(built["upgrade"], json.loads((ROOT / "release/upgrade-policy.json").read_text()))
        # A release needs a host image whose renderer is at least its own.
        self.assertEqual(built["minimum_renderer"], renderer.RENDERER_VERSION)
        renderer.render(example(), built, "/data", "/run/hot/secrets")
        with self.assertRaisesRegex(ValueError, "no newer than 3.0.1"):
            releaser.build("3.0.1", hot, upstream, policy={"minimum_source": "3.1.0"})
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
