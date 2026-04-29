#!/usr/bin/env bash
# Run AWS SES integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-3}
PKG="pkg-integration-aws-ses"

print_header "AWS SES" "$DELAY"

run_test "$PKG" "aws::integration::ses"

exit_with_status
