# web-search

Web search for Hot agents and apps. **Tavily**, **Brave**, **Exa**, and **Firecrawl** sit behind one normalized `search()`. You can pick a provider, fall back to the next on failure, and filter by freshness and domain, and each provider's native API is still there when you need more.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/web-search": "0.1.0"
```

## Setup

Set a key for any provider you have. `::web-search/search` uses the first configured one (in the order below) unless you choose another.

| Context Variable | Provider |
|---|---|
| `tavily.api.key` | [Tavily](https://tavily.com) (`tvly-...`), search built for LLM agents, with answers and extract |
| `brave.api.key` | [Brave Search API](https://api-dashboard.search.brave.com) subscription token |
| `exa.api.key` | [Exa](https://exa.ai), neural search with page contents and find-similar |
| `firecrawl.api.key` | [Firecrawl](https://firecrawl.dev) search (`fc-...`), the same key the `firecrawl` package uses |
| `web-search.provider` | Optional default provider: `tavily`, `brave`, `exa`, or `firecrawl` |

Keys can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
// Normalized: Vec<{title, url, content, score}>, best first
results ::web-search/search("PostgreSQL 18 release notes", 5)
map(results, (r) { `${r.title} - ${r.url}` })

// Choose the provider, fall back to the others on failure, and filter
recent ::web-search/search("hot.dev release notes", 5, {
  provider: "brave",
  fallback: true,
  freshness: "week",
  include-domains: ["hot.dev"],
  exclude-domains: ["example.com"]
})
```

## Normalized search

`search(query)`, `search(query, max-results)`, `search(query, options)`, and `search(query, max-results, options)` return `Vec<::web-search/SearchResult>`:

| Field | Meaning |
|---|---|
| `title` | Result title (HTML stripped) |
| `url` | Result URL |
| `content` | Plain-text snippet: Tavily's content, Brave's description (markup stripped, entities decoded), Exa's page text (up to 1,000 characters), or Firecrawl's description |
| `score` | Relevance, 0-1, when the provider reports one (Tavily). Brave and Firecrawl have none, and current Exa responses usually have none |

| Option | Meaning |
|---|---|
| `provider` | `"tavily"`, `"brave"`, `"exa"`, or `"firecrawl"`. Defaults to `web-search.provider`, then the first configured provider |
| `fallback` | `true` to try the other configured providers, in order, when one fails |
| `freshness` | `"day"`, `"week"`, `"month"`, or `"year"` |
| `include-domains` / `exclude-domains` | Vecs of hostnames |
| `max-results` | Used by `search(query, options)`. Default 10 |
| `tavily-api-key`, `brave-api-key`, `exa-api-key`, `firecrawl-api-key` | The provider's key for this call. See [Per-call credentials](#per-call-credentials) |

How each option maps to the providers:

| | Tavily | Brave | Exa | Firecrawl |
|---|---|---|---|---|
| max results (clamped) | `max_results` 1-20 | `count` 1-20 | `numResults` 1-100 | `limit` 1-100 |
| `freshness` | `time_range` | `freshness` `pd`/`pw`/`pm`/`py` | `startPublishedDate` (now minus 1/7/31/365 days) | `tbs` `qdr:d`/`w`/`m`/`y` |
| domains | `include_domains` / `exclude_domains` | `site:` operators in the query (`OR` between includes, `NOT site:` per exclude) | `includeDomains` / `excludeDomains` | `includeDomains` / `excludeDomains` |

Errors come back as an Err. With no provider configured, or with the chosen provider unconfigured or unknown, the Err carries a message naming what to set. A failed provider call carries `::web-search::http/HttpError` (`provider`, `status`, a readable `message`, `body`). When `fallback` runs out of providers, the Err carries `{message, errors: [{provider, error}]}`.

## Provider APIs

The native functions take each provider's own option names and return its raw response:

```hot
out ::web-search::tavily/search("qdrant vs pinecone", {include_answer: true, max_results: 5})
out.answer

pages ::web-search::tavily/extract(["https://example.com"], {format: "markdown"})
brave ::web-search::brave/search("rust jit compiler", {count: 5, freshness: "pm"})
exa ::web-search::exa/search("startups building sql runtimes", {numResults: 5, contents: {text: {maxCharacters: 2000}}})
similar ::web-search::exa/find-similar("https://hot.dev", {numResults: 5})
text ::web-search::exa/get-contents(["https://hot.dev"])
fc ::web-search::firecrawl/search("firecrawl changelog", {limit: 3})
```

| Module | Functions |
|---|---|
| `::web-search::tavily` | `search(query, options?)` (`POST /search`), `extract(urls, options?)` (`POST /extract`, up to 20 URLs) |
| `::web-search::brave` | `search(query, options?)` (`GET /web/search`) |
| `::web-search::exa` | `search(query, options?)`, `find-similar(url, options?)`, `get-contents(urls, options?)` |
| `::web-search::firecrawl` | `search(query, options?)` (`POST /v2/search`). For scraping and crawling, use the `firecrawl` package |

## Per-call credentials

Pass a provider's key in the options of `search`, so one run can act for many tenants and parallel flows don't share credentials. Credentials in the options are masked in call records, and nothing from them is written to ctx.

| Key | Provider |
|---|---|
| `tavily-api-key` | Tavily, instead of `tavily.api.key` |
| `brave-api-key` | Brave, instead of `brave.api.key` |
| `exa-api-key` | Exa, instead of `exa.api.key` |
| `firecrawl-api-key` | Firecrawl, instead of `firecrawl.api.key` |

```hot
results ::web-search/search("hot.dev release notes", 5, {
  provider: "brave",
  fallback: true,
  brave-api-key: "tenant-b-token",
  tavily-api-key: "tvly-tenant-b"
})
```

With any of these keys in the call, only those providers are candidates. Context keys are not used for that call: `fallback` tries only the providers the call passed a key for, `web-search.provider` applies only when it is one of them, and a `provider` without a key in the call is an Err. So a tenant's search never runs on the project's account.

The native provider functions take the same keys in a trailing `opts` Map. `opts` comes last; where a function already takes `options`, pass `{}` for those.

```hot
tenant {tavily-api-key: "tvly-tenant-b"}
out ::web-search::tavily/search("qdrant vs pinecone", {max_results: 5}, tenant)
```

A key that is present but blank or null is an error naming the key, never a fallback to context. `::web-search::api/resolve(opts)` is the one place this is decided.

## Notes and limits

- **Brave domain filters use search operators.** Brave documents its operators as experimental, so `site:`/`NOT site:` filtering is best-effort. Brave also caps `offset` at 9 pages of 20.
- **Exa content costs.** The normalized search asks Exa for page text capped at 1,000 characters per result, which Exa bills as contents. Call `::web-search::exa/search` directly to choose highlights or summaries instead.
- **Fallback is opt-in.** Without `fallback: true`, a provider failure is returned as is, so you never silently get results from a provider you did not choose.
- **Free tiers.** Tavily, Exa, and Firecrawl have free tiers or starter credits. Brave replaced its free plan in 2026 with a monthly credit that needs a card on file. None has a sandbox, so the integration test makes one small live search per configured provider.

## License

Apache-2.0 - see [LICENSE](LICENSE)
