# gemini

Google Gemini API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/gemini": "1.3.0"
```

## Configuration

Set your Gemini API key:

```hot
::hot::ctx/set({
    "gemini.api.key": "YOUR_API_KEY"
})
```

## Per-call credentials

Every typed function takes a trailing `opts` Map that wins over ctx for that call, so one run can serve many tenants. Nothing is written to ctx.

| `opts` key | ctx key | Notes |
| --- | --- | --- |
| `api-key` | `gemini.api.key` | Sent in the `x-goog-api-key` header |
| `base-url` | `gemini.base.url` | For proxies and gateways. In `opts` it needs `api-key` in `opts` too: the ctx key is never sent to a per-call URL |

```hot
answer ::gemini::chat/chat("gemini-2.0-flash", "Hello!", {api-key: tenant-key})
models ::gemini::models/list(::gemini::models/ModelsListRequest({page-size: "10", page-token: ""}), {api-key: tenant-key})
```

A blank or null key in `opts` is an error, never a fallback to ctx, so a tenant with a missing key can't run on your project's key. Leave the key out of `opts` to use ctx.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

## Features

### Chat/Generation

```hot
response ::gemini::chat/generate({
    contents: [{role: "user", parts: [{text: "Hello!"}]}]
}, "gemini-2.0-flash")

// Convenience function
response ::gemini::chat/chat("gemini-2.0-flash", "What is the capital of France?")
```

### Streaming

```hot
stream ::gemini::chat/generate-stream({
    contents: [{role: "user", parts: [{text: "Tell me a story"}]}]
}, "gemini-2.0-flash")

for-each(stream.body, (chunk) {
    println(chunk)
})
```

### Tool calling with hot-ai

`::gemini::chat-tools` satisfies the `::ai::chat` chat contract, so a Gemini model can run under `::ai::chat/run-loop` and `run-loop-stream` with tools. `hot-ai`'s built-in registry leaves every provider's `chat-fn` null, so wire the adapter on `ChatOptions`:

```hot
get-weather fn (city: Str): Str { `${city}: 18C` }

opts ::ai::chat/ChatOptions({
    chat-fn: ::gemini::chat-tools/chat-with-tools,
    chat-stream-fn: ::gemini::chat-tools/chat-with-tools-stream,
    model: "gemini-4-hydrogen",
    tools: [::ai::tool/from-fn(get-weather)]
})

answer ::ai::chat/run-loop(opts, "What's the weather in Paris?")
```

For a per-call key, use `chat-opts-fn` and `chat-stream-opts-fn` with `provider-opts: {api-key: tenant-key}`. For thinking, output ceiling, context caching, safety settings or a response schema, bind `chat-with-tools-options` with a `ChatRequestOptions` in a closure.

Thought signatures, which Gemini 3 requires back on the next turn, ride on each `ToolCall.meta` and go back on their own. Thought text stays out of the reply. A refused call, an error inside a stream, a stream cut short and a reply with no text and no call each come back as an Err you can check with `is-err`.

### Image Generation (Imagen)

Generate images using Google's Imagen model:

```hot
// Simple generation
response ::gemini::images/generate({
    prompt: "A serene lake at sunset",
    numberOfImages: 2,
    aspectRatio: "16:9"
})

// Generate and save to file (returns Media.Image)
result ::gemini::images/generate-to-file("output/sunset.png", "A serene lake at sunset")

match result {
    Media.Image => {
        println(result.file.path)   // "output/sunset.png"
        println(result.prompt)      // "A serene lake at sunset"
    }
    HttpError => { println("Error!") }
}
```

### Inline Image Generation (Gemini 2.0 Flash)

Gemini 2.0 Flash supports native image generation within chat responses:

```hot
// Generate image inline (unique to Gemini!)
result ::gemini::images/generate-inline-to-file("output/robot.png", "Draw a cute robot watering plants")

match result {
    Media.Image => { println(result.file.path) }
    HttpError => { println("Error!") }
}
```

### Batch Image Generation

```hot
batch ::gemini::images/generate-batch-to-files("output", {
    prompt: "A magical forest",
    numberOfImages: 3,
    aspectRatio: "1:1"
})

println(`Generated ${batch.succeeded} images`)
```

### Models

```hot
// List available models
models ::gemini::models/list()

// Get specific model
model ::gemini::models/get("gemini-2.0-flash")
```

### File Upload

```hot
// Upload a file
file ::gemini::files/upload("image.png", "image/png")

// List uploaded files
files ::gemini::files/list()
```

## API Base URL

`https://generativelanguage.googleapis.com`

## Documentation

Full documentation available at [hot.dev/pkg/gemini](https://hot.dev/pkg/gemini)

## License

Apache-2.0 - see [LICENSE](LICENSE)
