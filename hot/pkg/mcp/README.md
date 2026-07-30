# mcp

MCP client for connecting to Model Context Protocol servers from Hot. It supports
the stateless 2026-07-28 protocol and the legacy initialize-based protocols on
the same API.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/mcp": "1.4.0"
```

This package depends on `hot.dev/json-rpc` for JSON-RPC 2.0 message framing (installed automatically).

## Usage

### Connect to an MCP Server

`connect` probes `server/discover` and uses 2026-07-28 when available. It only
falls back to the legacy initialize handshake when the endpoint is not
recognizable as a modern server.

```hot
::mcp ::mcp::client
::types ::mcp::types

session ::mcp/connect(
  "https://my-server.example.com/mcp",
  types/ClientInfo({name: "my-app", version: "1.0.0"}),
  types/ClientCapabilities({elicitation: {form: {}}}),
  types/ConnectionOptions({
    headers: {"Authorization": "Bearer ..."},
    log-level: "info"
  })
)

println(session.server-info.name)
println(session.protocol-version)
```

Use `::mcp/initialize(...)` when a caller must explicitly use the legacy
handshake. `ConnectionOptions.mode` can also be set to `"modern"` or
`"legacy"` to pin an era.

### Discover and Call Tools

```hot
::tools ::mcp::tools

tools ::tools/list(session)
for-each(tools, fn (tool) { println(tool.name) })

result ::tools/call(session, "get_weather", {location: "Portland"})
println(first(result.content).text)
```

Modern calls automatically emit `MCP-Protocol-Version`, `Mcp-Method`,
`Mcp-Name`, and any valid `x-mcp-header` parameter headers required by the
listed tool definition.

### Fulfill Multi Round-Trip Input

The 2026-07-28 protocol returns server-to-client requests as an
`input_required` result. `call-with-input` handles the retry loop, creates a
fresh JSON-RPC ID for every round, and echoes the opaque request state.

```hot
result ::tools/call-with-input(
  session,
  "publish_report",
  {report-id: "rpt-42"},
  fn (request) {
    cond {
      eq(request.method, "elicitation/create") => {
        {action: "accept", content: {confirmed: true}}
      }
      => { err(`Unsupported input request: ${request.method}`) }
    }
  },
  3
)
```

### Stream a Tool Call

For long-running tools, use `call-stream` to receive progress notifications
and the final result as SSE events.

```hot
::tools ::mcp::tools

for-each(::tools/call-stream(session, "long_analysis", {query: "..."}), fn (event) {
  cond {
    eq(get(event.data, "method"), "notifications/progress") => {
      println(`Progress: ${event.data.params.message}`)
    }
    is-some(get(event.data, "result")) => {
      println(`Done: ${event.data.result}`)
    }
    => { null }
  }
})
```

### Read Resources

```hot
::resources ::mcp::resources

resources ::resources/list(session)
for-each(resources, fn (r) { println(`${r.name}: ${r.uri}`) })

contents ::resources/read(session, "file:///project/README.md")
println(first(contents).text)
```

### Get Prompts

```hot
::prompts ::mcp::prompts

prompts ::prompts/list(session)

messages ::prompts/get-prompt(session, "code-review", {language: "hot", code: "add(1, 2)"})
for-each(messages, fn (m) { println(`${m.role}: ${m.content.text}`) })
```

### Listen for Modern Notifications

```hot
::subscriptions ::mcp::subscriptions
::types ::mcp::types

events ::subscriptions/listen(
  session,
  ::types/SubscriptionFilter({tools-list-changed: true})
)
for-each(events, fn (event) { println(event.data) })
```

The helper checks each opt-in against the capabilities returned by discovery.

### Paginate Through All Tools

```hot
::tools ::mcp::tools

// Automatic: fetches all pages
all-tools ::tools/list-all(session)

// Manual: page by page
first-page ::tools/list-page(session, null)
if(is-some(first-page.next-cursor),
  ::tools/list-page(session, first-page.next-cursor),
  null)
```

### End-to-End Example

```hot
::myapp::agent ns

::mcp ::mcp::client
::tools ::mcp::tools
::types ::mcp::types

run fn () {
  // Connect to an MCP server
  session ::mcp/connect(
    "https://my-server.example.com/mcp",
    types/ClientInfo({name: "my-app", version: "1.0.0"}),
    null
  )

  println(`Connected to ${session.server-info.name}`)

  // List available tools
  tools ::tools/list(session)
  println(`${length(tools)} tools available`)

  // Find and call a specific tool
  tool first(filter(tools, fn (t) { eq(t.name, "search") }))
  if(is-some(tool), {
    result ::tools/call(session, tool.name, {query: "hello world"})
    println(first(result.content).text)
  }, {
    println("search tool not found")
  })
}
```

## Modules

| Module | Description |
|--------|-------------|
| `::mcp::client` | Automatic discovery, legacy initialization, request codecs, and MRTR helpers |
| `::mcp::tools` | Tool listing, validated routing headers, calls, MRTR, and SSE streaming |
| `::mcp::resources` | Resource listing, reading, caching metadata, and MRTR |
| `::mcp::prompts` | Prompt listing, retrieval, caching metadata, and MRTR |
| `::mcp::subscriptions` | Capability-gated 2026-07-28 notification streams |
| `::mcp::types` | All MCP type definitions (Session, Tool, Resource, Prompt, etc.) |

## Protocol

This package implements the [Model Context Protocol](https://modelcontextprotocol.io/)
2026-07-28 stateless Streamable HTTP protocol and remains compatible with
initialize-based Streamable HTTP servers using 2025-11-25, 2025-06-18, or
2025-03-26. The Hot server separately retains its deprecated 2024-11-05
HTTP+SSE endpoints for older clients. The package uses `hot.dev/json-rpc` for
JSON-RPC 2.0 framing.

## Documentation

- [MCP Specification](https://spec.modelcontextprotocol.io/)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/mcp)

## License

Apache-2.0
