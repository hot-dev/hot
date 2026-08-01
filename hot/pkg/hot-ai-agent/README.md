# hot-ai-agent

Reusable harness primitives for Hot AI agents.

`hot-ai` provides low-level AI building blocks under `::ai::*`: sessions,
memory, RAG, chat loops, tools, skills, and inter-agent bus messages.
`hot-ai-agent` extends that same namespace family with `::ai::agent::*`
for the application harness code that many agents otherwise reimplement.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/hot-ai-agent": "1.4.0"
```

`hot-ai-agent` 1.4.0 depends on `hot-ai` 1.7.0 or later. A current pair is:

```hot
"hot.dev/hot-ai": "1.8.1",
"hot.dev/hot-ai-agent": "1.4.0",
```

## Namespaces

- `::ai::agent` - package overview and common aliases.
- `::ai::agent::transport` - normalized inbound/outbound message shapes,
  transport tags, capabilities, and session metadata helpers.
- `::ai::agent::command` - slash-style command parsing and command
  declaration helpers.
- `::ai::agent::runtime` - per-agent runtime stores, session registry,
  counters, error logging, and time helpers.
- `::ai::agent::render` - neutral reply records and common text helpers.
- `::ai::agent::stream` - stable agent-level stream event labels and emit helpers.
- `::ai::agent::request` - request-scoped session, sender, and trusted context
  binding for statically registered tools.
- `::ai::agent::chat-turn` - the standard memory-grounded, streaming chat
  lifecycle.
- `::ai::agent::auth` - composable platform, shared-secret, and HMAC request
  verification.
- `::ai::agent::attachments` and `::ai::agent::blob` - bounded attachment
  normalization and durable content-addressed artifacts.
- `::ai::agent::callbacks` - validated and optionally signed completion
  callbacks.
- `::ai::agent::lifecycle` and `::ai::agent::synthesis` - session jobs and
  one-shot synthesis helpers.
- `::ai::agent::memory` and `::ai::agent::notify` - common memory commands and
  durable notification records.
- `::ai::agent::mcp` - helpers for agent-scoped MCP tools.

The package deliberately does not depend on transport vendor packages such as
Slack or Telegram. Adapters live in the application and call into these generic
types.

## Testing

```bash
hot test --project hot-ai-agent-tests
```
