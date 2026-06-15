#!/usr/bin/env bash
# Run Qwen integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-qwen"

print_header "Qwen" "$DELAY"

run_test "$PKG" "qwen::integration::models"
sleep "$DELAY"

run_test "$PKG" "qwen::integration::chat"
sleep "$DELAY"

run_test "$PKG" "qwen::integration::embeddings"

exit_with_status
