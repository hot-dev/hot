# legiscan

The [LegiScan API](https://legiscan.com/legiscan) for Hot, with a free key. Covers sessions, master bill lists, bills, bill text and full-text search for all US states and Congress.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/legiscan": "0.1.1"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::legiscan::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `legiscan.api-key` | `api-key` | none | **Required** (secret). A free key from [legiscan.com/legiscan](https://legiscan.com/legiscan) |
| `legiscan.base-url` | `base-url` | `https://api.legiscan.com` | API root |

LegiScan takes the key in the query string, as it offers no header. Every `url`, `message`, `body` and `headers` in an error is redacted, and so is the text of a transport failure. `api-key` from `opts` is masked in call records. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL.

## Quick start

```hot
::lg ::legiscan

// Sessions, then every bill's change_hash in one
sessions ::lg/session-list("CA")
session-id first(sessions).session_id
rows ::lg/master-list-raw(session-id)

// A bill and its latest text
bill ::lg/bill(first(rows).bill_id)
text ::lg/bill-text(last(bill.texts).doc_id)
text.mime
text.bytes

// Full-text search
found ::lg/search("money transmitter", {state: "CA"})
found.summary.count
first(found.results).title
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::legiscan` | Sessions: `session-list`. Lists: `master-list`, `master-list-raw`, `bill-rows`. Bills: `bill`, `bill-text`, `decode-text`. Search: `search`, `search-raw`, `search-params`, `search-result`. Changes: `changed-bills`, `hash-index`. Paging: `next-page`, `collect`. Helpers: `session-params` |
| `::legiscan::api` | `resolve`, `require-api-key`, `request`, `get-op`, `operation-url`, `decode-body`, `check-body`, `query-string`, `redact`, `redact-value`, `http-error`; type `HttpError` |

`master-list` and `master-list-raw` take a `session_id`, or a state abbreviation for its current session, which LegiScan says to use with caution.

## Avoiding repeat pulls

Each bill has a `change_hash`. Compare it with the one you stored and call `bill` only for those that differ, which is the workflow LegiScan's manual gives. The package keeps no state, so you hold the hashes: `changed-bills` takes them, and `hash-index` builds the Map to store.

```hot
::lg ::legiscan

rows ::lg/master-list-raw(1791)
changed ::lg/changed-bills(rows, stored-hashes)
bills map(changed, (row) { ::lg/bill(row.bill_id) })
next-hashes ::lg/hash-index(rows)
```

## Pagination

`search` returns 50 results a page and `search-raw` up to 2,000. `next-page(result)` reads `summary`, and `collect(fetch, opts, limit)` follows it. Each page spends one query.

```hot
::lg ::legiscan

rows ::lg/collect((o) { ::lg/search-raw("money transmitter", o) }, {state: "CA"}, 5000)
```

## Errors

LegiScan reports its own errors with HTTP 200 and `status: "ERROR"`. Those, and non-2xx responses, come back as an Err carrying `::legiscan::api/HttpError`:

- `source`: `"legiscan"`
- `status`: the HTTP status, so 200 for LegiScan's own errors
- `url`, `headers`, `body`: redacted
- `alert`: LegiScan's `alert.message`, when there is one
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary, never the key

A body with any `status` other than `"OK"`, or one that isn't valid JSON, is an Err, so a bill that doesn't exist is an Err rather than null: the manual doesn't document how to tell that from other errors. A missing API key is an Err before any request. Nothing retries inside a run.

## Notes and limits

- **Query limit**: a public key has 30,000 queries a month. Paid Pull plans have 100,000 to 250,000.
- **Frequency**: the [manual](https://legiscan.com/misc/LegiScan_API_User_Manual.pdf) gives the shortest useful interval for each operation: daily for `session-list`, 1 hour for `master-list`, `master-list-raw`, `search` and `search-raw`, 3 hours for `bill`, and `bill-text` never changes. Calling sooner returns unchanged cached data and still spends a query. LegiScan says most uses are served by daily updates.
- **Licence**: LegiScan states the API is licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), so credit LegiScan when you use the data.
- **Terms**: the site's [terms of service](https://legiscan.com/terms-of-service) bar automated access, and reproducing or reselling the Services, without a separate agreement. They don't say how that sits with an API key, so read them against your use and ask LegiScan if it matters.
- **Bill text**: a document is base64 inside JSON, so it must fit in `http.max-response-bytes` (16 MiB by default) in that form. `bill-text` decodes it to `bytes` and drops `doc`.
- **Other operations**: amendments, supplements, roll calls, people, datasets and the GAITS monitor list are not wrapped.

## License

Apache-2.0 - see [LICENSE](LICENSE)
