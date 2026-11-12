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
  every public mirror tree against the recorded manifest hashes. With no mirror
  path and no sibling checkout, it verifies only the manifest, which is how the
  required `hot` check runs it. Pass a skill
  name as the second argument only when intentionally syncing or checking one
  skill; filtered sync preserves every unrelated manifest hash so CI still
  detects unsynchronized source changes. A full sync removes mirror directories
  whose canonical skill was deleted.
  `check-doc-hot-blocks.sh` runs `hot check` on every ```hot block in the AI
  docs and in `resources/docs`, with stubs for the helpers they leave
  undefined (never for a name the block declares; `--show-stubs` lists them).
  A block that names a namespace of a package in `hot/pkg` is checked with
  that package loaded. A marker on the line before a block changes how it is
  checked: `<!-- hot-check: assume ::db -->` for namespaces that stand for
  the reader's own code, `<!-- hot-check: fragment -->` for the inside of a
  map literal, and `<!-- hot-check: skip -->` for a block that is not meant
  to compile. Skipped blocks are still swept: their `name fn (params)`
  signatures, `::ns/name` references and "- `::hot::ns` — `name`" bullets
  must name functions that exist with those arities.
  `check-doc-links.py` checks the internal links, heading anchors and
  `nav.md` of `resources/docs`; it reads files only.
  `check-demo-doc-pins.py` checks what `resources/docs/demos` says about the
  hot-demos repository (paths, packages, slash commands, environment
  variables, npm scripts) against `demo-doc-pins.json`. With
  `--demos HOT_DEMOS_CHECKOUT` it checks the pages against the checkout
  (nightly), and with `--write` as well it records them in the manifest.
  The CLI and configuration references are checked against clap and the
  `all` template by Rust tests: `cargo test -p hot_cli --lib cli_docs` and
  `cargo test -p hot_cli --lib conf_docs`.
  `check-go-doc-snippets.sh HOT_GO_CHECKOUT` compiles every ```go block in
  `resources/docs/api` against a hot-go checkout.
  `check-sdk-doc-pins.mjs` checks that the SDK version
  `resources/docs/api/sdks.md` pins is published on npm, PyPI, crates.io,
  Maven Central and the Go proxy (networked; nightly and release only).
  `check-pkg-pins.py` checks that each package in `hot/pkg` pins its siblings
  at their versions here (or at a version in its `ALLOWED_PINS`), by version
  rather than local or git path. `check-pkg-pins.py smoke` runs `hot check` on
  a project for each package in `pkg-publish.txt` with the package's real
  pins: pins at a version here load this checkout's copy, and any other pin
  downloads from pkg.hot.dev (exit 2 when only downloads failed). The repo's
  own projects load packages by local path, so they never compile these pins.
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
