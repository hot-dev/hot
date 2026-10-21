# PostgreSQL scoped runtime setup

PostgreSQL runtime connections require a validated backend scope for every
query. Migration 013 forces RLS on scoped platform tables; later migrations must
extend that coverage. Missing scope fails closed. SQLite continues to use the
shared application authorization contract and explicit SQL filters.

Use a schema-owner login for migrations and a separate non-owner runtime login.
Create the runtime login using your secret-management process; it must be
NOSUPERUSER, NOBYPASSRLS and unable to assume the owner or another privileged
role. Do not reuse a role that already has global grants or role memberships.
Apply migrations using `db.migration-uri`, then apply `provision_runtime.sql`
with psql variables `runtime_role` and `schema`. Apply that grant script again
after adding newly protected tables. It grants no access to SQLx migration
metadata and never grants TRUNCATE, REFERENCES, or TRIGGER. Reapplying it removes
the runtime login's direct table and column grants, direct schema CREATE, and
direct sequence grants before installing the required privileges.

Configure runtime `db.uri` separately from `db.migration-uri`. Keep migration
credentials out of long-running API, app and worker processes. Supply the
migration URI only to the deployment migration command. Runtime startup checks
privileged memberships, ownership, TRUNCATE, reference-table write grants,
migration-metadata access and enabled/forced table coverage. They also reject
REFERENCES (including column grants), TRIGGER, and server-file/server-program
roles. Qualification includes privileges available through role memberships,
even when the runtime login uses NOINHERIT and would need SET ROLE to use them.
The public migration chain currently hardcodes schema `hot`; enforced runtime
startup rejects other schema configurations until the full migration chain has
a supported schema-remapping upgrade strategy.

`db.rls.enforce` defaults to true for PostgreSQL. Explicit false disables startup
role qualification only for isolated development or privileged test fixtures.
It does **not** remove transaction scoping or disable database policies. A run
using false is not evidence of production RLS isolation. Never use false to
recover a production outage.

Scope settings are transaction-local and are set by `ScopedPgPool` on the same
connection used for each query, through one `SELECT hot.apply_scope(...)` call.
That function verifies the scoped environment against the scoped organizations
once per transaction, so policies compare each row with the verified value
instead of querying `env` per row; settings written directly with `set_config`
verify nothing and grant no environment rows. A read set always makes its
transaction read-only. Because scope is applied by a function call with
transaction-local settings, connection proxies that multiplex between
transactions (such as RDS Proxy, which pins sessions on `SET` and direct
`set_config` statements) can keep multiplexing. The scheduler is the exception:
its singleton is a session advisory lock held on a dedicated connection, so
connect it directly or with a pinned session. It checks the lock every 30
seconds and exits if the lock is lost, so a replacement never runs alongside
it. Set the runtime login's
`search_path`, `statement_timeout` and `lock_timeout` as role defaults
(`ALTER ROLE ... SET`) or in the proxy's initialization query: the connection
setup otherwise issues session-level `SET` statements, which pin.
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

Provision optional semantic-search indexes with `hot db provision-store-indexes`
from an administrative deployment job that has `db.migration-uri`. This command
reads only store configuration under its named provisioning operation, then
creates per-org/env/store/dimension HNSW indexes concurrently as the schema
owner. It is idempotent for valid indexes and reports interrupted invalid indexes
for explicit repair. It currently supports pgvector vector indexes with 1–2000
dimensions. Searches remain functional without these indexes; they use exact
scans. Re-run after stores are created or embedding dimensions change. Runtime
store creation never attempts DDL.

Direct-database CLI commands resolve the configured profile's current human
membership and active org/environment, then bind an exact environment handle.
Inherited root-admin access may select a child profile. Without a profile the
CLI accepts only an unambiguous single org/environment; it never silently picks
a child or sibling. The canonical selector is `hot.set.profile` in a Hot
configuration file, or `--profile NAME` on the CLI. Conflicting legacy
`profile.set` selectors are rejected. `hot init` uses the resolved profile and
seeds `local@hot.dev` / `local` / `development` only in a completely empty
database. Other profiles must already have an active user, authorized org and
existing environment; init does not grant membership or create a missing
environment in an existing account. Init/fixture seeding uses the narrow
onboarding operation. Pass `--db.migration-uri URI` to the administrative
migration command independently of the runtime `--db.uri URI`.

