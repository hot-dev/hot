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
import urllib.parse

SERVICES = ("app", "api", "worker", "scheduler", "task-worker")
# The local Valkey's only login; its ACL turns the default user off.
VALKEY_USER = "hot"
# `--askpass` reads the password from stdin; PING is the command it then sends.
VALKEY_HEALTHCHECK = f"valkey-cli --user {VALKEY_USER} --askpass PING < /run/secrets/VALKEY_PASSWORD | grep -qx PONG"
IMAGES = {"hot", "postgres", "valkey", "ingress"}
DIGEST = re.compile(r"^[a-z0-9][a-z0-9._:/-]*@sha256:[a-f0-9]{64}$")
MANIFEST_KEYS = {"schema_version", "release", "module_tag", "configuration_schema", "migrations", "upgrade", "images",
                 "minimum_renderer"}
# Hosts render and provision with the renderer and grant script baked into
# their image, not the release's. Bump this when a release depends on a
# change to either (a new setting, secret mount, or grant): its manifest then
# names this version, and older host images refuse to deploy it.
RENDERER_VERSION = 1
# The release workflow signs manifests keylessly with its GitHub OIDC identity.
RELEASE_SIGNER = r"^https://github\.com/hot-dev/hot/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$"
RELEASE_ISSUER = "https://token.actions.githubusercontent.com"
# What one default box takes from the host, outside every container's limit.
# Its Kata guest runs in its own /hot-box/<id> cgroup, and static sizing gives it
# default_memory (setup-host.sh) plus the box's memory limit (the task worker's
# BoxLimits::DEFAULT_MEMORY_MB) as RAM. The box's 500 MB /tmp is a tmpfs inside
# that guest, so it is part of this RAM and not added, although Hot's admission
# budget counts memory plus /tmp. QEMU, virtiofsd, and the Kata shim run on the
# host beside the guest: Kata's RuntimeClass for QEMU declares 160 MiB for them,
# rounded up here because it has not been measured. A box may ask for more memory
# and less /tmp than the default and take more than this, within Hot's budget.
BOX_MEMORY_MB = 512
KATA_GUEST_BASE_MB = 512
KATA_HOST_OVERHEAD_MB = 256
BOX_HOST_MB = BOX_MEMORY_MB + KATA_GUEST_BASE_MB + KATA_HOST_OVERHEAD_MB
# Per-size starting allocations within the VM sizes the stacks choose (8, 16,
# and 32 GB; 2, 4, and 8 vCPUs). A size's caps, BOX_HOST_MB for each of its
# container slots, and HOST_RESERVE_MB add up to no more than the VM. The task
# worker's cap also sets Hot's box budget (runtime_budget.rs): what the cap leaves
# after 512 MB for the process and reserved_memory_mb, less code_memory_percent
# for code VMs; 512 MB and 50% unless a size says otherwise. Hot fails a box that
# counts for more than the budget at once, and on small those defaults would leave
# 256 MB for a default box's 1012 MB of memory and /tmp, so small gives the box all
# of it. Guests run outside the cap and BOX_HOST_MB already plans for this one, so
# that moves Hot's count and no memory: the reserve is only a number to Hot, and a
# share of 0 still runs one code VM, as 256 MB does.
# Its disk_mb sets the disk budget, what it leaves after 10 GB: 8448 MB for each
# container slot's default box, within the box pool (40, 100, and 200 GB) that
# holds the boxes' images and writable rootfs. A box's /data is a file beside the
# pool on the data disk (BOX_DATA_DIRECTORY), allocated when the box starts, and
# the budget counts it. Where a local PostgreSQL or Valkey shares that disk, the
# pool's data file is allocated in full so box writes cannot take the database's
# space (setup-host.sh); its metadata file stays sparse. Boxes then use at most
# the pool, the budget, and the metadata, which stay under the disk alarm with
# room for everything else on the disk.
# Not measured: the small worker with a local embedding model. A worker loads the
# model into its own process the first time a ::hot::store map embeds, and past
# Hot's 512 MB reserve and one 256 MB VM its cap leaves 256 MB, against about 440 MB
# on disk for the default model. Small is exactly full, so more for the worker means
# less for something else (Real-account qualification in infra/README.md).
# Conservative caps, not benchmarked limits. Memory in MB.
SIZES = {
    "small": {"memory": {"postgres": 1280, "valkey": 384, "ingress": 128, "app": 768, "api": 512,
                         "scheduler": 256, "worker": 1024, "task-worker": 1536},
              "cpus": {"worker": 1.0, "task-worker": 1.0}, "default_cpus": 0.5, "valkey_maxmemory": 256,
              "task": {"max": 1, "code": 2, "container": 1, "disk_mb": 20480,
                       "reserved_memory_mb": 0, "code_memory_percent": 0}, "engine_threads": 2},
    "medium": {"memory": {"postgres": 2560, "valkey": 768, "ingress": 256, "app": 1024, "api": 768,
                          "scheduler": 256, "worker": 2048, "task-worker": 5120},
               "cpus": {"worker": 1.5, "task-worker": 2.0}, "default_cpus": 1.0, "valkey_maxmemory": 512,
               "task": {"max": 2, "code": 4, "container": 2, "disk_mb": 40960}, "engine_threads": 3},
    "large": {"memory": {"postgres": 6144, "valkey": 1536, "ingress": 256, "app": 2048, "api": 1792,
                         "scheduler": 512, "worker": 5120, "task-worker": 9216},
              "cpus": {"worker": 3.0, "task-worker": 4.0}, "default_cpus": 2.0, "valkey_maxmemory": 1024,
              "task": {"max": 4, "code": 8, "container": 4, "disk_mb": 81920}, "engine_threads": 6},
}
VM_MEMORY_MB = {"small": 8192, "medium": 16384, "large": 32768}
VM_VCPUS = {"small": 2, "medium": 4, "large": 8}
# What a host keeps for the OS, Docker, the agent, and Kata's containerd.
HOST_RESERVE_MB = 1024
# A scalable installation's hosts run one role each, on VMs of the size's
# memory and vCPUs (the contract's role machines), so the role has its VM to
# itself rather than a single machine's slice, and no CPU cap keeps it from
# the load that scales its group out. A worker host runs one worker loop per
# vCPU, within its PostgreSQL pool, since one loop would top out near one
# core. A task-worker host runs tasks in proportion to its vCPUs, within the
# box pool (40, 100, 200 GB). Its data disk holds no database, so the pool's
# files stay sparse there, and the budget and the pool together fit that disk.
# On medium and large, the task worker's cap, BOX_HOST_MB for each container slot,
# and HOST_RESERVE_MB add up to the VM. A small task host can reach 512 MB past
# that plan in the worst case, 5120 + 2 x 1280 against 7168, and then keeps 512 MB
# rather than 1 GB. This is accepted: it takes the container at its cap, although
# Hot's share of that cap for boxes is not used inside the container, and a 4608 MB
# cap or one container slot would fit but run one default box at a time.
SCALABLE_TASK = {
    "small": {"max": 2, "code": 4, "container": 2, "disk_mb": 30720},
    "medium": {"max": 4, "code": 8, "container": 4, "disk_mb": 81920},
    "large": {"max": 8, "code": 16, "container": 8, "disk_mb": 163840},
}
# Rewritten by the scheduler while it dispatches; on the scheduler's tmpfs.
SCHEDULER_HEARTBEAT = "/tmp/hot-scheduler-heartbeat"
# The task worker's slots in use, which the host agent reads.
TASK_STATUS = "/tmp/hot-task-status.json"
# Downloaded embedding models, on the data disk: the default model is larger
# than the services' memory-backed .hot tmpfs.
MODELS_PATH = "/var/lib/hot/models"
# Each role's directory under the data root. The task worker runs as root, so
# it keeps its own copy, beside the others' and not inside it: in a shared one,
# an interrupted root download leaves directories the other services cannot
# write, and root would load files they can.
MODEL_DIRECTORIES = {"app": "models", "worker": "models", "task-worker": "models-task-worker"}
MODEL_ROLES = tuple(MODEL_DIRECTORIES)
# Extracted bundles, on the data disk: a worker keeps every deployed build's,
# and one bundle can hold 100 MB, so a few would fill the memory-backed .hot
# tmpfs and count against the service's memory limit. Each role's directory
# under the data root, separate for the reason its models are, and where Hot
# extracts into, from its working directory.
EXTRACTION_DIRECTORIES = {"worker": "extractions", "task-worker": "extractions-task-worker"}
EXTRACTION_ROOTS = {"worker": ".hot/run", "task-worker": ".hot/task-worker"}
# Each box's /data, an ext4 file the task worker loop-mounts, under the data root.
# Its default is under its own /tmp, a tmpfs inside its container: too small, and
# out of sight of Kata's shim, which resolves a bind mount's source on the host.
BOX_DATA_DIRECTORY = "box-data"

