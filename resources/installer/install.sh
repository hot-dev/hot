#!/bin/sh
#
# Hot Dev Installer Script
# https://hot.dev
#
# Usage:
#   curl -fsSL https://get.hot.dev/install.sh | sh
#   curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version 1.2.3
#   curl -fsSL https://get.hot.dev/install.sh | sh -s -- --system
#
# By default Hot installs for the current user, without sudo: the binary goes
# in ~/.local/bin and its resources in ~/.local/share/hot (--prefix picks
# another folder). --system installs for every user of the machine with the
# macOS or Debian package, which needs sudo. Running as root or in CI, with a
# package install already on the machine, or for a release from before user
# installs, defaults to --system.
#

set -eu

HOT_VERSION="latest"
MODE=""
PREFIX="${HOT_INSTALL_PREFIX:-}"
MODIFY_PATH=true

# Identifiers of the macOS and Debian packages
MACOS_PKG_ID="dev.hot.hot.pkg"
DEB_PKG_NAME="hot"

# Marks a folder this script installed from the release archive. `hot update`
# reads it to update that install in place.
RECEIPT="install-receipt.json"

error() {
    echo "Error: $1" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: install.sh [options]

  --user            Install for the current user, without sudo (the default)
  --system          Install for every user of the machine (needs sudo)
  --prefix DIR      Install into DIR/bin and DIR/share/hot (default ~/.local)
  --version VER     Install version VER instead of the latest release
  --no-modify-path  Don't add the install's bin folder to your shell's PATH
  -h, --help        Show this help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version|-v)
            [ $# -ge 2 ] || error "$1 needs a version"
            HOT_VERSION="$2"
            shift 2
            ;;
        --prefix)
            [ $# -ge 2 ] || error "--prefix needs a folder"
            PREFIX="$2"
            shift 2
            ;;
        --user) MODE="user"; shift ;;
        --system) MODE="system"; shift ;;
        --no-modify-path) MODIFY_PATH=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Warning: ignoring unknown option $1" >&2; shift ;;
    esac
done

case "$HOT_VERSION" in
    *[!A-Za-z0-9.-]*|"") error "Invalid version: $HOT_VERSION" ;;
esac

if [ -n "${HOT_INSTALL_BASE_URL:-}" ]; then
    # A mirror, like `hot update`'s HOT_UPDATE_BASE_URL, keeps the latest
    # folder's names
    BASE_URL="${HOT_INSTALL_BASE_URL%/}"
    FILE_PREFIX="hot"
elif [ "$HOT_VERSION" = "latest" ]; then
    BASE_URL="https://get.hot.dev/releases/latest"
    # The latest folder names files without the version: hot_macos_arm64.pkg
    FILE_PREFIX="hot"
else
    HOT_VERSION="${HOT_VERSION#v}"
    BASE_URL="https://get.hot.dev/releases/${HOT_VERSION}"
    FILE_PREFIX="hot_${HOT_VERSION}"
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
fi

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Darwin) echo "macos" ;;
        Linux) echo "linux" ;;
        *) error "Unsupported operating system: $(uname -s)" ;;
    esac
}

# Detect architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64) echo "x86_64" ;;
        arm64|aarch64) echo "arm64" ;;
        *) error "Unsupported architecture: $(uname -m)" ;;
    esac
}

# Whether the macOS or Debian package is installed
package_installed() {
    case "$1" in
        macos) pkgutil --pkg-info "$MACOS_PKG_ID" >/dev/null 2>&1 ;;
        linux)
            command -v dpkg-query >/dev/null 2>&1 &&
                dpkg-query -W -f='${Status}' "$DEB_PKG_NAME" 2>/dev/null | grep -q "install ok installed"
            ;;
    esac
}

# How to remove the package install, for the message after a user install
package_remove_command() {
    case "$1" in
        macos) echo "sudo rm -rf /usr/local/bin/hot /usr/local/share/hot && sudo pkgutil --forget $MACOS_PKG_ID" ;;
        linux) echo "sudo dpkg -r $DEB_PKG_NAME" ;;
    esac
}

# Download file
download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        error "Neither curl nor wget found. Please install one of them."
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        return 1
    fi
}

# Download $1 into $2 and check it against the release's $1.sha256. With
# "optional" as $3, a release that publishes no checksum only warns.
download_verified() {
    echo "Downloading $(basename "$1")..."
    download "$1" "$2" || error "Download failed: $1"
    if ! download "$1.sha256" "$2.sha256" 2>/dev/null; then
        if [ "${3:-}" = "optional" ]; then
            echo "Warning: this release publishes no checksum for $(basename "$1"); not verified." >&2
            return 0
        fi
        error "This release publishes no checksum for $(basename "$1"). Releases before user installs were supported can only install with --system."
    fi
    read -r expected _ < "$2.sha256" || true
    actual="$(sha256_of "$2")" || error "Neither sha256sum nor shasum found to verify the download."
    [ "$actual" = "$expected" ] || error "Checksum mismatch for $(basename "$1"): expected $expected, got $actual"
}

