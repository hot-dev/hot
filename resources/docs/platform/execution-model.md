---
description: "Understand how Hot connects events, handlers, runs, retries, tasks, workers, and streams into durable workflows."
---

# Platform Execution Model

The platform execution model describes how Hot creates and connects durable
execution units. It is distinct from the
[Language Evaluation Model](/docs/language/execution-model), which describes
how expressions, arguments, flows, and Results behave inside one run or task
attempt.

## The Lifecycle

Most platform work follows this pattern:

<div class="my-8" style="overflow-x: auto; padding-bottom: 0.5rem;">
<svg viewBox="0 0 920 650" class="w-full max-w-4xl mx-auto" style="min-width: 44rem; font-family: system-ui, sans-serif;" role="img" aria-labelledby="platform-lifecycle-title platform-lifecycle-desc">
  <title id="platform-lifecycle-title">Hot Platform execution lifecycle</title>
  <desc id="platform-lifecycle-desc">An external trigger creates a persisted event. The event routes to matching handlers, each selected handler gets a run, and that run can emit a child event or start a task in the same stream.</desc>
  <style>
    .pem-stream { fill: #fafafa; stroke: #d1d5db; stroke-width: 2; stroke-dasharray: 7 7; }
    .pem-node { fill: #ffffff; stroke: #d1d5db; stroke-width: 1.5; }
    .pem-event { fill: #fff7ed; stroke: #f97316; }
    .pem-route { fill: #f9fafb; stroke: #9ca3af; }
    .pem-run { fill: #fef2f2; stroke: #ef4444; }
    .pem-task { fill: #f5f3ff; stroke: #8b5cf6; }
    .pem-title { fill: #111827; font-size: 17px; font-weight: 650; }
    .pem-sub { fill: #6b7280; font-size: 12.5px; }
    .pem-kicker { fill: #6b7280; font-size: 11px; font-weight: 700; letter-spacing: 1.4px; }
    .pem-arrow { fill: none; stroke: #9ca3af; stroke-width: 2; }
    .pem-arrow-label { fill: #6b7280; font-size: 11.5px; font-weight: 550; }
    .dark .pem-stream { fill: #111113; stroke: #3f3f46; }
    .dark .pem-node { fill: #1c1c20; stroke: #3f3f46; }
    .dark .pem-event { fill: #431407; stroke: #fb923c; }
    .dark .pem-route { fill: #18181b; stroke: #52525b; }
    .dark .pem-run { fill: #450a0a; stroke: #f87171; }
    .dark .pem-task { fill: #2e1065; stroke: #a78bfa; }
    .dark .pem-title { fill: #f4f4f5; }
    .dark .pem-sub, .dark .pem-kicker, .dark .pem-arrow-label { fill: #a1a1aa; }
    .dark .pem-arrow { stroke: #71717a; }
  </style>
  <defs>
    <marker id="pem-arrowhead" markerWidth="9" markerHeight="8" refX="8" refY="4" orient="auto" markerUnits="strokeWidth">
      <path d="M0,0 L0,8 L9,4 z" fill="#9ca3af"/>
    </marker>
  </defs>

  <!-- Entry point, outside the stream lineage boundary -->
  <rect x="210" y="14" width="500" height="62" rx="12" class="pem-node"/>
  <text x="460" y="40" text-anchor="middle" class="pem-title">API call, webhook, schedule, or send</text>
  <text x="460" y="60" text-anchor="middle" class="pem-sub">platform entry point</text>

  <!-- Shared stream boundary -->
  <rect x="20" y="108" width="880" height="522" rx="20" class="pem-stream"/>
  <text x="46" y="137" class="pem-kicker">STREAM · SHARED EXECUTION LINEAGE</text>

  <path d="M460 76 L460 162" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>

  <rect x="310" y="162" width="300" height="66" rx="12" class="pem-node pem-event"/>
  <text x="460" y="189" text-anchor="middle" class="pem-title">Persisted event</text>
  <text x="460" y="211" text-anchor="middle" class="pem-sub">creates or continues the stream</text>

  <path d="M460 228 L460 268" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>

  <rect x="310" y="268" width="300" height="66" rx="12" class="pem-node pem-route"/>
  <text x="460" y="295" text-anchor="middle" class="pem-title">Handler routing</text>
  <text x="460" y="317" text-anchor="middle" class="pem-sub">zero to many matching handlers</text>

  <path d="M460 334 L460 374" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>

  <rect x="300" y="374" width="320" height="66" rx="12" class="pem-node pem-run"/>
  <text x="460" y="401" text-anchor="middle" class="pem-title">Handler run attempt</text>
  <text x="460" y="423" text-anchor="middle" class="pem-sub">one run per selected handler</text>

  <!-- Branch to child event and task -->
  <path d="M460 440 L460 466 L235 466 L235 494" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>
  <path d="M460 440 L460 466 L685 466 L685 494" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>
  <text x="335" y="458" text-anchor="middle" class="pem-arrow-label">send(...)</text>
  <text x="585" y="458" text-anchor="middle" class="pem-arrow-label">task/start</text>

  <rect x="85" y="494" width="300" height="62" rx="12" class="pem-node pem-event"/>
  <text x="235" y="520" text-anchor="middle" class="pem-title">Child event</text>
  <text x="235" y="541" text-anchor="middle" class="pem-sub">inherits the stream ID</text>

  <rect x="535" y="494" width="300" height="62" rx="12" class="pem-node pem-task"/>
  <text x="685" y="520" text-anchor="middle" class="pem-title">Task resource</text>
  <text x="685" y="541" text-anchor="middle" class="pem-sub">same stream · linked origin run</text>

  <path d="M235 556 L235 582" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>
  <path d="M685 556 L685 582" class="pem-arrow" marker-end="url(#pem-arrowhead)"/>

  <rect x="105" y="582" width="260" height="34" rx="9" class="pem-node pem-run"/>
  <text x="235" y="604" text-anchor="middle" class="pem-title" style="font-size: 14px;">Downstream run(s)</text>

  <rect x="555" y="582" width="260" height="34" rx="9" class="pem-node pem-run"/>
  <text x="685" y="604" text-anchor="middle" class="pem-title" style="font-size: 14px;">Task execution run</text>
</svg>
</div>

A **stream** ties the whole chain together. Events, runs, retries, and tasks
carry the same stream ID as work continues. A new externally published event
creates a stream unless the caller supplies an existing `stream_id`.

## Platform Units

### Event

An event is a persisted message. The platform routes it to matching handler
definitions. One event may select zero, one, or multiple handlers; each selected
handler invocation receives its own run attempt.

Events use at-least-once delivery. Redelivery and retries can produce more than
one attempt, and events in the same stream are not guaranteed to execute in
strict order.

### Handler

A handler is a function definition registered for an event type. It is routing
metadata, not a separate execution record. The execution of a selected handler
is recorded as a run.

### Run

A run is one platform-invoked, top-level function execution attempt. API calls,
event handlers, schedules, and task workers can create runs.

Ordinary Hot function calls made inside that top-level function do **not**
create additional platform runs. They appear as calls within the current run's
execution trace. Publishing an event, starting a task, or retrying work crosses
a platform execution boundary and creates a new linked unit.

A retry is a new run attempt linked to its prior run through `origin_run_id`.
It keeps the same triggering data and stream.

### Task

A task is a long-running asynchronous resource started from a run or another
task. It inherits the current stream and records the run that started it.
Executing the task also creates a task-type run, so its function calls, result,
timing, and failures remain observable through the same run model.

Code tasks add messaging and checkpoints. Container tasks run an OCI container.
See [Tasks](/docs/tasks) for their lifecycles and APIs.

### Stream

A stream has two related roles:

1. **Execution lineage** — it correlates the events, runs, retries, and tasks
   that belong to one workflow or interaction.
2. **Live delivery** — clients can subscribe to run lifecycle notifications and
   data emitted with `::hot::stream/data`.

Events, runs, and task records are durable. User-emitted `stream:data` messages
are live delivery payloads and are not persisted as workflow records. A stream
is also not a serialization lock: workers may process related work
concurrently.

### Workers

Workers consume queued events and tasks. Event workers route events and execute
selected handlers as runs; task workers execute code or container tasks. Worker
scaling changes throughput, not the lineage relationships recorded in the
stream.

## What Creates a New Unit?

| Operation | Platform effect |
|-----------|-----------------|
| Call a Hot function normally | Stays inside the current run or task trace |
| `send(...)` | Persists a child event in the current stream |
| `send("hot:call", ...)` | Persists an event that dispatches another function as a new run |
| `::hot::task/start(...)` or `::hot::box/start(...)` | Creates a task linked to the current run and stream |
| Retry a failed run | Creates a new run linked to the prior attempt |
| `::hot::stream/data(...)` | Publishes ephemeral live data on the current stream |

## Persistence and Lineage

| Record | Key relationships |
|--------|-------------------|
| Event | `event_id`, `stream_id` |
| Run | `run_id`, `event_id`, `stream_id`, optional `origin_run_id` |
| Task | `task_id`, `stream_id`, `origin_run_id`, associated execution `run_id` |
| Stream | Correlation ID and aggregate history for the workflow |

For detailed state machines and APIs, continue with
[Runs, Events & Streams](/docs/platform/runs-events-streams),
[Tasks](/docs/tasks), and [Durable Execution](/docs/platform/durability).
