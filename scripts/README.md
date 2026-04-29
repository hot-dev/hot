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
- `integration/*.sh` contains package integration test runners. Use the package
  or service name as the filename, for example `integration/resend.sh`.
- `git-hooks/` contains hook scripts installed by `setup-git-hooks.sh`.

## Placement

Keep AWS deploy, cloud service, production operator, marketing site, and
customer/billing scripts in the private cloud/operations repository. If a
script needs private infrastructure, private content, or cloud-only environment
names, it does not belong here.
