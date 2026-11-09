# intercom

Intercom REST API bindings for Hot, covering the surface a support agent works. Conversations can be searched with cursor paging, replied to, noted, assigned, snoozed, reopened and closed. The package also covers contacts with notes and companies, tags, tickets, Help Center articles, data events, and admins, and verifies webhooks with `X-Hub-Signature`. Every request pins `Intercom-Version: 2.17` and goes to your workspace's region (US, EU or AU). `::intercom::api/request` covers any endpoint that isn't wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/intercom": "0.1.2"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `intercom.token` | yes, unless every call passes `token` | Access token from the Developer Hub (your app → Authentication), or an OAuth token |
| `intercom.admin.id` | for actions | The admin to act as for replies, notes, assignment, state changes and conversation tags. `::intercom::admins/me().id` is the token's own admin |
| `intercom.region` | no | `us` (default), `eu` (`api.eu.intercom.io`) or `au` (`api.au.intercom.io`). It must match the region your workspace is hosted in |
| `intercom.version` | no | Override the pinned `Intercom-Version` (default `2.17`) |
| `intercom.client.secret` | for webhooks | Your app's client secret (Developer Hub → Basic information). Used by `::intercom::webhooks/verify-request` |

## Per-call credentials

Every function that calls Intercom takes an optional trailing `opts` Map, so one run can act for many workspaces and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and a `token` from `opts` is masked in call records.

| Key | Effect |
|---|---|
| `token` | The access token for this call, instead of `intercom.token` |
| `region` | `us`, `eu` or `au` for this call, instead of `intercom.region` |
| `version` | The `Intercom-Version` for this call |
| `admin-id` | The admin to act as, instead of `intercom.admin.id`. Replies, notes, assignment, state changes and conversation tags need it when `opts` has a `token`, because `intercom.admin.id` belongs to the workspace of `intercom.token` |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
::conversations ::intercom::conversations

tenant {token: "tenant-b-token", region: "eu", admin-id: "991"}
page ::conversations/list-conversations({per_page: 20}, tenant)
::conversations/add-note("12345", "Escalated to tier 2.", tenant)
```

A `token` that is present but blank or null is an error naming the key, never a fallback to `intercom.token`. `::intercom::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::admins ::intercom::admins
::conversations ::intercom::conversations
::tags ::intercom::tags

// Find the admin id to put in intercom.admin.id
admin ::admins/me()

page ::conversations/search-conversations({
  operator: "AND",
  value: [
    {field: "open", operator: "=", value: true},
    {field: "priority", operator: "=", value: "priority"}
  ]
}, {per_page: 20})

