# ecfr

The [eCFR API](https://www.ecfr.gov/developers/documentation/api/v1) for Hot: the Code of Federal Regulations, with no key. Covers the titles list, structure, ancestry, version history, search, and full text by part or section.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/ecfr": "0.1.1"
}
```

Requires Hot 3.0.3 or later for `::hot::gzip`.

## Auth and settings

eCFR takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::ecfr::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `ecfr.base-url` | `base-url` | `https://www.ecfr.gov/api` | eCFR API root |

## Quick start

```hot
// One section as text, for citing
s ::ecfr/get-section(31, "1010.230")
s.citation  // "31 CFR 1010.230"
s.heading   // "§ 1010.230 Beneficial ownership requirements for legal entity customers."
s.text      // paragraphs split by a blank line

// A whole part, with every section in it
part ::ecfr/get-part(31, "1010")
map(part.sections, (x) { x.citation })

// All of 31 CFR Chapter X
chapter ::ecfr/full(31, {chapter: "X"})

// The text as it stood on a date
::ecfr/get-section(31, "1010.230", {date: "2024-01-01"})

// Where a section sits, and when it changed
::ecfr/ancestry(31, {section: "1010.230"})
::ecfr/versions(31, {part: "1010", section: "1010.100", issue-date-gte: "2024-01-01"})

// Search the text in force
date ::ecfr/latest-date(31)
page ::ecfr/search("beneficial ownership", {title: 31, date: date, per-page: 10})
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::ecfr` | Titles: `titles`, `get-title`, `latest-date`. Text: `get-section`, `get-part`, `full`, `full-xml`, `parse-full`. Hierarchy: `structure`, `ancestry`, `versions`. Search: `search`, `search-count`. Paging: `next-page`, `collect`. Helpers: `hierarchy-params`, `versions-params`, `search-params`, `dated-path`, `find-title`, `flatten-inline`, `node-text`, `blocks`, `citation-of` |
| `::ecfr::api` | `resolve`, `request`, `get-json`, `get-json-or-null`, `get-xml-or-null`, `body-text`, `query-string`, `path-segment`, `http-error`, `html-error`, `is-html`, `is-not-found`, `is-past-date`, `null-on-404`; type `HttpError` |

Pick a place in a title with the hierarchy keys: `subtitle`, `chapter`, `subchapter`, `part`, `subpart`, `section`, `appendix`.

`get-section` gives `{type, identifier, citation, heading, text, notes, source-note}`. `notes` holds eCFR's editorial notes on pending or delayed amendments, which aren't part of the rule text but do affect what it will say. `source-note` is the Federal Register history. `get-part` and `full` give `{type, identifier, citation, heading, authority, source, sections}`, with `part` and `subpart` on each section.

## Pagination

`search` pages by number: `per-page` and `page`. `next-page(page)` reads `meta`, and `collect(fetch, opts, limit)` follows it.

```hot
hits ::ecfr/collect((o) { ::ecfr/search("beneficial ownership", o) }, {title: 31, date: date}, 250)
```

## Errors

Non-2xx responses come back as an Err carrying `::ecfr::api/HttpError`:

- `source`: `"ecfr"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from eCFR's own error text

Lookups that find nothing return null rather than a 404 Err: `get-section`, `get-part`, `full`, `full-xml`, `structure` and `ancestry`. `get-title` and `latest-date` return null for an unknown title. A blank section or part is an Err, because the query would otherwise ask for the whole title.

Two cases to watch for:

- **A date past the title's current date is an Err, not null.** The section may well exist. Today is usually past that date, so use `latest-date` or leave `date` out.
- **An HTML page is an Err.** The site redirects clients it flags as automated to a CAPTCHA page, and `::hot::http` follows the redirect, so the page arrives as a 200. That `HttpError` has `status` 200.

Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **eCFR is not the official edition.** [NARA describes it](https://www.archives.gov/federal-register/cfr/about-ecfr) as an unofficial editorial compilation of CFR material and Federal Register amendments, one that doesn't provide legal notice to the public or judicial notice to the courts. It is generally current within two business days. Keep that in mind when citing text from it.
- **Ask for a part or section, not a title.** `http.max-response-bytes` caps a response at 16 MiB by default, `compression.max-output-bytes` caps decompressed output at 64 MiB, and `from-xml` refuses input over 64 MiB. Title 40 is about 153 MiB of XML, so it fails. `size` on `structure` and `ancestry` nodes gives a node's XML size in bytes, so check it first when unsure. For scale, Title 31 is about 10 MB, Chapter X about 750 KB and Part 1010 about 370 KB.
- **Compression is required for full text.** The `full` endpoint answers 406 unless the request accepts compression, so every request sends `accept-encoding: gzip`. A body is only decoded when `content-encoding` says gzip.
- **Dated calls may cost a second request.** Without `opts.date`, `structure`, `ancestry`, `full-xml`, `full`, `get-part` and `get-section` first read `up_to_date_as_of` from the titles list. Pass `date` in a loop.
- **Early dates read as missing.** A date before eCFR's history begins returns the same 404 as a missing section, so it gives null. 2000-01-01 does this for Title 31.
- **Search covers every version by default.** Without `date`, `search` also returns superseded text. Pass `date` for the text in force.
- **Parsed text is plain text.** Superscripts become `^n`, and a table becomes a line per row with ` | ` between cells. Images are dropped, so a formula or chart set as a figure is missing from `text`. Use `full-xml` when you need the source.
- **`authority` and `source` come from the top node.** They are set for a part but not for a chapter.
- **Scripted access goes through the API.** The site's Request Access notice says programmatic access to eCFR.gov and FederalRegister.gov is limited to the developer APIs, because of aggressive automated scraping. Fetching the website's HTML pages from a script gets the CAPTCHA redirect.
- **Rate limits.** This package states no figure because none could be confirmed. Keep requests modest, and stop and back off on a 429 or a CAPTCHA redirect.

## License

Apache-2.0 - see [LICENSE](LICENSE)
