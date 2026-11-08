# pinecone

Pinecone vector database bindings for Hot, on API version `2026-07`. Covers serverless index management (create, configure, readiness checks, delete), vector upsert/query/fetch/update/list/delete with namespaces, namespace management, integrated-embedding records, and Pinecone's hosted `embed` and `rerank` inference. Use it for production RAG beyond Hot-store prototypes.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/pinecone": "0.1.1"
```

## Setup

| Context Variable | Description |
|---|---|
| `pinecone.api.key` | API key from the [Pinecone console](https://app.pinecone.io), sent as `Api-Key` |
| `pinecone.api.version` | Optional. Overrides the `X-Pinecone-Api-Version` header (default `2026-07`, `::pinecone/API_VERSION`) |

Both can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::indexes ::pinecone::indexes
::vectors ::pinecone::vectors

// Once, at setup. A new index takes a few seconds to come up, so don't
// write to it in the same step.
::indexes/create-index("docs", 3, {metric: "cosine", tags: {team: "search"}})

// In a later step or a scheduled run: confirm it is ready, then use its host
index ::indexes/describe-index("docs")
if(not(::indexes/is-ready(index)), fail("index docs is not ready yet"))

::vectors/upsert(index.host, [
  {id: "doc-1#chunk-1", values: [0.1, 0.2, 0.3], metadata: {source: "guide.md"}},
  {id: "doc-1#chunk-2", values: [0.3, 0.2, 0.1], metadata: {source: "guide.md"}}
], "guides")

hits ::vectors/query(index.host, {
  vector: [0.1, 0.2, 0.3],
  topK: 5,
  namespace: "guides",
  includeMetadata: true,
  filter: {source: {"$eq": "guide.md"}}
})
map(hits.matches, (m) { `${m.id} ${m.score}` })

// Read back, change, list, and delete in the same namespace
::vectors/fetch(index.host, ["doc-1#chunk-1"], "guides")
::vectors/update(index.host, "doc-1#chunk-1", {setMetadata: {reviewed: true}}, "guides")
ids ::vectors/list-all-ids(index.host, {prefix: "doc-1#", namespace: "guides"})
::vectors/delete-vectors(index.host, {ids: ids, namespace: "guides"})

::indexes/delete-index("docs")
```

Writes are eventually consistent: a vector you just upserted can take a moment to show up in `query` and `list-ids`.

## Functions

**`::pinecone::indexes`** (control plane, api.pinecone.io)

| Function | Endpoint |
|---|---|
| `list-indexes()` | `GET /indexes` |
| `describe-index(name)` | `GET /indexes/{name}`: `host`, `status`, `schema`, `deployment`, `tags`, `deletion_protection` |
| `create-index(name, dimension, options?)` | `POST /indexes`, a dense vector index. Options: `metric`, `cloud`, `region`, `deletion_protection`, `tags`, `read_capacity` |
| `create-index-from-schema(name, schema, options)` | `POST /indexes` with an explicit schema (sparse, hybrid, or document indexes) |
| `create-index-for-model(name, model, options?)` | `POST /indexes/create-for-model`, an integrated-embedding index for `::pinecone::records` |
| `configure-index(name, options)` | `PATCH /indexes/{name}`: `deletion_protection`, `tags`, `read_capacity`, pod `deployment` |
| `delete-index(name)` | `DELETE /indexes/{name}` |
| `is-ready(index)`, `ready-check(index)` | Read a description's `status`: `is-ready` is true once `status.ready`; `ready-check` returns `"ready"`, `"pending"`, or `"failed"` |

**`::pinecone::vectors`** (data plane, on the index `host`)

| Function | Endpoint |
|---|---|
| `upsert(host, vectors, namespace?)` | `POST /vectors/upsert`, up to 1000 vectors |
| `query(host, options)` | `POST /query`: `vector`/`id`/`sparseVector`, `topK`, `namespace`, `filter`, `includeMetadata`, `includeValues` |
| `fetch(host, ids, namespace?)` | `GET /vectors/fetch` |
| `fetch-by-metadata(host, filter, options)` | `POST /vectors/fetch_by_metadata` |
| `update(host, id, changes, namespace?)` | `POST /vectors/update`: `values`, `sparseValues`, `setMetadata` |
| `update-by-filter(host, filter, set-metadata, options)` | `POST /vectors/update` by metadata filter (`namespace`, `dryRun`) |
| `list-ids(host, options?)` | `GET /vectors/list`, one page: `prefix`, `limit`, `paginationToken`, `namespace` |
| `list-all-ids(host, options?)` | Every matching ID across pages, as a Vec |
| `delete-vectors(host, options)` | `POST /vectors/delete`: `ids`, `filter`, or `deleteAll`, plus `namespace` |
| `describe-stats(host, filter?)` | `POST /describe_index_stats` |

