# hot-ai

Dependency-free building blocks for AI agents in Hot. Multi-tenant sessions, typed messages, scoped memory, RAG, context management, and inter-agent communication — all without depending on any specific AI provider package.

Provider-specific code (Anthropic, OpenAI, xAI, Gemini) stays in the user's project; `hot-ai` calls it through a `chat-fn` parameter.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/hot-ai": "1.5.0"
```

## Namespaces

### `::ai::session` — Multi-tenant primitives

Lightweight handle types for organizing agents across platforms.

```hot
::session ::ai::session

chat  ::session/Session({id: "telegram:12345"})
alice ::session/Identity({id: "u1", name: "alice"})

// Scoped keys for store names
::session/session-key("brain", chat, "memory")             // "agent:brain:session:telegram:12345:memory"
::session/user-key("brain", alice, "prefs")                // "agent:brain:user:u1:prefs" (cross-session)
::session/session-user-key("brain", chat, alice, "thread") // "agent:brain:session:telegram:12345:user:u1:thread"
::session/agent-key("brain", "kb")                         // "agent:brain:kb"
```

Use `session-user-key` for anything tied to a specific conversation
(per-chat threads, per-channel role) so a person who participates in
multiple sessions doesn't collide with themselves.

### `::ai::message` — Normalized message types

- `Role` enum (`User`, `Assistant`, `System`)
- `ChatMessage` — role + content for AI API calls
- `Message` — inbound user messages with `sender`, `session`, `source`
- `AgentMessage` — inter-agent envelopes

```hot
::msg ::ai::message

msg ::msg/user-msg("Hello!")                       // ChatMessage
m   ::msg/Message({
  id: "42", content: "hi team",
  sender: alice, session: chat,
  timestamp: 1700000000, source: "telegram",
})
```

### `::ai::chat` — Provider-agnostic dispatch

Detects the provider for any model name and bundles a caller-provided chat function.

```hot
::chat ::ai::chat

::chat/detect-provider("claude-sonnet-4-5")   // Provider.Anthropic
::chat/detect-provider("gpt-5.2")             // Provider.OpenAi
::chat/provider-name(Provider.Anthropic)      // "Anthropic"

opts ::chat/ChatOptions({
  chat-fn: ::anthropic::messages/chat,   // YOUR provider's chat function
  model:   "claude-sonnet-4-5",
  system:  "You are a helpful assistant.",
})
```

### `::ai::skill` — Prompt skills

Skills are named prompt augmentations that an agent can expose to the
model through `list_skills`, `read_skill`, and `apply_skill`.
Prompt-only skills are plain `Skill` values:

```hot
::skill ::ai::skill

refund-tone ::skill/Skill({
  name: "refund-tone",
  description: "How to answer refund requests",
  when: ["refund", "return policy"],
  body: "Be concise, empathetic, and cite the policy window.",
})
```

Function-backed skills are still supported with `meta {skill: ...}` and
`::skill/from-fn(fn-ref)`. Use that form when the skill needs a
`body-fn` or when you want to attach metadata to an existing function.

Markdown-authored skills use `*.skill.md` files under the project's
`resources.paths`. Hot codegen turns them into generated `.skill.hot`
files that export `Skill` values. Because generated skills construct
`::ai::skill/Skill`, projects with Markdown skill resources must include
`"hot.dev/hot-ai"` in `hot.project.<name>.deps`.

### `::ai::memory` — Three-level scoped memory

Session (shared conversation history), User (per-identity data), and KB (shared knowledge base). Plus per-user `Thread` for multi-turn conversations.

```hot
::mem ::ai::memory

mem ::mem/create-memory("brain", chat, alice)

// Session-level: remember a message, then recall semantically
::mem/remember(mem, message)
results ::mem/recall(mem, "what did we decide?", {limit: 10, min-score: 0.3})

// User-level: per-identity profile data
::mem/set-user-data(mem, "timezone", "America/New_York")
::mem/get-user-data(mem, "timezone")

// KB: shared, embedded knowledge
::mem/learn(mem, "onboarding", {content: "Welcome! Start with /help"})

// Per-(session, user) multi-turn thread — same `chat` you used to
// build `mem`, so memory and thread share the same scope.
t ::mem/thread("brain", chat, alice, {max-turns: 10})
::mem/thread-add(t, Role.User, "hello")
::mem/thread-add(t, Role.Assistant, "hi alice")
hist ::mem/thread-history(t, null)
```

### `::ai::rag` — Retrieve-Augment-Generate

Composes memory search with a caller-provided `chat-fn`.

```hot
::rag ::ai::rag

// Single-shot Q&A grounded in memory
result ::rag/ask(mem, "when did alice join?", opts, {limit: 10})
// => {answer: "...", sources: [...]}

// Multi-turn with thread history
result ::rag/ask-with-thread(mem, t, "and when did bob?", opts, {limit: 10})

// Summarize recent activity
summary ::rag/summarize(mem, opts, {hours: 24, instruction: "..."})
```

Pure helpers (testable without stores):
`format-session-results`, `format-kb-results`, `format-thread-lines`, `build-rag-prompt`

### `::ai::context` — Token budgets

```hot
::aictx ::ai::context

::aictx/estimate-tokens("hello world")
::aictx/sample(messages, {max-items: 300, strategy: "bookend"})
::aictx/fit(messages, {max-tokens: 8000})
::aictx/truncate("very long text", {max-chars: 2000})
```

### `::ai::bus` — Inter-agent communication

Typed event-bus patterns: targeted `tell`, `broadcast`, `collaborate`, `respond`.

```hot
::bus ::ai::bus

// Targeted delivery
::bus/tell(AgentMessage({
  session: chat, sender: alice,
  to-agent: "researcher", from-agent: "brain",
  content: "please research topic X", timestamp: 1700000000,
}))

// Collaboration request with async reply
::bus/collaborate(CollaborationRequest({
  session: chat, sender: alice,
  from-agent: "brain",
  question: "What's alice's last login?",
  reply-event: "brain:reply:42",
  correlation-id: "42",
}))
```

### `::ai::media` — Multi-modal media types

Unified types for AI-generated images, audio, and video (independent of provider).

## Testing

The package ships with 54 unit tests covering session, message, chat, memory (pure), RAG (pure), context, and bus type construction.

```bash
hot test --project pkg-test-hot-ai
```

Memory and RAG paths that touch persistent stores are exercised by running the demo agent (see `hot/src/team-agent`) rather than by unit tests.

## License

Apache-2.0 - see [LICENSE](LICENSE).
