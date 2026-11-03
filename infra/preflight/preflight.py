#!/usr/bin/env python3
"""Check an installation's cloud prerequisites before provisioning.

    preflight.py installation.tfvars.json [--summary FILE]

Runs read-only `aws` or `gcloud` commands with the caller's identity, such as
the plan role, and never reads secret values. Checks that need the
installation's own network or host are listed as host checks, with what
verifies them later. Exits 1 when a required check fails.
"""
import argparse
import ipaddress
import json
from pathlib import Path
import re
import socket
import subprocess
import sys

# Fixed networks on every host: the bundle's container network and the box
# guests' network. An installation network overlapping them breaks routing.
RUNTIME_SUBNET = "172.30.0.0/24"
GUEST_SUBNET = "10.88.0.0/16"
# Kept in step with the contract module's sizes (a test checks this).
SIZES = {
    "small": {"aws": "m8i.large", "gcp": "n2-standard-2", "cpus": 2},
    "medium": {"aws": "m8i.xlarge", "gcp": "n2-standard-4", "cpus": 4},
    "large": {"aws": "m8i.2xlarge", "gcp": "n2-standard-8", "cpus": 8},
}
# Scalable roles other than task-workers run general-purpose machines.
ROLE_MACHINES = {"small": {"aws": "m7i.large", "gcp": "e2-standard-2"},
                 "medium": {"aws": "m7i.xlarge", "gcp": "e2-standard-4"},
                 "large": {"aws": "m7i.2xlarge", "gcp": "e2-standard-8"}}
# Scalable instance bounds when `scaling` leaves them out (the contract's defaults).
SCALING_MAX = {"app": 4, "api": 4, "worker": 2, "task_worker": 1}
# A scalable AWS installation's host roles, hot-NAME-ROLE (a test checks this).
SCALABLE_ROLES = ("app", "api", "worker", "task-worker", "scheduler")
# The images a mirror holds, one repository each under its path.
MIRROR_IMAGES = ("hot", "postgres", "valkey", "ingress")
DATABASES = {"small": {"aws": "db.m7g.large", "gcp": "db-custom-2-8192", "valkey": "cache.m7g.large",
                       "valkey_gcp": "STANDARD_SMALL"},
             "medium": {"aws": "db.m7g.xlarge", "gcp": "db-custom-4-16384", "valkey": "cache.m7g.xlarge",
                        "valkey_gcp": "HIGHMEM_MEDIUM"},
             "large": {"aws": "db.m7g.2xlarge", "gcp": "db-custom-8-32768", "valkey": "cache.m7g.2xlarge",
                       "valkey_gcp": "HIGHMEM_MEDIUM"}}
# What the contract module's data object takes (a test checks this).
DATA_KEYS = {"postgres": {"mode", "runtime_role", "version", "tier", "storage_gb", "high_availability",
                          "backup_retention_days", "deletion_protection", "admin_password_version", "endpoint"},
             "valkey": {"mode", "node_type", "high_availability", "auth_token_version", "endpoint"}}
# The domain of Hot Cloud's senders, which Hot refuses to send from over SMTP
# (HOT_CLOUD_SENDER_DOMAIN in crates/hot/src/email.rs).
HOT_CLOUD_SENDER_DOMAIN = "notifications.hot.dev"
# Memorystore for Valkey's node types.
MEMORYSTORE_NODE_TYPES = ("SHARED_CORE_NANO", "STANDARD_SMALL", "HIGHMEM_MEDIUM", "HIGHMEM_XLARGE")
# The APIs a stack enables for itself.
GCP_APIS = ["compute.googleapis.com", "secretmanager.googleapis.com", "logging.googleapis.com",
            "monitoring.googleapis.com", "pubsub.googleapis.com"]
# No stack enables these; the bootstrap does when it creates deploy identities.
# A stack's apply creates service accounts and project IAM through the first
# two, which Google normally checks against the caller's project, so an
# identity from another project can apply with them off here. GitHub jobs and
# impersonation sign in through the others, which a terminal using its own
# identity does not need.
GCP_APPLY_APIS = ["iam.googleapis.com", "cloudresourcemanager.googleapis.com"]
GCP_SIGN_IN_APIS = ["iamcredentials.googleapis.com", "sts.googleapis.com"]
# The AWS quota for running On-Demand standard (A, C, D, H, I, M, R, T, Z) vCPUs.
AWS_VCPU_QUOTA = "L-1216C47A"
# How a cloud CLI says this identity may not look, which says nothing about
# what is there. Anything else, such as a repository or bucket not found or a
# lapsed sign-in, still fails its check.
REFUSED = re.compile(r"\(AccessDenied(?:Exception)?\)|\(403\)|PERMISSION_DENIED|HTTPError 403|does not have permission")

PASS, FAIL, WARN, HOST = "pass", "fail", "warn", "host"


class Cli:
    """Runs a cloud CLI and returns its parsed JSON output. Errors carry only
    the CLI's last line, shortened."""

    def __call__(self, *args):
        result = subprocess.run(list(args), capture_output=True, text=True, check=False)
        if result.returncode != 0:
            lines = (result.stderr or result.stdout).strip().splitlines()
            raise CliError((lines[-1] if lines else f"{args[0]} exited {result.returncode}")[:200])
        return json.loads(result.stdout or "null")


class CliError(Exception):
    pass


