# xai

xAI API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/xai": "1.1.1"
```

## Configuration

Set your xAI API key in your context:

```hot
ctx: {
  "xai.api.key": env("XAI_API_KEY")
}
```

## Usage

### Simple Chat

```hot
import ::xai::responses

// Simple one-shot chat
response responses/chat("grok-3", "What is the capital of France?")
println(response)
```

### With System Instructions

```hot
response responses/chat(
    "grok-3",
    "Write a haiku about programming",
    "You are a creative poet who loves technology."
)
println(response)
```

### Full Request Control

```hot
import ::xai::responses

request responses/CreateResponseRequest({
    model: "grok-3",
    input: [
        responses/user-message("Hello!"),
        responses/assistant-message("Hi there! How can I help you?"),
        responses/user-message("Tell me a joke")
    ],
    max_output_tokens: 1024,
    temperature: 0.8
})

response responses/create(request)
println(responses/extract-response-text(response))
```

### Streaming Responses

```hot
import ::xai::responses

request responses/CreateResponseRequest({
    model: "grok-3",
    input: "Write a short story about a robot"
})

response responses/create-stream(request)

for-each(response.body, (event) {
    text responses/extract-delta-text(event)
    cond {
        gt(length(text), 0) => { print(text) }
    }

    cond {
        responses/is-stream-done(event) => { println("\n---Done!---") }
    }
})
```

### Continuing Conversations

The Responses API supports stateful conversations. Previous responses are stored server-side for 30 days.

```hot
// First message
first-response responses/create(responses/CreateResponseRequest({
    model: "grok-3",
    input: "My name is Alice"
}))

// Continue the conversation using the previous response ID
second-response responses/create(responses/CreateResponseRequest({
    model: "grok-3",
    input: "What is my name?",
    previous_response_id: first-response.id
}))
```

### Disable Server-Side Storage

```hot
response responses/create(responses/CreateResponseRequest({
    model: "grok-3",
    input: "This conversation won't be stored",
    store: false
}))
```

## Embeddings

Create text embeddings for semantic search, similarity, and more.

```hot
import ::xai::embeddings

// Simple embedding
vector embeddings/embed("embedding-beta-v1", "Hello world")

// Batch embeddings
vectors embeddings/embed-batch("embedding-beta-v1", ["Hello", "World", "Test"])

// Calculate similarity between texts
vec1 embeddings/embed("embedding-beta-v1", "I love programming")
vec2 embeddings/embed("embedding-beta-v1", "Coding is my passion")
similarity embeddings/cosine-similarity(vec1, vec2)
println(`Similarity: ${similarity}`)  // ~0.85
```

## Image Generation

Generate images using the Aurora model. Returns unified `AIMedia.Image` types from `::ai::media`.

### Simple Image URL

```hot
import ::xai::images

url images/create-image("A futuristic city at sunset")
println(url)
```

### Generate and Save to File

```hot
import ::xai::images
import ::ai::media

// Generate and save to a file, returns AIMedia.Image
result images/generate-to-file("uploads/city.png", "A futuristic city at sunset")

match result {
    AIMedia.Image => {
        println(result.media.file.path)   // "uploads/city.png"
        println(result.media.file.size)   // 123456
        println(result.revised_prompt)    // The prompt xAI actually used
        println(result.provider)          // "xai"
    }
    HttpError => { println("Error generating image") }
}

// With options
result images/generate-to-file("output/cat.png", images/CreateImageRequest({
    prompt: "A cat wearing a spacesuit",
    quality: "hd",
    style: "vivid"
}))
```

### Generate Multiple Images

```hot
// Generate 3 images with numbered filenames, returns AIMediaBatch
batch images/generate-batch-to-files("uploads/landscape-{n}.png", "A beautiful mountain landscape", 3)
// Creates: uploads/landscape-0.png, uploads/landscape-1.png, uploads/landscape-2.png

println(`Generated ${batch.succeeded} images`)
for-each(batch.items, (media) {
    println(media.media.file.path)
})
```

## Live Search

Search the web and X (Twitter) with AI-powered responses.

```hot
import ::xai::search

// Search the web
response search/search-web("grok-3", "What is the latest news about AI?")
println(response)

// Search X (Twitter)
response search/search-x("grok-3", "What are people saying about the new iPhone?")
println(response)

// Search both web and X
response search/search-all("grok-3", "What's happening with climate change today?")
println(response)
```

### Using Search Tools Directly

```hot
import ::xai::responses
import ::xai::search

// Add search tools to a custom request
response responses/create(responses/CreateResponseRequest({
    model: "grok-3",
    input: "Find recent discussions about Rust programming",
    tools: [search/web-search-tool(), search/x-search-tool()]
}))
```

## List Available Models

```hot
import ::xai::models

all-models models/list()
for-each(all-models.data, (model) {
    println(model.id)
})
```

## API Base URL

`https://api.x.ai/v1`

## Available Models

### Chat Models
- `grok-4-1-fast-non-reasoning` - Latest vision-capable model
- `grok-4-1-fast-reasoning` - Latest with reasoning
- `grok-3` - Flagship model with advanced reasoning
- `grok-3-mini` - Faster, cost-effective model

### Embedding Models
- `embedding-beta-v1` - Text embeddings

### Image Models
- `aurora` - Image generation

## Modules

| Module | Description |
|--------|-------------|
| `::xai::responses` | Chat Responses API (main chat interface) |
| `::xai::embeddings` | Text embeddings |
| `::xai::images` | Image generation (Aurora) |
| `::xai::search` | Live web and X search |
| `::xai::models` | Model listing and info |
| `::xai::api` | Low-level authenticated requests |

## Documentation

- [xAI API Documentation](https://docs.x.ai/docs/overview)
- [Chat Responses Guide](https://docs.x.ai/docs/guides/chat)
- [Hot Package Documentation](https://hot.dev/pkg/xai)

## License

Apache-2.0 - see [LICENSE](LICENSE)
