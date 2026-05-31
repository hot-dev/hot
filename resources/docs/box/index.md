---
description: "Use Hot Box containers to run external tools, custom runtimes, binaries, and isolated task workloads from Hot code."
---

# Containers (Hot Box)

Hot Box lets you run Docker/OCI containers from Hot code. Containers execute asynchronously as tasks, returning immediately with a `TaskInfo` while the container runs in the background.

## Overview

Use `::hot::box/start` to run arbitrary container images—Python scripts, Node.js tools, system binaries, or any language. Containers are isolated, resource-limited, and billed by Compute Unit Seconds (CUS). See [Container Billing](/docs/box/billing) for CUS details.

## BoxConf

Configure a container with `BoxConf`:

| Field | Type | Description |
|-------|------|-------------|
| `image` | `Str` | Docker/OCI image to run (required) |
| `size` | `BoxSize` | Size preset (default: `"small"`) |
| `script` | `Str` | Shell script to execute (mutually exclusive with `cmd`) |
| `cmd` | `Vec<Str>` | Command and arguments (optional, uses image default) |
| `entrypoint` | `Vec<Str>` | Override image entrypoint (e.g. `[""]` to clear it) |
| `env` | `Map<Str, Str>` | Environment variables (optional) |
| `timeout` | `Int` | Timeout in seconds (overrides size default, max: 86400) |
| `network` | `Str` | `"internet"` (default) or `"none"` |
| `writable` | `Bool` | Writable root filesystem (default: `true`) |
| `tmp-size` | `Int` | `/tmp` tmpfs size in MB (overrides size default) |
| `disk-size` | `Int` | `/data` writable disk in MB (overrides size default) |
| `memory` | `Int` | Container memory in MB (overrides size default) |

`script` runs with `set -ex` (trace commands, exit on error). Use `script` for multi-line shell commands; use `cmd` for non-shell executables. Only one of `script` or `cmd` may be specified.

## BoxSize Presets

| Size | Memory | CPU | Tmp | Disk | Timeout | CUS Multiplier |
|------|--------|-----|-----|------|---------|----------------|
| `nano` | 64 MB | 10% | 32 MB | 256 MB | 60s | 0.25x |
| `micro` | 128 MB | 25% | 64 MB | 512 MB | 60s | 0.5x |
| `small` | 256 MB | 25% | 128 MB | 1 GB | 60s | 1.0x |
| `medium` | 512 MB | 50% | 256 MB | 5 GB | 300s | 2.0x |
| `large` | 1 GB | 75% | 500 MB | 10 GB | 600s | 4.0x |
| `xlarge` | 2 GB | 100% | 1 GB | 20 GB | 1800s | 8.0x |
| `2xlarge` | 4 GB | 100% | 2 GB | 50 GB | 3600s | 16.0x |
| `4xlarge` | 8 GB | 100% | 4 GB | 50 GB | 7200s | 32.0x |

## Network Access

| Value | Description |
|-------|-------------|
| `"internet"` | Outbound internet access via bridge networking (default) |
| `"none"` | No network access |

Network access may be restricted by your plan. See [Container Billing](/docs/box/billing) for plan limits.

## Security Model

**Default containers** (writable, with internet):

- **Writable root** — Root filesystem is writable (for `apk add`, `pip install`, etc.)
- **Runs as root** — Process runs as UID 0 with a subset of Linux capabilities
- **Process limit** — Maximum 512 processes
- **Image denylist** — Dangerous images (e.g. `docker:*`) are blocked
- **Internet access** — Outbound network access enabled

**Read-only containers** (`writable: false`):

- **Read-only root** — Root filesystem is read-only
- **Capabilities dropped** — All Linux capabilities are dropped
- **Runs as nobody** — Process runs as unprivileged user (UID 65534)
- **Process limit** — Maximum 100 processes
- **Image denylist** — Same denylist applies

Both modes provide `/data` (disk-backed) and `/tmp` (tmpfs) as writable directories. Use `writable: false` for maximum isolation; use `network: "none"` to disable outbound access.

## File Access

| Path | Type | Description |
|------|------|-------------|
| `/data` | Writable disk | Persistent writable storage (size from `disk-size`) |
| `/tmp` | tmpfs | Ephemeral tmpfs (size from `tmp-size`) |
| `hot://` | Storage | Access Hot storage via built-in file server |

## Image Policy

Hot uses an open policy with a denylist. All images are allowed except those matching denied names or prefixes. Images such as `docker:dind` and `docker:*` are blocked.

Check the current policy with `::hot::box/image-policy()`:

```hot
policy ::hot::box/image-policy()
policy.policy           // "open"
policy.denied            // ["docker:dind", ...]
policy.denied-prefixes   // ["docker:", "rancher/", ...]
```

## Example

```hot
::box ::hot::box

task ::box/start(BoxConf({
  image: "python:3.13-alpine",
  cmd: ["python", "-c", "print('Hello')"],
  size: "nano"
}))

// task.id — the task ID
// task.stream-id — the stream ID
```

Using `script` for multi-line shell commands:

```hot
task ::box/start(BoxConf({
  image: "alpine:latest",
  script: """
    echo 'hello from hot box' > /data/test.txt
    hotbox cp /data/test.txt hot://output/test.txt
  """,
  size: "nano",
}))
```

With environment variables and custom limits:

```hot
task ::box/start(BoxConf({
  image: "node:22-alpine",
  cmd: ["node", "-e", "console.log(process.env.NAME)"],
  env: {NAME: "Hot"},
  size: "medium",
}))
```

## Local Development

Running `::hot::box/start` locally requires **Docker** (Docker Desktop or Docker Engine). The `hot dev` command starts a task worker that uses Docker to run containers. If Docker is not installed or not running, `hot dev` will log a warning at startup, and any `::hot::box/start` calls will fail.

```bash
# Ensure Docker is running, then:
hot dev
```

To disable Hot Box (e.g. if you don't need containers), set `hot.box.enabled` to `false` in your project config or via environment variable:

```bash
HOT_BOX_ENABLED=false hot dev
```

## Useful Functions

| Function | Description |
|----------|-------------|
| `::hot::box/start(BoxConf)` | Start a container task, returns `TaskInfo` |
| `::hot::box/sizes()` | Get all size presets with resource profiles |
| `::hot::box/quota()` | Check remaining CUS and task quota |
| `::hot::box/enabled()` | Check if box is enabled in configuration |
| `::hot::box/image-policy()` | Get image denylist and policy |
