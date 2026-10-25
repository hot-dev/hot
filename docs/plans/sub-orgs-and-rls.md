# Sub-organizations and PostgreSQL RLS

This note describes the design of sub-organizations and PostgreSQL row level
security (RLS) in Hot 3.0. A root organization can own direct child
organizations. Each child owns its environments, projects, deployments,
credentials, and runtime data. The root owns the subscription and the human
member directory. Authorized root administrators can operate children.
Siblings stay isolated.

Related documents:

- [Sub-organization management](../../resources/docs/api/management.md) is the
  public API contract.
- [Self-Hosted PostgreSQL](../../resources/docs/configuration/postgres.md) is
  the operator guide for installation and upgrades.
- [RLS setup](../../resources/db/postgres/RLS_SETUP.md) covers PostgreSQL
  installation and upgrades in more depth.
- [RLS coverage](../../resources/db/postgres/RLS_COVERAGE.md) lists every table
  and its policy.
- [Maintained usage](../../resources/db/postgres/USAGE_COUNTERS.md) covers the
  quota counters.
- [Cancelled execution quota recovery](../../resources/db/postgres/CANCELLATION_QUOTA_RECOVERY.md)
  covers container reservations.

## Decisions and boundaries

| Concern | Decision |
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
customer's external database, isolate arbitrary code that holds platform
database credentials, or give each child dedicated compute. Account quotas and
scheduling fairness are separate from data visibility.

## Scope contract

### Identity and ownership

| Concept | Meaning |
| --- | --- |
| Principal | The authenticated user or credential, its issuer lineage where applicable, and its current grants |
| Environment scope | One validated `(org_id, env_id)` pair; required for all platform execution |
| Organization scope | One org and an authorized organization operation, such as member administration or environment discovery |
| Authorized read set | Explicit org and environment selections for aggregate reads; never used for execution or ordinary mutations |
| Billing owner | The root org, resolved from the hierarchy; never substituted for the resource-owning org |
| Execution context | Persisted resource scope, execution identifiers, and actor attribution, rebuilt by trusted host code |

Scope resolution and action permission are separate checks. Access requires
both. There is no general `is_admin` flag. Root membership, child
administration, billing, secret access, and platform maintenance stay distinct.

1. An environment belongs to exactly one org. A project and its builds operate
   only in that environment. Parent and child environments with the same name
   are different resources with different IDs.
2. Every event, run, task, stream, webhook invocation, and MCP invocation has
   one concrete environment scope. Scope survives dispatch, retries, child
   tasks, checkpoint and resume, cancellation, and restart recovery.
3. A related ID is valid only when its ownership matches the operation. The
   existence of a build, stream, task, blob, or file is not authorization.
4. The parent selects a child only through an effective grant. The hierarchy
   does not change the child's ownership or make parent resources a lookup
   fallback.
5. Scope comes from verified credentials, effective human grants, registered
   endpoint bindings, and persisted work. Headers, request bodies, service-key
   metadata, event payloads, and mutable `hot.request` values are not authority.
6. Customer code cannot change canonical scope through `::hot::ctx/set`.
7. Platform maintenance is a separately typed operation. Missing scope, a nil
   UUID, or a caller-supplied boolean cannot enable it.

Compiling, checking, and ad hoc evaluation that never touch scoped platform
data need no scope. Managed local execution resolves its concrete local org and
environment. A missing execution context never falls back to unrestricted
database access.

### Selection and null semantics

When a runtime API call omits an org selector, the credential's environment
applies. A child-bound credential cannot clear or replace that binding. Human
operations resolve an explicit route target against current grants. Cookies
may supply a navigation preference, but they are not authorization.

Hot rejects supplied but invalid targets, and conflicts between path, domain,
and credential. Executions and mutations never fall back to another org or
environment. An overview of several children is an explicit read mode with a
server-resolved selection. An absent child ID never means all children.

Some file, blob, and alert resources have organization or system scope. They
are classified by resource type, not by treating every null `env_id` the same
way. Existing same-org access to org-wide files and blobs stays. It does not
extend to ancestors or siblings. Creating or changing an org-wide resource
needs an explicit organization operation. New execution data always needs an
environment.

