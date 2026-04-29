# MCP Services

The [Model Context Protocol](https://modelcontextprotocol.io) (MCP) is an open standard that allows AI models and agents to discover and invoke tools. Hot functions can be exposed as MCP tools, making them callable by any MCP-compatible client—such as Claude, Cursor, or custom AI agents.

MCP tools are defined using the `mcp` metadata on Hot functions. When you deploy your code, Hot automatically registers these functions as MCP tools, generates JSON schemas from their type signatures, and serves them via a standards-compliant MCP endpoint.

## Defining MCP Tools

Add `mcp` metadata to any function to expose it as an MCP tool. The only required field is `service`, which groups related tools together.

### Basic Example

```hot
::myapp::weather ns

get-forecast
meta {mcp: {service: "weather"}}
fn (city: Str, days: Int): Map {
  ::http/get(`https://api.weather.com/forecast?city=${city}&days=${days}`).body
}

get-current
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

This registers two tools under the `weather` service. An MCP client connecting to the `weather` service endpoint will discover both `get-forecast` and `get-current` as available tools.

### Full Example with All Options

```hot
::myapp::geo ns
meta {ctx: {"geocode.api.key": {required: true}}}

::http ::hot::http
::ctx ::hot::ctx

lookup-address
meta {
  mcp: {
    service: "geo-tools",
    name: "lookup-address",
    title: "Address Lookup",
    description: "Geocode an address and return coordinates, timezone, and formatted address",
    annotations: {
      readOnlyHint: true,
      openWorldHint: true
    }
  }
}
fn (address: Str, country-code: Str): Map {
  api-key ::ctx/get("geocode.api.key")
  ::http/get(`https://api.geocode.com/v1/search?q=${address}&cc=${country-code}&key=${api-key}`).body
}
```

Note how `::hot::ctx/get` is used to retrieve an API key stored as a [context variable](/docs/app#context-variables). This keeps secrets out of your source code—configure them per environment in the Hot App.

## Metadata Fields

The `mcp` metadata is a map with the following fields:

| Field | Required | Description |
|-------|----------|-------------|
| `service` | Yes | Groups tools into a named service. Clients connect to a specific service endpoint. |
| `auth` | No | `"required"` (default) or `"none"`. Controls whether Hot validates credentials before invocation. See [Authentication](#authentication). |
| `name` | No | Override the auto-generated tool name. Defaults to `namespace_function_name` with hyphens normalized to underscores (e.g., `::myapp::weather`'s `get-forecast` becomes `myapp_weather_get_forecast`). |
| `description` | No | Human-readable description of what the tool does. Helps AI models choose the right tool. |
| `title` | No | Display title for the tool. |
| `input-schema` | No | Override the auto-generated input JSON Schema. By default, Hot generates this from the function's parameter types. |
| `output-schema` | No | Override the auto-generated output JSON Schema. By default, Hot generates this from the function's return type. |
| `icons` | No | Tool icons for display in MCP clients. |
| `annotations` | No | MCP tool annotations providing behavioral hints to clients. |

### Annotations

The `annotations` field follows the MCP specification for tool annotations. These provide hints to clients about the tool's behavior:

| Annotation | Type | Description |
|------------|------|-------------|
| `readOnlyHint` | Bool | Tool does not modify any state |
| `destructiveHint` | Bool | Tool may perform destructive operations (delete, overwrite) |
| `idempotentHint` | Bool | Calling with same args multiple times has same effect as once |
| `openWorldHint` | Bool | Tool interacts with external systems beyond the server |

```hot
safe-lookup
meta {
  mcp: {
    service: "my-service",
    annotations: {
      readOnlyHint: true,
      openWorldHint: false
    }
  }
}
fn (id: Str): Map {
  ::db/get("records", id)
}
```

## Auto-Generated Schemas

Hot automatically generates JSON Schema for your MCP tool's input and output based on the function's type signature. You rarely need to provide schemas manually.

```hot
// Hot automatically generates the input schema from these typed parameters
search-users
meta {
  mcp: {
    service: "users",
    description: "Search users by name and role"
  }
}
fn (name: Str, role: Str, active: Bool): Vec {
  ::db/query("SELECT * FROM users WHERE name LIKE ? AND role = ? AND active = ?",
    [name, role, active])
}
```

The generated input schema would be:

```json
{
  "type": "object",
  "properties": {
    "name": {"type": "string"},
    "role": {"type": "string"},
    "active": {"type": "boolean"}
  },
  "required": ["name", "role", "active"]
}
```

Custom types are also resolved automatically:

```hot
SearchParams type { query: Str, page: Int, per-page: Int }

