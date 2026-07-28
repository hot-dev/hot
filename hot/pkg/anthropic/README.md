# anthropic

anthropic API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/anthropic": "1.2.3"
```

## API Base URL

`https://api.anthropic.com`

## Documentation

Full documentation available at [hot.dev/pkg/anthropic](https://hot.dev/pkg/anthropic)

## Per-request controls

`::anthropic::chat-tools/chat-with-tools-options` and its streaming
counterpart accept `ChatRequestOptions` with `max-tokens` and `effort`. The
original four-argument `chat-with-tools` function remains compatible with
`::ai::chat/run-loop`.

Claude Opus 5 enables thinking by default, and thinking plus visible text
share `max_tokens`; use an output ceiling appropriate to the selected effort.

## License

Apache-2.0 - see [LICENSE](LICENSE)
