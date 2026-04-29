#!/bin/bash
# Generate a static package CDN directory structure
#
# Creates:
#   <output>/<namespace>/<package>/<version>.tar.gz    (includes docs.json)
#   <output>/<namespace>/<package>/<version>-docs.json (standalone for web)
#   <output>/<namespace>/<package>/versions.json
#
# Usage:
#   ./hot-pkg-cdn.sh [output_dir]
#
# Default output: ./cdn-output
#
# Only packages listed in hot/pkg/pkg-publish.txt are published.
# Set PACKAGE_CDN_NAMESPACE to change the top-level namespace directory.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PKG_SOURCE="$PROJECT_ROOT/hot/pkg"
PKG_DOCS="$PROJECT_ROOT/resources/pkg-docs"
OUTPUT_DIR="${1:-$PROJECT_ROOT/cdn-output}"
PACKAGE_CDN_NAMESPACE="${PACKAGE_CDN_NAMESPACE:-hot.dev}"
PUBLISH_LIST="$PKG_SOURCE/pkg-publish.txt"

echo "Generating CDN structure in $OUTPUT_DIR..."

# Load the publish list
if [ ! -f "$PUBLISH_LIST" ]; then
    echo "Error: pkg-publish.txt not found at $PUBLISH_LIST"
    echo "Create this file with one package name per line to control publishing."
    exit 1
fi

# Parse the publish list (strip comments and empty lines)
PUBLISHED_PACKAGES=()
while IFS= read -r line; do
    # Strip leading/trailing whitespace
    line=$(echo "$line" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    # Skip empty lines and comments
    if [ -n "$line" ] && [[ ! "$line" =~ ^# ]]; then
        PUBLISHED_PACKAGES+=("$line")
    fi
done < "$PUBLISH_LIST"

echo "Publishing ${#PUBLISHED_PACKAGES[@]} packages from pkg-publish.txt"

# Check if a package is in the publish list
is_published() {
    local pkg="$1"
    for published_pkg in "${PUBLISHED_PACKAGES[@]}"; do
        if [ "$pkg" = "$published_pkg" ]; then
            return 0
        fi
    done
    return 1
}

# Clean and create output directory
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/$PACKAGE_CDN_NAMESPACE"

# Create a temporary directory for package preparation
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Track all packages for summary
declare -a PACKAGES
declare -a SKIPPED_PACKAGES
DOCS_COUNT=0

for pkg_path in "$PKG_SOURCE"/*/; do
    pkg=$(basename "$pkg_path")

    # Skip hot-std (it's bundled with the CLI)
    if [ "$pkg" = "hot-std" ]; then
        continue
    fi

    # Skip packages not in the publish list
    if ! is_published "$pkg"; then
        SKIPPED_PACKAGES+=("$pkg")
        continue
    fi

    pkg_hot="$pkg_path/pkg.hot"
    if [ ! -f "$pkg_hot" ]; then
        echo "  Skipping $pkg (no pkg.hot)"
        continue
    fi

    # Extract version from pkg.hot
    VERSION=$(grep 'version:' "$pkg_hot" | head -1 | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')

    if [ -z "$VERSION" ]; then
        echo "  Skipping $pkg (no version found)"
        continue
    fi

    PKG_OUT="$OUTPUT_DIR/$PACKAGE_CDN_NAMESPACE/$pkg"
    mkdir -p "$PKG_OUT"

    # Prepare package directory with docs
    PKG_TEMP="$TEMP_DIR/$pkg"
    rm -rf "$PKG_TEMP"
    cp -r "$pkg_path" "$PKG_TEMP"

    # Check for pre-generated docs
    DOCS_SOURCE="$PKG_DOCS/$pkg/$VERSION/docs.json"
    HAS_DOCS=false
    if [ -f "$DOCS_SOURCE" ]; then
        # Copy docs.json into the package
        cp "$DOCS_SOURCE" "$PKG_TEMP/docs.json"
        HAS_DOCS=true
        DOCS_COUNT=$((DOCS_COUNT + 1))
    fi

    # Create tarball (from temp dir so it includes docs.json)
    TARBALL="$PKG_OUT/$VERSION.tar.gz"
    tar -czf "$TARBALL" -C "$TEMP_DIR" "$pkg"

    SIZE=$(ls -lh "$TARBALL" | awk '{print $5}')
    DOCS_INDICATOR=""
    if [ "$HAS_DOCS" = true ]; then
        DOCS_INDICATOR=" +docs"
        # Also create standalone docs.json for web display
        cp "$DOCS_SOURCE" "$PKG_OUT/$VERSION-docs.json"
    fi
    echo "  $pkg/$VERSION ($SIZE)$DOCS_INDICATOR"

    PACKAGES+=("$pkg")

    # Create or update versions.json
    VERSIONS_FILE="$PKG_OUT/versions.json"
    if [ -f "$VERSIONS_FILE" ]; then
        # Add version to existing file if not already present
        if ! grep -q "\"$VERSION\"" "$VERSIONS_FILE"; then
            # Use jq if available, otherwise recreate
            if command -v jq &> /dev/null; then
                jq --arg v "$VERSION" '.versions += [$v] | .latest = $v' "$VERSIONS_FILE" > "$VERSIONS_FILE.tmp"
                mv "$VERSIONS_FILE.tmp" "$VERSIONS_FILE"
            fi
        fi
    else
        # Create new versions.json
        cat > "$VERSIONS_FILE" << EOF
{
  "latest": "$VERSION",
  "versions": ["$VERSION"]
}
EOF
    fi
done

echo ""
echo "=== Summary ==="
echo "Output directory: $OUTPUT_DIR"
echo "Published packages: ${#PACKAGES[@]}"
echo "Packages with docs: $DOCS_COUNT"
if [ ${#SKIPPED_PACKAGES[@]} -gt 0 ]; then
    echo "Skipped (not in pkg-publish.txt): ${#SKIPPED_PACKAGES[@]}"
fi
echo ""
echo "Directory structure:"
echo "  $PACKAGE_CDN_NAMESPACE/"
for pkg in "${PACKAGES[@]}"; do
    echo "    $pkg/"
    ls "$OUTPUT_DIR/$PACKAGE_CDN_NAMESPACE/$pkg" | sed 's/^/      /'
done

echo ""
echo "To upload to object storage:"
echo "  PACKAGE_CDN_BUCKET=your-bucket ./scripts/hot-pkg-cdn-upload.sh $OUTPUT_DIR"
echo ""
echo "To test locally:"
echo "  cd $OUTPUT_DIR && python3 -m http.server 8080"
