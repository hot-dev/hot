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
- Buckets: each storage role (`storage.build`, `storage.file`) uses local
  disk, S3, or GCS. A bucket is used as it exists unless `provision` is
  true. Then the stack creates it in the installation's own cloud and region,
  with these settings:
  - private, encrypted, and TLS-only on S3;
  - versioned, with old versions kept for `noncurrent_days` (default 30);
  - abandoned multipart uploads deleted after a week;
  - never emptied or deleted by `tofu destroy`.

  Both roles may share one bucket under different prefixes, and the host
  may use only its roles' prefixes.
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
- Managed Valkey: set `data.valkey.mode` to `managed`. It is sized from the
  installation size unless you set `node_type`, never evicts queue entries,
  and requires TLS. With `high_availability`, a replica runs in another zone.
  - AWS: an ElastiCache Valkey 8.1 replication group with automatic failover.
    It requires an AUTH token, which, like the GCP database password, is
    generated during apply and written only to write-only attributes, on
    ElastiCache and on a Secrets Manager secret. Raise
    `data.valkey.auth_token_version` to set a new one.
  - GCP: a Memorystore for Valkey 8.0 instance with cluster mode disabled,
    with append-only persistence, reached through Private Service Connect.
    It uses IAM authentication, so there is no password at all: Hot services
    run with `redis.auth = gcp-iam`. They present the VM's short-lived access
    tokens, refreshed before they expire, and trust the instance's CA
    (`redis.ca-cert`). The host may connect to this instance only
    (`roles/memorystore.dbConnectionUser` with a condition). On a new
    network the stack adds the service connection policy Memorystore needs.
    An existing network must already have one for `gcp-memorystore` in the
    region; preflight checks for it.
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
image builds and deployments on each cloud and the real-cloud release gates
remain to be implemented and qualified. Do not apply these development examples to
production. No usable host image or published runtime digest is implied by the
placeholder references in the fixtures.

## Running OpenTofu yourself

The GitHub workflows are optional: you can plan and apply from a terminal
with a deploy identity you assume, and so can a CI system other than GitHub.
The stacks and the state bootstrap configure no credentials of their own,
so OpenTofu, `gcloud`, the `aws` CLI, `preflight.py`, and `selfhost.py` all
use the identity you give them.

**GCP: impersonate a service account**, such as an `infra-provisioner` you
already use, or the bootstrap's apply identity:

```sh
export GOOGLE_IMPERSONATE_SERVICE_ACCOUNT=infra-provisioner@PROJECT.iam.gserviceaccount.com
export CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT=$GOOGLE_IMPERSONATE_SERVICE_ACCOUNT
```

The first variable covers the Google provider and the `gcs` state backend;
the second covers `gcloud`. You need `roles/iam.serviceAccountTokenCreator`
on the service account.

**AWS: assume a role** with a CLI profile:

```ini
[profile hot-apply]
role_arn = arn:aws:iam::123456789012:role/infra-provisioner
source_profile = default
```

Run `export AWS_PROFILE=hot-apply`. The S3 state backend, the provider, and
the `aws` CLI all use it.

**Let the bootstrap create the identity,** or let people use the one GitHub
uses. Name the people who may assume each deploy identity, and optionally
rename it:

```hcl
# GCP bootstrap
operators   = { apply = ["user:ada@example.com", "group:platform@example.com"] }
names       = { apply = "infra-provisioner" }
apply_roles = ["roles/compute.admin", "..."]

# AWS bootstrap
operators         = { apply = ["arn:aws:iam::123456789012:role/platform-admins"] }
names             = { apply = "infra-provisioner" }
apply_policy_arns = ["arn:aws:iam::aws:policy/..."]
```

An identity is created when GitHub uses it, when operators are named for it,
or both:
- **GCP:** each operator gets `roles/iam.serviceAccountTokenCreator` on that
  service account.
- **AWS:** each operator is added to the role's trust policy for
  `sts:AssumeRole`.

The plan and apply identities can read and lock state; the release
identity's permissions come from each installation's `release_principal`.

**Bring your own identity** instead: give it the apply permissions, plus
read, write, and delete on the state objects (`roles/storage.objectUser` on
the bucket, or `s3:GetObject`, `PutObject`, `DeleteObject`, and
`ListBucket`). For release and host commands, set it as the installation's
`release_principal`.