# Where a single machine's cloud load balancer connects from: on GCP, Google's
# front-end ranges; on AWS, an Application Load Balancer's own subnets, which
# the stack passes as load_balancer_cidrs.
GCP_LOAD_BALANCER_RANGES = ("35.191.0.0/16", "130.211.0.0/22")
LOAD_BALANCER_PORT = 9443


def roles(config):
    """The Hot services this host runs: all of them on a single machine, or
    one role's share of a scalable installation."""
    return tuple(role for role in SERVICES if role in (config.get("roles") or SERVICES))


def web_roles(config):
    return tuple(role for role in ("app", "api") if role in roles(config))


def task_worker_memory_mb(available_mb):
    """The task worker's memory when its Kata guests share `available_mb`
    with it. Box guests run outside its container, and Hot gives them half
    of what its memory M leaves after the 1 GB it holds back, so
    M + (M - 1024) / 2 must fit. That is Hot's count: a default box takes
    BOX_HOST_MB on the host, and SCALABLE_TASK says how that compares."""
    return (2 * available_mb + 1024) // 3 // 512 * 512


def runs_migrations(config):
    """Whether this host migrates the database: the single machine, or the
    scalable installation's scheduler host."""
    return config.get("migrations", True)


def scalable(config):
    return config.get("preset", "single-machine") == "scalable"


def public_endpoints(config):
    return [role for role in ("app", "api") if config.get(role + "_exposure", "private") == "public"]


def load_balancer_sources(config):
    """The hops a single machine's public load balancer adds to
    X-Forwarded-For. Google's global load balancer also appends its forwarding
    rule's own address after the client's, which the stack passes as
    load_balancer_addresses. An Application Load Balancer connects from its
    own subnets, which the stack passes as load_balancer_cidrs; network.cidr
    does not describe an existing network, and is only the fallback for a
    configuration without them."""
    if config["cloud"] == "gcp":
        addresses = config.get("load_balancer_addresses", [])
        return [*GCP_LOAD_BALANCER_RANGES, *(ipaddress.ip_network(a).with_prefixlen for a in addresses)]
    if "load_balancer_cidrs" in config:
        return list(config["load_balancer_cidrs"])
    return [config.get("network", {}).get("cidr", "10.68.0.0/16")]


def trusted_proxies(config):
    """Where the app and API take client addresses from forwarding headers:
    Nginx, and a single machine's public load balancer. A scalable
    installation's passthrough load balancers keep the client's address and
    add no hop, so Nginx's peer is the client and nothing else is trusted."""
    if scalable(config) or not public_endpoints(config):
        return [RUNTIME_SUBNET]
    return [RUNTIME_SUBNET, *load_balancer_sources(config)]


# The bundle's container network, where PostgreSQL, Valkey, ingress, and the
# Hot services listen. Fixed so user code can be denied all of it.
RUNTIME_SUBNET = "172.30.0.0/24"
# Host networks that reach the host itself: the Kata guest bridge (the host
# is 10.88.0.1), routed to the host and never to a remote service. Host images
# run Docker with no default bridge, and this renderer ships in them, so
# 172.17.0.0/16 is an ordinary range an installation may use and an operator
# may allowlist. A docker0 added to a host anyway is one of the host's own
# addresses, which the agent denies (host_addresses).
HOST_BRIDGES = ("10.88.0.0/16",)


def listed_networks(config, key):
    """A setting's comma-separated entries, as Hot reads them."""
    value = config.get("settings", {}).get(key, "")
    return [entry.strip() for entry in value.split(",") if entry.strip()]


def operator_deny(config):
    """The operator's own network.deny-cidrs entries, from settings."""
    return listed_networks(config, "HOT_NETWORK_DENY_CIDRS")


def data_deny(config):
    """Networks where a managed or existing data service's standbys and replicas
    can be (data.<service>.endpoint.deny_cidrs). The address a service resolves
    to when a bundle renders is only its current one, and a failover moves it."""
    data = config.get("data", {})
    return [cidr for service in ("postgres", "valkey")
            for cidr in (data.get(service, {}).get("endpoint") or {}).get("deny_cidrs", [])]


