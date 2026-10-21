# PostgreSQL scope policy coverage

Migration 013 installs and forces RLS. Mutation policies are separate from read policies.
The scope settings are trusted backend context, not customer SQL credentials.

| Table | Ownership |
| --- | --- |
| `org` | Explicit policy in migration 013 |
| `env` | Explicit policy in migration 013 |
| `org_note` | Explicit policy in migration 013 |
| `team` | Explicit policy in migration 013 |
| `invite` | Explicit policy in migration 013 |
| `org_user` | Explicit policy in migration 013 |
| `org_usage` | Explicit policy in migration 013 |
| `org_plan` | Explicit policy in migration 013 |
| `org_child_provisioning` | Explicit policy in migration 013 |
| `api_key` | Explicit policy in migration 013 |
| `service_key` | Explicit policy in migration 013 |
| `session` | Explicit policy in migration 013 |
| `project` | Explicit policy in migration 013 |
| `domain` | Explicit policy in migration 013 |
| `run` | Explicit policy in migration 013 |
| `task` | Explicit policy in migration 013 |
| `event` | Explicit policy in migration 013 |
| `stream` | Explicit policy in migration 013 |
| `access` | Explicit policy in migration 013 |
| `agent` | Explicit policy in migration 013 |
| `workflow` | Explicit policy in migration 013 |
| `build` | Explicit policy in migration 013 |
| `call` | Exact `env_id`, validated against its run; direct policies in 017 |
| `event_handler` | Explicit policy in migration 013 |
| `mcp_tool` | Explicit policy in migration 013 |
| `webhook` | Explicit policy in migration 013 |
| `schedule` | Explicit policy in migration 013 |
| `schedule_log` | Explicit policy in migration 013 |
| `team_user` | Explicit policy in migration 013 |
| `alert_delivery` | Explicit policy in migration 013 |
| `alert_subscription_channel` | Explicit policy in migration 013 |
| `alert_subscription_destination` | Explicit policy in migration 013 |
| `file` | Explicit policy in migration 013 |
| `file_upload` | Explicit policy in migration 013 |
| `blob_object` | Explicit policy in migration 013 |
| `blob_ref` | Explicit policy in migration 013 |
| `alert_subscription` | Explicit policy in migration 013 |
| `store_map` | Explicit policy in migration 013 |
| `store_map_entry` | Explicit policy in migration 013 |
| `context` | Explicit policy in migration 013 |
| `alert` | Explicit policy in migration 013 |
| `alert_destination` | Explicit policy in migration 013 |
| `alert_channel` | Explicit policy in migration 013 |
| `user` | Explicit policy in migration 013 |
| `user_auth` | Explicit policy in migration 013 |
| `email_verification` | Explicit policy in migration 013 |
| `email_queue` | Explicit policy in migration 013 |
| `scheduler_state` | Explicit policy in migration 013 |

Reference tables deliberately have no RLS; runtime receives SELECT only:
`build_type`, `email_verification_status`, `invite_status`, `org_user_role`,
`run_status`, `run_type`, `task_status`, `team_user_role`, `org_plan_status`,
`alert_delivery_status`, `alert_destination_type`, `plan`.

SQLx migration metadata is deployment control state and receives no runtime grants.
Management credential tables introduced by migration 014 carry their own policies.

| Added control table | Ownership and operation |
| --- | --- |
| `account_request_window` | Exact root; Billing only (013) |
| `management_key` | Exact root Organization; Authentication read; immutable root (014, tightened by 018) |
| `api_key_management_issuer` | API key environment (writes need an exact environment scope); Authentication read; immutable issuer; delegated parent deletion restricted (014, 018) |

`StoreProvisioning` can read only `store_map` configuration; index DDL uses the
separately supplied schema-owner credentials. `Retention` reads session/plan
metadata and updates expired uploads. `BlobGc` reads env metadata so object
integrity checks remain effective. Organization scope does not read build rows,
even when project metadata is visible.

The runtime startup coverage check rejects any new ordinary table that lacks
both ENABLE and FORCE RLS unless it appears in the explicit reference-table
allowlist in `db/rls.rs`.
It also rejects runtime mutation privileges on those reference tables and any
runtime access to SQLx migration metadata. The restricted-role test uses the
same protected-table DML and reference-table SELECT grant rules as the deployment
script, and checks that extra reference or migration grants fail qualification.
Runtime qualification also rejects REFERENCES (including column grants), TRIGGER,
and the predefined server-file/server-program roles, which can bypass row policy
protection through foreign-key checks or direct server access.

Migration 018 validates delivery relationships before installing integrity
triggers: the alert, subscription, and destination must share an organization,
and an environment-specific subscription must match the alert environment.
Delivery ownership cannot be reassigned. SQLite has equivalent integrity
triggers, and the sender checks the subscription environment independently.

Migration 019 completes the retained-data preflight for the runtime integrity
checks introduced by 013: run/task origin runs and run events must exist in the
same environment. It reads under the named queue maintenance operation so a
non-BYPASS schema owner sees every retained relationship despite FORCE RLS.
An inconsistent link stops the upgrade for reviewed repair; no provenance is
rewritten automatically. SQLite applies the same retained-data preflight.

| Admission extension | Coverage |
| --- | --- |
| `task.reserved_compute_units`, `task.reserved_duration_ms` (015) | Existing exact task policies; root-locked admission narrows to env before claiming |
| `run_admission` (016) | Exact env or Billing root SELECT; Billing root INSERT validates org/env/root association; no runtime UPDATE/DELETE |

Run-admission backfill enumerates metadata under Membership and reads each
account under Billing, so a schema owner without BYPASSRLS retains existing
usage. The restricted-login integration test exercises durable sibling-shared
run limits and verifies that ordinary environment access cannot delete the
ledger to reset capacity.

| Maintained usage table | Coverage |
| --- | --- |
| `usage_storage` (017) | Exact env and org, account Billing, or named queue/scheduler/retention/alerts operation; immutable env/org/slot; no runtime DELETE |
| `usage_task` (015) | Exact env and org, account Billing, or named queue/scheduler/retention/alerts operation; immutable env/org; no runtime DELETE |
| `usage_run` (016) | Exact account Billing only; immutable root/period; DELETE permits pruning obsolete counter periods, never ledger rows |

Each counter table enables and forces RLS. Provisioning discovers these tables
and grants their protected DML privileges. Triggers run with the caller's
scope and rights, without SECURITY DEFINER or owner bypass. Scope policies on
the counter rows reject a sibling's env or org. Runtime access to arbitrary SQL
and construction of maintenance authority remain outside this boundary.

Migration 017 backfills `call.env_id` from its existing run, makes it required,
and validates the pair with a composite foreign key. Both ownership fields are
immutable. Call policies and retention queries use the local environment field,
so cascaded run deletion can account removed bytes without looking up the
already deleted run. PostgreSQL bulk deletes use transition tables to subtract
one aggregate per environment. SQLite enforces the ownership pair in triggers.

See [usage counter operation and validation](USAGE_COUNTERS.md) for formulas,
rollover, reconciliation and performance evidence.
