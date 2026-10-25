# Maintained account usage

Quota admission uses transactional counters instead of scanning retained history
while it holds the root account's row lock. The scope contract stays the same:
a root and its direct children share limits; each child owns its environments
and deployments. Shared deployments remain deferred. Usage shown in billing
reports keeps its existing source queries and meaning.

## Storage and call ownership

Migration 013 stores `call.env_id`, backfills it from the call's run, and makes
it required and immutable. PostgreSQL validates `(run_id, env_id)` against the
run; SQLite checks that pair in a trigger. A runtime call write always supplies
its trusted execution environment. Call policies and retention filters use
this field directly, including when the parent run is deleted by retention.

`usage_storage(env_id, org_id, slot, store_bytes, call_bytes)` tracks the sum of
`max(size, 0)` on store entries and calls. PostgreSQL assigns each backend to
`pg_backend_pid() % 16`; SQLite uses slot zero. Read the sum across **all** slots.
Individual slots may be negative after updates/deletes by another session.
They are not independent usage totals and must not be clamped to zero.

Insert and size-changing update triggers publish deltas in the same transaction
as the source write. PostgreSQL adds each delta to the backend's existing slot
with a plain UPDATE and only inserts a slot when none exists, so a trace batch
does not repeat speculative insertion per write. PostgreSQL statement delete triggers aggregate transition
rows by environment; SQLite subtracts row deltas. Cascaded run deletion subtracts
call bytes once, without looking up a run that is already gone. Store counters
have no environment foreign key: orphaned store entries remain charged to their
original organization, matching the existing accounting contract. Source and
counter ownership fields cannot be reassigned. PostgreSQL ownership guards run
only when an update changes an ownership column.

A limited write still holds the account root lock. Storage admission rejects an
increase beyond the cap and permits a decrease or unchanged usage even if the
account is already over its cap. Trace admission reads its own trigger changes
before committing. A rejected trace batch rolls back the counters and payloads;
terminal run state persists separately. Unlimited accounts skip admission usage
reads and the storage root lock, but still maintain counters so enabling limits
later observes all retained usage.

## Tasks and billing periods

Migration 014 creates one `usage_task` row per environment. The row tracks:

- active containers, reserved compute units and reserved milliseconds at all times;
- one `period_start` and completed compute units/milliseconds since that start.

The shared contribution definition is `hot.task_usage(task)` in PostgreSQL and
the `task_usage` view in SQLite. Triggers, initialization, rollover,
reconciliation and the usage readers (account and environment usage, dashboard
compute totals) all use it. Running tasks are active. A cancelled container with
a retained duration reservation remains active until termination is confirmed.
Legacy active container reservations fall back to timeout × 32 compute units
per second; active code tasks also reserve their timeout duration.

Inactive container compute usage uses nested error metering, then direct
`compute-units`, then the reserved proportional ceiling based on elapsed time
(capped at the reserved duration). Other inactive tasks contribute duration but
no container compute units. Values are nonnegative. Malformed compute-unit
values are treated as absent, including invalid JSON in SQLite, so task
completion can release reservations. Valid signed 64-bit metering is accepted;
out-of-range values use the fallback. Updates subtract the old period charge
before adding the replacement, avoiding intermediate overflow when the final
total fits. Heartbeats, timing and info-only updates
do not update the counters. On PostgreSQL, writes whose contribution does not
change (queued inserts, for example) skip the counter row entirely, so
concurrent enqueues in one environment do not serialize on it. A limited claim
reads and locks only stale counter rows, never other environments' current
rows, so it does not wait on their in-flight changes while holding the root
lock.

The migration initializes current reservations and leaves `period_start` null.
Before a limited task claim takes the root lock, each environment whose counter
period is missing or stale is initialized in its own committed transaction
under that environment's scope, recounting only tasks whose usage time is in
the period. A rejected claim cannot discard that work. A counter still stale
under the root lock (its period or environment changed in between) is
initialized there: a period change locks stale counter rows in environment
order, then recomputes
source totals in a **new statement snapshot**. A completion that committed while
the claim waited is included; subsequent completions wait for those row locks
and add their own deltas. The candidate task is locked before counters, under
its exact environment scope: PostgreSQL row locking requires the UPDATE policy,
which account Billing authority deliberately lacks for task rows. The claim
then restores Billing scope for account reads and narrows again for mutation.

## Run admission

`usage_run(root_org_id, period_start, admitted)` is the count of ledger entries
whose `admitted_at >= period_start`. Limited admission initializes a counter
lazily under the existing root lock and prunes obsolete counter periods.
The ledger INSERT trigger increments every existing matching period. This also
supports a backdated provider billing-period change. Unlimited admissions keep
writing the ledger without creating counter periods. The immutable ledger has
no runtime UPDATE or DELETE policy; pruning a counter never deletes admissions.

## Reconciliation

The singleton scheduler reconciles immediately at startup and hourly thereafter.
For each root it lists the account's environments, including orphaned counter or
store rows, in environment order. It then checks each counter separately:

