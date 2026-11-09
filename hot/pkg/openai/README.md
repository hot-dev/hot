# openai

OpenAI API bindings for Hot: Chat Completions with streaming, a tools-aware adapter for the `hot.dev/hot-ai` run loop, plus Responses, Models, Embeddings, Images, Audio, Moderations, Files, Batches, Fine-tuning, Vector Stores and the Organization admin APIs. The same chat adapter also drives any OpenAI-compatible server (Ollama, vLLM, LM Studio, Groq, OpenRouter, DeepSeek, Together, ...) through a configurable base URL or an explicit endpoint.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/openai": "1.3.3"
```

## Setup

| Context Variable | Description |
|---|---|
| `openai.api.key` | OpenAI API key (`sk-...`). A call with no key here or in its `opts` is an Err naming both. |
| `openai.base.url` | Optional. Base URL for every `::openai::*` call. Default `https://api.openai.com/v1`. Set it to route the package through a gateway or proxy that speaks the OpenAI API. |

Functions that take an explicit `::openai::api/Endpoint` read neither variable.

## Per-call credentials

Every typed function takes a trailing `opts` Map that wins over ctx for that call, so one run can serve many tenants. Nothing is written to ctx.

| `opts` key | ctx key | Notes |
| --- | --- | --- |
| `api-key` | `openai.api.key` | Sent as `Authorization: Bearer <key>`. Present but null or blank is an Err, never a fallback to ctx |
| `base-url` | `openai.base.url` | For proxies and gateways. In `opts` it needs `api-key` in `opts` too: the ctx key is never sent to a per-call URL |
| `endpoint` | | An `::openai::api/Endpoint`, on its own. Reads no ctx. A keyless one sends no `Authorization` header, which is how local servers work |

```hot
answer ::openai::chat/chat("gpt-4o-mini", "Hello!", {api-key: tenant-key})
models ::openai::models/list({api-key: tenant-key})
vector ::openai::embeddings/embed("Hello world", {api-key: tenant-key})
local-models ::openai::models/list({endpoint: ::openai::api/endpoint("http://localhost:11434/v1")})
```

The hot-ai run loop passes `ChatOptions.provider-opts` through to the `chat-opts-fn` and `chat-stream-opts-fn` slots:

```hot
ask fn (tenant-key: Str, question: Str): Str {
    opts ::ai::chat/ChatOptions({
        chat-opts-fn: ::openai::chat-tools/chat-with-tools,
        model: "gpt-4o-mini",
        provider-opts: {api-key: tenant-key}
    })
    ::ai::chat/run-loop(opts, question)
}
```

Keys passed in `opts` or an `Endpoint` are masked in the run's call records, including inside strings like `Bearer ${key}`. A key shorter than 8 bytes is masked only as a whole value. Failures come back as Errs, including a key `opts` can't use, so one tenant's bad key is an `is-err` check for that call.

`::openai::api/request` and `request-stream` also take a full URL. The key goes to it only when it is on the call's base URL (same scheme, host, port and path prefix), or when `opts` carries its own `api-key`. A URL whose host can't be read safely, with userinfo, backslashes or control characters, is always refused.

## Quick start

```hot
::chat ::openai::chat

answer ::chat/chat("gpt-4o-mini", "What is the capital of France?")

response ::chat/complete(::chat/ChatCompletionRequest({
    model: "gpt-4o-mini",
    max_completion_tokens: 200,
    messages: [
        ::chat/system-message("You are terse."),
        ::chat/user-message("Why is the sky blue?")
    ]
}))
text response.choices[0].message.content

stream ::chat/complete-stream(::chat/ChatCompletionRequest({
    model: "gpt-4o-mini",
    messages: [::chat/user-message("Count to three.")]
}))
for-each(stream.body, (event) { print(::chat/extract-chunk-text(event)) })
```

### Agent loop with tools (hot-ai)

