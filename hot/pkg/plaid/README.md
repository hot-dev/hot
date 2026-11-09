# plaid

Plaid bindings for Hot: bank account data for fintech builders. The package covers the Link token flow, Items, accounts, real-time balance, Auth, Identity, transactions (sync with the `has_more` loop, get, refresh, recurring), institutions, investments, liabilities and sandbox helpers. It also verifies webhooks against Plaid's ES256 signature. `::plaid::api/post` calls any other endpoint.

Webhook verification needs Hot 3.0.2 or later, for the ES256 builtin.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/plaid": "0.1.2"
}
```

It depends on `hot.dev/jwt`, which is fetched with it.

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `plaid.client-id` | yes, unless passed per call | Your Plaid client id |
| `plaid.secret` | yes, unless passed per call | The secret for the environment you call (sandbox and production secrets differ) |
| `plaid.env` | no | `"sandbox"` (default) or `"production"`. Anything else errs, including the decommissioned `"development"` |
| `plaid.version` | no | Overrides the `Plaid-Version` header (default `2020-09-14`, the current API version) |

Plaid takes `client_id` and `secret` in the JSON body, and the package injects them. Every function returns the decoded response body.

## Quick start: the Link flow

Link runs in your client (web, iOS, Android). The server creates the token, the client runs Link with it, and the server exchanges the result:

```hot
::link ::plaid::link

// 1. Server: create a link token for one of your users. Use your own
//    stable user id, not an email or other PII.
link ::link/create-link-token("user-42", "My App", {
  products: ["transactions"],
  country_codes: ["US"],
  webhook: "https://example.com/plaid/webhook",
  redirect_uri: "https://example.com/plaid/oauth"
})

// 2. Client: open Plaid Link with link.link_token. On success Link hands
//    the client a public_token, which it posts back to your server.

// 3. Server: exchange it (single use, valid for 30 minutes)
tokens ::link/exchange-public-token(public-token)
// tokens.access_token and tokens.item_id: store them (the access token
// encrypted). The access token doesn't expire.
```

No frontend yet? In sandbox, skip Link:

```hot
pt ::plaid::sandbox/create-public-token("ins_109508", ["transactions"], {webhook: "https://example.com/plaid/webhook"})
tokens ::plaid::link/exchange-public-token(pt.public_token)
::plaid::sandbox/fire-webhook(tokens.access_token, "SYNC_UPDATES_AVAILABLE")
```

## Per-call credentials

Every function that calls Plaid takes an optional trailing `opts` Map, so one run can act for many Plaid clients and parallel flows don't share credentials. Per-call credentials are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `client-id`, `secret` | The credentials for this call, instead of `plaid.client-id` and `plaid.secret`. Always together |
| `environment` | `"sandbox"` or `"production"` for this call, instead of `plaid.env` |
| `plaid-version` | The `Plaid-Version` for this call, instead of `plaid.version` |

`opts` always comes last. Where a function also takes Plaid's own `options`, pass `null` or `{}` for those.

```hot
tenant {client-id: "tenant-client-id", secret: "tenant-secret", environment: "production"}

link ::plaid::link/create-link-token("user-42", "My App", {}, tenant)
tokens ::plaid::link/exchange-public-token(public-token, tenant)
accounts ::plaid::accounts/get-accounts(tokens.access_token, null, tenant)
changes ::plaid::transactions/sync-all-transactions(tokens.access_token, null, {}, tenant)
```

A key that is present but blank or null is an error naming the key, never a fallback to context, and half a credential is an error too. `::plaid::api/resolve(opts)` is the one place this is decided. Item access tokens, and the tokens Plaid mints, are masked in call records as well.

## Transactions sync

Keep one cursor per Item, and sync when the `SYNC_UPDATES_AVAILABLE` webhook arrives. `sync-all-transactions` loops while `has_more` is true and returns the combined `added`, `modified` and `removed`. If Plaid reports `TRANSACTIONS_SYNC_MUTATION_DURING_PAGINATION`, it restarts from the first cursor, as Plaid requires. You store the cursor:

```hot
::store ::hot::store

cursors ::store/Map({name: "plaid-cursors"})
cursor ::store/get(cursors, item-id)

