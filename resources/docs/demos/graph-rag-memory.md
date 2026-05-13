# Graph-RAG Memory Demo

This demo focuses on the memory layer that sits behind every Hot agent: raw
records, compacted capsules, graph nodes/edges, hybrid retrieval, and
provenance-preserving citations. There are no transport adapters here — it's a
pure walkthrough of how memory accumulates and is queried.

**Expected time:** ~20 minutes. **Cost:** none — works with or without an
embedding provider.

## What You'll Build

A small in-process knowledge graph:

- raw session records from a synthetic team conversation,
- one compacted capsule that summarizes the window,
- five entity nodes and four `blocked-by` / `owns` edges,
- a hybrid query that returns vector hits *and* graph expansion as
  citations.

## Prerequisites

Install the Hot CLI and check `hot --version` works. The graph helpers used
here haven't shipped in `hot-ai` yet, so keep the main `hot` repo as a sibling
of `hot-demos`:

```text
hot-dev/
  hot/
  hot-demos/
```

If you don't have an embedding provider configured locally, the demo falls
back to graph expansion so you can still see citations and edges.

## Step 1: Clone And Configure

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/graph-rag-memory
cp .env.example .env
```

Override the package path if your checkout layout is different:

```bash
export HOT_AI_PATH=/path/to/hot/hot/pkg/hot-ai
```

## Step 2: Verify The Project

```bash
hot test
```

Three tests should pass: `test-seed-demo`, `test-hybrid-query-has-citations`,
and `test-neighbor-edges`. The query test is the one that exercises the
fallback path on machines without embeddings.

## Step 3: Start Hot Dev (Optional)

```bash
hot dev --open
```

This demo is function-driven, not webhook-driven — you'll exercise it from
`hot eval`. Keep Hot Dev running if you want to inspect the project source in
the App while you work.

## Step 4: Seed Memory

Run the seed function. The single-quoted argument is important — `::` and `/`
are Hot syntax, and your shell must pass them through unchanged.

```bash
hot eval '::graph-rag-memory::demo/seed-demo()'
```

Expected result:

```text
{records: 3, capsules: 1, nodes: 5, edges: 4}
```

That tells you 3 raw records were written, 1 capsule compacted from those
records, and the graph now holds 5 nodes (people, projects, tasks) connected
by 4 edges.

## Step 5: Run A Hybrid Query

```bash
hot eval '::graph-rag-memory::demo/query("What blocks TeamAgent launch readiness?")'
```

Expected result (counts will vary by embedding configuration):

```text
{
  vector-count: 0..3,
  edge-count: 4,
  citation-count: 4..7,
  citations: [
    {id: "edge:team-agent-docs", label: "TeamAgent blocked-by Docs walkthrough", excerpt: "…"},
    …
  ]
}
```

`vector-count` may be `0` if no embedding provider is configured — the demo
still returns graph edges and citations so the structure of the answer is
visible.

## Step 6: Trace The Code

Open `hot/src/graph-rag-memory/demo.hot` and follow the four functions you
just exercised:

- `seed-records` writes raw messages into session memory.
- `seed-capsule` compacts those records into a durable capsule with
  metadata.
- `seed-graph` extracts five entities and four edges, attaching provenance
  back to the source message.
- `query` runs hybrid recall and falls back to graph expansion if the
  embedding call fails.

The interesting bit is **provenance**: every node, edge, and citation knows
which raw record it came from, so the agent layer can cite memory back to
its source.

## Agent Graph Walkthrough

This demo is intentionally memory-centric, so it doesn't declare an `agent`
type or webhook. If you wrap these functions in an agent later:

- put `on-event` metadata on the ingest, compaction, extraction, and query
  handlers so they appear as graph nodes,
- prefer literal `send(...)` calls when you can — the compiler will pick
  them up automatically,
- declare any helper-hidden event names with `meta {sends: …}`.

## Source

The runnable project lives in
[hot-dev/hot-demos/graph-rag-memory](https://github.com/hot-dev/hot-demos/tree/main/graph-rag-memory).
