#!/usr/bin/env bash
# Run JSON-RPC integration tests.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-1}
PKG="pkg-integration-json-rpc"

print_header "JSON-RPC" "$DELAY"

run_test "$PKG" "json-rpc::integration::client"

exit_with_status
