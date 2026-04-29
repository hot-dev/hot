# postmark

Postmark API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/postmark": "1.1.0"
```

## Configuration

Set the Server API Token in context before making requests:

```hot
::ctx/set("postmark.server.token", "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")
```

You can get your Server API Token from the Postmark dashboard under Server → API Tokens.

## Usage Example

```hot
// Set API token
::ctx/set("postmark.server.token", "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")

// Send an email
result ::postmark::send/email({
  From: "hello@example.com",
  To: "user@example.com",
  Subject: "Hello from Hot!",
  HtmlBody: "<h1>Welcome!</h1>",
  TextBody: "Welcome!",
  MessageStream: "outbound"
})
```

## API Base URL

`https://api.postmarkapp.com/`

## Documentation

- [Postmark API Documentation](https://postmarkapp.com/developer)
- [Hot Package Documentation](https://hot.dev/pkg/postmark)

## License

Apache-2.0 - see [LICENSE](LICENSE)
