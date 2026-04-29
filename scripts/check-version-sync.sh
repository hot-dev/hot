#!/bin/bash
# Check that hot-std pkg.hot version matches resources/version.txt
# This ensures hot-std version stays in sync with the engine version

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VERSION_FILE="$PROJECT_ROOT/resources/version.txt"
PKG_FILE="$PROJECT_ROOT/hot/pkg/hot-std/pkg.hot"

if [ ! -f "$VERSION_FILE" ]; then
    echo "ERROR: resources/version.txt not found"
    exit 1
fi

if [ ! -f "$PKG_FILE" ]; then
    echo "ERROR: hot/pkg/hot-std/pkg.hot not found"
    exit 1
fi

# Extract versions
VERSION=$(head -1 "$VERSION_FILE" | tr -d '[:space:]')
PKG_VERSION=$(grep 'version:' "$PKG_FILE" | head -1 | sed 's/.*version: *"\([^"]*\)".*/\1/')

if [ -z "$VERSION" ]; then
    echo "ERROR: Could not extract version from resources/version.txt"
    exit 1
fi

if [ -z "$PKG_VERSION" ]; then
    echo "ERROR: Could not extract version from hot/pkg/hot-std/pkg.hot"
    exit 1
fi

if [ "$VERSION" != "$PKG_VERSION" ]; then
    echo "ERROR: Version mismatch!"
    echo "  resources/version.txt:     $VERSION"
    echo "  hot/pkg/hot-std/pkg.hot:   $PKG_VERSION"
    echo ""
    echo "To fix, update hot/pkg/hot-std/pkg.hot to use version: \"$VERSION\""
    echo "Or run: scripts/sync-version.sh"
    exit 1
fi

echo "✓ Versions in sync: $VERSION"
