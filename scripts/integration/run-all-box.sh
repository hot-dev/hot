#!/usr/bin/env bash
# Run ALL box package integration tests (requires Docker).
#
# Usage:
#   ./scripts/integration/run-all-box.sh                     # Run all box packages
#   ./scripts/integration/run-all-box.sh playwright ffmpeg    # Run specific packages
#
# Environment variables:
#   PKG_DELAY=3       Delay between packages (default: 3s, allows Docker cleanup)

set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
PKG_DELAY=${PKG_DELAY:-3}

# ── Package list ──────────────────────────────────────────────

BOX_PKGS=(ffmpeg imagemagick libreoffice pandoc playwright sox tesseract whisper)

# ── Resolve which packages to run ─────────────────────────────

if [ $# -eq 0 ]; then
    PACKAGES=("${BOX_PKGS[@]}")
else
    PACKAGES=("$@")
fi

# ── Pre-flight: Docker check ─────────────────────────────────

if ! docker info > /dev/null 2>&1; then
    echo "ERROR: Docker is not running. Box integration tests require Docker."
    exit 1
fi

# ── Run ───────────────────────────────────────────────────────

TOTAL_PASS=0
TOTAL_FAIL=0
TOTAL_SKIP=0
RESULTS=()

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "  Box Integration Test Suite"
echo "  Packages: ${#PACKAGES[@]}  |  Delay between packages: ${PKG_DELAY}s"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Packages to run: ${PACKAGES[*]}"
echo ""

for i in "${!PACKAGES[@]}"; do
    pkg="${PACKAGES[$i]}"
    script="$DIR/$pkg.sh"

    if [ ! -f "$script" ]; then
        echo "  No script found for '$pkg' (expected $script), skipping."
        RESULTS+=("SKIP  $pkg")
        TOTAL_SKIP=$((TOTAL_SKIP + 1))
        continue
    fi

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  [$(( i + 1 ))/${#PACKAGES[@]}]  $pkg"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

    if bash "$script"; then
        RESULTS+=("PASS  $pkg")
        TOTAL_PASS=$((TOTAL_PASS + 1))
    else
        RESULTS+=("FAIL  $pkg")
        TOTAL_FAIL=$((TOTAL_FAIL + 1))
    fi

    # Delay between packages (skip after the last one)
    if [ "$i" -lt "$(( ${#PACKAGES[@]} - 1 ))" ]; then
        echo ""
        echo "  Waiting ${PKG_DELAY}s before next package..."
        sleep "$PKG_DELAY"
    fi
done

# ── Summary ───────────────────────────────────────────────────

echo ""
echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "  Box Integration Test Suite — Summary"
echo "╠══════════════════════════════════════════════════════════╣"
for result in "${RESULTS[@]}"; do
    echo "  $result"
done
echo "╠══════════════════════════════════════════════════════════╣"
echo "  Passed: $TOTAL_PASS  |  Failed: $TOTAL_FAIL  |  Skipped: $TOTAL_SKIP"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""

[ "$TOTAL_FAIL" -eq 0 ]