In every case, provide the state passphrase as
`TF_VAR_state_encryption_passphrase` and use the installation's
`backend_override.tf`.

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
- existing buckets, and that buckets to provision are still free;
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

The agent also ships the host journal. That is every agent line, plus
warnings and errors from the rest of the host, such as the kernel's OOM
kills, Docker, and failed units. They go to the `host-journal` stream of the
same log group, or to the `hot-<installation>-host` log in Cloud Logging. It
reads from a saved journal cursor, so entries are not lost or sent twice
across restarts. Each source sends at most 500 entries per cycle, redacted
like diagnostics. With `logging.destination = local`, the journal stays on
the host too.

Every 30 seconds the host agent publishes these metrics, to CloudWatch
(namespace `Hot/SelfHost`, dimension `Installation`) or Cloud Monitoring
(`custom.googleapis.com/hot/<installation>/...`):

| Metric | Meaning |
| --- | --- |
| `AgentHeartbeat` | 1 while the agent runs |
| `UnhealthyServices` | Hot services, ingress, and local data services not running and healthy |
| `SchedulerRunning` | 1 while the scheduler runs and dispatches: it rewrites a heartbeat file every 30 seconds, and its health check fails after 2 minutes without one |
| `DependenciesReachable` | 1 when local data services are healthy and managed or existing ones accept connections |
| `Ready` | 1 when the app and API answer through the ingress |
| `DataDiskUsedPercent` | The larger of space and inodes used on the data disk |
| `BackupAgeHours` | Hours since the newest completed snapshot |
| `CertificateDaysLeft` | Days until the ingress TLS certificate expires (0 if unreadable) |

The stacks alert on them. Set `monitoring.alert_emails`; AWS sends each address
an SNS confirmation email first. Alerts reach those addresses independently of
Hot's own email, and both firing and resolving are sent.

| Alert | Fires when | First steps |
| --- | --- | --- |
| `host-down` | No heartbeat for 5 minutes (also EC2 `instance-impaired` on AWS) | Check the VM in the console; `diagnostics` once it answers. The agent restarts with the VM. |
| `services-unhealthy` | A service unhealthy for 15 minutes | `diagnostics` names it; `restart-service`, then read its logs. |
| `scheduler-stopped` | No dispatching scheduler for 10 minutes | Schedules are not firing. `restart-service scheduler`; it exits if it loses its database lock, so check database connectivity too, and read its `logs`. |
| `dependencies-unreachable` | PostgreSQL or Valkey unreachable for 10 minutes | Check the data service, its security group or firewall, and its status page. |
| `not-ready` | App or API not answering for 15 minutes | Check `services-unhealthy` and the ingress certificate. |
| `disk-pressure` | Data disk over 85% for 15 minutes | Grow the disk, or move builds and files to object storage. |
| `backup-stale` | No snapshot for 30 hours, or no backup status | Check the snapshot policy or schedule in the console. |
| `certificate-expiring` | The ingress certificate expires within 14 days | Put the renewed chain and key in the operator secret, then `restart-service ingress`. |

Liveness, dependency reachability, and end-to-end readiness are separate
signals, and a missing heartbeat or backup status alerts rather than passing.

## Operating an installation

[`operator/selfhost.py`](operator/selfhost.py) sends one host agent command
and waits for the agent's report. It uses the `aws` or `gcloud` CLI with your
identity (or the release role), never SSH or network access to the host:

```sh
python3 infra/operator/selfhost.py installation.tfvars.json command status
python3 infra/operator/selfhost.py installation.tfvars.json command diagnostics
python3 infra/operator/selfhost.py installation.tfvars.json logs worker
python3 infra/operator/selfhost.py installation.tfvars.json command restart-service --service scheduler
python3 infra/operator/selfhost.py installation.tfvars.json release 3.0.2
```

Commands reach the agent through the SSM command document (AWS) or the
command topic (GCP). Each report carries the command's request ID and goes to
the installation's log: the `hot-agent-status` stream of `/hot/<installation>`
in CloudWatch Logs, or the `hot-<installation>-status` log in Cloud Logging.
AWS also keeps a summary of the latest report in the `/hot/<installation>/status`
parameter. The release and command workflows run the same script, so a
workflow run and a manual run behave the same.

