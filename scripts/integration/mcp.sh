#!/usr/bin/env bash
# Run MCP integration tests with pauses between test groups.
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-mcp"

print_header "MCP" "$DELAY"

run_test "$PKG" "mcp::integration::client"
sleep "$DELAY"

run_test "$PKG" "mcp::integration::tools"
sleep "$DELAY"

run_test "$PKG" "mcp::integration::resources"
sleep "$DELAY"

run_test "$PKG" "mcp::integration::prompts"

exit_with_status
