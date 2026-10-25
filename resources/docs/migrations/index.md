---
description: "Upgrade Hot between releases, including the Hot 3.0 database, language, and API changes, and move local SQLite databases from 1.x to 2."
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
hot update --version 2.3.0
```

For older `hot` binaries that do not support `hot update --version`, use the hosted installer script instead.

macOS / Linux:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version 2.3.0
```

Windows PowerShell:

```powershell
$env:HOT_VERSION = "2.3.0"; irm https://get.hot.dev/install.ps1 | iex
```

Pinned installs are useful when you need to finish database migrations with an older release line before moving to a newer major version.

## Upgrading to Hot 3.0 {#upgrading-to-hot-30}

Hot 3.0 changes the database schema, the language, and API permissions. Read
this section before you upgrade.

### Before You Upgrade

- **Back up your database.** For a local project, stop `hot dev` and any
  other Hot process, then copy the `.hot/db/` directory. For PostgreSQL, take
  a snapshot. See [Backups](/docs/configuration/postgres#backups).
- **Hot 3.0 applies migrations 012, 013, and 014, and 015 on PostgreSQL.**
  Hot 2.6.11 added 010 and 011. Downgrading a migrated database is not
  supported. To go back, restore your backup.
- **Ignore the "Hot 1.x database" hint.** A 2.x release that opens a migrated
  database says it is a Hot 1.x database and suggests `hot db port-v1-to-v2`
  or deleting the SQLite file. That hint is wrong. Never delete the database.
  Keep using 3.0, or restore your backup.
- **Check your code on 3.0.** Run `hot check` and `hot test` with the 3.0 CLI
  before you deploy.

### Local Databases

In a project, the first `hot run`, `hot eval`, `hot repl`, `hot compile`,
`hot build`, `hot test`, or `hot deploy` migrates an outdated local SQLite
database and prints a one-line notice. You can also run `hot db migrate`
yourself. `hot db status` lists pending migrations. Hot never migrates a
PostgreSQL database implicitly.

`hot init` and first use create only the local identity: user
`local@hot.dev`, organization `local`, and environment `development`. If your
`hot.hot` selects a different local profile, that profile must already exist
in the database. A new or empty database no longer creates it. See
[Profiles](/docs/configuration#profiles).

### Deploying

<!-- TODO(curt): confirm 2.x bundle policy -->
Builds are tied to a major version of Hot. Upgrade the server first, then
redeploy every project with the 3.0 CLI. A 3.0 server doesn't run builds made
with a 2.x CLI, and a 2.x CLI can't deploy to a 3.0 server. In CI, pin the
setup-hot `version` input so pipelines move to 3.0 when you do. See
[Pin a Specific Version](/docs/ci-cd#pin-a-specific-version).

### Language Changes

- **Closed literal unions are checked at runtime.** A value outside a closed
  union now fails wherever it enters the type: a parameter, a typed field, a
  constructor, a return value, or an update. The result is
  `Err(ConstraintError.Violation)`. In 2.x these values passed. Mark a union
  `open` when its values come from outside your code, such as an API response:

  ```hot
  StopReason type open "end_turn" | "max_tokens" | "tool_use"
  ```

  Literal unions also export to tool and MCP JSON Schema as typed `enum`s.
- **A space before `(` never calls a function.** `a (...)` always binds `a` to
  the value in parentheses, at top level and in function bodies, blocks,
  branches, and flows. Through 2.6.10, function bodies parsed some of these as
  calls. Look for `not (x)`, `assert (...)`, and `if (x) { ... }`. `hot check`
  now warns with `binding-shadows-function` when such a binding shadows a
  function, and `hot test` warns about a test that makes no assertions.
- **New: parameter lists can span lines**, in functions and in lambdas. A
  lambda's `{` must be on the same line as its closing `)`. See
  [Long Parameter Lists](/docs/language/functions#long-parameter-lists).
- **`where` is reserved.** It starts a constraint, so it can't name a binding
  or a function.
- **New: `where` constraints.** Types, fields, parameters, return values, and
  bindings can take a predicate their values must pass. A failed check returns
  a `ConstraintError`, a new core type. See
  [Constraints](/docs/language/constraints).
- **Nested implicit `%` lambdas are errors** (since 2.6.11). In
  `map(groups, filter(%, gt(%, 0)))`, the inner `%` reports
  `nested-placeholder`. Write the inner function explicitly, as in
  `map(groups, filter(%, (n) { gt(n, 0) }))`.

### Packages

These packages model API responses with closed unions. Their older versions
reject real API values on Hot 3.0, so upgrade them:

| Package | Version | Requires |
|---------|---------|----------|
| `hot.dev/anthropic` | `1.2.5` | Hot 3.0 |
| `hot.dev/openai` | `1.2.8` | Hot 3.0 |
| `hot.dev/gemini` | `1.1.3` | Hot 3.0 |
| `hot.dev/xai` | `1.1.3` | Hot 3.0 |
| `hot.dev/qwen` | `1.0.3` | Hot 3.0 |
| `hot.dev/aws-bedrock` | `1.2.2` | |
| `hot.dev/postmark` | `1.1.1` | |
| `hot.dev/supabase` | `1.0.1` | |

### API Keys and Webhooks

- **Restricted API keys follow route permissions.** In 2.x, a restricted key
  could reach administrative routes outside its grants. Now every private
  route checks them. Full-access keys are unaffected. Review restricted keys
  before you upgrade:
  - Deploying needs `build:*` with `create`, `read`, and `execute`. A deploy
    that creates its project also needs `project:*` with `create`.
  - Activating a project needs `build:*` with `execute`.
  - Creating sessions or service keys needs an explicit `session` or `key`
    grant.

  See the [permission table](/docs/api/management).
- **Webhook `auth` fails closed.** Any explicit `auth` value other than
  `"none"` now requires authentication. In 2.x, only exactly `"required"` did,
  and any other value, such as `"Required"` or `true`, left the webhook public.
  A webhook with `auth: "public"` now returns 401. Use `"none"` for a public
  webhook.
- **Webhook responses can't set cookies on the shared API host.** Hot drops
  `Set-Cookie` from webhook responses there. Webhooks on a
  [custom domain](/docs/domains) are unchanged.
- **Usage is per environment.** `GET /v1/org/usage` reports the credential's
  environment and adds `env_id`, `usage_scope`, and `limits_scope`.
  `team_members` is `0`. A request whose `org_id` or `env_id` doesn't match
  the credential returns 403.
- **New management API.** Root administrators can create child organizations
  and manage them with `hm_` credentials. See
  [Sub-organizations](/docs/api/management).

### Account Limits

Hosted plans now enforce `runs_per_month` and the other account limits. When
an account reaches `runs_per_month`, queued event, webhook, and MCP work stays
pending and retries with backoff. See
[Monthly run admission](/docs/api/management#monthly-run-admission).

A self-hosted organization without a plan has no limits. If it has feature
overrides but no plan, any limit the overrides leave out stays unlimited.

### Self-Hosted PostgreSQL

Hot 3.0 uses row-level security on PostgreSQL. It needs a separate schema
owner for migrations (`db.migration-uri`) and a restricted runtime login
(`db.uri`). `hot db migrate` with only `db.uri` now fails with "PostgreSQL
requires a separate db.migration-uri". Migration 013 rewrites the `call`
table, and there is no rolling upgrade. Follow
[Upgrading from Hot 2.x](/docs/configuration/postgres#upgrading-from-hot-2x)
before you upgrade.

## Upgrading to Hot 2.6 {#upgrading-to-hot-26}

Hot 2.6 completes the move to one error idiom and removes the legacy
flow-result-modifier syntax:

- **`::hot::lang/try` and `::hot::lang/try-call` are removed.** Expected
  failures are `Result.Err` values — branch with `is-err` / `if-err`; use
  `OnErr.Preserve` for fan-out isolation and a task boundary
  (`::hot::task/start` + `await`) to supervise untrusted work. See
  [Error Handling](/docs/language/errors).
- **The `|map`, `|vec`, and `|one` flow result modifiers are removed** —
  annotate the binding or return type instead. `All<Map>` / `All<Vec>`
  collect all results (`x: All<Map> cond { ... }`); any other type on a
  collect-all flow takes the single final value (`x: Int parallel { ... }`).
  See [Flows](/docs/language/flows).

## Upgrading to Hot 2.3

Hot 2.3 includes a breaking cleanup to the public `Failure` and `Cancellation`
payload fields. Replace direct field access as follows:

- `failure.$msg` → `failure.msg`
- `failure.$err` → `failure.err`
- `cancellation.$msg` → `cancellation.msg`
- `cancellation.$data` → `cancellation.data`

## Upgrading to Hot 2.2

No code changes were required for 2.2 itself. The idioms it introduced are
now the standard forms — and, as of Hot 2.6, the only forms: the legacy
syntax that 2.2 still tolerated (`|map`/`|vec`/`|one` modifiers, `try-call`)
is removed, see [Upgrading to Hot 2.6](#upgrading-to-hot-26). The version
bump invalidates bytecode and AST cache entries from older versions; they
rebuild automatically on first run.

2.2 added:

- `All<Vec>` / `All<Map>` annotations for flow result shape — see [Flows](/docs/language/flows).
- `OnErr.Force` / `OnErr.Preserve` disposition for map-shaped higher-order functions — see [Error Handling](/docs/language/errors).

## Upgrading from Hot 1.x to Hot 2 {#upgrading-from-hot-1x-to-hot-2}

Hot 2 ships a clean baseline schema and does not migrate a Hot 1.x database in place. The upgrade path below covers local SQLite projects.

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
