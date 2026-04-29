#!/bin/bash

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Building Hot Dev bundles for macOS..."
cd "$(dirname "$0")/../"

# Build for macOS targets (native compilation)
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

echo "macOS builds created successfully!"
echo "Builds location: target/release/"

# Restore original directory
cd "$ORIGINAL_DIR"