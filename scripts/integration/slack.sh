#!/usr/bin/env bash
# Run Slack integration tests with pauses between test groups
# to respect API rate limits (Tier 1: ~1 req/sec).
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-slack"

print_header "Slack" "$DELAY"

run_test "$PKG" "slack::integration::auth"
sleep "$DELAY"

run_test "$PKG" "slack::integration::users"
sleep "$DELAY"

run_test "$PKG" "slack::integration::channels"
sleep "$DELAY"

run_test "$PKG" "slack::integration::messaging"
sleep "$DELAY"

run_test "$PKG" "slack::integration::files"
sleep "$DELAY"

run_test "$PKG" "slack::integration::apps"
sleep "$DELAY"

run_test "$PKG" "slack::integration::calls"

exit_with_status
