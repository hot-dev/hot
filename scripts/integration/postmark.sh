#!/usr/bin/env bash
# Run Postmark integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-5}
PKG="pkg-integration-postmark"

print_header "Postmark" "$DELAY"

run_test "$PKG" "postmark::integration::send"
sleep "$DELAY"

run_test "$PKG" "postmark::integration::server"
sleep "$DELAY"

run_test "$PKG" "postmark::integration::templates"

exit_with_status
