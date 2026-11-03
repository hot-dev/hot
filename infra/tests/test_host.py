"""Static checks on the host image recipe and the stacks that boot it.

They pin the isolation and no-SSH properties in the source, so a change that
weakens one fails here before any image is built.
"""
from pathlib import Path
from unittest import mock
import errno
import json
import os
import re
import stat
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
SETUP = (ROOT / "images/host/setup-host.sh").read_text()
PACKER = (ROOT / "images/packer/hot-host.pkr.hcl").read_text()
BUILD = (ROOT / "images/build-host.sh").read_text()
RUNTIME_IMAGE = (ROOT / "images/containers/runtime.Dockerfile").read_text()
RENDER =(ROOT / "runtime/render.py").read_text()
AWS = "\n".join(p.read_text() for p in sorted((ROOT / "opentofu/stacks/aws/single-machine").glob("*.tf")))
GCP = "\n".join(p.read_text() for p in sorted((ROOT / "opentofu/stacks/gcp/single-machine").glob("*.tf")))


def heredoc(target):
    """The body of the here-document that setup-host.sh writes to target."""
    match = re.search(rf"cat > {re.escape(target)} <<'?(\w+)'?\n(.*?)\n\1\n", SETUP, re.S)
    if not match:
        raise AssertionError(f"setup-host.sh writes no {target}")
    return match.group(2)


def unit_lines(target, key):
    """Every value a systemd unit written by setup-host.sh gives key."""
    return [line.split("=", 1)[1] for line in heredoc(target).splitlines() if line.startswith(key + "=")]


def loop_devices_added(existing=(), missing=(), refuse=None):
    """The devices hot-loop-devices adds, run against a fake /dev/loop-control on
    a kernel that already has existing. The kernel answers each add with refuse,
    when set, and leaves the nodes in missing out of /dev."""
    added, nodes = [], set(existing)

    def ioctl(descriptor, request, number):
        if descriptor != 99 or request != 0x4C80:
            raise AssertionError(f"unexpected ioctl {descriptor} {request:#x}")
        if refuse:
            raise OSError(refuse, os.strerror(refuse))
        if number in nodes:
            raise OSError(errno.EEXIST, os.strerror(errno.EEXIST))
        nodes.add(number)
        added.append(number)
        return number

    def node(path):
        number = int(path.removeprefix("/dev/loop"))
        if number not in nodes or number in missing:
            raise FileNotFoundError(path)
        return os.stat_result((stat.S_IFBLK | 0o660,) + (0,) * 9)

    script = compile(heredoc('"$LIBEXEC/hot-loop-devices"'), "hot-loop-devices", "exec")
    with mock.patch("fcntl.ioctl", ioctl), mock.patch("os.open", return_value=99), \
            mock.patch("os.close"), mock.patch("os.stat", node):
        exec(script, {"__name__": "__main__"})
    return added


def docker_unit_check(unit):
    """The exit status of the bake's check on docker.service, given its text."""
    check = re.search(r"^if grep -Eq '\^\(MountFlags.*?^fi$", SETUP, re.S | re.M).group(0)
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        (directory / "unit").write_text(unit)
        stub = directory / "systemctl"
        stub.write_text(f'#!/bin/sh\n[ "$*" = "cat docker.service" ] || exit 9\ncat {directory}/unit\n')
        stub.chmod(0o755)
        return subprocess.run(["bash", "-c", "fail() { exit 3; }\n" + check], capture_output=True,
                              env={**os.environ, "PATH": f"{directory}:{os.environ['PATH']}"}).returncode


