# mcp

MCP client for connecting to Model Context Protocol servers from Hot. Supports tool discovery and invocation, resource reading, prompt retrieval, and SSE streaming.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/mcp": "1.3.1"
```

This package depends on `hot.dev/json-rpc` for JSON-RPC 2.0 message framing (installed automatically).

## Usage

### Connect to an MCP Server

```hot
::mcp ::mcp::client
::types ::mcp::types

session ::mcp/initialize(
  "https://my-server.example.com/mcp",
  types/ClientInfo({name: "my-app", version: "1.0.0"}),
  null
)

println(session.server-info.name)
```

### Discover and Call Tools

```hot
::tools ::mcp::tools

tools ::tools/list(session)
for-each(tools, fn (tool) { println(tool.name) })

result ::tools/call(session, "get_weather", {location: "Portland"})
println(first(result.content).text)
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
  session ::mcp/initialize(
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
| `::mcp::client` | Session initialization, ping, internal request helpers |
| `::mcp::tools` | Tool listing (with pagination), calling, and SSE streaming |
| `::mcp::resources` | Resource listing, reading, and URI template discovery |
| `::mcp::prompts` | Prompt listing and retrieval with arguments |
| `::mcp::types` | All MCP type definitions (Session, Tool, Resource, Prompt, etc.) |

## Protocol

This package implements the [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) Streamable HTTP transport (spec revision 2025-11-25). It uses the `hot.dev/json-rpc` package for JSON-RPC 2.0 message framing.

## Documentation

- [MCP Specification](https://spec.modelcontextprotocol.io/)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/mcp)

## License

Apache-2.0
