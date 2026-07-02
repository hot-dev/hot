#!/bin/bash

set -e

usage() {
    cat <<'EOF'
Usage: build-mac.sh [--arch ARCH]

Build Hot Dev release binaries for macOS.

Options:
  --arch ARCH   Architecture to build (default: all)
                  all, both          - build arm64 and x86_64 (default)
                  native             - build for this machine only
                  arm64, aarch64     - Apple Silicon
                  x86_64, x64, amd64 - Intel Mac
                  aarch64-apple-darwin, x86_64-apple-darwin - full Rust target triple
  -h, --help    Show this help
EOF
}

ARCH="all"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)
            if [ -z "${2:-}" ]; then
                echo "Error: --arch requires a value"
                usage
                exit 1
            fi
            ARCH="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Error: Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

resolve_mac_targets() {
    case "$1" in
        all|both|"")
            echo "aarch64-apple-darwin x86_64-apple-darwin"
            ;;
        native)
            case "$(uname -m)" in
                arm64)
                    echo "aarch64-apple-darwin"
                    ;;
                x86_64)
                    echo "x86_64-apple-darwin"
                    ;;
                *)
                    echo "Error: Unsupported native architecture: $(uname -m)" >&2
                    exit 1
                    ;;
            esac
            ;;
        arm64|aarch64)
            echo "aarch64-apple-darwin"
            ;;
        x86_64|x64|amd64|intel)
            echo "x86_64-apple-darwin"
            ;;
        aarch64-apple-darwin|x86_64-apple-darwin)
            echo "$1"
            ;;
        *)
            echo "Error: Invalid --arch value: $1" >&2
            usage
            exit 1
            ;;
    esac
}

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Building Hot Dev bundles for macOS..."
cd "$(dirname "$0")/../"

TARGETS=($(resolve_mac_targets "$ARCH"))
echo "Targets: ${TARGETS[*]}"

for TARGET in "${TARGETS[@]}"; do
    echo "Building $TARGET..."
    cargo build --release --target "$TARGET"
done

echo "macOS builds created successfully!"
echo "Builds location: target/<target>/release/"

# Restore original directory
cd "$ORIGINAL_DIR"
