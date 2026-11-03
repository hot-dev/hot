#!/usr/bin/env bash
# Build the Hot self-host VM image for one cloud with Packer.
#
#   infra/images/build-host.sh aws|gcp RELEASE RUNTIME_IMAGE [packer -var args...]
#
# RUNTIME_IMAGE is the release's runtime image pinned by digest, from its
# release manifest. Examples:
#
#   infra/images/build-host.sh aws 3.0.1 ghcr.io/hot-dev/hot-runtime@sha256:... \
#       -var aws_region=us-east-2 -var aws_subnet_id=subnet-0123
#   infra/images/build-host.sh gcp 3.0.1 ghcr.io/hot-dev/hot-runtime@sha256:... \
#       -var gcp_project=my-project -var gcp_subnetwork=hot-build
#
# The build stops, and Packer deletes the build VM, after
# HOT_IMAGE_BUILD_TIMEOUT (default 120m, in timeout(1)'s format).
set -euo pipefail

cloud="${1:?usage: build-host.sh aws|gcp RELEASE RUNTIME_IMAGE [packer args...]}"
release="${2:?missing RELEASE}"
runtime_image="${3:?missing RUNTIME_IMAGE}"
shift 3
[[ "$cloud" == "aws" || "$cloud" == "gcp" ]] || { echo "cloud must be aws or gcp" >&2; exit 2; }
[[ "$runtime_image" =~ @sha256:[0-9a-f]{64}$ ]] || { echo "RUNTIME_IMAGE must be pinned by digest" >&2; exit 2; }
# On Google Cloud, Packer waits for the startup script's status with no limit.
time_limit=$(command -v timeout || command -v gtimeout) ||
    { echo "build-host.sh needs timeout from GNU coreutils (on macOS: brew install coreutils)" >&2; exit 2; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The build VM receives the setup script inline: nothing is fetched at boot.
# It goes to tmpfs, so the image does not keep a copy.
boot_script() {
    printf '#!/bin/bash\nset -euo pipefail\n'
    printf "cat > /run/hot-setup-host.sh <<'HOT_SETUP_EOF'\n"
    cat "$here/host/setup-host.sh"
    printf 'HOT_SETUP_EOF\n'
    printf 'bash /run/hot-setup-host.sh --runtime-image %q %s\n' "$runtime_image" "$1"
}

if [[ "$cloud" == "aws" ]]; then
    # EC2 user data is limited to 16 KB; cloud-init accepts it gzipped.
    boot_script --poweroff | gzip -9 > "$work/user-data.gz"
    vars=(-var "aws_user_data_file=$work/user-data.gz")
    only="amazon-ebs.hot-host"
else
    boot_script "" > "$work/startup.sh"
    vars=(-var "gcp_startup_script_file=$work/startup.sh")
fi
[[ "$cloud" == "gcp" ]] && only="googlecompute.hot-host"

packer init "$here/packer"
# TERM is the signal Packer's wrapper process forwards to the build, which then
# cancels and deletes the build VM before exiting; the wrapper ignores INT.
# --foreground lets Ctrl-C in a terminal still reach the whole process group.
"$time_limit" --foreground --signal=TERM --kill-after=15m "${HOT_IMAGE_BUILD_TIMEOUT:-120m}" \
    packer build -only="$only" -var "hot_release=$release" "${vars[@]}" "$@" "$here/packer"
