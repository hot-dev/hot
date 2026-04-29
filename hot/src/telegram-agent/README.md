# Team Brain — Multi-Transport AI Agent

An AI agent that gives any chat (Telegram group, Slack channel, …) a
searchable, summarizable memory. Built with Hot, `::hot::ai`, and one
adapter per transport.

> **Project note.** Local development runs in the default `hot-dev`
> project. The narrower `demo-team-brain` project remains available for
> targeted deploys. The directory (`hot/src/telegram-agent/`) and
> `::telegram-agent::*` namespace are kept for now to avoid a wide
> rename across the repo and history.

**What it does:**
- Records every message into session-scoped semantic memory.
- `/ask` — answers questions about chat history, with multi-turn
  conversation context per (session, user). Routes general-knowledge
  questions to a second `Researcher` agent over `::ai::bus`.
- `/summary` — generates an AI summary of recent activity.
- `/decisions` — extracts decisions, action items, and open questions.
- `/search` — semantic search over message history.
- `/stats`, `/diag` — usage stats and runtime diagnostics for the
  current session.
- Optional voice transcription (Telegram only) — voice notes get sent
  through Whisper (in a Hot Box) and indexed alongside text.
- Scheduled daily digest + weekly summary across every registered
  session.
- MCP tool `search_team_brain` so external AI clients can query a
  session's memory.
- Citations with deep links — `/ask` replies link back to the cited
  messages in the chat (Telegram chat link, Slack permalink).

Runs identically in local development and in production on Hot Cloud.

---

## Architecture: agent core + per-transport adapters

```
Telegram update ─┐                              ┌── reply via Telegram Bot API
                 │                              │
Slack event ─────┼─▶ adapter ─▶ IncomingMessage ─▶ agent core ─▶ ::transport/reply ─┼── reply via Slack chat.postMessage
                 │  (translates) (normalized)    (transport-blind) (dispatches)    │
…future          │                              │                                  │
transports ──────┘                              └── ::transport/show-typing        └── …
```

Three layers:

| Layer | File(s) | Knows about |
|---|---|---|
| **Adapter** | `telegram_adapter.hot`, `slack_adapter.hot` | Its transport's API, file ids, message shapes, deep-link format |
| **Transport** | `transport.hot` | A `Session` carries a `transport` tag in `meta`; dispatches `reply` / `show-typing` / `format-source-link` to the right adapter |
| **Agent core** | `agent.hot` | `Session` + `Identity` + `IncomingMessage` only — never imports a transport package directly |

Adding a new transport = add `${name}_adapter.hot` (translate
incoming + implement `reply-impl` / `show-typing-impl` /
`message-link`) and add one branch in each of `reply`, `show-typing`,
`format-source-link` in `transport.hot`. The agent core does not
change.

### Library functions vs. agent assembly (the "wrapper pattern")

The two adapter files are treated as **library code**: they expose
pure callable functions (`record-voice`, `check-updates`,
`on-telegram-update`, `on-slack-event`, …) but do **not** carry
agentic `meta` annotations like `schedule:`, `on-event:`, or
`webhook:`. All such annotations live in `agent.hot` (search for
*Transport wiring*) as **meta-bearing aliases** of the library
functions.

The shorthand looks like this:

```hot
::tg-adapter ::telegram-agent::telegram-adapter

tg-record-voice
meta {
    agent: TeamBrain,
    on-event: "telegram:record-voice",
    retry: 2,
}
::tg-adapter/record-voice
```

The compiler treats this as a registration of `tg-record-voice`
(arity, doc, and source location all flow through from
`::tg-adapter/record-voice`), with the alias's `meta` keys winning
over the library function's keys on collision.

Two equivalent meta placements are accepted, parallel to how `type`
and `enum` work:

```hot
// Form A — meta before value
tg-record-voice meta { on-event: "telegram:record-voice" } ::tg-adapter/record-voice

// Form B — meta after value
tg-record-voice ::tg-adapter/record-voice meta { on-event: "telegram:record-voice" }
```

