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
        v4.3.5/tailwindcss-linux-x64) echo e47695cee3f0d90623a91f3939b08bdee610eb5ad99340adff9d4145b284d6cc ;;
        v4.3.5/tailwindcss-linux-arm64) echo ffcd0d21dbd7c605dc12743ff0c0b47debc963c86b399674084c1c8f2c96ecad ;;
        v4.3.5/tailwindcss-macos-x64) echo a63702686a3ed048b3135d84a73be49ea9d47f880c9b20e54b3ef60934ad8cdf ;;
        v4.3.5/tailwindcss-macos-arm64) echo f3f4f5d1727e2b693b8f1c9a1492b5dd87008c86ea286bee1589cb458e6235fc ;;
        v4.3.5/tailwindcss-windows-x64.exe) echo 2803583268c53f4713e89943e211081fddb186c0a4a7d07b5c342c6ae853ec95 ;;
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
