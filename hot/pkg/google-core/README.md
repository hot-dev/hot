# google-core

The Google API foundation for Hot, shared by the `google-*` packages ([drive](../google-drive), gmail, sheets, calendar). It resolves credentials from context or per call, mints and caches tokens, handles scopes, turns Google errors into readable `HttpError`s (and tells you which ones are worth retrying), pages through `nextPageToken` lists, and runs the OAuth consent flow.

You rarely call it directly: the service packages use it for every request. Reach for it to run the consent flow, to act for many users from one app, or to call a Google API that has no package yet.

## Install

Add it to your project's deps in `hot.hot` (the service packages pull it in for you):

```hot
hot.project.my-app.deps {
  "hot.dev/google-core": "0.1.2"
}
```

It depends on [`hot.dev/jwt`](../jwt) (service-account assertions) and [`hot.dev/oauth2`](../oauth2) (token grants).

## Authentication

Set **one** of these in Context Variables. They are checked in this order:

| Credential | Context variables | Use it for |
|---|---|---|
| Access token | `google.access.token` | A token you obtained and refresh yourself |
| User refresh token | `google.oauth.refresh-token`, `google.oauth.client-id`, `google.oauth.client-secret` | Acting as one user who consented once (see [Consent flow](#consent-flow)) |
| Service account | `google.service.account` (the key JSON as a string) | Server-to-server access to the service account's own resources and to files shared with it |
| Domain-wide delegation | `google.service.account` plus `google.impersonate` (a user email) | A Workspace admin letting your service account act as any user in the domain |
| API key | `google.api.key` | Public data only (e.g. files shared "anyone with the link") |

Other context variables:

| Context Variable | Description |
|---|---|
| `google.scopes` | Overrides the scopes service-account tokens are minted with, for every call in the run (Vec or space/comma-separated Str; short names like `drive.readonly` are expanded) |

A refresh token without `google.oauth.client-id` is rejected before any network call. `google.service.account` also accepts an `authorized_user` file (what `gcloud auth application-default login` writes); it is used as a refresh token.

### Per-call credentials (multi-tenant)

Every request helper, and every function in the service packages, has an options arity taking an `opts` Map. Credentials in `opts` override context for that call only, so one app can serve many users:

| `opts` keys | Credential |
|---|---|
| `token` | Access token |
| `refresh-token`, `client-id`, `client-secret` | User refresh token |
| `service-account` (JSON Str or Map), `subject` | Service account, optionally impersonating `subject` |
| `api-key` | API key |
| `subject` alone | Context's service account, impersonating this user |
| `scopes` | Scopes for this call |
| `retry` | `false` to skip the one immediate retry of a failed connection (see [Retries](#retries)) |

```hot
::files ::google::drive::files

// Tenant A: their stored refresh token
a-files ::files/list-files({
  q: "trashed = false",
  refresh-token: "1//0g-tenant-a-refresh-token",
  client-id: "123.apps.googleusercontent.com",
  client-secret: "GOCSPX-your-secret"
})

// Domain-wide delegation: the service account in context, acting as one user
b-files ::files/list-files({q: "trashed = false", subject: "bob@example.com"})
```

When `opts` carries a primary credential (`token`, `refresh-token`, `service-account` or `api-key`), context credentials are ignored for that call, so one tenant can never fall back to another's. A credential key that is present but blank or null (`{token: ""}`) is an error naming the key, never a fallback to context; leave the key out to use context.

Per-call credentials, and every token Google returns (minted tokens and `exchange-code` results), are masked in call records. Nothing from `opts` is written to context.

## Scopes

Each service function asks for the narrowest default scope that fits (for Drive: `drive.readonly` to read, `drive` to write) and documents which scopes work.

- **Access and refresh tokens** carry whatever the user granted at consent time; the requested scope is not sent. Ask for the scopes you need in `authorize-url`.
- **Service accounts** get a token minted for the requested scopes. With domain-wide delegation, Google only issues tokens for scopes the admin authorised for your client ID. If you authorised, say, only `drive.readonly`, set `google.scopes` to `"drive.readonly"` (or pass `scopes` per call) so the write-scope default is never requested.

Prefer non-sensitive scopes: Drive's `drive.file` (files your app created or the user picked) needs only basic verification, while `drive` and `drive.readonly` are restricted. See the [google-drive README](../google-drive/README.md#auth-and-scopes).

## Token cache

Minted tokens are cached for the run in the execution context (as secrets), keyed by credential identity, sorted scope set and subject, until `expires_in` minus 60 seconds. A run that makes 500 calls mints one token per credential and scope set; the next run mints again. Explicit access tokens are never cached. If a cached token is rejected with 401, it is evicted and the call is retried once with a fresh token.

## Domain-wide delegation setup

1. In Google Cloud, create a project, enable the APIs you will call (e.g. Google Drive API), and create a service account.
2. Create a JSON key for it. New organisations block key creation by default (`iam.disableServiceAccountKeyCreation`); an org admin can allow it for the project.
3. Copy the service account's **Client ID** (the numeric unique ID).
4. In the Google Admin console: Security > Access and data control > API controls > Manage Domain Wide Delegation > Add new. Enter the client ID and the full scope URLs, comma-separated (e.g. `https://www.googleapis.com/auth/drive.readonly`).
5. Set `google.service.account` to the key JSON and `google.impersonate` to a user (or pass `subject` per call). Set `google.scopes` if you authorised fewer scopes than the package defaults request.

Changes can take a few minutes to apply. `unauthorized_client` from the token endpoint means the client ID or scope list does not match.

## Consent flow

For apps acting for users who sign in with Google. Create an OAuth client (type "Web application") with your redirect URI.

```hot
::oauth ::google::oauth

client-id ::hot::ctx/get("google.oauth.client-id")
client-secret ::hot::ctx/get("google.oauth.client-secret")
redirect "https://app.example.com/oauth/google"

// 1. Send the user to Google (offline access + consent prompt by default,
//    so a refresh token comes back; PKCE optional)
verifier ::oauth/pkce-verifier()
consent-url ::oauth/authorize-url(client-id, redirect, ["drive.file", "openid", "email"], "csrf-state-123", {
  code-verifier: verifier,
  login-hint: "ada@example.com"
})

// 2. In the callback, exchange ?code= for tokens
on-callback fn (code: Str): Map {
  tokens ::oauth/exchange-code(client-id, client-secret, code, redirect, {code-verifier: verifier})
  missing ::oauth/missing-scopes(tokens, "drive.file")
  // store tokens.refresh_token per user; check `missing` (users can untick scopes)
  {refresh-token: tokens.refresh_token, missing: missing}
}
```

`authorize-url` options: `access-type` (`"offline"`), `prompt` (`"consent"`; null to omit), `include-granted-scopes` (`true`), `login-hint`, `hd`, `code-verifier` or `code-challenge`, `extra`. Also: `refresh-access-token`, `revoke-token`, `granted-scopes`.

## Calling any Google API

```hot
::api ::google::api

// Raw response (non-2xx included)
response ::api/request("drive.readonly", "GET", "https://www.googleapis.com/drive/v3/about?fields=user", {}, "")

// Decoded JSON on 2xx, or Err(HttpError)
me ::api/json("drive.readonly", "GET", "https://www.googleapis.com/drive/v3/about?fields=user", {}, "", {retry: false})

// Every page of a nextPageToken list, at most 2000 items
messages ::api/collect-pages((page-token) {
  ::api/json("gmail.readonly", "GET", ::api/with-query("https://gmail.googleapis.com/gmail/v1/users/me/messages", {pageToken: page-token}))
}, "messages", 2000)
```

Map bodies are sent as JSON; Str and Bytes bodies verbatim (set `Content-Type` yourself). `fetch-page` returns one `Page {items, next-page-token, response}`.

## Errors

Failures come back as `Result.Err`:

- `::google::api/HttpError` for non-2xx responses: `status`, `headers`, `body`, plus `message` (`"google: HTTP 404 NOT_FOUND (notFound): File not found: 1AbC."`), `error-message`, `error-status`, `reason` and `reasons` from Google's `errors[].reason`, and `retry-after` (seconds, from a numeric `Retry-After` header). `::google::api/is-retryable(result)` is true for 408, 429, 5xx, rate-limit 403s and connection failures. Check a reason with `::google::api/has-reason(result, "insufficientFilePermissions")`.
- `::google::auth/TokenError` for credential problems: `message` (`"google: token exchange failed: invalid_grant (Token has been expired or revoked.)"`), `status`, `error-code`, `error-description`.

`err-message(result)` gives the readable message for both.

## Retries

google-core never sleeps or spins inside a run. It retries only one case, once and immediately: a connection failure (reset, refused, read error) on an idempotent method (GET, HEAD, PUT, DELETE, OPTIONS). Pass `{retry: false}` to turn that off.

Everything else comes back to you as `err(HttpError)`, including 429, 408, 500, 502, 503, 504 and the rate-limit 403s (`rateLimitExceeded`, `userRateLimitExceeded`). `HttpError.retry-after` holds the seconds from a numeric `Retry-After` header. To back off, use Hot's run-level retries: give the event handler, schedule or task a `retry` policy (see [Retries](https://hot.dev/docs/retries)), and fail the run when `::google::api/is-retryable(result)` says the error is transient:

```hot
::api ::google::api

sync-inbox
meta {
  schedule: "every 15 minutes",
  retry: {attempts: 5, delay: 2000, backoff: "exponential", max_delay: 60000, jitter: true}
}
fn (event) {
  page ::api/json("gmail.readonly", "GET", "https://gmail.googleapis.com/gmail/v1/users/me/messages", {}, "")
  cond {
    ::api/is-retryable(page) => { fail(`google: retrying later: ${err-message(page)}`) }
    is-err(page) => { println(`google: giving up: ${err-message(page)}`) }
    => { length(or(page.messages, [])) }
  }
}
```

Hot schedules the next attempt with the backoff you configured, so the worker is free in the meantime. Webhook handlers and MCP tools don't get run retries; hand the work to a task started with `::hot::task/start(fn, args, {retry: {attempts: 5, delay: 2000, backoff: "exponential"}})`. `is-retryable` is also true for a connection failure on any method. A failed `POST` (sending mail, creating an event) may still have reached Google, so retrying it can duplicate the write.

## Notes and limits

- The token cache lasts one run. Long-lived workers that need cross-run caching should store tokens themselves and pass `{token}`.
- Service accounts have no Drive storage of their own: uploads to a service account's My Drive fail with `storageQuotaExceeded`. Use a shared drive or domain-wide delegation.
- One run's responses are capped by `http.max-response-bytes` (16 MiB by default) with a 120 s timeout.

## License

Apache-2.0 - see [LICENSE](LICENSE)
