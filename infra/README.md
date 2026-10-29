# Self-host infrastructure

This directory contains the first local implementation of the cloud self-hosting
[plan](../docs/plans/opentofu-cloud-self-hosting.md). It is an **unqualified
development foundation**, not a supported installation release.

Implemented:

- A common single-machine input contract, with private ingress and independent
  Local/existing S3 choices for builds and files/blobs.
- AWS and GCP OpenTofu roots with pinned providers and committed lockfiles.
  New private networks have NAT egress; existing networks are reused without
  changing their routes. VMs have no public addresses or SSH ingress.
- Retained, encrypted cloud data disks, encrypted boot disks, nested
  virtualization, scoped secret access, and separate host identities.
- Required client-side encryption of OpenTofu state and saved plans. The
  encryption passphrase is an ephemeral input, supplied separately from runtime
  secrets. Cloud-backend encryption and access controls are also required.
- A development runtime-bundle renderer: the same pinned Hot image runs app,
  API, worker, scheduler, task-worker, and the migration job. PostgreSQL, Valkey,
  private HTTPS ingress, and restricted-role provisioning are included.
- File-backed secret consumption and provenance-based configuration redaction
  in Hot; strict backend selection; S3 temporary session-token support; JSON
  runtime logging.
- The single runtime image recipe
  ([`images/containers/runtime.Dockerfile`](images/containers/runtime.Dockerfile)),
  built by [`images/build-runtime.sh`](images/build-runtime.sh) and by the
  release workflow, which pushes it to `ghcr.io/hot-dev/hot-runtime` and signs
  it with Sigstore.
- Signed release manifests: [`release/release_manifest.py`](release/release_manifest.py)
  pins the runtime image and the upstream images in
  [`release/upstream-images.json`](release/upstream-images.json). The release
  workflow signs the manifest keylessly; the renderer verifies it with
  `cosign verify-blob` against the release workflow's tag identity.
- The host image recipe ([`images/host/setup-host.sh`](images/host/setup-host.sh),
  [`images/packer`](images/packer), built by
  [`images/build-host.sh`](images/build-host.sh)): Ubuntu 24.04 with Docker,
  Kata on QEMU, a dedicated containerd, CNI, `cosign`, and a guest image with
  the release's `hotbox`. SSH is masked, and Packer builds without SSH. Kata
  state lives on the data disk and stays off until it is mounted; box storage
  is capped by `HOT_BOX_POOL_GB`; a mangle-table baseline keeps guests off
  metadata, the host, other guests, and private ranges outside the allowlist.
- The host deploy agent ([`agent/hot_agent.py`](agent/hot_agent.py)): it
  follows the release pointer, verifies the signed manifest, writes each
  service's secrets to tmpfs, migrates, and restarts services with the
  scheduler stopped first and started last. Generated secrets (database and
  cache passwords, the session secret, the encryption key, the first admin's
  password) are created once in the cloud secret store; the operator puts the
  TLS certificate and key, and any SMTP, OAuth, or S3 credentials, in the
  operator secret the stack creates. Commands are a fixed list run through an
  SSM command document (AWS) or Pub/Sub (GCP).
- In Hot: SMTP email delivery, `hot admin bootstrap`/`reset-password` and the
  `app.signup` policy for operator-managed accounts, and outbound allowlists
  (`network.allow-cidrs`, `allow-hosts`, `deny-cidrs`) behind a fixed deny set.

The roots intentionally require `acknowledge_unqualified = true`. The bootstrap
also refuses to start if the supplied image lacks `hot-agent.service`. Real
image builds and deployments on each cloud, the optional egress proxy, existing-DB
integration, cloud monitoring/alerts, and the real-cloud release gates remain
to be implemented and qualified. Do not apply these development examples to
production. No usable host image or published runtime digest is implied by the
placeholder references in the fixtures.

## Local validation

Use OpenTofu 1.13.1 (the tested version) and Python 3.10 or newer. Docker Compose
is used to validate rendered bundles; the configuration test does not pull or
start containers. Provider initialization downloads the pinned provider plugins.
All `tofu test` cases use mock providers and do not access a cloud account.

```sh
python3 -m unittest discover -s infra/tests -v
python3 infra/runtime/render.py infra/examples/gcp-existing-s3.json

tofu fmt -check -recursive infra/opentofu
tofu -chdir=infra/opentofu/modules/single-machine-contract init -backend=false
tofu -chdir=infra/opentofu/modules/single-machine-contract test

tofu -chdir=infra/opentofu/stacks/aws/single-machine init -backend=false
tofu -chdir=infra/opentofu/stacks/aws/single-machine validate
tofu -chdir=infra/opentofu/stacks/aws/single-machine test

tofu -chdir=infra/opentofu/stacks/gcp/single-machine init -backend=false
tofu -chdir=infra/opentofu/stacks/gcp/single-machine validate
tofu -chdir=infra/opentofu/stacks/gcp/single-machine test
```

