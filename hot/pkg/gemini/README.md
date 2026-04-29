# gemini

Google Gemini API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/gemini": "1.1.0"
```

## Configuration

Set your Gemini API key:

```hot
::hot::ctx/set({
    "gemini.api.key": "YOUR_API_KEY"
})
```

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
