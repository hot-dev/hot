#!/bin/bash

### Build Windows binaries using Docker with mingw-w64 cross-compilation

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Building Hot Dev bundles for Windows using Docker..."
cd "$(dirname "$0")/../"

# Check if Docker is available
if ! command -v docker &> /dev/null; then
    echo "Error: Docker is not installed or not in PATH"
    echo "Please install Docker to use containerized builds"
    exit 1
fi

# Build the Docker image and extract binaries
echo "Building Docker image for Windows targets..."
docker build -f docker/build-windows.Dockerfile -t hot-windows-builder .

# Create output directory
mkdir -p target/docker-builds/windows

# Extract binaries from the Docker image builder stage
echo "Extracting Windows binaries..."
CONTAINER_ID=$(docker create hot-windows-builder:latest)
docker cp $CONTAINER_ID:/workspace/target/x86_64-pc-windows-gnu/release/hot.exe target/docker-builds/windows/hot-windows-x86_64.exe
docker rm $CONTAINER_ID

# Make binaries executable (not necessary for Windows, but good practice)
chmod +x target/docker-builds/windows/hot-windows-x86_64.exe

echo "Windows builds created successfully!"
echo "Builds location: target/docker-builds/windows/"
echo "  - hot-windows-x86_64.exe (64-bit)"

# Restore original directory
cd "$ORIGINAL_DIR"