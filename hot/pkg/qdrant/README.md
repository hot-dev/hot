# qdrant

Qdrant vector database bindings — the self-hosted/OSS RAG store (cloud works identically). Collections plus points: `upsert-points`, `search` (with Qdrant's filter DSL), `get-points`, `delete-points`, `count-points`. Context variables: `qdrant.url` (default `http://localhost:6333`), optional `qdrant.api.key`.

```hot
::qdrant/create-collection("docs", 1536)
hits ::qdrant/search("docs", {vector: embedding, limit: 5, with_payload: true})
```

## Per-call credentials

Every function that calls Qdrant takes an optional trailing `opts` Map, so one run can use several instances. `url` and `api-key` replace `qdrant.url` and `qdrant.api.key` for that call only. Nothing is written to ctx, so parallel flows can't see each other's keys.

A per-call `url` never borrows the context key, which belongs to the context instance: `{url: ...}` alone sends no auth header. Where a function has an optional argument, `opts` comes after it, so `count-points` without a filter is `count-points(name, {}, opts)`.

```hot
::qd ::qdrant

search-for-tenant fn (tenant: Map, embedding: Vec): Vec {
  ::qd/search("docs", {vector: embedding, limit: 5, with_payload: true}, {
    url: tenant.qdrant-url,
    api-key: tenant.qdrant-key
  })
}
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
