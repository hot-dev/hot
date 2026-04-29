#!/usr/bin/env bash
# Run AWS Lambda integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-lambda"

print_header "AWS Lambda" "$DELAY"

run_test "$PKG" "aws::integration::lambda"

exit_with_status