The mock fixtures include a test-only state-encryption passphrase. Real runs
must supply `TF_VAR_state_encryption_passphrase` from an independent protected
source and retain it for recovery. Never use a fixture key for a real backend.
Configure a protected remote backend separately; application storage and state
storage need not use the same provider. No backend credentials belong in JSON
examples or checked-in `.tf` files. Ignored `backend_override.tf` and
`encryption_override.tf` files are reserved for operator configuration.

## Development runtime bundle

The files in `examples/` contain non-secret development configurations. Existing
S3 is supported independently of VM cloud. The renderer never creates a bucket,
changes its policies, fetches secrets, starts services, or contacts a provider.
It refuses unknown/unimplemented settings and mutable container-image tags.

```sh
python3 infra/runtime/render.py infra/examples/gcp-existing-s3.json \
  --manifest infra/examples/development-manifest.json \
  --s3-auth files \
  --allow-unsigned-development-manifest \
  --output infra/.generated/gcp-example

docker compose -f infra/.generated/gcp-example/compose.json \
  --profile admin config --quiet
```

The development manifest contains deliberately unusable image references for
configuration validation only, so rendering it requires the explicit
unsigned-manifest acknowledgment. A published release manifest renders with its
signature instead:

```sh
python3 infra/runtime/render.py config.json \
  --manifest hot-selfhost-manifest.json \
  --signature-bundle hot-selfhost-manifest.sigstore.json \
  --output /etc/hot/bundle
```

Verification needs `cosign` on `PATH` (or `--cosign`) and accepts only
signatures from the `hot-dev/hot` release workflow on a `v*` tag; a fork that
publishes its own releases passes `--signer-identity`.

The resulting bundle has these prerequisites for future host-agent integration:

- A qualified amd64 runtime image with S3 and Kata, runtime resources and
  `hotbox`, plus pinned PostgreSQL/pgvector, Valkey, and unprivileged Nginx
  images. All Hot commands use one image digest.
- A mounted encrypted data disk and precreated `postgres`, `valkey`, `build`,
  and `file` directories. Bind mounts disable automatic host-path creation to
  avoid silently initializing storage on the boot filesystem.
- Secret files already staged on tmpfs in `/run/hot/secrets/<role>/<name>`.
  Values never appear in Compose environment variables or command arguments.
  Each role receives only its own mounted files; the task-worker runs privileged
  as a documented exception. Other Hot processes run as UID/GID 10001.
- Per-service `HOT_DB_URI` (restricted runtime role), `HOT_REDIS_URI`, and the
  encryption key where needed; the app additionally receives its session secret.
  File-based S3 authentication mounts each selected role's AWS access/secret
  key files. Identity mode uses the SDK credential chain; it does not create
  Google Cloud Storage resources on GCP. For temporary credentials, add
  `--s3-session-tokens` to mount the corresponding session-token files as well;
  automatic renewal/host-agent wiring remains follow-up work.
- A `postgres` bundle with `POSTGRES_PASSWORD` and `POSTGRES_RUNTIME_PASSWORD`;
  a `valkey` bundle with matching `VALKEY_ACL` and `VALKEY_PASSWORD`; and an
  `ingress` bundle with a trusted certificate/chain and private key covering both
  endpoint hostnames. File owners must match the selected image's service UID.
- A separate `migrate/HOT_DB_MIGRATION_URI` and `provision/PGPASSFILE`, containing
  deployment-only credentials. Migrations and the existing runtime grant script
  must complete before runtime services start. Initial SQL creates the restricted
  role only on a fresh PostgreSQL volume; it is not a password-rotation mechanism.
- A baked and qualified Kata/QEMU host with KVM, the dedicated containerd
  socket, CNI binaries/firewall, and bounded guest storage. The task-worker uses
  the established host mounts and host PID namespace; no Docker socket is mounted.

The bundle starts one scheduler and gives every Hot service a long stop deadline.
The future deploy agent must enforce drain and stop-before-start sequencing;
Compose alone does not implement scheduler fencing, backups, or safe upgrades.
Resource limits are conservative starting caps, not qualified size benchmarks.
Local database/cache traffic stays inside the host's container network and no
data ports are published; remote dependency TLS qualification is separate work.

Hostnames and supplied TLS material define private ingress. Internet access is
provided through outbound NAT. Source ranges/IAP access are explicit, and no
cloud stack publishes PostgreSQL/Valkey or port 22. Public/mixed ingress, GCS
(Hot itself supports it; the renderer does not select it yet),
managed data, customer-managed keys, and cross-provider bucket provisioning are
rejected or outside this slice; their implementation remains in the plan.
