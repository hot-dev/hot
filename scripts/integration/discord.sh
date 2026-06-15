#!/usr/bin/env bash
# Run Discord integration tests with pauses between test groups
# to respect Discord API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-discord"

print_header "Discord" "$DELAY"

run_test "$PKG" "discord::integration::channels"
sleep "$DELAY"

run_test "$PKG" "discord::integration::guilds"
sleep "$DELAY"

run_test "$PKG" "discord::integration::messages"
sleep "$DELAY"

run_test "$PKG" "discord::integration::users"
sleep "$DELAY"

run_test "$PKG" "discord::integration::webhooks"

exit_with_status
