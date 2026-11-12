# Queue-wait benchmark

An end-to-end latency benchmark: how long work waits between being published
and running its first instruction, through a real `hot dev` (API, event worker,
task worker and scheduler in one process).

It exists so that latency work on the workers and queues is measured, not
estimated. `crates/hot/tests/queue_wait_load.rs` measures a queue alone; this
measures the whole path, by stage.

## Running it

```bash
scripts/queue-wait-benchmark.sh
```

That builds a release `hot`, runs SQLite with the memory queue, SQLite with the
SQLite queue, and PostgreSQL with a Redis-protocol queue, and writes
`target/queue-wait-runs/<time>/report.md` with one table per scenario, plus the
raw numbers as JSON, the per-statement PostgreSQL counts as TSV, and each
backend's `hot dev` log.

For PostgreSQL and Redis the script starts two throwaway Docker containers
(PostgreSQL with `pg_stat_statements` preloaded, and Valkey) and removes them
when it ends. To use servers you already have, set `HOT_TEST_POSTGRES_URI` (a
login that may create databases and roles) and `HOT_REDIS_URI` (an **empty**
database on a throwaway server: the queues use Hot's own key names, and the
database is emptied afterwards). The benchmark creates and drops its own
PostgreSQL databases (the measured one, and a second that holds the
`pg_stat_statements` extension) and its runtime login. For statement counts
the server must preload `pg_stat_statements`; without it the times are still
reported.

One backend by hand:

```bash
cargo test --release -p hot_cli --test queue_wait_e2e -- \
    --ignored --nocapture --exact queue_wait_e2e_sqlite_db_memory_queue
```

Every test in that file is `#[ignore]`, so a plain `cargo test` runs none of
them. Projects run as a deployed bundle (`hot deploy --local`) by default,
which is what production runs; `HOT_BENCH_DEPLOY=live` measures the live
build `hot dev` serves while a project is edited. The memory queue always
measures the live build, because a deploy from another process cannot reach
an in-process queue. The settings (`HOT_BENCH_EVENTS`, `HOT_BENCH_BIN`, `HOT_BENCH_DEPLOY`,
and so on) are listed at the top of
`crates/hot_cli/tests/queue_wait_e2e.rs`.

Run it on a quiet machine. The stages are a few milliseconds each, and other
work on the machine adds more than that to every one of them. Compare medians,
and compare a change against a baseline taken on the same machine in the same
session.

### PostgreSQL login

With `HOT_BENCH_PSQL` set to a `psql` command line that reaches the server
(the script sets it to `docker exec -i <container> psql`), the benchmark
provisions a restricted runtime login and `hot dev` runs with row-level
security enforced, as a production deployment does. Without it `hot dev`
connects as the database owner with `db.rls.enforce false`: the statements are
the same, but the policies are not evaluated, so each is a little cheaper.

## What is measured

Each handler in the benchmark project reads the clock as its first
instruction and returns the reading, so a run's result says when its code
began. Everything else comes from what Hot stores: the event row, the run row
and its `info.queue_timing`, the task row and its `timing`.

Scenarios:

| scenario | what it shows |
|---|---|
| Event, workers idle | the fixed cost of one event with nothing else running |
| Event, handlers in flight | the same while other handlers are running and executors are free; any polling the claimer falls back to while work is in flight shows up in stage 3 |
| Event sent by a handler | `send()` from inside a run, to the next handler's first instruction |
| Burst | events published at once: the wait is for an executor slot |
| Code task, idle and in flight | `::hot::task/start` to the task's first instruction |
| `task/await` rows | how late the awaiting run learns that its task ended |

Events are published one at a time, each after the previous run has ended and
a random 20-80 ms gap. The gap is random so that publishes do not hold one
phase against a worker's poll interval.

### Event stages

| row | from | to |
|---|---|---|
| total | the harness sends the publish request | the handler's first instruction |
| 1 producer | publish request sent | the API stamps `event_time` (HTTP, authentication, rate limit, stream) |
| 2 producer | `event_time` | the message is in the queue (event insert, environment read, enqueue) |
| 3 queue | enqueued | a claimer takes the message |
| 3a | | the part of 3 spent in the worker's prefetch buffer (Redis) |
| 4 worker | claimed | an executor starts on it (hand-off channel, pickup check, crash quarantine) |
| 5 worker | | event loaded from the database, environment admission checked |
| 6 worker | | handlers found, earlier deliveries read, build loaded |
| 7 worker | | handler setup: project, build path, bytecode cache, context |
| 8 worker | | wait for a blocking execution slot |
| 9 worker | | the VM thread starts and takes the run's start time |
| 10 | `run.start_time` | first instruction: the `run:start` flush and its two publishes |

