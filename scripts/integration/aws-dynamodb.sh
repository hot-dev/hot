#!/usr/bin/env bash
# Run AWS DynamoDB integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-dynamodb"

print_header "AWS DynamoDB" "$DELAY"

run_test "$PKG" "aws::integration::dynamodb"

exit_with_status
