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

## Upgrading from Hot 3.0.8 to 3.0.9 {#upgrading-from-3-0-8-to-3-0-9}

**No migrations.** Replace the binaries and restart.

**What to check in your code:** run `hot check` before you deploy.

- **A function named like a core function.** If a namespace defines its own `add`, `get`, `length`, `send`, `if` or any other core name, a bare call of that name now always means your function. On 3.0.8 a call with an argument count your function doesn't declare, a spread call, or a call written above the definition could run the core function. Those calls are now `arity-mismatch` in `hot check`, or fail at run time with "Function '::ns/name' expects N arguments, got M". Call the core function by its path, such as `::hot::math/add(1, 2, 3)`, or rename yours. Other namespaces, imports and your own `core: true` functions are unchanged.
- **A qualified annotation followed by a qualified call.** `x: ::a/T ::a/f(v)` now calls `f`. On 3.0.8 it bound `x` to `v` without calling anything.
- **`type open` before a type that is not a literal union** is an error.
- **`hot check` reports more true errors:** wrong-typed calls to a namespace function named like a core one, calls through an import under a core name, and field reads in a `match` default arm that the narrowed type doesn't have. Those programs compiled before and failed when run.

**What to check in your webhooks:** an untyped map is treated as a response only when its `status` is an integer from 100 to 599. A handler that returned a map with some other `status` value, such as `{status: "ok"}`, now sends that map as the body. Return an `HttpResponse` to set a status.

**What to check in your packages:** 38 packages have new patch versions. Three `openai` signatures changed: `delete-certificate` takes `{certificate-id: Str}`, `modify-certificate` takes `(certificate-id, request)`, and `DeleteResponseResponse` is `{id, object?, deleted?}`. `supabase` single-value filters are no longer quoted, `hot-ai` `compact-window` returns an Err where it halted, and `slack` GET calls now send a `false` value.

**What to check in your deployment:**

- Interpreted call-heavy code is about 3% slower. Collection builders are much faster. See the release notes.
- `merge` no longer refuses a large left-hand map. Only the map being merged in counts against the expansion limits.
- A task `timeout` below zero now clamps to 1 second, not 7 days.
- `BoxQuota.tasks-remaining` is gone. Read `minutes-limit` and `compute-units-limit`, where `-1` means unlimited.
- A self-host image built from v3.0.8 or earlier starts no box. Rebuild the host image if you run boxes.
- In MCP tool schemas, `.` in `pattern` no longer matches `\r`, U+2028 or U+2029.

## Upgrading from Hot 3.0.7 to 3.0.8 {#upgrading-from-3-0-7-to-3-0-8}

**Migrations 036 to 039.** Run `hot db migrate` with the 3.0.8 CLI. They add indexes on `event(env_id, event_time)` and `event(stream_id, event_time)` and a partial index on `email_queue` for emails being sent, then drop `idx_event_stream_id`, which the new stream index covers. On Postgres each runs outside a transaction with `CONCURRENTLY`, so event writes don't wait. On a large `event` table the first two can take a while. If one is interrupted, Postgres leaves an invalid index behind: drop it and run `hot db migrate` again.

**What to check in your code:** run `hot check` before you deploy. These now fail to compile or return an Err:

- number literals followed by letters, such as `1e10`, `0xFF` and `1_000`;
- `!value`, which ran as `value`;
- a type with `where` declared inside a function;
- a local's `meta` on the line after its value;
- a non-finite Dec result, such as `pow(0, -1)` or `Dec("NaN")`;
- a failed `-> Str` conversion inside a template, which now halts.

`hot check` now also reports type errors in calls inside map and vec literals and in calls into a namespace checked later. Those programs compiled before and failed when run.

**What to check in your deployment:** each `hot` process holds about 40 MB more memory on mimalloc, now the default allocator. If a service no longer fits its limit, set `HOT_ALLOCATOR=system` in its environment. A dead worker's tasks are reaped after 60 to 90 seconds instead of 30. Workers still on 3.0.7 during a rolling upgrade fall back to the usual orphan reclaim.

