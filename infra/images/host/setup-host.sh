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
# --runtime-image supplies `hotbox`, which is baked into the Kata guest image.
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
apt-get update -q
apt-get install -y -q --no-install-recommends \
    ca-certificates curl jq python3 zstd e2fsprogs dmsetup thin-provisioning-tools \
    iproute2 iptables docker.io docker-compose-v2
apt-get upgrade -y -q

# ------------------------------------------------------------------
# No SSH, on either cloud. Operators reach the host through the agent's
# command channel only.
# ------------------------------------------------------------------
log "Disabling SSH"
systemctl disable --now ssh.socket ssh.service 2>/dev/null || true
systemctl mask ssh.socket ssh.service
rm -f /etc/ssh/ssh_host_*

log "Configuring Docker"
install -d -m 0755 /etc/docker
cat > /etc/docker/daemon.json <<'JSON'
{
  "log-driver": "local",
  "log-opts": {"max-size": "10m", "max-file": "3"}
}
JSON
systemctl enable docker

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
# Guest image: Kata's Ubuntu rootfs with this release's hotbox
# ------------------------------------------------------------------
log "Building the guest image with hotbox from ${RUNTIME_IMAGE}"
systemctl start docker
docker pull -q "$RUNTIME_IMAGE"
container=$(docker create "$RUNTIME_IMAGE")
docker cp "$container:/usr/share/hot/bin/hotbox-linux-x86_64" "$work/hotbox"
# The deploy agent and the bundle renderer it uses come from the same image.
docker cp "$container:/usr/share/hot/selfhost/." "$work/selfhost"
docker rm "$container" >/dev/null
docker image rm "$RUNTIME_IMAGE" >/dev/null
# Kata's shim bind-mounts hotbox from this host path into each guest.
install -m 0755 "$work/hotbox" /usr/local/bin/hotbox

install -d -m 0755 "$HOST_DIR/kata"
rootfs="$HOST_DIR/kata/hot-rootfs.img"
cp "$(readlink -f /opt/kata/share/kata-containers/kata-ubuntu-noble.image)" "$rootfs"
loop=$(losetup --find --show --partscan "$rootfs")
mnt="$work/rootfs"
mkdir -p "$mnt"
udevadm settle
if [[ -b "${loop}p1" ]]; then mount "${loop}p1" "$mnt"; else mount "$loop" "$mnt"; fi
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
default_maxmemory = 2048
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

[grpc]
  address = "/run/kata-containerd/containerd.sock"
  gid = ${KATA_CONTAINERD_GID}

[plugins]
  [plugins."io.containerd.grpc.v1.cri"]
    disable = true

  [plugins."io.containerd.snapshotter.v1.devmapper"]
    root_path = "/var/lib/kata/devmapper"
    pool_name = "hot-devpool"
    base_image_size = "30GB"
    discard_blocks = true
    fs_type = "ext4"

  [plugins."io.containerd.runtime.v2.task"]
    [plugins."io.containerd.runtime.v2.task"."io.containerd.kata.v2"]
      runtime_type = "io.containerd.kata.v2"
CONTAINERD

log "Persisting kernel modules and sysctls"
cat > /etc/modules-load.d/hot-host.conf <<'MODULES'
vhost_vsock
vmw_vsock_virtio_transport
vsock_loopback
br_netfilter
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
device="${1:?usage: hot-data-disk DEVICE [--allow-format]}"
allow_format="${2:-}"
[[ -b "$device" ]] || { echo "Data disk $device is not attached" >&2; exit 1; }
real=$(readlink -f "$device")
root_source=$(findmnt -n -o SOURCE /)
if [[ "$root_source" == "$real"* ]]; then
    echo "Refusing to use the boot disk $real as the data disk" >&2
    exit 1
fi
if ! blkid -p "$real" >/dev/null 2>&1; then
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
install -d -m 0755 /var/lib/hot/kata
DISK

# Size the box storage pool from /etc/hot/host.env (written by the agent from
# the installation's size) so box tasks cannot fill the data disk.
cat > "$LIBEXEC/kata-devpool-activate" <<'POOL'
#!/usr/bin/env bash
set -euo pipefail
pool="hot-devpool"
dir="/var/lib/kata/devmapper"
mountpoint -q /var/lib/kata || { echo "/var/lib/kata is not mounted" >&2; exit 1; }
dmsetup info "$pool" >/dev/null 2>&1 && exit 0
HOT_BOX_POOL_GB=40
# shellcheck source=/dev/null
[[ -f /etc/hot/host.env ]] && . /etc/hot/host.env
[[ "$HOT_BOX_POOL_GB" =~ ^[0-9]+$ && "$HOT_BOX_POOL_GB" -ge 10 ]] ||
    { echo "HOT_BOX_POOL_GB must be at least 10" >&2; exit 1; }
