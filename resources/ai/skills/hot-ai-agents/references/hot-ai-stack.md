# Hot AI Stack

## Contents

- [Package roles](#package-roles)
- [Project dependencies](#project-dependencies)
- [Agent declaration and runtime](#agent-declaration-and-runtime)
- [Typed tools](#typed-tools)
- [Provider-neutral chat loops](#provider-neutral-chat-loops)
- [Memory-grounded chat turns](#memory-grounded-chat-turns)
- [Prompt skills](#prompt-skills)
- [Sessions, identity, and request context](#sessions-identity-and-request-context)
- [Streams and structured events](#streams-and-structured-events)
- [One-shot and multimodal calls](#one-shot-and-multimodal-calls)

## Package Roles

Use the packages at their intended layer:

| Layer | Package | Owns |
| --- | --- | --- |
| AI primitives | `hot.dev/hot-ai` | normalized chat messages, tools, tool loops, prompt skills, sessions, identity, memory, RAG, context budgets, media, inter-agent messages |
| Agent harness | `hot.dev/hot-ai-agent` | runtime stores, transports, commands, request binding, chat-turn orchestration, streams, auth, attachments, callbacks, lifecycle jobs, notifications |
| Provider adapter | `hot.dev/anthropic`, `hot.dev/openai`, `hot.dev/gemini`, and peers | provider request/response translation and model-specific capabilities |
| Application | the agent project | domain types, prompts, policies, approval, event topology, canonical state, provider selection |

Do not push application policy into `hot-ai`. In particular, tool effects,
authorization, spending consent, retries, and user-visible state belong to the
application or harness.

## Project Dependencies

Use published, versioned packages from the `hot.dev` registry. A compatible
current set is:

```hot
hot.project.my-agent.deps {
    "hot.dev/hot-ai":       "1.8.1",
    "hot.dev/hot-ai-agent": "1.4.0",
    "hot.dev/anthropic":    "1.2.4",
}
```

Preserve a project's existing pins unless an upgrade is requested. Declare
both AI packages when importing both namespace families; provider packages are
optional and should match the selected provider.

Run `hot deps show` after resolution. It prints the exact package path selected
by the installed Hot executable. If this reference does not cover an API,
inspect the `pkg.hot`, README, declarations, or source at that resolved path.
Do not emit local-filesystem dependency specs or paths into a Hot source
checkout for an ordinary application.

## Agent Declaration and Runtime

Declare a typed agent identity and attach the type to each public handler:

```hot
::support ns

::runtime ::ai::agent::runtime

SupportAgent
meta {
    agent: {
        name: "Support",
        description: "Answers support questions with approved tools.",
        tags: ["support", "agent"],
    },
}
type {
    model: Str,
}

AGENT_NAME "support"
agent SupportAgent({model: "claude-sonnet-4-5"})
rt ::runtime/create-runtime(AGENT_NAME, {
    store-prefix: "support",
    max-errors: 10,
})

ask
meta {
    agent: SupportAgent,
    on-event: "support:ask",
}
fn (event) {
    // Normalize session and identity, register the session, then dispatch.
}
```

Use `create-runtime` for standard state, stats, error, and notification stores.
Call `register-session` from inbound handlers. If sibling namespaces need the
same runtime or store, export an accessor function rather than importing a
top-level value; cross-namespace value references can re-evaluate the defining
expression in the consumer environment.

## Typed Tools

Define the callable once as a typed Hot function:

```hot
::tool ::ai::tool

lookup-order
meta {doc: "Look up one order visible to the active customer."}
fn (order-id: Str): Map {
    // Authorize against request-bound identity before reading.
    {id: order-id, status: "processing"}
}

tools fn (): Vec<::tool/Tool> {
    [
        ::tool/from-fn(lookup-order, {
            name: "lookup-order",
            description: "Look up an order by id.",
        }),
    ]
}
```

`from-fn` derives JSON Schema from the typed signature and documentation.
Use a prebuilt `Tool` with `pass-input: true` only when an external schema
intentionally supplies one input map, as with an MCP wrapper.

Keep the following outside the core `Tool`:

- read-only/effectful/per-call classification;
- authorization and tenant scoping;
- approval before effects;
- redaction before logging or approval;
- stable idempotency keys for effectful dispatch;
- artifact storage for oversized results.

Fail closed: an unknown or failed effect classifier must require approval.

## Provider-Neutral Chat Loops

Use the normalized tools-aware contract:

```hot
(model: Str,
 messages: Vec<::ai::chat/Message>,
 system: Str?,
 tools: Vec<::ai::tool/Tool>?): ::ai::chat/ChatReply
```

For streaming, add `on-delta: Fn` and emit normalized
`::ai::chat/ReplyDelta` values.

```hot
::chat ::ai::chat
::anth ::anthropic::chat-tools

opts ::chat/ChatOptions({
    chat-fn: ::anth/chat-with-tools,
    chat-stream-fn: ::anth/chat-with-tools-stream,
    model: "claude-sonnet-4-5",
    system: "Answer only from tool evidence.",
    tools: tools(),
    max-iterations: 8,
    emit-steps: true,
})

answer ::chat/run-loop-stream(opts, question, on-delta)
```

Use `run-loop` for non-streaming replies and `run-loop-messages` when resuming
an explicit normalized history. The loop dispatches every tool call, appends
assistant and tool-result turns, and fails if it reaches the iteration cap.

Do not branch application code on native provider response blocks. Normalize
to `ChatReply`, `ToolCall`, `ToolResult`, and `ReplyDelta` in the provider
adapter.

## Memory-Grounded Chat Turns

Prefer `::ai::agent::chat-turn/run-chat-turn` for ordinary conversational
agents:

```hot
::chat-turn ::ai::agent::chat-turn

cfg ::chat-turn/TurnConfig({
    rt: rt,
    agent-name: AGENT_NAME,
    stream-label: AGENT_NAME,
    chat-opts: opts,
    system-base: "You are a concise support agent.",
    memory-opts: null,
})

result ::chat-turn/run-chat-turn(cfg, ::chat-turn/TurnInput({
    session,
    sender,
    text: question,
    attachments: [],
    payload: {session_id: session.id, message_id: message-id},
    ctx-extras: {tenant_id: tenant-id},
    fallback-text: null,
}))
```

The helper intentionally:

1. retrieves prior memory before storing the new user text;
2. persists the user turn;
3. binds session and identity for statically registered tools;
4. streams the reply;
5. persists the assistant turn.

Preserve that ordering. Persisting the user turn before retrieval can make the
new message appear as its own supporting memory.

Use `fallback-text` for an explicit deterministic no-provider path. Do not
silently present provider failure as model output.

## Prompt Skills

Use `::ai::skill/Skill` for prompt guidance selected by the model. Use
`::ai::tool/Tool` for executable capabilities.

```hot
refund-policy
meta {
    skill: {
        description: "Apply the refund policy to a customer reply.",
        when: ["refund", "return"],
        body: "Cite the policy window and identify any required approval.",
    },
}
fn () { {} }

opts ::chat/ChatOptions({
    chat-stream-fn: provider-stream-fn,
    model: model,
    skills: [refund-policy],
})
```

The chat loop adds `list_skills`, `read_skill`, and `apply_skill` when a skill
resolver is present. Use `skill-context` for harness-owned data such as
attachments or tenant identity; harness values override model-supplied keys.

For Markdown-authored `*.skill.md` resources, enable skill codegen, include
the generated source root, include the resource path, and depend on
`hot.dev/hot-ai`. Never hand-edit generated `.skill.hot` files.

## Sessions, Identity, and Request Context

Use stable, transport-prefixed ids:

```hot
session ::ai::session/Session({
    id: `web:chat:${chat-id}`,
    meta: {transport: "web"},
})
sender ::ai::session/Identity({
    id: `web:user:${user-id}`,
    name: user-name,
})
```

Choose the storage scope deliberately:

- `session-key`: shared conversation state;
- `user-key`: data that follows a person across every session;
- `session-user-key`: one person in one conversation;
- `agent-key`: shared agent state.

Call `::ai::agent::request/bind(session, sender, extras)` before model tools
run. Tools can then recover the active caller without accepting spoofable
identity fields from the model.

## Streams and Structured Events

Use `::ai::agent::stream` for stable labels and safe emission:

```hot
::stream/emit-reply-start(AGENT_NAME, payload)
::stream/emit-reply-delta(AGENT_NAME, {...payload, delta})
::stream/emit-reply-end(AGENT_NAME, {...payload, text})

::stream/emit(::stream/label(AGENT_NAME, "state", "updated"), {
    session_id: session.id,
    state,
})
```

Reply text is only one projection. Emit structured state, progress, sources,
approval requests, and artifacts as separate events so clients never need to
parse domain state back out of prose.

## One-Shot and Multimodal Calls

Do not force every AI call through a conversational tool loop. For a typed
visual analysis, transcription, or JSON extraction:

1. construct the provider's typed request;
2. request a constrained schema where supported;
3. parse and validate the response;
4. normalize it to an application type;
5. record provider, model, usage, and raw failure details;
6. persist the normalized observation before treating it as canonical.

Keep prompts in versioned Hot resources or source near the handler. Resolve
tenant or deployment overrides explicitly and retain prompt provenance.
