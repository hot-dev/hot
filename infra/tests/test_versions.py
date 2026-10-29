"""The published compatibility table matches what the repository pins."""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
REPO = ROOT.parent


def table():
    readme = (ROOT / "README.md").read_text()
    section = readme.split("## Versions and upgrades", 1)[1].split("\n## ", 1)[0]
    return {cells[0]: cells[1] for cells in (
        [cell.strip() for cell in line.strip("|").split("|")] for line in section.splitlines()
        if line.startswith("| ") and not line.startswith("| ---"))}


def find(path, pattern, groups=1):
    match = re.search(pattern, (REPO / path).read_text(), re.M)
    if not match:
        raise AssertionError(f"{pattern} not found in {path}")
    return ".".join(match.group(n) for n in range(1, groups + 1))


class Compatibility(unittest.TestCase):
    def test_the_table_matches_the_pins(self):
        rows = table()
        upstream = json.loads((ROOT / "release/upstream-images.json").read_text())
        host = "infra/images/host/setup-host.sh"
        lock = "infra/opentofu/stacks/{}/single-machine/.terraform.lock.hcl"
        provider = r'provider "registry.opentofu.org/hashicorp/{}" {{\n\s+version\s+= "([^"]+)"'
        expected = {
            "Runtime image base": "Debian 13 (trixie)" if "debian:trixie" in find(
                "infra/images/containers/runtime.Dockerfile", r"^ARG DEBIAN_IMAGE=(\S+)") else "?",
            "Local PostgreSQL": "{1} with pgvector {0}".format(*re.search(
                r"pgvector:([0-9.]+)-pg([0-9]+)@", upstream["postgres"]).groups()),
            "Local Valkey": re.search(r"valkey:([0-9.]+)-", upstream["valkey"]).group(1),
            "Ingress": "nginx " + re.search(r"nginx-unprivileged:([0-9.]+)-", upstream["ingress"]).group(1),
            "Managed PostgreSQL": find("infra/opentofu/modules/single-machine-contract/main.tf",
                                       r'version\s+= optional\(string, "([0-9]+)"\)') + " (RDS, Cloud SQL)",
            "Managed Valkey": find("infra/opentofu/stacks/aws/single-machine/valkey.tf",
                                   r'engine_version\s+= "([0-9.]+)"') + " (ElastiCache), " + find(
                "infra/opentofu/stacks/gcp/single-machine/valkey.tf",
                r'engine_version\s+= "VALKEY_([0-9]+)_([0-9]+)"', 2) + " (Memorystore)",
            "Host OS": "Ubuntu 24.04 LTS" if "ubuntu-2404-lts-amd64" in (
                REPO / "infra/images/packer/hot-host.pkr.hcl").read_text() else "?",
            "Kata Containers": find(host, r'^KATA_VERSION="([^"]+)"'),
            "containerd (box tasks)": find(host, r'^CONTAINERD_VERSION="([^"]+)"'),
            "CNI plugins": find(host, r'^CNI_VERSION="([^"]+)"'),
            "OpenTofu": find(".github/workflows/selfhost-plan.yml", r"TOFU_VERSION: ([0-9.]+)"),
            "AWS provider": find(lock.format("aws"), provider.format("aws")),
            "Google provider": find(lock.format("gcp"), provider.format("google")),
            "Random provider": find(lock.format("aws"), provider.format("random")),
            "Configuration schema": find("infra/release/release_manifest.py", r"^CONFIGURATION_SCHEMA = (\d+)"),
        }
        for component, version in expected.items():
            self.assertEqual(rows.get(component), version, component)
        self.assertEqual(find(lock.format("gcp"), provider.format("random")), expected["Random provider"])
        self.assertEqual(find(".github/workflows/selfhost.yml", r"tofu_version: ([0-9.]+)"), expected["OpenTofu"])


if __name__ == "__main__":
    unittest.main()
