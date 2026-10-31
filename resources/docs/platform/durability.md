---
description: "See how Hot persists runs and event chains, retries failed steps with the same input, and records execution history."
---

# Durable Execution

Hot provides durable execution out of the box. Every platform-invoked,
top-level function execution is a persisted **run** with recorded inputs,
outputs, and a full execution trace. Nested function calls appear inside that
trace. Multi-step workflows are chains of **events** and runs—each step is
independently tracked, retryable, and recoverable.

See the [Platform Execution Model](/docs/platform/execution-model) for how
events, handlers, runs, tasks, retries, workers, and streams connect before
diving into the durability guarantees below.

## How It Works

The durability model is built on three ideas:

1. **Runs are atomic, durable steps.** Each run takes input, executes, and produces a result. All of this is persisted — if a run fails, the platform knows exactly what happened and can retry it.

2. **Events are the workflow journal.** A run can emit events that trigger downstream runs. The chain of events connecting runs is itself persisted, forming a complete record of your workflow's progress.

3. **Retries use the original input.** When a run fails and is retried, the retry receives the same event data as the original. No partial state to reconstruct — each attempt is a clean execution with the same input.

<div class="my-8" style="overflow-x: auto; padding-bottom: 0.5rem;">
<svg viewBox="0 0 920 400" class="w-full max-w-4xl mx-auto" style="min-width: 44rem; font-family: system-ui, sans-serif;" role="img" aria-labelledby="durable-chain-title durable-chain-desc">
  <title id="durable-chain-title">Durable event and run retry chain</title>
  <desc id="durable-chain-desc">Event A triggers Run 1, which succeeds and emits Event B. Run 2 fails, then a new linked retry uses the same Event B data, succeeds, emits Event C, and triggers Run 3.</desc>
  <style>
    .dur-stream { fill: #fafafa; stroke: #d1d5db; stroke-width: 2; stroke-dasharray: 7 7; }
    .dur-node { fill: #ffffff; stroke: #d1d5db; stroke-width: 1.5; }
    .dur-event { fill: #fff7ed; stroke: #f97316; }
    .dur-success { fill: #f0fdf4; stroke: #22c55e; }
    .dur-failed { fill: #fef2f2; stroke: #ef4444; }
    .dur-retry { fill: #f5f3ff; stroke: #8b5cf6; }
    .dur-title { fill: #111827; font-size: 16px; font-weight: 650; }
    .dur-sub { fill: #6b7280; font-size: 12px; }
    .dur-kicker { fill: #6b7280; font-size: 11px; font-weight: 700; letter-spacing: 1.4px; }
    .dur-arrow { fill: none; stroke: #9ca3af; stroke-width: 2; }
    .dur-label { fill: #6b7280; font-size: 11.5px; font-weight: 550; }
    .dark .dur-stream { fill: #111113; stroke: #3f3f46; }
    .dark .dur-node { fill: #1c1c20; stroke: #3f3f46; }
    .dark .dur-event { fill: #431407; stroke: #fb923c; }
    .dark .dur-success { fill: #052e16; stroke: #4ade80; }
    .dark .dur-failed { fill: #450a0a; stroke: #f87171; }
    .dark .dur-retry { fill: #2e1065; stroke: #a78bfa; }
    .dark .dur-title { fill: #f4f4f5; }
    .dark .dur-sub, .dark .dur-kicker, .dark .dur-label { fill: #a1a1aa; }
    .dark .dur-arrow { stroke: #71717a; }
  </style>
  <defs>
    <marker id="dur-arrowhead" markerWidth="9" markerHeight="8" refX="8" refY="4" orient="auto" markerUnits="strokeWidth">
      <path d="M0,0 L0,8 L9,4 z" fill="#9ca3af"/>
    </marker>
  </defs>

  <rect x="20" y="22" width="880" height="356" rx="20" class="dur-stream"/>
  <text x="46" y="52" class="dur-kicker">ONE STREAM · EVERY STEP PERSISTED</text>

  <rect x="45" y="86" width="140" height="64" rx="12" class="dur-node dur-event"/>
  <text x="115" y="113" text-anchor="middle" class="dur-title">Event A</text>
  <text x="115" y="134" text-anchor="middle" class="dur-sub">workflow input</text>

  <path d="M185 118 L225 118" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>

  <rect x="225" y="86" width="150" height="64" rx="12" class="dur-node dur-success"/>
  <text x="300" y="113" text-anchor="middle" class="dur-title">Run 1</text>
  <text x="300" y="134" text-anchor="middle" class="dur-sub">succeeded</text>

  <path d="M375 118 L415 118" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>
  <text x="395" y="106" text-anchor="middle" class="dur-label">emits</text>

  <rect x="415" y="86" width="140" height="64" rx="12" class="dur-node dur-event"/>
  <text x="485" y="113" text-anchor="middle" class="dur-title">Event B</text>
  <text x="485" y="134" text-anchor="middle" class="dur-sub">next-step input</text>

  <path d="M555 118 L605 118" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>

  <rect x="605" y="86" width="170" height="64" rx="12" class="dur-node dur-failed"/>
  <text x="690" y="113" text-anchor="middle" class="dur-title">Run 2</text>
  <text x="690" y="134" text-anchor="middle" class="dur-sub">failed</text>

  <!-- Failure schedules a clean, linked retry on a second row -->
  <path d="M690 150 L690 200 C690 232 665 246 625 246 L375 246 C345 246 340 262 340 282" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>
  <text x="515" y="235" text-anchor="middle" class="dur-label">new attempt · same Event B data · linked origin_run_id</text>

  <rect x="255" y="282" width="170" height="64" rx="12" class="dur-node dur-retry"/>
  <text x="340" y="309" text-anchor="middle" class="dur-title">Run 2 retry</text>
  <text x="340" y="330" text-anchor="middle" class="dur-sub">succeeded</text>

  <path d="M425 314 L485 314" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>
  <text x="455" y="302" text-anchor="middle" class="dur-label">emits</text>

  <rect x="485" y="282" width="140" height="64" rx="12" class="dur-node dur-event"/>
  <text x="555" y="309" text-anchor="middle" class="dur-title">Event C</text>
  <text x="555" y="330" text-anchor="middle" class="dur-sub">workflow continues</text>

  <path d="M625 314 L685 314" class="dur-arrow" marker-end="url(#dur-arrowhead)"/>

  <rect x="685" y="282" width="150" height="64" rx="12" class="dur-node dur-success"/>
  <text x="760" y="309" text-anchor="middle" class="dur-title">Run 3</text>
  <text x="760" y="330" text-anchor="middle" class="dur-sub">next durable step</text>
</svg>
</div>

Every arrow in this chain is persisted. Every run captures its input, output, timing, and full execution trace. If anything fails, retries pick up from the failed step — not from the beginning of the workflow.

## What Gets Persisted

Every run automatically captures:

- **Input** — the event data or arguments that triggered it
- **Result** — the return value on success, or error details on failure
- **Execution trace** — every function call, intermediate value, and timing
- **Status** — `running`, `succeeded`, `failed`, `cancelled`, or `pending_retry`
- **Lineage** — which event triggered this run, and which run emitted that event

This happens without any additional code. Write a function, attach it to an event or schedule, and the platform handles the rest.

```hot
on-order-created meta {on-event: "order:created", retry: 3}
fn (event) {
  validated validate-order(event.data)
  charge-payment(validated)
  send("order:confirmed", {order-id: validated.id})
}
```

If `charge-payment` fails on the first attempt, the run is marked `pending_retry` and re-executed with the same `order:created` event data. The retry is linked to the original via `origin_run_id`, so you can trace the full history. On success, the `order:confirmed` event triggers the next step in the workflow.

## Multi-Step Workflows

Complex workflows are composed as chains of events and handlers. Each handler is a durable step:

```hot
::myapp::orders ns

// Step 1: Validate and charge
on-order-created meta {on-event: "order:created", retry: 3}
fn (event) {
  order validate-order(event.data)
  charge-payment(order)
  send("order:paid", {order-id: order.id, amount: order.total})
}

// Step 2: Fulfill
on-order-paid meta {on-event: "order:paid", retry: 5}
fn (event) {
  shipment create-shipment(event.data.order-id)
  send("order:shipped", {order-id: event.data.order-id, tracking: shipment.tracking})
}

// Step 3: Notify
on-order-shipped meta {on-event: "order:shipped", retry: 3}
fn (event) {
  send-shipping-notification(event.data.order-id, event.data.tracking)
}
```

Each step:

- **Runs independently** — a failure in step 2 doesn't re-run step 1
- **Retries automatically** — with configurable attempts, backoff, and jitter
- **Is fully observable** — inputs, outputs, and traces visible in Hot App
- **Passes data forward** — via event payloads, not hidden internal state

## Long-Running Work

For processes that outlive a single run — background jobs, data pipelines, real-time sessions — [Tasks](/docs/tasks) extend the same durability model with checkpoints and messaging.

## At-Least-Once Delivery

Hot guarantees **at-least-once delivery** for events. Events are persisted before being acknowledged, and failed handlers retry automatically when configured. This means:

- Events are never silently lost
- Handlers will execute at least once for every event
- Retries deliver the same event data
- Infrastructure redelivery can deliver an event more than once
- Strict event ordering is not guaranteed across concurrent workers

If your handler has side effects that shouldn't happen twice (charging a payment, sending an email), use idempotency techniques — check whether the work was already done before doing it again.

Queue envelopes are additive for rolling deploy compatibility. Workers hydrate the full event payload from the database before routing, so old queue messages remain readable while newer workers can still rely on the database as the source of truth.

```hot
on-payment meta {on-event: "payment:charge", retry: 3}
fn (event) {
  existing get-charge(event.data.idempotency-key)
  if(is-some(existing), existing, process-charge(event.data))
}
```

## Concurrent Store Writes

Workers run different events at the same time, including events of one
stream. When two handlers read the same [`::hot::store`](/pkg/hot.dev/hot-std/hot/store)
entry and each write back a changed copy, the later `put` replaces the earlier
one and its change is lost:

```hot
::store ::hot::store

cases ::store/Map({name: "cases"})

// Two answers that land together can lose one
record-answer-unsafely fn (case-id: Str, answer-id: Str, answer: Any) {
  current ::store/get(cases, case-id, {})
  ::store/put(cases, case-id, assoc(current, answer-id, answer))
}
```

The store's atomic writes compare and write in one database transaction that
holds the entry's lock:

| Write | Does |
|-------|------|
| `put-if-missing(store, key, value)` | Creates the entry only if the key is missing |
| `put-if(store, key, expected, value)` | Replaces the value only if it still equals `expected`; `null` expects a missing key |
| `increment(store, key, by)` | Adds to an Int and returns the new value; a missing key counts from 0 |

To change a value, read it, compute the new one, and retry while `put-if`
returns `false`. The computation must be pure, because it can run more than
once. Each retry means another writer changed the value first, so give the
loop a budget: a bug, such as an `expected` that isn't what `get` returned,
then fails with an error instead of retrying forever:

```hot
::store ::hot::store

cases ::store/Map({name: "cases"})

record-answer fn (case-id: Str, answer-id: Str, answer: Any, attempts: Int): Map {
  current ::store/get(cases, case-id)
  next assoc(if(is-null(current), {}, current), answer-id, answer)
  wrote ::store/put-if(cases, case-id, current, next)
  cond {
    wrote => { next }
    lte(attempts, 1) => { err(`case ${case-id} kept changing`) }
    => { record-answer(case-id, answer-id, answer, sub(attempts, 1)) }
  }
}

record-answer("case-1", "answer-1", {score: 4}, 20)
```

`increment` gives each caller its own value, so it can number a case's facts.
Concurrent handlers can finish out of order, so sort by the number rather than
relying on insertion order:

```hot
::store ::hot::store

facts ::store/Map({name: "facts"})
counters ::store/Map({name: "counters"})

record-fact fn (case-id: Str, fact: Map) {
  seq ::store/increment(counters, `${case-id}:seq`)
  ::store/put(facts, `${case-id}:${seq}`, {...fact, seq})
}

case-facts fn (case-id: Str): Vec {
  ::store/list(facts, {prefix: `${case-id}:`, limit: 500}) |> sort-by(%.value.seq)
}
```

`list` and `keys` with a `prefix` read only the keys that start with it, from
an index, and page with `limit` and `offset`. `filter` reads every entry.

An `increment` or a `put-if` that adds an entry is not idempotent: a retried or
redelivered event applies it again. Key the change by something the event
carries, such as an answer id, when a repeat must not count twice.

## Observability

All runs, events, retries, and tasks are visible in **Hot App**:

- **Run history** with status, duration, and retry badges
- **Execution traces** showing every function call and intermediate value
- **Event flow** connecting runs to the events that triggered them
- **Retry lineage** linking retries back to the original run via `origin_run_id`
- **Real-time updates** as workflows execute

You can filter by status, function, event type, or search across payloads to find exactly what you need.

## Summary

| Concept | Role |
|---------|------|
| **Runs** | Atomic, persisted function executions with full traces |
| **Events** | Persisted messages connecting runs into workflows |
| **Retries** | Automatic re-execution with original input on failure |
| **Tasks** | Long-running processes with checkpoints and messaging |
| **Hot App** | Real-time visibility into every step |

No replay engines, no hidden state machines. Each step is a function that takes input and produces output. The platform persists everything and handles recovery automatically.
