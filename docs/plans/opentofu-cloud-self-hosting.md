# OpenTofu cloud self hosting implementation plan

Status: implementation in progress. Direction selected on 2026-10-20.

The first local slice is documented in [infra/README.md](../../infra/README.md):
file-backed environment settings and redaction, strict storage selection,
temporary S3 credentials, JSON logging, a shared input/runtime-bundle contract,
and mock-tested private AWS/GCP VM roots. No cloud configuration is qualified
yet. Host images, the deploy/secret agent, and the remaining Phase 0/1 release
gates are still required before these stacks are supported installations.
Hot's native GCS backend (Phase 2's storage work) landed separately in #104;
the runtime bundle and stacks do not select it yet.

Provide supported Hot installations in customer-owned AWS accounts and Google
Cloud projects using OpenTofu. Deliver single-machine installations on both
clouds first, then a managed-data preset, then independently scalable services.
Add native Google Cloud Storage (GCS) support for both build artifacts and
application files. Automated per-project custom domains are outside this plan;
the installation's own hostname, routing, and HTTPS remain in scope.
All presets use private networks and private app/API access by default. Public
app and API ingress are independent, explicit installation options.
Require encryption at rest and encrypted cloud-service connections across all
presets, including private deployments, state storage, and backups.

## Presets and delivery order

| Preset | Hot services | PostgreSQL and Valkey | Build and file storage | Availability |
| --- | --- | --- | --- | --- |
| `single-machine` | App, API, worker, scheduler, and task-worker on one VM | Containers on the same VM | Retained local disk by default; optional S3 or GCS | One failure domain; maintenance can interrupt service |
| `managed-data` | The same five services on one VM | RDS and ElastiCache Valkey, or Cloud SQL and Memorystore for Valkey | S3 or GCS by default; retained local disk optional | Managed data recovery; application VM remains a single failure domain |
| `scalable` | Separate app/API services, queue workers, singleton scheduler, and task-worker pool | Managed PostgreSQL and Valkey | Shared S3 or GCS | Independently configured service/data availability; scheduler remains a single failure point |

