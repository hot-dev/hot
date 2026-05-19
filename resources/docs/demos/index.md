# Demos

Hot demos are complete, runnable projects you can clone, edit, and deploy. Each
one teaches one end-to-end pattern. The pages here explain *what to look for*
and *how to read* what you see in the Hot App; the source projects live in
[hot-dev/hot-demos](https://github.com/hot-dev/hot-demos).

## Available Demos

- **[TeamAgent](/docs/demos/team-agent)** — a multi-transport team memory agent.
  Records channel messages, answers from remembered context, and shows how a
  webhook fans out to event handlers in the Agent Graph.
- **[PersonalAgent](/docs/demos/personal-agent)** — an identity-first memory
  agent. Stores, recalls, and exports per-person memory.
- **[Graph-RAG Memory](/docs/demos/graph-rag-memory)** — the memory substrate
  behind the agent demos: raw records, capsules, graph nodes/edges, and hybrid
  retrieval with citations.
- **[Hot Chat](/docs/demos/hot-chat)** — a polished local web chat UI that talks
  to TeamAgent or PersonalAgent through the same webhook contract used by
  Telegram and Slack adapters.

## Identity Vocabulary

Every Hot agent works with the same two ideas. Holding them apart makes the
demos easier to read.

| Concept    | TeamAgent                       | PersonalAgent                          |
|------------|---------------------------------|----------------------------------------|
| Session    | the channel or thread           | a scratch context per person           |
| Identity   | the person who posted a message | the durable memory owner               |
| Memory     | scoped to the session           | scoped to the user                     |

Slack and Telegram adapters fill these in from native IDs. The web demos use
synthetic IDs so you can run everything without an account.

## How Demos Are Organized

Each demo is a standalone Hot project:

```text
demo-name/
  hot.hot
  README.md
  .env.example
  hot/
    src/
    test/
```

You can run one demo without cloning the main Hot repository:

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/team-agent
hot dev --open
```

## Package Dependencies

Demos prefer published Hot packages so they work for new users with a standard
Hot installation. When a demo depends on an unreleased package, its README
shows a local-path override so contributors can run from a sibling Hot
checkout.

## What To Look For In The Hot App

Most demos include an Agent Graph walkthrough. Open the Hot App, choose the
demo agent, and inspect:

- webhook, schedule, and MCP nodes that trigger handlers
- event nodes created from `on-event` handlers
- outgoing `sends` edges from literal `send(...)` calls or explicit
  `meta {sends: …}` declarations
- loops that go from incoming request → event handler → memory write → reply

If you only see a webhook and a single handler, that demo is intentionally
small — the docs flag where to grow it next.
