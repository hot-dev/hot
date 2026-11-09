# resend

resend API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/resend": "1.2.2"
```

## Configuration

Set the `resend.api.key` context variable to your Resend API key, or pass a key per call.

## Per-call credentials

Every function that calls Resend takes an optional trailing `opts` Map, so one run can send for several accounts. `api-key` replaces `resend.api.key` for that call only. Nothing is written to ctx, so parallel flows can't see each other's keys. A null or blank `api-key` is an error, never a fallback to the context key.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::emails ::resend::emails

send-for-tenant fn (tenant: Map, to: Str): Any {
  ::emails/send-email(::emails/POSTEmailsRequest({
    from: tenant.from,
    to: [to],
    subject: "Hello from Hot!",
    text: "Welcome!"
  }), {api-key: tenant.resend-key})
}
```

## API Base URL

`https://api.resend.com`

## Documentation

Full documentation available at [hot.dev/pkg/resend](https://hot.dev/pkg/resend)

## License

Apache-2.0 - see [LICENSE](LICENSE)
