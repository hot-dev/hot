---
description: "Choose between short-lived Hot runs and long-running tasks, including TaskInfo, code tasks, container tasks, and messaging."
---

# Tasks

Tasks are long-running, asynchronous processes on the Hot Platform. They extend
the platform run model with a durable resource, background execution, and
task-specific lifecycle controls. See the
[Platform Execution Model](/docs/platform/execution-model) for their place in
event and stream lineage.

## Ordinary Runs vs Tasks

| | Runs | Tasks |
|---|------|-------|
| **Duration** | Short-lived, synchronous | Long-running, asynchronous |
| **Trigger** | HTTP requests, events, schedules | Started from runs or other tasks |
| **Return** | Waits for completion, returns result | Returns immediately with `TaskInfo` |
| **Execution record** | The run is the execution attempt | The task resource links to a task-type run |
| **Use case** | Request-response, event handlers | Background jobs, containers, long-lived processes |

### Runs

**Runs** are short-lived, synchronous top-level function execution attempts.
Nested Hot function calls remain inside the current run's trace. Runs are
triggered by:

- HTTP requests (API calls, webhooks)
- Events (`send`, `hot:call`)
- Schedules (cron, dynamic schedules)

Runs block until the function completes. See [Runs, Events & Streams](/docs/platform/runs-events-streams) for details.

### Tasks

**Tasks** are long-running, asynchronous resources. When you start a task, the
current run returns immediately with a `TaskInfo` containing the task ID and
stream ID. The task inherits that stream, records the originating run, and
executes in the background on a task worker. Its execution is recorded as a
task-type run.

There are two types of tasks:

1. **Code Tasks** — Hot code with messaging (`::hot::task/start`, `::hot::task/send`, `::hot::task/receive`) and WebSocket support (`::hot::ws`)
2. **Container Tasks** — Docker/OCI containers via `::hot::box/start`

### When to Use Each

| Scenario | Use |
|----------|-----|
| Request-response, event handlers, scheduled jobs | **Runs** |
| Long-running Hot code with send/receive messaging | **Code Tasks** |
| Arbitrary languages, CLI tools, system binaries | **Container Tasks** |

## Task Lifecycle

Tasks move through these states:

