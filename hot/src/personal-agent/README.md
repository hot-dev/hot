# PersonalAgent Demo

PersonalAgent is the identity-first sibling of TeamAgent. It uses the shared
`hot-ai` memory/RAG primitives and the reusable `hot-ai-agent` harness, but its
policy treats user/profile memory as primary and session memory as context.

The demo starts with a custom web transport declared as
`service: "personal-agent", path: "/web/messages"`. In local Hot Dev with the
default profile, post to:

```text
http://localhost:4681/webhook/local/development/personal-agent/web/messages
```

Commands:

- `/remember <text>`
- `/recall <query>`
- `/forget <session|source|user> <selector>`
- `/export`
- `/brief`
- `/tasks`
- `/prefs`
- `/compact [hours]`
- `/memory`
