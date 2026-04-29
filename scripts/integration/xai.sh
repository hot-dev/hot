#!/usr/bin/env bash
# Run xAI integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-xai"

print_header "xAI" "$DELAY"

run_test "$PKG" "xai::integration::models"
sleep "$DELAY"

run_test "$PKG" "xai::integration::responses"
sleep "$DELAY"

run_test "$PKG" "xai::integration::collections"

exit_with_status
