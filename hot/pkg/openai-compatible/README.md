# openai-compatible

Generic client for OpenAI-compatible APIs. One package for every provider that speaks the OpenAI wire format — **OpenRouter**, Together, Fireworks, DeepSeek, Groq, local **Ollama**, and private gateways — giving you broad model access, fallback, and cost experimentation without one package per provider.

## Setup

| Context Variable | Description |
|---|---|
| `openai-compatible.base.url` | e.g. `https://openrouter.ai/api/v1` (constants provided) |
| `openai-compatible.api.key` | The provider's API key |

## Usage

```hot
::chat ::openai-compatible::chat

response ::chat/chat-completion(::chat/ChatCompletionRequest({
  model: "anthropic/claude-sonnet-4.5",
  messages: [{role: "user", content: "Hello!"}],
  // OpenRouter extras (ignored elsewhere): fallback + routing
  models: ["anthropic/claude-sonnet-4.5", "openai/gpt-5.2"]
}))
response.choices[0].message.content

// Streaming
stream ::chat/chat-completion-stream(request)
for-each(stream.body, (event) { ...event.data.choices[0].delta... })

// Models and embeddings
::openai-compatible::models/list-models()
::openai-compatible::embeddings/create-embeddings(...)
```

Known base URLs ship as constants: `::openai-compatible/OPENROUTER_BASE_URL`, `TOGETHER_BASE_URL`, `FIREWORKS_BASE_URL`, `DEEPSEEK_BASE_URL`, `GROQ_BASE_URL`, `OLLAMA_BASE_URL`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
