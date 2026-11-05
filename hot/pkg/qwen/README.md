# qwen

Qwen bindings for Hot. Qwen models are served by [Alibaba Cloud Model Studio](https://www.alibabacloud.com/en/product/modelstudio) (DashScope), and this package uses its OpenAI-compatible endpoint. It covers Chat Completions with thinking mode, web search and streaming, plus Models and Embeddings. It picks the base URL for your region or workspace, and provides a `hot.dev/hot-ai` provider so `::ai::chat/run-loop` agents can use Qwen with tools. Requests go through [`hot.dev/openai`](https://hot.dev/pkg/openai)'s endpoint and chat-tools adapter.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/qwen": "1.0.4"
```

It pulls in `hot.dev/openai` 1.3.0 and `hot.dev/hot-ai` 1.9.0.

## Setup

| Context Variable | Description |
|---|---|
| `qwen.api.key` | Model Studio (DashScope) API key, `DASHSCOPE_API_KEY` in Alibaba's docs. Keys are bound to the region they were created in. A call with no key here or in its `opts` is an Err naming both. |
| `qwen.region` | Optional. `intl` (Singapore, default), `us` (Virginia), `cn` (Beijing), `hk` (Hong Kong), `eu` (Frankfurt) or `jp` (Tokyo). Region ids such as `us-east-1` and names such as `singapore` also work. |
| `qwen.workspace.id` | Optional. Use the workspace domain `https://{id}.{region-id}.maas.aliyuncs.com/compatible-mode/v1`. Required for `eu` and `jp`, which have no shared domain. |
| `qwen.base.url` | Optional. Overrides everything above, for example to use a proxy. |

Base URLs without a workspace id:

| Region | Base URL |
|---|---|
| `intl` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| `us` | `https://dashscope-us.aliyuncs.com/compatible-mode/v1` |
| `cn` | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| `hk` | `https://cn-hongkong.dashscope.aliyuncs.com/compatible-mode/v1` |

A ctx file:

```hot
::myapp::ctx ns

::hot::ctx/set(filter-vals({
    "qwen.api.key": ::hot::env/get("DASHSCOPE_API_KEY", ""),
    "qwen.region": ::hot::env/get("QWEN_REGION", "")
}, (v) { not(is-empty(v)) }))
```

## Per-call credentials

Every function that calls the API takes a trailing `opts` Map that wins over ctx for that call, so one run can serve many tenants. Nothing is written to ctx.

| `opts` key | ctx key | Notes |
| --- | --- | --- |
| `api-key` | `qwen.api.key` | Present but null or blank is an Err, never a fallback to ctx |
| `region` | `qwen.region` | Needs `api-key` in `opts`. Must be a known region or alias |
| `workspace-id` | `qwen.workspace.id` | Needs `api-key` in `opts`. Must be a plain DNS label. Without `region` it uses `qwen.region` |
| `base-url` | `qwen.base.url` | Needs `api-key` in `opts`. Wins over `region` and `workspace-id` |
| `endpoint` | | An `::openai::api/Endpoint`, on its own. Reads no ctx |

`qwen.api.key` is never sent to a per-call host: `region`, `workspace-id` or `base-url` without `api-key` is an Err naming both. A per-call `region` doesn't use `qwen.workspace.id`, because a workspace lives in one region.

```hot
answer ::qwen::chat/chat("qwen3.8-flash", "What is the capital of France?", {api-key: tenant-key, region: "us"})
vector ::qwen::embeddings/embed("Hello world", {api-key: tenant-key})
models ::qwen::models/list({api-key: tenant-key, region: "eu", workspace-id: "llm-abc123"})
```

The hot-ai run loop passes `ChatOptions.provider-opts` through to Qwen's registration:

```hot
ask fn (tenant-key: Str, question: Str): Str {
    opts ::ai::chat/ChatOptions({
        model: "qwen3.8-flash",
        providers: ::ai::chat/with-providers([::qwen::ai/provider()]),
        provider-opts: {api-key: tenant-key, region: "us"}
    })
    ::ai::chat/run-loop(opts, question)
}
```

Keys passed in `opts` or an `Endpoint` are masked in the run's call records, including inside strings like `Bearer ${key}`. A key shorter than 8 bytes is masked only as a whole value.

## Quick start

```hot
::chat ::qwen::chat

answer ::chat/chat("qwen3.7-plus", "What is the capital of France?")
digits ::chat/chat("qwen3.8-flash", "What is 2+2?", "Respond with just the number.")

response ::chat/complete(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [
        ::chat/system-message("You are a helpful assistant."),
        ::chat/user-message("Hello!"),
        ::chat/assistant-message("Hi there! How can I help you?"),
        ::chat/user-message("Tell me a joke.")
    ],
    max_tokens: 1024,
    temperature: 0.8
}))
println(::chat/extract-response-text(response))
```

### Agent loop with tools (hot-ai)

`::qwen::ai/provider()` registers Qwen for models whose id starts with `qwen`. Use `qwen/<model>` for other models Model Studio hosts, such as `qwen/deepseek-v4.1-flash`; the `qwen/` part is stripped before the request. Its `variant` is `Provider.Qwen`, so Qwen keeps its own identity in the registry.

```hot
add-numbers
meta {doc: "Add two integers."}
fn (x: Int, y: Int): Int { add(x, y) }

ask fn (question: Str): Str {
    opts ::ai::chat/ChatOptions({
        model: "qwen3.8-flash",
        providers: ::ai::chat/with-providers([
            ::qwen::ai/provider(::qwen::ai/QwenOptions({enable-thinking: false}))
        ]),
        tools: [::ai::tool/from-fn(add-numbers)]
    })
    ::ai::chat/run-loop(opts, question)
}
```

`QwenOptions` sets the Qwen-specific fields on every request: `max-tokens` (default 4096, sent as `max_tokens`), `enable-thinking`, `thinking-budget`, `preserve-thinking`, `enable-search`, `search-options`, `enable-code-interpreter`, `top-k`, `repetition-penalty`, `reasoning-effort`, and `extra` for anything else. Unset fields are not sent, so model defaults apply. `::qwen::ai/chat-fn(opts)` and `chat-stream-fn(opts)` return the bound functions for `ChatOptions.chat-fn` / `chat-stream-fn`, and `chat-opts-fn(opts)` and `chat-stream-opts-fn(opts)` those for `chat-opts-fn` / `chat-stream-opts-fn`. `provider-for(endpoint, opts)` takes an explicit endpoint, such as `::qwen::api/endpoint-for("us", null, key)`, and reads no ctx.

Build the registry inside a function, as above: `provider()` reads ctx, which is set when the run starts. It builds without `qwen.api.key`, and a call is then an Err unless `provider-opts` carries an `api-key`.

### Streaming

```hot
::chat ::qwen::chat

response ::chat/complete-stream(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [::chat/user-message("Write a short story about a robot.")]
}))

for-each(response.body, (event) {
    text ::chat/extract-chunk-text(event)
    cond {
        gt(length(text), 0) => { print(text) }
        ::chat/is-stream-done(event) => { println("\n---Done!---") }
        => { null }
    }
})
```

### Thinking mode

Hybrid models (Qwen3 and later) return their reasoning separately from the answer. Several current models think by default. `enable_thinking` turns it on or off, and `thinking_budget` caps it. Streaming delivers reasoning in `delta.reasoning_content`; non-streaming responses carry it in `message.reasoning_content`. Some open-weight models only allow thinking when streaming.

```hot
::chat ::qwen::chat

response ::chat/complete-stream(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [::chat/user-message("Is 89 prime? Show your work.")],
    enable_thinking: true,
    thinking_budget: 2048
}))

for-each(response.body, (event) {
    thought ::chat/extract-chunk-reasoning(event)
    answer ::chat/extract-chunk-text(event)
    cond {
        gt(length(thought), 0) => { print(`[thinking] ${thought}`) }
        gt(length(answer), 0) => { print(answer) }
        => { null }
    }
})
```

### Web search, function calling, structured output

```hot
::chat ::qwen::chat

searched ::chat/complete(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [::chat/user-message("What's the latest news about AI?")],
    enable_search: true,
    search_options: {forced_search: true, search_strategy: "max"}
}))

tooled ::chat/complete(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [::chat/user-message("What's the weather in San Francisco?")],
    tools: [{
        type: "function",
        function: {
            name: "get_weather",
            description: "Get the current weather for a location",
            parameters: {
                type: "object",
                properties: {location: {type: "string", description: "City and state"}},
                required: ["location"]
            }
        }
    }],
    tool_choice: "auto"
}))

structured ::chat/complete(::chat/ChatCompletionRequest({
    model: "qwen3.7-plus",
    messages: [
        ::chat/system-message("Always respond in JSON."),
        ::chat/user-message("Return {\"answer\": <number>} for 2+2.")
    ],
    response_format: {type: "json_object"}
}))
```

`search_strategy` is `turbo` (default), `max` or `agent`; `enable_search_extension: true` adds vertical search.

### Embeddings and models

```hot
::embeddings ::qwen::embeddings

vector ::embeddings/embed("Hello world")
vectors ::embeddings/embed-batch(["Hello", "World", "Test"])

response ::embeddings/create(::embeddings/EmbeddingRequest({
    input: "Test text",
    model: "text-embedding-v4",
    dimensions: 256
}))
length(response.data[0].embedding)

for-each(::qwen::models/list().data, (model) { println(model.id) })
```

`text-embedding-v4` accepts `dimensions` of 2048, 1536, 1024 (default), 768, 512, 256, 128 or 64. `embed` and `embed-batch` return an Err on API errors instead of an empty vector.

## Function overview

| Module | Functions |
|---|---|
| `::qwen` | `base-url`, `resolve-base-url`, `region-info`, `is-workspace-id`, `REGIONS`, `BASE_URL`, `BASE_URL_US`, `BASE_URL_CN`, `BASE_URL_HK` |
| `::qwen::chat` | `complete`, `complete-stream`, `chat`, `request-body`, message helpers, `extract-response-text`, `extract-chunk-text`, `extract-chunk-reasoning`, `is-stream-done` |
| `::qwen::ai` | `provider`, `provider-for`, `chat-fn`, `chat-stream-fn`, `chat-opts-fn`, `chat-stream-opts-fn`, `QwenOptions`, `extra-body`, `request-options`, `compatible`, `is-qwen-model`, `QWEN_PROVIDER` |
| `::qwen::embeddings` | `create`, `embed`, `embed-batch`, `request-body` |
| `::qwen::models` | `list`, `get`, `model-path`, model id constants |
| `::qwen::api` | `resolve`, `endpoint`, `endpoint-for`, `request`, `request-stream` |

## Models

As of November 2026 Model Studio lists `qwen3.8-max` (flagship), `qwen3.7-plus` (balanced, multimodal input) and `qwen3.8-flash` (fast, low cost). Earlier generations are still served: `qwen3.6-plus`, `qwen3.5-plus`, `qwen3.5-flash`, `qwen3-max`, the `qwen-max` / `qwen-plus` / `qwen-flash` aliases, `qwen-long`, and `qwen-vl-plus` / `qwen-vl-max`. All of these are proprietary, hosted-only models. Embeddings: `text-embedding-v4` (default), `qwen3.7-text-embedding`, `text-embedding-v3`. Availability differs by region; check the Model Studio model list for yours.

## Notes and limits

- A key works only in the region it was created in. A mismatch returns 401 `invalid_api_key`.
- Non-2xx responses return an Err carrying `::openai::api/HttpError` (`status`, `headers`, `body`).
- The hot-ai adapter ignores `reasoning_content` and does not send it back on later turns.
- These bindings follow Alibaba's published OpenAI-compatibility docs (November 2026) and were not tested live.

## Documentation

- [OpenAI-compatible Chat Completions reference](https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-chat-completions)
- [OpenAI compatibility overview](https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope)
- [Model list](https://www.alibabacloud.com/help/en/model-studio/models)
- [Hot package documentation](https://hot.dev/pkg/qwen)

## License

Apache-2.0 — see [LICENSE](LICENSE)
