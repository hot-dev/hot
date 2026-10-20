---
description: "Root management credentials, sub-organization provisioning, and environment key isolation."
---

# Sub-organization management

A child organization owns its environments and deployments. Existing runtime API,
service, and session tokens continue to access exactly one environment. A root's
environment key cannot access a child environment. Missing selectors never mean
all children, and child lookups never fall back to parent resources.

Root administrators issue management credentials in the app. These tokens begin
with `hm_`, expire within 90 days, and are accepted only by `/v1/management/*`.
Send them as `Authorization: Bearer <management-token>`. Runtime tokens cannot
call management endpoints. Management credentials cannot issue other management
credentials or assign human access.

| Method and route | Required capability |
| --- | --- |
| `GET /v1/management/children` | `children:read` |
| `POST /v1/management/children` | `children:create` and explicit `all_children` |
| `PATCH /v1/management/children/{child_id}/state` | `children:update` for that child |
| `POST /v1/management/children/{child_id}/environments` | `environments:create` for that child |
| `POST /v1/management/children/{child_id}/environments/{env_id}/api-keys` | `runtime-keys:create` for that child and a delegation ceiling |
| `GET /v1/management/usage` | `usage:read` and explicit `all_children` |

Child creation accepts `name`, globally unique `slug`, optional `external_id`,
and a required `idempotency_key`. Retrying the same request with the same key
returns its original child/default environment; changing the request while
reusing the key is rejected. The response includes `org`, `env_id`, and `created`
inside the standard API response's `data` object. Children do not create billing
subscriptions. Environment provisioning accepts `name` and is idempotent within
that child by exact environment name. Lifecycle accepts `state` equal to
`active`, `suspended`, or `archived`.

A credential's target policy is either an explicit child-ID list or all children
of its root. It never crosses to another root. Listing accepts `limit` (maximum
500) and `offset`, and returns only authorized children. Account usage is an
explicit root aggregate; child runtime calls remain isolated.

Runtime-key issuance accepts `description` and the ordinary `permissions` map.
Requested permissions must fit the management credential's delegation ceiling.
The returned runtime token is bound to the specified child environment, and its
management issuer lineage is immutable. Service keys and sessions issued by that
API key remain subject to the whole issuance chain. Every request intersects
current ancestor ceilings with the presented credential's grants. Expiry,
revocation, and narrowing of an ancestor also restrict descendant credentials;
expansion never expands their stored grants.

Private API routes also enforce the effective permissions below. API-key status
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
unless revoked. Management-issued API keys retain their management dependency.
Long-lived streams close when authorization changes, checked at least every
30 seconds, and reconnect performs fresh authorization. Credential revocation
blocks new ingress and reads; already accepted work retains its original scope.
Organization suspension separately blocks new work admission and retries.

OpenAPI publishes the management routes and separate `management_auth` scheme.
Existing SDK runtime calls require no new selectors: use the child environment's
key. SDK 1.3.0 provides separate management clients for all five languages:
`HotManagementClient` in JavaScript, Python, Rust, and Java, and
`NewManagementClient` in Go. See the [SDK management guide](/docs/api/sdks#manage-sub-organizations)
for provisioning examples, async variants, and environment-bound runtime clients.
The management API can also be called directly with its separate credential.

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
when the migration is applied. The billing period start controls the monthly
window; accounts without a subscription use the calendar month. Local accounts
without a plan or feature overrides retain unlimited execution.

## Runtime usage visibility

`GET /v1/org/usage` keeps its existing usage, limits, percentages, and plan
fields, and identifies `env_id`, `usage_scope: "environment"`, and
`limits_scope: "billing_account"`. Consumption comes only from the credential's
environment, including admitted runs in the durable ledger. Limits and plan
metadata come from the root account. Percentages compare that environment's
consumption against the shared limits; they do not show account-wide consumption
or remaining capacity. The legacy `team_members` consumption field is zero
because directory seats do not belong to an environment. Account aggregates and
seat consumption require authorized management access. A plan lookup error
returns an error, never an unlimited plan.

Management usage reports `period_start` and `period_end` from the root's
subscription, with the calendar month as the fallback. Its `runs_this_period`
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
across processes. In-flight and SSE connection guards are local to each service
process.
