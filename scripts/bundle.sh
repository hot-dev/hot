#!/bin/bash

# Convenience script to run cargo bundle from workspace root
# This script changes to the hot_cli directory and runs cargo bundle

### Install cross-compilation toolchains

#brew tap messense/macos-cross-toolchains
# install x86_64-unknown-linux-gnu toolchain
#brew install x86_64-unknown-linux-gnu
# install aarch64-unknown-linux-gnu toolchain
#brew install aarch64-unknown-linux-gnu

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)

echo "Building Hot Dev bundles for macOS, Linux, and Windows..."
cd "$(dirname "$0")/../crates/hot_cli"

cargo bundle --release --target aarch64-apple-darwin
cargo bundle --release --target x86_64-apple-darwin


# cargo bundle --release --target aarch64-unknown-linux-gnu
# cargo bundle --release --target x86_64-unknown-linux-gnu

# cargo bundle --release --target aarch64-pc-windows-msvc
# cargo bundle --release --target x86_64-pc-windows-msvc

echo "Bundles created successfully!"
echo "Bundles location: crates/hot_cli/target/bundle/"

# Restore original directory
cd "$ORIGINAL_DIR"