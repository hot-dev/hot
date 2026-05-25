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

### hot scheduler

Run just the job scheduler:

```bash
hot scheduler
```

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
hot eval '1 + 2'
hot eval '::my-app::hello()'
hot eval 'send("user:created", {id: 123})'
hot eval '[1, 2, 3] | map(x => x * 2)'
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

Tests are functions with `meta {test: true}`:

```hot
should-add-numbers meta {test: true}
fn () {
  assert-eq(1 + 1, 2)
}
```

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
hot ai add              # Add AGENTS.md + .skills/hot-language/ to project
hot ai add --global     # Install skills to ~/.skills/ (available in all projects)
```

`hot ai add` installs the Hot language skill snapshot bundled with your Hot
release, so it works offline and does not require Node or GitHub access. To
install the latest public skill from the skills.sh ecosystem instead, use:

```bash
npx skills add hot-dev/hot-skills
```

Files created:
- `AGENTS.md` — AI agent instructions (passive context)
- `.skills/hot-language/` — Detailed Hot language skill with references

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
```

### hot update

Update Hot to the latest version:

```bash
hot update
```

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
| `-s, --src.path <DIR>` | Source directory |
| `-t, --test.path <DIR>` | Test directory |
| `--engine.threads <N>` | Engine threads (default: 4) |
| `--db.uri <URI>` | Database connection URI |
| `--log.level <LEVEL>` | Log level: off, trace, debug, error, warn, info |
| `--log.target <TARGET>` | Log output: stdout, file, none |
| `--log.dir <DIR>` | Directory for log files |
| `--log.rotation <ROTATION>` | Log rotation: hourly, daily, none (default: daily) |
| `--log.retention <COUNT>` | Number of log files to keep, 0 = keep all (default: 7) |
| `--deploy.auto <BOOL>` | Auto-deploy on CLI commands (default: true) |
| `--emitter.type <TYPE>` | Emitter: none, console, db (default: db in project, none otherwise) |
| `--with-tests <BOOL>` | Include test files in compile/check/watch (default: false) |
| `--show-conf` | Show configuration and exit |

## Environment Variables

The CLI reads configuration from environment variables prefixed with `HOT_`:

```bash
export HOT_DB_URI="postgres://localhost/hot"
export HOT_LOG_LEVEL="debug"
export HOT_API_PORT="8080"
```

See [Configuration](/docs/configuration) for more details.
