# openai

openai API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/openai": "1.2.7"
```

## API Base URL

`https://api.openai.com/v1`

## Documentation

Full documentation available at [hot.dev/pkg/openai](https://hot.dev/pkg/openai)

## Per-request controls

`::openai::chat-tools/chat-with-tools-options` and its streaming counterpart
accept `ChatRequestOptions` with `max-completion-tokens` and
`reasoning-effort`. The original four-argument `chat-with-tools` function
remains compatible with `::ai::chat/run-loop`.

For GPT-5.6 function tools through Chat Completions, use
`reasoning-effort: "none"`; reasoning-enabled tool loops should use the
Responses API.

## License

Apache-2.0 - see [LICENSE](LICENSE)
