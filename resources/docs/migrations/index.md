---
description: "Upgrade Hot between releases, including the Hot 3.0.0 to 3.0.1 upgrade, the Hot 3.0 database, language, and API changes, and move local SQLite databases from 1.x to 2."
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

## Upgrading from Hot 3.0.0 to 3.0.1 {#upgrading-from-3-0-0-to-3-0-1}

Hot 3.0.1 fixes the issues found in a full review of everything merged after
3.0.0. Most fixes change nothing you need to act on. A few retire 3.0.0
features or reject code that 3.0.0 accepted, and those are listed first. Read
**Before you upgrade** and **Code that 3.0.1 rejects** before you roll it out.

### Before you upgrade {#3-0-1-before-you-upgrade}

**Management credentials are retired.**
- Migration 020 revokes every `hm_` management credential and deactivates the
  environment keys one issued. Other keys are untouched.
- `/v1/management/*` returns `410 Gone`, and an `hm_` token gets `401
  management_credential_retired`.
- Use org API keys and the [`/v1/orgs` routes](/docs/api/management) instead.
  Version 1.3.0 of every SDK uses them.
- Before upgrading, list the keys the migration will deactivate. On
  PostgreSQL, run this as a superuser or `BYPASSRLS` role; on SQLite, drop the
  `hot.` prefixes:

```sql
SELECT k.api_key_id, k.description, e.name AS env, m.name AS issued_by
FROM hot.api_key_management_issuer i
JOIN hot.api_key k ON k.api_key_id = i.api_key_id
JOIN hot.env e ON e.env_id = k.env_id
JOIN hot.management_key m ON m.management_key_id = i.management_key_id
WHERE k.active;
```

Afterwards, mint replacements with `POST /v1/orgs/{org}/envs/{env}/api-keys`.
You can also reactivate a key in the App, and it then works as an ordinary
environment key.

**`hot init` no longer sets a known password.**
- Before 3.0.1, `hot init` gave the local identity, `local@hot.dev`, the
  password `hotdev` on any database. A PostgreSQL installation set up as the
  [PostgreSQL guide](/docs/configuration/postgres) describes therefore had an
  admin sign-in for the `local` organization that anyone could use.
- 3.0.1 gives the local identity no password, and the App refuses `hotdev`.
  In local development the App still signs the local identity in
  automatically.
- Remove the old password as well. On PostgreSQL, run this as a superuser or
  `BYPASSRLS` role:

```sql
DELETE FROM hot.user_auth
WHERE auth_type = 'email_password' AND auth_identifier = 'local@hot.dev';
```

**Set `HOT_ENV` on every deployment.**
- An unset `HOT_ENV` means local development. 3.0.0 then signed every visitor
  in to the App as the first user and signed sessions with a built-in secret,
  wherever the App listened.
- 3.0.1 keeps that only when the App listens on a loopback address or
  `HOT_ENV=development` is set. With `HOT_ENV` unset on a network address,
  the App signs nobody in by itself and refuses to start without
  `HOT_APP_SESSION_SECRET`.
- Set `HOT_ENV=production` and `HOT_APP_SESSION_SECRET`. The self-host bundle
  already does.

**PostgreSQL: a rolling upgrade.**
1. Run `hot db migrate` with the 3.0.1 CLI while 3.0.0 is still serving.
2. Re-run the 3.0.1 `provision_runtime.sql`.
3. Roll the services, workers first.

3.0.1 services refuse to start when the runtime login lacks grants on Hot's
tables, or when the schema is behind the release. You can roll back to 3.0.0
after the migration, but `hm_` credentials stay revoked.

**Migrations 017–023 run on upgrade.**
- **Lock waits:** migrations wait at most 5 s for a lock
  (`db.migration-lock-timeout-ms`) and retry up to 5 times.