<div class="my-8" style="overflow-x: auto; padding-bottom: 0.5rem;">
<svg viewBox="0 0 920 310" class="w-full max-w-4xl mx-auto" style="min-width: 44rem; font-family: system-ui, sans-serif;" role="img" aria-labelledby="task-lifecycle-title task-lifecycle-desc">
  <title id="task-lifecycle-title">Task lifecycle states</title>
  <desc id="task-lifecycle-desc">A queued task becomes running, then finishes as completed, failed, timed out, or cancelled.</desc>
  <style>
    .tl-node { fill: #ffffff; stroke: #d1d5db; stroke-width: 1.5; }
    .tl-queued { fill: #f9fafb; stroke: #9ca3af; }
    .tl-running { fill: #fffbeb; stroke: #f59e0b; }
    .tl-completed { fill: #f0fdf4; stroke: #22c55e; }
    .tl-failed { fill: #fef2f2; stroke: #ef4444; }
    .tl-timeout { fill: #fff7ed; stroke: #f97316; }
    .tl-cancelled { fill: #f9fafb; stroke: #71717a; }
    .tl-title { fill: #111827; font-size: 16px; font-weight: 650; }
    .tl-sub { fill: #6b7280; font-size: 12px; }
    .tl-arrow { fill: none; stroke: #9ca3af; stroke-width: 2; }
    .tl-label { fill: #6b7280; font-size: 11.5px; font-weight: 550; }
    .dark .tl-node { fill: #1c1c20; stroke: #3f3f46; }
    .dark .tl-queued, .dark .tl-cancelled { fill: #18181b; stroke: #71717a; }
    .dark .tl-running { fill: #422006; stroke: #fbbf24; }
    .dark .tl-completed { fill: #052e16; stroke: #4ade80; }
    .dark .tl-failed { fill: #450a0a; stroke: #f87171; }
    .dark .tl-timeout { fill: #431407; stroke: #fb923c; }
    .dark .tl-title { fill: #f4f4f5; }
    .dark .tl-sub, .dark .tl-label { fill: #a1a1aa; }
    .dark .tl-arrow { stroke: #71717a; }
  </style>
  <defs>
    <marker id="tl-arrowhead" markerWidth="9" markerHeight="8" refX="8" refY="4" orient="auto" markerUnits="strokeWidth">
      <path d="M0,0 L0,8 L9,4 z" fill="#9ca3af"/>
    </marker>
  </defs>

  <rect x="45" y="119" width="160" height="68" rx="12" class="tl-node tl-queued"/>
  <text x="125" y="148" text-anchor="middle" class="tl-title">queued</text>
  <text x="125" y="170" text-anchor="middle" class="tl-sub">waiting for a worker</text>

  <path d="M205 153 L285 153" class="tl-arrow" marker-end="url(#tl-arrowhead)"/>
  <text x="245" y="141" text-anchor="middle" class="tl-label">claimed</text>

  <rect x="285" y="119" width="180" height="68" rx="12" class="tl-node tl-running"/>
  <text x="375" y="148" text-anchor="middle" class="tl-title">running</text>
  <text x="375" y="170" text-anchor="middle" class="tl-sub">task worker executing</text>

  <path d="M465 153 C535 153 535 52 625 52" class="tl-arrow" marker-end="url(#tl-arrowhead)"/>
  <path d="M465 153 C535 153 535 119 625 119" class="tl-arrow" marker-end="url(#tl-arrowhead)"/>
  <path d="M465 153 C535 153 535 186 625 186" class="tl-arrow" marker-end="url(#tl-arrowhead)"/>
  <path d="M465 153 C535 153 535 253 625 253" class="tl-arrow" marker-end="url(#tl-arrowhead)"/>

  <rect x="625" y="24" width="220" height="56" rx="11" class="tl-node tl-completed"/>
  <text x="735" y="48" text-anchor="middle" class="tl-title">completed</text>
  <text x="735" y="67" text-anchor="middle" class="tl-sub">finished successfully</text>

  <rect x="625" y="91" width="220" height="56" rx="11" class="tl-node tl-failed"/>
  <text x="735" y="115" text-anchor="middle" class="tl-title">failed</text>
  <text x="735" y="134" text-anchor="middle" class="tl-sub">exited with an error</text>

  <rect x="625" y="158" width="220" height="56" rx="11" class="tl-node tl-timeout"/>
  <text x="735" y="182" text-anchor="middle" class="tl-title">timed_out</text>
  <text x="735" y="201" text-anchor="middle" class="tl-sub">exceeded its timeout</text>

  <rect x="625" y="225" width="220" height="56" rx="11" class="tl-node tl-cancelled"/>
  <text x="735" y="249" text-anchor="middle" class="tl-title">cancelled</text>
  <text x="735" y="268" text-anchor="middle" class="tl-sub">stopped cooperatively</text>
</svg>
</div>

| State | Description |
|-------|-------------|
| `queued` | Task is waiting for a worker |
| `running` | Task is executing |
| `completed` | Task finished successfully |
| `failed` | Task exited with an error |
| `timed_out` | Task exceeded its timeout |
| `cancelled` | Task was cancelled before completion |

## Starting Tasks

### Code Tasks

Use `::hot::task/start` to start a Hot function as a long-running task:

```hot
::task ::hot::task

// Start a task with no arguments
info ::task/start(::myapp/background-sync)

// Start a task with arguments
info ::task/start(::myapp/process-data, {url: "https://example.com"})

// Start with options (timeout, retry)
info ::task/start(::myapp/long-job, {input: data}, {
  timeout: 3600000,
  retry: {attempts: 3, delay: 5000, backoff: "exponential"}
})
```

### Container Tasks

Use `::hot::box/start` to run Docker/OCI containers:

```hot
::box ::hot::box

task ::box/start(BoxConf({
  image: "python:3.13-alpine",
  cmd: ["python", "-c", "print('Hello')"],
  size: "nano"
}))
```

See [Containers](/docs/box) for full container documentation.

## TaskInfo

Both `::hot::task/start` and `::hot::box/start` return a `TaskInfo` with:

| Field | Type | Description |
|-------|------|-------------|
| `id` | `Str` | Unique task identifier (UUID) |
| `stream-id` | `Str` | Stream this task belongs to |

For code tasks, `TaskInfo` also includes `stream` (the full stream object) and `origin-run` (the run that spawned the task).

## Waiting for Completion

Choose where to wait based on who needs the result:

- If later Hot code in the same execution depends on the result, call
  `::hot::task/await(info.id)`.
- If a client needs the result, return `info.id` from the run and use the
  official SDK task waiter. This lets the originating run finish while the task
  continues asynchronously.

All SDK waiters subscribe to `/v1/tasks/{task_id}/subscribe`. The first
`task:update` is always the latest persisted state, so the client cannot miss a
task that completed before it subscribed. The waiter reconnects when needed,
returns the completed task record, and raises a structured task error for
`failed`, `cancelled`, or `timed_out`.

| Language | Wait method |
|----------|-------------|
| JavaScript / TypeScript | `await hot.tasks.wait(taskId)` |
| Python | `hot.tasks.wait(task_id)` or `await async_hot.tasks.wait(task_id)` |
| Go | `client.Tasks.Wait(ctx, taskID, nil)` |
| Rust | `client.tasks().wait(task_id, TaskWaitOptions::default()).await` |
| Java | `client.tasks().waitFor(taskId)` |

See [SDKs](/docs/api/sdks#wait-for-a-background-task) for timeout examples and
language-specific failure types.

## Cancellation

Cancel a queued or running task with `::hot::task/cancel`:

```hot
::task ::hot::task

info ::task/start(::myapp/long-job, data)

// Later, cancel the task
cancelled ::task/cancel(info.id)
```

Returns `true` if the task was cancelled, `false` if it was already in a terminal state.

For running tasks, a cancellation message is delivered to the task's `receive` channel (as `{$cancel: true}`) so it can exit cooperatively.

## Messaging (Code Tasks Only)

Code tasks can receive messages from other runs or tasks using `::hot::task/send` and `::hot::task/receive`:

```hot
::task ::hot::task

// From a run: start a task and send it data
info ::task/start(::myapp/worker, null)
::task/send(info.id, {command: "process", payload: data})
::task/send(info.id, "shutdown")

// Inside the task function: receive messages
my-task fn (initial-args: Any): Any {
  msg ::task/receive()
  cond {
    eq(msg, "shutdown") => { "done" }
    => { process(msg) }
  }
}
```

`receive` blocks until a message arrives. Returns `null` when the task's inbox closes.

## Checkpoint & Restore (Code Tasks)

Long-running code tasks can save application state that persists across restarts. If a task is interrupted (worker crash, deploy) and retried, the new instance can call `restore()` to pick up where it left off.

```hot
::task ::hot::task

my-etl fn (config: Map): Any {
  // Restore previous state, or start fresh
  state or(::task/restore(), {offset: 0, processed: 0})

  // ... process batch starting from state.offset ...

  // Save progress
  ::task/checkpoint({offset: add(state.offset, batch-size), processed: add(state.processed, batch-size)})
}
```

`checkpoint` accepts any serializable value and returns `true` on success. `restore` returns the last checkpointed value, or `null` if no checkpoint exists. Both are only callable from inside a task.

You can also inspect a different task's checkpoint by passing a task ID: `::task/restore(task-id)`.

## WebSocket Support (Code Tasks)

Code tasks can maintain long-lived WebSocket connections using `::hot::ws`:

```hot
::ws ::hot::ws

// Inside a task
conn ::ws/connect("wss://echo.websocket.org", {headers: {}})
::ws/send(conn, {type: "hello", text: "world"})
msg ::ws/receive(conn)
::ws/close(conn)
```

WebSocket connections outlive a single run, making them ideal for real-time sessions inside tasks.
