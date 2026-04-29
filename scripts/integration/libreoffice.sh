#!/usr/bin/env bash
# Run LibreOffice box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-libreoffice"

print_header "LibreOffice (box)" "0"

run_box_test "$PKG" "libreoffice::integration::test"

exit_with_status