If you need transport-specific behaviour beyond registration (e.g.
wrapping the call in `try-call` for fault isolation, or doing some
pre/post-processing), upgrade the alias to a full wrapper:

```hot
tg-record-voice
meta { on-event: "telegram:record-voice", retry: 2 }
fn (event) {
    result ::hot::lang/try-call(() { ::tg-adapter/record-voice(event) })
    cond { not(result.ok) => { record-error("voice", result.error) } }
}
```

Why this pattern (regardless of which form you use):

- **No duplicate registrations across builds.** Hot's compiler
  scans every namespace it compiles for scheduled functions, event
  handlers, and webhooks. If the adapter file declared its own
  schedule, every project that includes the adapter would register
  the same cron / endpoint, leading to multi-fire schedules and
  ambiguous webhook routing (`ROUTING: Multiple builds have function
  …` warnings).
- **Project-specific tuning lives next to the rest of the agent
  assembly.** Retry counts, webhook service names, and cron tweaks
  belong with the agent that uses them, not the adapter.
- **Library reuse is straightforward.** Another project (or another
  agent in the same project) can reuse the adapter without
  inheriting an opinionated set of registrations.

This convention applies to **all library code in this repo** with
the explicit exception of `hot/pkg/hot-std/` — the standard library
does process schedule/call dispatch internally and registers the
core `hot:call`, `hot:schedule`, `hot:schedule:new`,
`hot:schedule:cancel` event handlers itself.

Each agent assembly file (here: `agent.hot` for Team Brain,
`researcher.hot` for the Researcher peer) **is** wiring code by
construction and does carry agentic annotations directly on its
own handlers (`record-message`, `handle-ask`, `daily-digest`, …).

### Multi-tenancy / session isolation

`Session.id` is namespaced by transport so collisions are impossible:

- Telegram: `telegram:${chat-id}` (e.g. `telegram:-1001234567890`)
- Slack:    `slack:${team-id}:${channel-id}` (e.g. `slack:T0123:C0456`)

Every per-conversation store (`brain-stats`, `brain-errors`, RAG
session memory, per-user threads) is keyed by `session.id`, and
per-user threads further nest the `Identity.id` via
`::ai::session/session-user-key`. One bot installed in three Telegram
groups and two Slack channels keeps five independent memories without
extra config.

### Transport-aware skills

The skill resolver (`session-skill-resolver` in `agent.hot`) inspects
the active session's transport tag and hides skills tagged for other
transports. So `telegram-tone` (mrkdwn-free, HTML-flavored) is invisible
in Slack, and `slack-tone` (mrkdwn) is invisible in Telegram. Skills
without a `transport:*` tag in `when:` are always visible.

Add a transport-specific skill by giving it a `when:` entry like
`"transport:slack"`; everything else stays the same.

---

## Quick Start — Telegram

### 1. Create a Telegram bot

