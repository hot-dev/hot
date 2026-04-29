#!/bin/bash

### Create Windows NSIS installers using Docker

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Creating Windows installer packages..."
cd "$(dirname "$0")/../"

# Check if the Windows builds exist
if [ ! -d "target/docker-builds/windows" ]; then
    echo "Error: Windows builds not found. Please run build-win.sh first."
    exit 1
fi

if [ ! -f "target/docker-builds/windows/hot-windows-x86_64.exe" ]; then
    echo "Error: Windows x86_64 binary not found. Please run build-win.sh first."
    exit 1
fi

# Check if Docker is available
if ! command -v docker &> /dev/null; then
    echo "Error: Docker is not installed or not in PATH"
    echo "Please install Docker to use containerized packaging"
    exit 1
fi

# Create packaging directories
mkdir -p target/packages

# Build the packaging Docker image
echo "Building Docker image for Windows packaging..."
docker build -f docker/package-windows.Dockerfile -t hot-windows-packager .

# Prepare certificate mount if available
CERT_MOUNT=""
if [ -d "certs" ] && [ -f "certs/codesign.pfx" ]; then
    echo "Code signing certificate found, will sign installer."
    CERT_MOUNT="-v $(pwd)/certs:/workspace/certs"
fi

# Pass code signing password if set
SIGN_ENV=""
if [ -n "$CODESIGN_PASSWORD" ]; then
    SIGN_ENV="-e CODESIGN_PASSWORD=$CODESIGN_PASSWORD"
fi

# Run packaging in container
echo "Running packaging in Docker container..."
docker run --rm \
    -v "$(pwd)/target/packages:/workspace/target/packages" \
    -v "$(pwd)/target/docker-builds:/workspace/target/docker-builds" \
    -v "$(pwd)/resources:/workspace/resources:ro" \
    -v "$(pwd)/hot/pkg/hot-std:/workspace/hot/pkg/hot-std:ro" \
    -v "$(pwd)/Cargo.toml:/workspace/Cargo.toml:ro" \
    $CERT_MOUNT \
    $SIGN_ENV \
    hot-windows-packager

# Extract version for success message (from resources/version.txt)
VERSION=$(head -1 resources/version.txt | tr -d '[:space:]')

echo ""
echo "=========================================="
echo "Windows Packaging Complete"
echo "=========================================="
echo ""
echo "Installers created in target/packages/:"
ls -la target/packages/*.exe 2>/dev/null || echo "(no installers found)"

echo ""
echo "To install on Windows:"
echo "  1. Copy the installer to a Windows machine"
echo "  2. Run the installer as Administrator"
echo "  3. The 'hot' command will be available after restarting your terminal"
echo ""

# Restore original directory
cd "$ORIGINAL_DIR"
