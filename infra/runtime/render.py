#!/usr/bin/env python3
"""Render a development runtime bundle; never fetch secrets or deploy resources."""
import argparse
import copy
import ipaddress
import json
from pathlib import Path
import re
import subprocess
import sys

SERVICES = ("app", "api", "worker", "scheduler", "task-worker")
# `--askpass` reads the password from stdin; PING is the command it then sends.
VALKEY_HEALTHCHECK = "valkey-cli --askpass PING < /run/secrets/VALKEY_PASSWORD | grep -qx PONG"
IMAGES = {"hot", "postgres", "valkey", "ingress"}
DIGEST = re.compile(r"^[a-z0-9][a-z0-9._:/-]*@sha256:[a-f0-9]{64}$")
MANIFEST_KEYS = {"schema_version", "release", "module_tag", "configuration_schema", "migrations", "upgrade", "images"}
# The release workflow signs manifests keylessly with its GitHub OIDC identity.
RELEASE_SIGNER = r"^https://github\.com/hot-dev/hot/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$"
RELEASE_ISSUER = "https://token.actions.githubusercontent.com"
# Per-size starting allocations within the VM sizes the stacks choose (8, 16,
# and 32 GB; 2, 4, and 8 vCPUs), leaving 1-2 GB for the OS, Docker, and Kata.
# Conservative caps, not benchmarked limits. Memory in MB.
SIZES = {
    "small": {"memory": {"postgres": 1536, "valkey": 384, "ingress": 128, "app": 768, "api": 768,
                         "scheduler": 256, "worker": 1536, "task-worker": 1536},
              "cpus": {"worker": 1.0, "task-worker": 1.0}, "default_cpus": 0.5, "valkey_maxmemory": 256,
              "task": {"max": 1, "code": 2, "container": 1, "disk_mb": 10240}, "engine_threads": 2},
    "medium": {"memory": {"postgres": 3072, "valkey": 768, "ingress": 256, "app": 1024, "api": 1024,
                          "scheduler": 256, "worker": 3072, "task-worker": 5120},
               "cpus": {"worker": 1.5, "task-worker": 2.0}, "default_cpus": 1.0, "valkey_maxmemory": 512,
               "task": {"max": 2, "code": 4, "container": 2, "disk_mb": 40960}, "engine_threads": 3},
    "large": {"memory": {"postgres": 6144, "valkey": 1536, "ingress": 256, "app": 2048, "api": 2048,
                         "scheduler": 512, "worker": 6144, "task-worker": 12288},
              "cpus": {"worker": 3.0, "task-worker": 4.0}, "default_cpus": 2.0, "valkey_maxmemory": 1024,
              "task": {"max": 4, "code": 8, "container": 4, "disk_mb": 81920}, "engine_threads": 6},
}
VM_MEMORY_MB = {"small": 8192, "medium": 16384, "large": 32768}
# Rewritten by the scheduler while it dispatches; on the scheduler's tmpfs.
SCHEDULER_HEARTBEAT = "/tmp/hot-scheduler-heartbeat"
# The task worker's slots in use, which the host agent reads.
TASK_STATUS = "/tmp/hot-task-status.json"

# Where a cloud load balancer's connections come from: the VPC on AWS (an
# Application Load Balancer), Google's front-end ranges on GCP.
GCP_LOAD_BALANCER_RANGES = ("35.191.0.0/16", "130.211.0.0/22")
LOAD_BALANCER_PORT = 9443


def roles(config):
    """The Hot services this host runs: all of them on a single machine, or
    one role's share of a scalable installation."""
    return tuple(role for role in SERVICES if role in (config.get("roles") or SERVICES))


def web_roles(config):
    return tuple(role for role in ("app", "api") if role in roles(config))


def runs_migrations(config):
    """Whether this host migrates the database: the single machine, or the
    scalable installation's scheduler host."""
    return config.get("migrations", True)


def scalable(config):
    return config.get("preset", "single-machine") == "scalable"


def public_endpoints(config):
    return [role for role in ("app", "api") if config.get(role + "_exposure", "private") == "public"]


def load_balancer_sources(config):
    if config["cloud"] == "gcp":
        return list(GCP_LOAD_BALANCER_RANGES)
    return [config.get("network", {}).get("cidr", "10.68.0.0/16")]


# The bundle's container network, where PostgreSQL, Valkey, ingress, and the
# Hot services listen. Fixed so user code can be denied all of it.
RUNTIME_SUBNET = "172.30.0.0/24"
CONFIG_KEYS = {"schema_version", "name", "cloud", "region", "zone", "project", "size", "host_image", "release_pointer", "secret_references", "app_hostname", "api_hostname", "app_exposure", "api_exposure", "storage", "network",
               # Read by the host agent, not by the bundle.
               "data_volume_id", "admin_email", "manifest_base_url", "status_reference", "command_subscription",
               "installation_parameter",
               "settings", "data", "backup", "monitoring", "logging",
               # Scalable installations: which roles this host runs, whether it
               # migrates, and the pointer that releases migrations.
               "preset", "roles", "migrations", "migration_pointer", "instance_group", "scaling", "data_disk", "db_pool",
               "image_registry"}
# data.postgres settings: the contract's, plus the endpoint a managed
# database's stack adds for the host.
POSTGRES_KEYS = {"mode", "runtime_role", "version", "tier", "storage_gb", "high_availability",
                 "backup_retention_days", "deletion_protection", "admin_password_version", "endpoint"}
