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

Migration 011 validates existing org/env, build/env, stream/env, credential/env,
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
enabled. The public CLI's bootstrap provider is a no-op; a private provider
that seeds data must run after grants exist and use the explicit onboarding or
billing operation appropriate to its rows. Rehearse that provider's ordering
separately instead of supplying owner credentials to a runtime pool.

For an existing database:

1. Restore a recent backup into a separate database and rehearse migrations
   010–017 with the intended owner/runtime roles, including a non-superuser,
   non-BYPASSRLS schema owner. Check duplicate environment
   names within each org before migration 010 adds its unique index. Inspect
   migration 011's ownership preflight and resolve any reported mismatch in a
   reviewed repair migration. Preserve original ownership evidence.
   Migration 016 similarly refuses alert deliveries whose subscription or
   destination belongs to another org, or whose subscription targets a different
   environment. Verify a refused migration leaves its earlier statements and
   SQLx ledger entry rolled back before applying an approved repair and retrying.
   Migration 017 validates retained run/task origin references and run/event
   links against their environments. Migration 011 already checks new writes,
   but its original preflight did not cover those historical links. A refusal
   preserves the offending records and earlier committed migrations, including
   016; investigate and repair the relationship explicitly before retrying.
2. Record row counts and account usage, including retained run history, before
   the change. Migration 014 backfills the admission ledger from retained runs;
   it cannot recover runs already deleted by retention. Reconcile any usage
   missing from that history against an authoritative account usage source
   before enforcing an existing paid account's monthly run cap.
   Migration 015 reconciles retained trace sizes, including UTF-8 and error
   payloads, before account trace-storage admission uses those totals.
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
and enforce recorded logical bytes. Trace-cap failure rolls back the trace
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
