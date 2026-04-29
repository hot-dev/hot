# Metadata

Metadata in Hot uses the `meta` keyword to attach information to functions, types, and namespaces. This powers documentation, testing, event handling, scheduling, and more.

## Syntax

Metadata comes in two forms:

### Map Form

Use `meta {...}` for key-value metadata:

{{snippet:meta#meta-map-form}}

Multiple fields:

{{snippet:meta#meta-multiple-fields}}

### Vector Form

Use `meta [...]` for simple tags:

{{snippet:meta#meta-vector-form}}

## Documentation

The `doc` field provides documentation for functions and types:

{{snippet:meta#meta-doc-field}}

Documentation is displayed in the Hot App dashboard and used by tooling.

## Core Functions

The `core: true` metadata marks functions and types as **globally available** without namespace qualification:

```hot
// In ::hot::math namespace
add
meta {core: true, doc: "Add two numbers"}
fn (a: Int, b: Int): Int {
  // ...
}
```

Now `add` can be called from any namespace without the `::hot::math/` prefix:

```hot
::myapp::calculator ns

// No need for ::hot::math/add
result add(1, 2)
```

### Making Your Functions Core

You can mark your own functions as core too:

```hot
::myapp::utils ns

// This function will be available everywhere in your project
format-currency
meta {core: true, doc: "Format a number as currency"}
fn (amount: Dec): Str {
  `$${amount}`
}
```

```hot
::myapp::orders ns

// Use without namespace prefix
total format-currency(99.99)
```

This is useful for utility functions you use throughout your codebase.

## Test Functions

Mark functions as tests with `meta ["test"]`:

{{snippet:meta#meta-test-functions}}

Run tests with `hot test`.

## Event Handlers

The `on-event` field registers a function as an event handler:

```hot
send-welcome-email
meta {on-event: "user:created"}
fn (event) {
  ::email/send({
    to: event.data.email,
    subject: "Welcome!",
    body: `Welcome, ${event.data.name}!`
  })
}
```

When a `user:created` event is sent, this handler runs automatically.

## Scheduled Functions

The `schedule` field runs functions on a schedule:

```hot
cleanup-old-sessions
meta {schedule: "@daily"}
fn (event) {
  ::db/delete-expired-sessions()
}

send-heartbeat
meta {schedule: "every 30 seconds"}
fn (event) {
  ::monitoring/ping()
}

generate-report
meta {schedule: "every 1 hour"}
fn (event) {
  ::reports/generate-hourly()
}
```

Schedule formats:
- `"@daily"`, `"@hourly"`, `"@weekly"`
- `"every N seconds"`, `"every N minutes"`, `"every N hours"`

## MCP Tools

The `mcp` field exposes a function as a [Model Context Protocol](/docs/mcp) tool, making it callable by AI models and agents:

```hot
get-weather
meta {
  mcp: {
    service: "weather",
    description: "Get current weather for a city"
  }
}
fn (city: Str): Map {
  ::http/get(`https://api.weather.com/current?city=${city}`).body
}
```

The `mcp` value is a map with these fields:

| Field | Required | Description |
|-------|----------|-------------|
| `service` | Yes | Groups tools into a named service with its own endpoint |
| `auth` | No | `"required"` (default) or `"none"`. Controls whether Hot validates credentials before invocation. |
| `name` | No | Override the auto-generated tool name |
| `description` | No | Human-readable description (helps AI choose the right tool) |
| `title` | No | Display title |
| `input-schema` | No | Override auto-generated input JSON Schema |
| `output-schema` | No | Override auto-generated output JSON Schema |
| `annotations` | No | MCP behavioral hints (`readOnlyHint`, `destructiveHint`, etc.) |

Input and output schemas are automatically generated from the function's type signature. Tools are grouped by `service` and accessible via the MCP endpoint at `/mcp/{org}/{env}/{service}`.

See [MCP Services](/docs/mcp) for the full reference on services, schemas, endpoints, and best practices.

## Webhook Endpoints

The `webhook` field exposes a function as a [webhook endpoint](/docs/webhooks), allowing external services to send HTTP requests to your Hot functions:

```hot
on-slack-event
meta {
  webhook: {
    service: "slack",
    path: "/events",
    description: "Handle incoming Slack events"
  }
}
fn (request: HttpRequest): HttpResponse {
  event from-json(request.body-raw)
  handle-event(event)
  HttpResponse({status: 200, body: {ok: true}})
}
```

The `webhook` value is a map with these fields:

| Field | Required | Description |
|-------|----------|-------------|
| `service` | Yes | Groups endpoints into a named service (part of the URL) |
| `path` | Yes | URL path within the service (e.g., `/events`) |
| `method` | No | HTTP method to match (default: `POST`) |
| `name` | No | Override the auto-generated endpoint name |
| `description` | No | Human-readable description |
| `auth` | No | `"none"` (default, public) or `"required"` (requires Bearer token — API key, service key, or session) |

Webhook endpoints are public by default and receive an `HttpRequest` with the full HTTP request details (from `::hot::http`). Return an `HttpResponse` to control the status code, headers, and body—all fields except `status` are optional.

See [Webhooks](/docs/webhooks) for the full reference on authentication, signature verification, and best practices.

## Secret Headers

The `secret-headers` field is a top-level meta field (not nested under `mcp` or `webhook`) that declares additional HTTP header names whose values should be masked in run logs. It works for both MCP tools and webhook handlers:

```hot
list-invoices
meta {
  mcp: {service: "billing", auth: "none"},
  secret-headers: ["x-api-key"]
}
fn (status: Str?): Vec { ... }

stripe-payment
meta {
  webhook: {service: "stripe", path: "/payment"},
  secret-headers: ["stripe-signature"]
}
fn (request: HttpRequest): HttpResponse { ... }
```

The following headers are always masked automatically: `authorization`, `cookie`, `proxy-authorization`, `set-cookie`. The entire `auth` subtree (in `hot.request` and in the webhook `HttpRequest` argument) is also always masked. Use `secret-headers` for custom credential headers specific to your integration.

## Retry Configuration

Event handlers and scheduled functions can automatically retry on failure using the `retry` field:

### Simple Format

Just specify the number of retry attempts (uses default 1 second delay):

```hot
process-payment
meta {on-event: "payment:process", retry: 3}
fn (event) {
  // Will retry up to 3 times on failure
  charge-card(event.data)
}
```

### Full Format

Specify custom attempts, delay, and advanced options:

```hot
sync-external-data
meta {
  schedule: "@hourly",
  retry: {
    attempts: 5,
    delay: 10000,
    backoff: "exponential",
    max_delay: 300000,
    jitter: true
  }
}
fn (event) {
  // Will retry up to 5 times with exponential backoff
  // Starting at 10s, doubling each attempt, capped at 5 minutes
  fetch-and-sync()
}
```

See [Retries](/docs/retries) for retry fields, backoff behavior, and platform limits.

## Context Requirements

The `ctx` field declares context variables (secrets and configuration) that a namespace requires:

```hot
::myapp::api ns
meta {ctx: {
  "openai.api.key": {required: true},
  "rate.limit": {required: false, default: 1000, secret: false}
}}
```

### Per-Key Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `required` | bool | `true` | Must be provided at runtime |
| `default` | any | none | Value if not provided (implies `required: false`) |
| `secret` | bool | `true` | If true, value will be masked in call logs |

### Examples

**Required secret (most common):**
```hot
meta {ctx: {"anthropic.api.key": {required: true}}}
```

**Optional with default (non-secret):**
```hot
meta {ctx: {"rate.limit": {default: 60, secret: false}}}
```

**Multiple keys:**
```hot
meta {ctx: {
  "aws.access_key_id": {required: true},
  "aws.secret_access_key": {required: true},
  "aws.region": {required: false, default: "us-east-1", secret: false}
}}
```

### Secret Masking

By default, all context values are considered secrets (`secret: true`). When a function calls `::hot::ctx/get` to retrieve a secret, the return value is masked as `"<secret>"` in the call database to prevent accidental exposure.

Mark a value as `secret: false` if it's safe to log (like configuration values, rate limits, etc.).

### Runtime Functions

Use these functions to access context values at runtime:

```hot
// Get a context value
api-key ::hot::ctx/get("openai.api.key")

// Set a context value
::hot::ctx/set("my.config", "value")

// Set a secret value (explicitly marks as secret for masking)
::hot::ctx/set-secret("api.token", token-value)
```

## Namespace Metadata

You can also attach metadata to namespaces:

```hot
::myapp::test::users meta ["test"] ns

// All functions in this namespace are test-related
```

## Combining Metadata

Combine multiple metadata fields in one map:

```hot
process-order
meta {
  doc: "Process an incoming order",
  on-event: "order:created",
  core: true
}
fn (event) {
  // ...
}
```

## Summary

| Metadata | Purpose |
|----------|---------|
| `doc: "..."` | Documentation |
| `core: true` | Globally available without namespace |
| `meta ["test"]` | Mark as test function |
| `on-event: "name"` | Event handler |
| `schedule: "..."` | Scheduled execution |
| `mcp: {...}` | Expose as MCP tool |
| `webhook: {...}` | Expose as webhook endpoint |
| `retry: N` or `retry: {...}` | Automatic retry on failure |
| `ctx: {...}` | Declare required context variables (secrets) |