VALKEY_KEYS = {"mode", "node_type", "high_availability", "auth_token_version", "endpoint"}
# Names that would carry a secret; those belong in the operator secret bundle.
SECRET_MARKERS = ("SECRET", "PASSWORD", "TOKEN", "_KEY", "_URI")


def validate(document):
    config = copy.deepcopy(document.get("installation", document))
    unknown = set(config) - CONFIG_KEYS
    if unknown:
        raise ValueError("Unknown installation fields: " + ", ".join(sorted(unknown)))
    if config.get("schema_version", 1) != 1 or config.get("cloud") not in {"aws", "gcp"}:
        raise ValueError("Expected schema_version=1 and cloud=aws|gcp")
    if not re.fullmatch(r"[a-z][a-z0-9-]{2,23}", config.get("name", "")):
        raise ValueError("Installation name must be 3-24 lowercase letters/digits/hyphens")
    if config.get("size", "small") not in {"small", "medium", "large"}:
        raise ValueError("Unknown size")
    if config.get("preset", "single-machine") not in {"single-machine", "scalable"}:
        raise ValueError("preset must be single-machine or scalable")
    if config.get("roles") is not None and (not config["roles"] or set(config["roles"]) - set(SERVICES)):
        raise ValueError("roles must name one or more of " + ", ".join(SERVICES))
    if scalable(config) and any(data_mode(config, s) == "local" for s in ("postgres", "valkey")):
        raise ValueError("A scalable installation needs managed or existing PostgreSQL and Valkey")
    if scalable(config) and any(s.get("backend") == "local" for s in config.get("storage", {}).values()):
        raise ValueError("A scalable installation needs S3 or GCS for builds and files")
    if config["cloud"] == "gcp" and not config.get("project"):
        raise ValueError("GCP compute requires a project")
    for endpoint in ("app", "api"):
        if config.get(endpoint + "_exposure", "private") not in {"private", "public"}:
            raise ValueError(f"{endpoint}_exposure must be private or public")
        if not re.fullmatch(r"[a-z0-9][a-z0-9.-]+[a-z0-9]", config.get(endpoint + "_hostname", "")):
            raise ValueError("App/API require DNS hostnames without schemes, ports, or paths")
    if config.get("admin_email") and not re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", config["admin_email"]):
        raise ValueError("admin_email must be an email address")
    if not (config.get("manifest_base_url") or "https://").startswith("https://"):
        raise ValueError("manifest_base_url must use https")
    data = config.get("data", {})
    if not isinstance(data, dict) or set(data) - {"postgres", "valkey"}:
        raise ValueError("data takes postgres and valkey settings only")
    for service, options in data.items():
        allowed = POSTGRES_KEYS if service == "postgres" else VALKEY_KEYS
        if not isinstance(options, dict) or set(options) - allowed:
            raise ValueError(f"Unknown data.{service} settings")
        if options.get("mode", "local") not in {"local", "managed", "existing"}:
            raise ValueError(f"data.{service}.mode must be local, managed, or existing")
    if data_mode(config, "valkey") == "managed":
        endpoint = data["valkey"].get("endpoint")
        # ElastiCache takes an AUTH token from a secret; Memorystore takes the
        # host's IAM tokens and presents its own CA.
        required = {"host", "port", "auth_secret"} if config["cloud"] == "aws" else {"host", "port", "auth", "ca_certificate"}
        if not isinstance(endpoint, dict) or set(endpoint) - required or not all(endpoint.get(k) for k in required):
            raise ValueError("Managed Valkey needs the endpoint the stack provisions: " + ", ".join(sorted(required)))
        if config["cloud"] == "gcp" and endpoint["auth"] != "gcp-iam":
            raise ValueError("Managed Valkey on GCP authenticates with gcp-iam")
    if data_mode(config, "postgres") == "managed":
        endpoint = data["postgres"].get("endpoint")
        required = {"host", "port", "admin_user", "admin_secret"} | ({"ca_certificate"} if config["cloud"] == "gcp" else set())
        if not isinstance(endpoint, dict) or set(endpoint) - (required | {"ca_certificate"}) or not all(endpoint.get(k) for k in required):
            raise ValueError("Managed PostgreSQL needs the endpoint the stack provisions: " + ", ".join(sorted(required)))
    if not re.fullmatch(r"[a-z_][a-z0-9_]{0,62}", runtime_role(config)) or runtime_role(config) in {"hot_owner", "hot_admin", "postgres"}:
        raise ValueError("data.postgres.runtime_role must be a PostgreSQL role name other than hot_owner, hot_admin, or postgres")
    logging = config.get("logging", {})
    if set(logging) - {"destination", "retention_days"} or logging.get("destination", "cloud") not in {"cloud", "local"}:
        raise ValueError("logging.destination must be cloud or local")
    settings = config.get("settings", {})
    if not isinstance(settings, dict):
        raise ValueError("settings must map HOT_* names to values")
    for key, value in settings.items():
        if not re.fullmatch(r"HOT_[A-Z0-9_]+", key) or not isinstance(value, str):
            raise ValueError(f"settings entry {key!r} must be a HOT_* name with a string value")
        if any(marker in key for marker in SECRET_MARKERS):
            raise ValueError(f"settings entry {key} looks like a secret; put it in the operator secret bundle")
    if config["app_hostname"] == config["api_hostname"]:
        raise ValueError("App and API must have distinct hostnames")
    if set(config.get("storage", {})) != {"build", "file"}:
        raise ValueError("Choose build and file storage independently")
    for role, storage in config["storage"].items():
        if set(storage) - {"backend", "bucket", "region", "prefix", "provision", "noncurrent_days"}:
            raise ValueError(f"Unknown {role} storage fields; secret values do not belong here")
        if storage.get("backend") not in {"local", "s3", "gcs"}:
            raise ValueError(f"{role}: select local, existing S3, or existing GCS")
        if storage["backend"] == "gcs" and config["cloud"] != "gcp":
            raise ValueError(f"{role}: GCS uses the VM's Google identity, so it needs cloud=gcp in this release")
        prefix = storage.get("prefix", role + "s")
        if prefix.startswith("/") or ".." in prefix:
            raise ValueError(f"{role}: unsafe storage prefix")
        if storage["backend"] == "gcs":
            if not re.fullmatch(r"[a-z0-9][a-z0-9._-]{1,61}[a-z0-9]", storage.get("bucket", "")) or not prefix.strip("/"):
                raise ValueError(f"{role}: GCS requires bucket and a nonempty relative prefix")
        if storage["backend"] == "s3":
            if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", storage.get("bucket", "")) or not storage.get("region") or not prefix.strip("/"):
                raise ValueError(f"{role}: S3 requires bucket, region, and a nonempty relative prefix")
    network = config.get("network", {})
    if set(network) - {"mode", "cidr", "client_cidrs", "network_id", "subnet_id", "public_subnet_ids", "data_subnet_ids", "private_subnet_ids"}:
        raise ValueError("Unknown network fields")
    if network.get("mode", "new") not in {"new", "existing"}:
        raise ValueError("Select new or existing networking")
    if network.get("mode") == "existing" and not all(network.get(k) for k in ("network_id", "subnet_id")):
        raise ValueError("Existing networking requires network_id and subnet_id")
    cidr = ipaddress.ip_network(network.get("cidr", "10.68.0.0/16"))
    if cidr.version != 4 or cidr.prefixlen > 24:
        raise ValueError("Network requires IPv4 /24 or larger")
    for client in network.get("client_cidrs", []):
        address = ipaddress.ip_network(client)
        if address.version != 4 or address.prefixlen == 0:
            raise ValueError("Client ranges must be explicit IPv4 CIDRs narrower than /0")
    for key in ("region", "zone", "host_image", "release_pointer", "secret_references"):
        if not config.get(key):
            raise ValueError(f"Missing {key}")
    for reference in config["secret_references"].values():
        if not isinstance(reference, str) or not (reference.startswith("arn:aws:secretsmanager:") or (reference.startswith("projects/") and "/secrets/" in reference)):
            raise ValueError("secret_references must contain cloud secret IDs, never secret values")
    return config


