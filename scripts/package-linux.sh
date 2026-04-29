#!/bin/bash

### Create .deb and .rpm packages for Linux builds using Docker

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Creating .deb and .rpm packages for Linux builds..."
cd "$(dirname "$0")/../"

# Check if the Linux builds exist
if [ ! -d "target/docker-builds/linux" ]; then
    echo "Error: Linux builds not found. Please run build-linux.sh first."
    exit 1
fi

if [ ! -f "target/docker-builds/linux/hot-linux-x86_64" ] || [ ! -f "target/docker-builds/linux/hot-linux-aarch64" ]; then
    echo "Error: Linux binaries not found. Please run build-linux.sh first."
    exit 1
fi

# Check if Docker is available
if ! command -v docker &> /dev/null; then
    echo "Error: Docker is not installed or not in PATH"
    echo "Please install Docker to use containerized packaging"
    exit 1
fi

# Create packaging directory
mkdir -p target/packages

# The packaging script and Dockerfile are already in docker/

# Build the packaging Docker image
echo "Building Docker image for Linux packaging..."
docker build --no-cache -f docker/package-linux.Dockerfile -t hot-linux-packager .

# Run packaging in container
echo "Running packaging in Docker container..."
docker run --rm \
    -v "$(pwd)/target/packages:/workspace/target/packages" \
    -v "$(pwd)/target/docker-builds:/workspace/target/docker-builds" \
    -v "$(pwd)/scripts:/workspace/scripts:ro" \
    -v "$(pwd)/resources:/workspace/resources:ro" \
    -v "$(pwd)/hot/pkg/hot-std:/workspace/hot/pkg/hot-std:ro" \
    hot-linux-packager

# Extract version for success message (from resources/version.txt)
VERSION=$(head -1 resources/version.txt | tr -d '[:space:]')

echo "Linux packaging completed successfully!"
echo "Packages created in target/packages/:"
ls -la target/packages/*.deb target/packages/*.rpm 2>/dev/null || echo "(no packages found)"

# Restore original directory
cd "$ORIGINAL_DIR"