# Install on macOS
install_macos() {
    echo "Installing Hot..."
    $SUDO installer -pkg "$1" -target / || error "Installation failed"
}

# Install on Linux
install_linux() {
    echo "Installing Hot..."
    if command -v dpkg >/dev/null 2>&1; then
        $SUDO dpkg -i "$1" || error "Installation failed"
    else
        $SUDO apt install -y "$1" || error "Installation failed"
    fi
}

# Install the release archive $1 into $PREFIX/bin/hot and $PREFIX/share/hot.
# $AS runs each step (empty, or sudo for a system install). Each step that
# replaces a file or folder is a rename within the prefix, so a failure
# leaves the old install working.
install_archive() (
    bin_dir="$PREFIX/bin"
    share_dir="$PREFIX/share"

    # Don't replace an install this script didn't make, such as a package's.
    if [ -e "$share_dir/hot" ] && [ ! -f "$share_dir/hot/$RECEIPT" ]; then
        error "$share_dir/hot holds a Hot install this script didn't make. Remove it, or choose another --prefix."
    fi
    if [ -e "$bin_dir/hot" ] && [ ! -f "$share_dir/hot/$RECEIPT" ]; then
        error "$bin_dir/hot already exists and this script didn't install it. Remove it, or choose another --prefix."
    fi

    $AS mkdir -p "$bin_dir" "$share_dir" || error "Could not create $bin_dir and $share_dir"
    work_dir="$($AS mktemp -d "$share_dir/.hot-install.XXXXXXXX")"
    stage="$work_dir/new"
    old_resources="$work_dir/old"
    new_exe=""
    swapping_resources=false
    committed=false

    # Called by the EXIT trap below.
    # shellcheck disable=SC2329
    cleanup_archive() {
        status=$?
        trap - 0 INT TERM
        rollback_ok=true
        # The final rename may have completed just before a signal arrived.
        if [ -n "$new_exe" ] && [ ! -e "$new_exe" ]; then
            committed=true
        fi
        if [ "$committed" != true ]; then
            if [ "$swapping_resources" = true ] && [ ! -e "$stage" ]; then
                $AS mv "$share_dir/hot" "$stage" || rollback_ok=false
            fi
            if [ -e "$old_resources" ] && [ "$rollback_ok" = true ]; then
                $AS mv "$old_resources" "$share_dir/hot" || rollback_ok=false
            fi
        fi
        if [ -n "$new_exe" ]; then
            $AS rm -f "$new_exe"
        fi
        if [ "$rollback_ok" = true ]; then
            $AS rm -rf "$work_dir"
        else
            echo "Could not restore the previous resources; they are preserved in $old_resources" >&2
        fi
        exit "$status"
    }
    trap cleanup_archive 0
    trap 'exit 1' INT TERM

    $AS mkdir "$stage"
    echo "Installing Hot into $PREFIX..."
    # System installs extract as root. Keep that ownership instead of giving
    # the installed files to the release builder's UID and GID.
    if ! $AS tar --no-same-owner -xzf "$1" -C "$stage" || [ ! -f "$stage/hot" ] || [ ! -d "$stage/pkg/hot-std" ]; then
        error "Could not unpack the hot binary and its standard library from $(basename "$1")"
    fi

    new_exe="$($AS mktemp "$bin_dir/.hot.new.XXXXXXXX")"
    $AS mv "$stage/hot" "$new_exe"
    $AS chmod 755 "$new_exe"
    printf '{"method": "archive"}\n' | $AS tee "$stage/$RECEIPT" >/dev/null

    swapping_resources=true
    if [ -e "$share_dir/hot" ]; then
        $AS mv "$share_dir/hot" "$old_resources"
    fi
    $AS mv "$stage" "$share_dir/hot"
    $AS mv -f "$new_exe" "$bin_dir/hot"
    committed=true
)

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
    esac
    return 1
}

