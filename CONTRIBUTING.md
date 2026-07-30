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

- `crates/hot/` — language, runtime, storage, and package-system internals.
- `crates/hot_cli/` — the `hot` binary.
- `crates/hot_app/` — local web app and dashboard.
- `hot/pkg/` — public Hot packages, including `hot-std`.
- `resources/docs/` — documentation source.
- `resources/init/`, `resources/db/` — project templates and database
  migrations packaged with the CLI.

## Development

### Prerequisites

- Rust (toolchain pinned in `rust-toolchain.toml`)
- Tailwind CSS CLI (`tailwindcss` on `PATH`; CI uses the version declared in
  `.github/workflows/hot.yml`)
- Docker, optional for most development but required for `::hot::box` container
  tasks and release packaging.
- Postgres 18 with pgvector and Valkey/Redis are optional for the full
  service-backed test suite.
- `protoc` is required only for the Linux Kata feature check.

### Build, Check, Test

Run these core checks before opening a pull request. The Linux CI matrix
additionally exercises service-backed and Docker suites:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
./scripts/hot-static-checks.sh check
cargo run --locked --bin hot -- init -c hot.test.hot --ctx hot/ci.ctx.hot
cargo run --locked --bin hot -- test -c hot.test.hot --ctx hot/ci.ctx.hot
```

On Linux with `protoc` installed, also compile-check the optional Kata backend:

```bash
cargo test -p hot_task_worker --features kata --locked --no-run
```

Run the local development stack:

```bash
cargo run --locked --bin hot -- dev
```

Install optional git hooks:

```bash
./scripts/setup-git-hooks.sh
```

### Database Changes

The repository uses SQLx's runtime query APIs and does not commit `.sqlx/`
offline-query metadata. Do not run `cargo sqlx prepare`.

For schema changes, add the next numbered migration under
`resources/db/sqlite/migrations/` and `resources/db/postgres/migrations/` when
the change applies to both databases. Follow the public/private migration
boundary in [`resources/db/V2_MIGRATIONS.md`](resources/db/V2_MIGRATIONS.md).

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
- **AI skill assets:** `resources/ai/skills/` contains the source skills
  bundled with the CLI. After editing any skill, run
  `bash scripts/sync-ai-assets.sh ../hot-skills` to update the public
  `hot-skills` mirror, then `bash scripts/check-ai-assets-sync.sh ../hot-skills`.
  Skills are installed-product documentation: use versioned registry packages,
  resolved dependency paths, and installed SDK metadata. Do not assume a Hot
  source checkout, local sibling packages, SDK repository clones, or access to
  internal applications.

## Testing Your Changes

- **Rust unit/integration tests:**
  `cargo test --workspace --all-targets --locked`.
- **Hot language and package tests:** initialize with
  `cargo run --locked --bin hot -- init -c hot.test.hot --ctx hot/ci.ctx.hot`,
  then run
  `cargo run --locked --bin hot -- test -c hot.test.hot --ctx hot/ci.ctx.hot`.
- **Optional package/integration tests:** scripts under `scripts/integration/`
  have package-specific prerequisites. Some need provider credentials, while
  others need local services or executables. They are not part of the default
  CI suite; run the scripts relevant to your change.

## Pull Requests

1. Fork the repository and create a branch from `main`.
2. Make your change with tests where applicable.
3. Run the relevant checks above locally.
4. Open a pull request against `main` using the PR template.
5. Be prepared for review feedback. Maintainers may push small fixups directly
   for typos or minor cleanups; larger changes will be requested as updates.

A maintainer will merge once the PR is approved and CI is green. We typically
squash-merge feature work; multi-commit refactors may be merged as-is when the
history is meaningful.

## Releases

`main` is the stable release branch. Maintainers update
`resources/version.txt`, run `scripts/sync-version.sh`, commit the synchronized
version files and refreshed `Cargo.lock` to `main`, and push a `vX.Y.Z` tag at
that commit. The tag must match `resources/version.txt`. Pushing it starts
`.github/workflows/release.yml`.

Publishing jobs are gated to the upstream `hot-dev/hot` repository. Depending
on repository configuration, the pipeline publishes the GitHub release,
installers, package CDN, and Homebrew formula updates using maintainer-only
credentials; forks cannot publish upstream artifacts.

## License

By contributing, you agree that your contributions will be licensed under the
[Apache License 2.0](LICENSE), the same license as the project.