1. Message [@BotFather](https://t.me/BotFather) on Telegram and run
   `/newbot`. Pick a name and username.
2. Save the **bot token** BotFather sends you — it looks like
   `123456789:ABC-DEF...`.
3. In the same chat with BotFather, run `/mybots` → select your bot →
   **Bot Settings** → **Group Privacy** → **Turn off**. This lets the
   bot read all group messages (otherwise it only sees messages that
   start with `/`).

### 2. Add the bot to a group

Create a Telegram group (or pick an existing one) and add your bot.
Group chat IDs are negative numbers — find yours by sending a message
in the group then:

```bash
curl "https://api.telegram.org/bot<YOUR_BOT_TOKEN>/getUpdates"
```

Look for `"chat":{"id":-100..., "type":"supergroup"}`.

### 3. Set context variables

```bash
hot ctx set telegram.bot.token "123456789:ABC-DEF..."
hot ctx set telegram.chat.id   "-1001234567890"

# At least one AI provider:
hot ctx set anthropic.api.key "sk-ant-..."
# …and/or
hot ctx set openai.api.key    "sk-..."

# Optional
hot ctx set brain.model            "claude-sonnet-4-5"
hot ctx set brain.polling.enabled  "true"      # local dev
hot ctx set telegram.webhook.secret "<random>" # production webhook verification
```

### 4. Run it

```bash
hot dev
```

You should see `check-updates` scheduled every 30 seconds (Telegram
long-poll loop). Chat in the group, then try `/start`, `/help`, `/ask
what did we decide about the launch?`, `/summary 6h`, `/ai`.

The bot **only stores messages sent after it joined the group.**

---

## Quick Start — Slack

### 1. Create a Slack app + bot user

1. Visit <https://api.slack.com/apps> → **Create New App** → from
   scratch → pick a workspace.
2. **OAuth & Permissions** → add bot scopes:
   - `chat:write` — for posting replies
   - `channels:history` — to receive channel messages via Events API
   - `groups:history` — same, for private channels
   - `users:read` — to resolve user display names for citations
3. Install the app to your workspace and save the **Bot User OAuth
   Token** (starts with `xoxb-`).
4. **Event Subscriptions** → enable, set the request URL to
   `https://<your-deploy-host>/team-brain/slack/events` (you can do
   this *after* deploying), and subscribe to bot events:
   - `message.channels`
   - `message.groups`
5. Invite the bot to a channel: `/invite @TeamBrain` from inside the
   target channel.

### 2. Set context variables

```bash
hot ctx set slack.api.key     "xoxb-..."
hot ctx set slack.team.id     "T0123ABC"     # find via /api/auth.test
hot ctx set slack.channel.id  "C0123XYZ"     # only used by the dev driver
hot ctx set slack.team.domain "myworkspace"  # for permalink generation; optional
```

### 3. Deploy and register the events URL

```bash
hot deploy --project demo-team-brain
```

Then point Slack's Event Subscriptions at the deployed
`/team-brain/slack/events` URL (Slack will challenge once with a
`url_verification` payload — the agent handles it automatically).

---

## Local Iteration Without a Real Bot

You don't need to register a Telegram bot or Slack app to iterate on
the agent. [dev-driver.hot](dev-driver.hot) builds a transport-stamped
`IncomingMessage` from CLI ctx variables and feeds it through the same
`::agent/process-incoming` entry point a real adapter would. Combine
with **dry mode** (`brain.dev.dry true`) and replies print to stdout
instead of reaching the live transport.

### One-off run via `--ctx`

Drop a small Hot file under `/tmp/dev-driver.ctx.hot`. Everything
reads from environment variables so secrets stay out of the file:

```hot
::hot::run::ctx ns

::env ::hot::env

::hot::ctx/set({
  // AI provider keys.
  "anthropic.api.key": ::env/get("ANTHROPIC_API_KEY", "PLACEHOLDER"),
  "openai.api.key":    ::env/get("OPENAI_API_KEY",    "PLACEHOLDER"),

  // Dry mode short-circuits all transport I/O.
  "brain.dev.dry":         "true",
  "brain.polling.enabled": "false",

  // Telegram (only used when dev.transport is "telegram").
  "telegram.bot.token": ::env/get("TELEGRAM_BOT_TOKEN", "dev-fake-token"),
  "telegram.chat.id":   ::env/get("TELEGRAM_CHAT_ID",   "-1001234567890"),

  // Slack (only used when dev.transport is "slack").
  "slack.team.id":      ::env/get("SLACK_TEAM_ID",     "T-DEV"),
  "slack.channel.id":   ::env/get("SLACK_CHANNEL_ID",  "C-DEV"),
  "slack.team.domain":  ::env/get("SLACK_TEAM_DOMAIN", "dev"),

  // Driver inputs — override via env.
  "dev.cmd":       ::env/get("DEV_CMD",       "help"),
  "dev.text":      ::env/get("DEV_TEXT",      ""),
  "dev.transport": ::env/get("DEV_TRANSPORT", "telegram"),
  "dev.user":      "curt-dev",
  "dev.user.id":   "9999",
})
```

Then export the credentials you actually need and run the driver:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."

# Telegram /help (no LLM needed)
hot run \
        --ctx /tmp/dev-driver.ctx.hot \
        hot/src/telegram-agent/dev-driver.hot

# Telegram /ask against seeded chat history
DEV_CMD=ask DEV_TEXT="What did we decide about the launch?" \
  hot run \
          --ctx /tmp/dev-driver.ctx.hot \
          hot/src/telegram-agent/dev-driver.hot

# Same agent, Slack session
DEV_TRANSPORT=slack DEV_CMD=diag \
  hot run \
          --ctx /tmp/dev-driver.ctx.hot \
          hot/src/telegram-agent/dev-driver.hot
```

The dry-run reply tag includes the session id so you can verify
transport routing at a glance:

```
[dry-reply session=telegram:-1001234567890 reply-to=…]
…
[dry-reply session=slack:T-DEV:C-DEV reply-to=…]
…
```

### Supported `dev.cmd` values

| dev.cmd     | maps to                                |
|-------------|----------------------------------------|
| `ask`       | `/ask <dev.text>`                      |
| `summary`   | `/summary <dev.text>` (e.g. `6` hours) |
| `decisions` | `/decisions <dev.text>`                |
| `search`    | `/search <dev.text>`                   |
| `stats`     | `/stats`                               |
| `diag`      | `/diag`                                |
| `help`      | `/help`                                |
| `text`      | plain message (gets recorded, no command) |

Dry mode short-circuits transport I/O only. Anything that needs the
LLM (`/ask`, `/summary`, `/decisions`, `/search` for embedding lookups)
still calls the configured provider, so export a real
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` when exercising those paths.
`/help` works without any provider credentials.

### Seeding memory before /ask

`/ask`, `/summary`, `/decisions`, and `/search` all read from
`::hot::store`, so an empty history yields empty answers. Run the
seed once to populate ~28 synthetic Telegram messages spanning a
launch decision, a postmortem, and several action items:

```bash
hot run \
        --ctx /tmp/dev-driver.ctx.hot \
        hot/src/telegram-agent/seed.hot
```

Memory persists in your local store across runs, so this is a
one-shot setup unless you wipe it. The seed registers its session
into the cross-transport `registered-sessions` index so scheduled
`daily-digest` / `weekly-summary` jobs can find it.

### Bus-routed commands under `hot run`

`/ask`, `/summary`, `/decisions`, and `/search` normally fan out via
`::ai::bus` and `meta {on-event: …}` handlers — that works under
`hot dev` (which keeps a worker alive) but **not** under `hot run`,
which exits as soon as the script finishes. The dev-driver papers
over this gap by calling `handle-ask` / `handle-summarize` /
`handle-decisions` / `handle-search` directly when invoked through
`hot run`. Other commands (`/help`, `/brain`, `/ai`, `/stats`,
`/diag`) flow through the real `process-incoming` path because they
reply inline.

### Going live without leaving local

Flip `brain.dev.dry` to `false` and the same dev-driver invocations
will actually post to your group/channel. No code changes — same
script, real bot.

---

## Tests

The project ships unit tests that drive the run-loop with mock
`chat-fn`s, asserting that the skill resolver index lands in the
system prompt and that the built-in skill tools (`list_skills`,
`read_skill`, `apply_skill`) get injected into the tool list:

```bash
hot test
```

These tests do not require any Telegram, Slack, or LLM credentials.

---

## Commands

| Command | What it does |
|---|---|
| `/start`, `/help` | Welcome message for new users |
| `/brain` | Show AI model and full command list |
| `/ask <question>` | Ask a question, grounded in session history (multi-turn per user) |
| `/summary` | Summarize the last 24 hours |
| `/summary <N>` | Summarize the last N hours (e.g. `/summary 6h`) |
| `/decisions` | Extract decisions, action items, and open questions from the last 48h |
| `/decisions <N>` | Same, custom window |
| `/search <query>` | Semantic search — returns raw matching messages with scores |
| `/ai` | Show AI model; list switchable aliases |
| `/ai <alias>` | Switch AI model (e.g. `/ai sonnet`, `/ai gpt`) |
| `/stats` | Messages recorded, top contributors (7d), AI calls + tokens today, active days |
| `/diag` | Active **transport**, model, polling/webhook/voice config, memory store sizes, registered sessions, recent errors |

All command replies are threaded to the original message
(Telegram `reply_parameters`, Slack `thread_ts`).

---

## Scheduled Jobs

| Handler | Schedule | Behavior |
|---|---|---|
| `check-updates` (in `telegram_adapter.hot`) | every 30 seconds | Telegram long-poll loop. Gated on `brain.polling.enabled`. |
| `daily-digest` | every day at 9am | Posts a digest of yesterday's activity to **every registered session** (each transport's adapter handles the actual send) |
| `weekly-summary` | every Monday at 9am | Same fan-out, weekly window |

