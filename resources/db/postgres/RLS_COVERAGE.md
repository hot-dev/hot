# PostgreSQL scope policy coverage

Migration 011 installs and forces RLS. Mutation policies are separate from read policies.
The scope settings are trusted backend context, not customer SQL credentials.

| Table | Ownership |
| --- | --- |
| `org` | Explicit policy in migration 011 |
| `env` | Explicit policy in migration 011 |
| `org_note` | Explicit policy in migration 011 |
| `team` | Explicit policy in migration 011 |
| `invite` | Explicit policy in migration 011 |
| `org_user` | Explicit policy in migration 011 |
| `org_usage` | Explicit policy in migration 011 |
| `org_plan` | Explicit policy in migration 011 |
| `org_child_provisioning` | Explicit policy in migration 011 |
| `api_key` | Explicit policy in migration 011 |
| `service_key` | Explicit policy in migration 011 |
| `session` | Explicit policy in migration 011 |
| `project` | Explicit policy in migration 011 |
| `domain` | Explicit policy in migration 011 |
| `run` | Explicit policy in migration 011 |
| `task` | Explicit policy in migration 011 |
| `event` | Explicit policy in migration 011 |
| `stream` | Explicit policy in migration 011 |
| `access` | Explicit policy in migration 011 |
| `agent` | Explicit policy in migration 011 |
| `workflow` | Explicit policy in migration 011 |
| `build` | Explicit policy in migration 011 |
| `call` | Explicit policy in migration 011 |
| `event_handler` | Explicit policy in migration 011 |
| `mcp_tool` | Explicit policy in migration 011 |
| `webhook` | Explicit policy in migration 011 |
| `schedule` | Explicit policy in migration 011 |
| `schedule_log` | Explicit policy in migration 011 |
| `team_user` | Explicit policy in migration 011 |
| `alert_delivery` | Explicit policy in migration 011 |
| `alert_subscription_channel` | Explicit policy in migration 011 |
| `alert_subscription_destination` | Explicit policy in migration 011 |
| `file` | Explicit policy in migration 011 |
| `file_upload` | Explicit policy in migration 011 |
| `blob_object` | Explicit policy in migration 011 |
| `blob_ref` | Explicit policy in migration 011 |
| `alert_subscription` | Explicit policy in migration 011 |
| `store_map` | Explicit policy in migration 011 |
| `store_map_entry` | Explicit policy in migration 011 |
| `context` | Explicit policy in migration 011 |
| `alert` | Explicit policy in migration 011 |
| `alert_destination` | Explicit policy in migration 011 |
| `alert_channel` | Explicit policy in migration 011 |
| `user` | Explicit policy in migration 011 |
| `user_auth` | Explicit policy in migration 011 |
| `email_verification` | Explicit policy in migration 011 |
| `email_queue` | Explicit policy in migration 011 |
| `scheduler_state` | Explicit policy in migration 011 |

Reference tables deliberately have no RLS; runtime receives SELECT only:
`build_type`, `email_verification_status`, `invite_status`, `org_user_role`,
`run_status`, `run_type`, `task_status`, `team_user_role`, `org_plan_status`,
`alert_delivery_status`, `alert_destination_type`, `plan`.

SQLx migration metadata is deployment control state and receives no runtime grants.
Management credential tables introduced by migration 012 carry their own policies.

| Added control table | Ownership and operation |
| --- | --- |
| `account_request_window` | Exact root; Billing only (011) |
| `management_key` | Exact root Organization; Authentication read; immutable root (012, tightened by 016) |
| `api_key_management_issuer` | API key environment (writes need an exact environment scope); Authentication read; immutable issuer; delegated parent deletion restricted (012, 016) |

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

Migration 016 validates delivery relationships before installing integrity
triggers: the alert, subscription, and destination must share an organization,
and an environment-specific subscription must match the alert environment.
Delivery ownership cannot be reassigned. SQLite has equivalent integrity
triggers, and the sender checks the subscription environment independently.

Migration 017 completes the retained-data preflight for the runtime integrity
checks introduced by 011: run/task origin runs and run events must exist in the
same environment. It reads under the named queue maintenance operation so a
non-BYPASS schema owner sees every retained relationship despite FORCE RLS.
An inconsistent link stops the upgrade for reviewed repair; no provenance is
rewritten automatically. SQLite applies the same retained-data preflight.

| Admission extension | Coverage |
| --- | --- |
| `task.reserved_compute_units`, `task.reserved_duration_ms` (013) | Existing exact task policies; root-locked admission narrows to env before claiming |
| `run_admission` (014) | Exact env or Billing root SELECT; Billing root INSERT validates org/env/root association; no runtime UPDATE/DELETE |

Run-admission backfill enumerates metadata under Membership and reads each
account under Billing, so a schema owner without BYPASSRLS retains existing
usage. The restricted-login integration test exercises durable sibling-shared
run limits and verifies that ordinary environment access cannot delete the
ledger to reset capacity.
