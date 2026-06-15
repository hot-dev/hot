#!/usr/bin/env bash
# Run Twilio integration tests with pauses between test groups
# to respect Twilio API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-twilio"

print_header "Twilio" "$DELAY"

run_test "$PKG" "twilio::integration::accounts"
sleep "$DELAY"

run_test "$PKG" "twilio::integration::calls"
sleep "$DELAY"

run_test "$PKG" "twilio::integration::messages"

exit_with_status
