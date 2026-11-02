# zendesk

Zendesk Support API bindings for Hot. Tickets are covered end to end: create, update, comment, merge, tag, and list with cursor pagination. The package also wraps ticket fields, satisfaction ratings and the cursor-based incremental export, plus users, organizations, views, macros, search and search export, and side conversations. Webhook deliveries are verified against their signed timestamp. It authenticates with an API token or an OAuth access token. `::zendesk::api/request` covers any endpoint that isn't wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/zendesk": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `zendesk.subdomain` | yes, unless a call's `opts` has it | `acme`, `acme.zendesk.com` or `https://acme.zendesk.com`, normalised to `acme`. Values that aren't a Zendesk handle are rejected |
| `zendesk.email` | with an API token | The agent or admin the API token acts as |
| `zendesk.api.token` | with an API token | Admin Center → Apps and integrations → APIs → Zendesk API. Sent as `Basic base64("{email}/token:{token}")` |
| `zendesk.oauth.token` | or OAuth | An OAuth access token, sent as `Bearer`. It wins when both forms are set |
| `zendesk.webhook.secret` | for webhooks | The webhook's signing secret. Used by `::zendesk::webhooks/verify-request` |

## Per-call credentials

Every function that calls Zendesk takes an optional trailing `opts` Map, so one run can act for many accounts and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and credentials from `opts` are masked in call records.

| Key | Effect |
|---|---|
| `email`, `api-token` | API token auth for this call. Pass both in the same `opts` |
| `oauth-token` | An OAuth access token for this call. It wins over `email` and `api-token` |
| `subdomain` | Another account's subdomain. Needs credentials in the same `opts`; context credentials are never sent to a per-call account |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
::tickets ::zendesk::tickets

tenant {subdomain: "tenant-b", email: "agent@tenant-b.example", api-token: "tenant-b-token"}
ticket ::tickets/get-ticket(35436, tenant)
open ::tickets/list-tickets({page_size: 50}, tenant)
```

With credentials in `opts`, context credentials are ignored for that call. A credential key that is present but blank or null is an error naming the key, never a fallback to context. `::zendesk::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::tickets ::zendesk::tickets
::users ::zendesk::users

requester ::users/create-or-update-user({email: "ada@example.com", name: "Ada Lovelace"})

created ::tickets/create-ticket("Login page down", "Customers report 500s on /login.", {
  priority: "urgent",
  requester_id: requester.user.id,
  tags: ["outage"]
})
id created.ticket.id

::tickets/add-comment(id, "Deployed a rollback; monitoring.", false)
::tickets/update-ticket(id, {status: "pending"})

thread ::tickets/list-all-comments(id, 500)
```

## Functions

**Tickets** (`::zendesk::tickets`)
- `list-tickets(options?)`: one cursor page. `list-all-tickets(limit, options?)` collects several
- `get-ticket(id)`, `show-many-tickets(ids)`, `create-ticket(subject, body, options?)`, `update-ticket(id, changes)`, `delete-ticket(id)`
- `add-comment(id, body, public, options?)`, `list-comments(id, options?)`, `list-all-comments(id, limit)`
- `merge-tickets(target-id, source-ids, {target_comment, source_comment})`: returns a `job_status`
- Tags: `list-ticket-tags`, `add-ticket-tags`, `set-ticket-tags`, `remove-ticket-tags`, `list-tags` (account-wide)
- `list-ticket-fields(options?)`, `list-satisfaction-ratings({score, start_time, end_time})`
- Incremental export: `export-tickets(start-time)`, then `export-tickets-next(after-cursor)` until `end_of_stream`

**Users** (`::zendesk::users`)
- `me()`, `get-user(id)`, `list-users(options?)`, `search-users(query, options?)`, `find-user-by-email(email)` (null when none)
- `create-user(fields)`, `update-user(id, fields)`, `create-or-update-user(fields)`

**Organizations** (`::zendesk::organizations`)
- `list-organizations`, `get-organization`, `create-organization`, `update-organization`, `create-or-update-organization`, `delete-organization`
- `search-organizations({name})` or `search-organizations({external_id})`: exact match

**Views and macros**
- `::zendesk::views`: `list-views`, `list-active-views`, `get-view`, `execute-view(id, options?)`, `list-view-tickets(id, options?)`, `list-all-view-tickets(id, limit)`, `count-view-tickets(id)`
- `::zendesk::macros`: `list-macros`, `list-active-macros`, `get-macro`, `preview-macro(ticket-id, macro-id)` (changes nothing), `apply-macro(ticket-id, macro-id)` (previews, then updates the ticket)

**Search** (`::zendesk::search`)
- `search(query, {sort_by, sort_order, page, per_page})`: offset pages of mixed results
- `count(query)`
- `export(query, object-type, {page_size, after})` and `search-all(query, object-type, limit)`: cursor-based, one type at a time

**Side conversations** (`::zendesk::side-conversations`)
- `list-side-conversations(ticket-id)`, `get-side-conversation`, `create-side-conversation(ticket-id, subject, body, to, options?)`, `reply-to-side-conversation(ticket-id, id, body, options?)`, `update-side-conversation`, `close-side-conversation`

**Escape hatch** (`::zendesk::api`)
- `request(method, path, body)` (pass `""` for no body), `list-page(path, options)`, `next-after(page)`, `collect(path, key, options, limit)`

## Pagination

List endpoints use Zendesk's cursor pagination. Pass `page_size` (sent as `page[size]`, default and maximum 100) and `after` (sent as `page[after]`). Each page carries `meta.has_more` and `meta.after_cursor`, and `::zendesk::api/next-after(page)` returns the next cursor, or null on the last page. Offset paging (`page=N`) stops at the first 100 pages (10,000 records) and returns a 400 past that, so these helpers don't use it. Cursor paging has no depth limit.

```hot
::tickets ::zendesk::tickets

