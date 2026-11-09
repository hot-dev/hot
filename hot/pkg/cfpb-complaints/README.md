# cfpb-complaints

The [CFPB Consumer Complaint Database API](https://cfpb.github.io/api/ccdb/) for Hot, with no key. Covers filtered search, one complaint by ID, aggregations and counts, company name suggestions, and paging.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/cfpb-complaints": "0.1.2"
}
```

## Auth and settings

The API takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::cfpb::complaints::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `cfpb.complaints.base-url` | `base-url` | `https://www.consumerfinance.gov/data-research/consumer-complaints/search/api/v1` | API root |

## Quick start

```hot
::c ::cfpb::complaints

// The company filter is exact, so get the name first
company first(::c/suggest-companies("jpmorgan"))   // "JPMORGAN CHASE & CO."

// Newest complaints for one company in two states
page ::c/search({
  company: company,
  state: ["CA", "NY"],
  date-received-min: "2026-01-01",
  sort: "created_date_desc",
  size: 50
})
::c/total(page)
first(::c/complaints(page)).issue
::c/freshness(page).last-indexed

// One complaint
::c/get-complaint("31224554")

// The shape of a company's complaints
aggs ::c/aggregations({company: company, date-received-min: "2026-01-01"})
::c/buckets(aggs, "product")           // [{key: "Checking or savings account", count: 7887, sub: [...]}, ...]
::c/buckets(aggs, "company_response")

// A trend: one count per window, with no shared days
::c/count({company: company, date-received-min: "2026-01-01", date-received-max: "2026-01-31"})
::c/count({company: company, date-received-min: "2026-02-01", date-received-max: "2026-02-28"})
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::cfpb::complaints` | Complaints: `search`, `get-complaint`. Counts: `count`, `aggregations`. Names: `suggest-companies`. Reading responses: `complaints`, `total`, `freshness`, `buckets`. Paging: `next-page`, `collect`. Helpers: `search-params`, `filter-params`, `page-size` |
| `::cfpb::complaints::api` | `resolve`, `request`, `get-json`, `decode-body`, `query-string`, `path-segment`, `http-error`, `not-json-error`; type `HttpError` |

Filters go in `opts`, and each takes a Str or a Vec of Str: `company`, `product`, `issue`, `state`, `zip-code`, `tags`, `timely`, `submitted-via`, `company-response`, `company-public-response`. Dates are `date-received-min`, `date-received-max`, `company-received-min` and `company-received-max`. `search-term` with `field` (`"all"` or `"company"`) searches text. `product` and `issue` select a second level with a bullet: `"Mortgage•FHA mortgage"`.

## Pagination

Set `size` (up to 100, default 25) and use a fixed `sort`. The API doesn't page by offset alone: page N needs `frm` and a `search_after` built from `_meta.break_points`. `next-page(page, opts)` works that out, and `collect(fetch, opts, limit)` follows it.

```hot
::c ::cfpb::complaints

rows ::c/collect((o) { ::c/search(o) }, {company: "JPMORGAN CHASE & CO.", sort: "created_date_desc"}, 500)
```

One query reaches 10,000 complaints at most, however many `total` reports. For more, split the query by date.

## Errors

Failed responses come back as an Err carrying `::cfpb::complaints::api/HttpError`:

- `source`: `"cfpb-complaints"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from the API's own error text

Bad parameters are a 400 that names the field. An unknown path is different: consumerfinance.gov answers it with an HTML page and status 200, so a 2xx body that isn't JSON is also an `HttpError`.

`get-complaint` returns null for an unknown ID, and a blank ID is an Err. `collect` returns an Err in place of the rows if any page fails. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Not a statistical sample.** CFPB says the database "is not a statistical sample of consumers' experiences in the marketplace", and that complaints aren't representative of all consumers' experiences with a product or company. CFPB also says complaints are not "information" under the Information Quality Act.
- **Read volume in context.** CFPB says to weigh complaint volume against company size and market share, and geography against population. A complaint count alone doesn't rank companies.
- **What gets published.** Only complaints sent to companies for response are eligible, and they appear after the company responds, confirming a commercial relationship, or after 15 days, whichever comes first. So the newest weeks are incomplete.
- **What is missing.** Complaints referred to other regulators aren't published, such as those about depository institutions with less than $10 billion in assets. No results for a small bank doesn't mean it has no complaints.
- **Labels change.** Older records keep the product and issue the consumer chose at the time, so categories don't line up across form changes.
- **No narratives, trends or geo.** CFPB removed consumer narratives and its visualizations in September 2026 ([Release 24](https://cfpb.github.io/api/ccdb/release-notes.html)). `/trends` and `/geo/states` now return the site's HTML page and are gone from the [API source](https://github.com/cfpb/ccdb5-api/blob/main/complaint_search/urls.py). Use `count` over date windows instead. "Consumer disputed" and "Consumer consent provided" are gone as well.
- **Aggregations ignore their own filter.** With `company` set, the `company` aggregation still lists every company, and the same goes for `state`. The other aggregations follow all the filters.
- **Date bounds include both ends.** The [API spec](https://cfpb.github.io/ccdb5-api/documentation/) says `date_received_max` is exclusive, but a live query with the same day as min and max returned that day's complaint, and the source sends an inclusive range. Windows that share a day count it twice.
- **Rate limits.** CFPB's published docs state none. The [API source](https://github.com/cfpb/ccdb5-api/blob/main/complaint_search/throttling.py) sets 20 searches and 5 single complaint reads per minute for an anonymous client. `collect` doesn't pace itself, so 20 pages at `size: 100` (2,000 complaints) is the most to expect in a minute.
- **Freshness.** The database generally updates daily. `freshness(page)` reports `stale` when the newest data is over 5 business days old, and `data-issue` when the latest load had a problem.
- **Terms.** CFPB says the data is "freely available for anyone to use, analyze, and build on", and the API's spec gives its license as CC0.
- CSV export (`format=csv`) isn't covered. CFPB limits filtered exports to 100,000 complaints and offers the whole dataset as a separate download.

## License

Apache-2.0 - see [LICENSE](LICENSE)