search
meta {mcp: {service: "search"}}
fn (params: SearchParams): Vec {
  // ...
}
```

## Services

Services are the organizational unit for MCP tools. Each service gets its own MCP endpoint and groups related tools together.

### Naming Conventions

Choose meaningful service names that describe the domain:

```hot
// Good: descriptive service names
meta {mcp: {service: "weather"}}
meta {mcp: {service: "user-management"}}
meta {mcp: {service: "data-analytics"}}

// Avoid: generic or overly broad names
meta {mcp: {service: "tools"}}
meta {mcp: {service: "api"}}
```

### Multiple Services

A single project can expose tools across multiple services. Functions in different namespaces can belong to the same service, and functions in the same namespace can belong to different services:

```hot
::myapp::users ns

// Both in the "admin" service
list-users
meta {
  mcp: {
    service: "admin",
    description: "List all users"
  }
}
fn (): Vec { ::db/query("SELECT * FROM users") }

create-user
meta {
  mcp: {
    service: "admin",
    description: "Create a new user"
  }
}
fn (name: Str, email: Str): Map { ::db/insert("users", {name: name, email: email}) }
```

```hot
::myapp::reports ns

// In a separate "reports" service
generate-report
meta {
  mcp: {
    service: "reports",
    description: "Generate a usage report"
  }
}
fn (start-date: Str, end-date: Str): Map { ... }
```

## MCP Endpoint

Once deployed, your MCP tools are available at:

```
https://api.hot.dev/mcp/{org-slug}/{env-name}/{service}
```

For local development with `hot dev`, the default org slug is `local` and the default environment is `development`:

```
http://localhost:4681/mcp/local/development/{service}
```

### Custom Domain URLs

If you have a [custom domain](/docs/domains) configured for your environment, you can use shorter URLs that omit the org and env:

```
https://your-domain.com/mcp/{service}
```

The organization and environment are resolved from the domain automatically. Both the standard URL and the custom domain URL work identically—the custom domain version is just shorter and branded.

The Hot App dashboard shows a domain selector when custom domains are available, letting you switch between the default URL and your custom domain URLs.

### Authentication

By default, MCP tools require authentication via the `Authorization` header:

```
Authorization: Bearer YOUR_TOKEN
```

Hot supports multiple auth modes per tool, controlled by the `auth` field in the `mcp` metadata. The default is `"required"`.

MCP service URLs are public identifiers, not secrets. Clients can call `tools/list` on a reachable service endpoint to discover tool names, descriptions, and input schemas. Tool execution still follows each tool's `auth` setting, so keep the default `"required"` unless you intentionally want a public tool.

#### API Keys

The standard authentication method. API keys are environment-scoped and can be restricted to specific MCP services via [permissions](#api-key-permissions).

```hot
// Default — auth: "required" (API key, service key, or session)
get-forecast
meta {mcp: {service: "weather"}}
fn (city: Str): Map {
  ::http/get(`https://api.weather.com/forecast?city=${city}`).body
}
```

#### Service Keys

[Service keys](/docs/authentication#service-keys) are long-lived, permission-scoped credentials for your customers. Attach metadata (e.g., customer ID, plan) to a service key, and it's available at runtime via `hot.request`:

```hot
get-usage
meta {mcp: {service: "billing", description: "Get usage for the calling customer"}}
fn (): Map {
  req ::hot::ctx/get("hot.request")
  customer-id req.auth.service-key.meta.customer_id
  fetch-usage-for(customer-id)
}
```

See [Caller Identity](#caller-identity-hotrequest) for the full `hot.request` structure.

#### Public Tools

Set `auth: "none"` to make a tool publicly accessible — no credentials required.

```hot
hash-text
meta {mcp: {service: "utils", auth: "none"}}
fn (text: Str): Str {
  ::hot::hash/sha256(text)
}
```

#### Pass-Through Auth

`auth: "none"` also enables pass-through auth patterns, where the MCP endpoint is open but your function extracts client-provided credentials from HTTP headers and relays them to downstream APIs. The caller's headers are available via `hot.request.headers`:

```hot
chat
meta {
  mcp: {
    service: "ai-proxy",
    auth: "none",
    description: "Chat with an LLM. Requires x-api-key header with your OpenAI key."
  }
}
fn (model: Str, message: Str): Map {
  req ::hot::ctx/get("hot.request")
  api-key get(req.headers, "x-api-key")
  if(is-null(api-key), fail("x-api-key header required"))

  ::http/post("https://api.openai.com/v1/chat/completions", {
    headers: {"Authorization": `Bearer ${api-key}`},
    body: {model: model, messages: [{role: "user", content: message}]}
  }).body
}
```

This is useful for BYOK (bring your own key) patterns where your customers provide their own API keys for third-party services.

#### Auth Modes Summary

| `auth` value | Default | Behavior |
|---|---|---|
| `"required"` | Yes | Hot validates credentials (API key, service key, or session). `hot.request.auth` contains caller identity. |
| `"none"` | No | No Hot credential check. Use for public tools or pass-through auth via `hot.request.headers`. |

#### Secret Headers

Certain HTTP headers are automatically masked in run logs to prevent credential leakage: `authorization`, `cookie`, `proxy-authorization`, and `set-cookie`. Values from the `hot.request.auth` subtree are always masked as well.

If your tool receives custom credentials via headers (e.g., `x-api-key`), declare them in the top-level `secret-headers` metadata so they are also masked:

```hot
list-invoices
meta {
  mcp: {service: "billing", auth: "none"},
  secret-headers: ["x-api-key", "x-customer-secret"]
}
fn (status: Str?): Vec {
  req ::ctx/get("hot.request")
  api-key req.headers.x-api-key
  // api-key value is masked in run logs
  ...
}
```

Non-sensitive headers (like `content-type`, `user-agent`) and other `hot.request` fields (`method`, `url`, `query`, `ip`) are **not** masked, so they remain visible in run logs for debugging.

### Protocol

The endpoint implements MCP over JSON-RPC 2.0 and supports both MCP transport styles:

| Transport | Endpoint(s) | Notes |
|-----------|-------------|-------|
| Streamable HTTP (2025-03-26) | `POST /mcp/{org}/{env}/{service}` | Modern MCP transport. Requests and responses use one HTTP endpoint. `tools/call` may return `text/event-stream`. |
| HTTP+SSE (2024-11-05, legacy) | `GET /mcp/{org}/{env}/{service}` + `POST /mcp/{org}/{env}/{service}/messages?sessionId=...` | Legacy transport for older clients. `GET` opens SSE and returns an `endpoint` event; `POST` sends JSON-RPC messages; responses arrive on the SSE stream. |

Supported methods:

| Method | Description |
|--------|-------------|
| `initialize` | Initialize the MCP session, returns server capabilities |
| `ping` | Health/liveness check, returns empty object `{}` |
| `notifications/initialized` | Client acknowledges initialization |
| `tools/list` | List all available tools for this service |
| `tools/call` | Execute a tool with arguments. Response may be JSON or SSE (`event: message` containing JSON-RPC payloads). |

### Timeouts

- `mcp.timeout` controls Streamable HTTP `tools/call` execution timeout (default: `60` seconds).
- `mcp.legacy.session-timeout` controls legacy HTTP+SSE session lifetime for `GET /mcp/{org}/{env}/{service}` (default: `300` seconds).

### Example: Connecting with curl

For local development, replace the base URL with `http://localhost:4681/mcp/local/development/{service}`.

