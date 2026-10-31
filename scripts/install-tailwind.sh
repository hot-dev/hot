#!/usr/bin/env bash
# install-tailwind.sh - download the Tailwind CSS standalone CLI and verify it
#
# Usage:
#   ./scripts/install-tailwind.sh VERSION INSTALL_DIR
#
# Installs INSTALL_DIR/tailwindcss (tailwindcss.exe on Windows) for this
# machine's OS and architecture. The download must match the SHA-256 pinned
# below for that release asset, or nothing is installed. To move to a new
# Tailwind release, add its assets' digests here; GitHub lists them with
#   gh release view VERSION -R tailwindlabs/tailwindcss --json assets \
#     --jq '.assets[] | [.name, .digest] | @tsv'

set -euo pipefail

version="${1:?usage: $0 VERSION INSTALL_DIR}"
install_dir="${2:?usage: $0 VERSION INSTALL_DIR}"

# SHA-256 of each release asset, by "<version>/<asset>".
pinned_sha256() {
    case "$1" in
        v4.3.3/tailwindcss-linux-x64) echo dc61b3ac6b8c9ca874c0cc4c57b2409791a64c5540404ca5f5367360babc313a ;;
        v4.3.3/tailwindcss-linux-arm64) echo 55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195 ;;
        v4.3.3/tailwindcss-macos-x64) echo 7922e0953f2110c05976e3bf58f14e643d90427575e766b7d433f5f80cbee7e1 ;;
        v4.3.3/tailwindcss-macos-arm64) echo cdf646702987a743464dff4d9c60fd4480d1c1e73dd819a9a67f1078815dce9d ;;
        v4.3.3/tailwindcss-windows-x64.exe) echo e0e260ce048014e9268f6237ff18f8ccf02cef521cbd0ae04e82c2cdf7aa3955 ;;
        *) return 1 ;;
    esac
}

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset=tailwindcss-linux-x64 ;;
    Linux-aarch64 | Linux-arm64) asset=tailwindcss-linux-arm64 ;;
    Darwin-x86_64) asset=tailwindcss-macos-x64 ;;
    Darwin-arm64) asset=tailwindcss-macos-arm64 ;;
    MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) asset=tailwindcss-windows-x64.exe ;;
    *) echo "install-tailwind: unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if ! expected="$(pinned_sha256 "$version/$asset")"; then
    echo "install-tailwind: no pinned SHA-256 for $version/$asset; add it to $0" >&2
    exit 1
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

binary=tailwindcss
[[ "$asset" == *.exe ]] && binary=tailwindcss.exe
mkdir -p "$install_dir"
download="$(mktemp "$install_dir/.tailwindcss.XXXXXX")"
trap 'rm -f "$download"' EXIT

curl -fsSL --retry 3 -o "$download" \
    "https://github.com/tailwindlabs/tailwindcss/releases/download/$version/$asset"
actual="$(sha256_of "$download")"
if [[ "$actual" != "$expected" ]]; then
    echo "install-tailwind: $version/$asset has SHA-256 $actual, expected $expected" >&2
    exit 1
fi
chmod +x "$download"
mv "$download" "$install_dir/$binary"
trap - EXIT
echo "Installed Tailwind CSS $version ($asset, sha256 $actual) to $install_dir/$binary"
