---
description: "Use the Hot CLI to create projects, run checks and tests, deploy apps, manage packages, and inspect resources."
---

# Hot CLI

The `hot` command-line interface is your primary tool for developing, testing, and deploying Hot applications.

## Quick Reference

| Command | Description |
|---------|-------------|
| `hot dev` | Start all services in development mode |
| `hot run <file>` | Execute a single `.hot` file |
| `hot eval '<code>'` | Evaluate Hot code directly |
| `hot test` | Run tests |
| `hot check` | Analyze code for errors |
| `hot deploy` | Deploy to Hot Cloud |

## Services

### hot dev

Start all services for local development:

```bash
hot dev
```

This starts:
- **API** at `http://localhost:4681` — API for function calls and events
- **App** at `http://localhost:4680` — Web dashboard for monitoring
- **Worker** — Processes background jobs and event handlers
- **Task worker** — Processes `::hot::task` and `::hot::box` tasks
- **Scheduler** — Runs scheduled functions

Common options:

```bash
hot dev --open                    # Open dashboard in browser
hot dev --api.port 8080           # Custom API port
hot dev --app.port 3000           # Custom App port
hot dev --worker.threads 16       # More worker threads
hot dev -d                        # Daemon mode (background)
```

Set `hot.dev.open true` in your `hot.hot` to always open the browser on start.

### hot api

Run just the API server:

```bash
hot api
hot api --api.port 8080 --api.host 0.0.0.0
```

### hot app

Run just the web application (dashboard):

```bash
hot app
hot app --app.port 3000
```

### hot worker

Run just the background worker:

```bash
hot worker
hot worker --worker.threads 16
```

### hot task-worker

Run just the task worker (processes `::hot::task` and `::hot::box` tasks):

```bash
hot task-worker
```

Local `::hot::box` tasks need Docker. If Docker is not running, `hot task-worker` (and `hot dev`) will log a warning and box starts will fail.

### hot scheduler

Run just the job scheduler:

```bash
hot scheduler
```

With PostgreSQL, only one scheduler runs at a time. It holds a session
advisory lock and confirms it every two seconds. If its database session ends
(a failover, a terminated backend, a network partition) or a check takes longer
than a second, it stops and exits non-zero, and its supervisor restarts it.
A second scheduler started while one is running exits immediately, so replace a
scheduler by stopping the old one first. Connect the scheduler to PostgreSQL
directly or through a session-mode pooler; it refuses to start through a
transaction- or statement-mode pooler that loses its lock.

## Running Code

### hot run

Execute a single `.hot` file:

```bash
hot run script.hot
hot run path/to/file.hot
hot run script.hot --value.format json    # Output as JSON
```

### hot eval

Evaluate Hot code directly from the command line:

```bash
hot eval 'add(1, 2)'
hot eval '::my-app::hi/hello()'
hot eval 'send("user:created", {id: 123})'
hot eval '[1, 2, 3] |> map(mul(%, 2))'
```

Output format:

```bash
hot eval '::my-app::get-user(1)' --value.format json
```

### hot repl

Start an interactive REPL session:

```bash
hot repl
```

## Testing & Development

### hot test

Run tests:

```bash
hot test                    # Run all tests
hot test user               # Run tests matching "user"
hot test "user signup"      # Run tests matching pattern
```

Tests are functions with `meta ["test"]` (or the equivalent `meta {test: true}`):

```hot
should-add-numbers meta ["test"]
fn () {
  assert-eq(add(1, 1), 2)
}
```

`hot test` warns about a test that makes no assertions. A common cause is a
call written with a space before `(`, as in `assert (x)`, which binds a
variable named `assert` instead of calling it.

#### Fixing the clock

Each test starts on the real clock. A test can fix it:

- `::hot::test/set-now(time)` fixes the clock at an RFC 3339 string, an
  `Instant` or a `ZonedDateTime`.
- `::hot::test/advance(duration)` moves it by a `Duration` of weeks, days or
  smaller units.
- `::hot::test/clear-now()` returns it to the real time.

