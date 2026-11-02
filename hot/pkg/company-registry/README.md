# company-registry

Free company registries for Hot, behind one normalized `Company` record:

- **GLEIF LEI API**: global legal entities, no key. Covers search, fuzzy names, LEI and ISIN lookups, and direct and ultimate parents and children.
- **SEC EDGAR**: US filers, no key but a contact User-Agent. Covers ticker-to-CIK lookups, submissions and filings, XBRL company facts, concepts and frames, filing documents, and full-text search.
- **UK Companies House**: free API key. Covers company and officer search, profiles, officers, persons with significant control, filing history and document downloads.

`lookup` searches every configured registry in parallel and returns `Company` records.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/company-registry": "0.1.0"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::company-registry::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `company-registry.sec.user-agent` | `user-agent` | none | **Required for SEC.** Your company name and a contact email, e.g. `"Acme Corp ops@acme.com"`. SEC calls fail before any request without it |
| `company-registry.companies-house.api-key` | `api-key` | none | **Required for Companies House** (secret). A free REST API key from the [developer hub](https://developer.company-information.service.gov.uk/), sent as the Basic-auth username with an empty password |
| `company-registry.companies-house.base-url` | `companies-house-base-url` | `https://api.company-information.service.gov.uk` | Set it to `https://api-sandbox.company-information.service.gov.uk` (test data, with a sandbox key) |
| `company-registry.companies-house.document-base-url` | `document-base-url` | `https://document-api.company-information.service.gov.uk` | Document API host |
| `company-registry.gleif.base-url` | `gleif-base-url` | `https://api.gleif.org/api/v1` | GLEIF API root |
| `company-registry.sources` | `sources` | GLEIF, plus every source whose credential is set | Default sources for `lookup` (a Vec or a comma list) |

## Per-call credentials

Every API function takes a trailing `opts` Map, so one run can serve many tenants.

```hot
tenant {api-key: "tenant-ch-key", user-agent: "Tenant Co ops@tenant.example"}
::company-registry::companies-house/company("00445790", tenant)
::company-registry::sec/submissions(320193, tenant)
```

`api-key` and `user-agent` from `opts` are masked in call records and never written to ctx. Present but blank or null, either one is an error naming the key, never a fallback to the Context Variable. A per-call `companies-house-base-url` or `document-base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL. GLEIF takes no credential, so `gleif-base-url` needs none.

## Quick start

```hot
::cr ::company-registry
::gleif ::company-registry::gleif
::sec ::company-registry::sec
::ch ::company-registry::companies-house

// Every configured registry at once
found ::cr/lookup("Tesco PLC", {limit: 3})
map(found.companies, (c) { `${c.source}: ${c.name} ${to-json(c.identifiers)}` })
found.errors   // [{source, message}] for any registry that failed

// GLEIF: search, then walk up the ownership tree
page ::gleif/search("apple inc", {page-size: 5})
apple ::cr/with-parents(::cr/from-gleif(first(page.data)))
apple.parents   // [{relationship: "direct", lei, name}, {relationship: "ultimate", ...}]

// SEC: ticker to CIK to filings
cik ::sec/cik-for-ticker("AAPL")            // "0000320193"
subs ::sec/submissions(cik)
tenk first(::sec/recent-filings(subs, 1, {forms: ["10-K"]}))
tenk.url        // the primary document

// Companies House: profile, officers and control
profile ::ch/company("00445790")
psc ::ch/persons-with-significant-control("00445790")
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::company-registry` | `lookup`, `lookup-source`, `from-gleif`, `with-parents`, `from-sec`, `from-sec-ticker`, `from-companies-house`; types `Company`, `Address` |
| `::company-registry::gleif` | Records: `search`, `search-legal-name`, `fuzzy-completions`, `autocompletions`, `get-record`, `by-isin`, `isins`. Relationships: `direct-parent`, `ultimate-parent`, `direct-parent-relationship`, `ultimate-parent-relationship`, `direct-parent-exception`, `ultimate-parent-exception`, `direct-children`, `ultimate-children`. Paging: `next-page`, `collect`. Helpers: `is-lei`, `lei-path`, `search-params`, `filter-params`, `page-params`, `completions` |
| `::company-registry::sec` | Tickers: `company-tickers`, `parse-tickers`, `find-ticker`, `search-tickers`, `cik-for-ticker`. Filings: `submissions`, `submissions-page`, `recent-filings`, `filing-index`, `filing-document`. XBRL: `company-facts`, `company-concept`, `frames`. Search: `full-text-search`, `full-text-params`. URLs: `pad-cik`, `cik-digits`, `accession-plain`, `submissions-url`, `company-facts-url`, `company-concept-url`, `frames-url`, `filing-folder-url`, `filing-document-url`, `filing-index-url` |
| `::company-registry::companies-house` | Search: `search-companies`, `search-officers`, `search-all`, `search-disqualified-officers`. Company: `company`, `registered-office-address`, `officers`, `officer-appointments`, `persons-with-significant-control`, `psc-statements`, `filing-history`, `filing`, `charges`. Documents: `document-metadata`, `document-content`, `document-id`. Paging: `next-start`, `collect`, `page-params`. Helpers: `auth-header`, `company-number`, `officer-id-from-link` |
| `::company-registry::api` | `resolve`, `require-user-agent`, `require-api-key`, `request`, `get-json`, `get-json-or-null`, `query-string`, `path-segment`, `http-error`, `is-not-found`, `columns-to-rows`; type `HttpError` |

## The Company record

| Field | GLEIF | SEC | Companies House |
|---|---|---|---|
| `name` | legal name | conformed name | company name (`title` in search results) |
| `identifiers` | `lei`, `registered-as`, `bic` | `cik` (10 digits), `tickers`, `ein`, `lei` | `uk-company-number` |
| `jurisdiction` | e.g. `"IE"`, `"US-DE"` | `US-{state of incorporation}` | `GB-EAW`, `GB-SCT`, `GB-NIR` |
| `status` | `active` / `inactive` | none | `active`, `dissolved`, `liquidation`, ... |
| `addresses` | `legal`, `headquarters` | `business`, `mailing` | `registered-office` |
| `incorporation-date` | `creationDate` | none (not published) | `date_of_creation` |
| `parents` | filled by `with-parents` | none | none (use `persons-with-significant-control`) |
| `raw` | the record | the submissions body or ticker row | the profile or search item |

Fields a source doesn't publish are null or empty. Identifier keys appear only when known.

## Pagination

- **GLEIF** pages by number: `page-size` (up to 200) and `page`. `next-page(page)` reads `meta.pagination`, and `collect(fetch, opts, limit)` follows it.
- **Companies House** pages by offset: `items-per-page` (up to 100) and `start-index`. `next-start(page)` and `collect(fetch, opts, limit)` follow `total_results`.
- **EDGAR** submissions carry at least a year of filings, up to 1,000, in `filings.recent`. Older pages are listed in `filings.files` and read with `submissions-page`. `recent-filings` turns the columnar arrays into rows and adds each document's `url`.

```hot
::gleif ::company-registry::gleif
::ch ::company-registry::companies-house

children ::gleif/collect((o) { ::gleif/direct-children("HWUPKR0MPOU8FGXBT394", o) }, {}, 500)
officers ::ch/collect((o) { ::ch/officers("00445790", o) }, {}, 300)
```

## Documents

```hot
::ch ::company-registry::companies-house
::sec ::company-registry::sec

// Companies House: the latest accounts as PDF bytes
history ::ch/filing-history("00445790", {category: "accounts", items-per-page: 1})
link first(history.items).links.document_metadata
meta-data ::ch/document-metadata(link)   // resources: available content types
pdf ::ch/document-content(link, "application/pdf")
pdf.bytes

// SEC: one filing document
doc ::sec/filing-document(320193, "0000320193-25-000079", "aapl-20250927.htm")
doc.content-type
```

`document-content` follows the Document API's redirect to a short-lived signed URL. Hot does not forward the API key to that other host. A document must fit in `http.max-response-bytes` (16 MiB by default).

## Errors

Non-2xx responses come back as an Err carrying `::company-registry::api/HttpError`:

- `source`: `"gleif"`, `"sec"` or `"companies-house"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from the registry's own error text, never a credential

Lookups that find nothing return null rather than a 404 Err: `get-record`, `direct-parent`, `ultimate-parent` and the other relationship calls, and `company`. A missing credential is an Err that names the context variable to set, before any request. `lookup` collects per-source failures in `errors` and still returns the other sources' companies. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **SEC fair access**: at most **10 requests per second** per client, across all SEC hosts. SEC blocks clients that exceed it or send no User-Agent. `company_tickers.json` is about 800 KB and changes daily: fetch it once per run (`company-tickers`) and reuse it with `find-ticker` and `search-tickers`. Large filers' `company-facts` run to several MB.
- **EDGAR full-text search** (`efts.sec.gov/LATEST/search-index`) is the endpoint behind EDGAR's full-text search page. SEC does not document it as a public API, so its response shape can change. It covers filings since 2001.
- `lookup`'s SEC search is a substring match on ticker-file names, so it only finds companies with listed tickers. To find other filers, use `full-text-search`, or look up by CIK.
- **GLEIF** asks clients to stay under 60 requests per minute. Level 2 parent data is self-reported. When no parent is reported, `direct-parent-exception` gives the reason (e.g. `NATURAL_PERSONS`). ISIN mappings come from GLEIF's ISIN-to-LEI relationship file via `isins` and `by-isin`.
- **Companies House**: 600 requests per 5 minutes per key. Company numbers are normalized to eight characters (`"6"` → `"00000006"`, `"SC1234"` → `"SC001234"`). The **Streaming API** (`stream.companieshouse.gov.uk`) is not wrapped: it needs a separate stream key and a connection held open indefinitely, which doesn't suit a run. Poll `filing-history` from a schedule instead.
- **Sandboxes**: Companies House has a self-serve sandbox (test data; create a sandbox application on the developer hub and set `companies-house-base-url`). GLEIF and EDGAR have no sandbox; both are free public APIs.
- **Data handling**: officer and PSC records contain personal data (names, months and years of birth, correspondence addresses). The package logs nothing, and errors carry URLs and statuses only. Store results under your own data-protection policy. None of these registries needs a sales contract.

## License

Apache-2.0 - see [LICENSE](LICENSE)