Live documentation and source browsing additionally require the configured
local profile and current live build. Their cache keys include the build ID.
A missing uploaded build never falls back to a similarly named host project.

Migration 013 validates existing org/env, build/env, stream/env, credential/env,
context/project and blob ownership before installing policies. Repair reported
rows through a reviewed migration, then retry; it never reassigns ownership
automatically. New writes also enforce those links and immutable owner columns.

Authenticated profile and notification pages use `DatabaseScope::User` with an
exact user UUID. This permits reading/updating only that identity, without
organization or execution access. Organization directories can read identities
only through an active membership in an explicitly authorized organization.
All scope GUC fields, including the user UUID, are reset transaction-locally.

## First installation and existing-database upgrades

For a fresh installation, create the schema-owner and runtime roles separately,
run the full migration chain as the owner, then run `provision_runtime.sql`.
The schema-owner login can be NOSUPERUSER and NOBYPASSRLS. Install required
extensions (`vector` and `pg_trgm`) into schema `hot` using an administrative
deployment connection first if that owner lacks extension-installation rights.
Only then start runtime services with the restricted login and enforcement
enabled. The public CLI's bootstrap provider is a no-op. A private provider
runs right after migrations under the onboarding operation, and its
configuration's `db.uri` is the migration URI with runtime-role qualification
off, so it can create its own tables and seed reference rows such as `plan`.
With enforcement on, `hot app` migrates and bootstraps at startup only when it
is configured with `db.migration-uri`; otherwise run `hot db migrate` (or set
`HOT_DB_MIGRATION_URI`) from the deployment's migration job.

Runtime qualification only rejects unprotected tables the runtime login can
reach. A provider's own tables in schema `hot` may stay unprotected if the
runtime login never needs them; tables it does need must enable and force RLS
with their own policies, after which `provision_runtime.sql` grants them like
platform tables. Billing-provider webhooks run under the `billing_webhook`
bootstrap operation, which can only read org plans: a provider verifies the
callback, resolves its account from its own records (policies on those tables
can allow `hot.scope_bootstrap('billing_webhook')`) and the org plan, then uses
`Org::billing_scope` for every other platform billing read and write.

For an existing database:

1. Restore a recent backup into a separate database and rehearse migrations
   012–020 with the intended owner/runtime roles, including a non-superuser,
   non-BYPASSRLS schema owner. Check duplicate environment
   names within each org before migration 012 adds its unique index. Inspect
   migration 013's ownership preflight and resolve any reported mismatch in a
   reviewed repair migration. Preserve original ownership evidence.
   Migration 018 similarly refuses alert deliveries whose subscription or
   destination belongs to another org, or whose subscription targets a different
   environment. Verify a refused migration leaves its earlier statements and
   SQLx ledger entry rolled back before applying an approved repair and retrying.
   Migration 019 validates retained run/task origin references and run/event
   links against their environments. Migration 013 already checks new writes,
   but its original preflight did not cover those historical links. A refusal
   preserves the offending records and earlier committed migrations, including
   018; investigate and repair the relationship explicitly before retrying.
2. Record row counts and account usage, including retained run history, before
   the change. Migration 016 backfills the admission ledger from retained runs;
   it cannot recover runs already deleted by retention. Reconcile any usage
   missing from that history against an authoritative account usage source
   before enforcing an existing paid account's monthly run cap.
   Migration 017 reconciles retained trace sizes, including UTF-8 and error
   payloads, backfills immutable call environments, and initializes storage
   counters before trace-storage admission uses them. Migration 015 initializes
   active task counters; period usage is initialized on the first limited claim.
   Migration 016 leaves run counters lazy, backed by the admission ledger.
3. Drain work and stop every old process using the database, including workers,
   scheduler, direct CLI sessions and private provider jobs. Take a recoverable
   backup. Apply the additive migrations and rerun the grant script as the
   owner. Do not mix older unscoped services with the new database policies.
   Test the grant script twice to verify idempotence. Table-level revocation
   alone does not remove older column ACLs; the script explicitly removes those
   direct grants, including grants on reference tables and `_sqlx_migrations`.
4. Start the scope-capable services with their restricted login. Verify startup
   qualification, a permitted read/write, cross-child denial, queue execution,
   an invite acceptance, and a retained-run ledger count. A successful health
   check alone does not establish that useful work is authorized correctly.
5. Resume traffic only after those checks pass. Monitor denied operations,
   queue retries, writer failures and account quota rejections. Retain the
   backup and migration logs for the agreed rollback window.

