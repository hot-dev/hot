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
DATABASES = {"small": {"aws": "db.m7g.large", "valkey": "cache.m7g.large"},
             "medium": {"aws": "db.m7g.xlarge", "valkey": "cache.m7g.xlarge"},
             "large": {"aws": "db.m7g.2xlarge", "valkey": "cache.m7g.2xlarge"}}
GCP_APIS = ["compute.googleapis.com", "secretmanager.googleapis.com", "logging.googleapis.com",
            "monitoring.googleapis.com", "pubsub.googleapis.com"]
# The AWS quota for running On-Demand standard (A, C, D, H, I, M, R, T, Z) vCPUs.
AWS_VCPU_QUOTA = "L-1216C47A"

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
        lines += [f"| {marks[s]} | {n} | {d.replace('|', '/')} |" for s, n, d in self.results]
        lines += ["", "🖥️ marks checks that need the installation's network or host; the deploy agent runs them."]
        return "\n".join(lines) + "\n"


def overlaps(cidrs, label):
    """Fail when an installation range overlaps a fixed host network."""
    bad = [f"{cidr} overlaps {fixed}" for cidr in cidrs for fixed in (RUNTIME_SUBNET, GUEST_SUBNET)
           if ipaddress.ip_network(cidr, strict=False).overlaps(ipaddress.ip_network(fixed))]
    return (FAIL, f"{label}: " + "; ".join(bad)) if bad else (PASS, f"{label}: no overlap with {RUNTIME_SUBNET} or {GUEST_SUBNET}")


def static_checks(report, installation):
    network = installation.get("network", {})
    if network.get("mode", "new") == "new":
        report.check("Network ranges", lambda: overlaps([network.get("cidr", "10.68.0.0/16")], "network.cidr"))
    if network.get("client_cidrs"):
        report.check("Client ranges", lambda: overlaps(network["client_cidrs"], "network.client_cidrs"))
    for role in ("app", "api"):
        host = installation.get(role + "_hostname", "")
        try:
            socket.getaddrinfo(host, 443)
            report.add(PASS, f"DNS for {role}", f"{host} resolves")
        except OSError:
            report.add(WARN, f"DNS for {role}", f"{host} does not resolve from here; private DNS may still serve it")
    data = installation.get("data", {})
    for service in ("postgres", "valkey"):
        if data.get(service, {}).get("mode") == "existing":
            report.add(HOST, f"Existing {service}", "TLS, version, extensions, and logins are checked from the host before migrating")
    report.add(HOST, "Box execution", "Kata on nested virtualization is exercised by the host's first box task")


