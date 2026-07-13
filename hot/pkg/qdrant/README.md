# qdrant

Qdrant vector database bindings — the self-hosted/OSS RAG store (cloud works identically). Collections plus points: `upsert-points`, `search` (with Qdrant's filter DSL), `get-points`, `delete-points`, `count-points`. Context variables: `qdrant.url` (default `http://localhost:6333`), optional `qdrant.api.key`.

```hot
::qdrant/create-collection("docs", 1536)
hits ::qdrant/search("docs", {vector: embedding, limit: 5, with_payload: true})
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
