#!/usr/bin/env bash
# Run AWS SQS integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-sqs"

print_header "AWS SQS" "$DELAY"

run_test "$PKG" "aws::integration::sqs"

exit_with_status
