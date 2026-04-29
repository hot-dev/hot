#!/usr/bin/env bash
# Run ALL published package integration tests.
#
# Usage:
#   ./scripts/integration/run-all.sh              # Run all packages
#   ./scripts/integration/run-all.sh ai            # Run AI packages only
#   ./scripts/integration/run-all.sh aws           # Run AWS packages only
#   ./scripts/integration/run-all.sh email         # Run email packages only
#   ./scripts/integration/run-all.sh messaging     # Run messaging packages only
#   ./scripts/integration/run-all.sh protocols     # Run protocol packages only
#   ./scripts/integration/run-all.sh anthropic xai # Run specific packages
#
# Environment variables:
#   DELAY=5           Override delay within each package script
#   PKG_DELAY=10      Delay between packages (default: 5s)

set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
PKG_DELAY=${PKG_DELAY:-5}

# ── Package groups ──────────────────────────────────────────────

AI_PKGS=(anthropic gemini openai xai)
AWS_PKGS=(aws-bedrock aws-dynamodb aws-lambda aws-s3 aws-secrets-manager aws-ses aws-sns aws-sqs)
EMAIL_PKGS=(postmark resend)
MESSAGING_PKGS=(slack telegram)
PROTOCOL_PKGS=(json-rpc mcp)

ALL_PKGS=("${AI_PKGS[@]}" "${AWS_PKGS[@]}" "${EMAIL_PKGS[@]}" "${MESSAGING_PKGS[@]}" "${PROTOCOL_PKGS[@]}")

# ── Resolve which packages to run ──────────────────────────────

resolve_packages() {
    local pkgs=()
    for arg in "$@"; do
        case "$arg" in
            ai)         pkgs+=("${AI_PKGS[@]}") ;;
            aws)        pkgs+=("${AWS_PKGS[@]}") ;;
            email)      pkgs+=("${EMAIL_PKGS[@]}") ;;
            messaging)  pkgs+=("${MESSAGING_PKGS[@]}") ;;
            protocols)  pkgs+=("${PROTOCOL_PKGS[@]}") ;;
            *)          pkgs+=("$arg") ;;
        esac
    done
    echo "${pkgs[@]}"
}

if [ $# -eq 0 ]; then
    PACKAGES=("${ALL_PKGS[@]}")
else
    # shellcheck disable=SC2207
    PACKAGES=($(resolve_packages "$@"))
fi

# ── Run ────────────────────────────────────────────────────────

TOTAL_PASS=0
TOTAL_FAIL=0
TOTAL_SKIP=0
RESULTS=()

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "  Integration Test Suite"
echo "  Packages: ${#PACKAGES[@]}  |  Delay between packages: ${PKG_DELAY}s"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Packages to run: ${PACKAGES[*]}"
echo ""

for i in "${!PACKAGES[@]}"; do
    pkg="${PACKAGES[$i]}"
    script="$DIR/$pkg.sh"

    if [ ! -f "$script" ]; then
        echo "⚠ No script found for '$pkg' (expected $script), skipping."
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

# ── Summary ────────────────────────────────────────────────────

echo ""
echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "  Integration Test Suite — Summary"
echo "╠══════════════════════════════════════════════════════════╣"
for result in "${RESULTS[@]}"; do
    echo "  $result"
done
echo "╠══════════════════════════════════════════════════════════╣"
echo "  Passed: $TOTAL_PASS  |  Failed: $TOTAL_FAIL  |  Skipped: $TOTAL_SKIP"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""

[ "$TOTAL_FAIL" -eq 0 ]
