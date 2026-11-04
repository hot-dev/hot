# sec-edgar

[SEC EDGAR](https://www.sec.gov/search-filings/edgar-application-programming-interfaces) for Hot: US filers, with no key but a contact User-Agent. Covers ticker-to-CIK lookups, submissions and filings, XBRL company facts, concepts and frames, filing documents, full-text search, and Form D notices of exempt offerings.

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

// Form D: a date window, then one filing
page ::edgar/form-d-filings("2026-10-20", "2026-10-31")
row first(page.filings)
d ::edgar/form-d(first(row.ciks), row.accession)
d.issuer.name
d.total-offering-amount   // an Int, or "Indefinite"
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::sec::edgar` | Tickers: `company-tickers`, `parse-tickers`, `find-ticker`, `search-tickers`, `cik-for-ticker`. Filings: `submissions`, `submissions-page`, `recent-filings`, `filing-index`, `filing-document`. XBRL: `company-facts`, `company-concept`, `frames`. Search: `full-text-search`, `full-text-params`. Form D: `form-d`, `parse-form-d`, `form-d-url`, `form-d-filings`, `collect-form-d-filings`, `form-d-page`, `form-d-filing`, `form-d-dataset`, `parse-form-d-dataset`, `form-d-dataset-url`. URLs: `pad-cik`, `cik-digits`, `accession-plain`, `submissions-url`, `company-facts-url`, `company-concept-url`, `frames-url`, `filing-folder-url`, `filing-document-url`, `filing-index-url` |
| `::sec::edgar::api` | `resolve`, `require-user-agent`, `request`, `get-json`, `query-string`, `path-segment`, `http-error`, `is-not-found`, `columns-to-rows`; type `HttpError` |

## Pagination

Submissions carry at least a year of filings, up to 1,000, in `filings.recent`. Older pages are listed in `filings.files` and read with `submissions-page`. `recent-filings` turns the columnar arrays into rows and adds each document's `url`.

`form-d-filings` returns one page of 100 with `next-from`; pass that as `opts.from`, or use `collect-form-d-filings` to follow the pages up to a limit. EDGAR serves only the first 10,000 hits of a search.

## Documents

```hot
::edgar ::sec::edgar

doc ::edgar/filing-document(320193, "0000320193-25-000079", "aapl-20250927.htm")
doc.content-type
doc.bytes
```

A document must fit in `http.max-response-bytes` (16 MiB by default).

## Form D

A Form D is a notice of an exempt offering. There are three ways in.

**One filing.** `form-d(cik, accession)` reads the filing's `primary_doc.xml`, and `parse-form-d` does the same for text you already have. It returns the issuers, related persons, amounts, exemptions, first sale date and whether it's an amendment. The `parse-form-d` docs list every key. Null means the filing has no such document, and a bad CIK is an Err.

```hot
::edgar ::sec::edgar

subs ::edgar/submissions(2162876)
row first(::edgar/recent-filings(subs, 1, {forms: ["D", "D/A"]}))
d ::edgar/form-d(subs.cik, row.accessionNumber)
d.is-amendment            // true
d.previous-accession      // "0002162876-26-000001"
d.exemptions              // ["06b", "3C", "3C.1"]
d.first-sale-date         // "2026-10-15"
map(d.related-persons, (p) { p.name })
```

Use `form-d` or `form-d-url` rather than the row's `url`. A Form D's `primaryDocument` is `xslFormDX01/primary_doc.xml`, which is the HTML rendering. `parse-form-d` refuses it.

**A date window.** `form-d-filings(start-date, end-date)` lists filings through `full-text-search`, newest first, as `{total, total-is-exact, from, filings, next-from}`. Each row is `{accession, form, filed, ciks, names, file-numbers, exemptions, url}`.

`forms` defaults to `["D"]`, which already includes amendments. EDGAR matches `forms=D` on the root form, so it returns D and D/A, while `forms=D,D/A` returns only the D/A filings: for 20 to 31 October 2026, 2,184 hits against 783. Pass `{forms: ["D/A"]}` for amendments alone, and filter on `form` for originals alone.

**A whole quarter.** `form-d-dataset(year, quarter)` reads SEC's [quarterly Form D data sets](https://www.sec.gov/data-research/sec-markets-data/form-d-data-sets), which go back to 2008 Q1. It makes two requests, one for the data sets page to find the zip and one for the zip, and returns null for a quarter SEC doesn't list.

```hot
::edgar ::sec::edgar

d ::edgar/form-d-dataset(2026, 3, {tables: ["offerings", "issuers"]})
length(d.offerings)                  // 15952
first(d.offerings).TOTALOFFERINGAMOUNT
```

`opts.tables` picks from `submissions`, `issuers`, `offerings`, `related-persons`, `recipients` and `signatures`. The default is the first three, because Hot records call values and the other three hold most of the data, including people's names and addresses. Rows keep SEC's column names, every value is a Str, and rows of one filing share `ACCESSIONNUMBER`. Each zip's `FormD_readme.html` defines the columns.

The data sets fit Hot's limits: 2026 Q3 is a 3.6 MB zip that expands to 21 MB, 2022 Q1 is 4.3 MB and 26 MB, and the largest table in either is under 10 MB, against 16 MiB for `http.max-response-bytes` and 64 MiB for `compression.max-output-bytes`. Those are the only recent quarters measured.

## Errors

Non-2xx responses come back as an Err carrying `::sec::edgar::api/HttpError`:

- `source`: `"sec-edgar"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from SEC's own error text, never the User-Agent

A missing User-Agent is an Err that names the context variable to set, before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Fair access**: at most **10 requests per second** per client, across all SEC hosts ([SEC's rules](https://www.sec.gov/search-filings/edgar-search-assistance/accessing-edgar-data)). SEC blocks clients that exceed it or send no User-Agent.
- `company_tickers.json` is about 800 KB and changes daily: fetch it once per run (`company-tickers`) and reuse it with `find-ticker` and `search-tickers`. Large filers' `company-facts` run to several MB.
- `search-tickers` is a substring match on ticker-file names, so it only finds companies with listed tickers. To find other filers, use `full-text-search`, or look up by CIK.
- **Full-text search** (`efts.sec.gov/LATEST/search-index`) is the endpoint behind EDGAR's full-text search page. SEC does not document it as a public API, so its response shape can change. It covers filings since 2001.
- **Full-text search depth**: EDGAR refuses `from` + page size past 10,000, and `hits.total` stops at 10,000 with relation `gte`. A quarter holds about 16,000 Form Ds, so list in narrower windows or use `form-d-dataset`.
- **Form D data sets**: SEC updates them quarterly and says it can't guarantee their accuracy, since they carry what registrants filed. Values aren't normalized, so `FILING_DATE` reads `30-SEP-2026`. Filings made after 5:30 PM Eastern on a quarter's last business day land in the next quarter's set.
- **Form D amounts** are Ints, or `"Indefinite"` as filed. An element the filing leaves out is null. `form-d-filing` exemptions come from the search index, upper-cased (`"06B"`); `parse-form-d` keeps the filing's own (`"06b"`).
- EDGAR has no sandbox. It is a free public API and needs no sales contract.

## License

Apache-2.0 - see [LICENSE](LICENSE)