For an event sent by a handler, "total" starts at the `send()` call.

The `stored:` rows are the fields Hot keeps, for comparison with the stages:
`queue_wait_us` is stage 3, `handler_dispatch_wait_us` is 4 to 6,
`worker_preparation_us` is 7 to 9, and `e.event_time to r.start_time` is what
the app's latency page charts as Waiting (it uses `created_at`).

### Task stages

| row | from | to |
|---|---|---|
| total | the handler calls `::hot::task/start` | the task's first instruction |
| 1 producer | | `task/start` returns |
| 2, 3 producer | | the task row is written, then the message is in the queue |
| 4 queue | enqueued | claimed |
| 5 worker | claimed | the task row, environment, admission, build and project are read (`admitted_at`) |
| 6 worker | | timing write, task lease, code slot and blocking slot (`code_setup_started_at`) |
| 6a | | the part of 6 spent waiting for a slot |
| 7 worker | | code setup: start event, message channel, bytecode, context (`workload_started_at`) |
| 8 | `workload_started_at` | first instruction |
| await | the task's last instruction, and its `stop_time` | `task/await` returns in the awaiting run |

### PostgreSQL statements

Under each sequential scenario on PostgreSQL the report gives the statements,
client/server exchanges and transactions per item, from `pg_stat_statements`,
and the statements issued most often. What the idle services issue anyway
(measured over ten idle seconds) is subtracted, and the harness's own
statements are left out.

The in-flight scenarios' counts include what the held handlers or tasks
issue themselves, so read the per-item cost from the idle scenarios.

A scoped statement is three exchanges: `BEGIN; SELECT hot.apply_scope(...)`
in one message, the statement, and `COMMIT`. The exchange count is therefore
the statement count less the `apply_scope` calls. The count covers the whole
`hot dev` process, so it includes the producer and the run's own writes, not
only the path from claim to first instruction.

### Server round trips

On PostgreSQL and Redis the report opens and closes with a "Server round
trips" table: one `SELECT 1` and one `PING` from the harness on an open
connection, 300 times each. The time an event or task spends talking to a
server is roughly its exchange count times this figure. On a busy machine, or
with the servers in a VM, it says how much of every other row is the
environment and not Hot.

## Timing fields

`run.info.queue_timing` (event runs), all in microseconds:

| field | meaning |
|---|---|
| `enqueued_at`, `claimed_at`, `queue_wait_us` | queue residence, ending when a claimer takes the message from the queue (on Redis, from the worker's prefetch buffer) |
| `prefetch_wait_us` | the part of `queue_wait_us` spent in the prefetch buffer (Redis only) |
| `handler_dispatch_wait_us` | claim to this handler's dispatch |
| `handoff_wait_us`, `hydrate_us`, `handler_lookup_us` | `handler_dispatch_wait_us` split: executor pickup, event load and admission, handler lookup |
| `worker_preparation_us` | dispatch to the run's start |
| `handler_setup_us`, `blocking_slot_wait_us`, `vm_start_us` | `worker_preparation_us` split: setup, slot wait, VM thread start |
| `retry_queue_age_us` | on a redelivery, the message's age in place of `queue_wait_us` |

The split fields were added with this benchmark. They are present only on
runs written by a worker that stamps them, and they sum to the window they
split. No earlier field changed its meaning.

For a second or later handler of one event, `handler_dispatch_wait_us` and so
`handler_lookup_us` include the handlers that ran before it.

The memory queue records no `enqueued_at`, only the wait; the benchmark
takes its enqueue time as the claim time less that wait.

Not in any stored field: the time from `run.start_time` to the first
instruction (stage 10). The run's own duration absorbs it. The event worker
logs it at debug level as `TIMING [<run>]: run_start_flush` and
`run_start_publish`.

`task.timing` gained two timestamps, `admitted_at` and
`code_setup_started_at` (code tasks), which split the claim-to-workload window
as the task stages above do. The rest is unchanged: `waiting_ms` runs from the task row's creation to
`workload_started_at`, which is taken after the wait for a code slot and a
blocking slot and just before the task's code is handed to a VM thread.
`queue_wait_ms` is null on a redelivery (a deferred or retried message), where
`retry_queue_age_ms` holds the message's age since it was requeued; neither
then covers the wait before the requeue, which `waiting_ms` does.
