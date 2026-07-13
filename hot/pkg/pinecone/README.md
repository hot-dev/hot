# pinecone

Pinecone vector database bindings for Hot: serverless index management plus upsert/query/fetch/delete — production RAG beyond Hot-store prototypes. Pair with `openai` / `openai-compatible` embeddings.

## Setup

Context variable `pinecone.api.key`.

## Usage

```hot
::indexes ::pinecone::indexes
::vectors ::pinecone::vectors

index ::indexes/create-index("docs", 1536, {metric: "cosine"})

::vectors/upsert(index.host, [
  {id: "doc-1", values: embedding, metadata: {source: "guide.md"}}
])

hits ::vectors/query(index.host, {
  vector: question-embedding,
  topK: 5,
  includeMetadata: true
})
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
