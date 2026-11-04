# un-sanctions

The UN Security Council Consolidated List for Hot. It downloads the free XML file and parses it into `Listing` records.

> **This package only fetches and parses.** It doesn't screen names, and it stores nothing: no cache, no `::hot::store`, no state between calls. Storing the data, matching names against it and reviewing any match are your job. List data alone is not a clearance decision. A name missing from this list clears no one, because it covers one list as of one publication. A name on it still needs a person to confirm it is the same party.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/un-sanctions": "0.1.0"
}
```

The list needs no account or key, and the package has no settings.

## Quick start

```hot
out ::un::sanctions/fetch()
out.version              // the first 16 hex digits of the body's SHA-256
length(out.entries)      // about 1,000
first(out.entries).name
```

### Skip the parse when nothing changed

The server answers GET only, so there is no cheap version check. `fetch` always downloads the 2 MB file. When its hash equals the version you pass, it skips the parse. Keeping that version between runs is up to you.

```hot
out ::un::sanctions/fetch(held-version)
out.unchanged            // true: not parsed, entries is null
```

## List and format

| List | File used | Size | Version |
|---|---|---|---|
| UN Security Council Consolidated List, about 1,000 | `consolidated.xml` (English) | 2 MB | SHA-256 of the body |

`https://scsanctions.un.org/resources/xml/en/consolidated.xml` redirects to a signed Azure Blob URL.

## Listing

| Field | Meaning |
|---|---|
| `id`, `data-id` | The `REFERENCE_NUMBER` (such as `"IQi.004"`) and `DATAID` |
| `kind` | `"individual"` or `"entity"` |
| `name`, `aliases` | `FIRST_NAME` to `FOURTH_NAME` joined, then every `ALIAS_NAME` and `NAME_ORIGINAL_SCRIPT` |
| `dobs` | Dates of birth as `YYYY-MM-DD`, `YYYY`, or a range `YYYY/YYYY` |
| `nationalities` | As published |
| `list-types` | The `UN_LIST_TYPE` values |
| `addresses` | One line per address |
| `comments` | The `COMMENTS1` text |
| `listed-on` | The `LISTED_ON` date, as published |
| `url` | The list file |

## Functions

| Namespace | Functions |
|---|---|
| `::un::sanctions` | `fetch`, `parse`, `parse-record`, `iso-date`; types `Listing`, `ListingKind` |
| `::un::sanctions::http` | `request`, `get-text`, `http-error`, `body-text`; type `HttpError` |
| `::un::sanctions::xml` | `records`, `blocks`, `block`, `texts`, `text`, `start-tags`, `attr`, `decode` |

## Errors

- `fetch` returns an Err when a download fails, and when the file parses to no records, so a changed file format can't pass for an empty list.
- Download failures are `HttpError` (`status`, `url`, `message`).
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Notes and limits

- The list is a public publication, free, with no sandbox or keys.
- The XML is read with regexes over record strings, which is nearly twice as fast in Hot as walking a parsed tree. It relies on the file's format: record elements do not nest inside themselves, and there are no CDATA sections or comments inside records.

## License

Apache-2.0 - see [LICENSE](LICENSE)
