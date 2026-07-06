---
description: "Understand how Hot represents function runs, emitted events, streaming output, logs, traces, and execution history."
---

# Runs, Events & Streams

Hot Platform uses three core primitives for workflow execution: **Runs**, **Events**, and **Streams**. Understanding these concepts is essential for building effective Hot applications.

## Runs

A **Run** is a single execution of a Hot function. Every function call creates a run, whether triggered by an API request, event, or schedule.

### Run Lifecycle

<svg viewBox="0 0 500 210" class="w-full max-w-md mx-auto my-6" style="font-family: system-ui, sans-serif;">
  <!-- Uses `.dark` selectors: the Hot App toggles dark mode via a class, not the OS setting. -->
  <!-- Neutral grays matching the app palette (input.css) — no blue tint. -->
  <style>
    .rls-running { fill: #fef9c3; stroke: #ca8a04; stroke-width: 1.5; }
    .rls-success { fill: #dcfce7; stroke: #16a34a; stroke-width: 1.5; }
    .rls-failed { fill: #fee2e2; stroke: #dc2626; stroke-width: 1.5; }
    .rls-cancelled { fill: #f8f8f8; stroke: #888888; stroke-width: 1.5; }
    .rls-retry { fill: #ffedd5; stroke: #ea580c; stroke-width: 1.5; }
    .rls-text { fill: #444444; font-size: 12px; font-weight: 500; }
    .rls-label { fill: #888888; font-size: 10px; font-style: italic; }
    .rls-arrow { stroke: #aaaaaa; stroke-width: 1.5; fill: none; }
    .rls-arrow-dashed { stroke: #aaaaaa; stroke-width: 1.5; fill: none; stroke-dasharray: 4 4; }
    .dark .rls-running { fill: #422006; }
    .dark .rls-success { fill: #052e16; }
    .dark .rls-failed { fill: #450a0a; }
    .dark .rls-cancelled { fill: #212126; stroke: #666666; }
    .dark .rls-retry { fill: #431407; stroke: #fb923c; }
    .dark .rls-text { fill: #f0f0f0; }
    .dark .rls-label { fill: #aaaaaa; }
  </style>
  <defs>
    <marker id="rls-arrow-head" markerWidth="8" markerHeight="8" refX="7" refY="3" orient="auto" markerUnits="strokeWidth">
      <path d="M0,0 L0,6 L7,3 z" fill="#aaaaaa"/>
    </marker>
  </defs>

  <!-- Running -->
  <rect x="30" y="96" width="84" height="32" rx="5" class="rls-running"/>
  <text x="72" y="117" text-anchor="middle" class="rls-text">running</text>

  <!-- Arrows to terminal states -->
  <path d="M 118 104 L 186 61" class="rls-arrow" marker-end="url(#rls-arrow-head)"/>
  <path d="M 118 112 L 186 112" class="rls-arrow" marker-end="url(#rls-arrow-head)"/>
  <path d="M 118 120 L 186 163" class="rls-arrow" marker-end="url(#rls-arrow-head)"/>

  <!-- Succeeded -->
  <rect x="192" y="44" width="94" height="32" rx="5" class="rls-success"/>
  <text x="239" y="65" text-anchor="middle" class="rls-text">succeeded</text>

  <!-- Failed -->
  <rect x="192" y="96" width="94" height="32" rx="5" class="rls-failed"/>
  <text x="239" y="117" text-anchor="middle" class="rls-text">failed</text>

  <!-- Cancelled -->
  <rect x="192" y="148" width="94" height="32" rx="5" class="rls-cancelled"/>
  <text x="239" y="169" text-anchor="middle" class="rls-text">cancelled</text>

  <!-- Failed -> pending_retry (when retry is configured) -->
  <path d="M 290 112 L 352 112" class="rls-arrow" marker-end="url(#rls-arrow-head)"/>
  <text x="296" y="104" class="rls-label">retry left?</text>

  <!-- Pending retry -->
  <rect x="358" y="96" width="112" height="32" rx="5" class="rls-retry"/>
  <text x="414" y="117" text-anchor="middle" class="rls-text">pending_retry</text>

  <!-- pending_retry -> new run (back to running) -->
  <path d="M 414 96 L 414 20 L 72 20 L 72 92" class="rls-arrow-dashed" marker-end="url(#rls-arrow-head)"/>
  <text x="120" y="14" class="rls-label">retries as a new run (linked via origin_run_id)</text>
</svg>

| State | Description |
|-------|-------------|
| `running` | Worker is executing the function |
| `succeeded` | Function completed successfully |
| `failed` | Function threw an error or timed out |
| `cancelled` | Run was cancelled before completion |
| `pending_retry` | Function failed but will be retried automatically |

Runs with `"retry"` metadata that fail are temporarily set to `pending_retry` until the retry executes. See [Retries](/docs/retries) for details.

### Run Data

Every run captures:

```json
{
  "run_id": "run_abc123xyz",
  "function": "::myapp::orders/process-order",
  "status": "succeeded",
  "input": {
    "order_id": "ord_12345"
  },
  "result": {
    "status": "processed",
    "total": 99.99
  },
  "started_at": "2024-12-04T10:30:00Z",
  "completed_at": "2024-12-04T10:30:02Z",
  "duration_ms": 2150,
  "trigger": {
    "type": "event",
    "event_id": "evt_xyz789"
  }
}
```

### Execution Trace

Hot captures a full execution trace for every run, showing:

- Each expression evaluated
- Intermediate values
- Function calls and returns
- Timing for each step
- Any errors with stack traces

### Triggering Runs

Runs can be triggered in several ways:

**1. API Call** (via `hot:call` event)
```bash
curl -X POST https://api.hot.dev/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"event_type": "hot:call", "event_data": {"fn": "::myapp::orders/process-order", "args": [{"order_id": "12345"}]}}'
```

**2. Event Handler**
```hot
on-order-created
meta {on-event: "order:created"}
fn (event) {
  process-order(event.data.order_id)
}
```

**3. Schedule** (recurring)
```hot
daily-report
meta {schedule: "0 0 * * *"}
fn (event) {
  generate-report()
}
```

**4. Dynamic Schedule** (one-time or created at runtime)
```hot
// Schedule a function to run in 10 minutes
send("hot:schedule:new", {
  fn: "::myapp::tasks/process",
  args: [{task_id: "123"}],
  schedule: "in 10 minutes"
})
```

See [Dynamic Schedules](/docs/schedules#dynamic-schedules) for more details.

**5. Direct Call** (from another run)
```hot
process-batch fn (orders) {
  // Each send creates a separate run
  map(orders, (order) {
    send("hot:call", {
      fn: "::myapp::orders/process-order",
      args: [order]
    })
  })
}
```

## Events

**Events** are messages that trigger asynchronous workflows. They decouple event producers from consumers, enabling scalable and maintainable systems.

### Event Structure

An Event in Hot has two fields:

```hot
Event type {
  type: Str,
  data: Any
}
```

The `send` function has two arities:

```hot
// Pass event type and data directly
send("user:created", {id: "usr_12345", email: "alice@example.com"})

// Or pass an Event
send(Event({type: "user:created", data: {id: "usr_12345", email: "alice@example.com"}}))
```

### Sending Events

**From Hot Code:**
```hot
// Send an event after user creation
create-user fn (data) {
  user insert-user(data)

  // Send event for other handlers (send is a core function)
  send("user:created", {
    id: user.id,
    email: user.email,
    name: user.name
  })

  user
}
```

**From the API:**
```bash
curl -X POST https://api.hot.dev/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "event_type": "user:created",
    "event_data": {"id": "usr_12345", "email": "alice@example.com"}
  }'
```

**From External Systems (Webhooks):**
Configure webhooks to forward events from services like Stripe, GitHub, or Slack directly to Hot.

### Event Handlers

Define handlers using the `on-event` metadata:

```hot
::myapp::notifications ns

// Handle a specific event type
on-user-created meta {on-event: "user:created"}
fn (event) {
  send-welcome-email(event.data.email)
}

```

### Event Delivery

Hot guarantees **at-least-once delivery** for events:

- Events are persisted before acknowledgment
- Failed handlers can be [retried automatically](/docs/retries) with configurable attempts and delay
- Retry status is visible in the Hot App UI

Delivery is optimized for durability and throughput, not strict global ordering. Concurrent workers may process different events from the same stream at the same time, and retries or infrastructure redelivery can arrive after newer events. Queue message fields are additive so rolling deploys can read older messages; workers hydrate the authoritative event payload from the database before routing.

Handlers should be idempotent when they perform external side effects, because the same event can be delivered more than once after a retry, worker crash, Redis pending-entry reclaim, or task reconciliation pass. This also applies to run timeouts: when a handler exceeds its run timeout it is recorded as a failure and retried according to its retry policy. The worker cancels the timed-out run cooperatively, but a handler stuck in non-cooperative work (a tight native loop or blocking syscall) can keep running in the background while its retry begins, so the two attempts may briefly overlap.

## Streams

**Streams** provide real-time, bidirectional data flow for scenarios where request/response isn't sufficient.

### Use Cases

- **AI/LLM Responses** - Stream tokens as they're generated
- **Live Updates** - Push data to clients in real-time
- **Long-Running Operations** - Report progress incrementally
- **Bidirectional Communication** - WebSocket-style interactions

### Server-Sent Events (SSE)

Stream data to clients in real-time using `::hot::stream/data`.

**Hot code** — emit chunks as they arrive:

```hot
handle-chat
meta { on-event: "chat:message" }
fn (event) {
  // Call a streaming AI API
  response ::anthropic::messages/post-stream({
    model: "claude-sonnet-4-20250514",
    max_tokens: 4096,
    messages: [{role: "user", content: event.data.message}]
  })

  // Process stream and emit chunks to the client
  process-stream(response.body, "")
}

// Recursive stream processor
process-stream fn (iter, accumulated: Str): Str {
  result next(iter)
  cond {
    result.done => { accumulated }
    => {
      delta or(result.value.data.delta.text, "")
      // Emit chunk to client in real-time
      ::hot::stream/data("ai:delta", { text: delta })
      process-stream(iter, concat(accumulated, delta))
    }
  }
}
```

**JavaScript client** — publish an event, then subscribe to the stream:

```javascript
// 1. Publish event to trigger the handler
const eventRes = await fetch('/v1/events', {
  method: 'POST',
  headers: {
    'Authorization': `Bearer ${API_KEY}`,
    'Content-Type': 'application/json'
  },
  body: JSON.stringify({
    event_type: 'chat:message',
    event_data: { message: 'Hello!' }
  })
});
const { data: { stream_id } } = await eventRes.json();

// 2. Subscribe to stream for real-time updates
// GET (classic SSE) and POST (streamable HTTP style) are both supported.
const response = await fetch(`/v1/streams/${stream_id}/subscribe`, {
  headers: {
    'Authorization': `Bearer ${API_KEY}`,
    'Accept': 'text/event-stream'
  }
});

const reader = response.body.getReader();
const decoder = new TextDecoder();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;

  const text = decoder.decode(value);
  // Parse SSE events (data: {...}\n\n format)
  for (const line of text.split('\n')) {
    if (line.startsWith('data: ')) {
      const event = JSON.parse(line.slice(6));
      if (event.type === 'stream:data') {
        // Real-time chunk from ::hot::stream/data
        appendToResponse(event.payload.text);
      }
      if (event.type === 'run:stop') {
        // Run completed
        console.log('Final result:', event.run.result);
      }
    }
  }
}
```

### Stream States

```
┌─────────┐    ┌─────────┐    ┌─────────┐
│  open   │ -> │ active  │ -> │ closed  │
└─────────┘    └─────────┘    └─────────┘
                    │
                    v
              ┌─────────┐
              │  error  │
              └─────────┘
```

### Viewing Streams

Active and completed streams are visible in the Hot App:

- Connection status and duration
- Messages sent/received
- Bandwidth usage
- Error details

## Monitoring

All runs, events, and streams are visible in the **Hot App** with:

- Real-time updates as executions happen
- Filtering by status, function, event type
- Full-text search across payloads
- Detailed drill-down views
