# congress-gov

The [Congress.gov API](https://api.congress.gov/) for Hot, with a free api.data.gov key. Covers bills and resolutions: lists, detail, actions, text versions and CRS summaries.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/congress-gov": "0.1.0"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::congress-gov::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `congress-gov.api-key` | `api-key` | none | **Required** (secret). A free key from [api.congress.gov/sign-up](https://api.congress.gov/sign-up/), sent in the `X-Api-Key` header |
| `congress-gov.base-url` | `base-url` | `https://api.congress.gov/v3` | API root |

`api-key` from `opts` is masked in call records. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL.

## Quick start

```hot
::cg ::congress-gov

// House bills in the 119th Congress updated since Nov 1
page ::cg/bills(119, "hr", {limit: 50, from-date-time: "2026-11-01T00:00:00Z"})

// One bill, its latest actions, text links and CRS summaries
bill ::cg/bill(117, "hr", 3076)
bill.latestAction.text
acts ::cg/actions(117, "hr", 3076, {limit: 10})
texts ::cg/text-versions(117, "hr", 3076)
first(first(texts.textVersions).formats).url
crs ::cg/summaries(117, "hr", 3076)
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::congress-gov` | Bills: `bills`, `bill`, `actions`, `text-versions`, `summaries`. Paging: `next-offset`, `page-items`, `collect`, `page-params`, `update-params`. Helpers: `bill-path`, `bill-type` |
| `::congress-gov::api` | `resolve`, `require-api-key`, `request`, `get-json`, `get-json-or-null`, `query-string`, `path-segment`, `http-error`, `is-not-found`; type `HttpError` |

## Pagination

Lists take `limit` (default 20, at most 250) and `offset`. `next-offset(page)` reads the offset out of the API's `pagination.next` link, and `collect(fetch, opts, limit)` follows it. Don't add the row count to the offset yourself: the text endpoint has returned three rows for `limit=2`.

```hot
::cg ::congress-gov

all-actions ::cg/collect((o) { ::cg/actions(117, "hr", 3076, o) }, {}, 500)
```

## Errors

Non-2xx responses, and 2xx responses that aren't JSON, come back as an Err carrying `::congress-gov::api/HttpError`:

- `source`: `"congress-gov"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` when the response has one
- `message`: a one-line summary built from the API's own error text, never a credential

`bill` returns null rather than a 404 Err when there is no such bill. A bad bill type or number, or a missing API key, is an Err before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Rate limit**: [5,000 requests per hour](https://github.com/LibraryOfCongress/api.congress.gov#rate-limit). The `X-RateLimit-Limit` and `X-RateLimit-Remaining` response headers show where you stand; `DEMO_KEY` is for trying the API and has a far lower limit ([api.data.gov](https://api.data.gov/docs/developer-manual/)).
- **Terms**: Congress.gov asks for no attribution, though it offers a citation: *Data retrieved from Congress.gov (https://www.congress.gov/)*. Keys follow the data.gov privacy policy and content follows the Library of Congress privacy policy.
- **Dates**: `from-date-time` and `to-date-time` filter on the record's update date, not an action date. That date moves whenever Congress.gov receives an update, so a bill can reappear with no new legislative action. They apply to `bills` and `actions`, not `text-versions` or `summaries`.
- **Text**: `text-versions` returns links to congress.gov, not the text itself. `date` is null on some versions.
- **Summaries**: `text` is HTML, and the API's own docs warn that it may not be valid.
- **Coverage**: bills from the 6th to 42nd Congresses (1799-1873) have partial data; see the API's [bill notes](https://github.com/LibraryOfCongress/api.congress.gov/blob/main/Documentation/BillEndpoint.md).

## License

Apache-2.0 - see [LICENSE](LICENSE)
