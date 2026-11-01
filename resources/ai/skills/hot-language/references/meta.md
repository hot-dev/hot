# Hot Metadata Reference

Metadata attaches platform behavior and documentation to namespaces, functions,
types, and aliases.

## Basic Forms

```hot
// Tags
test-add meta ["test"]
fn () { assert-eq(3, add(1, 2)) }

// Key-value metadata
greet
meta {doc: "Greets a user."}
fn (name: Str): Str { `Hello ${name}` }
```

## Tests, Events, Schedules, and Retries

```hot
on-user-created
meta {on-event: "user:created", retry: 3}
fn (event) {
    send-welcome-email(event.data.email)
}

daily-cleanup
meta {
    schedule: "@daily",
    retry: {
        attempts: 5,
        delay: 10000,
        backoff: "exponential",
        max_delay: 300000,
        jitter: true,
    },
}
fn (event) { cleanup() }
```

Common schedule strings include `@daily`, `@hourly`, `every 30 seconds`,
`every 5 minutes`, and `every day at 9am`. A metadata schedule must repeat:
since Hot 3.0.1, a single run such as `"in 10 minutes"`, `"2h"` or
`"2026-11-01"` is an `invalid-schedule` error. To run a function once, send a
`hot:schedule:new` event with `fn`, `args` and `schedule`.

`retry: 3` retries a failed run up to 3 times, 1 second apart; the map form
sets `attempts`, `delay` (ms), `backoff` (`"fixed"`, `"exponential"` or
`"linear"`), `max_delay` and `jitter`. Each retry is a new run linked to the
first through `origin_run_id`, so a handler with side effects must be safe to
run again.

### Run Timeouts

Since Hot 3.0.1, `timeout` (whole seconds) overrides the worker's run timeout
(5 minutes by default, capped by `worker.max-run-timeout`) for the function a
run starts with: an event handler, a scheduled function, or a called
function. It has no effect on functions called inside the run. A run that
times out fails and is retried under its `retry` policy. Use a task for work
longer than an hour.

```hot
draft-policy
meta {on-event: "drafting:requested", timeout: 1800, retry: 1}
fn (event) { run-drafting-loop(event.data) }
```

### Testing Events, Schedules and Time

Since Hot 3.0.1, `hot test` keeps what a test `send`s instead of publishing it,
and no handler runs on its own. The test drives them with `::hot::test`:

- `set-now(time)` fixes the clock (an RFC 3339 string, `Instant` or
  `ZonedDateTime`); `advance(duration)` moves it; `clear-now()` restores the
  real time. Every "now" function reads the fixed time.
- `deliver(event)` runs the `on-event` handlers of `{type, data}`, then
  delivers what they send until nothing is left; `deliver()` delivers what
  was sent so far. It returns one `{type, fn, result}` per handler run.
- A delivered `hot:schedule:new` arms a schedule (relative times count from
  when it was sent); `schedules()` lists them, and `run-due()` calls each one
  due by the clock, with the clock at its due time.

A handler that fails counts as a failed assertion; retries don't apply.

```hot
::store ::hot::store

steps ::store/Map({name: "steps"})

on-step-opened
meta {on-event: "step:opened"}
fn (event: Map) {
    ::store/put(steps, event.data.id, {status: "open"})
    send("hot:schedule:new", {fn: "::myapp::steps/check-due", args: [event.data.id], schedule: "in 2 days"})
}

check-due fn (id: Str) {
    step ::store/get(steps, id)
    if(eq(step.status, "open"), ::store/put(steps, id, {status: "late"}), null)
}

test-a-step-goes-late
meta ["test"]
fn () {
    ::hot::test/set-now("2026-10-14T09:00:00Z")
    ::hot::test/deliver({type: "step:opened", data: {id: "s-1"}})
    ::hot::test/advance(::hot::time/days(3))
    ::hot::test/run-due()
    assert-eq("late", ::store/get(steps, "s-1").status)
}
```

## Webhooks

```hot
HttpRequest ::hot::http/HttpRequest
HttpResponse ::hot::http/HttpResponse

signup-webhook
meta {
    webhook: {
        service: "leads",
        path: "/signup",
        method: "POST",
    },
    secret-headers: ["x-signature"],
}
fn (request: HttpRequest): HttpResponse {
    lead request.body
    send("lead:new", lead)
    HttpResponse({status: 200, body: {ok: true}})
}
```

Webhook handlers receive `HttpRequest`. Use `request.body` for parsed JSON and
`request.body-raw` when signature verification needs the exact bytes.

## MCP Tools

```hot
search-orders
meta {
    mcp: {
        service: "support",
        name: "search_orders",
        description: "Search customer orders by ID or status.",
    },
}
fn (params: Map): Vec<Map> {
    search-order-store(or(params.query, ""))
}
```

## Secret Headers

`secret-headers` is a top-level meta field (not nested under `mcp` or `webhook`)
that masks additional request headers in run logs. `authorization`, `cookie`,
`proxy-authorization`, and `set-cookie` are always masked automatically:

```hot
list-invoices
meta {
    mcp: {service: "billing"},
    secret-headers: ["x-api-key"],
}
fn (status: Str?): Vec<Map> { ... }

stripe-webhook
meta {
    webhook: {service: "stripe", path: "/payment"},
    secret-headers: ["stripe-signature"],
}
fn (request: HttpRequest): HttpResponse { ... }
```

## Context Requirements

Declare required secrets/configuration in metadata, then read values with
`::hot::ctx/get`:

```hot
call-api
meta {
    ctx: {
        "api.base_url": {
            required: false,
            default: "https://api.example.com",
            secret: false,
        },
        "api.key": {required: true},
    },
}
fn (path: Str): Map {
    base ::hot::ctx/get("api.base_url")
    key ::hot::ctx/get("api.key")
    request-api(base, key, path)
}
```

Context values are secret by default. Set `secret: false` only for values safe
to log.

## Function Aliases with Metadata

Aliases can carry metadata and then point at another function. This is useful
when one implementation needs multiple platform entry points:

```hot
handle-message fn (request: HttpRequest): HttpResponse {
    HttpResponse({status: 200, body: {ok: true}})
}

public-message-webhook
meta {webhook: {service: "chat", path: "/message"}}
handle-message
```

Define the function before the alias. An alias is a namespace value, and a
namespace value can't refer to a definition later in the file: the project
fails to load with `Variable 'handle-message' not found`, even though
`hot check` reports no issues.
