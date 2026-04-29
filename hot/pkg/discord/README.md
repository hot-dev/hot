# discord

Discord API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/discord": "1.1.0"
```

## Configuration

The `discord.bot.token` context variable is required. Set it to your Discord Bot Token via the Hot app.

For interaction webhook verification, also set `discord.public.key` to your application's Ed25519 public key.

## Usage

### Send a Message

```hot
::messages ::discord::messages

response ::messages/create-message(::messages/CreateMessageRequest({
  channel_id: "C01ABCDEF",
  content: "Hello from Hot!"
}))
```

### Send a Message with Embeds

```hot
::messages ::discord::messages

response ::messages/create-message(::messages/CreateMessageRequest({
  channel_id: "C01ABCDEF",
  embeds: [{
    title: "Deployment Complete",
    description: "Version 2.1.0 deployed to production",
    color: 3066993
  }]
}))
```

### Execute a Webhook (no bot auth required)

```hot
::webhooks ::discord::webhooks

::webhooks/execute-webhook(::webhooks/ExecuteWebhookRequest({
  webhook_id: "123456789",
  webhook_token: "your-webhook-token",
  content: "Alert: deployment complete!"
}))
```

### Reply to a Slash Command

```hot
::interactions ::discord::interactions

// Immediate response
::interactions/create-interaction-response(::interactions/CreateInteractionResponseRequest({
  interaction_id: event.id,
  interaction_token: event.token,
  type: 4,
  data: {content: "Hello from Hot!"}
}))
```

### Send a DM

```hot
::users ::discord::users
::messages ::discord::messages

dm ::users/create-dm(::users/CreateDMRequest({
  recipient_id: "123456789"
}))

::messages/create-message(::messages/CreateMessageRequest({
  channel_id: dm.id,
  content: "Hello via DM!"
}))
```

### List Guild Members

```hot
::guilds ::discord::guilds

members ::guilds/list-guild-members(::guilds/ListGuildMembersRequest({
  guild_id: "123456789",
  limit: 100
}))
```

## API Base URL

`https://discord.com/api/v10`

## Modules

| Module | Description |
|--------|-------------|
| `::discord::messages` | Send, edit, delete messages; reactions; pins |
| `::discord::channels` | Channel CRUD, permissions, invites, threads |
| `::discord::guilds` | Guild info, members, roles, bans, emoji, audit log |
| `::discord::users` | Current user, get user, DMs, guilds |
| `::discord::webhooks` | Webhook CRUD, execute, Ed25519 verification |
| `::discord::interactions` | Slash commands, interaction responses, followups |
| `::discord::api` | Low-level authenticated HTTP client |
| `::discord::core` | Shared configuration (BASE_URL) |

## Integration Tests

Integration tests run against the live Discord API. They require a bot account and a test server.

### 1. Create a Discord Application & Bot

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
2. Click **New Application**, give it a name (e.g. "Hot Test Bot")
3. Go to the **Bot** tab and click **Reset Token** to generate a bot token — copy it
4. Under **Privileged Gateway Intents**, enable **Server Members Intent** (needed for `list-guild-members` tests)

### 2. Create a Test Server

1. In Discord, click the **+** button in the server list to create a new server
2. Name it something like "Hot Integration Tests"
3. Create a dedicated text channel for tests (e.g. `#bot-testing`)

### 3. Invite the Bot to Your Test Server

Build an OAuth2 URL with the permissions the tests need:

1. In the Developer Portal, go to your app's **OAuth2** tab
2. Under **OAuth2 URL Generator**, select the `bot` scope
3. Select these **Bot Permissions**:
   - Send Messages
   - Read Message History
   - Manage Messages (for delete, bulk delete)
   - Add Reactions
   - Manage Webhooks
   - View Channels
   - Manage Channels (for channel info tests)
   - Manage Roles (for role tests)
4. Copy the generated URL, open it in your browser, and select your test server

### 4. Get Your IDs

Right-click items in Discord (with **Developer Mode** enabled in Settings > Advanced):

- **Channel ID**: Right-click the `#bot-testing` channel → Copy Channel ID
- **Guild ID**: Right-click the server name → Copy Server ID

### 5. Set Environment Variables

Add these to your `.env` file:

```
DISCORD_TEST_CHANNEL_ID=123456789012345678
DISCORD_TEST_GUILD_ID=123456789012345678
```

### 6. Set Context Variables

Set the bot token via the Hot app context variables:

```
discord.bot.token=your-bot-token-here
```

### 7. Run the Tests

```bash
hot test hot/pkg/discord/integration-test/
```

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `messages.hot` | Send, get, edit, delete messages; reactions; pins | Creates/deletes messages in the test channel |
| `channels.hot` | Get channel info, list guild channels | Read-only |
| `webhooks.hot` | Create webhook, execute it, delete it | Creates/deletes a webhook and a message |
| `users.hot` | Get current bot user, list bot guilds | Read-only |
| `guilds.hot` | Get guild info, roles, members | Read-only |

All tests that create resources clean up after themselves.

## Documentation

- [Discord API Documentation](https://discord.com/developers/docs)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/discord)

## License

Apache-2.0 - see [LICENSE](LICENSE)