`daily-digest` and `weekly-summary` iterate over
`list-registered-sessions()` and use `::hot::lang/try-call` so a
failure on one session (down API, evicted credentials) doesn't kill
the run for the others.

---

## Local Dev vs. Production

### Telegram

Two delivery modes:

- **Long-polling** (`check-updates` scheduled function) — self-contained,
  no public URL required. Default in local dev.
- **Webhook** (`on-telegram-update` handler at `/team-brain/updates`) —
  zero-latency, requires a public HTTPS URL and a one-time `setWebhook`
  call.

Don't run both — they double-deliver. In production:

```bash
hot ctx set brain.polling.enabled false
curl -X POST "https://api.telegram.org/bot<TOKEN>/setWebhook" \
  -d "url=https://<host>/team-brain/updates" \
  -d "secret_token=<your-telegram.webhook.secret>"
```

### Slack

Slack uses Events API webhooks exclusively (`on-slack-event` at
`/team-brain/slack/events`). No polling option. Set the request URL
in your app's **Event Subscriptions** settings after deploying.

---

## Deploy

```bash
hot ctx set brain.polling.enabled false
hot deploy --project demo-team-brain
```

Then register the Telegram webhook and/or Slack Events URL against
your deployed host.

---

## How It Works

```
Telegram ──webhook or long-poll──┐
                                 │
Slack ──Events API──────────────┐│
                                ││
                                ▼▼
                         [adapter.to-incoming]
                                │
                          IncomingMessage
                                │
                  ┌─────────────┴─────────────┐
                  ▼                           ▼
         direct-reply commands        bus-routed commands
         (/help, /diag, /stats,       (/ask, /summary,
          /ai, /brain)                 /decisions, /search)
                  │                           │
                  ▼                           ▼
        ::transport/reply           handle-ask / handle-summarize / …
                  │                           │
                  ▼                           ▼
        adapter.reply-impl          ::transport/reply
                  │                           │
                  ▼                           ▼
        Telegram / Slack            adapter.reply-impl
                                              │
                                              ▼
                                    Telegram / Slack
```

