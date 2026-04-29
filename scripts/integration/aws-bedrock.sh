#!/usr/bin/env bash
# Run AWS Bedrock integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-bedrock"

print_header "AWS Bedrock" "$DELAY"

run_test "$PKG" "aws::integration::bedrock"

exit_with_status
