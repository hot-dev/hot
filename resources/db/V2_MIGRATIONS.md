# Hot 2.0 Database Migration Policy

The public `hot` repository owns schema migrations and generic reference data
needed by local development and self-hosting.

Public migrations must include:

- Tables, columns, indexes, and constraints required by public crates.
- Generic reference rows such as statuses, roles, run types, and scheduler state.
- Schema required by provider implementations, even when the default public
  provider is local or no-op.

Public migrations must not include:

- Hot Cloud product seed data such as plan names, prices, provider IDs, quota
  packages, or cloud-only rollout rows.
- One-off production data corrections.
- Customer, environment, or deployment-specific data.

Hot Cloud seed data and private provider state belong in the private cloud
repository. The cloud binary is responsible for running public schema
migrations first, then applying cloud-owned migrations and private bootstrap
data.

For Hot 2.0, the intended public migration shape is a clean baseline:

- `001_hot_2_initial_schema.sql` per supported database.
- Optional follow-up migrations only for changes made after the baseline lands.
- A documented v1-to-v2 upgrade path for existing development databases that
  need to be preserved.

Hot 2 does not adopt a Hot 1.x schema in place. Carrying a v1 chain forward
would leave the database with v1-only tables, divergent column shapes, indexes,
and constraint names that the v2 baseline did not produce, and that drift would
silently affect future v2.x migrations. Hot 2 instead provides a single
explicit path that produces a database whose schema is byte-identical to one
produced by a fresh `hot init`:

- For SQLite, `hot db port-v1-to-v2` backs up the existing v1 file at
  `<original>.v1.bak.<utc-timestamp>`, applies the Hot 2 baseline migrations to
  a fresh file at the original path, and copies user-data rows from the backup
  into the new file using `ATTACH DATABASE`. Lookup and bootstrap tables that
  the v2 baseline pre-populates (`run_status`, `task_status`, `org_user_role`,
  `alert_channel`, `scheduler_state`, …) are not copied. v1-only tables
  (`subscription_plan`, `subscription`, `store`) have no Hot 2 destination and
  are reported as dropped; their rows survive only in the backup file.
- For Postgres there is no port command. Point Hot 2 at a fresh Postgres
  database (or schema). The Hot Cloud v1→v2 production backfill is owned by the
  private cloud repository, not by public `hot`.

When `hot db migrate` is pointed at a Hot 1.x database, it returns an error
that names the recovery path (`hot db port-v1-to-v2` for SQLite; fresh database
for Postgres).

Hot Cloud production continuity is owned by the private cloud repository. It
should use its own cloud migration ledger and backfill path to transform old
cloud billing tables such as `subscription_plan` and `subscription` into the
public `plan` / `org_plan` tables plus cloud-owned provider tables. Public
`hot` migrations must not carry that Stripe/customer/payment lifecycle state.
