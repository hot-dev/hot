"""Installation preflight against fake aws and gcloud CLIs."""
import copy
import importlib.util
import io
import json
from pathlib import Path
import re
import tempfile
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


def ecr_holding(*names):
    """ECR's answer to a repository's name, for a registry holding these. A
    listing is refused, as it is for an identity in another account."""
    def answer(args):
        if "--repository-names" not in args:
            raise preflight.CliError("An error occurred (AccessDeniedException) when calling the DescribeRepositories operation")
        name = args[args.index("--repository-names") + 1]
        if name not in names:
            raise preflight.CliError(f"An error occurred (RepositoryNotFoundException) when calling the DescribeRepositories "
                                     f"operation: The repository with name '{name}' does not exist")
        return {"repositories": [{"repositoryName": name}]}
    return answer


AWS = {
    ("sts", "get-caller-identity"): {"Account": "123456789012"},
    ("ec2", "describe-availability-zones"): {"AvailabilityZones": [{"State": "available"}]},
    ("ec2", "describe-instance-type-offerings"): {"InstanceTypeOfferings": [{"InstanceType": "m8i.large"}]},
    ("service-quotas", "get-service-quota"): {"Quota": {"Value": 64.0}},
    ("ec2", "describe-images"): {"Images": [{"State": "available", "Architecture": "x86_64"}]},
    ("acm", "describe-certificate"): {"Certificate": {"Status": "ISSUED", "SubjectAlternativeNames": ["*.hot.example"]}},
    ("rds", "describe-orderable-db-instance-options"): {"OrderableDBInstanceOptions": [{"EngineVersion": "17.6"}, {"EngineVersion": "18.6"}]},
    ("elasticache", "describe-cache-engine-versions"): {"CacheEngineVersions": [{}]},
    ("elasticache", "describe-reserved-cache-nodes-offerings"): {"ReservedCacheNodesOfferings": [{}]},
    ("s3api", "head-bucket"): None,
    ("s3api", "get-bucket-tagging"): {"TagSet": [{"Key": "HotInstallation", "Value": "hot-example"}]},
    ("s3api", "get-bucket-encryption"): {"ServerSideEncryptionConfiguration": {"Rules": [
        {"ApplyServerSideEncryptionByDefault": {"SSEAlgorithm": "AES256"}}]}},
    ("ec2", "describe-vpcs"): {"Vpcs": [{"CidrBlock": "10.40.0.0/16"}]},
    ("ec2", "describe-subnets"): {"Subnets": [
        {"SubnetId": "subnet-a", "AvailabilityZone": "us-east-1a", "VpcId": "vpc-0123"},
        {"SubnetId": "subnet-b", "AvailabilityZone": "us-east-1b", "VpcId": "vpc-0123"}]},
    ("ec2", "describe-route-tables"): {"RouteTables": [{"Routes": [
        {"DestinationCidrBlock": "10.40.0.0/16", "GatewayId": "local", "State": "active"},
        {"DestinationCidrBlock": "0.0.0.0/0", "NatGatewayId": "nat-0123", "State": "active"}]}]},
    ("ecr", "describe-repositories"): ecr_holding(*[f"hot-mirror/{image}" for image in preflight.MIRROR_IMAGES]),
    ("ssm", "describe-parameters"): {"Parameters": []},
    ("ssm", "list-tags-for-resource"): {"TagList": [{"Key": "HotInstallation", "Value": "hot-example"}]},
}
# What a bootstrapped project on which the stack has been applied has enabled.
ENABLED = preflight.GCP_APIS + preflight.GCP_APPLY_APIS + preflight.GCP_SIGN_IN_APIS
GCP = {
    ("projects", "describe"): {"lifecycleState": "ACTIVE"},
    ("services", "list"): [{"config": {"name": api}} for api in ENABLED],
    ("compute", "machine-types"): {"name": "n2-standard-2"},
    ("resource-manager", "org-policies"): {},
    ("compute", "regions"): {"quotas": [{"metric": "CPUS", "limit": 24.0, "usage": 4.0},
                                        {"metric": "N2_CPUS", "limit": 24.0, "usage": 4.0}]},
    ("compute", "project-info"): {"quotas": [{"metric": "CPUS_ALL_REGIONS", "limit": 32.0, "usage": 8.0}]},
    ("compute", "images"): {"status": "READY"},
    ("compute", "networks"): {"ipCidrRange": "10.40.0.0/20", "privateIpGoogleAccess": True},
    ("compute", "routers"): [{"network": "projects/example-project/global/networks/shared",
                              "nats": [{"sourceSubnetworkIpRangesToNat": "ALL_SUBNETWORKS_ALL_IP_RANGES"}]}],
    ("compute", "routes"): [{"network": "https://www.googleapis.com/compute/v1/projects/example-project/global/networks/shared",
                             "destRange": "0.0.0.0/0", "priority": 1000,
                             "nextHopGateway": "https://www.googleapis.com/compute/v1/projects/example-project/global/gateways/default-internet-gateway"}],
    ("sql", "tiers"): [{"tier": "db-n1-standard-2", "region": ["us-central1", "europe-west1"]}],
    ("artifacts", "repositories"): {"format": "DOCKER"},
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
        return response(args) if callable(response) else response


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

    def test_scalable_public_endpoints_need_no_certificate(self):
        # TLS passes through the load balancers to the hosts' ingress.
        document = aws_installation(preset="scalable", api_exposure="public")
        report = preflight.run(document, FakeCli(AWS))
        self.assertNotIn("Public certificate", statuses(report))

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

    def test_hosts_need_an_outbound_route_through_a_nat(self):
        network = {"mode": "existing", "network_id": "vpc-0123", "subnet_id": "subnet-a"}
        self.assertEqual(statuses(preflight.run(aws_installation(network=network), FakeCli(AWS)))["Outbound routes"], "pass")
        cli = FakeCli(AWS)
        cli.responses[("ec2", "describe-route-tables")] = {"RouteTables": [{"Routes": [
            {"DestinationCidrBlock": "0.0.0.0/0", "GatewayId": "igw-0123", "State": "active"}]}]}
        self.assertIn("use a NAT gateway", details(preflight.run(aws_installation(network=network), cli))["Outbound routes"])
        cli.responses[("ec2", "describe-route-tables")] = {"RouteTables": []}
        report = preflight.run(aws_installation(network=network), cli)
        self.assertIn("subnet-a has no default route", details(report)["Outbound routes"])
        main = [c for c in cli.calls if "Name=association.main,Values=true" in c]
        self.assertTrue(main, "a subnet without its own table uses the VPC's main table")

    def test_scalable_installations_count_every_role_at_its_maximum(self):
        network = {"mode": "existing", "network_id": "vpc-0123", "subnet_id": "subnet-a",
                   "private_subnet_ids": ["subnet-a"], "data_subnet_ids": ["subnet-a", "subnet-b"]}
        document = aws_installation(preset="scalable", network=network, scaling={"worker": {"min": 2, "max": 6}})
        cli = FakeCli(AWS)
        cli.responses[("service-quotas", "get-service-quota")] = {"Quota": {"Value": 16.0}}
        report = preflight.run(document, cli)
        self.assertIn("32 are needed", details(report)["vCPU quota"], "(4 + 4 + 6 + 1) m7i and 1 m8i, 2 vCPUs each")
        offered = [c for c in cli.calls if "describe-instance-type-offerings" in c]
        self.assertEqual({a.split("=")[-1] for c in offered for a in c if a.startswith("Name=instance-type")},
                         {"m7i.large", "m8i.large"})
        self.assertIn("private_subnet_ids must span two zones", details(report)["Subnets"])
        routes = [c for c in cli.calls if "describe-route-tables" in c]
        self.assertEqual(len(routes), 1, "the hosts' private subnets, not subnet_id alone")

    def test_an_ecr_mirror_needs_a_repository_per_image(self):
        document = aws_installation(image_registry="123456789012.dkr.ecr.us-west-2.amazonaws.com/hot-mirror")
        cli = FakeCli(AWS)
        report = preflight.run(document, cli)
        self.assertEqual(statuses(report)["Image mirror"], "pass", "by name, which another account's registry can answer")
        listed = [c for c in cli.calls if "describe-repositories" in c]
        self.assertEqual([c[c.index("--repository-names") + 1] for c in listed], [f"hot-mirror/{i}" for i in preflight.MIRROR_IMAGES])
        self.assertEqual(listed[0][listed[0].index("--region") + 1], "us-west-2", "the mirror's own region")
        self.assertEqual(listed[0][listed[0].index("--registry-id") + 1], "123456789012")
        self.assertNotIn("Image pulls", statuses(report))
        cli.responses[("ecr", "describe-repositories")] = ecr_holding("hot-mirror/hot")
        report = preflight.run(document, cli)
        self.assertEqual(statuses(report)["Image mirror"], "fail")
        self.assertIn("no repository hot-mirror/postgres, hot-mirror/valkey, hot-mirror/ingress", details(report)["Image mirror"])

    def test_an_ecr_mirror_this_identity_may_not_look_at_only_warns(self):
        # A central account's repositories let hosts pull; their policies need
        # not name the plan role, and the first deploy pulls them anyway.
        document = aws_installation(image_registry="210987654321.dkr.ecr.us-west-2.amazonaws.com/hot-mirror")
        denied = preflight.CliError("An error occurred (AccessDeniedException) when calling the DescribeRepositories operation: "
                                    "User: arn:aws:sts::123456789012:assumed-role/plan is not authorized to perform: ecr:DescribeRepositories")
        cli = FakeCli(AWS)
        cli.responses[("ecr", "describe-repositories")] = denied
        report = preflight.run(document, cli)
        self.assertFalse(report.failed, report.results)
        self.assertEqual(statuses(report)["Image mirror"], "warn")
        self.assertIn("may not look at hot-mirror/hot, hot-mirror/postgres, hot-mirror/valkey, hot-mirror/ingress", details(report)["Image mirror"])
        self.assertIn("the first deploy's image pull will (An error occurred (AccessDeniedException)", details(report)["Image mirror"])

        # Some repositories let it look: a confirmed miss still fails.
        def only_one_answers(args):
            name = args[args.index("--repository-names") + 1]
            if name == "hot-mirror/hot":
                return {"repositories": [{"repositoryName": name}]}
            if name == "hot-mirror/valkey":
                return ecr_holding()(args)
            raise denied
        cli.responses[("ecr", "describe-repositories")] = only_one_answers
        report = preflight.run(document, cli)
        self.assertEqual(statuses(report)["Image mirror"], "fail")
        self.assertIn("no repository hot-mirror/valkey;", details(report)["Image mirror"])
        # Only a refusal is let through.
        for other in ("An error occurred (ThrottlingException) when calling the DescribeRepositories operation",
                      "An error occurred (ExpiredTokenException) when calling the DescribeRepositories operation",
                      "Unable to locate credentials. You can configure credentials by running \"aws login\"."):
            with self.subTest(other):
                cli.responses[("ecr", "describe-repositories")] = preflight.CliError(other)
                self.assertEqual(statuses(preflight.run(document, cli))["Image mirror"], "fail")

    def test_an_existing_bucket_this_identity_may_not_look_at_only_warns(self):
        # head-bucket answers 403 outside the bucket's policy, and no plan reads
        # the bucket with this identity, so that alone must not stop one.
        storage = {"build": {"backend": "s3", "bucket": "central-hot"}, "file": {"backend": "local"}}
        cli = FakeCli(AWS)
        cli.responses[("s3api", "head-bucket")] = preflight.CliError("An error occurred (403) when calling the HeadBucket operation: Forbidden")
        report = preflight.run(aws_installation(storage=storage), cli)
        self.assertFalse(report.failed, report.results)
        self.assertEqual(statuses(report)["Bucket for build"], "warn")
        detail = details(report)["Bucket for build"]
        self.assertIn("may not look at central-hot", detail)
        self.assertIn("a mistyped name", detail)
        self.assertIn("nothing tries it before the first build or file write", detail, "no deploy reads a bucket")
        self.assertIn("lets hot-hot-example-host in", detail)
        self.assertFalse([c for c in cli.calls if "get-bucket-encryption" in c])
        report = preflight.run(aws_installation(storage=storage, preset="scalable"), cli)
        self.assertIn("hot-hot-example-task-worker", details(report)["Bucket for build"])
        # A bucket that is not there is no refusal.
        cli.responses[("s3api", "head-bucket")] = preflight.CliError("An error occurred (404) when calling the HeadBucket operation: Not Found")
        report = preflight.run(aws_installation(storage=storage), cli)
        self.assertEqual(statuses(report)["Bucket for build"], "fail")
        self.assertIn("Not Found", details(report)["Bucket for build"])

    def test_an_existing_bucket_on_a_customer_key_warns_about_the_host_roles(self):
        # The stacks grant the hosts S3 actions only, so KMS would refuse every
        # write long after the deploy reported success.
        storage = {"build": {"backend": "s3", "bucket": "acme-hot"}, "file": {"backend": "local"}}

        def bucket(encryption, **overrides):
            cli = FakeCli(AWS)
            cli.responses[("s3api", "get-bucket-encryption")] = encryption
            report = preflight.run(aws_installation(storage=storage, **overrides), cli)
            self.assertFalse(report.failed, report.results)
            return statuses(report)["Bucket for build"], details(report)["Bucket for build"]

        def encrypted(algorithm, key=None):
            default = {"SSEAlgorithm": algorithm, **({"KMSMasterKeyID": key} if key else {})}
            return {"ServerSideEncryptionConfiguration": {"Rules": [{"ApplyServerSideEncryptionByDefault": default, "BucketKeyEnabled": True}]}}
        arn = "arn:aws:kms:us-east-1:123456789012:key/0123"
        for algorithm, key in (("aws:kms", arn), ("aws:kms", "alias/hot"), ("aws:kms:dsse", arn)):
            with self.subTest(algorithm=algorithm, key=key):
                status, detail = bucket(encrypted(algorithm, key))
                self.assertEqual(status, "warn")
                self.assertIn(f"encrypts new objects with the KMS key {key}", detail)
                self.assertIn("must let hot-hot-example-host use it (kms:GenerateDataKey and kms:Decrypt)", detail)
        status, detail = bucket(encrypted("aws:kms", arn), preset="scalable")
        self.assertIn("must let " + ", ".join(f"hot-hot-example-{r}" for r in preflight.SCALABLE_ROLES) + " use it", detail)
        # S3's own keys let the hosts in, and nothing read says nothing.
        for fine in (encrypted("AES256"), encrypted("aws:kms"), encrypted("aws:kms", "alias/aws/s3"),
                     encrypted("aws:kms", "arn:aws:kms:us-east-1:123456789012:alias/aws/s3"), {}, None,
                     preflight.CliError("An error occurred (AccessDenied) when calling the GetBucketEncryption operation: Access Denied"),
                     preflight.CliError("An error occurred (ServerSideEncryptionConfigurationNotFoundError) when calling the GetBucketEncryption operation")):
            with self.subTest(fine):
                self.assertEqual(bucket(fine), ("pass", "acme-hot is reachable"))

    def test_a_bucket_to_provision_is_not_asked_about_its_key(self):
        storage = {"build": {"backend": "s3", "bucket": "acme-hot", "provision": True}, "file": {"backend": "local"}}
        cli = FakeCli(AWS)
        preflight.run(aws_installation(storage=storage), cli)
        self.assertFalse([c for c in cli.calls if "get-bucket-encryption" in c], "the stack sets AES256 on the ones it creates")

    def test_the_host_roles_named_are_the_stacks(self):
        stacks = ROOT / "opentofu/stacks/aws"
        self.assertIn('name = "hot-${local.config.name}-host"', (stacks / "single-machine/main.tf").read_text())
        self.assertRegex((stacks / "scalable/main.tf").read_text(),
                         r'resource "aws_iam_role" "role" \{\n  for_each = local\.roles\n  name     = "hot-\$\{local\.config\.name\}-\$\{each\.key\}"')
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        groups = re.search(r"^    groups = \{\n(.*?)\n    \}", contract, re.M | re.S).group(1)
        self.assertEqual(set(re.findall(r"^\s+([\w-]+)\s+= ", groups, re.M)), set(preflight.SCALABLE_ROLES))

    def test_provisioned_buckets_are_free_or_this_installations_own(self):
        storage = {"build": {"backend": "s3", "bucket": "acme-hot-builds", "region": "us-east-1", "provision": True},
                   "file": {"backend": "local"}}
        cli = FakeCli(AWS)
        cli.responses[("s3api", "head-bucket")] = preflight.CliError("An error occurred (404) when calling the HeadBucket operation: Not Found")
        self.assertEqual(statuses(preflight.run(aws_installation(storage=storage), cli))["Bucket for build"], "pass")
        # After the first apply created it, every later plan still passes.
        report = preflight.run(aws_installation(storage=storage), FakeCli(AWS))
        self.assertEqual(statuses(report)["Bucket for build"], "pass")
        self.assertIn("provisioned by this installation", details(report)["Bucket for build"])
        cli = FakeCli(AWS)
        cli.responses[("s3api", "get-bucket-tagging")] = {"TagSet": [{"Key": "HotInstallation", "Value": "other"}]}
        self.assertIn("already exists and is not tagged HotInstallation=hot-example",
                      details(preflight.run(aws_installation(storage=storage), cli))["Bucket for build"])
        cli.responses[("s3api", "get-bucket-tagging")] = preflight.CliError("An error occurred (NoSuchTagSet) when calling the GetBucketTagging operation")
        self.assertEqual(statuses(preflight.run(aws_installation(storage=storage), cli))["Bucket for build"], "fail")
        cli.responses[("s3api", "head-bucket")] = preflight.CliError("An error occurred (403) when calling the HeadBucket operation: Forbidden")
        self.assertEqual(statuses(preflight.run(aws_installation(storage=storage), cli))["Bucket for build"], "fail")

    def test_the_release_pointer_is_free_or_this_installations_own(self):
        cli = FakeCli(AWS)
        report = preflight.run(aws_installation(), cli)
        self.assertEqual(details(report)["Release pointer"], "/hot/hot-example/release is free to create")
        listed = next(c for c in cli.calls if "describe-parameters" in c)
        self.assertIn("Key=Name,Values=/hot/hot-example/release", listed)
        self.assertFalse([c for c in cli.calls if "list-tags-for-resource" in c])
        # After the first apply created it, every later plan still passes.
        cli = FakeCli(AWS)
        cli.responses[("ssm", "describe-parameters")] = {"Parameters": [{"Name": "/hot/hot-example/release"}]}
        report = preflight.run(aws_installation(), cli)
        self.assertEqual(statuses(report)["Release pointer"], "pass")
        self.assertIn("created by this installation", details(report)["Release pointer"])
        tags = next(c for c in cli.calls if "list-tags-for-resource" in c)
        self.assertEqual(tags[tags.index("--resource-id") + 1], "/hot/hot-example/release")
        self.assertFalse([c for c in cli.calls if "get-parameter" in c], "tags and names only")

    def test_a_release_pointer_from_v3_0_1_fails_until_imports_tf_adopts_it(self):
        def pointer(tags, imports=""):
            cli = FakeCli(AWS)
            cli.responses[("ssm", "describe-parameters")] = {"Parameters": [{"Name": "/hot/hot-example/release"}]}
            cli.responses[("ssm", "list-tags-for-resource")] = {"TagList": tags}
            return preflight.run(aws_installation(), cli, imports=imports)
        report = pointer([])
        self.assertEqual(statuses(report)["Release pointer"], "fail")
        block = 'import {\n  to = aws_ssm_parameter.release\n  id = "/hot/hot-example/release"\n}'
        self.assertIn("ParameterAlreadyExists", details(report)["Release pointer"])
        self.assertIn(block, details(report)["Release pointer"])
        table = report.markdown("Preflight")
        self.assertRegex(table, r"\| ❌ \| Release pointer \| [^\n]*import \{<br>  to = aws_ssm_parameter\.release<br>[^\n]*\|\n",
                         "one row of the table")
        other = [{"Key": "HotInstallation", "Value": "other"}]
        self.assertEqual(statuses(pointer(other))["Release pointer"], "fail")
        # Only the apply tags it, so the plan that imports it has to run.
        report = pointer([], imports=block)
        self.assertEqual(statuses(report)["Release pointer"], "pass")
        self.assertIn("imports.tf adopts it", details(report)["Release pointer"])
        unrelated = 'import {\n  to = aws_ssm_parameter.release_old\n  id = "/x"\n}\n'
        self.assertEqual(statuses(pointer([], imports=unrelated))["Release pointer"], "fail")

    def test_a_scalable_stack_creates_no_such_pointer(self):
        cli = FakeCli(AWS)
        cli.responses[("ssm", "describe-parameters")] = {"Parameters": [{"Name": "/hot/hot-example/release"}]}
        report = preflight.run(aws_installation(preset="scalable"), cli)
        self.assertNotIn("Release pointer", statuses(report))
        self.assertFalse([c for c in cli.calls if c[1] == "ssm"])

    def test_the_command_reads_the_imports_tf_beside_the_tfvars(self):
        cli = FakeCli(AWS)
        cli.responses[("ssm", "describe-parameters")] = {"Parameters": [{"Name": "/hot/hot-example/release"}]}
        cli.responses[("ssm", "list-tags-for-resource")] = {"TagList": []}
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(preflight, "Cli", lambda: cli), \
                mock.patch("sys.stdout", io.StringIO()) as stdout:
            tfvars = Path(tmp) / "installation.tfvars.json"
            tfvars.write_text(json.dumps(aws_installation()))
            self.assertEqual(preflight.main([str(tfvars)]), 1)
            self.assertIn('id = "/hot/hot-example/release"', stdout.getvalue())
            (Path(tmp) / "imports.tf").write_text('import {\n  to = aws_ssm_parameter.release\n  id = "/hot/hot-example/release"\n}\n')
            self.assertEqual(preflight.main([str(tfvars)]), 0)

    def test_managed_data_checks_regional_offerings(self):
        document = aws_installation(data={"postgres": {"mode": "managed"}, "valkey": {"mode": "managed"}})
        cli = FakeCli(AWS)
        cli.responses[("rds", "describe-orderable-db-instance-options")] = {"OrderableDBInstanceOptions": []}
        report = preflight.run(document, cli)
        self.assertIn("db.m7g.large", details(report)["Managed PostgreSQL"])
        self.assertEqual(statuses(report)["Managed PostgreSQL"], "fail")
        self.assertEqual(details(report)["Managed Valkey"], "Valkey 8.1 on cache.m7g.large")

    def test_managed_postgres_compares_the_versions_rds_lists(self):
        # The contract takes a major version, RDS lists full ones, and the
        # filter may match exactly, so a major version must not be asked of it.
        document = aws_installation(data={"postgres": {"mode": "managed"}})

        def postgres(*versions):
            cli = FakeCli(AWS)
            cli.responses[("rds", "describe-orderable-db-instance-options")] = {
                "OrderableDBInstanceOptions": [{"EngineVersion": v} for v in versions]}
            report = preflight.run(document, cli)
            return statuses(report)["Managed PostgreSQL"], details(report)["Managed PostgreSQL"], cli
        for versions in (("18.6",), ("17.6", "18.6"), ("18",), ("18.10", "18.2")):
            with self.subTest(versions):
                self.assertEqual(postgres(*versions)[:2], ("pass", "PostgreSQL 18 on db.m7g.large"))
        for versions in ((), ("17.6",), ("17.6", "16.18"), ("180.1",), ("1.8",)):
            with self.subTest(versions):
                status, detail, _ = postgres(*versions)
                self.assertEqual(status, "fail")
                self.assertEqual(detail, "RDS does not offer PostgreSQL 18 on db.m7g.large in us-east-1")
        listed = next(c for c in postgres("18.6")[2].calls if "describe-orderable-db-instance-options" in c)
        self.assertEqual(listed[listed.index("--db-instance-class") + 1], "db.m7g.large")
        self.assertNotIn("--engine-version", listed)
        self.assertNotIn("--max-items", listed, "the option for 18 may not be the first one listed")

    def test_managed_valkey_checks_the_node_type(self):
        document = aws_installation(data={"valkey": {"mode": "managed", "node_type": "cache.r7g.large"}})
        cli = FakeCli(AWS)
        cli.responses[("elasticache", "describe-reserved-cache-nodes-offerings")] = {"ReservedCacheNodesOfferings": []}
        report = preflight.run(document, cli)
        self.assertEqual(statuses(report)["Managed Valkey"], "warn")
        self.assertIn("no offering of cache.r7g.large in us-east-1", details(report)["Managed Valkey"])
        listed = next(c for c in cli.calls if "describe-reserved-cache-nodes-offerings" in c)
        self.assertEqual(listed[listed.index("--cache-node-type") + 1], "cache.r7g.large")


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
        cli.responses[("services", "list")] = [{"config": {"name": api}} for api in ENABLED
                                               if api == "compute.googleapis.com" or api not in preflight.GCP_APIS]
        report = preflight.run(gcp_installation(), cli)
        self.assertEqual(statuses(report)["Nested virtualization"], "fail")
        self.assertEqual(statuses(report)["APIs"], "warn")
        self.assertIn("secretmanager.googleapis.com", details(report)["APIs"])

    def test_apis_no_stack_enables_are_named(self):
        def without(*off):
            cli = FakeCli(GCP)
            cli.responses[("services", "list")] = [{"config": {"name": api}} for api in ENABLED if api not in off]
            return preflight.run(gcp_installation(), cli)
        report = without("iam.googleapis.com", "secretmanager.googleapis.com")
        self.assertEqual(statuses(report)["APIs"], "warn", "an identity from another project can apply with this off here")
        self.assertIn("an apply needs iam.googleapis.com here or in its identity's project, which no stack enables",
                      details(report)["APIs"])
        self.assertIn("the stack will enable secretmanager.googleapis.com", details(report)["APIs"])
        self.assertFalse(report.failed, report.results)
        report = without("sts.googleapis.com")
        self.assertEqual(statuses(report)["APIs"], "warn", "a terminal using its own identity does not need this")
        self.assertIn("impersonation need sts.googleapis.com", details(report)["APIs"])
        self.assertFalse(report.failed, report.results)

    def test_the_bootstrap_enables_the_apis_no_stack_enables(self):
        bootstrap = (ROOT / "opentofu/bootstrap/gcp/main.tf").read_text()
        for api in preflight.GCP_APPLY_APIS + preflight.GCP_SIGN_IN_APIS:
            self.assertIn(f'"{api}"', bootstrap)

    def test_compute_checks_wait_for_the_stack_to_enable_compute(self):
        disabled = preflight.CliError("Compute Engine API has not been used in project example-project before or it is disabled")
        cli = FakeCli(GCP)
        cli.responses[("services", "list")] = [{"config": {"name": api}} for api in ENABLED
                                               if api != "compute.googleapis.com"]
        for key in (("compute", "machine-types"), ("compute", "regions"), ("compute", "project-info")):
            cli.responses[key] = disabled
        report = preflight.run(gcp_installation(), cli)
        self.assertFalse(report.failed, report.results)
        self.assertIn("not checked until compute.googleapis.com is enabled", details(report)["Machine type"])
        self.assertEqual(statuses(report)["vCPU quota"], "warn")
        self.assertEqual(statuses(report)["Host image"], "pass", "the images project has Compute enabled")
        cli.responses[("compute", "images")] = disabled
        report = preflight.run(gcp_installation(host_image="projects/example-project/global/images/hot-host-amd64"), cli)
        self.assertFalse(report.failed, report.results)
        self.assertEqual(statuses(report)["Host image"], "warn")

    def test_managed_postgres_on_an_existing_network_needs_private_services_access(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        report = preflight.run(gcp_installation(network=network, data={"postgres": {"mode": "managed"}}), FakeCli(GCP))
        self.assertEqual(statuses(report)["Private services access"], "fail")
        cli = FakeCli(GCP)
        cli.responses[("services", "vpc-peerings")] = [{"service": "services/servicenetworking.googleapis.com"}]
        listed = {"postgres": {"mode": "managed", "endpoint": {"deny_cidrs": ["10.20.0.0/20"]}}}
        report = preflight.run(gcp_installation(network=network, data=listed), cli)
        self.assertEqual(statuses(report)["Private services access"], "pass")

    def test_private_services_range_on_an_existing_network_is_the_operators_to_deny(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        cli = FakeCli(GCP)
        cli.responses[("services", "vpc-peerings")] = [{"service": "services/servicenetworking.googleapis.com"}]
        for postgres in ({"mode": "managed"}, {"mode": "managed", "endpoint": None},
                         {"mode": "managed", "endpoint": {"deny_cidrs": []}}):
            report = preflight.run(gcp_installation(network=network, data={"postgres": postgres}), cli)
            self.assertFalse(report.failed, report.results)
            self.assertEqual(statuses(report)["Private services access"], "warn", postgres)
            detail = details(report)["Private services access"]
            self.assertIn("data.postgres.endpoint.deny_cidrs", detail)
            self.assertIn("gcloud compute addresses list --global --filter=purpose=VPC_PEERING", detail)
        # Valkey's entry does not name the database's range.
        other ={"postgres": {"mode": "managed"}, "valkey": {"mode": "existing", "endpoint": {"deny_cidrs": ["10.20.0.0/20"]}}}
        self.assertEqual(statuses(preflight.run(gcp_installation(network=network, data=other), cli))["Private services access"], "warn")
        # A network the stack creates has its range found for it.
        self.assertNotIn("Private services access", statuses(preflight.run(gcp_installation(data={"postgres": {"mode": "managed"}}), cli)))

    def test_hosts_need_cloud_nat_and_private_google_access(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        self.assertEqual(statuses(preflight.run(gcp_installation(network=network), FakeCli(GCP)))["Outbound routes"], "pass")
        cli = FakeCli(GCP)
        cli.responses[("compute", "networks")] = {"ipCidrRange": "10.40.0.0/20", "privateIpGoogleAccess": False}
        self.assertEqual(statuses(preflight.run(gcp_installation(network=network), cli))["Outbound routes"], "warn")
        cli.responses[("compute", "routers")] = [{"network": "projects/example-project/global/networks/other",
                                                   "nats": [{"sourceSubnetworkIpRangesToNat": "ALL_SUBNETWORKS_ALL_IP_RANGES"}]}]
        self.assertIn("no Cloud NAT", details(preflight.run(gcp_installation(network=network), cli))["Outbound routes"])
        subnetwork = {"name": "projects/example-project/regions/us-central1/subnetworks/hot", "sourceIpRangesToNat": ["ALL_IP_RANGES"]}
        cli.responses[("compute", "routers")] = [{"network": "projects/example-project/global/networks/shared", "nats": [
            {"sourceSubnetworkIpRangesToNat": "LIST_OF_SUBNETWORKS", "subnetworks": [subnetwork]}]}]
        self.assertNotEqual(statuses(preflight.run(gcp_installation(network=network), cli))["Outbound routes"], "fail")
        subnetwork["sourceIpRangesToNat"] = ["LIST_OF_SECONDARY_IP_RANGES"]
        self.assertIn("primary range", details(preflight.run(gcp_installation(network=network), cli))["Outbound routes"])

    def test_cloud_nat_needs_a_default_route_to_the_internet_gateway(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        cli = FakeCli(GCP)
        cli.responses[("compute", "routes")] = [dict(GCP[("compute", "routes")][0], tags=["proxy"])]
        report = preflight.run(gcp_installation(network=network), cli)
        self.assertEqual(statuses(report)["Outbound routes"], "fail", "a tagged route does not reach the hosts")
        self.assertIn("shared has no default route", details(report)["Outbound routes"])
        appliance = {"network": "projects/example-project/global/networks/shared", "destRange": "0.0.0.0/0", "priority": 100,
                     "nextHopIlb": "projects/example-project/regions/us-central1/forwardingRules/egress-proxy"}
        cli.responses[("compute", "routes")] = [*GCP[("compute", "routes")], appliance]
        report = preflight.run(gcp_installation(network=network), cli)
        self.assertEqual(statuses(report)["Outbound routes"], "warn", "egress through an appliance, not Cloud NAT")
        self.assertIn("through egress-proxy", details(report)["Outbound routes"])

    def test_a_default_route_may_be_learned_where_routes_list_cannot_show_it(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        shared = "projects/example-project/global/networks/shared"
        bgp = [{"network": shared, "bgpPeers": [{"name": "on-prem"}]}]

        def outbound(routes, routers, described=None):
            cli = FakeCli(GCP)
            cli.responses[("compute", "routes")] = routes
            cli.responses[("compute", "routers")] = routers
            cli.responses[("compute", "networks")] = {**GCP[("compute", "networks")], **(described or {})}
            report = preflight.run(gcp_installation(network=network), cli)
            return statuses(report)["Outbound routes"], details(report)["Outbound routes"], cli
        status, detail, cli = outbound([], bgp)
        self.assertEqual(status, "warn", "a Cloud Router's BGP peers can teach the default route")
        self.assertIn("shared has no static default route; a default route may be learned", detail)
        listed = next(c for c in cli.calls if c[1:4] == ("compute", "routers", "list"))
        self.assertNotIn("--regions", listed, "dynamic routing can be global, so every region counts")
        peered = {"peerings": [{"name": "transit", "importCustomRoutes": True}]}
        self.assertEqual(outbound([], [], peered)[0], "warn", "a peer can export the default route")
        # Neither source of learned routes: the network has no way out.
        self.assertEqual(outbound([], [])[0], "fail")
        self.assertEqual(outbound([], [{"network": "projects/example-project/global/networks/other",
                                        "bgpPeers": [{"name": "on-prem"}]}])[0], "fail", "another network's router")
        self.assertEqual(outbound([], [{"network": shared}])[0], "fail", "a router with no peers learns nothing")
        self.assertEqual(outbound([], [], {"peerings": [{"name": "services", "importCustomRoutes": False}]})[0], "fail")
        # A learned route can outrank the static one, so a missing NAT is only a warning there.
        status, detail, _ = outbound(GCP[("compute", "routes")], bgp)
        self.assertEqual(status, "warn")
        self.assertIn("no Cloud NAT in us-central1 covers hot's primary range, but a default route may be learned", detail)

    def test_an_artifact_registry_mirror_must_hold_docker_images(self):
        document = gcp_installation(image_registry="us-docker.pkg.dev/acme-mirror/hot")
        cli = FakeCli(GCP)
        self.assertEqual(statuses(preflight.run(document, cli))["Image mirror"], "pass")
        described = next(c for c in cli.calls if c[1:3] == ("artifacts", "repositories"))
        self.assertEqual(described[3:], ("describe", "hot", "--location", "us", "--project", "acme-mirror", "--format", "json"))
        cli.responses[("artifacts", "repositories")] = {"format": "MAVEN"}
        self.assertEqual(statuses(preflight.run(document, cli))["Image mirror"], "fail")

    def test_an_artifact_registry_mirror_this_identity_may_not_look_at_only_warns(self):
        # A plan needs only getIamPolicy on a mirror in another project.
        document = gcp_installation(image_registry="us-docker.pkg.dev/acme-mirror/hot")
        cli = FakeCli(GCP)
        for refused in ("ERROR: (gcloud.artifacts.repositories.describe) PERMISSION_DENIED: Permission "
                        "'artifactregistry.repositories.get' denied on resource (or it may not exist).",
                        "ERROR: (gcloud.artifacts.repositories.describe) User [plan@acme.iam.gserviceaccount.com] does not "
                        "have permission to access projects instance [acme-mirror] (or it may not exist)"):
            with self.subTest(refused):
                cli.responses[("artifacts", "repositories")] = preflight.CliError(refused)
                report = preflight.run(document, cli)
                self.assertFalse(report.failed, report.results)
                self.assertEqual(statuses(report)["Image mirror"], "warn")
                self.assertIn("may not look at hot in acme-mirror", details(report)["Image mirror"])
                self.assertIn("the first deploy's image pull will", details(report)["Image mirror"])
        cli.responses[("artifacts", "repositories")] = preflight.CliError(
            "ERROR: (gcloud.artifacts.repositories.describe) NOT_FOUND: Repository \"hot\" not found")
        self.assertEqual(statuses(preflight.run(document, cli))["Image mirror"], "fail", "a confirmed miss")

    def test_scalable_quota_counts_every_role(self):
        cli = FakeCli(GCP)
        report = preflight.run(gcp_installation(preset="scalable"), cli)
        self.assertEqual(statuses(report)["vCPU quota"], "warn")
        self.assertIn("CPUS has 20 of 24 free; 22 are needed", details(report)["vCPU quota"], "(4 + 4 + 2 + 1) e2, 2 vCPUs each")
        described = {c[4] for c in cli.calls if c[1:3] == ("compute", "machine-types")}
        self.assertEqual(described, {"e2-standard-2", "n2-standard-2"})
        cli.responses[("compute", "regions")] = {"quotas": [{"metric": "CPUS", "limit": 16.0, "usage": 0.0}]}
        cli.responses[("compute", "project-info")] = {"quotas": [{"metric": "CPUS_ALL_REGIONS", "limit": 12.0, "usage": 0.0}]}
        report = preflight.run(gcp_installation(preset="scalable"), cli)
        self.assertEqual(details(report)["vCPU quota"], "CPUS is 16; 22 are needed; CPUS_ALL_REGIONS is 12; 24 are needed")

    def test_current_use_only_warns_and_n2_has_its_own_quota(self):
        cli = FakeCli(GCP)
        # Other E2 workloads fill CPUS; the host's own vCPUs fill N2_CPUS.
        cli.responses[("compute", "regions")] = {"quotas": [{"metric": "CPUS", "limit": 24.0, "usage": 24.0},
                                                            {"metric": "N2_CPUS", "limit": 8.0, "usage": 8.0}]}
        report = preflight.run(gcp_installation(), cli)
        self.assertEqual(statuses(report)["vCPU quota"], "warn")
        self.assertEqual(details(report)["vCPU quota"].split(" (")[0], "N2_CPUS has 0 of 8 free; 2 are needed")
        self.assertFalse(report.failed, report.results)

    def test_provisioned_buckets_are_free_or_this_installations_own(self):
        storage = {"build": {"backend": "gcs", "bucket": "acme-hot", "provision": True}, "file": {"backend": "local"}}
        cli = FakeCli(GCP)
        cli.responses[("storage", "buckets")] = preflight.CliError("ERROR: (gcloud.storage.buckets.describe) gs://acme-hot not found: 404.")
        self.assertEqual(statuses(preflight.run(gcp_installation(storage=storage), cli))["Bucket for build"], "pass")
        cli.responses[("storage", "buckets")] = {"name": "acme-hot", "labels": {"hot-installation": "hot-example"}}
        self.assertIn("provisioned by this installation", details(preflight.run(gcp_installation(storage=storage), cli))["Bucket for build"])
        cli.responses[("storage", "buckets")] = {"name": "acme-hot"}
        report = preflight.run(gcp_installation(storage=storage), cli)
        self.assertEqual(statuses(report)["Bucket for build"], "fail")
        self.assertIn("is not labeled hot-installation=hot-example", details(report)["Bucket for build"])
        # Cloud Storage answers 403 for a name someone else holds.
        cli.responses[("storage", "buckets")] = preflight.CliError("ERROR: HTTPError 403: plan does not have storage.buckets.get access")
        self.assertEqual(statuses(preflight.run(gcp_installation(storage=storage), cli))["Bucket for build"], "fail")

    def test_an_existing_bucket_needs_uniform_bucket_level_access(self):
        storage = {"build": {"backend": "gcs", "bucket": "acme-hot"}, "file": {"backend": "local"}}
        cli = FakeCli(GCP)
        cli.responses[("storage", "buckets")] = {"name": "acme-hot", "uniform_bucket_level_access": True}
        report = preflight.run(gcp_installation(storage=storage), cli)
        self.assertEqual(details(report)["Bucket for build"], "acme-hot is reachable")
        # The stacks' conditional binding would fail the first apply partway.
        for described in ({"name": "acme-hot", "uniform_bucket_level_access": False}, {"name": "acme-hot"}):
            cli.responses[("storage", "buckets")] = described
            report = preflight.run(gcp_installation(storage=storage), cli)
            self.assertEqual(statuses(report)["Bucket for build"], "fail")
            self.assertIn("gcloud storage buckets update gs://acme-hot --uniform-bucket-level-access",
                          details(report)["Bucket for build"])

    def test_an_existing_bucket_the_identity_may_not_read_fails(self):
        # The infra README asks for storage.buckets.get there, and only this
        # read shows the access mode and owner that would fail an apply partway.
        storage = {"build": {"backend": "gcs", "bucket": "acme-hot"}, "file": {"backend": "local"}}
        cli = FakeCli(GCP)
        cli.responses[("storage", "buckets")] = preflight.CliError("ERROR: HTTPError 403: plan does not have storage.buckets.get access")
        self.assertEqual(statuses(preflight.run(gcp_installation(storage=storage), cli))["Bucket for build"], "fail")

    def test_an_existing_bucket_another_installation_provisioned_fails(self):
        # Its stack sets the whole IAM policy, so the two stacks would undo each
        # other's every apply, and this installation's hosts would lose access.
        storage = {"build": {"backend": "gcs", "bucket": "acme-hot", "prefix": "staging/builds"}, "file": {"backend": "local"}}
        uniform = {"name": "acme-hot", "uniform_bucket_level_access": True}
        cli = FakeCli(GCP)
        cli.responses[("storage", "buckets")] = {**uniform, "labels": {"hot-installation": "prod"}}
        report = preflight.run(gcp_installation(storage=storage), cli)
        self.assertEqual(statuses(report)["Bucket for build"], "fail")
        detail = details(report)["Bucket for build"]
        self.assertIn("labeled hot-installation=prod", detail)
        self.assertIn("its next apply would remove this installation's access", detail)
        self.assertIn("gcloud storage buckets update gs://acme-hot --remove-labels=hot-installation", detail)
        for labels in ({"hot-installation": "hot-example"}, {"team": "data"}, None):
            with self.subTest(labels):
                cli.responses[("storage", "buckets")] = {**uniform, "labels": labels}
                report = preflight.run(gcp_installation(storage=storage), cli)
                self.assertEqual(details(report)["Bucket for build"], "acme-hot is reachable")

    def test_every_gcp_stack_binds_the_bucket_with_a_condition(self):
        for path in ("single-machine/main.tf", "scalable/storage.tf"):
            stack = (ROOT / "opentofu/stacks/gcp" / path).read_text()
            binding = stack.split('resource "google_storage_bucket_iam_member" "existing"', 1)[1].split("\nresource ", 1)[0]
            self.assertIn("condition {", binding, path)

    def test_managed_data_checks_the_tier_and_node_type(self):
        report = preflight.run(gcp_installation(data={"postgres": {"mode": "managed"}, "valkey": {"mode": "managed"}}), FakeCli(GCP))
        self.assertEqual(details(report)["Managed PostgreSQL"], "PostgreSQL 18 on db-custom-2-8192")
        self.assertEqual(details(report)["Managed Valkey"], "Memorystore for Valkey on STANDARD_SMALL")
        bad = gcp_installation(data={"postgres": {"mode": "managed", "tier": "db-custom-3-2048"},
                                     "valkey": {"mode": "managed", "node_type": "HUGE"}})
        report = preflight.run(bad, FakeCli(GCP))
        self.assertEqual(statuses(report)["Managed PostgreSQL"], "fail")
        self.assertEqual(statuses(report)["Managed Valkey"], "warn")
        for tier, status in (("db-custom-2-2048", "fail"), ("db-custom-1-1024", "fail"), ("db-custom-1-3840", "pass")):
            sized = gcp_installation(data={"postgres": {"mode": "managed", "tier": tier}})
            report = preflight.run(sized, FakeCli(GCP))
            self.assertEqual(statuses(report)["Managed PostgreSQL"], status, tier)
        self.assertIn("at least 3.75 GB", details(preflight.run(bad, FakeCli(GCP)))["Managed PostgreSQL"])
        listed = gcp_installation(data={"postgres": {"mode": "managed", "tier": "db-n1-standard-2"}})
        self.assertEqual(statuses(preflight.run(listed, FakeCli(GCP)))["Managed PostgreSQL"], "warn", "until the stack enables sqladmin")
        cli = FakeCli(GCP)
        cli.responses[("services", "list")] = [{"config": {"name": api}} for api in ENABLED + ["sqladmin.googleapis.com"]]
        self.assertEqual(statuses(preflight.run(listed, cli))["Managed PostgreSQL"], "pass")
        cli.responses[("sql", "tiers")] = [{"tier": "db-n1-standard-2", "region": ["europe-west1"]}]
        self.assertIn("does not offer db-n1-standard-2 in us-central1", details(preflight.run(listed, cli))["Managed PostgreSQL"])

    def test_managed_valkey_on_an_existing_network_needs_a_connection_policy(self):
        network = {"mode": "existing", "network_id": "projects/example-project/global/networks/shared",
                   "subnet_id": "projects/example-project/regions/us-central1/subnetworks/hot"}
        cli = FakeCli(GCP)
        cli.responses[("network-connectivity", "service-connection-policies")] = []
        report = preflight.run(gcp_installation(network=network, data={"valkey": {"mode": "managed"}}), cli)
        self.assertEqual(statuses(report)["Memorystore connection policy"], "fail")
        self.assertIn("memorystore.googleapis.com", details(report)["APIs"])
        cli.responses[("network-connectivity", "service-connection-policies")] = [
            {"serviceClass": "gcp-memorystore", "network": "projects/example-project/global/networks/shared"}]
        report = preflight.run(gcp_installation(network=network, data={"valkey": {"mode": "managed"}}), cli)
        self.assertEqual(statuses(report)["Memorystore connection policy"], "pass")


class Refused(unittest.TestCase):
    def test_only_a_refusal_counts(self):
        for text in ("An error occurred (403) when calling the HeadBucket operation: Forbidden",
                     "An error occurred (AccessDenied) when calling the GetBucketEncryption operation: Access Denied",
                     "An error occurred (AccessDeniedException) when calling the DescribeRepositories operation: User: x",
                     "ERROR: (gcloud.artifacts.repositories.describe) PERMISSION_DENIED: Permission denied",
                     "ERROR: (gcloud.artifacts.repositories.describe) User [x] does not have permission to access",
                     "ERROR: HTTPError 403: x does not have storage.buckets.get access"):
            with self.subTest(text):
                self.assertTrue(preflight.refused(preflight.CliError(text)))
        for text in ("An error occurred (404) when calling the HeadBucket operation: Not Found",
                     "An error occurred (RepositoryNotFoundException) when calling the DescribeRepositories operation",
                     "An error occurred (ExpiredToken) when calling the HeadBucket operation",
                     "ERROR: (gcloud.artifacts.repositories.describe) NOT_FOUND: Repository \"bucket-403\" not found",
                     "ERROR: gs://acme-403 not found: 404.",
                     "Could not connect to the endpoint URL",
                     ""):
            with self.subTest(text):
                self.assertFalse(preflight.refused(preflight.CliError(text)))


class Static(unittest.TestCase):
    def test_network_ranges_must_avoid_the_host_networks(self):
        report = preflight.Report()
        with mock.patch.object(preflight.socket, "getaddrinfo", side_effect=OSError):
            preflight.static_checks(report, aws_installation(network={"cidr": "172.30.0.0/16"})["installation"])
        self.assertEqual(statuses(report)["Network ranges"], "fail")
        self.assertEqual(statuses(report)["DNS for app"], "warn")

    def test_wide_client_ranges_only_warn(self):
        def client_ranges(cidrs):
            report = preflight.Report()
            with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
                preflight.static_checks(report, aws_installation(network={"client_cidrs": cidrs})["installation"])
            return statuses(report)["Client ranges"], details(report)["Client ranges"]
        self.assertEqual(client_ranges(["10.0.0.0/8"])[0], "warn", "only clients inside 10.88.0.0/16 are cut off")
        self.assertIn("clients inside those networks cannot reach the hosts", client_ranges(["10.0.0.0/8"])[1])
        self.assertEqual(client_ranges(["10.0.0.0/8", "10.88.4.0/24"])[0], "fail", "a range inside a fixed network")
        self.assertEqual(client_ranges(["192.168.0.0/16"])[0], "pass")

    def test_image_pulls_without_a_mirror_are_left_to_the_host(self):
        report = preflight.Report()
        with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
            preflight.static_checks(report, aws_installation()["installation"])
        self.assertEqual(statuses(report)["Image pulls"], "host")

    def test_existing_data_services_are_left_to_the_host(self):
        report = preflight.Report()
        with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
            preflight.static_checks(report, aws_installation(data={"postgres": {"mode": "existing"}})["installation"])
        self.assertEqual(statuses(report)["Existing postgres"], "host")

    def test_a_data_setting_the_contract_does_not_know_fails(self):
        # The contract's typed object drops it with no error, so a misspelled or
        # misplaced deny list leaves its networks open to user code.
        for data, key in (({"postgres": {"mode": "existing", "endpoint": {"deny_cidr": ["10.1.2.0/24"]}}}, "data.postgres.endpoint.deny_cidr"),
                          ({"valkey": {"mode": "existing", "deny_cidrs": ["10.1.3.0/24"]}}, "data.valkey.deny_cidrs"),
                          ({"valkey": {"mode": "managed", "endpoint": {"host": "cache.internal"}}}, "data.valkey.endpoint.host"),
                          ({"valkey": {"mode": "managed", "tier": "small"}}, "data.valkey.tier"),
                          ({"postgress": {"mode": "managed"}}, "data.postgress")):
            with self.subTest(key):
                report = preflight.Report()
                with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
                    preflight.static_checks(report, aws_installation(data=data)["installation"])
                self.assertEqual(statuses(report)["Data settings"], "fail")
                self.assertTrue(details(report)["Data settings"].startswith(key + ":"), details(report)["Data settings"])
        report = preflight.run(aws_installation(data={"postgres": {"mode": "existing", "endpoint": {"deny_cidr": []}},
                                                      "valkey": {"mode": "existing", "deny_cidrs": []}}), FakeCli(AWS))
        self.assertTrue(report.failed, "it stops the plan")
        self.assertTrue(details(report)["Data settings"].startswith("data.postgres.endpoint.deny_cidr, data.valkey.deny_cidrs:"))

    def test_the_data_settings_the_contract_knows_pass(self):
        def data_settings(data):
            report = preflight.Report()
            with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
                preflight.static_checks(report, aws_installation(**data)["installation"])
            return statuses(report).get("Data settings")
        self.assertIsNone(data_settings({}), "nothing to check")
        self.assertEqual(data_settings({"data": {"postgres": {"mode": "existing", "endpoint": {"deny_cidrs": ["10.1.2.0/24"]}},
                                                 "valkey": {"mode": "existing", "endpoint": None}}}), "pass")
        self.assertEqual(data_settings({"data": {"postgres": {"mode": "managed", "tier": "db.m7g.large", "version": "18",
                                                              "storage_gb": 100, "high_availability": True, "runtime_role": "hot_app",
                                                              "backup_retention_days": 14, "deletion_protection": False,
                                                              "admin_password_version": 2},
                                                 "valkey": {"mode": "managed", "node_type": "cache.m7g.large",
                                                            "high_availability": True, "auth_token_version": 2}}}), "pass")

    def test_data_keys_match_the_contract(self):
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        for service, allowed in preflight.DATA_KEYS.items():
            block = re.search(rf"^\s+{service} = optional\(object\(\{{\n(.*?)\n\s+\}}\), \{{\}}\)$", contract, re.M | re.S).group(1)
            self.assertEqual(set(re.findall(r"^\s+(\w+)\s+= optional\(", block, re.M)), allowed, service)
            self.assertRegex(block, r"endpoint\s+= optional\(object\(\{ deny_cidrs = optional\(list\(string\), \[\]\) \}\), \{\}\)", service)

    def test_data_keys_match_the_stacks(self):
        # Each stack refuses an unknown key from its own copy of these lists,
        # because the contract's typed object would drop the key with no error.
        def allowed(variables, path):
            names = re.search(rf"setsubtract\(try\(keys\(var\.installation\.{re.escape(path)}\), \[\]\), \[(.*?)\]\)", variables)
            self.assertIsNotNone(names, path)
            return set(re.findall(r'"(\w+)"', names.group(1)))
        for cloud in ("aws", "gcp"):
            for preset in ("single-machine", "scalable"):
                stack = f"{cloud}/{preset}"
                variables = (ROOT / "opentofu/stacks" / stack / "variables.tf").read_text()
                with self.subTest(stack=stack):
                    self.assertEqual(allowed(variables, "data"), set(preflight.DATA_KEYS))
                    for service, keys in preflight.DATA_KEYS.items():
                        self.assertEqual(allowed(variables, f"data.{service}"), keys, service)
                        self.assertEqual(allowed(variables, f"data.{service}.endpoint"), {"deny_cidrs"}, service)

    def test_no_way_to_a_first_account_only_warns(self):
        def first_administrator(**overrides):
            report = preflight.Report()
            with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
                preflight.static_checks(report, aws_installation(**overrides)["installation"])
            self.assertFalse(report.failed, "a database with users needs no admin_email")
            return statuses(report).get("First administrator")
        self.assertEqual(first_administrator(), "warn", "the bundle's default is invite")
        self.assertEqual(first_administrator(settings={"HOT_APP_SIGNUP": "invite"}), "warn")
        self.assertEqual(first_administrator(admin_email="", settings={"HOT_EMAIL_PROVIDER": "smtp"}), "warn")
        self.assertIsNone(first_administrator(admin_email="admin@example.com"))
        self.assertIsNone(first_administrator(settings={"HOT_APP_SIGNUP": "open", "HOT_EMAIL_PROVIDER": "smtp",
                                                        "HOT_EMAIL_SMTP_HOST": "smtp.example.com", "HOT_EMAIL_FROM": "hot@example.com"}))
        self.assertIsNone(first_administrator(settings={"HOT_APP_SIGNUP": " Open ", "HOT_EMAIL_PROVIDER": " SMTP ",
                                                        "HOT_EMAIL_SMTP_HOST": " smtp.example.com ", "HOT_EMAIL_FROM": " hot@example.com "}),
                          "as Hot reads them")

    def test_open_sign_up_needs_a_way_to_verify_the_first_address(self):
        # crates/hot_app/src/signup.rs: the form admits only invited users when
        # no email can be delivered, and SMTP is the only provider Hot registers.
        def warning(**overrides):
            report = preflight.Report()
            with mock.patch.object(preflight.socket, "getaddrinfo", lambda host, port: []):
                preflight.static_checks(report, aws_installation(**overrides)["installation"])
            self.assertFalse(report.failed, "Google or GitHub sign-in or a database with users is enough")
            return details(report).get("First administrator")
        for provider in ({}, {"HOT_EMAIL_PROVIDER": ""}, {"HOT_EMAIL_PROVIDER": " None "}, {"HOT_EMAIL_PROVIDER": "smpt"}):
            with self.subTest(provider):
                text = warning(settings={"HOT_APP_SIGNUP": "open", **provider})
                self.assertRegex(text, "^admin_email is empty and sign-up is open, but open sign-up verifies addresses by email")
                self.assertIn("Google or GitHub sign-in is configured in the operator secret, which preflight does not read", text)
        self.assertIsNone(warning(admin_email="admin@example.com", settings={"HOT_APP_SIGNUP": "open"}))
        self.assertNotIn("Google", warning(), "invitation-only sign-up admits only the invited, by any route")
        # delivery_status in crates/hot/src/email.rs: a provider name alone does
        # not deliver. It takes a relay and a sender that is not Hot Cloud's,
        # which the init templates name when HOT_EMAIL_FROM is unset.
        smtp = {"HOT_APP_SIGNUP": "open", "HOT_EMAIL_PROVIDER": "smtp", "HOT_EMAIL_SMTP_HOST": "smtp.example.com",
                "HOT_EMAIL_FROM": "hot@example.com"}
        self.assertIsNone(warning(settings=smtp))
        for change, named in (({"HOT_EMAIL_SMTP_HOST": ""}, "settings has no HOT_EMAIL_SMTP_HOST"),
                              ({"HOT_EMAIL_SMTP_HOST": "  "}, "settings has no HOT_EMAIL_SMTP_HOST"),
                              ({"HOT_EMAIL_FROM": ""}, "settings has no HOT_EMAIL_FROM"),
                              ({"HOT_EMAIL_FROM": "hi@notifications.hot.dev"}, "HOT_EMAIL_FROM in settings is at notifications.hot.dev"),
                              ({"HOT_EMAIL_FROM": "HI@Notifications.Hot.Dev "}, "HOT_EMAIL_FROM in settings is at notifications.hot.dev"),
                              ({"HOT_EMAIL_FROM": "hot"}, "HOT_EMAIL_FROM in settings is not an email address"),
                              # check_sender: Hot writes HOT_EMAIL_FROM_NAME <HOT_EMAIL_FROM>, and lettre must parse it.
                              ({"HOT_EMAIL_FROM": "Acme <hi@acme.com>"}, "a display name belongs in HOT_EMAIL_FROM_NAME"),
                              ({"HOT_EMAIL_FROM": "hi@"}, "HOT_EMAIL_FROM in settings is not an email address"),
                              ({"HOT_EMAIL_FROM": "@acme.com"}, "HOT_EMAIL_FROM in settings is not an email address"),
                              ({"HOT_EMAIL_FROM": "h i@acme.com"}, "HOT_EMAIL_FROM in settings is not an email address"),
                              # connection_from_conf: the app checks these too, and render does not.
                              ({"HOT_EMAIL_SMTP_SECURITY": "ssl"}, "HOT_EMAIL_SMTP_SECURITY in settings is not starttls, tls, or none"),
                              ({"HOT_EMAIL_SMTP_PORT": "abc"}, "HOT_EMAIL_SMTP_PORT in settings is not a valid port"),
                              ({"HOT_EMAIL_SMTP_PORT": "0"}, "HOT_EMAIL_SMTP_PORT in settings is not a valid port"),
                              ({"HOT_EMAIL_SMTP_PORT": "65536"}, "HOT_EMAIL_SMTP_PORT in settings is not a valid port"),
                              ({"HOT_EMAIL_SMTP_PORT": "-587"}, "HOT_EMAIL_SMTP_PORT in settings is not a valid port"),
                              ({"HOT_EMAIL_SMTP_PORT": "58 7"}, "HOT_EMAIL_SMTP_PORT in settings is not a valid port"),
                              ({"HOT_EMAIL_SMTP_TIMEOUT_SECONDS": "soon"}, "HOT_EMAIL_SMTP_TIMEOUT_SECONDS in settings is not a positive integer"),
                              ({"HOT_EMAIL_SMTP_TIMEOUT_SECONDS": "0"}, "HOT_EMAIL_SMTP_TIMEOUT_SECONDS in settings is not a positive integer")):
            with self.subTest(change):
                text = warning(settings={**smtp, **change})
                self.assertRegex(text, "^admin_email is empty and sign-up is open, but open sign-up verifies addresses by email")
                self.assertIn(named, text)
                self.assertIsNone(warning(admin_email="admin@example.com", settings={**smtp, **change}))
        for missing, named in (("HOT_EMAIL_SMTP_HOST", "settings has no HOT_EMAIL_SMTP_HOST"), ("HOT_EMAIL_FROM", "settings has no HOT_EMAIL_FROM")):
            with self.subTest(missing):
                self.assertIn(named, warning(settings={k: v for k, v in smtp.items() if k != missing}))
        self.assertIsNone(warning(settings={**smtp, "HOT_EMAIL_FROM": "hot@notifications.hot.dev.example.com"}), "only Hot Cloud's own domain")
        for fine in ({"HOT_EMAIL_SMTP_SECURITY": ""}, {"HOT_EMAIL_SMTP_SECURITY": "starttls"}, {"HOT_EMAIL_SMTP_SECURITY": " TLS "},
                     {"HOT_EMAIL_SMTP_SECURITY": "none"}, {"HOT_EMAIL_SMTP_PORT": ""}, {"HOT_EMAIL_SMTP_PORT": "587"},
                     {"HOT_EMAIL_SMTP_PORT": " 2525 "}, {"HOT_EMAIL_SMTP_PORT": 465}, {"HOT_EMAIL_SMTP_PORT": "65535"},
                     {"HOT_EMAIL_SMTP_TIMEOUT_SECONDS": "30"}, {"HOT_EMAIL_SMTP_TIMEOUT_SECONDS": 45},
                     {"HOT_EMAIL_FROM": "first.last+tag@acme.example.com"}, {"HOT_EMAIL_FROM_NAME": "Acme, Inc <Support>"}):
            with self.subTest(fine):
                self.assertIsNone(warning(settings={**smtp, **fine}), "Hot delivers with this")

    def test_sizes_match_the_contract(self):
        contract = (ROOT / "opentofu/modules/single-machine-contract/main.tf").read_text()
        for size, expected in preflight.SIZES.items():
            line = re.search(rf"^\s+{size}\s+=\s+\{{ aws = \"([^\"]+)\", gcp = \"([^\"]+)\".*cpus = (\d+)", contract, re.M)
            self.assertEqual(line.groups(), (expected["aws"], expected["gcp"], str(expected["cpus"])), size)
            database = re.search(rf"^\s+{size}\s+=\s+\{{ aws = \"(db\.[^\"]+)\", gcp = \"([^\"]+)\".*valkey = \"([^\"]+)\", "
                                 rf"valkey_gcp = \"([^\"]+)\"", contract, re.M)
            self.assertEqual(database.groups(), tuple(preflight.DATABASES[size][k] for k in ("aws", "gcp", "valkey", "valkey_gcp")), size)
            role = re.search(rf"^\s+{size}\s+=\s+\{{ aws = \"(m7i[^\"]*)\", gcp = \"([^\"]+)\" \}}", contract, re.M)
            self.assertEqual(role.groups(), (preflight.ROLE_MACHINES[size]["aws"], preflight.ROLE_MACHINES[size]["gcp"]), size)
        for role, top in preflight.SCALING_MAX.items():
            self.assertRegex(contract, rf"\n\s+{role}\s+= optional\(object\(\{{(?:[^}}]|\n)*?max\s+= optional\(number, {top}\)", role)

    def test_the_markdown_report_marks_each_result(self):
        report = preflight.Report()
        report.add("pass", "Zone", "us-east-1a is available")
        report.add("fail", "Machine type", "a|b")
        text = report.markdown("Preflight for hot-example")
        self.assertIn("| ✅ | Zone | us-east-1a is available |", text)
        self.assertIn("| ❌ | Machine type | a/b |", text)


if __name__ == "__main__":
    unittest.main()
