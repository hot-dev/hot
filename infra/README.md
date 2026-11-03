# Self-host infrastructure

This directory contains the first local implementation of the cloud self-hosting
[plan](../docs/plans/opentofu-cloud-self-hosting.md). It is an **unqualified
development foundation**, not a supported installation release.

Implemented:

- A common single-machine input contract, with private ingress and independent
  Local/existing S3 choices for builds and files/blobs. An installation's name
  ends in a letter or digit. `network.cidr` and `client_cidrs` are IPv4 networks
  without host bits, in prefix form, and a new AWS network is /16 to /20 (/19 for
  a single machine with a public endpoint). S3 and GCS prefixes take letters,
  digits, `.`, `_`, `-`, and `/` separators, and an S3 region lowercase letters,
  digits, and `-`. Plan refuses anything else.
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
  it with Sigstore. Hosts pull it without signing in, so the package must be public.
  The first `selfhost-image` run creates it as private, as does any earlier release
  or preview run, so a maintainer sets it public once, at
  <https://github.com/orgs/hot-dev/packages/container/hot-runtime/settings>. GitHub
  cannot undo this and offers no API for it. Until then `selfhost-release-assets`
  fails at "Check that hosts can pull the runtime image" and attaches no manifest;
  after setting it public, re-run that job.
- Signed release manifests: [`release/release_manifest.py`](release/release_manifest.py)
  pins the runtime image and the upstream images in
  [`release/upstream-images.json`](release/upstream-images.json). The release
  workflow signs the manifest keylessly; the renderer verifies it with
  `cosign verify-blob` against the release workflow's tag identity.
