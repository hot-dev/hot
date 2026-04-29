#!/usr/bin/env bash
# Run AWS SNS integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-sns"

print_header "AWS SNS" "$DELAY"

run_test "$PKG" "aws::integration::sns"

exit_with_status
