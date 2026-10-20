# Sub organizations and PostgreSQL RLS implementation plan

Status: implemented and locally validated on branch `codex/sub-orgs-rls`;
deployment-specific Phase 6 gates remain below.
Product direction selected on 2026-10-19. Shared deployments remain deferred.
Source baseline: `4466aee` (Hot 2.6.10).

## Implementation record

The local branch implements the public repository portions of Phases 0–5:
typed scopes and restricted runtime connections, forced PostgreSQL policies,
direct-child organizations with independent environments/builds, inherited
root administration and explicit child grants, account billing/quota resolution,
management credentials and REST routes, runtime ingress and worker ownership,
and the hierarchical UI. Migrations 010–018 preserve existing org identities and
add ownership integrity, credential lineage, quota reservations, durable run
admissions, trace byte reconciliation, and retained runtime relationship checks.
SQLite shares application authorization
and quota semantics; PostgreSQL additionally enforces row policies.

The implementation includes exact CLI profile selection, independent migration
credentials, fail-closed live-source/cache access, and scoped integration test
execution. Administrative API access must honor effective credential permissions
as well as credential type and environment scope. An account-wide read view
does not authorize cross-child mutations.

Deployment instructions and qualification boundaries are maintained in
[RLS setup](../../resources/db/postgres/RLS_SETUP.md), with the table inventory in
[RLS coverage](../../resources/db/postgres/RLS_COVERAGE.md) and container recovery
in [cancellation quota recovery](../../resources/db/postgres/CANCELLATION_QUOTA_RECOVERY.md).
The public API contract is documented in
[sub-organization management](../../resources/docs/api/management.md).

Phase 6 is a release gate as well as a test implementation phase. Local tests
cannot complete private cloud onboarding/plan seeding, production role and proxy
qualification, provider billing reconciliation, remote S3/Kata deployment
qualification, or backup/restore rehearsal. Enforced PostgreSQL currently requires
schema `hot`. Hosted `sub_orgs` entitlement defaults to zero until the account's
plan/override enables it; unconfigured local accounts retain local defaults.
External SDK management helpers belong in their SDK repositories. Shared
deployments and the other deferred product boundaries below remain deferred.

