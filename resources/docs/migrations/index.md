# Migrations and Upgrades

This guide covers upgrading an existing Hot project and database between Hot releases.

## Updating Hot

Update to the latest published Hot release:

```bash
hot update
```

If your installed `hot` supports pinned updates, install a specific Hot version:

```bash
hot update --version 1.4.0
```

For older `hot` binaries that do not support `hot update --version`, use the hosted installer script instead.

macOS / Linux:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version 1.4.0
```

Windows PowerShell:

```powershell
$env:HOT_VERSION = "1.4.0"; irm https://get.hot.dev/install.ps1 | iex
```

Pinned installs are useful when you need to finish database migrations with an older release line before moving to a newer major version.

## Upgrading from Hot 1.x to Hot 2

Hot 2 ships a clean baseline schema and does not migrate a Hot 1.x database in place. The upgrade path depends on which database backend you use.

### SQLite (local development)

If you do not need to preserve your local data, the simplest path is to delete the SQLite file and let Hot 2 create a fresh one:

```bash
rm .hot/db/hot.sqlite.db
hot db migrate
```

To preserve your data, run the SQLite porter:

```bash
hot update
hot db port-v1-to-v2
```

`hot db port-v1-to-v2` writes a backup of your v1 file alongside it (named `hot.sqlite.db.v1.bak.<utc-timestamp>`), applies the Hot 2 baseline migrations to a fresh file at the original path, and copies your user-data rows from the backup using SQLite's `ATTACH DATABASE`. The resulting v2 file's schema is byte-identical to a fresh `hot init`. Tables Hot 2 pre-populates with seed rows (statuses, roles, alert channels, scheduler state) are not copied; v1-only tables (`subscription_plan`, `subscription`, `store`) have no Hot 2 destination and are reported as dropped. The v1 backup file is preserved; remove it manually when you no longer need it.

### Postgres (self-hosted or Hot Cloud)

There is no automatic port for Postgres in Hot 2. Point Hot 2 at a fresh Postgres database (or schema):

```bash
HOT_DB_URI=postgres://user:pass@host/hot_v2 hot db migrate
```

For Hot Cloud production environments, the v1→v2 data backfill is owned by the private cloud repository, not by the public `hot` binary.

### Before you upgrade

Back up your database before changing major versions. For SQLite, `hot db port-v1-to-v2` writes its own backup alongside the original, but a separate copy is still good practice. For Postgres:

```bash
pg_dump "$HOT_DB_URI" > hot-v1-backup.sql
```

Check the installed version before each phase:

```bash
hot version
```
