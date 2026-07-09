# webhooks

Webhook signature verification primitives for Hot. The recipes providers actually use — hex/base64 HMAC, timestamped HMAC with replay protection — implemented once with constant-time comparison, plus ready-made GitHub and Slack verifiers.

## Usage

```hot
// GitHub (X-Hub-Signature-256), secret from ctx github.webhook.secret
is-valid ::webhooks::github/verify-request(request)

// Slack request signing (X-Slack-Signature + timestamp, 300s replay window)
is-valid ::webhooks::slack/verify-request(request)

// Generic recipes for providers without a package yet
::webhooks/verify-hmac-sha256-hex(secret, request.body-raw, signature-hex)
::webhooks/verify-hmac-sha1-base64(secret, base-string, signature-b64)
::webhooks/verify-timestamped-hmac-sha256(secret, `v0:${ts}:${body}`, ts, sig, 300)
```

All verifiers fail closed: missing headers, wrong scheme prefixes, malformed or stale timestamps return `false`.

Provider packages with their own signing conventions (stripe, twilio, whatsapp, discord) ship their own `verify-request`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