### Human access

| Principal | Effective access |
| --- | --- |
| Active root admin | Root administration and inherited administration of active children, including selecting their runtime environments |
| Active root member | Existing root member rights; no implicit child rights |
| Root member with an active child member grant | Member operations in that child |
| Root member with an active child admin grant | Operational administration in that child; no root billing, hierarchy, or root member administration |
| Inactive root member or inactive user | No human child access, even if a child grant remains recorded |
| Child machine credential | Its own action grants in its exact environment; human ancestry does not broaden it |

Only root admins manage child access grants. Invitations stay with the root.
A child grant reuses a child `org_user` row for a root member. Inherited root
admins get no rows. A child may have no explicit members.

Root admin inheritance is a default role mapping. The resolver keeps billing,
child administration, data read, execution, credential issuance, and secret
access distinct, so later roles can receive only the capabilities they need.

### Credentials and revocation

Existing API keys keep their format and exact environment binding. A key in a
root environment gains no child access. User-issued API keys are
organization-owned. They survive removal of their creator unless revoked; the
creator is attribution, not a continuing personal grant.

Service keys and API sessions must match their issuing API key's environment.
On every request, the effective permission is the intersection of the
presented credential's grants and the issuer's current grants. Both must be
active and unexpired, and the root, child, and environment must admit the
operation. Expanding an issuer never expands a derived credential beyond its
stored grants. Narrowing or revoking an issuer affects later requests. Only API
keys mint service keys and API sessions. Derived credentials cannot delegate
further.

Authorization decisions are not cached. Each authenticated operation resolves
current activity and grants. Long-lived connections revalidate at reconnect,
before new client commands, and at least every 30 seconds. They close when
access is lost. That interval is the longest revocation window for an open
stream.

Expiry or revocation blocks new ingress and reads. Accepted durable work keeps
its persisted scope and may finish. It does not depend on the initiating
session staying valid. Org suspension is a separate admission decision.
Explicit cancellation stops accepted work.

## Scoped database access

### Units of work

Request-facing data operations use a scoped executor instead of a raw pool. On
PostgreSQL, an operation acquires a connection, begins a short transaction,
applies the validated scope with one `SELECT hot.apply_scope(...)` call, runs
every query in that transaction, and then commits or rolls back. SQLite uses
the same validated scope through explicit predicates.

Scope settings are transaction-local. No query runs on a pooled connection
whose scope was set by a different transaction, and no scope persists in a
session. Missing scope denies access. An error never falls back to an
unrestricted executor. Transactions stay short. They are never held across
external HTTP calls, workflow execution, container work, queue waits, or SSE
sessions.

A few discovery paths run before an environment is known: credential lookup,
membership and hierarchy resolution, public domain, webhook, and MCP route
lookup, and queue, schedule, lease, retention, billing, and cleanup work. Each
uses a named bootstrap or maintenance operation with narrow queries and its
own policies. None exposes an unrestricted pool to handlers or customer code.

### Database roles

| Responsibility | Authority |
| --- | --- |
| Schema owner and migrator | DDL and schema changes; credentials unavailable to normal runtime processes |
| Scoped runtime | Required DML and sequence use on protected data; not the owner; no superuser, `BYPASSRLS`, or assumable privileged roles; no `TRUNCATE`, `REFERENCES`, or `TRIGGER` |
| Bootstrap operations | Credential, identity, membership, onboarding, and route resolution; no general customer-data access |
| Maintenance operations | Named queue, scheduler, retention, alert, billing, provisioning, and cleanup work |
| Store index provisioning | HNSW index DDL as the schema owner, isolated from customer execution |

The runtime pool (`db.uri`) and the migration connection (`db.migration-uri`)
are configured separately. An enforced installation never uses the migration
connection when runtime credentials are missing. `provision_runtime.sql`
grants the runtime login what it needs. Startup qualification rejects a runtime
login that owns tables, is a superuser, bypasses RLS, can run DDL, or holds
unsafe privileges through role membership. Enforced runtime currently requires
schema `hot`.

