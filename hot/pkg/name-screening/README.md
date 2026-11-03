# name-screening

Name screening for Hot over the lists you choose. It normalizes and transliterates names, compares them with token-set and Jaro-Winkler similarity, adjusts for date of birth and country, caches a compact name index in `::hot::store` with a scheduled refresh, and returns explained results for human review.

This package holds no list. Each list is its own package, and you can also pass in your own:

| Package | List |
|---|---|
| `hot.dev/ofac` | US OFAC SDN and Consolidated (non-SDN) lists |
| `hot.dev/uk-sanctions` | UK Sanctions List |
| `hot.dev/eu-sanctions` | EU Consolidated Financial Sanctions List |
| `hot.dev/un-sanctions` | UN Security Council Consolidated List |

`hot.dev/opensanctions` calls the OpenSanctions `/match` API with your own key instead of a local cache, and returns the same result shape.

> **Screening never decides.** Every result is `"possible-match"` or `"no-match"`, with each match scored and explained. **A person must review every possible match** before anyone blocks, reports or clears a party. Names are shared by many people, lists carry several transliterations, and the score measures similarity, not probability. A `"no-match"` only means that no cached entry scored at or above the threshold on the lists searched. It is not a clearance, and the lists are only as current as the last refresh.
>
> **A list that is not installed or not refreshed was not searched.** You choose which lists to install, so a `"no-match"` covers those lists and nothing else. Every result names the lists searched in `sources`. Name the lists you need in the `sources` option, and a named list with no cached data is an Err.

## Install

Add this package and the lists you want to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/name-screening": "0.1.0",
  "hot.dev/ofac": "0.1.0",
  "hot.dev/uk-sanctions": "0.1.0",
  "hot.dev/eu-sanctions": "0.1.0",
  "hot.dev/un-sanctions": "0.1.0"
}
```

## Settings

Every setting below is optional. Each comes from per-call `opts` first, then the Context Variable, then the default (`::name-screening::config/resolve`). Per-call values are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `name-screening.store` | `store` | `"name-screening"` | Name of the cache's metadata store. Each list version gets its own data store, `<store>:<source>:<hash>` |
| `name-screening.max-age-days` | `max-age-days` | `7` | Lists not refreshed for longer produce a warning in screening results |

## Screen across all four official lists

Keep the lists your project must search in one place, and use it for both refreshing and screening. Passing `sources` means a list that was never refreshed is an Err, not a gap.

```hot
::my-app::screening ns

::cache ::name-screening::cache

// Every list this project must search.
lists fn (): Vec<Map> {
  concat(::ofac/lists(), ::uk::sanctions/lists(), ::eu::sanctions/lists(), ::un::sanctions/lists())
}

sources fn (): Vec<Str> {
  map(lists(), (l) { l.source })
}

// Fills the cache, then keeps it current. The EU list needs
// http.max-response-bytes of at least 33554432 (32 MiB) for this run.
refresh-screening-lists
meta {schedule: "0 5 * * *", retry: {attempts: 3, delay: 600000}}
fn (event) {
  ::cache/refresh-lists(lists())
}

screen fn (name: Str, opts: Map): Map {
  ::name-screening/screen(name, {...opts, sources: sources()})
}
```

```hot
result ::my-app::screening/screen("Ayman al-Zawahiri", {dob: "1951", kind: "individual"})
result.status                       // "possible-match"
result.review-required              // true
first(result.matches).score         // 1.0
first(result.matches).explanation   // ["name \"AL ZAWAHIRI, Dr. Ayman\" similarity 1", "year of birth matches (1951-06-19)"]
first(result.matches).entry.url     // the OFAC Sanctions List Search page
map(result.sources, (s) { s.list }) // the five lists searched
```

`refresh-lists` returns one row per list: `{source, status: "updated" | "unchanged", version, entries, ms}`, or `{source, status: "error", error}`. A list that fails keeps its previous version cached, and the other lists are still refreshed. To keep each run short, give lists their own schedule:

```hot
refresh-ofac
meta {schedule: "0 5 * * *"}
fn (event) {
  ::ofac/refresh()
}

