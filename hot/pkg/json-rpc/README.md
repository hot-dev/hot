# json-rpc

JSON-RPC 2.0 client for Hot. Supports standard request/response over HTTP and SSE streaming.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/json-rpc": "1.1.0"
```

## Usage

### Send a Request

```hot
::rpc ::json-rpc::client

req ::rpc/request("ping", {})
response ::rpc/send("https://server.example.com/rpc", {}, req)
println(response.result)
```

### Send a Request with Custom Headers

```hot
::rpc ::json-rpc::client

headers {Authorization: "Bearer my-api-key"}
req ::rpc/request("users/list", {limit: 10})
response ::rpc/send("https://server.example.com/rpc", headers, req)
println(response.result)
```

### Send a Notification

Notifications are fire-and-forget — no response is expected.

```hot
::rpc ::json-rpc::client

::rpc/notify(
  "https://server.example.com/rpc",
  {},
  ::rpc/notification("status/update", {ready: true})
)
```

### SSE Streaming

Use `send-stream` for long-running operations where the server sends incremental
events via Server-Sent Events.

```hot
::rpc ::json-rpc::client

req ::rpc/request("long/operation", {data: "..."})
response ::rpc/send-stream("https://server.example.com/rpc", {}, req)

// response.body is an iterator of parsed SSE events
for-each(response.body, fn (event) {
  println(event.data)
})
```

### Parse a Response

```hot
::rpc ::json-rpc::client

// Success response
response ::rpc/parse-response({jsonrpc: "2.0", id: "abc", result: {ok: true}})
println(response.result) // {ok: true}

// Error response — returns err()
response ::rpc/parse-response({jsonrpc: "2.0", id: "abc", error: {code: -32601, message: "Method not found"}})
// is-err(response) => true
```

## Modules

| Module | Description |
|--------|-------------|
| `::json-rpc::client` | Request builders, HTTP transport, SSE streaming |
| `::json-rpc::types` | Message types (Request, Response, Notification, Error) and standard error codes |

## Documentation

- [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/json-rpc)

## License

Apache-2.0
