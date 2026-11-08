# brave-search

[Brave Search API](https://api-dashboard.search.brave.com) bindings for Hot: web search from Brave's independent index, with country, language, safesearch, and freshness filters, and site operators in the query.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/brave-search": "0.1.1"
```

## Setup

| Context Variable | Description |
|---|---|
| `brave.api.key` | Subscription token from the [Brave Search API dashboard](https://api-dashboard.search.brave.com), sent as `X-Subscription-Token` |

The API key can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::bs ::brave::search

out ::bs/search("rust jit compiler", {count: 5, freshness: "pm"})
map(out.web.results, (r) { `${r.title} - ${r.url}` })

// Restrict sites with operators in the query
docs ::bs/search("release notes site:hot.dev NOT site:example.com", {count: 5})
```

Options are passed through as Brave documents them (snake_case: `search_lang`, `extra_snippets`, ...). Unset (null) options are dropped rather than sent.

## Functions

| Function | Endpoint | Returns |
|---|---|---|
| `search(query)`, `search(query, options)` | `GET /web/search` | `{web: {results: [{title, url, description}]}, ...}` |

`search` options:

| Option | Meaning |
|---|---|
| `count` | Results per request, max 20 |
| `offset` | Page index, max 9 |
| `country`, `search_lang` | As Brave documents them |
| `safesearch` | `"off"`, `"moderate"`, or `"strict"` |
| `freshness` | `"pd"`, `"pw"`, `"pm"`, `"py"`, or a range like `"2026-01-01to2026-06-30"` |
| `extra_snippets` | `true` for more snippets per result |

Errors come back as an Err. With no key set, the Err names what to set. A failed call carries `::brave::search::http/HttpError` (`status`, a readable `message`, `headers`, `body`).

## Per-call credentials

`search` takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The subscription token for this call, instead of `brave.api.key` |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "tenant-b-token"}
out ::brave::search/search("rust jit compiler", {count: 5}, tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `brave.api.key`. `::brave::search::api/resolve(opts)` is the one place this is decided.

## Notes and limits

- **Snippets contain markup.** Titles and descriptions come back as Brave sends them, with matched terms in `<strong>` and HTML entities such as `&amp;`.
- **Site operators are best-effort.** Brave documents its search operators as experimental, so `site:`/`NOT site:` filtering may not be exact.
- `offset` stops at 9 pages of 20.
- Brave replaced its free plan in 2026 with a monthly credit that needs a card on file. There is no sandbox, so the integration test makes one small live search.

## License

Apache-2.0 - see [LICENSE](LICENSE)
