#!/usr/bin/env bash
# Run Whisper box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-whisper"

print_header "Whisper (box)" "0"

run_box_test "$PKG" "whisper::integration::test"

exit_with_status