`::openai::chat-tools/chat-with-tools` and `chat-with-tools-stream` satisfy the `::ai::chat/run-loop` and `run-loop-stream` contracts:

```hot
add-numbers
meta {doc: "Add two integers."}
fn (x: Int, y: Int): Int { add(x, y) }

ask fn (question: Str): Str {
    opts ::ai::chat/ChatOptions({
        chat-fn: ::openai::chat-tools/chat-with-tools,
        model: "gpt-4o-mini",
        tools: [::ai::tool/from-fn(add-numbers)]
    })
    ::ai::chat/run-loop(opts, question)
}
```

`chat-with-tools-options` and `chat-with-tools-stream-options` take a `ChatRequestOptions` with `max-completion-tokens` and `reasoning-effort`; bind them in a closure when a session needs per-model controls. For GPT-5.6 function tools through Chat Completions, use `reasoning-effort: "none"`; reasoning-enabled tool loops should use the Responses API.

## OpenAI-compatible endpoints

An `::openai::api/Endpoint` is a base URL, an optional bearer key and optional extra headers. Every function that takes one reads no ctx, so a keyless local server never needs `openai.api.key`, and one program can talk to several providers.

```hot
::api ::openai::api

local ::api/endpoint("http://localhost:11434/v1")
router ::api/endpoint("https://openrouter.ai/api/v1", ::hot::ctx/get("openrouter.api.key"), {
    "HTTP-Referer": "https://example.com",
    "X-OpenRouter-Title": "Example App"
})

reply ::openai::chat/complete-with(local, {
    model: "llama3.2",
    messages: [{role: "user", content: "Say hi"}],
    max_tokens: 50
})
reply.choices[0].message.content
```

With no key (null or `""`) no `Authorization` header is sent. `complete-with` and `complete-stream-with` take the request as a plain Map, so provider-specific fields pass straight through, and return the decoded body without coercing it to `ChatCompletionResponse`. Non-2xx responses return an Err carrying `HttpError`.

### hot-ai providers for compatible servers

`::openai::compatible/provider` builds an `::ai::chat/ProviderRegistration` that reuses the chat-tools adapter for `chat-fn`, `chat-stream-fn` and their `provider-opts` counterparts. Give it a name, base URL, key and model prefix. Models that start with the prefix are routed to it, and the prefix is stripped before the request. Its `variant` is `Provider.Compat({name})`, so a compatible server keeps its own identity and never passes for OpenAI:

```hot
::compat ::openai::compatible

ask fn (model: Str, question: Str): Str {
    registry ::ai::chat/with-providers([
        ::compat/provider("Ollama", "http://localhost:11434/v1", null, "ollama/"),
        ::compat/provider("Groq", "https://api.groq.com/openai/v1", ::hot::ctx/get("groq.api.key"), "groq/")
    ])
    ::ai::chat/run-loop(::ai::chat/ChatOptions({model: model, providers: registry}), question)
}

answer ask("groq/llama-3.3-70b-versatile", "Name three prime numbers.")
```

Build registries inside a function, as here, rather than as namespace-level constants: provider construction reads ctx, which is set when the run starts.

For server quirks, build a `CompatibleProvider` with `ChatRequestOptions`:

```hot
::compat ::openai::compatible

deepseek fn (): ::ai::chat/ProviderRegistration {
  ::compat/provider(::compat/config(
    "DeepSeek",
    ::openai::api/endpoint("https://api.deepseek.com", ::hot::ctx/get("deepseek.api.key")),
    "deepseek/",
    ::openai::chat-tools/ChatRequestOptions({
        max-completion-tokens: 4096,
        max-tokens-param: "max_tokens",
        stream-usage: true,
        extra-body: {temperature: 0.2}
    })
  ))
}
```

- `max-tokens-param`: the body field for the output ceiling. Defaults to `max_completion_tokens`. Use `max_tokens` for servers that only accept the older name.
- `stream-usage: false`: omit `stream_options: {include_usage: true}` for servers that reject it.
- `extra-body`: provider-specific fields merged into every request (Qwen's `enable_thinking`, OpenRouter's `provider` routing, ...).

