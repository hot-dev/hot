"""Installation preflight against fake aws and gcloud CLIs."""
import copy
import importlib.util
from pathlib import Path
import re
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("preflight", ROOT / "preflight/preflight.py")
preflight = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preflight)


def aws_installation(**overrides):
    installation = {"name": "hot-example", "cloud": "aws", "region": "us-east-1", "zone": "us-east-1a",
                    "size": "small", "host_image": "ami-0123456789abcdef0",
                    "app_hostname": "app.hot.example", "api_hostname": "api.hot.example",
                    "storage": {"build": {"backend": "local"}, "file": {"backend": "local"}}}
    installation.update(overrides)
    return {"acknowledge_unqualified": True, "installation": installation}


def gcp_installation(**overrides):
    document = aws_installation(cloud="gcp", region="us-central1", zone="us-central1-a", project="example-project",
                                host_image="projects/example-images/global/images/hot-host-amd64")
    document["installation"].update(overrides)
    return document


AWS = {
    ("sts", "get-caller-identity"): {"Account": "123456789012"},
    ("ec2", "describe-availability-zones"): {"AvailabilityZones": [{"State": "available"}]},
    ("ec2", "describe-instance-type-offerings"): {"InstanceTypeOfferings": [{"InstanceType": "m8i.large"}]},
    ("service-quotas", "get-service-quota"): {"Quota": {"Value": 64.0}},
    ("ec2", "describe-images"): {"Images": [{"State": "available", "Architecture": "x86_64"}]},
    ("acm", "describe-certificate"): {"Certificate": {"Status": "ISSUED", "SubjectAlternativeNames": ["*.hot.example"]}},
    ("rds", "describe-orderable-db-instance-options"): {"OrderableDBInstanceOptions": [{}]},
    ("elasticache", "describe-cache-engine-versions"): {"CacheEngineVersions": [{}]},
    ("s3api", "head-bucket"): None,
    ("ec2", "describe-vpcs"): {"Vpcs": [{"CidrBlock": "10.40.0.0/16"}]},
    ("ec2", "describe-subnets"): {"Subnets": [
        {"SubnetId": "subnet-a", "AvailabilityZone": "us-east-1a", "VpcId": "vpc-0123"},
        {"SubnetId": "subnet-b", "AvailabilityZone": "us-east-1b", "VpcId": "vpc-0123"}]},
}
GCP = {
    ("projects", "describe"): {"lifecycleState": "ACTIVE"},
    ("services", "list"): [{"config": {"name": api}} for api in preflight.GCP_APIS],
    ("compute", "machine-types"): {"name": "n2-standard-2"},
    ("resource-manager", "org-policies"): {},
    ("compute", "regions"): {"quotas": [{"metric": "CPUS", "limit": 24.0, "usage": 4.0},
                                        {"metric": "N2_CPUS", "limit": 24.0, "usage": 4.0}]},
    ("compute", "images"): {"status": "READY"},
    ("compute", "networks"): {"ipCidrRange": "10.40.0.0/20"},
    ("services", "vpc-peerings"): [],
    ("storage", "buckets"): {"name": "acme-hot"},
}


class FakeCli:
    def __init__(self, responses):
        self.responses = copy.deepcopy(responses)
        self.calls = []

    def __call__(self, program, *args):
        self.calls.append((program, *args))
        key = tuple(args[:2])
        response = self.responses.get(key, KeyError)
        if isinstance(response, Exception):
            raise response
        if response is KeyError:
            raise AssertionError(f"unexpected command {program} {' '.join(args)}")
        return response


def statuses(report):
    return {name: status for status, name, _ in report.results}


def details(report):
    return {name: detail for _, name, detail in report.results}


@mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: [])
class Aws(unittest.TestCase):
    def test_a_ready_account_passes(self):
        cli = FakeCli(AWS)
        report = preflight.run(aws_installation(), cli)
        self.assertFalse(report.failed, report.results)
        self.assertEqual(details(report)["Cloud identity"], "account ****9012")
        self.assertTrue(all(call[0] == "aws" and "--region" in call for call in cli.calls))
        self.assertFalse(any("secretsmanager" in call or "get-secret-value" in call for call in cli.calls))

    def test_missing_capacity_fails_with_a_next_step(self):
        cli = FakeCli(AWS)
        cli.responses[("ec2", "describe-instance-type-offerings")] = {"InstanceTypeOfferings": []}
        cli.responses[("service-quotas", "get-service-quota")] = {"Quota": {"Value": 1.0}}
        report = preflight.run(aws_installation(), cli)
        self.assertEqual(statuses(report)["Machine type"], "fail")
        self.assertIn("choose another zone", details(report)["Machine type"])
        self.assertIn("2 are needed", details(report)["vCPU quota"])

    def test_cli_errors_fail_the_check_with_their_message(self):
        cli = FakeCli(AWS)
        cli.responses[("ec2", "describe-images")] = preflight.CliError("An error occurred (InvalidAMIID.NotFound)")
        report = preflight.run(aws_installation(), cli)
        self.assertEqual(statuses(report)["Host image"], "fail")
        self.assertIn("InvalidAMIID.NotFound", details(report)["Host image"])

    def test_public_endpoints_need_an_issued_covering_certificate(self):
        document = aws_installation(api_exposure="public")
        self.assertIn("public_certificate_arn", details(preflight.run(document, FakeCli(AWS)))["Public certificate"])
        document["public_certificate_arn"] = "arn:aws:acm:us-east-1:123456789012:certificate/0123"
        self.assertEqual(statuses(preflight.run(document, FakeCli(AWS)))["Public certificate"], "pass")
        cli = FakeCli(AWS)
        cli.responses[("acm", "describe-certificate")] = {"Certificate": {"Status": "ISSUED", "SubjectAlternativeNames": ["app.hot.example"]}}
        self.assertIn("does not cover api.hot.example", details(preflight.run(document, cli))["Public certificate"])

    def test_existing_networks_are_checked_for_zones_and_overlap(self):
        network = {"mode": "existing", "network_id": "vpc-0123", "subnet_id": "subnet-a", "data_subnet_ids": ["subnet-a"]}
        report = preflight.run(aws_installation(network=network), FakeCli(AWS))
        self.assertIn("two zones", details(report)["Subnets"])
        cli = FakeCli(AWS)
        cli.responses[("ec2", "describe-vpcs")] = {"Vpcs": [{"CidrBlock": "10.88.0.0/16"}]}
        network["data_subnet_ids"] = ["subnet-a", "subnet-b"]
        report = preflight.run(aws_installation(network=network), cli)
        self.assertEqual(statuses(report)["Subnets"], "pass")
        self.assertIn("overlaps 10.88.0.0/16", details(report)["Network ranges"])

    def test_managed_data_checks_regional_offerings(self):
        document = aws_installation(data={"postgres": {"mode": "managed"}, "valkey": {"mode": "managed"}})
        cli = FakeCli(AWS)
        cli.responses[("rds", "describe-orderable-db-instance-options")] = {"OrderableDBInstanceOptions": []}
        report = preflight.run(document, cli)
        self.assertIn("db.m7g.large", details(report)["Managed PostgreSQL"])
        self.assertEqual(statuses(report)["Managed PostgreSQL"], "fail")
        self.assertEqual(statuses(report)["Managed Valkey"], "pass")


@mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: [])
class Gcp(unittest.TestCase):
    def test_a_ready_project_passes(self):
        cli = FakeCli(GCP)
        report = preflight.run(gcp_installation(), cli)
        self.assertFalse(report.failed, report.results)
        image = next(call for call in cli.calls if call[1:3] == ("compute", "images"))
        self.assertIn("example-images", image)

    def test_policies_and_missing_apis(self):
        cli = FakeCli(GCP)
        cli.responses[("resource-manager", "org-policies")] = {"booleanPolicy": {"enforced": True}}
        cli.responses[("services", "list")] = [{"config": {"name": "compute.googleapis.com"}}]
        report = preflight.run(gcp_installation(), cli)
        self.assertEqual(statuses(report)["Nested virtualization"], "fail")
        self.assertEqual(statuses(report)["APIs"], "warn")
        self.assertIn("secretmanager.googleapis.com", details(report)["APIs"])

    def test_managed_postgres_on_an_existing_network_needs_private_services_access(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        report = preflight.run(gcp_installation(network=network, data={"postgres": {"mode": "managed"}}), FakeCli(GCP))
        self.assertEqual(statuses(report)["Private services access"], "fail")
        cli = FakeCli(GCP)
        cli.responses[("services", "vpc-peerings")] = [{"service": "services/servicenetworking.googleapis.com"}]
        report = preflight.run(gcp_installation(network=network, data={"postgres": {"mode": "managed"}}), cli)
        self.assertEqual(statuses(report)["Private services access"], "pass")

    def test_regional_quota_counts_current_use(self):
        cli = FakeCli(GCP)
        cli.responses[("compute", "regions")] = {"quotas": [{"metric": "N2_CPUS", "limit": 8.0, "usage": 7.0}]}
        report = preflight.run(gcp_installation(), cli)
        self.assertIn("N2_CPUS has 1 of 8 free", details(report)["vCPU quota"])

    def test_managed_valkey_is_refused(self):
        report = preflight.run(gcp_installation(data={"valkey": {"mode": "managed"}}), FakeCli(GCP))
        self.assertEqual(statuses(report)["Managed Valkey"], "fail")


class Static(unittest.TestCase):
    def test_network_ranges_must_avoid_the_host_networks(self):
        report = preflight.Report()
        with mock.patch.object(preflight.socket, "getaddrinfo", side_effect=OSError):
            preflight.static_checks(report, aws_installation(network={"cidr": "172.30.0.0/16"})["installation"])
        self.assertEqual(statuses(report)["Network ranges"], "fail")
        self.assertEqual(statuses(report)["DNS for app"], "warn")

    def test_existing_data_services_are_left_to_the_host(self):
        report = preflight.Report()
        with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
            preflight.static_checks(report, aws_installation(data={"postgres": {"mode": "existing"}})["installation"])
        self.assertEqual(statuses(report)["Existing postgres"], "host")

    def test_sizes_match_the_contract(self):
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        for size, expected in preflight.SIZES.items():
            line = re.search(rf"^\s+{size}\s+=\s+\{{ aws = \"([^\"]+)\", gcp = \"([^\"]+)\".*cpus = (\d+)", contract, re.M)
            self.assertEqual(line.groups(), (expected["aws"], expected["gcp"], str(expected["cpus"])), size)
            database = re.search(rf"^\s+{size}\s+=\s+\{{ aws = \"(db\.[^\"]+)\".*valkey = \"([^\"]+)\"", contract, re.M)
            self.assertEqual(database.groups(), (preflight.DATABASES[size]["aws"], preflight.DATABASES[size]["valkey"]), size)

    def test_the_markdown_report_marks_each_result(self):
        report = preflight.Report()
        report.add("pass", "Zone", "us-east-1a is available")
        report.add("fail", "Machine type", "a|b")
        text = report.markdown("Preflight for hot-example")
        self.assertIn("| ✅ | Zone | us-east-1a is available |", text)
        self.assertIn("| ❌ | Machine type | a/b |", text)


if __name__ == "__main__":
    unittest.main()
