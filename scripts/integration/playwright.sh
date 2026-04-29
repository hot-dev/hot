#!/usr/bin/env bash
# Run Playwright box integration tests (requires Docker).
source "$(dirname "$0")/_common.sh"

PKG="pkg-integration-playwright"

print_header "Playwright (box)" "0"

run_box_test "$PKG" "playwright::integration::test"

exit_with_status
