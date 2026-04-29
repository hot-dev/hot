#!/bin/bash

### Build Linux binaries using Docker

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Building Hot Dev bundles for Linux using Docker..."
cd "$(dirname "$0")/../"

# Check if Docker is available
if ! command -v docker &> /dev/null; then
    echo "Error: Docker is not installed or not in PATH"
    echo "Please install Docker to use containerized builds"
    exit 1
fi

# Build the Docker image and extract binaries
echo "Building Docker image for Linux targets..."
docker build -f docker/build-linux.Dockerfile -t hot-linux-builder .

# Create output directory
mkdir -p target/docker-builds/linux

# Extract binaries from the Docker image builder stage
echo "Extracting Linux binaries..."
CONTAINER_ID=$(docker create hot-linux-builder:latest)
docker cp $CONTAINER_ID:/workspace/target/x86_64-unknown-linux-gnu/release/hot target/docker-builds/linux/hot-linux-x86_64
docker cp $CONTAINER_ID:/workspace/target/aarch64-unknown-linux-gnu/release/hot target/docker-builds/linux/hot-linux-aarch64
docker rm $CONTAINER_ID

# Make binaries executable
chmod +x target/docker-builds/linux/hot-linux-x86_64
chmod +x target/docker-builds/linux/hot-linux-aarch64

echo "Linux builds created successfully!"
echo "Builds location: target/docker-builds/linux/"
echo "  - hot-linux-x86_64 (Intel/AMD 64-bit)"
echo "  - hot-linux-aarch64 (ARM 64-bit)"

# Restore original directory
cd "$ORIGINAL_DIR"