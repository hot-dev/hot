#!/usr/bin/env bash
# Run OpenAI integration tests with pauses between test groups
# to respect API rate limits.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-openai"

print_header "OpenAI" "$DELAY"

run_test "$PKG" "openai::integration::models"
sleep "$DELAY"

run_test "$PKG" "openai::integration::chat"
sleep "$DELAY"

run_test "$PKG" "openai::integration::embeddings"
sleep "$DELAY"

run_test "$PKG" "openai::integration::moderations"
sleep "$DELAY"

run_test "$PKG" "openai::integration::audio"
sleep "$DELAY"

run_test "$PKG" "openai::integration::images"

exit_with_status