install -d -m 0700 "$dir"
# Sparse files: the pool can never use more than its configured size.
[[ -f "$dir/data" ]] || truncate -s "${HOT_BOX_POOL_GB}G" "$dir/data"
[[ -f "$dir/meta" ]] || truncate -s "$(( HOT_BOX_POOL_GB / 10 + 1 ))G" "$dir/meta"
data=$(losetup --find --show "$dir/data")
meta=$(losetup --find --show "$dir/meta")
sectors=$(( $(blockdev --getsize64 "$data") / 512 ))
dmsetup create "$pool" --table "0 $sectors thin-pool $meta $data 128 32768"
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

# Host baseline for box guests, in the mangle table so it applies before the
# task worker's per-task filter rules and nothing they insert can bypass it.
# /etc/hot/guest-network.env (written by the agent) may set
# HOT_GUEST_ALLOW_CIDRS, HOT_GUEST_DENY_CIDRS, and HOT_GUEST_DNS.
cat > "$LIBEXEC/hot-guest-firewall" <<'FIREWALL'
#!/usr/bin/env bash
set -euo pipefail
bridge="kata-br0"
chain="HOT-GUEST-BASELINE"
HOT_GUEST_ALLOW_CIDRS=""
HOT_GUEST_DENY_CIDRS=""
HOT_GUEST_DNS="1.1.1.1,8.8.8.8"
# shellcheck source=/dev/null
[[ -f /etc/hot/guest-network.env ]] && . /etc/hot/guest-network.env
split() { tr ',' '\n' <<<"$1" | tr -d ' ' | grep -v '^$' | grep -v ':' || true; }

iptables -t mangle -N "$chain" 2>/dev/null || true
iptables -t mangle -F "$chain"
for resolver in $(split "$HOT_GUEST_DNS"); do
    for protocol in udp tcp; do
        iptables -t mangle -A "$chain" -d "$resolver" -p "$protocol" --dport 53 -j RETURN
    done
done
# Never reachable: the host, every metadata service, other guests, the
# installation's own networks.
for destination in 0.0.0.0/8 127.0.0.0/8 169.254.0.0/16 10.88.0.0/16 224.0.0.0/4 240.0.0.0/4 \
        $(split "$HOT_GUEST_DENY_CIDRS"); do
    iptables -t mangle -A "$chain" -d "$destination" -j DROP
done
for destination in $(split "$HOT_GUEST_ALLOW_CIDRS"); do
    iptables -t mangle -A "$chain" -d "$destination" -j RETURN
done
for destination in 10.0.0.0/8 100.64.0.0/10 172.16.0.0/12 192.168.0.0/16; do
    iptables -t mangle -A "$chain" -d "$destination" -j DROP
done
iptables -t mangle -A "$chain" -j RETURN
while iptables -t mangle -D FORWARD -i "$bridge" -j "$chain" 2>/dev/null; do :; done
iptables -t mangle -I FORWARD 1 -i "$bridge" -j "$chain"

# Guests never address the host, and guests have no IPv6.
iptables -C INPUT -i "$bridge" -j DROP 2>/dev/null || iptables -I INPUT 1 -i "$bridge" -j DROP
for direction in "FORWARD -i" "FORWARD -o" "INPUT -i"; do
    # shellcheck disable=SC2086
    ip6tables -C $direction "$bridge" -j DROP 2>/dev/null || ip6tables -I $direction "$bridge" -j DROP
done
FIREWALL
chmod 0755 "$LIBEXEC"/*

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
cat > /etc/systemd/system/kata-containerd.service <<'UNIT'
[Unit]
Description=containerd for Kata Containers
ConditionPathIsMountPoint=/var/lib/hot
Requires=var-lib-kata.mount hot-guest-firewall.service
After=var-lib-kata.mount hot-guest-firewall.service network.target

[Service]
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

# Enabled at boot by the installation's bootstrap, which first writes
# /etc/hot/installation.json.
cat > /etc/systemd/system/hot-agent.service <<'UNIT'
[Unit]
Description=Hot deploy agent
After=docker.service network-online.target
Wants=network-online.target
Requires=docker.service
ConditionPathExists=/etc/hot/installation.json

[Service]
ExecStart=/usr/local/bin/hot-agent run
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable hot-guest-firewall.service kata-containerd.service
mkdir -p /run/vc /tmp/hot-box-io /var/run/netns /var/lib/cni/networks

# ------------------------------------------------------------------
# Record what was baked, then reset per-instance state
# ------------------------------------------------------------------
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
apt-get clean
rm -rf /var/lib/apt/lists/*
cloud-init clean --logs --machine-id 2>/dev/null || truncate -s 0 /etc/machine-id
journalctl --rotate >/dev/null 2>&1 || true
journalctl --vacuum-time=1s >/dev/null 2>&1 || true

log "Host image baked"
if [[ "$POWEROFF" == "true" ]]; then
    sync
    systemctl poweroff
fi
