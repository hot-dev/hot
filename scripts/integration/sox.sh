#!/usr/bin/env bash
# Run SoX box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-sox"

print_header "SoX (box)" "0"

run_box_test "$PKG" "sox::integration::test"

exit_with_status
