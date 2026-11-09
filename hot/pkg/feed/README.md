# feed

A stateless RSS 2.0 and Atom 1.0 reader for Hot, with no key. Fetch a feed or parse feed text, get one shape for both formats, make conditional GETs, and diff entries against ids you store.

The package remembers nothing. Storing ids, ETags and dates belongs to your app.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/feed": "0.1.2"
}
```

## Auth and settings

Feeds take no credential. The User-Agent comes from per-call `opts` first, then the Context Variable, then the default (`::feed::api/resolve`).

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `feed.user-agent` | `user-agent` | `hot-dev-feed-pkg/0.1 (+https://hot.dev)` | Sent with every request. sec.gov asks for your company name and a contact email |

`user-agent` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable.

## Quick start

```hot
::feed ::feed

url "https://www.sec.gov/news/pressreleases.rss"
agent "Acme Corp ops@acme.example"

feed ::feed/fetch(url, {user-agent: agent})
feed.title                    // "Press Releases"
first(feed.entries).title
first(feed.entries).published     // "Fri, 30 Oct 2026 15:30:00 -0400"
first(feed.entries).published-at  // "2026-10-30T19:30:00Z"

// Next run: pass back what you stored
next ::feed/fetch(url, {user-agent: agent, etag: feed.etag, last-modified: feed.last-modified})
if(next.not-modified, [], ::feed/new-entries(next.entries, stored-ids))

// No network
::feed/parse(text)
```

## Result shape

`parse` returns the same Map for RSS and Atom. A missing or blank value is null.

| Key | RSS 2.0 | Atom 1.0 |
|---|---|---|
| `title` | `title` | `title` |
| `link` | `link`, then `atom:link` | `link` with `rel="alternate"` or no `rel`, then the first `link` |
| `updated` | `lastBuildDate`, `pubDate`, `dc:date`, `atom:updated` | `updated` |
| `updated-at` | `updated` as UTC, or null | the same |
| `entries` | each `item` | each `entry` |

Each entry:

| Key | RSS 2.0 | Atom 1.0 |
|---|---|---|
| `id` | `guid`, then the link | `id`, then the link |
| `title` | `title` | `title` |
| `link` | `link`, `atom:link`, then a permalink `guid` | as for the feed |
| `published` | `pubDate`, `dc:date` | `published` |
| `updated` | `atom:updated` | `updated` |
| `summary` | `description`, then `content:encoded` | `summary`, then `content` |
| `author` | every `dc:creator`, then `author` | every `author`, then the source's, then the feed's |
| `content` | `content:encoded` | `content` |
| `published-at`, `updated-at` | the dates as UTC, or null | the same |

`fetch` adds `not-modified`, `etag` and `last-modified`.

## Functions by area

| Namespace | Functions |
|---|---|
| `::feed` | Reading: `fetch`, `parse`, `read-response`. Diffing: `new-entries`. Helpers: `normalize-date`, `resolve-link`, `first-text` |
| `::feed::api` | `resolve`, `request`, `request-headers`, `decode`, `http-error`; type `HttpError` |

## Pagination

Feeds don't paginate here: a feed is the one document the publisher serves, usually its latest entries. An entry that scrolls off between two fetches is never seen, so poll more often than the feed turns over.

## Conditional GET

Pass the `etag` and `last-modified` from the last result. The request then carries `If-None-Match` and `If-Modified-Since`, and a 304 comes back as:

```hot
{title: null, link: null, updated: null, updated-at: null, entries: [], not-modified: true, etag: "...", last-modified: "..."}
```

A 304 that doesn't repeat a validator keeps the one you passed. Not every publisher sends them. On 4 Nov 2026 `sec.gov/news/pressreleases.rss` sent both and honoured them, while the Federal Register RSS and the EDGAR current filings Atom feed sent neither, so those two always return the whole feed.

## Errors

- Non-2xx other than 304 is an Err carrying `::feed::api/HttpError`: `source` (`"feed"`), `status`, `url`, `headers`, `body`, `retry-after`, `message`. A 404 is an Err too, because a feed that has gone away is worth knowing about. `url` is the URL as you gave it, so a token in a private feed's URL appears there.
- Malformed XML is the Err from `::hot::xml`, which is strict. Nothing is repaired or guessed at.
- XML that isn't RSS or Atom is an Err naming the root element. That covers an HTML block page and RSS 1.0 (RDF).
- Bytes in an encoding other than UTF-8, US-ASCII or ISO-8859-1, or invalid in the encoding they declare, are an Err.

Nothing retries inside a run. Use a handler or schedule `retry` policy.

## Notes and limits

- **Dates.** `published` and `updated` are always the raw strings. `published-at` and `updated-at` are UTC `YYYY-MM-DDTHH:MM:SSZ` with fractions dropped, and null for anything not certain: other formats, date-only values, two-digit years, a date that doesn't exist, and zone names with more than one meaning, such as `BST`.
- **Entries with no id.** An RSS item with no `guid` and no link has a null `id`. `new-entries` can't match it, so it always counts as new.
- **Namespaces go by prefix.** RSS extensions are read by their usual prefixes: `dc:`, `content:`, `atom:`. A feed binding those namespaces to other prefixes loses those fields. A prefixed Atom root such as `<a:feed>` is handled.
- **HTML stays HTML.** CDATA and escaped `type="html"` text arrive as markup, not stripped. `type="xhtml"` content comes back as its `<div>` markup.
- **Whitespace.** `::hot::xml` trims each text node, so a space between text and a neighbouring CDATA section or inline element is lost.
- **Relative links** resolve against `xml:base`, then the fetched URL. `parse` with neither leaves them as they are; pass `opts.base-url`. `..` segments stay in the URL.
- **Size.** A feed must fit in `http.max-response-bytes` (16 MiB by default). Requests don't ask for compression.
- **Publishers set their own terms.** Check each feed's. For SEC feeds, [SEC's fair access rules](https://www.sec.gov/search-filings/edgar-search-assistance/accessing-edgar-data) allow at most **10 requests per second** and ask you to declare a User-Agent with a contact.
- **OFAC has no recent-actions feed.** `https://ofac.treasury.gov/recent-actions` links none. `https://ofac.treasury.gov/rss.xml` parses, but on 4 Nov 2026 it listed sanctions program pages, the newest from February 2025, not recent actions. OFAC's alerts go out by email.

## License

Apache-2.0 - see [LICENSE](LICENSE)
