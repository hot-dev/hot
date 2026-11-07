# anthropic

anthropic API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/anthropic": "1.4.0"
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

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

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
counterpart accept `ChatRequestOptions` with `max-tokens`, `effort` and
`cache-control`. The original four-argument `chat-with-tools` function remains
compatible with `::ai::chat/run-loop`.

Claude Opus 5 enables thinking by default, and thinking plus visible text
share `max_tokens`; use an output ceiling appropriate to the selected effort.

### Prompt caching

A tool loop sends its whole prompt again on every call. Set `cache-control` to
read what doesn't change from Anthropic's cache:

```hot
options ::anthropic::chat-tools/ChatRequestOptions({
    max-tokens: 16384,
    cache-control: {type: "ephemeral"},   // or {type: "ephemeral", ttl: "1h"}
})
```

It marks the system prompt, which caches the tools with it, and the
conversation, whose mark moves forward each turn. Keep what changes between
calls out of the system prompt: a change there misses the cache for everything
after it. A prefix under the model's minimum isn't cached, and that isn't an
error.

`reply.usage.input-tokens` counts the whole prompt either way.
`cache-read-tokens` and `cache-write-tokens` say how much of it the cache
served and took, so a read count of zero on a later call means the prefix
changed.

## License

Apache-2.0 - see [LICENSE](LICENSE)
