# microsoft-graph

The Microsoft Graph v1.0 foundation for Hot, shared by the `microsoft-*` packages ([onedrive](../microsoft-onedrive), [sharepoint](../microsoft-sharepoint)). It resolves Entra ID credentials from context or per call (delegated auth code + PKCE with rotating refresh tokens, app-only with a client secret or a certificate, or a ready access token), mints and caches tokens for the run, targets national clouds, builds OData queries, follows `@odata.nextLink`, sends JSON batches, turns Graph errors into readable `HttpError`s (and tells you which are worth retrying), and manages change-notification subscriptions with webhook validation.

You rarely call it directly for files: the service packages use it for every request. Reach for it to run the sign-in flow, to act for many users or tenants from one app, to subscribe to change notifications, or to call any Graph endpoint that has no package yet.

## Install

Add it to your project's deps in `hot.hot` (the service packages pull it in for you):

```hot
hot.project.my-app.deps {
  "hot.dev/microsoft-graph": "0.1.1"
}
```

It depends on [`hot.dev/jwt`](../jwt) and [`hot.dev/oauth2`](../oauth2) (token grants and PKCE).

## Authentication

Register an app in the [Entra admin center](https://entra.microsoft.com) (App registrations), then set **one** credential in Context Variables. They are checked in this order:

| Credential | Context variables | Use it for |
|---|---|---|
| Access token | `microsoft.access.token` | A token you obtained and refresh yourself |
| Delegated refresh token | `microsoft.oauth.refresh-token`, `microsoft.client.id` (+ `microsoft.client.secret` for a web app) | Acting as one user who signed in once (see [Sign-in flow](#sign-in-flow)) |
| App-only, certificate | `microsoft.client.private-key` (PEM), `microsoft.client.certificate` (PEM) or `microsoft.client.thumbprint` (SHA-1 hex), `microsoft.client.id`, `microsoft.tenant.id` | Daemons and background sync; the recommended app credential |
| App-only, client secret | `microsoft.client.secret`, `microsoft.client.id`, `microsoft.tenant.id` | Daemons, when a certificate is not practical |

Other context variables:

| Context Variable | Secret | Description |
|---|---|---|
| `microsoft.tenant.id` | no | Tenant GUID or domain (`contoso.onmicrosoft.com`); for delegated sign-in also `common` (default), `organizations` or `consumers`. App-only needs a real tenant |
| `microsoft.scopes` | no | Scopes delegated refresh tokens are redeemed for (default `https://graph.microsoft.com/.default`, i.e. everything the user consented) |
| `microsoft.cloud` | no | `global` (default), `us-gov` (GCC High), `us-dod`, `china` (21Vianet) |
| `microsoft.graph.url`, `microsoft.login.url` | no | Override the Graph or login root (a proxy, or a cloud without a preset) |
| `microsoft.graph.client-state` | yes | Shared secret checked by `::microsoft::graph::subscriptions/verify-notifications` |

The client id, certificate, thumbprint, tenant and cloud settings are marked `secret: false`, so they show in call logs; secrets, private keys and tokens do not. A missing client id, or an app-only credential on `common`, is rejected before any network call.

### Per-call credentials (multi-tenant)

Every function in this package and in the service packages has an options arity taking an `opts` Map. Credentials and settings in `opts` override context for that call only:

| `opts` keys | Credential or setting |
|---|---|
| `token` | Access token |
| `refresh-token`, `client-id` (+ `client-secret` or `private-key`) | Delegated refresh token |
| `client-id`, `client-secret`, `tenant-id` | App-only, client secret |
| `client-id`, `private-key`, `certificate` or `thumbprint` / `thumbprint-sha256`, `tenant-id` | App-only, certificate |
| `tenant-id`, `cloud` | Target another tenant or national cloud with the context's app |
| `graph-url`, `login-url` | Another Graph or login root, such as a proxy. Needs a credential in the same `opts`: context credentials are never sent to a per-call URL |
| `scopes` | Delegated scopes for this call |
| `api-version` | `"beta"` instead of `"v1.0"` |
| `headers` | Extra request headers |
| `client-request-id` | Your own request id (`false` sends none) |
| `retry` | `false` to skip the one immediate retry of a failed connection |

```hot
::users ::microsoft::graph::users

// One user of many: their stored refresh token, the app identity from context
alex ::users/me({refresh-token: "0.AXoA-alex-refresh-token"})

// Another customer tenant, app-only with the context's certificate
people ::users/list-users({tenant-id: "fabrikam.onmicrosoft.com", select: ["id", "mail"]}, 100)
```

When `opts` carries a primary credential (`token`, `refresh-token`, `client-secret` or `private-key`), context user and app credentials are ignored for that call; only the app identity (`client-id`, `tenant-id`, `cloud`) is still read from context when `opts` leaves it out, and a per-call refresh token borrows the context's client secret or certificate for the same app. Per-call credentials are never written to the run's context, so parallel flows and tenants cannot see each other's, and they are masked in call records.

A credential key (`token`, `refresh-token`, `client-id`, `client-secret`, `private-key`) that is present but blank or null is an error naming the key, never a fallback to context, so a tenant whose token is missing does not run on the project's credentials. `graph-url` or `login-url` without `token`, `refresh-token`, `client-secret` or `private-key` in the same `opts` is an error naming both, and a per-call refresh token with a per-call URL borrows nothing from context. `::microsoft::graph::api/resolve(opts)` is the one place this is decided.

A full URL passed as `path`, such as an `@odata.nextLink`, gets credentials only over https on the call's Graph root or a Microsoft Graph cloud host (`graph.microsoft.com`, `graph.microsoft.us`, `dod-graph.microsoft.us`, `microsoftgraph.chinacloudapi.cn`). Plain http is allowed only on the host of a Graph root that is itself http, for tests and local proxies. Anything else, including a URL with userinfo, is an error naming the host, and nothing is sent or minted.

### Certificates

`microsoft.client.private-key` is the PEM private key (PKCS#8 `BEGIN PRIVATE KEY` or PKCS#1 `BEGIN RSA PRIVATE KEY`); upload the matching certificate under **Certificates & secrets**. The package signs an RS256 client assertion (Entra documents PS256; RS256 is accepted and is what MSAL sends with a SHA-1 thumbprint) with `x5t` (SHA-1) and, when it knows it, `x5t#S256` (SHA-256), computed from `microsoft.client.certificate` or taken from `microsoft.client.thumbprint` (the hex thumbprint the portal shows).

### Sign-in flow

Delegated access uses the authorization-code flow with PKCE. `offline_access` is added for you so Entra returns a refresh token.

```hot
::oauth ::microsoft::graph::oauth

CLIENT_ID "11111111-2222-3333-4444-555555555555"
REDIRECT "https://app.example.com/oauth/microsoft"

start-sign-in fn (state: Str, verifier: Str): Str {
  ::oauth/authorize-url(CLIENT_ID, REDIRECT, ["Files.Read.All", "Sites.Read.All", "User.Read"], state, {
    code-verifier: verifier,
    prompt: "select_account"
  })
}

finish-sign-in fn (code: Str, verifier: Str): Map {
  // Pass the client secret instead of null for a confidential (web) app
  ::oauth/exchange-code(CLIENT_ID, null, code, REDIRECT, {code-verifier: verifier})
}
```

Create the verifier with `::oauth/pkce-verifier()`, keep it with your `state`, and store `tokens.refresh_token` per user. For app-only permissions, a tenant admin consents once through `::oauth/admin-consent-url(client-id, redirect-uri, state)`.

### Refresh-token rotation

Entra returns a new refresh token every time one is redeemed. The newest is kept for the rest of the run; persist it so the 90-day window keeps sliding (single-page-app refresh tokens last 24 hours and cannot be extended):

```hot
::store ::hot::store

tokens ::store/Map({name: "microsoft-tokens"})

sync-user fn (user-id: Str): Bool {
  opts {refresh-token: ::store/get(tokens, user-id)}
  me ::microsoft::graph::users/me(opts)
  newest ::microsoft::graph::auth/latest-refresh-token(opts)
  if(is-some(newest), ::store/put(tokens, user-id, newest))
  not(is-err(me))
}
```

Store values are plain text, so anyone who can read this store can read the refresh tokens. Restrict who can.

`::microsoft::graph::oauth/refresh-access-token(client-id, refresh-token, opts)` redeems one explicitly and returns the full token response, `refresh_token` included. A revoked or expired refresh token fails with `error-code: "invalid_grant"`: send the user through sign-in again.

### Token cache

Minted tokens are cached in the run's execution context with `::hot::ctx/set-secret`, under a key that is a SHA-256 of the credential identity (cloud, tenant, client id, a hash of the client credential, a hash of the refresh token) and the scope set, never the credential itself. Each expires 5 minutes before Entra's `expires_in`. A run that makes 500 calls mints one token; the next run mints again. Explicit access tokens are never cached. A cached token answered with 401 is evicted and the call retried once.

## Permissions and admin consent

Graph permissions are either **delegated** (the app acts as the signed-in user, limited to what they can see) or **application** (app-only, tenant-wide). `User.Read`, `Files.Read`, `Files.ReadWrite` and, in most tenants, `Files.Read.All` and `Sites.Read.All` delegated permissions can be consented by users. **Every application permission needs a tenant admin's consent**, as do `Sites.Selected`, `Sites.FullControl.All` and the other `*.Selected` scopes. For least-privilege app-only access to SharePoint, see [`Sites.Selected`](../microsoft-sharepoint/README.md#least-privilege-app-only-access).

## Test tenants

- **Microsoft 365 Developer Program**: a free renewable E5 sandbox tenant, but only for members who qualify (currently Visual Studio subscribers and members of some partner programs). Check eligibility at [developer.microsoft.com/microsoft-365/dev-program](https://developer.microsoft.com/microsoft-365/dev-program).
- **Microsoft 365 Business Basic**: one paid seat gives you a real tenant with Entra ID, OneDrive for work or school and SharePoint, enough to test every flow including app-only and `Sites.Selected`.
- **Personal OneDrive**: a free Microsoft account works for delegated access (tenant `consumers` or `common`); app-only and SharePoint do not apply.

## Quick start

```hot
::api ::microsoft::graph::api
::users ::microsoft::graph::users

me ::users/me()

// Any endpoint: OData options in opts, ConsistencyLevel added when needed
people ::api/get-json("/users", {select: ["id", "displayName", "mail"], search: "displayName:alex", count: true, top: 25})

// Every page, or at most a limit
groups ::api/collect-pages("/groups", {select: "id,displayName", top: 999}, 2000)

// Raw request: the HttpResponse, non-2xx included
response ::api/request("GET", ::api/with-query("/me/drive", {select: "id,quota"}), {})
```

## Functions by area

**`::microsoft::graph::api`**: `request`, `json`, `get-json`, `resolve` (the credential resolver), `odata-params`, `query-string`, `with-query`, `query-headers`, `encode-segment`, `fetch-page`, `collect-pages`, `collect-with`, `to-page`, `batch`, `build-batch`, `batch-chunks`, `HttpError` (`HttpError(response)` builds it from a non-2xx response), `has-code`, `is-retryable`, `retry-after-seconds`, `api-params`, `credential-opts`.

**`::microsoft::graph::auth`**: `access-token`, `resolve-credentials`, `latest-refresh-token`, `certificate-thumbprints`, `client-assertion`, `cloud-config`, `normalize-scopes`, `token-url`, `evict-token`, `TokenError`.

**`::microsoft::graph::oauth`**: `pkce-verifier`, `pkce-challenge`, `authorize-url`, `admin-consent-url`, `exchange-code`, `refresh-access-token`, `client-credentials`, `granted-scopes`, `missing-scopes`.

**`::microsoft::graph::subscriptions`**: `create-subscription`, `get-subscription`, `list-subscriptions`, `renew-subscription`, `reauthorize-subscription`, `delete-subscription`, `max-expiration-minutes`, `expiration-for`, `build-subscription`, `validation-response`, `validation-token`, `accepted`, `parse-notifications`, `verify-notifications`, `change-notifications`, `lifecycle-notifications`, `needs-reauthorization`, `needs-resync`.

**`::microsoft::graph::users`**: `me`, `get-user`, `list-users`, `user-path`.

## OData queries and paging

Pass OData options by their short names: `select`, `expand`, `filter`, `orderby`, `top`, `skip`, `search`, `count` (or the `$` forms). Vec values are comma-joined, `$search` values are double-quoted, and values are percent-encoded per RFC 3986 (`%20`, never `+`). `search` and `count` add `ConsistencyLevel: eventual`, on every page, as Graph's advanced directory queries require.

`fetch-page(path, opts, next-link)` returns a `Page` (`items`, `next-link`, `delta-link`, `count`, `response`); `collect-pages(path, opts, limit)` follows `@odata.nextLink` to the end or to `limit`.

## Batching

`batch` sends up to 20 requests in one `POST /$batch` and gives the responses back in request order with `by-id`, `failed` and `retryable` (individual 429s): resend just those later. `batch-chunks` splits a longer list; `depends-on` orders requests inside a batch; Bytes bodies are base64-encoded as Graph requires.

```hot
out ::microsoft::graph::api/batch([
  {id: "me", method: "GET", url: "/me?$select=id,displayName"},
  {id: "drive", method: "GET", url: "/me/drive?$select=id,quota"},
  {id: "rename", method: "PATCH", url: "/me/drive/root:/notes.txt", body: {name: "notes-2026.txt"}}
])
out.failed
get(out.by-id, "drive").body.quota
```

## Change notifications (webhooks)

Subscribe to a resource, answer Graph's validation handshake, verify `clientState` on each notification, reply `202` within 3 seconds, and renew before the subscription expires. Notifications for drives and lists carry no item data: treat one as "run the delta sync now".

```hot
::subs ::microsoft::graph::subscriptions
::store ::hot::store

state ::store/Map({name: "graph-subscriptions"})
HOOK "https://api.example.com/graph/hook"

subscribe-drive fn (): Map {
  sub ::subs/create-subscription("/me/drive/root", "updated", HOOK, {
    client-state: ::hot::ctx/get("microsoft.graph.client-state"),
    lifecycle-url: "https://api.example.com/graph/lifecycle"
  })
  if(not(is-err(sub)), ::store/put(state, "drive", sub.id))
  sub
}

graph-hook
meta {webhook: {service: "graph", path: "/graph/hook"}}
fn (request) {
  handshake ::subs/validation-response(request)
  cond {
    is-some(handshake) => { handshake }
    => {
      notes ::subs/verify-notifications(request)
      cond {
        is-err(notes) => { ::hot::http/HttpResponse({status: 401, body: "bad clientState"}) }
        => {
          send("graph:changed", {resources: map(notes, (n) { n.resource })})
          ::subs/accepted()
        }
      }
    }
  }
}

renew-graph-subscription
meta {schedule: "0 4 * * 1"}
fn (event) {
  ::subs/renew-subscription(::store/get(state, "drive"), "/me/drive/root")
}
```

- **Validation.** `validation-response(request)` returns the `200 text/plain` echo of `validationToken` (Graph requires it within 10 seconds) or null for a normal notification. Use it on the lifecycle URL too.
- **Verification.** `verify-notifications(request)` checks every notification's `clientState` against `microsoft.graph.client-state` in constant time and fails closed (halts) when that is unset; `verify-notifications(request, secret)` takes the secret explicitly. Graph signs nothing else for basic notifications, so treat `clientState` as a password: at most 128 characters, random, never logged. Rich notifications (`includeResourceData`) also carry `validationTokens` JWTs, not checked here.
- **Lifetimes.** `max-expiration-minutes(resource)`: drive items and SharePoint lists 42,300 minutes (under 30 days), users and groups 41,760, Outlook 10,080 (1,440 with resource data), Teams 4,320, presence 60, others 4,230. New subscriptions and renewals default to the maximum minus an hour; anything under 45 minutes is raised to 45 by Graph. Renew from a Hot schedule well inside the lifetime.
- **Lifecycle notifications** arrive at `lifecycle-url` with `lifecycle-event`: `reauthorizationRequired` (call `reauthorize-subscription` or `renew-subscription`, not both within 10 minutes), `subscriptionRemoved` (create it again, then resync) and `missed` (resync with a delta query). `needs-reauthorization` and `needs-resync` sort a batch for you.
- **Duplicates.** Graph returns 409 for a second subscription with the same resource and change type: `list-subscriptions` and reuse it.

## Errors and retries

Endpoint functions return the decoded body, or an Err carrying `::microsoft::graph::api/HttpError`: `status`, `code` (`itemNotFound`, `accessDenied`, `activityLimitReached`, `nameAlreadyExists`, ...), `inner-code`, `error-message`, a readable `message` (`"microsoft-graph: HTTP 404 itemNotFound: The resource could not be found."`), `request-id` and `client-request-id` for Microsoft support, and `retry-after`. Credential failures carry `::microsoft::graph::auth/TokenError` with Entra's `error-code`, `error-codes` (`AADSTS` numbers), `trace-id` and `correlation-id`.

Nothing waits inside a run: Hot has no sleep. A dropped connection on an idempotent method is retried once, immediately. Throttling (429, and 503 with `Retry-After`) and 5xx come back as errors; check `is-retryable(result)` and let a handler, schedule or task `retry` policy reschedule (see [retries](https://hot.dev/docs/retries)):

```hot
sync-users
meta {
  schedule: "every 30 minutes",
  retry: {attempts: 5, delay: 10000, backoff: "exponential", max_delay: 300000, jitter: true}
}
fn (event) {
  users ::microsoft::graph::users/list-users({select: ["id", "mail"]}, 5000)
  cond {
    ::microsoft::graph::api/is-retryable(users) => { fail(`graph throttled, retrying: ${err-message(users)}`) }
    is-err(users) => { println(`graph: giving up: ${err-message(users)}`) }
    => { length(users) }
  }
}
```

## Data handling

Tokens, secrets, private keys and `clientState` are never logged or put in error messages. Whatever arrives per call or comes back from Entra (access, refresh and id tokens) is marked with `::hot::ctx/mark-secret`, so call records mask it; cached tokens live only in the run's secret context. Error messages include Graph's `error.message`, which can name an item but not credential material. Graph data you fetch (user profiles, file contents) is yours to store; keep refresh tokens in a secret store.

## Notes and limits

- National clouds need their own app registration, and tokens are not interchangeable between clouds; personal Microsoft accounts only exist in the global cloud.
- App-only tokens have no `/me`: address users by id or user principal name.
- `$search` and `$count` on directory objects need `ConsistencyLevel: eventual` (added automatically); `$filter` support varies by resource.
- Hot's HTTP client caps each response at `http.max-response-bytes` (16 MiB by default); the OneDrive package handles large downloads with Range requests.
- No Microsoft sales contract is needed: Graph is included with Microsoft 365 / Entra ID. Some APIs (Teams export, some security APIs) are metered or licensed separately and are not wrapped here.

## License

Apache-2.0 - see [LICENSE](LICENSE)
