# PersonalAgent Demo

PersonalAgent is the identity-first sibling of TeamAgent. It's still one chat
agent, but the durable memory key is the *person*, not the channel. That makes
it a good base for assistants, journaling apps, or per-user copilots.

**Expected time:** 15–20 minutes. **Cost:** none — the demo never calls an LLM.

## What You'll Build

A web-callable agent that:

- treats every incoming user as the durable memory owner,
- stores notes with `/remember`,
- searches per-user memory with `/recall`,
- generates a quick brief and a privacy review on demand.

You'll also see why this agent's graph is intentionally smaller than
TeamAgent's: command handling is direct, not event-driven, until you decide
you need it.

## Prerequisites

Install the Hot CLI and check `hot dev` works. PersonalAgent shares the
unreleased `hot-ai` and `hot-ai-agent` packages with TeamAgent, so clone the
main `hot` repo as a sibling of `hot-demos`:

```text
hot-dev/
  hot/
  hot-demos/
```

## Step 1: Clone And Configure

```bash
git clone https://github.com/hot-dev/hot-demos
cd hot-demos/personal-agent
cp .env.example .env
```

If your checkout layout differs, point at the local packages:

```bash
export HOT_AI_PATH=/path/to/hot/hot/pkg/hot-ai
export HOT_AI_AGENT_PATH=/path/to/hot/hot/pkg/hot-ai-agent
```

## Step 2: Verify The Project

```bash
hot test
```

You should see five tests pass. The most informative one is
`test-identity-session-default`: it confirms that without an explicit
`session_id`, PersonalAgent derives one from the user (`person:<user-id>`).

## Step 3: Start Hot Dev

```bash
hot dev --open
```

Keep this running and use a second terminal for the commands below. The
webhook is declared as `service: "personal-agent", path: "/web/messages"`, so
your local URL is:

```text
http://localhost:4681/webhook/local/development/personal-agent/web/messages
```

## Step 4: Remember A Preference

Send a message that starts with `/remember`. Notice we don't pass
`session_id` — PersonalAgent fills it in as `person:u-curt`, because the
memory should follow the user across sessions.

```bash
curl -X POST http://localhost:4681/webhook/local/development/personal-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{
    "user_id": "u-curt",
    "user_name": "Curt",
    "text": "/remember I prefer concise morning briefs grouped by project."
  }'
```

You should see `"text": "remembered"` in the response.

## Step 5: Recall That Preference

```bash
curl -X POST http://localhost:4681/webhook/local/development/personal-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{
    "user_id": "u-curt",
    "text": "/recall morning brief preferences"
  }'
```

The response is either a `Personal memory:` list or, if your local Hot
install has no embedding provider, a no-match note. The demo still
demonstrates per-user scoping either way.

## Step 6: Inspect And Export Memory

```bash
curl -X POST http://localhost:4681/webhook/local/development/personal-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{"user_id":"u-curt","text":"/memory"}'
```

Then:

```bash
curl -X POST http://localhost:4681/webhook/local/development/personal-agent/web/messages \
  -H 'content-type: application/json' \
  -d '{"user_id":"u-curt","text":"/export"}'
```

Both replies are scoped to `u-curt` only — different `user_id` values would
return different state.

## Optional: Use Hot Chat

For a browser walkthrough, run the [Hot Chat](/docs/demos/hot-chat) client and
select **PersonalAgent**. Quick prompts cover Remember, Recall, Brief, Tasks,
Memory, and Export. You can drag files in to attach them to a `/remember`.

## Commands In This Demo

| Command         | What it does                                                  |
|-----------------|---------------------------------------------------------------|
| `/remember <text>` | store a personal note (records attachments as metadata)    |
| `/recall <query>`  | search identity-scoped memory                              |
| `/brief`           | recall preferences and tasks for a quick brief             |
| `/tasks`           | recall commitments and next actions                        |
| `/memory`          | inspect record/capsule/graph counts for this user          |
| `/export`          | summarize the exportable memory bundle                     |
| `/privacy`         | show the memory shape with a privacy-review framing        |
| `/whoami`          | show transport, session, and user identity                 |
| `/guide`           | cheat sheet of the available commands                      |

The full PersonalAgent in the main Hot repo also implements `/forget`,
`/compact`, and `/prefs`. The standalone demo keeps a smaller surface so the
source stays readable in one file.

## Agent Graph Walkthrough

Open the Hot App, choose **PersonalAgent Demo**, and look at the Graph tab.
You'll see one webhook trigger (`on-web-message`) feeding `process-incoming`,
which dispatches commands directly. There are no event-loop edges yet — every
reply happens inline.

This is intentional: most personal-agent commands are I/O-light enough that
you don't need an event hop. If you later split a command into a background
job, add `on-event` to the new handler and declare the hidden helper sends in
`meta {sends: …}` so the Agent Graph stays accurate.

## Source

The runnable project lives in
[hot-dev/hot-demos/personal-agent](https://github.com/hot-dev/hot-demos/tree/main/personal-agent).
