---
description: "Upgrade Hot and move local SQLite databases from 1.x to 2 with hot update and hot db port-v1-to-v2."
---

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

Hot 2 ships a clean baseline schema and does not migrate a Hot 1.x database in place. The public upgrade path covers local SQLite projects; Hot Cloud's v1→v2 data backfill lives in the private cloud repository.

### Before you upgrade

Back up your database before changing major versions. `hot db port-v1-to-v2` writes its own backup of the v1 SQLite file alongside the original, but a separate copy is still good practice.

Check the installed version before each phase:

```bash
hot version
```

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