Memory is scoped to each session so the agent can serve multiple
chats across multiple transports without cross-contamination.
Per-(session, user) conversation threads
(`::ai::memory/thread`) keep `/ask` multi-turn while preventing
spillover when the same user works in multiple channels.

### Routing & Researcher

`handle-ask` classifies each `/ask` question:

- **Memory questions** ("what did *we* decide…?", anything mentioning
  a teammate or the chat itself) are answered by RAG over the session's
  memory.
- **External questions** ("what is X?", "explain Y") are forwarded to
  `Researcher` (defined in [researcher.hot](researcher.hot)) over
  `::ai::bus/collaborate`. Researcher uses a different model (default
  `gpt-5-mini`) and has no memory access — it answers from training
  data only and is prompted to flag uncertainty.
- The reply comes back over the bus to `handle-ask-response`, which
  posts it via `::transport/reply` (so it goes to the right transport
  automatically).

This second agent shows up as a node in the **Agent Graph**, connected
by `agents:collaborate` and `brain:ask-response` edges. Set
`researcher.model` to override its default model.

---

## Voice Transcription (Telegram only)

Voice notes can be transcribed via Whisper (running in a Hot Box
container) and indexed into memory alongside text messages. **Off by
default** — each voice message spins up a container, which costs
runtime minutes. Slack has no equivalent path in v1.

To enable:

