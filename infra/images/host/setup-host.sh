#!/usr/bin/env bash
# Bake a Hot self-host VM image: Ubuntu 24.04, amd64, on AWS or Google Cloud.
#
# Packer runs this without SSH, as EC2 user data or a Compute Engine startup
# script on a temporary build VM (see infra/images/packer). Everything an
# installation needs is installed here; at boot the image only activates
# services and never downloads or runs setup code.
#
#   setup-host.sh --runtime-image REF@sha256:DIGEST [--poweroff]
#
# --runtime-image supplies the deploy agent, its renderer, and the `hotbox` baked
# as a fallback. Each deploy installs the running release's own `hotbox`.
# --poweroff stops the VM when the bake succeeds, which is how the AWS build
# signals completion. A failed bake never powers off, so its build times out
# instead of producing an image.
set -euo pipefail

KATA_VERSION="3.28.0"
KATA_SHA256="99cefb46d70bc27b7bcffd7595be9010c6bed43e1cdfcf8078554c19e7c9b19d"
CONTAINERD_VERSION="1.7.30"
CONTAINERD_SHA256="ca0f27e34411504acd1cd24fcbaa71b9c47d31ce9408c47c54a2dc1810ceb1df"
CNI_VERSION="1.9.1"
CNI_SHA256="b98f74a0f8522f0a83867178729c1aa70f2158f90c45a2ca8fa791db1c76b303"
COSIGN_VERSION="v3.1.4"
COSIGN_SHA256="424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa"
KATA_CONTAINERD_GID="2000"
HOST_DIR="/opt/hot-host"
LIBEXEC="/usr/local/libexec/hot"

RUNTIME_IMAGE=""
POWEROFF=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runtime-image) RUNTIME_IMAGE="$2"; shift 2 ;;
        --poweroff) POWEROFF=true; shift ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

