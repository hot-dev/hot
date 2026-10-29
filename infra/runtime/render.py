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
IMAGES = {"hot", "postgres", "valkey", "ingress"}
DIGEST = re.compile(r"^[a-z0-9][a-z0-9._:/-]*@sha256:[a-f0-9]{64}$")
MANIFEST_KEYS = {"schema_version", "release", "module_tag", "configuration_schema", "migrations", "images"}
# The release workflow signs manifests keylessly with its GitHub OIDC identity.
RELEASE_SIGNER = r"^https://github\.com/hot-dev/hot/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$"
RELEASE_ISSUER = "https://token.actions.githubusercontent.com"
# The bundle's container network, where PostgreSQL, Valkey, ingress, and the
# Hot services listen. Fixed so user code can be denied all of it.
RUNTIME_SUBNET = "172.30.0.0/24"
CONFIG_KEYS = {"schema_version", "name", "cloud", "region", "zone", "project", "size", "host_image", "release_pointer", "secret_references", "app_hostname", "api_hostname", "app_exposure", "api_exposure", "storage", "network",
               # Read by the host agent, not by the bundle.
               "data_volume_id", "admin_email", "manifest_base_url", "status_reference", "command_subscription",
               "settings"}
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
    if config["cloud"] == "gcp" and not config.get("project"):
        raise ValueError("GCP compute requires a project")
    for endpoint in ("app", "api"):
        if config.get(endpoint + "_exposure", "private") != "private":
            raise ValueError("Public/mixed ingress is not implemented in this development slice")
        if not re.fullmatch(r"[a-z0-9][a-z0-9.-]+[a-z0-9]", config.get(endpoint + "_hostname", "")):
            raise ValueError("App/API require DNS hostnames without schemes, ports, or paths")
    if config.get("admin_email") and not re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", config["admin_email"]):
        raise ValueError("admin_email must be an email address")
    if not (config.get("manifest_base_url") or "https://").startswith("https://"):
        raise ValueError("manifest_base_url must use https")
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
        if set(storage) - {"backend", "bucket", "region", "prefix"}:
            raise ValueError(f"Unknown {role} storage fields; secret values do not belong here")
        if storage.get("backend") not in {"local", "s3"}:
            raise ValueError(f"{role}: select local or existing S3; GCS is not implemented yet")
        prefix = storage.get("prefix", role + "s")
        if prefix.startswith("/") or ".." in prefix:
            raise ValueError(f"{role}: unsafe storage prefix")
        if storage["backend"] == "s3":
            if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", storage.get("bucket", "")) or not storage.get("region") or not prefix.strip("/"):
                raise ValueError(f"{role}: S3 requires bucket, region, and a nonempty relative prefix")
    network = config.get("network", {})
    if set(network) - {"mode", "cidr", "client_cidrs", "network_id", "subnet_id"}:
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
           operator_secrets=None):
    """`operator_secrets` maps a Hot service role to extra secret names, such
    as SMTP or OAuth credentials, that it mounts from the operator bundle."""
    config = validate(document)
    if set(manifest) - MANIFEST_KEYS:
        raise ValueError("Unknown manifest fields: " + ", ".join(sorted(set(manifest) - MANIFEST_KEYS)))
    if manifest.get("schema_version") != 1 or set(manifest.get("images", {})) != IMAGES:
        raise ValueError("Manifest schema 1 requires hot/postgres/valkey/ingress images")
    if manifest.get("configuration_schema", 1) != 1:
        raise ValueError("This renderer supports configuration schema 1 only")
    if not all(DIGEST.fullmatch(image) for image in manifest["images"].values()):
        raise ValueError("Every image must be pinned by OCI sha256 digest")
    for path in (data_root, secrets_root):
        if not Path(path).is_absolute():
            raise ValueError("Data and tmpfs secret roots must be absolute paths")
    if s3_auth not in {"identity", "files"}:
        raise ValueError("S3 authentication must be identity or files")
    if s3_session_tokens and s3_auth != "files":
        raise ValueError("Session-token files require S3 file authentication")
    images = manifest["images"]
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
        "HOT_LOG_TARGET": "stdout", "HOT_LOG_LEVEL": "info", "HOT_DAEMON": "true",
        "HOT_APP_HOST": "0.0.0.0", "HOT_API_HOST": "0.0.0.0",
        "HOT_TASK_MAX_CONCURRENT": "1", "HOT_TASK_CODE_MAX_CONCURRENT": "2",
        "HOT_TASK_CONTAINER_MAX_CONCURRENT": "1", "HOT_TASK_WORKER_MEMORY_MB": "1536",
        "HOT_TASK_WORKER_DISK_MB": "10240", "HOT_BOX_ENABLED": "true",
        "HOT_WORKER_THREADS": "1", "HOT_ENGINE_THREADS": "2",
        "HOT_BOX_BACKEND": "kata", "HOT_BOX_CONTAINERD_SOCKET": "/run/kata-containerd/containerd.sock",
        "HOT_BOX_VMM": "qemu",
    }
    # User code may never reach the installation itself, even when an operator
    # allowlists private ranges that overlap it.
    deny = [RUNTIME_SUBNET]
    if config.get("network", {}).get("mode", "new") == "new":
        deny.append(config.get("network", {}).get("cidr", "10.68.0.0/16"))
    for key in config.get("settings", {}):
        if key in common_env or key.endswith(("_STORAGE_TYPE", "_STORAGE_PATH")) or "_AWS_S3_" in key:
            raise ValueError(f"settings entry {key} is set by the runtime bundle")
    for role in SERVICES:
        env, mounts = {**common_env, **config.get("settings", {})}, []
        if role in {"worker", "task-worker"}:
            env["HOT_NETWORK_DENY_CIDRS"] = ",".join(deny)
        keys = ["HOT_DB_URI", "HOT_REDIS_URI"]
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
            else:
                for field in ("bucket", "region", "prefix"):
                    env[prefix + "_AWS_S3_" + field.upper()] = storage.get(field, storage_role + "s")
                if s3_auth == "files":
                    keys += [prefix + "_AWS_ACCESS_KEY_ID", prefix + "_AWS_SECRET_ACCESS_KEY"]
                    if s3_session_tokens:
                        keys.append(prefix + "_AWS_SESSION_TOKEN")
        keys += [key for key in (operator_secrets or {}).get(role, []) if key not in keys]
        for key in keys:
            env[key + "_FILE"] = "/run/secrets/" + key
        services[role] = {
            "image": images["hot"], "platform": "linux/amd64", "command": [role],
            "user": "10001:10001", "working_dir": "/var/lib/hot", "init": True,
            "restart": "unless-stopped", "read_only": True, "stop_grace_period": "120s",
            "environment": env, "secrets": [secret(role, key) for key in keys],
            "volumes": mounts, "tmpfs": ["/tmp:size=268435456,uid=10001,gid=10001", "/var/lib/hot/.hot:size=268435456,uid=10001,gid=10001"],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            "logging": {"driver": "local", "options": {"max-size": "10m", "max-file": "3"}},
            "depends_on": {"postgres": {"condition": "service_healthy"}, "valkey": {"condition": "service_healthy"}},
            "mem_limit": {"app": "768m", "api": "768m", "worker": "1536m", "scheduler": "256m", "task-worker": "1536m"}[role],
            "cpus": 1.0 if role in {"worker", "task-worker"} else 0.5,
        }
    task = services["task-worker"]
    task.update(user="0:2000", privileged=True, pid="host", read_only=False)
    task.pop("cap_drop")
    task.pop("security_opt")
    for path, readonly in (("/run/kata-containerd", False), ("/run/vc", False), ("/var/run/netns", False), ("/opt/cni/bin", True), ("/var/lib/cni", False), ("/tmp/hot-box-io", False)):
        task["volumes"].append(bind(path, path, readonly))
    services["postgres"] = {
        "image": images["postgres"], "restart": "unless-stopped", "shm_size": "256m", "mem_limit": "1536m",
        "environment": {"POSTGRES_DB": "hot", "POSTGRES_USER": "hot_owner", "POSTGRES_PASSWORD_FILE": "/run/secrets/POSTGRES_PASSWORD"},
        "secrets": [secret("postgres", k) for k in ("POSTGRES_PASSWORD", "POSTGRES_RUNTIME_PASSWORD")],
        # PostgreSQL 18 images keep data in /var/lib/postgresql/<major>/docker and
        # refuse a mount at the older /var/lib/postgresql/data.
        "volumes": [bind(Path(data_root) / "postgres", "/var/lib/postgresql"), {"type": "bind", "source": "./init.sql", "target": "/docker-entrypoint-initdb.d/10-hot.sql", "read_only": True}],
        "healthcheck": {"test": ["CMD", "pg_isready", "-U", "hot_owner", "-d", "hot"], "interval": "5s", "timeout": "3s", "retries": 12},
        "logging": {"driver": "local", "options": {"max-size": "10m", "max-file": "3"}},
    }
    services["valkey"] = {
        "image": images["valkey"], "restart": "unless-stopped", "mem_limit": "384m",
        "command": ["valkey-server", "/etc/valkey/valkey.conf"],
        "secrets": [secret("valkey", k) for k in ("VALKEY_ACL", "VALKEY_PASSWORD")],
        "volumes": [bind(Path(data_root) / "valkey", "/data"), {"type": "bind", "source": "./valkey.conf", "target": "/etc/valkey/valkey.conf", "read_only": True}],
        "healthcheck": {"test": ["CMD-SHELL", "valkey-cli --askpass < /run/secrets/VALKEY_PASSWORD | grep -qx PONG"], "interval": "5s", "timeout": "3s", "retries": 12},
        "logging": {"driver": "local", "options": {"max-size": "10m", "max-file": "3"}},
    }
    services["ingress"] = {
        "image": images["ingress"], "restart": "unless-stopped", "user": "10001:10001", "read_only": True,
        "entrypoint": ["nginx", "-g", "daemon off;"], "ports": ["443:8443"], "mem_limit": "128m",
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"], "tmpfs": ["/tmp:size=16777216,uid=10001,gid=10001"],
        "secrets": [secret("ingress", k) for k in ("TLS_CERTIFICATE", "TLS_PRIVATE_KEY")],
        "volumes": [{"type": "bind", "source": "./nginx.conf", "target": "/etc/nginx/nginx.conf", "read_only": True}],
        "logging": {"driver": "local", "options": {"max-size": "10m", "max-file": "3"}},
        "depends_on": ["app", "api"],
    }
    services["migrate"] = {
        "image": images["hot"], "platform": "linux/amd64", "profiles": ["admin"],
        "command": ["db", "migrate"], "user": "10001:10001", "read_only": True,
        "environment": {"HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host",
            "HOT_DB_URI_FILE": "/run/secrets/HOT_DB_MIGRATION_URI",
            "HOT_DB_MIGRATION_URI_FILE": "/run/secrets/HOT_DB_MIGRATION_URI"},
        "secrets": [secret("migrate", "HOT_DB_MIGRATION_URI")],
        "tmpfs": ["/tmp:uid=10001,gid=10001"],
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        "depends_on": {"postgres": {"condition": "service_healthy"}},
    }
    if config.get("admin_email"):
        # The first administrator; rerunning changes nothing (hot admin bootstrap).
        services["bootstrap-admin"] = {
            "image": images["hot"], "platform": "linux/amd64", "profiles": ["admin"],
            "command": ["admin", "bootstrap", "--email", config["admin_email"], "--password-file", "/run/secrets/ADMIN_PASSWORD"],
            "user": "10001:10001", "read_only": True,
            "environment": {"HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host", "HOT_DB_URI_FILE": "/run/secrets/HOT_DB_URI"},
            "secrets": [secret("bootstrap-admin", k) for k in ("HOT_DB_URI", "ADMIN_PASSWORD")],
            "tmpfs": ["/tmp:uid=10001,gid=10001"],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
            "depends_on": {"postgres": {"condition": "service_healthy"}},
        }
    services["provision"] = {
        "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
        "entrypoint": ["psql"],
        "command": ["--host=postgres", "--username=hot_owner", "--dbname=hot", "--set=ON_ERROR_STOP=1", "--set=runtime_role=hot_runtime", "--set=schema=hot", "--file=/etc/hot/provision_runtime.sql"],
        "environment": {"PGPASSFILE": "/run/secrets/PGPASSFILE"},
        "secrets": [secret("provision", "PGPASSFILE")],
        "volumes": [{"type": "bind", "source": "./provision_runtime.sql", "target": "/etc/hot/provision_runtime.sql", "read_only": True}],
        "depends_on": {"migrate": {"condition": "service_completed_successfully"}},
        "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
    }
    # No Docker socket is ever mounted. The separate Kata daemon owns guests.
    compose = {"name": config["name"], "services": services, "secrets": secrets,
               "networks": {"default": {"ipam": {"config": [{"subnet": RUNTIME_SUBNET}]}}}}
    return compose


def nginx_config(config):
    servers = []
    for role, port in (("app", 4680), ("api", 4681)):
        servers.append(f'''server {{
  listen 8443 ssl;
  server_name {config[role + '_hostname']};
  location / {{
    proxy_pass http://{role}:{port};
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header X-Forwarded-For $remote_addr;
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
  client_body_temp_path /tmp/client_body;
  proxy_temp_path /tmp/proxy;
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
        files = {"compose.json": json.dumps(compose, indent=2) + "\n", "nginx.conf": nginx_config(config), "init.sql": INIT_SQL, "valkey.conf": VALKEY_CONF,
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