def user_code_deny(config, extra_deny=()):
    """Addresses user code may never reach, even when an operator allowlists
    private ranges that overlap them: the bundle's network, the host's
    bridges, a stack-created network, the data services' networks, `extra_deny`
    (data services' and the host's own addresses), and the operator's
    network.deny-cidrs. The task worker's box guests get the same list."""
    deny = [RUNTIME_SUBNET, *HOST_BRIDGES]
    if config.get("network", {}).get("mode", "new") == "new":
        deny.append(config.get("network", {}).get("cidr", "10.68.0.0/16"))
    for cidr in (*data_deny(config), *extra_deny, *operator_deny(config)):
        if cidr not in deny:
            deny.append(cidr)
    return deny


# The forms Hot's parsers take. Python's also take a netmask or hostmask after
# the slash (10.0.0.0/255.0.0.0) and an IPv6 zone (fe80::1%eth0).
IP_FORM = re.compile(r"[0-9A-Fa-f:.]+(/(0|[1-9][0-9]{0,2}))?")


def is_ip(value, network=True):
    """An IP address, or with `network` also a CIDR, as Hot parses them."""
    if not isinstance(value, str) or not IP_FORM.fullmatch(value):
        return False
    try:
        ipaddress.ip_network(value, strict=False) if network else ipaddress.ip_address(value)
    except (TypeError, ValueError):
        return False
    return True


def is_explicit_cidr(value):
    """An IPv4 network written as a CIDR, without host bits, narrower than /0."""
    if not isinstance(value, str) or "/" not in value or not IP_FORM.fullmatch(value):
        return False
    try:
        network = ipaddress.ip_network(value)
    except ValueError:
        return False
    return network.version == 4 and network.prefixlen > 0


# Hot and the agent read a range inside these as IPv4 (outbound::canonical_net),
# as they do an IPv4-mapped one, so ::/96 would mean 0.0.0.0/0.
IPV4_IN_IPV6 = (ipaddress.ip_network("::/96"), ipaddress.ip_network("64:ff9b::/96"))


def is_denied_network(value):
    """A data.<service>.endpoint.deny_cidrs entry, as the contract takes it: an
    IPv4 or IPv6 network in canonical form, without host bits, narrower than /0.
    The contract reads IPv4 written in IPv6, whether IPv4-mapped or inside ::/96
    or 64:ff9b::/96, as IPv4, and so refuses that too."""
    if not isinstance(value, str) or "/" not in value or not IP_FORM.fullmatch(value):
        return False
    try:
        network = ipaddress.ip_network(value)
    except ValueError:
        return False
    if network.version == 6 and (network.network_address.ipv4_mapped is not None
                                 or any(network.subnet_of(prefix) for prefix in IPV4_IN_IPV6)):
        return False
    return network.prefixlen > 0 and str(network) == value.lower()


# A network.allow-hosts entry: a name, or *.name for its subdomains. Hot refuses
# a scheme, a port, or a wildcard elsewhere (OutboundRules), and an entry with a
# trailing dot or other characters never matches a request's host.
ALLOWED_HOST = re.compile(r"(\*\.)?[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*")


# The proxy addresses this renderer lets through: http or https, a host, an
# optional port, and no path. Credentials may only be in the operator secret
# bundle's copy. Narrower than the URL parser behind Hot's check
# (egress_proxy_setting), so what passes here also passes there.
EGRESS_PROXY = re.compile(r"https?://(?:[^/?#\\]*@)?"
                          r"(?:\[(?P<ipv6>[0-9A-Fa-f:.]+)\]|(?P<name>[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*\.?))"
                          r"(?::(?P<port>[0-9]{1,5}))?/?", re.IGNORECASE)


def is_egress_proxy(value):
    """Whether `value` is an address Hot takes as network.egress-proxy; blank
    means none. The worker and task worker exit on one Hot refuses, while the
    deploy's readiness check, of the app and API only, still passes. Never
    repeat the value in a message: it may hold the proxy's password."""
    address = value.strip()
    if not address:
        return True
    match = EGRESS_PROXY.fullmatch(address)
    if not match or int(match["port"] or 0) > 65535:
        return False
    if match["ipv6"]:
        return ":" in match["ipv6"] and is_ip(match["ipv6"], network=False)
    # A URL parser reads a name that ends in a number as an IPv4 address.
    name = match["name"].rstrip(".")
    return not re.fullmatch(r"[0-9]+|0[Xx][0-9A-Fa-f]*", name.rpartition(".")[2]) or is_ip(name, network=False)


CONFIG_KEYS = {"schema_version", "name", "cloud", "region", "zone", "project", "size", "host_image", "release_pointer", "secret_references", "app_hostname", "api_hostname", "app_exposure", "api_exposure", "storage", "network",
               # Read by the host agent, not by the bundle.
               "data_volume_id", "admin_email", "manifest_base_url", "status_reference", "command_subscription",
               "installation_parameter",
               "settings", "data", "backup", "monitoring", "logging",
               # Scalable installations: which roles this host runs, whether it
               # migrates, and the pointer that releases migrations.
               "preset", "roles", "migrations", "migration_pointer", "instance_group", "scaling", "data_disk", "db_pool",
               "image_registry",
               # The address a GCP stack's public load balancer adds to
               # X-Forwarded-For after the client's, and the subnets an AWS
               # stack's public load balancer connects from.
               "load_balancer_addresses", "load_balancer_cidrs"}
# data.postgres settings: the contract's, plus the endpoint a managed
# database's stack adds for the host.
POSTGRES_KEYS = {"mode", "runtime_role", "version", "tier", "storage_gb", "high_availability",
                 "backup_retention_days", "deletion_protection", "admin_password_version", "endpoint"}