class Report:
    def __init__(self):
        self.results = []

    def add(self, status, name, detail=""):
        self.results.append((status, name, detail))

    def check(self, name, function):
        """Run one check; a CLI error fails it with the CLI's message."""
        try:
            status, detail = function()
        except CliError as error:
            status, detail = FAIL, str(error)
        self.add(status, name, detail)

    @property
    def failed(self):
        return any(status == FAIL for status, _, _ in self.results)

    def markdown(self, title):
        marks = {PASS: "✅", FAIL: "❌", WARN: "⚠️", HOST: "🖥️"}
        lines = [f"### {title}", "", "| | Check | Detail |", "| --- | --- | --- |"]
        lines += [f"| {marks[s]} | {n} | {'<br>'.join(d.replace('|', '/').splitlines())} |" for s, n, d in self.results]
        lines += ["", "🖥️ marks checks that need the installation's network or host; the deploy agent runs them."]
        return "\n".join(lines) + "\n"


def refused(error):
    """Whether a CLI error says this identity may not look (see REFUSED)."""
    return REFUSED.search(str(error)) is not None


def overlaps(cidrs, label, clients=False):
    """Fail when an installation range overlaps a fixed host network. A
    client range wider than the fixed network only warns: just the clients
    inside it cannot reach the hosts."""
    bad, wide = [], []
    for cidr in cidrs:
        network = ipaddress.ip_network(cidr, strict=False)
        for fixed in map(ipaddress.ip_network, (RUNTIME_SUBNET, GUEST_SUBNET)):
            if network.overlaps(fixed):
                (wide if clients and not network.subnet_of(fixed) else bad).append(f"{cidr} overlaps {fixed}")
    if bad:
        return FAIL, f"{label}: " + "; ".join(bad)
    if wide:
        return WARN, f"{label}: " + "; ".join(wide) + "; clients inside those networks cannot reach the hosts"
    return PASS, f"{label}: no overlap with {RUNTIME_SUBNET} or {GUEST_SUBNET}"


def machines(installation, cloud):
    """{machine type: instances} at the installation's largest: the single
    machine, or every scalable role at its maximum plus the scheduler."""
    size = installation.get("size", "small")
    if installation.get("preset", "single-machine") != "scalable":
        return {SIZES[size][cloud]: 1}
    scaling = installation.get("scaling", {})
    top = {role: scaling.get(role, {}).get("max", default) for role, default in SCALING_MAX.items()}
    return {ROLE_MACHINES[size][cloud]: top["app"] + top["api"] + top["worker"] + 1, SIZES[size][cloud]: top["task_worker"]}


def provisioned(bucket, owner, name, marker):
    """A bucket to provision that exists already passes only when this
    installation's own apply created it."""
    if owner == name:
        return PASS, f"{bucket} was provisioned by this installation"
    return FAIL, (f"{bucket} already exists and is not {marker}{name}; set provision to false to use it as an "
                  "existing bucket, or choose another name")


def mirror(installation):
    """(registry host, path) of an image mirror, or None."""
    registry = installation.get("image_registry", "")
    if not registry:
        return None
    host, _, path = registry.partition("/")
    return host, path.strip("/")


def unknown_data_keys(data):
    """The data settings the contract does not know. The stacks take
    `installation` as type any, but the contract's typed object drops what it
    does not know before anything validates it, so neither OpenTofu nor the
    renderer ever sees the key. Only here, in the raw tfvars, is it still there."""
    unknown = [f"data.{key}" for key in data if key not in DATA_KEYS]
    for service, allowed in DATA_KEYS.items():
        options = data.get(service) or {}
        unknown += [f"data.{service}.{key}" for key in options if key not in allowed]
        unknown += [f"data.{service}.endpoint.{key}" for key in options.get("endpoint") or {} if key != "deny_cidrs"]
    return unknown


def sign_up_mail_problem(settings):
    """Why Hot would not count on email for open sign-up, as far as settings
    show it (delivery_status in crates/hot/src/email.rs, with
    connection_from_conf and check_sender in email_smtp.rs), or None. The agent
    refuses these names in the operator secret, and the bundle sets HOT_APP_URL.
    A render refuses what the rules for the worker cover (check_smtp), so these
    are the ones that deploy cleanly and leave the form to invited users."""
    def setting(key):
        return str(settings.get(key, "")).strip()

    def positive(value, top):
        # As Rust parses an unsigned integer, which may start with a plus sign.
        return re.fullmatch(r"\+?[0-9]+", value) is not None and 0 < int(value) <= top

    if setting("HOT_EMAIL_PROVIDER").lower() != "smtp":
        return "HOT_EMAIL_PROVIDER in settings is not smtp"
    if not setting("HOT_EMAIL_SMTP_HOST"):
        return "settings has no HOT_EMAIL_SMTP_HOST"
    if setting("HOT_EMAIL_SMTP_SECURITY").lower() not in ("", "starttls", "tls", "none"):
        return "HOT_EMAIL_SMTP_SECURITY in settings is not starttls, tls, or none"
    port = setting("HOT_EMAIL_SMTP_PORT")
    if port and not positive(port, 65535):
        return "HOT_EMAIL_SMTP_PORT in settings is not a valid port"
    timeout = setting("HOT_EMAIL_SMTP_TIMEOUT_SECONDS")
    if timeout and not positive(timeout, 2**64 - 1):
        return "HOT_EMAIL_SMTP_TIMEOUT_SECONDS in settings is not a positive integer"
    sender = setting("HOT_EMAIL_FROM")
    if not sender:
        return "settings has no HOT_EMAIL_FROM, which otherwise names Hot Cloud's sender, and Hot refuses that over SMTP"
    # Hot adds HOT_EMAIL_FROM_NAME around this, so a name here does not parse.
    if not re.fullmatch(r"[^\s<>@]+@[^\s<>@]+", sender):
        return "HOT_EMAIL_FROM in settings is not an email address alone (a display name belongs in HOT_EMAIL_FROM_NAME)"
    if sender.rpartition("@")[2].lower() == HOT_CLOUD_SENDER_DOMAIN:
        return f"HOT_EMAIL_FROM in settings is at {HOT_CLOUD_SENDER_DOMAIN}, which Hot refuses over SMTP"
    return None


