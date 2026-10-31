---
description: "Run a self-hosted Hot installation on PostgreSQL with a separate schema owner, a restricted runtime login, and row-level security."
---

# Self-Hosted PostgreSQL

Local projects use SQLite. A self-hosted Hot installation can use PostgreSQL
instead. On PostgreSQL, Hot uses row-level security (RLS) to keep each
organization's and environment's rows apart. Every query runs inside a
verified scope, and a query without a scope sees no protected rows.

This page covers the logins Hot needs, a new installation, upgrading a Hot 2.x
database, and backups. SQLite projects don't use any of it.

## Two Logins

Hot connects to PostgreSQL with two logins:

- **A schema owner.** It owns schema `hot` and applies migrations. Give its URI
  only to migration and administration jobs. Keep it out of long-running
  services.
- **A restricted runtime login.** The API, app, worker, task worker, scheduler,
  and CLI commands connect with it. It must not own the schema or its tables.
  Create it with `NOSUPERUSER`, `NOBYPASSRLS`, `NOCREATEDB`, and
  `NOCREATEROLE`, and don't make it a member of a privileged role.

| Setting | Environment variable | CLI flag | Purpose |
|---------|----------------------|----------|---------|
| `hot.db.uri` | `HOT_DB_URI` | `--db.uri` | Runtime login |
| `hot.db.migration-uri` | `HOT_DB_MIGRATION_URI` | `--db.migration-uri` | Schema owner, for migrations and index provisioning |
| `hot.db.schema` | | | Must be `hot`, the default |
| `hot.db.rls.enforce` | `HOT_DB_RLS_ENFORCE` | | Check the runtime login at startup. Default `true` |

`db.migration-uri` applies only when `db.uri` is a PostgreSQL URI. With
SQLite, Hot ignores it.

When a service starts, it checks the runtime login. It refuses to start if the
login, directly or through a role it belongs to:

- is a superuser, can bypass RLS, or can create roles or databases;
- owns the tables or can create objects in the schema;
- holds `TRUNCATE`, `REFERENCES`, or `TRIGGER` privileges;
- can write the reference tables or read the migration ledger.

It also refuses a schema other than `hot`, a database that hasn't been
migrated, and any table the login can reach that has no RLS policy.

### `db.rls.enforce`

Leave `db.rls.enforce` at `true` in production. Setting it to `false` skips
the startup checks on the runtime login. Hot still scopes every query, and the
database policies still apply to a restricted login. But if you turn it off and
connect as a superuser, the schema owner, or a `BYPASSRLS` login, PostgreSQL
skips the policies, and the database no longer keeps organizations apart. Use
`false` only for isolated development or test fixtures. Never use it to recover
from a production outage.

## New Installation

1. As a database administrator, create the two logins, a database owned by the
   schema owner, and schema `hot`. Install the `vector` and `pg_trgm`
   extensions into schema `hot`, since the schema owner usually can't install
   them:

   ```sql
   CREATE ROLE hot_owner LOGIN PASSWORD '<owner-password>'
     NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION;
   CREATE ROLE hot_runtime LOGIN PASSWORD '<runtime-password>'
     NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION;
   CREATE DATABASE hot OWNER hot_owner;

   -- Connected to the new hot database:
   CREATE SCHEMA hot AUTHORIZATION hot_owner;
   CREATE EXTENSION vector WITH SCHEMA hot;
   CREATE EXTENSION pg_trgm WITH SCHEMA hot;
   ```

   Manage both passwords with your usual secret store. Don't reuse a role that
   already has other grants or role memberships for the runtime login.

2. Apply the migrations as the schema owner:

   ```bash
   export HOT_DB_URI="postgres://hot_runtime:<runtime-password>@db.example.com/hot"
   export HOT_DB_MIGRATION_URI="postgres://hot_owner:<owner-password>@db.example.com/hot"
   hot db migrate
   ```

