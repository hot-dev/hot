# slack

Slack API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/slack": "1.2.0"
```

## Configuration

Set the `slack.api.key` context variable to your Slack Bot User OAuth Token via the Hot app, or pass a token per call (see [Per-call credentials](#per-call-credentials)). A call with neither fails with an error naming the variable.

## Per-call credentials

Every function that calls Slack takes an optional trailing `opts` Map, so one run can act for several workspaces. `api-key` replaces `slack.api.key` for that call only. Nothing is written to ctx, so parallel flows can't see each other's tokens. A null or blank `api-key` is an error, never a fallback to the context token.

To verify webhooks per app, pass the signing secret directly or as `signing-secret` in `opts`, in place of the `slack.signing.secret` context variable.

```hot
::messaging ::slack::messaging
::webhooks ::slack::webhooks

post-for-workspace fn (workspace: Map, text: Str): Any {
  ::messaging/chat-post-message(::messaging/ChatPostMessageRequest({
    channel: workspace.channel,
    text: text
  }), {api-key: workspace.token})
}

is-from-workspace fn (request: HttpRequest, workspace: Map): Bool {
  ::webhooks/verify-request(request, {signing-secret: workspace.signing-secret})
}
```

Slack answers most failures, a rejected token included, with HTTP 200 and `{ok: false, error: "..."}`, so check `response.ok`.

## Usage

### Send a Message

```hot
::messaging ::slack::messaging

response ::messaging/chat-post-message(::messaging/ChatPostMessageRequest({
  channel: "C01ABCDEF",
  text: "Hello from Hot!"
}))
```

### List Channels

```hot
::channels ::slack::channels

response ::channels/conversations-list(::channels/ConversationsListRequest({}))
for-each(response.channels, (ch) {
  println(ch.name)
})
```

### Get User Info

```hot
::users ::slack::users

response ::users/users-info(::users/UsersInfoRequest({
  user: "U01ABCDEF"
}))
println(response.user.real_name)
```

### Upload a File

```hot
::files ::slack::files

response ::files/files-upload(::files/FilesUploadRequest({
  channels: "C01ABCDEF",
  filename: "report.txt",
  content: "File contents here",
  title: "My Report"
}))
```

## API Base URL

`https://slack.com/api`

## Modules

| Module | Description |
|--------|-------------|
| `::slack::messaging` | Chat messages, reactions, pins, scheduled messages |
| `::slack::channels` | Conversations (channels, DMs, groups) |
| `::slack::users` | User info, lists, profiles |
| `::slack::files` | File uploads, listing, sharing, deletion |
| `::slack::apps` | Bot info and app management |
| `::slack::calls` | RTM connections |
| `::slack::misc` | Auth, team info, emoji, views, dialogs, OAuth |
| `::slack::api` | Low-level authenticated HTTP client |
| `::slack::core` | Shared configuration (BASE_URL) |

## Documentation

- [Slack API Documentation](https://api.slack.com/methods)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/slack)

## License

Apache-2.0 - see [LICENSE](LICENSE)
