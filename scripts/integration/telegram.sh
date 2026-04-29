#!/usr/bin/env bash
# Run Telegram integration tests with pauses between test groups
# to respect Bot API rate limits (~30 req/sec, but be conservative).
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-telegram"

print_header "Telegram" "$DELAY"

run_test "$PKG" "telegram::integration::bot"
sleep "$DELAY"

run_test "$PKG" "telegram::integration::chat"
sleep "$DELAY"

run_test "$PKG" "telegram::integration::messages"
sleep "$DELAY"

run_test "$PKG" "telegram::integration::updates"

exit_with_status
