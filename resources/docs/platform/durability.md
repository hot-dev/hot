---
description: "Learn how Hot provides durable execution for workflows, retries, resumability, idempotency, and production reliability."
---

# Durable Execution

Hot provides durable execution out of the box. Every function call is a persisted **run** with recorded inputs, outputs, and a full execution trace. Multi-step workflows are chains of **events** and runs — each step is independently tracked, retryable, and recoverable.

## How It Works

The durability model is built on three ideas:

1. **Runs are atomic, durable steps.** Each run takes input, executes, and produces a result. All of this is persisted — if a run fails, the platform knows exactly what happened and can retry it.

2. **Events are the workflow journal.** A run can emit events that trigger downstream runs. The chain of events connecting runs is itself persisted, forming a complete record of your workflow's progress.

3. **Retries use the original input.** When a run fails and is retried, the retry receives the same event data as the original. No partial state to reconstruct — each attempt is a clean execution with the same input.

```
Event A → Run 1 (succeeds) → emits Event B
                                 ↓
                              Run 2 (fails)
                                 ↓
                              Run 2 retry (same Event B data) → succeeds → emits Event C
                                                                              ↓
                                                                           Run 3
```

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

If your handler has side effects that shouldn't happen twice (charging a payment, sending an email), use idempotency techniques — check whether the work was already done before doing it again.

```hot
on-payment meta {on-event: "payment:charge", retry: 3}
fn (event) {
  existing get-charge(event.data.idempotency-key)
  if(is-some(existing), existing, process-charge(event.data))
}
```

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
