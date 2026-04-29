#!/bin/bash
# Sync versions across the project
# - hot-std pkg.hot version with resources/version.txt
# - README.md installation sections with their pkg.hot versions
#
# Run this after bumping versions in resources/version.txt or any pkg.hot

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VERSION_FILE="$PROJECT_ROOT/resources/version.txt"
PKG_FILE="$PROJECT_ROOT/hot/pkg/hot-std/pkg.hot"
CARGO_TOML="$PROJECT_ROOT/Cargo.toml"

if [ ! -f "$VERSION_FILE" ]; then
    echo "ERROR: resources/version.txt not found"
    exit 1
fi

if [ ! -f "$PKG_FILE" ]; then
    echo "ERROR: hot/pkg/hot-std/pkg.hot not found"
    exit 1
fi

VERSION=$(head -1 "$VERSION_FILE" | tr -d '[:space:]')

if [ -z "$VERSION" ]; then
    echo "ERROR: Could not extract version from resources/version.txt"
    exit 1
fi

# Update the workspace version in Cargo.toml (used by cargo-deb, cargo-bundle, etc.)
if [[ "$OSTYPE" == "darwin"* ]]; then
    sed -i '' "1,/^version = /s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$CARGO_TOML"
    sed -i '' "s/version: \"[^\"]*\"/version: \"$VERSION\"/" "$PKG_FILE"
else
    sed -i "1,/^version = /s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$CARGO_TOML"
    sed -i "s/version: \"[^\"]*\"/version: \"$VERSION\"/" "$PKG_FILE"
fi

echo "✓ Updated Cargo.toml workspace version to: $VERSION"
echo "✓ Updated hot/pkg/hot-std/pkg.hot to version: $VERSION"

# Sync README.md installation sections with pkg.hot versions
echo ""
echo "Syncing README.md installation sections..."

PKG_DIR="$PROJECT_ROOT/hot/pkg"
updated=0

for pkg_path in "$PKG_DIR"/*/; do
    pkg=$(basename "$pkg_path")
    readme="$pkg_path/README.md"
    pkg_hot="$pkg_path/pkg.hot"

    # Skip if no README or no Installation section
    if [ ! -f "$readme" ] || ! grep -q '## Installation' "$readme" 2>/dev/null; then
        continue
    fi

    # Skip hot-std (bundled with CLI)
    if [ "$pkg" = "hot-std" ]; then
        continue
    fi

    # Get version from pkg.hot
    if [ -f "$pkg_hot" ]; then
        pkg_version=$(grep -oE 'version:\s*"[^"]+"' "$pkg_hot" | grep -oE '"[^"]+"' | tr -d '"' | head -1)
    else
        pkg_version="0.1.0"
    fi

    [ -z "$pkg_version" ] && pkg_version="0.1.0"

    # Update README installation section
    perl -i -0777 -pe "s/## Installation\n\n.*?\`\`\`hot\n.*?\`\`\`/## Installation\n\nAdd this to the \`deps\` in your \`hot.hot\` file:\n\n\`\`\`hot\n\"hot.dev\/$pkg\": \"$pkg_version\"\n\`\`\`/s" "$readme"

    updated=$((updated + 1))
done

echo "✓ Synced $updated README.md files"