refresh-eu-uk-un
meta {schedule: "30 5 * * *"}
fn (event) {
  ::cache/refresh-lists(lists(), {sources: ["uk", "eu", "un"]})
}
```

## What was searched

Every result says what it covers. `sources` holds one record per list searched, `{source, list, version, entries, fetched-at, checked-at}`, and `notice` ends with `Searched:` and the list names, so a stored `"no-match"` carries its coverage. Nothing outside `sources` was searched.

| Situation | Result |
|---|---|
| `sources` names lists, all cached | Those lists are searched |
| `sources` names a list with no cached data | Err naming the list |
| The same, with `allow-missing: true` | The other lists are searched. The list is named in `warnings` and `not-searched` |
| `sources` left out | Every list a refresh has been tried for is searched, and a warning says that any other list was not. A list whose first refresh failed is an Err, as above |
| Nothing cached, or nothing left to search | Always an Err, even with `allow-missing: true`. It is never a `"no-match"` |
| A list older than `max-age-days` | It is searched, and named in `warnings` |

Leave `sources` out only for a quick look. This package can't see which list packages you installed, so without `sources` it can't tell that a list you meant to use was never refreshed. It can only warn. `::name-screening::cache/status({sources: [...]})` shows the state of each list.

## Screen with options

```hot
r ::name-screening/screen("Banco Nacional de Cuba", {
  kind: "entity",               // or "individual", "vessel", "aircraft", or a Vec of kinds
  country: "CU",                // ISO code or name
  threshold: 0.8,               // default 0.85
  sources: ["ofac-sdn", "un"],  // the lists that must be searched
  limit: 5                      // default 10
})
map(r.matches, (m) { `${m.score} ${m.entry.source}:${m.entry.id} ${m.matched-name}` })
```

## Your own list

A list is a Map. Pass `entries` for data you already hold, or a `fetch` Fn that downloads it. `fetch` takes the cached version (a Str, or null) and returns `{version, entries, unchanged, published}`, or an Err to keep the previous version.

```hot
::entry ::name-screening::entry

watchlist {
  source: "internal",
  list: "Internal Watchlist",
  entries: [::entry/make({source: "internal", list: "Internal Watchlist", id: "1", kind: "entity", name: "Acme Trading Ltd"})]
}

::name-screening::cache/refresh-source(watchlist)
::name-screening/screen("Acme Trading", {sources: ["internal"]})
```

Without `version`, a hash of the entries is used, so unchanged data is not re-indexed. A list package exposes the same Map from a `lists` function, and uses `::name-screening::http` and `::name-screening::xml` for downloads and parsing.

Every list is parsed into the same `::name-screening::entry/Entry`:

| Field | Meaning |
|---|---|
| `source`, `list` | Source key (`"ofac-sdn"`, `"uk"`, ...) and list name |
| `id` | The source's own identifier |
| `kind` | `"individual"`, `"entity"`, `"vessel"` or `"aircraft"` |
| `name`, `aliases` | Primary name as published, and every other name (a.k.a., f.k.a., name variations, original script) |
| `dobs` | ISO dates: `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, or a range `YYYY/YYYY` |
| `nationalities` | ISO 3166-1 alpha-2 codes where known, otherwise as published |
| `programs` | The source's programs, regimes or list types |
| `addresses` | One line per address |
| `remarks`, `url` | Free-text remarks (capped at 2,000 characters), and where to read the listing |

## Functions by area

| Namespace | Functions |
|---|---|
| `::name-screening` | `screen` |
| `::name-screening::cache` | `refresh-lists`, `refresh-source`, `status`, `meta-of`, `open-sources`, `check-sources`, `check-list`, `select-lists`, `fetch-list`, `data-store-name` |
| `::name-screening::screen` | `screen-index` (in-memory indexes), `query-of`, `score-record`, `score-candidates`, `candidates-for`, `dob-adjustment`, `country-adjustment`, `result`, `notice-for`; types `Match`, `ScreenResult` |
| `::name-screening::entry` | `make`, `iso-date`, `year-range`, `clean-list`, `present`; types `Entry`, `EntryKind` |
| `::name-screening::index` | `build`, `record`, `rank-candidates`, `bucket-of`, `record-key`, `entry-countries` |
| `::name-screening::text` | `fold`, `clean`, `tokens`, `normalize`, `skeleton`, `blocking-keys`, `token-keys`, `key-tokens`, `merge-particles`, `collapse-repeats`, `chars` |
| `::name-screening::similarity` | `jaro`, `jaro-winkler`, `levenshtein`, `levenshtein-ratio`, `token-sim`, `token-set`, `name-similarity`, `round4` |
| `::name-screening::country` | `code`, `key`, `name` |
| `::name-screening::config` | `resolve` |
| `::name-screening::http` | `get-text`, `head`, `request`, `byte-ranges`, `http-error`, `body-text`, `too-large-message`; type `HttpError` |
| `::name-screening::xml` | `records`, `blocks`, `block`, `texts`, `text`, `start-tags`, `attr`, `decode` |

