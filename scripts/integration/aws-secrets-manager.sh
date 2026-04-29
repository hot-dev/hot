#!/usr/bin/env bash
# Run AWS Secrets Manager integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-secrets-manager"

print_header "AWS Secrets Manager" "$DELAY"

run_test "$PKG" "aws::integration::secrets-manager"

exit_with_status
