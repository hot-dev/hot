#!/bin/bash
# pkg-docs.sh - Generate package documentation for all published packages
#
# Usage:
#   ./scripts/pkg-docs.sh           # Generate docs for all published packages
#   ./scripts/pkg-docs.sh slack     # Generate docs for a specific package
#
# Reads enabled (uncommented) packages from hot/pkg/pkg-publish.txt
# Output goes to resources/pkg-docs/

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PUBLISH_FILE="$PROJECT_ROOT/hot/pkg/pkg-publish.txt"

if [ ! -f "$PUBLISH_FILE" ]; then
  echo "Error: $PUBLISH_FILE not found"
  exit 1
fi

if [ -n "$1" ]; then
  # Single package mode
  echo "Generating docs for: $1"
  cargo run -- docs --pkg "$1"
else
  # All published packages
  PKG_ARGS=""
  while IFS= read -r pkg; do
    [[ "$pkg" =~ ^#.*$ ]] && continue
    [[ -z "$pkg" ]] && continue
    pkg=$(echo "$pkg" | xargs)  # trim whitespace
    PKG_ARGS="$PKG_ARGS --pkg $pkg"
  done < "$PUBLISH_FILE"

  if [ -z "$PKG_ARGS" ]; then
    echo "No packages enabled in $PUBLISH_FILE"
    exit 0
  fi

  echo "Generating docs for all published packages..."
  echo "Packages:$PKG_ARGS"
  echo ""
  cargo run -- docs $PKG_ARGS
fi

echo ""
echo "Docs generated in resources/pkg-docs/"