| Command | Does |
| --- | --- |
| `status` | The deployed release, previous release, and migration level |
| `diagnostics` | Versions (release, migrations, host image, Kata), effective configuration, each service's state, health, and image digest, the Hot services' environment, data-service modes and reachability, and the current health metrics |
| `logs SERVICE` | The newest 200 lines (at most 48,000 characters) of a Hot service, the ingress, or a local data service |
| `backup` | An on-demand `pg_dump` of a local database to the data disk |
| `restart-service SERVICE` | Delivers secrets again and recreates one service; the scheduler stops before it starts |
| `drain-and-stop` | Stops the scheduler, then workers (2-minute grace), then the app, API, and ingress |
| `deploy-now` | Deploys the release pointer now (the `release` action sets the pointer first) |
| `roll-back` | After a failed upgrade: restores a local database from the pre-upgrade dump if the migrations started, then restarts the installed release |

Diagnostics and logs are redacted on the host before they are reported:
- every value in the host's secret files, and passwords inside connection URIs;
- URI credentials, bearer and basic credentials, JWTs, Hot API keys, AWS
  access key IDs, private keys, and `password=`/`token=`-style settings;
- the local part of email addresses, and all but the last four digits of AWS
  account IDs;
- secret references (ARNs and secret names) and CA certificates in the
  configuration, which is shown as set or unset.

Logs can still contain other user data, such as event payloads a service
logs. Treat a diagnostics bundle like the logs themselves. The command
workflow writes reports to its job summary, which anyone who can read the
operator repository's Actions runs can see.

## Versions and upgrades

Each release's signed manifest pins the Hot runtime image and the upstream
images by digest, and names its configuration schema, latest database
migration, and oldest supported source release. The rest is pinned in this
repository; a test keeps this table in step with those pins.

| Component | Version | Pinned in |
| --- | --- | --- |
| Hot runtime image | One digest per release | Release manifest |
| Runtime image base | Debian 13 (trixie) | `images/containers/runtime.Dockerfile` |
| Local PostgreSQL | 18 with pgvector 0.8.1 | `release/upstream-images.json` |
| Local Valkey | 8.1 | `release/upstream-images.json` |
| Ingress | nginx 1.30 | `release/upstream-images.json` |
| Managed PostgreSQL | 18 (RDS, Cloud SQL) | `opentofu/modules/single-machine-contract` |
| Managed Valkey | 8.1 (ElastiCache), 8.0 (Memorystore) | `opentofu/stacks/*/single-machine/valkey.tf` |
| Host OS | Ubuntu 24.04 LTS | `images/packer/hot-host.pkr.hcl` |
| Kata Containers | 3.28.0 | `images/host/setup-host.sh` |
| containerd (box tasks) | 1.7.30 | `images/host/setup-host.sh` |
| CNI plugins | 1.9.1 | `images/host/setup-host.sh` |
| OpenTofu | 1.13.1 | `selfhost-plan.yml` |
| AWS provider | 6.69.1 | Stack lockfiles |
| Google provider | 8.7.0 | Stack lockfiles |
| Random provider | 3.9.1 | Stack lockfiles |
| Configuration schema | 1 | `release/release_manifest.py` |

Upgrades follow these rules, and the deploy agent enforces them:

- **Supported jumps.** A release upgrades from its manifest's
  `upgrade.minimum_source` (set in
  [`release/upgrade-policy.json`](release/upgrade-policy.json)) or any later
  release. The agent refuses a larger jump; deploy a release in between first.
- **Order and downtime.** The agent stops the scheduler, readies the data
  services, takes a restore point if the schema changes, migrates and grants,
  then replaces the app, API, workers, and ingress, checks readiness, and
  starts the new scheduler. Schedules pause for the whole deploy. The
  previous release keeps serving requests and work until its containers are
  replaced, so it runs briefly against the migrated schema. For a full
  maintenance window, run `drain-and-stop` first. A single machine never runs
  two releases of the same service at once.
