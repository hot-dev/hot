#!/usr/bin/env bash
# Run Supabase Management API integration tests with pauses between test groups.
#
# Required context vars:
#   supabase.access.token
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-supabase-admin"

print_header "Supabase Admin" "$DELAY"

run_test "$PKG" "supabase::admin::integration::projects"
sleep "$DELAY"

run_test "$PKG" "supabase::admin::integration::organizations"

exit_with_status