def static_checks(report, installation):
    network = installation.get("network", {})
    if network.get("mode", "new") == "new":
        report.check("Network ranges", lambda: overlaps([network.get("cidr", "10.68.0.0/16")], "network.cidr"))
    if network.get("client_cidrs"):
        report.check("Client ranges", lambda: overlaps(network["client_cidrs"], "network.client_cidrs", clients=True))
    for role in ("app", "api"):
        host = installation.get(role + "_hostname", "")
        try:
            socket.getaddrinfo(host, 443)
            report.add(PASS, f"DNS for {role}", f"{host} resolves")
        except OSError:
            report.add(WARN, f"DNS for {role}", f"{host} does not resolve from here; private DNS may still serve it")
    data = installation.get("data", {})
    unknown = unknown_data_keys(data)
    if unknown:
        report.add(FAIL, "Data settings", f"{', '.join(unknown)}: the contract does not know these, and OpenTofu drops them "
                                          "with no error, so they would do nothing. A misspelled or misplaced deny list leaves "
                                          "its networks open to user code; it belongs at data.<service>.endpoint.deny_cidrs")
    elif data:
        report.add(PASS, "Data settings", "the contract knows every key")
    for service in ("postgres", "valkey"):
        if data.get(service, {}).get("mode") == "existing":
            report.add(HOST, f"Existing {service}", "TLS, version, extensions, and logins are checked from the host before migrating")
    # The bundle defaults to invitations, and only admin_email runs the job
    # that creates an administrator. A database with users needs neither. Open
    # sign-up's form verifies addresses by email, which takes SMTP, the only
    # provider Hot delivers through, with a relay and a sender of the
    # installation's own. Google and GitHub verify addresses themselves, but
    # their credentials are in the operator secret, which preflight never reads.
    settings = installation.get("settings", {})
    signup = settings.get("HOT_APP_SIGNUP", "invite").strip().lower()
    if not installation.get("admin_email"):
        problem = sign_up_mail_problem(settings)
        if signup != "open":
            report.add(WARN, "First administrator", "admin_email is empty and sign-up is by invitation, so no first account "
                                                    "can be created unless the database already has users; set admin_email and apply")
        elif problem:
            report.add(WARN, "First administrator", "admin_email is empty and sign-up is open, but open sign-up verifies "
                                                    f"addresses by email and {problem}, so "
                                                    "no first account can be created unless the database already has users "
                                                    "or Google or GitHub sign-in is configured in the operator secret, which "
                                                    "preflight does not read; set admin_email and apply")
    report.add(HOST, "Box execution", "Kata on nested virtualization is exercised by the host's first box task")
    if mirror(installation) is None:
        report.add(HOST, "Image pulls", "Hosts pull the release's pinned images from ghcr.io and docker.io over outbound HTTPS; the first deploy verifies it")


