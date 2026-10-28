# Org-scoped API keys

Status: implemented on branch `claude/design-org-scoped-keys-6a137a`, base Hot 3.0.0
(`v3.0.0`, 552b582). [Implementation](#implementation) records where the build
differs from the proposal below and how each open question was settled.

This note replaces the separate sub-organization management plane with orgs
as ordinary API resources and one kind of API key. A key's **scope** says
where it lives: one org, or one environment in an org. Its **permissions**
say what it may do there, in the URN grammar Hot already uses. Orgs,
environments and API keys get ordinary routes. Runtime stays
environment-bound.

Related documents:

- [Sub-organizations and PostgreSQL RLS](sub-orgs-and-rls.md) ("the sub-org
  plan") is the design that shipped in 3.0.0.
- [Sub-organization management](../../resources/docs/api/management.md) is
  the public 3.0.0 contract this note replaces.
- Leela's starting proposal is `INTEGRATION.md` §2.1 in
  `leela-dev/plans/reglang/`. This note works it out and corrects it in
  several places (see [Changes to INTEGRATION §2.1](#changes-to-integration-21)).

## Implementation

What was built, and where it departs from the proposal that follows.

**Storage: a separate `org_api_key` table, not a `scope` column.** SQLite
cannot relax `api_key.env_id NOT NULL` without rebuilding the table and every
foreign key to it, and the 013 RLS policies on `api_key` all key off
`env_id`. A separate table leaves `api_key`, its policies and every existing
query untouched, and makes the compile-time split natural: `OrgApiKey` and
`ApiKey` are different types from different tables, so an environment code
path cannot load an org key at all. Users still see one kind of key: the same
`hot_<id>_<secret>` format, the same permission map, the same `/v1/api-keys`
routes. Lineage is `org_api_key.issuer_org_api_key_id` for org keys and
`api_key_issuer` for environment keys an org key minted; issuers are always
org keys. `api_key` gains only `expires_at` and the rotation columns.
Migrations: PostgreSQL `019_org_api_keys.sql`, SQLite `016_org_api_keys.sql`.

**Compatibility: removed, not aliased.** 3.0.0 shipped `/v1/management/*` hours
before this work, behind a `sub_orgs` limit that defaults to 0, with no
released SDK. The migration deactivates any runtime key an `hm_` credential
issued (it is never turned into an independent key), then drops
`management_key` and `api_key_management_issuer`. There is no `hm_` parsing,
no translation table, no deprecation window. §7 below describes the
alternative that was not taken.

**Org-qualified project routes: re-dispatch, not an `EnvTarget` refactor.**
`/v1/orgs/{org}/envs/{env}/projects…` and `…/builds` authorize the org key,
project its grants onto that environment, and re-dispatch the request to the
same `/v1/projects…` handlers as an environment principal holding those
projected grants. The existing `route_permissions` policy then applies
unchanged. No project, context or build handler changed. The environment
admin routes are defined once (`env_admin_routes` in `server.rs`) and serve
both paths.

**Errors.** A wrong-scope credential gets `403` with code `key_scope_mismatch`
on both route families. A path naming an org outside the key's root, or an
environment of a different org, is `404`.

**Code map.** `crates/hot/src/permission.rs` (qualified URNs, segment matching,
own-org rule, projection), `crates/hot/src/db/org_api_key.rs` (keys, chain,
minting, rotation), `crates/hot/src/db/api_key.rs` (expiry, previous secret,
rotation), `crates/hot_api/src/handlers/orgs.rs` (routes, auth middleware,
re-dispatch), `crates/hot_api/src/auth.rs` (chain-limited environment keys,
scope errors), `crates/hot_app/src/handlers/sub_orgs.rs` (App issuance).

**Open questions, as settled in the build:**

| # | Settled |
| --- | --- |
| Q1 | Segment-aware `org:`/`env:` selectors; inner URNs keep prefix matching |
| Q2 | `api-key:`; service keys keep `key:` |
| Q3 | One permission map; grants inside an environment are delegable only |
| Q4 | Optional expiry, no per-root cap |
| Q5 | No self-rotation or self-revocation |
| Q6 | Org keys only in root orgs; no child org keys |
| Q7 | Removed now (see above) |
| Q8 | No adoption; `hm_`-issued keys are deactivated |
| Q9 | Same `hot_` prefix |
| Q10 | Audit on the key's own org for every request (not the target org, as recommended); the target is in the path |
| Q11 | Not built: key minting has no idempotency key |
| Q12 | IDs only in routes and grants |
| Q13 | Environment keys an org key minted rotate through `/v1/api-keys/{id}/rotate`; no App rotate button yet |
| Q14 | Org keys revoke irreversibly (`revoked_at`); environment keys still use the reversible `active` toggle |
| Q15 | Projects, context, builds, schedules and event-handler reads |
| Q16 | At most two org keys in a chain; environment keys below either |

**Not built yet:** child-org usage (`/v1/orgs/{org}/usage` serves the root's
account aggregate only), `PATCH /v1/orgs/{org}/envs/{env}`, closing an open
stream when a rotation's overlap ends (it closes at reconnect), the SDK
changes (§8), and an App view of minted keys.

## Summary

| 3.0.0 | Proposed |
| --- | --- |
| Two credentials: environment API keys (`hot_`) and management credentials (`hm_`), with two auth schemes | One API key. Its `scope` is `org` or `env` |
| Capability names: `children:create`, `runtime-keys:create`, … | URNs: `org:*`, `org:{org}/env:*`, `org:{org}/env:{env}/api-key:*`, … |
| Target policy: a child list or `all_children` | Selectors in the URN: `org:*` or `org:{id}` |
| A stored delegation ceiling on the management credential | The issuing key's own grants are the ceiling |
| `/v1/management/children/…` | `/v1/orgs/…` and `/v1/api-keys/…` |
| Only a person issues an `hm_` credential; at most 90 days; it stops when its creator stops being a root admin; everything it issued dies with it | A person issues the first org key. Org keys are org-owned with optional expiry. Rotation keeps lineage and cascades nothing. Revocation, narrowing and expiry still cascade |
| `HotManagementClient` (unreleased SDK branches) | One `HotClient`; the key decides which calls succeed |

Three things in this note are not in §2.1 and matter most:

1. **`org:` wildcards must match one segment, not a string prefix.** With
   today's matcher, `org:*` → `create` would also grant
   `org:X/env:Y/api-key:*` → `create`: "create a child" would imply "mint keys
   in every child".
2. **API keys need their own resource type, `api-key:`.** Today `key:` means
   service keys.
3. **Grants on an org key that name runtime resources are delegable only.**
   An org key needs `org:*/env:*/event:*` → `create` to mint an environment
   key that can send events, but it can never send events itself. That is
   how the 3.0.0 delegation ceiling survives inside one permission map.

## Why change

- One grammar. Today a person must learn two vocabularies
  (`children:update` vs `project:{id}` → `update`) and two auth schemes for
  the same account.
- Ordinary resources. Creating a child, an environment or a key is a `POST`
  to a collection, with the same envelope, errors, pagination and
  idempotency as projects.
- Keys that rotate quietly. In 3.0.0 every runtime key issued by an `hm_`
  credential dies with it, within 90 days, and when its creator leaves the
  root admin role. A tenant platform like Leela would have to reissue every
  child's key at least quarterly, and on every staff change.
- The SDK management client has not shipped (SDK 1.3.0 is on branches), so
  the API shape is cheap to change now and expensive after.

## Agreement with the sub-org plan

The sub-org plan's boundaries stay. This note changes how control-plane
credentials are expressed, not what the isolation contract is.

| Sub-org plan decision | This note | Why |
| --- | --- | --- |
| One root and one level of direct children | Agrees | `org:*` means the key's org's direct children. Nothing reaches a grandchild or another root |
| Runtime credentials target exactly one environment | Agrees | Org keys are rejected by every runtime route ([§4](#4-runtime-stays-environment-bound)) |
| Missing selectors never mean all children; aggregates are explicit | Agrees | Every `/v1/orgs/{org}` route names its org. `org:*` is an explicit grant, not an absent one |
| No parent data inheritance | Agrees | An org key that reads a child reads it through a grant naming that child |
| Effective permission is the intersection with every ancestor's current grants; no caching | Agrees | Extended to a chain that can start at an org key ([§5](#5-delegation-lineage-and-rotation)) |
| Delegation authority is never inferred from general child management | Agrees, and §2.1 as written breaks it | Minting needs an explicit `api-key:` grant. The segment matcher keeps `org:*` from implying it |
| Editing or rotating a delegated key keeps its lineage | Agrees | Rotation keeps the key ID, and lineage is keyed by ID |
| Management credentials never assign human child access | Agrees | No key can touch members, invitations, child grants or billing |
| Control-plane acts get audit records on an org, not a fabricated environment | Agrees | One audit shape for every org-key request ([§1](#1-key-scope)) |
| "A management credential is a separate credential type, not an expanded environment API key" | **Disagrees, in part** | The plan's reason was that an environment code path must never receive a control-plane credential. This note keeps that guarantee with a compile-time split inside Hot (`EnvApiKey` vs `OrgApiKey`) while giving users one key, one table, one grammar and one auth scheme. A runtime handler cannot be handed an org key because no function returns one where an environment key is expected |
| Separate capability vocabulary and permission schema | **Disagrees** | Capabilities become URNs in the existing grammar |
| Stored delegation ceiling on the credential | **Disagrees, in form** | The ceiling is the issuer's own grant map. Runtime grants on an org key are the ceiling, delegable only |
| Only root-admin humans issue or change management credentials; a management credential cannot create another | **Disagrees, in part** | Only a root admin creates the first org key. After that, minting another org key needs an explicit grant (`org:{own}/api-key:*` → `create`) that is withheld by default |
| Expiry of at most 90 days | **Disagrees** | Expiry is optional, like user-issued keys. Open question [Q4](#open-questions-for-curt) asks whether an org may set a cap |
| A management credential stops working when its creator stops being a root admin | **Disagrees** | The plan already says user-issued API keys are org-owned and survive their creator. Org keys follow that rule. Credentials that silently die on a staff change break unattended provisioning; revocation is the explicit kill switch |
| Everything a management credential issued dies when it expires | Agrees, for expiry and revocation | Only rotation stops cascading |

## Changes to INTEGRATION §2.1

| §2.1 says | This note | Why |
| --- | --- | --- |
| `org:*` grants list/create children | `org:*` matches one org segment only | String-prefix matching would make `org:*` cover everything under every child |
| `org:{id}/env:{env}/key:*` for API keys | `…/api-key:*` | `key:` already means service keys |
| A URN without the prefix keeps today's meaning, on either kind of key | Unprefixed URNs are valid only on environment keys. Org keys store qualified URNs only | One string, one meaning. On an org key, `env:*` could mean "my org's environments" or "an environment I'm bound to" |
| Rotate/revoke via `key:{id}`, "within the issuer's lineage" | Qualified `api-key:` grant **and** the target is the caller's descendant | Rotation returns a new secret. Rotating someone else's key would hand it to you |
| `/v1/orgs/usage` | `/v1/orgs/{org}/usage` | Avoids a collision with `/v1/orgs/{org}`, and names which root's aggregate |
| `/v1/keys/{id}` | `/v1/api-keys/{id}` | Matches the URN type and `/v1/service-keys` |
| `usage:*` → `read` | `org:{org}/usage:*` → `read` | Org keys have no unprefixed grants |
| Leela's org key lists only control grants | It also needs delegable runtime grants (`org:*/env:*/event:*` → `create`, `…/run:*` → `read`) | A minted key's grants must be a subset of the issuer's |
| Only revocation or narrowing cascades | Expiry, and root or child suspension, also cascade | They are the same current-chain check |
| `/v1/management/*` stays "for one release" | Depends on [Q7](#open-questions-for-curt) | 3.0.0 published it; removing it in 3.x is a breaking change unless we call it experimental |

## The model

### Keys

An API key has one scope, fixed at creation:

- **Environment key** (`scope: env`). Bound to `(org_id, env_id)`. Exactly
  today's API key. It calls runtime routes and today's unqualified
  administrative routes in its own environment. It issues service keys and
  sessions.
- **Org key** (`scope: org`). Bound to `org_id`. It calls only control-plane
  routes: `/v1/orgs/…` and `/v1/api-keys/…`. It never issues service keys or
  sessions.

Both have the same token format, permission map, description, active flag,
creator attribution, optional expiry, and optional issuer.

### Qualified URNs

A qualified URN names a resource in an org, optionally in an environment:

```text
org:{o}                         the org itself
org:{o}/usage:*                 that org's usage (for a root: the account aggregate)
org:{o}/api-key:{k}             an org key of that org     ({k} may be *)
org:{o}/env:{e}                 an environment             ({e} may be *)
org:{o}/env:{e}/{inner}         a resource inside the environment
```

`{inner}` is any URN valid on an environment key today, plus `api-key:{k}`:
`project:*`, `context:{project}/*`, `build:*`, `event:*`, `run:*`, `*:*`, …
`{o}`, `{e}` and `{k}` are IDs or a single `*`. Stored grants use IDs only,
never slugs.

Matching:

1. A selector `*` matches exactly one segment. `org:*` matches `org:A` but
   not `org:A/env:E` or `org:A/env:E/project:p`.
2. A literal selector matches only itself. The resource `org:*` (the
   children collection) is matched only by the pattern `org:*`.
3. `{inner}` patterns match with today's `resource_matches`, so
   `org:A/env:*/*:*` covers everything inside every environment of `A`, and
   `org:A/env:E/context:p/*` covers project `p`'s context.
4. `*:*` alone is not a valid grant on an org key. A "full access" org key
   is an explicit list.

Which org `{o}` may name: the key's own org, or a direct child of it. A
check first resolves the target org's parent. A grant can never select an
org outside the key's root, whatever its ID.

### Actions on new resource types

| Resource | Actions | Meaning |
| --- | --- | --- |
| `org:*` | `create`, `read` | Create a child; list children |
| `org:{o}` | `read`, `update` | Read; change name or lifecycle state |
| `org:{o}/env:*` | `create`, `read` | Provision; list environments |
| `org:{o}/env:{e}` | `read`, `update` | Read; rename or deactivate |
| `org:{o}/api-key:*`, `org:{o}/env:{e}/api-key:*` | `create`, `read` | Mint; list keys |
| `org:{o}/api-key:{k}`, `org:{o}/env:{e}/api-key:{k}` | `read`, `update`, `delete` | Read; rotate or narrow; revoke |
| `org:{o}/usage:*` | `read` | Usage |

An org has no `delete`; archiving is `update` to `state: archived`, as in
3.0.0.

### Delegable-only grants

An org key may hold grants inside an environment that no org-key route
uses: `org:*/env:*/event:*` → `create`, `org:*/env:*/run:*` → `read`,
`…/mcp:*`, `…/stream:*`. They exist only as a ceiling for keys it mints.
The org key itself is rejected by the routes those grants describe. The App
shows them as "may grant to keys it creates".

This replaces the 3.0.0 `delegation_permissions` field. The alternative, a
separate `delegate` map on org keys, is [Q3](#open-questions-for-curt).

---

## 1. Key scope

### Change

- `api_key` gains `scope` (`env` | `org`), `org_id` (NOT NULL, backfilled
  from the environment), `expires_at`, and rotation columns
  ([§5](#5-delegation-lineage-and-rotation)). `env_id` becomes nullable with
  `CHECK ((scope = 'env') = (env_id IS NOT NULL))`. The 013
  `immutable_resource_scope` trigger extends to `scope` and `org_id`.
- In Rust, two types load from the table: `EnvApiKey` (today's `ApiKey`,
  `env_id: Uuid`) and `OrgApiKey` (`org_id`, no environment). Every existing
  query adds `scope = 'env'`. A missed query fails closed: sqlx cannot
  decode a NULL `env_id` into `Uuid`.
- `hot_api::auth` gets a `Principal` enum: `Env(AuthContext)` for today's
  three credential kinds, and `Org(OrgPrincipal)`. Runtime middleware
  accepts only `Env`. Control-plane middleware accepts only `Org`. Handlers
  extract the variant they need, so the type split is enforced at compile
  time.
- Every org-key request writes a control-plane audit record: presented key,
  its issuer chain, method, route, target org and environment, status. The
  record goes to the **target** org when the route names one, and to the
  key's own org for collection routes and usage. Root admins already read
  child notes. This generalizes the `management.request` /
  `management.response` notes in `management.rs`.
- Org keys count against the root's request rate limit, as `hm_` does.

### Files

- A new migration in each backend: `resources/db/postgres/migrations/019_org_api_keys.sql` and `resources/db/sqlite/migrations/016_org_api_keys.sql`.
- `crates/hot/src/db/api_key.rs`, a new `crates/hot/src/db/org_api_key.rs`
  (or one module with two types), `crates/hot/src/db/mod.rs`.
- `crates/hot_api/src/auth.rs`, `crates/hot_api/src/server.rs`.
- `crates/hot_app/src/handlers/keys.rs`, `crates/hot_app/src/handlers/sub_orgs.rs`,
  `crates/hot_app/templates/sub_orgs.html` and the keys templates.
- `resources/db/postgres/RLS_COVERAGE.md`.

### Risks

- **RLS.** Today's `api_key` policies key off `env_id`. Org rows have none.
  New policies: an org row is visible only under `Organization{org_id}` of
  its own org or `Bootstrap(Authentication)`, and writable only under
  `Organization{org_id}` writes. It is never visible through `ReadSet`, an
  environment scope, or a child's scope. Child admins must not see the
  root's org keys.
- **A missed `scope = 'env'` filter.** It fails at decode time, not open.
  Grep-based CI check: every `FROM api_key` query names `scope`.
- **Audit volume.** One or two rows per request, as `hm_` writes today. A
  provisioning burst writes many. Acceptable; the plan already accepted it.

### Tests

- Migration: existing keys backfill `scope = 'env'` and the right `org_id`;
  the check constraint rejects an org row with an `env_id` and vice versa;
  the trigger rejects scope or org changes. SQLite and PostgreSQL upgrade
  tests (`sqlite_upgrade.rs`, `postgres_upgrade.rs`).
- RLS (`postgres_rls.rs`): an org row is invisible under a child scope, a
  sibling root's scope, a `ReadSet` and every environment scope.
- Loading an org row through any `EnvApiKey` function fails.
- Audit: each org-key route writes a record with the target org; a denied
  request writes one too.

### Migration from 3.0.0

Additive. Existing keys and tokens are unchanged. No data moves except the
`org_id` backfill.

## 2. URN grammar

### Change

- `crates/hot/src/permission.rs` gains a parser for qualified URNs into
  `(org selector, env selector?, inner)` and a matcher with the
  one-segment rule above. Today's `resource_matches` handles only `inner`.
- `Permissions::validate_for(scope)`: environment keys reject `org:` URNs;
  org keys reject unprefixed URNs and bare `*:*`. Actions are checked
  against the table above and, for `inner`, against today's
  `valid_actions`.
- `Permissions::project(org, env) -> Permissions`: the part of an org key's
  grants that applies inside one environment, with the prefix stripped.
  `org:*/env:*/*:*` projects to `*:*`. This is how subset checks and
  intersections compare an org key with an environment key.
- `intersection` and `validate_subset_of` use the new matcher for qualified
  URNs.
- New resource type `api-key`. `key` keeps meaning service keys.

### Files

`crates/hot/src/permission.rs` (and its tests), the permission editor in
`crates/hot_app/src/handlers/keys.rs` and its template, the permissions
table in `resources/docs/api/management.md` (renamed, see §7).

### Risks

- **Prefix matching.** The escalation in the summary. The matcher must be
  segment-aware for selectors, and tests must pin it.
- **`*:*` environment keys.** Every full-access environment key would match
  any `org:` URN under today's matcher. Defense in depth: scope is checked
  before grants (an environment key never reaches a control-plane check),
  and `validate_for(env)` rejects `org:` URNs at issuance.
- **IDs vs slugs.** Today project URNs accept "the same ID or slug as the
  request". For orgs and environments, grants store IDs; routes may accept a
  slug, but the check runs on the resolved ID. Otherwise an archived child's
  freed slug could later select a different org.
- **Intersection with mixed forms.** The same grant set may need to
  intersect qualified and projected forms. Always project to one
  environment first, then intersect unprefixed sets with today's code.

### Tests

- Table tests: `org:*` matches `org:A`, not `org:A/env:E`,
  `org:A/api-key:*`, or `org:A/env:E/project:p`. `org:*/env:*` matches
  `org:A/env:E` only. `org:A/env:*/*:*` matches `org:A/env:E/build:b`, not
  `org:B/env:E/build:b`.
- Validation: an environment key with `org:*` is rejected; an org key with
  `project:*` or `*:*` is rejected; `org:*` with `execute` is rejected.
- Projection and subset: a minted `{"event:*": ["create"]}` in `(A, E)` is a
  subset of `org:*/env:*/event:*` → `create` and of `org:A/env:E/*:*` →
  `*`, and not of `org:B/env:*/*:*` → `*`.
- Property test: for random grant sets, `intersection` is a subset of both
  sides under the new matcher.

### Migration from 3.0.0

None. Stored environment-key grants keep today's meaning exactly. The
grammar only adds forms that were invalid before (today `org` is an unknown
type, so `validate` accepts any action on it; check that no stored key
already uses an `org:` URN before tightening).

## 3. Routes

### Change

All routes below accept org keys only.

| Route | Grant |
| --- | --- |
| `GET /v1/orgs` | `org:*` → `read`. Lists the key's org's children, filtered to those the key may read |
| `POST /v1/orgs` (`name`, `slug`, `external_id`, `idempotency_key`) | `org:*` → `create` |
| `GET /v1/orgs/{org}` | `org:{org}` → `read` |
| `PATCH /v1/orgs/{org}` (`name`, `state`) | `org:{org}` → `update` |
| `GET`, `POST /v1/orgs/{org}/envs` | `org:{org}/env:*` → `read`, `create` |
| `GET`, `PATCH /v1/orgs/{org}/envs/{env}` | `org:{org}/env:{env}` → `read`, `update` |
| `GET`, `POST /v1/orgs/{org}/envs/{env}/api-keys` | `org:{org}/env:{env}/api-key:*` → `read`, `create` |
| `GET`, `POST /v1/orgs/{org}/api-keys` | `org:{org}/api-key:*` → `read`, `create` |
| `GET`, `PATCH`, `DELETE /v1/api-keys/{id}` | the key's qualified URN → `read`, `update`, `delete`, and lineage ([§5](#5-delegation-lineage-and-rotation)) |
| `POST /v1/api-keys/{id}/rotate` | the key's qualified URN → `update`, and lineage |
| `/v1/orgs/{org}/envs/{env}/projects…` | `org:{org}/env:{env}/` + today's project URN and action |
| `/v1/orgs/{org}/envs/{env}/projects/{p}/context…` | `…/context:{p}/*` etc., today's actions |
| `/v1/orgs/{org}/envs/{env}/projects/{p}/builds…`, `…/deploy`, `…/activate` | `…/build:*`, `…/build:{id}`, today's actions |
| `GET /v1/orgs/{org}/usage` | `org:{org}/usage:*` → `read`. For the key's own root: the account aggregate. For a child: that child |

Org-qualified project, context and build routes reuse today's handlers.
Those handlers take the environment from the key today; they change to take
an `EnvTarget { org_id, env_id }` plus a permission-check closure. The
unqualified routes build the target from the environment key; the qualified
routes build it from the path after authorizing the org key. The database
scope for the handler body is `Environment { org_id, env_id }` of the
target, never a parent scope.

The first release covers what provisioning and deploy need: projects,
context, builds, deploy, activate. Files, domains, schedules and event
handler reads under `/v1/orgs/…` are follow-ups ([Q15](#open-questions-for-curt)).

Creation stays idempotent: `POST /v1/orgs` by `idempotency_key` as in
3.0.0; `POST …/envs` by exact name within the org. `POST …/api-keys` takes
an optional `idempotency_key`. A retry with the same key returns the key's
metadata **without its secret** and a `secret_returned: false` flag; the
caller then rotates. Hot never stores a plaintext secret to replay it
([Q11](#open-questions-for-curt)).

### Files

New `crates/hot_api/src/handlers/orgs.rs` and
`crates/hot_api/src/handlers/api_keys.rs`; `projects.rs`, `context.rs`,
`builds.rs` (`EnvTarget` refactor); `server.rs`; `openapi.rs`; new
`resources/docs/api/orgs.md`; `resources/docs/api/sdks.md`.

### Risks

- **Handler refactor.** The unqualified routes must behave byte-for-byte as
  today. The refactor is the largest code change in this note.
- **Two path parameters that must agree.** `{env}` must belong to `{org}`;
  `{p}` must belong to `{env}`; a build must belong to `{p}`. Reject any
  mismatch with 404, never a fallback. This is the plan's "related ID is
  valid only when its ownership matches" rule.
- **Org lifecycle.** A suspended or archived child: `GET` and `PATCH
  /v1/orgs/{org}` still work (to reactivate). Every other route under it
  refuses, as `authorize_child(…, require_active)` does today.
- **Root's own environments.** A root org key may manage the root's own
  environments with `org:{root}/env:…` grants. That is intended, but those
  grants must be explicit; `org:*` never includes the key's own org.

### Tests

- One test per route for allow, deny without grant, deny with a grant on a
  sibling, deny for an environment key, deny for a service key or session.
- Mismatch tests: `{env}` of another child, project of another environment,
  build of another project, a child of another root.
- The unqualified project, context and build tests run unchanged.
- Idempotency: repeated `POST /v1/orgs` returns the same child; changed body
  with the same key is rejected; repeated key mint returns no secret.
- Suspended child: only read and lifecycle update succeed.

### Migration from 3.0.0

Additive. `/v1/management/*` keeps working ([§7](#7-compatibility)).

## 4. Runtime stays environment-bound

### Change

- `authenticate_token_inner` in `auth.rs` rejects org keys exactly as it
  rejects `hm_` today. So do webhook authentication, MCP, stream and SSE
  subscription, `/v1/env`, `/v1/org/usage`, and every unqualified
  administrative route.
- Requests that arrived through a custom domain (`ResolvedDomain`) reject
  org keys, as `management_auth_middleware` does.
- Org keys cannot issue service keys or sessions. A trigger on `session`
  and `service_key` rejects a parent `api_key` whose scope is `org`.
- Error: `403` with code `key_scope_mismatch` and a message naming the
  scope the route needs, so an SDK can say "this call needs an environment
  key".

### Files

`crates/hot_api/src/auth.rs`, `handlers/webhook.rs`, `handlers/mcp.rs`,
`handlers/streams.rs`, `handlers/events.rs`, the migration (triggers),
`crates/hot_api/src/models.rs` (`ApiError`).

### Risks

- **"Runtime stays environment-bound" is hygiene, not containment.** An org
  key that can mint environment keys with `event:*` → `create` can get
  runtime access to any child it can mint in. An org key that can upload and
  deploy builds can run code in the child, and that code reads the child's
  context and store. The boundary keeps an org key from being used
  *directly* at runtime and keeps runtime audit attributable to one
  environment. It does not shrink what a leaked org key with those grants
  can reach. See [Blast radius](#blast-radius-of-a-leaked-org-key).
- **A route forgotten.** Any route that accepts "a valid API key" without
  the `Env` variant. The `Principal` split makes this a compile error for
  handlers that extract `AuthContext`; middleware is the remaining surface.

### Tests

- For every runtime and unqualified route in `openapi.rs`, an org key with
  `org:*/env:*/*:*` → `*` gets `403 key_scope_mismatch`. Generate the list
  from the OpenAPI paths so a new route is covered by default.
- Custom-domain requests with an org key are rejected.
- Creating a service key or session with an org key fails, at the handler
  and at the trigger.

### Migration from 3.0.0

None. `hm_` is already rejected by these routes.

## 5. Delegation, lineage and rotation

### Change

**Lineage.** A new immutable table `api_key_issuer (api_key_id PK,
issuer_api_key_id NOT NULL, created_at)`, both `REFERENCES api_key ON
DELETE RESTRICT`, with the 013 immutability trigger. A human-issued key has
no row. The 3.0.0 `api_key_management_issuer` stays for `hm_`-issued keys
until [§7](#7-compatibility) ends.

**Minting.** A key mints a key only if:

1. it holds the collection grant (`org:{o}/env:{e}/api-key:*` or
   `org:{o}/api-key:*` → `create`);
2. the new key's grants, qualified to its scope, are a subset of the
   issuer's current effective grants. For an environment key in `(o, e)`
   that means `new ⊆ issuer.project(o, e)`;
3. the new key's `expires_at` is not later than the issuer's effective
   expiry (the earliest in its chain), when the issuer expires;
4. the chain depth stays within a limit (proposed: 3 keys, so human → org
   key → org key → environment key; service keys and sessions sit under
   that).

**Effective permissions.** On every request, for a key with an issuer:
`effective = own ∩ issuer.effective`, projected to the key's scope. The
walk checks each ancestor's active flag, expiry, and its org's admission
state. This generalizes `ManagementKey::effective_runtime_permissions`,
which `AuthContext::revalidate` and `authenticate_token_inner` already call
on every request and stream revalidation. Service keys and sessions keep
intersecting with their API key's (now chain-derived) grants.

**What cascades.** Revoking, deactivating, narrowing or expiring a key
restricts every descendant on its next request (streams within 30 seconds).
Suspending the root stops every org key in it; suspending a child stops
every key scoped to it. Widening an issuer never widens a descendant past
its stored grants.

**Rotation.** `POST /v1/api-keys/{id}/rotate` (`overlap_seconds`, default
86400, maximum 7 days) issues a new secret for the **same key ID**. The old
secret's hash moves to `previous_key_data` with `previous_expires_at`. Both
verify until then; rotating again during the overlap replaces the previous
secret. Lineage, grants, expiry and descendants are untouched because they
hang on the ID. Rotation is audited.

**Who may revoke, narrow or rotate a key.** A key may act on a key only if
the target is its strict descendant in `api_key_issuer` **and** it holds the
target's qualified `api-key:` grant. A root admin (in the App) may act on
any key in the root and its children. Child admins may act on keys in their
child. A key may not rotate itself in the first release ([Q5](#open-questions-for-curt)).

**Narrowing.** `PATCH /v1/api-keys/{id}` with `permissions` sets new stored
grants that must be a subset of the caller's effective grants (projected),
like minting. A root admin may set any grants; effective grants still
intersect with the issuer chain.

### Files

The migration (lineage table, rotation columns, triggers);
`crates/hot/src/db/api_key.rs` and `org_api_key.rs`; `crates/hot/src/permission.rs`;
`crates/hot_api/src/auth.rs` (chain walk; the secret generation in the
principal); `handlers/api_keys.rs`; `handlers/service_keys.rs`,
`handlers/sessions.rs` (issuer intersection unchanged but now chain-based);
`crates/hot/src/db/management_key.rs` (the chain walk treats an `hm_`
ancestor as one more link during §7).

### Risks

- **Rotation as theft.** Rotation hands the caller a working secret. That is
  why it needs descent, not just a grant: a sibling provisioning key with
  `org:*/env:*/api-key:*` → `update` must not be able to rotate Leela's
  server key and receive its secret.
- **Lineage laundering.** No route changes `issuer_api_key_id`. Rotation and
  narrowing keep it. Deleting a key with descendants is blocked by the
  foreign key, as `api_key_management_issuer` does today.
- **Overlap and open streams.** A stream opened with the previous secret
  must close once the overlap ends. The principal records which secret
  generation authenticated it; revalidation fails after
  `previous_expires_at`.
- **Revocation as an outage.** Revoking a key kills everything it minted.
  If Leela's single server key leaks, revoking it revokes every child's
  environment key. That is correct security behavior and an availability
  event; Leela needs a runbook (mint a new org key, re-mint every child
  key). [Q8](#open-questions-for-curt) asks whether a root admin may adopt
  descendants under a new issuer.
- **Reversible deactivation.** Today `active` toggles both ways in the App.
  Re-enabling an issuer revives its descendants. Recommendation: keep the
  toggle and add an irreversible `revoked_at` ([Q14](#open-questions-for-curt)).
- **Per-request cost.** A chain of depth 3 adds up to three key lookups and
  their org admission checks per request, uncached, as the plan requires.
  The 3.0.0 `hm_` walk already pays one level.
- **Org-key-minted org keys.** A key with `org:{own}/api-key:*` → `create`
  can mint a peer that also holds it, and so on to the depth limit. Every
  link is in the chain and revocable from the top, and the App shows the
  tree.

### Tests

- Subset: minting wider than the issuer fails, per resource and per action,
  including `*` actions and `*:*` inner grants.
- Expiry: a minted key cannot outlive an expiring issuer; an issuer's expiry
  stops descendants.
- Cascade: revoke, deactivate, narrow and expire an org key; its environment
  key, that key's service keys and sessions all lose access on the next
  request and within 30 seconds on an open stream. Widen it again;
  descendants stay at their stored grants.
- Rotation: old and new secrets work during the overlap; the old one fails
  after; descendants keep working throughout; `api_key_issuer` is unchanged;
  a stream opened on the old secret closes after the overlap.
- Lineage authority: a sibling key with the right grant cannot rotate,
  narrow or revoke a non-descendant; the ancestor can; a key cannot rotate
  itself.
- Depth limit: the fourth level fails.
- Suspension: suspending the root, or the child, stops the corresponding
  keys and their descendants; reactivation restores them.

### Migration from 3.0.0

Additive. User-issued environment keys have no issuer row and behave as
today. `hm_`-issued keys keep their `api_key_management_issuer` link and
die with their `hm_` credential ([§7](#7-compatibility)).

## 6. Ownership and the first org key

### Change

- Only a person who is a root admin (`is_admin && can_manage_account`, the
  check `require_root_admin` uses today) creates an org key in the App. The
  App page that issues `hm_` credentials today (`sub_orgs.rs`,
  `create_management_key`) issues org keys instead, with a grant editor and
  presets ("provision children", "deploy into children", "mint child
  environment keys").
- An org key belongs to the org. `created_by_user_id` is attribution. It
  keeps working after its creator leaves or changes role, like a
  user-issued key in 3.0.0. This drops the `validate_root` creator check
  for org keys.
- Expiry is optional. The App suggests one.
- An org key can mint another org key only with `org:{own}/api-key:*` →
  `create`, which no preset includes.
- No key can manage members, invitations, child human-access grants,
  billing, subscriptions or plan features. There are no URNs for them.
- In the first release, org keys exist only in root orgs. A child cannot
  hold an org key and a parent cannot mint one in a child ([Q6](#open-questions-for-curt)).

### Files

`crates/hot_app/src/handlers/sub_orgs.rs`, `crates/hot_app/templates/sub_orgs.html`,
`crates/hot_app/src/handlers/keys.rs`, `crates/hot/src/db/org_api_key.rs`.

### Risks

- **Orphaned powerful keys.** Without the creator check, a departed admin
  who copied an org key keeps a working credential. That is already true of
  user-issued environment keys. Mitigations: optional expiry, the key list
  showing last use, revocation, and [Q4](#open-questions-for-curt) (an org
  policy capping org-key lifetime).
- **Admins can widen an org key's stored grants.** Its descendants still
  intersect with their own stored grants, so widening the parent never
  widens a child key.

### Tests

- A non-admin root member, a child admin, and an admin of another root
  cannot create an org key.
- An org key keeps working after its creator is removed or demoted.
- An org key without `org:{own}/api-key:*` cannot mint an org key; with it,
  it can mint only a subset.
- A child org cannot hold an org key (constraint and handler).

### Migration from 3.0.0

The App stops issuing `hm_` credentials in the release that ships org keys.
Existing `hm_` credentials keep working until they expire or are revoked.

## 7. Compatibility

### Change

`hm_` tokens and `/v1/management/*` keep working, implemented on top of the
new model:

- `ManagementKey::verify_token` builds an in-memory `OrgPrincipal` whose
  grants are translated from its capabilities. Its 90-day expiry and
  creator check stay; they are part of what was issued.
- `/v1/management/*` handlers become thin wrappers over the `/v1/orgs`
  handlers, and accept both `hm_` and org keys. `/v1/orgs/*` also accepts
  `hm_`. Both families respond with `Deprecation` and `Sunset` headers on
  the management routes.
- OpenAPI keeps `management_auth` marked deprecated and adds the new routes
  under the ordinary bearer scheme.

Translation, per target `t` (each listed child, or `*` when `all_children`):

| 3.0.0 capability | Grants |
| --- | --- |
| `children:read` | `org:*` → `read` if `all_children`; else `org:{t}` → `read` per target (listing returns only those) |
| `children:create` (requires `all_children`) | `org:*` → `create` |
| `children:update` | `org:{t}` → `update` |
| `environments:create` | `org:{t}/env:*` → `create` |
| `runtime-keys:create` | `org:{t}/env:*/api-key:*` → `create`, plus each `delegation_permissions` grant `g` as `org:{t}/env:*/{g}` (delegable only) |
| `usage:read` (requires `all_children`) | `org:{root}/usage:*` → `read` |

The translation must never be wider than 3.0.0. In particular,
`children:read` with a target list must not become `org:*`.

### Files

`crates/hot/src/db/management_key.rs`, `crates/hot_api/src/handlers/management.rs`,
`crates/hot_api/src/openapi.rs`, `resources/docs/api/management.md`
(rewritten as the org and key reference, with a compatibility section).

### Risks

- **Semantic drift.** Two route families over one handler set can disagree
  in error codes or response envelopes. The 3.0.0 management response
  shapes (`{children, total}`, `{org, env_id, created}`) stay on the
  management routes.
- **Semver.** 3.0.0 published `/v1/management/*` as stable. Removing it in
  3.1 or 3.2 breaks semver unless we now call it experimental
  ([Q7](#open-questions-for-curt)).
- **Mixed chains.** During the compatibility window an environment key may
  descend from an `hm_` credential. The chain walk must treat that ancestor
  with its 3.0.0 rules (90 days, creator check) and its translated grants.

### Tests

- The existing management tests (`management.rs`,
  `management/postgres_lifecycle.rs`, `auth.rs`
  `management_ancestor_limits_runtime_keys_and_grandchildren`) pass
  unchanged.
- For each capability and target policy, the translated principal is
  allowed exactly the 3.0.0 operations and denied the rest, on both route
  families.
- An `hm_` credential still dies with its creator's admin role and at
  expiry, and its runtime keys die with it.
- Deprecation headers appear on management routes only.

### Migration from 3.0.0

1. Release N (org keys): schema, routes and the App switch to org keys. The
   App issues no new `hm_` credentials. Existing ones keep working.
2. N + 90 days: every `hm_` credential has expired, since none lived longer
   than 90 days. Runtime keys they issued have died with them; their owners
   have re-minted under org keys (see [Q8](#open-questions-for-curt) for
   a conversion path).
3. Removal of `/v1/management/*`, `hm_` parsing and the management tables,
   in the release [Q7](#open-questions-for-curt) picks.

Self-hosted operators need nothing beyond the migration.

## 8. SDKs

### Change

The five SDK repositories carry `HotManagementClient` (`NewManagementClient`
in Go) on unreleased branches `codex/sub-org-management-sdk` in `hot-js`,
`hot-python`, `hot-rust`, `hot-java` and `hot-go`. Before SDK 1.3.0 ships:

- Delete the management client. Add resources to the normal client:
  `client.orgs.list/create/get/update`, `client.orgs.envs(org).list/create`,
  `client.orgs.apiKeys(org)` and `client.orgs.envs(org).apiKeys(env)` for
  list and create, `client.apiKeys.get/update/revoke/rotate`,
  `client.orgs.envs(org).projects(env)…` for projects, context, builds and
  deploy, `client.orgs.usage(org)`. Naming follows each language's idiom.
- Map `403 key_scope_mismatch` to a typed error that says which scope the
  call needs.
- Keep the 1.3.0 runtime additions on those branches (proxy, stream and
  task changes) unchanged.
- Update the SDK contract fixtures (`openapi-operations.json`) and the
  integration fixture (`integration/suborg_fixture.py`) to provision with
  an org key.
- Hot's own docs (`resources/docs/api/sdks.md` and `management.md`) already
  describe `HotManagementClient` for SDK 1.3.0. They change with the API.

### Risks

- A client that mixes org and environment calls in one object fails at
  runtime, not at compile time. The typed error and docs make the failure
  obvious. Typed per-scope clients are the alternative; they reintroduce
  the split this note removes.
- Five repositories have to move together. The OpenAPI fixture test in each
  catches drift.

### Tests

Per SDK: unit tests for every new method's path and body; the integration
fixture provisions a child, an environment, a project and a key with an org
key, then runs events with the minted environment key; a runtime call with
the org key raises the typed scope error.

### Migration

None for users: no SDK with the management client has shipped.

---

## Blast radius of a leaked org key

Leela's proposed server key (corrected from §2.1):

```json
{
  "org:*": ["create", "read", "update"],
  "org:*/env:*": ["create", "read"],
  "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
  "org:*/env:*/project:*": ["create", "read", "update"],
  "org:*/env:*/context:*": ["create", "read", "update", "delete"],
  "org:*/env:*/build:*": ["create", "read", "execute"],
  "org:*/env:*/event:*": ["create"],
  "org:*/env:*/run:*": ["read"]
}
```

If it leaks, the holder can, in every Leela child: create, suspend and
archive children; create environments; overwrite context (for example the
callback URL and secret), though not read existing context values; upload
and deploy a build, which runs their code with that child's context and
store; and mint environment keys with event and run access. That is full
compromise of every tenant. They cannot reach another Hot root, the root's
own environments (no `org:{root}/…` grant), billing, members, or org-key
minting.

The 3.0.0 design has the same reach for the same job: Leela would need an
`hm_` credential with `runtime-keys:create` and a ceiling covering builds,
context and events. What differs is lifetime (90 days and the creator's
role vs optional expiry) and recovery (both cascade on revocation).

Ways to shrink it, in order of value:

1. Split keys by job. A provisioning key (`org:*`, `env:*`, `api-key:*`)
   in Leela's server; a deploy key (`project`, `context`, `build`) in the
   build worker. Neither alone has everything. Environment keys minted by
   the provisioning key survive revocation of the deploy key.
2. Expiry plus scheduled rotation by a human or an ancestor key.
3. Alerts on mint, deploy and lifecycle audit records.
4. Later: source IP allowlists on org keys (out of scope here).

## Order of changes

Each step is reviewable alone and keeps 3.0.0 behavior until the routes
open.

1. `permission.rs`: qualified URNs, segment matcher, projection,
   `validate_for(scope)`, `api-key` type. No callers yet.
2. The migrations (PostgreSQL 017, SQLite 015) and the `EnvApiKey` / `OrgApiKey` split, `scope = 'env'`
   on every query, RLS policies and tests.
3. `api_key_issuer`, the generalized chain walk in `auth.rs` (with `hm_`
   ancestors), the `Principal` enum, org-key rejection on runtime routes.
4. `/v1/orgs`, envs and api-key routes; rotation; audit.
5. `EnvTarget` refactor and org-qualified project, context and build
   routes.
6. `hm_` translation and `/v1/management/*` as wrappers; deprecation.
7. App: org-key issuance and the key tree; stop issuing `hm_`.
8. Docs and OpenAPI; then the five SDKs.

## Open questions for Curt

1. **Segment-aware `org:` wildcards.** `*` in an `org:`/`env:` selector
   matches one segment, while `inner` keeps today's prefix matching. OK?
   (Recommended. The alternative leaves §2.1's escalation in place.)
2. **`api-key:` as the type name.** Or rename service keys' `key:`? Renaming
   would break stored grants. (Recommend `api-key:`.)
3. **Delegable-only grants vs a separate ceiling.** One map, where runtime
   grants on an org key are delegable only, or a `delegate` map beside
   `permissions`? (Recommend one map; the App labels them.)
4. **Org-key expiry.** Optional with no cap, as you said, or may a root set
   a maximum lifetime for its org keys? (Recommend optional, plus an
   optional per-root cap later.)
5. **Self-rotation.** May a key rotate itself? Useful for unattended
   rotation of the top key; a thief can also use it to lock the owner out.
   (Recommend no in the first release; humans or ancestors rotate.)
6. **Org keys in children.** Can a child hold an org key, or a parent mint
   one in a child, for self-service child admins? (Recommend defer.)
7. **When `/v1/management/*` goes.** 3.0.0 published it as stable. Options:
   remove in 4.0; or document it as experimental now and remove it once
   every `hm_` has expired. (Your call: Leela is the only known caller.)
8. **Adopting descendants.** After revoking a key, may a root admin move its
   descendants under a new issuer (audited, subset-checked), or convert
   `hm_`-issued runtime keys? It saves a mass re-mint and is a laundering
   path if misused. (Recommend no for now; re-mint.)
9. **Token prefix.** Same `hot_` prefix for both scopes, or a distinct one
   so secret scanners can rank an org key higher? The server never trusts
   the prefix either way. (Recommend same prefix.)
10. **Audit placement.** Write org-key audit to the target org (child admins
    see what the parent did) or always to the key's org? (Recommend target
    org, key's org for collections and usage.)
11. **Idempotent key minting.** Return metadata without the secret on a
    retry (caller rotates), or no idempotency on mint? (Recommend the
    former.)
12. **Slugs in routes.** Accept org slugs in `/v1/orgs/{org}` paths
    (resolved to IDs before checks), or IDs only? (Recommend both, checks
    on IDs.)
13. **Environment keys rotating.** Should user-issued environment keys get
    `rotate` too (by a human in the App, or by an ancestor)? Cheap once
    rotation exists. (Recommend yes, App only.)
14. **Revocation vs deactivation.** Keep the reversible `active` toggle and
    add an irreversible `revoked_at`? (Recommend yes.)
15. **Org-qualified route coverage.** First release covers projects,
    context, builds and deploy. Add files, domains, schedules and event
    handler reads now or on demand? (Recommend on demand.)
16. **Delegation depth.** Three keys deep (human → org key → org key →
    environment key)? (Recommend three.)