- **Restore points.** Hot never migrates a database down. Before a release
  adds migrations, the agent records where to recover from:
  - a local database gets a `pg_dump` in `/var/lib/hot/backups/pre-<release>-*`,
    and the last 3 are kept;
  - a managed database gets the time to restore to with RDS or Cloud SQL
    point-in-time recovery;
  - an existing database gets the time, for its operator's backups.

  A retried deploy keeps the first attempt's restore point.
- **Rollback.** Pointing back to a release with the same migration level is a
  normal deploy. A release with fewer migrations is refused. Recovery is a
  forward fix (a newer release) or a restore from the restore point, then the
  earlier release.
- **Failures.** A failed deploy reports the steps it finished and what
  recovery means:
  - If it failed before migrating, the database is unchanged, and the
    previous release's scheduler starts again.
  - If it failed during or after migrating, a forward fix or a restore is
    needed.

  The agent does not retry a failed release on its own; run `deploy-now`
  after fixing the cause, or point at another release. If the host stopped
  mid-deploy, the agent resumes when it starts again.
- **Records.** `/var/lib/hot/releases/history.json` keeps the last 20 deploys:
  - release and previous release;
  - image digests and host image versions;
  - configuration schema;
  - migration levels before and after;
  - steps, restore point, and any error.

  `diagnostics` shows the last 5.

## Moving data off the host

To move a single machine's database or queue to a managed service (or an
existing one), change `data.postgres.mode` or `data.valkey.mode` and apply,
then deploy a release (the same one is fine) with the release workflow. The
agent notices the change and moves the data as part of that deploy. Plan a
maintenance window: Hot is unavailable from the first step until the new
services are ready.

1. It stops admissions: the scheduler, app, API, and ingress.
2. If the queue is moving, it lets the workers finish queued work, for up to
   15 minutes, and checks with `hot queue status`. Dead-lettered items stay
   in the old Valkey on the data disk, and the deploy record counts them.
3. It stops the workers.
4. It dumps the local database to `/var/lib/hot/backups/move-<release>-*`.
   This dump is also the recovery point.
5. It prepares the new database. Then it restores the dump in one
   transaction, which is refused if the database already holds Hot's
   tables.
6. It migrates, grants, and starts Hot on the new services.

Until the restore succeeds, any failure restarts the installed release on the
untouched local data. After the restore, the move is complete: the local copy
is no longer used, a failure is fixed forward with `deploy-now`, and
`roll-back` refuses. The old data directories stay on the data disk until you
remove them.

Moving a database back onto the host, or between managed and existing
databases, is not automated. The agent refuses that change before stopping
anything. Copy the database with `pg_dump` and `pg_restore` yourself.

## Recovery

The host's boot disk holds nothing recovery needs. The installation's
configuration is in OpenTofu state and the instance's boot data. Its secrets,
including the encryption key in the generated secret, are in the cloud secret
store. Release pointer and status are in SSM or Secret Manager. Its data is on
the retained data disk and in its daily snapshots, or in the managed database.
A restore must use the same installation's generated secret: the local
database's logins and Hot's encrypted values depend on it.

These are design targets for the single machine. They have not been measured
yet; the restore drills in the plan measure them on each cloud.

| Failure | Data lost (recovery point) | Target time to recover | Procedure |
| --- | --- | --- | --- |
| Host VM lost or broken | None: the data disk is retained | 30 minutes | Replace the instance |
| Data disk lost or corrupted | Up to a day: since the last daily snapshot | 1 hour | Restore the data disk from a snapshot |
| Failed upgrade, local database | Writes since the upgrade began | 15 minutes | `roll-back` |
| Failed upgrade or data loss, managed database | Point-in-time recovery, within `backup_retention_days` | Provider restore time plus a deploy | Provider point-in-time recovery |
| Region loss | Not covered by the single machine | — | — |

**Replace the instance.** The data disk survives instance replacement, and
the new VM mounts it, delivers secrets, and deploys the release pointer:

```sh
tofu apply -replace=aws_instance.host
```

On GCP the instance has deletion protection; lift it, then replace it:

```sh
gcloud compute instances update hot-NAME --zone ZONE --no-deletion-protection
tofu apply -replace=google_compute_instance.host
```

