# xai

xAI API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/xai": "1.1.5"
```

## Configuration

| Context Variable | Description |
|---|---|
| `xai.api.key` | xAI API key, used by every module except Collections management |
| `xai.management.api.key` | xAI Management API key, required by the Collections management functions |

Set them from the environment in a ctx file:

```hot
::myapp::ctx ns

::hot::ctx/set(filter-vals({
    "xai.api.key": ::hot::env/get("XAI_API_KEY", ""),
    "xai.management.api.key": ::hot::env/get("XAI_MANAGEMENT_API_KEY", "")
}, (v) { not(is-empty(v)) }))
```

## Usage

### Simple Chat

```hot
::responses ::xai::responses

answer ::responses/chat("grok-3", "What is the capital of France?")
println(answer)
```

### With System Instructions

```hot
::responses ::xai::responses

poem ::responses/chat(
    "grok-3",
    "Write a haiku about programming",
    "You are a creative poet who loves technology."
)
println(poem)
```

### Full Request Control

```hot
::responses ::xai::responses

request ::responses/CreateResponseRequest({
    model: "grok-3",
    input: [
        ::responses/user-message("Hello!"),
        ::responses/assistant-message("Hi there! How can I help you?"),
        ::responses/user-message("Tell me a joke")
    ],
    max_output_tokens: 1024,
    temperature: 0.8
})

response ::responses/create(request)
println(::responses/extract-response-text(response))
```

### Streaming Responses

```hot
::responses ::xai::responses

response ::responses/create-stream(::responses/CreateResponseRequest({
    model: "grok-3",
    input: "Write a short story about a robot"
}))

for-each(response.body, (event) {
    text ::responses/extract-delta-text(event)
    cond {
        gt(length(text), 0) => { print(text) }
        ::responses/is-stream-done(event) => { println("\n---Done!---") }
        => { null }
    }
})
```

### Continuing Conversations

The Responses API supports stateful conversations. Previous responses are stored server-side for 30 days.

```hot
::responses ::xai::responses

first-response ::responses/create(::responses/CreateResponseRequest({
    model: "grok-3",
    input: "My name is Alice"
}))

second-response ::responses/create(::responses/CreateResponseRequest({
    model: "grok-3",
    input: "What is my name?",
    previous_response_id: first-response.id
}))
```

### Disable Server-Side Storage

```hot
::responses ::xai::responses

response ::responses/create(::responses/CreateResponseRequest({
    model: "grok-3",
    input: "This conversation won't be stored",
    store: false
}))
```

## Image Generation

Generate images with `grok-imagine-image` (the default). File helpers return unified `AIMedia.Image` values from `::ai::media`.

### Simple Image URL

```hot
url ::xai::images/create-image("A futuristic city at sunset")
println(url)
```

### Generate and Save to File

```hot
::images ::xai::images
AIMedia ::ai::media/AIMedia

result ::images/generate-to-file("uploads/city.png", "A futuristic city at sunset")

match result {
    AIMedia.Image => {
        println(result.media.file.path)
        println(result.revised_prompt)
    }
    _ => { println("Error generating image") }
}

cat ::images/generate-to-file("output/cat.png", ::images/CreateImageRequest({
    prompt: "A cat wearing a spacesuit",
    quality: "hd",
    style: "vivid"
}))
```

### Generate Multiple Images

```hot
// Creates uploads/landscape-0.png, uploads/landscape-1.png, uploads/landscape-2.png
batch ::xai::images/generate-batch-to-files("uploads/landscape-{n}.png", "A beautiful mountain landscape", 3)

println(`Generated ${batch.succeeded} images`)
for-each(batch.items, (media) { println(media.media.file.path) })
```

## Live Search

Search the web and X with AI-powered responses.

```hot
::search ::xai::search

news ::search/search-web("grok-3", "What is the latest news about AI?")
posts ::search/search-x("grok-3", "What are people saying about the new iPhone?")
both ::search/search-all("grok-3", "What's happening with climate change today?")
```

### Using Search Tools Directly

```hot
::responses ::xai::responses
::search ::xai::search

response ::responses/create(::responses/CreateResponseRequest({
    model: "grok-3",
    input: "Find recent discussions about Rust programming",
    tools: [::search/web-search-tool(), ::search/x-search-tool()]
}))
```

## Collections (RAG)

Collections store documents with automatic embeddings for semantic, keyword or hybrid search. Creating and managing collections uses `xai.management.api.key`. `search` uses `xai.api.key`.

```hot
::collections ::xai::collections

results ::collections/search(::collections/SearchRequest({
    query: "What were the key revenue drivers?",
    collection_ids: ["col_abc123"],
    max_results: 10,
    retrieval_mode: ::collections/MODE_SEMANTIC
}))
first(results.results).content
```

## List Available Models

```hot
all-models ::xai::models/list()
for-each(all-models.data, (model) { println(model.id) })
```

## API Base URL

`https://api.x.ai/v1` (Collections management: `https://management-api.x.ai`)

## Available Models

Constants in `::xai::models`:

- Chat: `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning`, `grok-4-fast-reasoning`, `grok-4-fast-non-reasoning`, `grok-4-0709`, `grok-code-fast-1`, `grok-3`, `grok-3-mini`
- Vision: `grok-2-vision-1212`
- Images: `grok-imagine-image`, `grok-2-image-1212`
- Video: `grok-imagine-video`

## Modules

| Module | Description |
|--------|-------------|
| `::xai::responses` | Chat Responses API (main chat interface) |
| `::xai::images` | Image generation |
| `::xai::search` | Live web and X search |
| `::xai::collections` | Collections (document storage and search) |
| `::xai::models` | Model listing and info |
| `::xai::api` | Low-level authenticated requests |

## Documentation

- [xAI API Documentation](https://docs.x.ai/docs/overview)
- [Chat Responses Guide](https://docs.x.ai/docs/guides/chat)
- [Hot Package Documentation](https://hot.dev/pkg/xai)

## License

Apache-2.0 - see [LICENSE](LICENSE)