- The host image recipe ([`images/host/setup-host.sh`](images/host/setup-host.sh),
  [`images/packer`](images/packer), built by
  [`images/build-host.sh`](images/build-host.sh)): Ubuntu 24.04 with Docker,
  Kata on QEMU, a dedicated containerd, CNI, `cosign`, a guest image, and a fallback
  copy of `hotbox`. Each start of a task worker installs its release's own `hotbox`,
  and boxes use that ([Security updates](#security-updates)); they never run the
  copy in the guest image. SSH is masked, and Packer builds without SSH. Kata
  state lives on the data disk and stays off until it is mounted; box storage
  is capped by `HOT_BOX_POOL_GB`, and each box's `/data` is a file beside it, which
  Hot's disk budget limits ([At capacity](#capacity-retention-and-cost)); a
  mangle-table baseline keeps guests off metadata, the host, other guests, and
  private ranges outside the allowlist, and with an egress proxy limits them to the
  proxy and their resolvers. Docker
  runs with no default bridge (no `docker0`, no `172.17.0.0/16`), and its address
  pool is the runtime subnet `172.30.0.0/24`, so a Docker network added to a host
  later must name its own subnet.
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
  may use only its roles' prefixes. On GCS the stack sets a provisioned bucket's
  whole IAM policy: project owners and editors, and the host under its prefixes,
  with no `projectViewer` bindings. The next apply removes any other grant, including
  another installation's, so a bucket two installations share must not be
  provisioned by either.
- Settings: `settings` gives every Hot service non-secret `HOT_*` values. Plan
  refuses, and the renderer refuses on the host:
  - names that look like secrets (`SECRET`, `PASSWORD`, `TOKEN`, `_KEY`, `_URI`),
    and a `HOT_NETWORK_EGRESS_PROXY` that carries credentials: these go in the
    operator secret;
  - a `NAME_FILE` setting, since no container has that file: set `NAME` in
    `settings`, or in the operator secret if it carries a credential;
  - what the bundle sets: isolation, private networking, queue and log options,
    hosts, URLs, and ports, the database and its schema, row-level security, the
    store type, task and box sizing, worker threads, storage, client-address
    handling, and the models path. That includes `HOT_API_KNOWN_HOSTS`, `HOT_APP`,
    `HOT_APP_PORT`, `HOT_API`, `HOT_API_PORT`, `HOT_DB`, `HOT_DB_SCHEMA`,
    `HOT_DB_RLS_ENFORCE`, `HOT_RLS_ENFORCE`, `HOT_STORE_TYPE`,
    `HOT_STORE_MODELS_PATH`, `HOT_TASK_DATA_VOLUME_DIR`,
    `HOT_TASK_CONTAINER_RESERVED_MEMORY_MB`, and `HOT_TASK_CODE_MEMORY_PERCENT`;
    `HOT_WORKER_DB_MAX_CONNECTIONS` on a scalable installation; and
    `HOT_REDIS_AUTH` and `HOT_REDIS_CA_CERT` on managed Valkey on GCP. `HOT_APP`,
    `HOT_API`, and `HOT_DB` each take JSON that replaces a whole section.

  `HOT_NETWORK_ALLOW_CIDRS` and `HOT_NETWORK_DENY_CIDRS` take addresses and
  prefix-form networks, not netmasks or IPv6 zones. The operator's deny entries
  reach workers, task workers, and box guests, with the bundle's own added:
  `172.30.0.0/24`, `10.88.0.0/16`, a stack-created network, the data services and
  their networks (`data.<service>.endpoint.deny_cidrs`), and the host.
  `172.17.0.0/16` is an ordinary range, which an allowlist can open.
  `HOT_EMAIL_*` other than credentials and `HOT_APP_SIGNUP` belong here. Values
  reach containers literally: `$` is not interpolated.
- Existing networks: set `network.mode` to `existing`, with `network_id` and
  `subnet_id`. On AWS, a public endpoint also needs `public_subnet_ids` in two
  zones, including the host's zone on a single machine, and managed data needs
  `data_subnet_ids` in two zones. On GCP, give `network_id` as
  `projects/PROJECT/global/networks/NAME`: a bare name means the installation's
  own project. A Shared VPC does not work today: preflight looks up `subnet_id`
  and the network's routes in the installation's project, so a subnet in a host
  project fails preflight, and the plan workflow stops there.
- Existing data services: set `data.postgres.mode` or `data.valkey.mode` to
  `existing` and put the connection settings in the operator secret
  (`HOT_DB_URI` for the runtime login, `HOT_DB_MIGRATION_URI` for the schema
  owner, `POSTGRES_CA_CERT`, and `HOT_REDIS_URI`). Prepare the database as
  [Self-Hosted PostgreSQL](../resources/docs/configuration/postgres.md)
  describes; the bundle checks it, migrates, and grants the runtime login,
  and never creates or drops anything the operator owns. The stack does not know
  where the service runs, so list the networks of its standbys and replicas in
  `data.<service>.endpoint.deny_cidrs`.
- Managed PostgreSQL: set `data.postgres.mode` to `managed` and the stack
  provisions PostgreSQL 18 on RDS or Cloud SQL, reachable only from the host
  and only over TLS. It is sized from the installation size unless you set
  `tier` or `storage_gb`; `high_availability` selects Multi-AZ or a regional
  instance; backups are kept for `backup_retention_days` (default 7) with
  point-in-time recovery. On AWS, turning `high_availability` off keeps the
  instance in its current zone. On each deploy the agent signs in as the database
  administrator to create the `hot_owner` and runtime logins, the `hot`
  database and schema, and the `vector` and `pg_trgm` extensions, then checks,
  migrates, and grants as for an existing database.
  - On GCP, `storage_gb` and the size set only the starting disk, which Cloud
    SQL grows as it fills ([Resizing](#resizing)), and the stacks set
    `max_connections` to the size's budget (400, 800, or 1,600).
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
    in two zones. An existing GCP network must already have private services
    access, and the stack cannot look up its range, so list that in
    `data.postgres.endpoint.deny_cidrs` (find it with `gcloud compute addresses list
    --global --filter=purpose=VPC_PEERING`). Preflight warns while that is empty
    ([An existing network](#trust-and-identities)).
- Managed Valkey: set `data.valkey.mode` to `managed`. It is sized from the
  installation size unless you set `node_type`, never evicts queue entries,
  and requires TLS. With `high_availability`, a replica runs in another zone.
  - AWS: an ElastiCache Valkey 8.1 replication group with automatic failover.
    It requires an AUTH token, which, like the GCP database password, is
    generated during apply and written only to write-only attributes, on
    ElastiCache and on a Secrets Manager secret. Raise
    `data.valkey.auth_token_version` to set a new one. Do the same if an apply
    that creates the cache fails after the cache exists: the retry stores a new
    token the cache does not have, and hosts fail to log in until it is raised.
  - GCP: a Memorystore for Valkey 8.0 instance with cluster mode disabled,
    with append-only persistence, reached through Private Service Connect.
    It is multi-zone either way, because that cannot change after creation. Without
    `high_availability` it has no replica, and Memorystore chooses the primary's zone,
    which may not be the host's, so queue calls may cross zones. Turning
    `high_availability` on later adds a replica in another zone and replaces nothing.
    It uses IAM authentication, so there is no password at all: Hot services
    run with `redis.auth = gcp-iam`. They present the VM's short-lived access
    tokens, refreshed before they expire, and trust the instance's CA
    (`redis.ca-cert`). The host may connect to this instance only
    (`roles/memorystore.dbConnectionUser` with a condition). On a new
    network the stack adds the service connection policy Memorystore needs.
    An existing network must already have one for `gcp-memorystore` in the
    region; preflight checks for it. Hot checks the server certificate's name as
    well as its chain, and whether that passes against a real instance is not
    yet known ([Real-account qualification](#real-account-qualification)).
- Data networks: `data.postgres.endpoint.deny_cidrs` and
  `data.valkey.endpoint.deny_cidrs` list networks where a managed or existing
  service's standbys and replicas can be, which workers, task workers, and box
  guests are denied, since a service's address can move. The stacks add what they
  know ([An existing network](#trust-and-identities)). Each entry is a network with
  an explicit prefix narrower than `/0` and no host bits, with IPv4 written as IPv4:
  `10.68.4.0/24`, not `10.68.4.1/24` or `10.68.4.1`. A bare address or a `/0` goes in
  `settings.HOT_NETWORK_DENY_CIDRS`. A local service refuses the key, since the
  bundle's own network is denied already. Plan and preflight fail on any key the
  contract does not know under `data`, `data.<service>`, or
  `data.<service>.endpoint`, because OpenTofu would otherwise drop it with no error,
  and a misspelled deny list would leave its networks open.
- Public endpoints: set `app_exposure` or `api_exposure` to `public`. On a
  single machine, AWS needs `public_certificate_arn` (an ACM certificate you
  DNS-validate), and GCP issues a managed certificate for each public hostname
  once its DNS points at the `public_endpoint` output. A VM keeps no public
  address, and private endpoints are never served to the load balancer. A
  scalable installation uses no cloud certificate: TLS passes through to the
  hosts' ingress ([Scalable installations](#scalable-installations)).
- Backups, logs, and alerts: see [Backups, logs, and alerts](#backups-logs-and-alerts).
- Preflight: [`preflight/preflight.py`](preflight/preflight.py) checks an
  installation's account or project before provisioning, and the plan
  workflow runs it first. See [Preflight](#preflight).
- In Hot: SMTP email delivery, `hot admin bootstrap`/`reset-password` and the
  `app.signup` policy for operator-managed accounts, and outbound allowlists
  (`network.allow-cidrs`, `allow-hosts`, `deny-cidrs`) behind a fixed deny set.
- Scheduler lock liveness: the scheduler confirms its PostgreSQL singleton lock
  every two seconds and exits non-zero when it loses the lock, so its
  supervisor restarts it.

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
# For an IAM user: the bootstrap's roles trust users only with MFA.
mfa_serial = arn:aws:iam::123456789012:mfa/ada
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

# Either bootstrap, for GitHub's OIDC. `gh api repos/OWNER/REPO --jq '.id, .owner.id'`
github_repository          = "OWNER/REPO"
github_repository_id       = "123456789"
github_repository_owner_id = "987654321"
```

An identity is created when GitHub uses it, when operators are named for it,
or both:
- **GCP:** each operator gets `roles/iam.serviceAccountTokenCreator` on that
  service account. Whoever applies the bootstrap needs to create custom roles
  (`roles/iam.roleAdmin`) and, with `kms_key`, to set the key's IAM policy; that
  input is a full key name in the bucket's location. The bootstrap enables the
  `iam`, `iamcredentials`, `sts`, and `cloudresourcemanager` APIs, and `pubsub` when
  a plan identity exists, whose custom role lists Pub/Sub permissions, so a first
  apply may need a second run while they propagate.
- **AWS:** each operator is added to the role's trust policy for
  `sts:AssumeRole`. Operators are IAM role or user ARNs, not account roots, and
  a user's session must have MFA. The bootstrap never destroys the account's
  GitHub OIDC provider: clearing `github_repository`, or setting
  `create_github_oidc_provider = false`, fails at plan until you run
  `tofu state rm 'aws_iam_openid_connect_provider.github[0]'`.

With `github_repository` set, both repository IDs are required, and trust is
pinned to them. Renaming the repository or its owner means updating
`github_repository`.

The plan and apply identities read state and take its lock, and only apply
changes a state that exists. On GCP the plan identity can also create a state
object that is missing (`roles/storage.objectCreator`, on names ending in
`.tfstate`), since the `gcs` backend writes an empty state the first time it
opens one, and a new installation's first plan needs that. It cannot overwrite
or delete one, but a pull request that runs under it can create a state object
for an installation name with no state yet. The release identity's permissions
come from each installation's `release_principal`.

**Bring your own identity** instead: give it the apply permissions, plus
read, write, and delete on the state objects (`roles/storage.objectUser` on
the bucket, or `s3:GetObject`, `PutObject`, `DeleteObject`, and
`ListBucket`). For release and host commands, set it as the installation's
`release_principal`.

In every case, provide the state passphrase as
`TF_VAR_state_encryption_passphrase` and use the installation's
`backend_override.tf`. Both bootstraps print one: `tofu output -raw
backend_override`, with `NAME` to replace. On S3 it must set
`use_lockfile = true`, or nothing locks state; see
[State, drift, and teardown](#state-drift-and-teardown). With a KMS state key,
also set `kms_key_id`.

## Preflight

```sh
python3 infra/preflight/preflight.py installation/installation.tfvars.json
```

Preflight runs read-only `aws` or `gcloud` commands with your identity; the
plan role or service account is enough. It reads no secret values. It checks:

- the zone, machine types, vCPU quota, and host image. For a scalable
  installation it counts every role at its `scaling` maximum, plus the
  scheduler. Quota fails only when a limit is below the need; current use is not
  counted on AWS, and on GCP it warns, since it includes the installation's own
  hosts once they run. On GCP, E2 counts against `CPUS`, N2 against `N2_CPUS`,
  and the total against the project's `CPUS_ALL_REGIONS`;
- on GCP, the nested-virtualization organization policy and the APIs. The stack
  enables most of them, and until `compute.googleapis.com` (or `sqladmin` for a
  predefined Cloud SQL tier) is on, the checks that call it warn "not checked
  until ... is enabled"; a later plan runs them. No stack enables `iam`,
  `cloudresourcemanager`, `iamcredentials`, or `sts`, which the bootstrap does
  when it creates deploy identities. Those warn too: an apply needs the first
  two here or in its identity's project (an identity in this project otherwise
  gets a Google error at the first service account), and GitHub jobs and
  impersonation need the others;
- that network ranges avoid the host's container network (`172.30.0.0/24`)
  and box guest network (`10.88.0.0/16`). A `client_cidrs` range inside one of
  them fails; a wider one, such as `10.0.0.0/8`, warns, since only the clients
  inside those networks cannot reach the hosts;
- existing networks and their subnets;
- that the hosts' subnets on an existing network can reach the internet, which
  hosts need for images, release manifests, and cloud APIs.
  - On AWS that means a default route through a NAT or other gateway, not an
    internet gateway, since hosts get no public address.
  - On GCP it means an untagged `0.0.0.0/0` route. Through the default internet
    gateway, a Cloud NAT must cover the subnet's primary range, and Private
    Google Access off is a warning. Through an appliance, an internal load
    balancer, or a VPN it warns that it cannot verify the path. With no static
    default route, or no covering NAT, it warns if a Cloud Router has BGP peers
    or a peering imports custom routes, since it cannot see learned routes, and
    otherwise fails;
- with `image_registry`, that the mirror has a repository for each image (ECR)
  or is a Docker repository (Artifact Registry). A missing repository fails. If this
  identity may not look, as is usual for a mirror in another account or project, it
  warns, and the first deploy's image pull checks it;
- on an AWS single machine, the public certificate, and that
  `/hot/NAME/release` is free, tagged as this installation's, or adopted by an
  `imports.tf` beside the tfvars;
- that buckets to provision are free or already this installation's: tagged
  `HotInstallation=NAME` (S3) or labeled `hot-installation=NAME` (GCS) by its own
  apply. A bucket owned by anyone else fails;
- existing buckets.
  - Each GCS bucket must have uniform bucket-level access. One labeled
    `hot-installation` with another installation's name fails, because that
    installation's stack sets the bucket's whole IAM policy; remove a stale label
    with `gcloud storage buckets update gs://BUCKET --remove-labels=hot-installation`.
  - On an AWS installation, a missing S3 bucket fails. A bucket this identity may
    not read warns rather than fails, since no plan reads it with this identity and
    another account's bucket answers 403 unless its policy names the plan role. S3
    answers the same for a mistyped name, and nothing tries the bucket before the
    first build or file write.
  - On an AWS installation, an existing S3 bucket whose default encryption uses a
    customer-managed KMS key warns: the stacks grant the host roles S3 actions only,
    so the key's policy must let `hot-NAME-host` (on scalable, `hot-NAME-ROLE` for
    each role) use it (`kms:GenerateDataKey`, `kms:Decrypt`).
  - On a GCP installation, preflight does not look at S3 buckets, because the plan
    identity has no AWS access: a missing or mistyped bucket gets no failure and no
    warning. A deploy does not check it either. Nothing tries the bucket before the
    first build or file write, and a move onto it finds out only after it has stopped
    Hot, so check the name and the keys in the operator secret yourself;
- that the contract knows every key under `data`, `data.<service>`, and
  `data.<service>.endpoint`. Any other fails, because OpenTofu would drop it with no
  error;
- managed data. On AWS: the RDS engine version on the instance class, Valkey
  8.1, and the Valkey node type, which warns when ElastiCache lists no offering.
  On GCP: the Cloud SQL tier (a well-formed `db-custom` shape, or a predefined
  tier offered in the region) and the Memorystore node type, which warns when it
  is unknown. On an existing GCP network: private services access, which warns while
  `data.postgres.endpoint.deny_cidrs` is empty, and Memorystore's service connection
  policy. The GCP PostgreSQL version and Memorystore's regional availability are not
  checked; the apply reports those.

It warns when hostnames don't resolve from where it runs. It also warns when
`admin_email` is empty and no first account could be created unless the database
already has users: when sign-up is by invitation, and when sign-up is open but
settings lack `HOT_EMAIL_PROVIDER=smtp`, `HOT_EMAIL_SMTP_HOST`, or a `HOT_EMAIL_FROM`
of the installation's own, or hold a malformed SMTP port, security, or timeout.
Open sign-up through Google or GitHub needs none of that, but their credentials are
in the operator secret, which preflight does not read. It lists the checks only the
installation's host can make, such as an existing database's TLS and logins, which
the agent runs before migrating. It exits 1 when a required check fails, and the
plan workflow stops there.

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
only. Logs are lost with the host then, and on a scalable GCP installation
`logs` is refused and no command can read them, so use `cloud` there.

Every line names the host and release that wrote it, which matters once a
scalable installation runs many hosts. On AWS each container's stream is
`<instance ID>/<container>`. Cloud Logging records the instance and the
container's `hot.installation`, `hot.role`, and `hot.release` labels.

Hot's services log one JSON object per line. Every line the app or API logs
while handling a request carries that request's `request_id` in its span, and
the response returns it as `x-request-id`. A caller's own `x-request-id` is
kept when it is a short token (up to 64 letters, digits, `-`, `_`, `.`), so an
ID set by a load balancer or client can be followed through the logs, and the
ingress's access log records the same ID. Request spans record the method and
path, never the query string or headers, and webhook paths hide their token.
Workers and task workers name the `run_id` or `task_id` in the lines about one
run or task.

The agent also ships the host journal. That is every agent line, plus
warnings and errors from the rest of the host, such as the kernel's OOM
kills, Docker, and failed units. They go to the `host-journal` stream of the
same log group, or to the `hot-<installation>-host` log in Cloud Logging. It
reads from a saved journal cursor, so entries are not lost or sent twice
across restarts. Each source sends at most 500 entries per cycle, redacted
like diagnostics. Every entry names its `instance`, which is how to tell hosts
apart in the one stream a scalable AWS installation shares. With
`logging.destination = local`, the journal stays on the host too.

Every 30 seconds the host agent publishes these metrics, to CloudWatch
(namespace `Hot/SelfHost`, dimension `Installation`) or Cloud Monitoring
(`custom.googleapis.com/hot/<installation>/...`):

| Metric | Meaning |
| --- | --- |
| `AgentHeartbeat` | 1 while the agent runs. During a deploy, data move, or command, every 60 seconds, for up to 6 hours of one step |
| `UnhealthyServices` | Hot services, ingress, and local data services not running and healthy |
| `SchedulerRunning` | 1 while the scheduler runs and dispatches: it rewrites a heartbeat file every 30 seconds, and its health check fails after 2 minutes without one. The heartbeat stops if its last enqueue failed, or if its database sync, retry, or `@at` loop has not finished a pass in the longer of 90 seconds and 3 of its intervals, so a queue that refuses writes reads as stopped. It logs the reason every 30 seconds at error level |
| `DependenciesReachable` | 1 when local data services are healthy and managed or existing ones accept connections. Their addresses are read again every 5 minutes, so monitoring follows a changed URI |
| `Ready` | 1 when the app and API answer through the ingress |
| `DataDiskUsedPercent` | The larger of space and inodes used on the data disk |
| `BackupAgeHours` | On a single machine: hours since the newest completed snapshot |
| `CertificateDaysLeft` | Days until the ingress TLS certificate expires (0 if unreadable) |
| `QueueBacklog` | Jobs and tasks waiting or running in Hot's queue |
| `QueueDeadLetters` | Jobs dead-lettered after failing every attempt |
| `NewDeadLetters` | Jobs dead-lettered since the previous measurement. The previous count is on the host's disk (`releases/queue.json`), so an agent restart does not hide them; a replaced scalable scheduler host starts counting again |
| `JobsWaiting`, `JobWaitSeconds` | Jobs no worker has taken yet, and how long the oldest has waited |
| `TasksWaiting`, `TaskWaitSeconds` | The same for tasks, which task workers take |
| `MemoryUsedPercent` | Host memory in use: all of it except what the kernel reports as available |
| `SyntheticOk`, `SyntheticSeconds` | With `monitoring.synthetic_event`: whether the latest synthetic run succeeded, and how long it took. One point per check, about every 5 minutes |
| `TaskSlotsUsedPercent` | On task-worker hosts: the busiest of the task worker's code slots, container slots, and container memory, in use |

Cloud Monitoring's names are these in snake case: `TaskSlotsUsedPercent` is
`custom.googleapis.com/hot/<installation>/task_slots_used_percent`. A host's series
there is labelled with the instance's numeric `instance_id`, not its name.

The host that runs the scheduler measures the queue every minute with
`hot queue status --json`, inside the scheduler: one measurement for the
installation. On GCP these queue metrics are written for the `global`
resource (the project), not for an instance, so a group can scale on them.
The task worker rewrites its slots in use every 15 seconds
(`task.status-path`), and the agent publishes them with the other metrics.

The stacks alert on them. Set `monitoring.alert_emails`; AWS sends each address
an SNS confirmation email first. Alerts reach those addresses independently of
Hot's own email, and both firing and resolving are sent.

| Alert | Fires when | First steps |
| --- | --- | --- |
| `host-down` (`hosts-down` on scalable) | No heartbeat for 5 minutes; on scalable, no heartbeat from any host, so one silent host does not fire it. An AWS single machine also has `instance-impaired`, for failing EC2 status checks | Check the VM in the console; `diagnostics` once it answers. The agent restarts with the VM. A wedged Docker daemon fires it too, since no metrics go out, and so does one step of the agent running over 6 hours, even if it is still working: the journal then says `heartbeat_stopped`. |
| `services-unhealthy` | A service unhealthy for 15 minutes | `diagnostics` names it; `restart-service`, then read its logs. |
| `scheduler-stopped` | No dispatching scheduler for 10 minutes | Schedules are not firing. `restart-service scheduler`, which is refused while a failed or interrupted deploy's possibly partly applied migrations hold it; the refusal says what clears the hold. The scheduler exits if it loses its database lock, so check database connectivity too, and read its `logs`. A replacement host's scheduler keeps restarting until a dead host's database session gives up the lock. A deploy does not fail on this, so this alert is what reports a lock that never frees. |
| `dependencies-unreachable` | PostgreSQL or Valkey unreachable for 10 minutes | Check the data service, its security group or firewall, and its status page. |
| `not-ready` | App or API not answering for 15 minutes | Check `services-unhealthy` and the ingress certificate. The ingress starts without the app or API and answers 502 until their names resolve, and it can use an old address for up to 10 seconds after one moves. |
| `disk-pressure` | Data disk over 85% for 15 minutes | Grow the disk with a larger size. Moving builds and files to object storage stops them growing on the disk, but no command deletes what is there ([Capacity, retention, and cost](#capacity-retention-and-cost)). |
| `backup-stale` | Single machine only: no snapshot for 30 hours, or no backup status | Check the snapshot policy or schedule in the console. |
| `certificate-expiring` | The ingress certificate expires within 14 days | Put the renewed chain and key in the operator secret, then `restart-service ingress`. |
| `queue-backlog` | More than `monitoring.queue_backlog_alert` jobs (default 1000) queued for 15 minutes | Workers are not keeping up. Check `services-unhealthy` and the worker's `logs`; then move to a larger size, or on a scalable installation raise `scaling.worker.max` or `scaling.task_worker.max`. |
| `job-wait` | A job has waited more than `monitoring.queue_wait_alert` seconds (default 900) for 10 minutes | As for `queue-backlog`; a scalable installation at `scaling.worker.max` needs a higher maximum. |
| `task-wait` | A task has waited more than `monitoring.queue_wait_alert` seconds for 10 minutes | Task capacity is short: check the task-worker's `logs` and `TaskSlotsUsedPercent`, then raise `scaling.task_worker.max` or the size. |
| `new-dead-letters` | Jobs were dead-lettered since the last measurement | Read the worker's `logs` for the failing jobs' errors. |
| `memory-pressure` | A host over 90% memory for 15 minutes | Read `diagnostics` and the host journal for OOM kills; move to a larger size. |
| `database-connections` | Managed PostgreSQL over 80% of the size's planned connections (400, 800, or 1,600) for 15 minutes | Look for connection leaks in the services' `logs`; lower the scaling maximums or choose a larger size. |
| `synthetic-failed` | The synthetic event's run has not succeeded for 10 minutes (only with `monitoring.synthetic_event`) | Something fails end to end. Check `services-unhealthy`, `dependencies-unreachable`, and the queue alerts, then the API's and worker's `logs` for the synthetic event. |
| `alert-test` | The `test-alert` command was sent | Nothing is wrong: it checks the path to your inbox, and resolves by itself within about 15 minutes. |

On a scalable GCP installation the helper refuses `restart-service` and `logs`,
because a command reaches only one host. Read the logs in Cloud Logging, and replace
the role's instances instead of restarting
([Rotating secrets and certificates](#rotating-secrets-and-certificates)).

Liveness, dependency reachability, and end-to-end readiness are separate
signals, and a missing heartbeat or backup status alerts rather than passing.

Each stack also creates a dashboard named `hot-<installation>`, in
CloudWatch or Cloud Monitoring. It plots these metrics: health, the queue and
its waits, task slots, disk, certificate, and backup age.

**A synthetic check** runs Hot end to end from inside the installation's
network: a request through the API, a queued event, a worker run, and the
database. To turn it on:
1. Deploy a Hot project with a handler for an event type of your own, such
   as `ops:ping`, that succeeds quickly.
2. Put an API key that may publish and read that event in the operator
   secret as `HOT_SYNTHETIC_API_KEY`.
3. Set `monitoring.synthetic_event` to the event type.

Every 5 minutes, each host serving the API publishes the event through its
own ingress and waits up to a minute for the run to succeed. It reports
`SyntheticOk` and `SyntheticSeconds`; nothing is exposed publicly. Each check is
one data point, so `synthetic-failed` needs consecutive failed checks.

**Test the alert path** after installing and whenever the recipients change:
run the `test-alert` command (below). The agent publishes a test metric, and
the `alert-test` alert fires within a few minutes and then resolves. Both
messages reach `monitoring.alert_emails` the same way a real alert would.

**Silence alerts during planned work**, such as a resize or a restore:

```sh
python3 infra/operator/selfhost.py installation.tfvars.json maintenance start --minutes 90
python3 infra/operator/selfhost.py installation.tfvars.json maintenance end
```

- On AWS this disables the actions of the alarms that notify the installation's
  topic, `hot-NAME-alerts`, until `maintenance end`. The alarms still change
  state. An apply keeps the actions off, and the plan workflow's drift check does
  not report them, so a forgotten `maintenance end` silences paging until someone
  ends it. Until then `status` and `diagnostics` add `silenced`, naming the alarms,
  and a `warning`, and their exit code does not change. If the caller may not read
  the alarms, the `warning` says it could not check. An alarm an apply creates
  meanwhile starts with its actions on. CloudWatch notifies only on a change of state,
  so `maintenance end` lists alarms still in ALARM under `alarming` with a `warning`:
  they stay quiet until they recover and breach again, so check them then.
- On GCP it snoozes the policies that watch the installation's
  `custom.googleapis.com/hot/NAME/` metrics or its Cloud SQL instance. The snooze
  ends after `--minutes` (default 60), or at `maintenance end`.
- Either way another installation whose name starts with this one's (`prod` and
  `prod-eu`) is left alone.
- Nobody is notified while alerts are silenced, so end maintenance when the
  work is done.
- The release role is allowed to do both. This has not run on a real account
  ([Real-account qualification](#real-account-qualification)).

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
parameter, which the release role does not read. The release and command workflows
run the same script, so a workflow run and a manual run behave the same, given the
same `preset`. By hand, a command waits 10 minutes (`logs` 5) unless `--timeout`, in
seconds, says otherwise. The flag goes before the action, and is refused after it:
`selfhost.py installation.tfvars.json --timeout 1800 command deploy-now`. On GCP a
`deploy-now` can spend all of that waiting for a new grant (**Failures** in
[Versions and upgrades](#versions-and-upgrades)), so give it longer.
The command workflow's job limit is 30 minutes for `deploy-now`,
`roll-back`, and `backup` and 15 for the rest (`timeout_minutes`), and its helper
stops at 90% of that. Request IDs are `cli-<hex>` by hand and
`gha-<run ID>-<attempt>-<hex>` from a workflow.

Each command carries when the helper stops waiting for it. Past that time the
agent reports `expired` and does not run it, so a command reported as unanswered
does not run later; check `status` before sending it again. A command refused
before it changed anything is `rejected`. One that fails partway is `failed`,
with what it did and what to do next. The helper exits 1 on either, on `expired`,
and on `migration-failed` and `resume-failed`. While it waits, a failed read of the
log, a group, or a release pointer is tried again until the wait ends, with a warning
on stderr. A failed send or pointer write stops at once. A scalable release reads its
pointers before anything moves, so one this identity may not read, or a sign-in that
has lapsed by then, stops at once too. Once a command or release is under way, reads
of the log and the groups go on with a warning whatever the error, even a sign-in that
lapses during the wait: the helper then reports that nothing answered in time, with
the last read's error.

In the command workflow, every command goes out when its job starts, after any
environment approval, and waits on the host, where the agent takes them one at a time
in about the order sent (neither its lock nor Pub/Sub promises strict order). GitHub
holds none of them, so a newer command cannot replace or overtake one there. One sent
during a long deploy fails with "did not answer" if the wait outlasts the job limit,
and does not run later: send it again, or raise `timeout_minutes`. A job still waiting
for the environment's approval has not gone out, so reject one that should no longer
run: a `deploy-now` approved after a stop starts Hot again. Only the release workflow
still queues on GitHub, one run per installation.

| Command | Does |
| --- | --- |
| `status` | The deployed release, previous release, and migration level |
| `diagnostics` | Versions (release, migrations, host image, Kata, the `hotbox` in use), effective configuration, each service's state, health, and image digest, the Hot services' environment, data-service modes and reachability, the current health metrics, and up to 500 objects a storage move could not move. With Docker or the operator secret not answering, it says so (`services_error`, `health_error`, `dependencies_error`) and reports the rest |
| `logs SERVICE` | The newest 200 lines (at most 48,000 characters) of a Hot service, the ingress, or a local data service, within 2 minutes |
| `backup` | An on-demand `pg_dump` of a local database to the data disk, refused unless the disk has room for another like the newest dump, plus 256 MB. A failed one leaves nothing behind. A finished one stays on the disk: no command deletes it, so it counts toward `disk-pressure` from then on. On a stopped installation it starts only the data services |
| `restart-service SERVICE` | Delivers secrets again and recreates one service; the scheduler stops before it starts, and `app` or `api` also recreates the ingress, a brief interruption. Refused when the operator secret added or removed an entry for that service (run `deploy-now`); when an entry that service mounts is empty (for `app` or `api`, also an ingress entry), or holds a `HOT_NETWORK_EGRESS_PROXY` Hot refuses, since the service would exit on it and the running one keeps its value; for `app`, when a sign-in pair is half set or blank; after a partly failed storage move (fix the cause and run `deploy-now`, or put the storage setting back, apply, and run `deploy-now`); after a deploy that upgraded and moved the data failed or was cut short, when the installed bundle still describes the old copy (run `deploy-now`); while a roll-back is unfinished; and, for `scheduler`, while a failed or interrupted deploy's possibly partly applied migrations hold it. The refusal says what clears it |
| `drain-and-stop` | On a single machine only: on either cloud the helper refuses it on a scalable installation, sends nothing, and names the group resize ([below](#stopping-a-scalable-installation)). Stops the scheduler, then workers (2-minute grace), then the app, API, and ingress. Hot stays stopped, across reboots too, until the next deploy or `deploy-now`. An address change alone does not start it; `deploy-now` redeploys with the new address |
| `deploy-now` | Deploys the release pointer now (the `release` action sets the pointer first), and retries a failed deploy or migration. After `drain-and-stop`, or a start after a boot that failed, it starts the installed release again if neither the pointer nor the configuration changed. It also renders the same release again when the operator secret gained or lost an entry. One sent before a deploy of that release failed answers with that failure and retries nothing: send it again after fixing the cause |
| `roll-back` | After a failed upgrade: restores a local database from the pre-upgrade dump if the migrations started, then restarts the installed release. See [Recovery](#recovery) |
| `test-alert` | Fires the `alert-test` alert, which resolves by itself: checks that alerts reach `monitoring.alert_emails` |

On a scalable AWS installation a command reaches every host, and the helper
waits for each host that was in service when it was sent and still is. It returns
the first failure, or else the newest answer, with each host's state under
`answers`. A host without the service rejects it, which does not count while some
host has it. A host that does not answer fails the command and is named in the
error; if none answers, the helper prints only the error. The command document
there has no `backup`, `roll-back`, or `drain-and-stop`, and the helper refuses all
three before sending: the database is not on those hosts, and a stopped host would
not stay stopped. On either cloud it also refuses `logs` or `restart-service` naming
`postgres`, `valkey`, or any other service a scalable installation's hosts do not run.

On a scalable GCP installation a command reaches whichever host takes it first, so
the helper sends only `status`, `diagnostics`, and `test-alert`. It refuses
`drain-and-stop`, `restart-service`, `logs`, `deploy-now`, `roll-back`, and
`backup`, and says what to do instead.

On GCP the agent acknowledges a command before it runs it and remembers the last
200 request IDs, so a message delivered twice runs once.

Diagnostics and logs are redacted on the host before they are reported:
- every value in the host's secret files, also when JSON-escaped or printed one
  line at a time, and passwords inside connection URIs and service files;
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

### Stopping a scalable installation

No command stops a scalable installation. App and API groups replace a host that
fails its load balancer's health check, as a stopped host does, and a replacement has
no record of the stop: it deploys its role's release and serves again within minutes.
So the way to stop one is to resize its groups to 0, with an identity that may (the
release role or account may not), after `maintenance start`. Resize
`hot-NAME-scheduler` first, because it runs the migrations and measures the jobs that
workers scale on, then `hot-NAME-worker` and `hot-NAME-task-worker`, then `hot-NAME-api`
and `hot-NAME-app`. Nothing drains first, so work cut off on a host runs again later.

```sh
# AWS, for each group
aws autoscaling update-auto-scaling-group --auto-scaling-group-name GROUP --min-size 0 --desired-capacity 0

# GCP. The scheduler's group is zonal and has no autoscaler.
gcloud compute instance-groups managed resize hot-NAME-scheduler --size 0 --zone ZONE
# Each other group is regional: its autoscaling goes off first.
gcloud compute instance-groups managed update-autoscaling GROUP --region REGION --mode off
gcloud compute instance-groups managed resize GROUP --size 0 --region REGION
```

On AWS an autoscaled task-worker group (`max` above `min`) protects its hosts from
scale-in, so those hosts stay until you terminate them too:

```sh
aws autoscaling describe-auto-scaling-groups --auto-scaling-group-names hot-NAME-task-worker \
  --query 'AutoScalingGroups[0].Instances[].InstanceId' --output text
aws ec2 terminate-instances --instance-ids INSTANCE...
```

An apply restores the sizes and starts hosts: on AWS the groups' minimums, and on GCP
the scheduler's size and the autoscalers, which grow each group back to `min`. So apply
nothing while the stop must hold, and expect the drift check to report the change until
then. To bring an installation back after a database restore, follow the order in
**Going back** under [Upgrading](#upgrading). Neither cloud's route has been exercised
on a real account or project.

## Reaching a private installation

An endpoint is private unless its exposure is `public`, and no VM has a public address.
None of these ways in has run on a real account or project
([Real-account qualification](#real-account-qualification)).

**A routed network.** A VPN, peering, or other route from the client's network, with
that network in `network.client_cidrs`, which the hosts' security group or firewall
and a scalable installation's load balancers admit on 443. It takes no tunnel, and on a
scalable GCP installation it is the only way in: that firewall has no rule for IAP.

**A Session Manager port forward (AWS, on either preset).** This reaches the ingress
from the host's own SSM agent, so it needs no `client_cidrs` entry and opens no port:

```sh
aws ssm start-session --target INSTANCE --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["443"],"localPortNumber":["8443"]}'
curl --resolve app.example.com:8443:127.0.0.1 https://app.example.com:8443/signin
```

- Your machine needs the `session-manager-plugin` beside the `aws` CLI.
- `INSTANCE` is a single machine's `instance_id` output. On a scalable installation,
  only app and API hosts run the ingress, and each serves only its own hostname, so
  use an instance of `hot-NAME-app` or `hot-NAME-api`, listed as in
  [Stopping a scalable installation](#stopping-a-scalable-installation). Those IDs
  change as the groups replace hosts.
- Forward 443. On a single machine with a public endpoint, 9443 belongs to the load
  balancer and refuses the bundle's network, which is where a forwarded connection is
  expected to appear from.
- The client must send the installation's hostname, or the ingress refuses the
  connection: use `curl --resolve`, or a hosts entry pointing the hostname at
  `127.0.0.1`. Hot's own links name the hostname with no port, so they miss a local
  port other than 443.
- A new network reaches `ssmmessages.REGION.amazonaws.com` through NAT. An existing
  network without a NAT route needs a path to it, as well as to the `ssm` and
  `ec2messages` endpoints Run Command already uses. Preflight does not check this.

The hosts accept Session Manager's data channels and cannot tell a shell from a port
forward on them. So what limits a shell is IAM on the operator's principal, not the
host: a principal with `ssm:StartSession` and no document limit can open a shell on a
host that holds the installation's secrets. The stacks create no such policy, and the
bootstrap does not add `ssm:StartSession` to its roles, so attach one of your own to
the principal. This example follows AWS's documented pattern and has not been tested on
a real account:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "ssm:StartSession",
      "Resource": "arn:aws:ec2:REGION:ACCOUNT:instance/*",
      "Condition": {
        "StringEquals": { "ssm:resourceTag/HotInstallation": "NAME" },
        "BoolIfExists": { "ssm:SessionDocumentAccessCheck": "true" }
      }
    },
    {
      "Effect": "Allow",
      "Action": "ssm:StartSession",
      "Resource": "arn:aws:ssm:REGION::document/AWS-StartPortForwardingSession",
      "Condition": { "BoolIfExists": { "ssm:SessionDocumentAccessCheck": "true" } }
    },
    {
      "Effect": "Allow",
      "Action": ["ssmmessages:OpenDataChannel", "ssm:TerminateSession", "ssm:ResumeSession"],
      "Resource": "arn:aws:ssm:*:*:session/${aws:userid}-*"
    }
  ]
}
```

The policy limits the document, not the port: the caller chooses `portNumber`, so such a
principal can forward to any port listening on the host, not only 443. Little does: SSH
is masked, and the bundle publishes only the ingress's ports.

Every instance on both presets carries the `HotInstallation` tag. Keep the condition on
every statement that allows `ssm:StartSession`, including any in the principal's other
policies. AWS has said that without it, a `start-session` with no `--document-name` opens
the default shell even when the policy does not name that document, and its current
guide says the policy must name it. The condition is harmless if the second holds.

**An IAP tunnel (GCP, single machine).** The firewall admits IAP's range to 443, and the
identity needs `roles/iap.tunnelResourceAccessor` on the project or the VM, which neither
the stack nor the bootstrap grants. The stack does not enable `iap.googleapis.com`.

```sh
gcloud compute start-iap-tunnel hot-NAME 443 --local-host-port=localhost:8443 --zone ZONE
```

Connect as above, with the hostname sent to the local port.

## Versions and upgrades

Each release's signed manifest pins the Hot runtime image and the upstream
images by digest, and names its configuration schema, latest database
migration, oldest supported source release, and the renderer it needs. Only
`vX.Y.Z` tags get a manifest. A beta has none, a preview release or a prerelease
tag gets a warning and none, and a `vX.Y.Z` tag whose manifest does not verify
fails the release's manifest job. For maintainers: a preview release from a branch
uses up its `vX.Y.Z` for self-hosting. It takes that version's tag and GitHub release, so
a later tag for it is refused, and installations can never deploy that version. Increment
`resources/version.txt` before the real release. The rest is pinned in this repository; a
test keeps this table in step with those pins.

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
- **Host image.** A manifest's `minimum_renderer` names the renderer it needs. A
  host image whose renderer is older refuses the release: build a host image from
  the release's tag and deploy it first ([Security updates](#security-updates)).
- **Order and downtime.** The agent stops the scheduler, readies the data
  services, takes a restore point if the schema changes, migrates and grants,
  then replaces the app, API, workers, and ingress, checks readiness, and
  starts the new scheduler. A deploy, resume, or roll-back then fails if a
  service does not stay running, naming it with the end of its log, which adds
  up to about 30 seconds to a start. Schedules pause for the whole deploy. The
  previous release keeps serving requests and work until its containers are
  replaced, so it runs briefly against the migrated schema. For a full
  maintenance window on a single machine, run `drain-and-stop` first. A single
  machine never runs two releases of the same service at once.
- **Restore points.** Hot never migrates a database down. Before a release
  adds migrations, the agent records where to recover from:
  - a local database gets a `pg_dump` in `/var/lib/hot/backups/pre-<release>-*`,
    and the last 3 are kept;
  - a managed database gets the time to restore to with RDS or Cloud SQL
    point-in-time recovery;
  - an existing database gets the time, for its operator's backups.

  Later attempts, for the same or another release (a forward fix), keep the
  restore point from before the first attempt's migrations, and still count as
  partly migrated for recovery and `roll-back`, until a successful deploy or a
  roll-back. After a roll-back, the next attempt takes a new restore point. A
  dump is refused before it starts unless the data disk has room for another like
  the newest, plus 256 MB.
- **Rollback.** Pointing back to a release with the same migration level is a
  normal deploy. A release with fewer migrations than the database has is refused,
  before anything stops. Recovery is a forward fix (a newer release) or a restore
  from the restore point, then the earlier release.
  - The refusal goes by the database's own ledger, which the installed bundle's
    `migration-status` job reads (`hot db migrate --status`), so a restored database
    can take the earlier release. After an upgrade that succeeded on a single machine,
    run `drain-and-stop`, restore the managed or existing database to before it, then
    `release` the earlier version. On a local database `roll-back` refuses after a success,
    with `Nothing to roll back`, and no command restores the `pre-<release>` dump. The way
    back is the data disk: restore a snapshot from before the upgrade, which loses what was
    written since, and set the pointer first (**Restore over a later release** under
    [Recovery](#recovery)). A scalable installation stops by resizing its groups, and its
    pointers go back first: see **Going back** under [Upgrading](#upgrading).
  - If that job fails or times out (600 seconds, which covers pulling its image), the
    agent tries the job of the release being deployed, then a local database's
    recorded level, unless a failed deploy may have partly migrated it. Otherwise the
    deploy stops with `Could not read the database's migration level`, before anything
    stops, and waits for `deploy-now`. A replaced VM pulls the installed release's
    image for this, so keep that image pullable.
  - A database with no migrations where the agent recorded some is refused as empty
    or not this installation's. Check its connection, or restore it.
  - The recorded level is used only where there is no job to run: a first deploy, an
    installed release whose Hot or bundle predates the job, as with 3.0.1, whose
    `hot` answers the flag with a usage error, and a retry after the data moved. A
    scalable host that does not migrate goes by no level ([Upgrading](#upgrading)).
- **Failures.** A failed deploy reports the steps it finished and what
  recovery means, and its error names the job that failed, with the end of its
  output, redacted:
  - If it failed before its migrations began, the database is unchanged, and the
    previous release's scheduler starts again. If its services had started, the
    whole installed release restarts (step `previous-release-restarted`). If that
    fails too, that start is tried again every 5 minutes. Run `roll-back` to start it
    at once, or `deploy-now` to retry the release. A later failed deploy then says the
    installed release has not started again, not that it keeps running.
  - If it failed during or after migrating, which only a release that adds
    migrations can, a forward fix or a restore is needed. Until a roll-back or a
    successful deploy, a reboot starts the installed release without its
    scheduler, and the report's `scheduler` field says why.
  - A deploy that fails while Hot is stopped, by `drain-and-stop` or an earlier
    failed move, keeps it stopped. Pointing back at the installed release, or
    `deploy-now` once it is fixed, starts it.

  The agent does not retry a failed release on its own; run `deploy-now`
  after fixing the cause, or point at another release. If the host stopped
  mid-deploy, only the installed release's data services start (report `resumed`,
  with `interrupted` naming the release), and the deploy resumes. If the pointer
  and configuration went back meanwhile, it is recorded as failed and the
  installed release restarts. If the pointer went back at the installed release with
  a changed configuration while the database waits for roll-back, the interrupted
  deploy is recorded as failed and Hot stays stopped until `roll-back`. A start after
  a boot that fails (`resume-failed`), or that was killed partway, is tried again
  every 5 minutes, and by `deploy-now`.

  The agent signs in to a private image mirror before a start after a boot, a
  roll-back, and a scalable migration. During a deploy on GCP, an Artifact Registry
  mirror that refuses the host (denied, unauthorized, forbidden) is tried again with a
  fresh sign-in for up to 10 minutes before the deploy fails, since a new grant can
  take minutes to apply. Any other pull failure is reported at once.
- **Records.** `/var/lib/hot/releases/history.json` keeps the last 20 deploys:
  - release and previous release;
  - image digests and host image versions;
  - configuration schema;
  - migration levels before and after;
  - steps, restore point, and any error.

  `diagnostics` shows the last 5.

**Managed PostgreSQL and Valkey.**
- *Minor versions* are applied by the cloud. RDS and ElastiCache apply them
  in the instance's maintenance window (`auto_minor_version_upgrade`), and
  the stacks set `apply_immediately = false`, so other changes wait for the
  window too. Cloud SQL and Memorystore apply them during Google's
  maintenance. Each causes a short restart or, with `high_availability`, a
  failover. Hot reconnects; `dependencies-unreachable` alerts only after 10
  minutes. The scheduler exits if it loses its database lock and starts
  again.
- *Major versions* are fixed. The contract accepts PostgreSQL 18 only, and
  RDS is created with `allow_major_version_upgrade = false`. A new major
  version arrives with a Hot release and its own procedure.
- *Restores* use the provider's point-in-time recovery; see
  [Recovery](#recovery).

**Choices that cannot change in place.** Each of these has another route, in the
table. Most stop at plan: `prevent_destroy` refuses a plan that replaces the data
disk, the generated secret, or a provisioned bucket, and the plan workflow's
durable-resource guard fails one that replaces another durable resource.

| Choice | Instead |
| --- | --- |
| `name`, `cloud`, `region` | A new installation, then move data with backups |
| `zone` (single machine) | Restore a snapshot into a new installation in that zone |
| Bucket name or location | Keep the bucket. A provisioned bucket's change stops at plan, and a single machine's agent refuses another existing bucket. `hot db move-storage` moves between backends only and refuses another bucket of one backend, and records name their objects by paths that include the bucket, so copying the objects does not move them. No procedure has been exercised |
| Local to managed data | Changing the mode replaces nothing: the apply starts a data move ([Moving data off the host](#moving-data-off-the-host)) |

## Upgrading

1. **Check.**
   - Read the release notes and the manifest's `upgrade.minimum_source`,
     `minimum_renderer`, and `migrations.latest`; the release workflow verifies
     the manifest and prints it in its summary. Compare them with
     `command status`.
   - `command diagnostics` shows every service healthy and every dependency
     reachable. If the last deploy failed, follow its `recovery` first.
   - A local database is dumped to the data disk before migrations, so check
     `DataDiskUsedPercent`. A managed database's restore point is a time
     inside its backup window; Cloud SQL keeps at most 7 days of transaction
     logs, whatever `backup_retention_days` says. Back up an existing
     database yourself.
   - With `image_registry`, copy the release's images first
     ([Security updates](#security-updates)), and keep the installed release's: a
     deploy reads the database's level with its image.
   - Scalable: every group has its instances in service and no rolling
     update running.
2. **Silence alerts** with `maintenance start`
   ([Backups, logs, and alerts](#backups-logs-and-alerts)). A single
   machine's scheduler is stopped for the whole deploy, and
   `scheduler-stopped` fires after 10 minutes. For a full window on a single
   machine, run `drain-and-stop` too; the deploy starts everything again.
   The helper refuses it on a scalable installation, on either cloud. A release rolls
   each role in place, so it needs no stop. A full window there means
   [resizing the groups](#stopping-a-scalable-installation), and a release then
   follows the order in **Going back** below.
3. **Release** with the release workflow or
   `selfhost.py installation.tfvars.json release VERSION`. By hand the helper
   waits 30 minutes, or 60 on a scalable installation (`--timeout`, in seconds, before
   `release`).
   The workflow's job limit defaults to the same (`timeout_minutes`), and its
   helper stops at 90% of that. On scalable this one limit covers the whole
   release, the migration and every role. If it times out, the deploy may still
   be running: check `status` later, and the agent's lines in the host journal.
   On scalable the report names the role, the instances still pending, and
   `rolled`, or says the migration host may still be migrating.
4. **Verify.** The report says `deployed`; on scalable, `rolled` lists each
   role's instances. `status` shows the new release and the manifest's
   migration level. `diagnostics` shows `Ready` and `SchedulerRunning` at 1,
   and the new deploy record with its restore point. Sign in and run
   something a schedule or an event starts. On scalable, check every host on
   the dashboard. Then `maintenance end`.

**When it fails,** the helper exits 1, and the report gives the finished
steps, the error, and `recovery`. That includes `resume-failed`, as when
`deploy-now`, or a `release` of the installed version after `drain-and-stop`,
fails to start Hot again. On a single machine, follow **Failures** above and
[Roll back a failed upgrade](#recovery). On a scalable installation:
- **`migration-failed`.** Every role still runs the old release. A new release that
  any role would refuse (a half-set or blank sign-in pair, or an empty operator entry) stops
  here too, before the migration host changes anything, so the database and every role
  stay as they were. A first install, or a replaced scheduler host catching up, only
  logs that problem and still migrates, and the release then stops at the role that
  refuses it. A scheduler host with nothing installed, as when its group replaced it,
  does not start a release whose migration just failed there if its role pointer names
  that release too. No scheduler runs until the retry passes, and the scheduler alarm
  fires. `release` alone does not retry, because the host needs the retry. Fix
  the cause, then let the scheduler's host retry. On AWS, run `command deploy-now`; it
  reaches every host, only the scheduler's has a migration to retry, and it returns once that
  host has reported `migrated` or `migration-failed`. On a first release the role pointers
  still hold `none`, so the hosts also report a failed deploy of `none`, and the command
  exits 1 with `Release pointer value 'none' is not a release version`, whether or not the
  migration passed. That error says nothing about the migration: go on to `release`, which
  reads the migration's own report. On GCP the helper does not
  send it, so recreate the scheduler's instance, which migrates from the pointer,
  with an identity that may change instance groups (the release account may
  not):

  ```sh
  gcloud compute instance-groups managed list-instances hot-NAME-scheduler --zone ZONE
  gcloud compute instance-groups managed recreate-instances hot-NAME-scheduler --instances INSTANCE --zone ZONE
  ```

  Then run `release` again. It ignores a failure from before the scheduler's
  instance last started, or from an instance the group replaced, and waits for
  the new host's report, so it is safe to run while the new instance boots.
- **A role failed.** Its pointer already names the release, and its hosts
  do not retry on their own. Fix the cause, then run `command deploy-now`
  (AWS) or replace the group's instances (GCP; see
  [Rotating secrets and certificates](#rotating-secrets-and-certificates)).
  Running `release` again checks each role that is already pointed, by every
  instance's latest deploy report (`deployed`, `current`, or `failed`) since that
  role's pointer was last written; a later `status` or `logs` report changes nothing.
  It goes on only when none failed and each has deployed, and otherwise reports
  the failed role again. Then it moves the remaining roles. On GCP a failure from
  before an instance last started no longer counts, because a group can recreate an
  instance under the same name, and the release waits for that host's new report. A VM
  restarted in place does not retry a failed release, so the release waits out its
  timeout and then reports the earlier failure; the retry is to replace the group's
  instances.
- **A pointer that went away and came back.** A host reports a release when it acts
  on its pointer, and a release run again counts only reports from since each pointer
  was last written. A pointer that goes to another release and back before the host
  saw the change (3.0.1, 3.0.2, 3.0.1) gets no new report, so `release` times out
  however often it runs. Check `status`. If the hosts are on that release: on AWS, send
  `command deploy-now`, then `release` again. On GCP, with an identity that may change
  instance groups, recreate the scheduler's instance and replace the role's instances,
  then `release` again. A host that failed the other release and stayed on this one
  does report: it says `current`, once, when its pointer returns. A stopped host, or one
  after `resume-failed`, stays silent.
- **Going back.** Scalable hosts have no `roll-back`, so recovery is a
  forward fix or a point-in-time restore. Going back to a release with the same
  migration level is a normal `release`. Across migrations, keep to this order. A host
  that comes back deploys and migrates from its pointers, so an apply, a resize, or a
  replaced scheduler's host before step 3 runs the newer release's migrate on the
  restored database and takes it forward again. Only the scheduler's host reads the
  database's real level and refuses a release older than it: role hosts have no
  migration login, and would serve whatever level it is.
  1. Resize every group to 0, the scheduler's first
     ([Stopping a scalable installation](#stopping-a-scalable-installation)).
  2. Restore the database ([Recovery](#recovery)).
  3. Write all six pointers (`migrate`, `task-worker`, `worker`, `api`, `app`, and
     `scheduler`) to the earlier release, which the release identity may:

     ```sh
     aws ssm put-parameter --name /hot/NAME/release/POINTER --value VERSION --type String --overwrite
     printf '{"release": "VERSION"}' | gcloud secrets versions add hot-NAME-release-POINTER --data-file=-
     ```

  4. Restore only the scheduler's size, by hand and not with an apply, which restores
     them all:

     ```sh
     aws autoscaling update-auto-scaling-group --auto-scaling-group-name hot-NAME-scheduler --min-size 1 --desired-capacity 1
     gcloud compute instance-groups managed resize hot-NAME-scheduler --size 1 --zone ZONE
     ```

  5. Run `release` with that version. It writes nothing, since the pointers already name
     it. A migration that is refused, as when the restore did not go far enough back,
     leaves only the scheduler's host up, with its scheduler not started: restore again,
     then retry as under **`migration-failed`** above.
  6. Once `release` warns `the migration has passed`, restore the other groups, with an
     apply or by hand, and the same release waits for them. If it times out first,
     restoring the group and running `release` again ends the wait. It ends `deployed`
     once every host has reported.

  With every group at 0 and the pointers not written, `release` moves only the migrate
  pointer, waits out its timeout, and says the scheduler's group has no instance in
  service. None of this has run on a real account or project.

**Once, from an earlier stack.** None of this has run on a real account or
project ([Real-account qualification](#real-account-qualification)).
- **Order.** Build a host image from the release, and apply the stack with
  `host_image` set to it, before using that release's helper, or its release and
  command workflows. On AWS the helper sends `expires`, which the command document
  must declare, and the document passes it to the agent, which must accept it. An
  older helper against the new document means no expiry. On a scalable GCP
  installation, until the stack is applied, a release whose pointer already names the
  version stops with `Could not tell when the migrate pointer was last written`.
- **State bootstrap.** Apply it again, with `github_repository_id` and
  `github_repository_owner_id`: on GCP before the next plan, since a plan needs
  the new read role, and on AWS before a workflow asks for a session over one
  hour, or AWS refuses to issue the credentials. Apply and release roles allow 4
  hours, and the plan role 1. On GCP the root now also uses the `hashicorp/random`
  provider.
- **Backend.** An `s3` backend without `use_lockfile = true` or a `dynamodb_table`
  now fails the plan workflow.
- **Managed data's networks.** The first apply adds each service's networks to the
  installation document. A single machine sees a new configuration and redeploys the
  release it runs. On scalable, the new launch template versions or instance templates
  replace every role's hosts. Local data, and existing services with no entries, see
  no change.
- **GCP, provisioned buckets.** The first apply sets each bucket's whole IAM policy. It
  reports the old `google_storage_bucket_iam_member.storage` bindings as forgotten, not
  destroyed, and the host keeps access throughout. A grant added outside the stack is
  removed, and project viewers lose access to the objects.
- **A single machine from v3.0.1, on either cloud.** The stack now creates its own
  release pointer and secrets, and ignores `release_pointer` and `secret_references`
  in the tfvars. On AWS these are `/hot/NAME/release`, `hot/NAME/operator`, and
  `hot/NAME/generated`; on GCP, `hot-NAME-release`, `hot-NAME-operator`, and
  `hot-NAME-generated`, all in Secret Manager.
  - After the apply, move your entries into the new operator secret, as one JSON
    object ([Rotating secrets and certificates](#rotating-secrets-and-certificates)).
    Nothing reads the secrets the old references named, and without the certificate
    and key a deploy fails with
    `The operator secret bundle needs TLS_CERTIFICATE for HTTPS ingress`.
  - Then run `release`. A pointer the apply creates names no release, so the VM
    deploys nothing until then: on GCP it has no version, and on AWS it holds `none`,
    and the host reports a failed deploy of `none`. An AWS pointer adopted as below
    keeps its value: run `deploy-now` instead.
  - A secret made by hand under one of those names stops the apply with an
    already-exists error, and preflight checks only the AWS pointer. Before applying,
    adopt it with an `import` block in `imports.tf`, to `aws_secretsmanager_secret.operator`
    or `.generated`, or `google_secret_manager_secret.operator`, `.generated`, or
    `.release`. The other way is to copy it to another name and delete it; AWS keeps a
    deleted secret's name through its recovery window.
- **AWS single machine from v3.0.1.** The first apply replaces the host security
  group (the new one first, the old one deleted at the end), which removes its
  old inline 443 rules, and terminates the VM. Those two take no manual steps. The old
  disk attachment is only forgotten: terminating the VM releases the disk, which then
  attaches to the new VM. The new VM has a new network interface, so its private
  address changes once: repoint any private hostname at `tofu output private_ip`.
  A release pointer made by hand fails preflight unless `imports.tf`, beside the
  tfvars, adopts it:

  ```hcl
  import {
    to = aws_ssm_parameter.release
    id = "/hot/NAME/release"
  }
  ```
- **GCP single machine from v3.0.1.** The first apply reserves the host's private
  address and replaces the VM; the plan says `google_compute_instance.host will
  be replaced due to changes in replace_triggered_by`. Before applying, run
  `gcloud compute instances update hot-NAME --zone ZONE --no-deletion-protection`.
  If that was missed, the apply creates the address and stops at the refused
  delete with the VM still running. There is nothing to undo: lift protection and
  apply again. The data disk stays. The private IP changes once, so repoint any
  private hostname at `tofu output private_ip`.
- **A pre-release build of these stacks.**
  - GCP, a VM already public that the apply does not replace: the VM joins the
    load balancer's group through a new membership, and adding it again fails.
    Before the first apply, run
    `tofu import 'google_compute_instance_group_membership.public[0]' ZONE/hot-NAME-public/hot-NAME`
    with the usual variables, or
    `gcloud compute instance-groups unmanaged remove-instances hot-NAME-public --zone ZONE --instances hot-NAME`,
    which takes the endpoint down until the apply adds it back. An apply that also
    replaces the VM needs neither.
  - GCP, public: each hostname now has its own certificate, so the upgrade renames
    them, and public TLS fails until the new ones are active. Each hostname's DNS
    must point at the load balancer's address; plan a window. With only the API
    public, run `tofu state mv 'google_compute_managed_ssl_certificate.public[0]' 'google_compute_managed_ssl_certificate.public["api"]'`
    before applying, or the old certificate is destroyed before the proxy update.
  - GCP, managed PostgreSQL: the new `max_connections` flag restarts Cloud SQL once
    on the first apply.
  - GCP, a release account shared between installations: the first apply replaces
    its project bindings with conditional ones, and removing the old ones affects
    the others, so apply every installation that shares it before its next
    release.
  - GCP scalable: a name of 21 or more characters gets new role service accounts,
    which rolls the instances, and only the scheduler's account can now add
    versions of the generated secret. The stack no longer has the task hosts' retire
    role, so the next apply needs the right to delete that custom role.
  - GCP, managed Valkey: an instance created in a single zone keeps it, rather than
    being replaced.
- **A hand-run v3.0.1 data disk** (`data/postgres/PG_VERSION`) is unsupported, because
  this release's PostgreSQL image will not start on that layout. The deploy refuses it
  before it writes a secret or starts anything, and nothing on the disk changes. It needs
  a new installation, with the data dumped from the old database and restored there by
  hand. Stock v3.0.1 VMs have no agent and a blank disk, so they are not affected.

## Scalable installations

Set `preset` to `scalable` to run each Hot role on its own group of VMs.
Use the `stacks/<cloud>/scalable` root, with `preset: scalable` on all three
workflow callers: infrastructure, release, and command. The command workflow
requires the input, and the others default to `single-machine`. The helper
refuses a scalable installation called single-machine before anything moves when
it can read the scheduler's pointer. When it cannot (a GCP pointer with no version
yet, or a read that failed), it finds out only from a host's answer, after the
command is out, and fails the run saying so. Every host runs the same host image
and agent as a single machine, with its own role:

| Role | Group | Scaling (`scaling.<role>`) |
| --- | --- | --- |
| App, API | Spread across zones, behind their own load balancer | `min` 2, `max` 4 by default; scale on CPU |
| Worker | Spread across zones | Fixed at `min` (default 2) unless `max` is higher; then scale out on CPU and on the jobs waiting per host (`waiting_per_host`, default 50). Hosts then stay until you lower the count |
| Task-worker | Spread across zones, on the size's nested-virtualization family | Fixed at `min` (default 1) unless `max` is higher; then scale out on CPU and on the slots in use (`slots_target`, default 70%). On AWS idle hosts leave (`retire_idle_minutes`, default 30). On GCP they stay until you shrink the group |
| Scheduler | One instance, replaced stop-before-start | Always one |

- **Data and storage.** A scalable installation needs managed or existing
  PostgreSQL and Valkey, and S3 or GCS for builds and files; the contract
  refuses local ones. It does not move storage, so keep each role on the backend it
  started on: Hot refuses to read records through another. On GCP, where a role can
  be on S3 or GCS, the plan refuses a change of backend, comparing against what the
  stack recorded at its first apply (`terraform_data.storage_backends`): put the
  setting back. An installation that has stored nothing can record the new backend
  with `tofu apply -replace=terraform_data.storage_backends`. The apply that first
  makes that record cannot refuse a change made with it. Nothing refuses another bucket
  or prefix of the same backend, so keep those as they are too.
- **Database connections.** Each instance's PostgreSQL pool is limited to 20
  connections. The contract refuses scaling maximums whose total connections,
  plus administration, would exceed the database's limit for the size (400,
  800, or 1,600).
- **Each host's capacity.** A role's container gets the whole VM less 1 GB, and the
  ingress's cap on app and API hosts, with no CPU cap; a task worker's also leaves
  room for its box guests. A worker host runs as many
  runs at once as the size has vCPUs (small 2, medium 4, large 8). A task-worker
  host runs up to 2, 4, or 8 code tasks at once, held to its vCPUs although the bundle
  asks for twice that, and 2, 4, or 8 container tasks.
- **Endpoints.**
  - The app and API each get a passthrough TCP load balancer: internal by
    default, external when that endpoint's exposure is public.
  - TLS ends on the hosts' ingress with the operator's certificate, as on a
    single machine, so no key enters OpenTofu state, and long streams see no
    proxy timeout. The load balancer keeps the client's address, so the ingress
    ignores a client's `X-Forwarded-For`.
  - Point the hostnames at the `endpoints` output.
  - On GCP a private endpoint admits only `network.client_cidrs` and health
    checks. On AWS it admits `network.client_cidrs` and the hosts' private
    subnets, where the internal load balancer's nodes and health checks are, so
    anything else in those subnets can connect, including Hot's other roles.
- **Releases.** Each role follows its own release pointer, and the scheduler's
  host also follows a migration pointer. The release workflow (or
  `selfhost.py release`) works in order:
  1. It sets the migration pointer and waits for the migration report. If the
     pointer already names the release, only reports from since the pointer was last
     written count, because an older one is about an earlier release of that version.
     Writing the pointer by hand with the value it already holds gets no new report, so
     the release times out until `deploy-now` (AWS) or a new scheduler's instance (GCP)
     makes the host report again ([Upgrading](#upgrading)).
  2. It moves each role in turn (task-workers, workers, API, app, then the
     scheduler) and waits for every instance in that role's group.

  It lists each group again while it waits: an instance that leaves is no longer
  waited for, and one launched meanwhile must deploy too. A role with no instance
  in service is still pointed, and the release waits within its time limit for
  one to come up and deploy. A failure stops the release and says which roles
  already moved. Host image or machine changes replace instances through each
  group's rolling update; the scheduler's group recreates its instance, so two
  schedulers never run.
- **Queue-driven scaling.** With `max` above `min`:
  - Workers add hosts when more than `waiting_per_host` jobs wait per worker
    host, or on CPU. The scheduler's host measures the queue every minute. On
    both clouds they only scale out, because nothing drains a worker before it
    leaves: after a burst, hosts stay at the peak and keep costing money until you
    lower the count. On AWS, run `aws autoscaling set-desired-capacity
    --auto-scaling-group-name hot-NAME-worker --desired-capacity N`, or lower
    `scaling.worker.max`. On GCP, set `max` equal to `min`, a fixed count. Neither
    drains: jobs longer than the stop grace period are cut off and run again. An
    input change also replaces every instance ([Resizing](#resizing)).
  - Task-workers add hosts when their average share of slots in use passes
    `slots_target`. A group cannot tell which task hosts are running tasks,
    so it never removes them.
  - On AWS, a task host that has run nothing for `retire_idle_minutes` stops its
    task worker and leaves its group, which shrinks by one. Each host waits up to five
    minutes longer than the others, so idle hosts leave one at a time, and the group
    keeps `min`. A task claimed while its host stops goes back to the queue.
    `retire_idle_minutes = 0` keeps every task host. The task-worker role may terminate
    instances in its own group only.
  - On GCP, idle task hosts do not retire, and `retire_idle_minutes` is accepted and has
    no effect. IAM cannot limit a grant to one instance group, and a task host runs
    untrusted boxes, so task hosts have no grant on instance groups. After a burst the
    group stays at its peak size, and keeps costing that much, until you shrink it in a
    quiet window, so `scaling.task_worker.max` is also what bounds that cost. To shrink
    it, delete idle hosts, with an identity that has
    `compute.instanceGroupManagers.update` (the release account's `roles/compute.viewer`
    does not). The dashboard cannot say which hosts are idle, since its panel averages
    them, and no helper command can, since a command reaches only one host. In Metrics
    Explorer, read `custom.googleapis.com/hot/NAME/task_slots_used_percent` grouped by
    `instance_id`, and pick hosts that have read 0 for several minutes. A short task can
    fall between readings, and a host with no recent point may be busy: the agent
    publishes nothing when it cannot read the task worker's slots. The first command
    matches each `instance_id` to the name the second takes:

    ```sh
    gcloud compute instances list --filter='labels.hot-installation=NAME AND labels.hot-role=task-worker' --format='table(name,id)'
    gcloud compute instance-groups managed delete-instances hot-NAME-task-worker --region REGION --instances HOST
    ```

    The `instance_groups` output names the group. This changes no configuration. Whether
    the autoscaler adds the host back, and whether zone redistribution then replaces a
    host in another zone, cutting off any task on it, has not been tested
    ([Real-account qualification](#real-account-qualification)). A task cut off or
    claimed in the meantime goes back to the queue.
    The other way is an apply with `max` equal to `min`: the autoscaler then removes
    hosts, possibly ones running tasks, the input change replaces every instance
    ([Resizing](#resizing)), and restoring `max` replaces them again. Neither has run on
    a real project.
- **Hosts** keep only scratch disks; nothing on them outlives the instance.
  On GCP a command reaches one instance of the installation, so the helper sends
  only what any host can answer; on AWS it reaches every instance. See
  [Operating an installation](#operating-an-installation).
- **Clouds.**
  - GCP: managed instance groups (zonal for the scheduler) and passthrough
    load balancers. Autoscaled workers and task-workers only scale out, and idle
    hosts stay. A fixed
    count (`min` equal to `max`) is an autoscaler with equal bounds, so lowering
    it removes hosts, possibly ones running work. On a new network, Cloud NAT
    gives a single machine 4,096 to 65,536 ports and each scalable host 256 to
    8,192, from the one NAT address's 64,512.
  - AWS: Auto Scaling groups across two zones, a NAT gateway per zone, and
    Network Load Balancers. App and API groups replace hosts that fail the
    load balancer's health check. Autoscaled task-workers (`max` above `min`) are
    protected from the group's scale-in, and idle ones leave on their own. A fixed
    count is not protected, so lowering it, or the group's zone rebalancing, can
    remove a host running tasks, which go back to the queue. Autoscaled workers
    only scale out.
  - An existing AWS network needs `network.private_subnet_ids` for the hosts
    and `data_subnet_ids` for the database and cache, each in two or more
    zones, plus `public_subnet_ids` for a public endpoint, covering every zone of
    `private_subnet_ids`.
- **Alerts** fire when any host breaches a signal, when the scheduler stops
  or disappears, and when every host goes silent (`hosts-down`). On GCP a
  per-host incident closes after 30 minutes without data, so a host that left does
  not hold one open.

## Moving a single machine to the scalable preset

A single machine whose data is already managed or existing can move to the
scalable preset and keep its database and Valkey. This has not been
exercised on a real account.

**Before you start.**
- Both data services are `managed` or `existing`, and both storage roles
  are on S3 or GCS, each already moved
  ([Moving data off the host](#moving-data-off-the-host),
  [Moving builds and files to a bucket](#moving-builds-and-files-to-a-bucket)).
- The scaling maximums fit the size's connection budget.
- TLS will end on the hosts with the operator secret's certificate, for
  public endpoints too; the single machine's ACM or Google-managed
  certificate is no longer used. Clients must trust the operator's.
- A network the stack created is reused, and on AWS the scalable root adds
  the second zone's subnets and NAT gateway. On an existing AWS network, add
  the subnet IDs listed above.

**One state, two roots.** Plan `stacks/<cloud>/scalable` with the same
installation directory, so the same `backend_override.tf` and state. The
roots give the shared resources the same addresses: the generated and
operator secrets, the log group, the network, the managed database and
Valkey with their subnet, parameter, and security groups, and the buckets.
The scalable root adopts them as they are, without imports. The generated
secret matters most: a new one would mean a new encryption key, and
`prepare-postgres` would reset the database logins. The scalable plan
destroys what only the single machine has: the VM, its identity, the
snapshot policy, its alarms, its public load balancer, and its release
pointer. A separate state would need every shared resource removed from
the old state and adopted with `import` blocks in `installation/imports.tf`.

1. Run `status` and `diagnostics`, then `maintenance start`.
2. Run `drain-and-stop`. Queued work stays in Valkey for the new workers.
   Work cut off after the workers' 2 minutes is taken again later and runs
   again.
3. From a terminal with the installation's backend and state passphrase,
   take out of state what the plan must keep:

   ```sh
   # AWS. The host security group's description differs between the roots,
   # so the plan would replace it while the database and cache groups refer to it.
   tofu state rm aws_ebs_volume.data aws_security_group.host
   # Only with a public endpoint on a network the stack created (same range and zone):
   tofu state mv 'aws_subnet.public_second[0]' 'aws_subnet.public[1]'
   tofu state mv 'aws_route_table_association.public_second[0]' 'aws_route_table_association.public[1]'

   # GCP. The release pointer is a secret, which the durable-resource guard protects.
   tofu state rm google_compute_disk.data google_secret_manager_secret.release
   gcloud compute instances update hot-NAME --zone ZONE --no-deletion-protection
   ```

4. Set `preset` to `scalable`, and add `scaling` and any network fields.
   Set `preset: scalable` on the infrastructure, release, and command
   workflow callers, and `gcp_zone` for a GCP release; the helper takes the
   region from it. Each root refuses the other preset at plan, and a scalable
   release without a zone (GCP) or region (AWS) fails before any pointer moves.
   Remove from `imports.tf` any `import` block naming an address only the single-machine
   root has, such as an adopted `aws_ssm_parameter.release`: the workflow copies the
   file into whichever root it plans, and an import with no such resource fails the plan.
5. Plan with the scalable root, review the deletions, and apply. The apply
   terminates the VM. New hosts wait for their pointers; on AWS each first
   reports a failed deploy of `none`, the pointers' initial value. Until their
   role's first release, app and API hosts on both clouds fail the load balancer's
   health check, and their groups replace them about every 11 minutes. A release
   no longer fails on this: it follows the group.
6. Release the version the VM ran:
   `selfhost.py installation.tfvars.json release VERSION`.
7. Check each endpoint through its load balancer (`tofu output endpoints`)
   before changing DNS, from a network in `client_cidrs` for a private one:

   ```sh
   curl -sS -o /dev/null -w '%{http_code}\n' --connect-to app.example.com:443:APP_LB:443 https://app.example.com/signin
   ```

8. Point the hostnames at the `endpoints` output, then `maintenance end`.
9. Once satisfied, delete what you took out of state: the old data disk, the
   old host security group (AWS), and the `hot-NAME-release` secret (GCP),
   and the old disk's snapshots when you no longer need them.

Hot is down from step 2 until DNS points at the load balancers. Going back
is not automated: it means planning the single-machine root again with the
kept disk adopted through an `import` block.

## Moving data off the host

To move a single machine's database or queue to a managed service (or an
existing one), change `data.postgres.mode` or `data.valkey.mode` and apply. The
agent rereads the configuration every 30 seconds, and a new one redeploys the
release already running, so **the apply starts the move**, within about a minute,
with no release to run. Plan a maintenance window around the apply, which in the
example operator repository runs when its reviewers approve, and run
`maintenance start` first. Hot is unavailable from the first step until the new
services are ready. For an existing service, its entries may go in the operator
secret before the mode changes: reboots and restarts ignore them, while a deploy
or `deploy-now` that runs before the mode changes refuses them.

1. It stops admissions: the scheduler, app, API, and ingress.
2. If the queue is moving, it lets the workers finish queued work, for up to
   15 minutes, and checks with `hot queue status`. Workers stopped by
   `drain-and-stop` start for this, with no admissions, and stop again.
   Dead-lettered items stay in the old Valkey on the data disk, and the deploy
   record counts them.
3. It stops the workers.
4. It dumps the local database to `/var/lib/hot/backups/move-<release>-*`, of
   which the newest 3 are kept. This dump is also the recovery point.
5. It prepares the new database. Then it restores the dump in one
   transaction, which is refused if the database already holds Hot's
   tables. Staging the dump needs its size plus 256 MB free on the data disk.
6. It migrates, grants, and starts Hot on the new services.

Until the restore succeeds, any failure restarts the installed release on the
untouched local data, with its own bundle and secrets, unless `drain-and-stop`
had stopped it, when it stays stopped. After the restore, the move is complete:
the local copy is no longer used, a failure keeps Hot stopped across reboots and
is fixed forward with `deploy-now`, and `roll-back` refuses. The old data
directories stay on the data disk: no command removes them, so they count toward
`disk-pressure` from then on, and the way out is a larger size ([Resizing](#resizing)).

A move also runs after a reboot while Hot was stopped: the agent delivers the
installed secrets and starts the local data services first. A managed Valkey that
the configuration no longer describes cannot be drained then: describe it again,
run `deploy-now`, then change the mode.

Moving a database back onto the host, or between managed and existing
databases, is not automated. The agent refuses that change before stopping
anything. Copy the database with `pg_dump` and `pg_restore` yourself.

## Moving builds and files to a bucket

To move a single machine's builds or files from its disk to S3 or GCS,
set that role's `storage` to the bucket and apply. As with a data move, **the
apply starts the move**, in a redeploy of the release already running, so plan the
window around it and run `maintenance start` first. The agent sees that the
deployed storage differs from the configured storage. For a GCS target it first
checks that the host can write under the prefix, by writing and deleting
`<prefix>/.uploads/hot-agent-write-check`, for up to 10 minutes, because a new grant
can take minutes to apply. If it still cannot write, the deploy fails with the bucket
named, the installed release keeps running, and `deploy-now` retries. Then:

1. It stops Hot.
2. It runs `hot db move-storage` in the installed release's worker, which
   still has the old settings, plus the keys of an S3 target added to the operator
   secret with the same change. Each object is copied under the same key, read
   back and checked, and only then is its record rewritten. The move refuses to
   run while a scheduler is running.
3. It writes one JSON line per object to
   `/var/lib/hot/backups/storage-move-<role>-*.jsonl` as each finishes, so a move
   that fails still lists every object moved or failed.
4. It starts Hot on the bucket.

The objects stay on the data disk as a recovery copy: no worker or command deletes
them, on either configuration, so they still count toward `disk-pressure`.
If the move fails, Hot stays stopped, across reboots too,
because some records may already point at the bucket, and `roll-back` and
`restart-service` refuse. The failed report's `failed.storage_failed` gives the
role, the count, the first 8 IDs that did not move, and the move's manifest, and
`diagnostics` lists up to 500 under `storage_failed`.
- **Retry.** Fix the cause and run `deploy-now`: objects that already moved are
  skipped. A failure after every object moved also waits for `deploy-now`, which
  finishes on the bucket without moving them again.
- **Leave an object behind.** One that can never move can stay. Hot cannot read what
  stays behind: a file left there can only be deleted, and a build must be deployed
  again. Either setting takes an apply, then `deploy-now`; the setting alone does not
  retry.
  - *A few:* add their IDs to `settings.HOT_MOVE_STORAGE_SKIP`, comma-separated. It has
    to fit in the installation's configuration, which on AWS is about 100 to 150 IDs.
    An ID taken off the list moves again.
  - *Many:* set `settings.HOT_MOVE_STORAGE_SKIP_FAILED` to the count in the last report's
    `failed.storage_failed.count`. That leaves behind everything the report counted,
    however many. The count must match, so a stale or careless setting cannot abandon an
    unexpected number: if it differs, a move refuses it before it stops anything, and the
    error gives the report's count. It also refuses it with no report behind it, when the
    reported role is no longer moving, and when the move's manifest is gone or lists a
    different count. Plan and every deploy, even one with no storage move, refuse anything
    but a whole number of 1 or more, with no leading zero, spaces, or sign. To clear it,
    remove the entry.
  - For as long as `HOT_MOVE_STORAGE_SKIP_FAILED` is set, what it left behind in earlier
    rounds or for another role stays behind too. The refusal and recovery text state the
    total, and the failed record's `storage_left_behind` gives the count per role.
    Taking the setting out tries them all again. On a release whose worker has no
    `hot db move-storage --skip-file`, the deploy fails with Hot stopped and the earlier
    report standing: remove the setting, then list the IDs or fix the cause.
  - The finished deploy's report carries a `warning` with how many objects stayed behind
    and the manifest that lists them as `skipped`. Remove
    `HOT_MOVE_STORAGE_SKIP_FAILED` then, because the next storage move refuses it. Deploys
    with no storage move ignore it, and like any setting, removing it redeploys the same
    release, which recreates the services.
  - `HOT_MOVE_STORAGE_SKIP_FILE` cannot be a setting, under the `NAME_FILE` rule. By hand,
    `hot db move-storage --skip-file FILE`, or that variable, names a file of IDs, one per
    line, in addition to `HOT_MOVE_STORAGE_SKIP`. The flag wins over the variable, an
    unreadable file or a line that is not an ID refuses the move, and a dry run reads it
    too. For the setting above, the agent writes such a file beside the manifest, which
    only the job's user can read and which holds IDs only, and removes it when the move
    ends. A move that reports new failures keeps its IDs beside the manifest instead, as
    `storage-left-behind-<role>.txt`, until a move runs without the setting or a deploy
    finishes. A hard kill can leave the file until the next storage move or finished
    deploy.
- **Go back.** Put the storage setting back, apply, then run `deploy-now`, the one way
  to start Hot on the old storage. Its report carries a `warning`: records already
  moved stay in the bucket, unreadable until the move finishes. Only an apply takes
  the setting to the host, which reads it within about 30 seconds. A `deploy-now`
  that gets there first, as while the apply waits for reviewers, still sees the bucket
  and retries the move, which rewrites more records to it. Setting the storage again,
  applying, and running `deploy-now` resumes where it stopped. Once a move has
  finished, putting the setting back is refused and Hot stays stopped: the records
  point at the bucket, so set that storage again, apply, and run `deploy-now`.

These cases are refused before anything stops:
- changing storage and the release in the same deploy: change storage in a deploy
  of the release already running, then upgrade;
- moving data services and storage together: use separate deploys. A data move also
  waits while a storage move is unfinished, or has finished and its deploy has not: put
  `data.<service>` back, apply, run `deploy-now`, then move the data in its own deploy;
- changing data services or storage while a failed upgrade may have partly
  migrated the database ([Roll back a failed upgrade](#recovery));
- moving back onto the host, which is not automated: `hot db move-storage --to
  local` needs the database and the path together, and no agent command runs it;
- moving to another bucket or prefix of one backend, which nothing does:
  `hot db move-storage` refuses it too, since records name their bucket;
- moving out of GCS, which is not automated: the apply that changes the setting
  removes the host's grant on an existing bucket, so the move could not read it. Hot
  cannot read those objects until the setting is put back and applied, which restores
  the grant, so check before applying. A provisioned bucket's plan fails instead;
- on a scalable installation, a change of a role's backend, since it does not move
  storage: put `storage.ROLE` back ([Scalable installations](#scalable-installations)).

`hot db move-storage` run by hand prints the settings to apply when it finishes.

## Recovery

The host's boot disk holds nothing recovery needs. The installation's
configuration is in OpenTofu state and the instance's boot data. Its secrets,
including the encryption key in the generated secret, are in the cloud secret
store. The release pointer is in SSM or Secret Manager, and the agent's reports
are in the installation's log. Its data is on the retained data disk and in its
daily snapshots, or in the managed database.
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
the new VM mounts it, delivers secrets, and deploys the release pointer. It keeps
its private address. On AWS the VM is stopped before the disk detaches, so
PostgreSQL and Valkey shut down cleanly, and downtime is a little longer:

```sh
tofu apply -replace=aws_instance.host
```

On GCP the instance has deletion protection; lift it, then replace it:

```sh
gcloud compute instances update hot-NAME --zone ZONE --no-deletion-protection
tofu apply -replace=google_compute_instance.host
```

With a public endpoint, skipping the lift stops the apply with the VM running and
still in the load balancer's group, and the endpoint keeps serving. To recover, lift
protection and apply again. To undo the change instead (restore `host_image`, or drop
`-replace`), first run
`tofu import 'google_compute_instance_group_membership.public[0]' ZONE/hot-NAME-public/hot-NAME`,
because adding the VM again fails. A membership created before this setting still
removes the VM on its first replacement.

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

**Roll back a failed upgrade.** On a single machine, when a deploy fails after its
migrations started, the way back depends on where the database is (for a scalable
installation, see [Upgrading](#upgrading)):
- **A local database.** Run `roll-back`, then point the installation back at the
  installed release (the `release` action with that version), in that order: while
  Hot is stopped, `release` of that version is refused and names `roll-back`.
  `roll-back` checks for room for the dump (its size plus 256 MB) before stopping
  anything, restores the dump the agent took before the migrations, checks the
  database with the installed release's migrate and provision, and restarts it.
  - A roll-back that fails before it stops Hot, as at the room check or the registry
    sign-in, changes nothing: fix the cause and run it again.
  - One that got as far as stopping Hot: run it again, and it restores the dump from
    the start. Until it finishes, Hot stays stopped across reboots, and no start,
    `restart-service`, or deploy of any release runs on that database, also after a
    retry that fails before stopping anything.
  - If it restored and checked the database but Hot did not start, running it
    again only starts Hot, and does not restore again.
  - A partly migrated database that did not go through a failed roll-back may
    still go forward to a fixed release.
  - It refuses once it has finished, since a second restore would lose what was
    written since, and once the data has moved to the new database, or a storage move
    has started. After a data move that failed earlier than that, there is nothing to
    restore, and it only starts the installed release. With no dump to restore after
    migrations started, it says to restore the data disk from a snapshot.
  - It also refuses while a deploy cut short may have partly migrated the database:
    the next cycle or `deploy-now` takes that deploy up, and `roll-back` can run if it
    fails.
  - If migrations failed while a storage move was partway or finished, it refuses,
    and pointing back at the installed release is refused too. Hot stays stopped until
    a fixed release is deployed, with the storage setting put back if the move is
    partway, or with that storage kept if it finished.
- **A managed or existing database.** `roll-back` refuses and names the restore
  time. Run `drain-and-stop`, and restore the database to that time with the
  provider's point-in-time recovery (see **A managed database** below) or the
  operator's own backups. Then `release` the installed version: it points back
  and, since Hot is stopped, runs that release's migrate and provision on the
  database and starts it whole. That ends the failed attempt: the scheduler runs,
  later reboots start it, later deploys take a new restore point, and `roll-back`
  refuses. A database still holding the failed release's migrations is refused,
  Hot stays stopped, and after restoring you run `deploy-now` again. Without
  `drain-and-stop`, `deploy-now` answers `current`, with a `scheduler` field saying
  why it stays stopped.

If the deploy failed before migrating, the database is unchanged and
`roll-back` only makes sure the installed release is running.

While a failed upgrade may have partly migrated the database, a deploy that also
changes `data.*.mode` or storage is refused before anything stops. Run `roll-back`,
or put the setting back and deploy a fixed release, then make the change in its
own deploy. If a data move or a reboot had already stopped Hot when the migrations
failed, Hot stays stopped until one of those finishes.

**Restore over a later release.** The data disk carries the agent's record
of the release it ran. If the pointer now names a later release, the new VM
upgrades the restored data again. To stay on the restored release, set the
pointer before the VM boots:

```sh
aws ssm put-parameter --name /hot/NAME/release --value 3.0.1 --type String --overwrite
printf '{"release": "3.0.1"}' | gcloud secrets versions add hot-NAME-release --data-file=-
```

**A managed database.** Point-in-time recovery restores into a new instance.
The stacks have no input to adopt it, as `backup.restore_snapshot` does for
the data disk, and the agent takes the endpoint, CA, and administrator
secret from the instance in state. A single machine cannot use it as
`existing` either ([Hand off a managed database](#state-drift-and-teardown)).
What remains is copying the restored data into the installation's database
with `pg_dump` and `pg_restore`, from a machine that reaches both, with Hot
stopped (on a scalable installation, every group at 0). This has not been exercised.
Objects Hot deleted after the restore point are gone from the buckets too (retired files go after 15 minutes);
provisioned buckets keep old versions for `noncurrent_days`.

**Scalable installations.** Hosts hold nothing: terminate a broken one, and
its group starts another, which deploys its role's pointer. Recovery is the
managed database, the buckets, and the secrets. For a stop that must hold during a
restore, first [resize every group to 0](#stopping-a-scalable-installation), the
scheduler's first. An apply restores their sizes and starts hosts, so apply nothing
while the stop must hold. If the restore is followed by going back to an earlier
release, bring the groups back in the order in **Going back** under
[Upgrading](#upgrading).

**After any restore,** run `diagnostics` and check the `Ready`, scheduler,
and dependency signals. Then expect:
- **Schedules.** Cron occurrences missed while the scheduler was down are
  skipped, unless `settings.HOT_SCHEDULER_BACKFILL` is `"true"`. Then each is
  queued, at most 100 per schedule, counting from the restored database's
  last run and last sync. One-time `@at` schedules and retries that came due
  run either way.
- **Queued work.** Work queued in a local Valkey is restored with the data
  disk; work queued after the snapshot is lost. A starting worker skips
  entries never delivered that are older than 5 minutes (requests and
  responses), 1 hour (alerts and email), 4 hours (events), or 24 hours
  (tasks). Entries a worker had taken but not finished run again once idle;
  after more than 3 deliveries they are dead-lettered and their runs marked
  failed.
- **Duplicates.** Hot runs work at least once. Anything queued or running at
  the recovery point runs again, even if it finished after that point, and
  its calls outside Hot happen again. Backfill queues occurrences that ran
  after that point, and `@at` schedules that ran after it run again.
  Requests Hot accepted after the recovery point are lost, though their
  senders were told they succeeded.

**What one recovery point covers.**

| Part | Captured by | How far back |
| --- | --- | --- |
| Local PostgreSQL, Valkey, builds, and files | The data disk's daily snapshot, one crash-consistent instant for all of them. The agent's dumps (`pre-*`, `move-*`, `backup`) are on the same disk, and no command copies them elsewhere | Up to a day |
| Managed PostgreSQL | The provider's backups and point-in-time recovery | RDS: `backup_retention_days`. Cloud SQL: a point in time within 7 days, daily backups for `backup_retention_days` |
| Existing PostgreSQL | Your backups; the agent records only times | Yours |
| Builds, files, and internal blobs in S3 or GCS | Provisioned buckets keep replaced and deleted objects for `noncurrent_days`; no point in time | Per object |
| Queued work in ElastiCache | One daily automatic snapshot, which no input restores | A day |
| Queued work in Memorystore | Append-only persistence, no backups | None |
| Configuration | OpenTofu state (versioned bucket) and `installation.tfvars.json` (Git) | Their history |
| Application secrets | Secrets Manager keeps `AWSPREVIOUS` and waits 30 days before deleting a secret. Secret Manager keeps versions until destroyed, and deleting a secret deletes them | |
| Cloud keys | Provider-managed keys on disks, databases, caches, and buckets: nothing to back up. The state passphrase is yours to keep | |

Only the data disk's snapshot covers the database, the queue, and stored
bytes at one instant, and only when all three are local. With managed data,
a point-in-time restore rolls back the database alone: Valkey keeps entries
added since, whose rows are gone, and the database can show runs in progress
whose entries are gone. Nothing in the agent reconciles them.

The agent's own points: daily snapshots pause nothing; the pre-upgrade
point is taken with the scheduler stopped and the other services running;
only a data move's dump is taken with Hot drained and stopped. To take a
quiesced point on a single machine, run `drain-and-stop` (queued work stays
queued), then `backup`, a disk snapshot
(`aws ec2 create-snapshot --volume-id VOLUME`,
`gcloud compute snapshots create NAME --source-disk DISK --source-disk-zone ZONE`),
or note the time for a managed database, then `deploy-now` to start Hot
again.

Restore in this order:
1. The configuration: the state object's version and the state passphrase.
2. The original generated secret, before any host boots. The agent
   generates values only when the secret has none, and new ones cannot read
   the old data.
3. The release pointer.
4. The data.
5. Let the agent deploy; it starts the scheduler last.

## State, drift, and teardown

**State.** Each installation's state is in the bucket named in its
`backend_override.tf`. That bucket is versioned and kept outside the
installation's stack, and the state is encrypted with the state passphrase.
OpenTofu's lock serializes applies only if it is on. The `gcs` backend always
locks. An `s3` backend locks only with `use_lockfile = true`, and the plan
workflow refuses one without it or a `dynamodb_table`, with an error naming
`use_lockfile`. The workflow also fails without a `backend_override.tf`, or with
one that configures a local backend.

On GitHub, a call that can apply holds a concurrency group for its installation
directory from plan to apply: a merge made while an earlier apply waits for
approval plans after that apply finishes, and a newer waiting call replaces an
older one, which shows as canceled. Pull request plans and drift checks don't
queue; they wait up to 30 minutes for the state lock. The saved plan is kept for 7
days, and an approval after that fails with a message to run all jobs again.

- **Stale lock.** If a run died holding the lock, the next plan reports the
  lock's ID, holder, and time. Confirm in the Actions tab that the run has
  ended, then run `tofu force-unlock ID`. Never unlock while an apply may
  still be running.
- **Lost or damaged state.** Restore an earlier version of the state object
  from the bucket's version history (`aws s3api list-object-versions`, or
  `gcloud storage ls --all-versions`); the state bootstrap's buckets delete a
  version only once it is 90 days old and 20 newer ones exist. Never plan from
  empty state: it would try to create resources that already exist. If no version
  survives, adopt
  the resources with `import` blocks in `installation/imports.tf`, which the
  workflow copies into the stack. Start with the durable resources below,
  and repeat until the plan creates none of them. An import-only plan counts as
  a change: its summary shows `import ADDRESS`, and it applies on push.
- **Interrupted apply.** State keeps whatever finished; run the plan again
  and review it.
- **An apply that could not save state.** The apply job keeps OpenTofu's
  `errored.tfstate`, encrypted, as the run artifact
  `selfhost-plan-<dir>-<hash>-errored-state-<attempt>`, for 30 days. Apply nothing
  until it is restored, because a later plan starts from older state and would
  create those resources again. `tofu state push` cannot read an encrypted file,
  despite OpenTofu's message, so copy the file over the state object:

  ```sh
  aws s3 cp errored.tfstate s3://BUCKET/KEY
  gcloud storage cp errored.tfstate gs://BUCKET/PREFIX/default.tfstate
  ```

  The state bucket is versioned, so the older state remains. Then plan and
  confirm those resources show no changes. If credentials expired, the state lock
  may remain; see **Stale lock** above. A backend using `dynamodb_table` also needs
  its digest updated, which `use_lockfile` does not. On AWS the apply asks for a
  session of 4 hours (`aws_apply_session_seconds`), so that an apply that creates
  managed data can outlast the default hour and still save state.

**Drift.** The example operator repository plans every morning with
`drift_check`, and any change fails the run with the changes in its summary.
A change can be:
- a console edit;
- a resource changed by AWS or Google;
- a merge that was never applied;
- an import, move, or output change with nothing else, which also counts.

Apply the configuration, or change the configuration to keep what you want.
Nothing in the single-machine stacks is changed by a controller, such as an
autoscaler, that OpenTofu would fight.

A resize of managed data on AWS is expected to show up. The stacks set
`apply_immediately = false` on RDS and ElastiCache, so until the maintenance window
the provider reads the old `instance_class` or `node_type`, and every plan shows the
same update again. For up to a week the drift check fails, pull request comments list
the change, and each push produces another apply waiting for approval. To confirm it,
look for the new value under `PendingModifiedValues` in `aws rds describe-db-instances`
or `aws elasticache describe-cache-clusters`. Applying it again is harmless.

Alarm actions do not show up: they are left to `maintenance`, so `status` and
`diagnostics`, not the drift check, warn about a forgotten `maintenance end`.

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
- The data disk, generated secret, and provisioned buckets have `prevent_destroy`,
  so no plan that destroys them can be made, with `allow_data_deletion` or without it.
- Managed databases have deletion protection (`data.postgres.deletion_protection`,
  on by default), and RDS keeps a final snapshot. Memorystore's is always on, with no
  input.
- Secrets Manager waits 30 days before it deletes a secret.
- Snapshots outlive their disk.
- The plan workflow fails any plan that deletes or replaces a durable
  resource, unless the caller sets `allow_data_deletion: true` for a
  reviewed purge.
- Existing databases, caches, and buckets are never managed by the stacks.

**Hand off a managed database.** To stop managing a database the stack
created without deleting it, remove it from state before changing the mode.
Run `tofu` locally with the installation's backend, state passphrase, and
apply credentials. Managed Valkey shares the AWS data subnets, so leave
`'aws_subnet.data'` out while Valkey stays managed: the next plan would create
them again, and the apply would fail on their ranges. Remove it with whichever
service goes last.

```sh
# AWS (data subnets only exist on a network the stack created)
tofu state rm 'aws_db_instance.postgres[0]' 'aws_db_parameter_group.postgres[0]' \
  'aws_db_subnet_group.postgres[0]' 'aws_security_group.postgres[0]' 'aws_subnet.data'
# GCP (the address and peering only exist on a network the stack created)
tofu state rm 'google_sql_database_instance.postgres[0]' 'google_secret_manager_secret.postgres_admin[0]' \
  'google_secret_manager_secret_version.postgres_admin[0]' 'google_secret_manager_secret_iam_member.postgres_admin[0]' \
  'google_compute_global_address.services[0]' 'google_service_networking_connection.services[0]'
```

The stack then neither changes nor deletes it. Using it as `existing`
afterwards works only in some cases:
- A single machine's agent refuses to change PostgreSQL from `managed` to
  `existing`, before stopping anything; it moves only a local database.
- An `existing` database is verified with `sslmode=verify-full`, which a
  Cloud SQL certificate fails: it names no address.
- That leaves RDS behind a scalable installation, whose hosts start fresh.
  Set the mode to `existing` and put the connection settings in the operator
  secret. The logins the agent created stay as they are.

A database on a network the stack created still needs that network, so keep
the stack until the database moves. ElastiCache hands off the same way, with
the `valkey` resources in `valkey.tf`, on either preset; a single machine
drains the queue first. Memorystore has not been exercised this way: an
`existing` Valkey there would need `HOT_REDIS_AUTH=gcp-iam` and
`HOT_REDIS_CA_CERT` in `settings`, a `rediss://` `HOT_REDIS_URI` with no password
in the operator secret, and a connection grant of your own, since the stack's
`roles/memorystore.dbConnectionUser` grant goes with the mode change.

**Retire an installation.**
1. If the destroy is to delete a managed PostgreSQL whose
   `data.postgres.deletion_protection` is on, as it is by default, set that to `false`
   in a reviewed apply. Do this first: like any configuration change, it redeploys a
   single machine's release, which starts a stopped Hot again, and replaces a scalable
   installation's instances. RDS still keeps a final snapshot. To keep the database
   instead, take it out of state (**Hand off a managed database** above).
2. Run `drain-and-stop` on a single machine, once any deploy from step 1 has finished.
   A scalable installation refuses it, and its hosts go with the destroy.
3. Take out of state what has `prevent_destroy`, whether you want to keep it or not:
   `tofu destroy` stops with an error while any of it is there. Run `tofu state rm` on
   those your installation has, and delete by hand whichever you do not keep:
   - the data disk, on a single machine: `aws_ebs_volume.data` or
     `google_compute_disk.data`;
   - the generated secret: `aws_secretsmanager_secret.generated` or
     `google_secret_manager_secret.generated`;
   - a provisioned S3 bucket: `aws_s3_bucket.storage`. If you keep it, take its
     `aws_s3_bucket_*.storage` settings out too (`storage.tf`), or the destroy strips
     them from it, its encryption, public access block, and policy among them;
   - a provisioned GCS bucket, which takes both `google_storage_bucket.storage`
     and `google_storage_bucket_iam_policy.storage`, each keyed by the bucket's name: the
     policy has `prevent_destroy`, because destroying it empties the bucket's policy,
     even of its owners.
4. Lift the deletion protection that no input controls:
   - Memorystore, whose protection is always on:
     `gcloud memorystore instances update hot-NAME-valkey --location REGION --no-deletion-protection-enabled`,
     or take `google_memorystore_instance.valkey[0]` out of state to keep it.
   - A GCP single machine's VM:
     `gcloud compute instances update hot-NAME --zone ZONE --no-deletion-protection`.

   `tofu destroy` deletes everything else, such as the operator secret, the log group,
   or an ElastiCache group, so take out of state whatever more you want to keep.
5. Destroy the rest with `tofu destroy`.

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
  identity and no secret files. Its connections (`::hot::http`, `::hot::tcp`,
  `::hot::ws`, and box guests) cannot reach metadata, the host, the data
  services, or private ranges outside the allowlist; see
  [Outbound access for user code](../docs/plans/opentofu-cloud-self-hosting.md#outbound-access-for-user-code).
  These limits apply:
  - **Box image pulls.** The installation's containerd pulls box images itself,
    from the registry the code names. That registry is checked against the same
    policy, except Docker Hub, but containerd follows registry redirects and
    resolves names again, so a registry can steer the pull's requests to other
    addresses: redirects are not checked against the deny list. This gap is accepted.
    To say less about what answered, any failed pull from a registry other than Docker
    Hub gives one message, `Could not pull image`, with the reason only in the worker
    log. Timing is not collapsed: the task result still reports `image-pull-ms`, so
    how long a pull took stays visible to the caller. Pulls do not use
    `network.egress-proxy`. A private registry must be in `network.allow-hosts` or
    `network.allow-cidrs`, which also opens it to user code. On a network the stack
    created, an address inside its range stays denied, whatever is allowed, so the
    registry's endpoint has to be outside that range.
  - **An egress proxy.** With `network.egress-proxy`, the proxy resolves hostnames,
    and Hot checks only addresses written as IPs, so the proxy itself must refuse the
    always-blocked list, the whole deny list (including the data services and what the
    bundle adds), and private ranges. `::hot::tcp` and `::hot::ws` are refused. Box
    guests get no proxy settings from Hot: they can reach only the proxy and their
    resolvers, and use the proxy only if the box task sets `HTTP_PROXY` and `HTTPS_PROXY`
    in its env. Their DNS does not pass through the proxy, so public resolvers (`1.1.1.1`
    and `8.8.8.8`) stay reachable, and the task worker logs a warning when it starts,
    unless `settings.HOT_BOX_GUEST_DNS` names resolvers you can audit. The hosts' firewall
    looks the proxy's name up again every 5 minutes; if it does not resolve, guests stay
    limited to the addresses already in place. A task worker on the Docker backend with
    Hot Box on refuses to start with a proxy set, because its guests would connect
    directly. The bundle always uses Kata, so this affects only Hot run by hand.
  - **An existing network.** The whole of a new network is denied. On any network the
    data services' resolved addresses, the host's, your `HOT_NETWORK_DENY_CIDRS`, and
    each service's `data.<service>.endpoint.deny_cidrs` are too, for user code and box
    guests, whose firewall takes only IPv4 entries.
    - AWS: the stacks put each data subnet's range there, for RDS and for ElastiCache.
    - GCP, Cloud SQL: on a new network the stack adds the private services range it
      reserves. On an existing one it cannot look that range up, so list it in
      `data.postgres.endpoint.deny_cidrs`. That entry is optional, because a Cloud SQL
      failover keeps the instance's address, but without it user code an allowlist
      covers can reach the rest of the range.
    - GCP, Memorystore: the stack adds each Private Service Connect address of the
      instance, on any network.
    - An existing service, on either cloud, gets only your entries, because the stack
      does not know where it runs. Without them a replica's node is not listed.

    These entries deny everything in those networks, which may block destinations an
    allowlist covers if the networks are shared with other services, such as another
    Cloud SQL instance in the range, so give the data services subnets of their own. The
    load balancers and, on scalable, the other role hosts are not denied either, so
    keep allowlists off those ranges or add them to `HOT_NETWORK_DENY_CIDRS`.

    On any network the agent looks the services up again. A failover inside those
    networks, or inside a new network's range, needs no redeploy, and restarts nothing
    but the box guests' firewall baseline. An address outside them, which for a service
    with no entries, such as an existing one, can be any new address, redeploys the same
    release, which recreates workers and task workers. Earlier addresses stay denied. If
    that redeploy fails, it reports `failed` and waits for `deploy-now`, and Hot keeps
    running.
- Organizations are separated by PostgreSQL row-level security: the runtime
  login has neither `BYPASSRLS` nor schema rights. Storage keys are scoped
  per organization and environment.
- Accounts are by invitation: the bundle sets `HOT_APP_SIGNUP=invite`, and
  the first administrator is bootstrapped explicitly. The sign-up form then admits
  someone only through an invitation's link; [Google and GitHub
  sign-in](#sign-in-with-google-or-github) also admit an address with a pending
  invitation. A verification email creates an account only if it carries an invitation
  of its own that can still be applied, so a sign-up left pending from before a change
  from `open` to `invite` creates none, even if the address is invited later. That person
  signs up through the invitation's link. To let anyone who can reach the app sign
  up, set `settings.HOT_APP_SIGNUP` to `open`. The form also needs SMTP, which
  verifies addresses. Google and GitHub verify addresses themselves, so with
  either configured, `open` admits anyone with such an account, SMTP or not.

**Identities.** Each identity can do only what is listed, apart from the project-wide
Google Cloud roles described below the table:

| Identity | Used by | Can | Cannot |
| --- | --- | --- | --- |
| Plan role | Plan job (OIDC, pull requests and `main`) | Read the account or project, read state and take its lock. On GCP, create a missing state object | Change resources or an existing state |
| Apply role | Apply job (OIDC, approved environment, `main` only) | Create and change the stack's resources, and write state | Open a shell on a host, as long as its policies stay narrow (below). It does decide what hosts run: the stack's resources set the host image, the boot script, and on AWS the command document, so the limit on that is the review of its plan and the environment's approval |
| Release role | Release and command workflows (OIDC, approved environment, `main` only) | Write the release pointer, send the Hot command to this installation's hosts, read the agent's reports from the installation's log, and silence this installation's alerts. On a scalable installation, read the pointers and when each was last written, and list each role's instances | Read secrets, open a shell or Session Manager session, or use SSH |
| Host identity | The agent, Docker's log drivers, and Hot services | Read the release pointer and this installation's secrets, and create the generated secret once (scalable: only the scheduler's host). Write status, logs, and metrics. Read snapshot times, and use objects under its storage prefixes | Delete snapshots, read other secrets or, on AWS, other parameters, or change infrastructure |
| `hot_owner` login | `migrate` and `provision` jobs only | Own and migrate the schema, and grant the runtime login | Serve requests |
| Runtime login | Hot services | Read and write rows, filtered by row-level security | Change the schema or bypass row-level security |
| Managed database administrator | `prepare-postgres` job only | Create the logins, database, schema, and extensions | Serve requests |

On GCP the release account's `roles/logging.viewer`, `roles/monitoring.alertPolicyViewer`,
`roles/monitoring.snoozeEditor`, and on a scalable installation `roles/compute.viewer`,
are granted on the project. Their condition always holds: it only makes each binding
this installation's own, for destroy. So the account can read the project's logs, snooze
any alert policy in it, and on scalable read its Compute Engine resources, whichever
installation or workload they belong to; only the helper limits itself by name. A
project shared with other workloads or installations shares these grants.

On AWS the plan role uses `ReadOnlyAccess` with an inline deny, so it reads
resource settings and state, not data: no S3 objects outside the state bucket, log
events or queries, command output, RDS log files, or console output. On GCP it uses
`roles/viewer`, which can read logs and objects in existing buckets in the project, so
a pull request's plan can too. It cannot read the objects of a provisioned bucket,
whose policy has no `projectViewer` bindings. The bootstrap adds a custom role beside
it with `pubsub.subscriptions.getIamPolicy`, `pubsub.topics.getIamPolicy`,
`storage.buckets.get`, and `storage.buckets.getIamPolicy`, because a plan refreshes
IAM members and viewer lacks these. A plan identity you bring yourself needs the
same four, at the project, and someone with only viewer who runs preflight needs
`storage.buckets.get`. A storage bucket in another project needs `storage.buckets.get`
and `storage.buckets.getIamPolicy` granted there, and an Artifact Registry mirror in
another project `artifactregistry.repositories.getIamPolicy`; viewer covers it only
in the installation's own project. Without `artifactregistry.repositories.get` there,
preflight warns that it cannot check the mirror. A narrower custom policy also works.
The apply role's policies are yours to choose (`apply_policy_arns`, or `apply_roles`
on GCP). On AWS keep them narrow: the hosts accept Session Manager's data channels, so a
broad policy such as `PowerUserAccess`, which includes `ssm:StartSession`, lets the role
open a shell on a host. Neither the stacks nor the bootstrap add `ssm:StartSession` to a
role themselves, so whoever needs a port forward gets a grant of their own
([Reaching a private installation](#reaching-a-private-installation)).

OIDC trust is pinned to the operator repository's immutable IDs. Apply and release
accept only jobs in the approved environment on `main`, so the release and command
workflows must be dispatched from `main`. They trust the same subject, so a job in
that environment on `main` can assume either: the separation between them rests on
the review of `main`. Limit the environment's deployment branches to `main`, turn
on "Prevent self-review", and protect `main` with required reviews, because anyone
who can push to it directly reaches both.

Limits of sharing one release role across installations: on AWS each stack adds an
inline policy to it, and IAM caps a role's inline policies at 10,240 characters,
about 6 scalable or 9 single-machine installations. Use separate release roles
beyond that. On GCP each installation adds 3 or 4 conditional bindings to the
project's IAM policy, which IAM also limits. On an AWS single machine each command
logs one expected `AccessDenied` in CloudTrail, and a release two: the helper tries
to read the scalable scheduler's pointer, and the role may not.

**Shared identity on one machine.** The Hot services share the VM's
identity: S3 or GCS access and log shipping need it, so AWS allows a
metadata hop limit of 2. A compromised Hot service container, as opposed to
isolated user code, could therefore read this installation's secrets. User
code cannot: its outbound traffic never reaches metadata, and it has no
secret files. Separate identities per service arrive with the scalable
preset.

## Sign-in with Google or GitHub

Each installation registers its own OAuth client with the provider, and no hosted
credentials are shared. This has not run against either provider on an installation
([Real-account qualification](#real-account-qualification)).

**Google.**
1. In the Google Cloud console, in any project, configure the OAuth consent screen.
   Its user type decides who can sign in: Internal for your Google Workspace
   organization, External for any Google account, and only its test users while it is
   in testing. Hot asks for `openid`, `email`, and `profile`.
2. Create an OAuth client ID of type **Web application**, with this redirect URI,
   exactly: `https://<app_hostname>/auth/google/callback`. The app's hostname must be a
   real public domain, not an IP address, which Google refuses. The redirect goes
   through the person's browser, so the app itself need not be public.
3. Put `HOT_GOOGLE_CLIENT_ID` and `HOT_GOOGLE_CLIENT_SECRET` in the operator secret
   ([Rotating secrets and certificates](#rotating-secrets-and-certificates)). The ID
   may be in `settings` instead; the secret may not.
4. Run `deploy-now`, which renders the installed release again to mount them.
   `restart-service` is refused until then. On a scalable GCP installation, where the
   helper does not send `deploy-now`, replace the app group's instances. The app then
   shows the button on its sign-in and sign-up pages.

To rotate the secret later, update it and run `restart-service app`; on a scalable GCP
installation, replace the app group's instances instead.

**GitHub** works the same way: create an OAuth app in GitHub's developer settings, with
the callback URL `https://<app_hostname>/auth/github/callback`, and put
`HOT_GITHUB_CLIENT_ID` and `HOT_GITHUB_CLIENT_SECRET` in the operator secret. Hot asks
for `user:email`.

The bundle sets `HOT_OAUTH_REDIRECT_BASE_URL` to `https://<app_hostname>`, and
`settings` refuses it.

**What people get.**
- The provider must report a verified address, or sign-in fails.
- A verified address that matches an existing account signs in to that account and
  links the provider to it. That includes the first administrator.
- Sign-up by invitation still holds. A new account needs an invitation that can be
  applied, sent to the address the provider reports, with or without the invitation's
  link. With `settings.HOT_APP_SIGNUP` set to `open`, anyone with such an account gets
  one, whether or not the installation can send email.

**With one of a pair.** The pair needs both entries. With one missing or blank, or both
blank, any deploy, `deploy-now`, and `restart-service app` refuse it before anything
stops, naming what is missing. The app itself would refuse to start on one alone or on
an empty file; two entries of only spaces leave the provider off. A reboot does not
refuse it and starts the installed release, and the app then stays down on what it would
refuse, or on an entry removed while mounted. For which hosts of a scalable installation
refuse, see [Rotating secrets and certificates](#rotating-secrets-and-certificates).
To turn a provider off, remove both entries and run `deploy-now`; on a scalable GCP
installation, replace the app group's instances.

**Network.** The app's hosts must reach `oauth2.googleapis.com` and
`www.googleapis.com` for Google, and `github.com` and `api.github.com` for GitHub, over
HTTPS. On a new network they do through NAT; on an existing one, allow them. People's
browsers go to `accounts.google.com` or `github.com`.

## Rotating secrets and certificates

Hot services read secrets from files the agent writes to tmpfs from the
secret store's latest version. A new version takes effect when the agent
delivers secrets again:
- **Single machine:** `restart-service SERVICE` delivers every secret and
  recreates that one service. To restart them all, restart `app`, `api`,
  `worker`, `task-worker`, and `ingress`, then `scheduler`.
- **Scalable, AWS:** the command reaches every host. Hosts running the service
  restart it, and the others reject it. The helper waits for each host and reports
  the first failure among the hosts with the service, or else the newest answer,
  with each host's state under `answers`. A host that does not answer fails the
  command and is named in the error.
- **Scalable, GCP:** the helper refuses `restart-service`, since a command reaches
  one host, so replace the role's instances, with an identity that may change
  instance groups (the release account may not). Each new host delivers the current
  secrets as it deploys:

  ```sh
  gcloud compute instance-groups managed rolling-action replace hot-NAME-app \
    --region REGION --max-surge 3 --max-unavailable 0
  # The scheduler's group is zonal; never surge it.
  gcloud compute instance-groups managed rolling-action replace hot-NAME-scheduler \
    --zone ZONE --max-surge 0 --max-unavailable 1 --replacement-method recreate
  ```

A deploy delivers them too, after it has pulled the release's images and copied its
`hotbox`, so a release refused at either step leaves the services' files as they were,
and a rotated or removed entry does not reach the running release through it. A deploy
with a storage move, always of the running release, delivers them before those steps,
since the move's worker reads the target bucket's keys.

Each secret is one JSON object. Change only the keys you mean to, with an
identity that may read and write the secret (the release role may not):

```sh
aws secretsmanager get-secret-value --secret-id hot/NAME/operator --query SecretString --output text \
  | jq --rawfile c fullchain.pem --rawfile k key.pem '.TLS_CERTIFICATE = $c | .TLS_PRIVATE_KEY = $k' \
  | aws secretsmanager put-secret-value --secret-id hot/NAME/operator --secret-string file:///dev/stdin
gcloud secrets versions access latest --secret hot-NAME-operator \
  | jq --rawfile c fullchain.pem --rawfile k key.pem '.TLS_CERTIFICATE = $c | .TLS_PRIVATE_KEY = $k' \
  | gcloud secrets versions add hot-NAME-operator --data-file=-
```

The previous version stays (`AWSPREVIOUS`, or Secret Manager's older
versions), so you can put it back. A deploy refuses, before anything stops, an
operator secret with an unsupported key, a `NAME_FILE` key, a key also set in
`settings`, a value that is not a JSON string, a malformed `HOT_NETWORK_EGRESS_PROXY`, or,
on a host that mounts the entry, an empty value, as Hot reads it (a byte order mark and one line ending do not count): give
it a value or remove it. A half-set or blank Google or GitHub pair is refused only on the
host that runs the app. Other hosts log `sign_in_not_usable` and carry on, so a release
stops at the migration host, before it changes anything, or where that host is new, at
the app role ([Upgrading](#upgrading)). A reboot does not refuse those keys, and
`restart-service` refuses one only under its rules for an added or an empty entry, a
malformed egress proxy, or, for `app`, a half-set or blank pair, so they cannot keep the
installed release from starting. An empty value or a malformed egress proxy can: a reboot
delivers it anyway, and the service that mounts it exits. Every delivery does refuse a
value that is not a JSON string, whatever
its key: `restart-service` is rejected, and after a reboot the host reports `resume-failed` and
tries again every 5 minutes, with Hot down until the value is fixed.
The generated secret holds every generated value, the encryption key included, and
every later delivery uses its latest version, so change it only as the table says.

**Generated secret** (`hot/NAME/generated`, `hot-NAME-generated`):

| Value | Protects | Rotation | Effect |
| --- | --- | --- | --- |
| `HOT_ENCRYPTION_KEY` | Context variables, service key metadata, and registry credentials in queued box tasks | Unsupported. Hot reads one key, stores no key identifier with its ciphertext, and re-encrypts nothing, so a new key makes stored values unreadable | |
| `HOT_APP_SESSION_SECRET` | Session tokens, API key handoffs, and pending GitHub email choices | Put a new value (`openssl rand -hex 32`) as above, then restart `app`. Not exercised | Everyone is signed out; sign-ins in progress fail |
| `POSTGRES_PASSWORD` (`hot_owner`) | The schema owner, used only by deploy jobs | Local database: unsupported; it is set when the database is created, and migrations fail with a new value. Managed: `prepare-postgres` sets it before migrating, on every deploy (scalable: every migration), so change it, then deploy a newer release. Not exercised | None on running services |
| `POSTGRES_RUNTIME_PASSWORD` | The runtime login | Unsupported. Local: created only on a fresh volume. Managed: the next deploy's `prepare-postgres` changes it while running services still use the old value | |
| `VALKEY_PASSWORD` | Local Valkey | Unsupported: Valkey reads it only at start, no command restarts Valkey, and Valkey and every Hot service must change together | |
| `ADMIN_PASSWORD` | The first administrator's first password | Changing it does nothing: `hot admin bootstrap` never changes an existing user. `hot admin reset-password` needs database access, and no agent command runs it | |

**Operator secret** (`hot/NAME/operator`, `hot-NAME-operator`):

| Value | Protects | Rotation | Effect |
| --- | --- | --- | --- |
| `TLS_CERTIFICATE`, `TLS_PRIVATE_KEY` | HTTPS at the ingress, for both hostnames | Update, then restart `ingress` (scalable: on app and API hosts) | Connections through the ingress drop |
| `HOT_EMAIL_SMTP_USERNAME`, `HOT_EMAIL_SMTP_PASSWORD` | SMTP credentials | Update, then restart `worker` | The worker's running work gets 2 minutes |
| `HOT_EMAIL_SMTP_CA_CERT` | A private SMTP relay's CA certificates, in PEM. May be in `settings` instead; here only the worker mounts it | Update, then restart `worker` | As above |
| `HOT_GOOGLE_CLIENT_*`, `HOT_GITHUB_CLIENT_*` | OAuth sign-in ([setup](#sign-in-with-google-or-github)). An ID may be in `settings` instead; a secret goes only here | Update, then restart `app`, which refuses a pair with an entry missing or blank, or both blank | Sign-ins in progress fail |
| `HOT_NETWORK_EGRESS_PROXY` | An egress proxy address that carries credentials, delivered to the worker and task worker as `HOT_NETWORK_EGRESS_PROXY_FILE`. Percent-encode any `/ ? # \` in the credentials | Update, then restart `worker` and `task-worker`. A deploy, `deploy-now`, and `restart-service` of either refuse a malformed value before anything changes. A reboot delivers it unchanged, and these services then exit on it | The worker's and task worker's running work gets 2 minutes |
| `HOT_BUILD_AWS_*`, `HOT_FILE_AWS_*` | S3 access keys: only `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY`, and `SESSION_TOKEN`. Every S3 storage role has its keys or none does, and a mixed set is refused before anything stops. S3 on GCP requires them, since its VMs have no AWS identity, and a deploy without them is refused | Update, then restart every Hot service. Session tokens are never renewed | Each service restarts |
| `HOT_SYNTHETIC_API_KEY` | The synthetic check | Update; the agent reads it for each check | None |
| `HOT_DB_URI`, `HOT_REDIS_URI` (existing) | The runtime login and the Valkey login | Change the password at the service, update the URI, then restart every Hot service. Monitoring follows within 5 minutes | Until a service restarts, its new connections fail |
| `HOT_DB_MIGRATION_URI` (existing) | The schema owner, for deploy jobs | Update; the next deploy uses it | None |
| `POSTGRES_CA_CERT` (existing) | Verifying the database | Update, then restart every Hot service | Each service restarts |

These rows rotate a value in place. Adding or removing an entry, including the
first or last S3 keys or OAuth pair, takes `deploy-now`, which renders the installed
release again to mount or unmount it; until then `restart-service` is rejected,
naming the entries, and a new entry never reaches the service. After you remove an entry
a service mounts, that service will not start again until `deploy-now`: its file is gone,
so a reboot reports `resume-failed` and tries again every 5 minutes. `deploy-now` renders
the release without the entry, or names the cause if what is left is refused, as with one
of a sign-in pair or the TLS certificate. To clear a value, remove
the entry and run `deploy-now`, because blanking it and restarting is refused. Other
`HOT_EMAIL_*` settings (provider, sender, SMTP host, port, security) go in `settings`, which the
app also reads, and the agent refuses them here. With provider `smtp`, a deploy is
refused before anything stops for a username without a password or the reverse,
credentials or a CA certificate with security `none`, or a CA certificate with no
PEM header.

**Other secrets:**

| Secret | Protects | Rotation | Effect |
| --- | --- | --- | --- |
| Managed database administrator | The RDS `hot_admin` or Cloud SQL `postgres` login, used only by `prepare-postgres` on the host that migrates | AWS: RDS keeps it in its own secret; `aws secretsmanager rotate-secret --secret-id ARN` rotates it now. GCP: raise `data.postgres.admin_password_version` and apply. The agent reads the current value whenever it delivers secrets | None beyond what any input change does ([Resizing](#resizing)) |
| ElastiCache AUTH token | Every Hot service's Valkey connection | Raise `data.valkey.auth_token_version` and apply: the cache gets a new token, then the secret does. The input change redeploys the installed release on an AWS single machine, or refreshes every scalable group, and that delivers it. The same step recovers an apply that failed after creating the cache but before storing its token, when hosts fail AUTH. Not exercised: the stack sets no `auth_token_update_strategy` and `apply_immediately = false`, so whether the old token works until every host has the new one is not established | As for [Resizing](#resizing) |
| Memorystore | Valkey connections on GCP | Nothing to rotate: hosts use short-lived IAM tokens | |
| State passphrase | OpenTofu state and plans | Not provided: `encryption.tf` has one key and no `fallback` | |

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
   instance and keeps the data disk. On GCP, lift the instance's deletion
   protection first, as in [Recovery](#recovery). A release that needs a newer
   renderer refuses an older image and says to build one from its tag. A release
   that changes `hotbox` needs no rebuild. Each start of a task worker, whatever
   starts it, first copies the release's own `hotbox` from its digest-pinned runtime
   image to `/var/lib/hot/kata/hotbox` on the data disk, and boxes use that file. The
   copy baked into the image at `/usr/local/bin/hotbox` is only the fallback when no
   file is there, and boxes never run the copy in the guest image. A failed copy stops
   a deploy before anything stops, with `Could not copy hotbox out of the runtime image`,
   and later starts no service of that release. `diagnostics` shows `versions.hotbox`:
   `in_use` (`installed` or `fallback`), `installed_sha256`, and `fallback_from`, which
   comes from `versions.json`'s `hotbox_from` and describes the fallback only.

Between host image rebuilds, Ubuntu's unattended security updates (on by
default) patch the host's packages. Kernel updates take effect when the
instance is replaced. Rollback follows [Versions and upgrades](#versions-and-upgrades).

**Building a host image.** `images/build-host.sh aws|gcp RELEASE RUNTIME_IMAGE`,
with any `-var` arguments for Packer, takes the release's runtime image by digest,
from its manifest. It needs GNU `timeout` (on macOS, `brew install coreutils`
provides `gtimeout`) and stops the build after `HOT_IMAGE_BUILD_TIMEOUT` (default
`120m`): Packer cancels and deletes the build VM, and the script exits 124. If that
cleanup is still running 15 minutes later, only Packer's wrapper is killed (exit
137), and the build VM may be left to delete by hand.
- With `aws_kms_key_id`, the key's policy must let the identity that applies the
  stack, and on the scalable preset `AWSServiceRoleForAutoScaling`, use it
  (`kms:Decrypt`, `kms:GenerateDataKeyWithoutPlaintext`, `kms:ReEncrypt*`,
  `kms:DescribeKey`, `kms:CreateGrant`), or every launch fails with
  `Client.InternalError`. A grant from `aws kms create-grant` also works.
- On GCP, the build VM's service account (`gcp_service_account_email`, or the
  project's default compute account when empty) needs `compute.instances.get` and
  `compute.instances.setMetadata` on the build VM, and `iam.serviceAccounts.actAs`
  on itself, such as `roles/iam.serviceAccountUser`. Packer's own identity also
  needs `iam.serviceAccounts.actAs` on it. Without the first set the status never
  arrives, and the build waits for its time limit. In secure-by-default
  organizations the default account has no roles. The build VM's scopes are only
  `userinfo.email` and `compute`. With `-var gcp_subnetwork=...` it has no external
  address and needs outbound internet access, such as Cloud NAT.

**Image mirrors.** Set `image_registry` to pull every image from a
registry you control instead of `ghcr.io` and Docker Hub. Each image keeps
the manifest's digest, so the agent's signature check and the pinned content
are unchanged. Copy each release's images to `<image_registry>/hot`,
`/postgres`, `/valkey`, and `/ingress` before you deploy it. A tool that keeps
digests works, such as `crane copy <manifest image> <image_registry>/hot`.
- On AWS, an ECR mirror (`<account>.dkr.ecr.<region>.amazonaws.com/<path>`):
  the host role may read repositories under `<path>/` and nothing else.
- On GCP, an Artifact Registry mirror (`<location>-docker.pkg.dev/<project>/<repository>`):
  each host account gets `roles/artifactregistry.reader` on that
  repository, which may be in another project. The apply identity needs to
  set that repository's IAM policy.
- Any other registry is pulled from anonymously. The contract refuses the
  other cloud's registry, since the hosts have no identity there.

The agent signs in with the host's own identity before each pull; there is
no registry password to store.

## Capacity, retention, and cost

**Sizes.** One VM runs everything. These numbers are the configuration's
limits; they have not been benchmarked:

| Size | VM | Data disk | Concurrent tasks (code, container) | Box storage |
| --- | --- | --- | --- | --- |
| `small` | 2 vCPU, 8 GB (`m8i.large`, `n2-standard-2`) | 100 GB | 1 code, 1 container | 40 GB |
| `medium` | 4 vCPU, 16 GB (`m8i.xlarge`, `n2-standard-4`) | 200 GB | 2 code, 2 container | 100 GB |
| `large` | 8 vCPU, 32 GB (`m8i.2xlarge`, `n2-standard-8`) | 500 GB | 4 code, 4 container | 200 GB |

The bundle asks for twice those code tasks, and Hot holds them to the task worker's
CPUs (1, 2, and 4).

A size is not an availability or throughput promise. The single machine is
one failure domain.

**Memory, in MB.** Each service has a cap. The caps, room on the host for a default box
in each container slot, and the host's own 1,024 add up to the VM on every size:

| Size | PostgreSQL | Valkey | Ingress | App | API | Scheduler | Worker | Task worker | Box guests | Host |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `small` | 1,280 | 384 | 128 | 768 | 512 | 256 | 1,024 | 1,536 | 1,280 | 1,024 |
| `medium` | 2,560 | 768 | 256 | 1,024 | 768 | 256 | 2,048 | 5,120 | 2,560 | 1,024 |
| `large` | 6,144 | 1,536 | 256 | 2,048 | 1,792 | 512 | 5,120 | 9,216 | 5,120 | 1,024 |

A default box takes 1,280 on the host: its 512 of memory, Kata's base of 512, and 256
for QEMU, virtiofsd, and the shim, an allowance that has not been measured. Its 500 MB
`/tmp` is a tmpfs inside that memory. Hot's budget for boxes counts memory plus `/tmp`
and comes to 1,024, 2,048, or 4,096 by size, and Hot fails a box that counts for more at
once. So `small` runs a box of the default size, or a named size up to `medium`, and
refuses `large`.

**At capacity.**
- Tasks beyond the concurrent limit wait in the queue, and the work queue
  grows in Valkey, which never evicts (`noeviction`). When Valkey's memory
  is full, new work is refused with errors rather than dropped.
- Box tasks cannot exceed their storage pool. A box's `/data` is a fully allocated
  file beside the pool, in `data/box-data` on the data disk. Hot's disk budget
  (`HOT_TASK_WORKER_DISK_MB` less 10 GB) bounds it, and that budget and the pool
  together fit the data disk on every size. A box fails with
  `Requested /data volume could not be provisioned` when its file cannot be allocated, or
  when the data disk has less free for non-root writers than the disk budget counts for
  the box (8,448 MB for a default box), which leaves room for its root filesystem and for
  PostgreSQL. Space ext4 reserves for root does not count (5% by default: 5, 10, and
  25 GB by size). These limits bound box storage and do not set it apart: the pool's
  files are sparse, and box writes draw on the same free space as PostgreSQL and any
  local builds and files, so `disk-pressure` remains the warning.
- Services have memory limits: one that exceeds its limit is restarted
  without taking the others down. Box VMs run outside those limits. Each size's caps,
  one default box for each container slot, and 1 GB for the host fit the VM. A box that
  asks for more memory than the default, or a new box starting beside one the task
  worker adopted after a restart, can still exceed the VM's memory, and the host then
  kills whichever process it picks.
- On `small`, local embeddings have not been measured
  ([Real-account qualification](#real-account-qualification)), and `hot-ai` memory uses
  them by default. If the worker restarts on a store write, you can set
  `HOT_STORE_EMBEDDING_MODEL` to `bge-small-en-v1.5` in `settings`, give the map an
  `Embedding` with its own provider, or move to a larger size. A map that already has
  embeddings must be reindexed or recreated before its model changes.
- `disk-pressure` alerts at 85% of the data disk. Extracted bundles for every
  deployed build are on the data disk and count toward it. A worker removes a
  build's extraction within about 5 minutes once no deployment needs it and nothing
  is running it, and all of them are cleared and extracted again when the worker or
  task worker starts. Downloaded embedding models are there too, the task worker's
  apart, so a model both use is stored twice: about 440 MB for the default model.
  Some of what counts never leaves, because no command deletes files from the disk:
  on-demand `backup` dumps, the old data directories after a data move, and the objects
  a storage move copied.

Move to a larger size ([Resizing](#resizing)), which is the way out for all of that.
Moving builds and files to object storage stops them growing on the disk, but what is
there stays.

**Retention.**

| What | Kept for | Set by |
| --- | --- | --- |
| Data-disk snapshots | 7 days | `backup.retention_days` |
| Container logs in CloudWatch | 30 days | `logging.retention_days` |
| Container logs in Cloud Logging | The log bucket's retention (30 days by default) | Cloud Logging |
| Local container logs | 3 files of 10 MB per container | Fixed |
| Pre-upgrade and data-move database dumps | The last 3 of each | Fixed |
| On-demand `backup` dumps | As long as the data disk: no command deletes them | Fixed |
| Deploy history | The last 20 deploys | Fixed |
| Managed database backups | 7 days, with point-in-time recovery | `data.postgres.backup_retention_days` |
| Execution history (calls, and their argument and result blobs) | Forever | `settings.HOT_DB_RETENTION_CALL_DAYS` (`-1` to keep forever, or days from 1 to 36500; plan refuses anything else) |

The worker's daily maintenance deletes calls older than
`HOT_DB_RETENTION_CALL_DAYS` for every organization, unless the organization
has its own `call_retention_days` override. The scheduler enqueues it within 10
minutes of its first start, then once per 24 hours across restarts, and tries a
failed enqueue again at the next 10-minute check. The blobs those calls referenced
are then collected once nothing else references them, after a day's grace.
Builds and files stay until users delete them.

**Cost drivers.** The VM runs all the time and dominates the cost. Then come:
- the data disk and its snapshots;
- NAT for outbound traffic (AWS NAT gateway, GCP Cloud NAT) and, when
  enabled, the load balancers for public endpoints;
- managed PostgreSQL and Valkey instances (high availability doubles them);
- log ingestion and storage, metrics, and alarms;
- Secrets Manager secrets on AWS.

**Budgets.** Set `monitoring.monthly_budget` to create a monthly cost
budget. It emails `monitoring.alert_emails` at 80% of actual spend and when
forecast spend reaches 100%. Budgets alert; they never cap spending.
- AWS: the amount is in USD. The budget counts resources tagged
  `HotInstallation=<name>`, which the stacks tag. Activate `HotInstallation`
  as a cost allocation tag in the Billing console, once per account;
  until then the budget counts nothing. The apply role needs Budgets
  permissions.
- GCP: the amount is in the billing account's currency, and the budget counts
  the installation's project, including anything else in it. Set
  `monitoring.billing_account`. The apply identity needs
  `roles/billing.costsManager` on that billing account, and the plan identity
  `roles/billing.viewer`. The stack enables the Billing Budget API.

## Resizing

**A single machine** takes a larger `size` only.
1. Run [Preflight](#preflight) with the new size; it checks the machine
   type and quota.
2. Run `maintenance start`, then `drain-and-stop`. The silence holds through the
   apply below, on GCP for as long as `--minutes` says.
3. Change `size` and apply. The data disk grows in place, and its
   filesystem grows at the next deploy, roll-back, or backup.
   - Each cloud stops the VM, changes its machine type, and starts it again;
     the data disk stays attached.
   - The agent rereads the configuration from the installation's SSM
     parameter (AWS) or the instance's metadata (GCP), and redeploys.
4. The deploy starts Hot with the size's memory limits, task concurrency,
   and Valkey `maxmemory`. Check `status` and `diagnostics`, then
   `maintenance end`.

Hot is down from `drain-and-stop` until the deploy reports `deployed`.
- **Box storage does not grow.** Its pool files are created once, so the
  pool keeps its first size although `HOT_BOX_POOL_GB` changes.
- **Managed data follows the size** when `data.postgres.tier`,
  `storage_gb`, or `data.valkey.node_type` are unset. RDS and ElastiCache
  apply the change in their maintenance window, and until then every plan shows it
  again ([Drift](#state-drift-and-teardown)); Cloud SQL applies a new tier
  during the apply, which restarts it. Cloud SQL's disk is the exception: on GCP
  `storage_gb` and the size set only its starting size, and Cloud SQL grows it as it
  fills. Raising either later changes nothing, and neither does a smaller `storage_gb`,
  with no warning in the plan. To grow it ahead of demand, run
  `gcloud sql instances patch INSTANCE --storage-size=SIZE`.
- **A smaller size is refused.** EBS cannot shrink, so the AWS apply fails;
  on GCP a smaller disk would be a new disk, which `prevent_destroy`
  refuses. RDS storage that follows the size cannot shrink either; set
  `storage_gb` to keep it.
- **Other input changes apply in place.** The agent rereads the
  configuration every cycle, and a new one redeploys the installed release
  on the same VM. That covers settings, alerts, and data and storage moves.
  Only a new `host_image` replaces the VM, which then boots with the current
  configuration.

**A scalable installation**'s `size` sets each role's machine type, the
task-workers' scratch disk, the connection budget, and managed data that
follows the size. Every input, `scaling` included, is part of each host's
boot configuration, so any change replaces every instance of every role
through its group's rolling update:
- AWS refreshes each group: other groups stay at 100% healthy with up to
  200% during it (600-second warm-up), and the scheduler's instance is
  terminated before its replacement starts. Task-workers are replaced too, despite
  any scale-in protection, and tasks running on them go back to the queue.
- GCP updates each group with up to 3 extra instances and none unavailable,
  and recreates the scheduler's instance. That holds at the VM level only: worker
  and task-worker hosts count as available once they are running, before they have
  deployed, so a roll can briefly leave those roles with no host that has finished
  deploying, and it cuts off work on the hosts it deletes. Plan a maintenance window
  for a new `host_image` or a configuration change.

New hosts deploy their role's pointer, and schedules pause while the
scheduler's instance is replaced, so silence alerts first. New `scaling`
bounds must keep 1 ≤ `min` ≤ `max` ≤ 20 and fit the connection budget.
AWS moves each group's desired capacity into the new bounds, and GCP's
autoscalers take them, for a fixed count (`min` equal to `max`) too. On AWS,
hosts protected by an autoscaled group leave through the rolling refresh or idle
retirement, not through scale-in. A smaller size works for the hosts if the
maximums fit its budget, but RDS storage that follows the size cannot shrink.

## Real-account qualification

The stacks and helpers are tested with mock providers and stubbed cloud CLIs. These
have not run against a real account or project, and each can change what this guide
says:
- Both clouds:
  - A box task end to end on a real Kata host, with a `/data` volume, in every container
    slot of each size. That a box gets its `hotbox` from `/var/lib/hot/kata/hotbox`, and
    that the standalone fallback works: no test has run this bind mount through Kata's
    shim. From inside a box, write more than 256 MB to `/data` and read it back: neither
    Kata's shim nor this rshared bind of `data/box-data` has run together on a real host,
    and the local probe only showed the loop mount reaching the host's mount namespace.
  - On a scalable installation, the stop by resizing, then **Going back**: that hosts
    stay away while the stop holds, that a restored database stays at its level, and
    that the release ends `deployed`.
  - On `small`, the worker's peak memory when it loads the default embedding model.
    Deploy a bundle whose `::hot::store` map uses `EmbeddingOptions.Default`, write one
    entry, then read the worker container's cgroup `memory.peak` and `docker inspect`'s
    `.State.OOMKilled` and `.RestartCount`. Do the same for a store search in the app.
    The worker's cap is 1,024 MB, and past its 512 MB reserve and one 256 MB VM that
    leaves 256 MB, against about 440 MB for the model plus ONNX Runtime's overhead; the
    app's cap is 768 MB. A kill means `small`'s memory plan must change, and `small` is
    exactly full at 8,192 MB.
  - A real Google sign-in on an installation, as a new invited address and as the first
    administrator: no test reaches the token exchange, and no installation has run it.
    The same goes for GitHub's sign-in, which shares that exchange.
  - `test_psql_connects_through_a_service_file` on the first GitHub run.
- AWS:
  - `selfhost.py maintenance start` and `end` under the release role, on both
    presets.
  - A `selfhost.py` command and a release under the release role, on both presets:
    that `send-command` by the `HotInstallation` tag is allowed by the tag-conditioned
    `instance/*` grant, and that the helper reads `LastModifiedDate` from a role's
    pointer as it expects.
  - That an ECR mirror in another account answers `AccessDeniedException` to a look by
    name, which preflight turns into a warning.
  - A HEAD and a GET of a missing key under a role's prefix with the host role,
    recording 403 or 404. The result decides the prefix-limited `s3:ListBucket`
    grant in both stacks.
  - An upgrade apply from v3.0.1 with `client_cidrs` set and the disk mounted:
    no `DetachVolume` is called, the old security group is gone, only the configured
    ranges remain, and the disk attaches to the new VM.
  - Whether `send-command` rejects `expires` against a document that does not
    declare it, and whether an instance refresh shrinks a group with more protected
    hosts than its desired capacity.
  - Rotating the ElastiCache AUTH token.
  - A port forward under the example policy in
    [Reaching a private installation](#reaching-a-private-installation), on both presets:
    `start-session` with no `--document-name` must be `AccessDenied`, and with the
    port-forwarding document and `portNumber` 443 it must reach the ingress. Also from an
    existing network with no NAT route, whose `ssmmessages` path preflight does not check.
- GCP:
  - `selfhost.py maintenance start` and `end` under the release account, on both
    presets: `gcloud monitoring policies list`, then `snoozes create`, `list`, and
    `cancel`, with `roles/monitoring.alertPolicyViewer` and
    `roles/monitoring.snoozeEditor`.
  - A second plan after the first apply, on both presets, with a `release_principal`
    and GCS storage set, reports no changes. The dashboard is the known risk: its tiles
    leave out zero positions because the API omits them from what it returns.
  - That `lastStartTimestamp` moves after `recreate-instances`, and that
    `gcloud compute instances describe` works under the release role
    (`roles/compute.viewer`). The helper relies on both to ignore a stale migration
    failure. That `gcloud secrets versions describe latest --secret
    hot-NAME-release-migrate` returns `createTime` under the release account
    (`roles/secretmanager.viewer` on that secret), which the helper relies on to ignore
    a `migrated` report from an earlier release of the same version.
  - A real plan under the plan role once the viewer bindings are dropped, and
    `google_storage_bucket_iam_policy` with conditional bindings from
    `data.google_iam_policy`.
  - The first scalable release's reads of a pointer with no version and one this
    identity may not read: the helper matches `NOT_FOUND` and the denied errors by
    their text.
  - That a Cloud SQL failover keeps the instance's address, as the stacks' comments
    assume.
  - The live TLS test against a real Memorystore instance, the ignored
    `redis::tests::gcp_iam_tokens_and_a_private_ca_against_a_live_server`. Hot checks
    the server certificate's name as well as its chain, and a failure with
    `NotValidForName` means code must change.
  - The upgrade from v3.0.1 with its new address.
  - The upgrade from a pre-release build, on a public VM: the membership import and
    the certificates' rename.
  - Replacing a public VM, with `deletion_policy = "ABANDON"`: Compute Engine drops the
    deleted VM from the group, then the new membership adds the new one.
  - Shrinking a task-worker group: reading `task_slots_used_percent` by `instance_id`,
    matching the IDs to names by the `hot-role` label, `delete-instances` on an idle
    host, whether its autoscaler adds it back, and whether proactive zone redistribution
    then deletes a busy host in another zone. Also an apply with `max` equal to `min`,
    which only plan-level tests cover.
  - An IAP tunnel to a single machine, with `roles/iap.tunnelResourceAccessor`, and
    whether the project needs `iap.googleapis.com` enabled for it.
  - A host image build with the service account permissions above.

## Local validation

Use OpenTofu 1.13.1 (the tested version) and Python 3.10 or newer. Docker Compose
is used to validate rendered bundles; the configuration test does not pull or
start containers. Provider initialization downloads the pinned provider plugins.
All `tofu test` cases use mock providers and do not access a cloud account.

```sh
python3 -m unittest discover -s infra/tests -v
python3 -m unittest discover -s infra/github/tests -v
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
signatures from the `hot-dev/hot` release workflow on a `vX.Y.Z` tag.
`--signer-identity` affects this command only. A fork that publishes its own
releases changes `RELEASE_SIGNER` in `render.py` and builds its host images from
the fork. It also changes `repository:` and the default manifest URL in its copies
of the reusable workflows, and `DEFAULT_MANIFESTS` in the agent.

The resulting bundle has these prerequisites, which the stacks, the host image,
and the host agent satisfy on an installation:

- A qualified amd64 runtime image with S3 and Kata, runtime resources and
  `hotbox`, plus pinned PostgreSQL/pgvector, Valkey, and unprivileged Nginx
  images. All Hot commands use one image digest.
- A mounted encrypted data disk and precreated `postgres`, `valkey`, `build`,
  `file`, `models`, `models-task-worker`, `extractions`,
  `extractions-task-worker`, and `box-data` directories. Bind mounts disable
  automatic host-path creation to avoid silently initializing storage on the boot
  filesystem.
- Secret files already staged on tmpfs in `/run/hot/secrets/<role>/<name>`.
  Values never appear in Compose environment variables or command arguments.
  Each role receives only its own mounted files; the task-worker runs privileged
  as a documented exception. Other Hot processes run as UID/GID 10001.
- Per-service `HOT_DB_URI` (restricted runtime role), `HOT_REDIS_URI`, and the
  encryption key where needed; the app additionally receives its session secret.
  File-based S3 authentication mounts each selected role's AWS access/secret
  key files. Identity mode uses the SDK credential chain; it does not create
  Google Cloud Storage resources on GCP. For temporary credentials, add
  `--s3-session-tokens` to mount the corresponding session-token files as well.
  The host agent does this when the operator secret holds session tokens, and
  nothing renews them.
- A `postgres` bundle with `POSTGRES_PASSWORD` and `POSTGRES_RUNTIME_PASSWORD`;
  a `valkey` bundle with matching `VALKEY_ACL` and `VALKEY_PASSWORD`; and an
  `ingress` bundle with a trusted certificate/chain and private key covering both
  endpoint hostnames. File owners must match the selected image's service UID.
- A separate `migrate/HOT_DB_MIGRATION_URI` and `provision/PGPASSFILE`, containing
  deployment-only credentials; for a database off the host, libpq service files
  instead, `provision/HOT_DB_MIGRATION_SERVICE`,
  `check-postgres/HOT_DB_MIGRATION_SERVICE`, and
  `prepare-postgres/POSTGRES_ADMIN_SERVICE`. Migrations and the existing runtime grant script
  must complete before runtime services start. Initial SQL creates the restricted
  role only on a fresh PostgreSQL volume; it is not a password-rotation mechanism.
- A baked and qualified Kata/QEMU host with KVM, the dedicated containerd
  socket, CNI binaries/firewall, and bounded guest storage. The task-worker uses
  the established host mounts, the host's `/dev`, and the host PID namespace; no
  Docker socket is mounted.

The bundle starts one scheduler and gives every Hot service a long stop deadline.
The host agent enforces drain and stop-before-start sequencing; Compose alone does
not implement scheduler fencing, backups, or safe upgrades.
Resource limits are conservative starting caps, not qualified size benchmarks.
Local database/cache traffic stays inside the host's container network and no
data ports are published; remote dependency TLS qualification is separate work.

Hostnames and supplied TLS material define private ingress. Internet access is
provided through outbound NAT. Source ranges/IAP access are explicit, and no
cloud stack publishes PostgreSQL/Valkey or port 22. Customer-managed keys and
provisioning a bucket in the other cloud are outside this slice; their
implementation remains in the plan.