## How screening scores

1. **Normalize.** Names are lower-cased and transliterated with an explicit table, because hot-std has no Unicode normalization. The table covers Latin diacritics and ligatures (`ß` → `ss`), Russian, Ukrainian and Belarusian Cyrillic, and Greek. Apostrophes and dots are dropped, and other punctuation becomes a space. Honorifics (`Dr`, `Sheikh`) are dropped from individual names. Legal forms (`LLC`, `Ltd`, `GmbH`, `S.A.`, `Joint Stock Company`, `OOO`, ...) are dropped from entity, vessel and aircraft names. Scripts outside the table, such as Arabic, are kept as they are, so they match exactly but not fuzzily.
2. **Find candidates.** Each list's index maps blocking keys to entry ids. A token's keys are a coarse phonetic skeleton (so `mohammed`, `muhammad` and `mohamad` share `s:mhmd`) and its first four letters. The keys sit in 256 buckets per list. A screening call reads the query's buckets, ranks ids by the summed weight of shared keys (rare keys weigh more), and loads the top `max-candidates` (default 50) per list. A typo in a single-token name's first letters can miss the candidate stage.
3. **Score names.** The query is scored against the primary name and every alias, and the best counts. The name score is a soft token-set (Dice) score: every token takes its best Jaro-Winkler match on the other side (pairs with different first letters, or below 0.8, count 0), so word order doesn't matter, and extra words lower the score. Unmatched name particles (`al`, `bin`, `van`) are not counted, and a second pass joins particles to the next word (`al zawahiri` against `alzawahiri`).
4. **Adjust.** With a `dob`: +0.05 when the full date matches, +0.02 when the year matches or falls inside a listed range, nothing within one year, and -0.15 when every listed date of birth is further off. With a `country`: +0.03 when it matches a nationality or address country, and -0.05 when the entry lists countries and none of them match. Missing data is never penalized.
5. **Return.** Scores are clamped to [0, 1] and rounded to four places. Matches at or above `threshold` come back best first, with ties broken by source and id, so results are deterministic. Each match has `matched-field` (`"name"` or `"alias"`), `matched-fields` (plus `"dob"` and `"country"` when they contributed), and an `explanation`.

Tune `threshold` to your risk appetite. Lower values catch more spelling variants, and every extra hit is still a possible match for a person to review.

## Errors

- `screen` returns an Err when a required list has no cached data, so an empty cache can't pass as `"no-match"`. Pass `allow-missing: true` to skip such lists. They are then named in `warnings` and `not-searched`. When no list can be searched at all, it is an Err either way. See [What was searched](#what-was-searched).
- A refresh that fails returns an Err, or an `error` row in `refresh-lists`, and leaves the previous version in place. So does a list that returns no entries, since a changed file format could otherwise empty the cache. If the failure is a list's first refresh, the list is remembered without data, so screening without `sources` still reports it. Download failures are `::name-screening::http/HttpError` (`status`, `url`, `message`).
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Measured performance

These numbers come from Hot 3.0.2's **debug** build, on a machine with a load average of about 25. Expect a release build on an idle machine to be several times faster.

**Screening one name** against a cached list of 19,484 entries and a list of about 1,000 from a local SQLite store took 0.6 to 8.6 s across eight test names, typically 3 to 5 s. That is up to 50 candidates scored per list. Common names such as "Mohammed Ali" reach the most candidates and are slowest. Lower `max-candidates` to trade recall for speed.

Refresh times and cache sizes are in each list package's README.

## Data handling

- Screening runs locally against the cache. Names never leave your Hot environment.
- The package logs nothing. Error messages carry list URLs and HTTP statuses, never the names screened.
- Screening results contain personal data from public lists and the subject you screened. Store and retain them under your own compliance policy.

## Notes and limits

- Fuzzy matching only works within the transliteration table. A query in Arabic script matches only Arabic-script aliases a list publishes.
- A refresh writes a whole new store before switching to it and then deletes the old one. A screening call that started on the old version just before the switch can miss entries. Run refreshes outside business hours.
- With `allow-missing: true`, a mistyped source key is a warning rather than an Err, because the package has no fixed set of lists to check it against.
- `::name-screening::xml` reads XML with regexes over record strings and uses `::hot::xml` only to decode text. In Hot that is nearly twice as fast as walking a parsed tree: 55 s against 91 to 109 s for a 22 MB file on a debug build, with identical entries. It relies on the file's format (record elements do not nest inside themselves, and there are no CDATA sections or comments inside records).

## License

Apache-2.0 - see [LICENSE](LICENSE)
