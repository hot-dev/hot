---
description: "Attach metadata to Hot functions and types to define handlers, schedules, webhooks, MCP tools, agents, docs, and deploy behavior."
---

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

### Names in Metadata

A bare name in metadata refers to a definition, the same way it would in code
in that namespace. The compiler resolves it where the metadata is written:
namespace aliases are expanded and var-imports followed, so `::text/s4`, an
imported `s4`, and `::myapp::text/s4` all name the same definition. A name that
resolves to nothing is a compile error, and `hot check` reports it.

{{snippet:meta#meta-names}}

`::hot::meta/get(report-error)` returns the values those names have, whichever
namespace reads it: `cites` holds the `policy-text` string, and `clock-from` is
the `found-at` function, which you can call. The `agent`, `workflow`, and
`workflows` keys must name types.

A build records the metadata of its event handlers, schedules, webhooks, MCP
tools, agents, and workflows, in its manifest and in the database. There, each
name is the full name of its definition, such as `::myapp::text/s4`.

## Documentation

The `doc` field provides documentation for functions and types:

{{snippet:meta#meta-doc-field}}

Documentation is displayed in the Hot App dashboard and used by tooling.

## Core Functions

The `core: true` metadata marks functions and types as **globally available** without namespace qualification:

<!-- hot-check: skip -->
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

<!-- hot-check: assume ::email -->
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

<!-- hot-check: assume ::db ::monitoring ::reports -->
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
- Cron expressions: `"0 2 * * *"`, `"*/5 * * * *"` (5, 6, or 7 fields)
- Nicknames: `"@daily"`, `"@hourly"`, `"@weekly"`
- Intervals: `"every N seconds"`, `"every N minutes"`, `"every N hours"`
- Natural language: `"every day at 9:00 am"`, `"on Sunday at 12:00"`

All times are UTC. See [Schedules](/docs/schedules) for the full format reference and dynamic (runtime-created) schedules.

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

The following headers are always masked automatically: `authorization`, `cookie`, `proxy-authorization`, `set-cookie`, and any header named like a credential (see [Secret Masking](#secret-masking)). The values of the entire `auth` subtree (in `hot.request` and in the webhook `HttpRequest` argument) are also always masked. Use `secret-headers` for custom credential headers specific to your integration.

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

## Run Timeout

A run that takes longer than its timeout is recorded as failed and retried
under its `retry` policy. The worker's `worker.run-timeout` (5 minutes by
default) applies to every run. A function that needs more, or less, sets its
own with the `timeout` field, in seconds:

```hot
draft-policy
meta {on-event: "drafting:requested", timeout: 1800}
fn (event) {
  // May run for up to 30 minutes
  run-drafting-loop(event.data)
}
```

`timeout` applies to the function a run starts with: the handler of an event,
the scheduled function of a schedule, or the function a call names (from the
API, an MCP tool, a webhook, or a re-run). It has no effect on a function
called from inside a run. An alias without its own `timeout` takes its
target's.

The value must be a positive whole number; `hot check` reports any other
value as `invalid-timeout`. The worker caps it at `worker.max-run-timeout`
(1 hour by default, at most 24 hours) and says so in the failure of a run
that hits the cap. For longer work, start a [task](/docs/tasks), whose
timeout can reach 24 hours. A run keeps its last 5 seconds to record its
result, so `::hot::task/await` needs a `timeout` above 5. See
[Run Timeouts](/docs/platform/runs-events-streams#run-timeouts).

## Context Requirements

The `ctx` field declares context variables (secrets and configuration) that a function requires:

```hot
openai-key
meta {ctx: {
  "openai.api.key": {required: true},
  "rate.limit": {required: false, default: 1000, secret: false}
}}
fn (): Str {
  ::hot::ctx/get("openai.api.key")
}
```

`hot run` and `hot eval` refuse to start while a required key of a function they can reach is unset. `hot check` reports it as a `missing-context` warning instead, so projects can be checked without their secrets. Pass `--check.deny-warnings` to make it fail.

A namespace can declare `ctx` too. There it documents the keys the namespace's functions use, and generated docs list them, but nothing checks them. Declare a key on the functions that read it to have it checked.

```hot
::myapp::api ns
meta {ctx: {"openai.api.key": {required: true}}}
```

### Per-Key Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `required` | bool | `true` | Must be provided at runtime |
| `default` | any | none | Value if not provided (implies `required: false`) |
| `secret` | bool | `true` | If true, value will be masked in call records |

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

By default, all context values are considered secrets (`secret: true`). Hot keeps them, and other credentials, out of what it records about a run: call arguments, return values, errors and flows; run results and failures; alerts raised from those failures; and console emitter output. In those records a masked value reads `<secret>`.

Mark a value as `secret: false` if it's safe to log (like configuration values, rate limits, etc.). A key's declared `default` is in your source code, so it isn't treated as a secret inside longer strings, whatever `secret` says. It is still masked where it is a whole value, unless the key is `secret: false`.

Two independent filters do this.

**Registered secrets** are values the run marked as secret:

- The value of a declared secret key, once `::hot::ctx/get` reads it or `::hot::ctx/set` sets it.
- Values set with `::hot::ctx/set-secret`, from that call on, including that call's own record.
- Values passed to `::hot::ctx/mark-secret`. Use it for a credential that arrives as an ordinary argument, like a per-call `opts` Map.
- For webhooks and MCP tools, the request's `auth` subtree, its `original-url`, and its sensitive headers, including those named in `secret-headers`.

A registered value is masked wherever it appears inside a string, as itself or URL-encoded, form-encoded, JSON-escaped, base64-encoded or as a quoted CSV cell holds it, including base64 of a longer string like `Basic base64(user:secret)`. Inside a Map or Vec, each value is registered separately. So is a JSON object held in a string, like a service account key, and the credential after the scheme in `Bearer <credential>`.

The same goes for bytes: a `Bytes` value holding a registered value or a credential field is masked whole, wherever it is recorded. So are bytes rendered as numbers, as in `Bytes([83, 69, ...])`, which is how a failure's message shows them when an `Err` holds a response's `body-bytes`, and a Vec holding nothing but bytes. An `Err` that `hot run` prints as a program's result is masked too. Any other result is the program's output, and is printed as it is.

Values shorter than 8 bytes are masked only where they are the whole value, because inside other text they would match by chance. `true`, `false`, `null` and `""` are never masked.

**Keys and what they sign** are registered by the functions that use them, without anything marking them. That's the key or password given to `::hot::hmac/hmac-sha256`, `hmac-sha256-bytes`, `hmac-sha512`, `hmac-sha1` and `hmac-verify`, `::hot::crypto/rsa-sha256-sign` and `::hot::crypto/pbkdf2-hmac-sha256`, and what they return, as itself or in hex or base64. So a token signed in a run can't be rebuilt from its parts, and an HMAC you return from a run reads `<secret>` in its stored result. A run matches the first 64 of these inside longer strings, and the rest only as whole values, since a run may sign thousands of requests.

**Credential fields** are masked whether or not anything registered their values, by the field's name, in any case and in `snake_case`, `kebab-case`, `dot.case` or `camelCase`. A prefix doesn't matter: `stripe_api_key` matches `api_key`. The names, in full, are in `CREDENTIAL_FIELDS` in `crates/hot/src/lang/masking.rs`:

| Kind | Names |
|------|-------|
| Headers | `authorization`, `proxy-authorization`, `cookie`, `set-cookie` |
| Secrets | `password`, `passwd`, `passphrase`, `secret`, `client_secret`, `credential`, `credentials` |
| Keys | `api_key`, `x-api-key`, `apikey`, `api_secret`, `api_token`, `access_key`, `secret_key`, `private_key`, `signing_key`, `encryption_key` |
| Tokens | `access_token`, `refresh_token`, `id_token`, `auth_token`, `bearer_token`, `session_token`, `security_token`, `private_token`, `vault_token`, `csrf_token`, `xsrf_token` |
| OAuth | `client_assertion`, `code_verifier` |

These apply to Map fields and to fields inside strings: JSON, form-encoded `name=value` pairs and query strings, and `name: value` lines. A password in a URL (`postgres://user:password@host`) is masked too. A field whose value is `null`, a boolean or empty is left alone, since it shows only that the credential isn't there.

A bare `token`, `key` or `code` is not on the list: those names mostly hold pagination cursors and token counts, and masking them would make API traces much less useful. A credential under one of those names is masked only if it is registered, so register it with `::hot::ctx/mark-secret`.

### What Masking Doesn't Cover

Masking is a safety net for records, not a guarantee. Treat any secret that may have been recorded as exposed.

- **Before registration, for a moment.** Call rows the run has already written are masked again when it registers a secret: right away the first time, and at most every 30 seconds if it keeps registering new ones, then once more when the run ends. Until then, or if the process stops first, they hold what they held. If the database keeps refusing the update after a few tries, the run reports that its records couldn't be saved. So register a credential as soon as you have it, before passing it to anything else. Large values already spilled to blob storage aren't masked again.
- **Before registration, outside call rows.** What was already delivered to a stream or printed to the console can't be taken back.
- **Context keys no function declares.** A key is only a secret if some `meta {ctx: ...}` in the run's call graph declares it; `hot check` warns about the others. Reading an undeclared key with `::hot::ctx/get` doesn't register its value.
- **Values Hot can't recognize.** A secret that was hashed, encrypted, compressed, hex-encoded, reversed, encoded twice, split up or otherwise transformed doesn't match. Nor does a credential in free-form text under a name that isn't on the list.
- **Payloads handlers need.** Event data (`::hot::event/send` and webhook or MCP requests, in the `event` table) and task arguments and results (the `task` table) are the inputs and outputs of other code, so they are stored as they are. Don't put secrets in event data or task arguments: have the handler or task read them from the context. A value passed to another run this way isn't registered there until that run registers it. Above 1 MB as JSON, or with large strings or Bytes in it, it is stored as it is in blob storage.
- **A run's result as delivered.** The waiting webhook or MCP caller receives the run's real result, since it may be what they asked for, and `hot run` prints it. The stored `run.result` is masked, so **a caller that reads a result back through the API sees `<secret>`** for a credential field or registered secret in it.
- **Stream data.** `::hot::stream/data` and HTTP stream chunks go to their consumers with registered secrets masked, but not credential fields, which may be what a consumer asked for.
- **Large values spilled to blob storage.** Masking happens before a value is spilled, but a large value spilled before a secret was registered stays in its blob, though its record is masked and written again without it. That covers a single large string or Bytes, and any argument or return value over 1 MB as JSON.
- **Columns in raw CSV text.** A `password` column is a credential field once `from-csv` makes it a Map key, but not in the text passed to it. Mark such values with `::hot::ctx/mark-secret`, or use `iter-csv` so the text never enters a record.
- **Trace-level logs.** `HOT_LOG_LEVEL=trace` logs values as the VM handles them. Don't use it where logs are shared.

### Runtime Functions

Use these functions to access context values at runtime:

```hot
// Get a context value
api-key ::hot::ctx/get("openai.api.key")

// Set a context value
::hot::ctx/set("my.config", "value")

// Set a secret value (explicitly marks as secret for masking)
::hot::ctx/set-secret("api.token", token-value)

// Mask a value without storing it in the context, for a credential passed in
api-key ::hot::ctx/mark-secret(opts.api-key)
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
| `timeout: N` | Seconds a run of this function may take |
| `ctx: {...}` | Declare required context variables (secrets) |
