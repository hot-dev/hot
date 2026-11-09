# ransomware-live

The [ransomware.live](https://www.ransomware.live/) metadata API for Hot: recent claimed victims, victims by group, country or keyword, and ransomware groups. Works with the free v2 API, or the PRO API with a free key.

> **Victim names are claims, not confirmed breaches.** The data comes from what criminal groups post on their leak sites. Groups exaggerate, recycle old data and name the wrong company. ransomware.live's own terms say inclusion is no confirmation that an event occurred as described. Verify a claim independently before acting on it, and don't present it as fact.

> **Business use: read this first.** The site labels the free v2 API "Personal use only" and "not intended for corporate or business use". Its [Terms & Conditions](https://www.ransomware.live/t&c) (updated 19 August 2026) direct users to the PRO API with a free key, and they go further: **commercial use is "strictly prohibited without prior written approval" from the owner**, with or without a key. That covers including the data in commercial products or services, paid consultancy, and monetizing insights from it. A PRO key alone doesn't give you the right. Request approval through the [contact page](https://ransomware.live/contact) before using this package in a commercial product.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/ransomware-live": "0.1.2"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::ransomware-live::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `ransomware-live.api-key` | `api-key` | none | PRO key (secret), free from [ransomware.live/my](https://www.ransomware.live/my), sent as `X-API-KEY`. With a key every call goes to the PRO host. Without one it goes to the free v2 host |
| `ransomware-live.base-url` | `base-url` | `https://api.ransomware.live/v2` | Free v2 API root |
| `ransomware-live.pro-base-url` | `pro-base-url` | `https://api-pro.ransomware.live` | PRO API root |

`api-key` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable. A per-call `pro-base-url` needs an `api-key` in the same `opts`: the context key is never sent to a per-call URL.

## Quick start

```hot
::rl ::ransomware-live

// Latest claims, on the free API
latest ::rl/recent-victims()
first(latest).group

// With a PRO key
hits ::rl/search-victims("hospital", {api-key: "tenant-key"})

// By group or country
claimed ::rl/group-victims("lockbit3")
local ::rl/country-victims("LU")

// Groups
::rl/group("lockbit3").description
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::ransomware-live` | Victims: `recent-victims`, `group-victims`, `country-victims`, `search-victims`. Groups: `groups`, `group`. Key: `validate-key`. Helpers: `normalize-victim`, `normalize-group`, `rows`, `country-code`, `lookup-result` |
| `::ransomware-live::api` | `resolve`, `require-api-key`, `request`, `get-json`, `query-string`, `path-segment`, `http-error`, `is-not-found`; type `HttpError` |

Victim calls return records with the same field names on every endpoint and host: `victim`, `group`, `attackdate`, `discovered`, `country`, `activity`, `domain`, `claim_url` and `url`, plus whatever else the host sends. `attackdate` is the estimated attack or publication date, and `discovered` is when ransomware.live first saw the listing.

`groups` differs by host: v2 gives full records, and PRO gives `name`, `altname` and a `victims` count, with the full record from `group`.

## Pagination

Neither host paginates. `recent-victims` returns the latest 100, and the other calls return every match in one response. The largest v2 response seen was about 800 KB, well under `http.max-response-bytes`.

## Errors

Non-2xx responses come back as an Err carrying `::ransomware-live::api/HttpError`:

- `source`: `"ransomware-live"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` on 429s
- `message`: a one-line summary built from the API's own error text, never a credential

The v2 API answers 404 when nothing matches, so on v2 `group-victims`, `country-victims` and `search-victims` return `[]` and `group` returns null. Any other 404 is an Err: from `recent-victims` or `groups`, where it means a wrong URL or a moved endpoint, and from every PRO call, because what a PRO 404 means isn't confirmed. A list response in an unexpected shape is an Err, never an empty list. Nothing retries inside a run. Use a handler or schedule `retry` policy for 429s and 5xx.

## Notes and limits

- **Free v2 API** ([comparison](https://www.ransomware.live/api)): no key, one request per minute per endpoint, no new features, and victims are delayed compared with PRO. The T&C describe it as legacy, kept online for compatibility.
- **PRO API** ([spec](https://api-pro.ransomware.live/swagger.json)): the spec states 60 requests per minute and 500,000 per month per key, with 429s and `X-RateLimit-*` and `Retry-After` headers. The comparison page says "no rate limit, but fair use applies" and gives the same monthly figure. Plan for the stricter one.
- **Polling**: data is cached for 30 minutes. The T&C say polling faster brings nothing new and may breach fair usage, so schedule at 30 minutes or slower.
- **Fair usage** (T&C section 4, applies to all use): don't relay your customers' queries to ransomware.live, because your users must query your own system. Don't re-serve the data through your own API or feed, publish raw data in any form, clone the site, or use several accounts or keys to get around quotas.
- **Attribution** (T&C section 6): name ransomware.live as the source, with a link, wherever the data is displayed and in any report. The recommended citation is "Source: Ransomware.live".
- **No guarantees**: an independent personal project with no SLA, which may change or stop without notice. Tokens can be revoked without notice for breaches. French law governs the terms, and users indemnify the owner.
- **Leak links**: `claim_url` and a group's `locations` point at criminal leak sites, many on Tor, and `screenshot` shows a leak page. This package only reads the metadata API and never fetches them. Don't fetch them in your own code or show them as clickable links.
- **Corrections**: entries can be disputed through the site's [contact page](https://ransomware.live/contact). Removal is at the owner's discretion, so a claim may be taken down or stay after a dispute.
- **PRO is not live-tested here**: its response envelope isn't in the spec. The package accepts a bare list or a list under `victims` or `groups`, and returns an Err for anything else.
- Other PRO data (IoCs, negotiations, ransom notes, YARA rules, press) isn't wrapped.

## License

Apache-2.0 - see [LICENSE](LICENSE)