Policies and triggers run with the caller's rights. There are no SECURITY
DEFINER functions, and the table owner does not bypass forced RLS. Runtime
store creation never runs DDL. `hot db provision-store-indexes` creates
per-store HNSW indexes as the schema owner. Searches use exact scans until
those indexes exist.

Public migrations contain the portable schema and policies. Cluster roles,
credentials, and deployment-specific grants belong to provisioning tooling.

### Execution propagation

1. API and app middleware resolve one immutable request context from the
   actual principal and effective scope.
2. Entity caches authorize hits as well as database results. A cached org,
   environment, or build is never proof of access or active status. Rendered
   and data caches are namespaced by scope.
3. Accepted queue work persists its scope. Dispatch validates ownership against
   authoritative records and rejects conflicting queue IDs. Hydration never
   substitutes the parent org or another environment.
4. Scope carries through tasks, retries, dead-letter replay, checkpoints,
   recovery, cancellation, synchronous calls, and emitted events. Queued items
   from older versions may rebuild omitted org ownership from the authoritative
   environment row. A mismatch is rejected, not overwritten.
5. Global queue and schedule discovery is separate from scoped execution after
   a claim.
6. The trace writer groups writes by scope and commits each group in its own
   scoped transaction, keeping per-run order.
7. Files, blob rehydration, uploads, stores including vector search, pub/sub
   channels, and build and source caches enforce scope independently of
   database RLS. Blob cleanup keeps scope while it resolves references and
   deletes objects.

## PostgreSQL row level security

### Coverage

Every Hot table either has RLS enabled and forced, or appears in an explicit
reference-table allowlist in [rls.rs](../../crates/hot/src/db/rls.rs). Runtime
startup rejects any table the runtime login can reach that has neither.
[RLS coverage](../../resources/db/postgres/RLS_COVERAGE.md) lists each table.

| Family | Tables or ownership paths | Treatment |
| --- | --- | --- |
| Environment metadata | `env` | Authorized org-level environment discovery or exact environment access, without recursive policy dependencies |
| Direct environment | `access`, `agent`, `workflow`, `alert`, `api_key`, `service_key`, `session`, `domain`, `event`, `project`, `run`, `stream`, `task` | Exact authorized environment; owning org verified; secret columns stay privilege-restricted |
| Derived environment | `build` through project; `call` through its own `env_id`, validated against its run; `event_handler`, `mcp_tool`, `webhook`, `schedule` through build; `schedule_log` through schedule | Ownership joins or validated materialized scope; no lookup by global ID without scope |
| Organization | `org`, `org_user`, `team`, `team_user`, `invite`, `org_note`, `org_usage`, `org_plan`, `alert_destination` | Target-org capability; billing rows resolve only through root billing operations |
| Optional or mixed scope | `context` through environment or project; `file`, `file_upload`, `blob_object`, `blob_ref`; alert channels, subscriptions, deliveries, and association tables | Per-table null and shared semantics; associated rows share ownership |
| Environment stores | `store_map`, `store_map_entry` | Org plus environment; scoped uniqueness, text and vector search |
| Global identity and control | `user`, `user_auth`, `email_verification`, `email_queue`, `scheduler_state` | Narrow authentication and control operations; own-profile or authorized directory reads |
| Management and usage | `management_key`, `api_key_management_issuer`, `account_request_window`, `run_admission`, `usage_storage`, `usage_task`, `usage_run` | Root org, API key environment, or account billing scope, per table |
| Global reference | Plan catalog and role, status, and type tables | No RLS; runtime receives SELECT only |

Derived tables use ownership joins. Scope is materialized only on hot paths,
such as `call.env_id`, and is then protected by constraints and validated
during backfill.

### Policies

Trusted backend code establishes one scope mode per transaction:

