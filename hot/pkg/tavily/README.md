# tavily

[Tavily](https://tavily.com) bindings for Hot: web search built for LLM agents, with generated answers, news and finance topics, date and domain filters, and page content extraction.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/tavily": "0.1.1"
```

## Setup

| Context Variable | Description |
|---|---|
| `tavily.api.key` | API key from the [Tavily dashboard](https://app.tavily.com) (`tvly-...`), sent as `Authorization: Bearer` |

The API key can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
// Search, with an LLM-written answer
out ::tavily/search("qdrant vs pinecone pricing", {include_answer: true, max_results: 5})
out.answer
map(out.results, (r) { `${r.title} - ${r.url}` })

// Recent results from chosen sites
recent ::tavily/search("hot.dev release notes", {
  time_range: "week",
  include_domains: ["hot.dev"],
  exclude_domains: ["example.com"]
})

// Clean page content
pages ::tavily/extract(["https://example.com"], {format: "markdown"})
map(pages.results, (page) { page.raw_content })
```

Options are passed through as Tavily documents them (snake_case: `max_results`, `include_answer`, `search_depth`, ...). Unset (null) options are dropped rather than sent.

## Functions

| Function | Endpoint | Returns |
|---|---|---|
| `search(query)`, `search(query, options)` | `POST /search` | `{query, answer?, results: [{title, url, content, score}], ...}` |
| `extract(urls)`, `extract(urls, options)` | `POST /extract`, up to 20 URLs | `{results: [{url, raw_content}], failed_results: [{url, error}]}` |

`search` options:

| Option | Meaning |
|---|---|
| `search_depth` | `"basic"`, `"advanced"`, `"fast"`, or `"ultra-fast"` |
| `topic` | `"general"`, `"news"`, or `"finance"` |
| `max_results` | 0-20 |
| `time_range` | `"day"`, `"week"`, `"month"`, or `"year"` |
| `start_date` / `end_date` | `YYYY-MM-DD` |
| `include_domains` / `exclude_domains` | Vecs of hostnames |
| `include_answer` | `true` for an LLM-written answer in `.answer` |
| `include_raw_content` | `"markdown"` or `"text"` |
| `country`, `include_usage` | As Tavily documents them |

`extract` options: `extract_depth` (`"basic"`/`"advanced"`), `format` (`"markdown"`/`"text"`), `query` (rerank chunks by relevance), `include_images`, `timeout` (seconds).

Errors come back as an Err. With no key set, the Err names what to set. A failed call carries `::tavily::http/HttpError` (`status`, a readable `message`, `headers`, `body`).

## Per-call credentials

Every function that calls Tavily takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `tavily.api.key` |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "tvly-tenant-b"}
out ::tavily/search("qdrant vs pinecone", {max_results: 5}, tenant)
pages ::tavily/extract(["https://example.com"], {}, tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `tavily.api.key`. `::tavily::api/resolve(opts)` is the one place this is decided.

## Notes and limits

- `extract` can partly succeed: check both `results` and `failed_results`.
- Tavily has a free tier but no sandbox, so the integration test makes one small live search.

## License

Apache-2.0 - see [LICENSE](LICENSE)