**What to check in your email:** the first recovery sweep after the upgrade fails pending emails older than 6 hours.

## Upgrading from Hot 3.0.6 to 3.0.7 {#upgrading-from-3-0-6-to-3-0-7}

**Migration 035.** Run `hot db migrate` with the 3.0.7 CLI. It adds a nullable `revoked_at` column to `api_key`, a check that a revoked key is inactive, and a trigger that refuses any write undoing a revocation. Existing rows keep NULL.

**What to check in your keys:** environment keys revoked before this migration can't be told apart from disabled ones. They report `disabled` and can be re-enabled, so revoke any of them again if you need the revocation to be permanent.

**What to check in your packages:** `::ai::agent::callbacks/from-body` no longer reads `callback_secret_env` from the body, so pass the env var name as the second argument. Callback URLs must be `https` on a dotted public DNS name or a public IPv4 address. In the box packages, each `extra-args` element is now one argument, so `["-c:v libx264"]` becomes `["-c:v", "libx264"]`.

## Upgrading from Hot 3.0.5 to 3.0.6 {#upgrading-from-3-0-5-to-3-0-6}

**No migrations.** 3.0.6 doesn't change the database schema, so `hot db migrate` has nothing to apply.

**Optional: arm dynamic schedules after a deploy.** Activating a build cancels the project's dynamic schedules on its other builds, as it has since 3.0.5. Hot now sends a `hot:deploy:activated` event each time a build goes live, so an app can arm them again. See [Dynamic Schedules and Deploys](/docs/schedules#dynamic-schedules-and-deploys).

