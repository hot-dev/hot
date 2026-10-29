"""Static checks on the host image recipe and the stacks that boot it.

They pin the isolation and no-SSH properties in the source, so a change that
weakens one fails here before any image is built.
"""
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
SETUP = (ROOT / "images/host/setup-host.sh").read_text()
PACKER = (ROOT / "images/packer/hot-host.pkr.hcl").read_text()
AWS = "\n".join(p.read_text() for p in sorted((ROOT / "opentofu/stacks/aws/single-machine").glob("*.tf")))
GCP = "\n".join(p.read_text() for p in sorted((ROOT / "opentofu/stacks/gcp/single-machine").glob("*.tf")))


class HostImage(unittest.TestCase):
    def test_every_download_is_pinned_and_verified(self):
        downloads = re.findall(r'^fetch "(\S+)"', SETUP, re.M)
        self.assertGreaterEqual(len(downloads), 4)
        for name in ("KATA", "CONTAINERD", "CNI", "COSIGN"):
            self.assertRegex(SETUP, rf'{name}_SHA256="[0-9a-f]{{64}}"')
        self.assertIn("sha256sum --check", SETUP)
        self.assertNotRegex(SETUP, r"curl[^\n]*\|\s*(ba)?sh")

    def test_no_ssh_anywhere(self):
        self.assertIn("systemctl mask ssh.socket ssh.service", SETUP)
        self.assertEqual(PACKER.count('communicator          = "none"') + PACKER.count('communicator        = "none"'), 2)
        self.assertNotRegex(PACKER, r"ssh_username|use_iap|ssh_interface")
        for stack in (AWS, GCP):
            self.assertNotRegex(stack, r'(from_port|ports)\s*=\s*\[?\s*"?22"?')
        self.assertIn('block-project-ssh-keys = "TRUE"', GCP)

    def test_qemu_only_and_socket_restricted(self):
        self.assertNotIn("firecracker", SETUP.lower())
        self.assertIn("chmod 0660 /run/kata-containerd/containerd.sock", SETUP)
        self.assertNotRegex(SETUP, r"chmod 0?666")
        self.assertIn('KATA_CONTAINERD_GID="2000"', SETUP)

    def test_guest_baseline_is_authoritative(self):
        self.assertIn("iptables -t mangle -I FORWARD 1", SETUP)
        for destination in ("169.254.0.0/16", "127.0.0.0/8", "10.88.0.0/16",
                            "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"):
            self.assertIn(destination, SETUP)
        self.assertIn("net.bridge.bridge-nf-call-iptables = 1", SETUP)
        self.assertIn("br_netfilter", SETUP)
        self.assertIn('iptables -I INPUT 1 -i "$bridge" -j DROP', SETUP)

    def test_box_storage_waits_for_the_data_disk(self):
        unit = SETUP.split("kata-containerd.service <<'UNIT'")[1].split("UNIT")[0]
        self.assertIn("ConditionPathIsMountPoint=/var/lib/hot", unit)
        self.assertIn("refusing to initialize it without --allow-format", SETUP)
        # Restarted containers would otherwise start on the boot disk.
        self.assertIn("RequiresMountsFor=/var/lib/hot\\n' > /etc/systemd/system/docker.service.d/hot-data.conf", SETUP)
        self.assertIn("HOT_BOX_POOL_GB", SETUP)

    def test_metadata_is_hardened_on_aws(self):
        self.assertIn('http_tokens                 = "required"', AWS)
        self.assertIn("http_put_response_hop_limit = 2", AWS)

    def test_aws_boot_script_fits_in_user_data(self):
        packed = subprocess.run(["gzip", "-9c"], input=SETUP.encode() + b"\n" * 8,
                                capture_output=True, check=True).stdout
        self.assertLess(len(packed), 15 * 1024)


if __name__ == "__main__":
    unittest.main()