| Mode | Use |
| --- | --- |
| `environment` | Execution and environment reads and writes |
| `organization` | Organization operations, such as member administration and environment discovery |
| `read_set` | Explicit aggregate reads over authorized orgs and environments; the transaction is read-only |
| `user` | A signed-in user's own profile and notifications |
| `bootstrap` | A named operation: `authentication`, `routing`, `membership`, `onboarding`, or `billing_webhook` |
| `maintenance` | A named operation, such as `queue`, `scheduler`, `retention`, `alerts`, `billing`, `organization_provisioning`, `blob_gc`, `email`, or `store_provisioning` |

`hot.apply_scope(...)` verifies the scoped environment against the scoped org
once per transaction. Policies then compare each row with that verified value
instead of querying `env` per row. Settings written directly with `set_config`
verify nothing and grant no environment rows.

Each protected table has separate SELECT, INSERT, UPDATE, and DELETE policies.
Every policy is built from scope predicates, so combining permissive policies
never removes the scope restriction. `USING` checks existing rows and
`WITH CHECK` checks proposed rows, so an UPDATE cannot move data to another
scope. A read set never grants writes. Normal execution policies follow the
resource owner. They do not match every row under a billing root.

Missing, malformed, empty, or conflicting settings fail closed. Scope settings
come from trusted backend code. They are not a defense against an actor who
can run arbitrary SQL with the backend's credentials. RLS does not replace
foreign keys, column privileges, or application action permissions.
Authorization errors avoid revealing whether a sibling's resource exists.

### Ownership integrity

Independent foreign keys prove existence, not matching ownership. Hot also
enforces these relationships:

- Environment and org, and project and environment.
- Run and task versus build, event, stream, and origin run.
- API key versus service key and session environment.
- Agent and workflow, and schedule, handler, and endpoint versus build.
- File and upload, and blob reference and object, on org and optional environment.
- Store metadata and entries, and alert subscriptions, destinations, and deliveries.

Composite references are used where practical, and triggers where derived
ownership makes a composite key impractical. Ownership columns are immutable
through ordinary updates. Clearing optional provenance never clears or relaxes
data ownership. SQLite enforces the same relationships with triggers.

## Hierarchy, membership, and lifecycle

### Hierarchy

`org.parent_org_id` is nullable, references `org`, and restricts deletion.
Existing rows stay null and remain roots. The billing owner is parent-or-self,
resolved in one place. Root and billing IDs are not copied into resource rows.

Database triggers enforce one level of ancestry. An org cannot parent itself,
a child cannot become a parent, and only an active organization root can gain
children. The parent never changes after creation. Slugs stay globally unique,
and existing webhook and MCP route shapes are unchanged. An optional
`external_id`, unique within a parent, maps a child to a customer's own
records. It is a lookup attribute, never a credential.

Child creation runs in one transaction. It authorizes root administration,
checks the child limit, creates the child and its default development
environment, records the request's idempotency key, and commits. A retry with
the same key returns the original child. It does not add an inherited admin
membership or start a checkout.

### Membership

App and API share one resolver for the human access table. It checks the
active user, active root membership, the applicable child grant, root and
child status, and environment ownership.

Removing or deactivating a root membership deactivates that user's child
grants in the same transaction. Inviting the same user again does not restore
them; a root admin must grant them again. Child admins cannot edit root
membership, inherited admin rights, the hierarchy, or account billing. The
resolver records where access comes from, so the app can show a direct child
grant apart from inherited root administration.

### Lifecycle

`org.lifecycle_state` is `active`, `suspended`, or `archived`. The existing
`active` boolean stays as an enforced projection of that state. Deletion and
purge are separate from suspension.

New work needs an active root, an active target org, an active environment,
and account admission. Root suspension blocks new ingress and new execution
across the account. Child suspension applies only to that child. Scheduler
dispatch, queued-work admission, retries that start a new attempt, and
container starts all use the same status policy. Already-running work may
finish in its original scope.

Active root admins keep a narrow set of status, billing recovery, and
lifecycle operations while a root is suspended or archived. They do not regain
ordinary runtime access. An archived org moves to suspended before it can
become active again. Suspension and archiving never cascade-delete runtime
history. Hot refuses to delete a parent while it has children.

## Billing and quotas

Children have no `org_plan` and no checkout flow. Subscription, feature,
payment, invoice, and account-state reads resolve through the root. Child
subscription changes are rejected, not turned into a second account.

