#!/usr/bin/env bash

### Run and analyze the noisy-load Hot project against SQLite/memory and
### PostgreSQL/Valkey backends.
###
### Defaults compare the installed `hot` binary with `target/release/hot`.
### Override with env vars, for example:
###
###   WORKER_THREADS_MATRIX="1 2 4 8" BACKENDS="pg-valkey" \
###     scripts/noisy-load-benchmark.sh
###
### Outputs are written under target/noisy-load-runs/<timestamp>/.

set -euo pipefail

cd "$(dirname "$0")/.."

NOISY_DIR="${NOISY_DIR:-hot/test/noisy-load}"
RUN_ROOT="${RUN_ROOT:-target/noisy-load-runs/$(date +%Y%m%d-%H%M%S)}"

POSTGRES_IMAGE="${POSTGRES_IMAGE:-pgvector/pgvector:pg18}" # PostgreSQL 18.3
VALKEY_IMAGE="${VALKEY_IMAGE:-valkey/valkey:8}"
POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-hot-noisy-postgres}"
VALKEY_CONTAINER="${VALKEY_CONTAINER:-hot-noisy-valkey}"
POSTGRES_PORT="${POSTGRES_PORT:-55432}"
VALKEY_PORT="${VALKEY_PORT:-56379}"

DB_USER="${DB_USER:-hot}"
DB_PASSWORD="${DB_PASSWORD:-hot}"
DB_NAME="${DB_NAME:-hot}"
DB_URI="postgres://${DB_USER}:${DB_PASSWORD}@127.0.0.1:${POSTGRES_PORT}/${DB_NAME}"
VALKEY_URI="redis://127.0.0.1:${VALKEY_PORT}"

DURATION_SECONDS="${DURATION_SECONDS:-82}"
HOT_BOX_ENABLED="${HOT_BOX_ENABLED:-false}"
HOT_ENGINE_THREADS="${HOT_ENGINE_THREADS:-4}"
WORKER_THREADS_MATRIX="${WORKER_THREADS_MATRIX:-4}"
TASK_CONCURRENCY_MATRIX="${TASK_CONCURRENCY_MATRIX:-4}"
BACKENDS="${BACKENDS:-sqlite pg-valkey}"
BINARIES="${BINARIES:-system final}"
DEPLOY_MODE_MATRIX="${DEPLOY_MODE_MATRIX:-live}"
HOT_FINAL_BIN="${HOT_FINAL_BIN:-target/release/hot}"
BUILD_RELEASE="${BUILD_RELEASE:-auto}"
KEEP_CONTAINERS="${KEEP_CONTAINERS:-1}"
HOT_WORKER_READ_BATCH_SIZE="${HOT_WORKER_READ_BATCH_SIZE:-8}"
HOT_DB_WRITER_SHARDS="${HOT_DB_WRITER_SHARDS:-4}"
HOT_LOG_LEVEL_OVERRIDE="${HOT_LOG_LEVEL_OVERRIDE:-}"
DEPLOY_READY_TIMEOUT_SECONDS="${DEPLOY_READY_TIMEOUT_SECONDS:-60}"
DEPLOY_SCHEDULER_CUTOVER_TIMEOUT_SECONDS="${DEPLOY_SCHEDULER_CUTOVER_TIMEOUT_SECONDS:-30}"
DEPLOY_SETTLE_SECONDS="${DEPLOY_SETTLE_SECONDS:-15}"

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Error: required command not found: $1" >&2
        exit 1
    fi
}

require_cmd docker
require_cmd awk
require_cmd sed

if [[ ! -d "$NOISY_DIR" ]]; then
    echo "Error: noisy-load project not found at $NOISY_DIR" >&2
    exit 1
fi

if [[ "$BINARIES" == *"system"* ]]; then
    require_cmd hot
fi

