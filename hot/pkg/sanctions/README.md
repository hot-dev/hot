# sanctions

Sanctions screening for Hot over the free, official lists: the US OFAC SDN and Consolidated (non-SDN) lists, the UK Sanctions List, the EU Consolidated Financial Sanctions List and the UN Security Council Consolidated List. The package downloads and parses each list into one normalized `Entry` record, caches a compact name index in `::hot::store` with a scheduled refresh, and screens names with transliteration, token-set and Jaro-Winkler similarity, and date-of-birth and country adjustments. An optional bring-your-own-key backend calls the OpenSanctions `/match` API.

> **Screening never decides.** Every result is `"possible-match"` or `"no-match"`, with each match scored and explained. **A person must review every possible match** before anyone blocks, reports or clears a party. Names are shared by many people, lists carry several transliterations, and the score measures similarity, not probability. A `"no-match"` only means that no cached entry scored at or above the threshold on the lists searched. It is not a clearance, and the lists are only as current as the last refresh.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/sanctions": "0.1.0"
}
```

## Settings

The official lists need no account or key. Every setting below is optional. Each comes from per-call `opts` first, then the Context Variable, then the default (`::sanctions::config/resolve`). Per-call values are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `sanctions.store` | `store` | `"sanctions"` | Name of the cache's metadata store. Each list version gets its own data store, `<store>:<source>:<hash>` |
| `sanctions.max-age-days` | `max-age-days` | `7` | Lists not refreshed for longer produce a warning in screening results |
| `sanctions.opensanctions.api-key` | `api-key` | none | OpenSanctions API key (secret), only for `::sanctions::opensanctions` |
| `sanctions.opensanctions.base-url` | `base-url` | `https://api.opensanctions.org` | Your own self-hosted yente server, if you run one |
| `sanctions.opensanctions.dataset` | `dataset` | `"default"` | `"default"` (sanctions plus PEPs and more) or `"sanctions"` |

Runtime setting: **`http.max-response-bytes` must be at least 33554432 (32 MiB) for the run that refreshes the EU list.** The EU file is about 26 MB, and its server ignores Range requests. The UK file (about 22 MB) is fetched in 8 MiB Range requests, so it works with the default 16 MiB cap.

## Quick start

```hot
// Fill the cache once. Then refresh it daily from a schedule (below).
::sanctions::cache/refresh-lists()

result ::sanctions/screen("Ayman al-Zawahiri", {dob: "1951", kind: "individual"})
result.status                       // "possible-match"
result.review-required              // true
first(result.matches).score         // 1.0
first(result.matches).explanation   // ["name \"AL ZAWAHIRI, Dr. Ayman\" similarity 1", "year of birth matches (1951-06-19)"]
first(result.matches).entry.url     // the OFAC Sanctions List Search page
```

### Refresh daily from a schedule

```hot
::my-app::sanctions ns

refresh-sanctions-lists
meta {schedule: "0 5 * * *", retry: {attempts: 3, delay: 600000}}
fn (event) {
  ::sanctions::cache/refresh-lists()
}
```

`refresh-lists` returns one row per list: `{source, status: "updated" | "unchanged", version, entries, ms}`, or `{source, status: "error", error}`. A list that fails keeps its previous version cached, and the other lists are still refreshed. To keep each run short, give each list its own schedule:

```hot
::my-app::sanctions-split ns

refresh-ofac
meta {schedule: "0 5 * * *"}
fn (event) {
  ::sanctions::cache/refresh-lists({sources: ["ofac-sdn", "ofac-cons"]})
}

refresh-eu-uk-un
meta {schedule: "30 5 * * *"}
fn (event) {
  ::sanctions::cache/refresh-lists({sources: ["uk", "eu", "un"]})
}
```

### Screen with options

```hot
r ::sanctions/screen("Banco Nacional de Cuba", {
  kind: "entity",               // or "individual", "vessel", "aircraft", or a Vec of kinds
  country: "CU",                // ISO code or name
  threshold: 0.8,               // default 0.85
  sources: ["ofac-sdn", "un"],  // default: all five lists
  limit: 5                      // default 10
})
map(r.matches, (m) { `${m.score} ${m.entry.source}:${m.entry.id} ${m.matched-name}` })
```

### OpenSanctions (optional, your own key)

```hot
os ::sanctions::opensanctions/screen("Aleksandr Lukashenko", {kind: "individual", dataset: "sanctions", api-key: "..."})
os.status
```

## Lists and formats

| Source key | List | File used | Size | Change check |
|---|---|---|---|---|
| `ofac-sdn` | OFAC Specially Designated Nationals (about 19,500 entries) | SLS legacy CSV: `SDN.CSV` + `ALT.CSV` + `ADD.CSV` + `SDN_COMMENTS.CSV` | 5.8 + 1.1 + 1.7 MB | SHA-256 from the SLS file listing |
| `ofac-cons` | OFAC Consolidated (non-SDN) lists (SSI, FSE, NS-PLC, NS-MBS, CAPTA, NS-CMIC) | `CONS_PRIM.CSV` + `CONS_ALT.CSV` + `CONS_ADD.CSV` + `CONS_COMMENTS.CSV` | < 0.3 MB | SHA-256 or `lastUpdated` from the SLS file listing |
| `uk` | UK Sanctions List (FCDO), about 6,400 designations | `UK-Sanctions-List.xml` | 22 MB, Range-chunked | ETag (HEAD) |
| `eu` | EU Consolidated Financial Sanctions List (FSF), about 6,300 | `xmlFullSanctionsList_1_1` | 26 MB, needs a raised cap | Last-Modified (HEAD) |
| `un` | UN Security Council Consolidated List, about 1,000 | `consolidated.xml` (English) | 2 MB | SHA-256 of the body |

