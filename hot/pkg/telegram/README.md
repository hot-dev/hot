# telegram

Telegram Bot API bindings for Hot: send text, photos, documents, audio, video, voice, albums, polls, and locations (uploading files from `Bytes` or reusing file_ids and URLs), download what users send, edit and react to messages, answer callback and inline queries, manage chats, members, and bot commands, and receive updates by webhook or long polling with secret-token verification.

## Install

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/telegram": "1.1.0"
```

## Setup

Create a bot with [@BotFather](https://t.me/BotFather) (`/newbot`); it gives you the token.

| Context Variable | Description |
|---|---|
| `telegram.bot.token` | Bot token (`123456:ABC-...`). It goes in the URL path of every call, so never log Bot API or file URLs. |
| `telegram.webhook.secret` | The `secret_token` you pass to `set-webhook`; used by `::telegram::updates/verify-request` |

Every function also accepts its parameters as the Bot API names them (`chat_id`, `reply_markup`, ...). `chat_id` is an Int or an `@channelusername` Str.

## Per-call credentials

Every function that calls Telegram takes an optional trailing `opts` Map, so one run can act as many bots and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and a `bot-token` from `opts` is masked in call records, including inside Bot API URLs.

| Key | Effect |
|---|---|
| `bot-token` | The bot token for this call, instead of `telegram.bot.token` |

`opts` always comes last.

```hot
::messages ::telegram::messages

tenant {bot-token: "123456:tenant-b-token"}
sent ::messages/send-message(::messages/SendMessageRequest({chat_id: 123456789, text: "Hello"}), tenant)
me ::telegram::bot/get-me(tenant)
```

A `bot-token` that is present but blank or null is an error naming the key, never a fallback to `telegram.bot.token`. `::telegram::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::messages ::telegram::messages
::api ::telegram::api
::files ::telegram::files

// Text with formatting and an inline keyboard
sent ::messages/send-message(::messages/SendMessageRequest({
  chat_id: 123456789,
  text: "<b>Deploy finished.</b> Roll back?",
  parse_mode: "HTML",
  reply_markup: {inline_keyboard: [[{text: "Roll back", callback_data: "rollback"}, {text: "Keep", callback_data: "keep"}]]}
}))

// Upload a generated file
pdf ::api/input-file(::hot::file/read-file-bytes("report.pdf"), "report.pdf", "application/pdf")
::messages/send-document(::messages/SendDocumentRequest({chat_id: 123456789, document: pdf, caption: "Weekly report"}))

// Reuse a file Telegram already has (file_id) or let it fetch a URL
::messages/send-photo(::messages/SendPhotoRequest({chat_id: 123456789, photo: "https://example.com/chart.png"}))

// React to the message, then edit it
::messages/set-message-reaction(::messages/SetMessageReactionRequest({
  chat_id: 123456789, message_id: sent.message_id, reaction: [{type: "emoji", emoji: "👍"}]
}))
::messages/edit-message-text(::messages/EditMessageTextRequest({
  chat_id: 123456789, message_id: sent.message_id, text: "Deploy finished. Kept."
}))

// Download a voice note a user sent
on-voice fn (message: Map): Bytes {
  ::files/download-file(message.voice.file_id)
}
```

## Functions

**Messages** (`::telegram::messages`)
- `send-message`, `forward-message`, `copy-message`, `edit-message-text`, `edit-message-caption`, `edit-message-reply-markup`, `edit-message-media`, `delete-message`, `delete-messages`.
- Media: `send-photo`, `send-document`, `send-audio`, `send-video`, `send-voice` (each takes a file_id, a URL, or an `InputFile`), `send-media-group` (2-10 items, files allowed), `send-location`, `send-contact`, `send-poll`, `stop-poll`, `send-dice`.
- `send-chat-action` (typing indicator), `set-message-reaction`, `answer-callback-query`, `answer-inline-query`.

**Files** (`::telegram::files`): `get-file` resolves a file_id to a `file_path`; `download-file` returns the content as `Bytes` (up to 20 MB). `::telegram::api/input-file(bytes, filename, mime-type?)` builds uploads (up to 50 MB; photos 10 MB).

**Chats** (`::telegram::chat`): `get-chat`, `get-chat-member`, `get-chat-member-count`, `get-chat-administrators`; `ban-chat-member`, `unban-chat-member`, `restrict-chat-member`, `promote-chat-member`, `set-chat-permissions`; `set-chat-title`, `set-chat-description`, `pin-chat-message`, `unpin-chat-message`, `unpin-all-chat-messages`, `leave-chat`; invite links and join requests.

**Bot** (`::telegram::bot`): `get-me`; name, description, and short description getters and setters; `get-my-commands`, `set-my-commands`, `delete-my-commands`.

**Updates** (`::telegram::updates`): `set-webhook`, `delete-webhook`, `get-webhook-info`, `get-updates` (long polling), `verify-request`.

**Low level** (`::telegram::api`): `request(method, params, opts?)` calls any Bot API method, choosing JSON or multipart automatically; `parse-response`, `download`.

### Uploads

A field that accepts a file takes a Str (file_id or HTTP URL, sent as JSON) or an `InputFile` (sent as multipart/form-data, built as Bytes so binary content is never re-encoded). In albums and `edit-message-media`, put the `InputFile` in the InputMedia's `media` (or `thumbnail`/`cover`); it is uploaded as an `attach://` part:

