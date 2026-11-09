# courtlistener

The [Free Law Project CourtListener REST API v4](https://www.courtlistener.com/help/api/rest/) for Hot. Covers case-law and RECAP (PACER) search, dockets, docket entries, opinions and clusters by id, the court list, and cursor paging.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/courtlistener": "0.1.2"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::courtlistener::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `courtlistener.api-token` | `api-token` | none | **Required** for all calls except `courts` and `court` (secret). A token from a free CourtListener account, sent as `Authorization: Token <token>` |
| `courtlistener.base-url` | `base-url` | `https://www.courtlistener.com/api/rest/v4` | API root |

### What needs a token

Checked without a token on 4 Nov 2026:

| Endpoint | Without a token |
|---|---|
| API root, `/courts/`, `/courts/{id}/` | 200 |
| `/search/`, `/dockets/`, `/docket-entries/`, `/clusters/`, `/opinions/`, and `OPTIONS` on `/dockets/` | 401 |

The API documentation says many APIs are "open by default", but the endpoints above are not, so this package fails fast with an Err naming `courtlistener.api-token` before it sends a request. It also says some APIs "are only available to select users" without listing them all. It names the Fast Document Lookup API (`/recap-query/`), which this package doesn't wrap. A 403 from a call here means your account lacks access to that API. A paid membership raises rate limits only. The documentation doesn't tie any endpoint in this package to a membership.

## Per-call credentials

```hot
::cl ::courtlistener

::cl/docket(4214664, {api-token: "tenant-token"})
```

`api-token` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` needs an `api-token` in the same `opts`: the context token is never sent to a per-call URL.

## Quick start

```hot
::cl ::courtlistener

// Case law, newest first. Filter with fielded terms in the query.
page ::cl/search-opinions("\"data breach\" court_id:ca9", {order-by: "dateFiled desc"})
hit first(page.results)
hit.caseName

// From a search hit to the cluster and its docket
cluster ::cl/cluster(hit.cluster_id)
docket ::cl/docket(::cl/resource-id(cluster.docket))

// Federal cases with their filings, then the entries on one docket
recap ::cl/search-recap("caseName:\"acme corp\" dateFiled:[2026-01-01 TO 2026-06-30]")
entries ::cl/docket-entries(first(recap.results).docket_id, {order-by: "-id"})

// A docket by number. Numbers are not unique, so give the court.
found ::cl/dockets({court: "dcd", docket_number: "1:16-cv-00745"}, {fields: ["id", "case_name"]})
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::courtlistener` | Search: `search`, `search-opinions`, `search-recap`, `search-dockets`, `search-documents`. Dockets: `docket`, `dockets`, `docket-entries`. Case law: `cluster`, `opinion`. Courts: `courts`, `court`. Paging: `next-params`, `collect`. Helpers: `auth-header`, `search-params`, `refine-params`, `resource-id` |
| `::courtlistener::api` | `resolve`, `require-api-token`, `request`, `get-json`, `get-json-or-null`, `decode-body`, `query-string`, `path-segment`, `http-error`, `not-json-error`, `is-not-found`; type `HttpError` |

`search` takes `type`, `order-by`, `filed-after` and `filed-before` (`MM/DD/YYYY`, as on the search form), `highlight` (true) and `cursor`. `params` adds anything else the search form sends, such as `{stat_Unpublished: "on"}`. By default case-law search returns published opinions only.

`docket`, `dockets`, `docket-entries`, `cluster`, `opinion`, `courts` and `court` take `fields` and `omit` (a Vec or comma-separated Str, `__` for nested fields). The list calls also take `order-by`, `cursor` and `page`. `docket-entries` and `courts` take `filters`, and `dockets` takes them as its first argument: filter names are in the API's `OPTIONS` response (`date_filed__gte`, `court`, `docket_number`).

## Pagination

Responses carry `count`, `next` and `previous`. `next-params(page)` reads `next` and returns `{cursor: ...}` or `{page: n}` to merge into the next call's `opts`, or null on the last page. `collect(fetch, opts, limit)` follows it. Only the parameter is taken from `next`, so the token never follows a URL from a response.

```hot
::cl ::courtlistener

entries ::cl/collect((o) { ::cl/docket-entries(4214664, o) }, {order-by: "id"}, 40)
```

- `page` stops at 100 pages. Going deeper needs ordering by `id`, `date_modified` or `date_created`, which switches `next` to a cursor.
- Search supports deep cursor paging. For `type=d` and `type=r`, `count` is an estimate, within about 6%, above 2,000 results.
- `collect` returns an Err in place of the items if any page fails, and the default limit is 5 requests a minute. Keep `limit` small.

## Errors

Non-2xx responses come back as an Err carrying `::courtlistener::api/HttpError`:

- `source`: `"courtlistener"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from CourtListener's own `detail` text, never a credential

A 2xx body that isn't JSON, such as a maintenance page, is an `HttpError` too, with `status` 200, so `collect` can't read it as an empty page. `docket`, `cluster`, `opinion` and `court` return null rather than a 404 Err. A missing token is an Err that names the context variable to set, before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Rate limits**: by default 5 requests per minute, 50 per hour and 125 per day per account. Windows are rolling and all apply at once. The [documentation](https://wiki.free.law/c/courtlistener/help/api/rest/v4/rest-api-v47) doesn't state a limit for anonymous calls.
- **Higher limits**: [Free Law Project memberships](https://free.law/membership/) run from 10 per minute, 75 per hour and 300 per day ($10 a month) up to 25, 300 and 1,400 ($100 a month). They are for small firms, small government bodies, legal service organizations, small media, academics and pre-revenue organizations, "not for large or funded organizations". Those need a commercial agreement.
- **Terms** ([terms of service](https://wiki.free.law/c/terms/courtlistener/courtlistenercom-terms-of-service-and-policies)): access for a product or a team points to a commercial agreement. Multiple accounts or token rotation to get around rate limits are forbidden, as is sharing or pooling tokens. The service can't be used for FCRA purposes, such as decisions on credit, employment or housing eligibility.
- **Polling**: search results are cached for ten minutes, and the documentation recommends its alerts and webhooks over polling. Those aren't wrapped here.
- **Query logging**: search queries are logged unless logging is turned off in the account profile. Consider this before sending names you must keep confidential.
- **Ids**: the id in a CourtListener opinion URL is a cluster id, and opinion ids do not reliably match it. Clusters and opinions link parents by URL. `resource-id` gets the id.
- **Opinion text**: prefer `html_with_citations` to `plain_text`.
- **Data handling**: court records contain personal data. The package logs nothing, and errors carry URLs and statuses only.

## License

Apache-2.0 - see [LICENSE](LICENSE)
