# hot-ai

Dependency-free building blocks for AI agents in Hot. Multi-tenant sessions, typed messages, scoped memory, RAG, context management, and inter-agent communication — all without depending on any specific AI provider package.

Provider-specific code (Anthropic, OpenAI, xAI, Gemini) stays in the user's project; `hot-ai` calls it through a `chat-fn` parameter.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/hot-ai": "1.10.0"
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

### `::ai::skill` — Prompt skills (`SKILL.md` & Hot skills)

Skills are named prompt augmentations that an agent can expose to the
model through `list_skills`, `read_skill`, and `apply_skill` (plus
`search_skills` when searchable, and `list_skill_resources` /
`read_skill_resource` for bundled files).

#### Loading standard `SKILL.md` directories (recommended)

Mount skill directories (`<name>/SKILL.md` with optional `references/`,
`examples/`, `assets/`) in `hot.project.<name>.resources.paths` (or
pass `--resource.path` from the CLI) and load them at runtime:

```hot
::skill ::ai::skill

// Load a single skill (sets resource-root to "skills/invoice-parser")
invoice ::skill/from-resource("skills/invoice-parser/SKILL.md")

// Discover all SKILL.md (and *.skill.md) under an explicit prefix
resolver ::skill/resource-resolver("skills", {
  searchable: false,
  max-resource-chars: 250000,
  // Optional gate (filtered paths report "Resource not found"):
  // resource-filter: (skill, rel-path) { not(starts-with(rel-path, "examples/")) },
})

opts ::ai::chat/ChatOptions({
  chat-fn: provider-chat-fn,
  model: model,
  skill-resolver: resolver,
})
```

- **Portable frontmatter**: standard [agentskills.io](https://agentskills.io)
  top-level keys (`name`, `description`, `license`, `compatibility`,
  `metadata`, `allowed-tools`) map directly onto `Skill`. Hot extensions
  (`when`, `modalities`, `requires`) are read from top-level keys first,
  falling back to `metadata.hot-when`, `metadata.hot-modalities`, and
  `metadata.hot-requires` (comma-separated string or YAML list) so files
  can pass strict `skills-ref` validation. Unrecognized top-level keys
  are preserved in `Skill.extra` rather than `metadata`.
- **Typed fields**: `allowed-tools` accepts either the spec's
  space-separated string (paren-aware, e.g. `"Read Glob Bash(git add:*)"`)
  or `Vec<Str>`; call `::skill/allowed-tools-of(skill)` to normalize.
  `license`, `compatibility`, `resource-root` require `Str?`, and
  `metadata` / `extra` require `Map?`.
- **Tier-3 bundled files**: resolvers with `list-resources-fn` /
  `read-resource-fn` expose `list_skill_resources(name)` (returns
  `[{path, size}]`) and `read_skill_resource(name, path, offset?)`
  (capped at 250,000 characters per read with a newline-aligned
  continuation hint). Dotfiles, path traversal, `SKILL.md`, and nested
  skill directories are excluded. A `SKILL.md` at the top of a resource
  root has no directory of its own, so it exposes no bundled files; give
  each skill its own directory. Bundled scripts are never executed on
  workers; read them as text or pass their `::hot::resource/path` URI to
  `::hot::box`.
- **Package-bundled skills**: project `resources.paths` shadow
  dependency `resource-paths` per file. Always load from an explicit
  prefix (`from-resources("my-skills")`) rather than `""` so dependency
  skills cannot enter the prompt unintentionally; ship package skills
  under a package-named subdirectory.

#### Inline & function-backed skills

Prompt-only skills are plain `Skill` values, and function-backed skills
use `meta {skill: ...}` with `::skill/from-fn(fn-ref)`:

```hot
::skill ::ai::skill

refund-tone ::skill/Skill({
  name: "refund-tone",
  description: "How to answer refund requests",
  when: ["refund", "return policy"],
  body: "Be concise, empathetic, and cite the policy window.",
})
```

When a skill declares `tools` or `requires`, `::ai::chat/run-loop` and
`run-loop-stream` activate those bundled tools once the model calls
`read_skill` or `apply_skill` (seeding from assistant history on
`run-loop-messages`, never overriding host tools or built-ins, and
publishing `ai:skill:activate` when `emit-steps: true`). Skills with
non-empty `requires` also receive a trailing hint line naming their
required skills. Note that activating tools changes the request prefix
and rebuilds provider prompt caches once per activation that adds tools.

Static stub codegen for `*.skill.md` files remains supported as an
optional workflow (requires `"hot.dev/hot-ai"` in `deps`).

### Per-call provider credentials

`ChatOptions.provider-opts` carries a provider's `opts` (`api-key`, `base-url`, and so on) through `run-loop` and `run-loop-stream`, so one run can serve many tenants without touching ctx. It reaches the function in the `chat-opts-fn` or `chat-stream-opts-fn` slot, on `ChatOptions` or on a `ProviderRegistration`:

```hot
opts ::ai::chat/ChatOptions({
    chat-opts-fn: ::anthropic::chat-tools/chat-with-tools,
    model: "claude-sonnet-4-5",
    provider-opts: {api-key: tenant-key}
})

answer ::ai::chat/run-loop(opts, "What is 17 + 25?")
```

- The contracts are `(model, messages, system, tools, provider-opts)` and `(model, messages, system, tools, on-delta, provider-opts)`.
- A plain `chat-fn` or `chat-stream-fn` keeps working without `provider-opts`, but can't receive it: setting `provider-opts` for one is a failure naming the provider.
- A null or blank credential in `provider-opts` is a failure, never a fallback to ctx. Leave the key out to use ctx.
- A `base-url` in `provider-opts` needs a credential there too, and `management-base-url` needs `management-api-key`: a ctx key is never sent to a per-call URL.
- `::ai::rag` uses `chat-opts-fn` too. `count-tokens-fn` and a store's `embed-fn` take no `provider-opts`: bind a per-call key in a closure.
- The run loop keeps `provider-opts` out of chat history, step and delta stream records, skill context and failure payloads, and masks each credential in it in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before the run loop may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

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
