# exa

[Exa](https://exa.ai) bindings for Hot: neural web search with inline page contents, find-similar pages, and clean page text for URLs.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/exa": "0.1.1"
```

## Setup

| Context Variable | Description |
|---|---|
| `exa.api.key` | API key from the [Exa dashboard](https://dashboard.exa.ai), sent as `x-api-key` |

The API key can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
// Search, with page text inline
out ::exa/search("startups building sql runtimes", {
  numResults: 5,
  contents: {text: {maxCharacters: 2000}}
})
map(out.results, (r) { `${r.title} - ${r.url}` })

// Pages like this one
similar ::exa/find-similar("https://hot.dev", {numResults: 5})

// Clean text for known URLs
pages ::exa/get-contents(["https://hot.dev"])
map(pages.results, (page) { page.text })
```

Options are passed through as Exa documents them (camelCase: `numResults`, `includeDomains`, `startPublishedDate`, ...). Unset (null) options are dropped rather than sent.

## Functions

| Function | Endpoint | Returns |
|---|---|---|
| `search(query)`, `search(query, options)` | `POST /search` | `{results: [{title, url, text?, highlights?, summary?}], ...}` |
| `find-similar(url)`, `find-similar(url, options)` | `POST /findSimilar` | `{results: [{title, url, ...}], ...}` |
| `get-contents(urls)`, `get-contents(urls, options)` | `POST /contents` | `{results: [{url, title, text}], statuses}` |

`search` options:

| Option | Meaning |
|---|---|
| `type` | `"auto"` (default), `"lite"`, `"deep-lite"`, or `"deep"` |
| `numResults` | 1-100 |
| `category` | `"company"`, `"news"`, `"people"`, ... |
| `includeDomains` / `excludeDomains` | Vecs of hostnames |
| `startPublishedDate` / `endPublishedDate` | ISO 8601 |
| `contents` | Page content inline: `{text: {maxCharacters: 2000}}`, `{highlights: true}`, or `{summary: {}}` |

`find-similar` takes `numResults`, `includeDomains`/`excludeDomains`, `excludeSourceDomain`, `startPublishedDate`, and `contents`. `get-contents` takes URLs or Exa result ids; with no options it asks for `{text: true}`, and with options it sends what you pass, such as `{text: {maxCharacters: 5000}, highlights: true, maxAgeHours: 0}`.

Errors come back as an Err. With no key set, the Err names what to set. A failed call carries `::exa::http/HttpError` (`status`, a readable `message`, `headers`, `body`).

## Per-call credentials

Every function that calls Exa takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `exa.api.key` |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "exa-tenant-b"}
out ::exa/search("startups building sql runtimes", {numResults: 5}, tenant)
similar ::exa/find-similar("https://hot.dev", {}, tenant)
pages ::exa/get-contents(["https://hot.dev"], {text: true}, tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `exa.api.key`. `::exa::api/resolve(opts)` is the one place this is decided.

## Notes and limits

- **Content costs.** Exa bills `contents` (text, highlights, summaries) on top of the search, so ask only for what you use and cap text with `maxCharacters`.
- Exa has a free tier but no sandbox, so the integration test makes one small live search.

## License

Apache-2.0 - see [LICENSE](LICENSE)