**What to check in your packages:** the approved tags changed. `payments` is now `finance`, `automation` folds into `web`, and `email` into `messaging`. See [Tags](/docs/packages/manifest#tags).

## Upgrading from Hot 3.0.4 to 3.0.5 {#upgrading-from-3-0-4-to-3-0-5}

**Migrations 032, 033 and 034.** Run `hot db migrate` with the 3.0.5 CLI. Migration 032 adds a nullable `enqueue_payload` column to `event`, which holds an event's queue packet until it's enqueued. Migration 033 adds a nullable `quota_epoch` column to `parked_event`. Existing rows keep NULL, which means nothing is owed.

**Migration 034 builds an index concurrently.** On PostgreSQL it creates `event_outbox_due_idx` with `CREATE INDEX CONCURRENTLY`, so writes to `event` continue while it builds. It runs outside a transaction, and waits for transactions already open on `event`. If the build is interrupted, the next `hot db migrate` drops the invalid index and builds it again.

**Kata task-worker hosts need `nftables`.** Install it, or rebuild the host image, before you deploy the 3.0.5 task worker. Without `nft`, every box with network access fails. Boxes also lose `NET_RAW`, and run after the image's `ENTRYPOINT` as its `USER`. See [Box command compatibility](/docs/configuration/network).

**Optional: expire old notification streams.** Redis notification streams now expire when idle, and streams created before 3.0.5 keep no expiry. Run `scripts/expire-notification-streams.sh` to preview the backfill, then again with `--apply`. It deletes nothing and leaves existing expiries alone.

**What to check in your code:**

- A generated constructor's refusals are all a `ConstraintError.Violation`, including a missing required field, which used to be a plain `Str`. Code that matched the old error text needs the new text.
- `distinct` treats two maps with the same entries in a different order as one value.
- A rest parameter checks every argument, not only the positions it declares.

## Upgrading from Hot 3.0.3 to 3.0.4 {#upgrading-from-3-0-3-to-3-0-4}

**Migrations 030 and 031.** Run `hot db migrate` with the 3.0.4 CLI. Migration 030 replaces the unique index on `schedule`, and on PostgreSQL it blocks writes to that table while the new index builds. Migration 031 adds a nullable `enqueue_leased_until` column to `event`.

**Restart services straight after migrating.** A build deployed through a worker of the previous version after migration 030 fails. Deploy it again once the workers run 3.0.4.

**Most of what to check is in values that used to build and are now an Err.** Records, types over built-in ones, closed enum fields and collections now check what they're given:

- A record given a field of the wrong type is an Err, and so is null at a required field. Declare the field `T?` or fix the value. Numbers convert as at a parameter, so a `Dec` field given `5` holds a `Dec`, and `eq(record.amount, 5)` is false: compare with `5.0` or declare the field `Int`.
- With `Meters type Int`, `Meters("x")` is an Err, and so is null unless the type allows it. Declare a field `Name?`, or the type `Name type Str?`, where null is expected.
- A closed enum field given a Map that fits no variant, or more than one, is an Err. Declare the enum `open`, or the field `Map`, where a Map of another shape is expected.
- `Map<K, V>` fields, and types over a `Vec` or `Map`, now build what they hold, so `to-json` writes those values tagged and `untype` removes the tag. A value that can't be built is an Err. Declare `Map<K, Any>` or `Vec<Any>` where values of any shape are expected.
- A `Map<Int, V>` field reads the JSON key `"1"` as the number `1`, so read it with `1`, not `"1"`.

**Packages:** a package's records are checked by the Hot that runs them, so an older `hot.dev/openai` refuses responses with null in fields OpenAI leaves null. Move to openai 1.3.1, qwen 1.0.4 or openai-compatible 0.1.1. In any generated API package, a response whose real data breaks a field's declared type is now an Err.

**Behaviour changes:**

- `gt`, `lt`, `gte` and `lte` compare a value of a declared type as the number it holds, and give an `Err` for values with no order, such as `gt(null, 1)`. Like any `Err` it halts when used, and `if` reads it as false, so code that compared a missing field with a number takes the other branch or stops where it went on before.
- `sort` and `sort-by` put numbers in order by value, inside other values too.
- `untype` keeps a variant's name: `untype(Due.Days(3))` is `{Days: 3}`, not `3`, and `to-json(untype(value))` changes the same way. Read a payload with `match` or field access. Plain data saved by an earlier `untype` holds no variant name, so a record field of an enum type refuses a bare payload.
- A second key rotation during an overlap answers `409` `rotation_overlap_running`. To end an overlap early, rotate with `"overlap_seconds": 0`.
- In direct mode, `::hot::file/list-files` gives whole paths, so a script that added the folder to `path` itself now gets it twice. `hot check` refuses the result at a `Vec<Str>` parameter.
- `install.sh` with no mode chosen prefers an existing `~/.local` install over a package. A Linux machine with only the `.deb` now updates the package.

## Upgrading from Hot 3.0.2 to 3.0.3 {#upgrading-from-3-0-2-to-3-0-3}

**Migration 029** adds a nullable `event_handler_fn` column to `run`. Run `hot db migrate` with the 3.0.3 CLI.

**Packages withdrawn:** `sanctions`, `company-registry`, `web-search`, `crm` and `documents` are gone, with no compatibility layer. Move to the per-service packages: `ofac`, `uk-sanctions`, `eu-sanctions`, `un-sanctions` and `opensanctions`; `gleif`, `sec-edgar` and `companies-house`; `tavily`, `brave-search`, `exa` and `firecrawl`; `hubspot`, `salesforce` and `attio`; and `google-drive`, `microsoft-onedrive`, `microsoft-sharepoint`, `dropbox`, `box` and `aws-s3`. The sanctions packages fetch and parse only, so storing lists and matching names are up to your project.

**`from-xml`** returns an Err for text with more than one root element, such as `<p>a</p><p>b</p>`. It used to return the last root and drop the others. Wrap such text in one root element, or parse it with `::hot::html/from-html` if it is HTML.

**Packages** that use `::hot::html`, or call `::hot::gzip`, `::hot::zip` or `::hot::bytes/to-str`, need `hot-min-version: "3.0.3"`.

`::hot::gzip/decode` and `::hot::zip/read-entry` refuse output over 64 MiB. Raise that with `hot.compression.max-output-bytes` in `hot.hot`, or read larger data with `decode-stream` and `read-entry-stream`, which hold one chunk at a time.

**Behaviour changes:**

- A handler with no `retry` that shares an event with one that has it is no longer retried, and a retry no longer runs the event's other handlers. A handler that relied on either needs its own `retry`.
- `info().run.max-retries` and `retry-delay` now report a run's retries, including in a task. Code that treated a null as "no retries" now sees the real count. Code written as `or(run.max-retries, 0)` keeps working.
- A value nested over 120 levels, or one whose record would be over 32 MB and more than four times its size in memory, is recorded as `::hot::blob/Omitted`. The value itself is unchanged, and only its record differs.

## Upgrading from Hot 3.0.1 to 3.0.2 {#upgrading-from-3-0-1-to-3-0-2}

Hot 3.0.2 masks more secrets in call and run records. See [Secret Masking](/docs/language/meta#secret-masking) for what it covers and what it doesn't.

**Records written before you upgrade aren't changed.** On earlier versions, call records could hold secrets composed into longer strings (such as an `Authorization: Bearer` header), values passed to `::hot::ctx/set-secret`, tokens returned by API calls, and credentials passed as arguments. If your project made API calls that way:

1. **Rotate** the keys it used, if anyone you wouldn't give them to can read your call records.
2. **Purge** the call records you no longer need: `DELETE /v1/runs/{run_id}/calls`, `/v1/streams/{stream_id}/calls` or `/v1/env/calls` with an environment key, or `DELETE /v1/orgs/{org}/calls` with an org key. Repeat each request until `complete` is `true`. See the [API docs](/docs/api). Purges cover call records only: a run's stored result and failure, and event data, stay.

**Behaviour changes:**

- A stored `run.result` no longer holds credential fields or registered secrets. Code that reads a run's result back through the API to get a credential now sees `<secret>`. A webhook or MCP caller waiting on the run still receives the real result.
- A value is masked inside longer strings once registered, so a secret key holding a short, common value can hide more of a record than before. Declare `secret: false` on configuration values. Declared defaults aren't masked inside strings.
- Named functions now run implicit arrows in `map` and other callbacks, in `call`, and through a variable, as direct calls do. A callback that relied on receiving the unconverted value now gets the converted one.

**Packages** that call `::hot::ctx/mark-secret` need `hot-min-version: "3.0.2"`.

**Migrations 024 to 028.** Run `hot db migrate` with the 3.0.2 CLI. Migration 024 adds rollover columns to `usage_task`. Migration 025 builds a new index on `call`, and 026 drops the index it replaces. Migration 027 makes autovacuum visit `call` sooner after inserts. Migration 028 makes email addresses case-insensitive. On SQLite, 024 and 027 change nothing.

**On PostgreSQL, the upgrade builds an index on `call`.** Migrations 025 and 026 replace `call_env_start_time_idx` with an index that also holds each call's size, so hourly usage reconciliation no longer reads the whole table. Trace writes keep running during the build, though they slow down if they share a disk with it. It reads all of `call` and then waits for every older transaction to finish, so give your migration job room: a 2.4 GB table took about 20 s on a 125 MB/s disk. If the job is interrupted, run `hot db migrate` again.

**Email addresses now identify one account, whatever their capitals.** `DANA@corp.com` and `dana@corp.com` are the same address for sign-up, sign-in, verification, invitations and OAuth, and a second account can't be created under other capitals. Mail still goes to the address as it was typed. Only the letters A to Z fold: other letters, dots and `+` suffixes still make a different address.

**Accounts that already differ only in capitals are kept.** Migration 028 merges and deletes nothing, because it can't tell whether two accounts belong to one person, which password to keep, or which role wins in an organization they share. In each such set:

- The oldest active account answers to any spelling.
- Each of the others answers only to its exact spelling, with its own password, memberships and history.

To list those accounts, run this as a superuser or a `BYPASSRLS` role, because row-level security hides the rows from every other login, the schema owner included. On SQLite, drop the `hot.` prefix.

```sql
SELECT user_id, email, active FROM hot."user" WHERE email_exact_only ORDER BY lower(email), created_at;
```

To change which account answers to any spelling, set `email_exact_only` to true on the current one, then to false on the other, in that order.

**On PostgreSQL, migration 028 rewrites `user` and `email_verification`,** blocking reads and writes to them while it runs. Both are small, so this is usually brief.

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

**Migrations 017–023 run on upgrade.** A SQLite database that 3.0.0 migrated also gets 015 and 016, which 3.0.0 shipped for PostgreSQL only.
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
- **Exit status:** `hot app`, `hot api`, `hot worker`, `hot task-worker` and
  `hot scheduler` exit 1 when they stop on an error or a panic, so a
  supervisor's restart-on-failure policy restarts them. 3.0.0 exited 0.
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
- **Sub-organizations:** the organization's Children tab is now Sub-orgs, and
  the App and docs call child organizations sub-organizations.

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
- **Hot 3.0.0 applies migrations 012 to 016 on PostgreSQL and 012 to 014 on
  SQLite. Hot 3.0.1 applies 017 to 023, and on SQLite also 015 and 016.** Hot
  2.6.11 added 010 and 011. Downgrading a migrated database
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
- **Org API keys replace management credentials.** Parent organization
  administrators create sub-organizations and manage them with org API keys. See
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

`hot db migrate --status` gives tools the same answer as one JSON line on standard
output, and changes nothing:

```json
{"level":23,"latest":23,"pending":[],"modified":[],"unknown":[]}
```

- `level` is the highest migration in the database's ledger, whichever Hot applied
  it. 0 means none has run.
- `latest` is the highest migration this Hot has. A database a newer Hot migrated
  has a `level` above it.
- `pending` lists the migrations this Hot has and the database doesn't, `modified`
  those applied from a different file, and `unknown` those this Hot doesn't have.

On PostgreSQL it needs `db.migration-uri`, naming the same database as `db.uri`. It
exits non-zero when it can't read the ledger, including with no migration URI or a
SQLite file that isn't there, which it doesn't create. A database a newer Hot
migrated, or one with a modified migration, still exits 0. Earlier releases don't
have the flag, and exit with a usage error. Hot's self-hosted stacks run it
before an upgrade, so a database restored to before the upgrade can go back to the
earlier release.

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

## Hot 2.x Releases and Migrations {#hot-2x-migrations}

Each Hot 2.x release below added database migrations. A 2.x release that isn't
listed added none. The numbers are the same for SQLite and PostgreSQL.
`hot db status` lists the migrations a database still needs.

| Release | Migrations | What they add |
|---------|------------|---------------|
| 2.0.0 | 001 | The Hot 2 baseline schema |
| 2.0.9 | 002 | On SQLite, the `::hot::store` tables. On PostgreSQL, a required environment on every store |
| 2.1.0 | 003 | The embedding configuration of each store |
| 2.4.0 | 004 | Build runtime status and deployment order |
| 2.4.3 | 005 | The engine and `hot-std` version of each build |
| 2.5.0 | 006 | Blob tables for large values |
| 2.6.1 | 007 | A unique index for files that have no environment |
| 2.6.7 | 008 | Task timing |
| 2.6.8 | 009 | Task retry lineage |
| 2.6.11 | 010, 011 | Multipart upload completion and retry records |

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

## Upgrading to Hot 2.5 {#upgrading-to-hot-25}

Hot 2.5.0 stores large values outside the database. A large string or byte
value in a persisted call, run, or event is written to blob storage, and the
database row keeps a small reference to it. Blobs use the same backend as file
storage. See [Storage](/docs/configuration/storage).

This is on by default inside a project. To keep every value in the database,
set `hot.blob.mode "disabled"` in `hot.hot`.

## Upgrading to Hot 2.4 {#upgrading-to-hot-24}

From Hot 2.4.7, a project that doesn't set `hot.file.mode` uses `service` mode
for `::hot::file`. Earlier releases used `direct` mode. In `service` mode,
files go through Hot's file storage and are recorded in the database. In
`direct` mode, paths are read and written on the local filesystem as given.

If your local project relies on direct filesystem paths, set
`hot.file.mode "direct"` in `hot.hot`. Commands run outside a project still
default to `direct`. When `HOT_ENV` is not `development`, Hot always uses
`service`.

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
