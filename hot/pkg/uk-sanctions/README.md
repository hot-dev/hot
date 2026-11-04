# uk-sanctions

The UK Sanctions List published by the FCDO, for Hot. It downloads the free XML file and parses it into `Designation` records.

> **This package only fetches and parses.** It doesn't screen names, and it stores nothing: no cache, no `::hot::store`, no state between calls. Storing the data, matching names against it and reviewing any match are your job. List data alone is not a clearance decision. A name missing from this list clears no one, because it covers one list as of one publication. A name on it still needs a person to confirm it is the same party.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/uk-sanctions": "0.1.0"
}
```

The list needs no account or key, and the package has no settings.

## Quick start

```hot
out ::uk::sanctions/fetch()
out.version              // the file's ETag
length(out.entries)      // about 6,400
first(out.entries).name
```

### Skip a download when nothing changed

`current-version` sends one HEAD request and downloads no list. `fetch` does the same check when you pass the version you hold. Keeping that version between runs is up to you.

```hot
::uk::sanctions/current-version()

out ::uk::sanctions/fetch(held-version)
out.unchanged            // true: nothing downloaded, entries is null
```

## List and format

| List | File used | Size | Version |
|---|---|---|---|
| UK Sanctions List (FCDO), about 6,400 designations | `UK-Sanctions-List.xml` | 22 MB, Range-chunked | ETag (HEAD) |

The OFSI Consolidated List closed on 28 January 2026, and the FCDO UK Sanctions List is now the only source of UK designations. The XML is used: the CSV is about 50 MB because it repeats every field once per name. The file is larger than the default 16 MiB response cap, so it is fetched in 8 MiB Range requests and works with the default cap.

## Designation

| Field | Meaning |
|---|---|
| `id` | The `UniqueID` |
| `kind` | `"individual"`, `"entity"` or `"vessel"` (the list's `Ship`) |
| `name`, `aliases` | The primary name, then every other name and the non-Latin names |
| `dobs` | Dates of birth as `YYYY-MM-DD`, `YYYY-MM` or `YYYY` (`dd/mm/1963` is `"1963"`) |
| `nationalities` | As published |
| `regimes` | The sanctions regime names |
| `addresses` | One line per address |
| `other-information` | The `OtherInformation` text |
| `designation-source` | Who designated it, such as `"UN"` |
| `un-reference`, `ofsi-group-id` | The `UNReferenceNumber` and `OFSIGroupID` |
| `date-designated`, `last-updated` | As published (`dd/mm/yyyy`) |
| `url` | The list file |

## Functions

| Namespace | Functions |
|---|---|
| `::uk::sanctions` | `fetch`, `current-version`, `parse`, `parse-designation`, `iso-date`; types `Designation`, `DesignationKind` |
| `::uk::sanctions::http` | `request`, `head`, `get-text`, `byte-ranges`, `http-error`, `body-text`, `too-large-message`; type `HttpError` |
| `::uk::sanctions::xml` | `records`, `blocks`, `block`, `texts`, `text`, `start-tags`, `attr`, `decode` |

## Errors

- `fetch` returns an Err when a download fails, and when the file parses to no records, so a changed file format can't pass for an empty list.
- Download failures are `HttpError` (`status`, `url`, `message`). If the server stops supporting Range requests, the message says to raise `http.max-response-bytes`.
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Notes and limits

- **Fetch this list about once a day.** The server has a bot check. After a few full downloads within minutes, it answered every request from that IP address with HTTP 405 and a "Human Verification" page, whatever the client, for at least ten minutes. `fetch` and `current-version` return that as an Err. Use `current-version` to skip downloads, and give the schedule's `retry` policy a long delay.
- The list is a public government publication, free, with no sandbox or keys.
- The XML is read with regexes over record strings, which is nearly twice as fast in Hot as walking a parsed tree. It relies on the file's format: record elements do not nest inside themselves, and there are no CDATA sections or comments inside records.

## License

Apache-2.0 - see [LICENSE](LICENSE)
