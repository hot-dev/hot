---
description: "Run the Hot Chat demo with personal and team AI agents, slash commands, memory, streaming replies, and the Hot SDK."
---

# Hot Chat Demo

Hot Chat is a complete, runnable demo of two AI agents and a polished web UI
that drives them — all in one Hot project. It's the demo to point at when
someone asks *"what does a product on Hot look like?"*

- **Personal Mode** — identity-first memory. Notes follow the user across
  sessions, channels, and devices.
- **Team Mode** — session-first memory. Two people in the same chat share
  one memory; two channels stay independent.
- **One Next.js client** — a thin transport that publishes one typed event
  per message and renders the agent's reply over the run stream.

Both agents live under `hot/src/` in the same project and boot together with
one `hot dev`. The Next.js side is a thin transport — the agent is the
product.

**Expected time:** 15 minutes. **Cost:** none — the demo agents answer from
local memory by default. Set `ANTHROPIC_API_KEY` for live LLM replies.

## What You'll Get To See

- a clean chat UI that switches between Personal and Team modes live,
- quick-prompt chips that map to slash commands without baking policy into
  the UI,
- file attachments (drag-and-drop or paperclip) carried through to the agent
  as part of the same typed event,
- a transparent identity panel so you can read off the exact `session_id`
  and `user_id` the agent will see,
- per-command event handlers and streaming replies visible in the Agent
  Graph.

## Prerequisites

- **Hot CLI** 2.0.3+ — [hot.dev/download](https://hot.dev/download)
- **Node 20+** for the Next.js app
- A **Hot API key** for your local dev environment (one-time, see below)

No LLM API keys required — the demo agents answer from local memory.

The project's `hot.hot` declares published packages (`hot.dev/hot-ai`
**1.4.0**, `hot.dev/hot-ai-agent` **1.0.0**, `hot.dev/anthropic` **1.2.1**),
so dependencies resolve from the Hot package registry automatically.

## Step 1: Clone

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/hot-chat
```

## Step 2: Verify The Project

Compile and run the agent tests before booting the runtime:

```bash
hot test
```

You should see the tests pass for both Personal and Team agents. This
confirms the published deps resolve and both agents compile end to end.

## Step 3: Boot The Agents

```bash
hot dev --open
```

`hot dev` opens the Hot App at <http://localhost:4681> and registers both
agents under one project. Leave it running.

While the Hot App is open, generate an API key:

> *Hot App → API Keys → New Key.* Copy the value.

## Step 4: Start The Chat UI

In a second terminal:

```bash
cd hot-demos/hot-chat
cp .env.example .env
# paste the API key into HOT_API_KEY in .env
npm install
npm run dev
```

Open <http://localhost:3000>. The toolbar switches between Personal and Team
modes live.

## Step 5: Walk Through The Modes

### Personal Mode (identity-first)

Memory is keyed by **person** (`person:<user-id>`), so it follows the user
across sessions and devices.

1. Type `/remember I prefer launch updates that start with blockers` and
   press Enter. You'll see `remembered` stream into the assistant bubble.
2. Click **Recall preferences** (a quick-prompt chip) — the matching note
   comes back.
3. Refresh the browser and ask `/recall` again. Same answer — memory is
   keyed on you, not on the chat session.

Personal Mode commands, grouped by role:

| Role        | Command            | What it does                                              |
|-------------|--------------------|-----------------------------------------------------------|
| Write       | `/remember <text>` | store a personal note (free-chat does the same)           |
| Read        | `/recall <query>`  | search identity-scoped memory (deterministic, works offline) |
| Synthesis   | `/brief`           | preferences, open tasks, deadlines, projects              |
| Synthesis   | `/tasks`           | open tasks only, rendered as a checklist                  |
| Identity    | `/whoami`          | show transport, session, and user identity                |
| Help        | `/guide`           | cheat sheet of the available commands                     |

### Team Mode (session-first)

Memory is keyed by **session** (`web:chat:<id>`, `slack:T0:C0`,
`telegram:-100…`), so two channels stay independent while two members of the
same channel share one memory.

1. Switch to Team Mode in the toolbar.
2. Type *"we decided to ship docs before launch"*, then *"CI is the only
   blocker"*.
3. Ask `/ask what is blocking launch?` — the reply cites the matching
   records with attribution.

Team Mode commands, grouped by role:

| Role       | Command       | What it does                                                       |
|------------|---------------|--------------------------------------------------------------------|
| Write      | (no command)  | record the message into session memory                             |
| Read       | `/ask <q>`    | LLM-backed answer grounded on channel memory                       |
| Synthesis  | `/summary`    | distill the recent channel transcript                              |
| Synthesis  | `/decisions`  | decisions, action items, and open questions from the transcript    |
| Identity   | `/whoami`     | show transport, session, and user identity                         |
| Help       | `/guide`      | cheat sheet of the available commands                              |

## Step 6: Attach A File

Drag a small file (text, image, PDF — under 4 MB) anywhere onto the chat.
A chip appears below the composer. Send a message with it; the agent reply
will include `… with 1 attachment(s)`. The agent stores the file's name and
type as metadata; this demo doesn't deeply parse contents, but the same
wire shape is how a real product would forward documents to your agent.

## Step 7: Inspect Identity

Click **Identity** in the toolbar. You'll see the exact strings the agent
receives:

```text
Session       person:<your-uuid>      ← Personal Mode
              web:chat:<your-uuid>    ← Team Mode
