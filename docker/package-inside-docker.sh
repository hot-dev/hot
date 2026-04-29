#!/bin/bash
set -e

echo "Creating Linux packages..."

# Extract version from resources/version.txt (single source of truth)
VERSION=$(head -1 resources/version.txt | tr -d '[:space:]')
echo "Package version: $VERSION"

# Function to create packages for a specific architecture
create_packages_for_arch() {
    local arch=$1
    local binary_name=$2
    local target_triple=$3

    echo "Creating packages for $arch..."

    # Create .deb package
    echo "Creating .deb package for $arch..."
    # Create target-specific directory structure for cargo-deb
    mkdir -p "target/$target_triple/release"
    mkdir -p "target/release"
    cp "target/docker-builds/linux/$binary_name" "target/$target_triple/release/hot"
    cp "target/docker-builds/linux/$binary_name" "target/release/hot"
    chmod +x "target/$target_triple/release/hot"
    chmod +x "target/release/hot"
    # Copy resources needed at runtime to crate directory for cargo-deb asset resolution:
    # - db/ for migrations
    # - pkg/ for hot-std (standard library)
    # - ai/ for AI AGENTS.md and skills
    # - init/ for project initialization templates
    # - LICENSE.md for license display
    mkdir -p "crates/hot_cli/resources/db"
    cp -r "resources/db/"* "crates/hot_cli/resources/db/"
    mkdir -p "crates/hot_cli/scripts"
    cp "scripts/install-completions.sh" "crates/hot_cli/scripts/install-completions.sh"
    # Copy hot-std package (standard library)
    mkdir -p "crates/hot_cli/pkg"
    cp -r "hot/pkg/hot-std" "crates/hot_cli/pkg/"
    # Copy app assets (CSS, JS, images for dev server)
    mkdir -p "crates/hot_cli/resources/app"
    cp -r "resources/app/"* "crates/hot_cli/resources/app/"
    # Copy AI resources (AGENTS.md and skills)
    mkdir -p "crates/hot_cli/resources/ai"
    cp -r "resources/ai/"* "crates/hot_cli/resources/ai/"
    # Copy init templates
    mkdir -p "crates/hot_cli/resources/init"
    cp -r "resources/init/"* "crates/hot_cli/resources/init/"
    # Copy hotbox Linux binaries (for container tasks)
    mkdir -p "crates/hot_cli/resources/bin"
    for hb in target/docker-builds/linux/hotbox-linux-*; do
        [ -f "$hb" ] && cp "$hb" "crates/hot_cli/resources/bin/"
    done
    # Copy license and notice files
    cp "LICENSE" "crates/hot_cli/resources/LICENSE"
    cp "NOTICE" "crates/hot_cli/resources/NOTICE"
    cd crates/hot_cli
    cargo deb --target "$target_triple" --no-build --no-strip --output "../../target/packages/hot_${VERSION}_${arch}.deb"
    cd ../..

    # RPM packaging is intentionally disabled until the release flow enables it.
    echo "SKIPPING: .rpm package for $arch"
    cd crates/hot_cli
    #cargo generate-rpm --target "$target_triple" --payload-compress none --output "../../target/packages/hot_${VERSION}_${arch}.rpm"
    cd ../..

    # Clean up
    rm -rf "crates/hot_cli/target" "crates/hot_cli/resources" "crates/hot_cli/scripts" "crates/hot_cli/pkg" "target/release" "target/$target_triple"
}

# Create packages for x86_64
create_packages_for_arch "x86_64" "hot-linux-x86_64" "x86_64-unknown-linux-gnu"

# Create packages for aarch64
create_packages_for_arch "aarch64" "hot-linux-aarch64" "aarch64-unknown-linux-gnu"

echo "Linux packaging completed successfully!"