{{snippet:testing#testing-clock}}

`::hot::time/now`, `now-zoned`, `epoch-seconds` and the other "now" functions
read the fixed time, in the test, in its `parallel` branches and `pmap`
workers, and in the handlers it delivers events to. After each test the runner
returns the clock to the real time. Outside `hot test` these functions return
an error, so no other run reads a fixed clock.

#### Events and schedules

In a test, `send` keeps the event instead of publishing it, and no handler
runs on its own. The test delivers events itself:

- `::hot::test/deliver(event)` runs the `on-event` handlers of `event`, a map
  with `type` and `data`, inside the test. Then it delivers the events they
  send, until none is left. `deliver()` delivers what was sent so far. It
  returns one `{type, fn, result}` for each handler run.
- A delivered `hot:schedule:new` arms a schedule in the test, with relative
  times counted from the test's clock; `hot:schedule:cancel` disarms it.
  `::hot::test/schedules()` lists what is armed.
- `::hot::test/run-due()` calls each schedule due by the test's clock,
  soonest first. Each call runs with the clock at its due time, as the
  scheduler would have run it, and its events are delivered. Afterwards the
  clock is back where the test left it.

A handler or scheduled call that fails counts as a failed assertion; the
others still run. Handlers run as plain calls: retries and run records don't
apply.

{{snippet:testing#testing-scenario}}

### hot check

Analyze code for errors without executing:

```bash
hot check                          # Check all project sources (pretty output)
hot check path/to/file.hot         # Check a specific file or directory
hot check --check.format simple    # One line per diagnostic (compact)
hot check --check.format json      # Pretty-printed LSP-shaped diagnostics
hot check --check.format json-min  # Minified JSON, ideal for CI / editor tooling
```

The `json` and `json-min` formats emit an array of LSP `Diagnostic` objects
(`range`, `severity`, `code`, `source`, `message`, optional `file`). Diagnostic
messages embed the same ariadne snippets shown in pretty mode when source is
available. Exit code is `0` when no diagnostics are produced and `1` otherwise,
so JSON mode is safe to drive from CI:

```bash
hot check --check.format json-min > diagnostics.json || cat diagnostics.json
```

Warnings, such as a deprecated API or a binding that shadows a function, don't
fail the check by default. Pass `--check.deny-warnings` (or set
`hot.check.deny-warnings true`) to exit non-zero on warnings too:

```bash
hot check --check.deny-warnings
```

### hot watch

Watch for changes and continuously re-analyze. Supports the same `--check.format`
flag as `hot check`, including `json` / `json-min` for editor / CI integration:

```bash
hot watch                          # Pretty output, re-run on save
hot watch --check.format json-min  # Stream JSON diagnostics on every change
```

### hot fmt

Format Hot source files:

```bash
hot fmt                     # Format all files
hot fmt path/to/file.hot    # Format specific file
hot fmt --check             # Check without writing (CI mode)
```

## Build & Deploy

### hot build

Create a build bundle from your project:

```bash
hot build
hot build --build.dir ./dist
```

### hot builds

List available builds.

> **Note:** This command connects to Hot Cloud by default. To list builds from your local API, pass `--local` (requires `hot dev` or `hot api` to be running).

```bash
hot builds              # List builds on Hot Cloud
hot builds --local      # List builds from local API
```

### hot compile

Compile project source and create/update the live build:

```bash
hot compile
hot compile my-project
```

### hot deploy

Deploy a build to make it live.

> **Note:** This command connects to Hot Cloud by default. To deploy locally, pass `--local` (requires `hot dev` or `hot api` to be running).

```bash
hot deploy              # Build and deploy to Hot Cloud
hot deploy <build-id>   # Deploy a specific build to Hot Cloud
hot deploy --local      # Deploy via local API (for local dev or self-hosted)
```

### hot cache

Manage bytecode and package caches:

```bash
hot cache clear         # Clear all caches
hot cache status        # Show cache info
```

## Project Management

### hot init

Initialize Hot in a directory:

```bash
hot init              # Use current directory
hot init my-app       # Create my-app/ if needed, init there
hot init path/to/app  # Create nested path if needed, init there
```

The project name is taken from the directory name. Creates three things alongside your existing files:
- `hot.hot` — Project configuration (project root)
- `hot/src/<project>/hi.hot` — Starter file with tutorial
- `.hot/` — Local data: cache, database, logs (gitignored)

In a project, the first command that needs the local SQLite database (`hot run`, `hot eval`, `hot repl`, `hot compile`, `hot build`, `hot test`, or `hot deploy`) prepares it:

- A missing database is created, as `hot init` would.
- An outdated database is migrated, with a one-line notice.
- An empty database gets the local identity: user `local@hot.dev`, organization `local`, and environment `development`.

A profile other than the local identity must already exist in the database. See [Profiles](/docs/configuration#profiles). Hot never migrates a PostgreSQL database implicitly; use `hot db migrate`.

### hot project

Manage projects:

```bash
hot project list
hot project activate my-app
hot project deactivate my-app
```

### hot projects

List all projects:

```bash
hot projects
```

### hot deps

Manage dependencies:

```bash
hot deps list           # List all dependencies
hot deps show           # Show detailed dependency info
hot deps add openai     # Add a package
hot deps remove openai  # Remove a package
hot deps update         # Resolve and cache dependencies
```

### hot context

Manage encrypted context variables (secrets).

> **Note:** This command connects to Hot Cloud by default. To manage local context variables, pass `--local` (requires `hot dev` or `hot api` to be running).

```bash
hot context list                        # List all variables
hot context get OPENAI_API_KEY          # Get a variable
hot context set OPENAI_API_KEY sk-xxx   # Set a variable
hot context delete OPENAI_API_KEY       # Delete a variable
hot context list --local                # List from local API
```

### hot conf

Show current configuration or generate configuration templates:

```bash
hot conf                       # Show resolved configuration
hot conf generate              # Generate minimal config template
hot conf generate all          # Generate full config with all options
hot conf generate api          # Generate API server config
hot conf generate app          # Generate App server config
hot conf generate worker       # Generate Worker config
hot conf generate scheduler    # Generate Scheduler config
hot conf generate -o hot.hot   # Write template to file
```

Available templates:
- **(default)** — Minimal configuration for local development (~25 lines)
- **all** — Full configuration with all available options (~180 lines)
- **api** — API server configuration (for self-hosting)
- **app** — App server configuration (for self-hosting)
- **worker** — Worker configuration (for self-hosting)
- **scheduler** — Scheduler configuration (for self-hosting)

### hot db

Manage the database in `hot.db.uri`:

```bash
hot db status                    # Show the connection and pending migrations
hot db migrate                   # Apply pending migrations
hot db provision-store-indexes   # Create PostgreSQL vector indexes for stores
hot db port-v1-to-v2             # Port a Hot 1.x SQLite database to Hot 2
hot db redeploy                  # Redeploy the deployments another major version built
hot db move-storage              # Move builds or files to another storage backend
```

On PostgreSQL, `hot db migrate` and `hot db provision-store-indexes` connect
with the schema owner in `--db.migration-uri` (or `HOT_DB_MIGRATION_URI`).
Services keep using the restricted runtime login in `--db.uri`:

```bash
hot db migrate \
  --db.uri "postgres://hot_runtime:<password>@db.example.com/hot" \
  --db.migration-uri "postgres://hot_owner:<password>@db.example.com/hot"
```

After an upgrade to a new major version, `hot db redeploy` builds every
deployment from the earlier version again, checks it, and deploys the ones that
pass. `--dry-run` changes nothing, `--notify` emails the admins of each account
whose project fails, and `--org <slug>` limits the run to one organization.

`hot db move-storage` moves builds, or files and internal blobs, to another
storage backend while Hot is stopped; see
[Switching Backends](/docs/configuration/storage#switching-backends).

See [Self-Hosted PostgreSQL](/docs/configuration/postgres) for the setup, and
[Migrations and Upgrades](/docs/migrations) for `port-v1-to-v2` and
`redeploy`.

### hot key

Create a full-access API key in the configured database:

```bash
hot key create                          # Description defaults to "cli"
hot key create --description "ci"
KEY=$(hot key create --description "ci")
```

The key belongs to the selected profile's environment. `hot key create`
prints the key once to stdout and everything else to stderr, so you can
capture it in a script. It writes to the database directly, which suits local
development, CI against a local `hot dev`, and self-hosted installations. On
Hot Cloud, create keys in the app instead. See
[Authentication](/docs/authentication).

## Tooling

### hot lsp

Start the Language Server Protocol server (used by editors):

```bash
hot lsp
```

### hot completions

Generate shell completions:

```bash
hot completions bash > ~/.bash_completions/hot
hot completions zsh > ~/.zfunc/_hot
hot completions fish > ~/.config/fish/completions/hot.fish
```

### hot ai

Add AI coding support to help AI assistants understand Hot. This uses the open AGENTS.md and SKILL.md standards, which are supported by Cursor, Claude Code, GitHub Copilot, Windsurf, and many other AI coding tools.

```bash
hot ai add              # Add AGENTS.md + bundled skills to project
hot ai add --global     # Install skills to ~/.skills/ (available in all projects)
```

`hot ai add` installs the skill snapshots bundled with your Hot release, so it
works offline and does not require Node or GitHub access. Available skills can
vary by Hot version; use `hot ai list` to inspect the installed release. To
install the latest public skills from the skills.sh ecosystem instead, use:

```bash
npx skills add hot-dev/hot-skills
```

Files created:
- `AGENTS.md` — AI agent instructions (passive context)
- `.skills/hot-language/` — Detailed Hot language skill with references
- `.skills/hot-ai-agents/` — Hot AI agent and SDK integration guidance

Other commands:

```bash
hot ai list             # Show installed AI support files
hot ai update           # Update existing files to latest version
```

## Info

### hot version

Display version information:

```bash
hot version
hot --version
hot -V
```

### hot update

Update Hot to the latest version:

```bash
hot update
```

When the latest version is a new major version, `hot update` asks before
installing it. Without a terminal to ask on, it installs nothing and prints the
`hot update --version` command to run.

Install a specific release:

```bash
hot update --version 1.4.0
```

If your installed `hot` is too old to support `--version`, use the hosted installer script:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version 1.4.0
```

Use `--force` to reinstall the current or requested version:

```bash
hot update --force
hot update --version 1.4.0 --force
```

This downloads and installs the requested version of Hot. If you're already on the selected version, it will let you know unless `--force` is set.

### hot help

Display help information:

```bash
hot help
hot help dev
hot help deploy
```

## Global Options

These options work with most commands:

| Option | Description |
|--------|-------------|
| `-c, --conf <FILE>` | Configuration file(s) |
| `--ctx <FILE>` | Context file(s) for variables |
| `-p, --project <NAME>` | Project name to use |
| `--profile <NAME>` | User, organization, and environment profile to use (see [Profiles](/docs/configuration#profiles)) |
| `-s, --src.path <DIR>` | Source directory |
| `-t, --test.path <DIR>` | Test directory |
| `--engine.threads <N>` | Engine threads (default: 4) |
| `--db.uri <URI>` | Database connection URI |
| `--db.migration-uri <URI>` | Schema-owner URI for migrations on PostgreSQL (see [Self-Hosted PostgreSQL](/docs/configuration/postgres)) |
| `--log.level <LEVEL>` | Log level: off, trace, debug, error, warn, info |
| `--log.target <TARGET>` | Log output: stdout, file, none |
| `--log.dir <DIR>` | Directory for log files |
| `--log.rotation <ROTATION>` | Log rotation: hourly, daily, none (default: daily) |
| `--log.retention <COUNT>` | Number of log files to keep, 0 = keep all (default: 7) |
| `--deploy.auto <BOOL>` | Auto-deploy on CLI commands (default: true) |
| `--emitter.type <TYPE>` | Emitter: none, console, db (default: db in project, none otherwise) |
| `--with-tests <BOOL>` | Include test files in compile/check/watch (default: true for check, false for watch) |
| `--show-conf` | Show configuration and exit |

## Environment Variables

The CLI reads configuration from environment variables prefixed with `HOT_`:

```bash
export HOT_LOG_LEVEL="debug"
export HOT_API_PORT="8080"

# PostgreSQL: a restricted runtime login, plus the schema owner for migrations
export HOT_DB_URI="postgres://hot_runtime:<password>@db.example.com/hot"
export HOT_DB_MIGRATION_URI="postgres://hot_owner:<password>@db.example.com/hot"
```

Give `HOT_DB_MIGRATION_URI` only to migration jobs, not to long-running
services.

See [Configuration](/docs/configuration) for more details.
