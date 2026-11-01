# openai-compatible

Presets for OpenAI-compatible APIs, built on [`hot.dev/openai`](https://hot.dev/pkg/openai). One package for local servers (Ollama, vLLM, LM Studio), hosted inference (Groq, Together, Fireworks, Cerebras, DeepSeek, Mistral, Perplexity), routers (OpenRouter) and private gateways. Each preset becomes a `hot.dev/hot-ai` provider that runs through OpenAI's tools-aware chat adapter, so `::ai::chat/run-loop` can mix models from several providers. Chat (with streaming), models and embeddings helpers are included.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/openai-compatible": "0.1.0"
```

It pulls in `hot.dev/openai` 1.3.0 and `hot.dev/hot-ai` 1.8.1.

## Setup

Each named preset function reads its own ctx variables, and only the presets you call need their key. The env var column is the provider's usual name, for your ctx file.

| Preset | Base URL | Context variables | Env var |
|---|---|---|---|
| `ollama()` | `http://localhost:11434/v1` | `ollama.base.url` (optional), `ollama.api.key` (optional) | `OLLAMA_API_KEY` |
| `vllm()` | `http://localhost:8000/v1` | `vllm.base.url` (optional), `vllm.api.key` (only with `--api-key`) | `VLLM_API_KEY` |
| `lm-studio()` | `http://localhost:1234/v1` | `lm-studio.base.url` (optional) | |
| `groq()` | `https://api.groq.com/openai/v1` | `groq.api.key` | `GROQ_API_KEY` |
| `openrouter()` | `https://openrouter.ai/api/v1` | `openrouter.api.key`, plus optional `openrouter.app.url` and `openrouter.app.title` for attribution | `OPENROUTER_API_KEY` |
| `deepseek()` | `https://api.deepseek.com` | `deepseek.api.key` | `DEEPSEEK_API_KEY` |
| `together()` | `https://api.together.ai/v1` | `together.api.key` | `TOGETHER_API_KEY` |
| `fireworks()` | `https://api.fireworks.ai/inference/v1` | `fireworks.api.key` | `FIREWORKS_API_KEY` |
| `mistral()` | `https://api.mistral.ai/v1` | `mistral.api.key` | `MISTRAL_API_KEY` |
| `cerebras()` | `https://api.cerebras.ai/v1` | `cerebras.api.key` | `CEREBRAS_API_KEY` |
| `perplexity()` | `https://api.perplexity.ai` | `perplexity.api.key` | `PERPLEXITY_API_KEY` |
| `custom()` | any | `openai-compatible.url` (required), `openai-compatible.api.key` (optional) | `OPENAI_COMPATIBLE_URL`, `OPENAI_COMPATIBLE_API_KEY` |

Keyless servers get no `Authorization` header. Nothing here reads `openai.api.key`.

A ctx file for Groq and a local Ollama:

```hot
::myapp::ctx ns

::hot::ctx/set(filter-vals({
    "groq.api.key": ::hot::env/get("GROQ_API_KEY", ""),
    "ollama.base.url": ::hot::env/get("OLLAMA_BASE_URL", "")
}, (v) { not(is-empty(v)) }))
```

## Quick start

### Agent loop across providers

```hot
::oc ::openai-compatible

add-numbers
meta {doc: "Add two integers."}
fn (x: Int, y: Int): Int { add(x, y) }

ask fn (question: Str): Str {
    registry ::ai::chat/with-providers([
        ::oc/provider(::oc/ollama()),
        ::oc/provider(::oc/groq()),
        ::oc/provider(::oc/openrouter())
    ])
    opts ::ai::chat/ChatOptions({
        model: "groq/llama-3.3-70b-versatile",
        providers: registry,
        tools: [::ai::tool/from-fn(add-numbers)]
    })
    ::ai::chat/run-loop(opts, question)
}
```

Each preset claims models that start with its prefix (`ollama/`, `vllm/`, `lmstudio/`, `groq/`, `openrouter/`, `deepseek/`, `together/`, `fireworks/`, `mistral/`, `cerebras/`, `perplexity/`, `custom/`) and strips the prefix before the request. OpenRouter ids keep their vendor part: `openrouter/anthropic/claude-sonnet-4.5` is sent as `anthropic/claude-sonnet-4.5`. Built-in hot-ai providers (`claude-*`, `gpt-*`, ...) still resolve as usual.

To skip routing, bind one provider directly:

```hot
ask-local fn (question: Str): Str {
    local ::openai-compatible/ollama()
    opts ::ai::chat/ChatOptions({
        chat-fn: ::openai-compatible/chat-fn(local),
        chat-stream-fn: ::openai-compatible/chat-stream-fn(local),
        model: "llama3.2"
    })
    ::ai::chat/run-loop-stream(opts, question)
}
```

Build providers inside functions, as in these examples, rather than as namespace-level constants: the presets read ctx, which is set when the run starts.

### Direct chat, streaming, models, embeddings

```hot
::oc ::openai-compatible
::chat ::openai-compatible::chat

answer ::chat/chat(::oc/groq(), "llama-3.3-70b-versatile", "Why is the sky blue?", "Be terse.")

response ::chat/complete(::oc/openrouter(), {
    model: "anthropic/claude-sonnet-4.5",
    messages: [::chat/system-message("You are terse."), ::chat/user-message("Hello!")],
    models: ["anthropic/claude-sonnet-4.5", "openai/gpt-5.2"]
})
text ::chat/reply-text(response)

stream ::chat/complete-stream(::oc/ollama(), {
    model: "llama3.2",
    messages: [::chat/user-message("Count to five.")]
})
for-each(stream.body, (event) { print(::chat/extract-chunk-text(event)) })

ids map(::openai-compatible::models/list-models(::oc/ollama()).data, (m) { m.id })

vectors ::openai-compatible::embeddings/create-embeddings(::oc/ollama(), ::openai-compatible::embeddings/EmbeddingsRequest({
    model: "nomic-embed-text",
    input: ["first text", "second text"]
}))
```

Requests are plain Maps in OpenAI's shape. Messages can be `::openai::chat/Message` records, and every OpenAI role is accepted, including `developer`. Provider-specific fields pass straight through, such as OpenRouter's `models` fallback list and `provider` routing. Responses come back as decoded Maps. Non-2xx statuses, including on streams, return an Err carrying `::openai::api/HttpError`.

### Explicit keys and overrides

`configure(preset, api-key, overrides?)` reads no ctx. `overrides` can set `base-url`, `model-prefix`, `name`, `headers`, `max-tokens` (default 4096) and `extra-body`:

```hot
::oc ::openai-compatible

registry fn (): Vec<::ai::chat/ProviderRegistration> {
    gpu-box ::oc/configure(::oc/OLLAMA, null, {base-url: "http://gpu-box.internal:11434/v1", model-prefix: "gpu/"})
    routed ::oc/configure(::oc/OPENROUTER, ::hot::ctx/get("openrouter.api.key"), {
        headers: {"HTTP-Referer": "https://example.com", "X-OpenRouter-Title": "Example"},
        extra-body: {provider: {sort: "throughput"}}
    })
    ::ai::chat/with-providers([::oc/provider(gpu-box), ::oc/provider(routed)])
}
```

## Function overview

| Module | Functions |
|---|---|
| `::openai-compatible` | Presets (`OLLAMA`, `VLLM`, `LM_STUDIO`, `GROQ`, `OPENROUTER`, `DEEPSEEK`, `TOGETHER`, `FIREWORKS`, `MISTRAL`, `CEREBRAS`, `PERPLEXITY`, `PRESETS`), named ctx presets (`ollama()` ... `custom()`), `configure`, `provider`, `chat-fn`, `chat-stream-fn`, `openrouter-headers` |
| `::openai-compatible::chat` | `complete`, `complete-stream`, `chat`, `reply-text`, `prepare-request`, message helpers, `extract-chunk-text`, `is-stream-done` |
| `::openai-compatible::models` | `list-models`, `get-model`, `model-path` |
| `::openai-compatible::embeddings` | `EmbeddingsRequest`, `create-embeddings` |

## Notes and limits

- **Local servers under isolation.** Isolated runtimes block outbound requests to localhost and private addresses. Ollama, vLLM and LM Studio on `localhost` or a LAN address work only where private destinations are allowed: set `hot.network.allow-private true` in `hot.hot` (or `HOT_NETWORK_ALLOW_PRIVATE=true`) when `hot.engine.isolation` is on. Hosted Hot Cloud runs need a publicly reachable endpoint, such as Ollama's hosted API (`ollama.base.url` = `https://ollama.com/v1` with `ollama.api.key`).
- **Output ceiling.** Presets send `max_completion_tokens` (Groq, Cerebras) or `max_tokens` (everyone else) and default it to 4096. A ceiling you put in the request wins.
- **Stream usage.** `stream_options: {include_usage: true}` is sent only where documented (Ollama, vLLM, Groq, OpenRouter, DeepSeek). Mistral has been reported to reject it with a 422. Other providers return usage on the last chunk anyway, or report 0.
- **Groq** rejects `logprobs`, `logit_bias`, `top_logprobs`, `messages[].name` and `n` other than 1.
- **Perplexity** has no tool calling through this API. Perplexity ended Sonar Chat Completions support on 2026-09-27 and now translates these requests to its Agent API, so prefer its native API for new work.
- **DeepSeek** `deepseek-reasoner` streams `reasoning_content`. The adapter ignores it and never sends it back.
- Feature support varies by model on every provider (tools, vision, JSON mode). The registration's `meta.capabilities` is a hint, not a guarantee.
- These presets are checked against each provider's published OpenAI-compatibility docs (November 2026), not tested live.

## License

Apache-2.0 - see [LICENSE](LICENSE)
