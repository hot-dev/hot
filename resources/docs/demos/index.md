# Demos

Hot demos are complete, runnable projects you can clone, edit, and deploy.
Each one teaches one end-to-end pattern. The pages here explain *what to
look for* and *how to read* what you see in the Hot App; the source
projects live in [hot-dev/hot-demos](https://github.com/hot-dev/hot-demos).

## Start Here: Hot Chat

**[Hot Chat](/docs/demos/hot-chat)** is the culmination of several Hot Dev
platform features that combine into a powerful solution for AI-driven
products — and the best first impression of building on Hot. One Hot
project boots two AI agents that the Next.js client switches between live:

- **Personal Mode** — identity-first memory. Notes follow the user across
  sessions and devices.
- **Team Mode** — session-first memory. Channel members share one memory;
  channels stay independent.

You get a polished chat UI, file attachments, streaming replies, and a
transparent identity panel — all over the same typed-event wire contract
a Slack or Telegram adapter would use. Each slash command is one
`on-event` handler in the agent, so the Agent Graph stays readable as
the agent grows.

Run it with two terminals:

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/hot-chat
hot dev --open                  # terminal 1 — both agents
cp .env.example .env && npm install && npm run dev   # terminal 2 — UI
```

## Identity Vocabulary

Both modes work with the same two ideas. Holding them apart makes the
demo easier to read.

| Concept    | Team Mode                       | Personal Mode                          |
|------------|---------------------------------|----------------------------------------|
| Session    | the channel or thread           | a scratch context per person           |
| Identity   | the person who posted a message | the durable memory owner               |
| Memory     | scoped to the session           | scoped to the user                     |

Slack and Telegram adapters fill these in from native IDs. Hot Chat uses
synthetic IDs from `localStorage` so you can run everything without an
account.

## More Recipes In The Repo

These projects live in
[`hot-dev/hot-demos`](https://github.com/hot-dev/hot-demos) as standalone
Hot projects with their own README. They're complete and runnable —
they're recipes rather than full walkthroughs, so they live in the
repo rather than in this docs site.

- **Slack Bot** — multi-provider AI bot (Claude / GPT / Grok / Gemini)
  with live `!ai` switching and dual-mode polling-in-dev /
  webhooks-in-prod.
  [README](https://github.com/hot-dev/hot-demos/tree/main/slack-bot) ·
  [Tutorial](https://hot.dev/blog/build-ai-slack-bot)
- **My News** — scheduled job that fetches AI news sites in parallel,
  summarizes with Claude, and emails via Resend. A good first taste of
  schedules and `send(...)` triggers.
  [README](https://github.com/hot-dev/hot-demos/tree/main/my-news)
- **Graph-RAG Memory** — the memory substrate behind the Hot Chat
  agents: raw records, capsules, graph nodes/edges, and hybrid retrieval
  with citations. Function-driven via `hot eval` rather than a full
  agent — useful for understanding how memory works under the hood.
  [README](https://github.com/hot-dev/hot-demos/tree/main/graph-rag-memory)

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

Demos use published Hot packages from the registry — clone, run
`hot test` or `hot dev`, and dependencies resolve automatically.

Some demos include `hot/ctx.hot` as a local-development convenience.
`hot dev` loads that file to bridge values from `.env` into context
variables, but Hot Dev Cloud ignores it. For deployed demos, add the
same context variables in the Hot Dev App.

## What To Look For In The Hot App

Most demos include an Agent Graph walkthrough. Open the Hot App, choose
the demo agent, and inspect:

- webhook, schedule, and MCP nodes that trigger handlers
- event nodes created from `on-event` handlers
- outgoing `sends` edges from literal `send(...)` calls or explicit
  `meta {sends: …}` declarations
- loops that go from incoming request → event handler → memory write →
  reply

If you only see a single trigger and one handler, that demo is
intentionally small — the docs flag where to grow it next.
