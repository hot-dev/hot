# discord

Discord API v10 bindings for Hot: send messages with embeds, components, polls and file uploads; manage channels, threads, members, roles, bans, emoji, stickers, scheduled events and auto-moderation rules; run webhooks; register application commands; and serve an interactions endpoint with Ed25519 request verification.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/discord": "1.1.0"
```

## Setup

| Context Variable | Description |
|---|---|
| `discord.bot.token` | Bot token from the Developer Portal (**Bot** tab, **Reset Token**). Sent as `Authorization: Bot <token>`. Not needed for webhook-token and interaction-token calls. |
| `discord.public.key` | Application public key (**General Information** tab). Needed by `::discord::webhooks/verify-request` and `::discord::interactions/handle-request`. |

Invite the bot with the permissions your calls need (Send Messages, Manage Messages, Manage Roles, and so on). Listing guild members needs the privileged **Server Members Intent**, and reading message `content` needs the **Message Content Intent**.

## Quick start

```hot
::messages ::discord::messages
::guilds ::discord::guilds

// Send a message with an embed and a file
sent ::messages/create-message(::messages/CreateMessageRequest({
  channel_id: "1100000000000000000",
  content: "Nightly report",
  embeds: [{title: "Build 2026.11.01", description: "All green", color: 3066993}],
  files: [{filename: "report.csv", content: "suite,passed\napi,412\n"}]
}))

// Edit it: only the fields you set change
::messages/edit-message(::messages/EditMessageRequest({
  channel_id: "1100000000000000000",
  message_id: sent.id,
  content: "Nightly report (re-run)"
}))

// Time a member out for an hour, with an audit-log reason
::guilds/modify-guild-member(::guilds/ModifyGuildMemberRequest({
  guild_id: "1000000000000000000",
  user_id: "1300000000000000000",
  communication_disabled_until: "2026-11-01T18:00:00Z",
  reason: "Spam in #general"
}))

// Lift it again: `clear` sends an explicit null
::guilds/modify-guild-member(::guilds/ModifyGuildMemberRequest({
  guild_id: "1000000000000000000",
  user_id: "1300000000000000000",
  clear: ["communication_disabled_until"]
}))

// Page through history and members
history ::messages/get-message-history("1100000000000000000", 500)
members ::guilds/list-all-guild-members("1000000000000000000")
```

## Per-call credentials

Every function that calls Discord takes an optional trailing `opts` Map, so one run can act as many bots and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and a `token` from `opts` is masked in call records.

| Key | Effect |
|---|---|
| `token` | The bot token for this call, instead of `discord.bot.token` |

`opts` always comes last.

```hot
::messages ::discord::messages

