<p align="left">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="resources/brand/hot-dev-logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="resources/brand/hot-dev-logo-light.png">
    <img src="resources/brand/hot-dev-logo-light.png" alt="hot.dev" width="360">
  </picture>
</p>

<p align="left"><strong>Open source platform for backend workflows and AI agents</strong></p>

# Hot Dev

Hot Dev is an open source platform for backend workflows: events, schedules,
AI agents, MCP tools, long-running tasks, and service orchestration. It includes
execution tracing, a local dev runtime, and single-command deploys.

Hot is the language and runtime at its core. This repo contains the Hot
compiler, VM, and standard library, plus the platform built on top: the `hot`
CLI, API, web app, scheduler, event worker, task worker, and LSP server. Public
Hot packages live under `hot/pkg`, including `hot-std` and provider/tool
integrations.

Hot Dev Cloud is the hosted offering; its deployment infrastructure and private
operational tooling live outside this repository.

- Website: [hot.dev](https://hot.dev)
- Download: [hot.dev/download](https://hot.dev/download)
- Documentation: [hot.dev/docs](https://hot.dev/docs)
- License: [Apache-2.0](LICENSE)

## Example

This example shows how Hot wires webhooks, events, schedules, and MCP tools with
the same `meta` mechanism:

```hot
::myapp ns
::http ::hot::http
::uri ::hot::uri

// Receive a webhook, then fan out through an event.
on-signup
meta {
    webhook: {service: "leads", path: "/signup"},
    on-event: "lead:new",
}
fn (request) {
    send("lead:new", request.body)
    {ok: true}
}

// React to the event: score the lead and route it.
qualify-lead
meta {on-event: "lead:new"}
fn (event) {
    score score-lead(event.data)
    if(gte(score, 0.7),
        send("lead:qualified", event.data),
        send("lead:nurture", event.data))
}

// Run on a schedule.
weekly-summary
meta {schedule: "every monday at 9am"}
fn (event) {
    post-pipeline-summary()
}

// Expose a function as an MCP tool.
get-forecast
meta {
    mcp: {
        service: "weather",
        description: "Get the weather forecast for a location",
    },
}
fn (location: Str): Vec {
    loc ::uri/encode(location)
    response ::http/get(`https://wttr.in/${loc}?format=j1`)
    response.body.weather
}
```

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
hot deploy                # Build and deploy to Hot Dev Cloud
hot ai add                # Add AGENTS.md + Hot language skill for AI tools
```

`hot ai add` installs the Hot language skill snapshot bundled with the CLI.

## Related Repositories

- [`hot-demos`](https://github.com/hot-dev/hot-demos): runnable Hot projects,
  including AI agent examples.
- [`hot-skills`](https://github.com/hot-dev/hot-skills): Hot language skills for
  AI coding tools.
- [`setup-hot`](https://github.com/hot-dev/setup-hot): GitHub Action for
  installing Hot in CI.
- [`hot-js`](https://github.com/hot-dev/hot-js): JavaScript/TypeScript SDK for
  Hot.
- [`hot-python`](https://github.com/hot-dev/hot-python): Python SDK for Hot.
- [`hot-vsx`](https://github.com/hot-dev/hot-vsx): VS Code and Cursor extension
  for Hot.

## Repository Layout

```text
crates/
  hot/                    # Core language, runtime, storage, and internals
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

`crates/` is the Rust workspace (compiler, runtime, and services). The top-level
`hot/` tree holds Hot source: the publishable packages in `hot/pkg` and the Hot
language and package tests.

## Development

Prerequisites:

- Rust (version pinned in `rust-toolchain.toml`)
- Docker (optional for most development; required for `::hot::box` container
  tasks and release packaging)

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

## Releases

Releases are tagged from `resources/version.txt`; automation lives in
`.github/workflows/release.yml`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test, and submit
changes. To report a security issue, follow the process in
[SECURITY.md](SECURITY.md) rather than opening a public issue.

## License

Copyright 2025-2026 Hot Dev, LLC.

Hot is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE)
and [NOTICE](NOTICE). Brand usage for the Hot Dev name and artwork is covered
separately in [BRAND.md](BRAND.md).
