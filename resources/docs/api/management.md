---
description: "Org API keys, sub-organization provisioning, and environment key isolation."
---

# Sub-organizations

A child organization owns its environments and deployments. Environment API
keys, service keys, and sessions access exactly one environment. A root's
environment key cannot access a child environment. Missing selectors never mean
all children, and child lookups never fall back to parent resources.

## Org API keys

An **org API key** belongs to one root organization. It manages that
organization's children, their environments, and their API keys through
`/v1/orgs` and `/v1/api-keys`. It uses the same `hot_<id>_<secret>` format and
`Authorization: Bearer` header as an environment key, and the same permission
map, with qualified URNs.

An org key cannot call any other route. Runtime routes (events, runs, streams,
webhooks, MCP, files, sessions, service keys) answer `403` with the error code
`key_scope_mismatch`, and so do `/v1/orgs` routes called with an environment
key, service key, or session. To work inside an environment, an org key mints
an environment key there.

A root administrator creates the first org key on the organization's
**Children** tab in the app. It belongs to the organization and keeps working
if its creator leaves, until it expires or is revoked. Expiry is optional.

An org key can mint more keys:

- **Environment keys** in its own organization or a child. Their permissions
  must fit inside the org key's grants for that environment, and their expiry
  inside the org key's.
- **Org keys** in its own organization, with `org:{org}/api-key:*` → `create`.
  Their grants must fit inside the issuer's. A chain is at most two org keys
  deep.

A minted key remembers its issuer. Every request checks the whole chain: if
any key in it is revoked, expired, or narrowed, the keys below it lose that
access on their next request. Rotating a key changes only its secret, so
nothing below it is affected.

## Permissions

Org keys use qualified URNs. `{org}` and `{env}` are IDs or `*`, and `*` stands
for exactly one organization or environment.

| URN | Actions | Meaning |
| --- | --- | --- |
| `org:*` | `create`, `read`, `update` | Create children; read or change any child |
| `org:{org}` | `read`, `update` | One organization |
| `org:{org}/env:*` | `create`, `read` | Provision or list environments |
| `org:{org}/env:{env}` | `read` | One environment |
| `org:{org}/env:{env}/api-key:*` | `create`, `read`, `update`, `delete` | Mint, list, rotate, narrow, or revoke environment keys |
| `org:{org}/api-key:*` | `create`, `read`, `update`, `delete` | The same for org keys of the key's own organization |
| `org:{org}/usage:*` | `read` | Account usage (root organization only) |
| `org:{org}/env:{env}/{urn}` | that URN's actions | Any [environment permission](/docs/api#permissions) inside that environment |

Three rules keep these grants narrow:

- `org:*` means the key's children, never its own organization. Grant
  `org:{root}/…` explicitly to manage the root's own environments.
- `org:*` covers child organizations themselves, not what is inside them.
  `org:*` → `create` lets a key create children but not mint keys in them;
  that needs `org:*/env:*/api-key:*` → `create`.
- Grants inside an environment, such as `org:*/env:*/event:*` → `create`, set
  the ceiling for environment keys the org key mints. The org key itself still
  cannot publish events.

Environment keys keep today's unqualified grammar, and `org:` URNs are rejected
on them. A full-access `{"*:*": ["*"]}` environment key gains no org authority.

## Routes

All routes accept only org keys. IDs are UUIDs.

| Method and route | Required grant |
| --- | --- |
| `GET /v1/orgs` | Lists children the key may read (`org:*` or `org:{org}` → `read`) |
| `POST /v1/orgs` | `org:*` → `create` |
| `GET /v1/orgs/{org}` | `org:{org}` → `read` |
| `PATCH /v1/orgs/{org}` | `org:{org}` → `update` |
| `GET /v1/orgs/{org}/usage` | `org:{org}/usage:*` → `read` |
| `GET`, `POST /v1/orgs/{org}/envs` | `org:{org}/env:*` → `read`, `create` |
| `GET /v1/orgs/{org}/envs/{env}` | `org:{org}/env:{env}` → `read` |
| `GET`, `POST /v1/orgs/{org}/envs/{env}/api-keys` | `org:{org}/env:{env}/api-key:*` → `read`, `create` |
| `GET`, `POST /v1/orgs/{org}/api-keys` | `org:{org}/api-key:*` → `read`, `create` |
| `GET /v1/api-keys/{id}` | the key's qualified `api-key:{id}` URN → `read` |
| `PATCH /v1/api-keys/{id}` | `update`, and the caller issued the key |
| `DELETE /v1/api-keys/{id}` | `delete`, and the caller issued the key |
| `POST /v1/api-keys/{id}/rotate` | `update`, and the caller issued the key |
| `/v1/orgs/{org}/envs/{env}/projects…`, `/v1/orgs/{org}/envs/{env}/builds` | Today's project, context, and build grants, qualified by `org:{org}/env:{env}/` |

