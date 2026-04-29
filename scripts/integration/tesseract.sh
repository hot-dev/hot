#!/usr/bin/env bash
# Run Tesseract box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-tesseract"

print_header "Tesseract (box)" "0"

run_box_test "$PKG" "tesseract::integration::test"

exit_with_status
