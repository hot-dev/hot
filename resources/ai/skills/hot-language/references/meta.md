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
`every 5 minutes`, and `every day at 9am`.

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
        auth: "none",
    },
    on-event: "lead:signup",
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