if [[ "$BINARIES" == *"final"* ]]; then
    if [[ "$HOT_FINAL_BIN" != /* ]]; then
        HOT_FINAL_BIN="$PWD/$HOT_FINAL_BIN"
    fi
    if [[ "$BUILD_RELEASE" == "1" || ( "$BUILD_RELEASE" == "auto" && ! -x "$HOT_FINAL_BIN" ) ]]; then
        echo "Building release Hot binary: $HOT_FINAL_BIN"
        cargo build --release
    fi
    if [[ ! -x "$HOT_FINAL_BIN" ]]; then
        echo "Error: final Hot binary not executable: $HOT_FINAL_BIN" >&2
        echo "Run cargo build --release, set HOT_FINAL_BIN, or set BINARIES=system." >&2
        exit 1
    fi
fi

mkdir -p "$RUN_ROOT"

have_backend() {
    local needle="$1"
    for backend in $BACKENDS; do
        if [[ "$backend" == "$needle" ]]; then
            return 0
        fi
    done
    return 1
}

docker_rm_if_exists() {
    local name="$1"
    if docker ps -a --format '{{.Names}}' | awk -v n="$name" '$0 == n { found=1 } END { exit found ? 0 : 1 }'; then
        docker rm -f "$name" >/dev/null
    fi
}

wait_for_postgres() {
    echo "Waiting for PostgreSQL on port $POSTGRES_PORT..."
    for _ in $(seq 1 60); do
        if docker exec "$POSTGRES_CONTAINER" pg_isready -U "$DB_USER" -d "$DB_NAME" >/dev/null 2>&1; then
            docker exec "$POSTGRES_CONTAINER" postgres --version | tee "$RUN_ROOT/postgres-version.txt"
            return 0
        fi
        sleep 1
    done
    echo "Error: PostgreSQL did not become ready" >&2
    docker logs "$POSTGRES_CONTAINER" >&2 || true
    exit 1
}

valkey_cli() {
    docker exec "$VALKEY_CONTAINER" valkey-cli "$@" 2>/dev/null \
        || docker exec "$VALKEY_CONTAINER" redis-cli "$@"
}

wait_for_valkey() {
    echo "Waiting for Valkey on port $VALKEY_PORT..."
    for _ in $(seq 1 60); do
        if valkey_cli PING >/dev/null 2>&1; then
            valkey_cli INFO server | sed -n '1,12p' > "$RUN_ROOT/valkey-info.txt" || true
            return 0
        fi
        sleep 1
    done
    echo "Error: Valkey did not become ready" >&2
    docker logs "$VALKEY_CONTAINER" >&2 || true
    exit 1
}

start_containers() {
    docker_rm_if_exists "$POSTGRES_CONTAINER"
    docker_rm_if_exists "$VALKEY_CONTAINER"

    echo "Starting PostgreSQL/pgvector container: $POSTGRES_IMAGE"
    docker run -d \
        --name "$POSTGRES_CONTAINER" \
        -e POSTGRES_USER="$DB_USER" \
        -e POSTGRES_PASSWORD="$DB_PASSWORD" \
        -e POSTGRES_DB="$DB_NAME" \
        -p "${POSTGRES_PORT}:5432" \
        "$POSTGRES_IMAGE" >/dev/null

    echo "Starting Valkey container: $VALKEY_IMAGE"
    docker run -d \
        --name "$VALKEY_CONTAINER" \
        -p "${VALKEY_PORT}:6379" \
        "$VALKEY_IMAGE" >/dev/null

    wait_for_postgres
    wait_for_valkey
}

cleanup_containers() {
    if [[ "$KEEP_CONTAINERS" == "0" ]]; then
        docker_rm_if_exists "$POSTGRES_CONTAINER"
        docker_rm_if_exists "$VALKEY_CONTAINER"
    fi
}

reset_pg_valkey() {
    docker exec "$POSTGRES_CONTAINER" psql -U "$DB_USER" -d postgres -q -c \
        "DROP DATABASE IF EXISTS ${DB_NAME};" >/dev/null
    docker exec "$POSTGRES_CONTAINER" psql -U "$DB_USER" -d postgres -q -c \
        "CREATE DATABASE ${DB_NAME};" >/dev/null
    valkey_cli FLUSHALL >/dev/null
}

stop_hot_dev() {
    local pid="$1"
    if ! kill -0 "$pid" >/dev/null 2>&1; then
        wait "$pid" || true
        return
    fi

    kill -INT "$pid" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            wait "$pid" || true
            return
        fi
        sleep 1
    done

    echo "hot dev pid $pid did not stop after SIGINT; sending SIGTERM" >&2
    kill -TERM "$pid" >/dev/null 2>&1 || true
    wait "$pid" || true
}

binary_for_label() {
    local label="$1"
    case "$label" in
        system)
            echo hot
            ;;
        final)
            echo "$HOT_FINAL_BIN"
            ;;
        *)
            echo "Error: unknown binary label '$label'" >&2
            exit 1
            ;;
    esac
}

run_with_backend_env() {
    local backend="$1"
    shift

    if [[ "$backend" == "sqlite" ]]; then
        env -u HOT_DB_URI -u HOT_REDIS_URI -u HOT_QUEUE_TYPE \
            HOT_BOX_ENABLED="$HOT_BOX_ENABLED" \
            HOT_ENGINE_THREADS="$HOT_ENGINE_THREADS" \
            HOT_WORKER_READ_BATCH_SIZE="$HOT_WORKER_READ_BATCH_SIZE" \
            HOT_DB_WRITER_SHARDS="$HOT_DB_WRITER_SHARDS" \
            "$@"
    else
        env \
            HOT_DB_URI="$DB_URI" \
            HOT_REDIS_URI="$VALKEY_URI" \
            HOT_QUEUE_TYPE="redis" \
            HOT_SERIALIZATION_TYPE="json" \
            HOT_BOX_ENABLED="$HOT_BOX_ENABLED" \
            HOT_ENGINE_THREADS="$HOT_ENGINE_THREADS" \
            HOT_WORKER_READ_BATCH_SIZE="$HOT_WORKER_READ_BATCH_SIZE" \
            HOT_DB_WRITER_SHARDS="$HOT_DB_WRITER_SHARDS" \
            "$@"
    fi
}

wait_for_log_line() {
    local pid="$1"
    local log_file="$2"
    local pattern="$3"
    local label="$4"
    local elapsed=0

    while [[ "$elapsed" -lt "$DEPLOY_READY_TIMEOUT_SECONDS" ]]; do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            wait "$pid" || true
            echo "hot dev exited before $label; see $log_file" >&2
            return 1
        fi
        if [[ -f "$log_file" ]] && rg -q "$pattern" "$log_file"; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    echo "Timed out waiting for $label in $log_file" >&2
    return 1
}

wait_for_bundle_scheduler_cutover() {
    local pid="$1"
    local log_file="$2"
    local live_build_id="$3"
    local elapsed=0

    if [[ -z "$live_build_id" ]]; then
        echo "Could not determine initial live build id; using settle delay only" >&2
        return 0
    fi

    while [[ "$elapsed" -lt "$DEPLOY_SCHEDULER_CUTOVER_TIMEOUT_SECONDS" ]]; do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            wait "$pid" || true
            echo "hot dev exited before scheduler cutover; see $log_file" >&2
            return 1
        fi
        if [[ -f "$log_file" ]] \
            && rg -q "SCHEDULER removed job .* from build ${live_build_id}|SCHEDULER sync complete.*Removed: [1-9]" "$log_file"; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    echo "Timed out waiting for scheduler cutover from live build $live_build_id; using settle delay" >&2
    return 0
}

deploy_bundle_locally() {
    local label="$1"
    local backend="$2"
    local run_dir="$3"
    local log_file="$run_dir/local-deploy.log"
    local build_id_file="$run_dir/local-deploy-build-id.txt"
    local bin
    bin="$(binary_for_label "$label")"

    echo "Deploying bundle locally for $label on $backend"
    local -a deploy_cmd
    deploy_cmd=("$bin" deploy --local)
    if [[ "$backend" == "pg-valkey" ]]; then
        deploy_cmd+=(--db.uri "$DB_URI")
    fi

    (
        cd "$NOISY_DIR"
        run_with_backend_env "$backend" "${deploy_cmd[@]}"
    ) > "$log_file" 2>&1

    sed -n \
        -e 's/^✓ Successfully deployed build: //p' \
        -e 's/^✓ Accepted bundle deployment: //p' \
        "$log_file" | sed -n '1p' > "$build_id_file"
    if [[ ! -s "$build_id_file" ]]; then
        echo "Could not determine deployed bundle build id; see $log_file" >&2
        return 1
    fi
}

clear_queues_for_benchmark_window() {
    local label="$1"
    local backend="$2"
    local run_dir="$3"
    local log_file="$run_dir/queue-clear.log"
    local bin
    bin="$(binary_for_label "$label")"

    (
        cd "$NOISY_DIR"
        run_with_backend_env "$backend" "$bin" queue clear
    ) > "$log_file" 2>&1
}

mark_benchmark_window_start() {
    local run_dir="$1"
    date -u +"%Y-%m-%dT%H:%M:%SZ" > "$run_dir/benchmark-started-at.txt"
}

run_hot_dev() {
    local label="$1"
    local backend="$2"
    local worker_threads="$3"
    local task_concurrency="$4"
    local run_dir="$5"
    local deploy_mode="$6"
    local log_file="$run_dir/hot-dev.log"

    local -a cmd
    cmd=("$(binary_for_label "$label")" dev)

    if [[ -n "$HOT_LOG_LEVEL_OVERRIDE" ]]; then
        cmd+=(--log.level "$HOT_LOG_LEVEL_OVERRIDE")
    fi

    if [[ "$backend" == "pg-valkey" ]]; then
        cmd+=(
            --db.uri "$DB_URI"
            --redis.uri "$VALKEY_URI"
            --queue.type redis
            --serialization.type json
        )
    fi

    echo "Running $label on $backend (deploy=$deploy_mode, workers=$worker_threads, tasks=$task_concurrency)"
    (
        cd "$NOISY_DIR"
        rm -rf .hot
        if [[ "$backend" == "sqlite" ]]; then
            exec env -u HOT_DB_URI -u HOT_REDIS_URI -u HOT_QUEUE_TYPE \
                HOT_BOX_ENABLED="$HOT_BOX_ENABLED" \
                HOT_WORKER_THREADS="$worker_threads" \
                HOT_ENGINE_THREADS="$HOT_ENGINE_THREADS" \
                HOT_TASK_MAX_CONCURRENT="$task_concurrency" \
                HOT_WORKER_READ_BATCH_SIZE="$HOT_WORKER_READ_BATCH_SIZE" \
                HOT_DB_WRITER_SHARDS="$HOT_DB_WRITER_SHARDS" \
                "${cmd[@]}"
        else
            exec env \
                HOT_DB_URI="$DB_URI" \
                HOT_REDIS_URI="$VALKEY_URI" \
                HOT_QUEUE_TYPE="redis" \
                HOT_SERIALIZATION_TYPE="json" \
                HOT_BOX_ENABLED="$HOT_BOX_ENABLED" \
                HOT_WORKER_THREADS="$worker_threads" \
                HOT_ENGINE_THREADS="$HOT_ENGINE_THREADS" \
                HOT_TASK_MAX_CONCURRENT="$task_concurrency" \
                HOT_WORKER_READ_BATCH_SIZE="$HOT_WORKER_READ_BATCH_SIZE" \
                HOT_DB_WRITER_SHARDS="$HOT_DB_WRITER_SHARDS" \
                "${cmd[@]}"
        fi
    ) > "$log_file" 2>&1 &

    local pid=$!
    if [[ "$deploy_mode" == "bundle" ]]; then
        wait_for_log_line "$pid" "$log_file" "Live build ready" "live build readiness"
        local live_build_id
        live_build_id="$(sed -n "s/.*Live build created for project '.*': build_id=//p" "$log_file" | sed -n '1p')"
        deploy_bundle_locally "$label" "$backend" "$run_dir"
        local deployed_build_id
        deployed_build_id="$(sed -n '1p' "$run_dir/local-deploy-build-id.txt")"
        wait_for_log_line "$pid" "$log_file" "Extracted bundle ${deployed_build_id}|Bundle ${deployed_build_id} pre-compiled successfully|successfully loaded handlers and schedules for build ${deployed_build_id}" "bundle deployment processing"
        wait_for_bundle_scheduler_cutover "$pid" "$log_file" "$live_build_id"
        sleep "$DEPLOY_SETTLE_SECONDS"
        if [[ "$backend" == "pg-valkey" ]]; then
            clear_queues_for_benchmark_window "$label" "$backend" "$run_dir"
        fi
    fi

    mark_benchmark_window_start "$run_dir"

    local elapsed=0
    while [[ "$elapsed" -lt "$DURATION_SECONDS" ]]; do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            wait "$pid" || true
            echo "hot dev exited early after ${elapsed}s; see $log_file"
            return
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    stop_hot_dev "$pid"
}

sqlite_query() {
    local db_path="$1"
    local sql="$2"
    sqlite3 "$db_path" "$sql"
}

analyze_sqlite() {
    require_cmd sqlite3
    local run_dir="$1"
    local db_path="$NOISY_DIR/.hot/db/hot.sqlite.db"
    local metrics="$run_dir/metrics.tsv"

    if [[ ! -f "$db_path" ]]; then
        echo "missing_sqlite_db	true" | tee "$metrics"
        return
    fi

    sqlite_query "$db_path" "
select 'runs_total', count(*) from run;
select 'runs_per_sec', round(count(*) / ${DURATION_SECONDS}.0, 2) from run;
select 'runs_succeeded', count(*) from run where status_id = 2;
select 'runs_non_terminal_or_failed', count(*) from run where status_id not in (2);
select 'events_total', count(*) from event;
select 'events_unhandled', count(*) from event where handled = 0;
select 'tasks_total', count(*) from task;
select 'tasks_by_status', group_concat(status_name || ':' || status_count, ',')
from (
    select coalesce(ts.name, task.task_status_id) as status_name, count(*) as status_count
    from task
    left join task_status ts on ts.task_status_id = task.task_status_id
    group by status_name
    order by status_name
);
select 'run_ms_avg', round(avg((julianday(stop_time) - julianday(start_time)) * 86400000), 2)
from run where stop_time is not null;
select 'run_ms_p95', round(coalesce((
    select (julianday(stop_time) - julianday(start_time)) * 86400000
    from run
    where stop_time is not null
    order by (julianday(stop_time) - julianday(start_time)) * 86400000
    limit 1 offset (
        select max(cast(count(*) * 0.95 as int) - 1, 0)
        from run
        where stop_time is not null
    )
), 0), 2);
select 'queue_wait_ms_p95', round(coalesce((
    select (julianday(r.start_time) - julianday(e.created_at)) * 86400000
    from run r
    join event e on e.event_id = r.event_id
    where r.start_time is not null
    order by (julianday(r.start_time) - julianday(e.created_at)) * 86400000
    limit 1 offset (
        select max(cast(count(*) * 0.95 as int) - 1, 0)
        from run r2
        join event e2 on e2.event_id = r2.event_id
        where r2.start_time is not null
    )
), 0), 2);
" | tee "$metrics"

    cp "$db_path" "$run_dir/hot.sqlite.db"
}

psql_query() {
    PGPASSWORD="$DB_PASSWORD" psql \
        -h 127.0.0.1 \
        -p "$POSTGRES_PORT" \
        -U "$DB_USER" \
        -d "$DB_NAME" \
        -At \
        -c "$1"
}

analyze_pg_valkey() {
    require_cmd psql
    local run_dir="$1"
    local metrics="$run_dir/metrics.tsv"
    local benchmark_started_at=""
    local run_where=""
    local event_where=""
    local task_where=""
    local queue_wait_where="where r.start_time is not null"

    if [[ -s "$run_dir/benchmark-started-at.txt" ]]; then
        benchmark_started_at="$(sed -n '1p' "$run_dir/benchmark-started-at.txt")"
        run_where="where start_time >= timestamptz '$benchmark_started_at'"
        event_where="where created_at >= timestamptz '$benchmark_started_at'"
        task_where="where created_at >= timestamptz '$benchmark_started_at'"
        queue_wait_where="where r.start_time is not null and r.start_time >= timestamptz '$benchmark_started_at'"
    fi

    psql_query "
select 'runs_total', count(*) from hot.run $run_where;
select 'runs_per_sec', round(count(*) / ${DURATION_SECONDS}.0, 2) from hot.run $run_where;
select 'runs_succeeded', count(*) from hot.run ${run_where:-where true} and status_id = 2;
select 'runs_non_terminal_or_failed', count(*) from hot.run ${run_where:-where true} and status_id not in (2);
select 'events_total', count(*) from hot.event $event_where;
select 'events_unhandled', count(*) from hot.event ${event_where:-where true} and handled = false;
select 'tasks_total', count(*) from hot.task $task_where;
select 'tasks_by_status', string_agg(status_name || ':' || status_count, ',' order by status_name)
from (
    select coalesce(ts.name, t.task_status_id::text) as status_name, count(*)::text as status_count
    from hot.task t
    left join hot.task_status ts on ts.task_status_id = t.task_status_id
    $task_where
    group by status_name
) statuses;
select 'run_ms_avg', coalesce(round(avg(extract(epoch from (stop_time - start_time)) * 1000)::numeric, 2)::text, '0')
from hot.run ${run_where:-where true} and stop_time is not null;
select 'run_ms_p95', coalesce(round((percentile_cont(0.95) within group (
    order by extract(epoch from (stop_time - start_time)) * 1000
))::numeric, 2)::text, '0')
from hot.run ${run_where:-where true} and stop_time is not null;
select 'queue_wait_ms_p95', coalesce(round((percentile_cont(0.95) within group (
    order by extract(epoch from (r.start_time - e.created_at)) * 1000
))::numeric, 2)::text, '0')
from hot.run r
join hot.event e on e.event_id = r.event_id
$queue_wait_where;
" | tee "$metrics"

    {
        echo "task_dlq|$(valkey_cli XLEN '{hot:task}:deadletter')"
        echo "event_dlq|$(valkey_cli XLEN '{hot:event}:deadletter')"
        echo "task_stream|$(valkey_cli XLEN '{hot:task}')"
        echo "event_stream|$(valkey_cli XLEN '{hot:event}')"
    } | tee "$run_dir/valkey.tsv"
}

analyze_log() {
    local run_dir="$1"
    local log_file="$run_dir/hot-dev.log"
    local errors="$run_dir/errors.txt"

    if command -v rg >/dev/null 2>&1; then
        rg -n "ERROR|Redis error|Valkey error|DLQ|deadletter|panic|timed out" "$log_file" \
            > "$errors" || true
    else
        awk '/ERROR|Redis error|Valkey error|DLQ|deadletter|panic|timed out/ { print NR ":" $0 }' \
            "$log_file" > "$errors" || true
    fi

    echo "error_lines|$(wc -l < "$errors" | tr -d ' ')" | tee -a "$run_dir/metrics.tsv"
}

print_run_summary() {
    local run_dir="$1"
    echo
    echo "Summary: $run_dir"
    sed 's/^/  /' "$run_dir/metrics.tsv" || true
    if [[ -f "$run_dir/valkey.tsv" ]]; then
        sed 's/^/  /' "$run_dir/valkey.tsv"
    fi
    if [[ -s "$run_dir/errors.txt" ]]; then
        echo "  errors: see $run_dir/errors.txt"
    else
        echo "  errors: none matched"
    fi
}

run_case() {
    local backend="$1"
    local label="$2"
    local worker_threads="$3"
    local task_concurrency="$4"
    local deploy_mode="$5"
    local run_name="${backend}-${label}-${deploy_mode}-w${worker_threads}-t${task_concurrency}"
    local run_dir="$RUN_ROOT/$run_name"
    mkdir -p "$run_dir"

    if [[ "$backend" == "pg-valkey" ]]; then
        reset_pg_valkey
    fi

    run_hot_dev "$label" "$backend" "$worker_threads" "$task_concurrency" "$run_dir" "$deploy_mode"

    if [[ "$backend" == "sqlite" ]]; then
        analyze_sqlite "$run_dir"
    else
        analyze_pg_valkey "$run_dir"
    fi
    analyze_log "$run_dir"
    print_run_summary "$run_dir"
}

if have_backend pg-valkey; then
    start_containers
fi
trap cleanup_containers EXIT

echo "Noisy-load benchmark output: $RUN_ROOT"
echo "Backends: $BACKENDS"
echo "Binaries: $BINARIES"
echo "Deploy modes: $DEPLOY_MODE_MATRIX"
echo "Worker threads: $WORKER_THREADS_MATRIX"
echo "Task concurrency: $TASK_CONCURRENCY_MATRIX"
echo "Duration seconds: $DURATION_SECONDS"
if [[ -n "$HOT_LOG_LEVEL_OVERRIDE" ]]; then
    echo "Log level override: $HOT_LOG_LEVEL_OVERRIDE"
fi

for backend in $BACKENDS; do
    case "$backend" in
        sqlite|pg-valkey) ;;
        *)
            echo "Error: unsupported backend '$backend' (expected sqlite or pg-valkey)" >&2
            exit 1
            ;;
    esac

    for label in $BINARIES; do
        for deploy_mode in $DEPLOY_MODE_MATRIX; do
            case "$deploy_mode" in
                live|bundle) ;;
                *)
                    echo "Error: unsupported deploy mode '$deploy_mode' (expected live or bundle)" >&2
                    exit 1
                    ;;
            esac

            for worker_threads in $WORKER_THREADS_MATRIX; do
                for task_concurrency in $TASK_CONCURRENCY_MATRIX; do
                    run_case "$backend" "$label" "$worker_threads" "$task_concurrency" "$deploy_mode"
                done
            done
        done
    done
done

echo
echo "All benchmark artifacts are in $RUN_ROOT"
if [[ "$KEEP_CONTAINERS" != "0" && "$(have_backend pg-valkey && echo yes || echo no)" == "yes" ]]; then
    echo "Containers left running:"
    docker ps --filter "name=${POSTGRES_CONTAINER}" --filter "name=${VALKEY_CONTAINER}" \
        --format '  {{.Names}} {{.Image}} {{.Ports}}'
fi
