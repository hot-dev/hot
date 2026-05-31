---
description: "Build AI agents with Hot metadata, handlers, tools, memory patterns, commands, graph inspection, and cloud deployment."
---

# Agents

Agents are typed groups of event handlers, schedules, and webhooks that share identity. An agent is defined as a Hot type with `agent` metadata, and functions declare membership via `meta {agent: TypeName}`. When deployed, Hot tracks agent runs, surfaces health metrics, and groups observability data by agent.

Agents group handlers by type reference, giving you compile-time validation and structured config fields.

For a complete runnable example, see the [Hot Chat
demo](/docs/demos/hot-chat) — one Hot project that boots two AI agents
(session-first Team Mode and identity-first Personal Mode) behind a
polished Next.js client, over the same typed-event wire contract a Slack
or Telegram adapter would use.

## Defining an Agent

An agent starts with a type definition that has `agent` in its metadata. The type's struct fields become the agent's configuration, and the `doc` or `agent.description` provides a human-readable summary.

### Basic Example

```hot
::myapp::support ns

SupportAgent meta {
  doc: """AI-powered customer support agent""",
  agent: {
    name: "Support Agent",
    tags: ["support", "ai"],
  },
}
type {
  model: Str,
  system: Str,
  escalation-channel: Str,
}
```