def aws_checks(report, document, cli, imports=""):
    installation = document["installation"]
    region, zone = installation["region"], installation["zone"]
    size = installation.get("size", "small")
    scalable = installation.get("preset", "single-machine") == "scalable"
    aws = lambda *args: cli("aws", *args, "--region", region, "--output", "json")  # noqa: E731

    def identity():
        account = aws("sts", "get-caller-identity")["Account"]
        return PASS, "account ****" + account[-4:]
    report.check("Cloud identity", identity)

    def availability_zone():
        zones = aws("ec2", "describe-availability-zones", "--zone-names", zone)["AvailabilityZones"]
        state = zones[0]["State"] if zones else "missing"
        return (PASS, f"{zone} is available") if state == "available" else (FAIL, f"{zone} is {state}")
    report.check("Zone", availability_zone)

    needed = machines(installation, "aws")

    def offering():
        missing = [t for t in needed if not aws(
            "ec2", "describe-instance-type-offerings", "--location-type", "availability-zone",
            "--filters", f"Name=location,Values={zone}", f"Name=instance-type,Values={t}")["InstanceTypeOfferings"]]
        if missing:
            return FAIL, f"{', '.join(missing)} not offered in {zone}; choose another zone"
        return PASS, f"{', '.join(needed)} offered in {zone}"
    report.check("Machine type", offering)

    def vcpu_quota():
        quota = aws("service-quotas", "get-service-quota", "--service-code", "ec2", "--quota-code", AWS_VCPU_QUOTA)
        # Every size's machines have the size's vCPUs, whichever family.
        limit, need = quota["Quota"]["Value"], SIZES[size]["cpus"] * sum(needed.values())
        if limit < need:
            return FAIL, f"the On-Demand standard vCPU quota is {limit:g}; {need} are needed"
        return PASS, f"quota {limit:g} vCPUs for {need} needed (current use not counted)"
    report.check("vCPU quota", vcpu_quota)

    def image():
        images = aws("ec2", "describe-images", "--image-ids", installation["host_image"])["Images"]
        if not images:
            return FAIL, f"{installation['host_image']} was not found"
        found = images[0]
        if found.get("State") != "available" or found.get("Architecture") != "x86_64":
            return FAIL, f"the host image is {found.get('State')} for {found.get('Architecture')}; it must be available for x86_64"
        return PASS, f"{installation['host_image']} is available"
    report.check("Host image", image)

    network = installation.get("network", {})
    if network.get("mode") == "existing":
        def vpc():
            vpcs = aws("ec2", "describe-vpcs", "--vpc-ids", network["network_id"])["Vpcs"]
            ranges = [a["CidrBlock"] for a in vpcs[0].get("CidrBlockAssociationSet", [])] or [vpcs[0]["CidrBlock"]]
            return overlaps(ranges, network["network_id"])
        report.check("Network ranges", vpc)

        def subnets():
            ids = list(dict.fromkeys([network["subnet_id"], *network.get("private_subnet_ids", []),
                                      *network.get("public_subnet_ids", []), *network.get("data_subnet_ids", [])]))
            found = {s["SubnetId"]: s for s in aws("ec2", "describe-subnets", "--subnet-ids", *ids)["Subnets"]}
            if found[network["subnet_id"]]["AvailabilityZone"] != zone:
                return FAIL, f"{network['subnet_id']} is not in {zone}"
            for key in ("private_subnet_ids", "public_subnet_ids", "data_subnet_ids"):
                chosen = network.get(key, [])
                if chosen and len({found[s]["AvailabilityZone"] for s in chosen}) < 2:
                    return FAIL, f"network.{key} must span two zones"
            if any(found[s]["VpcId"] != network["network_id"] for s in ids):
                return FAIL, "every subnet must be in network_id"
            return PASS, f"{len(ids)} subnets in {network['network_id']}"
        report.check("Subnets", subnets)

        host_subnets = network.get("private_subnet_ids", []) if scalable else [network["subnet_id"]]

        def egress():
            problems = []
            for subnet in host_subnets:
                tables = aws("ec2", "describe-route-tables", "--filters", f"Name=association.subnet-id,Values={subnet}")["RouteTables"]
                if not tables:  # the VPC's main route table applies
                    tables = aws("ec2", "describe-route-tables", "--filters", f"Name=vpc-id,Values={network['network_id']}",
                                 "Name=association.main,Values=true")["RouteTables"]
                routes = [r for t in tables for r in t.get("Routes", [])
                          if r.get("DestinationCidrBlock") == "0.0.0.0/0" and r.get("State", "active") == "active"]
                if not routes:
                    problems.append(f"{subnet} has no default route")
                elif all(r.get("GatewayId", "").startswith("igw-") for r in routes):
                    problems.append(f"{subnet} routes to an internet gateway, but hosts get no public address; use a NAT gateway")
            if problems:
                return FAIL, "; ".join(problems) + ". Hosts need outbound HTTPS for images, release manifests, and cloud APIs"
            return PASS, f"{len(host_subnets)} host subnet(s) route outbound traffic through a NAT or other gateway"
        report.check("Outbound routes", egress)

    # A scalable installation passes TLS through to the hosts' ingress, so
    # only a single machine's public endpoint uses an ACM certificate.
    public = [installation[r + "_hostname"] for r in ("app", "api") if installation.get(r + "_exposure") == "public"]
    if public and not scalable:
        def certificate():
            arn = document.get("public_certificate_arn", "")
            if not arn:
                return FAIL, "public endpoints need public_certificate_arn"
            found = aws("acm", "describe-certificate", "--certificate-arn", arn)["Certificate"]
            names = found.get("SubjectAlternativeNames", [])
            missing = [h for h in public if not any(n == h or (n.startswith("*.") and h.split(".", 1)[-1] == n[2:]) for n in names)]
            if found.get("Status") != "ISSUED":
                return FAIL, f"the certificate is {found.get('Status')}; validate it in DNS first"
            return (FAIL, "the certificate does not cover " + ", ".join(missing)) if missing else (PASS, "issued and covers the public hostnames")
        report.check("Public certificate", certificate)

    # A single machine's stack creates its release pointer, which operators
    # on v3.0.1 created themselves; a second create fails partway through an
    # apply. The stack cannot adopt it: an import block breaks fresh installs.
    if not scalable:
        def release_pointer():
            parameter = f"/hot/{installation['name']}/release"
            found = aws("ssm", "describe-parameters", "--parameter-filters", f"Key=Name,Values={parameter}")
            if not found.get("Parameters"):
                return PASS, f"{parameter} is free to create"
            tagging = aws("ssm", "list-tags-for-resource", "--resource-type", "Parameter", "--resource-id", parameter)
            tags = {t["Key"]: t["Value"] for t in tagging.get("TagList", [])}
            if tags.get("HotInstallation") == installation["name"]:
                return PASS, f"{parameter} was created by this installation"
            # Only the apply tags it, and a failed check stops the plan.
            if re.search(r"\bto\s*=\s*aws_ssm_parameter\.release\b", imports):
                return PASS, f"{parameter} exists, and imports.tf adopts it"
            return FAIL, (f"{parameter} already exists and is not tagged HotInstallation={installation['name']}, so the "
                          "apply would fail with ParameterAlreadyExists. To keep it and its value, add this to imports.tf "
                          "beside the tfvars, which the workflow copies into the stack:\n"
                          f'import {{\n  to = aws_ssm_parameter.release\n  id = "{parameter}"\n}}')
        report.check("Release pointer", release_pointer)

    host_roles = ", ".join(f"hot-{installation['name']}-{role}" for role in (SCALABLE_ROLES if scalable else ("host",)))

    def customer_key(bucket_name):
        """The customer-managed KMS key an S3 bucket encrypts new objects with
        by default, or None. It only adds a warning, so a read that fails
        changes nothing. The AWS-managed aws/s3 key lets its account's S3 users
        in by itself, and the stack's host policies grant no key."""
        try:
            found = aws("s3api", "get-bucket-encryption", "--bucket", bucket_name) or {}
        except CliError:
            return None
        for rule in found.get("ServerSideEncryptionConfiguration", {}).get("Rules", []):
            default = rule.get("ApplyServerSideEncryptionByDefault", {})
            key = default.get("KMSMasterKeyID", "")
            if default.get("SSEAlgorithm", "").startswith("aws:kms") and key and not key.endswith("alias/aws/s3"):
                return key
        return None

    for role, storage in installation.get("storage", {}).items():
        if storage.get("backend") == "s3" and storage.get("provision"):
            def free(storage=storage):
                try:
                    aws("s3api", "head-bucket", "--bucket", storage["bucket"])
                except CliError as error:
                    if "404" in str(error) or "Not Found" in str(error):
                        return PASS, f"{storage['bucket']} is free to create"
                    raise
                # It exists: this installation's own once an apply created it.
                try:
                    tagging = aws("s3api", "get-bucket-tagging", "--bucket", storage["bucket"])
                    tags = {t["Key"]: t["Value"] for t in tagging.get("TagSet", [])}
                except CliError as error:
                    if "NoSuchTagSet" not in str(error):
                        raise
                    tags = {}
                return provisioned(storage["bucket"], tags.get("HotInstallation"), installation["name"], "tagged HotInstallation=")
            report.check(f"Bucket for {role}", free)
        elif storage.get("backend") == "s3":
            def bucket(storage=storage):
                name = storage["bucket"]
                try:
                    aws("s3api", "head-bucket", "--bucket", name)
                except CliError as error:
                    if not refused(error):
                        raise
                    # A bucket in another account answers 403 unless its policy
                    # names this identity, and no plan reads it with this one.
                    return WARN, (f"this identity may not look at {name}, so preflight cannot check it. S3 answers the same "
                                  f"for another account's bucket and for a mistyped name, and nothing tries it before the first "
                                  f"build or file write: check the name and that its policy lets {host_roles} in ({error})")
                key = customer_key(name)
                if key:
                    return WARN, (f"{name} encrypts new objects with the KMS key {key}, and the stack grants the host roles S3 "
                                  f"actions only. The key's policy must let {host_roles} use it (kms:GenerateDataKey and "
                                  "kms:Decrypt), or the first build or file write fails with AccessDenied")
                return PASS, f"{name} is reachable"
            report.check(f"Bucket for {role}", bucket)

    found_mirror = mirror(installation)
    if found_mirror and found_mirror[0].endswith(".amazonaws.com"):
        host, path = found_mirror

        def repositories():
            account, mirror_region = host.split(".")[0], host.split(".")[3]
            # By name: ECR authorizes another account through each repository's
            # own policy, so a listing of that registry would be refused however
            # the repositories are set up. A name it lacks fails on its own.
            missing, unseen, reason = [], [], ""
            for image in MIRROR_IMAGES:
                name = f"{path}/{image}".lstrip("/")
                try:
                    cli("aws", "ecr", "describe-repositories", "--registry-id", account, "--repository-names", name,
                        "--region", mirror_region, "--output", "json")
                except CliError as error:
                    if "RepositoryNotFoundException" in str(error):
                        missing.append(name)
                    elif refused(error):
                        unseen.append(name)
                        reason = str(error)
                    else:
                        raise
            if missing:
                return FAIL, f"{host} has no repository {', '.join(missing)}; create them and copy the release's images"
            if unseen:
                return WARN, (f"this identity may not look at {', '.join(unseen)} in {host}, so preflight cannot check "
                              f"them; the first deploy's image pull will ({reason})")
            return PASS, f"{host} has a repository for each image"
        report.check("Image mirror", repositories)

    data = installation.get("data", {})
    if data.get("postgres", {}).get("mode") == "managed":
        postgres = data["postgres"]
        instance_class = postgres.get("tier") or DATABASES[size]["aws"]

        def rds():
            version = postgres.get("version", "18")
            # The contract takes a major version and RDS lists full ones, such
            # as 18.6. Its --engine-version filter may match exactly, so every
            # version on the instance class is compared here.
            options = aws("rds", "describe-orderable-db-instance-options", "--engine", "postgres",
                          "--db-instance-class", instance_class)
            offered = [o.get("EngineVersion", "") for o in options.get("OrderableDBInstanceOptions", [])]
            if not any(v == version or v.startswith(version + ".") for v in offered):
                return FAIL, f"RDS does not offer PostgreSQL {version} on {instance_class} in {region}"
            return PASS, f"PostgreSQL {version} on {instance_class}"
        report.check("Managed PostgreSQL", rds)
    if data.get("valkey", {}).get("mode") == "managed":
        node_type = data["valkey"].get("node_type") or DATABASES[size]["valkey"]

        def elasticache():
            versions = aws("elasticache", "describe-cache-engine-versions", "--engine", "valkey", "--engine-version", "8.1")
            if not versions.get("CacheEngineVersions"):
                return FAIL, f"ElastiCache does not offer Valkey 8.1 in {region}"
            # ElastiCache has no orderable-node listing; its reserved offerings
            # name each node type the region has.
            offerings = aws("elasticache", "describe-reserved-cache-nodes-offerings", "--cache-node-type", node_type,
                            "--max-items", "1")
            if not offerings.get("ReservedCacheNodesOfferings"):
                return WARN, f"ElastiCache lists no offering of {node_type} in {region}; check that the node type is available there"
            return PASS, f"Valkey 8.1 on {node_type}"
        report.check("Managed Valkey", elasticache)


