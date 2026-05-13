---
name: group-summary
description: How TeamAgent should structure /summary and /decisions output for a chat session.
when:
  - summarize the group
  - generate digest
  - extract decisions
---

# Summarizing a chat session

TeamAgent summaries are often read on a phone, between meetings. Optimize
for **scannability**, not completeness.

## Structure

1. **One-line lede.** The single most important thing that happened in
   the window — pick a decision, a blocker, or the loudest topic.
2. **Themes (2–4 bullets).** Each bullet is `<b>Topic</b>: one
   sentence`. Group adjacent messages on the same topic into one
   bullet; do not enumerate every back-and-forth.
3. **Decisions & action items.** A short `<b>Decisions:</b>` list, then
   a separate `<b>Action items:</b>` list with the owner inlined as
   `<b>${name}</b> — …` when known.
4. **Open questions.** A final `<b>Open:</b>` list of anything
   explicitly unanswered. Skip the section if there are none.

## Tone

- Quote teammates by their display name in `<b>…</b>`. Avoid
  pronouns like "they" / "the team" when a specific person spoke.
- Stay neutral. Do not editorialize ("great point", "important
  decision") — let the items speak for themselves.
- If the window is mostly off-topic chatter, say so in one line and
  cut the summary short. Do not pad.

## Edge cases

- **Empty window** → reply with a single sentence ("No new messages
  recorded in the last `${hours}h.") and stop.
- **Single-author window** → frame as `<b>${name}</b> posted N
  messages, mostly about …` rather than as a multi-author summary.
- **Spammy / low-signal window** → produce a 1-line "mostly
  greetings and link-sharing" summary and stop.
