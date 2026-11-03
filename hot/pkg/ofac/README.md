# ofac

The US Treasury OFAC SDN and Consolidated (non-SDN) lists for Hot. It downloads the free Sanctions List Service files and parses them into `Entry` records.

> **This package only fetches and parses.** It doesn't screen names, and it stores nothing: no cache, no `::hot::store`, no state between calls. Storing the data, matching names against it and reviewing any match are your job. List data alone is not a clearance decision. A name missing from these lists clears no one, because it covers one list as of one publication. A name on it still needs a person to confirm it is the same party.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/ofac": "0.1.0"
}
```

The lists need no account or key, and the package has no settings.

## Quick start

```hot
out ::ofac/fetch("ofac-sdn")
out.version              // "5f240bed79d504e7"
out.published            // "2026-10-28T00:00:00"
length(out.entries)      // about 19,500
first(out.entries).name
```

### Skip a download when nothing changed

`current-version` reads the SHA-256 from the SLS file listing with one small request and downloads no list. `fetch` does the same check when you pass the version you hold. Keeping that version between runs is up to you.

```hot
::ofac/current-version("ofac-sdn")   // "5f240bed79d504e7"

out ::ofac/fetch("ofac-sdn", held-version)
out.unchanged                        // true: nothing downloaded, entries is null
```

## Lists and formats

`SOURCES` is `["ofac-sdn", "ofac-cons"]`, the two lists `fetch` takes. Fetch both for full US coverage: OFAC's sectoral and non-SDN designations are in `ofac-cons`.

| Source key | List | Files used | Size | Version |
|---|---|---|---|---|
| `ofac-sdn` | OFAC Specially Designated Nationals (about 19,500 entries) | SLS legacy CSV: `SDN.CSV` + `ALT.CSV` + `ADD.CSV` + `SDN_COMMENTS.CSV` | 5.8 + 1.1 + 1.7 MB | SHA-256 from the SLS file listing |
| `ofac-cons` | OFAC Consolidated (non-SDN) lists (SSI, FSE, NS-PLC, NS-MBS, CAPTA, NS-CMIC) | `CONS_PRIM.CSV` + `CONS_ALT.CSV` + `CONS_ADD.CSV` + `CONS_COMMENTS.CSV` | < 0.4 MB | SHA-256 or `lastUpdated` from the SLS file listing |

The Sanctions List Service (SLS) publishes every file at `https://sanctionslistservice.ofac.treas.gov/api/PublicationPreview/exports/<FILE>`, which redirects to a signed S3 URL. It lists the files with their size, date and SHA-256 at `POST /api/PublicationPreview/SdnList` and `.../ConsolidatedList`. The package uses the legacy CSV files because each one fits the default response cap. `SDN.XML` (29 MB) and the Advanced XML (128 MB) are larger than that.

## Entry

| Field | Meaning |
|---|---|
| `source`, `list` | `"ofac-sdn"` or `"ofac-cons"`, and the list's name |
| `id` | OFAC's `ent_num` |
| `kind` | `"individual"`, `"entity"`, `"vessel"` or `"aircraft"` |
| `name`, `title` | The name and title as published |
| `aliases` | The alternate names from the ALT file |
| `dobs` | Dates of birth from the Remarks column (`DOB 10 Dec 1948; alt. DOB 1950`), as `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, or a range `YYYY/YYYY` |
| `nationalities` | `nationality` and `citizen` items from the Remarks column, as published |
| `programs` | OFAC program tags |
| `addresses` | One line per address |
| `call-sign`, `vessel-type`, `tonnage`, `grt`, `vessel-flag`, `vessel-owner` | The vessel columns |
| `remarks` | The Remarks column, continued from the comments file |
| `url` | The entry's OFAC Sanctions List Search page |

## Functions

| Namespace | Functions |
|---|---|
| `::ofac` | `fetch`, `current-version`, `manifest`, `parse-manifest`, `version`, `parse`, `parse-csv`, `parse-remarks`, `iso-date`, `export-url`, `detail-url`; types `Entry`, `EntryKind`; constant `SOURCES` |
| `::ofac::http` | `request`, `get-text`, `http-error`, `body-text`; type `HttpError` |

## Errors

- `fetch` returns an Err when a download fails, and when the file parses to no records, so a changed file format can't pass for an empty list.
- Download failures are `HttpError` (`status`, `url`, `message`). A failure on any of a list's four files fails the whole `fetch`, so a list without its aliases or addresses never comes back.
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Measured performance

On Hot 3.0.2's **debug** build, on a machine with a load average of about 25, parsing OFAC SDN took about 20 to 40 s. Expect a release build on an idle machine to be several times faster.

## Notes and limits

- The lists are public government publications, free, with no sandbox or keys.

## License

Apache-2.0 - see [LICENSE](LICENSE)
