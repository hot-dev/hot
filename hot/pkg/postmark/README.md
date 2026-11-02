# postmark

Postmark API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/postmark": "1.2.0"
```

## Configuration

Set the Server API Token in context before making requests:

```hot
::hot::ctx/set("postmark.server.token", "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")
```

You can get your Server API Token from the Postmark dashboard under Server → API Tokens.

## Per-call credentials

Every function that calls Postmark takes an optional trailing `opts` Map, so one run can send through several servers. `server-token` replaces `postmark.server.token` for that call only. Nothing is written to ctx, so parallel flows can't see each other's tokens. A null or blank `server-token` is an error, never a fallback to the context token.

```hot
::send ::postmark::send

send-for-tenant fn (tenant: Map, to: Str): Any {
  ::send/send-single-email(::send/SendEmailRequest({
    From: tenant.from,
    To: to,
    Subject: "Hello from Hot!",
    TextBody: "Welcome!"
  }), {server-token: tenant.postmark-token})
}
```

## Usage Example

```hot
// Set API token
::hot::ctx/set("postmark.server.token", "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")

// Send an email
result ::postmark::send/send-single-email(::postmark::send/SendEmailRequest({
  From: "hello@example.com",
  To: "user@example.com",
  Subject: "Hello from Hot!",
  HtmlBody: "<h1>Welcome!</h1>",
  TextBody: "Welcome!"
}))
```

## API Base URL

`https://api.postmarkapp.com/`

## Documentation

- [Postmark API Documentation](https://postmarkapp.com/developer)
- [Hot Package Documentation](https://hot.dev/pkg/postmark)

## License

Apache-2.0 - see [LICENSE](LICENSE)