- **OFAC**: the Sanctions List Service (SLS) publishes every file at `https://sanctionslistservice.ofac.treas.gov/api/PublicationPreview/exports/<FILE>`, which redirects to a signed S3 URL. It lists the files with their size, date and SHA-256 at `POST /api/PublicationPreview/SdnList` and `.../ConsolidatedList`. The package uses the legacy CSV files because each one fits the default response cap. They carry the id, name, type, programs, aliases and addresses as columns. Dates of birth and nationalities come from the Remarks column (`DOB 10 Dec 1948; alt. DOB 1950; nationality Iran`), which is how OFAC publishes them in this format. `SDN.XML` (29 MB) and the Advanced XML (128 MB) are larger than the default cap. Each entry links to its OFAC Sanctions List Search page.
- **UK**: the OFSI Consolidated List closed on 28 January 2026, and the FCDO UK Sanctions List is now the only source of UK designations. The XML is used: the CSV is about 50 MB because it repeats every field once per name.
- **EU**: the FSF file URL takes a `token` query parameter. Its value, `dG9rZW4tMjAxNw`, is a fixed public token (base64 for `token-2017`), not a credential, and no account is needed.
- **UN**: `https://scsanctions.un.org/resources/xml/en/consolidated.xml` redirects to a signed Azure Blob URL. The server answers GET only, so the body hash detects changes.

Every list is parsed into the same `::sanctions::entry/Entry`:

| Field | Meaning |
|---|---|
| `source`, `list` | Dataset key (`"ofac-sdn"`, `"uk"`, ...) and list name |
| `id` | OFAC `ent_num`, UK Unique ID, EU logical id, or UN reference number |
| `kind` | `"individual"`, `"entity"`, `"vessel"` or `"aircraft"` |
| `name`, `aliases` | Primary name as published, and every other name (a.k.a., f.k.a., name variations, original script) |
| `dobs` | ISO dates: `YYYY-MM-DD`, `YYYY-MM`, `YYYY`, or a range `YYYY/YYYY` |
| `nationalities` | ISO 3166-1 alpha-2 codes where known, otherwise as published |
| `programs` | OFAC program tags, UK regime names, EU programme codes, UN list types |
| `addresses` | One line per address |
| `remarks`, `url` | Free-text remarks (capped at 2,000 characters), and where to read the listing |

## Functions by area

| Namespace | Functions |
|---|---|
| `::sanctions` | `screen` |
| `::sanctions::cache` | `refresh-lists`, `refresh-source`, `status`, `meta-of`, `fetch-source`, `open-sources`, `check-sources`, `data-store-name` |
| `::sanctions::screen` | `screen-index` (in-memory indexes), `query-of`, `score-record`, `score-candidates`, `candidates-for`, `dob-adjustment`, `country-adjustment`, `result`; types `Match`, `ScreenResult` |
| `::sanctions::index` | `build`, `record`, `rank-candidates`, `bucket-of`, `record-key`, `entry-countries` |
| `::sanctions::text` | `fold`, `clean`, `tokens`, `normalize`, `skeleton`, `blocking-keys`, `token-keys`, `key-tokens`, `merge-particles`, `collapse-repeats`, `chars` |
| `::sanctions::similarity` | `jaro`, `jaro-winkler`, `levenshtein`, `levenshtein-ratio`, `token-sim`, `token-set`, `name-similarity`, `round4` |
| `::sanctions::country` | `code`, `key`, `name` |
| `::sanctions::ofac` | `parse`, `parse-csv`, `parse-remarks`, `manifest`, `parse-manifest`, `version`, `fetch`, `export-url`, `detail-url` |
| `::sanctions::uk`, `::sanctions::eu`, `::sanctions::un` | `parse`, `parse-designation` / `parse-entity` / `parse-record`, `fetch` |
| `::sanctions::opensanctions` | `screen`, `match-body`, `match-query`, `match-url`, `parse-response`, `to-match` |
| `::sanctions::config` | `resolve`, `require-api-key` |
| `::sanctions::http` | `get-text`, `head`, `request`, `byte-ranges`, `http-error`, `body-text`; type `HttpError` |
| `::sanctions::xml` | `records`, `blocks`, `block`, `texts`, `text`, `start-tags`, `attr`, `decode` |

## How screening scores

