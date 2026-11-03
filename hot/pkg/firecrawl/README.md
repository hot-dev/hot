# firecrawl

Firecrawl v2 bindings for Hot: scrape any URL to clean markdown, search the web with optional page content, map a site's URLs, crawl whole sites, batch-scrape lists of URLs, and run LLM-structured extraction. Crawls, batch scrapes, and extractions are async jobs: start them, then pick up the results from a verified webhook or a later run. It is the read-the-web half of `web-search`.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/firecrawl": "0.1.0"
```

## Setup

| Context Variable | Description |
|---|---|
| `firecrawl.api.key` | API key from the [Firecrawl dashboard](https://www.firecrawl.dev/app) (`fc-...`), sent as `Authorization: Bearer` |
| `firecrawl.webhook.secret` | Webhook signing secret (dashboard, account settings, Advanced tab), for `::firecrawl::webhooks/verify-request` |

The API key can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::fc ::firecrawl

// One page as markdown
page ::fc/scrape("https://example.com", {formats: ["markdown", "links"]})
page.markdown

// Search, with each hit scraped
hits ::fc/search("firecrawl changelog", {limit: 3, scrapeOptions: {formats: ["markdown"]}})
map(hits.web, (hit) { hit.url })

// Start async jobs: each returns {id} right away
job ::fc/crawl("https://docs.firecrawl.dev", {limit: 10})
ex ::fc/extract(["https://firecrawl.dev/pricing"], {prompt: "List the plan names and monthly prices"})

// Later (a webhook, a schedule, or the next run): read the status
site ::fc/get-crawl-all(job.id)
if(::fc/is-done(site), map(site.data, (doc) { doc.metadata.sourceURL }), [])
```

Options are passed through as Firecrawl documents them (camelCase: `onlyMainContent`, `scrapeOptions`, `includePaths`, ...). Unset (null) options are dropped rather than sent.

## Functions

**Scrape, map, search** (synchronous)

| Function | Endpoint | Returns |
|---|---|---|
| `scrape(url)`, `scrape(url, options)` | `POST /scrape` | The document: `markdown`, `metadata`, plus any other requested formats |
| `map-site(url)`, `map-site(url, options)` | `POST /map` | `{links: [{url, title?, description?}]}` |
| `search(query)`, `search(query, options)` | `POST /search` | `{web: [{title, url, description, markdown?}], news?, images?}` |
| `credit-usage()` | `GET /team/credit-usage` | `{remainingCredits, planCredits, ...}` |

**Crawl** (async job)

| Function | Endpoint |
|---|---|
| `crawl(url)`, `crawl(url, options)` | `POST /crawl`, returns `{id, url}` |
| `get-crawl(id)` | `GET /crawl/{id}`, one status page |
| `get-crawl-all(id)` | `GET /crawl/{id}` following `next` |
| `cancel-crawl(id)` | `DELETE /crawl/{id}` |
| `get-crawl-errors(id)` | `GET /crawl/{id}/errors` |
| `list-active-crawls()` | `GET /crawl/active` |

**Batch scrape** (async job): `batch-scrape(urls, options)`, `get-batch-scrape(id)`, `get-batch-scrape-all(id)`, `cancel-batch-scrape(id)`, `get-batch-scrape-errors(id)`, mirroring the crawl functions over `/batch/scrape`.

**Extract** (async job): `extract(urls, options)` starts the job and returns `{id}`; `get-extract(id)` reads its status, with the result in `data` once it completes.

### Finishing jobs

Nothing in this package blocks waiting for a job. Start the job, keep its `id`, and pick up the result in one of two ways:

- **Webhook** (preferred). Pass `webhook: {url}` to `crawl` or `batch-scrape`, and Firecrawl POSTs `crawl.completed` / `batch_scrape.completed` when the job finishes (see below).
- **Check from a later run.** From a schedule or the next step of a workflow, call `get-crawl-all`, `get-batch-scrape-all`, or `get-extract`, and act once `is-done` is true. `is-done` is true for `"completed"`, `"failed"`, and `"cancelled"`, so check `status` for the outcome.

```hot
::store ::hot::store

pending ::store/Map({name: "firecrawl-pending-crawls"})

start-crawl fn (url: Str): Str {
  job ::firecrawl/crawl(url, {limit: 25})
  ::store/put(pending, job.id, {url: url})
  job.id
}

check-crawls
meta {schedule: "*/5 * * * *"}
fn (event) {
  map(::store/keys(pending), (id) {
    job ::firecrawl/get-crawl-all(id)
    cond {
      not(::firecrawl/is-done(job)) => { null }
      => {
        ::store/delete(pending, id)
        send("crawl:finished", {id: id, status: job.status, pages: length(job.data)})
      }
    }
  })
}
```

Results of large jobs arrive in pages of about 10MB, each linking to the next with `next`. `get-crawl-all` and `get-batch-scrape-all` follow those links, and only to the Firecrawl API, so your key never goes to another host. While a job is still running, they stop when they have caught up and leave `next` set.

## Webhooks

Pass `webhook: {url, events?, metadata?}` to `crawl` or `batch-scrape` to receive `crawl.started`, `crawl.page`, `crawl.completed`, and `batch_scrape.*` events instead of polling. Firecrawl signs each delivery with HMAC-SHA256 over the raw body in `X-Firecrawl-Signature: sha256=<hex>`:

```hot
firecrawl-webhook
meta {webhook: {service: "firecrawl", path: "/firecrawl/webhook"}}
fn (request) {
  if(not(::firecrawl::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: {received: request.body.type}}))
}
```

`verify-request(request)` reads `firecrawl.webhook.secret` and fails when it is unset; `verify-request(request, secret)` takes it explicitly, and an empty secret never verifies. Firecrawl signs no timestamp, so there is no replay window to enforce. Deduplicate on the payload's `webhookId` if a replayed delivery would matter.

## Errors

Non-2xx responses return an Err carrying `::firecrawl::api/HttpError`: `status`, a readable `message` built from Firecrawl's `error` field (for example `Firecrawl API error 402: Payment required`), `headers`, and the raw `body`. Transport failures come back as the HTTP client's Err. A missing API key is an Err before any request is sent.

## Per-call credentials

Every function that calls Firecrawl takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `firecrawl.api.key` |
| `base-url` | Another API root, such as a self-hosted Firecrawl (`https://firecrawl.example.com/v2`). Needs an `api-key` in the same `opts`; `firecrawl.api.key` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "fc-tenant-b"}
page ::firecrawl/scrape("https://example.com", {formats: ["markdown"]}, tenant)
job ::firecrawl/crawl("https://docs.firecrawl.dev", {limit: 10}, tenant)
site ::firecrawl/get-crawl-all(job.id, tenant)
credits ::firecrawl/credit-usage(tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `firecrawl.api.key`. `::firecrawl::api/resolve(opts)` is the one place this is decided. `::firecrawl::webhooks/verify-request` takes an explicit secret too, and masks it the same way. `get-*-all` only follows `next` links under the call's own API root.

## Notes and limits

- **Always set `limit` on crawls.** Firecrawl's default is 10,000 pages, and each page costs credits.
- `extract` was synchronous in older Firecrawl APIs. In v2 it only starts the job, so read results through `get-extract` from a later run.
- Firecrawl's free plan includes a one-time batch of credits, enough for the integration test (one scrape of example.com). There is no separate sandbox.
- `::firecrawl::api/request(method, path, body)` reaches any other v2 endpoint (`/agent`, `/scrape/{id}/interact`, ...). Pass `""` as `body` for requests without one.

## License

Apache-2.0 - see [LICENSE](LICENSE)
