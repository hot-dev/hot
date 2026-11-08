# regulations-gov

The [Regulations.gov API](https://open.gsa.gov/api/regulationsgov/) for Hot, with a free api.data.gov key. Covers documents, dockets and public comments, read-only, with a way past the paging ceiling of one query.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/regulations-gov": "0.1.1"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::regulations-gov::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `regulations-gov.api-key` | `api-key` | none | **Required** (secret). A free key from [api.data.gov](https://open.gsa.gov/api/regulationsgov/#getting-started), sent in the `X-Api-Key` header |
| `regulations-gov.base-url` | `base-url` | `https://api.regulations.gov/v4` | API root. The API also has a staging host, `https://api-staging.regulations.gov` |

`api-key` from `opts` is masked in call records. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL.

## Quick start

```hot
::reg ::regulations-gov

// CFPB documents posted since October, newest first
page ::reg/documents({
  filters: {agencyId: "CFPB", postedDate: {ge: "2026-10-01"}},
  sort: "-postedDate",
  page-size: 25
})

// One document and its comments. commentOnId takes the objectId, not the id
doc ::reg/document("FAA-2018-1084-0001")
comments ::reg/comments({filters: {commentOnId: doc.data.attributes.objectId}, page-size: 50})
comments.meta.totalElements

// A docket, with its RIN
docket ::reg/docket("FAA-2018-1084")
docket.data.attributes.rin
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::regulations-gov` | Lists: `documents`, `comments`, `dockets`. Detail: `document`, `comment`, `docket`. Past the ceiling: `all-documents`, `all-comments`, `all-dockets`, `collect-windows`. Paging: `next-page`, `page-params`, `filter-params`, `list-params`. Windows: `eastern-date-time`, `window-opts`, `next-cursor` |
| `::regulations-gov::api` | `resolve`, `require-api-key`, `request`, `get-json`, `get-json-or-null`, `decode-body`, `query-string`, `path-segment`, `http-error`, `not-json-error`, `is-not-found`; type `HttpError` |

`filters` is a Map of field to value. A Map value gives the range forms, so `{postedDate: {ge: "2026-01-01", le: "2026-02-01"}}` sends `filter[postedDate][ge]` and `filter[postedDate][le]`. `postedDate` and `commentEndDate` are `yyyy-MM-dd`. `lastModifiedDate` is `yyyy-MM-dd HH:mm:ss` in Eastern time.

## Pagination

Lists take `page-size` (held to 5 to 250) and `page`, and `next-page(page)` reads `meta`. The [OpenAPI spec](https://open.gsa.gov/api/regulationsgov/v4/openapi.yaml) documents pages 1 to 20, so one query reaches 5,000 records.

To go further, `all-documents`, `all-comments` and `all-dockets` follow the [documented procedure](https://open.gsa.gov/api/regulationsgov/#examples): sort on `lastModifiedDate`, read 20 pages of 250, take the last row's `lastModifiedDate`, convert it from UTC to Eastern time, and restart with `filter[lastModifiedDate][ge]`. The overlap at each boundary is dropped.

```hot
::reg ::regulations-gov

doc ::reg/document("EOIR-2020-0003-0001")
rows ::reg/all-comments({filters: {commentOnId: doc.data.attributes.objectId}}, 20000)
```

A full page 20 restarts from the last row whatever `hasNextPage` says, because that flag can end at the paging ceiling rather than with the data. So exactly 5,000 rows costs one more request. `limit` is required, and each page is one request against the hourly limit: 20,000 rows is at least 80 requests. If a whole window of 5,000 rows shares one timestamp the walk can't move on, and it stops with an Err rather than dropping rows.

## Errors

Non-2xx responses come back as an Err carrying `::regulations-gov::api/HttpError`:

- `source`: `"regulations-gov"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` when the response has one
- `message`: a one-line summary built from the API's own error text, never a credential

A 2xx body that isn't JSON, such as a maintenance page, is an `HttpError` too, with `status` 200, so the `all-*` functions can't read it as an empty page. `document`, `comment` and `docket` return null rather than a 404 Err when there is no such record, and a blank id is an Err. A missing API key is an Err that names the context variable to set, before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Rate limit**: the API refers to [api.data.gov's limits](https://api.data.gov/docs/rate-limits/) without giving its own number, and api.data.gov's default is 1,000 requests per hour, which a service can change. The `X-RateLimit-Limit` and `X-RateLimit-Remaining` response headers show the real figure. GSA considers increases for GET keys case by case. `DEMO_KEY` is for demonstrations only.
- **`lastModifiedDate` is beta**: GSA says it "may be removed" once a permanent bulk download exists. The `all-*` functions depend on it.
- **Paging ceiling**: the docs say pages 1 to 20. On 4 Nov 2026 the API also served page 21 and reported `totalPages: 40` for 418 rows at size 5, so the real ceiling may be 40 pages. The package stays in the documented range.
- **Personal data**: comments are public but carry personal data. The API can return `firstName`, `lastName`, `city`, `organization` and more, as each agency configures it, and titles often name the commenter. The package logs nothing. Store results under your own data-protection policy.
- **Read-only**: posting comments needs a separate key and is not wrapped.
- **Documents**: `withdrawn` marks a withdrawn document, and `documents` doesn't return the RIN: read it from `docket`. File links are in `fileFormats` on `document`.
- **Attachments** are only returned with `{include-attachments: true}`, in `included`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
