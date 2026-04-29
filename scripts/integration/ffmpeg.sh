#!/usr/bin/env bash
# Run FFmpeg box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-ffmpeg"

print_header "FFmpeg (box)" "0"

run_box_test "$PKG" "ffmpeg::integration::test"

exit_with_status
