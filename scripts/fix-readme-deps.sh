#!/bin/bash
# Sync README.md installation sections with versions from pkg.hot
#
# This script updates the Installation section in README.md files to use
# the simple version format and sync the version from pkg.hot.
#
# Usage:
#   ./fix-readme-deps.sh           # Update all packages
#   ./fix-readme-deps.sh anthropic # Update specific package

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PKG_DIR="$PROJECT_ROOT/hot/pkg"

# If a specific package is provided, only update that one
SPECIFIC_PKG="$1"

echo "Syncing README.md installation sections..."

updated=0
skipped=0

for pkg_path in "$PKG_DIR"/*/; do
    pkg=$(basename "$pkg_path")
    readme="$pkg_path/README.md"
    pkg_hot="$pkg_path/pkg.hot"

    # If specific package requested, skip others
    if [ -n "$SPECIFIC_PKG" ] && [ "$pkg" != "$SPECIFIC_PKG" ]; then
        continue
    fi

    # Skip if no README
    if [ ! -f "$readme" ]; then
        continue
    fi

    # Skip hot-std (bundled with CLI, no installation needed)
    if [ "$pkg" = "hot-std" ]; then
        continue
    fi

    # Skip if README doesn't have an Installation section
    if ! grep -q '## Installation' "$readme" 2>/dev/null; then
        skipped=$((skipped + 1))
        continue
    fi

    # Get version from pkg.hot
    if [ -f "$pkg_hot" ]; then
        version=$(grep -oE 'version:\s*"[^"]+"' "$pkg_hot" | grep -oE '"[^"]+"' | tr -d '"' | head -1)
    else
        version="0.1.0"
    fi

    if [ -z "$version" ]; then
        version="0.1.0"
    fi

    # Use perl for multi-line replacement
    perl -i -0777 -pe "s/## Installation\n\n.*?\`\`\`hot\n.*?\`\`\`/## Installation\n\nAdd this to the \`deps\` in your \`hot.hot\` file:\n\n\`\`\`hot\n\"hot.dev\/$pkg\": \"$version\"\n\`\`\`/s" "$readme"

    echo "  Updated: $pkg (version $version)"
    updated=$((updated + 1))
done

echo ""
echo "Done! Updated $updated READMEs, skipped $skipped (no Installation section)"
