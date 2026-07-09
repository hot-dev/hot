# web-search

Web search for Hot agents and apps: **Tavily**, **Brave**, and **Exa** behind one normalized `search()`, plus each provider's full native API. The missing "eyes" for `hot-ai-agent`.

## Setup

Set whichever key you have — `::web-search/search` picks it up automatically:

| Context Variable | Provider |
|---|---|
| `tavily.api.key` | Tavily (built for LLM agents; search + extract + answers) |
| `brave.api.key` | Brave Search |
| `exa.api.key` | Exa (neural/semantic search + contents + find-similar) |

## Usage

```hot
// Normalized: Vec<{title, url, content, score}>, best first
results ::web-search/search("PostgreSQL 18 release notes", 5)

// Provider-native surfaces
out ::web-search::tavily/search("qdrant vs pinecone", {include_answer: true})
out.answer

pages ::web-search::tavily/extract(["https://example.com/post"])
similar ::web-search::exa/find-similar("https://hot.dev")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
