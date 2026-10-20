# PR #101 follow-up handoff

Working note for whoever continues the sub-organizations and PostgreSQL row-level
security work (PR #101). It records what is done, what is in progress, and the
detailed plan for each remaining item. Delete this file, or fold the parts worth
keeping into `RLS_SETUP.md` / `RLS_COVERAGE.md`, before the branch merges.

Written 2026-10-20. Line numbers refer to branch `pr101-fixes` at the commit that
adds this file.

---

## 1. Where things stand

### Branches

| Branch | State |
| --- | --- |
| `codex/sub-orgs-rls` (PR #101) | Still at `262bcde`. Not modified by the fix round. |
| `pr101-fixes` | `262bcde` + 10 fix commits + this note. Pushed to `origin` as its own branch. No PR opened from it yet. |
| `main` | `4e2ef13`, 6 commits ahead of the PR's merge base `4466aee` (#95 Rust 1.99.1 and deps, #96 to #100 language fixes). `main` has migrations 001 to 009 only, so the PR's 010 to 018 need no renumbering. |

A paired private deployment branch exists with its own handoff note. Nothing in
this file depends on it.

### Fix commits on `pr101-fixes`

All 15 findings from the PR review are fixed.

| Commit | Summary |
| --- | --- |
| `a51ae54` | Apply database scope once per transaction (`hot.apply_scope`), verify environments up front, make policy helpers inlinable. |
| `91f1e76` | Scope event publishing, the email outbox and billing webhooks. |
| `d97a68b` | Keep call history on subscription lookup errors (`-1` = keep) and hold the scheduler singleton lock. |
| `a531ce7` | Admit only runnable work; `RunError::Suspended` vs `QuotaExceeded`; `TaskError::QuotaExhausted` fails the task instead of redelivering forever. |
| `cd017cf` | Skip storage accounting (root lock and recount) for unlimited accounts; expire trace-writer run scopes (finished-run tombstones, 300 s TTL). |
| `7527481` | Compare permission grants as sets; clear cookies when a session cannot be used. |
| `d9fabd2` | Keep observed (unenforced) public traffic off the shared request budget. |
| `a30bcb6` | Run migrations and bootstrap only with `db.migration-uri` (`HOT_DB_MIGRATION_URI`). |
| `aa57cb9` | Document scope application and managed PostgreSQL rollout. |
| `b91782b` | Let billing webhooks read org plans to resolve their account (`BootstrapOperation::BillingWebhook`). |

### Test status

- During the fix round the full workspace suite passed against PostgreSQL
  (2,567 tests, 0 failures). `b91782b` was added afterwards and verified with
  targeted tests plus an end-to-end restricted-role run (migrate as a
  `NOSUPERUSER NOBYPASSRLS` owner, provision the runtime role, run `hot db status`
  and `hot app` as the runtime login). Rerun the full suite before merging.
- Migrations 010 to 018 are unshipped and were edited in place (011, 012, 015).
  Any database that applied an earlier revision fails sqlx checksum validation and
  must be recreated. The counter work below edits 013, 014 and 015 again.

### Fix-round performance numbers (PG 18, restricted runtime role)

| Query | PR as written | After fixes |
| --- | --- | --- |
| Environment run count, 20k rows | 465 ms | 17 ms |
| call join run, 100k calls | 3.1 s | 0.2 s |
| Read set | 776 ms | 61 ms |
| Billing task aggregate | about 3 s | 99 ms |
| Account call-bytes SUM | about 4.6 s at 54k calls | 596 ms at 100k calls |
| `run_admission` COUNT, 50k rows | 1.0 s | 42 ms |

The last three are still proportional to history. Section 3 replaces them.

---

## 2. Landing plan

1. Decide how the fixes reach PR #101:
   - (a) fast-forward `codex/sub-orgs-rls` to `pr101-fixes` (the PR then carries
     the fixes; cleanest review history), or
   - (b) open a PR from `pr101-fixes` into `codex/sub-orgs-rls`. Note: CI
     workflows only run on PRs into `main`, so a stacked PR gets no CI, or
   - (c) open a PR from `pr101-fixes` into `main` that supersedes #101.
2. Rebase onto `main` (`4e2ef13`). Expect conflicts in `Cargo.toml` /
   `Cargo.lock` (#95 bumped Rust to 1.99.1 and many dependencies; the PR adds
   dependencies). Re-resolve the lockfile rather than hand-merging it. No
   migration conflicts are expected.
3. After the rebase: `cargo fmt --check`, `cargo clippy --workspace --all-targets
   -- -D warnings`, `cargo test` (SQLite), the PostgreSQL suite (section 8),
   `cargo run test` (Hot tests) and `cargo run -- check`.
4. Keep the paired private deployment change version-aligned: it pins the public
   release that contains #101 and these fixes.
5. Remove this file (or fold it into the RLS docs) before merging.

---

## 3. In progress: maintained account usage counters

Status: design settled, prototype trigger benchmarked on a scratch database,
**no code committed**. This section is the full plan.

### 3.1 Problem

For accounts with limits, three admission paths recount history under the
account's root org row lock (`SELECT ... FROM org WHERE org_id = root FOR UPDATE`),
which serializes every writer in the account for the duration of the recount.

| Path | Code | Current cost |
| --- | --- | --- |
| Store writes and trace batches | `crates/hot/src/store/quota.rs`: `QuotaKind::usage_sql()` (line 19) is run by `begin!` (line 160) and again by `Budget::check_postgres` / `check_sqlite` (lines 51, 70) | SUM over all `store_map_entry` rows, or over `call` join `run` join `env` join `org`, for the whole account. Twice per trace batch. About 0.6 s at 100k calls. |
| Task claims | `crates/hot/src/db/task/quota.rs`: `claim_for_worker_with_quota` (line 56), aggregate at line 141 (PG) / 229 (SQLite) | Five-way aggregate over the account's active tasks plus every task in the billing period. 99 ms at 40k tasks. |
| Run admission | `crates/hot/src/db/run/quota.rs`: `admission!` macro (line 79), COUNT at line 118 | `COUNT(*)` over `run_admission` since the period start, per admission. 42 ms at 50k. |

Unlimited accounts (`limit < 0`) already skip the store/call recount and the
task aggregate (`usage_required`). Run admission always takes the root lock.

### 3.2 Semantics that must not change

- Store and call bytes: `SUM(CASE WHEN size < 0 THEN 0 ELSE size END)` over the
  account (root org plus direct children, `o.org_id = root OR o.parent_org_id =
  root`). Store rows are matched by `store_map_entry.org_id`; call rows through
  `run.env_id` and `env.org_id`.
- `Budget::check_total` (line 40): a write is rejected only when
  `total > used && limit >= 0 && total > limit`. A write that does not increase
  usage is allowed even over the limit. `Budget::replace` (line 85) keeps its
  incremental check. `used` is read at `begin!` under the root lock.
- Calls: `check_postgres` recounts after the batch's upserts inside the same
  transaction, so it must see the transaction's own changes.
- Runs: `COUNT(*) WHERE root_org_id = root AND admitted_at >= period`, where
  period is `org_plan.current_period_start` or the UTC calendar month start.
  `run_admission` rows are immutable and never deleted at runtime.
- Tasks: the five values of the current aggregate (formulas in 3.5), with the
  period test `COALESCE(stop_time, start_time, created_at) >= period`.
- Every admission check stays under the root org lock, as today.

### 3.3 Design overview

Three counter tables, maintained by triggers in the same transaction that
changes the counted rows, read under the root lock instead of recounting:

| Table | Key | Holds | Maintained by |
| --- | --- | --- | --- |
| `usage_storage` | `(env_id, slot)` | `store_bytes`, `call_bytes` (+ `org_id`) | Triggers on `call`, `store_map_entry`; BEFORE DELETE trigger on `run` |
| `usage_task` | `env_id` | active containers, reserved CU, reserved ms (always current); `period_start`, used CU, used ms (for one tracked period) | Trigger on `task` |
| `usage_run` | `(root_org_id, period_start)` | `admitted` | Trigger on `run_admission` INSERT; rows created lazily per period |

Why per environment rather than per account: triggers only know the row's
environment cheaply, env-scoped writers can then write counters under the same
policy as the rows they count, re-parenting an org needs no counter moves, and
accounts have few environments (billing scope already precomputes up to 256).
Reads aggregate the account's rows (`JOIN org o ON o.org_id = u.org_id WHERE
o.org_id = root OR o.parent_org_id = root`).

Counters are maintained for every account, limited or not ("always on"). See
3.12 for the lazy alternative and why it was not chosen.

### 3.4 `usage_storage`

```sql
CREATE TABLE hot.usage_storage (
    env_id uuid NOT NULL,
    org_id uuid NOT NULL,
    slot smallint NOT NULL,
    store_bytes bigint NOT NULL DEFAULT 0,
    call_bytes bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (env_id, slot)
);
CREATE INDEX usage_storage_org_idx ON hot.usage_storage (org_id);
```

- **Slots.** PostgreSQL writes to `slot = pg_backend_pid() % 16`. With one row
  per environment, every trace batch for that environment would hold the row
  lock from its first call upsert until commit, serializing the writer's shards
  (4 per process by default, `HOT_DB_WRITER_SHARDS`) and processes. Limited
  accounts already serialize on the root lock, but unlimited accounts do not
  today. Readers `SUM` the slots. SQLite always uses slot 0 (single writer).
- **No FK to `env`.** `store_map_entry` has no env FK either; leaving counter
  rows when an env is deleted keeps orphaned store bytes counted under their org,
  as the current SUM does. (`Env::delete_env` has no callers today.)
- `org_id` comes from `env.org_id`. `env` is readable under every scope that
  writes calls or store entries (env, queue, scheduler, retention, alerts,
  billing), checked against the `env` read policy in 011.

Call triggers (PostgreSQL):

```sql
CREATE FUNCTION hot.count_call_storage() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    delta bigint := GREATEST(NEW.size, 0)
        - CASE WHEN TG_OP = 'UPDATE' THEN GREATEST(OLD.size, 0) ELSE 0 END;
    owner_env uuid;
    owner_org uuid;
BEGIN
    IF delta = 0 THEN RETURN NULL; END IF;
    SELECT e.env_id, e.org_id INTO owner_env, owner_org
      FROM hot.run r JOIN hot.env e ON e.env_id = r.env_id
     WHERE r.run_id = NEW.run_id;
    INSERT INTO hot.usage_storage AS u (env_id, org_id, slot, call_bytes)
    VALUES (owner_env, owner_org, pg_backend_pid() % 16, delta)
    ON CONFLICT (env_id, slot) DO UPDATE SET call_bytes = u.call_bytes + EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_call_storage_insert AFTER INSERT ON hot.call
    FOR EACH ROW EXECUTE FUNCTION hot.count_call_storage();
CREATE TRIGGER count_call_storage_update AFTER UPDATE OF size ON hot.call
    FOR EACH ROW WHEN (OLD.size IS DISTINCT FROM NEW.size)
    EXECUTE FUNCTION hot.count_call_storage();

-- Bulk deletes (retention) aggregate once per statement.
CREATE FUNCTION hot.uncount_call_storage() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.usage_storage AS u (env_id, org_id, slot, call_bytes)
    SELECT e.env_id, e.org_id, pg_backend_pid() % 16, -SUM(GREATEST(d.size, 0))
      FROM deleted d JOIN hot.run r ON r.run_id = d.run_id JOIN hot.env e ON e.env_id = r.env_id
     GROUP BY e.env_id, e.org_id
    HAVING SUM(GREATEST(d.size, 0)) <> 0
     ORDER BY e.env_id
    ON CONFLICT (env_id, slot) DO UPDATE SET call_bytes = u.call_bytes + EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER uncount_call_storage AFTER DELETE ON hot.call
    REFERENCING OLD TABLE AS deleted FOR EACH STATEMENT
    EXECUTE FUNCTION hot.uncount_call_storage();
```

Notes:

- Separate INSERT and UPDATE triggers because a `WHEN` clause that references
  `OLD` is not allowed on INSERT triggers, and transition tables cannot be
  combined with column lists or multiple events.
- An `INSERT ... ON CONFLICT DO UPDATE` on `call` fires the INSERT trigger for
  new rows and the UPDATE trigger for merged rows. The writer's upsert always
  recomputes `size` (start event, then end event with the return value).
- `ORDER BY e.env_id` makes multi-environment deleters take counter row locks in
  a consistent order.
- **Run deletion.** `call.run_id` is `ON DELETE CASCADE`. Cascaded call deletes
  run after the run row is gone, so the statement trigger cannot resolve the
  environment and would drop those bytes. Fix: a BEFORE DELETE row trigger on
  `run` deletes the run's calls first, while the run still exists:

  ```sql
  CREATE FUNCTION hot.delete_run_calls() RETURNS trigger LANGUAGE plpgsql AS $$
  BEGIN
      DELETE FROM hot.call WHERE run_id = OLD.run_id;
      RETURN OLD;
  END $$;
  CREATE TRIGGER delete_run_calls BEFORE DELETE ON hot.run
      FOR EACH ROW EXECUTE FUNCTION hot.delete_run_calls();
  ```

  The explicit delete is subject to RLS, unlike the FK cascade. The run and call
  delete policies allow the same scopes today (env writes, queue, scheduler,
  retention, alerts; plus retention for calls), so every call the cascade would
  remove is visible to the explicit delete. Keep it that way, or reconciliation
  (3.10) will report the drift. Item 4 (env_id on `call`) removes this special
  case entirely.
- Store triggers follow the same pattern on `store_map_entry` using the row's
  own `org_id` / `env_id` (no lookup), `AFTER INSERT`, `AFTER UPDATE OF size`
  with `WHEN`, and a statement-level `AFTER DELETE` (map deletes and clears
  remove many entries at once). `store_map_entry` org/env are immutable
  (`immutable_resource_scope`), so updates never move bytes between envs.
- Store write sites to keep in mind: `crates/hot/src/store/postgres.rs` inserts
  at lines 82/87 and deletes at 375/551/568/579; SQLite equivalents at
  `store/sqlite.rs` 87/92 and 386/562/577/588.

SQLite has row triggers only:

```sql
CREATE TRIGGER count_call_storage_insert AFTER INSERT ON call
WHEN MAX(NEW.size, 0) <> 0
BEGIN
    INSERT INTO usage_storage (env_id, org_id, slot, call_bytes)
    SELECT e.env_id, e.org_id, 0, MAX(NEW.size, 0)
      FROM run r JOIN env e ON e.env_id = r.env_id
     WHERE r.run_id = NEW.run_id
    ON CONFLICT (env_id, slot) DO UPDATE SET call_bytes = call_bytes + excluded.call_bytes;
END;
```

plus matching UPDATE OF size and DELETE triggers, the same for
`store_map_entry`, and `BEFORE DELETE ON run` deleting the run's calls. SQLite
gotchas: `INSERT ... SELECT ... ON CONFLICT` needs a `WHERE` on the SELECT to
parse (present above); `MAX(x, NULL)` is NULL, so wrap nullable inputs in
`IFNULL` so a NULL contribution never reaches a `NOT NULL` counter.

### 3.5 `usage_task`

```sql
CREATE TABLE hot.usage_task (
    env_id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    active_containers bigint NOT NULL DEFAULT 0,
    reserved_compute_units bigint NOT NULL DEFAULT 0,
    reserved_duration_ms bigint NOT NULL DEFAULT 0,
    period_start timestamptz,          -- NULL until the first claim sets it
    compute_units bigint NOT NULL DEFAULT 0,
    duration_ms bigint NOT NULL DEFAULT 0
);
CREATE INDEX usage_task_org_idx ON hot.usage_task (org_id);
```

No slots: task status changes are infrequent single-row statements.

Per-task contribution, copied from the current PostgreSQL aggregate
(`task/quota.rs` line 141). With

- `active(t)` = `task_status_id = 2 OR (task_status_id = 5 AND task_type = 'container' AND reserved_duration_ms > 0)`
- `ts(t)` = `COALESCE(stop_time, start_time, created_at)`

| Column | Contribution of task `t` |
| --- | --- |
| `active_containers` | 1 if `active(t) AND task_type = 'container'`, else 0 |
| `reserved_compute_units` | if `active(t)`: `reserved_compute_units` when > 0, else `(GREATEST(timeout_ms, 0) * 32 + 999) / 1000` for containers, else 0 |
| `reserved_duration_ms` | if `active(t)`: `reserved_duration_ms` when > 0, else `GREATEST(timeout_ms, 0)` |
| `compute_units` (period) | if `NOT active(t) AND task_type = 'container' AND ts(t) >= period_start`: `GREATEST(COALESCE(result->'$val'->'err'->>'compute-units', result->>'compute-units', CASE WHEN reserved_duration_ms > 0 THEN (reserved_compute_units * LEAST(GREATEST(COALESCE(duration_ms, 0), 0), reserved_duration_ms) + reserved_duration_ms - 1) / reserved_duration_ms ELSE reserved_compute_units END, 0), 0)` |
| `duration_ms` (period) | if `NOT active(t) AND ts(t) >= period_start`: `GREATEST(COALESCE(duration_ms, 0), 0)` |

Put the contribution in one SQL function (for example
`hot.task_usage(t hot.task)` returning a composite) used by the trigger, the
migration initialization and reconciliation, so the formula exists once per
backend. Queued tasks contribute 0 unless a retry left old values behind; the
trigger handles any row state generically.

Trigger: `AFTER INSERT OR DELETE` plus `AFTER UPDATE OF task_status_id,
task_type, timeout_ms, reserved_compute_units, reserved_duration_ms, result,
duration_ms, stop_time, start_time, created_at` with a `WHEN` comparing those
columns, so heartbeats (`worker_id`, `last_heartbeat_at`), `info`, `timing`,
`container_id`, `run_id` and `next_retry_at` updates never fire it. It upserts
the env row:

```sql
ON CONFLICT (env_id) DO UPDATE SET
    active_containers = u.active_containers + EXCLUDED.active_containers,
    reserved_compute_units = u.reserved_compute_units + EXCLUDED.reserved_compute_units,
    reserved_duration_ms = u.reserved_duration_ms + EXCLUDED.reserved_duration_ms,
    compute_units = u.compute_units
        + CASE WHEN new_ts >= u.period_start THEN new_cu ELSE 0 END
        - CASE WHEN old_ts >= u.period_start THEN old_cu ELSE 0 END,
    duration_ms = ... same shape ...
```

A freshly inserted row has `period_start` NULL, so its period columns stay 0
until the claim path initializes them.

**Malformed `compute-units`.** The current aggregate casts the JSON value with
`::bigint`; a non-integer string makes every claim in the account fail. Inside a
trigger the same cast would make the task's completion UPDATE fail instead
(worse: the task cannot finish). Use a guarded cast (`CASE WHEN v ~
'^-?[0-9]{1,18}$' THEN v::bigint END`) and treat malformed values as missing.
This is a deliberate deviation; confirm it (3.13).

**Period handling in the claim path.** The account's period is
`org_plan.current_period_start` or the calendar month start, as today. Inside the
claim transaction (READ COMMITTED, root org row already locked):

1. For each env of the account, `INSERT INTO usage_task (env_id, org_id) ...
   ON CONFLICT DO NOTHING`, so every row exists. A concurrent uncommitted insert
   by a trigger makes this wait, then do nothing.
2. `SELECT env_id FROM usage_task WHERE env_id = ANY($envs) AND period_start IS
   DISTINCT FROM $period ORDER BY env_id FOR UPDATE`. Writers already holding
   those rows finish first.
3. In a **new statement** (fresh READ COMMITTED snapshot, taken after the locks),
   recompute `compute_units` and `duration_ms` for the stale envs and set
   `period_start = $period`.
4. Read `SUM` of the five columns over the account's rows.

Correctness: a task writer locks its env row in its trigger, in the same
statement as its task change. Writers that committed before step 2 are visible to
step 3. Writers in flight at step 2 are waited out. Writers after step 2 block in
their trigger until the claim commits, then apply their delta relative to the new
`period_start`; their task change was invisible to step 3. Lock order is root
org row, then counter rows. Task writers outside the claim take only their own
counter row, so no cycle exists.

Re-initialization costs one scan of each env's tasks per billing period; the
current code pays that scan on every claim.

### 3.6 `usage_run`

```sql
CREATE TABLE hot.usage_run (
    root_org_id uuid NOT NULL,
    period_start timestamptz NOT NULL,
    admitted bigint NOT NULL,
    PRIMARY KEY (root_org_id, period_start)
);
CREATE FUNCTION hot.count_run_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE hot.usage_run SET admitted = admitted + 1
     WHERE root_org_id = NEW.root_org_id AND period_start <= NEW.admitted_at;
    RETURN NULL;
END $$;
CREATE TRIGGER count_run_admission AFTER INSERT ON hot.run_admission
    FOR EACH ROW EXECUTE FUNCTION hot.count_run_admission();
```

- Each existing row `(root, P)` always equals `COUNT(*) WHERE admitted_at >= P`,
  for any `P`, including admissions whose clock is slightly before `P`.
- Rows are created lazily in `admission!` when the account is limited and the
  `(root, period)` row is missing: delete the root's other rows (prune old
  periods), then insert `COUNT(*)` since the period. All `run_admission` inserts
  happen under the root org lock, and so does creation, so creation never races
  an insert. Unlimited accounts never get rows; their trigger is a no-op indexed
  UPDATE.
- No DELETE handling: `run_admission` has no runtime UPDATE/DELETE policy and is
  immutable by design (014).
- SQLite: check that `period_start` and `admitted_at` are bound in the same text
  format, since SQLite compares timestamps as text. The existing COUNT already
  depends on this; the trigger must compare the same way.

### 3.7 RLS policies and grants

Counter tables get `ENABLE` and `FORCE ROW LEVEL SECURITY`, so
`provision_runtime.sql` grants the runtime role DML (it grants DML only on
RLS-forced tables and SELECT on others). Triggers run as the invoking role under
the session's scope, so policies must admit every scope that writes the counted
rows. `INSERT ... ON CONFLICT DO UPDATE` also needs SELECT visibility of the
existing row.

| Table | SELECT, INSERT, UPDATE allowed for | DELETE |
| --- | --- | --- |
| `usage_storage` | `hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)` (env scope; reads may use `scope_env_allows`); `scope_maintenance` of `queue`, `scheduler`, `retention`, `alerts` (call writers); `hot.scope_billing_org(org_id)` (admission reads, reconciliation) | none |
| `usage_task` | env scope as above; `queue`, `scheduler`, `retention`, `alerts` (task writers); billing via `hot.scope_billing_org(org_id)` (claim re-init, reads, reconciliation) | none |
| `usage_run` | `hot.scope_maintenance('billing') AND root_org_id = hot.scope_org_id()`, the same rule as the `run_admission` insert policy | billing, for pruning old periods |

Also:

- Follow the house style in 011 (`hot_scope_read`, `hot_scope_insert`,
  `hot_scope_update`, `hot_scope_delete`).
- Add the tables to `RLS_COVERAGE.md`.
- Check tests that enumerate protected tables and the runtime-role qualification
  in `crates/hot/src/db/rls.rs` (it flags reachable tables without RLS).
- An env-scoped session can update its own env's counters. That is within its
  authority (it can already change its own calls and store entries) and
  reconciliation repairs any mistake. It can never reach another env.

### 3.8 Migration placement and initialization

Unshipped migrations may be edited in place, so no new migration files:

| Migration | Adds |
| --- | --- |
| `013_task_quota_reservations` | `usage_task`, contribution function, trigger, policies, initialization |
| `014_run_admission` | `usage_run`, trigger, policies (no backfill; rows are created per period) |
| `015_call_storage_accounting` | `usage_storage`, call and store triggers, `run` BEFORE DELETE trigger, policies, initialization **after** the existing size backfill |

Both backends. Initialization rules:

- `CREATE TRIGGER` takes a SHARE ROW EXCLUSIVE lock on the table until the
  migration commits, which blocks concurrent writes. A recount in the same
  migration transaction after creating the trigger is therefore exact even while
  old-version processes are still running.
- PostgreSQL migrations run as an owner subject to FORCE RLS. Set scope the way
  014 and 015 do: a maintenance scope that can read the table (for example
  `scheduler` or `retention`), or a per-env loop with `hot.apply_scope(...)`,
  restoring the prior settings at the end.
- `usage_task`: one row per existing env with exact active columns and
  `period_start` NULL.
- `usage_storage`: one row per env (slot 0) with exact store and call bytes.
- Migration 016 to 018 do not modify counted rows, so later migrations cannot
  skew the counters.
- Initialization adds a full scan of `call`, `store_map_entry` and `task` at
  migration time. Record its duration in the rehearsal (section 7).

### 3.9 Rust read paths

- `store/quota.rs`: replace `usage_sql()` with a counter sum per kind
  (`SUM(store_bytes)` or `SUM(call_bytes)` over the account's `usage_storage`
  rows). `begin!` reads it under the root lock as `used`; `check_postgres` /
  `check_sqlite` read it again after the batch (the transaction's own trigger
  updates are visible). `Budget`, `replace` and `check_total` stay unchanged.
- `task/quota.rs`: keep `usage_required` (unlimited accounts skip). Replace both
  aggregates with steps 1 to 4 of 3.5, returning the same five-value tuple to
  `check_capacity`.
- `run/quota.rs`: replace the COUNT with the `usage_run` row read or lazy
  creation (3.6). Keep the `used >= limit` check.
- Keep the PostgreSQL / SQLite split the files already use (macros
  `begin!` / `admission!`, explicit SQLite branches in `task/quota.rs`).

### 3.10 Reconciliation

New module (for example `crates/hot/src/db/usage.rs`) with
`reconcile_account(db, root_org_id) -> UsageDrift`, using a snapshot diff, which
needs no locks:

1. Open a REPEATABLE READ transaction under `Maintenance { Billing, root }` (on
   SQLite a read transaction is already a consistent snapshot). For each env of
   the account, read the counters and recompute the truth with the original
   formulas: store and call byte sums, the five task values for the row's own
   `period_start`, and `COUNT(*)` per `usage_run` row. The difference is the
   drift.
2. In a new READ COMMITTED transaction, apply the drift as a relative delta:
   storage to slot 0 (upsert); task only `WHERE period_start IS NOT DISTINCT
   FROM` the period read in step 1; run per row.

Why this is exact: counters and counted rows change in the same transaction, so
one snapshot sees both or neither for every writer. The difference therefore
equals the drift accumulated outside the triggers. Applying it as a delta keeps
every change committed after the snapshot. Do not use one `UPDATE counter =
(SELECT recount)` statement: under READ COMMITTED its subquery snapshot predates
the row lock and would overwrite concurrent deltas.

Wire it into the scheduler as a periodic job (hourly or daily) over roots with
counter rows. Log non-zero drift as a warning with the env and kind, since drift
means a write path bypassed the triggers (for example manual SQL). Never hold the
root org lock while reconciling. Consider a CLI entry point for operators.

### 3.11 Alternatives considered and rejected

1. **Maintain counters only for limited accounts** (rows exist means
   maintained). Zero cost for unlimited accounts, but creating a row races
   writers whose trigger found no row. Race-free creation needs either shared
   advisory locks in every trigger plus an exclusive lock for creation (lock
   table pressure when one statement touches many envs, so keys must be
   bucketed, and creation stalls every writer in the bucket during the recount),
   or an xmin fence (record `pg_snapshot_xmax(pg_current_snapshot())` after the
   row commits, wait until the oldest running xid passes it, then snapshot-diff).
   Both are much more intricate. Revisit only if always-on cost proves too high.
2. **Deltas computed in Rust** (`RETURNING` sizes): misses cascades, bulk deletes
   and any other writer. PostgreSQL before 18 has no `RETURNING OLD`, and a CTE
   pre-read of the old size is racy under concurrent upserts of the same row.
3. **Deferred constraint triggers** applying deltas at commit (shorter lock
   hold): events still queue per row, and cascaded call deletes cannot resolve
   their env at commit time.
4. **Append-only delta log plus compaction**: no row-lock contention, but every
   change becomes two writes, readers sum all uncompacted deltas, and compaction
   needs its own scheduling.
5. **Daily buckets for period values**: needs an expression index on task
   timestamps and an exact recount of the partial first day.
6. **One counter row per account**: every writer in an account contends on one
   row, and env-scoped writers would need write access to account-level rows.

### 3.12 Prototype measurements so far

Scratch database copied from a migrated test database; runtime role; environment
scope; prototype row trigger on `call` (run lookup plus slot upsert):

- Trigger function self time (`track_functions = 'all'`,
  `pg_stat_user_functions`): **24.5 µs per call write** (21,960 calls, 538 ms).
  For comparison in the same run: the existing `prevent_scope_reassignment`
  trigger on `call` costs 11.3 µs per merging upsert, and `hot.apply_scope` 44.8 µs per
  transaction.
- Server-side batches (300 transactions of 20 upserts inside `DO` blocks):
  6,370 ms without the trigger, 6,550 ms with it (+2.8%).
- Separate statements from one psql session (150 transactions of 20):
  6,172 to 6,236 ms without, 6,246 to 6,337 ms with (+1.2% to 1.6%).
- pgbench `-M prepared`, one client, writer's exact upsert SQL: 4.5 to 5.5 ms
  per transaction without, 7.4 to 9.4 ms with. BEGIN and COMMIT latencies also
  doubled (BEGIN 0.09 to 0.18 ms, COMMIT 0.8 to 1.5 to 1.9 ms), during autovacuum
  on `call`, checkpoint writes and host load averages of 3 to 11. Each trigger
  component measured alone (lookup only, update only, upsert only) was within
  noise of the baseline; only the combination showed the jump. **Inconclusive:
  re-measure on a quiet machine** before deciding whether the hot path needs
  more work. Options if it does: use the verified scope env
  (`hot.scope_env_verified`) instead of the run lookup in environment scope
  (saves about 8 µs), or item 4.
- Updating the same counter row many times in one transaction walks a growing
  HOT chain (quadratic). The trace writer flushes at 100 pending writes
  (`WRITE_BATCH_SIZE` in `database_writer.rs`), so this stays small.
- Multi-client pgbench numbers on this machine were too noisy to quantify slot
  benefits; the slot design rests on the lock-hold argument in 3.4.

### 3.13 Decisions to confirm

1. Accept always-on maintenance for unlimited (self-hosted) accounts: about
   25 µs CPU plus one extra row version and WAL record per call write.
   Recommended, pending the quiet-machine re-measurement.
2. Treat malformed `compute-units` as missing instead of erroring (3.5).
   Recommended.
3. 16 slots in PostgreSQL, none in SQLite.
4. Reconciliation repairs automatically and logs a warning (recommended), or
   only reports.
5. Usage display stays on its own queries for now (item 6).

### 3.14 Test plan

- **SQLite, always run.** For each counter, after every step, compare the
  counters with the original recount queries:
  - Calls: insert, upsert merge (size change), negative size, retention delete
    (`Call::delete_older_than`, `delete_older_than_for_org`), run delete
    (`Run::delete_run`, exercising the BEFORE DELETE trigger).
  - Store: insert, overwrite larger and smaller, single delete, map delete,
    clear.
  - Tasks: queued insert, claim, completion with `compute-units`, failure using
    the proportional fallback, cancellation request keeping its reservation,
    settlement releasing it (`cancellation_quota.rs` in `hot_task_worker`),
    reaper requeue, retry, period rollover.
  - Runs: admissions across a period change and a backdated period start.
  - A randomized sequence test (property style) comparing counters with recounts.
- **PostgreSQL** (section 8), as the runtime role under RLS:
  - Env-scope writes update counters; another env cannot read or write them.
  - Billing scope reads the whole account.
  - Retention-scope deletes update counters.
  - Two connections writing one env land in different slots and sum exactly.
  - Reconciliation repairs drift injected as the owner while writers run.
- **Unchanged and passing:** existing quota tests in `store/quota.rs`,
  `task/quota.rs`, `run/quota.rs`, the trace-writer quota tests,
  `hot_task_worker` cancellation quota tests and `subscription/runtime_usage.rs`.
- **Migrations:** apply 001 to 018 to a database that already holds data, on
  both backends, and check the initialized counters against recounts.

### 3.15 Measurement plan (done means before/after at 100k+ rows)

Seed one limited account (and a few noise accounts) with at least 100k calls,
100k store entries, 100k tasks across two periods and 100k `run_admission` rows.
Time, before and after:

- `begin!` + `check_postgres` for one trace batch
- one store write
- one task claim
- one run admission

Also time a trace-upsert pgbench run for an unlimited account (write-path
overhead), and the migration's counter initialization on that data set.

### 3.16 Implementation checklist

1. 013 (PG and SQLite): `usage_task`, contribution function, trigger,
   initialization, policies.
2. 014: `usage_run`, trigger, policies.
3. 015: `usage_storage`, call and store triggers, `run` BEFORE DELETE trigger,
   initialization, policies.
4. `RLS_COVERAGE.md`, `rls.rs` expectations, protected-table test lists.
5. Rust read paths (3.9).
6. Reconciliation module, scheduler job, logging (3.10).
7. Tests (3.14).
8. Measurements (3.15); note migration-time cost in `RLS_SETUP.md`.
9. Commit in reviewable steps (schema and triggers, read paths,
   reconciliation, tests and docs).

---

## 4. Planned: store `env_id` on `call`

The `call` policies (011, line 347 and following) test `EXISTS (SELECT 1 FROM
hot.run p WHERE p.run_id = call.run_id)` per row, which costs a run lookup per
call row under RLS (call join run at 100k calls: 0.2 s after the fixes). Every
other env-owned table filters on its own `env_id`.

Plan:

- Add `call.env_id`. Backfill it inside migration 015's per-env loop, which
  already rewrites every retained call row, so no second table rewrite. Then
  `SET NOT NULL` (a validating scan, no rewrite).
- Set it in the trace writer's inserts (`database_writer.rs` PostgreSQL upsert
  around line 1129, SQLite insert around line 2528; the writer already knows the
  env from the execution context and verifies run ownership in
  `trace_batch_scope`). Keep it immutable via `prevent_scope_reassignment`, and
  check it against `run.env_id` (a BEFORE INSERT trigger, or a composite FK on
  `(run_id, env_id)` backed by a unique index on `run`).
- Rewrite the call policies to `hot.scope_env_allows(env_id)` /
  `hot.scope_env_writes(env_id)` like other env tables.
- Index `(env_id, start_time)` for retention (`delete_older_than_for_org` joins
  run to filter by env today) and usage queries.
- Counters: the call triggers drop the run lookup and the `run` BEFORE DELETE
  trigger (cascaded deletes carry `env_id` in the transition table).
- SQLite parity, plus updating tests that insert calls directly.

Doing this before the counters simplifies 3.4; doing it after is also fine.

---

## 5. Planned: transactions spanning several statements

`ScopedPgPool` runs every pooled statement as BEGIN, `SELECT
hot.apply_scope(...)`, the statement, COMMIT: four round trips and one scope
application (about 45 µs of function time) per statement. Plan:

- Add a unit-of-work API (for example `pool.transaction(|tx| ...)`) that applies
  the scope once and runs several statements.
- Move hot paths first: API handlers that issue several reads, and per-request
  loaders.
- Keep the task-local scope rules: an explicit `with_scope` wins, and
  `tokio::spawn` does not inherit scope.
- Measure request latency before and after on a restricted-role setup.

---

## 6. Planned: usage display queries

`OrgUsageStats::calculate_environment` (`db/subscription/runtime_usage.rs`) and
`calculate` / `calculate_account` (`db/subscription.rs`, lines 941 and 883)
still SUM and COUNT history on every usage request. Their semantics differ from
enforcement (call bytes within the retention window, task counts by
`created_at`). After section 3 lands, consider reading store bytes and the run
count from the counters and leave the rest. This is optional; no correctness
issue.

---

## 7. Rollout notes (public-safe)

- Recreate any development or rehearsal database that applied earlier revisions
  of 010 to 018 (checksums changed).
- Migration 015 rewrites every retained `call` row in one transaction: WAL about
  the size of the table, roughly double the table size until vacuum, replica
  lag. Counter initialization adds full scans of `call`, `store_map_entry` and
  `task`. Time both on a copy of production-sized data.
- Rerun `provision_runtime.sql` after migrations that add tables (the counter
  tables need runtime grants).
- Managed PostgreSQL without a superuser cannot grant BYPASSRLS, so `pg_dump` of
  forced-RLS tables fails. Rely on snapshots and point-in-time recovery
  (documented in `RLS_SETUP.md`).

---

## 8. Verification recipes

PostgreSQL for tests (the container used during this work):

```bash
docker run -d --name pr101-pg -e POSTGRES_USER=hot -e POSTGRES_PASSWORD=hot -p 127.0.0.1:55491:5432 pgvector/pgvector:pg18
HOT_TEST_POSTGRES_URI=postgres://hot:hot@127.0.0.1:55491/hot HOT_TEST_POSTGRES_RESET_SCHEMA=1 cargo test --workspace
```

`psql` and `pgbench` come from the PostgreSQL client tools (libpq). If a test
database was ever migrated by `psql` without the sqlx ledger and lacks
`hot.apply_scope`, drop and recreate it.

Restricted-role rig (mirrors managed PostgreSQL):

1. Create an owner role `NOSUPERUSER NOBYPASSRLS` that owns the database, with
   the required extensions preinstalled by an admin.
2. Migrate with `HOT_DB_MIGRATION_URI` pointing at the owner.
3. As the owner: `psql "$OWNER_URI" -v runtime_role=hot_runtime -v schema=hot -f
   resources/db/postgres/provision_runtime.sql`.
4. Run services with `HOT_DB_URI` as the runtime login. `hot app` logs that it
   skips migrations and bootstrap.
5. Ad hoc checks as the runtime role: `BEGIN; SELECT hot.apply_scope('environment',
   '<org>', '<env>', '[]', '[]', '', ''); ...` (rows stay invisible without scope).

Benchmark harness used for 3.12:

- `CREATE DATABASE bench TEMPLATE hot` (no other connections to `hot`), then run
  `provision_runtime.sql` for the runtime role in the copy.
- Seed a user, org, env, project, build, stream and run.
- pgbench script per transaction: `BEGIN; SET LOCAL ROLE hot_runtime; SELECT
  hot.apply_scope(...)`, then 10 inserts and 10 merges using the writer's exact
  upsert SQL with call ids built from `md5(:client_id || '-' || :n || '-k')::uuid`,
  then `COMMIT`.
- `-M prepared -r` gives per-statement latency; `ALTER DATABASE ... SET
  track_functions = 'all'` with `pg_stat_user_functions` gives trigger self
  time. Watch `pg_stat_activity` for autovacuum and checkpoints during runs.

---

## 9. Build and tooling notes

- Share one `CARGO_TARGET_DIR` across worktrees; a full target directory for this
  workspace is about 23 GB.
- With a target directory outside the repository, symlink `resources` into
  `<target>/debug/` and `<target>/debug/deps/`; tests locate resources relative to
  the test binary (otherwise about 200 tests fail).
- The pre-commit hook runs `cargo fmt --check`, workspace clippy with
  `-D warnings`, a kata cross-check when the Linux target is installed,
  `cargo run -- check` (needs `hot/ctx.hot`; copy `hot/ci.ctx.hot`, never real
  credentials) and the Hot formatter check. Export `CARGO_TARGET_DIR` for it, or
  it builds a second full target directory.
- Do not set `CARGO_NET_OFFLINE`; the `hot` binary then fails to link
  (`ort-sys`).

---

## 10. Known limitations after the fix round

- Limited accounts still recount history on every admission (section 3).
- Call row visibility costs a run lookup per row (section 4).
- Every pooled statement is its own transaction (section 5).
- The trace writer keeps a finished run's scope for 300 s (tombstones, pruned
  every 60 s). Trace writes arriving later than that for a finished run fail
  scope resolution; watch writer errors under heavy backlog.
- `DatabaseScope::Bootstrap(Health)` deliberately skips `apply_scope` so health
  checks work before migrations; it can see no protected rows.
- Observe-mode rate limiting uses the in-process limiter and fails open.
- The billing webhook bootstrap scope can read `org_plan` rows for any org,
  which it needs to resolve the account; it then narrows to the account's
  billing scope.
