#!/bin/bash
#
# Hot Dev Installer Script
# https://hot.dev
#
# Usage:
#   curl -fsSL https://get.hot.dev/install.sh | sh
#   curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version 1.2.3
#
# This script detects your OS and architecture, downloads the appropriate
# installer package, and installs Hot silently.
#

set -e

HOT_VERSION="latest"
while [ $# -gt 0 ]; do
    case "$1" in
        --version|-v) HOT_VERSION="$2"; shift 2 ;;
        *) shift ;;
    esac
done

if [ "$HOT_VERSION" = "latest" ]; then
    BASE_URL="https://get.hot.dev/releases/latest"
else
    BASE_URL="https://get.hot.dev/releases/${HOT_VERSION#v}"
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
fi

error() {
    echo "Error: $1" >&2
    exit 1
}

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Darwin)
            echo "macos"
            ;;
        Linux)
            echo "linux"
            ;;
        *)
            error "Unsupported operating system: $(uname -s)"
            ;;
    esac
}

# Detect architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)
            echo "x86_64"
            ;;
        arm64|aarch64)
            echo "arm64"
            ;;
        *)
            error "Unsupported architecture: $(uname -m)"
            ;;
    esac
}

# Get the download URL for the installer
get_download_url() {
    local os=$1
    local arch=$2

    case "${os}_${arch}" in
        macos_arm64)
            echo "${BASE_URL}/hot_macos_arm64.pkg"
            ;;
        macos_x86_64)
            echo "${BASE_URL}/hot_macos_x86_64.pkg"
            ;;
        linux_arm64)
            echo "${BASE_URL}/hot_linux_arm64.deb"
            ;;
        linux_x86_64)
            echo "${BASE_URL}/hot_linux_x86_64.deb"
            ;;
        *)
            error "No installer available for ${os} ${arch}"
            ;;
    esac
}

# Download file
download() {
    local url=$1
    local output=$2

    if command -v curl &> /dev/null; then
        curl -fsSL "$url" -o "$output"
    elif command -v wget &> /dev/null; then
        wget -q "$url" -O "$output"
    else
        error "Neither curl nor wget found. Please install one of them."
    fi
}

# Install on macOS
install_macos() {
    local pkg_file=$1

    echo "Installing Hot..."
    $SUDO installer -pkg "$pkg_file" -target / || error "Installation failed"
}

# Install on Linux
install_linux() {
    local deb_file=$1

    echo "Installing Hot..."
    if command -v dpkg &> /dev/null; then
        $SUDO dpkg -i "$deb_file" || error "Installation failed"
    elif command -v apt &> /dev/null; then
        $SUDO apt install -y "$deb_file" || error "Installation failed"
    else
        error "Neither dpkg nor apt found. Please install the .deb package manually."
    fi
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

    # Detect platform
    local os=$(detect_os)
    local arch=$(detect_arch)
    echo "Detected platform: ${os} ${arch}"

    # Get download URL
    local url=$(get_download_url "$os" "$arch")
    local filename=$(basename "$url")
    local tmp_file="/tmp/${filename}"

    # Download
    echo "Downloading ${filename}..."
    download "$url" "$tmp_file"

    # Install based on OS
    case "$os" in
        macos)
            install_macos "$tmp_file"
            ;;
        linux)
            install_linux "$tmp_file"
            ;;
    esac

    # Cleanup
    rm -f "$tmp_file"

    # Verify installation
    echo ""
    if command -v hot &> /dev/null; then
        local version=$(hot version 2>/dev/null || echo "unknown")
        echo "Hot installed successfully!"
        echo "Version: ${version}"
    else
        echo "Installation completed, but 'hot' command not found in PATH."
        echo "You may need to restart your terminal or add Hot to your PATH."
    fi

    echo ""
    echo "Documentation: https://hot.dev/docs"
    echo ""
}

main "$@"