Usage stays attributed to the org and environment that ran the work. Account
totals roll up root and child usage exactly once. Seats count the root member
directory. Child grants add no seats.

Account limits apply to the root and all its children together: runs, compute,
storage, schedules, environments, domains, seats, and requests.
Per-credential and per-environment limits still apply on top. Admission
reserves capacity atomically under a lock on the root account, so concurrent
sibling activity cannot oversubscribe it.

Run, task, store, and trace storage admission read maintained counters instead
of scanning history. Triggers update each counter in the same transaction as
the source write. Runs reserve
capacity in a durable admission ledger before user code or a container starts.
An hourly reconciliation compares counters with their source rows. See
[maintained usage](../../resources/db/postgres/USAGE_COUNTERS.md).

The `sub_orgs` feature caps how many children a root can have. A plan that
omits it allows none. Accounts with no plan and no feature overrides keep
unlimited local defaults. When an org has feature overrides but no plan, any
limit the overrides omit is unlimited. Archived children still count. Child
settings may restrict capabilities but never expand the root's entitlements.

## APIs and credentials

### Runtime and management APIs

Runtime APIs stay environment-bound. Root and child runtime calls use their own
environment credentials with unchanged token formats. Unrelated org selectors
are rejected, not ignored. Scope applies to collections, counts, search, direct
ID reads, exports, downloads, updates, deletes, reruns, and cancellation.

The management API under `/v1/management/*` handles child creation, listing,
and lifecycle, environment provisioning, runtime key issuance, and account
usage. It accepts only management credentials. Root admins manage children in
the app through the same resolver. Every mutation targets one org, and one
environment where applicable. Aggregate reads accept authorized filters,
pagination, and bounded result sizes.

### Management credentials

A management credential is a separate credential type, not an expanded
environment API key. It has a root owner, a target policy (an explicit child
list or all children), capabilities, an expiry of at most 90 days, revocation,
a hashed secret, and issuer attribution. Its tokens begin with `hm_`. Runtime
endpoints, webhooks, and MCP execution reject it, and management endpoints
reject runtime tokens.

Only authenticated root admins issue or change management credentials. A
management credential cannot create another management credential or assign
human child access. A root admin can revoke it. Management credentials stop
working when their creator stops being a root administrator.

Runtime key issuance is a separate capability. It has a stored ceiling on the
permissions it may delegate and a concrete child environment target. Hot
records the issuance lineage and checks every ancestor in the chain (management
key, API key, then service key or session) on each authenticated operation and
stream revalidation. That check covers activity, expiry, scope, and delegation
ceilings. Revoking or narrowing a management key restricts the whole chain.
When a management key expires, the runtime keys, sessions, and service keys
issued through it stop working. Expanding it never expands stored descendant
grants. Editing or rotating a delegated key keeps its lineage. It never becomes
an independent user-issued key.

### Attribution

Permission checks and restriction discovery intersect current issuer and
derived grants. That includes service-name restrictions, stream filters, list
endpoints, MCP discovery, and webhook execution. Attribution and rate
accounting use the credential or human actually presented. The issuer ID stays
separate from that credential. Management operations record an audit note on
the root org. Credential secrets are never logged.

### Webhooks, domains, MCP, and streams

- Child endpoints resolve to child environments and child deployments. An
  authenticated webhook needs a credential with matching scope. Ancestor keys
  are not substitutes. Anonymous routes and capability tokens resolve to one
  registered environment and cannot choose an org from request data.
- URL, custom domain, credential, endpoint, and deployment must agree. Domain
  and route caches are scoped and recheck current ownership.
- MCP discovery, invocation, transport sessions, message posts, reconnection,
  and stream replay follow the same rules. Transport sessions bind to the
  actual principal and effective scope.
- SSE replay and live pub/sub are protected independently. Subscription
  channels, cursors, terminal results, and blob rehydration are scoped.
- File uploads, multipart completion, downloads, signed object access, blob
  references, store search, and container file access check ownership. A valid
  storage identifier alone grants nothing.
