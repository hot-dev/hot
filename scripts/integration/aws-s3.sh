#!/usr/bin/env bash
# Run AWS S3 integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-aws-s3"

print_header "AWS S3" "$DELAY"

run_test "$PKG" "aws::integration::s3"

exit_with_status
