# cloudflare

Cloudflare API bindings for Hot: zones and DNS records, cache purge, Workers KV, Worker scripts and routes, R2 buckets, D1 queries, Pages deployments, and Turnstile verification. Readable errors from Cloudflare's `errors` array, page- and cursor-based pagination with collect-all helpers, and `::cloudflare::api/json` for any endpoint not covered.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/cloudflare": "0.1.0"
}
```

## Setup

| Context Variable | Description |
|---|---|
| `cloudflare.token` | An [API token](https://dash.cloudflare.com/profile/api-tokens), sent as `Authorization: Bearer`. Grant only what you use (Zone → DNS Edit, Zone → Cache Purge, Account → Workers KV Storage Edit, Workers Scripts Edit, Workers R2 Storage Edit, D1 Edit, Cloudflare Pages Edit) |
| `cloudflare.api.key` + `cloudflare.email` | Legacy Global API Key pair, used only when `cloudflare.token` is unset |
| `cloudflare.account.id` | Account ID for account-scoped APIs: KV, Workers, R2, D1, Pages. Zones, DNS, and cache purge don't need it |
| `cloudflare.turnstile.secret` | A Turnstile widget's secret key, for `::cloudflare::turnstile` |

A missing token or account id is an Err carrying `ConfigError` with a message that names the variable. Account-scoped calls never send a request to `/accounts//...`.

All of these can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::zones ::cloudflare::zones
::dns ::cloudflare::dns
::cache ::cloudflare::cache

zone ::zones/find-zone("example.com")

// Upsert an A record
existing ::dns/find-dns-records(zone.id, "api.example.com", "A")
record cond {
  is-empty(existing) => { ::dns/create-dns-record(zone.id, {type: "A", name: "api", content: "203.0.113.7", proxied: true}) }
  => { ::dns/update-dns-record(zone.id, first(existing).id, {content: "203.0.113.7"}) }
}

// Purge what a deploy changed
::cache/purge-urls(zone.id, ["https://example.com/", "https://example.com/pricing"])
::cache/purge-tags(zone.id, ["product-1234"])
```

## Functions

| Module | Functions |
|---|---|
| `::cloudflare::zones` | `list-zones`, `list-all-zones`, `find-zone`, `get-zone` |
| `::cloudflare::dns` | `list-dns-records`, `list-all-dns-records`, `find-dns-records`, `get-dns-record`, `create-dns-record`, `update-dns-record` (PATCH), `overwrite-dns-record` (PUT), `delete-dns-record` |
| `::cloudflare::cache` | `purge-urls`, `purge-tags`, `purge-hosts`, `purge-prefixes`, `purge-everything`, `purge` (explicit body) |
| `::cloudflare::kv` | `list-namespaces`, `list-all-namespaces`, `create-namespace`, `delete-namespace`, `list-keys`, `list-all-keys`, `read`, `read-text`, `read-json`, `read-metadata`, `is-not-found`, `write`, `write-bulk`, `delete-key`, `delete-bulk` |
| `::cloudflare::workers` | `list-scripts`, `get-script-settings`, `deploy-module`, `delete-script`, `list-routes`, `create-route`, `delete-route` |
| `::cloudflare::r2` | `list-buckets`, `list-all-buckets`, `create-bucket`, `get-bucket`, `delete-bucket`, `s3-endpoint` |
| `::cloudflare::d1` | `list-databases`, `list-all-databases`, `find-database`, `get-database`, `query`, `rows` |
| `::cloudflare::pages` | `list-projects`, `list-all-projects`, `get-project`, `list-deployments`, `get-deployment`, `create-deployment`, `retry-deployment`, `rollback-deployment` |
| `::cloudflare::turnstile` | `siteverify`, `is-valid` |
| `::cloudflare::api` | `json`, `request`, `send`, `json-page`, `account-json`, `query`, `collect-pages`, `collect-cursor`, `next-cursor`, `multipart-body` |

Single-object calls return the envelope's `result`. List calls return a `ListPage` (`.result` items, `.result_info` paging metadata). The `list-all-*` forms follow every page up to a limit (10,000 items by default; pass a `max` to change it).

### DNS filters

```hot
::dns ::cloudflare::dns

page ::dns/list-dns-records(zone-id, {type: "TXT", content: {contains: "verification"}, per_page: 100})
page.result_info.total_count

cnames ::dns/list-all-dns-records(zone-id, {type: "CNAME", name: {endswith: ".example.com"}})
```

`name`, `content`, `comment`, and `tag` take an exact string or an operator map (`exact`, `contains`, `startswith`, `endswith`); nested maps are sent in Cloudflare's dot form (`name.contains=...`).

### Workers KV

```hot
::kv ::cloudflare::kv

::kv/write(namespace-id, "flag:checkout-v2", "on")
::kv/write(namespace-id, "session:abc", "{\"user\":\"u_42\"}", {expiration_ttl: 3600, metadata: {user: "u_42"}})

flag ::kv/read-text(namespace-id, "flag:checkout-v2")   // "on"
session ::kv/read-json(namespace-id, "session:abc")      // {user: "u_42"}
avatar ::kv/read(namespace-id, "avatar:42")             // Bytes