first-page ::tickets/list-tickets({page_size: 50, sort: "-updated_at"})
second-page ::tickets/list-tickets({page_size: 50, sort: "-updated_at", after: ::zendesk::api/next-after(first-page)})

// Or collect up to a limit
recent ::tickets/list-all-tickets(500, {sort: "-updated_at"})
```

Search has its own limits. `search` pages by offset: at most 100 results a page and 1,000 per query, and asking past that returns a 422. Results can repeat between pages. For more than 1,000 results, use `search-all` or `export`. The export endpoint pages by cursor, returns one object type at a time ordered by `created_at`, and its cursor expires after an hour. It is rate-limited to 100 requests a minute.

For syncing, prefer the incremental export over listing:

```hot
::tickets ::zendesk::tickets

batch ::tickets/export-tickets(sub(::hot::time/epoch-seconds(), 3600))
batch.end_of_stream
more ::tickets/export-tickets-next(batch.after_cursor)
```

## Webhooks

Zendesk signs each delivery with `X-Zendesk-Webhook-Signature`, the base64 HMAC-SHA256 of `X-Zendesk-Webhook-Signature-Timestamp` followed by the raw body. `verify-request` checks it over `::hot::http/raw-body` in constant time. It then rejects deliveries whose signed timestamp is more than five minutes from now; pass a third argument to widen the window, or 0 to disable the check. It fails closed: an unset `zendesk.webhook.secret` halts with a message, an empty explicit secret never verifies, and a missing signature or timestamp fails.

```hot
zendesk-webhook
meta {webhook: {service: "zendesk", path: "/zendesk/webhook"}}
fn (request) {
  if(not(::zendesk::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: "ok"}))
}
```

Test deliveries sent from Admin Center before a webhook exists are signed with Zendesk's static test secret, `::zendesk::webhooks/TEST_SIGNING_SECRET`.

## Notes and limits

- Functions return the decoded JSON body, so a ticket is under `.ticket`. Empty 204 responses (deletes) return null.
- Non-2xx responses return an Err carrying `HttpError {status, headers, body, retry-after}`. On a 429, `retry-after` is the number of seconds Zendesk asked you to wait. `body.error` and `body.description` hold Zendesk's error.
- Request bodies omit unset (null) fields instead of sending JSON `null`.
- Some endpoints have their own rate limits. Incremental exports allow 10 requests a minute. `execute-view` allows 5 requests a minute per view per agent, and its results may be up to 10 minutes old.
- `merge-tickets` and other bulk calls return a `job_status`. Poll it with `::zendesk::api/request("GET", "/job_statuses/{id}", "")`.
- The incremental export, satisfaction ratings and organization search require an admin.

## License

Apache-2.0 - see [LICENSE](LICENSE)
