#!/usr/bin/env bash
# Run Resend integration tests with pauses between test groups
# to respect the 2 req/sec API rate limit.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-5}
PKG="pkg-integration-resend"

print_header "Resend" "$DELAY"

run_test "$PKG" "resend::integration::emails::test-send-and-retrieve-email"
sleep "$DELAY"

run_test "$PKG" "resend::integration::emails::test-send-batch-emails"
sleep "$DELAY"

run_test "$PKG" "resend::integration::api-keys"
sleep "$DELAY"

run_test "$PKG" "resend::integration::domains"
sleep "$DELAY"

run_test "$PKG" "resend::integration::audiences::test-list-audiences"
sleep "$DELAY"

run_test "$PKG" "resend::integration::audiences::test-audience-create-and-delete"

exit_with_status
