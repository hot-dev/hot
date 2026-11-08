# gemini

Google Gemini API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/gemini": "1.2.1"
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