changes ::plaid::transactions/sync-all-transactions(access-token, cursor, {count: 500})
// Apply changes.added, changes.modified and changes.removed (removed
// entries carry transaction_id) to your own tables, then persist the
// cursor. Persisting it before applying would lose changes on a crash.
::store/put(cursors, item-id, changes.next_cursor)
```

The first sync on a new Item can come back empty with `transactions_update_status` `NOT_READY`. Wait for the webhook. `max-pages` (default 100) bounds one call; if it's hit, the result has `has_more: true` and a valid cursor to continue from.

## Functions

| Namespace | Functions |
|---|---|
| `::plaid::link` | `create-link-token`, `get-link-token`, `exchange-public-token`, `build-link-token-request` |
| `::plaid::items` | `get-item`, `remove-item`, `update-item-webhook` |
| `::plaid::accounts` | `get-accounts`, `get-balances`, `get-auth`, `get-identity` |
| `::plaid::transactions` | `sync-transactions`, `sync-all-transactions`, `get-transactions`, `refresh-transactions`, `get-recurring-transactions` |
| `::plaid::institutions` | `get-institutions`, `get-institution`, `search-institutions` |
| `::plaid::investments` | `get-holdings`, `get-investment-transactions`, `get-liabilities` |
| `::plaid::sandbox` | `create-public-token`, `fire-webhook`, `reset-login` (they err unless `plaid.env` is `"sandbox"`) |
| `::plaid::webhooks` | `verify-webhook`, `check-webhook`, `get-verification-key`, `cached-verification-key`, `event` |
| `::plaid::api` | `post`, `env-base-url`, `current-env`, `plaid-version`, `is-error-code` |

Most endpoint functions take an optional last `options` argument, which becomes Plaid's `options` object (for example `{account_ids: [...]}`). Unset fields are never sent as JSON `null`. `create-link-token` options are Plaid's top-level fields: `products`, `optional_products`, `required_if_supported_products`, `country_codes`, `language`, `webhook`, `redirect_uri`, `android_package_name`, `user` (merged with `client_user_id`), `user_id`, `access_token` (update mode), `account_filters`, `transactions: {days_requested}`, `hosted_link`, and so on.

```hot
::accounts ::plaid::accounts
::institutions ::plaid::institutions

balances ::accounts/get-balances(access-token, {account_ids: [account-id]})
item ::plaid::items/get-item(access-token)
hits ::institutions/search-institutions("chase", ["transactions"], ["US"])

// Re-authenticate an Item that needs it (Link update mode)
relink ::plaid::link/create-link-token("user-42", "My App", {access_token: access-token})
```

## Webhooks

The 1-arity `verify-webhook` fetches the key with the context credentials. Pass a null `jwk` and per-call credentials to fetch it with a tenant's: `verify-webhook(request, null, {}, tenant)`.

Plaid signs each webhook with an ES256 JWT in the `Plaid-Verification` header. `verify-webhook` checks the following:

- the header is present, and the JWT header says `alg: ES256`;
- the signature verifies against Plaid's key for the JWT's `kid`;
- `iat` is no more than 5 minutes old;
- the `request_body_sha256` claim matches the SHA-256 of the raw body, compared in constant time.

The 1-arity form fetches the key from `/webhook_verification_key/get`. It caches the key by `kid` in a persistent store for a day, so a key Plaid later expires is noticed, and it refuses keys whose `expired_at` has passed. It fails closed: without the header the result is false, and without credentials the call fails.

```hot
plaid-webhook
meta {webhook: {service: "plaid", path: "/plaid/webhook"}}
fn (request) {
  cond {
    not(::plaid::webhooks/verify-webhook(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    => {
      e ::plaid::webhooks/event(request)
      // e.webhook_type "TRANSACTIONS", e.webhook_code "SYNC_UPDATES_AVAILABLE", e.item_id
      ::hot::http/HttpResponse({status: 200, body: {received: true}})
    }
  }
}
```

`check-webhook(request, jwk, options)` returns the claims, or an Err saying what failed. Use it when you fetch keys yourself or need the reason. It takes the options `max-age` (seconds, default 300), `leeway` (default 0) and `now`. Plaid hashes the body exactly as sent, with 2-space indented JSON. Verify `request.body-raw` (the package uses `::hot::http/raw-body`), never a re-serialised `request.body`.

## Errors and PII

A non-2xx response is an Err carrying `HttpError {status, headers, body, error_type, error_code, error_message, display_message, request_id, documentation_url, suggested_action}`. `display_message` is the only field meant for end users. Quote `request_id` to Plaid support. `::plaid::api/is-error-code(e, "ITEM_LOGIN_REQUIRED")` tests for a specific code.

Treat what flows through this package as sensitive:

- Never log access tokens, public tokens or the client secret. Store access tokens encrypted.
- `/auth/get` returns account and routing numbers, and `/identity/get` returns names, emails, phones and addresses. Don't log those responses, and keep only what you need.
- `client_user_id` should be an opaque id, not an email or phone number.

## Notes and limits

- **Sandbox** is free and needs no frontend: use `create-public-token`, `fire-webhook` and `reset-login`. Test credentials are `user_good` / `pass_good`, and `ins_109508` is First Platypus Bank. Production needs Plaid's approval for each product.
- `get-balances` hits the institution and is billed per call. `get-accounts` returns cached balances for free.
- `/transactions/get` and `/investments/transactions/get` page by `count` (max 500) and `offset`. Prefer sync for keeping a copy up to date.
- `remove-item` is irreversible: it invalidates the access token and stops billing.

## License

Apache-2.0 - see [LICENSE](LICENSE)
