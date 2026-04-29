#!/bin/bash

### Build Hot Dev for all supported platforms and architectures

set -e

# Save current directory
ORIGINAL_DIR=$(pwd)
cd "$(dirname "$0")/../"

echo "=========================================="
echo "Hot Dev - Multi-Platform Build"
echo "=========================================="
echo ""

# Extract version
VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
echo "Building Hot Dev version: $VERSION"
echo ""

# Supported targets
TARGETS=(
    "x86_64-unknown-linux-gnu:linux:x86_64"
    "aarch64-unknown-linux-gnu:linux:aarch64"
    "x86_64-apple-darwin:macos:x86_64"
    "aarch64-apple-darwin:macos:aarch64"
    "x86_64-pc-windows-msvc:windows:x86_64"
    "aarch64-pc-windows-msvc:windows:aarch64"
)

# Check if Rust toolchain is installed
if ! command -v cargo &> /dev/null; then
    echo "❌ Cargo not found. Please install Rust: https://rustup.rs/"
    exit 1
fi

# Function to build for a target
build_target() {
    local target_info="$1"
    IFS=':' read -r target platform arch <<< "$target_info"

    echo "Building for $platform $arch ($target)..."

    # Check if target is installed
    if ! rustup target list --installed | grep -q "$target"; then
        echo "  → Installing target $target..."
        if ! rustup target add "$target"; then
            echo "  ❌ Failed to install target $target"
            return 1
        fi
    fi

    # Special setup for cross-compilation
    local build_env=""
    case "$target" in
        "aarch64-unknown-linux-gnu")
            if [[ "$OSTYPE" == "linux-gnu"* ]]; then
                # Check if cross-compilation tools are available
                if command -v aarch64-linux-gnu-gcc &> /dev/null; then
                    build_env="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc"
                else
                    echo "  ⚠️  Cross-compilation linker not found. Install with:"
                    echo "     sudo apt-get install gcc-aarch64-linux-gnu"
                    echo "  ⚠️  Attempting build anyway (may fail)..."
                fi
            fi
            ;;
        "aarch64-pc-windows-msvc")
            if [[ "$OSTYPE" != "msys" && "$OSTYPE" != "win32" ]]; then
                echo "  ⚠️  Cross-compiling Windows ARM64 from non-Windows may fail"
            fi
            ;;
    esac

    # Build
    echo "  → Building..."
    if [ -n "$build_env" ]; then
        env $build_env cargo build --release --target "$target"
    else
        cargo build --release --target "$target"
    fi

    # Prepare output directory
    mkdir -p "target/builds/$platform"

    # Copy binary to organized structure
    local binary_name="hot"
    local output_name="hot-$platform-$arch"

    if [[ "$platform" == "windows" ]]; then
        binary_name="hot.exe"
        output_name="hot-$platform-$arch.exe"
    fi

    if [ -f "target/$target/release/$binary_name" ]; then
        cp "target/$target/release/$binary_name" "target/builds/$platform/$output_name"
        echo "  ✅ Built successfully: target/builds/$platform/$output_name"
        return 0
    else
        echo "  ❌ Build failed: binary not found"
        return 1
    fi
}

# Build summary tracking
successful_builds=0
total_builds=${#TARGETS[@]}
build_results_file=$(mktemp)

echo "Building for $total_builds targets..."
echo ""

# Build each target
for target_info in "${TARGETS[@]}"; do
    IFS=':' read -r target platform arch <<< "$target_info"

    if build_target "$target_info"; then
        echo "$platform-$arch:✅" >> "$build_results_file"
        ((successful_builds++))
    else
        echo "$platform-$arch:❌" >> "$build_results_file"
    fi
    echo ""
done

# Summary
echo "=========================================="
echo "Build Summary"
echo "=========================================="

if [ $successful_builds -eq $total_builds ]; then
    echo "🎉 All builds completed successfully! ($successful_builds/$total_builds)"
elif [ $successful_builds -gt 0 ]; then
    echo "⚠️  Partial build success ($successful_builds/$total_builds)"
else
    echo "❌ All builds failed"
fi

echo ""
echo "Build Results:"
for target_info in "${TARGETS[@]}"; do
    IFS=':' read -r target platform arch <<< "$target_info"
    result=$(grep "^$platform-$arch:" "$build_results_file" | cut -d: -f2)
    echo "  $platform $arch: $result"
done

echo ""

if [ $successful_builds -gt 0 ]; then
    echo "Built binaries in target/builds/:"
    for platform in linux macos windows; do
        if [ -d "target/builds/$platform" ]; then
            echo "  $platform/:"
            ls -la "target/builds/$platform/" 2>/dev/null | grep -v '^total' | sed 's/^/    /'
        fi
    done

    echo ""
    echo "Next steps:"
    echo "  - Run './scripts/package.sh' to create platform packages"
    echo "  - Or copy individual binaries from target/builds/"
fi

echo ""
echo "=========================================="

# Cleanup
rm -f "$build_results_file"

# Restore original directory
cd "$ORIGINAL_DIR"

# Exit with error if no builds succeeded
if [ $successful_builds -eq 0 ]; then
    exit 1
fi