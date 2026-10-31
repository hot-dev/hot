# Scripts

These scripts are public-repo tooling for building, checking, packaging, releases,
package documentation, and local integration tests.

## Layout

- `build-*.sh`, `package-*.sh`, and `bundle.sh` are release/package build
  entrypoints.
- `hot-pkg-cdn*.sh` and `pkg-docs.sh` are public package documentation/CDN
  helpers.
- `check-*.sh`, `sync-*.sh`, and `fix-*.sh` are repository maintenance helpers.
  `check-agents-sync.sh` verifies that root `AGENTS.md` was generated from
  `resources/ai/AGENTS.md` by `cargo run --locked --bin hot -- ai add`.
  `sync-ai-assets.sh` copies every canonical `resources/ai/skills/` skill
  into a sibling `hot-skills` checkout, and `check-ai-assets-sync.sh` verifies
  every public mirror tree against the recorded manifest hashes. Pass a skill
  name as the second argument only when intentionally syncing or checking one
  skill; filtered sync preserves every unrelated manifest hash so CI still
  detects unsynchronized source changes. A full sync removes mirror directories
  whose canonical skill was deleted.
  `check-doc-hot-blocks.sh` runs `hot check` on every ```hot block in the AI
  docs, with stubs for the helpers they leave undefined (never for a name the
  block declares; `--show-stubs` lists them); mark a block that is not meant
  to compile with `<!-- hot-check: skip -->` on the line before it. Skipped
  blocks are still swept: their `name fn (params)` signatures, `::ns/name`
  references and "- `::hot::ns` — `name`" bullets must name functions that
  exist with those arities.
  `check-go-doc-snippets.sh HOT_GO_CHECKOUT` compiles every ```go block in
  `resources/docs/api` against a hot-go checkout.
  `check-sdk-doc-pins.mjs` checks that the SDK version
  `resources/docs/api/sdks.md` pins is published on npm, PyPI, crates.io,
  Maven Central and the Go proxy (networked; nightly and release only).
  `install-tailwind.sh VERSION DIR` installs the Tailwind CLI for this
  platform after checking its pinned SHA-256; add a release's digests there
  before moving `TAILWIND_VERSION`.
- `ci/` holds checks the nightly workflow runs: `memory-smoke.sh` runs the
  `ci/memory-smoke/*.hot` fixtures (in a 1 GiB container there), and
  `compat-diff.sh` reports errors this build gives on the previous release's
  sources that the previous release didn't, apart from intended breaks listed
  in `ci/compat-allowlist.txt`.
- `integration/*.sh` contains package integration test runners. Use the package
  or service name as the filename, for example `integration/resend.sh`.
- `noisy-load-benchmark.sh` runs the `hot/test/noisy-load` stress project
  against SQLite/memory and PostgreSQL/Valkey, then writes DB/queue/log metrics
  under `target/noisy-load-runs/`.
- `git-hooks/` contains hook scripts installed by `setup-git-hooks.sh`.

## Placement

Keep AWS deploy, cloud service, production operator, marketing site, and
customer/billing scripts in the private cloud/operations repository. If a
script needs private infrastructure, private content, or cloud-only environment
names, it does not belong here.
