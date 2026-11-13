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

A deployment sets `HOT_ENV=production`, and the app also needs
`HOT_APP_SESSION_SECRET` and the
[`self-host` experience](/docs/configuration/accounts#product-experience). With
`HOT_ENV` unset, Hot runs in local development mode: the app signs every visitor
in on a loopback address, and on a network address it signs nobody in by itself
and refuses to start without a session secret.

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

With `log.format` `json` on stdout, output from `print`, `println`, `eprint` and
`eprintln` in Hot code is logged too, so every line stays one JSON object. Each
call is one record with target `hot::user_output` and a `stream` field
(`stdout` or `stderr`), shown at any log level.

When `log.target` is set to `file`, logs are written to the configured directory with automatic rotation and cleanup based on the retention setting.

The Docker client library logs at `info` at most, even at `debug` or `trace`: its debug output includes each container's environment, and with it the secrets passed to Hot Box containers.

## Queue Configuration

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.queue.type` | Queue backend: `sqlite`, `memory`, `redis`, or `none` | `sqlite` in a project |
| `hot.queue.event-orphan-idle-ms` | Milliseconds an event may sit with a worker that stopped responding before another worker takes it over | `60000` |
| `hot.queue.task-orphan-idle-ms` | The same for a task and its task worker. With Redis it must be at least the task lease, or the task worker refuses to start | `120000` |
| `hot.queue.infra-retry-backoff-ms` | Milliseconds a worker or task worker waits before it retries work that failed on the database or the queue | `1000` |
| `hot.queue.metrics-enabled` | Record queue wait times | `true` |
| `hot.queue.wait-target-p99-ms` | The 99th-percentile queue wait those metrics are measured against, in milliseconds | `1000` |

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

### Running Several Replicas

With `redis`, several `hot api`, `hot app`, `hot worker`, and `hot task-worker`
processes can share one deployment. Where their state lives:

- **Redis:** stream pub/sub (`stream:data`, run events, dashboard updates, task
  messages) and MCP HTTP+SSE transport sessions, so any API replica can serve
  any subscription. `hot api` and `hot app` PING Redis at startup, up to ten
  times one second apart, and refuse to start if it never answers. A subscription that
  loses its Redis connection reconnects with backoff and resumes after the last
  entry it delivered. Each stream keeps about its last 1,000 entries, so a
  subscriber further behind than that, or reconnecting after that many more were
  published, skips the oldest. A Redis restart without persistence loses them;
  run events are also recovered from the database, but `stream:data` is not
  stored anywhere else.
- **Database:** the per-account request rate window, shared by every replica.
- **Each process:** in-flight request and SSE connection limits (with N API
  replicas an account can hold N times each limit, so size them per replica),
  and 60-second caches of environment ownership, features, and custom-domain
  lookups. A domain lookup that misses is cached too, so a newly verified
  domain can take up to a minute to resolve on every replica.
- **Configuration:** every `hot app` replica needs the same
  `HOT_APP_SESSION_SECRET`. Sessions and the one-time API key handoff are
  signed with it, so a request can land on any replica.

## Redis Configuration

Redis is needed only when `hot.queue.type` is `redis`.

| Setting | Environment variable | Description | Default |
|---------|----------------------|-------------|---------|
| `hot.redis.uri` | `HOT_REDIS_URI` | Redis connection URI. `rediss://` connects over TLS. Also `--redis.uri` on the service commands | unset |
| `hot.redis.cluster` | `HOT_REDIS_CLUSTER` | Connect in cluster mode. Set it for a cluster behind one endpoint. A URI that lists several endpoints, separated by commas, is taken as a cluster without it | `false` |
| `hot.redis.ca-cert` | `HOT_REDIS_CA_CERT` | PEM certificates to trust for a `rediss://` server instead of the system's roots. Standalone servers only | unset |
| `hot.redis.auth` | `HOT_REDIS_AUTH` | `password` uses the credentials in the URI. `gcp-iam` signs in as the VM's service account, for Memorystore for Valkey. It needs a `rediss://` URI and a standalone server | `password` |

`hot.redis.ca-cert` holds the certificates themselves, not a path. To read them
from a file, set `HOT_REDIS_CA_CERT_FILE` (see
[Secrets from Files](#secrets-from-files)). The templates that
`hot conf generate` prints set `hot.redis.uri` to `redis://localhost:6379`
unless `HOT_REDIS_URI` is set.

## Service Tuning

`hot conf generate all` prints the full template: the service keys, each with
its default and the environment variable that sets it. The tables below
describe the keys an operator is most likely to tune. An environment variable
is named from its key: `HOT_WORKER_RUN_TIMEOUT` sets `hot.worker.run-timeout`.

### Worker

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.worker.threads` | Worker threads, and the most runs at once | `2` |
| `hot.worker.run-timeout` | Seconds a run may take when its function sets no `timeout` | `300` |
| `hot.worker.max-run-timeout` | Cap, in seconds, on a function's own `timeout`. At most 86400 | `3600` |
| `hot.worker.cancel-on-timeout` | Ask a run to stop when it times out | `true` |
| `hot.worker.shutdown-timeout` | Seconds to wait for runs at shutdown. `hot dev` uses 1 unless you set it | `60` |
| `hot.worker.vm-concurrency` | Runs executing at once. It is capped by `worker.threads` and, under a container memory limit, by the memory figures below. `auto` also caps it at the CPU count | `auto` |
| `hot.worker.vm-memory-mb` | Memory counted for each run in that calculation | `256` |
| `hot.worker.reserved-memory-mb` | Memory held back from that calculation | `512` |
| `hot.worker.shared-process` | Count half the CPUs for `auto`, for a worker that shares them with other services | `false` |
| `hot.worker.max-blocking-executions` | Cap on executions that hold a thread. `-1` is twice the run concurrency | `-1` |
| `hot.worker.db-max-connections` | Ceiling on the PostgreSQL connection pool. `auto` is 50 | `auto` |
| `hot.worker.db-reserved-connections` | Connections added to the PostgreSQL pool on top of the run and code-task concurrency | `4` |
| `hot.worker.local-write-concurrency` | Write headroom added to the SQLite connection pool | `1` |
| `hot.worker.read-batch-size` | Events read from the queue in one round trip | `8` |
| `hot.worker.tenant-service-cache-max-entries` | Most entries in the worker's cache of stores and context values. `0` turns it off | `1024` |
| `hot.worker.event-ordering` | Only `current` is implemented | `current` |
| `hot.worker.handler-concurrency` | Only `serial` is implemented | `serial` |

### Task Worker

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.task.code-max-concurrent` | Most code tasks at once. The CPU count and the memory budget can lower it | `500` |
| `hot.task.code-vm-memory-mb` | Memory counted for each code task | `256` |
| `hot.task.code-memory-percent` | Share of the task worker's usable memory for code tasks. Box tasks get the rest | `50` |
| `hot.task.container-max-concurrent` | Most box tasks at once. The memory and disk budgets can lower it. `auto` uses `hot.task.max-concurrent` | `auto` |
| `hot.task.max-concurrent` | Box tasks at once when `container-max-concurrent` is `auto` | `4` |
| `hot.task.worker-memory-mb` | Memory the task worker may budget. A lower container memory limit wins | `8192` |
| `hot.task.worker-disk-mb` | Disk the task worker may budget for box tasks | `51200` |
| `hot.task.recovery-reserved-slots` | Slots held back to adopt containers still running after a restart | `1` |
| `hot.task.container-reserved-memory-mb` | Memory held back for each of those slots | `512` |
| `hot.task.container-reserved-disk-mb` | Disk held back for each of those slots | `10240` |
| `hot.task.shutdown-drain-seconds` | Seconds running tasks get to finish at shutdown before they are cancelled. `hot dev` uses 5 unless you set it | `30` |
| `hot.task.shutdown-container-timeout-seconds` | Seconds a box task waits for a free container slot | `30` |
| `hot.task.reconcile-queued-after-seconds` | A task still queued in the database after this long is put on the queue again | `60` |
| `hot.task.reconcile-interval-seconds` | How often that check runs | `30` |
| `hot.task.capacity-fairness` | Only `none` is implemented | `none` |
| `hot.box.backend` | What runs box tasks: `docker` or `kata`. `kata` needs Linux and a `hot` built with the Kata backend | `docker` |
| `hot.box.containerd.socket` | The containerd socket of the Kata backend. Docker ignores it | `/run/kata-containerd/containerd.sock` |
| `hot.box.vmm` | The virtual machine monitor of the Kata backend: `qemu` or `firecracker` | `qemu` |

### Scheduler and Retries

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.scheduler.backfill` | On restart, fire the scheduled events missed while the scheduler was down | `false` |
| `hot.scheduler.sync-interval-seconds` | How often to sync schedules from the database | `30` |
| `hot.scheduler.retry-interval-seconds` | How often to poll for due retries | `1` |
| `hot.scheduler.at-interval-seconds` | How often to poll for due one-shot schedules | `1` |
| `hot.scheduler.singleton` | Allow one scheduler at a time on PostgreSQL (see [hot scheduler](/docs/cli#hot-scheduler)) | `true` |
| `hot.retry.max-attempts` | Most attempts a function's `retry` may ask for. A higher value is lowered to this | `10` |
| `hot.retry.max-delay-ms` | Longest retry delay a function may ask for | `3600000` |
| `hot.retry.default-delay-ms` | Retry delay when a function gives none | `1000` |

### API and App

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.api.host` | Address the API server binds to. Also `--api.host` | `localhost` |
| `hot.api.port` | Port the API server listens on. Also `--api.port` | `4681` |
| `hot.app.host` | Address the app server binds to. Also `--app.host`. On any address but a local one, the app turns off its local development sign-in | `localhost` |
| `hot.app.port` | Port the app server listens on. Also `--app.port` | `4680` |

### Running Code

Every process that runs Hot code reads these: the CLI, the worker and the
task worker.

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.engine.threads` | Threads the engine runs Hot code on. Also `--engine.threads` | `4` |
| `hot.jit.mode` | `enabled` compiles often-called functions to native code. `disabled`, `off`, `false` or `0` turns that off. Also `--jit.mode` and `HOT_JIT` | `enabled` on supported platforms |
| `hot.jit.threshold` | Calls before a function is compiled. Also `--jit.threshold` | `100` |
| `hot.jit.hof.fusion` | Compile a pipeline of `map`, `filter` and the like as one loop. Also `--jit.hof.fusion` | `true` |
| `hot.emitter.type` | Where calls and runs are recorded: `db`, `console` or `none`. Also `--emitter.type` | `db` in a project, `none` outside one |
| `hot.value.format` | How `hot run`, `hot eval` and `hot repl` print values: `hot` or `json`. Also `--value.format` | `hot` |
| `hot.blob.mode` | `service` moves large strings and bytes out of recorded calls, runs and events into blob storage, and records a reference. `disabled` records them whole | `service` in a project, `disabled` outside one |
| `hot.blob.spill.threshold-bytes` | A value larger than this is moved to blob storage | `16384` in local development, `65536` when deployed |
| `hot.blob.preview.max-bytes` | Bytes of a moved value kept beside its reference as a preview | `256` |

### Command Defaults

These set what a CLI command does when you don't pass the flag.

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.check.format` | Output of `hot check` and `hot watch`: `pretty`, `simple`, `json` or `json-min`. Also `--check.format` | `pretty` |
| `hot.check.raw` | Leave out the "No issues found" line. Also `--check.raw` | `false` |
| `hot.check.deny-warnings` | Exit non-zero on warnings too. Also `--check.deny-warnings` | `false` |
| `hot.check.with-tests` | Check test files as well as sources, in `hot check` and `hot watch`. Also `--with-tests` | `true` |
| `hot.watch.with-tests` | Make `hot watch` watch the test directories too, so a change to a test file starts a check. Also `--with-tests` | `false` |
| `hot.watch.debounce` | Milliseconds `hot watch` waits after a change. Also `--watch.debounce` | `200` |
| `hot.deploy.auto` | Deploy each live build that `hot dev` creates. Also `--deploy.auto` | `true` |
| `hot.deploy.ctx.strict` | Make `hot deploy --local` stop when a required context variable is unset. Also `--strict` | `false` |
| `hot.build.file.max-bytes` | Largest build the API accepts as an upload, in bytes. `hot build` and `hot deploy` check the bundle against it first | `104857600` |

### Database, App and Files

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.db.statement-timeout-ms` | PostgreSQL statement timeout | `30000` |
| `hot.db.lock-timeout-ms` | PostgreSQL lock timeout | `5000` |
| `hot.db.local.retention.call.days` | In local development, the worker deletes call data older than this many days. `-1` keeps it | `7` |
| `hot.db.sqlite.cache-size-kb` | SQLite page cache, in KiB | `65536` |
| `hot.db.sqlite.mmap-size-bytes` | Bytes of the database file to memory-map for reads. `0` turns it off | `268435456` |
| `hot.db.sqlite.wal-autocheckpoint-pages` | Checkpoint after this many WAL pages | `4000` |
| `hot.db.sqlite.journal-size-limit-kb` | Soft cap on the WAL file after a checkpoint, in KiB | `65536` |
| `hot.db.sqlite.synchronous` | `normal`, `full` or `off` | `normal` |
| `hot.db.sqlite.temp-store` | `memory`, `file` or `default` | `memory` |
| `hot.db.sqlite.auto-vacuum` | `incremental` or `none`. An existing database may need one full `VACUUM` for a change to apply | `incremental` |
| `hot.db.sqlite.maintenance.wal-checkpoint` | Truncate the WAL in daily maintenance | `true` |
| `hot.db.sqlite.maintenance.incremental-vacuum-pages` | Pages to reclaim in each maintenance run. `0` skips it | `1000` |
| `hot.app.session.timeout` | Hours before an app sign-in expires. The templates that `hot conf generate` prints write `24` | `720` |
| `hot.file.max-bytes` | Ceiling on the size of a stored file, in bytes. The lower of this and the plan's limit applies. `-1` leaves it to the plan | `-1` |

`hot.db.sqlite.*` and `hot.app.session.timeout` take their environment
variables only through the `::env/get` lines of the generated template, so keep
those lines in `hot.hot` to set them from the environment.

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
trusted end of the forwarding chain. An entry may also be a bare IP address,
read as a single host. When enabled, startup fails if the list is empty or
invalid. Forwarding headers from untrusted peers, or malformed header values,
are ignored in favor of the direct peer address.

`trusted-proxies` also accepts a comma-delimited string, which is useful for an
environment-backed deployment setting. The environment variables are
`HOT_NETWORK_CLIENT_IP_TRUSTED_PROXY`, `HOT_NETWORK_CLIENT_IP_HEADER`, and
`HOT_NETWORK_CLIENT_IP_TRUSTED_PROXIES`. A `hot.hot` generated by an earlier
release reads `HOT_CLIENT_IP_*` instead; it keeps working as written.

## API Hostnames

The API takes a request's host for a [custom domain](/docs/domains) unless it
knows the name as its own, and answers a name it cannot find with 404 on every
route, `/status` included. It knows `localhost`, IP addresses, and the host of
`HOT_API_URL`. If clients reach the API on other names, list them in
`HOT_API_KNOWN_HOSTS`, separated by commas:

```bash
HOT_API_URL=https://api.example.com
HOT_API_KNOWN_HOSTS=api.internal.example.com
```

[Hot's self-hosted stacks](https://github.com/hot-dev/hot/blob/main/infra/README.md) set `HOT_API_KNOWN_HOSTS` to
the installation's API hostname, and its `settings` refuses it.

## Secrets from Files

Settings supplied through `HOT_*` environment variables can instead name a
file that holds the value: set `<NAME>_FILE`, for example
`HOT_DB_URI_FILE=/run/secrets/db-uri`. This covers the configuration overlay,
values `hot.hot` reads with `::hot::env/get("HOT_...")` (such as the API key
and Stripe settings in the generated templates), `HOT_ENCRYPTION_KEY`,
`HOT_APP_SESSION_SECRET`, the GitHub and Google OAuth client settings, and
`HOT_OAUTH_REDIRECT_BASE_URL`. Hot drops one trailing newline and does not copy
the value into the process environment. `HOT_ENCRYPTION_KEY` and
`HOT_APP_SESSION_SECRET` are kept once found: rotating either takes a restart.

Setting both `<NAME>` and `<NAME>_FILE`, or naming an empty or unreadable
file, stops the command with an error that names the variable and path but
not the value. For a `HOT_*_FILE` name that is not a Hot setting, Hot prints a
warning and ignores it. `hot conf` and `--show-conf` print such values as
`***` and list their source files under `_secret-files`.
`HOT_MOVE_STORAGE_SKIP_FILE` is the one exception: only `hot db move-storage` reads
it, as [a list of ids](/docs/configuration/storage#after-a-move) that adds to
`HOT_MOVE_STORAGE_SKIP`.

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
- **[Email](/docs/configuration/email)** - Sending verification, invitation, and alert emails through SMTP
- **[Accounts and Sign-Up](/docs/configuration/accounts)** - The first administrator, sign-up policy, and password resets
- **[Outbound Network Access](/docs/configuration/network)** - Which destinations deployed code can reach
- **[Self-Hosted PostgreSQL](/docs/configuration/postgres)** - Running Hot on PostgreSQL with row-level security

## Worker resource budgets

Hot values can share collection storage. Serializing a shared value copies each
occurrence, so a small value in memory can expand into an enormous payload.
Expansion checks apply when serializing, formatting, hashing collection keys, or
sending payloads outside the VM. They do not limit `length`, `get`, or ordinary
collection updates. Cancellation is sampled every 1,024 interpreter steps or JIT
tail iterations; native I/O also observes the run deadline and cancellation.

In-memory operations (formatting, hashing, map keys, JSON and XML output, run
results) accept values nested up to 4,096 levels. Values persisted in records,
events, and task, schedule, alert, checkpoint, stream, and store payloads are
limited to 120 levels. A run record holds an `Omitted` marker for a deeper result.
YAML frontmatter accepts anchors and aliases; aliases add at most 1 Mi nodes.

Operators can set these environment variables before starting workers. Positive
integer overrides raise or lower the budget; invalid or zero values use the
default. Raise memory budgets together with worker memory and concurrency sizing.

| Environment variable | Default | Resource measured |
| --- | --- | --- |
| `HOT_MAX_EXPANDED_VALUE_NODES` | Twice `HOT_MAX_COLLECTION_SIZE`, plus one | Expanded value occurrences at an output boundary |
| `HOT_MAX_EXPANDED_VALUE_BYTES` | `HOT_MAX_ALLOC_BYTES` (256 MiB) | Str and Bytes payload, plus 8 bytes for every other value occurrence |
| `HOT_MAX_STRING_BYTES` | 256 MiB | Produced text or checked serialized output |
| `HOT_MAX_RECURSION_DEPTH` | 4,096 | Function calls open at once in one run; a tail call doesn't count |
| `HOT_MAX_PARALLEL_DEPTH` | 64 | `parallel` flows open inside one another; each branch runs on a thread of its own |
| `HOT_SQLITE_MAX_QUERY_ROWS` | `HOT_MAX_COLLECTION_SIZE` (16 Mi) | Rows materialized by one query |
| `HOT_SQLITE_MAX_QUERY_CELLS` | `HOT_MAX_COLLECTION_SIZE` (16 Mi) | Cells materialized by one query |
| `HOT_SQLITE_MAX_QUERY_BYTES` | `HOT_MAX_ALLOC_BYTES` (256 MiB) | Query result storage and individual SQLite values |
| `HOT_XML_MAX_NODES`, `HOT_YAML_MAX_NODES` | Expanded-value node budget | Parsed events, nodes, and attributes; YAML alias expansion counts too |
| `HOT_XML_MAX_DEPTH`, `HOT_YAML_MAX_DEPTH` | 256 | Parser nesting; raising it needs stack-budget validation, especially for YAML |
| `HOT_NATIVE_IO_TIMEOUT_SECONDS` | Unset (no limit) | Native I/O (sockets, HTTP, WebSocket, SQLite) outside a run deadline; runs use their remaining deadline, and cancellation always applies |
| `HOT_BOX_MAX_IMAGE_BYTES` | 4 GiB | Compressed manifest and layer sizes per image |
| `HOT_BOX_MAX_CACHED_IMAGES` | 128 | Cached image count before eviction; active images are retained |

### Memory allocator

`hot` allocates with [mimalloc](https://github.com/microsoft/mimalloc). It is
faster than the system allocator for Hot's many small values, and it holds
more memory: about 40 MB more in each `hot` process, and a higher peak under
load. To run a process on the system allocator, set `HOT_ALLOCATOR=system` in
its environment before it starts:

```bash
HOT_ALLOCATOR=system hot worker
```

Use it where a service has to fit a tight memory limit, or to run glibc's heap
checks (`MALLOC_CHECK_`), which only see what glibc allocates. A worker limits
glibc to four malloc arenas (`MALLOC_ARENA_MAX` overrides that); the limit has
no effect under mimalloc. A `hot` built with `--no-default-features` has only
the system allocator.

Persisted values retain the existing record nesting limit so every fleet reader
can safely consume them. Trace records also retain their inline threshold and
blob offload policy. Other outgoing and persisted payloads use the configurable
expansion budgets above; the trace inline threshold is not a blanket payload
ceiling. An operator override does not change the record nesting format. Parser
input continues to use `HOT_MAX_PARSE_INPUT_BYTES`.

Run notification streams expire after the configured `worker.max-run-timeout`
plus one hour, with a two-hour default. Task inboxes retain their eight-day idle
window to cover seven-day tasks. Every publish renews expiry atomically. For old
persistent keys, `scripts/expire-notification-streams.sh` previews a TTL backfill;
pass `--apply` to perform it. Set `HOT_RUN_STREAM_IDLE_TTL_SECONDS` for that script
when workers allow runs longer than one hour. This affects notifications;
durable run and task results remain in the database.
