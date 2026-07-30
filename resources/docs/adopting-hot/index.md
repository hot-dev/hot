---
description: "Adopt Hot incrementally beside an existing application by moving one webhook, event, schedule, queue worker, agent, or container job at a time."
---

# Adopting Hot in an Existing Application

Hot is designed to live beside the application you already have. You do not
need to move your API, database, frontend, or every background job at once.
Start with one workflow boundary that is difficult to run, retry, or debug
today.

This guide covers an incremental adoption path for existing JavaScript,
Python, Go, Rust, Java, and other applications.

## Choose a Good First Workflow

The best first workflow is operationally meaningful but isolated enough to
move safely:

- A webhook that performs several downstream actions
- A cron job that needs history, alerts, or retries
- A queue worker whose failures are difficult to diagnose
- An AI agent loop that needs durable tools, memory, or streaming
- A browser, media, OCR, or data job that needs an isolated container
- A multi-step process already connected by application events

Avoid starting with the broadest or most latency-sensitive path in your
system. The first goal is to evaluate the Hot development and operating model,
not prove that every backend concern belongs in Hot.

## 1. Install and Initialize

Install Hot, then initialize it in the existing repository:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh

cd my-existing-app
hot init
```

`hot init` adds `hot.hot`, `hot/`, and the gitignored `.hot/` directory. Your
existing source and configuration remain in place.

Run the local platform:

```bash
hot dev --open
```

This starts the API, scheduler, worker, and Hot App. See
[Getting Started](/docs/getting-started) for the complete setup path.

## 2. Define the Boundary

Treat the event payload or function arguments as a contract between your
application and Hot.

For example, an existing application can publish a `customer:created` event:

```json
{
  "event_type": "customer:created",
  "event_data": {
    "id": "cus_123",
    "email": "new@example.com",
    "plan": "starter"
  }
}
```

The first Hot handler can own one downstream action:

```hot
::myapp::customers ns

send-welcome-email meta {
  doc: "Send the first product email to a new customer",
  on-event: "customer:created",
  retry: {attempts: 5, delay: 1000, backoff: "exponential"},
}
fn (event) {
  customer event.data
  deliver-welcome-email(customer.email, customer.plan)
}
```

Keep event names and payloads explicit. Add identifiers needed for
idempotency, correlation, authorization, and debugging at the boundary rather
than fetching them implicitly from unrelated process state.

## 3. Connect the Existing Application

Applications can publish events through the Hot HTTP API:

```bash
curl -X POST http://localhost:4681/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "event_type": "customer:created",
    "event_data": {
      "id": "cus_123",
      "email": "new@example.com",
      "plan": "starter"
    }
  }'
```

For application code, use an [official Hot SDK](/docs/api/sdks) for
JavaScript/TypeScript, Python, Go, Rust, or Java. SDKs can publish events, call
Hot functions, and subscribe to streams without hand-building each request.

Authenticated Hot clients belong on trusted servers. Do not expose a Hot API
key in browser or mobile client code.

## 4. Make Side Effects Idempotent

Hot events use at-least-once delivery. A handler may receive the same logical
event more than once because of retries or infrastructure redelivery.

For side effects such as charging a card, sending an email, or provisioning an
account:

1. Put a stable idempotency key in the event payload.
2. Pass it to an external API when that API supports idempotency.
3. Otherwise record completion in the system that owns the side effect.
4. Return the existing result when the same key is seen again.

```hot
charge-customer meta {
  on-event: "billing:charge-requested",
  retry: 3,
}
fn (event) {
  key event.data.idempotency-key
  existing find-charge(key)
  if(
    is-some(existing),
    existing,
    create-charge(event.data, key),
  )
}
```

See [Durable Execution](/docs/platform/durability) for delivery, retries,
event lineage, and long-running task behavior.

## 5. Test the Operational Path

Exercise success, failure, and retry behavior locally:

```bash
hot test
hot dev --open
```

For the first migrated workflow, verify:

- The source application can publish the event or call the function.
- Payload validation fails clearly when required fields are missing.
- A transient failure retries with the expected policy.
- A duplicate event cannot repeat a protected side effect.
- Inputs, results, failures, and intermediate values appear in Hot App.
- Downstream events preserve the identifiers needed to follow the chain.
- Alerts reach the intended destination when retries are exhausted.

## 6. Cut Over Gradually

Choose a rollout method based on the side effect:

### Shadow

Publish the event to Hot while the existing worker remains authoritative.
Let the Hot handler validate, transform, or calculate without performing the
final side effect. Compare results before switching ownership.

### Dual-read

Let Hot process the workflow while both the old and new observability paths are
available. Keep only one path authorized to perform non-idempotent actions.

### Narrow cutover

Move a small cohort, event type, tenant, or scheduled invocation to Hot. Expand
after successful runs and failure recovery have been observed.

Do not run two independently authorized implementations of a payment,
notification, or provisioning side effect unless both share a proven
idempotency boundary.

## Common Migration Patterns

### Cron job to schedule

Move the job body into a Hot function and add `schedule` metadata:

```hot
daily-account-sync meta {
  schedule: "every day at 2am",
  retry: {attempts: 3, backoff: "exponential"},
}
fn (event) {
  sync-accounts()
}
```

Use Hot App for run history and [Alerts](/docs/alerts) for failure
notifications.

### Queue worker to event handler

Publish a domain event through the API or an SDK, then attach one or more Hot
handlers with `on-event`. Each handler becomes an independently persisted and
retryable run.

See [Events & Handlers](/docs/events).

### HTTP webhook to Hot webhook

Add `webhook` metadata to a function, validate the incoming request, and emit
an internal event for downstream work. This keeps the externally visible
response path short while durable handlers perform slower side effects.

See [Webhooks](/docs/webhooks).

### Long-running worker to task

Use a [code task](/docs/tasks) for long-running Hot code with messaging and
checkpoints. Use a [container task](/docs/box) when the job needs a browser,
system binary, Python environment, media tool, or custom OCI image.

### AI loop to agent

Define typed agent identity and attach handlers, schedules, webhooks, tools,
and memory patterns. Keep model calls and side effects in observable functions
with explicit event or tool boundaries.

See [Agents](/docs/agents) and the [Hot Chat demo](/docs/demos/hot-chat).

## When to Move the Next Workflow

Expand Hot's boundary when the first workflow demonstrates a clear improvement
in at least one of these areas:

- Less queue, worker, scheduler, or deployment infrastructure to operate
- Faster diagnosis through run, event, and expression traces
- Safer recovery through independent retries or task checkpoints
- A clearer contract between application code and background work
- Reusable tools, packages, events, or workflow patterns
- A simpler path from local development to production

If the workflow remains simpler and clearer in the existing application, keep
it there. Hot should own the work that benefits from its execution and
observability model.

## Next Steps

- [Getting Started](/docs/getting-started)
- [Official SDKs](/docs/api/sdks)
- [Events & Handlers](/docs/events)
- [Durable Execution](/docs/platform/durability)
- [CI/CD](/docs/ci-cd)
- [Hot Cloud Pricing](/pricing)
