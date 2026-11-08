# gdelt

The [GDELT DOC 2.0 API](https://blog.gdeltproject.org/gdelt-doc-2-0-api-debuts/) for Hot: worldwide news coverage in 65 languages, with no key. Covers article lists and coverage timelines by query, timespan or date range.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/gdelt": "0.1.1"
}
```

## Auth and settings

GDELT takes no credential. The one setting comes from per-call `opts` first, then the Context Variable, then the default (`::gdelt::api/resolve`). Every API function takes a trailing `opts` Map.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `gdelt.base-url` | `base-url` | `https://api.gdeltproject.org/api/v2/doc/doc` | DOC API endpoint |

## Quick start

```hot
// Latest English-language coverage from the past day
found ::gdelt/articles("\"data breach\" sourcelang:english", {timespan: "24h", sort: "DateDesc", max-records: 25})
first(found).title
::gdelt/iso-date(first(found).seendate)   // "2026-11-03T12:15:00Z"

// A fixed window, in UTC
::gdelt/articles("(ransomware OR extortion) domain:reuters.com", {start: "2026-10-01", end: "2026-10-08T00:00:00Z"})

// Coverage over time
body ::gdelt/timeline("\"data breach\"", {timespan: "7d"})
::gdelt/timeline-points(body)             // [{series, date, value}, ...]
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::gdelt` | `articles`, `timeline`. Parsers: `article-list`, `timeline-points`. Helpers: `doc-params`, `datetime`, `iso-date` |
| `::gdelt::api` | `resolve`, `request`, `get-json`, `parse-body`, `query-string`, `http-error`, `query-error`; types `HttpError`, `QueryError` |

Options for both calls:

| opts key | API parameter | Notes |
|---|---|---|
| `timespan` | `timespan` | `15min`, `24h`, `7d`, `2w`, `3m`. Minimum 15 minutes, default 3 months |
| `start`, `end` | `startdatetime`, `enddatetime` | UTC: `YYYYMMDDHHMMSS`, `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SSZ` |
| `sort` | `sort` | `DateDesc`, `DateAsc`, `ToneDesc`, `ToneAsc`, `HybridRel`. Default relevance |
| `max-records` | `maxrecords` | `articles` only. Default 75, capped at 250 |
| `mode` | `mode` | `timeline` only: `TimelineVol` (default), `TimelineVolRaw`, `TimelineVolInfo`, `TimelineTone`, `TimelineLang`, `TimelineSourceCountry` |
| `smooth` | `timelinesmooth` | `timeline` only. Moving window, capped at 30 steps |

All query operators go inside the query string: `"exact phrase"`, `(a OR b)` (no nesting), `-excluded`, `domain:`, `domainis:`, `sourcecountry:`, `sourcelang:`, `theme:`, `near20:"a b"`, `repeat3:"word"`, `tone<-5`.

## Pagination

GDELT doesn't paginate. `articles` returns at most 250 records for one request and there is no offset, so to see more, split the window with `start` and `end`.

## Errors

Non-2xx responses come back as an Err carrying `::gdelt::api/HttpError`:

- `source`: `"gdelt"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` when GDELT sends one
- `message`: a one-line summary with the start of GDELT's own text

A success response that isn't JSON comes back as an Err carrying `::gdelt::api/QueryError`, with `source`, `url`, `text` (GDELT's whole message) and `message`. GDELT is reported to reject bad queries this way, as plain text with a 200, even with `format=json`. That wasn't confirmed against the live API, because the test request was rate limited, and the docs don't describe errors.

A response with no `articles` is `[]`, not an Err. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx, with a delay well above five seconds.

## Notes and limits

- **Rate limit**: GDELT's 429 response says "Please limit requests to one every 5 seconds", and points high-traffic users to its [web n-grams dataset](https://blog.gdeltproject.org/using-the-new-web-ngrams-dataset-to-find-relevant-coverage/). The package doesn't pace requests, so space them out in your flow. The API can answer 429 even for a client well under this rate: from the test IP it did so 45 seconds after a successful request. That response carries no `Retry-After`. Responses set `Cache-Control: max-age=900`.
- **Window**: the docs give the searchable window as a rolling three months.
- **Text**: the API returns links and metadata, not article text. GDELT searches machine translations of 65 languages, so a match can come from an article in another language. `seendate` is when GDELT saw the article, not its publication date.
- **Matches are not findings**: a query match means a word appeared in coverage. It says nothing about a particular company or person.
- **Other modes**: image, tone chart and word cloud modes aren't wrapped.

## License

Apache-2.0 - see [LICENSE](LICENSE)
