#!/usr/bin/env bash
# Run Supabase integration tests with pauses between test groups.
#
# Required context/env vars:
#   supabase.url or SUPABASE_URL
#   supabase.anon.key/supabase.api.key or SUPABASE_ANON_KEY/SUPABASE_API_KEY
#   supabase.service.key or SUPABASE_SERVICE_KEY/SUPABASE_SERVICE_ROLE_KEY
#
# Optional env vars:
#   SUPABASE_TEST_TABLE    Defaults to hot-integration-test
#   SUPABASE_TEST_BUCKET   Optional existing bucket; object tests create a temp bucket when unset
#   SUPABASE_TEST_FUNCTION
source "$(dirname "$0")/_common.sh"

DELAY=${DELAY:-2}
PKG="pkg-integration-supabase"
export SUPABASE_TEST_TABLE="${SUPABASE_TEST_TABLE:-hot-integration-test}"

print_header "Supabase" "$DELAY"
echo "SUPABASE_TEST_TABLE=${SUPABASE_TEST_TABLE}"
if [ -z "${SUPABASE_URL:-}" ]; then
    echo "SUPABASE_URL is not exported; relying on supabase.url from Hot context if configured."
fi
if [ -z "${SUPABASE_ANON_KEY:-}" ] && [ -z "${SUPABASE_API_KEY:-}" ]; then
    echo "SUPABASE_ANON_KEY/SUPABASE_API_KEY is not exported; relying on supabase.anon.key/supabase.api.key from Hot context if configured."
fi
if [ -z "${SUPABASE_SERVICE_KEY:-}" ] && [ -z "${SUPABASE_SERVICE_ROLE_KEY:-}" ]; then
    echo "SUPABASE_SERVICE_KEY/SUPABASE_SERVICE_ROLE_KEY is not exported; relying on supabase.service.key from Hot context if configured."
fi
if [ -z "${SUPABASE_TEST_BUCKET:-}" ]; then
    echo "SUPABASE_TEST_BUCKET is not set; object storage tests will create a temporary bucket."
fi
echo ""

run_test "$PKG" "supabase::integration::db"
sleep "$DELAY"

run_test "$PKG" "supabase::integration::auth"
sleep "$DELAY"

run_test "$PKG" "supabase::integration::storage"
sleep "$DELAY"

if [ -n "${SUPABASE_TEST_FUNCTION:-}" ]; then
    run_test "$PKG" "supabase::integration::functions"
else
    echo "──────────────────────────────────────────"
    echo "Skipping supabase::integration::functions"
    echo "Set SUPABASE_TEST_FUNCTION to run edge function tests."
    echo "──────────────────────────────────────────"
fi

exit_with_status