User identity web:user:<your-uuid>
```

That one difference — Personal Mode derives `session_id` from the
identity, Team Mode trusts the caller's `session_id` — is the entire
identity-first / session-first split made literal. Edit your display name
and the agent picks it up on the next message. Identity is stored only in
your browser's `localStorage` — clear site data to reset.

## Step 8: Open The Agent Graph

In the Hot App, click into either agent and open the **Graph** tab. Each
slash command shows up as its own typed event wired to its own handler:

- `personal-agent:remember` → `remember`
- `personal-agent:recall` → `recall`
- `team-agent:ask` → `ask-question`
- `team-agent:record` → `record-message`
- …and so on, one node per command.

There is no central dispatch function and no big `cond`. Add a command by
writing one more `on-event` handler.

## Wire Contract

The browser parses slash commands client-side and POSTs a typed event to
the Next.js server route, which forwards it (with the API key) to Hot's
`/v1/streams/subscribe-with-event`:

```json
{
  "event_type": "team-agent:ask",
  "event_data": {
    "session_id":  "web:chat:<uuid>",
    "user_id":     "web:user:<uuid>",
    "user_name":   "Demo User",
    "message_id":  "web:<chat-id>:<timestamp>",
    "timestamp":   1700000000,
    "question":    "what's blocking launch?",
    "attachments": [{"name": "notes.md", "type": "text/markdown", "size": 412, "text": "…"}],
    "metadata":    {"client": "hot-chat", "target": "team-agent"}
  }
}
```

The matching `on-event` handler runs and emits
`team-agent:reply:start` / `:delta` / `:end` stream events. The browser
reads those and renders the assistant message as it arrives. A Slack or
Telegram adapter can publish the same events from native message shapes
— the wire contract is the contract.

## Project Layout

```text
hot-chat/
  src/                            # Next.js app
    app/api/chat/route.ts         # SSE proxy via @hot-dev/sdk/proxy
    lib/agent-client.ts           # demo command map + @hot-dev/sdk/agent
  hot.hot                         # one project, two agents
  hot/
    src/
      personal-agent.hot          # per-command event handlers
      team-agent.hot              # per-command event handlers
    test/
      personal-agent.hot
      team-agent.hot
```

Both agents are short, single-file projects. Diff them to see the *one*
structural difference: Personal Mode derives `session_id` from the
identity; Team Mode trusts the caller's `session_id`.

## Why This Architecture

- **Browser → server route → Hot stream.** Auth, CORS, and rate-limiting
  can live in the Next.js route later without touching the browser code.
- **No actions in the URL.** The UI passes free text and attachments; the
  agent decides what to do based on slash commands and typed events.
- **Stable IDs.** `chatId` and `userId` come from `localStorage`, so memory
  follows the user across page reloads. Production would replace these
  with your auth system's identifiers.
- **Per-command event handlers.** Each command is one `on-event` handler.
  The Agent Graph stays accurate as the agent grows.

## Build For Production

```bash
npm run build
npm start
```

The production build is what CI exercises. There's no agent-specific
config in the build — point `HOT_API_URL` and `HOT_API_KEY` at any
deployed Hot environment.

## Going Further

The standalone demo keeps a smaller command surface so the source stays
readable in one file each. The **full** TeamAgent and PersonalAgent in the
main Hot repo (`hot/hot/src/team-agent/`, `hot/hot/src/personal-agent/`)
add `/forget`, `/why`, `/export`, `/compact`, `/search`, `/stats`,
`/diag`, `/ai`, scheduled digests, a `Researcher` peer, and more —
production-shaped reference implementations for when you outgrow the demo.

## Source

The runnable project lives in
[hot-dev/hot-demos/hot-chat](https://github.com/hot-dev/hot-demos/tree/main/hot-chat).
