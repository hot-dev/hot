---
description: "Expose Hot functions as webhooks with request handling, authentication, deployment, and event-driven follow-up work."
---

# Webhooks

Webhooks allow external services to send HTTP requests to your Hot functions. When a service like Slack, Stripe, or GitHub needs to notify your application of an event, it sends an HTTP request to a webhook URL. Hot receives the request, runs your function, and returns the response.

Webhook endpoints are defined using the `webhook` metadata on Hot functions. When you deploy your code, Hot automatically registers these functions as webhook endpoints and serves them via public HTTP routes.

## Defining Webhook Endpoints

Add `webhook` metadata to any function to expose it as a webhook endpoint. The required fields are `service` (a logical grouping) and `path` (the URL path within that service).

### Basic Example

```hot
::myapp::slack ns

on-slack-event
meta {webhook: {service: "slack", path: "/events"}}
fn (request: HttpRequest): HttpResponse {
  // Process the incoming Slack event
  HttpResponse({status: 200, body: {ok: true}})
}
```

This registers a `POST /events` endpoint under the `slack` service. External services send HTTP requests to the webhook URL, and Hot calls your function with an `HttpRequest` (from `::hot::http`).

### Full Example with All Options

```hot
::myapp::payments ns

::stripe-wh ::stripe::webhooks

stripe-payment
meta {
  webhook: {
    service: "stripe",
    path: "/payment",
    method: "POST",
    name: "stripe_payment_handler",
    description: "Handle Stripe payment webhook events",
    auth: "required"
  }
}
fn (request: HttpRequest): HttpResponse {
  // Verify the request came from Stripe before trusting it
  // (see "Provider Signature Verification" below)
  if(not(::stripe-wh/verify-request(request)),
    HttpResponse({status: 401, body: {error: "Invalid signature"}}),
    {
      event from-json(request.body-raw)
      process-payment(event)
      HttpResponse({status: 200, body: {received: true}})
    })
}
```

Webhook handlers can be grouped under an [agent](/docs/agents) by adding `agent: TypeName` to the metadata. This enables per-agent run tracking, health metrics, and observability in the Hot App.

## Metadata Fields

The `webhook` metadata is a map with the following fields:

| Field | Required | Description |
|-------|----------|-------------|
| `service` | Yes | Groups endpoints into a named service. Part of the webhook URL. Must be URL-safe (alphanumeric, hyphens, underscores, dots). |
| `path` | Yes | The URL path for this endpoint within the service (e.g., `/events`, `/payment`). |
| `method` | No | HTTP method to match. Defaults to `POST`. Can be `GET`, `PUT`, `PATCH`, `DELETE`, or `POST`. |
| `name` | No | Override the auto-generated endpoint name. Defaults to `namespace_function_name`. |
| `description` | No | Human-readable description of what the endpoint does. |
| `auth` | No | Authentication mode: `"none"` (default, public) or `"required"` (requires Bearer token — API key, service key, or session). |

## HttpRequest and HttpResponse

Webhook handlers use the standard `HttpRequest` and `HttpResponse` types from `::hot::http`.

### HttpRequest

Your function receives an `HttpRequest` with the full details of the incoming HTTP request:

```hot
HttpRequest type {
  method: Str,          // HTTP method (GET, POST, etc.)
  url: Str,             // Request URL path (internal, token-free — safe to log)
  original-url: Str?,   // The URL as the caller requested it, pre-rewrite
  headers: Map?,        // HTTP headers (lowercase keys)
  query: Map?,          // Query string parameters
  body: Any?,           // Parsed body (JSON-decoded if applicable)
  body-raw: Str?,       // Raw request body as a string
  body-bytes: Bytes?,   // Verbatim body bytes (only when not valid UTF-8)
  ip: Str?,             // Client IP address (from proxy headers)
  auth: Map?            // Caller identity (when auth is "required")
}
```

