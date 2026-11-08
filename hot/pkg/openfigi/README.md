# openfigi

[OpenFIGI](https://www.openfigi.com/api) for Hot. OpenFIGI is Bloomberg's free, open symbology API: it maps identifiers such as ISINs, CUSIPs, SEDOLs and tickers to FIGIs (Financial Instrument Global Identifiers), and searches FIGIs by keyword and attributes.

OpenFIGI returns identifiers and descriptive fields only: `figi`, `name`, `ticker`, `exchCode`, `securityType`, `securityType2`, `marketSector`, `compositeFIGI`, `shareClassFIGI` and `securityDescription`. It has no prices, history or reference data. Use it to normalize the identifiers you hold, for example before requesting data from a market-data provider that accepts FIGIs.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/openfigi": "0.1.1"
}
```

## Auth and settings

OpenFIGI works without a key, at lower limits. A free key from [openfigi.com](https://www.openfigi.com/api#api-key) raises them. Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::openfigi::api/resolve`).

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `openfigi.api-key` | `api-key` | none | Optional (secret). Sent as the `X-OPENFIGI-APIKEY` header |
| `openfigi.base-url` | `base-url` | `https://api.openfigi.com/v3` | API root |

An `api-key` in `opts` is used for that call only: it is masked in call records and never written to context. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` only gets a per-call `api-key`: the context key is never sent to a per-call URL.

## Limits

Per 60-second window, per key, or per IP address without one:

| | Without a key | With a key |
|---|---|---|
| `/mapping` and `/mapping/values` requests | 25 | 250 |
| Jobs per `/mapping` request | 10 | 100 |
| `/search` and `/filter` requests | 5 | 20 |

Search and filter return up to 100 results a page, and at most 15,000 in all. `map-jobs` refuses a batch over the per-request limit before sending it; `map-all` splits any list into batches of that size.

## Quick start

```hot
::figi ::openfigi

// One identifier
::figi/by-isin("US0378331005", {filters: {exchCode: "US"}})   // [{figi: "BBG000B9XRY4", ticker: "AAPL", ...}]
first(::figi/by-ticker("IBM", "US")).figi                       // "BBG000BLNNH6"
::figi/by-figi("BBG000B9XRY4").name                             // "APPLE INC"

// Many identifiers, in batches, results in job order
isins ["US0378331005", "US4592001014", "US5949181045"]
out ::figi/map-all(map(isins, (i) { ::figi/job("ID_ISIN", i, {exchCode: "US"}) }))
map(out, (r) { {isin: r.job.idValue, figi: get(first(r.figis), "figi"), problem: or(r.warning, r.error)} })

// Keyword search and attribute filters
page ::figi/search("apple", {filters: {exchCode: "US", securityType: "Common Stock"}})
page.next                                                        // a token for the next page, when there is one
::figi/filter-figis({filters: {exchCode: "US", securityType2: "Common Stock"}}).total

// Follow page tokens
::figi/collect((o) { ::figi/search("ibm", o) }, {filters: {exchCode: "US"}}, 300)

// Valid field values, and FIGI syntax checks
::figi/values("exchCode")
::figi/is-figi("BBG000B9XRY4")                                   // true
```

Request fields keep OpenFIGI's own names (`idType`, `idValue`, `exchCode`, `micCode`, `currency`, `marketSecDes`, `securityType`, `securityType2`, `includeUnlistedEquities`, `optionType`, `strike`, `contractSize`, `coupon`, `expiration`, `maturity`, `stateCode`), so the [OpenFIGI documentation](https://www.openfigi.com/api/documentation) applies as written. A few rules the API enforces per job: `exchCode` and `micCode` can't be used together; `securityType2` is required with `BASE_TICKER` and `ID_EXCH_SYMBOL`; options and warrants need `expiration`, and pools need `maturity`.

## Functions

| Namespace | Functions |
|---|---|
| `::openfigi` | Mapping: `job`, `map-jobs`, `map-all`, `mapping-results`, `map-id`, `by-isin`, `by-cusip`, `by-sedol`, `by-ticker`, `by-figi`. Search: `search`, `filter-figis`, `search-body`, `collect`. Fields: `values`. Syntax: `is-figi`, `check-digit` |
| `::openfigi::api` | `resolve`, `request`, `auth-headers`, `http-error`, `path-segment`, `given`; type `HttpError`; `LIMITS` |

## Results and errors

- `map-jobs` returns OpenFIGI's raw results in job order: `{data: [...]}`, `{warning: "No identifier found."}`, or `{error: "..."}` for a job OpenFIGI rejects.
- `map-all` and `mapping-results` pair each job with its result: `{job, figis, warning, error}`, with `figis` always a Vec.
- `map-id` and the `by-*` helpers return `[]` when nothing matches, and an Err when OpenFIGI rejects the job (e.g. `"Invalid idValue format."`). `by-figi` returns one record or null.
- `search` and `filter-figis` return `{data, next}`, and `filter-figis` adds `total`. An `error` in the body becomes an Err.
- A non-2xx response comes back as an Err carrying `::openfigi::api/HttpError`: `status`, `url`, `headers`, `body`, `retry-after` (seconds, from `Retry-After` or, on a 429, `ratelimit-reset`) and `message`. The key is sent in a header, so it never appears in a URL or message.

Nothing retries inside a run. `map-all` stops at the first failed batch. Use a handler or schedule `retry` policy for 429s and 5xx, and keep each run under the limits above.

## Tests

- Unit tests (hermetic): `hot test -p pkg-test-openfigi`
- Live, read-only: `OPENFIGI_LIVE=1 hot test -p pkg-integration-openfigi`, optionally with `OPENFIGI_API_KEY`

## License

Apache-2.0 - see [LICENSE](LICENSE)