All presets use the public runtime and the same service configuration contract.
Use PostgreSQL and Valkey even on the single machine to keep behavior aligned
with the distributed presets. Run the five production service commands
separately; `hot dev` is not the production supervisor.
The table describes defaults. PostgreSQL and Valkey can each be replaced with
an existing service independently; the first release qualifies existing
PostgreSQL, and existing Valkey follows (see
[Reference configurations](#reference-configurations)).

Storage backend and bucket ownership are independent of compute provider; see
[Storage choice and bucket ownership](#storage-choice-and-bucket-ownership).
The first release qualifies Local and S3 on both clouds. GCS work runs in
parallel and gates only configurations that select GCS.

Delivery order follows the phases below. Phase 2 (GCS) runs alongside Phase 1
and blocks nothing that selects Local or S3:

| Phase | Delivers | Depends on |
| --- | --- | --- |
| 0 | Public runtime packaging, production configuration, onboarding, release manifest | — |
| 1 | AWS and GCP single-machine stacks, GitHub Actions release delivery, initial release gates | 0 |
| 2 | Native GCS storage on any compute cloud | 0 |
| 3 | Managed-data stacks and data migration | 1 (and 2 for GCS configurations) |
| 4 | Scalable stacks; Compute Engine on GCP. Cloud Run is an optional future evaluation | 3 |

## Decisions and ownership

- OpenTofu owns cloud resources. Keep independent AWS and GCP modules and root
  stacks, with separate state per installation and environment.
- Select compute provider, application-storage backends, and bucket ownership
  independently. Existing buckets remain externally managed; provisioning a
  bucket requires explicit selection of its provider and ownership.
- Workloads use private addresses without public VM or ECS task IPs. Public
  app/API access, when selected, terminates at ingress with private backends.
  Provisioned PostgreSQL, Valkey, workers, and scheduler remain private.
- Explicitly configure supported encryption controls and validate effective
  settings. Provider-managed keys are the baseline; support customer-managed
  AWS KMS or Google Cloud KMS keys for services that offer them.
- Keep Packer for reproducible host images. Share operating-system/runtime
  setup where practical, with provider-specific image sources and publication.
- Thin installer helpers collect inputs, validate them, render configuration,
  and invoke OpenTofu. `aws` and `gcloud` support authentication, state bootstrap,
  diagnostics, and operational commands. Do not build a second complete
  provisioning implementation in shell.
- Public `hot` owns self-host images, configuration, migrations, modules,
  examples, qualification checks, and operator documentation.
- Private `hot-cloud` can later consume pinned public modules. Importing its
  existing infrastructure is a separate project, after fresh installations
  work. No public installation requires a sibling private checkout, private
  registry, private bootstrap data, or Hot Cloud credentials.
- Infrastructure provisioning and application release steps have explicit
  ownership. The release process supplies immutable image/configuration
  versions; OpenTofu owns deployment resource specifications. On VM presets the
  release process owns only a per-installation release pointer, so a release
  never changes instance bootstrap data. Autoscalers may own replica counts
  through a documented exception. Two controllers must not continuously
  overwrite the same fields. See
  [Release delivery and GitHub Actions](#release-delivery-and-github-actions).
- Release deployment supports a published GitHub Actions workflow as a
  first-class driver. It wraps the same OpenTofu and helper commands as manual
  operation; no deployment logic exists only in CI.
- Publish OpenTofu modules, root stacks, and reusable workflows as versioned
  tags of public `hot`, released together with the runtime. Each tag pins its
  compatible runtime releases. Examples reference tags or commit SHAs, never a
  branch. An OpenTofu registry listing can follow.
- Use OpenTofu-native protections: client-side state and plan encryption,
  ephemeral values or write-only arguments for secrets where providers support
  them, `removed` blocks for ownership handoffs, and service-level deletion
  protection for durable resources (see section 7 below).
- Build one `linux/amd64` Hot runtime image per release and run every Hot
  service and one-shot job from it with a different command. Existing cloud
  packaging mixes ARM64 services with an x86_64 task worker; that image set
  cannot be reused unchanged on one VM. See [Runtime image](#runtime-image).
- Store installation secrets in AWS Secrets Manager or GCP Secret Manager.
  Deliver them to services as read-only files on tmpfs, which Hot reads through
  `HOT_<NAME>_FILE` variables, never as plaintext environment values on VMs.
  See [Secrets delivery](#secrets-delivery).
- User code always runs under Hot's outbound destination policy. Internet
  access is allowed; installation infrastructure and cloud metadata are never
  reachable from user code. See
  [Outbound access for user code](#outbound-access-for-user-code).
- Support fresh installations first. Switching a preset, bucket, or storage
  backend requires an explicit data migration and cutover.

## Existing implementation and gaps

Use these public components as the implementation starting points:

| Component | Current behavior and planned work |
| --- | --- |
| [Public CLI](../../crates/hot_cli/Cargo.toml) | Owns service composition and supports a `kata` feature. Add release feature wiring and public image packaging. |
| [Build storage](../../crates/hot/src/storage.rs) | `BuildStorage` has Local and S3 implementations. Add GCS and bounded-memory path uploads. |
| [File storage](../../crates/hot/src/file_storage.rs) | Includes database metadata, conditional writes, streaming, multipart uploads, and internal blob storage. GCS must implement this whole contract. |
| [Configuration templates](../../resources/init) | Add portable self-host settings and GCS options; validate effective environment/configuration precedence. |
| [Runtime logging](../../crates/hot/src/log.rs) | Existing tracing subscribers support text stdout/file logging and rotation. Add structured JSON and qualified collection; exported application metrics and distributed traces require explicit implementation. |
| [Product mode](../../crates/hot/src/product.rs) | `self-host` suppresses cloud billing/upsells. Explicit production environment and authentication configuration are still required. |
| [Email interface](../../crates/hot/src/email.rs) | Public runtime defaults to an unconfigured sender. Add a public transport and register it in public service composition. |
| [PostgreSQL setup](../../resources/db/postgres/RLS_SETUP.md) | Requires separate migration/runtime roles, extensions, public migrations, and runtime grants. Keep this contract in every preset. |
| [Scheduler](../../crates/hot_scheduler/src/server.rs) | A session-level PostgreSQL advisory lock rejects a competing scheduler at startup, but the holder never verifies that it still owns the lock (see [`try_acquire_advisory_lock`](../../crates/hot/src/db/mod.rs)). Add lock-liveness enforcement, then plan one active scheduler with stop-before-start replacement. |
| [Task worker networking](../../crates/hot_task_worker/src/cni.rs) | Host namespace operations require deliberate host mounts, privileges, and network isolation in the single-machine package. `hot-cloud` has a working AWS host setup to port; see [Box task host](#box-task-host). |
| [Environment configuration](../../crates/hot_cli/src/conf.rs) | `hot.hot` files run as Hot code, then `apply_env_vars` overlays every `HOT_*` variable onto its configuration key. Some secrets bypass configuration and are read directly from the environment: `HOT_ENCRYPTION_KEY` ([`context_encryption.rs`](../../crates/hot/src/context_encryption.rs)), `HOT_APP_SESSION_SECRET` ([`hot_app/src/server.rs`](../../crates/hot_app/src/server.rs), [`auth.rs`](../../crates/hot_app/src/auth.rs)), and `HOT_REDIS_URI`. Redaction is based on key names only. Add `_FILE` support and route every secret through resolved configuration. |
| [Outbound destination policy](../../crates/hot/src/outbound.rs) | `::hot::http` and `::hot::tcp` apply `PublicOnly` when `hot.engine.isolation` is on: it blocks loopback, private, link-local, and metadata addresses, pins resolved addresses, and refuses redirects. The worker and task-worker templates default isolation to `true`; the all-in-one template defaults it to `false`. `network.allow-private` is all-or-nothing and, when on, also exposes metadata and the installation's own services. |

Unknown storage backend names currently fall back to local storage in the
factories. Replace that behavior with clear configuration errors for unknown or
uncompiled backends, preserving the intended default when no backend is set.

## Scheduler availability and deferred failover

Scheduler HA and automatic standby failover are outside the scope of all three
presets. The invariant is that at most one scheduler sends scheduling events at
a time, including during deployment, recovery, and any future failover.
Run one scheduler instance and use stop-before-start replacement.

One instance and stop-before-start replacement do not by themselves enforce the
invariant. The singleton lock is a PostgreSQL session advisory lock, so it is
released as soon as its connection ends. That happens on managed-database
failover, network interruption, idle or TCP timeouts, and server restarts,
while the old process may still be sending events. A replacement or restarted
instance can then acquire the lock alongside it. Required changes, in all presets:

- Check the lock periodically on the same connection that holds it, for example
  by querying `pg_locks` for the session's own advisory lock. Bound the check
  interval and timeout below the event-dispatch interval.
- On a failed check, a lost connection, or any uncertainty, stop sending events
  immediately and exit. Supervision restarts the process, which reacquires the
  lock before it does any work. Never reconnect and continue sending events
  without reacquiring the lock.
- Never route the scheduler's lock connection through a transaction-mode or
  statement-mode pooler (PgBouncer transaction mode, RDS Proxy pinning
  exceptions, or similar). Validate this for existing PostgreSQL endpoints.
- On ECS, deploy the scheduler with `minimumHealthyPercent=0` and
  `maximumPercent=100`. On Compute Engine and single VMs, stop the old scheduler
  before starting the new one.
- Qualify lock loss during database failover, connection kill
  (`pg_terminate_backend`), network partition, and deployment overlap. Each
  case must result in at most one sender, as measured by event publication.

The scheduler is a potential single point of failure: a scheduler process or
host failure can interrupt scheduled event dispatch until it recovers. Include
scheduler health monitoring, an actionable alert, and documented restart/recovery
steps. Verify what happens to overdue schedules after recovery and document the
catch-up policy; do not imply uninterrupted scheduling in the scalable preset.

Track active/standby failover as a future enhancement. A standby may take over
only after exclusive scheduling ownership is established and the former active
instance can no longer publish. Qualify ownership loss, network partitions,
host recovery, and deployment overlap before enabling it. Multiple standby
processes must never become multiple active event senders. This is future work,
not a release gate for the presets in this plan.

## Responsibilities currently supplied by hot-cloud

The private composition registers domain, email, billing, and database bootstrap
providers. Its deployment layer also supplies image assets, host setup, routing,
and release operations. Account for each responsibility explicitly:

| Responsibility | Self-host treatment |
| --- | --- |
| Automated custom-domain certificates and distributions | Exclude per-project domain provisioning. Disable or hide unsupported domain management flows and retain a clear API error. Standard project routes must work through the installation endpoint. |
| Installation DNS and TLS | Include operator-supplied hostnames, stable endpoints, certificate issuance/renewal or supplied certificates, and trusted ingress configuration. No requirement to host the customer's DNS with a particular provider. |
| Resend delivery for application and alert emails | Add public email delivery. Prefer a configurable SMTP transport for the first portable implementation; a public Resend adapter can be added independently. Both app and worker processes need the registered provider. |
| First-user access and account lifecycle | Provide explicit operator bootstrap, sign-in, verification, invitations, and recovery. Determine public signup versus invitation-only policy. Do not rely on development auto-login or cloud seed records. |
| Google/GitHub OAuth configuration | The implementation already exists in public Hot. Make it optional and accept the installation's own client registrations, secrets, and callback URLs; do not reuse hosted OAuth credentials. |
| Stripe checkout, subscriptions, usage billing, and cloud plan seeds | Exclude. Keep product mode `self-host`, billing disabled, and qualify the existing no-subscription behavior. Operational resource limits and quotas remain configurable independently of billing. |
| Cloud-only database migrations and legacy backfills | Exclude private schema/data. Retain public migrations, extension setup, restricted runtime grants, and an idempotent self-host onboarding step. |
| Marketing site, newsletter, blog, legal/pricing pages, and private docs overlays | Exclude the `hot_web` service. Keep the public app and runtime assets; make support/docs links configurable and avoid installation flows that require the private web service. |
| Hosted URLs, email branding, sender identities, and reserved organization names | Replace hosted defaults with installation inputs. Preserve general route-name protections; do not copy staff/brand-specific reserved names. |
| Reverse proxy and release safeguards | Carry forward correct routing, trusted forwarding, upload limits, streaming behavior, immutable images, migrations, health checks, logs, and rollback records. Review proxy rules for self-host use instead of copying private filters wholesale. |
| Kata/containerd/CNI setup and executable artifact verification | Extract a generic public build/setup path with pinned artifacts, checksums, and host isolation validation, starting from the current AWS implementation described in [Box task host](#box-task-host). |
| Secrets delivered as S3-hosted `.env` files loaded through ECS `environmentFiles` | Do not carry forward. Use the [Secrets delivery](#secrets-delivery) model. |

The first release must demonstrate real access to a fresh installation. If
email is optional, define an explicit operator-managed onboarding and recovery
mode and disable unavailable email actions; do not silently run normal
verification/invitation flows with an unconfigured sender.

Public [feature resolution](../../crates/hot/src/db/features.rs) returns
unlimited entitlements when no subscription or organization override applies.
Set machine capacity, task concurrency, file limits, and data/log retention
explicitly; unlimited product entitlements do not establish a safe VM budget.
Public [OAuth configuration](../../crates/hot_app/src/oauth.rs) already accepts
Google/GitHub client settings and `HOT_OAUTH_REDIRECT_BASE_URL`.

## Installation configuration and sizes

Treat topology, capacity, and availability as independent inputs. Provide a
small set of tested combinations with advanced overrides:

- Installation name, cloud/account/project, region/zone, environment, release
  version, networking choice, and app/API endpoint configuration.
- Independent app/API exposure, both private by default, plus new or existing
  network/subnets, authorized private clients, DNS/TLS, and outbound connectivity.
- Encryption policy and optional per-resource customer-managed key references,
  including separately bootstrapped state encryption and restore access.
- Preset: `single-machine`, `managed-data`, or `scalable`.
- Size: `small`, `medium`, or `large`, mapped to provider-specific resources.
  Benchmark CPU, memory, disk/IOPS, and task concurrency before publishing
  minimum requirements. Permit explicit VM, database, cache, and disk overrides.
- PostgreSQL and Valkey mode, selected independently: local, managed, or existing
  endpoint. Local data containers belong to the single-machine topology.
  Existing services must pass version, extension, privilege, TLS, and command
  compatibility checks.
- Build storage and file/blob storage: independently select local, S3, or GCS,
  regardless of compute provider. Select existing or provisioned ownership for
  each cloud bucket, with separate prefixes and explicit credentials/identity.
  Local storage requires a single host with a shared retained volume;
  distributed services require S3 or GCS for each storage role.
- Optional capabilities: box execution, email delivery, backup policy, and
  advanced monitoring. The default full installation includes all five Hot services;
  any reduced service selection must validate its feature dependencies.
- Logging/monitoring destinations, optional tracing, retention/volume limits,
  and operator notification channels. Select these independently of application
  storage; support cloud-native defaults or existing operator-owned collectors.
- Availability settings: database/cache replicas and failover, service minimums,
  worker pool bounds, and recovery expectations. A size label does not imply HA.

Full box-enabled VM installs require a tested Linux image, architecture, machine
family, and working `/dev/kvm` with nested virtualization enabled when needed.
Reject unsupported combinations before deployment where possible and verify
them on the host before admitting tasks. Never silently downgrade Kata isolation.

Derive task memory/concurrency from remaining capacity after reserving CPU,
memory, and disk for the OS, PostgreSQL, Valkey, ingress, and app/API/worker.
Give task scratch storage its own limits so a job cannot fill the database disk.
The current standalone task-worker sizing defaults are not single-VM defaults.

Record the resolved installation configuration and release manifest so an
upgrade uses the same resource identities. Keep secrets out of that manifest.
Plain OpenTofu usage remains supported; an interactive installer is a wrapper.

### Reference configurations

The input combinations multiply quickly: preset × compute cloud × storage
backend per role × bucket ownership × PostgreSQL and Valkey modes × exposure ×
key mode. Support is defined by named reference configurations, not by the
product of every input. Each reference configuration has a published example,
a qualification run on its cloud, and recovery measurements.

Initial single-machine release (both clouds unless noted):

| ID | Cloud | PostgreSQL / Valkey | Builds / files | App / API exposure | Keys |
| --- | --- | --- | --- | --- | --- |
| `sm-local-private` | AWS, GCP | local / local | local / local | private / private | provider-managed |
| `sm-s3-public` | AWS | local / local | provisioned S3 / provisioned S3 | public / public | provider-managed |
| `sm-existing-s3-private` | GCP | local / local | existing S3 / existing S3 | private / private | provider-managed |
| `sm-existing-pg` | AWS, GCP | existing / local | local / local | private / private | provider-managed |

Qualified after the initial release, each before it is marked supported:
customer-managed keys, existing Valkey, mixed app/API exposure, mixed
build/file backends, provisioned S3 from GCP compute, and GCS after Phase 2.
Managed-data reference configurations are added when that phase ships; the
scalable ones are defined in
[Scalable reference configurations](#scalable-reference-configurations).

The configuration schema can accept settings before they are qualified.
Validation classifies each resolved configuration as a reference configuration,
a qualified variation, or unqualified. An unqualified configuration requires an
explicit operator acknowledgment input and is reported as such in status and
diagnostic bundles. Validation never silently changes it into a supported
configuration. Settings that are not yet implemented fail validation.

### Storage choice and bucket ownership

Support the following independently for build artifacts and application files.
Internal blobs use the selected file backend. Each role selects one configured
backend; this does not imply per-object dispatch across several backends.

| Backend | AWS compute | GCP compute | Resource ownership |
| --- | --- | --- | --- |
| Local | Supported on a single host | Supported on a single host | Installation manages paths on retained encrypted disk, shared by the relevant service containers |
| S3 | Supported | Supported | Existing bucket by default when supplied, or explicit bucket provisioning using AWS credentials |
| GCS | Supported after GCS qualification | Supported after GCS qualification | Existing bucket by default when supplied, or explicit bucket provisioning using Google Cloud credentials |

Examples include a GCP VM using only an existing S3 bucket, an AWS VM using GCS,
and a single host using local builds with S3-backed files/blobs. Publish tested
examples and qualify the supported compute/backend combinations. Keep backend
selection explicit; never infer it from VM cloud or silently substitute storage.
On one host, local PostgreSQL and Valkey still need the VM's retained encrypted
disk and their own backup policy; choosing remote application storage does not
move them.

For existing buckets, accept bucket name, region/location as required, prefixes,
authentication references, and the encryption policy. Validate runtime read,
write, composition/multipart, and cleanup permissions in a dedicated test prefix.
Do not create/import the bucket or change its IAM, encryption, retention,
lifecycle, public-access settings, or deletion policy automatically. Verify the
required security policy or request operator-side changes before proceeding.
Removing the installation does not remove existing buckets or their contents.

Optional new buckets use provider-specific storage modules/roots with explicit
ownership and independent authentication, even when compute is in the other
cloud. Keep retention, encryption, and teardown behavior consistent with this
plan. Avoid requiring an unused cloud provider's application-storage resources
or credentials in compute roots; pass resolved storage configuration to runtime.

Use SDK credentials: attached identity in its native cloud where appropriate,
qualified federation where available, or explicit credentials held in secret
storage. Neither backend invokes `aws` or `gcloud` at runtime. Using S3 from GCP
does not require GCS storage configuration; GCP compute provisioning still needs
Google Cloud authentication. Operator CLIs are optional setup/diagnostic tools.

Validate HTTPS access from private runtime networks to the selected storage
provider, including NAT/proxy/routing, credential renewal, KMS access, and any
organization policy. Document cross-cloud latency, transfer cost, and location
implications without making the bucket public or giving workload VMs public IPs.

Choose OpenTofu's state backend separately from application storage. A GCP
compute stack can use an existing S3 state backend and S3 application storage
without provisioning a GCS bucket; default cloud-aligned examples are conveniences.
State access and runtime storage permissions remain separate.

Switching backend, bucket, or prefixes for populated storage requires an explicit
data/reference migration and cutover. Record both storage-role selections in
release/backup manifests without credentials. Local storage on either preset
that runs on one host requires retained disk and file/blob backups; distributed
presets must reject local storage for shared persistent artifacts.

### Private networking and optional public ingress

Use this contract for every preset from the first single-machine release.
Proposed inputs, to be implemented in the installer and OpenTofu roots:

```hcl
app_exposure = "private" # private | public
api_exposure = "private" # private | public
```

Private access requires authorized network reachability as well as normal Hot
authentication. Accept an existing VPN, peering/routed connection, or private
client/CI runner; document IAM-controlled Session Manager or IAP port
forwarding to the HTTPS ingress port for operator access and initial bootstrap.
Do not use SSH (see [No SSH access](#no-ssh-access)). Preserve the intended
HTTPS hostname through tunnels.
Installation outputs must explain the access prerequisites and private URLs.
Do not open public ingress as a fallback when the operator cannot connect.

| Concern | AWS | GCP |
| --- | --- | --- |
| Workloads | Private subnets; no EC2/ECS public IP assignment | VPC subnets; no external VM addresses |
| Private web ingress | Private VM ingress for one host; internal ALB when load balancing is needed | Private VM ingress for one host; internal Application Load Balancer for groups |
| Optional public web ingress | Internet-facing HTTPS ALB targeting private workloads | External HTTPS Application Load Balancer targeting private workloads |
| Operator access | Session Manager port forwarding to HTTPS ingress and Hot SSM command documents, with scoped IAM and required service connectivity; no SSH or interactive shell | IAP TCP forwarding to HTTPS ingress and the agent's Pub/Sub command topic, with scoped IAM and firewall rules; no SSH |

Only selected app/API routes may be published on public listeners. Keep
dashboard and API routing separable even when both run on one VM; a shared
reverse-proxy catch-all or spoofed Host header must not expose a private service.
Private-only services have no public route, including alternate provider URLs
in any future Cloud Run option. Restrict backend ports to authorized private
clients, ingress proxies, and health-check sources as appropriate. Apply the
policy to IPv4 and IPv6; retain authentication in every exposure mode.

Provision managed data services without public access and restrict their
firewalls to runtime/migration identities or approved sources. Local data
containers must not publish database/cache ports onto unrestricted host
interfaces. Existing services retain their ownership; validate private routing
and DNS from the actual runtime and migration networks. Any publicly addressed
existing dependency must be an explicit configuration exception.

Private ingress and outbound internet access are separate settings. Plan
outbound access for artifact/image downloads, secrets, logs, object storage,
email, OAuth, external APIs, and user tasks. New networks provide managed NAT
where internet egress is required; accept validated existing NAT/proxies.
Use AWS service endpoints or Private Google Access where appropriate.
Restricted egress requires an explicit dependency inventory and mirrored
artifacts when needed; do not imply air-gapped operation. Report NAT, endpoint,
and optional load-balancer costs independently of VM size.

Support private DNS and trusted TLS without requiring a publicly reachable
HTTP certificate challenge. This is on the critical path for private-default
installs, so the initial release fixes the mechanism:

- Private endpoints use an operator-supplied certificate and chain. The
  installation reads it from the secret store, and the operator's existing
  public or private CA issues it. The operator renews it; the installation
  reloads ingress on change and alerts before expiry.
- Public endpoints may instead use ACM certificates on AWS or Google-managed
  certificates on GCP, validated through DNS records the operator creates in
  their own DNS. The installation outputs the required records and waits for
  validation; it does not require write access to the operator's DNS.
- Document a cloud private CA (AWS Private CA, Google Certificate Authority
  Service) as an operator option, including its standing cost. Do not create
  one by default.
- Automated ACME DNS-01 issuance for a short list of DNS providers is a
  follow-up, not an initial dependency.

Tunnel-based access (SSM or IAP) must keep the certificate's hostname. Document
the client-side name resolution (private DNS zone, VPN DNS, or a hosts entry)
that maps it to the tunnel endpoint. Document email-link, OAuth callback, and
inbound-webhook reachability for private installations.
Accept existing network/subnet IDs and approved routes, DNS, and source ranges;
do not silently alter shared route tables, peering, or firewall policies.
GCP internal load balancing includes its proxy-only subnet and required probes.

The initial release qualifies private/private and public/public exposure (see
[Reference configurations](#reference-configurations)). Qualify each mixed
combination before marking it supported. Prove that default installs are
unreachable from the public internet but usable from authorized private clients,
that required egress works, and that public listeners cannot reach the other
private service or data/admin ports. Validate browser API dependencies rather
than weakening access policy to make a mixed configuration work. Test disabling
public access removes public reachability without replacing durable data.

### Outbound access for user code

The worker and task-worker need internet access for user code: `::hot::http`
and `::hot::tcp` calls, and box tasks. That code is untrusted and runs inside the
installation's private network. On one VM, the worker process shares the host
with PostgreSQL, Valkey, and the cloud metadata endpoint, and it legitimately
needs all three. Network rules cannot separate them from the worker process,
so Hot's application-level policy is the boundary for normal execution. The
host's guest firewall is the boundary for box tasks.

- **Isolation is mandatory.** The self-host configuration forces
  `hot.engine.isolation=true` for every service that executes user code, and
  validation rejects disabling it. The all-in-one template's `false` default
  never applies to a production installation.
- **Replace `network.allow-private` with an allowlist.** Operators can permit
  specific private CIDRs and hostnames for user code, for example internal
  APIs. A deny set always applies and cannot be overridden: IPv4 and IPv6 cloud
  metadata, the ECS task metadata endpoint, loopback, link-local, the host's own
  addresses, and the installation's PostgreSQL, Valkey, ingress, and agent
  endpoints. The allowlist is empty by default.
- **Box guests follow the same policy** through the host isolation chain in
  [Box task host](#box-task-host). Guests get internet access through NAT, plus
  allowlisted private destinations when configured. The same deny set applies.
- **Optional egress proxy.** Operators can route user-code traffic only
  through an HTTP(S) proxy for domain allowlisting or audit. Hot's own storage,
  database, Valkey, and secret-store connections bypass it. When a proxy is
  used, the proxy resolves and connects, so it must enforce the deny set
  itself. Qualify address pinning and redirect handling through the proxy.
- **Stable egress addresses.** New networks attach an Elastic IP to the NAT
  gateway (AWS) or reserve Cloud NAT addresses (GCP). Installation outputs
  publish them so customers can allowlist them at third-party APIs.
- **Mail ports.** GCP blocks outbound port 25 and AWS restricts it by default.
  The SMTP transport and preflight require port 587 or 465.
- **NAT cost.** Route storage traffic through S3 gateway endpoints or Private
  Google Access so it skips NAT. Report NAT data volume in cost reporting,
  because box-task traffic can dominate it.
- **Guest DNS.** Box guests use a configurable resolver. The default is a
  public resolver, as today. Restricted-egress networks supply an approved
  resolver, permitted explicitly in the guest chain.

Qualify from both normal and box execution: metadata over IPv4 and IPv6, local
PostgreSQL and Valkey, the host and other private addresses, DNS rebinding,
redirects to internal addresses, and alternate address encodings (decimal,
IPv4-mapped IPv6). All must be blocked while public destinations and
allowlisted private destinations work.

### Encryption and key management

Encryption is required for all presets. Inventory every provisioned service's
at-rest and in-transit controls, including supporting infrastructure. Set
encryption fields explicitly where the provider exposes them; where encryption
is automatic with no configurable switch, record and verify the service's
guarantee. Do not treat a private network as a substitute for TLS.

| Resource | Required configuration |
| --- | --- |
| VM/container storage | Encrypt boot and data disks, persistent volumes, snapshots, and configurable managed-container scratch storage. Keep application temporary files and swap on encrypted storage. Document any provider-only encryption or unsupported local/ephemeral disk choice. |
| PostgreSQL | Enable RDS/Cloud SQL at-rest encryption and require encrypted remote connections with server identity verification or a qualified authenticated connector. Cover replicas, backups, exports, and restores. |
| Redis/Valkey | Enable ElastiCache/Memorystore at-rest controls for persistence/backups and require TLS-only client connections with certificate verification. Qualify client support, reconnects, failover, and certificate rotation. |
| S3/GCS | Explicit bucket encryption policy for builds, files, internal blobs, multipart parts, backups, logs, and OpenTofu state. Use HTTPS-only access and reject insecure transport where service policy supports it. |
| Secrets, images, and logs | Configure encryption for secret stores, container registries, and log storage wherever controls are exposed. Inventory monitoring/audit destinations and exported data separately. |
| App/API ingress | Trusted HTTPS for private and public endpoints; configure TLS policies and certificate renewal. Re-encrypt load-balancer-to-backend traffic where supported and document backend certificate-verification behavior. |

Provider-managed keys are the default. Expose customer-managed encryption as a
policy with per-service AWS KMS key ARNs or Google Cloud KMS key references;
allow operator-owned keys or explicitly provisioned keys. Validate service,
region/location, engine/tier, provider, and key-policy compatibility before
creation. If a requested customer-managed policy is unsupported, fail validation
with the affected resource identified rather than silently using another key.

Grant only required runtime, service-agent, deployment, and restore access to
keys. Keep key material out of configuration, manifests, images, and OpenTofu
outputs. State, saved plans, and migration artifacts require protected storage
and access controls even when their cloud storage is encrypted.

Key ownership and lifetime must outlast encrypted data, retained snapshots,
backups, and state. Protect provisioned keys from normal teardown and accidental
destruction; external keys remain outside stack ownership. Document rotation,
old-key retention, key-access failure recovery, and restore permissions.
Changing a default key or rotating it does not imply existing objects/backups
have been re-encrypted. Treat immutable encryption choices and required resource
replacements as explicit migration work with a reviewed plan.

For existing services/buckets, validate TLS from the actual runtime and migration
clients and inspect encryption settings where permissions permit. Obtain
operator attestation where settings cannot be inspected; do not report unknown
at-rest settings as verified or modify external services automatically. Report
policy failures before installation and never retry with plaintext or disabled
certificate verification. Audit single-host local connections separately;
require TLS whenever PostgreSQL/Valkey traffic crosses hosts or reaches a
managed/existing remote endpoint.

For GCS, use bucket-default encryption with optional CMEK and qualify any
explicit per-request key selection. The runtime must honor an existing bucket's
encryption policy without changing it. Verify encryption of ordinary files,
builds, hidden blobs, temporary parts, and composed destination objects. Include
Cloud Storage service-agent KMS permissions and key-location compatibility;
retries must never fall back from a required key to provider-managed encryption.

Acceptance includes effective cloud settings, rejection of plaintext database/
cache connections, invalid-certificate rejection, isolated key-permission
failure/recovery tests, and restoration using retained keys. The initial
release qualifies provider-managed keys. Customer-managed keys are qualified
per service and cloud before they are marked supported. Record any service limitation explicitly; encryption at rest does not
claim to protect data in process memory or from an authorized application.

### Existing PostgreSQL and Redis or Valkey

Support bring-your-own data services, starting with existing PostgreSQL in the
initial single-machine release. Existing Valkey follows, qualified before it is
marked supported. Each dependency has its own mode; for example, use existing PostgreSQL with local
Valkey, or provision a managed database while using existing Valkey.

| Mode | Installation behavior |
| --- | --- |
| `local` | Start the corresponding data container with retained storage on the single host. |
| `managed` | OpenTofu provisions the cloud data service and manages its declared lifecycle. |
| `existing` | Configure an operator-supplied service; create neither its local container nor its managed cloud resources. |

For existing PostgreSQL, accept host/port/database, TLS settings, and separate
runtime and migration credential references. For existing Redis/Valkey, accept
endpoint(s), TLS/authentication references, cluster mode, and a supported logical
database selection where applicable. Keep credentials in secret storage.

Validate connectivity from the actual runtime network, supported versions and
commands, PostgreSQL extensions and restricted role setup, and suitable queue
durability/eviction settings. Use the designated database's supported `hot` schema
exclusively for this installation and an isolated queue keyspace.
Do not assume two Hot installations can safely share
the same Redis database; require a supported isolated logical database or
dedicated instance until configurable namespaces are implemented and tested.

Existing services remain outside this stack's resource ownership: no automatic
import, resizing, replacement, engine upgrades, backup-policy changes, or
deletion. Document required operator-side networking and administrative setup.
Applying Hot migrations and grants to the designated database remains an explicit
installation step, using supplied administrative access or operator-run commands.

Changing an already-provisioned service from `managed` to `existing` requires an
explicit ownership handoff and reviewed plan; it must not cause a conditional
resource removal to destroy customer data. Likewise, changing endpoints is a
data migration/cutover when an installation already contains data.

## Runtime image

The `hot` binary already includes the `app`, `api`, `worker`, `task-worker`,
and `scheduler` services and the `db` administration commands
([CLI](../../crates/hot_cli/src/cli.rs)). Package it once.

- Build one Hot runtime image per release for `linux/amd64`. It contains `hot`
  built with the S3, GCS (once qualified), and Kata features, plus `hot-std`,
  app assets, migrations, `provision_runtime.sql`, ONNX Runtime, `hotbox` for
  each supported guest architecture, and the task-worker host tools (`ctr`,
  `iproute2`, `iptables`).
- Every service and one-shot job runs the same image digest with a different
  command: app, API, worker, scheduler, task-worker, migrations, and
  onboarding. ECS task definitions and Compute Engine instance templates in
  the scalable preset reference the same digest.
- The release manifest pins the one Hot image digest plus upstream PostgreSQL,
  Valkey, and ingress proxy images by digest. Choose the ingress proxy in
  Phase 0.
- One image keeps services from drifting to mismatched versions on a VM and
  simplifies signing, mirroring, pre-pulling, and rollback.
- The trade-off is size and attack surface: app and API carry ONNX Runtime,
  `hotbox`, and developer subcommands they do not use. A slimmer service build
  can follow if measurements justify it.
- The task-worker runs from this image as a privileged container, the pattern
  proven in `hot-cloud` (see [Box task host](#box-task-host)). Running the same
  binary as a host systemd service remains a later evaluation.

## Secrets delivery

Installation secrets include database URIs, the session secret, the
context-encryption master key, SMTP and OAuth credentials, and any storage
credentials that are not covered by cloud identity. Hot reads them from files,
not from plaintext environment values.

### Hot runtime changes

- For any `HOT_<NAME>_FILE` variable, `apply_env_vars` reads the file and sets
  the configuration key that `HOT_<NAME>` would set, after trimming one
  trailing newline. Setting both `HOT_<NAME>` and `HOT_<NAME>_FILE` is an
  error. A missing, unreadable, or empty file stops startup with an error naming
  the variable and path, never the value.
- Configuration paths filled from files are always redacted, whatever their
  key names.
- Route every secret through resolved configuration, including
  `HOT_ENCRYPTION_KEY`, `HOT_APP_SESSION_SECRET` (removing its duplicate read
  path), and `HOT_REDIS_URI`. Audit the remaining direct `HOT_*` environment
  reads, and add a test that fails when a secret-like variable is read
  directly.
- Audit templates for values derived from `::env/get` during configuration
  evaluation; `_FILE` values arrive in the overlay step afterwards.
- Never place resolved secrets into the process environment or into the
  environment of child processes, `hotbox`, or task containers.
- Plain `HOT_<NAME>` values keep working for local development and for
  platforms that inject environment variables natively.

### Delivery per platform

| Platform | Delivery |
| --- | --- |
| Single VM and managed-data VM (AWS and GCP) | The host deploy agent fetches secrets with the VM identity at service start, writes one file per value to tmpfs under `/run/hot/secrets/<service>/` (mode `0400`, owned by the service user), and mounts each service's files read-only through Compose `secrets`. The service environment contains only `HOT_<NAME>_FILE` paths, so values do not appear in `docker inspect` or `/proc/<pid>/environ`. Files are removed when the service stops. |
| Compute Engine groups (GCP scalable) | Same agent pattern on each instance. |
| ECS (AWS scalable) | ECS task-definition `secrets` with `valueFrom` (Secrets Manager ARN or SSM parameter), fetched with a per-service execution role. These arrive as environment variables because ECS cannot mount secrets as files; document this exception. |

- Store secrets as one JSON bundle per service role, keyed by `HOT_*` names,
  plus shared secrets such as the encryption master key. The agent expands each
  key into a file. This keeps the secret count small, at roughly $0.40 per
  secret per month on AWS.
- Each service receives only its own files. Migration credentials go only to
  the migration job; the session secret only to the app.
- The agent's identity can read every installation secret on its VM. That is a
  documented single-host limitation; in-process fetching per service, with
  per-service identities, is a possible later enhancement.
- First boot generates the session secret and encryption master key and stores
  them with a create-if-absent operation, so they never enter OpenTofu state.
- Rotation: update the secret, then use the agent's `restart-service` command,
  which renders the files again. Live reload without restart is not in scope.
- `hot conf` and the diagnostic bundle show `HOT_<NAME>_FILE` paths and
  redacted values only.

## Release delivery and GitHub Actions

Infrastructure changes and application releases are separate operations with
separate permissions. Both can run from a GitHub Actions workflow in the
operator's own repository, or manually with the same commands.

### Release manifest and pointer

- Each Hot self-host release publishes a signed release manifest. It records
  the Hot runtime image digest, pinned upstream image digests, the host image, the configuration schema version, migration
  set and compatibility class, and the module tag it was qualified with.
- Each installation has a release pointer naming its desired manifest: an SSM
  Parameter Store parameter on AWS, or a Parameter Manager parameter on GCP.
  OpenTofu creates the pointer with an initial value and ignores later value
  changes. The release process owns the value.
- VM bootstrap data never contains a release version. A release therefore never
  replaces an instance or changes its user-data or metadata.

### Host deploy agent (VM presets)

A systemd-managed agent on the VM watches the pointer, polling on a short
interval, and also accepts requests on the command channel described below.
On a pointer change it:

1. Verifies the manifest signature and image digests, and rejects any
   unsupported version jump.
2. Pre-pulls images and runs host preflight checks.
3. Pauses admissions, drains work within the configured deadline, and takes a
   backup checkpoint when the migration class requires one.
4. Runs migrations with the migration identity, which is never the runtime
   identity.
5. Stops the old scheduler before any new scheduler starts, renders each
   service's secret files, then restarts services in dependency order.
6. Runs readiness and smoke checks from the host, so they work on private
   installations.
7. Records the result, completed steps, and the previous release.
8. On failure, rolls back automatically only when the migration class permits
   it. Otherwise it stops and reports the required forward fix or restore.

The agent writes redacted status to a per-installation status parameter, which
the workflow and operators read. Its identity can read the pointer and the
installation's secrets, consume its command channel, and write status and
diagnostics only. Missed triggers
converge on the next poll.

### No SSH access

No supported workflow, CI job, or helper uses SSH, on either cloud.

- Host images disable `sshd`. EC2 instances have no key pair. GCP instances
  set `block-project-ssh-keys` and do not enable OS Login. Firewall rules and
  security groups open no port 22, including from the IAP range.
- IAP TCP forwarding (GCP) and Session Manager port forwarding (AWS) connect
  only to the HTTPS ingress port, for private app/API access and initial
  bootstrap. They never connect to SSH.
- Session Manager interactive shells and the GCP serial console are not part
  of any documented workflow or CI role, and are disabled by default.
  Enabling either is an operator break-glass decision outside supported
  operation.

### Agent command channel

Operators and workflows reach the host only through the agent's command
channel. It accepts a fixed allowlist of operations: `deploy-now`, `status`,
`diagnostics`, `backup`, `restart-service`, and `drain-and-stop`. It never
accepts arbitrary shell commands.

| | AWS | GCP |
| --- | --- | --- |
| Request | SSM Run Command using Hot-defined command documents (never `AWS-RunShellScript`), targeted by installation tag | Message on a per-installation Pub/Sub topic; the agent consumes a pull subscription, so no inbound connectivity is needed |
| Result | Status parameter; diagnostic bundles in the installation's operations bucket prefix | Same |

Every request carries a request ID for idempotency and is authorized by cloud
IAM on the channel. The agent validates its schema and rejects unknown
operations. Results are redacted before they leave the host. The same request
schema is used on both clouds, so helper commands and workflows behave
identically.

For scalable presets, the release supplies the manifest version as an OpenTofu
input. The reviewed apply updates ECS task definitions and Compute Engine
instance templates, and the same drain and stop-before-start rules apply. See
[Release and deployment](#release-and-deployment).

### GitHub Actions workflows

Publish reusable workflows and composite actions in public `hot`, versioned with
the module tags. Operators call them from their own infrastructure repository.
That repository holds non-secret `tfvars`, the provider lockfile, and a pinned
`uses:` reference (tag or commit SHA).

| Workflow | Trigger | Does | Cloud role |
| --- | --- | --- | --- |
| `selfhost-plan` | Pull request | `tofu fmt`/`validate`, configuration and reference-configuration classification, cloud preflight, `tofu plan`. Posts a redacted plan summary. | Plan: read-only, plus state read and lock |
| `selfhost-plan` with `apply: true` | Merge or manual, gated by a GitHub Environment | A second job applies exactly the saved plan from the first; OpenTofu refuses it if state changed since the plan | Apply: provisioning |
| `selfhost-release` | Manual (`workflow_dispatch` with release version) or release promotion | Verifies the manifest, writes the pointer, sends `deploy-now`, waits for status, fails on timeout or failed checks | Release: write pointer, read status, send Hot command documents to tagged instances (AWS) or publish to the installation's command topic (GCP); no SSH or shell access |

Requirements:

- Authenticate with GitHub OIDC only: an AWS IAM role through
  `aws-actions/configure-aws-credentials`, or GCP Workload Identity Federation
  through `google-github-actions/auth`. Store no long-lived cloud keys in GitHub
  secrets. Trust policies restrict the repository, ref, and GitHub Environment
  through the token's `sub` claim. Each workflow gets its own role, matching
  the IAM matrix in section 4.
- Use GitHub Environments with required reviewers for production apply and
  release. Promote releases staging → production through environments.
- Serialize each installation and environment with a `concurrency` group
  (`cancel-in-progress: false`), in addition to the OpenTofu state lock.
- Saved plans can contain sensitive values. Encrypt them with the stack's
  OpenTofu encryption configuration, keep artifact retention short, and never
  print them unredacted in logs or summaries.
- Set minimal `permissions:` (`id-token: write`, `contents: read`, plus
  `pull-requests: write` for plan comments). Pin third-party actions by commit
  SHA.
- GitHub-hosted runners use only cloud control-plane APIs: state, plan/apply,
  pointer, commands, and status. Checks that need private network access, such
  as endpoint smoke checks and existing-database validation, run in the host
  agent or on an operator self-hosted runner inside the network. CI never opens
  public ingress.
- Ship an example operator repository and a GitHub Enterprise Server
  compatibility note. The reusable workflows are
  [`selfhost-plan.yml`](../../.github/workflows/selfhost-plan.yml) and
  [`selfhost-release.yml`](../../.github/workflows/selfhost-release.yml); the
  example operator repository is
  [`infra/github/example-operator-repo`](../../infra/github/example-operator-repo). Other CI systems call the same helper commands; the
  GitHub workflows are a supported convenience, not the only path.

Hot's own [release workflow](../../.github/workflows/release.yml) extends to
build, sign, and publish self-host images and manifests. Cloud qualification
runs in GitHub Actions against isolated test accounts and projects, using the
same reusable workflows and OIDC.

## Planned repository layout

Paths below are proposed additions, not existing installer commands.

```text
infra/
  README.md
  opentofu/
    bootstrap/
      aws/
      gcp/
    storage/
      aws/
      gcp/
    modules/
      aws/
      gcp/
    stacks/
      aws/
        single-machine/
        managed-data/
        scalable/
      gcp/
        single-machine/
        managed-data/
        scalable/
    examples/
      aws/
      gcp/
  runtime/
    compose/
    config/
    ingress/
    systemd/
  images/
    containers/
    packer/
    host/
  agent/
  scripts/
  tests/
  github/
    actions/
    example-operator-repo/
.github/workflows/
  selfhost-plan.yml
  selfhost-apply.yml
  selfhost-release.yml
resources/docs/self-hosting/
```

Share runtime manifests, configuration rendering, image contents, smoke tests,
and release contracts. Keep cloud resource implementations under their own
modules. Avoid a single root stack with conditionals for every cloud and preset.
Use separate roots for state bootstrap and the installation so teardown cannot
remove the only copy of its own state.

## Phase 0 Public runtime and installation contract

- [x] Build the single [runtime image](#runtime-image) from a clean
      public-only checkout using `hot_cli`. Wire required S3/GCS/Kata features
      explicitly and include every component listed there.
      Recipe: [`runtime.Dockerfile`](../../infra/images/containers/runtime.Dockerfile)
      (S3 and GCS always on, `--features kata`, statically linked ONNX
      Runtime, `hotbox`, `ctr`, the Linux package layout). The `selfhost`
      workflow builds and smoke-tests it when it changes, and the build passes
      there.
- [ ] Publish the versioned runtime image and host/runtime artifacts. Pin image
      digests, base images, upstream PostgreSQL/Valkey/ingress images, host
      dependencies, and artifact checksums; allow operators to mirror them into
      their own registries. Choose the ingress proxy.
      The release workflow's `selfhost-image` job pushes
      `ghcr.io/hot-dev/hot-runtime:<version>` and signs it. Upstream images are
      pinned in [`upstream-images.json`](../../infra/release/upstream-images.json);
      the ingress proxy is unprivileged Nginx, and `image_registry` mirrors
      every image by digest. Host dependencies are pinned by SHA-256 in
      `setup-host.sh`. Missing: a published host image. Operators bake it with
      `build-host.sh` from the newest Ubuntu 24.04 base, which is not pinned,
      and the manifest does not name it.
- [x] Implement `HOT_<NAME>_FILE` support, provenance-based redaction, and
      routing of every secret through resolved configuration, as described in
      [Secrets delivery](#secrets-delivery).
      Secrets outside configuration (`HOT_ENCRYPTION_KEY`,
      `HOT_APP_SESSION_SECRET`, OAuth client settings) go through
      `hot::environment::read_optional`, which applies the same `_FILE` rules;
      `crates/hot/tests/secret_env_reads.rs` fails on any direct read of a
      secret-like variable.
- [x] Enforce mandatory isolation for user-code services, replace
      `network.allow-private` with the allowlist and fixed deny set, and add the
      optional user-code egress proxy, as described in
      [Outbound access for user code](#outbound-access-for-user-code).
      Self-hosted production workers refuse isolation off;
      `network.allow-cidrs`, `allow-hosts`, and `deny-cidrs` sit behind a
      fixed deny set; the runtime bundle denies its own networks; and
      `network.egress-proxy` sends user-code HTTP(S) through the operator's
      proxy (raw TCP is refused then).
- [ ] Create shared Compose and systemd supervision for app, API, worker,
      scheduler, task-worker, PostgreSQL, Valkey, and HTTPS ingress.
      Configure orderly startup and shutdown, local liveness, and separate
      installation readiness checks that exercise dependencies.
      The renderer's Compose bundle restarts services (`unless-stopped`),
      gives them 120 seconds to stop, and health-checks PostgreSQL, Valkey,
      and the scheduler's heartbeat. systemd runs the agent, Kata's
      containerd, the guest firewall, and the data-disk mount, and the agent
      starts the scheduler last and stops it first. Missing: readiness checks
      that exercise dependencies. Readiness probes `/signin` and `/status`,
      which touch neither PostgreSQL nor Valkey, and dependencies get only a
      TCP check. The app, API, and workers have no container health checks.
- [x] Set `HOT_ENV=production`, `HOT_PRODUCT_EXPERIENCE=self-host`, billing off,
      explicit endpoint URLs, persistent session/encryption keys, and restricted
      runtime database credentials. Disable custom-domain provisioning.
      The renderer sets these for every service (`common_env`); billing needs
      Hot Cloud mode, and `domain.mode` defaults to `none`. The agent
      generates the session and encryption keys once (`put_secret_if_absent`),
      and the runtime login is `NOSUPERUSER NOBYPASSRLS`.
- [x] Implement public email delivery and the first-admin bootstrap/recovery
      flow. Persist generated secrets across retries and upgrades. Test normal
      authentication without development fallbacks, email verification,
      invitations, alert delivery, and optional OAuth callbacks.
      SMTP delivery, `hot admin bootstrap`/`reset-password`, and `app.signup`
      are in; without email, the sign-up form admits only invited users and
      admins share invitation links. The agent generates the first admin's
      password once and runs `hot admin bootstrap` on every deploy.
- [x] Run extension installation, public migrations, grants, and onboarding as
      distinct administrative steps before starting runtime services. Respect
      the supported `hot` schema and RLS requirements in the existing runbook.
      Migration credentials never become normal service credentials.
- [ ] Define signed release manifests, supported configuration schema, size
      maps, service dependencies, reference-configuration classification, and
      stable outputs for endpoints and resource IDs.
      Manifests are defined and signed keylessly by the release workflow
      ([`release_manifest.py`](../../infra/release/release_manifest.py)); the
      renderer and agent verify them with `cosign`. The configuration schema
      is the contract module's `installation` type, checked again by
      `render.validate`. Sizes map in `local.sizes` and `SIZES`, Compose
      `depends_on` orders services, and the stacks output endpoints and
      resource IDs. Missing: reference-configuration classification (every
      root takes a blanket `acknowledge_unqualified`, and `render.py` reports
      `unqualified-development`), and the manifest does not name a host image.
- [x] Add scheduler lock-liveness enforcement as described in
      [Scheduler availability](#scheduler-availability-and-deferred-failover),
      with tests for connection kill and lock loss.
      [#159](https://github.com/hot-dev/hot/pull/159): a bounded check every
      two seconds, a startup check that rejects transaction-mode poolers, and
      a PostgreSQL test that kills the lock session.
- [ ] Implement independent private-default app/API exposure and validate
      endpoint, client-network, DNS/TLS, and egress requirements. Implement the
      initial certificate mechanism: operator-supplied certificates, plus
      DNS-validated cloud certificates for public endpoints.
      Implemented: `app_exposure` and `api_exposure` are each private or
      public. A public endpoint gets a cloud load balancer (an ALB with the
      operator's DNS-validated ACM certificate on AWS; a global external ALB
      with a Google-managed certificate on GCP) that re-encrypts to a separate
      host port serving only public hostnames; the VM stays private. App and
      API trust forwarded client addresses from Nginx and the load balancer
      only. Qualification remains.
- [ ] Ensure public binaries include required PostgreSQL/Valkey TLS support,
      trusted CA handling, and strict verification; validate the encryption
      policy and immutable creation-time settings before provisioning.
      `sqlx` and `redis` use rustls, and `redis.ca-cert` trusts a private CA.
      The agent requires `sslmode=verify-full` with the operator's CA for
      existing PostgreSQL, verifies RDS's hostname and Cloud SQL's instance
      CA, and uses `rediss://` for remote Valkey. Missing: an encryption
      policy to validate, since every resource uses provider-managed keys;
      creation-time settings are checked only as contract inputs.
- [ ] Implement independent data-service modes and existing-endpoint validation.
      Omit provisioning and local containers for each existing dependency;
      verify teardown without owning external services. Qualify existing
      PostgreSQL initially, including its pooler mode for the scheduler.
      Implemented: `data.postgres.mode` and `data.valkey.mode` (`local`,
      `managed`, or `existing`). Existing PostgreSQL requires
      `sslmode=verify-full` with the operator's CA, a read-only
      `check-postgres` job checks the schema, extensions, and runtime login
      before migrating, and its addresses are denied to user code; existing
      Valkey requires `rediss://`. The scheduler refuses a session-breaking
      pooler at startup (#159). Qualification on each cloud remains.
- [ ] Implement independent storage-role/backend and bucket-ownership inputs.
      Qualify the Local/S3 reference configurations, including a GCP VM using
      existing S3 without GCS setup.
      Builds and files each select local, S3, or GCS (GCS on GCP compute, with
      the VM identity and IAM limited to the prefix), as an existing bucket or
      one the stack provisions (`provision`). Qualification remains.
- [x] Make migration and bootstrap retries idempotent; a migration failure
      blocks rollout. Record versions and completed steps without exposing secrets.
      `migrate`, `provision`, and `hot admin bootstrap` are safe to rerun, and
      an interrupted deploy resumes where it stopped. The agent records each
      deploy's versions and steps, and a migration failure stops the rollout
      with recovery guidance.

Completion: using only public source/artifacts, start the entire stack on a
Linux host and sign in, create a project, deploy a build, serve an HTTP request,
execute an event, a scheduled invocation, and a box task, and upload/download
a file. Prove tenant/environment isolation and disabled billing/domain flows.

## Phase 1 Single-machine AWS and GCP installs

The cloud tracks can run in parallel once the shared runtime contract is stable.
Both use the same application images and acceptance suite.

### Initial single-machine release gates

The initial supported release on each cloud requires all of the following,
in addition to the functional, network, encryption, and recovery checks below:

| Area | Required evidence before release |
| --- | --- |
| Installation preflight | Validate permissions, quotas, regional capacity prerequisites, network/DNS/TLS, existing dependencies, and box requirements before provisioning. Report actionable failures without leaving avoidable partial resources. |
| Upgrade compatibility | Publish the supported version matrix, upgrade sequence, migration rules, and rollback boundary. The first release has no earlier qualified release, so exercise an upgrade from a representative fixture (a pre-release build with populated data) and its supported recovery path. Later releases upgrade from earlier qualified releases. |
| Release delivery | Deploy a release and a rollback to each reference configuration through the GitHub Actions workflows with OIDC-only credentials, and through the manual commands. Show that a release does not replace the instance and that concurrent runs are serialized. |
| Recovery targets | Publish numerical data-loss and recovery-time targets, backup frequency/retention, and measured full-restore evidence for the qualified configuration on each cloud. |
| Trust and IAM | Publish the code-author trust model and identity/permission matrix. Verify normal Hot executions and box tasks cannot obtain cloud credentials or reach protected infrastructure outside their declared capabilities. |
| Basic diagnostics | Provide documented status, version/configuration checks, service logs, dependency checks, and a redacted diagnostic bundle usable on a private installation. |
| Logging and monitoring baseline | Structured logs for all services, bounded local rotation, host/service health, scheduler heartbeat, critical alert rules and a tested configured notification path. Verify selected export and collector-failure behavior. |
| Repeatable security updates | Exercise a documented process to rebuild, verify, publish, install, and validate a pinned security release, with safe drain/replacement and a stated rollback or restore path. |

All eight operational areas below remain part of the implementation plan.
Broader diagnostic automation, release-security tooling, drift automation, and
capacity/cost tooling can follow this initial gate. Existing requirements for
safe resource limits, protected state, encryption, and retained data still apply.

### Box task host

`hot-cloud` runs box tasks in production on AWS. Use it as the reference
implementation and port it to the public repository, adapting it for one VM and
for GCP.

What exists today:

- **Instance:** `m8i.large` with EC2 nested virtualization
  (`CpuOptions.NestedVirtualization: enabled`), not bare metal. One task-worker
  per instance through an ECS capacity provider. IMDSv2 required with hop
  limit 1, encrypted root and data EBS volumes, ECS Exec off by default, and
  private subnets behind a NAT gateway with an Elastic IP.
- **Host image:** a Packer AMI on ECS-optimized Amazon Linux 2023 with Kata
  3.28.0 running QEMU through the Go shim. Firecracker is installed but unused
  because Dragonball kernel-panics under nested KVM. A dedicated
  `kata-containerd` systemd service exposes a `0660` socket owned by group
  2000. A devmapper thin pool on sparse loopback files (100G data, 10G
  metadata) on the data volume holds guest filesystems. Kata guests default to
  1 vCPU and 512 MB, with annotation overrides, `sandbox_cgroup_only`, and host
  seccomp. The guest rootfs is content-addressed and checked by SHA-256.
- **Container:** a separate x86_64 image run with `privileged: true`,
  `pidMode: host`, and user `0:2000`. It mounts `/run/kata-containerd`,
  `/run/vc`, `/var/run/netns`, `/opt/cni/bin` (read-only), `/var/lib/cni`,
  and `/tmp/hot-box-io`. Its health check matches its own cgroup because the
  host PID namespace shows other `hot` processes.
- **Guest network:** CNI bridge `kata-br0` on `10.88.0.0/16` with masquerade.
  A `HOT-KATA-ISOLATION` chain drops traffic to `169.254.169.254`,
  `169.254.170.2`, `169.254.0.0/16`, `10.0.0.0/8`, `172.16.0.0/12`,
  `192.168.0.0/16`, `100.64.0.0/10`, multicast, and the guest subnet.
  Guest-to-guest traffic, guest-to-host traffic, and all guest IPv6 are
  dropped. `br_netfilter` is persisted so bridged traffic passes through the
  rules. Guests use public DNS resolvers.
- **Rollout:** the AMI is rebuilt when host files change. Boot user-data still
  downloads `setup-kata.sh` from S3, verifies its SHA, and runs it while the
  instance is held in DRAINING.
- **Guardrails:** CI runs `validate-task-worker-isolation.sh`, which checks the
  hop limit, encryption, the full drop list, socket mode, and the absence of
  `docker.sock`.

Changes for self-hosting:

- [ ] Port the host setup into the shared Packer recipes. Bake everything into
      the image; boot only activates services and never downloads and runs
      setup scripts. Ship only the QEMU path.
      Recipe: [`setup-host.sh`](../../infra/images/host/setup-host.sh) and
      [`hot-host.pkr.hcl`](../../infra/images/packer/hot-host.pkr.hcl), Ubuntu
      24.04 on both clouds, built without SSH (`communicator = "none"`). Check
      this once a real build passes on each cloud.
- [ ] Run the task-worker from the single runtime image with the same
      privileges and mounts. Qualify its Compose networking on one VM.
      The renderer runs it from the Hot image as `0:2000`, privileged, in the
      host PID namespace, with the `hot-cloud` mounts. Qualification on a VM
      remains.
- [x] Put the devmapper pool on its own volume, or give it a hard size limit
      derived from the size label, so box storage cannot fill the PostgreSQL
      disk.
      Sparse pool files sized by `HOT_BOX_POOL_GB` in `/etc/hot/host.env`;
      the agent writes it from the size label.
- [ ] On one VM, containers use the instance identity, so AWS needs IMDS hop
      limit 2. The guest chain and the `PublicOnly` policy remain the
      metadata boundaries for box and normal execution. Qualify both.
      The AWS stacks set `http_put_response_hop_limit = 2`, the guest baseline
      drops 169.254.0.0/16, and the bundle forces isolation for every service.
      Qualification remains.
- [x] Extend the guest chain with the user-code allowlist and fixed deny set
      from [Outbound access for user code](#outbound-access-for-user-code), and
      make the guest DNS resolver configurable.
      The task worker's per-task chains read `network.allow-cidrs`,
      `network.deny-cidrs`, and `box.guest-dns`; the host's baseline runs in
      the mangle table so per-task rules cannot bypass it.
- [x] Derive task concurrency, memory, and disk settings from the VM size
      instead of the standalone task-worker defaults.
      `SIZES` in [`render.py`](../../infra/runtime/render.py) sets every
      container's memory and CPU caps, Valkey's `maxmemory`, and the task
      limits per size, within the VM and leaving room for the OS. Starting
      values to replace with benchmarks.
- [ ] Port the isolation validation into public qualification checks, run
      against the built image as well as the scripts, and extend it to GCP.
      Static checks are in [`test_host.py`](../../infra/tests/test_host.py);
      checks against a built image and a running host remain.

### Shared VM lifecycle

- [ ] Create versioned Packer recipes with Docker/Compose plus the
      [Box task host](#box-task-host) setup. Configure the task-worker's host
      PID/mount access explicitly.
      `hot-host.pkr.hcl` pins its plugins and names each image for its Hot
      release; the bake installs Docker and Compose from Ubuntu and records
      versions in `versions.json`. The renderer sets the task-worker's host
      PID namespace and mounts. A real build on each cloud remains.
- [ ] Encrypt boot/scratch storage and attach a retained encrypted data disk
      separately from replaceable boot state. Mount PostgreSQL, Valkey, local
      builds/files/blobs, and required TLS state consistently. An absent expected
      disk must stop startup rather than initialize an empty installation.
      AWS boot and data volumes set `encrypted = true`; GCP disks use Google's
      encryption. The data disk is retained (`prevent_destroy`),
      `hot-data-disk` refuses a missing or blank disk unless a first install
      passes `--allow-format`, and Docker waits for the mount.
      `prepare_host` creates the directories the bundle bind-mounts, giving
      local builds and files to UID 10001, and `hot-data-disk` grows the
      filesystem when the disk grows. Qualification remains.
- [x] Make first-boot scripts idempotent; upgrades never format existing disks.
      Limit bootstrap to identity, mounts, secret retrieval, reading the release
      pointer, and service activation. Bootstrap data stays stable across releases.
      The boot script only writes `/etc/hot/installation.json` and starts the
      agent, and it names no release. The agent mounts the disk, formatting it
      only on a first install, creates secrets if absent, and reads the
      pointer.
- [ ] Implement the host deploy agent and release pointer from
      [Release delivery](#release-delivery-and-github-actions), with status
      reporting and the drain, migrate, and stop-before-start sequence.
      [`hot_agent.py`](../../infra/agent/hot_agent.py) (standard-library
      Python, shipped in the runtime image and installed by the host bake)
      verifies signed manifests, delivers secrets as tmpfs files, deploys in
      order (old scheduler stopped first, migrations before services, new
      scheduler last), checks readiness through the ingress, and reports
      status. The stacks create the pointer, status, secret containers, and
      command channel (an SSM command document on AWS; Pub/Sub on GCP). On
      GCP the pointer is a Secret Manager secret, which has a `latest` alias,
      and status goes to Cloud Logging. Missing: a normal deploy neither
      pauses admissions nor drains; the old app, API, and workers run until
      they are replaced (`drain-and-stop` is separate). A real deployment
      remains.
- [x] Use VM identity to retrieve secrets; keep secret values out of images,
      instance metadata/user-data, command logs, process environments, and
      ordinary OpenTofu outputs. Deliver them as tmpfs files through the agent
      as described in [Secrets delivery](#secrets-delivery).
      Generate session and encryption keys on first boot and store them in the
      secret store through a create-if-absent operation, so they never enter
      OpenTofu state. Use ephemeral values or write-only arguments for any
      secret OpenTofu must pass. Document any remaining state-held secret and
      restrict its access.
- [x] Configure local Valkey persistence explicitly: AOF enabled, a stated
      `appendfsync` policy, and a matching published recovery-point target.
      Include the AOF in backup and restore procedures.
      `appendonly yes` with `appendfsync everysec`; the AOF lives on the data
      disk, which is snapshotted daily (infra/README.md, Backups and alerts).
- [ ] Implement the private-network contract, with no public workload addresses
      and private app/API ingress by default. Test optional public ingress and
      operator access; PostgreSQL, Valkey, and task internals remain private.
      No VM has a public address, ingress publishes only 443 (and 9443 for a
      public endpoint's load balancer), the firewall admits `client_cidrs`
      (and IAP on GCP), and data services publish no ports. Testing remains.
- [ ] Verify guest tasks cannot reach host databases, cloud metadata,
      control-plane sockets, or other guests, while authorized file-service
      access and outbound networking continue to work.
      The host's mangle-table baseline drops the host, metadata, other guests,
      and private ranges outside the allowlist, and `test_host.py` checks it
      statically. Verification on a running host remains.
- [x] Provide health/status, logs, backup, restore, upgrade, and shutdown
      helpers. Keep dependency outages from triggering endless liveness restarts.
      `selfhost.py` runs the agent's `status`, `diagnostics`, `logs`,
      `backup`, `roll-back`, `deploy-now`, `restart-service`, and
      `drain-and-stop` commands, and snapshot restores use
      `backup.restore_snapshot`. Health checks never restart a container, so
      an outage only reports.
- [ ] Define shutdown/drain deadlines and queue recovery behavior; replacement
      starts only after storage ownership and scheduler ownership are safe.
      Services get 120 seconds to stop; `drain-and-stop` stops the scheduler
      first, then workers, then the web services; data moves drain for up to
      15 minutes. The data disk attaches to one VM, and the scheduler's lock
      fences a second scheduler. Missing: queue recovery after an abrupt stop
      is not documented.

### AWS track

- [x] Bootstrap or reuse an S3 state bucket with versioning, encryption, access
      restrictions, and locking. Enable OpenTofu client-side state and plan
      encryption with an AWS KMS key provider. Pin OpenTofu and AWS provider
      versions.
      [`bootstrap/aws`](../../infra/opentofu/bootstrap/aws) creates a versioned,
      TLS-only, never-public state bucket and, optionally, GitHub OIDC roles
      for plan, apply, and release. Stacks grant the release role only their
      own pointer, status, and command (`release_principal`).
- [ ] Provision VPC/subnets or validated existing networking, firewall rules,
      EC2 identity, stable endpoint, retained EBS data volume, and the host image.
      Include private routing, required egress, Session Manager connectivity,
      Hot SSM command documents, and public ALB resources only for explicitly
      selected public web access. Create no key pair and no port 22 rules.
      `stacks/aws/single-machine` creates or reuses the VPC, with a private
      subnet behind NAT and an Elastic IP, an S3 gateway endpoint, the host
      role, a retained encrypted EBS volume, the SSM command document, and an
      ALB only for public endpoints; it creates no key pair or port 22 rule.
      Missing: a stable private endpoint, since the `private_ip` output
      changes when the instance is replaced.
- [ ] Use `m8i` as the reference instance family, as proven in `hot-cloud`.
      Qualify other nested-virtualization families and regions before listing
      them. `m8i` sets the AWS price floor for the `small` size. Enable nested
      virtualization in the launch configuration and run a real Kata job during
      qualification.
      Sizes use `m8i.large` to `m8i.2xlarge`, with nested virtualization
      enabled (`cpu_options`). Qualification, including a real Kata job,
      remains.
- [ ] Configure administration through documented AWS access and installation
      HTTPS. Store secrets in Secrets Manager, with a KMS key per the
      encryption policy and VPC endpoint access from private subnets.
      Secrets are in Secrets Manager under the AWS-managed key, reached
      through NAT. Missing: a customer-managed key option, Secrets Manager and
      SSM interface endpoints, and documentation of Session Manager port
      forwarding to 443, which the host role allows.
- [ ] Add optional S3 build/file buckets with runtime IAM and explicit
      retention/deletion behavior; local disk remains the initial default.
      Honor existing-bucket selections and the independent storage contract,
      including GCS after qualification.
      `storage.<role>.provision` creates the bucket: encrypted, public access
      blocked, TLS-only, versioned with `noncurrent_days` retention, abandoned
      uploads removed after 7 days, and never force-destroyed. The plan
      workflow's durable-resource guard covers it.
      AWS compute cannot use GCS yet: the contract requires `cloud = gcp` for
      it.

### GCP track

- [x] Bootstrap or reuse a protected state backend; provide GCS as the native
      example and support existing S3 state independently of compute cloud.
      Enable versioning, encryption policy, access restrictions, and locking.
      Enable OpenTofu client-side state and plan encryption with a Cloud KMS
      key provider. Pin OpenTofu and Google provider versions. State storage is independent
      of Hot's application-level GCS implementation.
      [`bootstrap/gcp`](../../infra/opentofu/bootstrap/gcp) creates the GCS
      state bucket and, optionally, a Workload Identity pool limited to the
      operator's repository with plan, apply, and release service accounts.
- [ ] Provision or reuse project networking, required APIs, firewall rules,
      service account, stable endpoint, retained persistent data disk, and host
      image. Avoid disabling shared project APIs on installation removal.
      Include private routing, required egress, IAP forwarding to the HTTPS
      port, the agent's Pub/Sub command topic and subscription, and external
      ingress resources only for explicitly selected public web access. Set
      `block-project-ssh-keys`, leave OS Login off, and create no port 22 rules.
      `stacks/gcp/single-machine` creates or reuses the network, with Cloud
      NAT on a reserved address and Private Google Access, enables APIs with
      `disable_on_destroy = false`, admits 443 only from `client_cidrs` and
      IAP, and adds the service account, retained disk, Pub/Sub commands, and
      a load balancer only for public endpoints; SSH keys, OS Login, and the
      serial port are off. Missing: a stable private endpoint, since the
      instance's internal address is not reserved.
- [ ] Select a supported nested-virtualization machine family/image and check
      organization policy and regional availability. Run the same real Kata
      qualification as AWS.
      Sizes use `n2-standard-*` on Cascade Lake or later with nested
      virtualization, and preflight checks the machine type in the zone and
      the `compute.disableNestedVirtualization` policy. The real Kata
      qualification remains.
- [ ] Configure documented GCP administration access and HTTPS. Store
      secrets in Secret Manager, retrieved by the agent with the attached
      service account through Private Google Access.
      The agent reads Secret Manager with the VM's service account through
      Private Google Access, and the firewall admits IAP to 443. Missing:
      documentation of IAP TCP forwarding for operators, and the IAM it needs.
- [ ] Run the full stack with local build/file storage without AWS credentials.
      Also qualify existing S3 with explicit AWS identity and no GCS application
      storage provisioning. Connect optional GCS buckets when Phase 2 passes.
      `gcp-local.json` and `gcp-existing-s3.json` cover both; S3 keys come
      from the operator secret (`HOT_BUILD_AWS_*`, `HOT_FILE_AWS_*`), and GCS
      uses the VM's identity. Qualification remains.

Completion on each cloud: fresh install, interrupted-install retry, unchanged
second plan, reboot, stop/start, a release and a rollback through the deploy
agent (from GitHub Actions and manually), host replacement with retained data,
scheduler lock loss, and explicit teardown behavior. Restore a database backup and its required
object data into a fresh installation and rerun application smoke checks.
Meet the initial release gates above on each cloud before marking its
single-machine preset supported. Optional GCS-backed single-machine storage is
qualified after Phase 2 and does not require changing to the managed-data preset.

Single-machine backups must be application-consistent and stored outside the
VM. Define PostgreSQL backup/restore, Valkey persistence and pending-work
reconciliation, object/blob recovery, and restoration of session/encryption
material. A disk snapshot alone is not the complete recovery procedure.

## Phase 2 Native Google Cloud Storage

Status: the `gcs-storage` feature and the GCS build, file, and blob backends
landed in #104 (multipart uploads use temporary parts and composition).
Re-check the items below against it before closing them; bundle and stack
support for selecting GCS is still open.

This phase begins alongside Phase 1. Its completion gates GCS-backed application
storage on either compute cloud. It does not gate GCP managed-data/scalable
configurations that use qualified S3 storage, or a single-host configuration
using retained local storage.

- [x] Add a `gcs-storage` feature and maintained Rust client dependencies.
      Evaluate the official Google Cloud Storage Rust SDK against streaming,
      generation preconditions, composition, and authentication requirements;
      pin compatible versions in the workspace lockfile.
      `crates/hot` has the feature on Google's official `google-cloud-storage`
      SDK, pinned in `Cargo.lock`, and `hot_cli` enables it.
- [x] Implement `GcsBuildStorage`: byte/path upload, retrieval, existence,
      deletion, backend identification, and stable `gs://bucket/key` locations.
      Override path uploads to avoid loading staged builds entirely into memory.
      `storage::gcs::GcsBuildStorage` streams staged builds from disk
      (`put_file`) and stores `gs://bucket/key` locations.
- [x] Implement the full `FileStorage` interface: writes, reads, streaming reads,
      database-backed metadata/listing, deletes, conditional writes, multipart
      lifecycle, and hidden blob bytes used by persisted execution data.
      `file_storage::gcs::GcsFileStorage` implements every method; listings
      come from the database.
- [x] Preserve org/environment key boundaries and existing path normalization.
      Use `storage_backend="gcs"` and canonical storage paths. Keep internal
      blobs outside user-visible Files listings.
      Keys are `{prefix}/{org}/{env}/...`, records name the `gcs` backend, and
      blobs never reach listings, which come from the database.
- [x] Preserve conditional-write semantics: stage unique object bytes, commit
      the database compare-and-swap, then clean up losing/superseded objects.
      Keep application ETags distinct from GCS generations and ETags.
      Cleanup must not delete objects still referenced by committed metadata.
      Writes land at unique `.v-` or `.cas-` keys and the record swap commits;
      a losing write's object is deleted by generation, and ETags are Hot's
      MD5.
- [ ] Adapt numbered, retryable multipart uploads explicitly. A sequential
      resumable session alone does not satisfy Hot's API. First evaluate GCS
      XML API multipart uploads: they provide numbered, replaceable parts with
      list, complete, and abort, which map closely onto Hot's S3-shaped API.
      Check SDK support, authentication with Application Default Credentials,
      CMEK behavior, and part limits. Fall back to durable temporary parts with
      generation-aware composition only if the XML API does not qualify. Either
      path must support replacement, retries, abort, and expiration cleanup.
      Implemented with the fallback: parts are generation-pinned objects under
      `.uploads/`, composed in stages (`compose_staged`), and abort and the
      worker's expired-upload sweep remove them. Missing: a record of why the
      XML API did not qualify.
- [ ] Add `hot.build.gcs.bucket/prefix`, `hot.file.gcs.bucket/prefix`, and
      corresponding `HOT_BUILD_GCS_*` / `HOT_FILE_GCS_*` environment variables.
      Document and test configuration precedence across every service/template.
      `GcsConfig::resolve` reads `HOT_<ROLE>_GCS_*`, then `<role>.gcs.*`, with
      a test, and `configuration/storage.md` documents the precedence. Every
      service template lists the GCS settings beside the S3 ones.
- [x] Use Application Default Credentials, attached service accounts in GCP,
      and federated credentials for CI. Do not require runtime service-account
      key files or invoke `gcloud` from storage operations.
      `GcsClient::connect` uses Application Default Credentials and never runs
      `gcloud`; `gcs-integration.yml` authenticates with Workload Identity
      Federation.
- [ ] Supply optional bucket provisioning with scoped IAM, public-access
      prevention, encryption/retention policy, and abandoned-upload cleanup.
      For existing buckets, validate requirements without assuming ownership.
      Differentiate permission errors, missing objects, conditional conflicts,
      and retryable failures.
      The GCP stack provisions buckets on request: uniform access, public
      access prevention, and versioning with noncurrent retention. Existing
      buckets are only granted to, never managed, and preflight checks only
      that they exist. `GcsError` separates the failure kinds. A lifecycle
      rule deletes week-old objects under each prefix's `.uploads/`, where Hot
      stages multipart parts. There is no CMEK option yet.
- [ ] Honor bucket-default encryption and optional CMEK for files, builds,
      blobs, temporary parts, and compose outputs. Test KMS permissions,
      rotation/old-object reads, and required-key failure without fallback.
      Hot never sets a per-object key, so every object follows the bucket's
      default encryption. Missing: a CMEK option in the stacks, and the KMS
      permission, rotation, and required-key tests.
- [x] Audit build/file/blob loading and cleanup for S3-specific dispatch and
      path parsing. Keep existing Local/S3 records readable under their supported
      configuration; changing a backend does not silently rewrite data.
      Backends are selected strictly (`resolve_storage_backend`), records from
      another backend or bucket are refused (`ensure_record_backend`,
      `key_from_uri`), and only `hot db move-storage` rewrites them.

Completion: shared backend contract tests plus a real temporary GCS bucket
integration run using workload identity. Cover deploy/execute round trips,
large-file streaming, conditional-write races, quota rejection, tenant isolation,
blob spill/rehydration, out-of-order/replaced multipart parts, composition across
multiple requests, retry/abort/expiry cleanup, and metadata/object failure
recovery. Retain Local and S3 regression coverage. Emulators alone do not qualify
IAM, generation preconditions, or real composition behavior.

## Phase 3 Managed-data preset

- [ ] Reuse the single-VM runtime with local PostgreSQL/Valkey disabled.
      Provision RDS PostgreSQL and ElastiCache Valkey on AWS; Cloud SQL
      PostgreSQL and Memorystore for Valkey on GCP. Support validated existing
      endpoints as an alternative.
      PostgreSQL is done as `data.postgres.mode = "managed"` in the
      single-machine stacks (`stacks/*/single-machine/postgres.tf`), not a
      separate root, so each service chooses local, managed, or existing
      independently. Managed Valkey is done on AWS (ElastiCache, TLS-only,
      with an AUTH token written only to write-only attributes, and
      `noeviction`). On GCP it is Memorystore for Valkey, with cluster mode
      disabled, TLS, and IAM authentication. Hot gained `redis.auth = gcp-iam`
      (metadata-server tokens through redis-rs's streaming credentials,
      re-authenticating connections when the token changes) and
      `redis.ca-cert` for the instance's private CA. Both were checked against
      a live TLS Valkey; Memorystore itself is unqualified.
- [ ] Configure private connectivity, TLS, managed secrets, database versions,
      required extensions, owner/runtime roles, capacity, backups, and
      independently selectable failover/replica settings.
      Explicitly enable at-rest encryption and TLS-only access under the
      encryption policy, including replicas, snapshots, and restore targets.
      Done for PostgreSQL, apart from replicas and restore targets:
      - Connectivity: the host's security group only on AWS; private
        services access on GCP.
      - TLS: required by the server, with the RDS hostname or the Cloud SQL
        instance CA verified by clients.
      - Secrets: the administrator password stays out of OpenTofu state
        (an RDS-managed secret; write-only attributes on GCP).
      - Database: PostgreSQL 18, with logins, database, schema, and
        extensions created by the agent.
      - Capacity and backups: size-derived tiers; point-in-time backups;
        selectable Multi-AZ or regional HA; deletion protection; encrypted
        storage.
- [ ] Validate Redis Streams, consumer groups, Lua operations, reclaim,
      cluster-aware connections, and failover against each managed offering.
      Configure queue-appropriate persistence/eviction and monitoring.
      Adding shards does not distribute one stream's write load automatically.
      Configured: `noeviction` on ElastiCache and Memorystore, AOF on
      Memorystore, TLS on both, and the dependency and queue alerts.
      Validation against each managed offering remains.
- [ ] Use shared S3/GCS build and file storage by default; keep VM scratch data
      disposable. Make database connection budgets fit the selected size.
      Accept retained local storage while all services remain on one VM;
      preserve its disk and backup lifecycle. Keep S3/GCS independent of compute
      cloud and preserve externally owned buckets.
      Storage is chosen per role with no default, local storage stays allowed
      on one VM, existing buckets are never managed, and a scalable
      installation refuses local storage. Missing: GCS from AWS compute, and a
      connection budget for a single machine on managed PostgreSQL; only the
      scalable contract checks one.
- [x] Implement local-to-managed migration instructions/tools: pause admissions,
      drain or reconcile in-flight work, back up/copy data, verify objects and
      metadata, switch connections, validate, and resume. Retain a documented
      recovery point and account for writes made after cutover.
      Changing `data.*.mode` triggers the move during the next deploy (see
      [Moving data off the host](../../infra/README.md#moving-data-off-the-host)).
      The agent stops admissions, drains a moving queue, and stops workers.
      It dumps the local database, which is also the recovery point, and
      restores it in one transaction into the prepared target, refusing a
      target that already holds Hot's tables. Then it migrates and resumes.
      The move commits once the restore succeeds; before that, a failure
      restarts the installed release on the untouched local data. No writes
      happen after the dump, because Hot is stopped. Reverse and
      managed-to-existing moves are refused.
- [x] For storage moves, create an old-to-new object manifest with checksums.
      Update build/file/blob backend and path records under controlled cutover,
      preserving IDs, references, and unique conditional-write object names.
      Retain old bytes and metadata for recovery; copying bytes and changing
      configuration alone does not migrate persisted storage references.
      `hot db move-storage` moves builds, or files and internal blobs:
      - each object is copied under the same key below its organization and
        environment, which keeps version suffixes, so conditional-write names
        stay unique;
      - each copy is read back and checked (SHA-256, or the blob's BLAKE3
        hash), then that record alone is rewritten, with the file's backend
        and path in one update. The update retires the source object under
        the source backend, so the target's cleanup never deletes it;
      - a JSON-lines manifest is written, and reruns are resumable.
      On a single machine, a storage change triggers the move during a deploy
      of the same release. The agent also redeploys whenever the
      configuration changes; before this, `settings` and data-mode changes
      waited for the next release.
- [ ] Document restore and supported database/cache upgrades, including any
      maintenance downtime and immutable service choices.
      [Versions and upgrades](../../infra/README.md#versions-and-upgrades)
      covers managed minor and major versions, maintenance restarts and
      failovers, and the choices that replace a resource. Missing: how a
      point-in-time restore, which creates a new RDS instance or Cloud SQL
      clone, takes the stack's place.

Completion: the same application acceptance suite on managed services, including
a demonstrated move from a populated single-machine installation, managed-data
restore, and recovery following VM replacement and data-service failover.
During RDS and Cloud SQL failover, the scheduler must release ownership and
reacquire the lock with no interval in which two schedulers send events.

## Phase 4 Scalable presets

The scalable preset runs each Hot role as its own independently sized and
released service, on managed data, across at least two availability zones.
`hot-cloud` already runs this shape on AWS (ECS services behind an ALB, plus an
EC2 task-worker pool), but its tooling scales each service to 0 or 1
(`scale-ecs-services.sh`). Running several replicas of one role is therefore not
yet proven anywhere; see [Cross-instance readiness](#cross-instance-readiness).

| Role | AWS target | GCP target |
| --- | --- | --- |
| App | ECS Fargate service, Nginx sidecar, at least 2 tasks across 2 AZs | Regional managed instance group, at least 2 instances across 2 zones |
| API | Separate ECS Fargate service, same pattern | Separate regional managed instance group |
| Queue worker | ECS Fargate service, at least 2 tasks | Regional managed instance group, at least 2 instances |
| Scheduler | One ECS Fargate task, `minimumHealthyPercent=0`, `maximumPercent=100` | One-instance managed instance group with stop-before-start replacement |
| Task-worker | EC2 Auto Scaling group of `m8i` hosts with the [Box task host](#box-task-host) setup, scale-in protection while tasks run | Regional managed instance group on a qualified nested-virtualization family |
| Migrations | One-off ECS `RunTask` with the migration role | Ephemeral Compute Engine instance with the migration service account, deleted after reporting |
| PostgreSQL | RDS PostgreSQL, Multi-AZ | Cloud SQL for PostgreSQL, regional high availability |
| Valkey | ElastiCache for Valkey replication group, Multi-AZ automatic failover, TLS | Memorystore for Valkey with replicas, TLS |
| Storage | S3 (or qualified GCS) | GCS (or S3) |

### Scalable reference configurations

| ID | Cloud | Storage | App / API exposure | Keys |
| --- | --- | --- | --- | --- |
| `sc-aws-private` | AWS | S3 | private / private | provider-managed |
| `sc-aws-public` | AWS | S3 | public / public | provider-managed |
| `sc-gcp-private` | GCP | GCS (requires Phase 2) | private / private | provider-managed |
| `sc-gcp-s3-private` | GCP | existing S3 | private / private | provider-managed |

All use managed PostgreSQL and Valkey in the same region. Existing PostgreSQL
or Valkey, customer-managed keys, and mixed exposure follow the same
qualified-variation rules as the single-machine preset. Multi-region
deployment and cross-region disaster recovery are out of scope; cross-region
backup copies are an optional setting.

### Topology and ingress

```text
clients ── HTTPS ──> load balancer (internal, or internet-facing when selected)
                       ├─ host = app endpoint ─> app target group  ─> [nginx sidecar ─> hot app]
                       └─ host = api endpoint ─> api target group  ─> [nginx sidecar ─> hot api]
worker, scheduler, task-worker: no inbound routes
all Hot roles ─> RDS / Cloud SQL, Valkey, object storage, secret store (private endpoints)
user-code egress ─> NAT (stable egress IPs)
```

- One load balancer with host-based routing to separate app and API backends.
  Public exposure uses an internet-facing ALB (AWS) or external Application
  Load Balancer (GCP); private uses the internal variants. Each endpoint's
  exposure remains independent.
- Keep the Nginx configuration from the single-machine bundle as the single
  source for body limits (API `client_max_body_size 300M`), buffering, and
  streaming routes. On AWS it runs as a sidecar container in the same task;
  on GCP it runs in the same instance's Compose bundle.
- Streaming routes use 330 s proxy timeouts today. Set the ALB idle timeout
  (60 s by default) and the GCP backend-service timeout (30 s by default) above
  that, for example 360 s, and qualify SSE reconnects through rolling
  replacement.
- Health checks target the sidecar's `/health`. Use deregistration delay and
  connection draining long enough for in-flight uploads; streaming clients are
  expected to reconnect.
- Load-balancer-to-target traffic is re-encrypted with an installation-local
  certificate on the sidecar. Document that ALB and GCP load balancers do not
  verify backend certificates.
- AWS WAF or Cloud Armor is an optional setting for public endpoints.
- The task-worker, worker, and scheduler have no listeners reachable from
  the load balancer.

### Networking and identities

- **AWS:** private subnets in at least 2 AZs. NAT gateways default to one per
  AZ, with a single-NAT cost option documented as a resilience trade-off. VPC
  endpoints for S3 (gateway), ECR, Secrets Manager, CloudWatch Logs, SSM, and
  STS.
- **GCP:** a regional subnet with Private Google Access, regional Cloud NAT
  with reserved addresses, and a proxy-only subnet for internal load balancing.
- One security group or firewall tag per role. Only the load balancer reaches
  app and API. Only Hot roles reach PostgreSQL and Valkey. Task-worker hosts
  have their own group and receive no inbound traffic.
- One identity per role: an ECS task role plus execution role per service on
  AWS, and one service account per managed instance group on GCP. Each reads
  only its own secrets and the storage prefixes its role needs. The migration
  identity is separate and used only by the migration job.
- Secrets reach ECS services through per-service `secrets` (environment
  variables, the documented exception) and reach Compute Engine instances
  through the agent's tmpfs files.

### Data tier

- **PostgreSQL:** RDS Multi-AZ or Cloud SQL regional HA, private IP only, TLS
  required, point-in-time recovery enabled, deletion protection on. Verify the
  extensions in the [RLS setup](../../resources/db/postgres/RLS_SETUP.md),
  including pgvector, for each engine version.
- **Connection budget:** the sum over roles of replicas × pool size, plus
  migration and admin headroom, must stay below the engine's connection limit.
  Validation rejects replica bounds that exceed it. RDS Proxy is optional for
  app, API, and worker. The scheduler always connects directly, because its
  advisory lock needs a dedicated session.
- **Valkey:** start with cluster mode disabled and one primary plus at least
  one replica, with automatic failover and TLS. Use Hot's cluster mode only
  where the managed offering requires it, and qualify it. Adding shards does
  not spread the write load of a single stream.
- **Queue durability on failover:** replication is asynchronous, and ElastiCache
  offers snapshots but not append-only persistence. A failover can lose
  recently acknowledged queue writes. Qualify Hot's reconciliation from
  PostgreSQL (`reconcile-queued-after-seconds`) as the recovery path, and
  publish the resulting recovery-point behavior.

### Release and deployment

The release workflow supplies the manifest version as an OpenTofu input, and
the reviewed apply rolls each role. Order:

1. Verify the manifest and reject unsupported version jumps.
2. Run the migration job. Only backward-compatible (expand) migrations run
   during a rolling release. Incompatible migrations require a maintenance
   window: drain workers, stop the scheduler, then migrate.
3. Roll task-worker hosts: drain each host's slots, then replace it through
   instance refresh (AWS) or a managed instance group rolling update (GCP).
4. Roll queue workers within their drain deadline.
5. Roll API, then app, with surge capacity and health-gated progression.
6. Replace the scheduler last, stop-before-start.

- **AWS:** each release registers new task definitions using the single image
  digest, with deployment circuit breakers and automatic rollback. OpenTofu
  ignores `desired_count` on autoscaled services. Fargate caps `stopTimeout` at
  120 s, so worker drain must finish or durably requeue within that deadline.
- **GCP:** each release creates a new immutable instance template, and the
  managed instance group performs a rolling update with `maxSurge` and
  `maxUnavailable` set per role. Unlike the single VM, replacement here is the
  intended behavior. Instances drain when the agent receives the shutdown
  signal. Use Compute Engine graceful shutdown where available, and never
  depend on shutdown scripts alone.
- Rollback redeploys the previous manifest when the migration class allows it.
  Otherwise the release stops and reports the required forward fix or restore.

### Autoscaling

| Role | Signal | Bounds and scale-in |
| --- | --- | --- |
| App, API | CPU and request count per target | Minimum 2; scale-in through normal connection draining |
| Queue worker | Hot-published backlog and oldest-message age | Fixed counts first; autoscale after the drain protocol is qualified |
| Task-worker | Hot-published queued tasks and free slots | AWS: per-instance scale-in protection while tasks run, as `hot-cloud` does today. GCP managed instance groups lack per-instance scale-in protection, so scale-in goes only through the agent drain path, with autoscaler scale-in disabled or rate-limited |
| Scheduler | None | Always exactly one |

Hot must publish the queue and slot metrics through the monitoring contract
(CloudWatch or Cloud Monitoring); a Valkey queue does not supply them by
itself. Upper bounds derive from the connection budget, so autoscaling cannot
exhaust PostgreSQL or Valkey.

### Sizes and availability

- Sizes map to per-role CPU and memory, replica bounds, task-host instance
  types, and data-tier classes. `hot-cloud` staging is the starting point to
  benchmark from: API 0.25 vCPU / 512 MB, app 0.5 vCPU / 1 GB, worker
  0.5 vCPU / 1 GB, scheduler 0.25 vCPU / 512 MB, and `m8i.large` task hosts.
  These are not qualified sizes.
- Set Fargate ephemeral storage for build-upload staging explicitly, and give
  Compute Engine instances bounded scratch disks.
- The target availability claim is: the loss of one instance per role, or of
  one zone for stateless roles, does not stop service. Data services recover
  through their managed failover. Scheduled dispatch pauses until the single
  scheduler is replaced; publish that measured gap. Publish only claims
  exercised by the acceptance checks.
- Main cost drivers: NAT per AZ, load balancers, Multi-AZ or HA database
  classes, Valkey replicas, task hosts, VPC endpoints, and logs. Report them
  separately from size labels.

### Cross-instance readiness

These code changes are needed before any role runs more than one replica. None
depends on cloud work, so they can start now; they also benefit `hot-cloud`.

[#144](https://github.com/hot-dev/hot/pull/144) completed the first four items.

- [x] The app's API-key handoff
      ([`keys.rs`](../../crates/hot_app/src/handlers/keys.rs)) kept the pending
      key hash in an in-process map, so the create request could land on an
      instance that never saw it. The new-key page now carries the hash in a
      five-minute HS256 form token, signed with a key derived from
      `HOT_APP_SESSION_SECRET` and bound to the key ID, user, org, and env.
      Any app instance can verify it. The key ID's primary key makes the token
      single-use. This needs no migration and no sticky sessions; every app
      replica already shares the session secret.
- [x] Env, Org, Project, and Build entity caches
      ([`entity_cache.rs`](../../crates/hot/src/db/entity_cache.rs)) are read
      only on SQLite, which is single-host. PostgreSQL reads always reach the
      database and RLS. The env-wide event-handler cache is also SQLite-only and
      rechecks `runtime_revision` every 500 ms. No change was needed.
- [x] The API's in-flight and SSE connection limits
      ([`rate_limit.rs`](../../crates/hot_api/src/rate_limit.rs)) stay per
      process and are now documented as per-replica limits. Request RPS
      already uses a shared PostgreSQL counter. Connection caps protect each
      replica's own sockets and memory. Shared counters would need crash-safe
      leases and a Valkey round trip on every admission. Size the per-org and
      per-credential caps per replica: an account can hold up to N times them.
      The app's auth-surface limits
      ([`hot_app/src/rate_limit.rs`](../../crates/hot_app/src/rate_limit.rs))
      are already documented as per-process backstops.
- [x] Stream pub/sub uses Redis exactly when `queue.type` is `redis`. Memory
      pub/sub only occurs with the memory or SQLite queue, and neither can span
      hosts. With a Redis queue, the app, API, and worker now refuse to start
      if they cannot create Redis pub/sub, instead of warning and serving
      without it. MCP HTTP+SSE message delivery depends on it. Multi-replica
      configuration validation in the deployment tooling must still require
      `queue.type = "redis"`.
- [ ] Verify legacy MCP HTTP+SSE, environment subscriptions, and stream
      subscriptions when the paired requests land on different instances.
      Not started: no test pairs these requests across two instances.
- [ ] Verify sessions, uploaded builds, files, internal blobs, and per-instance
      app caches across instances and through rolling replacement.
      Not started.

### GCP app/API hosting recommendation

Recommend Compute Engine for the initial supported GCP app/API tier. The first
two presets already host these services on a VM; the scalable preset moves them
into separate managed instance groups with independent capacity and releases.
Keep web services separate from worker groups, especially privileged task hosts.
Reuse the image, supervision, identity, logging, and replacement tooling built
for the earlier presets. Configure load balancing, TLS, health checks, rolling
updates, and autoscaling explicitly; VM hosting still requires OS/image upkeep.

Cloud Run remains a useful future option for installations that prioritize
managed HTTP autoscaling, revision traffic splitting/rollback, and reduced
web-host administration. Scale-to-zero can suit intermittent traffic, but the
installation's workers and data services still have their own capacity costs.
Compute Engine groups can also scale independently, so that capability alone
does not require Cloud Run.

Qualify these current Hot behaviors before offering Cloud Run:

- [Dashboard SSE](../../crates/hot_app/src/handlers/envs.rs) and
  [API streams](../../crates/hot_api/src/handlers/streams.rs) need tested request
  timeouts, reconnect behavior, and concurrency budgets. Cloud Run supports
  streaming, but requests have a maximum duration of 60 minutes.
- [File upload parts](../../crates/hot_api/src/handlers/files.rs) allow up to
  256 MiB. Cloud Run's HTTP/1 request limit is 32 MiB; larger uploads require
  a qualified HTTP/2 server path or an explicit upload protocol change.
- [Legacy MCP HTTP+SSE](../../crates/hot_api/src/handlers/mcp.rs) spawns work and
  returns `202` before processing finishes. The paired SSE connection can be on
  another instance. Request-based CPU allocation is unsafe for this pattern;
  qualify instance-based CPU plus termination/recovery behavior, or move the
  work into durable processing before supporting that billing mode.
- Build upload staging and regenerable app caches need temporary-storage and
  memory budgets under concurrent load. Persistent artifacts remain in shared
  object storage. Bound replicas and per-instance pools to protect PostgreSQL
  and Valkey capacity.

Cloud Run app/API qualification is optional future work and does not gate
release of the three presets. The VM ingress path must also pass streaming,
upload-size, timeout, and rolling-replacement checks.

### GCP worker hosting recommendation

Use Compute Engine managed instance groups for the scalable preset's workers.
Keep ordinary queue workers and privileged box task-workers in separate groups
so their machine types, permissions, capacity, and release policies can differ.
Only the box task-worker group requires nested virtualization. The earlier
presets continue to colocate the services on their single application VM.

Cloud Run worker pools fit continuous pull-based queue consumers, but Hot's
current drain sequence can exceed their ten-second termination window.
They also require explicit replica management or an external autoscaler, and
cannot provide the host privileges required by the current Kata box worker.
Compute Engine reuses the host/image tooling already required by the
single-machine preset and gives the release controller a place to drain work
before deliberate VM replacement.

VM hosting adds OS/image patching and host operations. Use pinned images,
supervision, conservative autohealing, and tested replacement procedures.
VM failures can still be abrupt; durable retry/reconciliation remains required.
Do not assume a VM shutdown script or managed instance group automatically
waits for Hot's work to finish.
Measure time to reclaim interrupted work using the effective queue lease
thresholds, and test replay-safe external effects; recovery is not necessarily
immediate or exactly-once.

Start with fixed worker counts, then add queue/capacity metrics and scale-out.
Enable automatic scale-in only after a drain protocol stops new claims and
finishes or durably requeues active work before the chosen instance is removed.
Publish Hot metrics to Cloud Monitoring for scaling; a Redis/Valkey queue does
not automatically supply the required autoscaling signal.

Evaluating Cloud Run worker pools is optional future work and does not gate
release of the three presets.

### Implementation and acceptance

- [ ] Complete [Cross-instance readiness](#cross-instance-readiness).
      Four of its six items are done (#144); the two verification items
      remain.
- [ ] Extract independently scalable service modules using the same runtime
      image, configuration, secrets, and migration contract. Deliver secrets
      through per-service ECS `secrets` on AWS and the agent's tmpfs files on
      Compute Engine.
      Two decisions, made while implementing and open for review:
      - **AWS uses Auto Scaling groups, not ECS.** Both clouds run role
        groups of the same host image and agent, so secrets stay in tmpfs
        files on AWS too, there is one delivery path, and workers keep their
        drain time rather than Fargate's 120-second stop limit.
      - **One passthrough TCP load balancer per endpoint.** TLS ends on the
        hosts' ingress with the operator's certificate, rather than an L7
        load balancer per installation, so no key enters OpenTofu state and
        streams see no proxy timeout. WAF or Cloud Armor would need L7 load
        balancers.

      The renderer and agent run one role per host. Each role follows its
      own release pointer, and the scheduler's host migrates from a migration
      pointer before any role rolls.
- [ ] Implement the topology, ingress, networking, identities, and data tier
      above for each scalable reference configuration, including the
      streaming timeouts and the connection-budget validation.
      `stacks/gcp/scalable` and `stacks/aws/scalable` implement this. The
      AWS stack has the following:
      - private hosts in two zones, with a NAT gateway per zone and an S3
        gateway endpoint;
      - an identity per role;
      - Auto Scaling groups, with stop-before-start instance refresh for the
        scheduler and scale-in protection for task-workers;
      - a Network Load Balancer per endpoint;
      - RDS and ElastiCache, as on a single machine.

      Passthrough load balancers have no proxy timeouts for streams. The
      contract rejects scaling maximums that exceed the connection budget,
      with a 20-connection pool per instance. Qualification on both clouds
      remains.
- [ ] Configure ingress behavior, readiness, graceful shutdown, connection
      budgets, replica bounds, and rolling deployments per service, following
      the release order above.
      Implemented: `/hot-lb-health` checks, 300-second deregistration and
      draining, a 20-connection pool per instance within the contract's
      budget, `scaling` bounds, and the release order in `selfhost.py`.
      Missing: draining before an instance leaves; there are no Auto Scaling
      lifecycle hooks or Compute Engine graceful shutdown, so work stops when
      Docker stops the containers.
- [x] Publish queue backlog, oldest-message age, and task-slot metrics for
      autoscaling.
      `hot queue status --json` reports each queue's waiting and running
      items and its oldest item's wait. It counts from the consumer group:
      acknowledged entries stay in a stream until it is trimmed, so the
      stream's length overcounts. With `task.status-file`, the task worker
      writes its code and container slots in use, and its container memory.
      The scheduler's host publishes the queue backlog, dead letters, and
      jobs and tasks waiting, with the oldest one's wait, every minute.
      Task-worker hosts publish their slots in use. The stacks alert on a
      sustained backlog, long waits, and new dead letters.
- [ ] Qualify Valkey failover with queue reconciliation, and PostgreSQL
      failover with scheduler lock loss.
      The mechanisms exist: the scheduler's lock-liveness check (#159) and the
      worker's reconciliation of orphaned runs. The failover drills remain.
- [ ] Scale queue workers using measured queue demand, age/lag, and capacity.
      Scale task hosts using queued/running demand and available slots; drain
      hosts before scale-in and preserve durable task recovery.
      Implemented:
      - Workers scale on jobs waiting per host (`scaling.worker.waiting_per_host`)
        as well as CPU: target tracking on AWS, a per-group metric on GCP.
      - Task-workers scale out on their average slots in use
        (`slots_target`) as well as CPU.
      - Groups never remove task hosts. A task host idle for
        `retire_idle_minutes` stops its task worker, then leaves its group,
        which shrinks by one.

      Qualification on each cloud remains.
- [ ] Keep one scheduler instance with stop-before-start replacement
      (`minimumHealthyPercent=0`, `maximumPercent=100` on ECS), lock-liveness
      enforcement, health monitoring, and tested restart/recovery behavior.
      Scheduler HA and standby failover remain deferred as described above.
      On AWS the scheduler's group refreshes with `min_healthy_percentage = 0`
      and `max_healthy_percentage = 100`; on GCP it runs in a one-instance
      zonal group that recreates its instance (no surge). Both have the
      heartbeat, lock-liveness checks, and an alert when its status is
      missing. Restart and recovery tests remain.
- [x] Extend the release workflow to scalable presets: apply manifest versions
      through reviewed OpenTofu plans, roll each service with its drain rules,
      and report per-service status.
      The release workflow migrates first, then advances each role's pointer
      in the planned order and waits for every instance in the role's group.
      The first failure stops the release.
      Releases move per-role pointers rather than OpenTofu inputs, so a
      release needs no plan.
- [ ] Implement GCP app/API instance groups and HTTPS ingress, including
      independent capacity, health checks, rolling replacement, and verified
      streaming/upload behavior through the complete load-balancer path.
      `stacks/gcp/scalable` has the following:
      - regional groups with CPU autoscaling from two;
      - autohealing through `/hot-lb-health`;
      - surge rolling updates;
      - one internal or external passthrough load balancer per endpoint,
        with 300-second connection draining.

      Qualification remains.
- [ ] Implement GCP worker instance templates, separate group identities,
      health checks, metrics, and controlled replacement/scale-in. Prove abrupt
      VM loss recovers durable work as well as testing the graceful drain path.
      Each role has its own instance template and service account, scoped to
      its pointer, the shared secrets, Valkey, and the storage prefixes.
      Workers scale on jobs waiting per host, task-workers scale out on slots
      in use, and idle task hosts leave their group. Missing: autohealing
      health checks for worker and task-worker groups, and a drain before
      worker scale-in. The VM-loss drills remain.
- [ ] Prove cross-instance behavior for sessions, uploaded builds, files,
      internal blobs, cache invalidation, and database/queue coordination.
      Not started.
- [ ] Document migration from managed-data compute to distributed compute.
      Local storage must first move to S3 or GCS. Then drain the VM, stop its
      scheduler, start the scalable roles against the same managed data, switch
      endpoints, verify, and retire the VM.
      Not started: infra/README.md does not describe moving a single machine
      to the scalable preset.

Completion: multiple app/API/worker instances can serve one installation;
instance loss and rolling upgrades preserve durable work and tenant boundaries;
autoscaling respects configured limits; data restore and rollback procedures
work. Publish availability claims only for configurations exercised by these
checks.

## Logging, monitoring, and alerting

Make this a supported runtime/operations contract, independent of compute and
application-storage choices. A GCP VM using S3 can use Google Cloud monitoring
or an existing operator destination without coupling telemetry to S3.

### Collection and destination choices

| Profile | Planned implementation |
| --- | --- |
| AWS native default | CloudWatch Logs and metrics/alarms, with a pinned CloudWatch agent on EC2 and the appropriate ECS collection configuration |
| GCP native default | Cloud Logging and Cloud Monitoring using a pinned Ops Agent on Compute Engine |
| Existing destination | Structured log collection and OpenTelemetry/OTLP metrics with explicit endpoint, TLS/authentication, signal support, and operator-owned storage/alerting |
| Local logging | Bounded container/journal logs and private status/health access; use an operator-supplied external monitor for production outage alerts |

- [ ] Keep log and metric destinations configurable independently, with tracing
      optional. Do not require an additional self-hosted monitoring stack on the
      application VM. Mirror/verify agent artifacts and include their resource
      footprint and security updates in the host lifecycle.
      Logs go to the cloud or stay local (`logging.destination`), and nothing
      but the baked agent and Docker's drivers runs on the VM. Missing:
      metrics always go to CloudWatch or Cloud Monitoring, with no
      existing-destination option, and there is no tracing.
- [ ] Use structured JSON stdout logs for app, API, worker, scheduler, and
      task-worker, with timestamp, severity, service, installation/environment,
      release, and instance identity. Include validated request/run/task IDs
      where relevant for correlation. Keep host, ingress, database/cache,
      migration/backup, and application execution output distinguishable.
      - Every service uses `HOT_LOG_FORMAT=json`. Its container labels, or its
        stream name on AWS, carry the installation, role, release, and
        instance.
      - App and API request spans carry a validated `request_id`, which is
        either the caller's `x-request-id` or a new UUID and is returned in
        the response.
      - Workers and task workers log `run_id` and `task_id` fields.
      - Ingress, host journal, and data services log to their own streams.

      Severity and environment fields on every line, and a review of log
      levels, remain.
- [ ] Configure collection/parsing explicitly for the container/journal source;
      avoid duplicate ingestion and preserve collection positions across
      rotation/restart. GCP Ops Agent uses log receivers for logs; its OTLP
      receiver covers metrics/traces and must not be assumed to ingest logs.
      Containers are collected by Docker's `awslogs` and `gcplogs` drivers,
      which read no files, so there are no positions to lose or lines to
      ingest twice. CloudWatch Logs Insights parses the JSON lines;
      `gcplogs` stores each line as a message string. The agent ships the
      host journal (its own lines, plus warnings and errors host-wide) from
      saved journal cursors. A failed shipment retries from the same cursor,
      so nothing is skipped or sent twice.
      Parsing the JSON lines into fields on GCP remains.
- [ ] Set production log levels, local size/retention limits, central retention,
      and optional audit-log retention. Record local-log loss on host failure
      when centralized collection is disabled. Existing log stores and
      notification channels retain external ownership.
      Services log at `info`, Docker keeps 3 files of 10 MB per container,
      CloudWatch keeps `logging.retention_days`, and the guide says local logs
      are lost with the host. Missing: Cloud Logging retention (the bucket
      default applies), audit-log retention, and existing log stores.

### Required signals and alerts

| Layer | Signals to qualify |
| --- | --- |
| Host/containers | CPU, memory/OOM, disk space/inodes/I/O, restarts, service/agent availability, and collection/export failures |
| App/API | Request rate, normalized-route latency/error rates, active streaming connections, and connection-pool pressure |
| Queue workers | Backlog/oldest eligible message age, completion/failure/retry/reclaim rates, in-flight work, and worker capacity |
| Scheduler | Active-owner/process heartbeat, last successful dispatch/maintenance, and overdue-schedule lag, distinguishing idle scheduling from a stopped scheduler |
| Task workers | Ready/occupied slots, task age/failure/drain state, and Kata/KVM/host setup failures |
| Data/storage/recovery | PostgreSQL availability/connections/locks/capacity; Valkey availability/memory/evictions; storage/KMS errors; backup age/failure and restore verification age |

- [ ] Define units, measurement windows, bounded labels, and collection paths
      for application metrics. Do not assume existing diagnostic log statements
      already supply a stable metrics API. Reuse measured queue signals for
      later autoscaling through a documented exporter contract.
      The agent's metrics have CloudWatch units and only installation (and, on
      GCP, instance) labels, and infra/README.md documents them; the queue's
      come from `hot queue status --json` and drive autoscaling. Missing:
      application metrics, such as request rates, latency, and pool pressure.
- [x] Deliver basic host/service health, scheduler heartbeat, backup status,
      bounded logs, and critical rules for the initial single-machine release.
      Critical alerts cover host/service loss, missing scheduler heartbeat,
      disk pressure, backup failure/staleness, and inaccessible dependencies.
      Add detailed application dashboards and tracing in subsequent slices.
      Done:
      - The agent publishes health, scheduler, dependency, readiness, disk,
        and backup-age metrics.
      - CloudWatch alarms and Cloud Monitoring alert policies cover every
        listed critical case and email operator-chosen addresses.
      - Container logs ship non-blocking to CloudWatch Logs or Cloud Logging
        through Docker's drivers, with a bounded local copy.
- [x] Alert on missing signals as well as bad values. Monitor the singleton
      scheduler from outside its process/host; absence of logs is not proof of
      health. This adds detection/recovery guidance without adding scheduler HA.
      The heartbeat and backup-status alerts fire on missing data. The
      scheduler alarm runs in the cloud, and its signal is dispatch-level:
      the scheduler rewrites `scheduler.heartbeat-file` from a job on its
      own dispatch loop. Its container's health check fails when the file
      is 2 minutes old, which the agent reports as not running.
- [ ] Route infrastructure alerts through cloud monitoring or an existing
      external monitor, independently of Hot's worker/email pipeline. Accept
      operator-selected recipients/channels and test the configured path.
      Document thresholds, sustained windows, severity, runbooks, maintenance
      suppression, deduplication, and resolution notifications.
      - Routing: SNS or Cloud Monitoring email channels
        (`monitoring.alert_emails`). Thresholds, windows, and first steps are
        in infra/README.md.
      - Testing the path: the `test-alert` agent command publishes a test
        metric. An `alert-test` alert fires on it and then resolves.
      - Silencing: `selfhost.py maintenance start/end` disables the alarm
        actions on AWS and snoozes the alert policies on GCP.

      Only email channels are created (AWS outputs `alert_topic_arn` for other
      subscribers), and severity and deduplication are not documented.
      Delivery on each cloud still needs qualification.
- [ ] Use private synthetic checks from an authorized network for sign-in/API
      and a representative queued execution. Keep liveness, dependency readiness,
      and end-to-end checks separate; do not expose monitoring endpoints publicly.
      - Readiness (`/signin` and `/status` through each host's ingress) and
        dependency reachability are separate metrics.
      - With `monitoring.synthetic_event`, the hosts serving the API publish
        that event every 5 minutes, through their own ingress, with an API
        key the operator holds, then wait for its run (`SyntheticOk`,
        `synthetic-failed`).

      Qualification remains.

### Privacy, reliability, and qualification

- [ ] Redact at source and at collection boundaries. Exclude credentials,
      authorization/cookie headers, signed URLs, raw query strings, payloads,
      and environment secrets from infrastructure telemetry. Explicitly define
      access/retention for user execution output and audit events; operational
      log access must not become an accidental cross-organization data channel.
      - HTTP request spans record the method and path only, never the query
        string or headers, and webhook paths hide their capability token.
      - Secrets reach services as files, not environment values.
      - The agent redacts diagnostics and logs before reporting them.

      User payloads that Hot code logs are not detected. Neither access nor
      retention is defined yet for execution output in central logs.
- [ ] Apply the network/encryption contracts to logs, metrics, alerts, and
      optional traces: least-privilege export identities, protected local
      listeners, verified TLS, retention/deletion policies, and supported KMS
      configuration. No telemetry is sent to Hot Cloud by default.
      Export uses the host's identity, scoped to the installation's log group
      and metric namespace on AWS and project roles on GCP, over HTTPS to
      cloud APIs; nothing goes to Hot Cloud. Missing: KMS keys for logs, and
      Cloud Logging retention.
- [ ] Bound buffering, retry, disk use, event sizes, rate, and metric cardinality.
      Keep user/request/run IDs out of metric labels. Collector outages or log
      floods must not block execution or fill the database disk; expose dropped
      telemetry and delivery failures. Retain usable local diagnostics.
      Docker ships without blocking through a 4 MB buffer and keeps 3 files of
      10 MB, the journal ships 500 entries per source per cycle, and metric
      labels carry no user, request, or run IDs. Missing: dropped lines and
      failed shipments are only logged on the host, not exposed as metrics.
- [ ] Offer optional sampled OpenTelemetry tracing with correlation across
      HTTP, queued work, and task execution. Verify each destination/agent's
      supported signals; full tracing is not an initial release gate.
      Not started: Hot has no OpenTelemetry exporter.
- [ ] Qualify process/host loss, scheduler inactivity, disk pressure, stale
      backups, notification delivery, redaction, rotation, collector restart,
      and destination outage. Verify data arrives with correct timestamps and
      labels; publish basic queries/status views, alert rules, collection costs,
      and supported retention defaults for the chosen profile.
      infra/README.md publishes the alert rules, dashboards, retention
      defaults, and cost drivers; the drills remain.

## Operations and release acceptance

### 1. Installation preflight

- [ ] Implement a reusable preflight workflow that runs before provisioning and
      can recheck an existing installation. Validate cloud identity/permissions,
      enabled APIs, quotas, service/region/machine support, organization policy,
      and nested virtualization when box execution is selected.
      [`preflight.py`](../../infra/preflight/preflight.py) is a read-only
      script, and the plan workflow runs it first. It checks identity, zone,
      machine type, vCPU quota, host image, and managed-data offerings, plus
      GCP APIs and the nested-virtualization policy. Whether nested
      virtualization actually works is shown by the host's first box task.
      Permissions are not checked beyond the caller's identity.
- [ ] Check CIDR overlap, required routes and egress, private client access,
      DNS/TLS, secrets/keys, registry access, and existing database/cache versions,
      extensions, roles, TLS, and required commands from the appropriate network.
      Preflight checks:
      - CIDR overlap with the host's fixed networks;
      - existing subnets and private services access;
      - DNS (a warning only);
      - the public certificate;
      - existing buckets;
      - default routes through a NAT on existing networks (Cloud NAT and
        Private Google Access on GCP);
      - an image mirror's repositories.

      Pulls from public registries, and existing databases, are checked from
      the host, by the first deploy and by the agent.
- [x] Separate static/cloud checks from host-only qualification; clearly report
      checks that require a private runner or provisioned host. Recheck critical
      conditions after provisioning; availability observations are not capacity
      reservations. Fail safely and make interrupted-install retry idempotent.
      Preflight runs before every plan and lists host-only checks (`HOST`)
      with what verifies them; the agent rechecks the database and readiness
      on each deploy and resumes an interrupted one.
- [x] Produce actionable, redacted results identifying missing prerequisites
      and which resources, if any, were already created. Initial release gate.
      Results name the missing prerequisite and the next step, mask the
      account ID, and go to the job summary. Which resources an interrupted
      apply already created comes from the plan.

### 2. Supported versions and upgrade contract

- [x] Publish a compatibility matrix for Hot service images, configuration and
      DB schema, PostgreSQL/extensions, Valkey, host OS, Kata/containerd, and
      OpenTofu/providers. Pin dependencies and commit root-stack lockfiles.
      [Versions and upgrades](../../infra/README.md#versions-and-upgrades)
      publishes the matrix; a test fails when it disagrees with the pins.
- [ ] Define supported source/target releases, ordering, maintenance downtime,
      admission pause/drain behavior, migrations, and post-upgrade checks.
      Qualify mixed service versions wherever rolling deployment permits them;
      otherwise require coordinated replacement. Preserve singleton scheduling.
      Each manifest names its oldest source release, and the agent refuses
      larger jumps. The single machine replaces services in one ordered deploy
      with the scheduler stopped throughout; the order and downtime are
      documented.
      A scalable release runs mixed versions while roles roll; that is not
      qualified.
- [x] Record image digests, host image, configuration version, schema version,
      previous release, and completed migration steps. Classify migrations by
      backward compatibility and identify when application rollback is safe,
      when a forward fix is required, and when recovery needs a data restore.
      The agent keeps the last 20 deploys (digests, host versions,
      configuration schema, migration levels, steps, restore point, error).
      Hot never migrates down, so every migration counts as one-way: rollback
      is safe only at the same migration level, and otherwise needs a forward
      fix or a restore. Before a release adds migrations the agent takes a
      `pg_dump` of a local database or records the point-in-time restore time.
- [ ] Test representative upgrade fixtures and interrupted-upgrade recovery for
      the initial release; test supported prior releases as they become
      available. Reject unsupported jumps and engine/provider combinations.
      The host deploy agent and release workflow enforce these rules; they
      are not just documentation.
      Agent tests cover refused jumps and downgrades, resumed interrupted
      deploys, failed releases that wait for `deploy-now` instead of retrying
      every poll, a retry that keeps the first restore point, and a failure
      before migrating that restarts the previous scheduler. Real upgrade
      fixtures need a second release.

### 3. Measurable backup and recovery

- [ ] Publish numerical recovery point objectives (maximum acceptable data
      loss) and recovery time objectives per qualified preset/configuration.
      Define backup schedules, retention, encryption, verification, and alerts;
      record measured results and prerequisites rather than untested promises.
      [Recovery](../../infra/README.md#recovery) states single-machine
      recovery points and target times per failure, marked as unmeasured.
- [ ] Define a recoverable point across PostgreSQL, local/object bytes, internal
      blobs, queued work, configuration, application secrets, and cloud keys.
      Document pause/snapshot ordering, pending-work reconciliation, duplicate
      event effects, and scheduler catch-up after restore.
      [Recovery](../../infra/README.md#recovery) covers the data disk's
      crash-consistent snapshot (PostgreSQL, the Valkey AOF, local builds and
      files), secrets in the secret store, and schedule catch-up. Missing:
      consistency between the database and S3 or GCS objects, and duplicate
      event effects.
- [ ] Restore to a fresh host without depending on the failed VM, its boot disk,
      or its cached credentials/configuration. Include state-backend recovery,
      retained resource inventory, private access, and key/secret retrieval.
      Run application flows and measure elapsed recovery and recovered data age.
      The boot disk holds nothing recovery needs. `backup.restore_snapshot`
      creates the data disk from a snapshot, and instance replacement
      reattaches the retained disk. `roll-back` restores a local database
      from the pre-upgrade dump. Docker now waits for the data disk, so a
      reboot can never start the data services on the boot disk. Drills on
      real accounts remain.
- [ ] Require the documented targets and restore drill for the initial
      single-machine release on both clouds. Extend drills to managed data and
      distributed compute as those presets ship; track regional disaster
      recovery separately from an ordinary VM-loss recovery claim.
      The targets are in [Recovery](../../infra/README.md#recovery), marked
      unmeasured; the drills on both clouds remain.

### 4. Workload trust and IAM

- [x] State the supported trust model for administrators, code authors,
      organizations, and executions. Default account creation to invitation-only
      after explicit first-admin bootstrap; public signup is an operator choice.
      [Trust and identities](../../infra/README.md#trust-and-identities)
      states the model. The bundle sets `HOT_APP_SIGNUP=invite`, which
      operators may override.
- [ ] Separate bootstrap/provisioning, deployment, migration, runtime, backup,
      and restore permissions. Publish a least-privilege matrix and document
      any single-host identity sharing; migration authority is not a runtime
      capability. Use short-lived identities where supported. Include the
      GitHub Actions plan, apply, and release roles, their OIDC trust
      conditions, and the deploy agent's pointer/status identity.
      The guide's identity matrix covers plan, apply, release, host, and
      database logins. It also documents that Hot services on one VM share
      the host identity, while user code has neither identity nor secrets.
      People can assume the same short-lived deploy identities as CI:
      service account impersonation on GCP, or role assumption on AWS. The
      bootstrap's `operators` and `names` inputs create and name them, for
      example as an `infra-provisioner`.
      No least-privilege apply policy is published: `apply_policy_arns` and
      `apply_roles` have no default.
- [ ] Test both normal Hot execution and box execution against cloud metadata,
      host sockets/files, credentials, protected data services, and other
      organizations. Distinguish intentional application integrations from
      access to installation infrastructure; qualify egress controls and DNS/
      redirect handling rather than relying on box network policy alone. Use
      the test list in
      [Outbound access for user code](#outbound-access-for-user-code), and
      verify that user code cannot read secret files or service environments.
      Isolation is mandatory, `HOT_NETWORK_DENY_CIDRS` denies user code the
      installation's networks, and box guests sit behind the host baseline.
      The tests on a running installation remain.
- [ ] Test private/public ingress under the declared trust model. Record
      isolation limitations and required controls; this is an initial release
      gate and does not imply that every topology provides identical isolation.
      [Trust and identities](../../infra/README.md#trust-and-identities)
      records the shared host identity on one machine. Testing each exposure
      remains.

### 5. Operator workflows and diagnostics

- [ ] Define supported workflows for status, diagnostics, upgrade, backup,
      restore, resizing, secret/certificate rotation, and controlled shutdown.
      Keep plain OpenTofu usable and specify helper inputs, outputs, and failures.
      Each release and infrastructure workflow is available both as a helper
      command and through the GitHub Actions workflows, with the same behavior.
      [`selfhost.py`](../../infra/operator/selfhost.py) sends an agent command
      and waits for its report by request ID; the release and command
      workflows run it, so they match manual use. infra/README.md documents
      upgrades, restores, resizing (`size`, then apply), and certificate
      rotation (`restart-service ingress`). Missing: a procedure for rotating
      the generated secrets.
- [ ] Require basic status, versions, redacted effective configuration, service
      logs, dependency checks, and a redacted diagnostic bundle for the initial
      release. Deliver them through the agent command channel, so they need
      neither SSH nor network access to the VM, and avoid exposing a public
      management endpoint. Test redaction of secrets,
      tokens, user payloads, and sensitive configuration.
      `diagnostics` reports versions, effective configuration, services,
      dependencies, and health; `logs` reports a service's newest lines. Both
      are redacted on the host: known secret values, credential shapes, email
      local parts, account IDs, and secret references. Free-form user payloads
      in logs are not detected.
- [ ] Add broader operational signals, dashboards, and actionable alerts for
      scheduler inactivity, queue age, retries/failures, task capacity, disk/
      memory pressure, connection pressure, certificate expiry, and backup
      failures. Document thresholds, ownership, response steps, and export options.
      The ingress certificate's remaining days are a metric with an alert at
      14 days. Queue waits, task slots, and host memory are metrics with
      alerts. Managed PostgreSQL alerts at 80% of the size's planned
      connections. Each stack has a dashboard of the agent's metrics.
      Apply the logging/monitoring contract above: its baseline is required for
      initial installs; detailed dashboards and tracing can follow.

### 6. Security maintenance and release process

- [x] Define supported-release lifetimes, security reporting/advisory channels,
      patch priorities, and operator upgrade instructions. Track application,
      OS/base image, TLS libraries, Kata, containerd, and bundled dependencies.
      [Security updates](../../infra/README.md#security-updates) covers the
      latest-release support policy, advisories, and what each layer pins
      and how it is updated.
- [ ] Require a repeatable security-update process for the initial release:
      rebuild pinned artifacts, verify their integrity, publish immutable
      versions, back up, drain/replace, apply required migrations, and run smoke
      checks. Exercise the process on both clouds through the release workflow
      and document downtime plus the compatible rollback or restore path.
      Preserve scheduler exclusivity. Sign release manifests from the initial
      release, because the deploy agent verifies them; broader provenance follows.
      [Security updates](../../infra/README.md#security-updates) documents the
      process, and manifests are signed from the first release. Exercising it
      on each cloud remains.
- [ ] Add artifact signing and verification, build provenance, dependency/SBOM
      inventories, vulnerability scanning/triage, and signing-key rotation.
      Allow customer mirrors while preserving artifact identity and verification.
      Keep remediation decisions and supported versions tied to release records.
      The runtime image is signed and carries build provenance and an SBOM.
      Each release attaches a Trivy vulnerability report, which never blocks
      it. `image_registry` points the hosts at a customer mirror. Images keep
      their manifest digests, and hosts sign in to ECR or Artifact Registry
      with their own identity.

### 7. State recovery and infrastructure drift

- [x] Use protected remote state, deployment locks, and a separate state
      bootstrap lifecycle. Document lost-state recovery, interrupted applies,
      lock-owner checks before stale-lock recovery, and retained resource IDs.
      [State, drift, and teardown](../../infra/README.md#state-drift-and-teardown)
      covers stale locks, restoring a state version, adoption through
      `imports.tf`, and interrupted applies.
- [x] Define drift detection and review for manual cloud edits, module/provider
      upgrades, and controllers such as autoscalers. Preserve field ownership;
      do not overwrite autoscaled counts or adopt external resources silently.
      The plan workflow's `drift_check` fails a scheduled run on any change,
      and the example operator repository runs it daily.
- [ ] Document import/adoption and managed-to-existing handoffs with reviewed
      plans. Use `removed` blocks with `destroy = false` so a mode change stops
      managing a resource without destroying it. Never rely on toggling `count`.
      The managed-data resources are count-based, and a `removed` block cannot
      apply to a resource whose block remains in the configuration. Handoffs
      therefore use documented `tofu state rm` lists, and a test keeps those
      addresses real. Deletion protection and the workflow's durable-resource
      guard stop an unplanned destroy.
- [x] Protect durable disks, databases, buckets, secrets, and keys from
      replacement/deletion; normal teardown retains data. `prevent_destroy`
      cannot be set from a variable, so use service-level protection (RDS and
      Cloud SQL `deletion_protection`, EBS and persistent-disk retention,
      `force_destroy = false` on buckets, KMS key destroy protection) driven by
      an explicit purge input. Alternatively, place durable resources in a
      separate data root. Specify the purge procedure and the ongoing costs of
      retained resources.
      Every plan fails if it deletes or replaces a durable resource unless
      the caller sets `allow_data_deletion`. Retirement removes kept resources
      from state before `tofu destroy`; purging is a separate manual step,
      and the guide lists what retained resources cost.
- [ ] Exercise safe recovery and ownership handoff cases in infrastructure
      qualification; add automated drift reporting as the operator tooling grows.
      Drift is reported automatically (`drift_check`, daily in the example
      operator repository); the recovery and handoff exercises remain.

### 8. Capacity, retention, and cost controls

- [ ] Benchmark tested size combinations and publish workload assumptions,
      concurrent execution limits, queue throughput/age, DB connection budgets,
      scratch/durable disk needs, and scaling bounds. Size labels are not HA,
      cost, or throughput guarantees.
      [Capacity, retention, and cost](../../infra/README.md#capacity-retention-and-cost)
      publishes the configured limits per size, marked as not yet
      benchmarked, and the behavior at capacity.
- [x] Define behavior at capacity: backpressure, admission rejection, retry,
      or bounded scale-out. Keep recovery/migration/admin headroom and prevent
      autoscaling from overwhelming data services or exceeding declared limits.
      infra/README.md defines it: tasks wait in the queue, Valkey refuses new
      work when full (`noeviction`), box tasks stay within their pool, and the
      contract keeps scaling maximums within the database's connection budget,
      leaving 40 connections for administration.
- [ ] Set and qualify retention/cleanup for logs, execution history, builds,
      files/blobs, multipart staging, snapshots, and backups. Reconcile metadata
      and object references before deletion; distinguish user data from
      disposable artifacts. Test disk-full and cleanup-failure behavior.
      The guide tabulates retention for snapshots, logs, dumps, deploy
      history, managed backups, and execution history. Organizations without
      a plan (self-hosted) keep calls for `db.retention.call.days`
      (`HOT_DB_RETENTION_CALL_DAYS`, default forever); the worker's
      maintenance deletes older calls and deactivates their blob references
      for blob collection. Disk-full and cleanup-failure tests remain.
- [ ] Publish estimates and cost drivers for compute, managed-service minimums,
      replicas, storage, requests, NAT/endpoints, load balancers, logging, backups,
      and KMS. Offer spend alerts and usage reporting with documented limits;
      do not present alerts as guaranteed spend caps.
      The guide lists the cost drivers. `monitoring.monthly_budget` creates
      an AWS or GCP budget that emails the alert addresses; estimates and
      usage reporting remain.

## Validation matrix and first implementation slices

| Layer | Required checks |
| --- | --- |
| Static infrastructure | `tofu fmt`, init/validate against pinned providers, configuration validation, and reviewed plans for each supported preset |
| Public packaging | Clean public-only build of the single amd64 runtime image, runtime assets/features present, every service and job runs from one digest, no private dependency or hosted credentials |
| Secrets delivery | `_FILE` loading and fail-closed errors, conflict rejection, provenance-based redaction, no secrets in process environments or `docker inspect` on VMs, per-service file scoping, first-boot create-if-absent, rotation through `restart-service`, and no direct secret environment reads remaining |
| User-code egress | Mandatory isolation, allowlist plus fixed deny set for normal and box execution, metadata/data-service/host blocking, DNS rebinding and redirect cases, optional proxy behavior, stable egress IP output, and SMTP port checks |
| Box task host | Ported isolation chain and validation on both clouds, baked host image with no boot-time downloads, bounded devmapper storage, and a real Kata task on each reference configuration |
| Local integration | All services with PostgreSQL/Valkey, migrations/grants, onboarding/email, HTTP/event/schedule/box/file flows |
| Cloud single machine | Each reference configuration: fresh install, repeat apply, host lifecycle, isolation, upgrade, and full restore |
| Scheduler exclusivity | Lock check on the holding connection, exit on lock loss, connection kill, database failover, network partition, deployment overlap, and pooler rejection |
| Release delivery | GitHub Actions plan/apply/release with OIDC only, environment approvals, concurrency serialization, signed-manifest verification, agent release and rollback without instance replacement, private-network smoke checks, and parity with manual commands |
| No SSH | No reachable SSH port from any source, no instance keys, CI roles without shell or SSH permissions, and every documented workflow completed through port forwarding and the agent command channel only; the agent rejects unknown or malformed commands |
| Network exposure | Private default and the qualified app/API exposure combinations; private bootstrap, DNS/TLS, certificate reload and expiry alerts, egress, public-route isolation, and removal of public access |
| Encryption | Effective at-rest settings, remote TLS enforcement and verification, provider-managed keys (customer-managed keys as qualified), key-access recovery, OpenTofu-encrypted state/plans, encrypted backups, and restore with retained keys |
| Initial release gates | Preflight failures and retry, upgrade compatibility/recovery, release delivery, measured restore targets, trust/IAM checks, basic private diagnostics/redaction, and a rehearsed security update on each cloud |
| Logging/monitoring | Correct collection/retention/redaction, basic host/service and scheduler health, off-host alerts and tested delivery, bounded buffering, and collector/destination failure recovery |
| Operational lifecycle | Drift/state recovery and ownership handoffs, capacity/backpressure and retention, expanded diagnostics, release security, and cost visibility |
| Storage | Local/S3/GCS contract checks and real-cloud identity/behavior checks |
| Storage independence | AWS/GCP with Local/S3 and qualified GCS; mixed build/file backends, existing-bucket non-ownership, private-network access, independent state backend, and cross-cloud credentials |
| Managed data | PostgreSQL/RLS/extensions, queue commands/failover, private networking, migration, and restore |
| Scalable | Each scalable reference configuration: cross-instance correctness (handoff store, entity-cache invalidation, shared limits, Valkey pub/sub), streaming through load-balancer timeouts, zone and instance loss, worker interruption and recovery, Valkey and PostgreSQL failover, singleton handoff, connection-budget enforcement, autoscaling bounds, and ordered rolling releases with rollback |

Cloud qualification runs use isolated accounts/projects or installation
namespaces, explicit spending limits, and cleanup that distinguishes disposable
test resources from retained customer data.

Recommended implementation slices, each within the phase shown:

| Slice | Phase | Work |
| --- | --- | --- |
| 1 | 0 | Public production configuration, email/onboarding, scheduler lock-liveness, `_FILE` secrets and secret routing, user-code egress policy, and the single runtime image with signed release manifests |
| 2 | 0 | Shared single-machine runtime, box task host port, storage layout, certificate handling, and end-to-end smoke checks |
| 3 | 1 | OpenTofu state/bootstrap contracts (with state encryption) and the AWS single-machine stack |
| 4 | 1 | Host deploy agent, release pointer, and GitHub Actions plan/apply/release workflows on AWS |
| 5 | 1 | GCP single-machine stack and release delivery using the same runtime, agent, and workflows, qualifying Local and existing S3 without the GCS backend |
| 6 | 2 | GCS storage and real-bucket parity checks; can start after slice 1, in parallel with slices 2–5 |
| 7 | 1 | Initial single-machine release gates for every reference configuration: preflight, upgrades, release delivery, measured recovery, trust/IAM, basic diagnostics, logging/monitoring, repeatable security updates, and the operator guide. Broader tooling for all eight operational areas continues in later slices |
| 8 | 3 | Managed-data stacks and data migration qualification |
| 9 | 4 | [Cross-instance readiness](#cross-instance-readiness) code changes. Independent of cloud work; can start now, in parallel with any slice |
| 10 | 4 | Scalable AWS and GCP stacks, using Compute Engine app/API and worker groups on GCP. Any future Cloud Run option requires separate runtime qualification |

Before implementation, resolve exact image distribution, first supported OS and
machine families (`m8i` on AWS; the GCP family), the ingress proxy, SMTP
dependency, bootstrap UX, the manifest signing scheme, and benchmarked size
defaults within these slices. They do not change the selected presets,
OpenTofu ownership, the certificate mechanism, or the exclusion of
custom-domain automation.

## Provider references

- [OpenTofu providers](https://opentofu.org/docs/language/providers/) and state
  backends for [S3](https://opentofu.org/docs/language/settings/backends/s3/) and
  [GCS](https://opentofu.org/docs/language/settings/backends/gcs/), and
  [state and plan encryption](https://opentofu.org/docs/language/state/encryption/).
- [GitHub Actions OpenID Connect](https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-deployments/about-security-hardening-with-openid-connect),
  [`aws-actions/configure-aws-credentials`](https://github.com/aws-actions/configure-aws-credentials),
  and [`google-github-actions/auth`](https://github.com/google-github-actions/auth).
- [ECS secrets in environment variables](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/secrets-envvar-secrets-manager.html),
  [Compose secrets](https://docs.docker.com/compose/how-tos/use-secrets/),
  and [GCP Secret Manager](https://docs.cloud.google.com/secret-manager/docs/overview).
- [PostgreSQL advisory locks](https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS).
- Nested virtualization on [EC2](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html)
  and [Compute Engine](https://docs.cloud.google.com/compute/docs/instances/nested-virtualization/overview).
- [Memorystore for Valkey](https://docs.cloud.google.com/memorystore/docs/valkey/product-overview).
- [CloudWatch agent and OpenTelemetry](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/CloudWatch-OTLPCloudWatchAgent.html),
  [GCP Ops Agent](https://docs.cloud.google.com/logging/docs/agent/ops-agent),
  and [GCP OTLP signal support](https://docs.cloud.google.com/monitoring/agent/ops-agent/otlp).
- [OpenTelemetry sensitive-data handling](https://opentelemetry.io/docs/security/handling-sensitive-data/).
- Encryption for [RDS](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/Overview.Encryption.html),
  [ElastiCache at rest](https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/at-rest-encryption.html),
  and [ElastiCache TLS](https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/in-transit-encryption.html).
- Memorystore for Valkey [CMEK](https://docs.cloud.google.com/memorystore/docs/valkey/about-cmek)
  and [TLS](https://docs.cloud.google.com/memorystore/docs/valkey/about-in-transit-encryption),
  plus [GCS CMEK](https://docs.cloud.google.com/storage/docs/encryption/customer-managed-keys).
- [AWS internal/public load balancing](https://docs.aws.amazon.com/elasticloadbalancing/latest/userguide/how-elastic-load-balancing-works.html)
  and [Session Manager private endpoints](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-getting-started-privatelink.html).
- [GCP internal Application Load Balancer](https://docs.cloud.google.com/load-balancing/docs/l7-internal),
  [IAP TCP forwarding](https://docs.cloud.google.com/iap/docs/using-tcp-forwarding),
  [Pub/Sub pull subscriptions](https://docs.cloud.google.com/pubsub/docs/pull),
  and [Cloud NAT](https://docs.cloud.google.com/nat/docs/overview).
- [Cloud Run worker pools](https://docs.cloud.google.com/run/docs/overview/what-is-cloud-run)
  and [container lifecycle contract](https://docs.cloud.google.com/run/docs/container-contract).
- Cloud Run [HTTP limits](https://docs.cloud.google.com/run/quotas) and
  [CPU allocation and billing](https://docs.cloud.google.com/run/docs/configuring/billing-settings).
- [Compute Engine managed instance groups](https://docs.cloud.google.com/compute/docs/instance-groups)
  and [Cloud Monitoring autoscaling metrics](https://docs.cloud.google.com/compute/docs/autoscaler/scaling-cloud-monitoring-metrics).
- [GCS Rust SDK](https://docs.cloud.google.com/rust/docs/quickstart-storage),
  [Application Default Credentials](https://docs.cloud.google.com/docs/authentication/application-default-credentials),
  [object composition](https://docs.cloud.google.com/storage/docs/composing-objects),
  and [XML API multipart uploads](https://docs.cloud.google.com/storage/docs/multipart-uploads).
