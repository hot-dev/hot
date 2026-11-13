# slack-admin

Slack Admin API (`admin.*`) bindings for Hot, for Enterprise organizations: manage channels across workspaces (including bulk archive, delete and move, retention and sharing), workspaces, users and sessions, app approvals, emoji, invite requests, IDP groups, admin roles, information barriers, auth policies, analytics exports, workflows and functions. Failed calls come back as errors, not as `ok: false` bodies that look like success.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/slack-admin": "0.11.3"
```

It depends on `hot.dev/slack` (1.1.0), which is installed with it.

## Setup

| Context Variable | Description |
|---|---|
| `slack.admin.token` | A user token (`xoxp-...`) from an app installed on the **whole Enterprise organization** by an Org Owner or Org Admin, carrying the `admin.*` scopes your calls need. |

The Admin API does not accept bot tokens (`xoxb-...`) or tokens from a single-workspace install: those fail with `not_allowed_token_type` or `not_an_enterprise`. To get a suitable token:

1. Create a Slack app and add the **user token** scopes you need under **OAuth & Permissions** (for example `admin.conversations:write`, `admin.users:read`, `admin.teams:read`).
2. Have an Org Owner or Org Admin install it on the Enterprise organization (not a workspace) through the OAuth v2 flow. For channel methods, the installing admin also needs the Channel Management role.
3. Store the resulting user token as `slack.admin.token`.

This package uses its own context variable so a project can use a bot token with `hot.dev/slack` (`slack.api.key`) and an admin user token here side by side.

## Per-call credentials

Every function that calls Slack takes an optional trailing `opts` Map, so one run can act for many organizations and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and a `token` from `opts` is masked in call records.

| Key | Effect |
|---|---|
| `token` | The org-level admin user token for this call, instead of `slack.admin.token` |

`opts` always comes last.

```hot
::users ::slack::admin::users

tenant {token: "xoxp-tenant-b"}
page ::users/list(::users/ListRequest({team_id: "T0123ABCDEF", limit: 50}), tenant)
everyone ::users/list-all(::users/ListRequest({team_id: "T0123ABCDEF"}), 1000, tenant)
```

A `token` that is present but blank or null is an error naming the key, never a fallback to `slack.admin.token`. `::slack::admin::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::conversations ::slack::admin::conversations
::users ::slack::admin::users
::teams ::slack::admin::teams

// Find public channels with no messages since 2025-01-01 and archive them
stale ::conversations/lookup-all(::conversations/LookupRequest({
  team_ids: ["T0123ABCDEF"],
  last_message_activity_before: 1735689600
}), 100)
job ::conversations/bulk-archive(::conversations/BulkArchiveRequest({channel_ids: stale}))
job.bulk_action_id

// Offboard: sign out everywhere, then remove from a workspace
::users/session-reset(::users/SessionResetRequest({user_id: "U0123ABCDEF"}))
::users/remove-user(::users/RemoveUserRequest({team_id: "T0123ABCDEF", user_id: "U0123ABCDEF"}))

