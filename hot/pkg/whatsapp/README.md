# whatsapp

WhatsApp Business Cloud API bindings for Hot, calling Meta's Graph API directly with no third-party gateway. Send text, media, template, and interactive messages (reply buttons, lists, CTA URL buttons, location requests, Flows), quote earlier messages, mark messages read with a typing indicator, upload and download media, manage templates, and verify and unpack webhooks.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/whatsapp": "1.1.0"
```

## Functions by area

### Messages (`::whatsapp::messages`)

| Function | Sends |
|---|---|
| `send-text` | Text, with optional link preview |
| `send-image`, `send-video`, `send-audio`, `send-document`, `send-sticker` | Media by uploaded id or public URL |
| `send-location`, `send-contacts` | A location pin, contact cards |
| `send-reaction` | An emoji reaction (`""` removes it) |
| `send-template` | An approved template, the only message type allowed outside the 24-hour customer service window |
| `send-buttons`, `send-list` | Interactive reply buttons (max 3) and list menus |
| `send-cta-url` | A body plus one button that opens a URL |
| `send-location-request` | A "Send location" button; the reply arrives as a `location` message |
| `send-flow` | A WhatsApp Flow by `flow_id` or `flow_name` |
| `mark-read` | Read receipt, with `typing_indicator: true` to show "typing..." |
| `send-message` | Any prebuilt payload (the escape hatch) |

Every send request accepts `reply_to` (a `wamid`), sent as `context.message_id`. Each `send-*` has a pure `build-*-body` twin that returns the exact JSON payload, which is handy for logging and tests.

```hot
::messages ::whatsapp::messages

::messages/send-cta-url(::messages/SendCtaUrlRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  body_text: "Your invoice is ready.",
  display_text: "View invoice",
  url: "https://example.com/invoices/123"
}))

::messages/send-flow(::messages/SendFlowRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  body_text: "Book your appointment in a few taps.",
  flow_cta: "Book now",
  flow_id: "1234567890",
  screen: "APPOINTMENT",
  data: {service: "haircut"}
}))

::messages/mark-read(::messages/MarkReadRequest({
  phone_number_id: "109876543210",
  message_id: "wamid.HBg...",
  typing_indicator: true
}))
```

### Media (`::whatsapp::media`)

`upload-media` posts a binary-safe `multipart/form-data` body (assembled as `Bytes`, so images and audio are never mangled as text) and returns `{id}`. `get-media` returns a short-lived URL plus `mime_type`, `sha256`, and `file_size`. `download-media` returns the file as `Bytes`, and `download-media-by-id` does both steps. `delete-media` removes an upload. `get-media` and `delete-media` take an optional `phone_number_id` ownership check.

```hot
::media ::whatsapp::media
::messages ::whatsapp::messages

photo ::hot::http/get("https://example.com/photo.png").body-bytes
uploaded ::media/upload-media(::media/UploadMediaRequest({
  phone_number_id: "109876543210",
  content: photo,
  mime_type: "image/png",
  filename: "photo.png"
}))
::messages/send-image(::messages/SendImageRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  image_id: uploaded.id,
  caption: "Here is your photo"
}))

// Media a customer sent you (the id comes from the webhook message)
file ::media/download-media-by-id("1234567890")
file.mime_type
length(file.bytes)
```

### Templates (`::whatsapp::templates`)

`list-templates` filters by `status`, `name`, `category`, and `language`, selects `fields`, and pages with `limit` plus `after`/`before` cursors. `next-cursor` reads the next cursor from a page and returns null on the last page. Also: `get-template`, `create-template` (with `parameter_format` and `allow_category_change`), `edit-template`, and `delete-template` (by name, or one language by `hsm_id`).

```hot
::templates ::whatsapp::templates