def aws_checks(report, document, cli):
    installation = document["installation"]
    region, zone = installation["region"], installation["zone"]
    size = installation.get("size", "small")
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

    instance_type = SIZES[size]["aws"]

    def offering():
        offers = aws("ec2", "describe-instance-type-offerings", "--location-type", "availability-zone",
                     "--filters", f"Name=location,Values={zone}", f"Name=instance-type,Values={instance_type}")
        if offers["InstanceTypeOfferings"]:
            return PASS, f"{instance_type} is offered in {zone}"
        return FAIL, f"{instance_type} is not offered in {zone}; choose another zone"
    report.check("Machine type", offering)

    def vcpu_quota():
        quota = aws("service-quotas", "get-service-quota", "--service-code", "ec2", "--quota-code", AWS_VCPU_QUOTA)
        limit, need = quota["Quota"]["Value"], SIZES[size]["cpus"]
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
            ids = [network["subnet_id"], *network.get("public_subnet_ids", []), *network.get("data_subnet_ids", [])]
            found = {s["SubnetId"]: s for s in aws("ec2", "describe-subnets", "--subnet-ids", *ids)["Subnets"]}
            if found[network["subnet_id"]]["AvailabilityZone"] != zone:
                return FAIL, f"{network['subnet_id']} is not in {zone}"
            for key in ("public_subnet_ids", "data_subnet_ids"):
                chosen = network.get(key, [])
                if chosen and len({found[s]["AvailabilityZone"] for s in chosen}) < 2:
                    return FAIL, f"network.{key} must span two zones"
            if any(found[s]["VpcId"] != network["network_id"] for s in ids):
                return FAIL, "every subnet must be in network_id"
            return PASS, f"{len(ids)} subnets in {network['network_id']}"
        report.check("Subnets", subnets)

    public = [installation[r + "_hostname"] for r in ("app", "api") if installation.get(r + "_exposure") == "public"]
    if public:
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

    for role, storage in installation.get("storage", {}).items():
        if storage.get("backend") == "s3":
            def bucket(storage=storage):
                aws("s3api", "head-bucket", "--bucket", storage["bucket"])
                return PASS, f"{storage['bucket']} is reachable"
            report.check(f"Bucket for {role}", bucket)

    data = installation.get("data", {})
    if data.get("postgres", {}).get("mode") == "managed":
        postgres = data["postgres"]
        instance_class = postgres.get("tier") or DATABASES[size]["aws"]

        def rds():
            options = aws("rds", "describe-orderable-db-instance-options", "--engine", "postgres",
                          "--engine-version", postgres.get("version", "18"), "--db-instance-class", instance_class,
                          "--max-items", "1")
            if not options.get("OrderableDBInstanceOptions"):
                return FAIL, f"RDS does not offer PostgreSQL {postgres.get('version', '18')} on {instance_class} in {region}"
            return PASS, f"PostgreSQL {postgres.get('version', '18')} on {instance_class}"
        report.check("Managed PostgreSQL", rds)
    if data.get("valkey", {}).get("mode") == "managed":
        def elasticache():
            versions = aws("elasticache", "describe-cache-engine-versions", "--engine", "valkey", "--engine-version", "8.1")
            return (PASS, "Valkey 8.1") if versions.get("CacheEngineVersions") else (FAIL, f"ElastiCache does not offer Valkey 8.1 in {region}")
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

    def apis():
        enabled = {s["config"]["name"] for s in gcloud("services", "list", "--enabled")}
        needed = GCP_APIS + (["sqladmin.googleapis.com", "servicenetworking.googleapis.com"] if managed_postgres else [])
        missing = [api for api in needed if api not in enabled]
        if missing:
            return WARN, "the stack will enable " + ", ".join(missing) + "; the apply identity needs serviceusage permission"
        return PASS, "required APIs are enabled"
    report.check("APIs", apis)

    machine_type = SIZES[size]["gcp"]

    def machine():
        gcloud("compute", "machine-types", "describe", machine_type, "--zone", zone)
        return PASS, f"{machine_type} is offered in {zone}"
    report.check("Machine type", machine)

    def nested_virtualization():
        policy = gcloud("resource-manager", "org-policies", "describe", "compute.disableNestedVirtualization", "--effective")
        if (policy or {}).get("booleanPolicy", {}).get("enforced"):
            return FAIL, "the organization policy compute.disableNestedVirtualization is enforced; box tasks need nested virtualization"
        return PASS, "nested virtualization is allowed"
    report.check("Nested virtualization", nested_virtualization)

    def cpu_quota():
        quotas = {q["metric"]: q for q in gcloud("compute", "regions", "describe", region).get("quotas", [])}
        need = SIZES[size]["cpus"]
        for metric in ("CPUS", machine_type.split("-")[0].upper() + "_CPUS"):
            quota = quotas.get(metric)
            if quota and quota["limit"] - quota["usage"] < need:
                return FAIL, f"{metric} has {quota['limit'] - quota['usage']:g} of {quota['limit']:g} free; {need} are needed"
        return PASS, f"{need} vCPUs fit the regional quotas"
    report.check("vCPU quota", cpu_quota)

    def image():
        parts = installation["host_image"].split("/")
        if len(parts) != 5 or parts[0] != "projects" or parts[2:4] != ["global", "images"]:
            return FAIL, "host_image must be projects/PROJECT/global/images/NAME"
        found = cli("gcloud", "compute", "images", "describe", parts[4], "--project", parts[1], "--format", "json")
        return (PASS, f"{parts[4]} is ready") if found.get("status") == "READY" else (FAIL, f"the host image is {found.get('status')}")
    report.check("Host image", image)

    network = installation.get("network", {})
    if network.get("mode") == "existing":
        def subnet():
            name = network["subnet_id"].rsplit("/", 1)[-1]
            found = gcloud("compute", "networks", "subnets", "describe", name, "--region", region)
            ranges = [found["ipCidrRange"], *[r["ipCidrRange"] for r in found.get("secondaryIpRanges", [])]]
            return overlaps(ranges, name)
        report.check("Network ranges", subnet)
        if managed_postgres:
            def peering():
                name = network["network_id"].rsplit("/", 1)[-1]
                peerings = gcloud("services", "vpc-peerings", "list", "--network", name)
                if any(p.get("service") == "services/servicenetworking.googleapis.com" for p in peerings or []):
                    return PASS, "private services access is configured"
                return FAIL, f"{name} has no private services access, which Cloud SQL needs"
            report.check("Private services access", peering)

    for role, storage in installation.get("storage", {}).items():
        if storage.get("backend") == "gcs":
            def bucket(storage=storage):
                gcloud("storage", "buckets", "describe", "gs://" + storage["bucket"])
                return PASS, f"{storage['bucket']} is reachable"
            report.check(f"Bucket for {role}", bucket)
    if data.get("valkey", {}).get("mode") == "managed":
        report.add(FAIL, "Managed Valkey", "not available on GCP yet; use local or existing Valkey")


def run(document, cli=None):
    report = Report()
    installation = document["installation"]
    static_checks(report, installation)
    (aws_checks if installation["cloud"] == "aws" else gcp_checks)(report, document, cli or Cli())
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tfvars")
    parser.add_argument("--summary", help="Append a Markdown report to this file, such as $GITHUB_STEP_SUMMARY")
    args = parser.parse_args(argv)
    with open(args.tfvars) as handle:
        document = json.load(handle)
    report = run(document)
    for status, name, detail in report.results:
        print(f"{status.upper():5} {name}: {detail}")
    if args.summary:
        with open(args.summary, "a") as handle:
            handle.write(report.markdown(f"Preflight for {document['installation']['name']}"))
    return 1 if report.failed else 0


if __name__ == "__main__":
    sys.exit(main())
