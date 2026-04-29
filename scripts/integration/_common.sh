#!/usr/bin/env bash
# Shared helpers for integration test scripts.
# Source this file from individual package scripts:
#   source "$(dirname "$0")/_common.sh"

set -e

PASS=0
FAIL=0
TOTAL=0

run_test() {
    local pkg="$1"
    local filter="$2"
    TOTAL=$((TOTAL + 1))
    echo "──────────────────────────────────────────"
    echo "Running: $filter"
    echo "──────────────────────────────────────────"
    if cargo run test -p "$pkg" "$filter"; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

run_box_test() {
    local pkg="$1"
    local filter="$2"
    TOTAL=$((TOTAL + 1))
    echo "──────────────────────────────────────────"
    echo "Running (box): $filter"
    echo "──────────────────────────────────────────"
    if cargo run test --integration -p "$pkg" "$filter"; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

print_header() {
    local name="$1"
    local delay="$2"
    echo ""
    echo "╔══════════════════════════════════════════╗"
    echo "  $name Integration Tests"
    echo "  (${delay}s delay between test groups)"
    echo "╚══════════════════════════════════════════╝"
    echo ""
}

print_results() {
    echo ""
    echo "══════════════════════════════════════════"
    echo "Results: $PASS passed, $FAIL failed (of $TOTAL)"
    echo "══════════════════════════════════════════"
}

exit_with_status() {
    print_results
    [ "$FAIL" -eq 0 ]
}