This registers `SupportAgent` as an agent. The type name is the identifier; `name` is a display label for the [Hot App](/docs/app#agents).

### Full Example

```hot
::acme::support ns

::store ::hot::store
::ctx ::hot::ctx

EmbeddingOptions ::store/EmbeddingOptions

SupportAgent meta {
  doc: """
    Customer support agent that responds to tickets,
    searches a knowledge base, escalates when uncertain,
    and reviews interactions daily.
  """,
  agent: {
    name: "Support Agent",
    description: "AI-powered support with semantic KB search and escalation",
    tags: ["support", "ai", "customer-facing"],
  },
}
type {
  model: Str,
  system: Str,
  escalation-channel: Str,
  tone: Str,
}

support-agent SupportAgent({
  model: ::ctx/get("support.model", "claude-sonnet"),
  system: ::ctx/get("support.system", "You are a helpful support agent."),
  escalation-channel: ::ctx/get("support.escalation", "#support"),
  tone: ::ctx/get("support.tone", "professional"),
})

// Shared knowledge base (static name, safe at namespace level)
kb ::store/Map({name: "support:kb", embedding: EmbeddingOptions.Default})

on-ticket meta {agent: SupportAgent, on-event: "support:ticket"}
fn (event) {
  // Per-stream memory (needs event.stream-id, so created inside the handler)
  memory ::store/Map({name: `support:${event.stream-id}`, embedding: EmbeddingOptions.Default})
  context ::store/search(kb, event.data.message, {limit: 5})
  history ::store/search(memory, event.data.message, {limit: 10})
  response generate-reply(support-agent, context, history, event.data.message)
  ::store/put(memory, Uuid(), {role: "assistant", content: response})
  send("support:response", {ticket-id: event.data.ticket-id, response: response})
}

on-feedback meta {agent: SupportAgent, on-event: "support:feedback"}
fn (event) {
  memory ::store/Map({name: `support:${event.stream-id}`, embedding: EmbeddingOptions.Default})
  ::store/put(memory, Uuid(), {type: "feedback", rating: event.data.rating})
}

daily-review meta {agent: SupportAgent, schedule: "0 9 * * 1-5"}
fn () {
  summarize-yesterday(kb)
}

on-escalation meta {agent: SupportAgent, on-event: "support:escalate"}
fn (event) {
  ::hot::slack/post-message(support-agent.escalation-channel, `Needs help: ${event.data.reason}`)
}
```

This defines one agent with four handlers: two event-driven, one scheduled, and one for escalation. All share the `support-agent` instance for configuration and `::hot::store` maps for memory.

## Agent Metadata

The `agent` key in the type's metadata is a map with the following fields:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | No | Display name for the agent. Falls back to the type name (e.g., `SupportAgent`). |
| `description` | No | Short description for the agent. Falls back to the top-level `doc` metadata. |
| `tags` | No | List of strings for categorization and filtering in the Hot App. |

The top-level `doc` metadata serves as the default description. If both `doc` and `agent.description` are present, `agent.description` takes priority in agent-specific contexts (the App dashboard, API responses).

## Grouping Handlers

Functions declare membership in an agent via `meta {agent: TypeName}`. This works with all handler types:

### Event Handlers

```hot
on-ticket meta {agent: SupportAgent, on-event: "support:ticket"}
fn (event) {
  process-ticket(event.data)
}
```

### Scheduled Functions

```hot
daily-review meta {agent: SupportAgent, schedule: "0 9 * * 1-5"}
fn () {
  review-interactions()
}
```

### Webhooks

```hot
on-stripe-payment meta {
  agent: BillingAgent,
  webhook: {service: "billing", path: "/stripe"},
}
fn (request) {
  process-payment(or(request.body, request.data))
  {ok: true}
}
```

The `agent` reference is a type name, not a string. The compiler resolves it to the agent type, catching typos at compile time. A single agent can have any number of handlers across event handlers, schedules, and webhooks.

## Agents vs Workflows

Agents and workflows are related but separate:

- **Agents** describe actor/runtime identity. They own config fields, runtime attribution, health metrics, runs, streams, and memory patterns.
- **Workflows** describe process topology. They group handlers, events, schedules, webhooks, and MCP tools into a named flow, but a workflow definition is not required for Hot to discover the flow.

Named workflows use typed definitions, similar to agents:

```hot
LeadQualification meta {
  doc: "Scores inbound leads and routes sales or nurture outcomes",
  workflow: {
    name: "Lead Qualification",
    tags: ["sales", "ai"],
  },
}
type {}
```

Handlers can opt into one or more named workflows:

```hot
qualify-lead meta {
  agent: LeadQualifier,
  workflows: [LeadQualification],
  on-event: "lead:new",
}
fn (event) {
  score enrich-and-score(event.data)
  send("lead:qualified", merge(event.data, {score: score}))
}
```

If a handler has no `agent` or `workflow` metadata, Hot still records its triggers and sends. The Hot App can show these as unnamed project-level workflows in the environment-wide workflow graph. This keeps observability complete while letting you add names only where they are useful.

## Event Sends

When a handler calls `send("event-name", data)`, the compiler detects this statically and records the event name in the handler's metadata. This powers the [Agent Graph](#agent-graph) — sends appear as outgoing edges from the handler to the target event type — and the [code documentation generator](/docs/app#docs), which shows sends on every documented function.

### Automatic Detection

Static send extraction works out of the box. The compiler scans function bodies for `send()` calls and resolves the event name from literal strings or namespace-level constants:

```hot
on-order meta {agent: OrderAgent, on-event: "order:created"}
fn (event) {
  validate(event.data)
  send("inventory:reserve", event.data)
  send("audit:log", {action: "order-created", order-id: event.data.id})
}
```

After compilation, this handler's metadata will include `sends: ["inventory:reserve", "audit:log"]` automatically. No annotation is needed.

Variable references are resolved when the value is a string constant in the same namespace:

```hot
inventory-event "inventory:reserve"

on-order meta {agent: OrderAgent, on-event: "order:created"}
fn (event) {
  send(inventory-event, event.data)
}
```

Dynamic event names (e.g., `send(event.type, data)`) cannot be resolved statically and are silently skipped.

### Manual `sends` Declarations

You can declare sends explicitly in `meta` to document events that are dynamically generated, or to add descriptions:

```hot
on-order meta {
  agent: OrderAgent,
  on-event: "order:created",
  sends: ["inventory:reserve", "audit:log"],
}
fn (event) {
  send("inventory:reserve", event.data)
  send("audit:log", {action: "order-created"})
}
```

Entries can be strings or rich objects with a `doc` field for documentation:

```hot
on-order meta {
  agent: OrderAgent,
  on-event: "order:created",
  sends: [
    {event: "inventory:reserve", doc: "Reserve stock for the ordered items"},
    {event: "audit:log", doc: "Record order creation for compliance"},
  ],
}
fn (event) {
  send("inventory:reserve", event.data)
  send("audit:log", {action: "order-created"})
}
```

Rich object descriptions appear in the inspector panel beneath each send edge in the [Agent Graph](#agent-graph).

### Merge Behavior

Manual and static sends are merged together:

- Existing manual `sends` entries are preserved as-is (including rich objects with `doc`)
- Statically detected event names are added alongside, deduplicated
- For the same event name, the manual declaration takes precedence

This means you can rely on automatic detection for most cases and add manual declarations only when you need richer metadata or have dynamically generated event names.

### Non-Agent Functions

Send extraction works for **all** functions, not just agent-tagged handlers. If a standalone event handler or scheduled function calls `send()`, the event names are recorded in its metadata. The [code documentation generator](/docs/app#docs) surfaces sends on every documented function as a badge and detail listing.

## Config Fields

The agent type's struct fields define its configuration. These are the per-deployment knobs — model selection, system prompts, channel names, thresholds.

```hot
SupportAgent meta {
  agent: {name: "Support Agent"},
}
type {
  model: Str,
  system: Str,
  escalation-channel: Str,
  confidence-threshold: Dec,
}
```

Create an instance using [context variables](/docs/app#context-variables) for environment-specific values:

```hot
support-agent SupportAgent({
  model: ::ctx/get("support.model", "claude-sonnet"),
  system: ::ctx/get("support.system", "You are a helpful support agent."),
  escalation-channel: ::ctx/get("support.escalation", "#support"),
  confidence-threshold: Dec(::ctx/get("support.threshold", "0.85")),
})
```

Handlers reference the instance directly — `support-agent.model`, `support-agent.escalation-channel`. Config fields are visible in the Agent Dashboard's Overview tab.

## Agent Runs

When a handler with `meta {agent: TypeName}` executes, the run is automatically tagged with the agent's qualified name (e.g., `::acme::support/SupportAgent`). This tagging enables:

- **Filtering** — view runs by agent in the Hot App
- **Metrics** — per-agent success rate, average duration, and run count
- **Health monitoring** — the Dashboard shows agent health with color-coded indicators
- **Attribution** — trace any run back to the agent that produced it

No additional code is needed. The agent tagging happens at the runtime level when a handler declares `meta {agent: ...}`.

## Agent Skills

Agents that use `hot.dev/hot-ai` can expose prompt skills to the model. A skill is a named instruction bundle that the chat run loop advertises through `list_skills`, `read_skill`, and `apply_skill`.

Prompt-only skills are `::ai::skill/Skill` values:

```hot
::skill ::ai::skill

support-tone ::skill/Skill({
  name: "support-tone",
  description: "How to answer customer support questions",
  when: ["support reply", "refund", "angry customer"],
  body: """
  Be concise, empathetic, and concrete. Cite the policy or next step.
  """,
})
```

Function-backed skills are also supported with `meta {skill: ...}` and `::skill/from-fn`, but those functions are metadata sources. The built-in skill tools read `body` or call `body-fn`; they do not invoke the skill function itself.

Markdown-authored skills live in project resources as `*.skill.md` files. Skill codegen turns them into generated `.skill.hot` files that export `Skill` values, so they can be listed directly in `::skill/for-agent([...])`. Because generated skills construct `::ai::skill/Skill`, projects with Markdown skill resources must declare a `hot.dev/hot-ai` dependency:

```hot
hot.project.support.deps {
  "hot.dev/hot-ai": {}
}
```

## Agent Memory

Agents use [`::hot::store`](/pkg/hot.dev/hot-std/hot/store) for persistent memory. Store maps support optional embedding-based semantic search, which is useful for knowledge bases and conversation history.

Store data is scoped to the current Hot organization and environment and is
stored in the main Hot database. This means agent memory works in project,
worker, and deployed runtime contexts; it is not a standalone filesystem-backed
mode like `::hot::file` direct access.

### Per-Stream Memory

Each stream can have its own memory, isolated by stream ID. Since the map name depends on the stream ID, create it inside the handler where `event` is available:

```hot
EmbeddingOptions ::hot::store/EmbeddingOptions

on-ticket meta {agent: SupportAgent, on-event: "support:ticket"}
fn (event) {
  memory ::store/Map({name: `support:${event.stream-id}`, embedding: EmbeddingOptions.Default})

  history ::store/search(memory, event.data.message, {limit: 10})
  ::store/put(memory, Uuid(), {role: "user", content: event.data.message})

  response generate-reply(support-agent, history, event.data.message)
  ::store/put(memory, Uuid(), {role: "assistant", content: response})
}
```

### Shared Knowledge Base

A knowledge base shared across all streams uses a static name, so the map definition goes at the namespace level. Handlers search it; a separate handler or schedule populates it:

```hot
EmbeddingOptions ::hot::store/EmbeddingOptions

kb ::store/Map({name: "support:kb", embedding: EmbeddingOptions.Default})

on-ticket meta {agent: SupportAgent, on-event: "support:ticket"}
fn (event) {
  context ::store/search(kb, event.data.message, {limit: 5})
}

seed-kb meta {agent: SupportAgent, on-event: "kb:seed"}
fn (event) {
  ::store/put-many(kb, {
    "returns": {title: "Returns", content: "Refunds available within 30 days of purchase."},
    "shipping": {title: "Shipping", content: "Free shipping on orders over $50."},
  })
}
```

### Plain Key-Value State

Not all agent state needs embeddings. Use plain maps for counters, flags, and structured data:

```hot
counters ::store/Map({name: "support:counters"})

on-ticket meta {agent: SupportAgent, on-event: "support:ticket"}
fn (event) {
  total or(::store/get(counters, "total-tickets"), 0)
  ::store/put(counters, "total-tickets", add(total, 1))
}
```

## Lifecycle

Agents are metadata-driven — they're discovered from your source code and registered automatically:

1. **Define** — Add `agent` metadata to a type and `meta {agent: TypeName}` to handler functions
2. **Deploy** — Run `hot deploy` (or `hot dev` for local development). The compiler scans types for `agent` metadata and registers agent definitions.
3. **Execute** — When events arrive, schedules fire, or webhooks receive requests, handlers run with agent attribution. Each run is tagged with the agent's qualified name.
4. **Observe** — View agent health, metrics, handlers, and runs in the [Hot App](/docs/app#agents)

When you redeploy, agent definitions are updated automatically. If a type's `agent` metadata is removed, the agent is unregistered. If handler functions remove their `agent` reference, those handlers still execute but are no longer attributed to the agent.

## Handler Documentation

Add a `doc` field to any handler's metadata to provide a description. This description appears in the Agent Graph inspector panel when you click a handler node:

```hot
nurture-lead
meta {
  doc: "Nurtures leads by sending a welcome email and re-queueing for scoring",
  agent: LeadQualifier,
  on-event: "lead:nurture",
  sends: [
    {event: "email:send", doc: "Send welcome drip email"},
    {event: "lead:new", doc: "Re-queue for scoring after nurture delay"},
  ],
}
fn (event) {
  send-drip-email(event.data.email)
  send("lead:new", event.data)
}
```

The `doc` field works on all handler types: event handlers, scheduled functions, webhooks, and MCP tools. For MCP tools, the MCP `description` field takes priority; `doc` is used as a fallback when no MCP description is provided.

## Viewing Agents in the App

The Hot App provides dedicated views for agents. See [Hot App > Agents](/docs/app#agents) for details.

### Agents List

The **Agents** page shows all deployed agents as a card grid. Each card displays the agent name, namespace, description, tags, handler count, and project. Use the search bar to filter by name, namespace, or project. A topology graph spanning all agents is available at the top of the page.

### Agent Dashboard

Click an agent to open its dashboard. The default view is the **Graph** tab, which shows the agent's topology:

- **Graph tab** — Interactive topology graph showing the agent's handlers, triggers (events, schedules, webhooks, MCP tools), and event sends as a directed graph. Click any node to open the inspector panel with details. Toggle between horizontal and vertical layouts using the direction buttons in the toolbar.
- **Handlers tab** — All event handlers, schedules, and webhooks linked to this agent with trigger details, retry config, and source locations
- **Runs tab** — Paginated run history filtered to this agent
- **Streams tab** — Streams where this agent participated

### Agent Graph

The agent graph visualizes the flow of data through an agent. Nodes represent two categories:

- **Functions** (blue icons) — Event handlers, scheduled functions, webhook handlers, and MCP tool handlers. Each shows the function name and agent membership.
- **Triggers** (green icons) — Events, schedules, webhooks, and MCP tools that invoke functions. Each uses a distinct icon to indicate its type.

Edges show the relationships: triggers connect to the handlers they invoke, and handlers connect to the events they send.

**Inspector panel** — Click any node to open the inspector sidebar, which shows the node's full details: namespace, source file location, retry configuration, description (from `doc` metadata), handled events, and sent events. For send edges with `doc` annotations, the description appears below the event name. Toggle the inspector with the panel icon in the toolbar.

**Layout** — Switch between horizontal (left-to-right) and vertical (top-to-bottom) layouts using the direction toggle in the toolbar.

**Download** — Export the graph as a PNG image using the download button.

**Zoom** — Use the zoom slider or mouse wheel to zoom in and out on large graphs.

### Dashboard Health Widget

The main Dashboard includes an **Agent Health** widget showing each deployed agent with a health indicator:

- **Green dot** — 95%+ success rate
- **Yellow dot** — 80–95% success rate
- **Red dot** — Below 80% success rate

The widget also shows agent vs. non-agent run counts, giving you a quick sense of how much of your workload is agent-driven.

## Patterns

### Event-Driven Agent

The most common pattern. The agent responds to external events:

```hot
InboxTriager meta {agent: {name: "Inbox Triager", tags: ["email"]}}
type { rules: Vec }

on-email meta {agent: InboxTriager, on-event: "email:received"}
fn (event) {
  classify-and-route(event.data)
}
```

### Scheduled Agent

An agent that runs on a schedule:

```hot
DailyBriefing meta {agent: {name: "Daily Briefing", tags: ["reporting"]}}
type { sources: Vec, channel: Str }

briefing DailyBriefing({
  sources: ["github", "stripe", "analytics"],
  channel: ::ctx/get("briefing.channel", "#general"),
})

morning-report meta {agent: DailyBriefing, schedule: "0 8 * * 1-5"}
fn () {
  data aggregate-sources(briefing.sources)
  summary generate-summary(data)
  send-to-channel(briefing.channel, summary)
}
```

### Hybrid Agent

Combines events, schedules, and webhooks:

```hot
LeadQualifier meta {
  agent: {name: "Lead Qualifier", tags: ["sales", "ai"]},
}
type { model: Str, threshold: Dec, crm-key: Str }

qualifier LeadQualifier({
  model: ::ctx/get("leads.model", "claude-sonnet"),
  threshold: Dec(::ctx/get("leads.threshold", "0.7")),
  crm-key: ::ctx/get("leads.crm-key"),
})

on-signup meta {agent: LeadQualifier, webhook: {service: "leads", path: "/signup"}}
fn (request) {
  send("lead:new", or(request.body, request.data))
  {ok: true}
}

qualify-lead meta {agent: LeadQualifier, on-event: "lead:new"}
fn (event) {
  score enrich-and-score(event.data, qualifier.model)
  if(gte(score, qualifier.threshold),
    send("lead:qualified", merge(event.data, {score: score})),
    send("lead:nurture", merge(event.data, {score: score})))
}

weekly-pipeline meta {agent: LeadQualifier, schedule: "0 9 * * 1"}
fn () {
  generate-pipeline-report()
}
```

## Best Practices

**Name agents by domain responsibility.** An agent should own a coherent area of functionality. `SupportAgent`, `BillingAgent`, and `LeadQualifier` are clear; `UtilityAgent` or `MainAgent` are not.

**Keep handler count focused.** Each agent should have a small number of handlers with a clear purpose. If an agent has more than 8–10 handlers, consider splitting it into separate agents.

**Use config fields for tunable parameters.** Model names, thresholds, channel names, and system prompts belong in config fields. This makes agents reusable across environments without code changes.

**Use tags for categorization.** Tags like `["support", "ai"]` or `["billing", "webhook"]` help organize agents in the App, especially as the number of deployed agents grows.

**Use `::hot::store` for agent memory.** Enable embeddings when you need semantic search (conversation history, knowledge bases). Use plain maps for counters, state flags, and structured data.

**Use streams for multi-step workflows.** Send events with a `stream_id` to group related runs under a single stream. This gives you end-to-end visibility into agent workflows in the Streams view.

**Prefer event-driven over polling.** Use `on-event` handlers to react to changes rather than scheduled polling. Events are more efficient and produce clearer audit trails.