1. A short transaction compares one counter (an environment's storage, an
   environment's tasks, or one run period) with its source rows in a **single
   statement**, so both come from one snapshot.
2. The same transaction adds the difference. Writes committed after that
   statement keep their own trigger deltas.

Live environments are checked under their exact environment scope, which keeps
RLS checks cheap; orphaned rows and run counters use Billing authority. No
snapshot spans an account, so a heavy account does not hold back row cleanup
for other tenants. Each repair holds at most one counter row, so it cannot
deadlock with ordered writers such as retention or period initialization.

On PostgreSQL each check takes a transaction advisory lock for its counter, so
two reconcilers never add the same difference twice; a check that another
reconciler holds is skipped until the next tick. No dedicated coordination
connection is needed. SQLite checks hold the writer lock, and a mutex serializes
local reconcilers. A changed task period skips that correction and is checked
again next time. A pruned run period is never recreated by repair. Storage
repairs use slot zero. Warnings include root/environment, differences and repair
outcomes; errors retry on the next tick. Drift is evidence to investigate, not a
reason to bypass RLS. The background source scans still scale with retained
history.

The scheduler singleton is a session advisory guard on a detached connection,
which closes on drop. A held guard does not consume the only slot of a
one-connection data pool or return a locked session to it. Allow that dedicated
coordination connection in the database connection budget.

## Scoped units of work

`ScopedPgPool::transaction` applies the validated scope once, lends a connection
to an async callback, and commits only on success. Errors and cancellation drop
the transaction and roll back. It never exposes an unscoped pool. Project list
rows and count use this API. `begin_repeatable_read` chooses isolation before
applying scope, since scope validation itself can read ownership data.

## Validation and performance

The test suite compares source totals and counters through writes, shrinking
updates, negative sizes, deletes, rollback, run cascades, task cancellation and
settlement, malformed metering, period rollover, and concurrent repair.
Populated SQLite/PostgreSQL upgrades verify call ownership, initialized bytes
and legacy active task reservations. PostgreSQL uses NOSUPERUSER NOBYPASSRLS
owner/runtime roles, checks sibling counter denial, backend slots, retention,
unit-of-work rollback and a completion racing period initialization.

Performance measurements belong to a disposable PostgreSQL 18 database with a
restricted runtime role. Compare the reviewed pre-counter implementation with
this implementation on 100,000 calls, store entries, tasks and admissions, with
two sibling environments and two task/admission periods. Warm tracked periods
before measuring ordinary admission. Record first-use and migration costs
separately. Single-client SQL transaction latency is evidence about database
work, not end-to-end request latency or a production throughput promise.

### Local comparison, 2026-10-20

PostgreSQL 18/pgvector in Docker on macOS, loopback TCP, runtime login with
NOSUPERUSER/NOBYPASSRLS. Baseline: the same code before maintained counters.
One account has two child environments and 100,000
rows in **each** of `call`, `store_map_entry`, `task`, and `run_admission`;
tasks/admissions span two periods. Three noise accounts add 1,000 rows each per
source table. Both copies were vacuumed/analyzed. No builds/tests ran during
measurement. The final composite call FK replaces the original run-only FK.

Each value is the median of three five-second `pgbench -n -M prepared -c 1 -j 1`
runs; brackets show the range of those run averages, in milliseconds. SQL
transactions reproduce account locks, scope switches, usage reads and a
representative mutation, then roll back to keep the fixture stable. The trace
case reads usage both before and after its upsert. The store case includes the
full org/env/map/key predicate. Tracked periods are initialized before these
steady-state measurements. Project pagination compares two scoped transactions
with one unit of work. Rust serialization, HTTP routing, commit/WAL durability,
network distance, multi-client contention and production proxies are excluded.

| SQL transaction | Before, ms | After, ms |
| --- | ---: | ---: |
| Limited trace batch (one upsert) | 24.305 [21.519–42.059] | 1.714 [1.479–1.822] |
| Limited store write | 927.399 [905.033–985.105] | 49.084 [45.451–52.934] |
| Limited task claim | 128.667 [127.660–135.305] | 1.984 [1.945–2.011] |
| Limited run admission | 11.830 [11.038–13.247] | 0.670 [0.562–0.740] |
| Unlimited trace upsert | 0.347 [0.343–0.578] | 0.409 [0.360–0.444] |
| Project rows plus count | 0.580 [0.550–0.702] | 0.441 [0.364–0.494] |

Always-on counters add work to unlimited writes. The small absolute overhead
here and overlapping ranges do not establish a reliable percentage throughput
cost. A trace flush writes each call twice (start and completion) in one
transaction. For 50 calls, the median server time of 20 such transactions was
10.3 ms, against 6.8 ms with the call counter triggers disabled; before counters
updated existing slots directly and ownership guards skipped unchanged rows, it
was 14.4 ms. Store writes still have an existing JSON-key lookup cost under RLS; this
change removes quota history scans, not every source-row lookup. Multi-client
throughput and proxy behavior require deployment-specific measurements.

A populated copy of the baseline was upgraded with SET ROLE to a schema owner
that has neither SUPERUSER nor BYPASSRLS. On 103,000 rows per source table,
adding/initializing task counters took 0.095 s, adding the lazy run counter took
0.027 s, and the call environment/size rewrite, ownership constraint, indexes,
storage triggers and initial totals took 5.463 s. Each phase committed
separately, matching the migration boundaries at the time; the call rewrite and
ownership constraint now run in 013, before it installs RLS, and the counters
and storage totals in 014. Source bytes, active-task counts and call ownership
matched after commit. This measured the counter additions and the complete
revised call accounting, not the earlier migration chain. Full
SQLx upgrades and refusal/rollback paths are covered independently by tests.
These fixture times do not size a production maintenance window: rehearse on a
recent snapshot with representative history, account count, storage and WAL.

First-period initialization for the 100,000-row account, measured separately
under the restricted role, took 172.97 ms for tasks and
44.63 ms for runs (including one fresh psql connection each).
That measurement predates per-environment initialization, which now runs before
the claim takes the account lock and scans only each environment's tasks in the
period; still include it in billing-period rollover and first-enforcement
rehearsals.
