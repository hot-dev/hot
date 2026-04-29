# slack-admin

Slack Admin API bindings for Hot. Requires the `slack` package for shared authentication and HTTP client.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/slack-admin": "0.10.0"
```

## Configuration

Uses the same `slack.api.key` context variable as the `slack` package. Set it to your Slack Bot User OAuth Token via the Hot app.

## Includes

All endpoints live in the `::slack::admin` namespace:

| API Group | Endpoints |
|-----------|-----------|
| admin.conversations | archive, create, delete, rename, invite, search, unarchive, getTeams, setTeams, getConversationPrefs, setConversationPrefs, convertToPrivate, disconnectShared, restrictAccess, ekm |
| admin.teams | create, list, admins.list, owners.list, settings (info, setDefaultChannels, setDescription, setDiscoverability, setIcon, setName) |
| admin.users | assign, invite, list, remove, setAdmin, setOwner, setRegular, setExpiration, session.invalidate, session.reset |
| admin.apps | approve, restrict, approved.list, restricted.list, requests.list |
| admin.emoji | add, addAlias, list, remove, rename |
| admin.inviteRequests | approve, deny, list, approved.list, denied.list |
| admin.usergroups | addChannels, addTeams, listChannels, removeChannels |

## Documentation

- [Slack Admin API Documentation](https://api.slack.com/methods?filter=admin)
- [Hot Package Documentation](https://hot.dev/pkg/slack-admin)

## License

Apache-2.0 - see [LICENSE](LICENSE)