- **019 checks your data:** it refuses to run over duplicate project names in
  an environment, or stores whose organization or environment is gone, and it
  names the rows. Repair queries are under [Migration 019 refuses to
  run](#migration-019-refuses-to-run).
- **Write blocking:** 019 and 021 block writes to the tables they index while
  they run. With millions of files, pre-create 021's index `CONCURRENTLY`
  first; the statement is in the [PostgreSQL
  guide](/docs/configuration/postgres).
- **022 and 023 (stores):** 022 adds a nullable `key_text` column to store
  entries and commits immediately, with no table rewrite. 023 fills it in one
  environment at a time, then builds the store prefix index. Store reads
  continue throughout 023. Store writes wait while the index builds. Expect
  023 to take longer on large stores.
- **Status:** `hot db status` lists applied, pending and mismatched
  migrations. On PostgreSQL, pass `--db.migration-uri`.
- **SQLite:** a local database is copied to `<db>.pre-migrate.bak` before
  migrations change it.

**Packages, SDKs and the self-host bundle.**
- **hot-std:** hot-std 3.0.1 requires Hot 3.0.1.
- **hot-ai-agent:** 1.5.0 requires Hot 3.0.1; stay on 1.4.1 with Hot 3.0.0.
- **Image helpers:** gemini 1.1.4, openai 1.2.9 and xai 1.1.4 fix their image
  helpers.
- **SDKs:** version 1.3.0 of the Go, Java, JavaScript, Python and Rust SDKs
  replaces `HotManagementClient` with the org API, and needs Hot 3.0.1.
- **JavaScript proxy:** the JavaScript SDK's `/proxy` helper now requires
  `allowEventTypes`, and rejects `hot:` event types unless they are listed
  exactly. This is a breaking change for proxy users.
- **Self-host bundle:** re-render it. The Valkey healthcheck now passes, nginx
  runs with a read-only filesystem, `HOT_DAEMON` no longer respawns commands,
  and `HOT_APP_URL`/`HOT_API_URL` name your hosts.

### Code that 3.0.1 rejects {#3-0-1-code-that-is-rejected}

These are compile errors in `hot check`, `hot build`, `hot deploy`, `hot run`
and when a worker compiles a deployed bundle. A deployed 3.0.0 bundle that
contains one fails until you fix it and redeploy. Run `hot check` on your
projects with the 3.0.1 CLI before rolling out workers.

| 3.0.1 rejects | Fix |
|---|---|
| A `match` on a closed literal union that misses members | Add the missing arms or a `_ =>` arm |
| A bare name in meta that names nothing (`tags: [billing]`) | Quote it (`"billing"`) or define it |
| An unknown variant of a closed enum in meta (`Priority.Nope`) | Use a declared variant |
| A `timeout` meta that isn't a positive integer (`"30s"`) | Write whole seconds (`30`) |
| A record literal that violates a `where` clause, including in new positions | Fix the value, or compute it at run time |
| A non-exhaustive `match` on a binding of a call whose return type is an enum | Add the missing arms or `_ =>` |
| A meta `schedule` that names one run (`"2026-11-01"`, `"in 10 minutes"`, `"2h"`, `"tomorrow at 9am"`) | Use `every …`, or send a `hot:schedule:new` event to run once |
| A type alias defined in terms of itself (`A type A \| "x"`, `A type B` with `B type A`) | Recurse through a record field or a `Vec`/`Map` element |
| An arrow that adds a variant to a closed enum | Declare the variant, or declare the enum `enum open` |
| A `pkg.hot` that isn't literal data (bindings, templates, calls, spreads) | Write each value out |
| A package `src-paths`, `test-paths` or `resource-paths` outside the package (`../shared`, absolute paths) | Move the files into the package |
| A bare dependency name next to a coordinate with the same package name (`utils` next to `acme.com/utils`) | Use full coordinates |
| A non-hot.dev dependency with an empty spec (`"acme.com/x": {}`) | Give it a `local`, `git` or `pkg` source |
| Expressions nested more than 256 levels deep | Split them into functions |
| `::ns/var[i]` and `::ns/var.a.b` | Bind the reference first: `v ::ns/var`, then `v[i]` |
| A wrong-arity call or other call error inside `${...}` or in the first arm of a `cond` | Fix the call (these failed at run time anyway) |

`hot check` also reports new **warnings**:
- unresolved names inside literals, templates, arms and spreads;
- `unresolved-type` for an undeclared type in a record field, variant payload
  or return annotation;
- `unknown-variant` for a misspelled `Enum.Variant`.

Warnings fail a check only with `--check.deny-warnings`.

### Language and runtime changes {#3-0-1-language-and-runtime}

**Matching and binding**
- **Payload binding:** in a match arm on a variant whose payload isn't a
  record, a binding the arm names (`Status.Open (steps) =>`) holds the
  payload. The matched variable, including a `fn match` parameter, keeps the
  variant, as in 3.0.0, so an arm can pass it on as the enum. Mixed and
  optional payloads (`Vec<Int | Dec>`, `Ask?`, `Ask | Str`) bind the payload.
- **No matching arm:** a `match` with no matching arm and no default returns
  `null` in the interpreter too. It used to return the previous call's result.

**Statements and results**
- **Dotted calls:** a namespace-level `obj.field(args)` calls the field, as it
  does inside a function body. A bare `obj.field` line is a read. Before, both
  replaced the field.
- **Run results:** `hot run` and `hot eval` print the run's own result, even
  when namespaces read each other's values.
- **Value cycles:** a value cycle across namespaces is a run-time error that
  names the cycle, instead of a crash.
- **Meta names:** a meta name whose namespace hasn't run yet reads as written,
  instead of running its initializer a second time.

**Parallel flows and calls**
- Parallel flows inside lambdas and callbacks run their bindings.
- Parallel results come back in source order.
- Functions containing a `parallel` flow run in the interpreter.
- `call-lib` evaluates its arguments once.

**Printing**
- `Str()`, `println` and templates print nested containers in Hot form (`{k:
  [1]}`).

**Compiled code (the JIT)**

Compiled code, used after 100 calls or sooner with `--jit.threshold`, now
gives the interpreter's results and errors:
- collecting flows with no matching arm;
- bindings inside branches, which no longer change the outer variable;
- calls through aliases, imports and top-level lambdas;
- literal arms on an Err subject;
- `length` of a non-collection, which is an Err;
- spreads of a non-map and appends to a non-vector, which halt;
- late-capturing lambdas;
- wrong-arity calls.

Compiled self-recursion counts toward `HOT_MAX_RECURSION_DEPTH`. Running out
of native stack fails the call instead of re-running it. A panic reached from
compiled code fails the run instead of aborting the worker. Functions over
4,096 bytecode instructions aren't compiled.

**Core names**
- A bare call to a core name (`first`, `length`, `get`, …) reaches a project's
  own core function, or a var import of that name.

**ctx keys**
- A ctx key that any reachable function requires is required, even when
  another declares it optional.

### Standard library and store {#3-0-1-std-and-store}

- **`is-zero`:** `is-zero(0.0)` is `true` everywhere. `is-zero(null)` and
  `is-zero` of a map are Errs.
- **`mod`:** it takes the dividend's sign for Decs too: `mod(-7.0, 2)` is
  `-1.0`.
- **`rand-int()`:** returns a random non-negative Int; it always returned 0.
- **`flatten`:** keeps strings whole, and flattens nested vectors at every
  depth.
- **Map builders:** `group-by`, `index-by`, `frequencies`, `map-keys`,
  `map-vals`, `filter-keys` and `filter-vals` are linear. A `filter-keys` or
  `filter-vals` predicate that returns an Err now halts.
- **`Duration`:** keeps weeks and sub-second units, so `::hot::time/weeks(n)`
  moves dates.
- **`::hot::store` values:** stores Dec values exactly, so `get`, `list`,
  `vals` and `search` return Decs (19.90 stays 19.90). Values written before
  3.0.1 read back as they were stored.
- **`put-if`:** matches a Dec read back with `get`.
- **`list` and `keys`:** reject a `limit` or `offset` that isn't an Int.
- **S3 uploads:** an S3 multipart upload started on Hot 2.6.10 or earlier is
  aborted with an upload conflict; start it again.

### `hot test` {#3-0-1-hot-test}

- A relative schedule counts from when its `hot:schedule:new` was sent.
- `deliver` runs alias event handlers, as the worker does, and is linear in
  the events delivered.
- `exit(0)` in a test or handler returns, and `exit(N)` fails it. A handler
  that calls `cancel()` is not a failure.
- `deliver(event)` outside `hot test` errors without publishing.
- `advance` refuses a zero duration.

### CLI, formatter and language server {#3-0-1-cli}

- **`hot check`:**
  - **Exit codes:** 0 when it passes, 1 for errors, 2 when it couldn't run. An
    unknown `--check.format`, a missing path, or src paths with no `.hot`
    files now exit 2.
  - **JSON output:** `--check.format json` reports the same schedule, box-size
    and policy errors as the text check.
- **Secret scan:** the `hot check` and `hot watch` secret scan honours
  `hot.build.allow-secret-shape`, and reports `secret-shape` diagnostics.
- **`hot fmt`:**
  - **Refusals:** it refuses, untouched, a file it can't format safely: one
    that doesn't parse, or whose formatting would change the program or a
    string literal. `--check` exits 1 for files it can't read.
  - **Options:** `--force` is removed.
  - **What it keeps:** block strings, meta comments and trailing commas are
    kept.
- **Daemon mode:** only `hot dev`, `hot api`, `hot app`, `hot worker`, `hot
  task-worker` and `hot scheduler` daemonize, and only with `-d`.
  `hot.daemon`/`HOT_DAEMON` no longer make other commands respawn.
- **Projects:**
  - `hot deploy -p X` deploys to remote project X.
  - `hot build`, `hot compile` and `hot deploy` stop for a project `hot.hot`
    doesn't define.
  - `hot init` turns the directory name into a valid project name, and ignores
    `hot/dev.key`.
- **Dependencies:** `hot deps add`/`remove` exit 1 with the `hot.hot` snippet
  to write. They were placeholders.
- **Queues:** `hot queue status` prints the backend and where its queues live.
  It and `hot queue clear` no longer print the Redis password.
- **Environment:**
  - `HOT_WORKER_RUN_TIMEOUT`, `HOT_WORKER_MAX_RUN_TIMEOUT` and
    `HOT_WORKER_CANCEL_ON_TIMEOUT` take effect.
  - `<NAME>_FILE` works for values `hot.hot` reads with `::hot::env/get`.
  - `HOT_APP_SESSION_SECRET` and `HOT_ENCRYPTION_KEY` are kept for the process
    once found, so rotating them takes a restart.
- **Language server:** positions are UTF-16, and the workspace root comes from
  the editor. Diagnostics match `hot check` and update when other files
  change. Formatting and workspace symbols are new.
- **Skills:** an installed `.skills/` directory is no longer compiled as
  project source.

### API, streams and workers {#3-0-1-api}

- **`/v1/orgs` responses:**
  - `POST /v1/orgs` returns `409 slug_taken`/`external_id_taken` instead of
    500, and an idempotent replay returns 200.
  - Env-key and org-key lists return every key with real pagination.
  - Org key objects have a chain-aware `status`.
- **Errors:** every `/v1` error uses the JSON envelope. Webhooks and MCP
  answer an org key with `403 key_scope_mismatch`.
- **Projects and builds:**
  - Deactivating a project returns 200.
  - Deleting a project with builds returns `409 project_has_builds`.
  - `GET /v1/builds` includes deactivated projects.
- **Usage:** monthly allowances no longer reset early for month-end renewals.
- **Streams:**
  - Each run's `run:start` arrives before its data and its end, and each event
    arrives once.
  - With Redis, subscriptions survive quiet periods and Redis errors.
  - A retried attempt's `run:fail` carries `status: "pending_retry"`.
- **MCP and calls:**
  - MCP `tools/call` binds named arguments in declared order on PostgreSQL.
    Redeploy tools that have optional parameters between required ones.
  - A `hot:call` to a missing function fails at once.
- **Queues and workers:**
  - A run killed by a trimmed Redis stream entry, a recovered Redis outage, or
    a worker crash is re-run instead of lost.
  - Setup failures are retried.
  - An event that crashes its worker three times alone is dead-lettered with a
    failed run.
  - On memory queues, an event whose handler panics every time is
    dead-lettered when its attempts run out instead of redelivered forever.
  - A run records at most 10,000 calls by default
    (`hot.emitter.max-calls-per-run`).
- **Schedules:**
  - Dynamic schedules receive their `args`.
  - Schedules a 3.0.0 bundle stored from one-time strings keep running until
    the project is redeployed.
  - `hot dev` restarts its scheduler after a slow database instead of
    stopping.
- **Hot Box:**
  - `::hot::task/start(..., {type: "container"})` fails; use
    `::hot::box/start`.
  - Cancelling a box task stops its container.
  - Untagged images pull `:latest`.
  - Box task runs finish.

### App {#3-0-1-app}

- **XSS fixes:** stored and reflected XSS in the streams list, the API-key
  pages, the event and run data links, and the webhook pages is fixed.
- **Key form:** the new-API-key form keeps a Restricted key restricted after a
  validation error.
- **Error pages:** unknown ids return a 404 page, and a suspended or archived
  workspace explains why.
- **Front-end libraries:** htmx 2.0.11 and Alpine 3.17.5.
- **Look and feel:** higher-contrast text, unified status badges, a visible
  focus ring, and scrolling tab bars on phones.

### Dependencies and security {#3-0-1-dependencies}

- **RustSec advisories cleared:**
  - zip: RUSTSEC-2026-0367, -0368 and -0369;
  - ctutils, in the AES-GCM used for stored context secrets:
    RUSTSEC-2026-0385;
  - crypto-bigint: RUSTSEC-2026-0387;
  - pulldown-cmark: RUSTSEC-2026-0414.
- **Dependency updates:** cranelift 0.137, rustyline 19, jsonschema 0.60 (MCP
  schema-validation fixes), parking_lot 0.13, google-cloud-storage 1.21, and
  tower-lsp-server for the language server.
- **Packaging:** the Linux `.deb` no longer depends on `libssl3t64`, and
  release builds use `--locked`.
- **Reporting vulnerabilities:** send reports to `security@hot.dev`.

## Upgrading to Hot 3.0 {#upgrading-to-hot-30}

Hot 3.0 changes the database schema, the language, and API permissions. Read
this section before you upgrade.

### Before You Upgrade

- **Back up your database.** For a local project, stop `hot dev` and any
  other Hot process, then copy the `.hot/db/` directory. For PostgreSQL, take
  a snapshot. See [Backups](/docs/configuration/postgres#backups).
- **Hot 3.0.0 applies migrations 012 to 016 on PostgreSQL, and 3.0.1 applies
  017 to 023.** Hot 2.6.11 added 010 and 011. Downgrading a migrated database
  is not supported. To go back, restore your backup.
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

Builds are tied to a major version of Hot. A 3.0 server doesn't run builds made
with a 2.x CLI, and a 2.x CLI can't deploy to a 3.0 server. In CI, pin the
setup-hot `version` input so pipelines move to 3.0 when you do. See
[Pin a Specific Version](/docs/ci-cd#pin-a-specific-version).

Before you stop the 2.x servers, let their queues drain. Work queued for a 2.x
build can't run on 3.0.

After you upgrade the server, redeploy every project with the 3.0 CLI. To
redeploy them all at once, run `hot db redeploy` with the 3.0 CLI and the
server's configuration:

```bash
hot db redeploy --dry-run   # Build and check each 2.x deployment, change nothing
hot db redeploy             # Deploy the ones that pass
hot db redeploy --notify    # Also email the admins of each account whose project fails
```

It builds each 2.x deployment's source again with 3.0 and checks it as
`hot deploy` does. A project that passes gets a new build, and switches over
when a worker activates it. A project that fails keeps its 2.x deployment.
`--notify` also raises a `deploy:failed` alert for it. `--org <slug>` limits
the run to one organization and its sub-organizations.

Until a 2.x deployment is redeployed:

- Its events and function calls wait, and run on the new deployment.
- Its schedules are paused.
- Its webhooks return HTTP 503, and its MCP tools return an error.
- Its tasks, and calls already bound to its 2.x build, fail with a message to
  redeploy.
- Other projects that handle the same event still run their handlers.

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
- **Functions are lexically scoped.** A function reads its own parameters and
  locals, then its namespace, then core. Through 2.6.11, a caller's parameter
  or local hid a namespace binding of the same name inside every function it
  called: a `check` that reads the namespace's `limit` saw its caller's
  `limit` parameter instead. Now it reads the namespace value. Lambdas and
  functions defined in a function body still see the bindings around them. To
  give a function a caller's value, pass it as an argument.
- **A parameter or local shadows a function of its name.** `scale(x)` inside
  `apply fn (x: Int, scale: Fn)` calls the parameter. Through 2.6.11, it called
  a function named `scale` declared before `apply`. A `lazy` parameter doesn't
  shadow.
- **Same-arity overloads dispatch on argument types.** A call picks the
  overload declared with the argument's types, else the most specific one that
  accepts them: a single type, then a union or nullable type, then `Any`.
  Through 2.6.11, a variable or call result as the argument reached whichever
  overload was declared last.
- **Result checks never catch a halt.** `is-err`, `is-ok`, `if-err`, and
  `if-ok` used to treat a halt in their argument (`fail()`, `cancel()`, an
  `Err` consumed inside it) as an `Err`. Now they check the value only, and the
  halt propagates, so `is-err(f())` agrees with `r f()` then `is-err(r)`. Code
  that used `if-err(may-fail(), fallback)` to catch a `fail()` should return
  `err(...)` instead, or run the work as a task. See
  [Checks Never Catch Halts](/docs/language/errors#checks-never-catch-halts).
- **A parameter declared `Result` receives an `Err`.** Any other parameter
  consumes its argument, also when a function is called through `map` and
  other higher-order functions; callbacks used to receive `Err` elements
  unconsumed. A callback over `OnErr.Preserve` results declares its parameter
  `Result`: `map(results, (r: Result) { ... })`. See
  [Accepting a Result](/docs/language/errors#accepting-a-result).
- **A failing core call returns an `Err` on every path.** A bare call such as
  `add("x", 1)` halted on the spot in the interpreter, while
  `::hot::math/add("x", 1)` and compiled code returned the `Err`. Now all three
  return it, and it halts where it's consumed.
- **Int overflow returns an `Err`.** `add`, `sub`, `mul`, `pow`, and `abs`
  wrapped around when an Int result didn't fit in 64 bits, and `pow` rounded
  large results. Now an overflow gives `Err("Integer overflow")`, as
  `div(1, 0)` gives `Err("Division by zero")`. So do `floor`, `ceil`, `round`,
  and `Int` of a `Dec` too large for an Int. Use a `Dec` for larger numbers.
- **Calling a value that isn't a function returns an `Err`.** With `v` bound
  to `5`, a map, or `null`, `v(1)` returned `1`, and other calls returned
  `null`. Now they give `Err("Cannot call a value of type Int")`.
- **`lazy` only defers evaluation.** Consuming an `Err` inside a lazy argument
  halts as it does anywhere else; it used to pass the `Err` along as data.
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
- **App sign-in cookies are `__Host-` cookies.** Outside local development,
  the app's session cookies carry the `__Host-` prefix, so no other host can
  set or replace them. Everyone signs in again once after the upgrade.
- **Usage is per environment.** `GET /v1/org/usage` reports the credential's
  environment and adds `env_id`, `usage_scope`, and `limits_scope`.
  `team_members` is `0`. A request whose `org_id` or `env_id` doesn't match
  the credential returns 403.
- **Org API keys replace management credentials.** Root administrators can
  create child organizations and manage them with org-scoped API keys. See
  [Sub-organizations](/docs/api/management). Hot 3.0.0 shipped this as `hm_`
  management credentials and `/v1/management/*`. Hot 3.0.1 removes both:
  `/v1/management/*` returns 410 Gone, and an `hm_` credential is refused.
  Migration 020 revokes every management credential and deactivates the
  environment keys one issued, so they stop working. Before you upgrade, list
  those keys and the clients that use them. On PostgreSQL, run this as a
  superuser or a `BYPASSRLS` role, since row-level security hides the rows
  from other logins; on SQLite, drop the `hot.` prefixes:

  ```sql
  SELECT k.api_key_id, k.description, e.name AS env, m.name AS issued_by
  FROM hot.api_key_management_issuer i
  JOIN hot.api_key k ON k.api_key_id = i.api_key_id
  JOIN hot.env e ON e.env_id = k.env_id
  JOIN hot.management_key m ON m.management_key_id = i.management_key_id
  WHERE k.active;
  ```

  After the upgrade, create an org key in the app and mint replacement keys
  through `/v1/orgs`. A deactivated key can also be reactivated in the app;
  it then works as an ordinary environment key.

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
table, and there is no rolling upgrade from 2.x. Follow
[Upgrading from Hot 2.x](/docs/configuration/postgres#upgrading-from-hot-2x)
before you upgrade.

From 3.0.0 to 3.0.1 you can upgrade while 3.0.0 keeps serving. See
[Upgrading from Hot 3.0.0](/docs/configuration/postgres#upgrading-from-hot-300).

## Troubleshooting Migrations {#troubleshooting-migrations}

`hot db status` shows which migrations a database has applied, which are
pending, and which were applied from a different file. On PostgreSQL, pass the
schema owner's `--db.migration-uri`: the runtime login can't read the
migration ledger. A migration that fails rolls back on its own, while the ones
before it stay applied, so check the status before you retry.

On PostgreSQL, each migration statement waits at most 5 seconds for a lock
held by running traffic, then the migration rolls back and is retried, up to
five times. Set `db.migration-lock-timeout-ms` to change the wait.

### Migration 019 refuses to run {#migration-019-refuses-to-run}

Migration 019 adds keys that earlier releases didn't enforce, and stops with
the first row that breaks one. Repair the rows and run `hot db migrate`
again. On PostgreSQL, run these queries as a superuser or a `BYPASSRLS` role:
row-level security hides the rows from every other login, the schema owner
included. On SQLite, drop the `hot.` prefixes.

- **An environment has two projects with the same name.** List them, then
  rename all but one in each group, for example with
  `PATCH /v1/projects/{project_id}` on Hot 3.0.0:

  ```sql
  SELECT env_id, name, count(*) FROM hot.project
  GROUP BY env_id, name HAVING count(*) > 1;
  ```

- **A store references a missing organization or environment.** Its
  organization or environment was deleted, so nothing can read it. Delete its
  rows:

  ```sql
  DELETE FROM hot.store_map_entry s WHERE NOT EXISTS (SELECT 1 FROM hot.org o WHERE o.org_id = s.org_id)
    OR NOT EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = s.env_id);
  DELETE FROM hot.store_map s WHERE NOT EXISTS (SELECT 1 FROM hot.org o WHERE o.org_id = s.org_id)
    OR (s.env_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = s.env_id));
  ```

### A migration was applied from a different file {#a-migration-was-applied-from-a-different-file}

Hot checks that every migration a database has applied matches the file this
release ships. When one doesn't, a Hot build with different migrations, such
as an unreleased development build, migrated the database. Hot stops instead
of guessing. Don't delete the database: restore the backup you took before
that build ran, then upgrade with a release. A database a newer Hot release
migrated says so instead: upgrade Hot to that release.

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
