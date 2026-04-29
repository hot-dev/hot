<p align="left">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="resources/brand/hot-dev-logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="resources/brand/hot-dev-logo-light.png">
    <img src="resources/brand/hot-dev-logo-light.png" alt="hot.dev" width="360">
  </picture>
</p>

<p align="left"><strong>Backend Workflows for the AI Age</strong></p>

# Hot Dev

Hot Dev is an open source platform for backend workflows — events, schedules,
AI agents, MCP tools, long-running tasks, and service orchestration — with
built-in execution tracing, a local dev runtime, and one-command deploys.

Hot is the language and runtime at its core. This repo contains the Hot
compiler, VM, and standard library, plus the platform components built on top:
the `hot` CLI, API, web app, scheduler, event worker, task worker, and LSP
server.

Hot Dev Cloud is the hosted offering; its deployment infrastructure and private
operational tooling live outside this repository.

- Website: [hot.dev](https://hot.dev)
- Download: [hot.dev/download](https://hot.dev/download)
- Documentation: [hot.dev/docs](https://hot.dev/docs)
- License: [Apache-2.0](LICENSE)

## What Is Included

- Hot compiler, runtime, and standard library.
- Platform services and developer tools: the `hot` CLI, API, web app/dashboard,
  event worker, scheduler, task worker, `hotbox`, and LSP server.
- Public Hot packages under `hot/pkg`, including `hot-std` and provider/tool
  integrations.
- Public documentation, package documentation generation, installer resources,
  and release packaging scripts.

## Install

Install the latest released CLI:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh
```

Or build from source:

```bash
cargo build --release --bin hot
```

## Quick Start

```bash
hot init
hot dev --open    # start the local dev stack and open the dashboard
```

Common commands:

```bash
hot run file.hot          # Run one Hot file
hot check                 # Type/check project source
hot test                  # Run Hot tests
hot ai add                # Add AGENTS.md + Hot language skill for AI tools
```

## Repository Layout

```text
crates/
  hot/                    # Core language, runtime, storage, packages, docs
  hot_cli/                # CLI binary
  hot_api/                # API service
  hot_app/                # App/Dashboard
  hot_worker/             # Event worker
  hot_task_worker/        # Long-running task and container worker
  hot_scheduler/          # Scheduled function runner
  hot_lsp/                # LSP server
  hot_docs/               # Documentation and package rendering
  hotbox/                 # Helper binary for containerized file access
hot/
  pkg/                    # Public Hot packages
  test/                   # Hot language/package tests
resources/                # Docs, app assets, migrations, init templates
scripts/                  # Build, package, docs, and integration helpers
docker/                   # Public release build/package Dockerfiles
```

## Development

Prerequisites:

- Rust (version pinned in `rust-toolchain.toml`)
- Docker, optional for most development but required for `::hot::box` container
  tasks and release packaging.

Common checks:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo run --locked --bin hot -- check --ctx hot/ci.ctx.hot
cargo run --locked --bin hot -- test -c hot.test.hot --ctx hot/ci.ctx.hot
```

Run the local development stack:

```bash
cargo run --locked --bin hot -- dev
```

Install optional git hooks:

```bash
./scripts/setup-git-hooks.sh
```

## Packages

Public packages live under `hot/pkg`. The publish allowlist is
`hot/pkg/pkg-publish.txt`; only listed packages are included in public package
publishing flows.

Package documentation JSON can be regenerated from checked-in packages with:

```bash
./scripts/pkg-docs.sh
```

## Releases

Public release automation lives in `.github/workflows/release.yml`. Releases are
versioned from `resources/version.txt`; tagged releases should use immutable
`vX.Y.Z` tags that match that file.

## License

Copyright 2025-2026 Hot Dev, LLC.

Hot is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE)
and [NOTICE](NOTICE).

Brand usage for the Hot Dev name and artwork is covered separately in
[BRAND.md](BRAND.md).
