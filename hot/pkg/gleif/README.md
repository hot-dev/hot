# gleif

The [GLEIF LEI API](https://www.gleif.org/en/lei-data/gleif-api) for Hot: global legal entities, with no key. Covers search, fuzzy names, LEI and ISIN lookups, and direct and ultimate parents and children.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/gleif": "0.1.1"
}
```

## Auth and settings

GLEIF takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::gleif::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `gleif.base-url` | `base-url` | `https://api.gleif.org/api/v1` | GLEIF API root |

## Quick start

```hot
// Search, then walk up the ownership tree
page ::gleif/search("apple inc", {page-size: 5})
lei first(page.data).id
direct ::gleif/direct-parent(lei)       // an LEI record, or null
ultimate ::gleif/ultimate-parent(lei)

// A misspelled name
::gleif/fuzzy-completions("appel inc")  // [{name: "Apple Inc.", lei: "HWUPKR0MPOU8FGXBT394"}, ...]

// One record, by LEI or ISIN
record ::gleif/get-record("HWUPKR0MPOU8FGXBT394")
record.attributes.entity.legalName.name
issuers ::gleif/by-isin("US0378331005")
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::gleif` | Records: `search`, `search-legal-name`, `fuzzy-completions`, `autocompletions`, `get-record`, `by-isin`, `isins`. Relationships: `direct-parent`, `ultimate-parent`, `direct-parent-relationship`, `ultimate-parent-relationship`, `direct-parent-exception`, `ultimate-parent-exception`, `direct-children`, `ultimate-children`. Paging: `next-page`, `collect`. Helpers: `is-lei`, `lei-path`, `search-params`, `filter-params`, `page-params`, `completions` |
| `::gleif::api` | `resolve`, `request`, `get-json`, `get-json-or-null`, `query-string`, `path-segment`, `http-error`, `is-not-found`; type `HttpError` |

## Pagination

GLEIF pages by number: `page-size` (up to 200) and `page`. `next-page(page)` reads `meta.pagination`, and `collect(fetch, opts, limit)` follows it. An Err on any page is returned in place of the items.

```hot
children ::gleif/collect((o) { ::gleif/direct-children("HWUPKR0MPOU8FGXBT394", o) }, {}, 500)
```

## Errors

Non-2xx responses come back as an Err carrying `::gleif::api/HttpError`:

- `source`: `"gleif"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from GLEIF's own error text

Lookups that find nothing return null rather than a 404 Err: `get-record`, `direct-parent`, `ultimate-parent` and the other parent relationship and exception calls. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- GLEIF asks clients to stay under 60 requests per minute.
- Level 2 parent data is self-reported. When no parent is reported, `direct-parent-exception` gives the reason (e.g. `NATURAL_PERSONS`).
- ISIN mappings come from GLEIF's ISIN-to-LEI relationship file via `isins` and `by-isin`.
- GLEIF has no sandbox. It is a free public API and needs no sales contract.

## License

Apache-2.0 - see [LICENSE](LICENSE)
