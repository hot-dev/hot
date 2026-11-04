# fdic-bankfind

The [FDIC BankFind Suite API](https://api.fdic.gov/banks/docs/) for Hot, with an optional key. Covers institutions, branches, mergers and other structure changes, failures, quarterly financials, and yearly summaries.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/fdic-bankfind": "0.1.0"
}
```

## Auth and settings

FDIC doesn't require a key today. Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::fdic::bankfind::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `fdic.bankfind.api-key` | `api-key` | none | Optional (secret). An [api.data.gov](https://api.data.gov/signup/) key, sent as the `api_key` query parameter |
| `fdic.bankfind.base-url` | `base-url` | `https://api.fdic.gov/banks` | API root |

## Per-call credentials

```hot
::b ::fdic::bankfind

::b/institution(628, {api-key: "tenant-fdic-key"})
```

`api-key` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` without an `api-key` in the same `opts` sends no key: the context key is never sent to a per-call URL.

## Quick start

```hot
::b ::fdic::bankfind

// One bank by FDIC certificate number
chase ::b/institution(628, {fields: ["NAME", "ACTIVE", "ASSET", "REGAGNT"]})
chase.NAME

// A filtered list: a Map, or the API's own query string
page ::b/institutions({
  filters: {STALP: "IA", ACTIVE: 1},
  fields: ["CERT", "NAME", "CITY", "ASSET"],
  sort-by: "ASSET",
  sort-order: "DESC",
  limit: 25
})
::b/rows(page)
::b/total(page)
::b/institutions({filters: "DEP:[50000 TO *] AND ACTIVE:1", fields: "CERT,NAME"})

// Find a bank by a rough name
::b/institutions({search: "NAME:JPMorgan", fields: "CERT,NAME,ACTIVE"})

// Mergers and structure changes for the bank itself
::b/rows(::b/institution-history(628, {limit: 100}))

// What absorbed First Republic?
::b/rows(::b/history({filters: {OUT_CERT: 59017, ORG_ROLE_CDE: "FI"}}))

// Failures and financials
::b/failure(59017)
::b/failures({filters: {FAILYR: 2023}, sort-by: "FAILDATE", sort-order: "DESC"})
::b/institution-financials(628, {fields: "REPDTE,ASSET,DEP,ROA", limit: 4})
::b/summary({filters: "STNAME:\"Iowa\" AND YEAR:2023"})
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::fdic::bankfind` | Lists: `institutions`, `locations`, `history`, `failures`, `financials`, `summary`. By certificate: `institution`, `branches`, `institution-history`, `failure`, `institution-financials`. Reading responses: `rows`, `total`. Paging: `next-page`, `collect`. Helpers: `filter-string`, `and-filters`, `phrase`, `list-params` |
| `::fdic::bankfind::api` | `resolve`, `request`, `get-json`, `decode-body`, `query-string`, `redact-url`, `redact`, `redact-value`, `drop-key`, `http-error`, `not-json-error`; type `HttpError` |

Every list takes `filters`, `fields`, `sort-by`, `sort-order`, `limit` and `offset` in `opts`. `institutions` and `history` also take `search`. `history`, `failures`, `financials` and `summary` also take `agg-by`, `agg-term-fields`, `agg-sum-fields` and `agg-limit`.

`filters` is a Map or the API's [query string](https://api.fdic.gov/banks/docs/#filter-syntax). Use a Map for outside text: it quotes values, so a name can't change the query. `phrase` does the same for one value in a hand-built string.

## Pagination

`limit` defaults to 10 and goes up to 10,000, and `offset` skips records. `next-page(page)` gives the next offset, and `collect(fetch, opts, limit)` follows it, 1,000 rows a page unless `opts.limit` says otherwise.

```hot
::b ::fdic::bankfind

offices ::b/collect((o) { ::b/branches(628, o) }, {fields: "UNINUM,OFFNAME,CITY,STALP"}, 6000)
```

Offsets past 10,000 work: a live query at `offset=10000` returned rows. `next-page` and `collect` don't apply to `agg-by` queries, where `meta.total` counts the records aggregated, not the rows returned.

## Errors

Non-2xx responses come back as an Err carrying `::fdic::bankfind::api/HttpError`:

- `source`: `"fdic-bankfind"`
- `status`
- `url`, `headers`, `body`: redacted, so none of them carries `api_key`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from FDIC's own error text, with no key in it

A 2xx body that isn't JSON, such as a maintenance page, is an `HttpError` too, with `status` 200. A transport failure is a plain Err with the key redacted too. `institution` and `failure` return null when nothing matches, and lists return an empty page. `collect` returns an Err in place of the rows if any page fails. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Key.** FDIC's docs say users are "currently not required to register" for a key, and the spec marks `api_key` optional. Every live check here ran without one. The key path has not been run against the live API. The API echoes query parameters in `meta.parameters`, so `api_key` is dropped from there, though no real key has shown whether it appears.
- **Host.** `api.fdic.gov/banks` is the current API. `banks.data.fdic.gov/api/...` still answers, with a 301 to the same path on `api.fdic.gov/banks`. `banks.data.fdic.gov` remains the BankFind website.
- **Rate limits.** FDIC publishes none, and its docs give no terms of use. The API sits behind the api.data.gov gateway, whose responses without a key carried `x-ratelimit-limit: 20` and a reset time about 2 seconds ahead, which looks like a short rolling window. The gateway's [default for a key](https://api.data.gov/docs/developer-manual/) is 1,000 requests an hour, and limits may vary by service. Whether a key changes FDIC's limit is not stated.
- **Units.** Dollar amounts are in thousands: `ASSET: 4091315000` is about $4.09 trillion.
- **Dates and types vary by dataset.** Filters take `YYYY-MM-DD`, but responses mix formats: `DATEUPDT` is `03/06/2025`, `FAILDATE` is `11/3/2023`, `PROCDATE` is `2026-10-29T00:00:00`, and `REPDTE` is `20260630`. `CERT` is a number from `/institutions` and a string from `/locations`.
- **Uppercase names.** Field names in `filters`, `fields` and `sort-by` must be uppercase.
- **Inactive banks.** `institutions` includes closed and merged banks. Filter on `ACTIVE:1` for open ones.
- **Branch events dominate history.** JPMorgan Chase Bank has 13,414 history rows and 51 of its own, so `institution-history` filters on `ORG_ROLE_CDE:FI`.
- **Financial fields.** `/financials` has over 2,000 fields, so name the ones you want. `limit` falls to 500 when `fields` has more than 250, and wide pages can pass the 16 MiB response cap in `::hot::http`.
- The spec calls the API "Beta". `/sod` (Summary of Deposits) and `/demographics` aren't covered. CSV output isn't either.

## License

Apache-2.0 - see [LICENSE](LICENSE)
