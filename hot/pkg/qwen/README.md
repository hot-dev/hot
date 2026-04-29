# qwen

Qwen API bindings for Hot.

Qwen models are served by [Alibaba Cloud Model Studio](https://www.alibabacloud.com/en/product/modelstudio) (DashScope). This package talks to the OpenAI-compatible endpoint at `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` (Singapore). If you'd rather target the mainland China endpoint, override `::qwen/BASE_URL` with `::qwen/BASE_URL_CN` in your own code.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/qwen": "1.0.0"
```

## Configuration

Set your DashScope API key in your context. Qwen docs refer to this as `DASHSCOPE_API_KEY`:

```hot
ctx: {
  "qwen.api.key": env("DASHSCOPE_API_KEY")
}
```

## Usage

### Simple chat

```hot
import ::qwen::chat

answer chat/chat("qwen3.6-plus", "What is the capital of France?")
println(answer)
```

### With system instructions

```hot
answer chat/chat(
    "qwen3.6-plus",
    "What is 2+2?",
    "You are a math tutor. Respond with just the number."
)
println(answer) // => "4"
```

### Full request control

```hot
import ::qwen::chat

request chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [
        chat/system-message("You are a helpful assistant."),
        chat/user-message("Hello!"),
        chat/assistant-message("Hi there! How can I help you?"),
        chat/user-message("Tell me a joke.")
    ],
    max_tokens: 1024,
    temperature: 0.8
})

response chat/complete(request)
println(chat/extract-response-text(response))
```

### Streaming responses

```hot
import ::qwen::chat

response chat/complete-stream(chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [{role: "user", content: "Write a short story about a robot."}]
}))

for-each(response.body, (event) {
    text chat/extract-chunk-text(event)
    cond {
        gt(length(text), 0) => { print(text) }
    }

    cond {
        chat/is-stream-done(event) => { println("\n---Done!---") }
    }
})
```

### Thinking mode (Qwen3 / Qwen3.5 / Qwen3.6)

Qwen3+ hybrid models can emit their reasoning separately from the final answer. Enable it with `enable_thinking` and optionally cap it with `thinking_budget`. Qwen requires streaming for thinking-mode responses on most models.

```hot
response chat/complete-stream(chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [{role: "user", content: "Is 89 prime? Show your work."}],
    enable_thinking: true,
    thinking_budget: 2048
}))

for-each(response.body, (event) {
    thought chat/extract-chunk-reasoning(event)
    answer  chat/extract-chunk-text(event)
    cond {
        gt(length(thought), 0) => { print(`[thinking] ${thought}`) }
        gt(length(answer), 0)  => { print(answer) }
    }
})
```

### Web search (Qwen-native feature)

Qwen exposes web search as request fields rather than as an OpenAI-style tool. They are wired through directly:

```hot
response chat/complete(chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [{role: "user", content: "What's the latest news about AI?"}],
    enable_search: true,
    search_options: {forced_search: true, search_strategy: "turbo"}
}))
```

### Function calling

```hot
response chat/complete(chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [{role: "user", content: "What's the weather in San Francisco?"}],
    tools: [{
        type: "function",
        function: {
            name: "get_weather",
            description: "Get the current weather for a location",
            parameters: {
                type: "object",
                properties: {
                    location: {type: "string", description: "City and state"}
                },
                required: ["location"]
            }
        }
    }],
    tool_choice: "auto"
}))
```

### Structured output

```hot
response chat/complete(chat/ChatCompletionRequest({
    model: "qwen3.6-plus",
    messages: [
        {role: "system", content: "Always respond in JSON."},
        {role: "user", content: "Return {\"answer\": <number>} for 2+2."}
    ],
    response_format: {type: "json_object"}
}))
```

## Embeddings

```hot
import ::qwen::embeddings

// Simple embedding (defaults to text-embedding-v4)
vector embeddings/embed("Hello world")

// Batch embeddings
vectors embeddings/embed-batch(["Hello", "World", "Test"])

// Full control (custom dimensions)
response embeddings/create(embeddings/EmbeddingRequest({
    input: "Test text",
    model: "text-embedding-v4",
    dimensions: 256
}))
length(response.data[0].embedding) // => 256
```

`text-embedding-v4` accepts `dimensions` values of 2048, 1536, 1024 (default), 768, 512, 256, 128, or 64.

## List available models

```hot
import ::qwen::models

all-models models/list()
for-each(all-models.data, (model) {
    println(model.id)
})
```

## API base URL

International (default): `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`
Mainland China:          `https://dashscope.aliyuncs.com/compatible-mode/v1`

## Available models

### Chat models

- `qwen3.6-plus` — latest flagship, 1M context, full tool support
- `qwen3.5-flash` — fast & cost-effective, 1M context
- `qwen3.5-plus` — balanced Qwen3.5
- `qwen3-max` — 256k context, strong reasoning and coding
- `qwen-plus`, `qwen-flash`, `qwen-turbo`, `qwen-max` — legacy commercial
- `qwen-long` — long-document analysis
- `qwen-vl-plus`, `qwen-vl-max` — vision-language

### Embedding models

- `text-embedding-v4` — latest (default)
- `text-embedding-v3` — previous generation

## Modules

| Module                | Description                                 |
|-----------------------|---------------------------------------------|
| `::qwen::chat`        | Chat Completions API (main chat interface) |
| `::qwen::embeddings`  | Text embeddings                            |
| `::qwen::models`      | Model listing and info                     |
| `::qwen::api`         | Low-level authenticated requests           |

## Documentation

- [Qwen Cloud API documentation](https://docs.qwencloud.com/)
- [OpenAI compatibility guide](https://docs.qwencloud.com/api-reference/toolkitframework/openai-compatible/overview)
- [Chat Completions reference](https://docs.qwencloud.com/api-reference/chat/openai-chat)
- [Alibaba Cloud Model Studio](https://www.alibabacloud.com/en/product/modelstudio)
- [Hot package documentation](https://hot.dev/pkg/qwen)

## License

Apache-2.0 — see [LICENSE](LICENSE)