**Children.** `POST /v1/orgs` accepts `name`, globally unique `slug`, optional
`external_id`, and a required `idempotency_key`. Retrying with the same key
returns the original child and default environment with `created: false`;
reusing the key with different values is rejected. Children do not create
billing subscriptions. `PATCH /v1/orgs/{org}` accepts `state` equal to
`active`, `suspended`, or `archived`, for a child only. A suspended or archived
child can still be read and have its state changed; every other route under it
is refused.

**Environments.** `POST /v1/orgs/{org}/envs` accepts `name` and is idempotent
by exact name within that organization.

**API keys.** Minting accepts `description`, `permissions`, and an optional
`expires_at`. The token is returned once. `PATCH /v1/api-keys/{id}` replaces
the key's `permissions` with grants inside the caller's own.
`POST /v1/api-keys/{id}/rotate` accepts an optional `overlap_seconds` (default
86400, maximum 604800) during which the previous secret keeps working; the key
ID, its lineage, and the keys it issued do not change. Rotating, narrowing, or
revoking needs more than a grant: the caller must have issued the key, directly
or through keys it issued. Rotation returns the new secret, so a grant alone
would hand over another key's secret. No key rotates or revokes itself; a root
administrator can revoke any org key in the app.

**Projects, context, and builds.** Every `/v1/projects…` route and
`/v1/builds` is also served under `/v1/orgs/{org}/envs/{env}`. The handler is
the same one an environment key reaches, and it checks the org key's grants for
that environment, so `org:*/env:*/project:*` → `create` allows
`POST /v1/orgs/{org}/envs/{env}/projects`. Context values are write-only, as
they are for environment keys.

Every org key request is audited on the key's organization, with the key, its
issuer, the route, and the response status, and it counts against the account's
API request rate. Requests through a custom domain cannot use org keys.

## Environment API keys

Private API routes enforce the effective permissions below. API-key status
alone grants no administrative action. A full-access `{"*:*": ["*"]}` key keeps
its existing access; an event-only or MCP-only key cannot read secrets, upload
builds, administer files, or mint credentials. Missing policies deny access.

| API operation | Required resource and action |
| --- | --- |
| List/create projects | `project:*` with `read` / `create` |
| Read/update/delete one project | `project:{project_id_or_slug}` with `read` / `update` / `delete` |
| Activate/deactivate project | `project:{project_id_or_slug}` with `update`; activation also requires `build:*` with `execute` because it can redeploy |
| List builds, deployed/live build, schedules, event handlers | `build:*` with `read` |
| Upload build | `build:*` with `create` |
| Read/download/deploy one build | `build:{build_id}` with `read` / `read` / `execute` |
| List/create project context variables | `context:{project_id_or_slug}/*` with `read` / `create` |
| Update/delete context variable | `context:{project_id_or_slug}/{key}` with `update` / `delete` |
| List runs or run statistics | `run:*` with `read` |
| Read/subscribe to one run | `run:{run_id}` with `read` |
| List/create/revoke all sessions | `session:*` with `read` / `create` / `delete` |
| Revoke one session | `session:{session_id}` with `delete` |
| List/create/revoke all service keys | `key:*` with `read` / `create` / `delete` |
| Read/update/revoke one service key | `key:{service_key_id}` with `read` / `update` / `delete` |
| Read environment, environment subscription, or `/org/usage` | `env:{env_id}` with `read` |
| List/create domains | `domain:*` with `read` / `create` |
| Read/verify/delete domain | `domain:{domain_id}` with `read` / `update` / `delete` |
| List files | `file:*` with `read` |
| Read/download/delete file | `file:{file_id}` with `read` / `read` / `delete` |
| Upload or replace file by path | `file:{path}` with `create` |
| Initiate/write/complete multipart upload | `file:*` with `create` |
| Abort multipart upload | `file:*` with `delete` |
| Download blob reference | `blob:{blob_ref_id}` with `read` |

