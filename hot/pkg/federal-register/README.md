# federal-register

The [Federal Register API](https://www.federalregister.gov/developers/documentation/api/v1) for Hot: rules, proposed rules, notices and presidential documents, with no key. Covers document search, documents by number, public inspection and the agencies list.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/federal-register": "0.1.2"
}
```

## Auth and settings

The Federal Register takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::federal-register::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `federal-register.base-url` | `base-url` | `https://www.federalregister.gov/api/v1` | Federal Register API root |

## Quick start

```hot
// FinCEN rules and proposed rules affecting 31 CFR 1010 this year
page ::federal-register/search({
  agencies: "financial-crimes-enforcement-network",
  type: ["RULE", "PRORULE"],
  publication-date-gte: "2026-01-01",
  cfr-title: 31,
  cfr-part: 1010,
  order: "newest"
})
first(page.results).title

// One document, or null
doc ::federal-register/get-document("2026-18194")

// Filed but not yet published
out ::federal-register/public-inspection()
out.results

// Agency slugs for the agencies condition
map(::federal-register/agencies(), (a) { a.slug })
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::federal-register` | Documents: `search`, `get-document`. Public inspection: `public-inspection`, `get-public-inspection`. Agencies: `agencies`. Paging: `next-page`, `collect`. Helpers: `search-params`, `page-params`, `significant-param` |
| `::federal-register::api` | `resolve`, `request`, `get-json`, `get-json-or-null`, `query-string`, `path-segment`, `http-error`, `html-error`, `is-html`, `is-not-found`, `null-on-404`; type `HttpError` |

`search` takes these `opts`:

| opts key | Condition |
|---|---|
| `term` | Full text. Wrap a phrase in double quotes |
| `type` | `"RULE"`, `"PRORULE"`, `"NOTICE"`, `"PRESDOCU"`, or a Vec of them |
| `agencies` | An agency slug or a Vec of them |
| `publication-date-gte`, `publication-date-lte`, `publication-date-is` | `YYYY-MM-DD` |
| `cfr-title`, `cfr-part` | Documents affecting that part of the CFR |
| `significant` | True for rules deemed significant under EO 12866 |
| `order` | `"relevance"`, `"newest"` or `"oldest"` |
| `fields` | A Vec of the fields to return |
| `per-page`, `page` | Paging, up to 1000 per page |

## Pagination

Search results page with a cursor held in `next_page_url`. `next-page(page)` turns that into a value for `opts.after`, and `collect(fetch, opts, limit)` follows it.

```hot
page ::federal-register/search({cfr-title: 31, cfr-part: 1010, per-page: 100})
more ::federal-register/search({after: ::federal-register/next-page(page)})

docs ::federal-register/collect(::federal-register/search, {cfr-title: 31, cfr-part: 1010, order: "newest"}, 250)
```

Follow `next-page` rather than counting `page` numbers. `opts.after` already carries the conditions, so it needs nothing else. A `next_page_url` with no `/api/v1/` path, such as from a proxy that rewrites links, is an Err rather than a last page.

## Errors

Non-2xx responses come back as an Err carrying `::federal-register::api/HttpError`:

- `source`: `"federal-register"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from the API's own error text, such as `agencies: invalid value`

Lookups that find nothing return null rather than a 404 Err: `get-document` and `get-public-inspection`. An unknown agency slug in `search` is a 400 Err, not an empty result.

An HTML page is an Err too. The site redirects clients it flags as automated to a CAPTCHA page, and `::hot::http` follows the redirect, so the page arrives as a 200. That `HttpError` has `status` 200.

Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **`count` stops at 10000.** A count of exactly 10000 means at least that many, so narrow the search by date or agency to see the real number.
- **`per-page: 1` isn't honored.** The API answers with its default of 20 results. `collect` trims to `limit`, and `per-page: 2` works as asked.
- **`public-inspection` has no conditions or paging.** It lists what's on inspection now, so filter `results` in the caller.
- **`significant` comes from the Unified Agenda.** The API describes it as an associated Unified Agenda entry deemed significant under EO 12866, so a document with no such entry won't match `significant: true`.
- **Scripted access goes through the API.** The site's Request Access notice says programmatic access to FederalRegister.gov and eCFR.gov is limited to the developer APIs, because of aggressive automated scraping. Fetching the website's HTML pages from a script gets the CAPTCHA redirect, so leave links such as `html_url` for people to open.
- **Rate limits.** This package states no figure because none could be confirmed. Keep requests modest, and stop and back off on a 429 or a CAPTCHA redirect.

## License

Apache-2.0 - see [LICENSE](LICENSE)
