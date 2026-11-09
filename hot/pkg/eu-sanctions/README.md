# eu-sanctions

The EU Consolidated Financial Sanctions List for Hot. It downloads the free Financial Sanctions Files (FSF) XML and parses it into `SanctionEntity` records.

> **This package only fetches and parses.** It doesn't screen names, and it stores nothing: no cache, no `::hot::store`, no state between calls. Storing the data, matching names against it and reviewing any match are your job. List data alone is not a clearance decision. A name missing from this list clears no one, because it covers one list as of one publication. A name on it still needs a person to confirm it is the same party.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/eu-sanctions": "0.1.2"
}
```

## Settings

The list needs no account or key.

Runtime setting: **`http.max-response-bytes` must be at least 33554432 (32 MiB) for the run that fetches this list.** The file is about 26 MB, and its server ignores Range requests. Below that, `fetch` returns an Err saying how far to raise it. `current-version` works at any setting.

## Quick start

```hot
out ::eu::sanctions/fetch()
out.version              // the file's Last-Modified
length(out.entries)      // about 6,300
first(out.entries).name
```

### Skip a download when nothing changed

`current-version` sends one HEAD request and downloads no list. `fetch` does the same check when you pass the version you hold. Keeping that version between runs is up to you.

```hot
::eu::sanctions/current-version()

out ::eu::sanctions/fetch(held-version)
out.unchanged            // true: nothing downloaded, entries is null
```

## List and format

| List | File used | Size | Version |
|---|---|---|---|
| EU Consolidated Financial Sanctions List (FSF), about 6,300 | `xmlFullSanctionsList_1_1` | 26 MB, needs a raised cap | Last-Modified (HEAD) |

The FSF file URL takes a `token` query parameter. Its value, `dG9rZW4tMjAxNw`, is a fixed public token (base64 for `token-2017`), not a credential, and no account is needed.

## SanctionEntity

| Field | Meaning |
|---|---|
| `id` | The `logicalId` |
| `eu-reference`, `un-id` | The `euReferenceNumber` and `unitedNationId` |
| `kind` | `"individual"` (the list's `person`) or `"entity"` |
| `name`, `aliases` | The first alias with no `nameLanguage`, then every other `wholeName` |
| `dobs` | Dates of birth as `YYYY-MM-DD` or `YYYY` |
| `citizenships` | ISO 3166-1 alpha-2 code, or the country description when the list gives no code |
| `programmes` | The regulations' programme codes |
| `addresses` | One line per address |
| `remarks` | The `<remark>` texts, joined with `; ` |
| `url` | The list's page on data.europa.eu |

## Functions

| Namespace | Functions |
|---|---|
| `::eu::sanctions` | `fetch`, `current-version`, `parse`, `parse-entity`, `iso-date`; types `SanctionEntity`, `SubjectKind`; constant `NEEDED_RESPONSE_BYTES` |
| `::eu::sanctions::http` | `request`, `head`, `get-text`, `http-error`, `body-text`, `too-large-message`; type `HttpError` |
| `::eu::sanctions::xml` | `records`, `blocks`, `block`, `texts`, `text`, `start-tags`, `attr`, `decode` |

## Errors

- `fetch` returns an Err when a download fails, and when the file parses to no records, so a changed file format can't pass for an empty list.
- Download failures are `HttpError` (`status`, `url`, `message`). A file over the response cap gives a message saying how far to raise `http.max-response-bytes`.
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Notes and limits

- The list is a public publication, free, with no sandbox or keys.
- The XML is read with regexes over record strings, which is nearly twice as fast in Hot as walking a parsed tree. It relies on the file's format: record elements do not nest inside themselves, and there are no CDATA sections or comments inside records.

## License

Apache-2.0 - see [LICENSE](LICENSE)
