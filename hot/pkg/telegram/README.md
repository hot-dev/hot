# telegram

Telegram Bot API bindings for Hot. Send messages, manage chats, handle commands, and process webhooks.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/telegram": "1.1.0"
```

## Configuration

The `telegram.bot.token` context variable is required. Set it to your Telegram Bot Token via the Hot app.

For webhook verification, also set `telegram.webhook.secret` to the secret token you used when calling `setWebhook`.

## Usage

### Send a Message

```hot
::messages ::telegram::messages

response ::messages/send-message(::messages/SendMessageRequest({
  chat_id: 123456789,
  text: "Hello from Hot!"
}))
response.message_id  // => 42
```

### Send a Message with Formatting

```hot
::messages ::telegram::messages

response ::messages/send-message(::messages/SendMessageRequest({
  chat_id: 123456789,
  text: "<b>Bold</b>, <i>italic</i>, and <code>code</code>",
  parse_mode: "HTML"
}))
```

### Send a Photo

```hot
::messages ::telegram::messages

response ::messages/send-photo(::messages/SendPhotoRequest({
  chat_id: 123456789,
  photo: "https://example.com/image.jpg",
  caption: "Check this out!"
}))
```

### Send an Inline Keyboard

```hot
::messages ::telegram::messages

response ::messages/send-message(::messages/SendMessageRequest({
  chat_id: 123456789,
  text: "Choose an option:",
  reply_markup: {
    inline_keyboard: [
      [{text: "Option A", callback_data: "a"}, {text: "Option B", callback_data: "b"}],
      [{text: "Visit Site", url: "https://hot.dev"}]
    ]
  }
}))
```

### Handle a Callback Query

```hot
::messages ::telegram::messages

::messages/answer-callback-query(::messages/AnswerCallbackQueryRequest({
  callback_query_id: event.callback_query.id,
  text: "You chose an option!"
}))
```

### Set Bot Commands

```hot
::bot ::telegram::bot

::bot/set-my-commands(::bot/SetMyCommandsRequest({
  commands: [
    {command: "start", description: "Start the bot"},
    {command: "help", description: "Show help"},
    {command: "status", description: "Check system status"}
  ]
}))
```

### Set Up a Webhook

```hot
::updates ::telegram::updates

::updates/set-webhook(::updates/SetWebhookRequest({
  url: "https://myapp.hot.dev/telegram/webhook",
  secret_token: "my-secret-token",
  allowed_updates: ["message", "callback_query"]
}))
```

### Verify a Webhook Request

```hot
::updates ::telegram::updates

is-valid ::updates/verify-request(request)
```

### Get Chat Info

```hot
::chat ::telegram::chat

response ::chat/get-chat(::chat/GetChatRequest({
  chat_id: -1001234567890
}))
response.title  // => "My Group"
```

## API Base URL

`https://api.telegram.org/bot{token}/{method}`

## Modules

| Module | Description |
|--------|-------------|
| `::telegram::messages` | Send, edit, delete messages; media; polls; callbacks |
| `::telegram::chat` | Chat info, members, bans, permissions, invites |
| `::telegram::bot` | Bot info, name, description, commands |
| `::telegram::updates` | Webhook setup, polling, webhook verification |
| `::telegram::api` | Low-level Bot API client |
| `::telegram::core` | Shared configuration (BASE_URL) |

## Integration Tests

Integration tests run against the live Telegram Bot API.

### 1. Create a Bot via BotFather

1. Open Telegram and search for [@BotFather](https://t.me/BotFather)
2. Send `/newbot` and follow the prompts to name your bot
3. BotFather will give you a **bot token** like `123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11` -- copy it

### 2. Get a Test Chat ID

The bot needs a chat to send messages to. The easiest way:

1. Start a conversation with your bot in Telegram (search for it by username, click "Start")
2. Send any message to the bot
3. Visit `https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates` in your browser
4. Find the `chat.id` field in the response -- this is your test chat ID

For group testing:
1. Add the bot to a test group
2. Send a message in the group
3. Check `getUpdates` for the group's `chat.id` (negative number for groups)

### 3. Set Context Variables

```
telegram.bot.token=123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11
```

### 4. Set Environment Variables

Add to your `.env` file:

```
TELEGRAM_TEST_CHAT_ID=123456789
```

### 5. Run the Tests

```bash
hot test hot/pkg/telegram/integration-test/
```

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `bot.hot` | getMe, set/get/delete commands | Temporarily sets bot commands (cleaned up) |
| `messages.hot` | Send, edit, delete message; typing indicator | Sends/deletes a message in test chat |
| `chat.hot` | Get chat info, member count | Read-only |
| `updates.hot` | Get webhook info | Read-only |

All tests that create resources clean up after themselves.

## Documentation

- [Telegram Bot API Documentation](https://core.telegram.org/bots/api)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/telegram)

## License

Apache-2.0 - see [LICENSE](LICENSE)
