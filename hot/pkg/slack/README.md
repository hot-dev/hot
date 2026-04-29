# slack

Slack API bindings for Hot.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/slack": "1.1.0"
```

## Configuration

The `slack.api.key` context variable is required. Set it to your Slack Bot User OAuth Token via the Hot app.

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