Suffix wildcards apply normally, so `env:*` covers the key's own environment and
`context:*` covers context variables within it. Project-specific URNs use the
same ID or slug as the request; URL parameters are decoded before checking.
Collection grants authorize the collection as a whole; an item grant cannot
list other items. The collection and item checks do not replace ownership checks.
Credential creation still requires requested descendant grants to be subsets.
Service keys and sessions remain excluded from these administrative routes,
even if their permission map names an administrative resource. Their existing
event, stream, task, webhook, and MCP authorization continues to apply.

User-issued API keys are organization-owned and survive removal of their creator
unless revoked. Keys an org key minted stay limited by that org key's current
grants. Long-lived streams close when authorization changes, checked at least
every 30 seconds, and reconnect performs fresh authorization. Credential
revocation blocks new ingress and reads; already accepted work retains its
original scope. Organization suspension separately blocks new work admission
and retries.

The SDKs do not wrap the org API yet; call it over HTTP. Runtime SDK calls need
no new selectors: use the child environment's key.

## Monthly run admission

The root account's `runs_per_month` applies across the root and every child.
API, webhook, MCP, event, scheduled, and task execution reserve capacity before
user code or a container starts. An event's selected handlers reserve together:
if the account cannot admit the whole group, no handler in that group starts.
Queued work remains pending when capacity is exhausted, and workers log the
admission failure and retry with backoff. Webhook and MCP calls also use the
queue: their wait can time out (HTTP 504 for webhooks, a JSON-RPC timeout for
MCP) while the accepted work remains pending and may execute after capacity
becomes available. Direct VM execution fails before running user code.

Reservations use durable run identities. Infrastructure redelivery of the same
run does not consume another slot. A run that was admitted still consumes its
slot if later setup or execution fails. Deleting retained run history, suspending
and resuming a child, or retrying admission does not reset account usage. The
admission ledger is separate from run history and is seeded with existing usage
when the migration is applied. Daily maintenance prunes ledger rows older than
400 days, longer than any billing period. The billing period start controls the monthly
window; accounts without a subscription use the calendar month. An account
without a plan has unlimited execution. If it has feature overrides but no
plan, any limit the overrides leave out stays unlimited.

## Runtime usage visibility

`GET /v1/org/usage` keeps its existing usage, limits, percentages, and plan
fields, and identifies `env_id`, `usage_scope: "environment"`, and
`limits_scope: "billing_account"`. Consumption comes only from the credential's
environment, including admitted runs in the durable ledger. Limits and plan
metadata come from the root account. Percentages compare that environment's
consumption against the shared limits; they do not show account-wide consumption
or remaining capacity. The legacy `team_members` consumption field is zero
because directory seats do not belong to an environment. Account aggregates and
seat consumption require an org key with `org:{root}/usage:*`. A plan lookup error
returns an error, never an unlimited plan.

Account usage (`GET /v1/orgs/{root}/usage`) reports `period_start` and
`period_end` from the root's subscription, with the calendar month as the fallback. Its `runs_this_period`
counts durable admissions across root and children, including task runs and
reservations; history retention cannot lower that count. Root billing views use
the same ledger and subscription period.

## Shared account capacity

Children share the root's environment, domain, schedule, seat, file, store,
trace, run, and compute limits. Sibling writes reserve capacity under the same
account lock. Replacing stored data accounts for the size difference rather
than charging both versions. Store and trace storage limits measure their
recorded logical row bytes; they are not a physical database disk limit or an
object-store bill. Blob storage remains separately attributed and retained.

Cancelling a container does not immediately release its capacity. The worker
first confirms termination and persists measured usage; unconfirmed cleanup
keeps the reservation for recovery. API request-per-second limits are shared
across processes, and org API requests count against them too. In-flight and
SSE connection guards are local to each service process.