sessions ::kv/list-all-keys(namespace-id, {prefix: "session:"}, 5000)
::kv/delete-bulk(namespace-id, map(sessions, %.name))
```

KV values are raw bytes: `read` returns `Bytes`, and `read-text` / `read-json` decode them. An absent key is an Err with status 404; check it with `is-not-found(result)` on the bound result. Writes accept Str or Bytes (a Map is stored as JSON) and are visible at the edge within about 60 seconds.

### Workers, R2, D1, and Pages

```hot
::workers ::cloudflare::workers
::r2 ::cloudflare::r2
::d1 ::cloudflare::d1
::pages ::cloudflare::pages

::workers/deploy-module("hello", "export default { fetch() { return new Response('hi') } }", {
  compatibility_date: "2025-09-01",
  bindings: [{type: "plain_text", name: "GREETING", text: "hi"}]
})
::workers/create-route(zone-id, "example.com/hello/*", "hello")

::r2/create-bucket("exports", {location_hint: "enam"})
buckets ::r2/list-all-buckets({name_contains: "export"})

users ::d1/rows(db-id, "SELECT id, email FROM users WHERE plan = ? LIMIT ?", ["pro", 50])

deployment ::pages/create-deployment("marketing-site", {branch: "main"})
recent ::pages/list-deployments("marketing-site", {env: "production"})
```

## Turnstile

Verify the widget token on the server before trusting a form post. A failed check is `false`; a missing secret is an Err, never `true`.

```hot
signup
meta {webhook: {service: "forms", path: "/signup"}}
fn (request) {
  token or(get(request.body, "cf-turnstile-response"), "")
  verified ::cloudflare::turnstile/is-valid(token, {remoteip: request.ip, expected_hostname: "example.com"})
  if(verified,
    ::hot::http/HttpResponse({status: 200, body: {ok: true}}),
    ::hot::http/HttpResponse({status: 403, body: "verification failed"}))
}
```

`siteverify(token, opts)` returns Cloudflare's full answer (`success`, `error-codes`, `hostname`, `action`, `cdata`). Tokens are single-use and expire after 300 seconds. Cloudflare publishes [test sitekeys and secrets](https://developers.cloudflare.com/turnstile/troubleshooting/testing/) that always pass or always fail.

## Per-call credentials

Every function that calls Cloudflare takes an optional trailing `opts` Map, so one run can act for many accounts and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `token` | An API token for this call, instead of `cloudflare.token` |
| `api-key` + `email` | A Global API Key pair for this call. Pass both; a `token` in the same `opts` wins |
| `account-id` | The account for account-scoped calls, instead of `cloudflare.account.id` |
| `secret-key` | Turnstile only: the widget's secret key, instead of `cloudflare.turnstile.secret` |

`opts` always comes last. Where a function already takes `params` or `options`, pass `{}` for those.

```hot
tenant {token: "tenant-b-token", account-id: "acc123"}
zone ::cloudflare::zones/find-zone("example.com", tenant)
::cloudflare::dns/create-dns-record(zone.id, {type: "A", name: "api", content: "203.0.113.7"}, tenant)
::cloudflare::kv/write("ns-id", "flag:checkout-v2", "on", {}, tenant)
ok ::cloudflare::turnstile/is-valid("widget-token", {secret-key: "0x4AAA...", remoteip: "203.0.113.9"})
```

One of these keys present but blank or null is an error naming the key, never a fallback to context. `::cloudflare::api/resolve(opts)` is the one place this is decided, and the Turnstile functions apply the same rules to `secret-key`.

## Errors

A non-2xx response, or a 2xx envelope with `success: false`, is an Err carrying `HttpError`:

| Field | Value |
|---|---|
| `status` | HTTP status |
| `message` | Cloudflare's `errors` joined into one line, e.g. `"6003: Invalid request headers (6111: Invalid format for Authorization header)"` |
| `errors` | The raw `errors` array (`{code, message, error_chain?}`) |
| `headers`, `body` | The response headers and decoded body |

```hot
out ::cloudflare::zones/list-zones()
if-err(out, (e) { println(`Cloudflare said: ${e.message}`) })
```

## Notes and limits

- **R2 objects** are not part of the Cloudflare API. Read and write them through R2's S3-compatible endpoint with the `aws-s3` package: endpoint `::cloudflare::r2/s3-endpoint(account-id)` (`https://{account_id}.r2.cloudflarestorage.com`), region `auto`, and an R2 access key pair from the dashboard.
- **Workers deploys** upload one ES module. Multi-module bundles, static assets, Durable Object migrations, and secrets are better served by `wrangler deploy`.
- **Pages** `create-deployment` starts a build of a Git-connected project. Direct Upload deploys (sending built files) need `wrangler pages deploy`.
- **D1** query params are sent as strings, as the REST API defines them; SQLite's type affinity converts them for numeric columns and `LIMIT`.
- **Cache purge** limits (URLs per request, requests per minute) depend on the plan.
- Collect-all helpers fetch pages serially; use the single-page functions with `page` / `cursor` for very large accounts.

## License

Apache-2.0 - see [LICENSE](LICENSE)