def firewall_calls(env="", forward="-P FORWARD ACCEPT\n", restore_exit=0, list_exit=0):
    """What hot-guest-firewall does, given the contents of
    /etc/hot/guest-network.env and of mangle's FORWARD chain, run against stub
    iptables tools: the iptables-restore input (printed only when the commit
    succeeds), then each other iptables call."""
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        stub = directory / "iptables"
        (directory / "forward").write_text(forward)
        # -C finds nothing, as on a host that has no rules yet.
        stub.write_text(f'#!/bin/sh\ncase " $* " in *" -C "*) exit 1;; *" -S "*) cat {directory}/forward; exit {list_exit};; esac\n'
                        'echo "$*"\n')
        stub.chmod(0o755)
        (directory / "ip6tables").symlink_to(stub)
        restore = directory / "iptables-restore"
        restore.write_text(f'#!/bin/sh\ninput=$(cat)\n[ "$*" = "--noflush" ] || exit 9\n'
                           f'[ {restore_exit} = 0 ] || exit {restore_exit}\necho "$input"\n')
        restore.chmod(0o755)
        (directory / "guest-network.env").write_text(env)
        script = heredoc('"$LIBEXEC/hot-guest-firewall"').replace("/etc/hot/guest-network.env",
                                                                  str(directory / "guest-network.env"))
        return subprocess.run(["bash", "-c", script], check=True, capture_output=True, text=True,
                              env={**os.environ, "PATH": f"{directory}:{os.environ['PATH']}"}).stdout.splitlines()


def guest_baseline(env=""):
    """The rules hot-guest-firewall gives its chain, in order."""
    prefix = "-A HOT-GUEST-BASELINE "
    return [line.removeprefix(prefix) for line in firewall_calls(env) if line.startswith(prefix)]