```bash
# Initialize session
curl -X POST https://api.hot.dev/mcp/my-org/production/weather \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-03-26",
      "capabilities": {},
      "clientInfo": {"name": "my-client", "version": "1.0"}
    }
  }'

# List available tools
curl -X POST https://api.hot.dev/mcp/my-org/production/weather \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}'

# Call a tool
curl -X POST https://api.hot.dev/mcp/my-org/production/weather \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
      "name": "myapp_weather_get_forecast",
      "arguments": {"city": "San Francisco", "days": 5}
    }
  }'
```

### Example: Streaming `tools/call` Response (Streamable HTTP)

For long-running tool calls, use `-N` so curl prints SSE chunks as they arrive:

```bash
curl -N -X POST https://api.hot.dev/mcp/my-org/production/weather \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 4,
    "method": "tools/call",
    "params": {
      "name": "myapp_weather_get_forecast",
      "arguments": {"city": "San Francisco", "days": 5}
    }
  }'

# Example output
# event: message
# data: {"jsonrpc":"2.0","method":"notifications/message", ...}
#
# event: message
# data: {"jsonrpc":"2.0","id":4,"result":{...}}
```

### Example: Legacy HTTP+SSE Transport

```bash
# 1) Open SSE stream and capture the endpoint event
curl -N https://api.hot.dev/mcp/my-org/production/weather \
  -H "Authorization: Bearer $HOT_API_KEY"

# First SSE event:
# event: endpoint
# data: /mcp/my-org/production/weather/messages?sessionId=<uuid>

# 2) POST JSON-RPC to the messages endpoint from the endpoint event
curl -X POST "https://api.hot.dev/mcp/my-org/production/weather/messages?sessionId=<uuid>" \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'

# 3) Read JSON-RPC responses from the existing SSE stream
```

