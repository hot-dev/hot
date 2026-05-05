# Hot Chat Demo

Hot Chat is a polished local web chat UI for Hot agents. It is intentionally a
*transport client*, not a new agent: it sends normalized messages to TeamBrain
or PersonalAgent through Hot webhooks, using the same payload shape a Telegram
or Slack adapter would.

**Expected time:** 15 minutes. **Cost:** none.

## What You'll Get To See

- a clean chat UI that switches between TeamBrain and PersonalAgent live,
- quick-prompt chips that map to slash commands without baking policy into
  the UI,
- file attachments (drag-and-drop or paperclip) carried through to the agent
  as part of the same webhook payload,
- a transparent identity panel so you can read off the exact `session_id` and
  `user_id` the agent will see.

This is the demo to point at when someone asks "what would my product look
like on top of Hot?" — the UI is generic and the wire format is the same
contract used by every other transport.

## Prerequisites

Hot Chat needs an agent to talk to. Start one of the agent demos in another
terminal first; you can switch the active agent inside Hot Chat at any time.

```bash
cd hot-demos/personal-agent
hot dev --open
```

(Use `team-brain` instead if you'd rather start there.)

The demo expects the local Hot Dev defaults:

```text
http://localhost:4681/webhook/local/development/personal-agent/web/messages
http://localhost:4681/webhook/local/development/team-brain/web/messages
```

## Step 1: Install And Run

```bash
cd hot-demos/hot-chat
cp .env.example .env
npm install
npm run dev
```

Open [http://localhost:3000](http://localhost:3000).

## Step 2: Try A Quick Prompt

The first time you load Hot Chat, the conversation is empty and you see a
column of suggestions. Click **Daily brief** (PersonalAgent) or **Decisions**
(TeamBrain). The chip sends a slash command immediately and the agent reply
streams in below.

Try writing a message of your own next:

- with PersonalAgent: type *"I prefer launch updates that start with
  blockers"* and press Enter. PersonalAgent replies `remembered`. Click
  **Recall preferences** to verify it stuck.
- with TeamBrain: type *"We decided to ship docs before launch"*, then click
  **Ask the team** and adjust the prefilled question. The reply lists the
  matching memory.

## Step 3: Attach A File

Drag a small file (text, image, PDF — under 4 MB) anywhere onto the chat. A
chip appears below the composer. Send a message with it; the agent reply will
include `… with 1 attachment(s)`. The agent stores the file's name and type as
metadata; this demo doesn't deeply parse contents, but the same wire shape is
how a real product would forward documents to your agent.

## Step 4: Inspect Identity

Click **Identity** in the toolbar. You'll see the exact strings the agent
receives:

```text
Session       person:<your-uuid>      ← PersonalAgent
              web:chat:<your-uuid>    ← TeamBrain
User identity web:user:<your-uuid>
```

Edit your display name; the agent picks it up on the next message. Identity is
stored only in your browser's `localStorage` — clear site data to reset.

## Transport Contract

The Next.js server forwards each message as JSON to the Hot webhook:

```json
{
  "session_id": "person:<uuid>",
  "user_id":    "web:user:<uuid>",
  "user_name":  "Demo User",
  "text":       "/recall launch notes",
  "message_id": "web:<chat-id>:<timestamp>",
  "reply_mode": "response",
  "attachments": [
    {"name": "notes.md", "type": "text/markdown", "size": 412, "text": "…"}
  ],
  "metadata": {"client": "hot-chat", "kind": "command"}
}
```

A Telegram or Slack adapter can produce the same shape from native messages.
That's why Hot Chat doesn't need agent-specific code: the wire contract is
the contract.

## Why This Architecture

- **Browser → server route → Hot webhook.** Auth, CORS, and rate-limiting can
  live in the Next.js route later without touching the browser code.
- **No actions in the URL.** The UI passes free text and attachments; the
  agent decides what to do based on slash commands.
- **Stable IDs.** `chatId` and `userId` come from `localStorage`, so memory
  follows the user across page reloads. Production would replace these with
  your auth system's identifiers.

## Build For Production

```bash
npm run build
npm start
```

The production build is what CI exercises. There's no agent-specific config
in the build — point `HOT_AGENT_BASE_URL`, `HOT_ORG_SLUG`, and `HOT_ENV_NAME`
at any deployed Hot environment.

## Source

The runnable project lives in
[hot-dev/hot-demos/hot-chat](https://github.com/hot-dev/hot-demos/tree/main/hot-chat).