# Add $1 to PATH in the user's shell startup file
add_to_path() {
    on_path "$1" && return 0

    case "$1" in
        "${HOME:-/nonexistent}"/*) shown="\$HOME${1#"$HOME"}" ;;
        *) shown="$1" ;;
    esac
    case "$(basename "${SHELL:-sh}")" in
        zsh) rc="${ZDOTDIR:-${HOME:-}}/.zshrc"; line="export PATH=\"$shown:\$PATH\"" ;;
        bash)
            rc="${HOME:-}/.bashrc"
            if [ "$(uname -s)" = "Darwin" ]; then
                # macOS terminals start login shells, which read the first of
                # these that exists. Creating .bash_profile would hide .profile.
                rc="${HOME:-}/.bash_profile"
                for f in .bash_profile .bash_login .profile; do
                    if [ -f "${HOME:-}/$f" ]; then rc="$HOME/$f"; break; fi
                done
            fi
            line="export PATH=\"$shown:\$PATH\""
            ;;
        fish) rc="${HOME:-}/.config/fish/conf.d/hot.fish"; line="fish_add_path \"$shown\"" ;;
        *) rc="${HOME:-}/.profile"; line="export PATH=\"$shown:\$PATH\"" ;;
    esac

    echo ""
    if [ "$MODIFY_PATH" != true ] || [ -z "${HOME:-}" ]; then
        echo "$1 is not on your PATH. Add it with:"
        echo "  $line"
        return 0
    fi
    if [ ! -f "$rc" ] || ! grep -qF "$line" "$rc"; then
        mkdir -p "$(dirname "$rc")"
        printf '\n# Added by the Hot installer\n%s\n' "$line" >> "$rc"
    fi
    echo "Added $1 to your PATH in $rc."
    echo "Open a new terminal, or run this in the current one:"
    echo "  $line"
}

# Main installation flow
main() {
    echo ""
    echo "Hot Dev Installer"
    echo ""
    echo "License: Apache-2.0"
    echo ""
    echo "By continuing, you accept the Apache License, Version 2.0."
    echo ""

    os="$(detect_os)"
    arch="$(detect_arch)"
    echo "Detected platform: ${os} ${arch}"

    chosen=false
    if [ -n "$MODE" ] || [ -n "$PREFIX" ]; then
        chosen=true
    fi
    if [ -z "$MODE" ]; then
        if [ -n "$PREFIX" ]; then
            MODE="user"
        elif [ -z "$SUDO" ]; then
            # Root, as in a Dockerfile: install for the whole machine.
            MODE="system"
        elif [ -n "${CI:-}" ]; then
            # CI runners have sudo, and later steps expect hot on the system PATH.
            MODE="system"
        elif package_installed "$os"; then
            MODE="system"
            echo "Updating the existing system install. Use --user to install for just you."
        else
            MODE="user"
        fi
    fi

    # Another hot already on PATH (a package, Homebrew, a copy)
    other_hot="$(command -v hot 2>/dev/null || true)"

    tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t hot-install)"
    trap 'rm -rf "$tmp_dir"' EXIT
    trap 'exit 1' INT TERM

    # Releases whose hot can run from any folder publish their archive's
    # checksum. An older release installs for the whole machine.
    archive="${FILE_PREFIX}_${os}_${arch}.tar.gz"
    if [ "$MODE" = "user" ] && ! download "${BASE_URL}/${archive}.sha256" "$tmp_dir/probe.sha256" 2>/dev/null; then
        if [ "$chosen" = true ]; then
            error "This Hot release ($HOT_VERSION) can't be installed for one user or into a chosen folder. Install it with --system."
        fi
        echo "This Hot release ($HOT_VERSION) can't be installed for one user, so it installs for the whole machine, with sudo."
        MODE="system"
    fi

    AS=""
    if [ "$MODE" = "system" ] && [ -z "$PREFIX" ] && [ "$os" = "macos" ]; then
        file="${FILE_PREFIX}_${os}_${arch}.pkg"
        download_verified "${BASE_URL}/${file}" "$tmp_dir/$file" optional
        install_macos "$tmp_dir/$file"
        bin_dir="/usr/local/bin"
    elif [ "$MODE" = "system" ] && [ -z "$PREFIX" ] && command -v dpkg >/dev/null 2>&1; then
        file="${FILE_PREFIX}_${os}_${arch}.deb"
        download_verified "${BASE_URL}/${file}" "$tmp_dir/$file" optional
        install_linux "$tmp_dir/$file"
        bin_dir="/usr/bin"
    else
        checksum=""
        if [ "$MODE" = "system" ]; then
            # Linux without dpkg, or a chosen --prefix: the release archive,
            # for the whole machine. An older release's hot finds its
            # resources in /usr/local/share/hot, so it needs no checksum.
            PREFIX="${PREFIX:-/usr/local}"
            AS="$SUDO"
            checksum="optional"
        else
            [ -n "${HOME:-}" ] || error "HOME is not set; pass --prefix"
            PREFIX="${PREFIX:-$HOME/.local}"
        fi
        download_verified "${BASE_URL}/${archive}" "$tmp_dir/$archive" "$checksum"
        install_archive "$tmp_dir/$archive"
        bin_dir="$PREFIX/bin"
    fi

    # Verify installation
    echo ""
    if version="$("$bin_dir/hot" version 2>/dev/null)"; then
        echo "Hot installed successfully!"
        echo "Version: ${version}"
    else
        error "Installation completed, but $bin_dir/hot does not run."
    fi

    if [ "$MODE" = "user" ]; then
        add_to_path "$bin_dir"
        if [ -n "$other_hot" ] && [ "$other_hot" != "$bin_dir/hot" ]; then
            echo ""
            echo "Another hot is installed at $other_hot."
            echo "Which one runs depends on the order of your PATH."
            if package_installed "$os"; then
                echo "To remove the system install:"
                echo "  $(package_remove_command "$os")"
            fi
        fi
    fi

    echo ""
    echo "Documentation: https://hot.dev/docs"
    echo ""
}

main "$@"
