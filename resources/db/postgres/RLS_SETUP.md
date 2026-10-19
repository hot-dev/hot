# PostgreSQL scoped runtime setup

PostgreSQL runtime connections require a validated backend scope for every
query. Migration 011 forces RLS on scoped platform tables; later migrations must
extend that coverage. Missing scope fails closed. SQLite continues to use the
shared application authorization contract and explicit SQL filters.

Use a schema-owner login for migrations and a separate non-owner runtime login.
Create the runtime login using your secret-management process; it must be
NOSUPERUSER, NOBYPASSRLS and unable to assume the owner or another privileged
role. Do not reuse a role that already has global grants or role memberships.
Apply migrations using `db.migration-uri`, then apply `provision_runtime.sql`
with psql variables `runtime_role` and `schema`. Apply that grant script again
after adding newly protected tables. It grants no access to SQLx migration
metadata and never grants TRUNCATE.

Configure runtime `db.uri` separately from `db.migration-uri`. Keep migration
credentials out of long-running API, app and worker processes. Supply the
migration URI only to the deployment migration command. Runtime startup checks
privileged memberships, ownership, TRUNCATE and enabled/forced table coverage.
The public migration chain currently hardcodes schema `hot`; enforced runtime
startup rejects other schema configurations until the full migration chain has
a supported schema-remapping upgrade strategy.

`db.rls.enforce` defaults to true for PostgreSQL. Explicit false disables startup
role qualification only for isolated development or privileged test fixtures.
It does **not** remove transaction scoping or disable database policies. A run
using false is not evidence of production RLS isolation. Never use false to
recover a production outage.

Scope settings are transaction-local and are set by `ScopedPgPool` on the same
connection used for each query. `ReadSet` starts a read-only transaction.
Task-local scope is not inherited by Tokio spawned tasks: bind a cloned database
handle with `DatabasePool::with_scope` or explicitly establish scope inside the
new task. Pool-bound authority takes precedence over task-local authority.

Bootstrap and maintenance modes are trusted internal operations, each with
explicit policies. They are not exposed to customer code or request-selected
mode strings. These policies defend against missing row filters; backend
credentials and arbitrary SQL execution remain outside that threat boundary.

Run migration SQL and role tests in a dedicated test database. Never enable a
table's policies while an older service using that shared schema remains
unconverted. Enforcement is database/schema-wide; use a separate database for
a canary. Roll back only to a scope-capable application version, keeping RLS and
ownership data intact. Diagnose a denied operation instead of bypassing it.
