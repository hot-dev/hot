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
- Existing data services: set `data.postgres.mode` or `data.valkey.mode` to
  `existing` and put the connection settings in the operator secret
  (`HOT_DB_URI` for the runtime login, `HOT_DB_MIGRATION_URI` for the schema
  owner, `POSTGRES_CA_CERT`, and `HOT_REDIS_URI`). Prepare the database as
  [Self-Hosted PostgreSQL](../resources/docs/configuration/postgres.md)
  describes; the bundle checks it, migrates, and grants the runtime login,
  and never creates or drops anything the operator owns.
- Managed PostgreSQL: set `data.postgres.mode` to `managed` and the stack
  provisions PostgreSQL 18 on RDS or Cloud SQL, reachable only from the host
  and only over TLS. It is sized from the installation size unless you set
  `tier` or `storage_gb`; `high_availability` selects Multi-AZ or a regional
  instance; backups are kept for `backup_retention_days` (default 7) with
  point-in-time recovery. On each deploy the agent signs in as the database
  administrator to create the `hot_owner` and runtime logins, the `hot`
  database and schema, and the `vector` and `pg_trgm` extensions, then checks,
  migrates, and grants as for an existing database.
  - The administrator password never enters OpenTofu state. RDS keeps it in
    its own secret. On GCP it is generated during apply and written only to
    write-only attributes on Cloud SQL and on a Secret Manager secret. If an
    apply stops between the two, raise `data.postgres.admin_password_version`
    and apply again to set a new one in both.
  - RDS connections verify the endpoint's hostname against the RDS CA bundle,
    which the agent downloads from AWS. Cloud SQL connections verify the
    instance's own CA, which signs only that instance's certificates.
  - `deletion_protection` (default on) keeps OpenTofu and the cloud from
    deleting the database, including when `data.postgres.mode` changes. Turn
    it off in a reviewed apply first. RDS also keeps a final snapshot.
  - New networks get the subnets (AWS) or private services access (GCP) the
    database needs. On an existing AWS network, give `network.data_subnet_ids`
    in two zones; an existing GCP network must already have private services
    access.
- Managed Valkey (AWS): set `data.valkey.mode` to `managed` for an ElastiCache
  Valkey 8.1 replication group, sized from the installation size unless you set
  `node_type`, with `high_availability` adding a replica in a second zone and
  automatic failover.
  - The cache requires TLS and an AUTH token, and never evicts queue entries.
  - Like the GCP database password, the token is generated during apply and
    written only to write-only attributes, on ElastiCache and on a Secrets
    Manager secret. Raise `data.valkey.auth_token_version` to set a new one.
  - Memorystore for Valkey accepts only IAM tokens, which Hot's Valkey client
    does not request yet, so GCP installations use local or existing Valkey
    for now.
- Public endpoints: set `app_exposure` or `api_exposure` to `public`. AWS
  needs `public_certificate_arn` (an ACM certificate you DNS-validate); GCP
  issues a managed certificate once your DNS points at the `public_endpoint`
  output. The VM keeps no public address, and private endpoints are never
  served to the load balancer.