**Restore the data disk from a snapshot.** Snapshots are named or tagged with
the installation (`HotDataVolume` on AWS, label `hot-installation` on GCP).
Stop managing the current disk without deleting it, set
`backup.restore_snapshot` to the snapshot, and apply with a new instance:

```sh
tofu state rm aws_ebs_volume.data          # GCP: google_compute_disk.data
tofu apply -replace=aws_instance.host      # with backup.restore_snapshot set
```

On GCP, lift the instance's deletion protection first, as above. The restored
GCP disk is named after its snapshot, so it does not collide with the kept
one. The old disk is left as it was; delete it in the console once the
restore is verified. `restore_snapshot` is read only when a disk is created,
so leaving it set changes nothing later.

**Roll back a failed upgrade.** When a deploy fails after its migrations
started, run `roll-back`, then point the installation back at the installed
release (the `release` action with that version):
- A local database is restored from the dump the agent took before the
  migrations, and the installed release is checked and restarted.
- For a managed or existing database, `roll-back` refuses and names the
  restore time. Restore the database to that time with RDS or Cloud SQL
  point-in-time recovery, or the operator's own backups, then deploy the
  installed release.

If the deploy failed before migrating, the database is unchanged and
`roll-back` only makes sure the installed release is running.

**After any restore,** run `diagnostics` and check the `Ready`, scheduler,
and dependency signals. Schedules that came due while Hot was down run when
the scheduler next evaluates them. Work queued in a local Valkey is restored
with the data disk; work queued after the snapshot is lost.

## State, drift, and teardown

**State.** Each installation's state is in the bucket named in its
`backend_override.tf`. That bucket is versioned and kept outside the
installation's stack, and the state is encrypted with the state passphrase.
GitHub's concurrency group and OpenTofu's lock serialize applies.

- **Stale lock.** If a run died holding the lock, the next plan reports the
  lock's ID, holder, and time. Confirm in the Actions tab that the run has
  ended, then run `tofu force-unlock ID`. Never unlock while an apply may
  still be running.
- **Lost or damaged state.** Restore an earlier version of the state object
  from the bucket's version history (`aws s3api list-object-versions`, or
  `gcloud storage ls --all-versions`). Never plan from empty state: it would
  try to create resources that already exist. If no version survives, adopt
  the resources with `import` blocks in `installation/imports.tf`, which the
  workflow copies into the stack. Start with the durable resources below,
  and repeat until the plan creates none of them.
- **Interrupted apply.** State keeps whatever finished; run the plan again
  and review it.

**Drift.** The example operator repository plans every morning with
`drift_check`, and any change fails the run with the changes in its summary.
A change can be:
- a console edit;
- a resource changed by AWS or Google;
- a merge that was never applied.

Apply the configuration, or change the configuration to keep what you want.
Nothing in the single-machine stacks is changed by a controller, such as an
autoscaler, that OpenTofu would fight.

**Durable resources.** Deleting these loses data, or the keys to read it:

| | AWS | GCP |
| --- | --- | --- |
| Data disk | `aws_ebs_volume.data` | `google_compute_disk.data` |
| Generated secrets (encryption key, passwords) | `aws_secretsmanager_secret.generated` | `google_secret_manager_secret.generated` |
| Operator secrets | `aws_secretsmanager_secret.operator` | `google_secret_manager_secret.operator` |
| Managed PostgreSQL | `aws_db_instance.postgres[0]` | `google_sql_database_instance.postgres[0]` |
| Managed Valkey | `aws_elasticache_replication_group.valkey[0]` | `google_memorystore_instance.valkey[0]` |
| Container logs | `aws_cloudwatch_log_group.host` | |
| Provisioned buckets | `aws_s3_bucket.storage` | `google_storage_bucket.storage` |

They are protected in several ways:
- The data disk and generated secret have `prevent_destroy`, so no plan that
  destroys them can be made.
- Managed databases have deletion protection (`data.postgres.deletion_protection`,
  on by default), and RDS keeps a final snapshot.
- Secrets Manager waits 30 days before it deletes a secret.
- Snapshots outlive their disk.
- The plan workflow fails any plan that deletes or replaces a durable
  resource, unless the caller sets `allow_data_deletion: true` for a
  reviewed purge.
- Existing databases, caches, and buckets are never managed by the stacks.

