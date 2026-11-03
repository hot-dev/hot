# ofac

The US Treasury OFAC SDN and Consolidated (non-SDN) lists for [`hot.dev/name-screening`](../name-screening/README.md). This package downloads the free Sanctions List Service files, parses them into normalized `Entry` records, and refreshes them into the screening cache. Screening itself lives in `hot.dev/name-screening`.

> **Screening never decides.** Every result is `"possible-match"` or `"no-match"`, with each match scored and explained. **A person must review every possible match** before anyone blocks, reports or clears a party. Names are shared by many people, lists carry several transliterations, and the score measures similarity, not probability. A `"no-match"` only means that no cached entry scored at or above the threshold on the lists searched. It is not a clearance, and the lists are only as current as the last refresh.
>
> **A list that is not installed or not refreshed was not searched.** You choose which lists to install, so a `"no-match"` covers those lists and nothing else. Every result names the lists searched in `sources`. Name the lists you need in the `sources` option, and a named list with no cached data is an Err.

Screen both OFAC sources for full US coverage. This package covers the OFAC lists only, so a name on the UK, EU or UN lists is not searched unless you install and refresh those too. To screen across all four official lists, see the `hot.dev/name-screening` README.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/ofac": "0.1.0"
}
```

`hot.dev/name-screening` comes with it as a dependency. It holds the `Entry` record, the cache and the scoring.

## Settings

The list needs no account or key. Refreshing and screening use the `hot.dev/name-screening` settings: `name-screening.store` (default `"name-screening"`) and `name-screening.max-age-days` (default `7`). See its README.

## Quick start

```hot
// Fill the cache once. Then refresh it daily from a schedule (below).
::ofac/refresh()

result ::name-screening/screen("Ayman al-Zawahiri", {dob: "1951", kind: "individual", sources: ::ofac/SOURCES})
result.status                       // "possible-match"
result.review-required              // true
first(result.matches).explanation   // ["name \"AL ZAWAHIRI, Dr. Ayman\" similarity 1", "year of birth matches (1951-06-19)"]
first(result.matches).entry.url     // the OFAC Sanctions List Search page
map(result.sources, (s) { s.list }) // ["OFAC SDN List", "OFAC Consolidated (non-SDN) List"]
```

### Refresh daily from a schedule

```hot
::my-app::ofac ns

refresh-ofac
meta {schedule: "0 5 * * *", retry: {attempts: 3, delay: 600000}}
fn (event) {
  ::ofac/refresh()
}
```

`refresh` returns one row per list: `{source, status: "updated" | "unchanged", version, entries, ms}`, or `{source, status: "error", error}`. `::ofac/refresh({sources: ["ofac-sdn"]})` refreshes one list.

## Lists and formats

| Source key | List | File used | Size | Change check |
|---|---|---|---|---|
| `ofac-sdn` | OFAC Specially Designated Nationals (about 19,500 entries) | SLS legacy CSV: `SDN.CSV` + `ALT.CSV` + `ADD.CSV` + `SDN_COMMENTS.CSV` | 5.8 + 1.1 + 1.7 MB | SHA-256 from the SLS file listing |
| `ofac-cons` | OFAC Consolidated (non-SDN) lists (SSI, FSE, NS-PLC, NS-MBS, CAPTA, NS-CMIC) | `CONS_PRIM.CSV` + `CONS_ALT.CSV` + `CONS_ADD.CSV` + `CONS_COMMENTS.CSV` | < 0.3 MB | SHA-256 or `lastUpdated` from the SLS file listing |

The Sanctions List Service (SLS) publishes every file at `https://sanctionslistservice.ofac.treas.gov/api/PublicationPreview/exports/<FILE>`, which redirects to a signed S3 URL. It lists the files with their size, date and SHA-256 at `POST /api/PublicationPreview/SdnList` and `.../ConsolidatedList`. The package uses the legacy CSV files because each one fits the default response cap. They carry the id, name, type, programs, aliases and addresses as columns. Dates of birth and nationalities come from the Remarks column (`DOB 10 Dec 1948; alt. DOB 1950; nationality Iran`), which is how OFAC publishes them in this format. `SDN.XML` (29 MB) and the Advanced XML (128 MB) are larger than the default cap. Each entry links to its OFAC Sanctions List Search page, and `id` is the OFAC `ent_num`.

## Functions

| Namespace | Functions |
|---|---|
| `::ofac` | `refresh`, `lists`, `fetch`, `parse`, `parse-csv`, `parse-remarks`, `manifest`, `parse-manifest`, `version`, `export-url`, `detail-url`; constant `SOURCES` |

## Errors

- A refresh that fails gives a `status: "error"` row and leaves the previous version cached, so a schedule keeps screening the last good data. The list then goes stale, and screening warns once it is older than `name-screening.max-age-days`.
- A refresh that parses to no entries counts as a failure too, so a changed file format can't empty the cache.
- `::name-screening/screen` returns an Err when a list you name has no cached data, and when no list can be searched at all. An empty cache can't pass as `"no-match"`.
- Download failures are `::name-screening::http/HttpError` (`status`, `url`, `message`).
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Measured performance

These numbers come from Hot 3.0.2's **debug** build, on a machine with a load average of about 25. Expect a release build on an idle machine to be several times faster. Parsing OFAC SDN took about 20 to 40 s, building its index about 130 s, and writing it about 13 to 40 s. The time goes to Hot-level string work per name, which is why refreshing belongs in a schedule. The cache holds about 13 MB of records for OFAC SDN, plus 256 buckets of postings.

## Notes and limits

- OFAC's sectoral and non-SDN designations are in `ofac-cons`. Screen both sources for full US coverage.
- The lists are public government publications, free, with no sandbox or keys.

## License

Apache-2.0 - see [LICENSE](LICENSE)