convo first(page.conversations)
::conversations/add-note(convo.id, "Customer is on the enterprise plan; escalate to tier 2.")
::conversations/reply(convo.id, "Thanks for the report! A fix ships this week.")
::tags/tag-conversation(convo.id, ::tags/create-tag("escalated").id)
::conversations/snooze-conversation(convo.id, add(::hot::time/epoch-seconds(), 86400))
```

## Functions

**Conversations** (`::intercom::conversations`)
- `list-conversations(options?)` and `search-conversations(query, options?)` return one page: `{conversations, pages, total_count}`. `options` takes `per_page` (max 150) and `starting_after`
- `search-all-conversations(query, limit)`: follows cursors and returns a Vec
- `get-conversation(id, options?)`
- `reply(id, body, options?)` (visible to the customer) and `add-note(id, body)` (internal)
- `assign-conversation(id, assignee-id, {type: "admin"|"team", body, skip_reopen})`, `snooze-conversation(id, until-unix-seconds)`, `open-conversation(id)`, `close-conversation(id, message?)`

**Contacts** (`::intercom::contacts`)
- `create-contact(fields)`, `get-contact(id)`, `update-contact(id, fields)`, `find-contact-by-email(email)` (null when none), `find-contact-by-external-id(id)`
- `list-contacts(options?)`, `search-contacts(query, options?)`, `search-all-contacts(query, limit)`
- Notes: `create-note(contact-id, body, admin-id?)`, `list-notes(contact-id, options?)`
- Companies: `attach-company(contact-id, company-id)`, `detach-company`, `list-companies(contact-id)`

**Companies** (`::intercom::companies`)
- `upsert-company(fields)` (create or update by your `company_id`), `update-company(id, fields)`, `get-company(id)`, `find-company(company-id)`, `list-companies(options?)`
- `attach-contact(company-id, contact-id)`, `detach-contact`, `list-contacts(company-id, options?)`

**Tags** (`::intercom::tags`)
- `list-tags()`, `create-tag(name)`, `rename-tag(id, name)`, `get-tag(id)`, `delete-tag(id)`
- `tag-contact(contact-id, tag-id)`, `untag-contact`, `tag-conversation(conversation-id, tag-id)`, `untag-conversation`

**Tickets** (`::intercom::tickets`)
- `list-ticket-types()`, `get-ticket-type(id)`, `list-ticket-states()`
- `create-ticket(ticket-type-id, contacts, {ticket_attributes, company_id, assignment, ...})`, `get-ticket(id)`, `update-ticket(id, changes)`
- `search-tickets(query, options?)`, `search-all-tickets(query, limit)`

**Help Center** (`::intercom::articles`)
- `list-articles(options?)`, `get-article(id)`, `search-articles(phrase, {state, help_center_id, highlight})`

**Data events** (`::intercom::events`)
- `submit-event(event-name, {email}|{user_id}|{id}, {created_at, metadata})`: `created_at` defaults to now
- `list-events(contact, options?)`: events from the last 90 days

**Admins** (`::intercom::admins`)
- `me()`, `list-admins()`, `get-admin(id)`, `find-admin-by-email(email)`, `list-teams()`

**Escape hatch** (`::intercom::api`)
- `request(method, path, body, opts?)` (pass `""` for no body), `resolve(opts)`, `next-cursor(page)`, `collect-pages(fetch, key, cursor, limit, acc)`

## Pagination

List and search endpoints return one page plus `pages.next.starting_after`. `::intercom::api/next-cursor(page)` reads that cursor and returns null on the last page. Pass it back as `starting_after`:

```hot
::conversations ::intercom::conversations

first-page ::conversations/list-conversations({per_page: 50})
cursor ::intercom::api/next-cursor(first-page)
second-page ::conversations/list-conversations({per_page: 50, starting_after: cursor})

// Or let the package follow cursors up to a limit
open ::conversations/search-all-conversations({field: "open", operator: "=", value: true}, 500)
```

Articles, companies and contact notes page by number (`page`, `per_page`) instead.

## Webhooks

Intercom signs each notification with `X-Hub-Signature: sha1=<hex>`, the HMAC-SHA1 of the raw body keyed with your app's client secret. `verify-request` checks it over `::hot::http/raw-body` in constant time and accepts only the `sha1=` scheme. It fails closed: an unset `intercom.client.secret` halts with a message, and an empty explicit secret never verifies.

```hot
intercom-webhook
meta {webhook: {service: "intercom", path: "/intercom/webhook"}}
fn (request) {
  if(not(::intercom::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: request.body.topic}))
}
```

Intercom signs no timestamp, so there is no replay window to enforce. Notifications can arrive more than once, so deduplicate on the notification `id`. Intercom also sends a `HEAD` request to validate the endpoint URL when you register it.

## Notes and limits

- Functions return the decoded JSON body. Empty 202/204 responses (such as `submit-event`) return null. Non-2xx responses return an Err carrying `HttpError {status, headers, body}`, with Intercom's `errors` list in `body`.
- Request bodies omit unset (null) fields instead of sending JSON `null`.
- Intercom's rate limits are per app and per workspace. A 429 includes `X-RateLimit-Reset` in `HttpError.headers`.
- `2.17` evaluates contact-search date filters in UTC and moves QA attributes out of conversation `custom_attributes`. Set `intercom.version` to stay on an older version.
- `GET /companies` stops at 10,000 companies. Use `::intercom::api/request` with `/companies/scroll` beyond that.

## License

Apache-2.0 - see [LICENSE](LICENSE)
