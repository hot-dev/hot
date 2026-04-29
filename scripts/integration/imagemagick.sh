#!/usr/bin/env bash
# Run ImageMagick box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-imagemagick"

print_header "ImageMagick (box)" "0"

run_box_test "$PKG" "imagemagick::integration::test"

exit_with_status