def data_mode(config, service):
    """local runs the service in the bundle; managed uses the cloud service
    the stack provisions; existing uses the operator's."""
    return config.get("data", {}).get(service, {}).get("mode", "local")


def runtime_role(config):
    return config.get("data", {}).get("postgres", {}).get("runtime_role", "hot_runtime")


def log_settings(config, instance=""):
    """Container logs go to the cloud's log service with the VM's identity,
    or stay on the host. Shipping never blocks a container: when the
    destination is slow or down, lines beyond a bounded buffer are dropped.
    Docker keeps a bounded local copy either way, for `docker logs`.

    Each line can be traced to its installation, role, release, and host:
    CloudWatch streams are named for the instance and container, and Cloud
    Logging records the instance and the container's hot.* labels."""
    local = {"max-size": "10m", "max-file": "3"}
    if config.get("logging", {}).get("destination", "cloud") == "local":
        return {"driver": "local", "options": local}
    shipping = {"mode": "non-blocking", "max-buffer-size": "4m",
                "cache-max-size": local["max-size"], "cache-max-file": local["max-file"]}
    if config["cloud"] == "aws":
        tag = f"{instance}/{{{{.Name}}}}" if instance else "{{.Name}}"
        return {"driver": "awslogs", "options": {"awslogs-region": config["region"], "awslogs-group": f"/hot/{config['name']}",
                                                  "tag": tag, **shipping}}
    return {"driver": "gcplogs", "options": {"gcp-project": config["project"], "labels": "hot.installation,hot.role,hot.release",
                                             **shipping}}


def mirrored(name, image, registry):
    """An image reference in a mirror registry: the repository is named for
    the image's role (hot, postgres, valkey, ingress), and the digest is
    kept, so the content is the release's exactly."""
    if not registry:
        return image
    return f"{registry.rstrip('/')}/{name}@{image.split('@', 1)[1]}"


def verify_manifest_signature(manifest_path, bundle_path, signer=RELEASE_SIGNER, cosign="cosign"):
    """Accept a manifest only when its Sigstore bundle names the release workflow."""
    result = subprocess.run(
        [cosign, "verify-blob", "--bundle", str(bundle_path),
         "--certificate-identity-regexp", signer,
         "--certificate-oidc-issuer", RELEASE_ISSUER, str(manifest_path)],
        capture_output=True, text=True, check=False)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().splitlines()
        raise ValueError("Manifest signature did not verify" + (": " + detail[-1] if detail else ""))


