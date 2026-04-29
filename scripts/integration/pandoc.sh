#!/usr/bin/env bash
# Run Pandoc box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-pandoc"

print_header "Pandoc (box)" "0"

run_box_test "$PKG" "pandoc::integration::test"

exit_with_status