VALKEY_KEYS = {"mode", "node_type", "high_availability", "auth_token_version", "endpoint"}
# Names that would carry a secret; those belong in the operator secret bundle.
SECRET_MARKERS = ("SECRET", "PASSWORD", "TOKEN", "_KEY", "_URI")
# Settings the bundle sets itself, besides common_environment, and settings that
# would break what it hard-codes: Nginx proxies to the default ports, the
# database jobs use schema hot, and with RLS enforcement off (Hot reads either
# name) the services would migrate with the restricted runtime login, which Hot
# would also stop checking. A section's own name takes JSON that replaces the
# whole section (HOT_DB='{"rls":{"enforce":false}}'), and only what the bundle sets
# comes back after it, so the sections that hold those are refused as well. The
# contract refuses the same ones (infra/opentofu/modules/single-machine-contract),
# so they fail at plan rather than on the host; test_runtime.py compares the two.
RESERVED_SETTINGS = ("HOT_STORE_MODELS_PATH", "HOT_API_KNOWN_HOSTS", "HOT_APP", "HOT_APP_PORT", "HOT_API", "HOT_API_PORT",
                     "HOT_DB", "HOT_DB_SCHEMA", "HOT_DB_RLS_ENFORCE", "HOT_RLS_ENFORCE", "HOT_TASK_DATA_VOLUME_DIR")
# A NAME_FILE names a file in the container, and the bundle sets those for what
# it mounts. Nothing can mount one for a setting, so Hot would fail to start.
RESERVED_SUFFIXES = ("_STORAGE_TYPE", "_STORAGE_PATH", "_FILE")
RESERVED_PARTS = ("_AWS_S3_", "_GCS_")
RESERVED_PREFIXES = ("HOT_NETWORK_CLIENT_IP_",)


def validate(document):
    config = copy.deepcopy(document.get("installation", document))
    unknown = set(config) - CONFIG_KEYS
    if unknown:
        raise ValueError("Unknown installation fields: " + ", ".join(sorted(unknown)))
    if config.get("schema_version", 1) != 1 or config.get("cloud") not in {"aws", "gcp"}:
        raise ValueError("Expected schema_version=1 and cloud=aws|gcp")
    # Cloud resource names built from it (hot-NAME) may not end in a hyphen.
    if not re.fullmatch(r"[a-z][a-z0-9-]{1,22}[a-z0-9]", config.get("name", "")):
        raise ValueError("Installation name must be 3-24 lowercase letters/digits/hyphens, starting with a letter "
                         "and ending with a letter or digit")
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
        # A stack writes null for a service with no endpoint.
        endpoint = options.get("endpoint") or {}
        if not isinstance(endpoint, dict):
            raise ValueError(f"data.{service}.endpoint must be an object")
        networks = endpoint.get("deny_cidrs", [])
        if not isinstance(networks, list) or not all(is_denied_network(network) for network in networks):
            raise ValueError(f"data.{service}.endpoint.deny_cidrs must list IPv4 or IPv6 networks narrower than /0, "
                             "without host bits, with IPv4 written as IPv4")
        # A local service is on the bundle's own network, which is denied already.
        if networks and options.get("mode", "local") == "local":
            raise ValueError(f"data.{service}.endpoint.deny_cidrs applies to managed and existing services, not local ones")
        # Nothing reads the rest of a service the stack does not provision, so a
        # misspelled key would leave its networks open with no word of it.
        if options.get("mode", "local") != "managed" and set(endpoint) - {"deny_cidrs"}:
            raise ValueError(f"data.{service}.endpoint takes only deny_cidrs unless the stack provisions the service")
    if data_mode(config, "valkey") == "managed":
        endpoint = data["valkey"].get("endpoint")
        # ElastiCache takes an AUTH token from a secret; Memorystore takes the
        # host's IAM tokens and presents its own CA.
        required = {"host", "port", "auth_secret"} if config["cloud"] == "aws" else {"host", "port", "auth", "ca_certificate"}
        if not isinstance(endpoint, dict) or set(endpoint) - (required | {"deny_cidrs"}) or not all(endpoint.get(k) for k in required):
            raise ValueError("Managed Valkey needs the endpoint the stack provisions: " + ", ".join(sorted(required)))
        if config["cloud"] == "gcp" and endpoint["auth"] != "gcp-iam":
            raise ValueError("Managed Valkey on GCP authenticates with gcp-iam")
    if data_mode(config, "postgres") == "managed":
        endpoint = data["postgres"].get("endpoint")
        required = {"host", "port", "admin_user", "admin_secret"} | ({"ca_certificate"} if config["cloud"] == "gcp" else set())
        if not isinstance(endpoint, dict) or set(endpoint) - (required | {"ca_certificate", "deny_cidrs"}) or not all(endpoint.get(k) for k in required):
            raise ValueError("Managed PostgreSQL needs the endpoint the stack provisions: " + ", ".join(sorted(required)))
    if not re.fullmatch(r"[a-z_][a-z0-9_]{0,62}", runtime_role(config)) or runtime_role(config) in {"hot_owner", "hot_admin", "postgres"}:
        raise ValueError("data.postgres.runtime_role must be a PostgreSQL role name other than hot_owner, hot_admin, or postgres")
    # The local database creates hot_runtime once, when it is initialized, and
    # the services log in as it.
    if data_mode(config, "postgres") == "local" and runtime_role(config) != "hot_runtime":
        raise ValueError("A local database's runtime login is hot_runtime; data.postgres.runtime_role applies to "
                         "managed and existing databases only")
    addresses = config.get("load_balancer_addresses", [])
    if not isinstance(addresses, list) or not all(is_ip(a, network=False) for a in addresses):
        raise ValueError("load_balancer_addresses must list IP addresses")
    if "load_balancer_cidrs" in config:
        subnets = config["load_balancer_cidrs"]
        if not isinstance(subnets, list) or not subnets or not all(is_explicit_cidr(c) for c in subnets):
            raise ValueError("load_balancer_cidrs must list explicit IPv4 CIDRs narrower than /0")
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
        if key.endswith("_FILE"):
            raise ValueError(f"settings entry {key} names a file no container has; set {key[:-5]} here, "
                             "or in the operator secret bundle if it carries a credential")
        if reserved_setting(config, key):
            raise ValueError(f"settings entry {key} is set by the runtime bundle")
    # Settings end up in tfvars, compose.json, and every container's
    # environment; the bundle's copy reaches only the services that use it.
    if "@" in settings.get("HOT_NETWORK_EGRESS_PROXY", ""):
        raise ValueError("settings entry HOT_NETWORK_EGRESS_PROXY carries credentials; "
                         "put it in the operator secret bundle")
    # Hot won't start user code with an entry it can't parse, and says so only
    # in the worker's log.
    for key in ("HOT_NETWORK_ALLOW_CIDRS", "HOT_NETWORK_DENY_CIDRS"):
        bad = [entry for entry in listed_networks(config, key) if not is_ip(entry)]
        if bad:
            raise ValueError(f"settings {key} entries must be IP addresses or CIDRs: " + ", ".join(bad))
    # Nor with these. A mistaken one may hold credentials, so no message repeats it.
    if not all(ALLOWED_HOST.fullmatch(entry) for entry in listed_networks(config, "HOT_NETWORK_ALLOW_HOSTS")):
        raise ValueError("settings HOT_NETWORK_ALLOW_HOSTS entries must be hostnames or *.domain wildcards, "
                         "without a scheme, port, or path")
    if not is_egress_proxy(settings.get("HOT_NETWORK_EGRESS_PROXY", "")):
        raise ValueError("settings entry HOT_NETWORK_EGRESS_PROXY must be an http:// or https:// proxy address: "
                         "a host, an optional port, and no path")
    # The app panics on anything else, once the old release has stopped.
    if settings.get("HOT_APP_SIGNUP", "invite").strip().lower() not in {"open", "invite"}:
        raise ValueError("settings entry HOT_APP_SIGNUP must be open or invite")
    # Every Hot service exits on these (hot::redis::configure). gcp-iam needs
    # Google's metadata server and a rediss:// URI, which a local Valkey's isn't.
    redis_auth = settings.get("HOT_REDIS_AUTH", "").strip()
    redis_ca = settings.get("HOT_REDIS_CA_CERT", "").strip()
    if redis_auth not in {"", "password", "gcp-iam"}:
        raise ValueError("settings entry HOT_REDIS_AUTH must be password or gcp-iam")
    if redis_auth == "gcp-iam" and (config["cloud"] != "gcp" or data_mode(config, "valkey") == "local"):
        raise ValueError("settings entry HOT_REDIS_AUTH can be gcp-iam only on GCP, with a managed or existing Valkey")
    if redis_ca and "-----BEGIN CERTIFICATE-----" not in redis_ca:
        raise ValueError("settings entry HOT_REDIS_CA_CERT must hold PEM certificates")
    if (settings.get("HOT_REDIS_CLUSTER", "").strip().lower() in {"1", "true", "on", "yes"}
            and (redis_auth == "gcp-iam" or redis_ca or memorystore(config))):
        raise ValueError("settings entry HOT_REDIS_CLUSTER cannot be on with gcp-iam or a CA certificate, "
                         "which support standalone servers only")
    # Hot keeps calls forever on anything else and says so only in the worker's
    # log, which no one reads until the data disk fills.
    retention = settings.get("HOT_DB_RETENTION_CALL_DAYS", "-1")
    if retention != "-1" and not (re.fullmatch(r"[1-9][0-9]{0,4}", retention) and int(retention) <= 36500):
        raise ValueError("settings entry HOT_DB_RETENTION_CALL_DAYS must be -1, to keep calls forever, "
                         f"or a whole number of days from 1 to 36500, not {retention!r}")
    # How many failed objects a storage move may leave behind. The agent holds it
    # against its last report, but only once a move starts. The contract refuses
    # the same values. A mistaken one may hold anything, so no message repeats it.
    if not re.fullmatch(r"[1-9][0-9]*", settings.get("HOT_MOVE_STORAGE_SKIP_FAILED", "1")):
        raise ValueError("settings entry HOT_MOVE_STORAGE_SKIP_FAILED must be a whole number of 1 or more, the count of "
                         "failed objects the last storage move reported; to clear it, remove its entry")
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
            if (not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", storage.get("bucket", ""))
                    or not re.fullmatch(r"[a-z0-9-]+", storage.get("region", "")) or not prefix.strip("/")):
                raise ValueError(f"{role}: S3 requires bucket, region, and a nonempty relative prefix")
        # The stacks put the prefix into the host's IAM resource patterns and
        # conditions, where a wildcard or a quote would widen its grant. These
        # forms also leave no $ in what the bundle carries, where Compose
        # escaping would double it and the agent's comparison with the deployed
        # bundle would see a move. A local backend carries none of them.
        if storage["backend"] != "local" and not re.fullmatch(r"[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*/?", prefix):
            raise ValueError(f"{role}: a storage prefix takes letters, digits, '.', '_', '-', and '/' separators")
    network = config.get("network", {})
    if set(network) - {"mode", "cidr", "client_cidrs", "network_id", "subnet_id", "public_subnet_ids", "data_subnet_ids", "private_subnet_ids"}:
        raise ValueError("Unknown network fields")
    if network.get("mode", "new") not in {"new", "existing"}:
        raise ValueError("Select new or existing networking")
    if network.get("mode") == "existing" and not all(network.get(k) for k in ("network_id", "subnet_id")):
        raise ValueError("Existing networking requires network_id and subnet_id")
    if not is_ip(network.get("cidr", "10.68.0.0/16")):
        raise ValueError("Network requires IPv4 /24 or larger")
    cidr = ipaddress.ip_network(network.get("cidr", "10.68.0.0/16"))
    if cidr.version != 4 or cidr.prefixlen > 24:
        raise ValueError("Network requires IPv4 /24 or larger")
    if not all(is_explicit_cidr(client) for client in network.get("client_cidrs", [])):
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


