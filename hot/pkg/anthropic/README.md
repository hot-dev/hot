# anthropic

anthropic API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/anthropic": "1.3.0"
```

## API Base URL

`https://api.anthropic.com`

## Documentation

Full documentation available at [hot.dev/pkg/anthropic](https://hot.dev/pkg/anthropic)

## Per-call credentials

Every typed function takes a trailing `opts` Map that wins over ctx for that call, so one run can serve many tenants. Nothing is written to ctx.

| `opts` key | ctx key | Notes |
| --- | --- | --- |
| `api-key` | `anthropic.api.key` | Sent in the `x-api-key` header |
| `base-url` | `anthropic.base.url` | For proxies and gateways. In `opts` it needs `api-key` in `opts` too: the ctx key is never sent to a per-call URL |
| `api-version` | `anthropic.api.version` | The `anthropic-version` header. Default `2023-06-01` |

```hot
answer ::anthropic::messages/chat("claude-sonnet-4-5", "Hello!", {api-key: tenant-key})
models ::anthropic::models/list({api-key: tenant-key})
```

A blank or null key in `opts` is an error, never a fallback to ctx, so a tenant with a missing key can't run on your project's key. Leave the key out of `opts` to use ctx.

Hot stores the arguments of every function call in the run's call records and only masks ctx secrets, so a key passed in `opts` is stored there as plain text.

To use a per-call key in `::ai::chat/run-loop`, which needs `hot.dev/hot-ai` 1.9.0 in your project:

```hot
opts ::ai::chat/ChatOptions({
    chat-opts-fn: ::anthropic::chat-tools/chat-with-tools,
    chat-stream-opts-fn: ::anthropic::chat-tools/chat-with-tools-stream,
    model: "claude-sonnet-4-5",
    provider-opts: {api-key: tenant-key}
})
answer ::ai::chat/run-loop(opts, "What is 17 + 25?")
```

## Per-request controls

`::anthropic::chat-tools/chat-with-tools-options` and its streaming
counterpart accept `ChatRequestOptions` with `max-tokens` and `effort`. The
original four-argument `chat-with-tools` function remains compatible with
`::ai::chat/run-loop`.

Claude Opus 5 enables thinking by default, and thinking plus visible text
share `max_tokens`; use an output ceiling appropriate to the selected effort.

## License

Apache-2.0 - see [LICENSE](LICENSE)
