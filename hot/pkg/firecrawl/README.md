# firecrawl

Firecrawl bindings: `scrape` a URL to clean markdown, `map-site` to discover URLs, `crawl`/`get-crawl` whole sites, and `extract` structured data with a prompt or schema. The read-the-web half of `web-search`. Context variable: `firecrawl.api.key`.

```hot
page ::firecrawl/scrape("https://hot.dev/docs", {formats: ["markdown"]})
page.markdown
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