def memorystore(config):
    """Whether Valkey is Memorystore, reached with IAM tokens and the instance's
    CA certificate, which the bundle sets."""
    return data_mode(config, "valkey") == "managed" and config["cloud"] == "gcp"


def runtime_role(config):
    return config.get("data", {}).get("postgres", {}).get("runtime_role", "hot_runtime")


def allocations(config):
    """Each container's memory in MB, and the task worker's limits."""
    size_name = config.get("size", "small")
    size = SIZES[size_name]
    allocated, task_limits = dict(size["memory"]), size["task"]
    if scalable(config):
        hot_roles = roles(config)
        share = (VM_MEMORY_MB[size_name] - HOST_RESERVE_MB
                 - (size["memory"]["ingress"] if web_roles(config) else 0)) // len(hot_roles)
        allocated.update({role: share for role in hot_roles})
        if "task-worker" in hot_roles:
            allocated["task-worker"] = task_worker_memory_mb(share)
            task_limits = SCALABLE_TASK[size_name]
    return allocated, task_limits


def common_environment(config):
    """What every Hot service is told, which `settings` may not change."""
    size_name = config.get("size", "small")
    size = SIZES[size_name]
    allocated, task_limits = allocations(config)
    worker_threads, engine_threads = 1, size["engine_threads"]
    if scalable(config):
        worker_threads = engine_threads = VM_VCPUS[size_name]
    env = {
        "HOT_ENV": "production", "HOT_PRODUCT_EXPERIENCE": "self-host",
        "HOT_ENGINE_ISOLATION": "true", "HOT_NETWORK_ALLOW_PRIVATE": "false",
        "HOT_QUEUE_TYPE": "redis", "HOT_EMITTER_TYPE": "db",
        # Hot's default store is SQLite, which refuses the PostgreSQL database
        # every bundle has, so every ::hot::store call would fail.
        "HOT_STORE_TYPE": "postgres",
        "HOT_FILE_MODE": "service", "HOT_BLOB_MODE": "service", "HOT_LOG_FORMAT": "json",
        "HOT_LOG_TARGET": "stdout", "HOT_LOG_LEVEL": "info",
        "HOT_APP_HOST": "0.0.0.0", "HOT_API_HOST": "0.0.0.0",
        # Links in email, OAuth redirects, and the API address the app shows
        # use the installation's own hostnames, never Hot's cloud defaults.
        "HOT_APP_URL": f"https://{config['app_hostname']}", "HOT_API_URL": f"https://{config['api_hostname']}",
        "HOT_OAUTH_REDIRECT_BASE_URL": f"https://{config['app_hostname']}",
        "HOT_COOKIE_DOMAIN": config["app_hostname"],
        "HOT_TASK_MAX_CONCURRENT": str(task_limits["max"]),
        "HOT_TASK_CODE_MAX_CONCURRENT": str(task_limits["code"]),
        "HOT_TASK_CONTAINER_MAX_CONCURRENT": str(task_limits["container"]),
        "HOT_TASK_WORKER_MEMORY_MB": str(allocated["task-worker"]),
        "HOT_TASK_CONTAINER_RESERVED_MEMORY_MB": str(task_limits.get("reserved_memory_mb", 512)),
        "HOT_TASK_CODE_MEMORY_PERCENT": str(task_limits.get("code_memory_percent", 50)),
        "HOT_TASK_WORKER_DISK_MB": str(task_limits["disk_mb"]), "HOT_BOX_ENABLED": "true",
        "HOT_WORKER_THREADS": str(worker_threads), "HOT_ENGINE_THREADS": str(engine_threads),
        "HOT_BOX_BACKEND": "kata", "HOT_BOX_CONTAINERD_SOCKET": "/run/kata-containerd/containerd.sock",
        "HOT_BOX_VMM": "qemu",
    }
    if scalable(config):
        # Each instance's PostgreSQL pool, so every role's maximum fits the
        # database's connection budget.
        env["HOT_WORKER_DB_MAX_CONNECTIONS"] = str(config.get("db_pool", 20))
    return env