page ::templates/list-templates(::templates/ListTemplatesRequest({
  waba_id: "102938475610",
  status: "APPROVED",
  limit: 50
}))
names map(page.data, (t) { t.name })
more ::templates/next-cursor(page)   // pass as `after` for the next page
```

### Business (`::whatsapp::business`)

`get-business-profile`, `update-business-profile` (unset fields are left unchanged), `get-phone-number`, and `list-phone-numbers`.

### Low level (`::whatsapp::api`, `::whatsapp::core`)

`::whatsapp::api/request` calls any Graph endpoint with the configured token, and `::whatsapp::api/url` builds versioned URLs. `::whatsapp::core/api-version` reports the version in use.

## Errors

A non-2xx response returns an Err carrying `::whatsapp::api/HttpError` `{status, headers, body}`. Meta's error object is at `body.error` (`message`, `type`, `code`, `error_subcode`, `fbtrace_id`), and `::whatsapp::api/error-message` extracts the message safely even when the body is not JSON.

```hot
::messages ::whatsapp::messages

sent ::messages/send-text(::messages/SendTextRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  text: "Hello"
}))
if-err(sent, (e) { println(`WhatsApp send failed: ${::whatsapp::api/error-message(e)}`) })
```

## Webhooks

Meta calls your endpoint twice over: a GET to verify the subscription, then signed POSTs with notifications. `verify-request` checks `X-Hub-Signature-256` (HMAC-SHA256 of `::hot::http/raw-body(request)` with your App Secret, compared in constant time) and fails closed when the secret is missing. `verify-challenge` checks `hub.mode` and `hub.verify_token` (constant time) and returns `hub.challenge`. `extract-messages` and `extract-statuses` flatten a notification into inbound messages (each with `phone_number_id` and the sender's `contact`) and delivery statuses.

```hot
whatsapp-webhook
meta {webhook: {service: "whatsapp", path: "/whatsapp/webhook"}}
fn (request) {
  cond {
    eq(request.method, "GET") => {
      challenge ::whatsapp::webhooks/verify-challenge(or(request.query, {}))
      if(is-err(challenge),
        ::hot::http/HttpResponse({status: 403, body: "forbidden"}),
        ::hot::http/HttpResponse({status: 200, headers: {Content-Type: "text/plain"}, body: challenge}))
    }
    not(::whatsapp::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    => {
      map(::whatsapp::webhooks/extract-messages(request.body), (m) {
        if(eq(m.type, "text"), println(`${m.from}: ${m.text.body}`), null)
      })
      ::hot::http/HttpResponse({status: 200, body: "ok"})
    }
  }
}
```

Meta does not sign a timestamp, so there is no replay window to enforce; deduplicate on the message `id`, since Meta retries deliveries.

## Notes and limits

- **24-hour window.** Free-form messages (text, media, interactive) only reach users who messaged you in the last 24 hours. Outside it, send an approved template.
- **Test numbers.** A new app's free test business number can only message up to 5 recipient numbers you verify on the API Setup page. The pre-approved `hello_world` template works there.
- **Media.** Media URLs from `get-media` expire after 5 minutes, and uploads persist for 30 days. Limits: images 5 MB, video and audio 16 MB, documents 100 MB, stickers 100 KB static or 500 KB animated.
- **Typing indicator.** It clears when you reply or after 25 seconds, so only send it when a reply is coming.
- **API version.** Each Graph API version is supported for about two years. Pin one with `whatsapp.api.version` if you need to hold back an upgrade.

## Integration tests

`hot test -p pkg-integration-whatsapp` runs read-only checks (phone number, business profile, template list) and sends one text and one location to your test recipient. Each test skips with a note unless `WHATSAPP_ACCESS_TOKEN` and the test's own variables are set: `WHATSAPP_TEST_PHONE_NUMBER_ID`, `WHATSAPP_TEST_RECIPIENT` (a verified test recipient in international format), and `WHATSAPP_TEST_WABA_ID`. Meta's developer app setup includes a free test business number.

## Documentation

- [WhatsApp Cloud API](https://developers.facebook.com/docs/whatsapp/cloud-api)
- [Graph API changelog](https://developers.facebook.com/docs/graph-api/changelog)

## License

Apache-2.0 - see [LICENSE](LICENSE)
