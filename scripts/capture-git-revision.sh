#!/usr/bin/env bash
# Capture git revision to a file that will be included in the build

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Get git revision
GIT_REVISION=$(git -C "$ROOT_DIR" rev-parse --verify HEAD 2>/dev/null || true)
if [ -z "$GIT_REVISION" ]; then
    GIT_REVISION="unknown"
fi

# Write to resources directory
mkdir -p "$ROOT_DIR/resources"
printf '%s' "$GIT_REVISION" > "$ROOT_DIR/resources/git-revision.txt"

echo "Captured git revision: $GIT_REVISION"

