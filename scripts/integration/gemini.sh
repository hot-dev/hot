#!/usr/bin/env bash
# Run Gemini integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-gemini"

print_header "Gemini" "$DELAY"

run_test "$PKG" "gemini::integration::models"
sleep "$DELAY"

run_test "$PKG" "gemini::integration::chat"
sleep "$DELAY"

run_test "$PKG" "gemini::integration::embeddings"
sleep "$DELAY"

run_test "$PKG" "gemini::integration::files"

exit_with_status