`provision_runtime.sql` is transactional and checks the resulting effective
privileges before committing. Unsafe PUBLIC grants, privileges available through
group roles, privileged attributes, and ownership cause it to abort. It does not
silently revoke shared grants or role memberships, since other applications may
depend on them. Inspect the reported grant source, remove or narrow the relevant
PUBLIC/group grant or runtime membership through an approved administrative
change, then rerun the script. If the script aborts, its direct-grant remediation
rolls back too. Use a new dedicated runtime login when an existing role cannot
be safely separated. Follow with startup qualification and real scoped
read/write and cross-org denial checks; script success alone is not a complete
application smoke test.

Managed PostgreSQL services such as Amazon RDS have no superuser, and only a
superuser or an existing BYPASSRLS role can create a BYPASSRLS role. Forced RLS
therefore applies to every login, including the master user and the table
owner. Logical dumps with `pg_dump` fail (it disables row security and stops
when a policy applies), and ad hoc queries see no protected rows until the
session establishes a scope with `SELECT hot.apply_scope(...)` inside a
transaction. Rely on snapshots and point-in-time recovery for backups. The
master user (CREATEDB, CREATEROLE, `rds_superuser`) fails runtime
qualification by design: use it, or the current table owner, only as the
migration login, and create a separate runtime login without those
memberships. Migration 017 rewrites every retained call row in one
transaction, under RLS for a non-bypass owner; size the window, WAL, storage
headroom and replica lag for the `call` table, and keep `statement_timeout`
off for the migration login.

Restart S3 multipart uploads begun by the older application: new upload tokens
bind the staged object version, so an old in-flight token cannot complete an
upload after this upgrade. Already committed files keep their existing paths.

Cancelled containers retain compute and concurrency reservations until their
runtime confirms termination. See [cancelled quota recovery](CANCELLATION_QUOTA_RECOVERY.md)
for worker fencing, conservative charges, and recovery of unreachable runtimes.

Account request-per-second admission is shared through PostgreSQL/SQLite.
In-flight request and SSE connection guards remain per-process resource limits;
they are additional safeguards, not distributed account concurrency counters.

Store and trace storage admission serialize sibling writes on the account root
and enforce recorded logical bytes. Accounts without a storage limit skip both
the root lock and the admission usage read. Transactional counters remain active
for every account so adding a limit cannot start from incomplete usage. Trace-cap failure rolls back the trace
batch and is reported by emitter flush/shutdown; run terminal state is committed
separately. Diagnose the storage failure before replaying work, since workflow
side effects may already have completed. These logical limits do not cap total
database disk allocation or separately retained blob objects.

Migration failure must stop rollout. SQLx rolls back the failed transactional
migration, but earlier migrations in the chain may already be committed; inspect
its migration state before retrying. After successful migration, an application
rollback must keep the policies, immutable ownership and admission ledger.
Use a previously verified scope-capable release or fix the denied operation
forward. Restoring a database backup is a coordinated recovery operation that
also needs queue/object-store reconciliation; it is not an automatic response
to an RLS error.

## Deployment verification outside this repository

The public repository's local PostgreSQL and SQLite tests do not qualify private
cloud bootstrap implementations, deployed database role inheritance, managed
connection proxies, provider billing/usage reconciliation, remote object stores,
or hosted queue variants. Those integrations remain deployment release gates.
Run the same positive/negative scope and cancellation checks against each
deployed connection path, and verify that private jobs use named operations and
never inherit migration credentials. Shared deployments across child orgs remain
deferred; each child currently owns its environments and deployments.

## Usage counters and development migration revisions

[Maintained usage counters](USAGE_COUNTERS.md) replace history scans in quota
admission. Include their initialization and the first billing-period task/run
claim in the rehearsal window. Monitor hourly reconciliation warnings: they
identify source/counter drift, applied deltas, and a retry after an error.
Reconciliation retains ordinary writes and does not hold the admission root
lock. It checks one counter per short transaction, but still reads account
history, so budget for that hourly background load.

Migrations 012–020 on this development branch are unshipped and were revised in
place, including 015–017 for counters and call ownership. A scratch database
that already applied an earlier revision will fail SQLx checksum validation.
Recreate only disposable development/test databases with known ownership and
retain any needed data first. Never edit the SQLx ledger to conceal a checksum
mismatch, and never use this development reset procedure on a deployed database.
A deployed revision requires a new forward migration instead.