### Initial implementation qualification — 2026-10-19

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` and workspace Clippy with `-D warnings` | Passed |
| `cargo test --workspace --all-targets --locked --offline --no-fail-fast` | 2,522 passed, zero failed, nine ignored across 32 targets |
| Service-backed fixtures | Dedicated PostgreSQL with pgvector and Valkey; restricted-login RLS, ownership, sibling quota races, migration 015 repair, and real app-router checks passed |
| Docker cancellation regression, explicitly enabled | Passed; reservation held while running, released only after verified exit |
| `scripts/hot-static-checks.sh check` | Passed project/package checks, formatting, documentation snippets, and bundled skill checks |
| `hot init` and `hot test -c hot.test.hot --ctx hot/ci.ctx.hot` using isolated SQLite storage | Passed; 909 tests and 2,460 assertions |
| `node --test scripts/test-app-scope.mjs` | Three browser scope regressions passed |
| Fresh PostgreSQL CLI installation | Owner migrations, restricted-runtime grants, init, profile selection, and managed eval passed; eval persisted a succeeded terminal record with tracing disabled |
| Standalone CLI / invalid explicit profile | Ad-hoc eval succeeded; invalid explicit scope failed without unmanaged fallback |
| Repository checks | AGENTS source synchronization, workflow YAML parsing, and diff whitespace checks passed |

The local offline Hot build used an existing ONNX Runtime library through
`ORT_LIB_PATH`; this is a machine setup detail, not a repository dependency
change. Default-ignored tests are not counted as passes; the explicit Docker
regression is additional to the workspace total. These results do not replace
the deployment-specific gates above or the Phase 6 rollout rehearsal below.

### Isolation review follow-up — 2026-10-19

A second review covered the branch against `4466aee`, including PostgreSQL
policies/grants, API and browser authorization, credential lineage, custom
routing, queued execution, local source/cache access, and bundle resources.
The review produced the following repairs and regression coverage:

| Boundary | Repair |
| --- | --- |
| Account management credentials | Root environment connections cannot read or mutate management keys; the root ownership field is immutable. |
| Delegated key lineage | Deleting an API key cannot cascade away its management issuer and remove issuer ceilings/revocation. Revoke with `active=false`. |
| Runtime PostgreSQL grants | Startup rejects REFERENCES (including column grants), TRIGGER, and server-file/server-program role memberships in addition to earlier owner/BYPASSRLS/DDL checks. |
| Alert delivery | Alert, subscription, and destination must share an org; environment-specific subscriptions must match the alert. Ownership is immutable and the sender checks the environment again. |
| Custom domains and MCP | Standard MCP routes honor the resolved domain's exact environment. Positive domain cache entries recheck current verified ownership on every request, including remote deletion/reassignment. |
| File permissions | Authorization uses the normalized storage path, so a prefix grant cannot escape through parent-path components. |
| Deployment mutations | Build/project ownership is checked before reactivating a project. |
| Browser object reads | Call hierarchy/detail/search use the selected environment on SQLite as well as PostgreSQL. |
| Pending API key creation | Temporary key material binds to the user, org, and environment that generated it; another scope cannot consume it. |
| Teams | Adding/reactivating users requires effective active access to the team's org, including current child grants. |
| Stale tabs | Filters, dynamic links, row navigation, copied URLs, and reloads retain the rendered scope. |
| Retry origin | Payload retry metadata cannot override the validated queue context with a run owned by another environment. |
| Worker host source/context | Both worker types authorize live builds against the selected profile before source/cache use. Missing deployed bundles stop execution. Only authorized live builds receive local development context. |
| Module initialization | Cache compilation produces artifacts without executing module code. Initializers execute only when the program is invoked with its authorized runtime scope and services. |
| Bytecode memory cache | Entries and compilation locks include the cache directory, so a caller-controlled manifest key in one bundle cannot supply bytecode to another bundle's cache. |
| Bundle resources/dependencies | Bundles use their own dependencies and resource registries, including warm cache hits and parallel/pmap branches. Manifest paths and cached resource reads reject traversal and symlink escapes. |
| Tool and skill metadata | Runtime metadata registries are scoped to the executing program, including concurrent same-name functions and parallel/pmap branches; another build's metadata cannot supply a missing entry. |

Migration 016 checks existing alert delivery relationships before installing
new integrity triggers. An inconsistent relationship aborts migration for
operator investigation; migration does not rewrite customer ownership. Runtime
roles provisioned with broader grants must be narrowed before the new startup
qualification succeeds. Positive domain routing now costs a verified-domain
lookup per request to avoid stale cross-instance routing authority.

Final review validation:

| Check | Result |
| --- | --- |
| Rust workspace, all targets, locked/offline | 2,547 passed, zero failed, nine ignored across 35 targets. Dedicated PostgreSQL 18/pgvector and Valkey fixtures were enabled; the final run started from a fresh disposable database. |
| Restricted PostgreSQL login | RLS visibility/writes, management credential separation, delegated issuer retention, alert delivery ownership, and unsafe-role/grant rejection passed. |
| Rust formatting and workspace Clippy with `-D warnings` | Passed. |
| Hot static checks | Project/package checks, source formatting, documentation snippets, and bundled skill checks passed. |
| Isolated SQLite `hot init` and main Hot suite | 909 tests and 2,460 assertions passed. |
| Affected Hot package suites | `pkg-test-hot-ai`: 203 tests / 416 assertions; `pkg-test-hot-ai-agent`: 109 / 247; `pkg-test-mcp`: 26 / 73. All passed. |
| Browser scope and repository checks | Five browser scope regressions, AGENTS synchronization, and diff whitespace checks passed. |

The new runtime regressions exercise concurrent same-name metadata, duplicate
manifest cache keys in different build directories, module initialization,
warm caches, nested parallel/pmap work, and missing resource manifests. The
initial implementation's deployment-specific qualification limits still apply;
this review did not rerun external provider, remote S3, or Kata qualification.

These fixes strengthen the same root/child scope contract. They do not add
shared deployments or remove the deployment-specific release gates above.

### Existing-database upgrade rehearsal — 2026-10-19

New PostgreSQL and SQLite integration suites build populated databases at
migrations 009 and 015, retain their real SQLx ledgers/checksums, and run the
application migration entry point through the latest migration. PostgreSQL uses
separate NOSUPERUSER, NOBYPASSRLS, NOINHERIT owner and runtime logins. Only the
disposable fixture's role/database/extension setup and historical data seeding
use an administrator. Every application migration and the actual `psql`
provisioning script run as the restricted schema owner.

The rehearsal found and repaired two upgrade gaps:

- Legacy column ACLs survived table-level revocation, and NOINHERIT group
  privileges could evade runtime qualification. Provisioning now removes
  direct table/column/schema CREATE grants, validates effective and assumable
  authority before committing, and refuses unsafe PUBLIC/group grants without
  changing shared principals. Startup qualification checks the same memberships.
- Migration 011's historical preflight omitted run/task origin and run/event
  relationships already checked on new PostgreSQL writes. Additive migration
  017 checks those retained links on both databases without changing historical
  checksums or silently rewriting provenance.

The tests verify migration 016's policy, FK, trigger, and ledger rollback for
cross-org destinations, cross-org subscriptions, and wrong-environment
subscriptions. They retain the offending data, explicitly repair it, and retry.
Migration 017 failures preserve earlier committed migrations, including 016.
Successful upgrades preserve credentials, delegated issuer links, alert retry
state, run results, the admission backfill, and UTF-8 trace sizes. Runtime smoke
checks cover permitted reads/writes, management-key separation, parent/child
isolation, and denied DDL, TRUNCATE, ledger reads, and foreign mutations.
Provisioning is exercised twice, including rollback on unsafe shared grants
and recovery after their explicit removal. CI installs `psql` for these tests.

| Check | Result |
| --- | --- |
| Populated upgrade regressions | Four PostgreSQL and three SQLite tests passed, including the strengthened latest-ledger and valid non-NULL relationship assertions. |
| Rust workspace, all targets, locked/offline | 2,554 passed, zero failed, nine ignored across 37 targets with dedicated PostgreSQL 18/pgvector and Valkey fixtures. |
| Rust formatting and workspace Clippy with `-D warnings` | Passed. |
| Hot static checks | Project/package checks, formatting, documentation snippets, and bundled skill checks passed; the sandbox denied optional package-cache writes without affecting validation. |
| Isolated SQLite `hot init` and main Hot suite | 909 tests and 2,460 assertions passed. |
| Browser scope and repository checks | Five browser scope tests, AGENTS synchronization, workflow YAML parsing, and diff whitespace checks passed. |

These are synthetic populated upgrade fixtures, not a restore of a production
backup. Private bootstrap/provider ordering, deployed proxies/roles, account
usage reconciliation, and production backup/restore remain deployment gates.

### Restricted-role lifecycle validation — 2026-10-19

The API and app lifecycle fixtures each create an isolated PostgreSQL database,
apply the real runtime provisioning script, and open the application pool with
role enforcement enabled. Initial account identities and marker data use the
fixture administrator; child creation, environment provisioning, credentials,
lifecycle mutations, and HTTP reads use the restricted runtime login. The API
test assembles the same router and middleware as production. The app test uses
the real app router and browser-session cookies.

The review repaired these issues:

- Child webhook calls selected a global default user for attribution. A child
  with only inherited root membership exposes no such user under RLS, causing
  event insertion to fail. Authenticated calls now use the credential creator;
  public calls use the selected deployment creator. The handler resolves and
  verifies deployment ownership before persisting or enqueueing work.
- An explicit, unrecognized webhook `auth` value silently made the endpoint
  public. Only omission and the documented `"none"` value retain public access;
  all other explicit values require authentication.
- Credential mutation redirects omitted selected org/env identifiers. Another
  tab could switch navigation cookies before the redirect was followed. API-key
  create/edit redirects and service-key revocation redirects now retain the
  workspace that handled the request.
- RLS-hidden project UUIDs surfaced as database errors and produced HTTP 500.
  Project lookup now maps a missing row to the domain's not-found error, so
  foreign and nonexistent project IDs both return HTTP 404.
- A live deployment rehearsal exposed an ownership trigger that rejected
  webhook redeployment. Webhook capability IDs intentionally survive new builds
  of the same project. Migration `018` permits that build reassignment while
  rejecting moves to another project, including projects in the same environment.
- Queue serialization discarded the transient webhook `original-url`, although
  worker hydration needs it for signature verification. The thin envelope now
  carries only URL restoration hints; request bodies, headers, and execution
  payloads still come from the scoped database record. Restoration requires the
  URL to match that record's redacted path, and durable event data stays redacted.

The fixtures exercise child creation/idempotence, independent environments and
same-name sibling resources, API/service key creation and revocation, issuer
ceilings, token-class separation, suspension and archive recovery, root recovery,
explicit child grants and revocation, stale forms, competing cookie switches,
and sibling read/mutation denial. Webhook checks inspect persisted events and
queued org/env/build/project/creator identities, reject sibling credentials and
capabilities without creating work, and ignore a wrong-environment completion.
Public and authenticated calls use distinct deployment and credential creators.
The webhook test also covers current custom-domain ownership after cache hits.

The automated webhook fixture controls the completion publisher after validating
the actual queue handoff. It checks the thin queue envelope and the authoritative
persisted deployment target; worker hydration restores the omitted project data.
The separate live deployment rehearsal runs actual workflow code in the worker.
Neither rehearsal qualifies external webhook providers, deployed proxies, or
cloud queues.

The live rehearsal used PostgreSQL 18, a dedicated Valkey instance, and the
rebuilt CLI running API, app, and worker processes with database tracing enabled.
The dev startup migration check used a separate owner connection; inspection of
active service connections showed only the restricted runtime login. That login
had neither superuser, bypass-RLS, nor inherited privileges, and an unscoped org
query returned no rows.

- Two child projects deployed the same namespace/function/service/path with
  distinct response markers. Live-to-bundle and subsequent bundle deployment
  preserved each webhook capability ID.
- Ten real queue-to-worker executions (five per child) returned the correct
  marker and exact original URL, including its encoded query. All ten persisted
  runs succeeded with the correct child environment and deployed build.
- Sibling capability substitution returned 404 without creating an event.
  Child suspension/archive and root suspension returned 403 without creating an
  event. The active sibling kept executing; valid recovery restored execution.
  Direct archive-to-active recovery remained rejected.
- Durable webhook event data contained redacted URLs, with no capability tokens.

The populated upgrade regression now starts at migrations `009`, `015`, and
`017`, retains existing webhook rows, applies the current chain, reprovisions
runtime grants, and verifies same-project redeployment plus rejection of
cross-project/environment reassignment. The live fixture also applied `018`
to its existing deployment before the successful rehearsal.

Final validation:

| Check | Result |
| --- | --- |
| Rust workspace, all targets, with PostgreSQL and Valkey enabled | 2,559 passed, 0 failed, 9 ignored across 37 targets |
| Production API/router restricted-role lifecycle | Passed, including API/service keys, issuer revocation/ceilings, webhook routing, lifecycle denials, and sibling isolation |
| PostgreSQL RLS and populated upgrade regressions | Passed, including migration 018 redeployment and the existing migration failure/recovery cases |
| Browser scope JavaScript tests | 5 passed |
| Live CLI/API/UI/Valkey/worker rehearsal | 10 successful executions; lifecycle, sibling denial, deployment identity, URL delivery, and persisted scope assertions passed |
| Isolated Hot initialization and main suite | 909 tests and 2,460 assertions passed |
| Rust formatting, Clippy with warnings denied, Hot static checks, AGENTS synchronization, and diff checks | Passed |

The service-backed suite used `HOT_TEST_POSTGRES_URI`, `REDIS_URL`, and
`HOT_REDIS_URI` against disposable local fixtures. Those fixtures, including
the live-worker database and restricted role, were removed after validation.

### PR follow-up and maintained usage — 2026-10-20

Integrated the incoming review fixes and current main while preserving the
published branch history. Scope is applied once per transaction, migration
credentials stay out of runtime startup, failed retention lookups preserve
history, observed requests avoid shared enforced budgets, and runnable-work
admission distinguishes suspension from exhausted capacity.

Migrations 013–015 now maintain storage, task and run counters under the same
scope and account-lock contract. Calls carry immutable environment ownership
validated against their run, including retention and cascaded deletion.
Period rollover takes a fresh snapshot after counter locks; hourly reconciliation
compares consistent snapshots and adds relative repairs without stopping
ordinary writes. Invalid metering falls back while valid signed 64-bit values
remain accepted. Candidate task locks use exact environment authority because
Billing scope deliberately cannot update task rows.

A scoped unit-of-work API combines multi-statement reads, beginning with project
pagination. Detached advisory sessions keep one-connection data pools usable
while the scheduler or reconciler holds coordination locks. Tests cover those
boundaries plus sibling counter denial, slot deltas, rollback, cancellation,
period changes, full-range metering and populated owner/runtime upgrades.

Final local qualification passed 2,586 Rust workspace tests (10 ignored),
915 Hot tests and 2,509 assertions each on isolated SQLite and restricted
PostgreSQL/Valkey, Clippy with warnings denied, browser scope tests, Hot static
checks, and AGENTS/AI mirror synchronization. The 100,000-row account benchmark
and populated non-BYPASS owner backfill are recorded in the counter document.

The temporary engineering handoff is folded into
[usage counter operation and measured performance](../../resources/db/postgres/USAGE_COUNTERS.md),
[RLS setup](../../resources/db/postgres/RLS_SETUP.md) and
[RLS coverage](../../resources/db/postgres/RLS_COVERAGE.md). The paired provider
change has its own bootstrap/billing integration and one-off migration-task
checks. Snapshot-copy rehearsal, deployed integrations and production cutover
remain explicit release gates. Shared deployments and usage-display semantics
remain deferred.

Hot will support a parent organization with child organizations that each own
their environments, projects, deployments, credentials, and runtime data.
The parent owns the subscription and human member directory. Authorized parent
administrators can operate children; siblings remain isolated.

Implement the scope contract and PostgreSQL row level security before enabling
child creation. Preserve existing standalone organizations and environment-bound
credentials. Shared deployments, shared releases, and fleet deployment tooling
are deferred to separate plans.

## Decisions and boundaries

The following defaults define the initial implementation. Changes to these
defaults must update the contract and its acceptance tests before implementation.

| Concern | Initial decision |
| --- | --- |
| Hierarchy | One root and one level of direct children |
| Resource ownership | Each environment has one owning org; each child owns its own environments and deployments |
| Tenant identifier | No new `tenant_id`; the child `org_id` is the isolation identity |
| Existing orgs | Remain roots with unchanged IDs, environments, routes, keys, and data |
| Root workloads | Remain supported in the root's own environments |
| Billing | Subscription, payment administration, and account limits belong to the root |
| Human identity | Existing global users; invitations and the member directory belong to the root |
| Child membership | Optional explicit child grants to existing root members; no copied memberships for inherited root admins |
| Parent access | Root admins inherit child administration; ordinary root members need explicit child grants |
| Runtime credentials | Continue to target exactly one environment |
| Aggregate access | Explicit, authorized, read-only selection; never inferred from missing IDs |
| Data sharing | No automatic parent secret, context, file, store, or deployment inheritance |
| Lifecycle | Create new children; no reparenting, nesting, detaching, or converting existing orgs into children |
| Database support | Shared application authorization on PostgreSQL and SQLite; PostgreSQL adds RLS |

This boundary covers Hot-managed resources. It does not add policies to a
customer's external database, isolate arbitrary code with platform database
credentials, or provide dedicated compute for each child. Account quotas and
scheduling fairness are separate from data visibility.

## Existing implementation and change map

These are the primary entry points for implementation. Links resolve to the
repository source; the observations use the baseline described above.

| Area | Current structure | Main source locations |
| --- | --- | --- |
| Database access | Public PostgreSQL and SQLite pools; repositories commonly execute against the pool directly | [db/mod.rs](../../crates/hot/src/db/mod.rs), [db/run.rs](../../crates/hot/src/db/run.rs) |
| Ownership | Environment to org, project to environment, build to project; many execution rows carry `env_id` | [env.rs](../../crates/hot/src/db/env.rs), [project.rs](../../crates/hot/src/db/project.rs), [build.rs](../../crates/hot/src/db/build.rs) |
| API authentication | Actual credential context plus a compatibility parent API key; handlers have mixed extraction patterns | [hot_api/auth.rs](../../crates/hot_api/src/auth.rs), [handlers/events.rs](../../crates/hot_api/src/handlers/events.rs) |
| Permissions and credentials | Common resource/action permissions; derived credentials are checked against the issuer at creation | [permission.rs](../../crates/hot/src/permission.rs), [service_keys.rs](../../crates/hot_api/src/handlers/service_keys.rs), [sessions.rs](../../crates/hot_api/src/handlers/sessions.rs) |
| Human access | Direct org membership, current org/env cookies, scattered admin checks | [hot_app/auth.rs](../../crates/hot_app/src/auth.rs), [db/org.rs](../../crates/hot/src/db/org.rs) |
| Runtime authority | Host execution context is distinct from mutable language context | [lang/event/mod.rs](../../crates/hot/src/lang/event/mod.rs), [lang/hot/ctx.rs](../../crates/hot/src/lang/hot/ctx.rs) |
| Background writes | Trace writer shards by run ID and batches writes into transactions | [database_writer.rs](../../crates/hot/src/lang/emitter/database_writer.rs) |
| Workers and scheduler | Queue discovery, ownership hydration, dispatch, retry, and recovery cross service boundaries | [hot_worker/server.rs](../../crates/hot_worker/src/server.rs), [hot_task_worker/lib.rs](../../crates/hot_task_worker/src/lib.rs), [hot_scheduler/server.rs](../../crates/hot_scheduler/src/server.rs) |
| Files and data | Files, blobs, stores, streams, caches, and object storage enforce scope outside SQL too | [file_storage.rs](../../crates/hot/src/file_storage.rs), [blob.rs](../../crates/hot/src/blob.rs), [store/mod.rs](../../crates/hot/src/store/mod.rs), [stream/mod.rs](../../crates/hot/src/stream/mod.rs) |
| Billing | Plans, features, usage, and onboarding currently resolve against one org | [features.rs](../../crates/hot/src/db/features.rs), [subscription.rs](../../crates/hot/src/db/subscription.rs), [billing_provider.rs](../../crates/hot_app/src/billing_provider.rs) |
| Endpoint routing | Webhooks, MCP, and domains resolve to environments and deployed builds | [webhook.rs](../../crates/hot_api/src/handlers/webhook.rs), [mcp.rs](../../crates/hot_api/src/handlers/mcp.rs), [domain_resolver.rs](../../crates/hot_api/src/domain_resolver.rs) |

At the source baseline, public schema migrations contained no platform RLS policy implementation.
The initial dump's `SET row_security = off` is not a policy or evidence of the
deployed role configuration. Production role provisioning and private cloud
integration must be verified as part of the rollout.

## Phase sequence

| Phase | Deliverable | Dependency and release boundary |
| --- | --- | --- |
| 0 | Scope contract and executable authorization fixtures | Establishes all later invariants |
| 1 | Scoped repositories, credentials, and asynchronous propagation | Preserves standalone-org behavior; no child creation |
| 2 | PostgreSQL RLS, ownership constraints, and enforcement rollout | Must pass under restricted runtime roles before children are enabled |
| 3 | Hierarchy schema, membership, billing, quotas, and lifecycle | Feature remains disabled until end-to-end coverage is complete |
| 4 | API, key management, webhooks, MCP, CLI, and SDK contracts | Child runtime access and programmatic administration |
| 5 | Parent/child UI and explicit aggregate views | Uses the same authorization and billing resolvers as APIs |
| 6 | Migration rehearsal, isolation qualification, and staged release | Enables child creation only after all release gates pass |

API and UI implementation can proceed in parallel once the Phase 3 interfaces
are stable. Schema and policy design can overlap Phase 1, but enforcement cannot
precede conversion of every caller of a protected table, including background
writers. Each phase should be split into the reviewable changes listed below.

## Phase 0 Scope contract

### Identity and ownership

Introduce shared types and authorization interfaces in the core crate. The
names below describe responsibilities rather than a required Rust API shape.

| Concept | Required information and meaning |
| --- | --- |
| Principal | Actual authenticated user or credential, issuer lineage where applicable, and current grants |
| Environment scope | One validated `(org_id, env_id)` pair; required for all platform execution |
| Organization scope | One org and an authorized organization operation, such as member administration or environment discovery |
| Authorized read set | Explicit org/environment selections for aggregate reads; cannot be used for execution or ordinary mutations |
| Billing owner | Root org resolved from the hierarchy; never substituted for the resource-owning org |
| Execution context | Persisted resource scope, execution identifiers, and actor attribution reconstructed by trusted host code |

Treat scope resolution and action permission as separate checks. Access requires
both. Avoid a general-purpose `is_admin` boolean that conflates root membership,
child administration, billing, secret access, and platform maintenance.

1. An environment belongs to exactly one org. A project and its builds operate
   only in that environment. Parent and child environments with the same name
   are different resources with different IDs.
2. Every event, run, task, stream, webhook invocation, and MCP invocation has one
   concrete environment scope. Scope survives dispatch, retries, child tasks,
   checkpoint/resume, cancellation, and restart recovery.
3. A related ID is valid only when its ownership matches the operation. Existence
   of a build, stream, task, blob, or file is insufficient authorization.
4. The parent may select a child only through an effective grant. Hierarchy does
   not change the child's ownership or make parent resources a lookup fallback.
5. Scope comes from verified credentials, effective human grants, registered
   endpoint bindings, and persisted work. Headers, request bodies, service-key
   metadata, event payloads, and mutable `hot.request` values are not authority.
6. Customer code cannot change canonical scope through `::hot::ctx/set`.
7. Platform maintenance is a separately typed and audited operation. Missing
   scope, a nil UUID, or a boolean supplied by a caller cannot enable it.

Scope-free compilation, checking, or standalone local evaluation may remain
supported where it does not access scoped platform repositories. Local managed
execution resolves its concrete local org/environment. An absent execution
context must never select unrestricted database access as a convenience fallback.

### Selection and null semantics

For existing runtime APIs, omission of an org selector uses the credential's
environment. A child-bound credential cannot clear or replace that binding.
Human operations resolve an explicit route target against current grants.
Cookies may supply a navigation preference, but are not authorization.

Reject supplied but invalid targets and path/domain/credential conflicts.
Do not fall back to another org or environment for an execution or mutation.
An overview of multiple children is an explicit read mode with a server-resolved
selection; an absent child ID never means all children.

Some existing file/blob and alert resources legitimately have organization or
system scope. Classify these by resource type instead of interpreting every
null `env_id` the same way. Preserve existing documented same-org access to
org-wide file/blob resources where supported; do not extend it to ancestors or
siblings. Creating or changing an org-wide resource requires an explicit
organization operation. New execution data always requires an environment.

### Human access defaults

| Principal | Effective access |
| --- | --- |
| Active root admin | Root administration and inherited administration of active children, including selecting their runtime environments |
| Active root member | Existing root member rights; no implicit child rights |
| Root member with an active child member grant | Existing member operations in that child |
| Root member with an active child admin grant | Operational administration in that child; no root billing, hierarchy, or root member administration |
| Inactive root member or inactive user | No human child access, even if a child grant remains recorded |
| Child machine credential | Its own action grants in its exact environment; human ancestry does not broaden it |

Only root admins manage child human-access assignments in the initial release.
Invitations remain root-owned. Reuse child `org_user` rows for explicit grants
to root members; do not create rows merely to represent inherited root admins.
Children may have zero explicit members. Team-based child grants and external
users who belong only to a child are deferred.

Root admin inheritance is a default role mapping, not a rule that every future
root role inherits every capability. Keep billing, child administration, data
read, execution, credential issuance, and secret access distinct in the resolver
so later roles can receive only the capabilities they need.

### Credential and revocation defaults

Existing API keys retain their format and exact environment binding. A key in
a root environment gains no child access. Organization-owned API keys survive
removal of their creating user unless explicitly revoked; creator identity is
attribution rather than a continuing personal access grant.

Service keys and API sessions must match their issuing API key's environment.
For every request, effective action permission is the intersection of the
presented credential's grants and the issuer's current grants. Both must be
active and unexpired where expiry applies. Verify root, child, and environment
admission status too. Issuer expansion cannot expand a derived credential beyond
its stored grants. Narrowing or revoking an issuer affects subsequent requests.

Only API keys mint runtime service keys and API sessions. Preserve the current
restriction against further delegation by those derived credentials. Validate
permissions and exact scope at issuance and use the actual presented principal
for attribution and rate accounting.

Do not cache authorization decisions in the initial implementation. Resolve
current activity and grants for each authenticated operation. Long-lived
connections revalidate at reconnect, before new client commands, and at most
every 30 seconds; close them on lost access. This interval is a maximum revocation
window for an already-open stream, not a promise of immediate cancellation.

Credential expiration or revocation blocks new ingress and reads. Already
accepted durable work retains its persisted scope and may finish; it does not
depend on a short-lived initiating session remaining valid. Org suspension is a
separate admission decision described in Phase 3. Explicit cancellation remains
available for stopping accepted work.

### Scope contract deliverables and gate

- Add the shared principal, validated scope, and `authorize(action, target)`
  interfaces, including explicit organization and aggregate read modes.
- Encode the access matrix and null semantics as shared fixtures for API, app,
  worker, and database integration tests.
- Inventory every authorization entry point and all raw-pool callers. Mark
  bootstrap, maintenance, and provisioning exceptions individually.
- Record compatibility changes, especially dynamic issuer permission narrowing
  and stricter inactive-membership handling, for release notes.

Exit gate: tests establish allowed and denied operations for a root with its
own workload, two children, and an unrelated root. No production caller may
construct an authorized scope directly from unchecked request fields.

## Phase 1 Scoped access and execution propagation

### Database units of work

Introduce a scoped repository executor instead of passing a raw `DatabasePool`
to request-facing data operations. PostgreSQL operations acquire a connection,
begin a short transaction, establish validated transaction-local scope, execute
all queries through that transaction, and commit or roll back. SQLite uses the
same validated scope and explicit predicates without PostgreSQL session state.

Setting scope through one pooled connection and querying through the pool is
forbidden: the query could use another connection. Session-persistent scope
settings are also forbidden. Missing scope must deny scoped access, with no
fallback to an unrestricted executor after an error.

Keep transactions short. Do not hold them during external HTTP calls, workflow
execution, container work, a queue wait, or an SSE session. Each independent
unit of work obtains its own authorized executor. Cancellation and error paths
must roll back before a connection is reused.

Provide separate repository interfaces for these unavoidable discovery paths:

- Credential lookup and issuer validation before the environment is known.
- User, membership, and hierarchy resolution before a target is authorized.
- Public domain, webhook capability, and anonymous MCP route lookup.
- Queue claiming, schedule discovery, leases, retention, billing aggregation,
  and blob garbage collection.

Each interface has narrow operations and result fields. It cannot expose a
general unrestricted pool to handlers or customer-code execution. Authentication
secret reads and control-plane discovery require specific grants even when they
are not scoped to a single environment.

### Role and provisioning separation

Runtime pool creation and migrations currently resolve the same database URI.
Introduce explicit runtime versus migration/provisioning configuration and
separate deployment secrets. Do not quietly use the migration connection when
runtime credentials are absent in an RLS-enforced installation.

Define logical role responsibilities before choosing deployment-specific role
names. Use separate login identities where process responsibilities require it.

| Responsibility | Allowed authority |
| --- | --- |
| Schema owner and migrator | DDL and schema changes; credentials unavailable to normal runtime processes |
| Scoped runtime | Required DML and sequence use on protected data; non-owner, no superuser or `BYPASSRLS`, no ability to assume privileged roles; no `TRUNCATE` or blanket `GRANT ALL` |
| Authentication and routing bootstrap | Minimal credential/identity and endpoint resolution operations; no generic customer-data access |
| Coordination and maintenance | Explicit claim, lease, retention, usage, and cleanup operations with auditability |
| Resource provisioner | Necessary index/provisioning DDL, isolated from customer execution |

Move per-store HNSW index creation out of `PgStore::ensure_store` into a
provisioning operation. Define provisioning status, retries, and readiness so
restricting runtime DDL does not silently break vector search setup.

Where narrow security-definer functions are appropriate, pin a safe search
path, schema-qualify objects, restrict execute privileges, validate arguments,
and keep owners distinct from the runtime login. Prefer these limited operations
over giving all workers a blanket RLS bypass role.

For each helper, declare its execution owner's applicable policies. Being the
table owner does not bypass forced RLS. Prefer explicit policies and restricted
table/column grants for a non-login helper owner; any narrowly justified bypass
authority needs separate review and must be inaccessible through role switching
or arbitrary SQL from the runtime. Test bootstrap and maintenance helpers with
no request scope so forced policies cannot silently suppress required work.

Public migrations contain portable schema and policies. Cluster role creation,
credentials, and deployment-specific grants belong in explicit provisioning
tooling, with a documented self-hosted setup and private cloud integration.
Support configured schemas; do not assume every installation uses `hot`.

### Application and asynchronous conversion

1. Resolve one immutable request context in API and app middleware. Replace
   authorization-sensitive parent-`ApiKey` extraction with the actual principal
   and effective scope, including event publication.
2. Convert repositories and direct SQL in handlers, dashboards, search, exports,
   and aggregate endpoints. Track every remaining raw-pool use in the manifest.
3. Authorize entity-cache hits as well as database results. Cache immutable data
   independently of authority; never treat a cached org/env/build as proof of
   access or active status. Namespace rendered/data caches by relevant scope.
4. Persist scope with accepted queue work. At dispatch, validate ownership
   against authoritative records and reject conflicting queue IDs. Hydration
   cannot silently substitute the parent org or another environment.
5. Carry scope through tasks, retries, dead-letter replay, checkpoints, recovery,
   cancellations, synchronous calls, and emitted events. Queued items from older
   versions may reconstruct omitted org ownership from authoritative env rows;
   mismatches must be rejected rather than overwritten.
6. Separate global queue/schedule discovery from scoped execution after claiming.
   Scope-free scheduler discovery must not become scope-free runtime access.
7. Partition database-writer batches by canonical org/environment before opening
   transactions. Existing run-ID shards can contain multiple scopes. Preserve
   per-run ordering and failure handling without using a broad role to keep
   mixed-scope batching.
8. Verify files, blob rehydration, uploads, stores including vector search,
   pub/sub channels, and build/source caches independently of database RLS.
   Blob GC must retain scope while resolving references and deleting objects.

Exit gate: standalone-org behavior passes on both databases, every protected
repository path has a scope or a reviewed privileged operation, and a worker
running mixed-org work cannot reuse another item's scope. No child creation is
enabled in this phase.

## Phase 2 PostgreSQL RLS and ownership constraints

### Table coverage manifest

Add a version-controlled manifest covering every application table. CI must
require new tables to declare an ownership path and access category. Include
read/write policies, applicable roles, required indexes, lifecycle operations,
and the reason for any RLS exemption. Global does not mean publicly readable.

| Initial family | Tables or ownership paths | Policy treatment |
| --- | --- | --- |
| Environment metadata | `env` | Authorized org-level environment discovery or exact-env access; preserve an acyclic ownership-policy dependency |
| Direct environment | `access`, `agent`, `workflow`, `alert`, `api_key`, `service_key`, `session`, `domain`, `event`, `project`, `run`, `stream`, `task` | Exact authorized env; verify owning org; secret columns remain privilege-restricted |
| Derived environment | `build` through project; `call` through run; `event_handler`, `mcp_tool`, `webhook`, `schedule` through build; `schedule_log` through schedule | Ownership joins or validated materialized scope; no unreviewed bypass for lookup by global ID |
| Organization | `org`, `org_user`, `team`, `team_user`, `invite`, `org_note`, `org_usage`, `org_plan`, `alert_destination` | Target-org capability; billing rows resolve only through explicit root billing operations |
| Optional or mixed scope | `context` through env or project; `file`, `file_upload`, `blob_object`, `blob_ref`; alert channels, subscriptions, deliveries, and association tables | Per-table null/shared semantics and consistent ownership of associated rows |
| Environment stores | `store_map`, `store_map_entry` | Org plus env; preserve scoped uniqueness, text/vector search, and indexing |
| Global identity and sensitive control | `user`, `user_auth`, `email_verification`, `email_queue`, `scheduler_state` | Narrow auth/control operations; own-profile or authorized directory projections where needed |
| Global reference | Plan catalog, role/status/type tables | Deliberate read grants and restricted writes; no embedded customer or cloud pricing seeds |

The manifest must enumerate the actual tables rather than relying only on this
family summary. Include all alert join/delivery tables and new hierarchy or
management-credential tables as they are introduced.

Prefer straightforward ownership joins for existing derived tables initially.
Materialize scope on high-volume paths only when measured query plans justify
it; any duplicated scope requires constraints or controlled writes and backfill
validation. Avoid recursive policy dependencies involving org/env discovery.

### Policy contract

Enable and force RLS on scoped tables. Use non-owner runtime roles without
superuser or `BYPASSRLS` authority. Evaluate existing rows with `USING` and
proposed writes with `WITH CHECK`; ensure UPDATE cannot move data to another
scope. PostgreSQL documents owner bypass and policy behavior in
[Row Security Policies](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)
and [CREATE POLICY](https://www.postgresql.org/docs/current/sql-createpolicy.html).

Use transaction-local context on the same connection; it expires when that
transaction ends. See [SET](https://www.postgresql.org/docs/current/sql-set.html).
Missing, malformed, empty, or conflicting settings must fail closed. Scope
settings are supplied by trusted backend code; custom settings are not proof
against an actor who can run arbitrary SQL using the backend's credentials.

Keep the isolation predicate mandatory when combining policies. Permissive
policies combine with OR, so an additional action/read policy must not
accidentally remove the scope restriction. Use restrictive scope policies or a
reviewed equivalent composition, with at least one intended permissive grant.
Aggregate selection uses a separately authorized read executor; it does not
enable broad writes. Enforce read-only transactions or an equivalent restricted
role for that executor. Keep normal execution policies tied to the resource owner,
without automatically matching every row under a billing root.

Test joins, views, upserts, deletes, returning clauses, bulk writes, and
maintenance functions under the real runtime roles. RLS does not replace
foreign keys, column privileges, or application action permissions. Normalize
constraint/authorization errors so they do not unnecessarily disclose sibling
resource existence.

### Ownership integrity and migrations

Audit existing rows before validating new invariants. Independent foreign keys
prove existence but often do not prove matching ownership. Cover at least:

- Environment/org and project/environment ownership.
- Run/task versus build, event, stream, and origin run ownership.
- API key versus service key/session environment.
- Agent/workflow and schedule/handler/endpoint versus build ownership.
- File/upload and blob reference/object org and optional-env agreement.
- Store metadata/entry ownership and alert subscription/destination associations.

Use composite references where practical and database-enforced equivalents
where derived ownership makes a composite key impractical. Scope-bearing
ownership is immutable through ordinary updates. Account for asynchronous
observability writes: provenance may be optional, but clearing optional
provenance must never clear or relax data ownership.

Add forward migrations; do not edit already-released migrations. Backfill in
bounded batches and validate constraints after auditing existing data. Plan
index creation and validation to avoid prolonged locks; SQLite may require
table rebuilding to establish equivalent constraints. Use each backend's next
available migration number at implementation time.

Follow [the migration policy](../../resources/db/V2_MIGRATIONS.md): public schema
and generic role/reference data belong here; cloud plans, provider IDs, prices,
customer corrections, and private bootstrap data do not. Do not run
`cargo sqlx prepare`; this repository uses SQLx runtime query APIs.

### RLS rollout gate

Ship scope-capable code before enabling policies. Activate enforcement by table
family only after every caller and policy dependency is ready. A family is not
complete merely because HTTP reads work; writes, worker recovery, trace
emission, maintenance, and bootstrap all count.

Enabling a table's RLS affects every process sharing its database/schema.
An enforcement canary therefore uses an isolated database/schema. Do not enable
RLS in a shared schema while an older service instance still expects unscoped
access; a service-instance traffic canary alone cannot isolate that change.

CI must provision the migrated schema with a privileged test setup connection,
then execute RLS assertions using restricted runtime credentials. Existing CI
PostgreSQL setup credentials must not be treated as the role under test. Verify
role attributes, ownership, effective grants, and inability to assume a bypass
role before accepting results.

Exit gate: deliberate omitted-filter queries cannot read or mutate another
scope, absent scope denies access, pooled reuse is safe after success/error/
cancellation, mixed-scope background writes pass, and representative query
plans and pool usage have been measured. Establish latency and throughput
budgets from the existing workload before rollout and investigate regressions
outside those budgets. Complete baseline coverage before enabling children.

## Phase 3 Hierarchy membership billing and lifecycle

### Schema and compatibility

Add nullable `org.parent_org_id` referencing `org(org_id)` with deletion
restricted and an index for child listing. Existing rows remain null. Derive
`billing_owner_org_id` as parent-or-self through one resolver; do not persist
duplicate root/billing IDs in every resource row.

Enforce one-level ancestry in both application code and database operations:
reject self-parenting, a parent that is already a child, and assigning a parent
to an org that already owns children. An ordinary CHECK constraint cannot alone
validate other rows. Use concurrency-safe triggers or an equivalently restricted
provisioning procedure, with transactional locking and concurrent-create tests.
Hierarchy ownership is immutable in this release.

Preserve globally unique slugs and existing webhook/MCP route shapes. Add an
optional parent-local `external_id` for customer mapping and idempotent
provisioning; it is a lookup attribute, never an authorization credential.
Use request idempotency for creation so a retry cannot consume another child
slot or create a second default environment.

Enable child creation for organization roots. Individual accounts retain their
current behavior; any account-type conversion must use an explicit supported
flow before child creation.

### Membership and effective access

Implement the Phase 0 access matrix in one resolver shared by app and API.
Check active user, active root membership, applicable child grant, root/child
status, and environment ownership. Audit and correct existing membership
queries and role checks that do not consistently enforce active flags.

Removing root membership immediately removes human child access on subsequent
requests and transactionally deactivates all explicit child grants. Retain audit
history, but re-inviting the same global user must not reactivate those grants;
a root admin must assign them again. If temporary membership suspension is
introduced, represent it separately from removal and define its restoration
behavior explicitly instead of overloading this removal path.

Child admins cannot edit root membership, inherited admin rights, the hierarchy,
or account billing. Add explicit access provenance to responses so UI and audit
can distinguish direct child grants from inherited root administration.

### Billing and account limits

Children receive no independent `org_plan` or checkout flow. All subscription,
feature, onboarding, payment, invoice, and account-state reads resolve through
the billing owner. Reject child subscription mutations explicitly rather than
silently creating a second account.

Keep usage attributed to the actual execution org and environment, then roll up
root-owned plus child-owned consumption exactly once. Preserve billing owner
at accounting-event time for durable usage records. Make ingestion/retries
idempotent and reconcile root totals against source usage.

Count human seats from the root member directory under the existing seat policy;
child grants add no seats. Apply the same root resolution to pending invitations
and seat admission checks. Do not count inherited memberships as additional
users.

All applicable account quotas use aggregate consumption, including runs,
compute, storage, schedules, environments, domains, and requests. Preserve
per-credential and per-environment limits as additional constraints. Root quota
checks and reservations must be atomic under concurrent sibling activity; a
read-then-write check in each child can oversubscribe the account.

Add a generic sub-org-count entitlement. Hosted accounts default to zero until
their cloud plan explicitly enables a limit; local/self-hosted behavior follows
the existing unlimited policy. Cloud plan values are private seed data.
Provisioning checks the root's child count atomically. Archived children with
retained data continue to count until their lifecycle reaches a defined deleted
state, preventing archive/create cycles from avoiding limits.

Child settings may restrict approved capabilities but cannot expand root
entitlements. Per-child budgets can be an optional follow-up; if implemented,
they are additional ceilings, not allocations that increase the root allowance.

### Lifecycle

Persist an org lifecycle state with values `active`, `suspended`, and `archived`.
During compatibility rollout retain the existing `active` boolean as an enforced
projection of that state, update all writers, and prevent inconsistent values.
Backfill true as active and false as suspended; audit any null/invalid legacy
values and resolve them fail-closed before validation. Deletion/purge is a
separate irreversible workflow, not a synonym for suspension.

Effective new-work admission requires both root and target org to be active,
the environment to be active, and account admission/entitlements to permit the
operation. Active root admins retain narrowly defined status, billing recovery,
and lifecycle-management operations while a root is suspended or archived;
they do not regain ordinary runtime access. Restoring an archived org first
moves it to suspended; resuming activity is a separate explicit action. A
purged org cannot be restored through this path.

Create a child in one transaction: authorize root administration, reserve/check
the child limit, create the child, create its default development environment,
record audit attribution, and commit. Do not insert an inherited admin
membership or trigger checkout. External side effects use idempotent jobs after
commit.

Root suspension blocks new ingress and new execution starts throughout the
account. Child suspension applies only to that child. Scheduler dispatch,
queued-work admission, retries that would start a new attempt, and container
starts must consult the same status policy. Queued work is held with an explicit
reason until reactivation or cancellation; it must not be repeatedly claimed
and requeued without backoff.

Already-running work may finish in its original scope, including narrowly
authorized result/trace finalization. Suspension does not grant users renewed
data access, and does not authorize new child tasks or workflow starts. Expose
explicit cancellation for emergency stops. Document suspension versus account
termination separately so enforcement does not depend on incidental worker
timing.

Suspend/archive without cascading runtime history deletion. Root admins may
inspect administrative status and usage and reactivate a child through explicit
management operations; ordinary suspended-scope data access remains denied.
Hard deletion requires cleared retained resources or a separate audited purge
workflow. Refuse parent deletion while children remain. No reparenting,
detaching, promotion, or import of existing standalone orgs in this release.

Exit gate: legacy orgs behave identically, child creation is atomic/idempotent,
root-member revocation applies to children, children bypass checkout correctly,
and concurrent sibling activity cannot multiply entitlements or evade quotas.

## Phase 4 APIs credentials and integrations

### Runtime and management API separation

Existing runtime APIs remain environment-bound. Root and child runtime calls
use their respective environment credentials, with unchanged token formats.
Reject unrelated org selectors instead of silently ignoring them. Apply scope
to collections, counts, search, direct-ID reads, exports, downloads, updates,
deletes, reruns, and cancellation.

Add explicit management operations for child creation/listing/status, child
access grants, environment provisioning, and root usage overview. Human callers
use authenticated root-admin sessions and the shared resolver. Every mutation
targets one org, and an environment when applicable. Aggregate read endpoints
accept authorized filters, pagination, and bounded result sizes.

For programmatic parent administration, introduce an explicitly typed root
management credential rather than expanding existing environment API keys.
It has a root owner, an explicit target selection policy, capabilities, expiry,
revocation, hashed secret, and issuer attribution. Register its authentication
and permission schema separately. Runtime endpoints, webhooks, and MCP execution
reject management credentials.

Only authenticated root-admin humans may issue or change management credentials.
Management keys cannot create other management keys or assign human child-access
grants in this release. A root-admin human can revoke them; a constrained
management key cannot use membership assignment to escape its target or runtime
delegation ceiling. Management credentials are root-owned rather than personal
credentials, so removal of their creator does not silently revoke them; an
administrator must explicitly revoke them.

The first management credential contract covers provisioning and administration.
Runtime key issuance is a distinct capability with a stored ceiling on the
runtime permissions it may delegate and a concrete child/environment target.
For automated issuance, persist issuance lineage, enforce current
issuer scope and delegation ceilings, and cascade management-key revocation or
narrowing to credentials issued through it. Do not infer delegation authority
from a general child-management permission. Existing user-issued environment
keys retain their organization-owned lifecycle.

For the chain management key to API key to service key/session, check every
ancestor's current activity, expiry, scope, and applicable delegation ceiling on
each authenticated operation and stream revalidation. Issued credentials expire
no later than an expiring ancestor. Narrowing the management key restricts the
entire chain; its expansion cannot expand stored descendant grants. Editing or
rotating a delegated key preserves lineage and cannot convert it into an
independent user-issued key. Use explicit revocation state or current-chain
evaluation so asynchronous cascade processing cannot leave an access window.

Document these management endpoints and credential rules before implementing
clients. A human-admin-only management API may ship as an intermediate step;
programmatic provisioning is not complete until the management-credential
contract and its tests are delivered.

### Derived credentials and attribution

Update all permission checks and restriction discovery to intersect current
issuer and derived grants. This includes service-name restrictions, stream
filters, list endpoints, MCP discovery, and webhook execution, not just a single
`has_permission` call. Remove compatibility dependencies on a broader parent
key in publishers and handlers.

Audit records identify the presented credential or human, its authority source,
the target org/env, and the billing owner where relevant. Keep the issuer ID
separate from the credential actually used. Add a control-plane audit record
shape for org operations that have no environment; do not fabricate an env ID
to fit the existing environment-bound access log. Never log credential secrets.

### Webhooks domains MCP and streams

- Child endpoints resolve to child environments and child deployments. An
  authenticated webhook requires matching credential scope; ancestor keys are
  not substitutes. Anonymous routes/capability tokens still establish one
  registered environment and cannot choose arbitrary orgs from request data.
- Validate agreement between URL, custom domain, credential, endpoint, and
  deployment. Scope domain and route caches and invalidate changed bindings.
- Apply the same rule to MCP discovery, invocation, transport session creation,
  message posts, reconnection, and stream replay. Bind transport sessions to the
  actual principal and effective scope.
- Protect SSE replay and live pub/sub independently. Scope subscription channels,
  cursors, terminal-result reads, and blob rehydration. Revalidate authorization
  according to the revocation contract without holding a database transaction
  for the connection lifetime.
- Verify file uploads, multipart completion, download authorization, signed
  object access, blob references, store search, and container file access. Do
  not allow a valid storage identifier to bypass ownership checks.
- Preserve scope on outgoing workflow effects and alert deliveries. Parent
  alert configuration is not inherited by children unless a later explicit
  sharing feature authorizes it.

### CLI SDK and documentation compatibility

Remote CLI commands using a child environment key should work without a new
runtime selector. Update local profiles and org/environment selection to use
effective inherited access. Add explicit management commands only against the
published management API; keep token types and their accepted endpoints clear.

Update OpenAPI models, authentication docs, onboarding, webhook/domain examples,
and SDK contract fixtures. SDK implementations in separate repositories require
coordinated follow-up changes and compatibility tests; do not claim completion
from changes to this repository alone. Existing clients must continue to work
with existing environment-bound endpoints and keys.

Exit gate: two child environments with identical resource names remain isolated
through REST, webhooks, MCP, streaming, uploads, and SDK/CLI access. Root runtime
keys cannot execute child workloads. Management credentials cannot be used as
runtime credentials, and delegated issuance cannot exceed its ceiling.

## Phase 5 Parent and child UI

Use durable route/query state for the effective target. Cookies retain only the
last navigation choice. Server handlers validate route targets, memberships,
and environment ownership for each action; stale tabs and conflicting cookies
cannot redirect a mutation to another org.

Implement these surfaces using the shared API/service contracts:

- A root/child picker and environment picker, with a visible breadcrumb and
  an explicit root-workspace choice for root-owned deployments.
- Root child listing, search, provisioning, status, and lifecycle controls.
- Root member management with explicit child-access assignments. Show inherited
  root administration separately from direct child grants.
- Child operational pages for projects, builds, runs, events, tasks, schedules,
  contexts, keys, endpoints, domains, files, stores, and streams.
- Child settings that identify the root billing owner and link authorized root
  admins to account billing. Child admins receive no checkout/payment controls.
- An explicit read-only all-children overview for usage, health, and activity,
  with child attribution on every row and links into a concrete target.

Changing scope resets selected resources, pagination, filters that are not
portable, cached responses, and live subscriptions. Open a new scoped stream
after closing the previous one. Scope browser/cache keys and test multiple tabs.
Bulk operations must name concrete targets and authorize each; an overview's
read scope cannot be passed to a mutation handler.

Exit gate: inherited root admins can operate a child with no direct memberships;
ordinary root members cannot discover unauthorized child data; stale selections
and scope changes do not display or mutate another child's resources.

## Phase 6 Qualification rollout and operations

### Acceptance matrix

Use a fixture with a root-owned environment, two children with matching names
for projects/stores/files, and an unrelated root. Exercise allowed paths as well
as denials so RLS cannot appear correct merely by returning no data everywhere.

| Area | Required tests |
| --- | --- |
| Database scope | Missing/malformed scope; deliberately omitted WHERE filters; sibling reads/writes; updates moving ownership; joins and upserts |
| Pool lifecycle | Sequential reuse, parallel scopes, rollback, timeout, cancellation, nested/savepoint behavior, no session-state carryover |
| Role configuration | Non-owner runtime role, no bypass/superuser/privileged membership, owner-policy configuration, denied DDL/TRUNCATE and unrelated global tables; helper behavior under FORCE RLS |
| Ownership graph | Cross-env build/stream/event/origin references; blob ref/object mismatch; same-ID attempts across scope; null org-wide resources |
| Human grants | Root admin inheritance; root member denial; explicit child grants; inactive membership/user; remove then re-invite without restored child grants; child-to-parent and sibling denial |
| Credentials | Wrong env, issuer narrowing/expansion/revocation/expiry across the full chain, derived-key subset, immutable lineage on rotation, token-type separation, denial of management-key delegation and human-grant assignment |
| API and UI | Lists/counts/search/exports, direct IDs, rerun/cancel, stale tabs/cookies, aggregate read restrictions, no parent fallback |
| Routing | Child endpoint with sibling credential; domain/path disagreement; anonymous/capability routing; MCP session replay under a different principal |
| Streams and caches | Cache-hit authorization, scope switching, SSE replay/reconnect, open-stream revocation within the defined interval |
| Runtime | Schedules, fan-out, child tasks, retries, recovery, dead letters, mixed-org trace batches, suspension admission and finalization, root recovery and archive transitions |
| Non-SQL data | Redis and in-memory paths, file/blob downloads, multipart uploads, object references, store/vector search, container file access |
| Billing | Root plus child rollup exactly once, deduplicated seats, sibling quota races, limits after suspension/archive, child checkout denial |
| Migration | Fresh install, upgrade from the current migration chain, retained queued work, old-client compatibility, SQLite schema/application parity |

Add PostgreSQL role-enforced integration tests to the existing service-backed CI
suite. Keep SQLite tests for common semantics. Run the repository checks in
[CONTRIBUTING.md](../../CONTRIBUTING.md) for implementation changes, including
formatting, Clippy, Rust tests, Hot static checks, and relevant runtime suites.
Storage/queue backend variants and cloud provider integrations require their
own qualification; PostgreSQL passing does not prove Redis or S3 isolation.

### Deployment order and release gates

1. Deploy scoped application code and configuration support while behavior
   remains standalone-org only. Inventory private cloud consumers and old
   service versions before changing database enforcement.
2. Provision restricted roles and separate migration credentials. Rehearse
   fresh self-hosted setup and hosted upgrade, including custom schemas.
3. Apply additive integrity migrations and policies. Canary enforcement in an
   isolated database/schema; verify all consumers of each shared schema are
   scope-capable before enabling its converted table families.
4. Complete baseline RLS coverage and runtime-role tests. Deploy hierarchy schema,
   root billing resolution, and child-capable code with creation disabled.
5. Enable internal test roots, then a limited pilot. Qualify account limits,
   background jobs, key revocation, public endpoints, UI streams, and billing
   provider reconciliation before expanding access.
6. Enable eligible roots only after the entire acceptance matrix passes and
   operational runbooks are ready. Record completion per table/service family
   rather than describing partial coverage as complete isolation.

Monitor authorization denials, unexpected empty reads, database policy errors,
pool saturation, query latency, queue age, held work, missing execution traces,
stream closures, scope mismatches, quota contention, and usage discrepancies.
Include org/env and operation identifiers where safe; keep secrets and customer
payloads out of diagnostic logs.

### Rollback and recovery

Preserve policies and additive ownership columns during an application rollback.
Roll back only to a version that understands scoped execution and, once children
exist, hierarchy, billing ownership, and child admission. Record the minimum
compatible version at each release gate.

Disable new child creation or pause affected ingress/dispatch when necessary;
do not automatically disable RLS or retry failed queries through an unrestricted
pool. Retain child records and runtime history. A policy repair is an explicit
migration with verification, not a silent access expansion.

Rehearse database and object-store backup/restore together, including policy,
role, reference, and queue state. Privileged maintenance must account for all
intended data rather than silently producing a filtered backup. Verify restore
under runtime roles before reopening access.

Exit gate: migration and rollback rehearsals pass, production role/grant checks
are observable, runbooks cover denied/missing scope and worker stalls, and
parent/child billing reconciles with source usage.

## Suggested reviewable changes

These are implementation slices, not calendar estimates. Avoid enabling a
slice whose callers or policy dependencies remain unconverted.

1. Shared scope/principal contract, authorization matrix, and active-state fixes.
2. Scoped PostgreSQL/SQLite repository executor and bootstrap interfaces.
3. Request authentication conversion and current-issuer permission intersection.
4. Worker/queue propagation, scoped trace batching, and cache authorization.
5. Runtime/migration/provisioning credential separation and HNSW provisioning.
6. Table manifest, ownership integrity migrations, and runtime-role CI harness.
7. RLS policies by table family, query-plan review, and enforcement rollout.
8. Hierarchy schema, root resolution, explicit child grants, and lifecycle.
9. Billing/feature/onboarding resolution and atomic account quota accounting.
10. Child management API, idempotent provisioning, audit, and credential contract.
11. Management credentials and delegated issuance with revocation tests.
12. Webhook/MCP/domain/stream/storage qualification and CLI/OpenAPI/SDK changes.
13. Hierarchical UI, member assignment, billing display, and aggregate views.
14. Full acceptance suite, migration rehearsal, runbooks, and gated rollout.

## Deferred work

Shared deployments, shared releases, deployment fan-out, and fleet rollout
management require separate designs. This implementation must not load a parent
build when a child has no deployment, share secrets by ancestry, or let a run use
another org's deployment implicitly.

Also defer arbitrary-depth hierarchies, reparenting or promotion, independent
child subscriptions, child-only external users, team-derived child grants,
cross-child runtime transactions, and any additional tenant dimension inside an
environment. Preserve separate concepts for actor, resource owner, and billing
owner so later work can add explicit relationships without weakening isolation.
