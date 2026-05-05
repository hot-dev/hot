# hot-ai-agent

Reusable harness primitives for Hot AI agents.

`hot-ai` provides low-level AI building blocks under `::ai::*`: sessions,
memory, RAG, chat loops, tools, skills, and inter-agent bus messages.
`hot-ai-agent` extends that same namespace family with `::ai::agent::*`
for the application harness code that many agents otherwise reimplement.

## Namespaces

- `::ai::agent` - package overview and common aliases.
- `::ai::agent::transport` - normalized inbound/outbound message shapes,
  transport tags, capabilities, and session metadata helpers.
- `::ai::agent::command` - slash-style command parsing and command
  declaration helpers.
- `::ai::agent::runtime` - per-agent runtime stores, session registry,
  counters, error logging, and time helpers.
- `::ai::agent::render` - neutral reply records and common text helpers.
- `::ai::agent::stream` - stable agent-level stream labels and emit helpers.
- `::ai::agent::mcp` - helpers for agent-scoped MCP tools.

The package deliberately does not depend on transport vendor packages such as
Slack or Telegram. Adapters live in the application and call into these generic
types.

## Testing

```bash
hot test --project hot-ai-agent-tests
```
