#!/usr/bin/env bash

### Run the end-to-end queue-wait benchmark (publish to first instruction)
### against SQLite with the memory and SQLite queues, and against PostgreSQL
### with a Redis-protocol queue.
###
###   scripts/queue-wait-benchmark.sh
###
### The benchmark itself is crates/hot_cli/tests/queue_wait_e2e.rs; this script
### builds a release `hot`, provides the PostgreSQL and Valkey servers, runs
### the three backends one after another and joins their reports into
### target/queue-wait-runs/<time>/report.md. What the rows mean is in
### docs/queue-wait-benchmark.md.
###
### Servers. With HOT_TEST_POSTGRES_URI and HOT_REDIS_URI both set, those are
### used (the Redis database must be empty; it is emptied again afterwards, and
### the benchmark creates and drops its own PostgreSQL databases). Otherwise the
### script starts two throwaway Docker containers and removes them when it
### ends. It never touches a container it did not start.
###
### Settings (environment):
###
###   BACKENDS="sqlite pg-redis"   which backends to run
###   PROFILE=release              cargo profile: release or dev
###   JOBS=                        cargo -j value (default: cargo's)
###   POSTGRES_IMAGE, VALKEY_IMAGE, POSTGRES_PORT=55461, VALKEY_PORT=56361
###   HOT_BENCH_*                  passed through; see queue_wait_e2e.rs
###
### Run it on a quiet machine: the numbers are latencies of a few
### milliseconds, and a busy machine adds more than that to every stage.

set -euo pipefail

cd "$(dirname "$0")/.."

BACKENDS="${BACKENDS:-sqlite pg-redis}"
PROFILE="${PROFILE:-release}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-pgvector/pgvector:pg18}"
VALKEY_IMAGE="${VALKEY_IMAGE:-valkey/valkey:8}"
POSTGRES_PORT="${POSTGRES_PORT:-55461}"
VALKEY_PORT="${VALKEY_PORT:-56361}"
POSTGRES_CONTAINER="hot-queue-wait-postgres-$$"
VALKEY_CONTAINER="hot-queue-wait-valkey-$$"

export HOT_BENCH_OUT="${HOT_BENCH_OUT:-$PWD/target/queue-wait-runs/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$HOT_BENCH_OUT"

started_containers=()

cleanup() {
    for container in "${started_containers[@]:-}"; do
        if [[ -n "$container" ]]; then
            docker rm -f "$container" >/dev/null 2>&1 || true
        fi
    done
}
trap cleanup EXIT

have_backend() {
    [[ " $BACKENDS " == *" $1 "* ]]
}

start_servers() {
    if ! command -v docker >/dev/null 2>&1 || ! docker ps >/dev/null 2>&1; then
        echo "Docker is not available and HOT_TEST_POSTGRES_URI / HOT_REDIS_URI are not both set:" >&2
        echo "skipping the PostgreSQL + Redis backend." >&2
        return 1
    fi

    echo "Starting $POSTGRES_IMAGE as $POSTGRES_CONTAINER on port $POSTGRES_PORT"
    docker run -d --name "$POSTGRES_CONTAINER" \
        -e POSTGRES_USER=hot -e POSTGRES_PASSWORD=hot -e POSTGRES_DB=hot \
        -p "127.0.0.1:${POSTGRES_PORT}:5432" \
        "$POSTGRES_IMAGE" \
        -c shared_preload_libraries=pg_stat_statements \
        -c pg_stat_statements.max=10000 \
        -c max_connections=300 >/dev/null
    started_containers+=("$POSTGRES_CONTAINER")

    echo "Starting $VALKEY_IMAGE as $VALKEY_CONTAINER on port $VALKEY_PORT"
    docker run -d --name "$VALKEY_CONTAINER" \
        -p "127.0.0.1:${VALKEY_PORT}:6379" \
        "$VALKEY_IMAGE" >/dev/null
    started_containers+=("$VALKEY_CONTAINER")

    for _ in $(seq 1 60); do
        if docker exec "$POSTGRES_CONTAINER" pg_isready -U hot -d hot >/dev/null 2>&1 \
            && docker exec "$POSTGRES_CONTAINER" psql -U hot -d hot -qc 'select 1' >/dev/null 2>&1; then
            export HOT_TEST_POSTGRES_URI="postgres://hot:hot@127.0.0.1:${POSTGRES_PORT}/hot"
            export HOT_REDIS_URI="redis://127.0.0.1:${VALKEY_PORT}"
            export HOT_BENCH_PSQL="docker exec -i $POSTGRES_CONTAINER psql"
            return 0
        fi
        sleep 1
    done
    echo "PostgreSQL did not become ready" >&2
    docker logs "$POSTGRES_CONTAINER" >&2 || true
    return 1
}

cargo_args=(test -p hot_cli --test queue_wait_e2e)
if [[ "$PROFILE" == "release" ]]; then
    cargo_args+=(--release)
fi
if [[ -n "${JOBS:-}" ]]; then
    cargo_args+=(-j "$JOBS")
fi

# Build once and run the test binary itself for each backend, so that the
# backends are not separated by further cargo invocations.
echo "Building ($PROFILE)"
build_log="$HOT_BENCH_OUT/build.log"
cargo "${cargo_args[@]}" --no-run 2>&1 | tee "$build_log"
test_binary="$(sed -n 's/.*Executable tests\/queue_wait_e2e\.rs (\(.*\))$/\1/p' "$build_log" | tail -n 1)"
if [[ -z "$test_binary" || ! -x "$test_binary" ]]; then
    echo "Could not find the benchmark's test binary in $build_log" >&2
    exit 1
fi

run_backend() {
    "$test_binary" --ignored --nocapture --test-threads 1 --exact "$1"
}

if have_backend sqlite; then
    run_backend queue_wait_e2e_sqlite_db_memory_queue
    run_backend queue_wait_e2e_sqlite_db_sqlite_queue
fi

if have_backend pg-redis; then
    if [[ -n "${HOT_TEST_POSTGRES_URI:-}" && -n "${HOT_REDIS_URI:-}" ]] || start_servers; then
        run_backend queue_wait_e2e_postgres_db_redis_queue
    fi
fi

report="$HOT_BENCH_OUT/report.md"
{
    echo "# Queue-wait benchmark"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- commit: $(git rev-parse --short HEAD)$(git diff --quiet || echo ' (with uncommitted changes)')"
    echo "- machine: $(uname -srm), $(getconf _NPROCESSORS_ONLN) CPUs"
    echo "- load average when the run ended: $(uptime | sed 's/.*load averages*: *//')"
    echo "- profile: $PROFILE"
    echo
    for backend in sqlite-db-memory-queue sqlite-db-sqlite-queue postgres-db-redis-queue; do
        if [[ -f "$HOT_BENCH_OUT/$backend.md" ]]; then
            cat "$HOT_BENCH_OUT/$backend.md"
            echo
        fi
    done
} > "$report"

echo "Report: $report"