**Hand off a managed database.** To stop managing a database the stack
created without deleting it, for example to keep it as `existing`, remove it
from state before changing the mode. Run `tofu` locally with the
installation's backend, state passphrase, and apply credentials:

```sh
# AWS (data subnets only exist on a network the stack created)
tofu state rm 'aws_db_instance.postgres[0]' 'aws_db_parameter_group.postgres[0]' \
  'aws_db_subnet_group.postgres[0]' 'aws_security_group.postgres[0]' 'aws_subnet.data'
# GCP (the address and peering only exist on a network the stack created)
tofu state rm 'google_sql_database_instance.postgres[0]' 'google_secret_manager_secret.postgres_admin[0]' \
  'google_secret_manager_secret_version.postgres_admin[0]' 'google_secret_manager_secret_iam_member.postgres_admin[0]' \
  'google_compute_global_address.services[0]' 'google_service_networking_connection.services[0]'
```

Then set the mode to `existing` and put the connection settings in the
operator secret. The logins the agent created stay as they are. A database on
a network the stack created still needs that network, so keep the stack
until the database moves. Managed Valkey hands off the same way, with the
`valkey` resources in `valkey.tf`.

**Retire an installation.**
1. Run `drain-and-stop`.
2. Remove the durable resources you want to keep from state with
   `tofu state rm`.
3. Destroy the rest with `tofu destroy`; on GCP, lift the instance's deletion
   protection first.

Retained resources keep costing money until you delete them yourself:
- disks and snapshots, per GB-month;
- a database instance. RDS restarts a stopped instance after 7 days, so take a
  final snapshot and delete the instance instead;
- secrets;
- stored logs.

Purging is always a manual, separate step.

## Trust and identities

**Trust model.**
- Administrators are trusted: they hold the cloud roles below and Hot's
  administrator accounts.
