---
name: hot-ai-agents
description: Build, review, and integrate production-quality AI agents on Hot using hot-ai, hot-ai-agent, provider packages, event handlers, streams, stores, tools, skills, memory, and the official Hot SDKs. Use for conversational agents, tool loops, durable event-driven workflows, multimodal analysis agents, browser or service clients, and upgrades of existing Hot agent code to current public v2 APIs.
---

# Hot AI Agents

Build agents as durable Hot applications, not as a single prompt wrapped in an
HTTP handler. Keep domain policy, provider adapters, runtime state, and client
transport as explicit layers.

Use the `hot-language` skill and the project's `AGENTS.md` whenever editing
`.hot` files. Hot syntax and error propagation differ materially from
conventional languages.

## Establish the Installed Surface

Inspect the installed project surface in this order before changing an agent:

1. Read every applicable `AGENTS.md`.
2. Read the app's `hot.hot`, client dependency manifest, and lockfile.
3. Preserve the explicit registry versions in
   `hot.project.<name>.deps` unless the task includes an upgrade.
4. Run `hot deps show` to locate the exact versioned packages resolved by the
   installed Hot executable. Inspect their shipped `pkg.hot`, README, or source
   only when an API detail is not covered by this skill.
5. Inspect the installed SDK version and its shipped declarations or API docs
   for the target language.
6. Treat unversioned examples and search results as secondary evidence; they
   can lag the installed product.

Do not assume the Hot source repository or sibling SDK repositories exist.
Never replace a registry dependency with a local path merely to inspect it.
Do not guess provider capabilities, package versions, event labels, or SDK
method names from memory.

## Choose the Agent Shape

Select the smallest architecture that preserves the required behavior:

- Use a typed, provider-specific request for a one-shot transformation such as
  multimodal classification or transcription.
- Use `::ai::chat/run-loop` or `run-loop-stream` for a conversational tool
  loop.
- Use `::ai::agent::chat-turn/run-chat-turn` for the standard
  memory-grounded, streaming conversation lifecycle.
- Use multiple event handlers for durable, pausable, independently retryable
  workflow stages.
- Use typed ports around provider calls when the workflow must be testable
  offline or compare multiple providers.

Read [references/hot-ai-stack.md](references/hot-ai-stack.md) before creating
or changing Hot-side agent code.

## Design the Boundaries

Keep these boundaries visible:

1. Define domain types, invariants, and event payloads independently of a
   provider.
2. Normalize provider results at one adapter boundary.
3. Derive model-facing tool schemas from typed Hot functions with
   `::ai::tool/from-fn`.
4. Add effect classification, approval, redaction, and idempotency in the
   application layer; these are domain policy, not provider concerns.
5. Store canonical state durably. Treat reply deltas as presentation, not
   state.
6. Emit stable, structured stream events for clients.
7. Keep API keys and provider secrets on the server or in Hot context.

Read [references/production-patterns.md](references/production-patterns.md)
for durable workflow, safety, streaming, and testing patterns.

## Integrate Clients

Use an official SDK for authenticated Hot API work. Keep browser traffic behind
a backend-for-frontend route.

- Use atomic subscribe-with-event for the first event so publishing cannot
  race subscription setup.
- For a one-run request, consume the SDK's subscribe-with-event iterator.
- For a multi-turn UI, allocate one stream id, keep one subscription, publish
  later turns into that stream, and reconnect to the same stream when the
  connection closes.
- Rebuild UI state from durable storage or structured state snapshots because
  stream reattachment does not replay missed `stream:data` frames.
- Preserve Hot API wire keys such as `event_type`, `event_data`, and
  `stream_id`; SDK-only options use the target language's naming convention.

Read [references/sdk-clients.md](references/sdk-clients.md) whenever adding or
reviewing a JavaScript/TypeScript, Python, Go, Rust, or Java client.

## Validate

Validate in layers:

1. Run `hot check` for the owning project.
2. Run focused Hot tests for the package or app.
3. Test provider adapters with deterministic fixtures before credentialed
   integration tests.
4. Test duplicate delivery, retry, partial failure, approval waits, and
   reconnect behavior.
5. Run the target SDK's typecheck and test commands.
6. Verify the browser bundle contains no Hot API key or provider secret.

Use [examples/agent.hot](examples/agent.hot) as a compact, offline-checkable
example of an agent declaration, typed tool, normalized chat loop, event
handler, runtime registration, and agent-scoped reply stream.