def reserved_setting(config, key):
    """Whether the runtime bundle sets `key` itself, so `settings` may not."""
    return (key in common_environment(config) or key in RESERVED_SETTINGS or key.endswith(RESERVED_SUFFIXES)
            or any(part in key for part in RESERVED_PARTS) or key.startswith(RESERVED_PREFIXES)
            # The bundle sets Memorystore's login, and Hot exits on a setting
            # beside the _FILE that mounts its CA.
            or (key in ("HOT_REDIS_AUTH", "HOT_REDIS_CA_CERT") and memorystore(config)))


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


def valkey_acl(password):
    """The local Valkey's ACL file: the default user is off, so every client
    logs in as VALKEY_USER."""
    return f"user default off\nuser {VALKEY_USER} on >{password} ~* &* +@all\n"


def pg_service(uri, name="hot"):
    """A libpq connection service file for a postgres:// URI. The bundle's
    database jobs connect through one (PGSERVICEFILE, then service=<name>),
    so neither the connection string nor its password is ever in a process's
    arguments, which every process on the host can read. Values mean what they
    mean to libpq reading the URI: only %XX is decoded, so a + stays a +. A
    repeated or blank query parameter is refused: the file leaves out a blank
    value, so sslmode=verify-full&sslmode= would connect with libpq's default
    after the agent's check had read verify-full."""
    parts = urllib.parse.urlsplit(uri)
    if parts.scheme not in ("postgres", "postgresql") or not parts.hostname:
        raise ValueError("A database connection must be a postgres:// URI")

    def decoded(text):
        # A UnicodeDecodeError is a ValueError; bytes are never replaced.
        return urllib.parse.unquote(text, errors="strict")

    params = {"host": parts.hostname, "port": str(parts.port or ""),
              "dbname": decoded(parts.path.lstrip("/")),
              "user": decoded(parts.username or ""),
              "password": decoded(parts.password or "")}
    queried = set()
    for pair in parts.query.split("&"):
        if not pair:  # no query, or a trailing &
            continue
        key, _, value = pair.partition("=")
        key, value = decoded(key), decoded(value)
        if key in queried or not value:
            raise ValueError(f"The database connection's {key} is repeated or blank")
        queried.add(key)
        params[key] = value
    lines = [f"[{name}]"]
    for key, value in params.items():
        if not value:
            continue
        # One keyword=value per line, with the value's ends trimmed; a NUL
        # would end the value early.
        if (not re.fullmatch(r"[a-z_]+", key) or key == "service" or re.search(r"[\r\n\0]", value)
                or value != value.strip()):
            raise ValueError(f"The database connection's {key} cannot be written to a service file")
        lines.append(f"{key}={value}")
    return "\n".join(lines) + "\n"


def extraction_entrypoint(role):
    """Start a service with nothing extracted, as a tmpfs would. Hot does not
    sync an extraction, so a host crash or a restored snapshot can leave one
    marked complete with empty files, and a deployed build's is never swept.
    A one-off command on the service's image, which can run beside it, leaves
    the directory alone."""
    clear = f"find {EXTRACTION_ROOTS[role]} -mindepth 1 -delete"
    return ["sh", "-c", f'[ "$1" != {role} ] || {clear}; exec /usr/bin/hot "$@"', "sh"]