- Code authors are not trusted with the installation: their code runs with
  mandatory isolation, and box tasks run in Kata VMs. User code has no cloud
  identity and no secret files. It cannot reach metadata, the host, the data
  services, or private ranges outside the allowlist; see
  [Outbound access for user code](../docs/plans/opentofu-cloud-self-hosting.md#outbound-access-for-user-code).
- Organizations are separated by PostgreSQL row-level security: the runtime
  login has neither `BYPASSRLS` nor schema rights. Storage keys are scoped
  per organization and environment.
- Accounts are by invitation: the bundle sets `HOT_APP_SIGNUP=invite`, and
  the first administrator is bootstrapped explicitly. To let anyone who can
  reach the app sign up, set `settings.HOT_APP_SIGNUP` to `open`. Open sign-up
  also needs SMTP, which verifies addresses.

**Identities.** Each identity can do only what is listed:

| Identity | Used by | Can | Cannot |
| --- | --- | --- | --- |
| Plan role | Plan job (OIDC, this repository) | Read the account or project, and read and lock state | Change resources |
| Apply role | Apply job (OIDC, approved environment only) | Create and change the stack's resources, and write state | Run anything on the host |
| Release role | Release and command workflows (OIDC, approved environment only) | Write the release pointer, send the Hot command to this host, and read the agent's status and reports | Read secrets, open a shell or Session Manager session, or use SSH |
| Host identity | The agent, Docker's log drivers, and Hot services | Read the release pointer and this installation's secrets, and create the generated secret once. Write status, logs, and metrics. Read snapshot times, and use objects under its storage prefixes | Delete snapshots, read other secrets, or change infrastructure |
| `hot_owner` login | `migrate` and `provision` jobs only | Own and migrate the schema, and grant the runtime login | Serve requests |
| Runtime login | Hot services | Read and write rows, filtered by row-level security | Change the schema or bypass row-level security |
| Managed database administrator | `prepare-postgres` job only | Create the logins, database, schema, and extensions | Serve requests |

The plan role uses AWS's `ReadOnlyAccess` or GCP's `roles/viewer`; a
narrower custom policy also works. The apply role's policies are yours to
choose (`apply_policy_arns`). OIDC trust is limited to the operator
repository, and for apply and release to the approved environment.

**Shared identity on one machine.** The Hot services share the VM's
identity: S3 or GCS access and log shipping need it, so AWS allows a
metadata hop limit of 2. A compromised Hot service container, as opposed to
isolated user code, could therefore read this installation's secrets. User
code cannot: its outbound traffic never reaches metadata, and it has no
secret files. Separate identities per service arrive with the scalable
preset.

## Security updates

**Supported releases and advisories.** As in
[SECURITY.md](../SECURITY.md), security fixes ship in the latest release
only. Watch the repository's releases and security advisories, and report
vulnerabilities privately as SECURITY.md describes.

**What a release pins:**
- the runtime image (Debian packages, Hot, and its Rust dependencies), signed
  with cosign, with build provenance and an SBOM attached by the build;
- the PostgreSQL, Valkey, and ingress images by digest;
- a signed manifest naming all of these, which the agent verifies before it
  deploys.

Each release also attaches `hot-runtime-vulnerabilities.json`, a Trivy scan
of the runtime image, for triage. The scan never blocks a release.

**Applying a security release.**
1. Deploy it with the release workflow: the agent verifies the manifest,
   takes a restore point if the schema changes, migrates, replaces the
   services, and checks readiness.
2. If the release notes say the host image changed, rebuild it with
   `images/build-host.sh` and set `host_image`. The apply replaces the
   instance and keeps the data disk.

Between host image rebuilds, Ubuntu's unattended security updates (on by
default) patch the host's packages. Kernel updates take effect when the
instance is replaced. Rollback follows [Versions and upgrades](#versions-and-upgrades).

Mirroring images to your own registry is not supported yet: manifests name
`ghcr.io` and Docker Hub images by digest.

## Capacity, retention, and cost

**Sizes.** One VM runs everything. These numbers are the configuration's
limits; they have not been benchmarked:

| Size | VM | Data disk | Concurrent tasks (code, container) | Box storage |
| --- | --- | --- | --- | --- |
| `small` | 2 vCPU, 8 GB (`m8i.large`, `n2-standard-2`) | 100 GB | 2 code, 1 container | 40 GB |
| `medium` | 4 vCPU, 16 GB (`m8i.xlarge`, `n2-standard-4`) | 200 GB | 4 code, 2 container | 100 GB |
| `large` | 8 vCPU, 32 GB (`m8i.2xlarge`, `n2-standard-8`) | 500 GB | 8 code, 4 container | 200 GB |

A size is not an availability or throughput promise. The single machine is
one failure domain.

**At capacity.**
- Tasks beyond the concurrent limit wait in the queue, and the work queue
  grows in Valkey, which never evicts (`noeviction`). When Valkey's memory
  is full, new work is refused with errors rather than dropped.
- Box tasks cannot exceed their storage pool.
- Services have memory limits: one that exceeds its limit is restarted
  without taking the others down.
- `disk-pressure` alerts at 85% of the data disk.

Move to a larger size (`size`, then apply; the instance is replaced and the
disk kept), or move builds and files to object storage.

**Retention.**

| What | Kept for | Set by |
| --- | --- | --- |
| Data-disk snapshots | 7 days | `backup.retention_days` |
| Container logs in CloudWatch | 30 days | `logging.retention_days` |
| Container logs in Cloud Logging | The log bucket's retention (30 days by default) | Cloud Logging |
| Local container logs | 3 files of 10 MB per container | Fixed |
| Pre-upgrade database dumps | The last 3 | Fixed |
| On-demand `backup` dumps | Until you delete them | You |
| Deploy history | The last 20 deploys | Fixed |
| Managed database backups | 7 days, with point-in-time recovery | `data.postgres.backup_retention_days` |

Execution history, builds, and files are kept by Hot's own settings.

**Cost drivers.** The VM runs all the time and dominates the cost. Then come:
- the data disk and its snapshots;
- NAT for outbound traffic (AWS NAT gateway, GCP Cloud NAT) and, when
  enabled, the load balancers for public endpoints;
- managed PostgreSQL and Valkey instances (high availability doubles them);
- log ingestion and storage, metrics, and alarms;
- Secrets Manager secrets on AWS.

Use your cloud's budgets and cost alerts. Hot does not cap spending.

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
cloud stack publishes PostgreSQL/Valkey or port 22. Customer-managed keys and
provisioning a bucket in the other cloud are outside this slice; their
implementation remains in the plan.