def data_disk(*flags, signatures="", wipefs_exit=0, probe_exit=2, label=""):
    """Run hot-data-disk against stub disk tools: what wipefs lists and how it
    exits, how blkid -p exits, and the label plain blkid reads until mkfs.ext4
    writes hot-data. Returns the finished process, whether mkfs.ext4 ran, and
    whether the disk was mounted."""
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        calls = directory / "calls"
        calls.touch()
        (directory / "signatures").write_text(signatures)
        (directory / "label").write_text(label)
        (directory / "systemd/docker.service.d").mkdir(parents=True)
        disk = directory / "disk"
        disk.touch()
        stubs = {
            "findmnt": "echo /dev/boot-disk",
            "wipefs": f'cat {directory}/signatures\nexit {wipefs_exit}',
            "blkid": f'case "$*" in -p*) exit {probe_exit};; *LABEL*) cat {directory}/label;; *UUID*) echo disk-uuid;; esac',
            "mkfs.ext4": f'echo mkfs.ext4 >> {calls}\necho hot-data > {directory}/label',
            "systemctl": f'echo "systemctl $*" >> {calls}',
            "resize2fs": "exit 0",
            "install": "exit 0",
        }
        for name, body in stubs.items():
            stub = directory / name
            stub.write_text(f"#!/bin/sh\n{body}\n")
            stub.chmod(0o755)
        # The stub disk is a file, and the unit files stay in this directory.
        script = heredoc('"$LIBEXEC/hot-data-disk"').replace('[[ -b "$device" ]]', '[[ -e "$device" ]]') \
                                                    .replace("/etc/systemd/system", str(directory / "systemd"))
        result = subprocess.run(["bash", "-c", script, "hot-data-disk", str(disk), *flags], capture_output=True, text=True,
                                env={**os.environ, "PATH": f"{directory}:{os.environ['PATH']}"})
        log = calls.read_text()
        return result, "mkfs.ext4" in log, "enable --now var-lib-hot.mount" in log


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
        self.assertIn('echo "-I FORWARD 1 -i $bridge -j $chain"', SETUP)
        for destination in ("169.254.0.0/16", "127.0.0.0/8", "10.88.0.0/16",
                            "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"):
            self.assertIn(destination, SETUP)
        self.assertIn("net.bridge.bridge-nf-call-iptables = 1", SETUP)
        self.assertIn("br_netfilter", SETUP)
        self.assertIn('iptables -I INPUT 1 -i "$bridge" -j DROP', SETUP)

    # The same ranges, in the same place, as ALWAYS_BLOCKED in the task
    # worker's per-task chain (crates/hot_task_worker/src/guest_network.rs).
    ALWAYS_DROPPED = [f"-d {destination} -j DROP" for destination in (
        "0.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "10.88.0.0/16", "224.0.0.0/4", "240.0.0.0/4")]

    def test_guest_baseline_defaults(self):
        rules = guest_baseline()
        self.assertEqual(rules[:6], self.ALWAYS_DROPPED)
        self.assertIn("-d 1.1.1.1 -p udp --dport 53 -j RETURN", rules)
        self.assertLess(rules.index("-d 8.8.8.8 -p tcp --dport 53 -j RETURN"), rules.index("-d 10.0.0.0/8 -j DROP"))
        self.assertEqual(rules[-1], "-j RETURN")

    def test_resolvers_never_open_the_ranges_guests_can_never_reach(self):
        rules = guest_baseline("HOT_GUEST_DNS=169.254.169.254,10.68.0.2\nHOT_GUEST_DENY_CIDRS=10.68.0.0/16\n")
        self.assertEqual(rules[:6], self.ALWAYS_DROPPED)
        # A private resolver is still reachable inside the operator's deny
        # list, on port 53 only.
        resolver = rules.index("-d 10.68.0.2 -p udp --dport 53 -j RETURN")
        self.assertLess(resolver, rules.index("-d 10.68.0.0/16 -j DROP"))

    def test_guest_baseline_applies_the_operators_lists_in_order(self):
        rules = guest_baseline("HOT_GUEST_DENY_CIDRS=10.68.0.0/16\nHOT_GUEST_ALLOW_CIDRS=10.20.0.0/16\n")
        self.assertLess(rules.index("-d 10.68.0.0/16 -j DROP"), rules.index("-d 10.20.0.0/16 -j RETURN"))
        self.assertLess(rules.index("-d 10.20.0.0/16 -j RETURN"), rules.index("-d 10.0.0.0/8 -j DROP"))

    def test_an_egress_proxy_is_the_guests_only_way_out(self):
        # The proxy is private and inside the deny list: guests must still
        # reach it, on its port, and reach nothing else.
        rules = guest_baseline("HOT_GUEST_DNS=10.68.0.2\nHOT_GUEST_DENY_CIDRS=10.30.0.0/16\n"
                               "HOT_GUEST_ALLOW_CIDRS=10.20.0.0/16\n"
                               "HOT_GUEST_PROXY=10.30.0.7:3128,192.168.5.10:8080\n")
        self.assertEqual(rules, [*self.ALWAYS_DROPPED,
                                 "-d 10.68.0.2 -p udp --dport 53 -j RETURN",
                                 "-d 10.68.0.2 -p tcp --dport 53 -j RETURN",
                                 "-d 10.30.0.7 -p tcp --dport 3128 -j RETURN",
                                 "-d 192.168.5.10 -p tcp --dport 8080 -j RETURN",
                                 "-j DROP"])

    def test_a_malformed_egress_proxy_leaves_the_chain_alone(self):
        for entry in ("proxy.internal:3128", "10.30.0.7", "[fd00::1]:3128", "999.1.1.1:3128", "10.0.0.256:3128",
                      "010.0.0.5:3128", "10.0.0.5:70000", "10.0.0.5:0"):
            with self.subTest(entry), self.assertRaises(subprocess.CalledProcessError) as failure:
                guest_baseline(f"HOT_GUEST_PROXY={entry}\n")
            self.assertIn("HOT_GUEST_PROXY", failure.exception.stderr)
            self.assertEqual(failure.exception.stdout, "", "no iptables call")
        self.assertIn("-d 255.255.255.255 -p tcp --dport 65535 -j RETURN",
                      guest_baseline("HOT_GUEST_PROXY=255.255.255.255:65535\n"))

    def test_the_baseline_is_replaced_in_one_transaction(self):
        calls = firewall_calls()
        commit = calls.index("COMMIT")
        self.assertEqual(calls[:2], ["*mangle", ":HOT-GUEST-BASELINE - [0:0]"])
        self.assertEqual(calls[commit - 1], "-I FORWARD 1 -i kata-br0 -j HOT-GUEST-BASELINE")
        # Nothing changes the mangle table outside the commit, so nothing can
        # leave the chain empty or half built, or the jump to it missing.
        self.assertEqual([call for call in calls[commit + 1:] if "mangle" in call], [])
        self.assertTrue(all(line.startswith("-A HOT-GUEST-BASELINE ") for line in calls[2:commit - 1]), calls)

    def test_the_jump_moves_to_the_front_of_forward_in_the_same_transaction(self):
        jump = "FORWARD -i kata-br0 -j HOT-GUEST-BASELINE"
        calls = firewall_calls(forward=f"-P FORWARD ACCEPT\n-A FORWARD -j OTHER\n-A {jump}\n-A {jump}\n")
        self.assertEqual(calls[calls.index("COMMIT") - 3:calls.index("COMMIT")],
                         [f"-D {jump}", f"-D {jump}", "-I FORWARD 1 -i kata-br0 -j HOT-GUEST-BASELINE"])
        self.assertFalse([call for call in calls if "OTHER" in call])

    def test_a_rejected_transaction_stops_the_script_and_changes_nothing(self):
        # The stub exits 9, not 2, without --noflush, which would flush every
        # other chain in the table.
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            firewall_calls("HOT_GUEST_DENY_CIDRS=999.1.1.1/32\n", restore_exit=2)
        self.assertEqual(failure.exception.returncode, 2)
        self.assertEqual(failure.exception.stdout, "", "no other iptables call")

    def test_a_failed_read_of_forward_changes_nothing(self):
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            firewall_calls(list_exit=4)
        self.assertEqual(failure.exception.returncode, 4)
        self.assertEqual(failure.exception.stdout, "", "no iptables-restore or other iptables call")

    def test_nothing_feeding_iptables_restore_needs_the_xtables_lock(self):
        # iptables-legacy-restore holds the lock while it reads its input, so
        # an iptables call inside the pipeline deadlocks. This hung for real.
        script = heredoc('"$LIBEXEC/hot-guest-firewall"')
        self.assertIn("\ntransaction | iptables-restore --noflush\n", script)
        transaction = script.split("transaction() {\n")[1].split("\n}\n")[0]
        self.assertNotRegex(transaction, r"\biptables\b")

    def test_box_storage_waits_for_the_data_disk(self):
        unit = SETUP.split("kata-containerd.service <<'UNIT'")[1].split("UNIT")[0]
        self.assertIn("ConditionPathIsMountPoint=/var/lib/hot", unit)
        self.assertIn("refusing to initialize it without --allow-format", SETUP)
        # Restarted containers would otherwise start on the boot disk.
        self.assertIn("RequiresMountsFor=/var/lib/hot\\n' > /etc/systemd/system/docker.service.d/hot-data.conf", SETUP)
        self.assertIn("HOT_BOX_POOL_GB", SETUP)

    def test_a_blank_data_disk_is_formatted_only_when_allowed(self):
        result, formatted, mounted = data_disk("--allow-format")
        self.assertEqual((result.returncode, formatted, mounted), (0, True, True), result.stderr)
        result, formatted, mounted = data_disk()
        self.assertEqual((result.returncode, formatted, mounted), (1, False, False))
        self.assertIn("refusing to initialize it without --allow-format", result.stderr)

    def test_a_hot_data_disk_is_mounted_and_never_formatted(self):
        result, formatted, mounted = data_disk("--allow-format", signatures="disk 0x438 ext4 disk-uuid hot-data\n",
                                               probe_exit=0, label="hot-data")
        self.assertEqual((result.returncode, formatted, mounted), (0, False, True), result.stderr)

    def test_a_disk_with_conflicting_signatures_is_never_formatted(self):
        # The upgrade from v3.0.1 passes --allow-format. On a filesystem with a
        # stale signature blkid -p and plain blkid both report nothing, as they
        # do on a blank disk; only wipefs lists what is there.
        result, formatted, mounted = data_disk("--allow-format", signatures="disk 0x438 ext4 disk-uuid hot-data\n"
                                                                            "disk 0x8001 iso9660\n")
        self.assertEqual((result.returncode, formatted, mounted), (1, False, False))
        self.assertIn("is not a Hot data disk", result.stderr)

    def test_a_disk_wipefs_cannot_probe_is_never_formatted(self):
        # Nothing listed is not the same as nothing there.
        result, formatted, mounted = data_disk("--allow-format", wipefs_exit=1)
        self.assertEqual((result.returncode, formatted, mounted), (1, False, False))
        self.assertNotIn("is not a Hot data disk", result.stderr)

    def test_a_signature_only_blkid_finds_is_never_formatted(self):
        result, formatted, mounted = data_disk("--allow-format", probe_exit=0)
        self.assertEqual((result.returncode, formatted, mounted), (1, False, False))
        self.assertIn("is not a Hot data disk", result.stderr)

    def test_the_runtime_image_can_format_a_boxs_data_volume(self):
        # The task worker runs these tools in this image, for every container
        # task. The slim base image has all but mkfs.ext4; a tool from anywhere
        # else has to be in the final stage's install list, not the builder's.
        volume = (ROOT.parent / "crates/hot_task_worker/src/data_volume.rs").read_text()
        tools = set(re.findall(r'(?:killable_command|Command::new)\("([^"]+)"\)', volume))
        self.assertEqual(tools - {"fallocate", "truncate", "mount", "umount", "chown"}, {"mkfs.ext4"})
        final_stage = RUNTIME_IMAGE.rsplit("\nFROM ", 1)[1]
        install = re.search(r"apt-get install -y --no-install-recommends \\\n((?:\s+\S+ \\\n)+)", final_stage).group(1)
        self.assertIn("e2fsprogs", re.findall(r"(\S+) \\\n", install))

    def test_metadata_is_hardened_on_aws(self):
        self.assertIn('http_tokens                 = "required"', AWS)
        self.assertIn("http_put_response_hop_limit = 2", AWS)

    def test_aws_boot_script_fits_in_user_data(self):
        packed = subprocess.run(["gzip", "-9c"], input=SETUP.encode() + b"\n" * 8,
                                capture_output=True, check=True).stdout
        self.assertLess(len(packed), 15 * 1024)

    def test_task_worker_bind_sources_exist_after_boot(self):
        # The renderer binds these with create_host_path false; /run is tmpfs
        # and /tmp is emptied at boot, so a bake-time mkdir does not last.
        line = next(l for l in RENDER.splitlines() if '("/run/kata-containerd"' in l)
        sources = re.findall(r'\("(/[^"]+)", (?:True|False)\)', line)
        self.assertIn("/tmp/hot-box-io", sources)
        tmpfiles = [l.split()[1] for l in heredoc("/etc/tmpfiles.d/hot-host.conf").splitlines()
                    if l.startswith("d ")]
        for source in sources:
            if source == "/opt/cni/bin":
                self.assertIn("install -d -m 0755 /opt/cni/bin", SETUP)
                continue
            path = re.sub(r"^/var/run/", "/run/", source)
            self.assertTrue(any(p == path or p.startswith(path + "/") for p in tmpfiles), source)
        self.assertIn("systemd-tmpfiles --create /etc/tmpfiles.d/hot-host.conf", SETUP)
        self.assertNotRegex(SETUP, r"mkdir -p /run/")

    def test_largest_box_fits_the_kata_memory_ceiling(self):
        kata = tomllib.loads(heredoc("/opt/kata/share/defaults/kata-containers/configuration.toml")
                             .replace("${rootfs}", "/rootfs.img"))
        qemu = kata["hypervisor"]["qemu"]
        self.assertTrue(kata["runtime"]["static_sandbox_resource_mgmt"])
        limits = (ROOT.parent / "crates/hot_task_worker/src/box_limits.rs").read_text()
        largest = int(re.search(r"SYSTEM_MAX_MEMORY_MB: u64 = ([0-9_]+);", limits).group(1).replace("_", ""))
        # Static sizing starts each VM with default_memory plus the box's limit.
        self.assertGreaterEqual(qemu["default_maxmemory"], qemu["default_memory"] + largest)

    def test_docker_takes_no_address_range_of_its_own(self):
        daemon = json.loads(heredoc("/etc/docker/daemon.json"))
        self.assertEqual(daemon["bridge"], "none")
        runtime = re.search(r'^RUNTIME_SUBNET = "([^"]+)"', RENDER, re.M).group(1)
        preflight = (ROOT / "preflight/preflight.py").read_text()
        self.assertIn(f'RUNTIME_SUBNET = "{runtime}"', preflight)
        self.assertEqual(daemon["default-address-pools"], [{"base": runtime, "size": int(runtime.split("/")[1])}])

    def test_kata_containerd_runs_without_cri(self):
        config = tomllib.loads(heredoc("/etc/kata-containerd/config.toml")
                               .replace("${KATA_CONTAINERD_GID}", "2000"))
        self.assertIn("io.containerd.grpc.v1.cri", config["disabled_plugins"])
        self.assertEqual(list(config["plugins"]), ["io.containerd.snapshotter.v1.devmapper"])
        self.assertEqual(config["grpc"]["gid"], 2000)

    def test_firewall_restart_leaves_kata_containerd_running(self):
        unit = "/etc/systemd/system/kata-containerd.service"
        # Requires= would carry the agent's firewall restart to containerd.
        self.assertNotIn("hot-guest-firewall", " ".join(unit_lines(unit, "Requires") + unit_lines(unit, "BindsTo")))
        self.assertIn("hot-guest-firewall.service", unit_lines(unit, "Wants"))
        self.assertIn("hot-guest-firewall.service", " ".join(unit_lines(unit, "After")))
        checks = unit_lines(unit, "ExecStartPre")
        self.assertEqual(checks[:2], ["/usr/bin/systemctl is-active --quiet hot-guest-firewall.service",
                                      "/usr/sbin/iptables -t mangle -C FORWARD -i kata-br0 -j HOT-GUEST-BASELINE"])
        self.assertIn("-I FORWARD 1 -i kata-br0 -j HOT-GUEST-BASELINE", firewall_calls())

    def test_agent_starts_without_docker(self):
        unit = "/etc/systemd/system/hot-agent.service"
        self.assertEqual(unit_lines(unit, "Requires"), [])
        self.assertIn("docker.service", " ".join(unit_lines(unit, "Wants")))

    def test_pool_activation_does_not_leak_loop_devices(self):
        pool = heredoc('"$LIBEXEC/kata-devpool-activate"')
        attaches = re.findall(r"losetup [^\n)]*", pool)
        self.assertEqual(len(attaches), 3)
        for attach in attaches[:2]:
            self.assertIn("--nooverlap --find --show", attach)
        self.assertIn('losetup -d "$meta" "$data"', pool)

    def test_loop_devices_exist_before_docker_starts_a_container(self):
        # The task worker's /dev is the host's, so a device added later reaches it
        # too. These are a margin, there before Docker restarts the worker at boot.
        self.assertIn('task["volumes"].append(bind("/dev", "/dev"))', RENDER)
        unit = "/etc/systemd/system/hot-loop-devices.service"
        self.assertEqual(unit_lines(unit, "Before"), ["docker.service"])
        self.assertEqual(unit_lines(unit, "WantedBy"), ["docker.service"])
        self.assertEqual(unit_lines(unit, "Type"), ["oneshot"])
        self.assertEqual(unit_lines(unit, "RemainAfterExit"), ["yes"])
        self.assertEqual(unit_lines(unit, "ExecStart"), ["/usr/local/libexec/hot/hot-loop-devices"])
        self.assertRegex(SETUP, r"\nsystemctl enable [^\n]*hot-loop-devices\.service")
        self.assertIn("loop", heredoc("/etc/modules-load.d/hot-host.conf").splitlines())
        # The bake runs it too, after the helpers are executable, so an image
        # whose kernel refuses is never built.
        self.assertLess(SETUP.index('chmod 0755 "$LIBEXEC"/*'), SETUP.index('\n"$LIBEXEC/hot-loop-devices"\n'))

    def test_loop_devices_hold_the_pool_every_box_and_the_images_snaps(self):
        # Ubuntu's kernel starts with 8. A box holds one for its /data, and 8 more
        # leave room for the snaps in the cloud images.
        added = loop_devices_added(existing=range(8))
        self.assertEqual(added, list(range(8, 8 + len(added))))
        pool = heredoc('"$LIBEXEC/kata-devpool-activate"').count("losetup --nooverlap --find --show")
        boxes = max(int(slots) for slots in re.findall(r'"container": (\d+)', RENDER))
        self.assertEqual((pool, boxes), (2, 8))
        self.assertGreaterEqual(8 + len(added), pool + boxes + 8)

    def test_loop_device_setup_fails_when_a_device_is_not_there(self):
        # A device that already exists is fine, as on a kernel with more than 8.
        self.assertEqual(loop_devices_added(existing=range(64)), [])
        with self.assertRaises(PermissionError):
            loop_devices_added(refuse=errno.EPERM)
        with self.assertRaises(FileNotFoundError):
            loop_devices_added(missing={31})

    def test_the_data_disk_stays_shared_for_the_box_data_bind(self):
        # Docker refuses an rshared bind whose source is not on a shared mount, and
        # the worker's loop mounts have to reach Kata's shim on the host through it.
        self.assertIn('propagation="rshared"', RENDER)
        self.assertEqual(re.findall(r"(?:MountFlags|PrivateMounts)=|--make-r?(?:private|slave)", SETUP), [])
        self.assertIn("\nOptions=defaults,nofail\n", heredoc('"$LIBEXEC/hot-data-disk"'))

    def test_the_bake_refuses_a_docker_unit_with_private_mounts(self):
        self.assertEqual(docker_unit_check("[Service]\nExecStart=/usr/sbin/dockerd\nDelegate=yes\n"), 0)
        self.assertEqual(docker_unit_check("[Service]\nMountFlags=slave\n"), 3)
        self.assertEqual(docker_unit_check("[Service]\nPrivateMounts=yes\n"), 3)

    def test_the_releases_hotbox_directory_is_made_on_the_data_disk(self):
        # Each deploy installs the running release's hotbox here. Every path
        # that first mounts the disk runs hot-data-disk, which makes this after
        # the mount, so it lands on the disk and not under the mount point.
        disk = heredoc('"$LIBEXEC/hot-data-disk"')
        self.assertLess(disk.index("systemctl enable --now var-lib-hot.mount"),
                        disk.index("install -d -m 0755 /var/lib/hot/kata"))

    def test_the_baked_hotbox_is_a_fallback_from_the_runtime_image(self):
        # The task worker mounts this path when no deploy has installed one, and
        # the runtime image path is where the bake and each deploy copy it from.
        self.assertIn('install -m 0755 "$work/hotbox" /usr/local/bin/hotbox', SETUP)
        self.assertIn('docker cp "$container:/usr/share/hot/bin/hotbox-linux-x86_64" "$work/hotbox"', SETUP)
        self.assertIn(" /usr/share/hot/bin/hotbox-linux-x86_64\n", RUNTIME_IMAGE)
        self.assertIn('"hotbox_from": "${RUNTIME_IMAGE}"', heredoc('"$HOST_DIR/versions.json"'))

    def test_apt_waits_for_its_lock(self):
        calls = re.findall(r"^\s*(apt-get|apt_get) (\S+)", SETUP, re.M)
        self.assertTrue(calls)
        self.assertEqual([c for c in calls if c[0] == "apt-get"], [("apt-get", "-q")])
        self.assertIn("DPkg::Lock::Timeout=", SETUP)
        self.assertIn("--force-confold", SETUP)

    def test_image_keeps_no_build_state(self):
        # The GCP wrapper's gcloud runs after the bake; its state stays in tmpfs.
        self.assertIn('mount -t tmpfs -o mode=0700,size=16m tmpfs "$gcloud_config"', SETUP)
        self.assertIn("cat > /run/hot-setup-host.sh", BUILD)
        self.assertNotIn("/root/setup-host.sh", BUILD)

    def test_gcp_build_passes_validation_and_is_bounded(self):
        omit = re.search(r"^\s*omit_external_ip\s*=\s*(.+)$", PACKER, re.M).group(1)
        internal = re.search(r"^\s*use_internal_ip\s*=\s*([^#\n]+?)\s*(#.*)?$", PACKER, re.M).group(1)
        # The googlecompute plugin rejects omit_external_ip without use_internal_ip.
        self.assertEqual(internal, omit)
        self.assertIn("service_account_email = var.gcp_service_account_email", PACKER)
        self.assertNotIn("devstorage", PACKER)
        # Packer waits for the startup script's status without a limit.
        # TERM, because Packer's wrapper process ignores INT and only forwards TERM.
        self.assertRegex(BUILD, r'"\$time_limit" --foreground --signal=TERM --kill-after=\S+ "\$\{HOT_IMAGE_BUILD_TIMEOUT:-\d+m\}" \\\n\s+packer build ')
        account = re.search(r'variable "gcp_service_account_email" \{.*?\n\}', PACKER, re.S).group(0)
        for permission in ("compute.instances.get", "compute.instances.setMetadata",
                           "iam.serviceAccounts.actAs on itself"):
            self.assertIn(permission, account)

    def test_aws_image_key_requirements_are_stated(self):
        key = re.search(r'variable "aws_kms_key_id" \{.*?\n\}', PACKER, re.S).group(0)
        self.assertIn("AWSServiceRoleForAutoScaling", key)


if __name__ == "__main__":
    unittest.main()