tenant {token: "tenant-b-bot-token"}
::messages/create-message(::messages/CreateMessageRequest({channel_id: "1100000000000000000", content: "Deploy finished"}), tenant)
me ::discord::users/get-current-user(tenant)
```

A `token` that is present but blank or null is an error naming the key, never a fallback to `discord.bot.token`. `::discord::api/resolve(opts)` is the one place this is decided. Webhook-token and interaction-token routes never send a bot token, even with a `token` in `opts`. The tokens in their paths are masked in call records.

## Functions by area

| Module | What it covers |
|---|---|
| `::discord::messages` | `get-channel-messages`, `get-message-history` (pages back), `get-message`, `create-message`, `edit-message`, `delete-message`, `bulk-delete-messages`, `crosspost-message`; reactions (`create-reaction`, `get-reactions`, `delete-*-reaction*`); pins (`get-channel-pins`, `get-all-pins`, `pin-message`, `unpin-message`) |
| `::discord::channels` | `get-channel`, `modify-channel`, `delete-channel`, permission overwrites, invites, `follow-announcement-channel`, `trigger-typing-indicator`, `get-guild-channels`, `create-guild-channel`, `modify-guild-channel-positions`; threads (`start-thread-from-message`, `start-thread-without-message`, `start-forum-thread`, join/leave, members, active and archived lists) |
| `::discord::guilds` | `get-guild`, `get-guild-preview`, `modify-guild`; members (`get-guild-member`, `list-guild-members`, `list-all-guild-members`, `search-guild-members`, `modify-guild-member`, `modify-current-member`, role add/remove, `remove-guild-member`); roles (CRUD, positions); bans (`get-guild-bans`, `get-all-guild-bans`, `create-guild-ban`, `bulk-guild-ban`, `remove-guild-ban`); emoji (CRUD); prune; `get-guild-audit-log`; `get-guild-invites` |
| `::discord::users` | `get-current-user`, `get-user`, `modify-current-user`, `get-current-user-guilds`, `get-all-current-user-guilds`, `leave-guild`, `create-dm` |
| `::discord::webhooks` | Webhook CRUD with the bot token or the webhook token, `execute-webhook` (files, threads, forum posts), webhook message get/edit/delete, `verify-request` (Ed25519) |
| `::discord::interactions` | `handle-request` (verify + PING/PONG + inline reply), response builders (`message-response`, `deferred-response`, `update-message-response`, `autocomplete-response`, `modal-response`), `option-value`, `create-interaction-response`, original response and followup CRUD, application commands (global and guild: list, create, get, edit, delete, bulk overwrite), `get-guild-command-permissions` |
| `::discord::applications` | `get-current-application` (your `application_id`), `edit-current-application` |
| `::discord::events` | Guild scheduled events: list, create, get, modify (start, end, cancel), delete, interested users |
| `::discord::automod` | Auto-moderation rules: list, get, create, modify, delete |
| `::discord::stickers` | `get-sticker`, `list-sticker-packs`, guild sticker list/get/create (upload)/modify/delete |
| `::discord::api` | `call`, `call-ok`, `build-request`, `body`, `query`, `paginate`, `multipart-body`, `HttpError`, `is-rate-limited`, and the raw `request` |

### Unset fields, `clear`, and `reason`

Request bodies only carry the fields you set. Discord reads a JSON `null` as "clear this field", so an unset optional is never sent. When you do want to clear a field (remove an avatar, lift a timeout, move a channel out of its category, disconnect a member from voice), name it in the request's `clear` list: `clear: ["avatar"]`.

Moderation and modify calls take `reason`, which Discord stores in the guild audit log (sent URL-encoded as `X-Audit-Log-Reason`).

### Files

`create-message`, `edit-message`, `execute-webhook`, `edit-webhook-message`, `start-forum-thread`, `create-interaction-response`, the original-response edit and followups take `files`: `[{filename, content, content_type?, description?}]`. `content` is Bytes (for example from `::hot::file/read-file-bytes`) or text. The request goes out as `multipart/form-data` with `payload_json` and `files[n]`; create calls fill in `attachments` from `files` for you. Refer to an uploaded image in an embed as `attachment://<filename>`.

```hot
::messages ::discord::messages

::messages/create-message(::messages/CreateMessageRequest({
  channel_id: "1100000000000000000",
  embeds: [{title: "Latency", image: {url: "attachment://latency.png"}}],
  files: [{filename: "latency.png", content: ::hot::file/read-file-bytes("latency.png")}]
}))
```

## Interactions endpoint

Point your application's **Interactions Endpoint URL** at a Hot webhook. `handle-request` verifies the Ed25519 signature over the raw body (with a 5-minute replay window), answers 401 when it fails, answers Discord's PING with PONG, and otherwise returns whatever your handler builds as the inline response:

```hot
::myapp::discord ns

::interactions ::discord::interactions

HttpRequest ::hot::http/HttpRequest
HttpResponse ::hot::http/HttpResponse

on-discord-interaction
meta {webhook: {service: "discord", path: "/interactions", method: "POST"}}
fn (request: HttpRequest): HttpResponse {
  ::interactions/handle-request(request, (interaction) {
    command get-in(interaction, ["data", "name"])
    cond {
      eq(command, "hello") => {
        who or(::interactions/option-value(interaction, "name"), "world")
        ::interactions/message-response({content: `Hello, ${who}!`})
      }
      eq(command, "report") => {
        // Longer work: ACK now (ephemeral), send the result later
        send("report:requested", {application_id: interaction.application_id, token: interaction.token})
        ::interactions/deferred-response(true)
      }
      => { ::interactions/message-response({content: "Unknown command"}, true) }
    }
  })
}

on-report-requested
meta {on-event: "report:requested"}
fn (event) {
  ::interactions/edit-original-interaction-response(::interactions/EditOriginalInteractionResponseRequest({
    application_id: event.data.application_id,
    interaction_token: event.data.token,
    content: "Report ready"
  }))
}
```

