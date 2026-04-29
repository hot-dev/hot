#!/bin/bash

### Build script for all platforms
### This script calls individual platform-specific build scripts

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)
SCRIPT_DIR="$(dirname "$0")"

echo "Building Hot Dev for all platforms..."

# Build for macOS (working)
echo "===================="
echo "Building for macOS..."
echo "===================="
"$SCRIPT_DIR/build-mac.sh"

# Build for Linux (using Docker)
echo ""
echo "======================"
echo "Building for Linux..."
echo "======================"
"$SCRIPT_DIR/build-linux.sh"

# Build for Windows (using Docker with mingw-w64)
echo ""
echo "========================"
echo "Building for Windows..."
echo "========================"
"$SCRIPT_DIR/build-win.sh"

echo ""
echo "Build process completed!"
echo "Successfully built for all platforms:"
echo "  - macOS: target/release/ (native builds)"
echo "  - Linux: target/docker-builds/linux/ (Docker builds)"
echo "  - Windows: target/docker-builds/windows/ (Docker builds)"
echo ""
echo "Individual platform builds can be run with:"
echo "  - macOS: ./scripts/build-mac.sh"
echo "  - Linux: ./scripts/build-linux.sh (requires Docker)"
echo "  - Windows: ./scripts/build-win.sh (requires Docker)"

# Restore original directory
cd "$ORIGINAL_DIR"