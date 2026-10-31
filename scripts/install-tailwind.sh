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
        v4.3.0/tailwindcss-linux-x64) echo 73f0e5459054e5cfaa8ab6f3b940f3fbe0f13cc7fd83bc24e7c655033c203400 ;;
        v4.3.0/tailwindcss-linux-arm64) echo 8f48dcb72be3b351c10563c5329b4638ba8516820dc3b3a1609625a166e87cbd ;;
        v4.3.0/tailwindcss-macos-x64) echo 2ba252f770817091e6d0d12a84e0dd531bcc29aad1bfd9d976a3aff1a071b67a ;;
        v4.3.0/tailwindcss-macos-arm64) echo 56b4bbc62dbdc4614a78930d9c6986423a2ec63e4e640144a59a5d95c914322e ;;
        v4.3.0/tailwindcss-windows-x64.exe) echo e4303d06a337899d426583508f55c143ce89a585f90ed080f482d02fd83093c1 ;;
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