// Every workspace in the org
workspaces ::teams/list-all(::teams/ListRequest({}), 1000)
```

Each function takes a typed request record named after it (`archive` takes `ArchiveRequest`, `session-reset` takes `SessionResetRequest`) and returns Slack's response body. Unset fields are never sent. Lists of ids go out comma-separated and Map arguments (such as `prefs`) as the stringified JSON Slack expects.

## Functions by area

| Module | Slack methods |
|---|---|
| `::slack::admin::conversations` | `archive`, `unarchive`, `bulk-archive`, `bulk-delete`, `bulk-move`, `convert-to-private`, `convert-to-public`, `create`, `delete-conversation`, `rename`, `invite`, `search` / `search-all`, `lookup` / `lookup-all`, `get-teams` / `get-teams-all`, `set-teams`, `disconnect-shared`, `get-conversation-prefs`, `set-conversation-prefs`, `get-custom-retention`, `set-custom-retention`, `remove-custom-retention`, `restrict-access-add-group` / `-list-groups` / `-remove-group`, `ekm-list-original-connected-channel-info` |
| `::slack::admin::teams` | `create`, `list` / `list-all`, `admins-list`, `owners-list`, `settings-info`, `settings-set-name`, `settings-set-description`, `settings-set-icon`, `settings-set-discoverability`, `settings-set-default-channels` |
| `::slack::admin::users` | `list` / `list-all`, `invite`, `assign`, `remove-user`, `set-admin`, `set-owner`, `set-regular`, `set-expiration`, `get-expiration`, `session-list`, `session-reset`, `session-reset-bulk`, `session-invalidate`, `session-get-settings`, `session-set-settings`, `session-clear-settings`, `unsupported-versions-export` |
| `::slack::admin::apps` | `approve`, `restrict`, `clear-resolution`, `uninstall`, `approved-list`, `restricted-list`, `requests-list`, `requests-cancel`, `activities-list`, `config-lookup`, `config-set` |
| `::slack::admin::emoji` | `add-emoji`, `add-alias`, `list`, `rename`, `remove-emoji` |
| `::slack::admin::invite-requests` | `list`, `approve`, `deny`, `approved-list`, `denied-list` |
| `::slack::admin::usergroups` | `add-channels`, `add-teams`, `list-channels`, `remove-channels` |
| `::slack::admin::roles` | `list-assignments`, `add-assignments`, `remove-assignments` |
| `::slack::admin::barriers` | `list`, `create`, `update-barrier`, `delete-barrier` |
| `::slack::admin::auth` | `policy-get-entities`, `policy-assign-entities`, `policy-remove-entities` |
| `::slack::admin::analytics` | `get-records` (parsed records), `get-file` (gzipped JSON lines, returned as Bytes) |
| `::slack::admin::workflows` | `search` / `search-all`, `permissions-lookup`, `collaborators-add`, `collaborators-remove`, `unpublish` |
| `::slack::admin::functions` | `list`, `permissions-lookup`, `permissions-set` |
| `::slack::admin::api` | `call` (any admin method), `paginate`, `next-cursor`, `form-body`, `SlackError`, `is-rate-limited` |

Every cursor-paginated method also has a `-all` variant that follows `next_cursor` and returns the concatenated items, capped by `max-items` (default 1000). To page by hand, pass `cursor` from `::slack::admin::api/next-cursor(response)`.

For a method this package doesn't wrap, use `::slack::admin::api/call`:

```hot
res ::slack::admin::api/call("admin.teams.settings.info", {team_id: "T0123ABCDEF"})
res.team.name
```

## Errors

Slack reports failures with HTTP 200 and `ok: false`. Every function turns that into an `Err(SlackError)`, so a failed destructive call never reads as a success:

| Field | Meaning |
|---|---|
| `error` | Slack's error code: `missing_scope`, `not_allowed_token_type`, `channel_not_found`, `invalid_arguments`, ... |
| `messages` | `response_metadata.messages`, the per-argument detail Slack sends with `invalid_arguments` |
| `needed`, `provided` | Scopes, on `missing_scope` |
| `method`, `status`, `body` | The admin method, HTTP status, and raw body |
| `retry_after` | Seconds to wait, on HTTP 429 (`error: "ratelimited"`) |

```hot
::conversations ::slack::admin::conversations

result ::conversations/delete-conversation(::conversations/DeleteConversationRequest({channel_id: "C0123ABCDEF"}))
if-err(result, (e) {
  cond {
    ::slack::admin::api/is-rate-limited(e) => { `rate limited, retry in ${e.retry_after}s` }
    eq(e.error, "missing_scope") => { `needs ${e.needed}` }
    => { `${e.method} failed: ${e.error}` }
  }
})
```

Calls are not retried automatically.

## Webhooks

The Admin API has no webhooks of its own. To react to admin-relevant events (channel created, user changed), subscribe to the Events API in your Slack app and verify requests with `::slack::webhooks/verify-request` from `hot.dev/slack`.

## Notes and limits

- Requests are `POST https://slack.com/api/<method>` with a form-encoded body and `Authorization: Bearer <token>`, the encoding Slack's own SDKs use for these methods.
- Admin methods are rate-limited per method (mostly Tier 2, 20+ per minute; some Tier 3 or 4). Bulk jobs (`bulk-archive`, `bulk-delete`, `bulk-move`, `session-reset-bulk`) run in the background and return before they finish; only one bulk channel job runs at a time (`action_already_in_progress`).
- `bulk-delete` and `delete-conversation` are irreversible.
- `analytics/get-file` returns the gzip file as Bytes, and `analytics/get-records` decompresses and parses it. A file that decompresses past `compression.max-output-bytes` (64 MiB by default) is an Err from `get-records`; read those with `get-file` and `::hot::gzip/decode-stream`.
- `emoji/list` returns `emoji` as a Map of names, so it has no `-all` variant; page it with `cursor`.
- Some arguments are required only in some cases (for example `team_id` with an org-level token on `invite-requests`, or exactly one of `app_id` / `request_id` on `apps/approve`). These are typed as optional and documented on each function.
- There is no free sandbox with Admin API access on a normal plan. Slack's Developer Program can provision Enterprise sandboxes for testing.

## Tests

Unit tests cover form encoding, request building, `ok: false` and HTTP error handling, rate limits, cursor pagination and the analytics response: `hot test -p pkg-test-slack-admin`. There is no live integration suite.

## Documentation

- [Slack Admin API methods](https://docs.slack.dev/reference/methods/?family=admin)
- [Admin API overview](https://docs.slack.dev/admins/)
- [Hot package documentation](https://hot.dev/pkg/hot.dev/slack-admin)

## License

Apache-2.0 - see [LICENSE](LICENSE)
