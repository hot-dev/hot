# companies-house

The [UK Companies House](https://developer.company-information.service.gov.uk/) Public Data and Document APIs for Hot, with a free API key. Covers company and officer search, profiles, officers, persons with significant control, filing history and document downloads.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/companies-house": "0.1.0"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::companies-house::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `companies-house.api-key` | `api-key` | none | **Required** (secret). A free REST API key from the [developer hub](https://developer.company-information.service.gov.uk/), sent as the Basic-auth username with an empty password |
| `companies-house.base-url` | `base-url` | `https://api.company-information.service.gov.uk` | Set it to `https://api-sandbox.company-information.service.gov.uk` (test data, with a sandbox key) |
| `companies-house.document-base-url` | `document-base-url` | `https://document-api.company-information.service.gov.uk` | Document API host |

## Per-call credentials

```hot
::ch ::companies-house

::ch/company("00445790", {api-key: "tenant-ch-key"})
```

`api-key` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` or `document-base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL.

## Quick start

```hot
::ch ::companies-house

found ::ch/search-companies("tesco", {items-per-page: 5})
number first(found.items).company_number

// Profile, officers and control
profile ::ch/company(number)
officers ::ch/officers(number)
psc ::ch/persons-with-significant-control(number)
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::companies-house` | Search: `search-companies`, `search-officers`, `search-all`, `search-disqualified-officers`. Company: `company`, `registered-office-address`, `officers`, `officer-appointments`, `persons-with-significant-control`, `psc-statements`, `filing-history`, `filing`, `charges`. Documents: `document-metadata`, `document-content`, `document-id`. Paging: `next-start`, `collect`, `page-params`. Helpers: `auth-header`, `company-number`, `officer-id-from-link` |
| `::companies-house::api` | `resolve`, `require-api-key`, `request`, `get-json`, `get-json-or-null`, `query-string`, `path-segment`, `http-error`, `is-not-found`; type `HttpError` |

## Pagination

Companies House pages by offset: `items-per-page` (up to 100) and `start-index`. `next-start(page)` and `collect(fetch, opts, limit)` follow `total_results`. An Err on any page is returned in place of the items.

```hot
::ch ::companies-house

officers ::ch/collect((o) { ::ch/officers("00445790", o) }, {}, 300)
```

## Documents

```hot
::ch ::companies-house

// The latest accounts as PDF bytes
history ::ch/filing-history("00445790", {category: "accounts", items-per-page: 1})
link first(history.items).links.document_metadata
meta-data ::ch/document-metadata(link)   // resources: available content types
pdf ::ch/document-content(link, "application/pdf")
pdf.bytes
```

`document-content` follows the Document API's redirect to a short-lived signed URL. Hot does not forward the API key to that other host. A document must fit in `http.max-response-bytes` (16 MiB by default).

## Errors

Non-2xx responses come back as an Err carrying `::companies-house::api/HttpError`:

- `source`: `"companies-house"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from Companies House's own error text, never a credential

`company` returns null rather than a 404 Err when there is no such company. A missing API key is an Err that names the context variable to set, before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Rate limit**: 600 requests per 5 minutes per key.
- Company numbers are normalized to eight characters (`"6"` → `"00000006"`, `"SC1234"` → `"SC001234"`).
- The **Streaming API** (`stream.companieshouse.gov.uk`) is not wrapped: it needs a separate stream key and a connection held open indefinitely, which doesn't suit a run. Poll `filing-history` from a schedule instead.
- **Sandbox**: Companies House has a self-serve sandbox (test data; create a sandbox application on the developer hub and set `base-url`).
- **Data handling**: officer and PSC records contain personal data (names, months and years of birth, correspondence addresses). The package logs nothing, and errors carry URLs and statuses only. Store results under your own data-protection policy. The API needs no sales contract.

## License

Apache-2.0 - see [LICENSE](LICENSE)
