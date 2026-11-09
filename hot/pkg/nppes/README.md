# nppes

The [CMS NPI Registry API](https://npiregistry.cms.hhs.gov/api-page), version 2.1, for Hot, with no key. Covers lookups by NPI, searches by name, organization, taxonomy and location, and paging.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/nppes": "0.1.2"
}
```

## Auth and settings

The API takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::nppes::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `nppes.base-url` | `base-url` | `https://npiregistry.cms.hhs.gov/api` | API root |

## Quick start

```hot
// One provider by NPI
mayo ::nppes/get-provider("1881018208")      // a record, or null
mayo.basic.organization_name                 // "MAYO CLINIC"
::nppes/is-active(mayo)
::nppes/location-address(mayo).city
::nppes/primary-taxonomy(mayo).desc

// Organizations, with a trailing wildcard
page ::nppes/organizations("mayo clinic*", {state: "MN", limit: 50})
::nppes/results(page)

// Individuals by name, taxonomy and state
::nppes/individuals("smith", {first-name: "jo*", taxonomy-description: "Internal Medicine", state: "NY"})

// Any criteria
::nppes/search({city: "baltimore", postal-code: "212*", limit: 200})

// Check an NPI's shape before a lookup
::nppes/is-npi("1234567893")                 // true
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::nppes` | Providers: `get-provider`, `search`, `organizations`, `individuals`. Reading responses: `results`, `location-address`, `mailing-address`, `primary-taxonomy`, `is-active`. Paging: `next-page`, `collect`. Helpers: `is-npi`, `search-params`, `page-limit` |
| `::nppes::api` | `resolve`, `request`, `get-json`, `decode-body`, `query-string`, `http-error`, `not-json-error`; type `HttpError` |

Criteria go in `opts`: `number`, `enumeration-type`, `first-name`, `last-name`, `use-first-name-alias`, `name-purpose`, `organization-name`, `taxonomy-description`, `address-purpose`, `city`, `state`, `postal-code` and `country-code`.

## Pagination

`limit` defaults to 10 and goes up to 200, and `skip` goes up to 1,000. `next-page(page, opts)` gives the next `skip`, and `collect(fetch, opts, limit)` follows it, 200 records a page unless `opts.limit` says otherwise. When a page size doesn't divide 1,000, `next-page` stops short of `skip` 1,000. `collect` makes one more request there and drops the records it has already read.

```hot
rows ::nppes/collect((o) { ::nppes/organizations("mayo clinic*", o) }, {state: "MN"}, 500)
```

One set of criteria reaches `skip` 1,000 plus a page, so 1,200 records at 200 a page. `collect` stops there, and the API gives no total, so that many records means there may be more. Narrow the criteria, by state or postal code, and collect each part. An Err on any page is returned in place of the records.

## Errors

Failed responses come back as an Err carrying `::nppes::api/HttpError`:

- `source`: `"nppes"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from the API's own error text

The API reports bad criteria with status 200 and an `Errors` body, such as `state` alone or a wildcard after one letter. Those are an `HttpError` too, with `status` 200 and `body.Errors` listing `{description, field, number}`. So is a 2xx body that isn't a JSON object, such as a maintenance page, which would otherwise read as no such provider.

`get-provider` returns null when the registry has no such NPI, and a search with no matches is an empty page. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Search limits.** CMS says a query returns at most 200 results, `skip` can pass up to 1,000 records, and the two together reach "a maximum of 1,200 records over six requests".
- **Limits fail silently.** Live, `limit=201` returned 200 records with no error, and `skip=1001` returned the same record as `skip=1000`. So `search` caps `limit` at 200, and a `skip` over 1,000 is an Err rather than a repeat of old records.
- **`result_count` is the page, not the total.** The API has no total count.
- **Criteria rules.** `enumeration-type` can't be the only criterion. `state` needs another criterion besides `enumeration-type` and `country-code`. `country-code` works alone unless it is `US`. Wildcards are trailing only, after at least two characters.
- **Name matching.** `use-first-name-alias` defaults to true, so `first-name: "Robert"` also finds Rob and Bob. `organization-name` checks every name on a record, so a result can show a different name from the one searched. `taxonomy-description` takes an exact description or specialty, or a trailing wildcard.
- **Version.** Only 2.1 works. CMS has retired 1.0 and 2.0, and a request with no `version` gets an `Unsupported Version` error. CMS lists no phase-out date for 2.1.
- **Address order.** CMS says the first address is always the primary practice location and the second is the mailing address. Other locations are in `practiceLocations`, and a record has up to 15 taxonomies and 50 other identifiers.
- **Freshness.** CMS says the information is updated daily, and a record's pending changes are not shown. Only the Freedom of Information Act data is public.
- **Deactivated NPIs.** CMS's own registry website won't show an NPI with status `D`, as it doesn't display NPIs that are not in service. A null from `get-provider` therefore doesn't show an NPI was never issued. No deactivated NPI was checked against the API here.
- **Personal data.** Type 1 records describe individuals, and Type 2 records name an authorized official with a phone number. Treat them as personal data when you store them.
- **Rate limits and terms.** The API page states no rate limit and no terms of use, and responses carry no rate limit headers. Keep bulk work to the [NPPES data dissemination](https://download.cms.gov/nppes/NPI_Files.html) files that page points to.

## License

Apache-2.0 - see [LICENSE](LICENSE)