### Connecting AI Clients

Most MCP-compatible AI clients can connect directly to your Hot MCP endpoint. Configure them with:

- **URL**: `https://api.hot.dev/mcp/{org}/{env}/{service}` (or `https://your-domain.com/mcp/{service}` with a [custom domain](#custom-domain-urls))
- **Transport**: Streamable HTTP (preferred) or HTTP+SSE (legacy clients)
- **Authentication**: Bearer token (API key, service key, or session). Not required for tools with `auth: "none"`.

## Caller Identity (`hot.request`)

When an MCP tool is invoked, Hot automatically populates the `hot.request` context variable with an `::hot::http/HttpRequest` containing HTTP request details and caller identity. Access it via `::hot::ctx/get("hot.request")`.

This is the same `HttpRequest` type used by [webhooks](/docs/webhooks#httprequest-and-httpresponse). For MCP, `body` and `body-raw` are `null` (the tool arguments come from the MCP protocol, not the HTTP body).

### Structure

```hot
::ctx ::hot::ctx

req ::ctx/get("hot.request")

// HTTP context — always present
req.method             // "POST"
req.url                // "/mcp/my-org/production/weather"
req.headers            // Map<Str, Str> — all HTTP headers (lowercased keys)
req.query              // Map<Str, Str> — query string parameters
req.ip                 // client IP address

// Auth context — present when auth: "required" (default)
req.auth.type                  // "api-key" | "service-key" | "session"
req.auth.service-key.id        // service key UUID (when type = "service-key")
req.auth.service-key.name      // service key name (when type = "service-key")
req.auth.service-key.meta      // service key metadata (when type = "service-key")

// When auth: "none"
req.auth                       // null
```

### Using Headers

HTTP headers are available on all requests regardless of auth mode. Header keys are lowercased:

```hot
get-forecast
meta {mcp: {service: "weather"}}
fn (city: Str): Map {
  req ::hot::ctx/get("hot.request")
  region or(get(req.headers, "x-region"), "us-east-1")
  fetch-forecast(city, region)
}
```

For `auth: "none"` tools, headers are the mechanism for pass-through auth:

```hot
proxy-api
meta {mcp: {service: "proxy", auth: "none"}}
fn (endpoint: Str): Map {
  req ::hot::ctx/get("hot.request")
  token get(req.headers, "authorization")
  if(is-null(token), fail("Authorization header required"))
  ::http/get(endpoint, {headers: {"Authorization": token}}).body
}
```

### Using Metadata for Customer Context

Service key metadata is the recommended way to pass customer context into your Hot functions. When you create a service key for a customer and attach metadata (e.g., `{"customer_id": "acme-123", "plan": "enterprise"}`), that metadata is automatically decrypted and available at runtime:

```hot
::myapp::billing ns

::ctx ::hot::ctx

get-usage
meta {mcp: {service: "billing", description: "Get usage for the calling customer"}}
fn (): Map {
  req ::ctx/get("hot.request")
  customer-id req.auth.service-key.meta.customer_id
  fetch-usage-for(customer-id)
}
```

This lets you build multi-tenant MCP services where each customer's service key carries their identity, and your functions can use it to scope data access, enforce limits, or customize behavior — without requiring the customer to pass their own ID as a parameter.

### Security

Sensitive values in `hot.request` are automatically masked in run logs. Specifically:

- The entire `auth` subtree (credential type, service key metadata, etc.)
- Values of sensitive HTTP headers: `authorization`, `cookie`, `proxy-authorization`, `set-cookie`
- Values of any headers declared in `secret-headers` metadata

Non-sensitive fields like `method`, `url`, `query`, `ip`, and non-sensitive headers remain visible in run logs for debugging.

## API Key Permissions

API keys can be restricted to only allow MCP access, and further restricted to specific services:

| Permission | Format | Access |
|------------|--------|--------|
| Full Access | `{"*:*": ["*"]}` | Unrestricted access to all API endpoints including MCP |
| MCP (all services) | `{"mcp:*": ["execute"]}` | MCP tool invocation for all services |
| MCP (specific service) | `{"mcp:weather": ["execute"]}` | MCP tool invocation for a specific service only |

For example, an API key with permission `{"mcp:weather": ["execute"]}` can only invoke tools in the `weather` service. It cannot access other services or any non-MCP API endpoints.

Permissions are configured when creating or editing API keys in the Hot App. See [Hot App > API Keys](/docs/app#api-keys) for details.

## Lifecycle

MCP tools are driven by metadata in your source code:

1. **Define**: Add `mcp` metadata to functions in your Hot code
2. **Deploy**: Run `hot deploy` (or `hot dev` for local development)
3. **Discover**: MCP clients connect and call `tools/list` to discover available tools
4. **Invoke**: Clients call `tools/call` to execute tools; Hot runs the function and returns the result

When you redeploy, the tool registry updates automatically. If a function's `mcp` metadata is removed, the tool is unregistered. If it's added back, the tool reappears—API key permissions that reference the service are preserved across these changes.

## Retries

The `retry` metadata does **not** apply to MCP tool invocations. MCP uses a synchronous request/response model—the client sends a `tools/call` request and waits for the result. If the function fails, the error is returned immediately to the MCP client. The client can then decide whether to retry.

This differs from event handlers and scheduled functions, which run asynchronously and benefit from server-side retries. If you have a function that serves as both an MCP tool and an event handler, the `retry` configuration will apply to event-triggered runs but not to MCP-triggered runs.

```hot
// retry applies to event handler runs, not MCP tool calls
process-data
meta {
  on-event: "data:received",
  mcp: {service: "data"},
  retry: 3
}
fn (data: Map): Map {
  transform-and-store(data)
}
```

## Best Practices

**Write clear descriptions.** AI models rely on tool descriptions to decide which tool to use. Be specific about what the tool does, what it returns, and any side effects.

```hot
// Good: specific and informative
meta {
  mcp: {
    service: "crm",
    description: "Search contacts by name, email, or company. Returns up to 50 matching contacts sorted by relevance."
  }
}

// Avoid: vague
meta {
  mcp: {
    service: "crm",
    description: "Search contacts"
  }
}
```

**Use typed parameters.** Hot auto-generates JSON Schema from your function signatures. Well-typed parameters produce better schemas, which help AI models provide correct arguments.

```hot
// Good: typed parameters with clear names
fn (customer-id: Str, start-date: Str, end-date: Str, include-refunds: Bool): Vec { ... }

// Less helpful: untyped
fn (params: Map): Map { ... }
```

**Group related tools into a service.** Keep services focused on a single domain. This makes it easy to grant targeted API key permissions and helps clients discover related tools.

**Use annotations for safety hints.** Mark read-only tools as `readOnlyHint: true` and destructive tools as `destructiveHint: true` so AI clients can make informed decisions about tool usage.