```hot
::messages ::telegram::messages
::api ::telegram::api

before ::hot::file/read-file-bytes("before.jpg")
after ::hot::file/read-file-bytes("after.jpg")
::messages/send-media-group(::messages/SendMediaGroupRequest({
  chat_id: 123456789,
  media: [
    {type: "photo", media: ::api/input-file(before, "before.jpg", "image/jpeg"), caption: "Before"},
    {type: "photo", media: ::api/input-file(after, "after.jpg", "image/jpeg"), caption: "After"}
  ]
}))
```

### Errors

When Telegram answers `ok: false` (bad chat, blocked by the user, flood control) the call returns `err(TelegramError)` with `error_code`, `description`, the HTTP `status`, and `retry_after` / `migrate_to_chat_id` from the response parameters. Anything else that fails (a proxy's HTML 502, a network error) is `err(HttpError)`.

```hot
::messages ::telegram::messages

notify fn (chat-id: Int, text: Str): Any {
  sent ::messages/send-message(::messages/SendMessageRequest({chat_id: chat-id, text: text}))
  if-err(sent, (e) {
    cond {
      is-some(get(e, "retry_after")) => { err(`rate limited; retry in ${e.retry_after}s`) }
      => { err(e) }
    }
  })
}
```

## Webhooks

Register your endpoint with a secret, then verify the `X-Telegram-Bot-Api-Secret-Token` header on every update:

```hot
::updates ::telegram::updates
::messages ::telegram::messages

HttpRequest ::hot::http/HttpRequest
HttpResponse ::hot::http/HttpResponse

setup fn (): Bool {
  ::updates/set-webhook(::updates/SetWebhookRequest({
    url: "https://myapp.hot.dev/telegram/webhook",
    secret_token: ::hot::ctx/get("telegram.webhook.secret"),
    allowed_updates: ["message", "callback_query"]
  }))
}

on-update
meta {webhook: {service: "telegram", path: "/webhook"}}
fn (request: HttpRequest): HttpResponse {
  cond {
    not(::updates/verify-request(request)) => { HttpResponse({status: 401, body: "bad secret"}) }
    is-some(request.body.callback_query) => {
      ::messages/answer-callback-query(::messages/AnswerCallbackQueryRequest({
        callback_query_id: request.body.callback_query.id,
        text: "Got it"
      }))
      HttpResponse({status: 200, body: "ok"})
    }
    => { HttpResponse({status: 200, body: "ok"}) }
  }
}
```

`verify-request` compares the header with `telegram.webhook.secret` (or an explicit 2nd argument) in constant time and fails closed: a missing secret, a missing header, or an empty value never verifies. Telegram's secret token is a shared secret, not a signature over the body, and carries no timestamp, so there is no replay window to check. Answer quickly with a 2xx; Telegram retries failed deliveries.

For local development without a public URL, poll with `get-updates` instead (pass `offset` = last `update_id` + 1 to confirm updates). Polling and a webhook are mutually exclusive: while a webhook is set, `getUpdates` answers 409.

## Notes and limits

- Unset and null top-level parameters are omitted from requests; a call with no parameters is sent as GET.
- Bot API limits: about 30 messages per second overall and 1 per second per chat (20 per minute in groups); exceeding them returns 429 with `retry_after`.
- Bots can download files up to 20 MB and upload up to 50 MB through the cloud Bot API.
- Bots can't message a user who hasn't started a chat with them (403).
- Request types cover the commonly used parameters of Bot API 10.3 (August 2026); `::telegram::api/request` reaches any method or parameter not wrapped here.

## Integration tests

The suite in `integration-test/` runs against a real bot when configured and otherwise skips every test. Bots are free; use a dedicated test bot.

| Environment Variable | Purpose |
|---|---|
| `TELEGRAM_BOT_TOKEN` | Test bot token |
| `TELEGRAM_TEST_CHAT_ID` | A chat the bot can post in (send the bot a message, then read `chat.id` from `getUpdates`); enables the message and chat tests |
| `TELEGRAM_TEST_WEBHOOK` | Set to `1` to run the set-webhook/delete-webhook round trip |
| `TELEGRAM_WEBHOOK_SECRET` | Optional, for `verify-request` with context |

```bash
hot test --project pkg-integration-telegram
```

| Test File | What It Tests | Side Effects |
|---|---|---|
| `bot.hot` | getMe, commands, name, descriptions | Temporarily changes commands and descriptions, then restores the originals |
| `messages.hot` | Send, edit, forward, copy, delete; location, dice, chat action | Sends messages to the test chat and deletes them |
| `chat.hot` | Chat info, member count, administrators, bot membership | Read-only (administrators requires a group) |
| `updates.hot` | Webhook info, getUpdates, webhook round trip | The round trip runs only with `TELEGRAM_TEST_WEBHOOK=1` and only when the bot has no webhook, then deletes its temporary one; it never replaces a real webhook (Telegram doesn't reveal a webhook's secret, so it couldn't be restored). getUpdates is skipped while a webhook is set and never confirms updates. |

## Documentation

- [Telegram Bot API](https://core.telegram.org/bots/api)
- [Hot package documentation](https://hot.dev/pkg/hot.dev/telegram)

## License

Apache-2.0 - see [LICENSE](LICENSE)
