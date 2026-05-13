# TeamAgent Demo

TeamAgent gives a chat — a Telegram group, a Slack channel, or a web app like
[Hot Chat](/docs/demos/hot-chat) — a searchable team memory. This standalone
demo focuses on the web transport so you can run it without setting up
Telegram or Slack.

**Expected time:** 20–30 minutes. **Cost:** none — the demo never calls an LLM.

## What You'll Build

A small team-memory agent that:

- normalizes incoming chat messages from any transport into a typed event,
- records every regular message into session-scoped memory,
- answers `/ask` from remembered context,
- shows up as a connected graph in the Hot App.

You'll also see how a single agent can serve multiple chats (Slack + Telegram +
web) without code changes — the trick is keeping `Session` and `Identity`
separate, so memory stays per-channel while attribution stays per-person.

## Prerequisites

Install the Hot CLI and confirm `hot dev` works on your machine. The current
development version of TeamAgent depends on unreleased `hot-ai` and
`hot-ai-agent` packages, so clone the main `hot` repo as a sibling of
`hot-demos`:

```text
hot-dev/
  hot/
  hot-demos/
```

When the packages are published, this step goes away — the demo will resolve
them from the public registry.

## Step 1: Clone And Configure

Get a copy of the demo and copy the env example so you can edit local paths if
needed:

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/team-agent
cp .env.example .env
```

If your `hot` checkout isn't a sibling of `hot-demos`, point at it explicitly:

```bash
export HOT_AI_PATH=/path/to/hot/hot/pkg/hot-ai
export HOT_AI_AGENT_PATH=/path/to/hot/hot/pkg/hot-ai-agent
```

## Step 2: Verify The Project

Compile and run the demo's tests once before booting Hot Dev:

```bash
hot test
```

You should see four tests pass. This confirms the local package paths resolve
and TeamAgent compiles end to end.

## Step 3: Start Hot Dev

```bash
hot dev --open
```

The Hot App opens in your browser. Leave this terminal running and use a
second one for the steps below.

The webhook is declared in source as
`service: "team-agent", path: "/web/messages"`. Hot Dev's default profile uses
`org.slug = "local"` and `env.name = "development"`, so the full URL on this
machine is:

```text
http://localhost:4681/webhook/local/development/team-agent/web/messages
```

That same shape works in production, with your real org slug and environment
name (e.g. `https://api.hot.dev/webhook/acme/prod/team-agent/web/messages`).

## Step 4: Record A Team Message

This is what a chat adapter looks like under the hood — it sends a normalized
JSON payload. `session_id` identifies the channel; `user_id` identifies the
person. TeamAgent stores both.

```bash
curl -X POST http://localhost:4681/webhook/local/development/team-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{
    "session_id": "demo-channel",
    "user_id": "u-curt",
    "user_name": "Curt",
    "text": "We decided to ship the TeamAgent demo after the docs pass."
  }'
```

You should see `{"ok": true, "status": "response", "text": "message recorded"}`.
Send a couple more messages so there's something to ask about later.

## Step 5: Ask About Remembered Context

Now ask a question. TeamAgent finds matching messages from this session and
returns them. The standalone demo replies inline (`reply_mode: "response"` is
the default), so you'll see the answer in the curl response right away.

```bash
curl -X POST http://localhost:4681/webhook/local/development/team-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{
    "session_id": "demo-channel",
    "user_id": "u-curt",
    "text": "/ask what did we decide about the demo?"
  }'
```

The response includes the matching memory in `text`. In a real chat, an
adapter would post that text back to the channel.

## Step 6: Inspect Memory

Quickly check what TeamAgent has stored for this session:

```bash
curl -X POST http://localhost:4681/webhook/local/development/team-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-channel","user_id":"u-curt","text":"/memory"}'
```

You'll get a memory inspection summary — record count, capsule count, and
graph entry count.

## Optional: Use Hot Chat

For a browser walkthrough, run the [Hot Chat](/docs/demos/hot-chat) client and
select **TeamAgent** from the agent switcher. Quick prompts mirror the curl
examples here, and you can drag files in to attach them to messages.

## Commands In This Demo

The standalone TeamAgent demo implements these commands. Each one is a short
function in `hot/src/team-agent/demo.hot` so you can read the whole flow in
one file:

| Command       | What it does                                                          |
|---------------|-----------------------------------------------------------------------|
| (no command)  | record the message into session memory                                |
| `/ask <q>`    | answer from remembered context (inline)                               |
| `/summary`    | show a few recent records as a quick summary                          |
| `/decisions`  | recall messages tagged as decisions, action items, or open questions  |
| `/memory`     | inspect record/capsule/graph counts for this session                  |
| `/audit`      | show counts plus a hint about selective deletion                      |
| `/whoami`     | show the current transport, session id, and user id                   |
| `/guide`      | cheat sheet of the available commands                                 |

The full TeamAgent in the main Hot repo adds `/forget`, `/why`,
`/export`, `/compact`, `/search`, `/stats`, `/diag`, `/ai`, scheduled digests,
and a `Researcher` peer. The standalone demo deliberately keeps a smaller
surface area so the source stays readable.

## Agent Graph Walkthrough

Open the Hot App, choose **TeamAgent Demo**, and click the **Graph** tab. You
should see a webhook trigger on the left (`on-web-message`), feeding a single
helper (`process-incoming`), which sends to two event handlers:

- `remember-message` for normal messages,
- `answer-question` for `/ask`.

Click any node to open its inspector. The `process-incoming` node lists both
outgoing events under **Sends**, because the source declares them in
`meta {sends: …}`. That's the convention to follow when an agent emits events
through a helper that hides the literal `send(...)` from the compiler.

## Source

The runnable project lives in
[hot-dev/hot-demos/team-agent](https://github.com/hot-dev/hot-demos/tree/main/team-agent).
