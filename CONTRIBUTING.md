# Contributing to Hot

Thanks for your interest in contributing to Hot Dev. This document covers what
you need to know to build the project, propose changes, and get them merged.

## Code of Conduct

Please be respectful in all interactions: issues, pull requests, discussions,
and chat. We follow the spirit of the
[Contributor Covenant](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).

## Reporting Issues

- **Bugs and feature requests:** open a GitHub issue using one of the templates.
- **Security vulnerabilities:** see [SECURITY.md](SECURITY.md). Do not open a
  public issue.
- **Questions and discussion:** the [docs](https://hot.dev/docs) usually answer
  most "how do I..." questions; if not, please open a discussion or issue.

For bug reports, the more reproducible the better: a minimal `.hot` snippet,
the exact `hot` version, OS, and the error output go a long way.

## Project Layout

See the [Repository Layout](README.md#repository-layout) section of the README
for a tour of the crates and resources directories. The most common areas:

- `crates/hot/` — language, runtime, storage, and packages core.
- `crates/hot_cli/` — the `hot` binary.
- `crates/hot_app/` — local web app and dashboard.
- `hot/pkg/` — public Hot packages, including `hot-std`.
- `resources/docs/`, `resources/init/`, `resources/db/` — docs, project
  templates, and database migrations shipped with the binary.

## Development

### Prerequisites

- Rust (toolchain pinned in `rust-toolchain.toml`)
- Docker, optional for most development but required for `::hot::box` container
  tasks and release packaging.

### Build, Check, Test

The same commands CI runs:

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

### SQLx Offline Mode

`.cargo/config.toml` sets `SQLX_OFFLINE=false` with `force = true` so local
builds verify queries against a live database during development. CI overrides
this to `true` (queries are checked against committed `.sqlx/` query metadata).

If you change a SQL query, regenerate the offline query data:

```bash
cargo sqlx prepare --workspace
```

Commit the resulting `.sqlx/` changes alongside your query change.

## Style and Conventions

- **Rust:** `cargo fmt` and `cargo clippy` must pass with no warnings.
- **Hot:** see [`AGENTS.md`](AGENTS.md) for the language style guide. The Hot
  language has unusual rules (no infix operators, no `=` for assignment, etc.)
  that tooling will not always catch.
- **Comments:** explain *why*, not *what*. Avoid narrating code.
- **Commits:** keep them small and focused; prefer a series of clean commits
  over one large squash. Imperative present tense in subject lines.
- **Generated files:** root `AGENTS.md` is generated from
  `resources/ai/AGENTS.md` by `cargo run --locked --bin hot -- ai add`. Do not
  hand-edit the root file. CI verifies they are in sync via
  `scripts/check-agents-sync.sh`.
- **AI skill assets:** `resources/ai/skills/hot-language/` is the source copy
  bundled with the CLI. After editing it, run
  `bash scripts/sync-ai-assets.sh ../hot-skills` to update the public
  `hot-skills` mirror, then `bash scripts/check-ai-assets-sync.sh ../hot-skills`.

## Testing Your Changes

- **Rust unit/integration tests:** `cargo test --workspace`.
- **Hot language and package tests:** `cargo run --locked --bin hot -- test -c hot.test.hot --ctx hot/ci.ctx.hot`.
- **Provider/integration tests:** under `scripts/integration/`. These require
  real provider credentials and are not run in CI for outside contributors.
  Skip them unless you have your own credentials and can run them locally.

## Pull Requests

1. Fork the repository and create a branch from `main`.
2. Make your change with tests where applicable.
3. Run the CI commands above locally.
4. Open a pull request against `main` using the PR template.
5. Be prepared for review feedback. Maintainers may push small fixups directly
   for typos or minor cleanups; larger changes will be requested as updates.

A maintainer will merge once the PR is approved and CI is green. We typically
squash-merge feature work; multi-commit refactors may be merged as-is when the
history is meaningful.

## Releases

Releases are cut by maintainers from the `stable` branch and tagged
`vX.Y.Z` matching `resources/version.txt`. The release pipeline
(`.github/workflows/release.yml`) is gated to the upstream repository and
publishes installers, packages, and Homebrew formula updates that require
maintainer-only secrets. It will not run on forks.

## License

By contributing, you agree that your contributions will be licensed under the
[Apache License 2.0](LICENSE), the same license as the project.
