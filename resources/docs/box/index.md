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
| `image` | `Str` | Docker/OCI image to run (required). A name without a tag, like `alpine`, means `alpine:latest` |
| `size` | `BoxSize` | Size preset. Without one, the task worker's [box defaults](#without-a-size) apply |
| `script` | `Str` | Shell script to execute (mutually exclusive with `cmd`) |
| `cmd` | `Vec<Str>` | Command and arguments (optional, uses image default) |
| `entrypoint` | `Vec<Str>` | Override image entrypoint (e.g. `[""]` to clear it) |
| `env` | `Map<Str, Str>` | Environment variables (optional) |
| `timeout` | `Int` | Timeout in seconds, from when a task worker picks the task up (overrides size default, max: 86400). Pulling the image and preparing the container count against it |
| `network` | `Str` | `"internet"` (default) or `"none"` |
| `writable` | `Bool` | Writable root filesystem (default: `true`) |
| `tmp-size` | `Int` | `/tmp` tmpfs size in MB (overrides size default) |
| `disk-size` | `Int` | `/data` writable disk in MB (overrides size default) |
| `memory` | `Int` | Container memory in MB (overrides size default) |
| `mounts` | `Vec<BoxMount>` | Project resources to mount into the container, read-only (see [Resource Mounts](#resource-mounts)) |
| `registry-auth` | `BoxRegistryAuth` | Names the context secret that holds a private image's registry credential (see [Private Images](#private-images)) |

`script` runs with `set -e`: it stops at the first failing command. Commands aren't traced, because their expanded arguments can hold secrets; put `set -x` at the top of a script to trace it. Use `script` for multi-line shell commands; use `cmd` for non-shell executables. Only one of `script` or `cmd` may be specified.

A task's `timeout` covers the whole task on the worker, image pull included, so give a large image's first run some room. Pulls and other preparation are never billed; see [Container Billing](/docs/box/billing#what-counts-as-billable-time).

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

### Without a Size

A box that names no `size` doesn't get the `small` preset. It gets the task
worker's box defaults, for each field the box doesn't set itself, and it counts
for billing as the largest preset with no more memory than it has: `medium`, with
these defaults.

| Setting | Environment variable | Default |
|---------|----------------------|---------|
| `task.box-default-memory-mb` | `HOT_TASK_BOX_DEFAULT_MEMORY_MB` | 512 |
| `task.box-default-tmp-mb` | `HOT_TASK_BOX_DEFAULT_TMP_MB` | 500 |
| `task.box-default-disk-mb` | `HOT_TASK_BOX_DEFAULT_DISK_MB` | 5120 |
| `task.box-default-timeout-secs` | `HOT_TASK_BOX_DEFAULT_TIMEOUT_SECS` | 60 |
| `task.box-default-cpu-quota` | `HOT_TASK_BOX_DEFAULT_CPU_QUOTA` | 50000 (50% of a CPU) |

### What a Worker Admits

A task worker holds a memory and a disk budget for its boxes. Each box counts its
memory plus its `/tmp` against the memory budget, and its `/data` plus 2 GB and a
further quarter of `/data` against the disk budget. A box that could never fit
fails at once with `Requested resources exceed worker capacity`. One that only has
to wait for others to finish stays queued.

On Hot's self-hosted AWS and Google Cloud stacks, the installation's size sets the
budgets, and with them the largest preset a host admits. A box with no `size` fits
on all of them.

| Installation size | Single machine | Scalable task host |
|-------------------|----------------|--------------------|
| `small` | `medium` | `large` |
| `medium` | `large` | `xlarge` |
| `large` | `xlarge` | `2xlarge` |

A single machine's sizes set memory aside for one default box in each container
slot. A larger `task.box-default-memory-mb`, a box that asks for more memory and
less `/tmp` than the default, or a new box that starts beside one the task worker
adopted after a restart can still run the VM out of memory, and the host then
kills whichever process it picks. On `small`, which admits a box up to `medium`,
the second can take up to 512 MB more than the plan and the third about 1280 MB;
`medium` has more room. A scalable task host plans the same way, but on `small` its
worst case leaves the host 512 MB rather than 1 GB.

## Network Access

| Value | Description |
|-------|-------------|
| `"internet"` | Outbound internet access via bridge networking (default) |
| `"none"` | No network access |

Network access may be restricted by your plan. See [Container Billing](/docs/box/billing) for plan limits.

On a self-hosted installation with an egress proxy, `"internet"` on the Kata backend means through that proxy only, and the task must set it in `env`. See [Box tasks](/docs/configuration/network#box-tasks).

## Security Model

**Default containers** (writable, with internet):

- **Writable root** — Root filesystem is writable (for `apk add`, `pip install`, etc.)
- **Runs as root** — Process runs as UID 0 with a subset of Linux capabilities
- **Process limit** — Maximum 512 processes on the Kata backend, 100 on the Docker backend (which `hot dev` uses)
- **Image denylist** — Dangerous images (e.g. `docker:*`) are blocked
- **Internet access** — Outbound network access enabled

**Read-only containers** (`writable: false`):

- **Read-only root** — Root filesystem is read-only
- **Capabilities dropped** — All Linux capabilities are dropped
- **Runs as nobody** — Process runs as unprivileged user (UID 65534)
- **Process limit** — Maximum 100 processes on both backends
- **Image denylist** — Same denylist applies

Both modes provide `/data` (disk-backed) and `/tmp` (tmpfs) as writable directories. Use `writable: false` for maximum isolation; use `network: "none"` to disable outbound access.

## File Access

| Path | Type | Description |
|------|------|-------------|
| `/data` | Writable disk | Persistent writable storage (size from `disk-size`) |
| `/tmp` | tmpfs | Ephemeral tmpfs (size from `tmp-size`) |
| `hot://` | Storage | Access Hot storage via built-in file server |

On Linux, a box's `/data` is a file the task worker allocates in full before the
box starts, wherever the filesystem supports that, in `task.data-volume-dir`
(`HOT_TASK_DATA_VOLUME_DIR`, default `/tmp/hot-data-volumes`). The file stays
allocated for the box's lifetime, whatever the box writes. The disk must also have
room for the box's image and writable root, as
[the disk budget counts them](#what-a-worker-admits), so a default 5,120 MB `/data`
needs 8,448 MB free. Only space free to non-root writers counts, not blocks reserved
for root. With less, the box fails:

```text
Requested /data volume could not be provisioned: Data volume I/O error: the disk has N MB available, less than the M MB a box with a K MB /data may use
```

On a tmpfs directory the file isn't allocated: the box uses memory as it writes, and
its writes fail once the tmpfs is full.

A task worker in a container needs that directory at the same path on the host,
with shared mount propagation. [Hot's self-hosted stacks](https://github.com/hot-dev/hot/blob/main/infra/README.md)
set this up, and an installation's `settings` refuses it.

## Resource Mounts

`mounts` puts files from your project's resources (`hot.project.<name>.resources.paths`) into the container. Each `BoxMount` names a `resource` path, relative to a resource root, and its `dest`, an absolute path in the container:

```hot
task ::box/start(BoxConf({
  image: "node:22-alpine",
  cmd: ["node", "/app/index.js"],
  mounts: [BoxMount({resource: "fetcher/index.js", dest: "/app/index.js"})],
}))
```

Mounts are read-only. `mounts` also takes a map of container path to resource path, `{"/app": "fetcher"}`, where a value of `{path: "fetcher", readonly: false}` makes that mount writable. Mounts need the Docker backend for now: a Kata task worker fails the task.

## Image Policy

Hot uses an open policy with a denylist. All images are allowed except those matching denied names or prefixes. Images such as `docker:dind` and `docker:*` are blocked.

Check the current policy with `::hot::box/image-policy()`:

```hot
policy ::hot::box/image-policy()
policy.policy           // "open"
policy.denied            // ["docker:dind", ...]
policy.denied-prefixes   // ["docker:", "rancher/", ...]
```

## Private Images

To run an image from a private registry, keep its credential in a context
secret and name that secret with `registry-auth`. Your code holds only the
secret's name, never the token.

The secret's value is a map with `registry`, `username` and `token`. In a
deployed environment, add it as a context variable of the environment, for
example `ghcr-pull`. Each environment keeps its own credential. In
development, set it in `hot/ctx.hot`, reading the token from your shell
rather than writing it into the file:

```hot
::hot::ctx/set-secret("ghcr-pull", {
  registry: "ghcr.io",
  username: "my-bot",
  token: ::hot::env/get("GHCR_TOKEN"),
})
```

Then start the box with the secret's name:

```hot
task ::box/start(BoxConf({
  image: "ghcr.io/my-org/my-tool:1",
  network: "none",
  registry-auth: BoxRegistryAuth({secret: "ghcr-pull"}),
}))
```

How it works:

- `start` reads the secret from the run's own context, checks that its
  `registry` is the image's registry, and seals the credential for the task
  worker. The task's args, the run's trace and the logs carry only the
  secret's name and its registry.
- `start` refuses a secret that isn't set, isn't a map with `registry`,
  `username` and `token`, or is for a different registry than the image.
- Every start pulls with that task's own credential, so the registry decides
  each time. An image already cached on a worker can't be used by a task
  that lacks a credential the registry accepts.
- `registry-auth` takes only `secret`. `hot check` reports any other shape,
  such as a token written inline, as `invalid-registry-auth`.

The image denylist applies to private images by name, like any other image.

With isolation on, `start` also refuses an image whose registry resolves to an address your code may not reach, with `image registry '<name>' is not reachable`. A registry on a private address, including one behind a private VPC endpoint such as ECR, needs `network.allow-hosts` or `network.allow-cidrs` to cover it, and that entry also opens the address to your code. On a network one of Hot's self-hosted stacks created, an address inside its range stays denied, whatever is allowed, so the endpoint has to be outside that range. See [Image registries](/docs/configuration/network#image-registries).

A pull that fails from a registry other than Docker Hub gives only one message, whatever the cause, including a wrong token: `Could not pull image '<image>'. Check the image name and tag, that its registry is reachable from this worker, and, for a private image, its registry-auth`. The reason is in the worker log. On Docker Hub, the message names the `registry-auth` secret the registry refused.

**Self-hosted Kata workers** pull private images through containerd's
Transfer service, so the token never appears in a process's arguments. That
needs containerd 1.7 or later, with an `unpack_config` entry for the
devmapper snapshotter:

```toml
[plugins."io.containerd.transfer.v1.local"]
  [[plugins."io.containerd.transfer.v1.local".unpack_config]]
    platform = "linux/amd64"
    snapshotter = "devmapper"
```

Without the Transfer service, a worker can't pull with `registry-auth`. From a registry other than Docker Hub, the task shows only the message above, and the reason is in the worker log.

The task worker needs the same `HOT_ENCRYPTION_KEY` as the run worker so it
can open the sealed credential.

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
