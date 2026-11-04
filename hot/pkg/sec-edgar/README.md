# sec-edgar

[SEC EDGAR](https://www.sec.gov/search-filings/edgar-application-programming-interfaces) for Hot: US filers, with no key but a contact User-Agent. Covers ticker-to-CIK lookups, submissions and filings, XBRL company facts, concepts and frames, filing documents, and full-text search.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/sec-edgar": "0.1.0"
}
```

## Auth and settings

The User-Agent comes from per-call `opts` first, then the Context Variable (`::sec::edgar::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. A per-call User-Agent is never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `sec.edgar.user-agent` | `user-agent` | none | **Required.** Your company name and a contact email, e.g. `"Acme Corp ops@acme.com"`. Calls fail before any request without it |

## Per-call User-Agent

```hot
::edgar ::sec::edgar

::edgar/submissions(320193, {user-agent: "Tenant Co ops@tenant.example"})
```

`user-agent` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable.

## Quick start

```hot
::edgar ::sec::edgar

// Ticker to CIK to filings
cik ::edgar/cik-for-ticker("AAPL")            // "0000320193"
subs ::edgar/submissions(cik)
tenk first(::edgar/recent-filings(subs, 1, {forms: ["10-K"]}))
tenk.url        // the primary document

// XBRL facts
concept ::edgar/company-concept(cik, "us-gaap", "AccountsPayableCurrent")
f ::edgar/frames("us-gaap", "AccountsPayableCurrent", "USD", "CY2019Q1I")

// Full-text search
res ::edgar/full-text-search("\"climate risk\"", {forms: ["10-K"], start-date: "2025-01-01", end-date: "2025-03-31"})
res.hits.total.value
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::sec::edgar` | Tickers: `company-tickers`, `parse-tickers`, `find-ticker`, `search-tickers`, `cik-for-ticker`. Filings: `submissions`, `submissions-page`, `recent-filings`, `filing-index`, `filing-document`. XBRL: `company-facts`, `company-concept`, `frames`. Search: `full-text-search`, `full-text-params`. URLs: `pad-cik`, `cik-digits`, `accession-plain`, `submissions-url`, `company-facts-url`, `company-concept-url`, `frames-url`, `filing-folder-url`, `filing-document-url`, `filing-index-url` |
| `::sec::edgar::api` | `resolve`, `require-user-agent`, `request`, `get-json`, `query-string`, `path-segment`, `http-error`, `columns-to-rows`; type `HttpError` |

## Pagination

Submissions carry at least a year of filings, up to 1,000, in `filings.recent`. Older pages are listed in `filings.files` and read with `submissions-page`. `recent-filings` turns the columnar arrays into rows and adds each document's `url`.

## Documents

```hot
::edgar ::sec::edgar

doc ::edgar/filing-document(320193, "0000320193-25-000079", "aapl-20250927.htm")
doc.content-type
doc.bytes
```

A document must fit in `http.max-response-bytes` (16 MiB by default).

## Errors

Non-2xx responses come back as an Err carrying `::sec::edgar::api/HttpError`:

- `source`: `"sec-edgar"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from SEC's own error text, never the User-Agent

A missing User-Agent is an Err that names the context variable to set, before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Fair access**: at most **10 requests per second** per client, across all SEC hosts. SEC blocks clients that exceed it or send no User-Agent.
- `company_tickers.json` is about 800 KB and changes daily: fetch it once per run (`company-tickers`) and reuse it with `find-ticker` and `search-tickers`. Large filers' `company-facts` run to several MB.
- `search-tickers` is a substring match on ticker-file names, so it only finds companies with listed tickers. To find other filers, use `full-text-search`, or look up by CIK.
- **Full-text search** (`efts.sec.gov/LATEST/search-index`) is the endpoint behind EDGAR's full-text search page. SEC does not document it as a public API, so its response shape can change. It covers filings since 2001.
- EDGAR has no sandbox. It is a free public API and needs no sales contract.

## License

Apache-2.0 - see [LICENSE](LICENSE)
