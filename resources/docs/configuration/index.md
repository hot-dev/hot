---
description: "Configure a Hot workspace with hot.hot projects, dependencies, profiles, services, and environment-backed settings."
---

# Hot Configuration

Configure your Hot project using the `hot.hot` configuration file.

## Creating a Configuration File

The easiest way to create a `hot.hot` file is with `hot init`:

```bash
hot init
```

This creates a minimal `hot.hot` file with sensible defaults for local development.

To see all available configuration options, use:

```bash
hot conf generate all
```

You can also generate component-specific templates for self-hosting:

```bash
hot conf generate api        # API server config
hot conf generate app        # App server config
hot conf generate worker     # Worker config
hot conf generate scheduler  # Scheduler config
```

## Overview

The `hot.hot` file is the central configuration for your Hot project. It defines:

- **Projects** - Named project configurations with source paths and dependencies
- **Dependencies** - External packages your project uses
- **Settings** - Global defaults like the active project, profile, and remote
- **Services** - Database, Redis, logging, and other infrastructure settings

## Basic Structure

A minimal `hot.hot` file (created by `hot init`) looks like:

```hot
// Hot Configuration File
::hot::conf ns

::env ::hot::env

hot.set.project "my-app"
hot.set.remote "hot-dev"

// Remote API (hot.dev)
hot.remote.hot-dev.url ::env/get("HOT_API_URL", "https://api.hot.dev")
hot.remote.hot-dev.key ::env/get("HOT_API_KEY", "")

// Project Configuration
hot.project.my-app.src.paths ["./hot/src"]
hot.project.my-app.test.paths ["./hot/test"]
hot.project.my-app.resources.paths ["./hot/resources"]
hot.project.my-app.deps {}

// Open the browser to the app when running `hot dev`
hot.dev.open false
```

That's all you need for local development. Database, logging, and other services use sensible defaults.

For production or advanced configuration, add settings as needed:

```hot
// Database Configuration (defaults to local SQLite)
hot.db.uri ::env/get("HOT_DB_URI", "sqlite:./.hot/db/hot.sqlite.db")

// Logging Configuration
hot.log.level ::env/get("HOT_LOG_LEVEL", "info")
hot.log.target ::env/get("HOT_LOG_TARGET", "stdout")

// Dependencies
hot.project.my-app.deps {
  "hot.dev/anthropic": "0.9.0",
  "hot.dev/openai": "0.9.0"
}
```

Run `hot conf generate all` to see all available options.

To run on PostgreSQL, see [Self-Hosted PostgreSQL](/docs/configuration/postgres).
It uses a second URI, `hot.db.migration-uri`, for migrations.

## Profiles

A profile names the user, organization, and environment that CLI commands use
with the local database. Without one, Hot uses the local identity that
`hot init` creates: user `local@hot.dev`, organization `local`, and
environment `development`. This profile selects it explicitly:

```hot
hot.set.profile "local-dev"

hot.profile.local-dev.user.email "local@hot.dev"
hot.profile.local-dev.org.slug "local"
hot.profile.local-dev.env.name "development"
```

Pass `--profile NAME` to select a profile for one command.

`hot init`, and the first command in a fresh checkout, create only the local
identity, and only in an empty database. Any other profile must already exist
in the database: an active user, an organization the user belongs to, and an
existing environment in it. Hot doesn't create them for you. The older
`hot.profile.set` key still works, but it is an error if it names a different
profile than `hot.set.profile`.

## Minimum Version Requirement

Use `hot.min-version` to specify the minimum Hot version required for your project:

```hot
hot.min-version "1.0.0"
```

When set, Hot will check this requirement at startup and display a clear error if the requirement is not met:

```
Version requirement not met: Hot version 1.0.0 is required, but you are running 0.11.0
This project requires Hot 1.0.0 or later.
```

This is useful for:
- **Team coordination** - Ensure all team members are on a compatible version
- **CI/CD** - Fail fast with a clear message before builds
- **Feature requirements** - When your code uses features from a specific Hot version

## Logging Configuration

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.log.level` | Log level: trace, debug, info, warn, error, off | `info` |
| `hot.log.target` | Output target: stdout, file, none | `stdout` |
| `hot.log.format` | Output format: full, simple, json (one object per line) | `full` for services, `simple` for CLI commands |
| `hot.log.dir` | Directory for log files (when target is file) | `.hot/log` |
| `hot.log.rotation` | File rotation: hourly, daily, none | `daily` |
| `hot.log.retention` | Number of log files to keep (0 = keep all) | `7` |

When `log.target` is set to `file`, logs are written to the configured directory with automatic rotation and cleanup based on the retention setting.

## Queue Configuration

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.queue.type` | Queue backend: `sqlite`, `memory`, `redis`, or `none` | `sqlite` in a project |

