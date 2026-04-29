# Events and Event Handlers

Events are the primary way to trigger asynchronous work in Hot. Event handlers run when specific events occur, enabling decoupled, scalable workflows.

## Event Handlers

Define event handlers using the `on-event` metadata:

```hot
::myapp::users ns

// Handle user creation events
on-user-created meta {on-event: "user:created"}
fn (event) {
  // event.data contains the event payload
  send-welcome-email(event.data.email)
  create-default-settings(event.data.id)
}

```

Event handlers can be grouped under an [agent](/docs/agents) by adding `agent: TypeName` to the metadata. This enables per-agent run tracking, health metrics, and observability in the Hot App.

## Event Schemas

Hot events appear in two related shapes:

### 1) Hot language shape (`send`)

When publishing from Hot code, the event shape is:

| Field | Type | Description |
|-------|------|-------------|
| `type` | `Str` | Event name (for example `"user:created"`) |
| `data` | `Any` | Event payload |

`send("user:created", {...})` uses this shape.

### 2) HTTP API shape (`POST /v1/events`)

When publishing through the Hot API, the request body is:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_type` | `string` | Yes | Event name |
| `event_data` | `json` | Yes | Event payload |
| `stream_id` | `uuid` | No | Append to an existing stream instead of creating a new one |

Response payload fields include:

| Field | Description |
|-------|-------------|
| `event_id` | Published event UUID |
| `stream_id` | Stream UUID containing this event |
| `event_type` | Event name |
| `event_data` | Event payload |
| `event_time` | Event timestamp |

## System Events in `hot-std`

`hot-std` defines built-in handlers for several reserved `hot:*` event types. These power core platform behavior.

| Event Type | Purpose | Expected Payload (`event.data`) |
|------------|---------|----------------------------------|
| `hot:call` | Execute a function asynchronously | `{fn: "::ns/var", args: [...]}` |
| `hot:schedule` | Internal scheduler trigger for scheduled functions | `{fn: "::ns/var", args: [...]}` |
| `hot:schedule:new` | Create a dynamic one-time or recurring schedule | `{fn: "::ns/var", args: [...], schedule: "..."}` |
| `hot:schedule:cancel` | Cancel a dynamic schedule | `{schedule-id: "uuid"}` or `{fn: "::ns/var"}` |

These handlers are defined in `hot/pkg/hot-std/src/hot/lang.hot`.

> The `hot:` namespace is reserved for system behavior. Prefer your own event namespace (for example `user:created`, `billing:invoice-paid`) for application events.

## Sending Events

Send events from your code to trigger handlers:

```hot
// Send an event (send is a core function)
send("user:created", {
  id: user-id,
  email: email,
  name: name
})
```

Or send events via the Hot API:

```bash
curl -X POST https://api.hot.dev/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"event_type": "user:created", "event_data": {"id": "123", "email": "user@example.com"}}'
```

## Background Jobs

Any function can be executed as a background job by sending a `hot:call` event:

```bash
# Execute a function asynchronously via event
curl -X POST https://api.hot.dev/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "event_type": "hot:call",
    "event_data": {
      "fn": "::myapp::jobs/process-order",
      "args": [{"order_id": "12345"}]
    }
  }'
```

Or from Hot code:

```hot
// Queue a background job
send("hot:call", {
  fn: "::myapp::jobs/process-order",
  args: [{order_id: "12345"}]
})
```

The response includes an `event_id`. You can retrieve runs triggered by the event:

```bash
curl https://api.hot.dev/v1/events/$EVENT_ID/runs \
  -H "Authorization: Bearer $HOT_API_KEY"
```

## Retries

Event handlers can retry automatically when they fail:

```hot
on-payment-received meta {
  on-event: "payment:received",
  retry: 3
}
fn (event) {
  update-account-balance(event.data)
}
```

For full retry configuration (attempts, delay, backoff, jitter, and limits), see [Retries](/docs/retries).