- Outgoing workflow effects and alert deliveries keep their scope. Children do
  not inherit parent alert configuration.

### CLI and SDKs

Remote CLI commands with a child environment key need no new selector. Local
profiles select an org and environment through effective access, including
inherited root admin access. Existing SDK clients keep working with existing
environment-bound endpoints and keys. Management clients call only the
published management API with its separate credential.

## App

Routes carry the effective target. Cookies keep only the last navigation
choice. Server handlers validate route targets, memberships, and environment
ownership on every action, so stale tabs and conflicting cookies cannot send a
mutation to another org.

The app provides:

- An org and environment picker with a breadcrumb, including the root's own
  workspace.
- Child listing, search, provisioning, status, and lifecycle controls for root
  admins.
- Root member management with explicit child access grants. Inherited root
  administration shows separately from direct grants.
- Child operational pages for projects, builds, runs, events, tasks, schedules,
  contexts, keys, endpoints, domains, files, stores, and streams.
- Child settings that name the root billing owner. Child admins get no checkout
  or payment controls.
- A read-only overview of all children for usage, health, and activity, with
  child attribution on every row.

Changing scope resets selected resources, pagination, non-portable filters,
cached responses, and live subscriptions. Bulk operations name concrete targets
and authorize each one. An overview's read scope never reaches a mutation
handler.

## Migrations

PostgreSQL and SQLite use the same migration numbers.

| Migration | Contents |
| --- | --- |
| 012 org hierarchy | `parent_org_id`, `external_id`, and `lifecycle_state` on `org`; hierarchy triggers; idempotent child provisioning; root-only plans and invitations; child grant validation and revocation; the shared request-rate window; unique environment names within an org |
| 013 scope isolation | Upgrade preflight; immutable `call.env_id` with trace size reconciliation; `hot.apply_scope` and the scope helpers; RLS enabled, forced, and with policies on every protected table (PostgreSQL only); ownership triggers; management credential tables |
| 014 usage accounting | Task reservation columns; maintained storage, task, and run counters; the durable run admission ledger, backfilled from retained runs |

Every upgrade check runs first in 013, before any change. An inconsistent
ownership relationship stops the migration for a reviewed repair. The
migration never reassigns ownership automatically. Then 013 rewrites retained
`call` rows in one pass, before it installs RLS. 012 adds a unique index on
environment names within an org, so duplicate names must be repaired first.

Migrations are forward-only. Released migrations are never edited. Public
migrations hold the portable schema, policies, and generic reference data.
Deployment-specific data, such as plan catalogs, prices, and provider IDs,
stays out of them. See [the migration policy](../../resources/db/V2_MIGRATIONS.md).
Operator steps are in [RLS setup](../../resources/db/postgres/RLS_SETUP.md).

An application rollback keeps the policies, immutable ownership, and the
admission ledger. Recovery never disables RLS or retries failed queries through
an unrestricted pool. A policy repair is an explicit migration, not a silent
access expansion.

## Testing

Isolation tests use a root with its own environment, two children with
matching project, store, and file names, and an unrelated root. They exercise
allowed paths as well as denials, so RLS cannot look correct merely by
returning no data. PostgreSQL tests provision the schema with a privileged
setup connection, then run assertions as the restricted runtime login. They
cover missing scope, omitted filters, sibling reads and writes, pool reuse
after errors and cancellation, role qualification, the ownership graph, human
grants, credential chains, routing, streams and caches, runtime recovery,
non-SQL data, billing rollups, and populated upgrades on both databases.

## Deferred work

Shared deployments, shared releases, deployment fan-out, and fleet rollout
management need separate designs. A child with no deployment never loads a
parent build. Secrets are not shared by ancestry, and a run never uses another
org's deployment implicitly.

Also deferred: arbitrary-depth hierarchies, reparenting or promotion,
independent child subscriptions, child-only external users, team-based child
grants, cross-child runtime transactions, and any additional tenant dimension
inside an environment. Actor, resource owner, and billing owner stay separate
concepts, so later work can add explicit relationships without weakening
isolation.