The default `sqlite` backend is a durable local queue that works across Hot
processes without another service. Hot owns its files, schema, tables, and indexes
under `.hot/db/queue/`; it is separate from `hot.db.uri` and application database
migrations. Use `memory` only when every producer and consumer runs in the same
process, or `redis` for distributed deployments.

SQLite queues are durable across process restarts for standalone Hot services.
`hot dev` intentionally starts a fresh queue session and clears its managed SQLite
queues before launching services, preserving the session-scoped behavior of the
former in-memory default and preventing interrupted local handlers from replaying.
Before clearing queue rows, startup marks associated in-progress Runs and Tasks
failed so they are not left in a misleading `running` state. The reset removes
only messages present at the startup snapshot; a sibling producer that enqueues
after that boundary is preserved.

`hot dev` takes exclusive ownership of the project's SQLite queue session.
Standalone `hot api`, `hot app`, `hot worker`, `hot task-worker`, and
`hot scheduler` processes share a compatible lock with one another, but cannot
run against that queue while `hot dev` owns it. This prevents a dev restart from
clearing a standalone worker's live lease. `hot run` and `hot eval` remain able to
publish into a running dev session.

Integration-mode `hot test` runs all requested services in one process and forces
the memory queue, isolating tests from both `hot dev` and durable project queues.

## Trusted Proxy Client IPs

Client IP forwarding stays in compatibility mode by default: the API uses the
first `X-Forwarded-For` value, then `X-Real-IP`, as before. When either header
is present in compatibility mode, the API logs a warning because clients can
spoof these values when the service is directly reachable.

Self-hosted deployments can opt into validated proxy identity:

```hot
hot.network.client-ip.trusted-proxy true
hot.network.client-ip.header "x-forwarded-for"
hot.network.client-ip.trusted-proxies ["10.0.0.0/8", "2001:db8:1234::/48"]
```

List every proxy CIDR that may connect directly to the API or appear at the
trusted end of the forwarding chain. When enabled, startup fails if the list is
empty or invalid. Forwarding headers from untrusted peers, or malformed header
values, are ignored in favor of the direct peer address.

`trusted-proxies` also accepts a comma-delimited string, which is useful for an
environment-backed deployment setting.

## Secrets from Files

Settings supplied through `HOT_*` environment variables can instead name a
file that holds the value: set `<NAME>_FILE`, for example
`HOT_DB_URI_FILE=/run/secrets/db-uri`. This covers the configuration overlay,
values `hot.hot` reads with `::hot::env/get("HOT_...")` (such as the API key
and Stripe settings in the generated templates), `HOT_ENCRYPTION_KEY`,
`HOT_APP_SESSION_SECRET`, and the GitHub and Google OAuth client settings. Hot
drops one trailing newline and does not copy the value into the process
environment. `HOT_ENCRYPTION_KEY` and `HOT_APP_SESSION_SECRET` are read once,
when first used, and kept: rotating either takes a restart.

Setting both `<NAME>` and `<NAME>_FILE`, or naming an empty or unreadable
file, stops the command with an error that names the variable and path but
not the value. For a `HOT_*_FILE` name that is not a Hot setting, Hot prints a
warning and ignores it. `hot conf` and `--show-conf` print such values as
`***` and list their source files under `_secret-files`.

Hyphenated settings map from their environment names too:
`HOT_WORKER_RUN_TIMEOUT` sets `worker.run-timeout`, and a variable whose
dotted spelling isn't a setting sets the hyphenated key `hot.hot` already has.

## Configuration Format

Hot configuration uses a dotted notation where each setting is a separate assignment:

```hot
// Setting a simple value
hot.log.level "info"

// Setting from environment with default
hot.api.port Int(::env/get("HOT_API_PORT", "4681"))

// Setting a list
hot.project.my-app.src.paths ["./hot/src", "./lib"]

// Setting a map (for dependencies)
hot.project.my-app.deps {
  "hot.dev/anthropic": "0.9.0"
}
```

## Sections

- **[Dependencies](/docs/configuration/dependencies)** - How to declare and manage package dependencies
- **[Projects](/docs/configuration/projects)** - Configuring multiple projects in one workspace
- **[Storage](/docs/configuration/storage)** - Where Hot stores files, blobs, and build artifacts
- **[Self-Hosted PostgreSQL](/docs/configuration/postgres)** - Running Hot on PostgreSQL with row-level security
