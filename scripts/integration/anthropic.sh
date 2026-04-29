#!/usr/bin/env bash
# Run Anthropic integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-anthropic"

print_header "Anthropic" "$DELAY"

run_test "$PKG" "anthropic::integration::models"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::messages"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::batches"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::beta"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::files"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::skills"
sleep "$DELAY"

run_test "$PKG" "anthropic::integration::prompt::caching"

exit_with_status