**`::pinecone::namespaces`**: `list-namespaces(host, options?)`, `describe-namespace(host, namespace)`, `create-namespace(host, name, schema?)`, and `delete-namespace(host, namespace)`.

**`::pinecone::records`** (integrated embedding): `upsert-records(host, namespace, records)` and `search-records(host, namespace, text, options?)`.

```hot
::indexes ::pinecone::indexes
::records ::pinecone::records

// At setup:
::indexes/create-index-for-model("notes", "llama-text-embed-v2", {field_map: {text: "chunk_text"}})

// Later, once describe-index reports it ready:
index ::indexes/describe-index("notes")

::records/upsert-records(index.host, "notes", [
  {_id: "n1", chunk_text: "Hot runs workflows as code.", category: "docs"},
  {_id: "n2", chunk_text: "Pinecone stores the vectors.", category: "docs"}
])

out ::records/search-records(index.host, "notes", "what runs workflows?", {
  top_k: 5,
  fields: ["chunk_text"],
  rerank: {model: "bge-reranker-v2-m3", rank_fields: ["chunk_text"], top_n: 2}
})
map(out.result.hits, (h) { h.fields.chunk_text })
```

**`::pinecone::inference`** (no index needed)

```hot
::inference ::pinecone::inference

vectors ::inference/embed("llama-text-embed-v2", ["Hot runs workflows as code."], {input_type: "passage"})
first(vectors.data).values

ranked ::inference/rerank("bge-reranker-v2-m3", "What is Hot?", [
  "Hot is a workflow language.",
  "Pinecone is a vector database."
], {top_n: 1})
first(ranked.data).index
```

`::pinecone::api/json(method, url, body)` reaches anything else, such as the Documents API, backups, or imports.

## Errors

Non-2xx responses return an Err carrying `::pinecone::api/HttpError`: `status`, a readable `message` (for example `Pinecone API error 400 (INVALID_ARGUMENT): ...`), `headers`, and the raw `body`.

## Per-call credentials

Every function that calls Pinecone takes an optional trailing `opts` Map, so one run can act for many projects and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `pinecone.api.key` |
| `api-version` | The `X-Pinecone-Api-Version` for this call, instead of `pinecone.api.version` |

`opts` always comes last. Where a function already takes `options`, a namespace, or `parameters`, pass `{}` or `null` for those.

```hot
::indexes ::pinecone::indexes
::vectors ::pinecone::vectors

tenant {api-key: "tenant-b-key"}
index ::indexes/describe-index("docs", tenant)
::vectors/upsert(index.host, [{id: "doc-1", values: [0.1, 0.2, 0.3]}], "guides", tenant)
hits ::vectors/query(index.host, {vector: [0.1, 0.2, 0.3], topK: 5}, tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `pinecone.api.key`. `::pinecone::api/resolve(opts)` is the one place this is decided.

An index `host` is a per-call URL, so `pinecone.api.key` only goes to `https://*.pinecone.io` hosts, which is what `describe-index` returns. Any other host, such as Pinecone Local at `http://localhost:5080`, needs an `api-key` in the call's `opts`; without one the call is an Err before anything is sent.

## Notes and limits

- **API version.** Requests send `X-Pinecone-Api-Version: 2026-07`, the latest stable version as of November 2026. Pinecone supports each stable version for at least 12 months. On `2026-07`, `POST /indexes` is schema-based: `create-index` sends the reserved `_values` field, which still creates a classic vector index. `describe-index` reports its dimension and metric under `schema.fields._values` rather than at the top level. To use the older body and response shape, set `pinecone.api.version` to `2026-04`. Indexes created before `2026-07` keep working with every function here.
- **Namespaces.** Without a namespace argument, calls use the default namespace (`__default__`, `::pinecone/DEFAULT_NAMESPACE`). Pass the same namespace to `fetch`, `update`, `list-ids`, and `delete-vectors` that you upserted with.
- **New indexes are not ready at once.** `create-index` returns before the index can take data, usually for a few seconds. Nothing here blocks waiting for it. Create the index in a setup step, then call `describe-index` from a later step or a schedule and upsert once `is-ready` is true. `ready-check` returning `"failed"` means the index will not come up.
- **Deletion protection.** `delete-index` fails while `deletion_protection` is `"enabled"`. Run `configure-index(name, {deletion_protection: "disabled"})` first.
- **Starter plan.** Pinecone's free Starter plan covers serverless indexes (on AWS `us-east-1`, the default here), inference, and records. It has monthly read, write, and egress allowances, and `describe-stats` rejects a metadata filter on serverless indexes. There is no separate sandbox.
- **Records.** `upsert-records` takes at most 96 records per call. Pinecone calls the Records API legacy: for new full-text or integrated-embedding work, consider a document index (`create-index-from-schema` with `string` fields) and the Documents API.

## License

Apache-2.0 - see [LICENSE](LICENSE)