1. **Normalize.** Names are lower-cased and transliterated with an explicit table, because hot-std has no Unicode normalization. The table covers Latin diacritics and ligatures (`ß` → `ss`), Russian, Ukrainian and Belarusian Cyrillic, and Greek. Apostrophes and dots are dropped, and other punctuation becomes a space. Honorifics (`Dr`, `Sheikh`) are dropped from individual names. Legal forms (`LLC`, `Ltd`, `GmbH`, `S.A.`, `Joint Stock Company`, `OOO`, ...) are dropped from entity, vessel and aircraft names. Scripts outside the table, such as Arabic, are kept as they are, so they match exactly but not fuzzily.
2. **Find candidates.** Each list's index maps blocking keys to entry ids. A token's keys are a coarse phonetic skeleton (so `mohammed`, `muhammad` and `mohamad` share `s:mhmd`) and its first four letters. The keys sit in 256 buckets per list. A screening call reads the query's buckets, ranks ids by the summed weight of shared keys (rare keys weigh more), and loads the top `max-candidates` (default 50) per list. A typo in a single-token name's first letters can miss the candidate stage.
3. **Score names.** The query is scored against the primary name and every alias, and the best counts. The name score is a soft token-set (Dice) score: every token takes its best Jaro-Winkler match on the other side (pairs with different first letters, or below 0.8, count 0), so word order doesn't matter, and extra words lower the score. Unmatched name particles (`al`, `bin`, `van`) are not counted, and a second pass joins particles to the next word (`al zawahiri` against `alzawahiri`).
4. **Adjust.** With a `dob`: +0.05 when the full date matches, +0.02 when the year matches or falls inside a listed range, nothing within one year, and -0.15 when every listed date of birth is further off. With a `country`: +0.03 when it matches a nationality or address country, and -0.05 when the entry lists countries and none of them match. Missing data is never penalized.
5. **Return.** Scores are clamped to [0, 1] and rounded to four places. Matches at or above `threshold` come back best first, with ties broken by source and id, so results are deterministic. Each match has `matched-field` (`"name"` or `"alias"`), `matched-fields` (plus `"dob"` and `"country"` when they contributed), and an `explanation`.

Tune `threshold` to your risk appetite. Lower values catch more spelling variants, and every extra hit is still a possible match for a person to review.

## Errors

- `screen` returns an Err when a requested list has never been refreshed, so an empty cache can't pass as `"no-match"`. Pass `allow-missing: true` to skip such lists. They are then named in `warnings`.
- A refresh that fails returns an Err, or an `error` row in `refresh-lists`, and leaves the previous version in place. Download failures are `::sanctions::http/HttpError` (`status`, `url`, `message`). An EU file over the response cap gives a message saying how far to raise `http.max-response-bytes`.
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Measured performance

These numbers come from Hot 3.0.2's **debug** build, on a machine with a load average of about 25. Expect a release build on an idle machine to be several times faster.

- **Screening one name** against the cached OFAC SDN (19,484 entries) and UN lists from a local SQLite store took 0.6 to 8.6 s across eight test names, typically 3 to 5 s. That is up to 50 candidates scored per list. Common names such as "Mohammed Ali" reach the most candidates and are slowest. Lower `max-candidates` to trade recall for speed.
- **Refreshing** (debug build): parsing OFAC SDN took about 20 to 40 s, building its index about 130 s, and writing it about 13 to 40 s. UK took about 60 s to index, EU about 90 s, and UN about 10 s. The time goes to Hot-level string work per name, which is why `refresh-lists` belongs in a schedule.
- **Cache size**: about 13 MB of records for OFAC SDN, plus 256 buckets of postings, and a few MB for each other list.

## Data handling

- Screening runs locally against the cache. Names never leave your Hot environment unless you call `::sanctions::opensanctions/screen`, which sends the name, date of birth and country to OpenSanctions (or to your own yente server via `base-url`).
- The package logs nothing. Error messages carry list URLs and HTTP statuses, never the names screened or API keys.
- Screening results contain personal data from public lists and the subject you screened. Store and retain them under your own compliance policy.

## Notes and limits

- **OpenSanctions licensing**: OpenSanctions data is licensed CC BY-NC 4.0. It is free for non-commercial use, and **commercial use needs a license**. The hosted API is a paid service with a free trial. See [opensanctions.org/licensing](https://www.opensanctions.org/licensing/). The official lists this package downloads are public government publications.
- The official lists are free and have no sandbox or keys. OpenSanctions offers API trial keys.
- OFAC's sectoral and non-SDN designations are in `ofac-cons`. Screen both OFAC sources for full US coverage.
- Fuzzy matching only works within the transliteration table. A query in Arabic script matches only the Arabic-script aliases that the UK and UN lists publish.
- The EU file needs `http.max-response-bytes` of at least 32 MiB. If your runtime caps responses lower, leave `eu` out of `refresh-lists` and `screen`.
- A refresh writes a whole new store before switching to it and then deletes the old one. A screening call that started on the old version just before the switch can miss entries. Run refreshes outside business hours.
- `::hot::xml` is not used: the package reads the list XML with regexes over record strings (`::sanctions::xml`). That is faster in Hot, and it decodes entities correctly. It relies on each list's current layout.

## License

Apache-2.0 - see [LICENSE](LICENSE)