- Backups, logs, and alerts: see [Backups, logs, and alerts](#backups-logs-and-alerts).
- Preflight: [`preflight/preflight.py`](preflight/preflight.py) checks an
  installation's account or project before provisioning, and the plan
  workflow runs it first. See [Preflight](#preflight).
- In Hot: SMTP email delivery, `hot admin bootstrap`/`reset-password` and the
  `app.signup` policy for operator-managed accounts, and outbound allowlists
  (`network.allow-cidrs`, `allow-hosts`, `deny-cidrs`) behind a fixed deny set.

The roots intentionally require `acknowledge_unqualified = true`. The bootstrap
also refuses to start if the supplied image lacks `hot-agent.service`. Real
image builds and deployments on each cloud, managed Valkey on GCP, host
journal shipping, restore tooling, and the real-cloud release gates remain to
be implemented and qualified. Do not apply these development examples to
production. No usable host image or published runtime digest is implied by the
placeholder references in the fixtures.

## Preflight

```sh
python3 infra/preflight/preflight.py installation/installation.tfvars.json
```

Preflight runs read-only `aws` or `gcloud` commands with your identity; the
plan role or service account is enough. It reads no secret values. It checks:

- the zone, machine type, vCPU quota, and host image;
- on GCP, the APIs and the nested-virtualization organization policy;
- that network ranges avoid the host's container network (`172.30.0.0/24`)
  and box guest network (`10.88.0.0/16`);
- existing networks and their subnets;
- the public certificate on AWS;
- existing buckets;
- managed PostgreSQL and Valkey offerings in the region.

It warns when hostnames don't resolve from where it runs. It lists the checks
only the installation's host can make, such as an existing database's TLS and
logins, which the agent runs before migrating. It exits 1 when a required
check fails, and the plan workflow stops there.

## Backups, logs, and alerts

The retained data disk is snapshotted daily at `backup.time_utc` (default
`03:00`) and each snapshot is kept for `backup.retention_days` (default 7).
AWS uses Data Lifecycle Manager and GCP a snapshot schedule. The host cannot
delete snapshots.

The disk holds local PostgreSQL, Valkey's append-only file, and local builds
and files. Snapshots are crash-consistent: PostgreSQL recovers from its WAL
on restore, and Valkey loses at most the last second of queue writes
(`appendfsync everysec`). The recovery point is the last snapshot, so at most
a day. Managed databases keep their own point-in-time backups. The `backup`
command adds an on-demand `pg_dump` of a local database.

Container logs (every Hot service, the ingress, the data services, and the
migration and admin jobs) go to CloudWatch Logs in `/hot/<installation>`, kept
for `logging.retention_days` (default 30), or to Cloud Logging, with the VM's
identity. Docker ships them without blocking: if the destination is slow or
down, lines beyond a 4 MB buffer are dropped rather than stalling Hot. A
bounded local copy (3 files of 10 MB per container) remains for
`docker logs`. Set `logging.destination` to `local` to keep logs on the host
only. Logs are lost with the host then.

Every 30 seconds the host agent publishes these metrics, to CloudWatch
(namespace `Hot/SelfHost`, dimension `Installation`) or Cloud Monitoring
(`custom.googleapis.com/hot/<installation>/...`):

| Metric | Meaning |
| --- | --- |
| `AgentHeartbeat` | 1 while the agent runs |
| `UnhealthyServices` | Hot services, ingress, and local data services not running and healthy |
| `SchedulerRunning` | 1 while the scheduler container runs |
| `DependenciesReachable` | 1 when local data services are healthy and managed or existing ones accept connections |
| `Ready` | 1 when the app and API answer through the ingress |
| `DataDiskUsedPercent` | The larger of space and inodes used on the data disk |
| `BackupAgeHours` | Hours since the newest completed snapshot |

The stacks alert on them. Set `monitoring.alert_emails`; AWS sends each address
an SNS confirmation email first. Alerts reach those addresses independently of
Hot's own email, and both firing and resolving are sent.

| Alert | Fires when | First steps |
| --- | --- | --- |
| `host-down` | No heartbeat for 5 minutes (also EC2 `instance-impaired` on AWS) | Check the VM in the console; `diagnostics` once it answers. The agent restarts with the VM. |
| `services-unhealthy` | A service unhealthy for 15 minutes | `diagnostics` names it; `restart-service`, then read its logs. |
| `scheduler-stopped` | No scheduler for 10 minutes | Schedules are not firing. `restart-service scheduler`; it exits if it loses its database lock, so check database connectivity too. |
| `dependencies-unreachable` | PostgreSQL or Valkey unreachable for 10 minutes | Check the data service, its security group or firewall, and its status page. |
| `not-ready` | App or API not answering for 15 minutes | Check `services-unhealthy` and the ingress certificate. |
| `disk-pressure` | Data disk over 85% for 15 minutes | Grow the disk, or move builds and files to object storage. |
| `backup-stale` | No snapshot for 30 hours, or no backup status | Check the snapshot policy or schedule in the console. |

Liveness, dependency reachability, and end-to-end readiness are separate
signals, and a missing heartbeat or backup status alerts rather than passing.

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