`::compat/chat-fn(p)` and `::compat/chat-stream-fn(p)` return the bound functions for `ChatOptions.chat-fn` when you don't need registry routing, and `chat-opts-fn(p)` and `chat-stream-opts-fn(p)` do the same for `ChatOptions.chat-opts-fn`. `::compat/list-models(p)` and `::compat/create-embeddings(p, request)` call `/models` and `/embeddings` on the same endpoint. `list-models`, `create-embeddings` and the opts-taking chat functions apply a call's `opts` over the provider's endpoint: `api-key`, `base-url` (with `api-key` when the provider has a key), or a whole `endpoint`.

The `hot.dev/openai-compatible` package ships ready-made presets (Ollama, vLLM, LM Studio, Groq, OpenRouter, DeepSeek, Together, Fireworks, Mistral, Cerebras, Perplexity) on top of this, and `hot.dev/qwen` uses it for Alibaba Cloud Model Studio.

Low-level: `::openai::api/endpoint-request(ep, method, path, headers?, body?)`, `endpoint-request-stream(...)`, and the pure `build-endpoint-request(...)`, `endpoint-headers(...)`, `target-url(...)`, `join-url(...)` and `normalize-base-url(...)` helpers.

## Function overview

| Module | What it covers |
|---|---|
| `::openai::chat` | Chat Completions: `complete`, `complete-stream`, `chat`, message helpers, SSE chunk helpers, and `complete-with` / `complete-stream-with` for explicit endpoints |
| `::openai::chat-tools` | hot-ai adapter: `chat-with-tools`, `chat-with-tools-stream`, `*-options` and `*-at` (explicit endpoint) variants, `count-tokens` heuristic |
| `::openai::compatible` | `CompatibleProvider`, `provider`, `resolve`, `chat-fn`, `chat-stream-fn`, `chat-opts-fn`, `chat-stream-opts-fn`, `list-models`, `create-embeddings` |
| `::openai::api` | Authenticated requests, `resolve`, `base-url`, `Endpoint`, `endpoint`, `with-opts`, `default-endpoint`, `endpoint-request` |
| `::openai::models`, `::openai::embeddings`, `::openai::images`, `::openai::audio`, `::openai::moderations`, `::openai::realtime` | Typed wrappers for those APIs |
| `::openai::create`, `get`, `list`, `retrieve`, `modify`, `update`, `delete`, `cancel`, ... | Generated bindings for the rest of the REST API (Responses, Files, Batches, Fine-tuning, Vector Stores, Assistants, Evals, Containers) |
| `::openai::organization` | Admin API keys, projects, users, invites, usage and costs |

## Notes and limits

- Isolated runtimes block outbound requests to localhost and private addresses. A local Ollama/vLLM/LM Studio endpoint works only where the runtime allows private destinations: `hot.network.allow-private true` in `hot.hot` (or `HOT_NETWORK_ALLOW_PRIVATE=true`) when `hot.engine.isolation` is on. Hosted Hot Cloud runs need a publicly reachable endpoint.
- `organization` and `project` are not settings of this package. To send `OpenAI-Organization` or `OpenAI-Project`, put them in an `Endpoint`'s `headers`.
- Explicit-endpoint calls return plain Maps rather than the typed `ChatCompletionResponse`, because compatible servers differ in which fields they return.
- The adapter omits `tools` when the tool list is empty, because servers reject `tools: []`.
- `count-tokens` is a character heuristic (about 4 characters per token); pass an exact counter through `ChatOptions.count-tokens-fn` when it matters.

## Documentation

- [OpenAI API reference](https://platform.openai.com/docs/api-reference)
- [Hot package documentation](https://hot.dev/pkg/openai)

## License

Apache-2.0 - see [LICENSE](LICENSE)