def render(document, manifest, data_root, secrets_root, s3_auth="identity", s3_session_tokens=False,
           operator_secrets=None, extra_deny=(), instance=""):
    """`operator_secrets` maps a Hot service role to extra secret names, such
    as SMTP or OAuth credentials, that it mounts from the operator bundle.
    `extra_deny` adds addresses user code may never reach, such as an
    existing database's."""
    config = validate(document)
    if set(manifest) - MANIFEST_KEYS:
        raise ValueError("Unknown manifest fields: " + ", ".join(sorted(set(manifest) - MANIFEST_KEYS)))
    if manifest.get("schema_version") != 1 or set(manifest.get("images", {})) != IMAGES:
        raise ValueError("Manifest schema 1 requires hot/postgres/valkey/ingress images")
    if manifest.get("configuration_schema", 1) != 1:
        raise ValueError("This renderer supports configuration schema 1 only")
    if set(manifest.get("upgrade", {})) - {"minimum_source"}:
        raise ValueError("Unknown manifest upgrade fields")
    if not all(DIGEST.fullmatch(image) for image in manifest["images"].values()):
        raise ValueError("Every image must be pinned by OCI sha256 digest")
    for path in (data_root, secrets_root):
        if not Path(path).is_absolute():
            raise ValueError("Data and tmpfs secret roots must be absolute paths")
    if s3_auth not in {"identity", "files"}:
        raise ValueError("S3 authentication must be identity or files")
    if s3_session_tokens and s3_auth != "files":
        raise ValueError("Session-token files require S3 file authentication")
    images = {name: mirrored(name, image, config.get("image_registry", "")) for name, image in manifest["images"].items()}
    size = SIZES[config.get("size", "small")]
    memory = {role: f"{mb}m" for role, mb in size["memory"].items()}
    secrets, services = {}, {}

    def secret(role, key):
        name = role + "-" + key
        secrets[name] = {"file": str(Path(secrets_root) / role / key)}
        return {"source": name, "target": key}

    def bind(source, target, readonly=False):
        return {"type": "bind", "source": str(source), "target": target,
                "read_only": readonly, "bind": {"create_host_path": False}}

    common_env = {
        "HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host",
        "HOT_ENGINE_ISOLATION": "true", "HOT_NETWORK_ALLOW_PRIVATE": "false",
        "HOT_QUEUE_TYPE": "redis", "HOT_EMITTER_TYPE": "db",
        "HOT_FILE_MODE": "service", "HOT_BLOB_MODE": "service", "HOT_LOG_FORMAT": "json",
        "HOT_LOG_TARGET": "stdout", "HOT_LOG_LEVEL": "info",
        "HOT_APP_HOST": "0.0.0.0", "HOT_API_HOST": "0.0.0.0",
        # Links in email, OAuth redirects, and the API address the app shows
        # use the installation's own hostnames, never Hot's cloud defaults.
        "HOT_APP_URL": f"https://{config['app_hostname']}", "HOT_API_URL": f"https://{config['api_hostname']}",
        "HOT_OAUTH_REDIRECT_BASE_URL": f"https://{config['app_hostname']}",
        "HOT_COOKIE_DOMAIN": config["app_hostname"],
        "HOT_TASK_MAX_CONCURRENT": str(size["task"]["max"]),
        "HOT_TASK_CODE_MAX_CONCURRENT": str(size["task"]["code"]),
        "HOT_TASK_CONTAINER_MAX_CONCURRENT": str(size["task"]["container"]),
        "HOT_TASK_WORKER_MEMORY_MB": str(size["memory"]["task-worker"]),
        "HOT_TASK_WORKER_DISK_MB": str(size["task"]["disk_mb"]), "HOT_BOX_ENABLED": "true",
        "HOT_WORKER_THREADS": "1", "HOT_ENGINE_THREADS": str(size["engine_threads"]),
        "HOT_BOX_BACKEND": "kata", "HOT_BOX_CONTAINERD_SOCKET": "/run/kata-containerd/containerd.sock",
        "HOT_BOX_VMM": "qemu",
    }
    if scalable(config):
        # Each instance's PostgreSQL pool, so every role's maximum fits the
        # database's connection budget.
        common_env["HOT_WORKER_DB_MAX_CONNECTIONS"] = str(config.get("db_pool", 20))
    # User code may never reach the installation itself, even when an operator
    # allowlists private ranges that overlap it.
    deny = [RUNTIME_SUBNET]
    if config.get("network", {}).get("mode", "new") == "new":
        deny.append(config.get("network", {}).get("cidr", "10.68.0.0/16"))
    deny += [cidr for cidr in extra_deny if cidr not in deny]
    pg_local = data_mode(config, "postgres") == "local"
    valkey_local = data_mode(config, "valkey") == "local"
    # A managed or existing PostgreSQL is reached over TLS verified against its CA.
    db_ca = [] if pg_local else ["POSTGRES_CA_CERT"]
    # Memorystore: IAM tokens, and the instance's CA for TLS.
    valkey_iam = data_mode(config, "valkey") == "managed" and config["cloud"] == "gcp"
    valkey_ca = ["HOT_REDIS_CA_CERT"] if valkey_iam else []
    data_ready = {name: {"condition": "service_healthy"}
                  for name, local in (("postgres", pg_local), ("valkey", valkey_local)) if local}
    for key in config.get("settings", {}):
        if (key in common_env or key.endswith(("_STORAGE_TYPE", "_STORAGE_PATH")) or "_AWS_S3_" in key
                or "_GCS_" in key or key.startswith("HOT_NETWORK_CLIENT_IP_")):
            raise ValueError(f"settings entry {key} is set by the runtime bundle")
    # Defaults operators may change in `settings`: accounts are by invitation
    # (after the first administrator is bootstrapped) until they open sign-up.
    defaults = {"HOT_APP_SIGNUP": "invite"}
    for role in roles(config):
        env, mounts = {**common_env, **defaults, **config.get("settings", {})}, []
        if role in {"worker", "task-worker"}:
            env["HOT_NETWORK_DENY_CIDRS"] = ",".join(deny)
        if role in {"app", "api"}:
            # Client addresses come from forwarding headers only through Nginx
            # and, for public endpoints, the cloud load balancer.
            proxies = [RUNTIME_SUBNET] + (load_balancer_sources(config) if public_endpoints(config) else [])
            env.update(HOT_NETWORK_CLIENT_IP_TRUSTED_PROXY="true", HOT_NETWORK_CLIENT_IP_HEADER="x-forwarded-for",
                       HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES=",".join(proxies))
        keys = ["HOT_DB_URI", "HOT_REDIS_URI", *db_ca, *valkey_ca]
        if valkey_iam:
            env["HOT_REDIS_AUTH"] = "gcp-iam"
        if role != "scheduler":
            keys.append("HOT_ENCRYPTION_KEY")
        if role == "app":
            keys.append("HOT_APP_SESSION_SECRET")
        for storage_role, storage in config["storage"].items():
            prefix = "HOT_" + storage_role.upper()
            env[prefix + "_STORAGE_TYPE"] = storage["backend"]
            if storage["backend"] == "local":
                target = "/var/lib/hot/" + storage_role
                env[prefix + "_STORAGE_PATH"] = target
                mounts.append(bind(Path(data_root) / storage_role, target))
            elif storage["backend"] == "gcs":
                # The VM's service account; no credential files.
                env[prefix + "_GCS_BUCKET"] = storage["bucket"]
                env[prefix + "_GCS_PREFIX"] = storage.get("prefix", storage_role + "s")
            else:
                for field in ("bucket", "region", "prefix"):
                    env[prefix + "_AWS_S3_" + field.upper()] = storage.get(field, storage_role + "s")
                if s3_auth == "files":
                    keys += [prefix + "_AWS_ACCESS_KEY_ID", prefix + "_AWS_SECRET_ACCESS_KEY"]
                    if s3_session_tokens:
                        keys.append(prefix + "_AWS_SESSION_TOKEN")
        keys += [key for key in (operator_secrets or {}).get(role, []) if key not in keys]
        for key in keys:
            if key.startswith("HOT_"):
                env[key + "_FILE"] = "/run/secrets/" + key
        services[role] = {
            "image": images["hot"], "platform": "linux/amd64", "command": [role],
            "user": "10001:10001", "working_dir": "/var/lib/hot", "init": True,
            "restart": "unless-stopped", "read_only": True, "stop_grace_period": "120s",
            "environment": env, "secrets": [secret(role, key) for key in keys],
            "volumes": mounts, "tmpfs": ["/tmp:size=268435456,uid=10001,gid=10001", "/var/lib/hot/.hot:size=268435456,uid=10001,gid=10001"],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            "depends_on": dict(data_ready),
            "mem_limit": memory[role],
            "cpus": size["cpus"].get(role, size["default_cpus"]),
        }
    # The scheduler rewrites its heartbeat every 30 seconds while it
    # dispatches schedules, so a stalled scheduler turns unhealthy even though
    # its process runs. ($$ escapes Compose interpolation.)
    if "scheduler" in services:
        scheduler = services["scheduler"]
        scheduler["environment"]["HOT_SCHEDULER_HEARTBEAT_FILE"] = SCHEDULER_HEARTBEAT
        scheduler["healthcheck"] = {
            "test": ["CMD-SHELL", f"test $$(( $$(date +%s) - $$(cat {SCHEDULER_HEARTBEAT} 2>/dev/null || echo 0) )) -lt 120"],
            "interval": "30s", "timeout": "5s", "retries": 3, "start_period": "90s"}
    if "task-worker" in services:
        task = services["task-worker"]
        task.update(user="0:2000", privileged=True, pid="host", read_only=False)
        task["environment"]["HOT_TASK_STATUS_FILE"] = TASK_STATUS
        task.pop("cap_drop")
        task.pop("security_opt")
        for path, readonly in (("/run/kata-containerd", False), ("/run/vc", False), ("/var/run/netns", False), ("/opt/cni/bin", True), ("/var/lib/cni", False), ("/tmp/hot-box-io", False)):
            task["volumes"].append(bind(path, path, readonly))
    if pg_local:
        services["postgres"] = {
            "image": images["postgres"], "restart": "unless-stopped", "shm_size": "256m", "mem_limit": memory["postgres"],
            "environment": {"POSTGRES_DB": "hot", "POSTGRES_USER": "hot_owner", "POSTGRES_PASSWORD_FILE": "/run/secrets/POSTGRES_PASSWORD"},
            "secrets": [secret("postgres", k) for k in ("POSTGRES_PASSWORD", "POSTGRES_RUNTIME_PASSWORD")],
            # PostgreSQL 18 images keep data in /var/lib/postgresql/<major>/docker and
            # refuse a mount at the older /var/lib/postgresql/data.
            "volumes": [bind(Path(data_root) / "postgres", "/var/lib/postgresql"), {"type": "bind", "source": "./init.sql", "target": "/docker-entrypoint-initdb.d/10-hot.sql", "read_only": True}],
            "healthcheck": {"test": ["CMD", "pg_isready", "-U", "hot_owner", "-d", "hot"], "interval": "5s", "timeout": "3s", "retries": 12},
        }
    if valkey_local:
        services["valkey"] = {
            "image": images["valkey"], "restart": "unless-stopped", "mem_limit": memory["valkey"],
            "command": ["valkey-server", "/etc/valkey/valkey.conf"],
            "secrets": [secret("valkey", k) for k in ("VALKEY_ACL", "VALKEY_PASSWORD")],
            "volumes": [bind(Path(data_root) / "valkey", "/data"), {"type": "bind", "source": "./valkey.conf", "target": "/etc/valkey/valkey.conf", "read_only": True}],
            "healthcheck": {"test": ["CMD-SHELL", VALKEY_HEALTHCHECK], "interval": "5s", "timeout": "3s", "retries": 12},
        }
    services["ingress"] = {
        "image": images["ingress"], "restart": "unless-stopped", "user": "10001:10001", "read_only": True,
        "entrypoint": ["nginx", "-g", "daemon off;"],
        # A scalable installation's hosts are reached only through passthrough
        # load balancers, which keep the port: 443 goes to the listener that
        # routes by Host header.
        "ports": [f"443:{LOAD_BALANCER_PORT}"] if scalable(config) else
                 ["443:8443"] + ([f"{LOAD_BALANCER_PORT}:{LOAD_BALANCER_PORT}"] if public_endpoints(config) else []),
        "mem_limit": memory["ingress"],
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"], "tmpfs": ["/tmp:size=16777216,uid=10001,gid=10001"],
        "secrets": [secret("ingress", k) for k in ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY")],
        "volumes": [{"type": "bind", "source": "./nginx.conf", "target": "/etc/nginx/nginx.conf", "read_only": True}],
        "depends_on": list(web_roles(config)),
    }
    if not web_roles(config):
        del services["ingress"]
    services["migrate"] = {
        "image": images["hot"], "platform": "linux/amd64", "profiles": ["admin"],
        "command": ["db", "migrate"], "user": "10001:10001", "read_only": True,
        "environment": {"HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host",
            "HOT_DB_URI_FILE": "/run/secrets/HOT_DB_MIGRATION_URI",
            "HOT_DB_MIGRATION_URI_FILE": "/run/secrets/HOT_DB_MIGRATION_URI"},
        "secrets": [secret("migrate", k) for k in ("HOT_DB_MIGRATION_URI", *db_ca)],
        "tmpfs": ["/tmp:uid=10001,gid=10001"],
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        "depends_on": {k: v for k, v in data_ready.items() if k == "postgres"},
    }
    if config.get("admin_email"):
        # The first administrator; rerunning changes nothing (hot admin bootstrap).
        services["bootstrap-admin"] = {
            "image": images["hot"], "platform": "linux/amd64", "profiles": ["admin"],
            "command": ["admin", "bootstrap", "--email", config["admin_email"], "--password-file", "/run/secrets/ADMIN_PASSWORD"],
            "user": "10001:10001", "read_only": True,
            "environment": {"HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host", "HOT_DB_URI_FILE": "/run/secrets/HOT_DB_URI",
                            "HOT_APP_URL": common_env["HOT_APP_URL"]},
            "secrets": [secret("bootstrap-admin", k) for k in ("HOT_DB_URI", "ADMIN_PASSWORD", *db_ca)],
            "tmpfs": ["/tmp:uid=10001,gid=10001"],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            "depends_on": {k: v for k, v in data_ready.items() if k == "postgres"},
        }
    grant = ["--set=ON_ERROR_STOP=1", f"--set=runtime_role={runtime_role(config)}", "--set=schema=hot",
             "--file=/etc/hot/provision_runtime.sql"]
    provision = {
        "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
        "volumes": [{"type": "bind", "source": "./provision_runtime.sql", "target": "/etc/hot/provision_runtime.sql", "read_only": True}],
        "depends_on": {"migrate": {"condition": "service_completed_successfully"}},
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
    }
    if pg_local:
        provision.update(entrypoint=["psql"], command=["--host=postgres", "--username=hot_owner", "--dbname=hot", *grant],
                         environment={"PGPASSFILE": "/run/secrets/PGPASSFILE"},
                         secrets=[secret("provision", "PGPASSFILE")])
    else:
        # The connection string is a secret, so it is read from its file, never
        # passed as an argument or environment value.
        via_uri = ["sh", "-c", 'exec psql "$(cat /run/secrets/HOT_DB_MIGRATION_URI)" "$@"', "psql"]
        provision.update(entrypoint=via_uri, command=grant,
                         secrets=[secret("provision", k) for k in ("HOT_DB_MIGRATION_URI", *db_ca)])
        if data_mode(config, "postgres") == "managed":
            # The stack's administrator login creates the owner and runtime
            # logins, the database, the schema, and its extensions; rerunning
            # it only resets the two passwords to the generated ones.
            services["prepare-postgres"] = {
                "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
                "entrypoint": ["sh", "-c", 'exec psql "$(cat /run/secrets/POSTGRES_ADMIN_URI)" "$@"', "psql"],
                "command": ["--set=ON_ERROR_STOP=1", f"--set=runtime_role={runtime_role(config)}",
                            "--file=/etc/hot/prepare_postgres.sql"],
                "secrets": [secret("prepare-postgres", k) for k in
                            ("POSTGRES_ADMIN_URI", "POSTGRES_ADMIN_HOT_URI", "POSTGRES_PASSWORD",
                             "POSTGRES_RUNTIME_PASSWORD", *db_ca)],
                "volumes": [{"type": "bind", "source": "./prepare_postgres.sql", "target": "/etc/hot/prepare_postgres.sql", "read_only": True}],
                "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            }
        # Read-only preconditions the database administrator owns; see the
        # self-hosted PostgreSQL guide.
        # Restores a local database's dump when an installation moves it here
        # (see the agent's data moves). It refuses a database that already
        # has Hot's tables, and restores in one transaction.
        services["restore-postgres"] = {
            "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
            "entrypoint": ["sh", "/etc/hot/restore_postgres.sh"],
            "secrets": [secret("check-postgres", k) for k in ("HOT_DB_MIGRATION_URI", *db_ca)],
            "tmpfs": ["/tmp:size=16777216,uid=10001,gid=10001"],
            "volumes": [{"type": "bind", "source": "./restore_postgres.sh", "target": "/etc/hot/restore_postgres.sh", "read_only": True},
                        bind(Path(data_root) / "moves", "/move", readonly=True)],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        }
        services["check-postgres"] = {
            "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
            "entrypoint": via_uri,
            "command": ["--set=ON_ERROR_STOP=1", f"--set=runtime_role={runtime_role(config)}",
                        "--file=/etc/hot/check_postgres.sql"],
            "secrets": [secret("check-postgres", k) for k in ("HOT_DB_MIGRATION_URI", *db_ca)],
            "volumes": [{"type": "bind", "source": "./check_postgres.sql", "target": "/etc/hot/check_postgres.sql", "read_only": True}],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        }
    services["provision"] = provision
    if not runs_migrations(config):
        for job in ("migrate", "provision", "bootstrap-admin", "check-postgres", "prepare-postgres", "restore-postgres"):
            services.pop(job, None)
    used = {s["source"] for service in services.values() for s in service.get("secrets", [])}
    secrets = {name: spec for name, spec in secrets.items() if name in used}
    for name, service in services.items():
        service["logging"] = log_settings(config, instance)
        service["labels"] = {"hot.installation": config["name"], "hot.role": name,
                             "hot.release": str(manifest.get("release", ""))}
    # No Docker socket is ever mounted. The separate Kata daemon owns guests.
    compose = {"name": config["name"], "services": services, "secrets": secrets,
               "networks": {"default": {"ipam": {"config": [{"subnet": RUNTIME_SUBNET}]}}}}
    return compose


def nginx_config(config):
    servers = []
    # Private access on 8443 requires SNI naming an endpoint. A cloud load
    # balancer re-encrypts without SNI, so public endpoints get their own port,
    # routed by Host header, where anything else is refused.
    web = web_roles(config)
    listeners = [] if scalable(config) else [("8443", "$remote_addr", web)]
    behind_balancer = web if scalable(config) else tuple(r for r in public_endpoints(config) if r in web)
    if behind_balancer:
        listeners.append((str(LOAD_BALANCER_PORT), "$proxy_add_x_forwarded_for", behind_balancer))
        health = "http://api:4681/status" if "api" in web else "http://app:4680/signin"
        servers.append(f'''server {{
  listen {LOAD_BALANCER_PORT} ssl default_server;
  location = /hot-lb-health {{
    proxy_pass {health};
  }}
  location / {{
    return 444;
  }}
}}''')
    for listen, forwarded, roles in listeners:
        for role, port in (("app", 4680), ("api", 4681)):
            if role not in roles:
                continue
            servers.append(f'''server {{
  listen {listen} ssl;
  server_name {config[role + '_hostname']};
  location / {{
    proxy_pass http://{role}:{port};
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header X-Forwarded-For {forwarded};
    proxy_set_header X-Request-ID $request_id;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_read_timeout 3600s;
  }}
}}''')
    return '''pid /tmp/nginx.pid;
error_log /dev/stderr warn;
events { worker_connections 1024; }
http {
  include /etc/nginx/mime.types;
  log_format operational escape=json '{"status":$status,"duration":$request_time,"request_id":"$request_id","method":"$request_method"}';
  access_log /dev/stdout operational;
  client_max_body_size 300m;
  # The root filesystem is read-only, so every temp path nginx creates at
  # startup, used or not, goes under the /tmp tmpfs.
  client_body_temp_path /tmp/client_body;
  proxy_temp_path /tmp/proxy;
  fastcgi_temp_path /tmp/fastcgi;
  uwsgi_temp_path /tmp/uwsgi;
  scgi_temp_path /tmp/scgi;
  ssl_protocols TLSv1.2 TLSv1.3;
  ssl_certificate /run/secrets/TLS_CERTIFICATE;
  ssl_certificate_key /run/secrets/TLS_PRIVATE_KEY;
  server { listen 8443 ssl default_server; ssl_reject_handshake on; }
''' + "\n".join(servers) + "\n}\n"


INIT_SQL = r'''\set runtime_password `cat /run/secrets/POSTGRES_RUNTIME_PASSWORD`
CREATE SCHEMA IF NOT EXISTS hot AUTHORIZATION hot_owner;
CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA hot;
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA hot;
SELECT format('CREATE ROLE hot_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L', :'runtime_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'hot_runtime') \gexec
ALTER ROLE hot_runtime SET search_path = hot, public;
'''
def valkey_conf(size="small"):
    return VALKEY_CONF.replace("maxmemory 256mb", f"maxmemory {SIZES[size]['valkey_maxmemory']}mb")


PREPARE_POSTGRES_SQL = r'''\set owner_password `cat /run/secrets/POSTGRES_PASSWORD`
\set runtime_password `cat /run/secrets/POSTGRES_RUNTIME_PASSWORD`
\set hot_uri `cat /run/secrets/POSTGRES_ADMIN_HOT_URI`
SELECT format('CREATE ROLE hot_owner LOGIN PASSWORD %L', :'owner_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'hot_owner') \gexec
SELECT format('ALTER ROLE hot_owner WITH LOGIN PASSWORD %L', :'owner_password') \gexec
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', :'runtime_role', :'runtime_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'runtime_role') \gexec
SELECT format('ALTER ROLE %I WITH LOGIN PASSWORD %L', :'runtime_role', :'runtime_password') \gexec
SELECT format('ALTER ROLE %I SET search_path = hot, public', :'runtime_role') \gexec
-- A managed administrator is not a superuser: it acts as the owner to give
-- it the database and schema.
GRANT hot_owner TO CURRENT_USER;
SELECT 'CREATE DATABASE hot OWNER hot_owner'
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'hot') \gexec
REVOKE ALL ON DATABASE hot FROM PUBLIC;
SELECT format('GRANT CONNECT ON DATABASE hot TO %I', :'runtime_role') \gexec
\connect :hot_uri
CREATE SCHEMA IF NOT EXISTS hot AUTHORIZATION hot_owner;
CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA hot;
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA hot;
'''


# Restores /move/postgres.dump (a local database's pg_dump) as the schema
# owner. Extensions and the schema already exist, created by the database's
# administrator, so they are left out, as are ownership and grants, which
# provisioning applies.
RESTORE_POSTGRES_SH = r'''set -eu
uri="$(cat /run/secrets/HOT_DB_MIGRATION_URI)"
tables="$(psql "$uri" --no-psqlrc --tuples-only --no-align \
  --command="SELECT count(*) FROM pg_tables WHERE schemaname = 'hot'")"
if [ "$tables" != "0" ]; then
  echo "The database already has $tables of Hot's tables; refusing to restore over them" >&2
  exit 3
fi
pg_restore --list /move/postgres.dump | grep -v -E ' (SCHEMA|EXTENSION) - | COMMENT - EXTENSION ' > /tmp/restore.list
exec pg_restore --no-owner --no-privileges --exit-on-error --single-transaction \
  --use-list=/tmp/restore.list --dbname="$uri" /move/postgres.dump
'''

CHECK_POSTGRES_SQL = r'''SELECT set_config('hot.runtime_role', :'runtime_role', false);
DO $$
DECLARE
  runtime text := current_setting('hot.runtime_role');
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'hot') THEN
    RAISE EXCEPTION 'Schema hot does not exist; create it as the self-hosted PostgreSQL guide describes';
  END IF;
  IF (SELECT count(*) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
      WHERE n.nspname = 'hot' AND e.extname IN ('vector', 'pg_trgm')) < 2 THEN
    RAISE EXCEPTION 'Install the vector and pg_trgm extensions into schema hot';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = runtime AND rolcanlogin
                 AND NOT rolsuper AND NOT rolbypassrls AND NOT rolcreaterole AND NOT rolcreatedb) THEN
    RAISE EXCEPTION 'Runtime login % must exist and be NOSUPERUSER NOBYPASSRLS NOCREATEROLE NOCREATEDB', runtime;
  END IF;
END $$;
'''

VALKEY_CONF = '''bind 0.0.0.0
protected-mode yes
port 6379
aclfile /run/secrets/VALKEY_ACL
appendonly yes
appendfsync everysec
maxmemory 256mb
maxmemory-policy noeviction
dir /data
logfile ""
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("configuration", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--data-root", default="/var/lib/hot/data")
    parser.add_argument("--secrets-root", default="/run/hot/secrets")
    parser.add_argument("--s3-auth", choices=("identity", "files"), default="identity")
    parser.add_argument("--s3-session-tokens", action="store_true")
    parser.add_argument("--signature-bundle", type=Path, help="Sigstore bundle published with the manifest")
    parser.add_argument("--signer-identity", default=RELEASE_SIGNER, help="Certificate identity regex; change only for a fork's releases")
    parser.add_argument("--cosign", default="cosign")
    parser.add_argument("--allow-unsigned-development-manifest", action="store_true")
    args = parser.parse_args()
    try:
        document = json.loads(args.configuration.read_text())
        config = validate(document)
        if args.manifest is None:
            print(json.dumps({"valid": True, "cloud": config["cloud"], "name": config["name"], "qualification": "unqualified-development"}))
            return 0
        if args.signature_bundle is not None:
            verify_manifest_signature(args.manifest, args.signature_bundle, args.signer_identity, args.cosign)
        elif not args.allow_unsigned_development_manifest:
            raise ValueError("Pass the manifest's --signature-bundle; only development rendering may use --allow-unsigned-development-manifest")
        if args.output is None:
            raise ValueError("Rendering requires --output pointing to a new directory")
        manifest = json.loads(args.manifest.read_text())
        compose = render(document, manifest, args.data_root, args.secrets_root, args.s3_auth, args.s3_session_tokens)
        args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        grant_script = Path(__file__).resolve().parents[2] / "resources/db/postgres/provision_runtime.sql"
        files = {"compose.json": json.dumps(compose, indent=2) + "\n", "nginx.conf": nginx_config(config), "init.sql": INIT_SQL, "valkey.conf": valkey_conf(config.get("size", "small")), "check_postgres.sql": CHECK_POSTGRES_SQL, "prepare_postgres.sql": PREPARE_POSTGRES_SQL, "restore_postgres.sh": RESTORE_POSTGRES_SH,
                 "provision_runtime.sql": grant_script.read_text()}
        for name, content in files.items():
            path = args.output / name
            path.write_text(content)
            path.chmod(0o600 if name == "compose.json" else 0o644)
        print(json.dumps({"bundle": str(args.output), "qualification": "unqualified-development"}))
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Configuration error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
