# Tasks

Tasks are long-running, asynchronous processes on the Hot Platform. Understanding the distinction between **Runs** and **Tasks** helps you choose the right execution model for your workflow.

## Runs vs Tasks

| | Runs | Tasks |
|---|------|-------|
| **Duration** | Short-lived, synchronous | Long-running, asynchronous |
| **Trigger** | HTTP requests, events, schedules | Started from runs or other tasks |
| **Return** | Waits for completion, returns result | Returns immediately with `TaskInfo` |
| **Use case** | Request-response, event handlers | Background jobs, containers, long-lived processes |

### Runs

**Runs** are short-lived, synchronous function executions. Each run executes a Hot function to completion and returns a result. Runs are triggered by:

- HTTP requests (API calls, webhooks)
- Events (`send`, `hot:call`)
- Schedules (cron, dynamic schedules)

Runs block until the function completes. See [Runs, Events & Streams](/docs/platform/runs-events-streams) for details.

### Tasks

**Tasks** are long-running, asynchronous processes. When you start a task, execution returns immediately with a `TaskInfo` containing the task ID and stream ID. The task runs in the background on the task worker.

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

```
queued → running → completed | failed | timed_out | cancelled
```

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