Register the commands once (guild commands appear instantly, global ones are for production):

```hot
::interactions ::discord::interactions

app ::discord::applications/get-current-application()
::interactions/bulk-overwrite-guild-commands(::interactions/BulkOverwriteGuildCommandsRequest({
  application_id: app.id,
  guild_id: "1000000000000000000",
  commands: [
    {name: "hello", description: "Say hello", options: [{name: "name", description: "Who to greet", type: 3}]},
    {name: "report", description: "Build the daily report"}
  ]
}))
```

To verify by hand, `::discord::webhooks/verify-request(request)` returns a Bool (pass a public key as the second argument, and `0` as the third to disable the replay window on hosts with an unreliable clock). The same check covers Discord's webhook events, which are signed the same way.

## Errors and rate limits

Every call returns the parsed body, or an `Err(HttpError)` for non-2xx responses: `{status, headers, body}` where `body` is Discord's `{code, message, errors?}`. On HTTP 429 the error also carries `retry_after` (seconds), `global` and `scope`; check with `::discord::api/is-rate-limited`:

```hot
result ::discord::messages/create-message(::discord::messages/CreateMessageRequest({
  channel_id: "1100000000000000000",
  content: "hello"
}))
if-err(result, (e) {
  if(::discord::api/is-rate-limited(e), `rate limited, retry in ${e.retry_after}s`, `failed: ${e.status}`)
})
```

Calls are not retried automatically.

## Notes and limits

- Requests go to `https://discord.com/api/v10` with a `DiscordBot (https://hot.dev/pkg/hot.dev/discord, 1.1.0)` User-Agent.
- Pins use the current `/channels/{id}/messages/pins` endpoints, which return `{items: [{pinned_at, message}], has_more}`; the older `/channels/{id}/pins` routes are deprecated. Pinning needs the `PIN_MESSAGES` permission.
- Reaction `emoji` takes the character itself (`"👍"`) or `name:id` for custom emoji; it is URL-encoded for you.
- Webhook-token and interaction-token routes never send the bot token, so an HTTP-only interactions app only needs `discord.public.key`.
- Interaction tokens last 15 minutes and the first response must arrive within 3 seconds; use `deferred-response` for anything slower.
- The Gateway (WebSocket events, presence, voice) is not covered; use an interactions endpoint or webhook events for inbound traffic.

## Integration tests

The suite in `integration-test/` runs against the live API. Without credentials each test prints a skip line and passes.

1. Create an application and bot in the [Developer Portal](https://discord.com/developers/applications) and copy the bot token. Enable the **Server Members Intent**.
2. Create a test server with a `#bot-testing` channel and invite the bot (OAuth2 URL Generator, `bot` scope) with Send Messages, Read Message History, Manage Messages, Pin Messages, Add Reactions, Manage Webhooks, View Channels, Manage Channels and Manage Roles.
3. With Developer Mode on, copy the channel and server ids.
4. Set the environment variables (the suite's `ctx.hot` copies the token and key into context):

```
DISCORD_BOT_TOKEN=your-bot-token
DISCORD_PUBLIC_KEY=your-application-public-key
DISCORD_TEST_CHANNEL_ID=123456789012345678
DISCORD_TEST_GUILD_ID=123456789012345678
```

5. Run `hot test -p pkg-integration-discord`.

| Test file | What it tests | Side effects |
|---|---|---|
| `messages.hot` | Send, get, edit, delete; reactions; pins | Creates and deletes messages in the test channel |
| `channels.hot` | Get channel, list guild channels | Read-only |
| `webhooks.hot` | Create, execute, read and delete a webhook | Creates and deletes a webhook of its own |
| `users.hot` | Current bot user and its guilds | Read-only |
| `guilds.hot` | Guild info, roles, members | Read-only |

Unit tests (request building, multipart, errors, pagination, interactions and signature verification) need no credentials: `hot test -p pkg-test-discord`.

## Documentation

- [Discord developer documentation](https://discord.com/developers/docs)
- [Hot package documentation](https://hot.dev/pkg/hot.dev/discord)

## License

Apache-2.0 - see [LICENSE](LICENSE)