```bash
hot ctx set brain.voice.enabled true
hot ctx set brain.voice.model   "base"   # tiny | base | small | medium | large
```

When enabled, the Telegram adapter intercepts voice/audio messages
before they reach the agent core and dispatches a
`telegram:record-voice` event to its own `record-voice` handler, which
downloads the OGG, calls `::whisper/transcribe`, and stores the
transcript exactly like a text message (with `source: "voice"` in
metadata). The bot also drops a small `Transcribed voice message: …`
reply so the system is visibly working in the demo.

Use `/diag` to see whether voice is enabled and which model is
configured.

---

## Filming the Demo

To prep a fresh Telegram chat with realistic-looking history before
recording:

```bash
hot ctx set telegram.chat.id "-1001234567890"
hot run hot/src/telegram-agent/seed.hot
```

This posts ~28 synthetic messages from five fictional teammates over
the previous 24 hours, covering a launch decision, a checkout-bug
postmortem, action items, shared links, and an open hiring question.
After seeding, `/summary`, `/decisions`, `/search`, and `/ask` all
return interesting results immediately — no live group needed.

The seed only writes to memory; it does **not** post anything to
Telegram or Slack.

---

## Source Layout

```
hot/src/telegram-agent/
├── agent.hot              # TeamBrain core: handlers, schedules, MCP tool, /stats, /diag,
│                            transport-aware skill resolver, registered-sessions index
├── transport.hot          # Transport-agnostic surface: IncomingMessage, reply,
│                            show-typing, format-source-link, dry-run gate
├── telegram_adapter.hot   # Telegram: session/identity translation, reply-impl,
│                            polling loop, webhook, voice download + transcription
├── slack_adapter.hot      # Slack: session/identity translation, reply-impl,
│                            Events API webhook, permalink generator
├── skills.hot             # Inline-body skills: telegram-tone, slack-tone,
│                            coach-on-stats; bundled-tool example
├── researcher.hot         # Researcher agent: handles external questions over ::ai::bus
├── seed.hot               # Synthetic conversation for filming
├── dev-driver.hot         # Local iteration harness (dry-run-friendly, multi-transport)
├── resources/skills/      # *.skill.md sources for codegen'd skills
└── README.md              # This file

hot/pkg/hot-ai/             # Reusable AI-agent library (session, memory, rag, bus, skill, …)
hot/pkg/telegram/           # Telegram Bot API bindings
hot/pkg/slack/              # Slack Web API bindings
hot/pkg/whisper/            # Whisper-in-a-Hot-Box for voice transcription
```

---

## Troubleshooting

**Bot doesn't see Telegram group messages** — BotFather → `/mybots`
→ your bot → Bot Settings → Group Privacy → **Turn off**.

**Slack bot doesn't see channel messages** — Reinstall the app after
adding `channels:history` / `groups:history` scopes; OAuth tokens
don't auto-refresh new scopes.

**"Missing required context variable"** — Run `hot ctx list` to see
what's set, then use `hot ctx set <name> <value>` to fill gaps.

**`/ask` says "I don't have enough message history"** — The bot only
stores messages received after it joined. Chat for a bit first, then
ask again; or run the seed for Telegram.

**Webhook returns 401 (Telegram)** — `telegram.webhook.secret` must
match the `secret_token` you passed to `setWebhook`. Unset both to
disable verification.

**Slack URL verification fails** — The `on-slack-event` handler
auto-replies to `url_verification` callbacks. If Slack still complains,
double-check the Request URL points at `/team-brain/slack/events` on
the deployed host (not local).

**Duplicate Telegram responses in production** — You have both polling
and the webhook enabled. Set `brain.polling.enabled = false`.

**Voice messages aren't transcribed** — Voice is opt-in and Telegram-
only. Set `brain.voice.enabled = true`. `/diag` will confirm the
current state.

**`/ask` always says "Asked Researcher…"** — Routing classified the
question as external. Try phrasing it as "what did *we* decide…" or
"what was discussed about…" to keep it in memory; or check `/stats`
to confirm the chat actually has messages stored.
