#!/bin/bash

set -e

usage() {
    cat <<'EOF'
Usage: package-local.sh [--install]

Build and package Hot Dev for this machine only.

macOS:
  Runs build-mac.sh and package-mac.sh with --arch native, producing a .pkg
  installer in target/packages/.

Linux:
  Runs cargo build --release for the native host. Use the binary from the repo
  checkout (resources are resolved from the workspace), or copy it to your PATH.

Options:
  --install     Install the macOS .pkg after packaging (requires sudo)
  -h, --help    Show this help
EOF
}

INSTALL=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --install)
            INSTALL=true
            shift
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

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

case "$(uname -s)" in
    Darwin)
        echo "Building and packaging Hot Dev for native macOS..."
        "$SCRIPT_DIR/build-mac.sh" --arch native

        # Cross-compile the hotbox Linux binary and stage it in resources/bin/
        # so package-mac.sh includes it in the installer payload
        # (/usr/local/share/hot/bin/). Without it, installed builds cannot
        # bind-mount hotbox into Docker container tasks (ffmpeg, whisper, ...)
        # and those tasks fail. CI release builds get these binaries from the
        # Linux runners; locally we must build them here.
        if docker info >/dev/null 2>&1; then
            "$SCRIPT_DIR/build-hotbox.sh"
            mkdir -p "$REPO_ROOT/resources/bin"
            cp "$REPO_ROOT"/target/hotbox-linux-* "$REPO_ROOT/resources/bin/"
            chmod +x "$REPO_ROOT/resources/bin/hotbox-linux-"*
        else
            echo "Warning: Docker is not available — skipping hotbox cross-compile."
            echo "Container tasks (ffmpeg, whisper, ...) will not work with this package."
        fi

        "$SCRIPT_DIR/package-mac.sh" --arch native

        VERSION=$(head -1 resources/version.txt | tr -d '[:space:]')
        case "$(uname -m)" in
            arm64)
                TARGET="aarch64-apple-darwin"
                ;;
            x86_64)
                TARGET="x86_64-apple-darwin"
                ;;
            *)
                echo "Error: Unsupported native architecture: $(uname -m)"
                exit 1
                ;;
        esac

        PKG="$REPO_ROOT/target/packages/hot_${VERSION}_${TARGET}.pkg"
        if [ ! -f "$PKG" ]; then
            echo "Error: Expected package not found at $PKG"
            exit 1
        fi

        echo ""
        echo "Package ready: $PKG"
        echo "Install with:"
        echo "  sudo installer -pkg \"$PKG\" -target /"

        if [ "$INSTALL" = true ]; then
            echo ""
            echo "Installing package..."
            sudo installer -pkg "$PKG" -target /
            echo "Installed Hot Dev $VERSION for $TARGET"
        fi
        ;;

    Linux)
        if [ "$INSTALL" = true ]; then
            echo "Error: --install is only supported on macOS"
            exit 1
        fi

        echo "Building Hot Dev release binary for native Linux ($(uname -m))..."
        cargo build --release

        BIN="$REPO_ROOT/target/release/hot"
        echo ""
        echo "Build ready: $BIN"
        echo ""
        echo "From this repo checkout, run Hot directly with:"
        echo "  $BIN"
        echo ""
        echo "To install the binary on your PATH:"
        echo "  sudo install -m 755 \"$BIN\" /usr/local/bin/hot"
        echo ""
        echo "Note: running from the repo checkout is recommended for local dev;"
        echo "Hot resolves resources from this workspace automatically."
        ;;

    *)
        echo "Error: package-local.sh is not supported on $(uname -s)"
        echo "Supported platforms: macOS (Darwin), Linux"
        exit 1
        ;;
esac