When Hot delivers a webhook request, all common fields are populated. The `ip` field is extracted from `x-forwarded-for` or `x-real-ip` proxy headers. The `auth` field is only present when the endpoint has `auth: "required"` and the caller authenticates successfully — see [Caller Identity](#caller-identity-hotrequest) for the full structure.

Two fields exist specifically for signature verification. `original-url` is the full URL the provider actually called — scheme, host, path (webhook token included), and query string intact — which is what providers that sign their request URL (Twilio, HubSpot) hash; it contains the webhook token, so don't log it. `body-bytes` carries the verbatim body bytes only when the body is not valid UTF-8 (in that case `body-raw` is a lossy conversion); use `::hot::http/raw-body(request)` to get the right one without checking.

This is the same `HttpRequest` type used by [MCP tools](/docs/mcp#caller-identity-hotrequest) (via `hot.request`), providing a unified request representation across both systems.

### HttpResponse

Return an `HttpResponse` to control the HTTP response sent back to the caller:

```hot
HttpResponse type {
  status: Int,     // HTTP status code (200, 201, 404, etc.)
  headers: Map?,   // Response headers (optional)
  body: Any?       // Response body (will be JSON-encoded, optional)
}
```

Only `status` is required. Omit `headers` and `body` when not needed (e.g., a `204 No Content` response).

If your function returns a plain value (not an `HttpResponse`), Hot wraps it as a `200 OK` JSON response automatically.

```hot
// These are equivalent:
fn (request: HttpRequest): HttpResponse {
  HttpResponse({status: 200, body: {ok: true}})
}

fn (request: HttpRequest): Map {
  {ok: true}  // Automatically becomes 200 JSON response
}
```

## Webhook URL

Once deployed, your webhook endpoints are available at:

```
https://api.hot.dev/webhook/{org-slug}/{env-name}/{service}/{path}/{token}
```

For local development with `hot dev`:

```
http://localhost:4681/webhook/local/development/{service}/{path}/{token}
```

The final `{token}` segment is a per-endpoint secret generated by Hot. It makes the URL unguessable, so spoofers can't hit your endpoint by knowing only the service and path. Copy the complete URL — including the token — from the **Webhooks** view in the Hot App; requests with a missing or wrong token are rejected.

### Examples

| Metadata | URL |
|----------|-----|
| `service: "slack", path: "/events"` | `https://api.hot.dev/webhook/my-org/production/slack/events/{token}` |
| `service: "stripe", path: "/payment"` | `https://api.hot.dev/webhook/my-org/production/stripe/payment/{token}` |
| `service: "github", path: "/push"` | `https://api.hot.dev/webhook/my-org/staging/github/push/{token}` |

The URL includes both the organization slug and environment name, so you can have separate webhook endpoints for development, staging, and production.

### Custom Domain URLs

If you have a [custom domain](/docs/domains) configured for your environment, you can use shorter URLs that omit the org and env:

```
https://your-domain.com/webhook/{service}/{path}/{token}
```

The organization and environment are resolved from the domain automatically. Both the standard URL and the custom domain URL work identically—the custom domain version is just shorter and branded.

| Metadata | Default URL | Custom Domain URL |
|----------|------------|-------------------|
| `service: "slack", path: "/events"` | `https://api.hot.dev/webhook/my-org/production/slack/events/{token}` | `https://hooks.acme.com/webhook/slack/events/{token}` |
| `service: "stripe", path: "/payment"` | `https://api.hot.dev/webhook/my-org/production/stripe/payment/{token}` | `https://hooks.acme.com/webhook/stripe/payment/{token}` |

The Hot App dashboard shows a domain selector when custom domains are available, letting you switch between the default URL and your custom domain URLs.

## Authentication

Webhook endpoints are **public by default**—no API key is required. This is necessary because external services (Slack, Stripe, GitHub, etc.) cannot provide your API key when sending webhook requests.

### Optional API Key Authentication

For webhooks where you control the sender, you can require authentication:

```hot
internal-hook
meta {
  webhook: {
    service: "internal",
    path: "/sync",
    auth: "required"
  }
}
fn (request: HttpRequest): Map {
  // Only accessible with a valid credential
  sync-data(request.body)
}
```

When `auth` is set to `"required"`, the caller must include an `Authorization: Bearer <token>` header. The token can be an API key, service key, or session token. The credential must have a `webhook` permission (e.g., `{"webhook:*": ["execute"]}` or `{"webhook:internal/*": ["execute"]}`).

Authenticated webhook handlers receive the caller's identity in the `auth` field of the `HttpRequest` argument. The same data is also available via `::hot::ctx/get("hot.request")` for consistency with MCP tools.

### Caller Identity (`hot.request`)

Every webhook invocation — authenticated or not — populates the `hot.request` context variable with the same `HttpRequest` that your function receives as its argument. Access it via `::hot::ctx/get("hot.request")`.

When the endpoint requires authentication, `hot.request.auth` (and `request.auth`) contains the caller's identity:

```hot
internal-sync
meta {webhook: {service: "internal", path: "/sync", auth: "required"}}
fn (request: HttpRequest): Map {
  request.auth.type                      // "api-key" | "service-key" | "session"
  request.auth.service-key.meta          // service key metadata (if service key)
  sync-data(request.body)
}
```

### Secret Headers

Certain HTTP headers are automatically masked in run logs to prevent credential leakage: `authorization`, `cookie`, `proxy-authorization`, and `set-cookie`. Values from the `auth` subtree are always masked as well.

If your webhook receives custom credentials via headers, declare them in the top-level `secret-headers` metadata so they are also masked:

```hot
stripe-payment
meta {
  webhook: {service: "stripe", path: "/payment"},
  secret-headers: ["stripe-signature"]
}
fn (request: HttpRequest): HttpResponse {
  sig get(request.headers, "stripe-signature", "")
  // sig value is masked in run logs
  process-payment(request.body)
  HttpResponse({status: 200, body: {received: true}})
}
```

Non-sensitive headers (like `content-type`, `user-agent`) and other request fields (`method`, `url`, `query`, `ip`) are **not** masked, so they remain visible in run logs for debugging.

### Provider Signature Verification

For external providers, authenticate requests by verifying their cryptographic signature in your Hot code. Most providers (Slack, Stripe, GitHub, etc.) sign webhook payloads using HMAC-SHA256 or similar.

```hot
::slack ::slack::webhooks
::ctx ::hot::ctx

on-slack-event
meta {webhook: {service: "slack", path: "/events"}}
fn (request: HttpRequest): HttpResponse {
  // Verify the request is from Slack
  signing-secret ::ctx/get("slack.signing.secret")
  if(not(::slack/verify-request(request, signing-secret)), {
    HttpResponse({status: 401, body: {error: "Invalid signature"}})
  }, {
    // Process the verified event
    event from-json(request.body-raw)
    handle-slack-event(event)
    HttpResponse({status: 200, body: {ok: true}})
  })
}
```

Hot's provider packages ship `verify-request` functions with the correct recipe for each provider: Slack, Stripe, GitHub, Shopify, WhatsApp, and Discord verify over the payload (and timestamp, where the provider signs one), while Twilio and HubSpot — which sign the URL they call — verify against `request.original-url` automatically. Each takes the secret from a context variable in its 1-arity form or explicitly in its 2-arity form, and the timestamp-checking verifiers accept a replay-window tolerance (0 disables it).

For providers without a package, write a verifier with the fail-closed helpers in `::hot::hmac` (`hmac-verify-hex`, `hmac-verify-base64` — constant-time, false on malformed input) and `::hot::time/within-seconds-of-now` for replay windows, hashing `::hot::http/raw-body(request)`.

## API Key Permissions

API keys can be restricted to only allow webhook access, and further restricted to specific services:

| Permission | Format | Access |
|------------|--------|--------|
| Full Access | `{"*:*": ["*"]}` | Unrestricted access to all API endpoints including webhooks |
| Webhooks (all services) | `{"webhook:*": ["execute"]}` | Webhook endpoint access for all services |
| Webhooks (specific service) | `{"webhook:internal": ["execute"]}` | Webhook endpoint access for a specific service only |

For example, an API key with permission `{"webhook:internal": ["execute"]}` can only call webhook endpoints in the `internal` service. It cannot access other services or any non-webhook API endpoints.

Permissions are configured when creating or editing API keys in the Hot App. See [Hot App > API Keys](/docs/app#api-keys) for details.

## Execution Model

Webhook handlers execute **synchronously**. When a request arrives, Hot calls your function and waits for it to return before sending the HTTP response. This is important for services like Slack that require a response within 3 seconds.

For long-running work, acknowledge the webhook immediately and process asynchronously using `send()`:

```hot
on-slack-event
meta {webhook: {service: "slack", path: "/events"}}
fn (request: HttpRequest): HttpResponse {
  // Acknowledge immediately
  event from-json(request.body-raw)
  send("slack:event:received", event.data)

  // Return 200 right away (processing happens in event handler)
  HttpResponse({status: 200, body: {ok: true}})
}

// Separate event handler does the heavy lifting
process-slack-event
meta {on-event: "slack:event:received", retry: 3}
fn (event) {
  // This runs asynchronously with retries
  do-expensive-work(event.data)
}
```

## Lifecycle

Webhook endpoints are driven by metadata in your source code:

1. **Define**: Add `webhook` metadata to functions in your Hot code
2. **Deploy**: Run `hot deploy` (or `hot dev` for local development)
3. **Configure**: Give the webhook URL to the external service
4. **Receive**: External services send HTTP requests; Hot calls your function and returns the response

When you redeploy, the endpoint registry updates automatically. If a function's `webhook` metadata is removed, the endpoint is unregistered.

## Retries

The `retry` metadata does **not** apply to webhook invocations. Webhooks use a synchronous request/response model—the caller sends a request and waits for the result. If the function fails, the error is returned immediately as a 500 response. The calling service can then decide whether to retry.

Use the `send()` pattern shown above to defer work to event handlers that support server-side retries.

## Best Practices

**Respond quickly.** Many webhook providers have strict timeouts (Slack requires a response within 3 seconds). Acknowledge the webhook immediately and defer heavy processing to event handlers.

**Verify signatures.** Always verify webhook signatures for external providers. Never trust incoming requests without verification—webhook URLs are public and can receive spoofed requests.

**Use the raw body for signature verification.** Signature verification requires the exact bytes the sender signed — never `request.body` (which is parsed). Use `::hot::http/raw-body(request)`: it returns `body-raw` (the original string) for normal payloads and `body-bytes` for non-UTF-8 payloads, where `body-raw` alone would be lossy.

**Return appropriate status codes.** Return `200` for success, `401` for authentication failures, and `400` for bad requests. Many providers will retry on `5xx` errors, so only return 500 for genuine failures.

```hot
// Good: specific status codes
HttpResponse({status: 200, body: {ok: true}})
HttpResponse({status: 401, body: {error: "Invalid signature"}})
HttpResponse({status: 400, body: {error: "Missing event type"}})
```

**Group related endpoints by service.** Use meaningful service names that match the provider or domain:

```hot
meta {webhook: {service: "slack", path: "/events"}}
meta {webhook: {service: "slack", path: "/commands"}}
meta {webhook: {service: "stripe", path: "/payment"}}
meta {webhook: {service: "github", path: "/push"}}
```