def literal(value):
    """Compose interpolates $ in every value; nothing the bundle renders
    means it, so each is escaped as $$."""
    if isinstance(value, dict):
        return {key: literal(item) for key, item in value.items()}
    if isinstance(value, list):
        return [literal(item) for item in value]
    return value.replace("$", "$$") if isinstance(value, str) else value


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
    existing database's and the host's own (see user_code_deny)."""
    config = validate(document)
    if set(manifest) - MANIFEST_KEYS:
        raise ValueError("Unknown manifest fields: " + ", ".join(sorted(set(manifest) - MANIFEST_KEYS)))
    if manifest.get("schema_version") != 1 or set(manifest.get("images", {})) != IMAGES:
        raise ValueError("Manifest schema 1 requires hot/postgres/valkey/ingress images")
    if manifest.get("configuration_schema", 1) != 1:
        raise ValueError("This renderer supports configuration schema 1 only")
    needed = manifest.get("minimum_renderer", 1)
    if not isinstance(needed, int) or isinstance(needed, bool) or needed > RENDERER_VERSION:
        raise ValueError(f"Release {manifest.get('release')} needs renderer {needed}, and this host image has "
                         f"{RENDERER_VERSION}; build a host image from the release's tag and deploy it first")
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
    memory = {role: f"{mb}m" for role, mb in allocations(config)[0].items()}
    secrets, services = {}, {}

    def secret(role, key):
        name = role + "-" + key
        secrets[name] = {"file": str(Path(secrets_root) / role / key)}
        return {"source": name, "target": key}

    def bind(source, target, readonly=False, propagation=None):
        options = {"create_host_path": False}
        if propagation:
            options["propagation"] = propagation
        return {"type": "bind", "source": str(source), "target": target, "read_only": readonly, "bind": options}

    common_env = common_environment(config)
    # User code may never reach the installation itself, even when an operator
    # allowlists private ranges that overlap it.
    deny = user_code_deny(config, extra_deny)
    pg_local = data_mode(config, "postgres") == "local"
    valkey_local = data_mode(config, "valkey") == "local"
    # A managed or existing PostgreSQL is reached over TLS verified against its CA.
    db_ca = [] if pg_local else ["POSTGRES_CA_CERT"]
    # Memorystore: IAM tokens, and the instance's CA for TLS.
    valkey_iam = memorystore(config)
    valkey_ca = ["HOT_REDIS_CA_CERT"] if valkey_iam else []
    data_ready = {name: {"condition": "service_healthy"}
                  for name, local in (("postgres", pg_local), ("valkey", valkey_local)) if local}
    # Defaults operators may change in `settings`: accounts are by invitation
    # (after the first administrator is bootstrapped) until they open sign-up.
    defaults = {"HOT_APP_SIGNUP": "invite"}
    for role in roles(config):
        env, mounts = {**common_env, **defaults, **config.get("settings", {})}, []
        if role in {"worker", "task-worker"}:
            # The operator's own deny entries stay, with the bundle's added.
            env["HOT_NETWORK_DENY_CIDRS"] = ",".join(deny)
        if role in {"app", "api"}:
            env.update(HOT_NETWORK_CLIENT_IP_TRUSTED_PROXY="true", HOT_NETWORK_CLIENT_IP_HEADER="x-forwarded-for",
                       HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES=",".join(trusted_proxies(config)))
        if role == "api":
            # The API takes any other Host for a custom domain and answers 404,
            # on /status too.
            env["HOT_API_KNOWN_HOSTS"] = config["api_hostname"]
        if role in MODEL_DIRECTORIES:
            env["HOT_STORE_MODELS_PATH"] = MODELS_PATH
            mounts.append(bind(Path(data_root) / MODEL_DIRECTORIES[role], MODELS_PATH))
        if role in EXTRACTION_ROOTS:
            mounts.append(bind(Path(data_root) / EXTRACTION_DIRECTORIES[role], "/var/lib/hot/" + EXTRACTION_ROOTS[role]))
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
        }
        if role in EXTRACTION_ROOTS:
            services[role]["entrypoint"] = extraction_entrypoint(role)
        if not scalable(config):
            services[role]["cpus"] = size["cpus"].get(role, size["default_cpus"])
    # The scheduler rewrites its heartbeat every 30 seconds while it
    # dispatches schedules, so a stalled scheduler turns unhealthy even though
    # its process runs.
    if "scheduler" in services:
        scheduler = services["scheduler"]
        scheduler["environment"]["HOT_SCHEDULER_HEARTBEAT_PATH"] = SCHEDULER_HEARTBEAT
        scheduler["healthcheck"] = {
            "test": ["CMD-SHELL", f"test $(( $(date +%s) - $(cat {SCHEDULER_HEARTBEAT} 2>/dev/null || echo 0) )) -lt 120"],
            "interval": "30s", "timeout": "5s", "retries": 3, "start_period": "90s"}
    if "task-worker" in services:
        task = services["task-worker"]
        task.update(user="0:2000", privileged=True, pid="host", read_only=False)
        task["environment"]["HOT_TASK_STATUS_PATH"] = TASK_STATUS
        task.pop("cap_drop")
        task.pop("security_opt")
        for path, readonly in (("/run/kata-containerd", False), ("/run/vc", False), ("/var/run/netns", False), ("/opt/cni/bin", True), ("/var/lib/cni", False), ("/tmp/hot-box-io", False)):
            task["volumes"].append(bind(path, path, readonly))
        # Kata's shim takes a box's /data by this path on the host, so it is the
        # same path on both sides, and rshared carries the worker's loop mounts
        # from its container out to the host.
        box_data = str(Path(data_root) / BOX_DATA_DIRECTORY)
        task["environment"]["HOT_TASK_DATA_VOLUME_DIR"] = box_data
        task["volumes"].append(bind(box_data, box_data, propagation="rshared"))
        # A privileged container's /dev is a snapshot of the host's device nodes
        # at start, and `mount -o loop` needs the node for whichever loop device
        # the kernel hands out, including one created later. Without it, boxes past
        # the first few fail with "failed to setup loop device". The worker is
        # already privileged with the host's PIDs, so this grants nothing new. Docker
        # then mounts no /dev/shm, /dev/pts, or /dev/mqueue of its own, so those are
        # the host's as well; the worker uses none of them.
        task["volumes"].append(bind("/dev", "/dev"))
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
    # How far the database has migrated, as one JSON line on stdout, for the agent
    # to trust over what it last recorded: a restore can lower it. It reads with
    # migrate's login and changes nothing, and with no logs, stdout holds only
    # that line. A release's image from before the command exits non-zero.
    services["migration-status"] = {
        **services["migrate"], "command": ["db", "migrate", "--status"],
        "environment": {**services["migrate"]["environment"], "HOT_LOG_TARGET": "none"},
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
        # The connection, password included, is read from a libpq service
        # file (see pg_service), never passed as an argument, which any
        # process on the host can read, or as an environment value.
        owner = {"entrypoint": ["psql", "--dbname=service=hot"],
                 "environment": {"PGSERVICEFILE": "/run/secrets/HOT_DB_MIGRATION_SERVICE"}}
        provision.update(owner, command=grant,
                         secrets=[secret("provision", k) for k in ("HOT_DB_MIGRATION_SERVICE", *db_ca)])
        if data_mode(config, "postgres") == "managed":
            # The stack's administrator login creates the owner and runtime
            # logins, the database, the schema, and its extensions; rerunning
            # it only resets the two passwords to the generated ones.
            services["prepare-postgres"] = {
                "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
                "entrypoint": ["psql", "--dbname=service=hot"],
                "environment": {"PGSERVICEFILE": "/run/secrets/POSTGRES_ADMIN_SERVICE"},
                "command": ["--set=ON_ERROR_STOP=1", f"--set=runtime_role={runtime_role(config)}",
                            "--file=/etc/hot/prepare_postgres.sql"],
                "secrets": [secret("prepare-postgres", k) for k in
                            ("POSTGRES_ADMIN_SERVICE", "POSTGRES_ADMIN_HOT_URI", "POSTGRES_PASSWORD",
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
            "environment": dict(owner["environment"]),
            "secrets": [secret("check-postgres", k) for k in ("HOT_DB_MIGRATION_SERVICE", *db_ca)],
            "tmpfs": ["/tmp:size=16777216,uid=10001,gid=10001"],
            "volumes": [{"type": "bind", "source": "./restore_postgres.sh", "target": "/etc/hot/restore_postgres.sh", "read_only": True},
                        bind(Path(data_root) / "moves", "/move", readonly=True)],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        }
        services["check-postgres"] = {
            "image": images["postgres"], "profiles": ["admin"], "user": "10001:10001", "read_only": True,
            "entrypoint": list(owner["entrypoint"]), "environment": dict(owner["environment"]),
            "command": ["--set=ON_ERROR_STOP=1", f"--set=runtime_role={runtime_role(config)}",
                        "--file=/etc/hot/check_postgres.sql"],
            "secrets": [secret("check-postgres", k) for k in ("HOT_DB_MIGRATION_SERVICE", *db_ca)],
            "volumes": [{"type": "bind", "source": "./check_postgres.sql", "target": "/etc/hot/check_postgres.sql", "read_only": True}],
            "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
        }
    services["provision"] = provision
    if not runs_migrations(config):
        for job in ("migrate", "migration-status", "provision", "bootstrap-admin", "check-postgres", "prepare-postgres",
                    "restore-postgres"):
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
    # Operator values such as settings reach Hot exactly as written.
    return literal(compose)


def nginx_config(config):
    servers = []
    # Private access on 8443 requires SNI naming an endpoint. A cloud load
    # balancer re-encrypts without SNI, so public endpoints get their own port,
    # routed by Host header, where anything else is refused.
    web = web_roles(config)
    listeners = [] if scalable(config) else [("8443", "$remote_addr", web, "")]
    behind_balancer = web if scalable(config) else tuple(r for r in public_endpoints(config) if r in web)
    if behind_balancer:
        # A scalable installation's passthrough load balancers keep the
        # client's address as the peer, so a forwarded header is the
        # client's own claim and is replaced. A single machine's port is
        # the load balancer's alone: a connection from the bundle's network
        # (user code calling the host's own address) is refused, since its
        # forwarded header would be believed.
        forwarded = "$remote_addr" if scalable(config) else "$proxy_add_x_forwarded_for"
        guard = "" if scalable(config) else f"\n  deny {RUNTIME_SUBNET};"
        listeners.append((str(LOAD_BALANCER_PORT), forwarded, behind_balancer, guard))
        health_role = "api" if "api" in web else "app"
        health = {"api": "http://api:4681/status", "app": "http://app:4680/signin"}[health_role]
        servers.append(f'''server {{
  listen {LOAD_BALANCER_PORT} ssl default_server;{guard}
  location = /hot-lb-health {{
    set $hot_health_upstream {health};
    proxy_pass $hot_health_upstream;
    proxy_set_header Host {config[health_role + '_hostname']};
  }}
  location / {{
    return 444;
  }}
}}''')
    for listen, forwarded, roles, guard in listeners:
        for role, port in (("app", 4680), ("api", 4681)):
            if role not in roles:
                continue
            servers.append(f'''server {{
  listen {listen} ssl;
  server_name {config[role + '_hostname']};{guard}
  location / {{
    set $hot_upstream http://{role}:{port};
    proxy_pass $hot_upstream;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
    proxy_set_header X-Forwarded-For {forwarded};
    proxy_set_header X-Request-ID $hot_request_id;
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
  # A name written directly in proxy_pass is looked up once, at startup, and a
  # crash restart of the app or API can move its address. Through a variable
  # Nginx asks Docker's DNS while serving, so it follows the move.
  resolver 127.0.0.11 valid=10s ipv6=off;
  # A caller's own request ID is kept when Hot would keep it (a short token
  # of letters, digits, -, _, and .); otherwise Nginx makes one.
  map $http_x_request_id $hot_request_id {
    "~^[A-Za-z0-9._-]{1,64}$" $http_x_request_id;
    default $request_id;
  }
  log_format operational escape=json '{"status":$status,"duration":$request_time,"request_id":"$hot_request_id","method":"$request_method"}';
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
# provisioning applies. It connects through the PGSERVICEFILE the bundle sets.
RESTORE_POSTGRES_SH = r'''set -eu
tables="$(psql --dbname=service=hot --no-psqlrc --tuples-only --no-align \
  --command="SELECT count(*) FROM pg_tables WHERE schemaname = 'hot'")"
if [ "$tables" != "0" ]; then
  echo "The database already has $tables of Hot's tables; refusing to restore over them" >&2
  exit 3
fi
pg_restore --list /move/postgres.dump | grep -v -E ' (SCHEMA|EXTENSION) - | COMMENT - EXTENSION ' > /tmp/restore.list
exec pg_restore --no-owner --no-privileges --exit-on-error --single-transaction \
  --use-list=/tmp/restore.list --dbname=service=hot /move/postgres.dump
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
    parser.add_argument("--signer-identity", default=RELEASE_SIGNER,
                        help="Certificate identity regex for this command only; hosts always check RELEASE_SIGNER")
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