def gcp_checks(report, document, cli):
    installation = document["installation"]
    project, region, zone = installation["project"], installation["region"], installation["zone"]
    size = installation.get("size", "small")
    gcloud = lambda *args: cli("gcloud", *args, "--project", project, "--format", "json")  # noqa: E731

    def identity():
        project_info = gcloud("projects", "describe", project)
        state = project_info.get("lifecycleState")
        return (PASS, f"project {project} is active") if state == "ACTIVE" else (FAIL, f"project {project} is {state}")
    report.check("Cloud identity", identity)

    data = installation.get("data", {})
    managed_postgres = data.get("postgres", {}).get("mode") == "managed"
    managed_valkey = data.get("valkey", {}).get("mode") == "managed"
    enabled = None

    def apis():
        nonlocal enabled
        enabled = {s["config"]["name"] for s in gcloud("services", "list", "--enabled")}
        needed = GCP_APIS + (["sqladmin.googleapis.com", "servicenetworking.googleapis.com"] if managed_postgres else [])
        needed += ["memorystore.googleapis.com", "networkconnectivity.googleapis.com",
                   "serviceconsumermanagement.googleapis.com"] if managed_valkey else []
        missing = [api for api in needed if api not in enabled]
        apply_off = [api for api in GCP_APPLY_APIS if api not in enabled]
        sign_in_off = [api for api in GCP_SIGN_IN_APIS if api not in enabled]
        how = "which no stack enables: run gcloud services enable, or the bootstrap with deploy identities"
        notes = []
        if apply_off:
            notes.append(f"an apply needs {', '.join(apply_off)} here or in its identity's project, {how}")
        if sign_in_off:
            notes.append(f"GitHub jobs and impersonation need {', '.join(sign_in_off)}, {how}")
        if missing:
            notes.append("the stack will enable " + ", ".join(missing) + "; the apply identity needs serviceusage permission")
        if notes:
            return WARN, "; ".join(notes)
        return PASS, "required APIs are enabled"
    report.check("APIs", apis)

    def using(api, name, function):
        """Run a check that calls `api` in the project. Until the stack
        enables the API, the call would fail, so the check waits for the
        next plan instead of stopping this one."""
        if enabled is not None and api not in enabled:
            report.add(WARN, name, f"not checked until {api} is enabled; the stack enables it, then a plan checks this")
        else:
            report.check(name, function)

    needed = machines(installation, "gcp")

    def machine():
        for machine_type in needed:
            gcloud("compute", "machine-types", "describe", machine_type, "--zone", zone)
        return PASS, f"{', '.join(needed)} offered in {zone}"
    using("compute.googleapis.com", "Machine type", machine)

    def nested_virtualization():
        policy = gcloud("resource-manager", "org-policies", "describe", "compute.disableNestedVirtualization", "--effective")
        if (policy or {}).get("booleanPolicy", {}).get("enforced"):
            return FAIL, "the organization policy compute.disableNestedVirtualization is enforced; box tasks need nested virtualization"
        return PASS, "nested virtualization is allowed"
    report.check("Nested virtualization", nested_virtualization)

    def cpu_quota():
        regional = {q["metric"]: q for q in gcloud("compute", "regions", "describe", region).get("quotas", [])}
        everywhere = {q["metric"]: q for q in gcloud("compute", "project-info", "describe").get("quotas", [])}
        cpus = SIZES[size]["cpus"]
        # E2 and N1 machines share the region's CPUS quota; other families
        # (N2) have their own, and every vCPU counts toward the project's
        # CPUS_ALL_REGIONS.
        needs = {}
        for machine_type, count in needed.items():
            family = machine_type.split("-")[0].upper()
            metric = "CPUS" if family in ("E2", "N1") else family + "_CPUS"
            needs[metric] = needs.get(metric, 0) + cpus * count
        total = cpus * sum(needed.values())
        quotas = [(regional.get(metric), metric, need) for metric, need in needs.items()]
        quotas.append((everywhere.get("CPUS_ALL_REGIONS"), "CPUS_ALL_REGIONS", total))
        quotas = [(quota, metric, need) for quota, metric, need in quotas if quota]
        # Like the AWS check, only the limit can fail: current use includes
        # this installation's own hosts once they run.
        short = [f"{metric} is {quota['limit']:g}; {need} are needed" for quota, metric, need in quotas if quota["limit"] < need]
        if short:
            return FAIL, "; ".join(short)
        tight = [f"{metric} has {quota['limit'] - quota['usage']:g} of {quota['limit']:g} free; {need} are needed"
                 for quota, metric, need in quotas if quota["limit"] - quota["usage"] < need]
        if tight:
            return WARN, "; ".join(tight) + " (current use includes this installation's hosts once they run)"
        return PASS, f"{total} vCPUs fit the regional and project quotas"
    using("compute.googleapis.com", "vCPU quota", cpu_quota)

    def image():
        parts = installation["host_image"].split("/")
        if len(parts) != 5 or parts[0] != "projects" or parts[2:4] != ["global", "images"]:
            return FAIL, "host_image must be projects/PROJECT/global/images/NAME"
        found = cli("gcloud", "compute", "images", "describe", parts[4], "--project", parts[1], "--format", "json")
        return (PASS, f"{parts[4]} is ready") if found.get("status") == "READY" else (FAIL, f"the host image is {found.get('status')}")
    image_project = installation["host_image"].split("/")[1:2]
    if image_project == [project]:
        using("compute.googleapis.com", "Host image", image)
    else:
        report.check("Host image", image)

    network = installation.get("network", {})
    if network.get("mode") == "existing":
        def subnet():
            name = network["subnet_id"].rsplit("/", 1)[-1]
            found = gcloud("compute", "networks", "subnets", "describe", name, "--region", region)
            ranges = [found["ipCidrRange"], *[r["ipCidrRange"] for r in found.get("secondaryIpRanges", [])]]
            return overlaps(ranges, name)
        using("compute.googleapis.com", "Network ranges", subnet)

        def egress():
            subnet_name = network["subnet_id"].rsplit("/", 1)[-1]
            network_name = network["network_id"].rsplit("/", 1)[-1]
            found = gcloud("compute", "networks", "subnets", "describe", subnet_name, "--region", region)
            needs = "hosts need outbound HTTPS for images, release manifests, and cloud APIs"
            unseen = f"a default route may be learned through a Cloud Router or peering, which preflight cannot check; {needs}"

            def may_learn_routes():
                # `routes list` shows static routes only, not ones from a
                # Cloud Router's BGP peers (in any region, since dynamic
                # routing can be global) or imported over peering.
                return (any(r.get("bgpPeers") and r.get("network", "").rsplit("/", 1)[-1] == network_name
                            for r in gcloud("compute", "routers", "list") or [])
                        or any(p.get("importCustomRoutes")
                               for p in (gcloud("compute", "networks", "describe", network_name) or {}).get("peerings", [])))
            # The hosts carry no network tags, so only untagged routes reach
            # them; the lowest priority number wins.
            routes = sorted((r for r in gcloud("compute", "routes", "list") or []
                             if r.get("destRange") == "0.0.0.0/0" and not r.get("tags")
                             and r.get("network", "").rsplit("/", 1)[-1] == network_name),
                            key=lambda r: int(r.get("priority", 1000)))
            if not routes:
                if may_learn_routes():
                    return WARN, f"{network_name} has no static default route; {unseen}"
                return FAIL, f"{network_name} has no default route (0.0.0.0/0); {needs}"
            if not routes[0].get("nextHopGateway", "").endswith("/default-internet-gateway"):
                hop = next((str(routes[0][k]).rsplit("/", 1)[-1] for k in ("nextHopInstance", "nextHopIlb", "nextHopIp",
                            "nextHopVpnTunnel", "nextHopHub", "nextHopPeering", "nextHopGateway") if routes[0].get(k)),
                           "another next hop")
                return WARN, f"outbound traffic leaves {network_name} through {hop}, which preflight cannot check; {needs}"

            # The internet gateway needs Cloud NAT, since hosts get no public
            # address, for the subnet's primary range at least.
            def covers(nat):
                if nat.get("sourceSubnetworkIpRangesToNat", "") in ("ALL_SUBNETWORKS_ALL_IP_RANGES",
                                                                    "ALL_SUBNETWORKS_ALL_PRIMARY_IP_RANGES"):
                    return True
                return any(s.get("name", "").rsplit("/", 1)[-1] == subnet_name
                           and {"ALL_IP_RANGES", "PRIMARY_IP_RANGE"} & set(s.get("sourceIpRangesToNat", ["ALL_IP_RANGES"]))
                           for s in nat.get("subnetworks", []))
            nat = any(covers(n) for r in gcloud("compute", "routers", "list", "--regions", region) or []
                      if r.get("network", "").rsplit("/", 1)[-1] == network_name for n in r.get("nats", []))
            if not nat:
                if may_learn_routes():
                    return WARN, f"no Cloud NAT in {region} covers {subnet_name}'s primary range, but {unseen}"
                return FAIL, f"no Cloud NAT in {region} covers {subnet_name}'s primary range; {needs}"
            if not found.get("privateIpGoogleAccess"):
                return WARN, f"{subnet_name} has Private Google Access off, so Google APIs go through Cloud NAT"
            return PASS, f"Cloud NAT and Private Google Access cover {subnet_name}"
        using("compute.googleapis.com", "Outbound routes", egress)
        if managed_postgres:
            def peering():
                name = network["network_id"].rsplit("/", 1)[-1]
                peerings = gcloud("services", "vpc-peerings", "list", "--network", name)
                if not any(p.get("service") == "services/servicenetworking.googleapis.com" for p in peerings or []):
                    return FAIL, f"{name} has no private services access, which Cloud SQL needs"
                # Cloud SQL's addresses come from a range peered into the
                # network. The stack knows only the range it reserves itself,
                # so on an existing network the operator names this one.
                if not (data["postgres"].get("endpoint") or {}).get("deny_cidrs"):
                    return WARN, ("private services access is configured, but data.postgres.endpoint.deny_cidrs is empty, so user code "
                                  "is denied only the address the database resolves to when a bundle renders; list the private services "
                                  "range there (find it with gcloud compute addresses list --global --filter=purpose=VPC_PEERING)")
                return PASS, "private services access is configured"
            report.check("Private services access", peering)
        if managed_valkey:
            def connection_policy():
                name = network["network_id"].rsplit("/", 1)[-1]
                policies = gcloud("network-connectivity", "service-connection-policies", "list", "--region", region)
                if any(p.get("serviceClass") == "gcp-memorystore" and p.get("network", "").rsplit("/", 1)[-1] == name
                       for p in policies or []):
                    return PASS, "a Memorystore service connection policy exists"
                return FAIL, (f"{name} has no service connection policy for gcp-memorystore in {region}, "
                              "which Memorystore's Private Service Connect endpoints need")
            report.check("Memorystore connection policy", connection_policy)

    found_mirror = mirror(installation)
    if found_mirror and found_mirror[0].endswith("-docker.pkg.dev"):
        host, path = found_mirror

        def repository():
            mirror_project, repository_name = path.split("/")[:2]
            try:
                found = cli("gcloud", "artifacts", "repositories", "describe", repository_name, "--location",
                            host[:-len("-docker.pkg.dev")], "--project", mirror_project, "--format", "json")
            except CliError as error:
                if not refused(error):
                    raise
                # A plan needs only getIamPolicy on a mirror in another project.
                return WARN, (f"this identity may not look at {repository_name} in {mirror_project}, so preflight cannot "
                              f"check it; the first deploy's image pull will ({error})")
            if found.get("format") != "DOCKER":
                return FAIL, f"{repository_name} is a {found.get('format')} repository; the mirror needs a Docker one"
            return PASS, f"{host}/{mirror_project}/{repository_name} is a Docker repository"
        report.check("Image mirror", repository)

    for role, storage in installation.get("storage", {}).items():
        if storage.get("backend") == "gcs" and storage.get("provision"):
            def free(storage=storage):
                try:
                    found = gcloud("storage", "buckets", "describe", "gs://" + storage["bucket"])
                except CliError as error:
                    if "404" in str(error) or "not found" in str(error).lower():
                        return PASS, f"{storage['bucket']} is free to create"
                    raise
                # It exists: this installation's own once an apply created it.
                owner = (found.get("labels") or {}).get("hot-installation")
                return provisioned(storage["bucket"], owner, installation["name"], "labeled hot-installation=")
            report.check(f"Bucket for {role}", free)
        elif storage.get("backend") == "gcs":
            def bucket(storage=storage):
                found = gcloud("storage", "buckets", "describe", "gs://" + storage["bucket"])
                # The stack that provisioned a bucket sets its whole IAM policy,
                # so that stack and this one would undo each other's every apply.
                owner = (found.get("labels") or {}).get("hot-installation")
                if owner and owner != installation["name"]:
                    return FAIL, (f"{storage['bucket']} is labeled hot-installation={owner}: that installation's stack sets the "
                                  "bucket's whole IAM policy, so its next apply would remove this installation's access, and "
                                  f"each apply here would add it back. Use a bucket of this installation's own; if {owner} no "
                                  f"longer manages it: gcloud storage buckets update gs://{storage['bucket']} "
                                  "--remove-labels=hot-installation")
                # The stacks' IAM binding on it carries a condition, which
                # Cloud Storage accepts only with uniform access.
                if found.get("uniform_bucket_level_access") is not True:
                    return FAIL, (f"{storage['bucket']} has fine-grained access, and the stack's conditional IAM binding "
                                  "needs uniform bucket-level access: gcloud storage buckets update "
                                  f"gs://{storage['bucket']} --uniform-bucket-level-access")
                return PASS, f"{storage['bucket']} is reachable"
            report.check(f"Bucket for {role}", bucket)

    if managed_postgres:
        postgres = data["postgres"]
        tier = postgres.get("tier") or DATABASES[size]["gcp"]
        custom = re.fullmatch(r"db-custom-(\d+)-(\d+)", tier)

        def cloud_sql():
            if custom:
                cpus, memory = int(custom[1]), int(custom[2])
                if ((cpus != 1 and (cpus % 2 or cpus > 96)) or memory % 256 or memory < 3840
                        or not 921.6 * cpus <= memory <= 6656 * cpus):
                    return FAIL, (f"{tier} is not a Cloud SQL machine: use 1 or an even number of vCPUs up to 96, and "
                                  "0.9 to 6.5 GB of memory per vCPU in multiples of 256 MB, at least 3.75 GB in all")
                return PASS, f"PostgreSQL {postgres.get('version', '18')} on {tier}"
            tiers = gcloud("sql", "tiers", "list")
            if not any(t.get("tier") == tier and region in t.get("region", []) for t in tiers or []):
                return FAIL, f"Cloud SQL does not offer {tier} in {region}"
            return PASS, f"PostgreSQL {postgres.get('version', '18')} on {tier}"
        if custom:
            report.check("Managed PostgreSQL", cloud_sql)
        else:
            using("sqladmin.googleapis.com", "Managed PostgreSQL", cloud_sql)
    if managed_valkey:
        node_type = data["valkey"].get("node_type") or DATABASES[size]["valkey_gcp"]

        def memorystore():
            if node_type not in MEMORYSTORE_NODE_TYPES:
                return WARN, f"{node_type} is not a Memorystore for Valkey node type preflight knows ({', '.join(MEMORYSTORE_NODE_TYPES)})"
            return PASS, f"Memorystore for Valkey on {node_type}"
        report.check("Managed Valkey", memorystore)


def run(document, cli=None, imports=""):
    """`imports` is the text of the installation's imports.tf, if it has one."""
    report = Report()
    installation = document["installation"]
    static_checks(report, installation)
    if installation["cloud"] == "aws":
        aws_checks(report, document, cli or Cli(), imports)
    else:
        gcp_checks(report, document, cli or Cli())
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tfvars")
    parser.add_argument("--summary", help="Append a Markdown report to this file, such as $GITHUB_STEP_SUMMARY")
    args = parser.parse_args(argv)
    with open(args.tfvars) as handle:
        document = json.load(handle)
    imports = Path(args.tfvars).with_name("imports.tf")
    report = run(document, imports=imports.read_text() if imports.exists() else "")
    for status, name, detail in report.results:
        print(f"{status.upper():5} {name}: {detail}")
    if args.summary:
        with open(args.summary, "a") as handle:
            handle.write(report.markdown(f"Preflight for {document['installation']['name']}"))
    return 1 if report.failed else 0


if __name__ == "__main__":
    sys.exit(main())
