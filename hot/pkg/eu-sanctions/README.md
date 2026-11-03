# eu-sanctions

The EU Consolidated Financial Sanctions List for [`hot.dev/name-screening`](../name-screening/README.md). This package downloads the free Financial Sanctions Files (FSF) XML, parses it into normalized `Entry` records, and refreshes it into the screening cache. Screening itself lives in `hot.dev/name-screening`.

> **Screening never decides.** Every result is `"possible-match"` or `"no-match"`, with each match scored and explained. **A person must review every possible match** before anyone blocks, reports or clears a party. Names are shared by many people, lists carry several transliterations, and the score measures similarity, not probability. A `"no-match"` only means that no cached entry scored at or above the threshold on the lists searched. It is not a clearance, and the lists are only as current as the last refresh.
>
> **A list that is not installed or not refreshed was not searched.** You choose which lists to install, so a `"no-match"` covers those lists and nothing else. Every result names the lists searched in `sources`. Name the lists you need in the `sources` option, and a named list with no cached data is an Err.

This package covers the EU list only, so a name on the OFAC, UK or UN lists is not searched unless you install and refresh those too. To screen across all four official lists, see the `hot.dev/name-screening` README.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/eu-sanctions": "0.1.0"
}
```

`hot.dev/name-screening` comes with it as a dependency. It holds the `Entry` record, the cache and the scoring.

## Settings

The list needs no account or key. Refreshing and screening use the `hot.dev/name-screening` settings: `name-screening.store` (default `"name-screening"`) and `name-screening.max-age-days` (default `7`). See its README.

Runtime setting: **`http.max-response-bytes` must be at least 33554432 (32 MiB) for the run that refreshes this list.** The file is about 26 MB, and its server ignores Range requests. Below that, the refresh fails with a message saying how far to raise it, and the previous version stays cached. If your runtime caps responses lower, this list can't be refreshed, and it was not searched.

## Quick start

```hot
// Fill the cache once. Then refresh it daily from a schedule (below).
::eu::sanctions/refresh()

result ::name-screening/screen("Asbat al-Ansar", {kind: "entity", sources: ::eu::sanctions/SOURCES})
result.status                       // "possible-match"
result.review-required              // true
first(result.matches).explanation
map(result.sources, (s) { s.list }) // ["EU Consolidated Financial Sanctions List"]
```

### Refresh daily from a schedule

```hot
::my-app::eu-list ns

refresh-eu-sanctions
meta {schedule: "0 5 * * *", retry: {attempts: 3, delay: 600000}}
fn (event) {
  ::eu::sanctions/refresh()
}
```

`refresh` returns one row: `{source, status: "updated" | "unchanged", version, entries, ms}`, or `{source, status: "error", error}`.

## List and format

| Source key | List | File used | Size | Change check |
|---|---|---|---|---|
| `eu` | EU Consolidated Financial Sanctions List (FSF), about 6,300 | `xmlFullSanctionsList_1_1` | 26 MB, needs a raised cap | Last-Modified (HEAD) |

The FSF file URL takes a `token` query parameter. Its value, `dG9rZW4tMjAxNw`, is a fixed public token (base64 for `token-2017`), not a credential, and no account is needed. `id` is the EU logical id, and `programs` holds the programme codes.

## Functions

| Namespace | Functions |
|---|---|
| `::eu::sanctions` | `refresh`, `lists`, `fetch`, `parse`, `parse-entity`; constants `SOURCES`, `NEEDED_RESPONSE_BYTES` |

## Errors

- A refresh that fails gives a `status: "error"` row and leaves the previous version cached, so a schedule keeps screening the last good data. The list then goes stale, and screening warns once it is older than `name-screening.max-age-days`.
- A refresh that parses to no entries counts as a failure too, so a changed file format can't empty the cache.
- `::name-screening/screen` returns an Err when a list you name has no cached data, and when no list can be searched at all. An empty cache can't pass as `"no-match"`.
- Download failures are `::name-screening::http/HttpError` (`status`, `url`, `message`).
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Measured performance

On Hot 3.0.2's **debug** build, on a machine with a load average of about 25, indexing took about 90 s. Expect a release build on an idle machine to be several times faster. The cache holds a few MB of records, plus 256 buckets of postings.

## Notes and limits

- The file needs `http.max-response-bytes` of at least 32 MiB. See [Settings](#settings).
- The list is a public publication, free, with no sandbox or keys.
- The list is read with regexes over record strings (`::name-screening::xml`), which relies on its format. See the `hot.dev/name-screening` README.

## License

Apache-2.0 - see [LICENSE](LICENSE)