3. Grant the runtime login its privileges with `provision_runtime.sql`. The
   script is in the Hot source repository under
   [`resources/db/postgres/`](https://github.com/hot-dev/hot/tree/main/resources/db/postgres).
   Use the copy from the release tag that matches your Hot version. Run it with
   `psql` as the schema owner:

   ```bash
   psql "$HOT_DB_MIGRATION_URI" -v runtime_role=hot_runtime -v schema=hot \
     -f provision_runtime.sql
   ```

   The script runs in one transaction. It removes the runtime login's direct
   grants, then grants only what Hot needs. It aborts without changes if the
   login is still unsafe, for example through a `PUBLIC` or group grant. Fix
   the reported grant and run it again. It is safe to run more than once.

4. Create the first identity. Services and CLI commands act as a user in an
   organization, and a new database has none. `hot init` creates the local
   identity (user `local@hot.dev`, organization `local`, environment
   `development`); on PostgreSQL it needs the schema owner too:

   ```bash
   hot init --db.migration-uri "$HOT_DB_MIGRATION_URI"
   ```

   Without it, CLI commands fail with "User with email 'local@hot.dev' not
   found". A hosted setup that signs users in through the app can skip this
   step.

5. Start the services with `HOT_DB_URI` only. Don't give them
   `HOT_DB_MIGRATION_URI`.

Run `provision_runtime.sql` again after any later migration that adds tables,
or the runtime login can't use them. Since rerunning it is safe, the simplest
rule is to run it after every `hot db migrate`. Services refuse to start when
the runtime login can't use one of Hot's tables, or when the schema is behind
the release, and name the table or migration.

`hot app` applies migrations at startup only when it has `db.migration-uri`.
Without it, run `hot db migrate` from your deployment's migration job before
starting the new version.

## Upgrading from Hot 2.x {#upgrading-from-hot-2x}

Hot 3.0 adds migrations 012 to 016. They add organization hierarchy, install
the RLS policies, add usage accounting, let retention prune the run admission
ledger, and count each account's file storage. Plan the upgrade as a
maintenance window.

1. **Rehearse first.** Restore a recent backup into a separate database, and
   run the whole upgrade there with the same logins you plan to use.

2. **Repair duplicate environment names.** Migration 012 adds a unique index on
   each organization's environment names. Earlier releases could create
   duplicates. Find them with:

   ```sql
   SELECT org_id, name, count(*) FROM hot.env
   GROUP BY org_id, name HAVING count(*) > 1;
   ```

   Rename all but one environment in each group before you migrate.

3. **Set up the logins.** A 2.x installation often uses one login that owns
   the tables. Keep that login, or the current table owner, as the schema
   owner, and create a new runtime login as shown above.

4. **Stop every 2.x process.** Stop the API, app, and scheduler first, and let
   the workers and task workers finish the work already queued: 3.0 can't run
   work queued for a 2.x build. Then stop them, and any CLI sessions that use
   the database. There is no rolling upgrade: 2.x processes can't run against
   the migrated database.

5. **Take a backup.** See [Backups](#backups). Downgrading after the migration
   is not supported, so this backup is your way back.

6. **Migrate.** Install the 3.0 CLI and run `hot db migrate` with both URIs.
   Migration 013 rewrites every row of the `call` table in one transaction,
   under exclusive locks. Budget disk space and WAL of about the size of that
   table, and allow for replica lag. Turn off `statement_timeout` for the
   schema owner, for example with
   `ALTER ROLE hot_owner SET statement_timeout = 0`.

   Migration 013 first checks that existing rows belong to matching
   organizations and environments. If it finds a mismatch, it rolls back,
   reports the rows, and changes nothing. Repair those rows and run it again.
   Hot never reassigns ownership on its own.

7. **Provision the runtime login.** Run `provision_runtime.sql` as shown above.

8. **Start the 3.0 services** with the runtime login. Check that they start,
   that a run succeeds, and that one organization can't see another's data.

9. **Provision search indexes.** If you use semantic search on stores, run
   `hot db provision-store-indexes`. See
   [Store Search Indexes](#store-search-indexes).

10. **Redeploy your projects** with the 3.0 CLI, or all at once with
    `hot db redeploy` (start with `--dry-run`). See
    [Upgrading to Hot 3.0](/docs/migrations#upgrading-to-hot-30).

Multipart file uploads started before the upgrade can't complete afterward.
Start them again.

If a migration fails, stop the rollout. A failed migration rolls back, but
earlier migrations in the chain may already be committed. Check
`hot db status --db.migration-uri "$HOT_DB_MIGRATION_URI"` before you retry:
it lists applied, pending and mismatched migrations. See
[Troubleshooting Migrations](/docs/migrations#troubleshooting-migrations).

## Upgrading from Hot 3.0.0 {#upgrading-from-hot-300}

Hot 3.0.1 adds migrations 017 to 023. You can apply them while 3.0.0 keeps
serving, then roll the services:

1. **Take a backup.** See [Backups](#backups).
2. **Migrate** with the 3.0.1 CLI and both URIs, as for a new installation.
   3.0.0 processes keep working on the migrated schema: migration 020 keeps
   the tables they read for authentication, though no longer the `hm_`
   management credentials it retires. See
   [API Keys and Webhooks](/docs/migrations#api-keys-and-webhooks).
3. **Provision the runtime login** with the 3.0.1 `provision_runtime.sql`.
   The migrations add tables, and 3.0.1 services refuse to start until the
   runtime login can use them.
4. **Roll the services** to 3.0.1, workers and task workers first.

Each migration waits at most 5 seconds for a lock that running traffic
holds, then retries (see
[Troubleshooting Migrations](/docs/migrations#troubleshooting-migrations)).
While they run, migrations 019 and 021 block writes to the tables they
index: projects and stores, and files. With millions of files, create 021's
index ahead of time as the schema owner, without blocking writes. The
migration then keeps it:

```sql
CREATE INDEX CONCURRENTLY IF NOT EXISTS file_active_storage_path_idx
    ON hot.file(storage_backend, storage_path) WHERE active = true;
```

A `CREATE INDEX CONCURRENTLY` that fails leaves an invalid index. Check
`SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid`, and drop and
recreate any index it lists before you migrate.

Migration 022 adds a nullable `key_text` column to store entries and commits
at once, with no table rewrite. Migration 023 fills it in one environment at
a time, then builds the store prefix index. Store reads continue throughout
023; store writes wait while the index builds, so expect it to take longer on
large stores.

Rolling back to 3.0.0 after the migration works, except for `hm_` management
credentials, which stay revoked.

## Backups

Forced RLS applies to every login except superusers and `BYPASSRLS` roles. That
changes how logical backups work:

- `pg_dump` as any other login fails. It turns row security off and stops when
  a policy applies.
- `pg_dump --enable-row-security` finishes, but it dumps no rows from the
  protected tables.

Use storage snapshots or point-in-time recovery. If you need logical dumps, run
`pg_dump` with a dedicated backup role that has `BYPASSRLS`. Only a superuser
can create one, so managed services such as Amazon RDS can't. There, rely on
snapshots and point-in-time recovery.

On managed services, the master user can create roles and databases, so it
fails the runtime checks by design. Use it, or the table owner, only as the
schema owner.

## Store Search Indexes

`hot db provision-store-indexes` creates vector indexes for stores that use
semantic search. It needs `db.migration-uri`, because index creation runs as
the schema owner. It skips indexes that already exist and reports interrupted
ones for repair. Searches still work without these indexes; they scan instead.
Run it again after you create stores or change an embedding dimension.

## Connection Proxies

Hot sets each query's scope inside its transaction, so a proxy that pools
connections per transaction can keep working. Two cautions:

- The scheduler holds a session-level lock. Connect it directly, or through a
  pinned session.
- When Hot opens a connection, it sets `search_path`, `statement_timeout`,
  `lock_timeout`, and `jit` with session `SET` statements. A proxy that pools per
  transaction may run a later transaction on a server connection that never
  saw them. Set the same values on the runtime login with `ALTER ROLE ... SET`
  so every server connection starts with them. Some proxies, such as RDS
  Proxy, also pin a session after a `SET`; check how yours handles them.

## JIT Compilation

Hot turns PostgreSQL's JIT compilation off on its connections. Its queries
are short lookups, and under row-level security their cost estimates can
cross the JIT thresholds, so compiling them takes longer than running them.
Set `db.jit = true` to leave it on.

## Reference

The design notes behind this page are in the Hot source repository:
[`RLS_SETUP.md`](https://github.com/hot-dev/hot/blob/main/resources/db/postgres/RLS_SETUP.md)
covers operations in more depth, and
[`RLS_COVERAGE.md`](https://github.com/hot-dev/hot/blob/main/resources/db/postgres/RLS_COVERAGE.md)
lists every table and its policy.