log() { echo "[hot-host] $*"; }
fail() { echo "[hot-host] ERROR: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || fail "run as root"
[[ "$(uname -m)" == "x86_64" ]] || fail "only amd64 hosts are supported"
# shellcheck source=/dev/null
. /etc/os-release
[[ "$ID" == "ubuntu" && "$VERSION_ID" == "24.04" ]] || fail "Ubuntu 24.04 is required"
[[ "$RUNTIME_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || fail "--runtime-image must be pinned by digest"

# Download to a file and check its pinned SHA-256 before anything uses it.
fetch() {
    local url="$1" sha="$2" dest="$3"
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$dest" "$url"
    echo "${sha}  ${dest}" | sha256sum --check --quiet -
}

log "Installing packages"
export DEBIAN_FRONTEND=noninteractive
# A fresh cloud image may start apt's own timers at first boot. Pause them for
# the bake (they stay enabled in the image), let a run already under way
# finish, and wait for dpkg's lock rather than failing on it. An upgrade keeps
# the image's configuration files instead of prompting on a closed stdin.
systemctl stop apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
for _ in $(seq 1 120); do
    [[ "$(systemctl show -p ActiveState --value apt-daily.service apt-daily-upgrade.service)" == *activating* ]] || break
    sleep 5
done
apt_get() {
    apt-get -q -o DPkg::Lock::Timeout=600 \
        -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold "$@"
}
apt_get update
apt_get install -y --no-install-recommends \
    ca-certificates curl jq python3 zstd e2fsprogs dmsetup thin-provisioning-tools \
    iproute2 iptables docker.io docker-compose-v2
apt_get upgrade -y

# ------------------------------------------------------------------
# No SSH, on either cloud. Operators reach the host through the agent's
# command channel only.
# ------------------------------------------------------------------
log "Disabling SSH"
systemctl disable --now ssh.socket ssh.service 2>/dev/null || true
systemctl mask ssh.socket ssh.service
rm -f /etc/ssh/ssh_host_*

log "Configuring Docker"
# Hot's containers use only the bundle's network, on the fixed runtime subnet
# that preflight checks. No default bridge (docker0 would take 172.17.0.0/16
# and shadow any installation range there), and no other range Docker could
# pick on its own.
install -d -m 0755 /etc/docker
cat > /etc/docker/daemon.json <<'JSON'
{
  "log-driver": "local",
  "log-opts": {"max-size": "10m", "max-file": "3"},
  "bridge": "none",
  "default-address-pools": [{"base": "172.30.0.0/24", "size": 24}]
}
JSON
systemctl enable docker
# The task worker's bind of each box's /data is rshared, so Docker has to stay in
# the host's mount namespace, where /var/lib/hot is a shared mount. Docker refuses
# that bind otherwise, so stop here on a package whose unit changes it.
if grep -Eq '^(MountFlags|PrivateMounts)=' <<<"$(systemctl cat docker.service)"; then
    fail "docker.service must not set MountFlags or PrivateMounts"
fi

# ------------------------------------------------------------------
# Kata Containers (QEMU only), its own containerd, CNI plugins, cosign
# ------------------------------------------------------------------
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

log "Installing Kata Containers ${KATA_VERSION}"
fetch "https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/kata-static-${KATA_VERSION}-amd64.tar.zst" \
    "$KATA_SHA256" "$work/kata.tar.zst"
tar --zstd -xf "$work/kata.tar.zst" -C /
ln -sf /opt/kata/bin/containerd-shim-kata-v2 /usr/local/bin/containerd-shim-kata-v2
# QEMU looks for its ROMs in share/qemu.
ln -sfn /opt/kata/share/kata-qemu/qemu /opt/kata/share/qemu

log "Installing containerd ${CONTAINERD_VERSION} for Kata"
fetch "https://github.com/containerd/containerd/releases/download/v${CONTAINERD_VERSION}/containerd-${CONTAINERD_VERSION}-linux-amd64.tar.gz" \
    "$CONTAINERD_SHA256" "$work/containerd.tar.gz"
tar -xzf "$work/containerd.tar.gz" -C "$work" bin/containerd bin/ctr
install -m 0755 "$work/bin/containerd" /usr/local/bin/kata-containerd
install -m 0755 "$work/bin/ctr" /usr/local/bin/ctr

log "Installing CNI plugins ${CNI_VERSION}"
fetch "https://github.com/containernetworking/plugins/releases/download/v${CNI_VERSION}/cni-plugins-linux-amd64-v${CNI_VERSION}.tgz" \
    "$CNI_SHA256" "$work/cni.tgz"
install -d -m 0755 /opt/cni/bin
tar -xzf "$work/cni.tgz" -C /opt/cni/bin

log "Installing cosign ${COSIGN_VERSION} for release-manifest verification"
fetch "https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/cosign-linux-amd64" \
    "$COSIGN_SHA256" "$work/cosign"
install -m 0755 "$work/cosign" /usr/local/bin/cosign

# ------------------------------------------------------------------
# Guest image: Kata's Ubuntu rootfs, and a fallback hotbox
# ------------------------------------------------------------------
log "Building the guest image and the fallback hotbox from ${RUNTIME_IMAGE}"
# Restart, so the bake fails here if Docker rejects the configuration above.
systemctl restart docker
docker pull -q "$RUNTIME_IMAGE"
container=$(docker create "$RUNTIME_IMAGE")
docker cp "$container:/usr/share/hot/bin/hotbox-linux-x86_64" "$work/hotbox"
# The deploy agent and the bundle renderer it uses come from the same image.
docker cp "$container:/usr/share/hot/selfhost/." "$work/selfhost"
docker rm "$container" >/dev/null
docker image rm "$RUNTIME_IMAGE" >/dev/null
# Only a fallback: each deploy installs the running release's hotbox at
# /var/lib/hot/kata/hotbox, and the task worker has Kata's shim bind-mount that
# into each guest. It mounts this copy only when that file is absent.
install -m 0755 "$work/hotbox" /usr/local/bin/hotbox

install -d -m 0755 "$HOST_DIR/kata"
rootfs="$HOST_DIR/kata/hot-rootfs.img"
cp "$(readlink -f /opt/kata/share/kata-containers/kata-ubuntu-noble.image)" "$rootfs"
loop=$(losetup --find --show --partscan "$rootfs")
mnt="$work/rootfs"
mkdir -p "$mnt"
udevadm settle
if [[ -b "${loop}p1" ]]; then mount "${loop}p1" "$mnt"; else mount "$loop" "$mnt"; fi
# Boxes never run this copy: a box's container has its own root, and the
# bind-mounted hotbox.
install -D -m 0755 "$work/hotbox" "$mnt/usr/local/bin/hotbox"
umount "$mnt"
losetup -d "$loop"
sha256sum "$rootfs" | cut -d' ' -f1 > "$rootfs.sha256"

log "Installing the deploy agent"
install -d -m 0755 "$HOST_DIR/agent" "$HOST_DIR/runtime"
install -m 0644 "$work/selfhost/agent/hot_agent.py" "$HOST_DIR/agent/hot_agent.py"
install -m 0644 "$work/selfhost/runtime/render.py" "$work/selfhost/runtime/provision_runtime.sql" "$HOST_DIR/runtime/"
# SSM Run Command documents and operators invoke `hot-agent command ...`.
cat > /usr/local/bin/hot-agent <<'AGENT'
#!/bin/sh
exec /usr/bin/python3 -I /opt/hot-host/agent/hot_agent.py "$@"
AGENT
chmod 0755 /usr/local/bin/hot-agent

log "Writing Kata and containerd configuration"
install -d -m 0755 /opt/kata/share/defaults/kata-containers
cat > /opt/kata/share/defaults/kata-containers/configuration.toml <<KATA
[hypervisor.qemu]
path = "/opt/kata/bin/qemu-system-x86_64"
kernel = "/opt/kata/share/kata-containers/vmlinux.container"
image = "${rootfs}"
rootfs_type = "ext4"
machine_type = "q35"
valid_hypervisor_paths = ["/opt/kata/bin/qemu-system-x86_64"]
kernel_params = "agent.log_vport=1025"
enable_annotations = ["enable_iommu", "kernel_params", "default_vcpus", "default_memory"]
default_vcpus = 1
default_maxvcpus = 0
default_memory = 512
# Static sizing adds each box's memory limit to default_memory, so this must
# hold the largest box (SYSTEM_MAX_MEMORY_MB, 8192, in the task worker's
# box_limits.rs) on top of it. Kata lowers it to the host's RAM, and the task
# worker's budget keeps boxes within that.
default_maxmemory = 9216
memory_slots = 10
use_vsock = true
disable_seccomp = false
enable_mem_prealloc = false
enable_hugepages = false
enable_iommu = false
enable_debug = false
disable_nesting_checks = true
block_device_driver = "virtio-scsi"
block_device_cache_set = true
block_device_cache_direct = true
entropy_source = "/dev/urandom"
valid_entropy_sources = ["/dev/urandom", "/dev/random", ""]
shared_fs = "virtio-fs"
virtio_fs_daemon = "/opt/kata/libexec/virtiofsd"
disable_selinux = false
firmware = ""
firmware_volume = ""

[agent.kata]
enable_debug = false
enable_tracing = false
kernel_modules = []
debug_console_enabled = false
dial_timeout = 30

[runtime]
create_container_timeout = 60
enable_debug = false
internetworking_model = "macvtap"
disable_guest_seccomp = true
enable_tracing = false
disable_new_netns = false
sandbox_cgroup_only = true
static_sandbox_resource_mgmt = true
disable_guest_empty_dir = false
enable_pprof = false
KATA

if ! getent group kata-containerd >/dev/null; then
    groupadd --gid "$KATA_CONTAINERD_GID" kata-containerd
fi
[[ "$(getent group kata-containerd | cut -d: -f3)" == "$KATA_CONTAINERD_GID" ]] ||
    fail "kata-containerd must use GID $KATA_CONTAINERD_GID"

install -d -m 0755 /etc/kata-containerd
cat > /etc/kata-containerd/config.toml <<CONTAINERD
version = 2
root = "/var/lib/kata/containerd"
state = "/run/kata-containerd"
# The task worker uses containerd's own API and names the Kata runtime for
# each box. The Kubernetes CRI plugin would only add a streaming server and
# CNI polling behind the task worker's socket.
disabled_plugins = ["io.containerd.grpc.v1.cri"]

[grpc]
  address = "/run/kata-containerd/containerd.sock"
  gid = ${KATA_CONTAINERD_GID}

[plugins]
  [plugins."io.containerd.snapshotter.v1.devmapper"]
    root_path = "/var/lib/kata/devmapper"
    pool_name = "hot-devpool"
    base_image_size = "30GB"
    discard_blocks = true
    fs_type = "ext4"
CONTAINERD

log "Persisting kernel modules and sysctls"
cat > /etc/modules-load.d/hot-host.conf <<'MODULES'
vhost_vsock
vmw_vsock_virtio_transport
vsock_loopback
br_netfilter
loop
MODULES
# Bridged guest traffic must pass through iptables, or per-task policy is
# bypassed for guest-to-guest frames.
cat > /etc/sysctl.d/90-hot-host.conf <<'SYSCTL'
net.bridge.bridge-nf-call-iptables = 1
net.bridge.bridge-nf-call-ip6tables = 1
SYSCTL

# ------------------------------------------------------------------
# Host helpers
# ------------------------------------------------------------------
install -d -m 0755 "$LIBEXEC" /etc/hot

# Mount the retained data disk. Refuses to format unless told to, so an
# absent or replaced disk never silently becomes an empty installation.
cat > "$LIBEXEC/hot-data-disk" <<'DISK'
#!/usr/bin/env bash
# hot-data-disk DEVICE [--allow-format]
set -euo pipefail
device="${1:?usage: hot-data-disk DEVICE|auto [--allow-format]}"
allow_format="${2:-}"
if [[ "$device" == "auto" ]]; then
    # The one attached disk that is not the boot disk.
    root_disk="/dev/$(lsblk -no PKNAME "$(findmnt -n -o SOURCE /)")"
    device=$(lsblk -dpno NAME,TYPE | awk -v root="$root_disk" '$2 == "disk" && $1 != root {print $1; exit}')
    [[ -n "$device" ]] || { echo "No data disk is attached" >&2; exit 1; }
fi
[[ -b "$device" ]] || { echo "Data disk $device is not attached" >&2; exit 1; }
real=$(readlink -f "$device")
root_source=$(findmnt -n -o SOURCE /)
if [[ "$root_source" == "$real"* ]]; then
    echo "Refusing to use the boot disk $real as the data disk" >&2
    exit 1
fi
# Only a disk with nothing on it is blank. blkid -p alone cannot say: it also
# finds nothing on a disk whose signatures conflict, such as a filesystem with a
# stale signature from earlier use, and formatting that destroys the filesystem.
# The separate assignment lets set -e stop on a disk wipefs cannot probe.
signatures=$(wipefs --noheadings "$real")
if [[ -z "$signatures" ]] && ! blkid -p "$real" >/dev/null 2>&1; then
    if [[ "$allow_format" != "--allow-format" ]]; then
        echo "Data disk $real has no filesystem; refusing to initialize it without --allow-format" >&2
        exit 1
    fi
    mkfs.ext4 -q -L hot-data "$real"
fi
[[ "$(blkid -s LABEL -o value "$real")" == "hot-data" ]] ||
    { echo "Data disk $real is not a Hot data disk (label hot-data)" >&2; exit 1; }
uuid=$(blkid -s UUID -o value "$real")
cat > /etc/systemd/system/var-lib-hot.mount <<UNIT
[Unit]
Description=Hot data disk

[Mount]
What=/dev/disk/by-uuid/${uuid}
Where=/var/lib/hot
Type=ext4
Options=defaults,nofail

[Install]
WantedBy=local-fs.target
UNIT
# Containers bind-mount the data disk and restart with Docker, so Docker must
# never start them before the disk is mounted (the mount is nofail).
install -d /etc/systemd/system/docker.service.d
printf '[Unit]\nRequiresMountsFor=/var/lib/hot\n' > /etc/systemd/system/docker.service.d/hot-data.conf
systemctl daemon-reload
systemctl enable --now var-lib-hot.mount
# A disk enlarged for a bigger size grows its filesystem, online; an
# unchanged one is left as it is.
resize2fs "$real"
# Kata's state, and the running release's hotbox, which the agent installs here.
install -d -m 0755 /var/lib/hot/kata
DISK

# Size the box storage pool from /etc/hot/host.env (written by the agent from
# the installation's size). That caps what box tasks can store. By default the
# pool's data file is sparse and takes its space from the data disk as boxes
# write, so on a disk already holding other data a box can still fill it. Where
# local PostgreSQL or Valkey shares the disk, the agent sets
# HOT_BOX_POOL_PREALLOCATE=1, and the data file takes its full size when the
# pool starts. It does not keep all of it: when a box first writes part of a
# pool block, dm-thin zeroes the block with a write-zeroes request, and the
# loop device turns that into a hole in the file, handing that space back to
# the disk until a later write takes it again. So hot-box-pool-hold.timer runs
# this with --hold every minute and fills the holes again, as long as the disk
# still has room for them. That leaves the disk exposed to what boxes punch
# within a minute, and, once other data has taken part of the holes' space, to
# all of them, which it then reports. The metadata file stays sparse; the pool uses little
# of it.
cat > "$LIBEXEC/kata-devpool-activate" <<'POOL'
#!/usr/bin/env bash
# kata-devpool-activate [--hold]
set -euo pipefail
pool="hot-devpool"
dir="/var/lib/kata/devmapper"
mode="${1:-}"
[[ -z "$mode" || "$mode" == --hold ]] || { echo "usage: kata-devpool-activate [--hold]" >&2; exit 2; }
mountpoint -q /var/lib/kata || { echo "/var/lib/kata is not mounted" >&2; exit 1; }
if [[ "$mode" == --hold ]]; then
    # Only a running pool that started held: one that started sparse still
    # passes discards down, which punch holes again (the agent warns of it).
    table=$(dmsetup table "$pool" 2>/dev/null) || exit 0
    [[ " $table " == *" no_discard_passdown "* ]] || exit 0
else
    dmsetup info "$pool" >/dev/null 2>&1 && exit 0
fi
HOT_BOX_POOL_GB=40
HOT_BOX_POOL_PREALLOCATE=0
# shellcheck source=/dev/null
[[ -f /etc/hot/host.env ]] && . /etc/hot/host.env
[[ "$HOT_BOX_POOL_GB" =~ ^[0-9]+$ && "$HOT_BOX_POOL_GB" -ge 10 ]] ||
    { echo "HOT_BOX_POOL_GB must be at least 10" >&2; exit 1; }
[[ "$HOT_BOX_POOL_PREALLOCATE" == 0 || "$HOT_BOX_POOL_PREALLOCATE" == 1 ]] ||
    { echo "HOT_BOX_POOL_PREALLOCATE must be 0 or 1" >&2; exit 1; }
if [[ "$mode" == --hold ]]; then
    [[ "$HOT_BOX_POOL_PREALLOCATE" == 1 && -f "$dir/data" ]] || exit 0
fi
features=""
if [[ "$HOT_BOX_POOL_PREALLOCATE" == 1 ]]; then
    # A pool from before keeps its size, and only its holes still need space.
    if [[ -f "$dir/data" ]]; then
        created=0
        size=$(stat -c %s "$dir/data")
        held=$(( $(stat -c '%b * %B' "$dir/data") ))
    else
        created=1
        size=$(( HOT_BOX_POOL_GB * 1024 * 1024 * 1024 ))
        held=0
    fi
    needed=$(( size > held ? size - held : 0 ))
    if (( needed > 0 )); then
        # What a non-root writer such as PostgreSQL can still use must cover
        # what the data file still needs and a margin. fallocate that runs out
        # of space fails but keeps what it took, which can leave the disk with
        # nothing free, so a disk without room is refused here, before the data
        # file is written. A new data file, or one that never held its space,
        # leaves 1 GB. One that held it before ("held" marks it, from its first
        # full allocation until it starts sparse) has only the holes boxes
        # punched in it to take back, which leaves the disk with what it had
        # free before them, so the hold, and activation after a reboot or on a
        # replaced VM, leave only 64 MB for the extent blocks fallocate may add:
        # a larger margin would give up the pool's space in just the near-full
        # case where holding it matters, and box tasks with it.
        # kata-containerd retries this every few seconds, and a refusal only
        # reads. "<3>" logs a refusal at err priority (systemd's
        # SyslogLevelPrefix), so the agent's host journal, which ships warning
        # and above, carries why box tasks do not run, or why the running
        # pool's space is not held.
        margin=$(( 1024 * 1024 * 1024 ))
        if [[ "$mode" == --hold || ( "$created" == 0 && -f "$dir/held" ) ]]; then
            margin=$(( 64 * 1024 * 1024 ))
        fi
        avail=$(df -B1 --output=avail /var/lib/kata | tail -n 1)
        [[ "$avail" =~ ^\ *[0-9]+$ ]] || { echo "Cannot read the data disk's free space" >&2; exit 1; }
        if (( needed + margin > avail )); then
            if [[ "$mode" == --hold ]]; then
                echo "<3>The box storage pool has $(( needed / 1024 / 1024 )) MB of its data file no longer held," \
                     "and only $(( avail / 1024 / 1024 )) MB of the data disk is free; box tasks can fill the disk" \
                     "under the local database until space is freed or the disk enlarged" >&2
            else
                echo "<3>The box storage pool needs $(( needed / 1024 / 1024 )) MB more of the data disk and" \
                     "$(( margin / 1024 / 1024 )) MB to spare, and only $(( avail / 1024 / 1024 )) MB is free; box" \
                     "tasks do not run on this host, though code tasks do, until the disk is enlarged" \
                     "(a larger size, apply, then deploy-now)" >&2
            fi
            exit 1
        fi
        install -d -m 0700 "$dir"
        # Over a running pool's file this only turns holes into unwritten
        # extents, which read as zeros, as the holes did, so no data changes.
        if ! fallocate -l "$size" "$dir/data"; then
            if (( created )); then rm -f "$dir/data"; fi
            echo "<3>Could not allocate the box storage pool" >&2
            exit 1
        fi
    fi
    [[ -f "$dir/held" ]] || : > "$dir/held"
    [[ "$mode" == --hold ]] && exit 0
    # Discards still free the pool's blocks for reuse, but no longer punch
    # holes in the data file. Block zeroing stays on: without it a box could
    # read a block another box freed.
    features=" 1 no_discard_passdown"
else
    install -d -m 0700 "$dir"
    # Sparse: the pool can never use more than its configured size. Its
    # discards punch holes in the data file again, so it no longer counts as
    # held.
    rm -f "$dir/held"
    [[ -f "$dir/data" ]] || truncate -s "${HOT_BOX_POOL_GB}G" "$dir/data"
fi
[[ -f "$dir/meta" ]] || truncate -s "$(( HOT_BOX_POOL_GB / 10 + 1 ))G" "$dir/meta"
# kata-containerd retries this every few seconds while the pool fails to
# activate, so reuse the loop devices already on these files and release them
# on failure rather than attaching new ones each time.
data=$(losetup --nooverlap --find --show "$dir/data")
meta=$(losetup --nooverlap --find --show "$dir/meta")
sectors=$(( $(blockdev --getsize64 "$data") / 512 ))
dmsetup create "$pool" --table "0 $sectors thin-pool $meta $data 128 32768$features" ||
    { losetup -d "$meta" "$data" || true; exit 1; }
POOL

cat > "$LIBEXEC/kata-containerd-socket-permissions" <<SOCKET
#!/usr/bin/env bash
set -euo pipefail
for _ in \$(seq 1 50); do
    if [[ -S /run/kata-containerd/containerd.sock ]]; then
        chown root:kata-containerd /run/kata-containerd/containerd.sock
        chmod 0660 /run/kata-containerd/containerd.sock
        exit 0
    fi
    sleep 0.1
done
echo "Kata containerd socket was not created" >&2
exit 1
SOCKET

# Each box's /data is a loop mount made inside the task worker's container. The
# renderer binds the host's /dev there, so a device the kernel adds on demand
# works too, and this is a margin rather than what makes the mount possible.
# Ubuntu's generic kernel builds the driver in with 8 devices, which a modprobe
# option cannot change, and the image's snaps use some of them, so without this a
# busy host's boxes would depend on the kernel adding a device as they start. Here
# the devices are there first, and the bake stops on a kernel that refuses them.
# This asks the driver itself, which works whether it is built in or a module, and
# hot-loop-devices.service runs it before Docker. 32 holds the pool's 2, the 8 box
# slots of the largest size, and the snaps.
cat > "$LIBEXEC/hot-loop-devices" <<'LOOPS'
#!/usr/bin/python3 -I
import errno
import fcntl
import os
import stat

COUNT = 32
LOOP_CTL_ADD = 0x4C80  # linux/loop.h

control = os.open("/dev/loop-control", os.O_RDWR | os.O_CLOEXEC)
for number in range(COUNT):
    try:
        fcntl.ioctl(control, LOOP_CTL_ADD, number)
    except OSError as error:
        if error.errno != errno.EEXIST:
            raise
os.close(control)
for number in range(COUNT):
    if not stat.S_ISBLK(os.stat(f"/dev/loop{number}").st_mode):
        raise SystemExit(f"/dev/loop{number} is not a block device")
LOOPS

# Host baseline for box guests, in the mangle table so it applies before the
# task worker's per-task filter rules and nothing they insert can bypass it.
# /etc/hot/guest-network.env (written by the agent) may set
# HOT_GUEST_ALLOW_CIDRS, HOT_GUEST_DENY_CIDRS, HOT_GUEST_DNS, and
# HOT_GUEST_PROXY (the egress proxy, as comma-separated IPv4:port).
cat > "$LIBEXEC/hot-guest-firewall" <<'FIREWALL'
#!/usr/bin/env bash
set -euo pipefail
bridge="kata-br0"
chain="HOT-GUEST-BASELINE"
HOT_GUEST_ALLOW_CIDRS=""
HOT_GUEST_DENY_CIDRS=""
HOT_GUEST_DNS="1.1.1.1,8.8.8.8"
HOT_GUEST_PROXY=""
# shellcheck source=/dev/null
[[ -f /etc/hot/guest-network.env ]] && . /etc/hot/guest-network.env
entries() { tr ',' '\n' <<<"$1" | tr -d ' ' | grep -v '^$' || true; }
split() { entries "$1" | grep -v ':' || true; }

rule() { echo "-A $chain $*"; }
valid_proxy() {
    local octet='(0|[1-9][0-9]{0,2})'
    [[ "$1" =~ ^$octet\.$octet\.$octet\.$octet:([1-9][0-9]{0,4})$ ]] &&
        ((BASH_REMATCH[1] <= 255 && BASH_REMATCH[2] <= 255 && BASH_REMATCH[3] <= 255 &&
            BASH_REMATCH[4] <= 255 && BASH_REMATCH[5] <= 65535))
}

for proxy in $(entries "$HOT_GUEST_PROXY"); do
    if ! valid_proxy "$proxy"; then
        echo "HOT_GUEST_PROXY entry $proxy is not IPv4:port" >&2
        exit 1
    fi
done

# Everything in the mangle table is one transaction: a value iptables rejects
# changes nothing, and running boxes never see the chain empty or half built,
# or the jump to it missing. Naming the chain empties it, in the same commit.
# The first attempt on a host commits nothing if it fails, so the check in
# kata-containerd.service keeps containerd down. FORWARD is read up front:
# iptables-legacy-restore holds the xtables lock while it reads its input, so
# an iptables call feeding it would wait forever.
forward=$(iptables -t mangle -S FORWARD)
transaction() {
    echo '*mangle'
    echo ":$chain - [0:0]"
    # Never reachable: the host, every metadata service, other guests. These
    # come first, so no resolver or other setting below can open them.
    for destination in 0.0.0.0/8 127.0.0.0/8 169.254.0.0/16 10.88.0.0/16 224.0.0.0/4 240.0.0.0/4; do
        rule -d "$destination" -j DROP
    done
    # A private resolver is reachable on port 53 only, even inside the deny list.
    for resolver in $(split "$HOT_GUEST_DNS"); do
        for protocol in udp tcp; do
            rule -d "$resolver" -p "$protocol" --dport 53 -j RETURN
        done
    done
    if [[ -n "$HOT_GUEST_PROXY" ]]; then
        # Guests reach the internet only through the operator's proxy, which may
        # be on a private address. The per-task rules say the same.
        for proxy in $(entries "$HOT_GUEST_PROXY"); do
            rule -d "${proxy%:*}" -p tcp --dport "${proxy##*:}" -j RETURN
        done
        rule -j DROP
    else
        # The installation's own networks, the operator's allowlist, and then
        # whatever else is private.
        for destination in $(split "$HOT_GUEST_DENY_CIDRS"); do
            rule -d "$destination" -j DROP
        done
        for destination in $(split "$HOT_GUEST_ALLOW_CIDRS"); do
            rule -d "$destination" -j RETURN
        done
        for destination in 10.0.0.0/8 100.64.0.0/10 172.16.0.0/12 192.168.0.0/16; do
            rule -d "$destination" -j DROP
        done
        rule -j RETURN
    fi
    # The jump goes first in FORWARD, wherever an earlier run left it.
    sed -n "s/^-A \(FORWARD -i $bridge -j $chain\)\$/-D \1/p" <<<"$forward"
    echo "-I FORWARD 1 -i $bridge -j $chain"
    echo COMMIT
}
transaction | iptables-restore --noflush

# Guests never address the host, and guests have no IPv6.
iptables -C INPUT -i "$bridge" -j DROP 2>/dev/null || iptables -I INPUT 1 -i "$bridge" -j DROP
for direction in "FORWARD -i" "FORWARD -o" "INPUT -i"; do
    # shellcheck disable=SC2086
    ip6tables -C $direction "$bridge" -j DROP 2>/dev/null || ip6tables -I $direction "$bridge" -j DROP
done
FIREWALL
chmod 0755 "$LIBEXEC"/*
# Once now, so the bake fails here if this kernel refuses.
"$LIBEXEC/hot-loop-devices"

# ------------------------------------------------------------------
# systemd units
# ------------------------------------------------------------------
log "Installing systemd units"
cat > /etc/systemd/system/var-lib-kata.mount <<'UNIT'
[Unit]
Description=Kata state on the Hot data disk
RequiresMountsFor=/var/lib/hot
ConditionPathIsMountPoint=/var/lib/hot

[Mount]
What=/var/lib/hot/kata
Where=/var/lib/kata
Type=none
Options=bind
UNIT

# WantedBy, so if this fails Docker and everything else still start, and a box
# past the kernel's own devices relies on the kernel adding one.
cat > /etc/systemd/system/hot-loop-devices.service <<'UNIT'
[Unit]
Description=Loop devices for box data volumes
Before=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/libexec/hot/hot-loop-devices

[Install]
WantedBy=docker.service
UNIT

cat > /etc/systemd/system/hot-guest-firewall.service <<'UNIT'
[Unit]
Description=Hot box guest network baseline
After=docker.service network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/libexec/hot/hot-guest-firewall

[Install]
WantedBy=multi-user.target
UNIT

# Starts only once the data disk is mounted; the agent starts it after
# mounting on first boot, and it starts on its own on later boots.
# The firewall is Wants=, not Requires=: the agent restarts it before each
# deploy to apply new settings, and Requires= would restart containerd with
# it, which ends every running box. The first two ExecStartPre lines still
# keep containerd down until the guest baseline is in place.
cat > /etc/systemd/system/kata-containerd.service <<'UNIT'
[Unit]
Description=containerd for Kata Containers
ConditionPathIsMountPoint=/var/lib/hot
Requires=var-lib-kata.mount
Wants=hot-guest-firewall.service
After=var-lib-kata.mount hot-guest-firewall.service network.target

[Service]
ExecStartPre=/usr/bin/systemctl is-active --quiet hot-guest-firewall.service
ExecStartPre=/usr/sbin/iptables -t mangle -C FORWARD -i kata-br0 -j HOT-GUEST-BASELINE
ExecStartPre=/usr/local/libexec/hot/kata-devpool-activate
ExecStart=/usr/local/bin/kata-containerd --config /etc/kata-containerd/config.toml
ExecStartPost=/usr/local/libexec/hot/kata-containerd-socket-permissions
Restart=always
RestartSec=5
KillMode=process
OOMScoreAdjust=-999
LimitNOFILE=1048576
LimitNPROC=infinity
LimitCORE=infinity

[Install]
WantedBy=multi-user.target
UNIT

# Fills the holes block zeroing punches in a held pool's data file (see
# kata-devpool-activate). A pool that does not hold its space is left alone.
cat > /etc/systemd/system/hot-box-pool-hold.service <<'UNIT'
[Unit]
Description=Hold the box storage pool's space on the data disk
ConditionPathIsMountPoint=/var/lib/kata

[Service]
Type=oneshot
ExecStart=/usr/local/libexec/hot/kata-devpool-activate --hold
Nice=10
UNIT

cat > /etc/systemd/system/hot-box-pool-hold.timer <<'UNIT'
[Unit]
Description=Hold the box storage pool's space every minute

[Timer]
OnBootSec=1min
OnUnitActiveSec=1min
AccuracySec=5s

[Install]
WantedBy=timers.target
UNIT

# Enabled at boot by the installation's bootstrap, which first writes
# /etc/hot/installation.json. Docker is Wants=, not Requires=: Docker waits
# for the data disk, and the agent must still run, and report, when the disk
# fails to mount or Docker restarts.
cat > /etc/systemd/system/hot-agent.service <<'UNIT'
[Unit]
Description=Hot deploy agent
After=docker.service network-online.target
Wants=docker.service network-online.target
ConditionPathExists=/etc/hot/installation.json

[Service]
ExecStart=/usr/local/bin/hot-agent run
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable hot-loop-devices.service hot-guest-firewall.service kata-containerd.service \
    hot-box-pool-hold.timer

# The task worker bind-mounts these, and Docker does not create a missing
# bind source. /run is tmpfs and /tmp is emptied at boot, so they are made on
# every boot, before Docker starts and restarts the task worker.
cat > /etc/tmpfiles.d/hot-host.conf <<'TMPFILES'
d /run/kata-containerd 0711 root root -
d /run/vc 0750 root root -
d /run/netns 0755 root root -
d /tmp/hot-box-io 0700 root root -
d /var/lib/cni/networks 0755 root root -
TMPFILES
systemd-tmpfiles --create /etc/tmpfiles.d/hot-host.conf

# ------------------------------------------------------------------
# Record what was baked, then reset per-instance state
# ------------------------------------------------------------------
# hotbox_from is where the fallback came from, not the hotbox a deploy installs.
cat > "$HOST_DIR/versions.json" <<JSON
{
  "kata": "${KATA_VERSION}",
  "containerd": "${CONTAINERD_VERSION}",
  "cni": "${CNI_VERSION}",
  "cosign": "${COSIGN_VERSION}",
  "hotbox_from": "${RUNTIME_IMAGE}",
  "guest_rootfs_sha256": "$(cat "$rootfs.sha256")"
}
JSON

log "Cleaning per-instance state"
apt_get clean
rm -rf /var/lib/apt/lists/*
cloud-init clean --logs --machine-id 2>/dev/null || truncate -s 0 /etc/machine-id
journalctl --rotate >/dev/null 2>&1 || true
journalctl --vacuum-time=1s >/dev/null 2>&1 || true
# On Google Cloud, Packer's startup-script wrapper runs gcloud as root after
# this script ends, to report the bake's status. Keep the configuration and
# access token it writes in memory, so the image never holds them.
gcloud_config="${CLOUDSDK_CONFIG:-${HOME:-/root}/.config/gcloud}"
rm -rf "$gcloud_config"
install -d -m 0700 "$gcloud_config"
mount -t tmpfs -o mode=0700,size=16m tmpfs "$gcloud_config"

log "Host image baked"
if [[ "$POWEROFF" == "true" ]]; then
    sync
    systemctl poweroff
fi
