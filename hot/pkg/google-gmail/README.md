# google-gmail

Gmail API v1 bindings for Hot, built for email workflows and agents: send safe MIME email (HTML, attachments, non-ASCII subjects and names, messages up to 35 MB), reply and forward in thread, search and read messages as flat `Email` records, threads, drafts for human review, labels, and incremental sync with Pub/Sub push notifications. Credentials, scopes and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-gmail": "0.1.0"
}
```

## Auth and scopes

Set credentials once in Context Variables (any `google-core` flow), or pass them per call in `opts` for multi-tenant apps:

| Context Variable | Description |
|---|---|
| `google.access.token` | A ready OAuth access token |
| `google.oauth.refresh-token` + `google.oauth.client-id` + `google.oauth.client-secret` | A user's refresh token (see the [consent flow](../google-core/README.md#consent-flow)) |
| `google.service.account` + `google.impersonate` | Service-account key JSON with domain-wide delegation, impersonating a Workspace user |
| `google.scopes` | Scopes for service-account tokens, overriding the defaults below |
| `google.gmail.push.token` | Shared secret checked by `::google::gmail::sync/verify-push` |
| `google.gmail.api-root` | Optional API root override for a proxy or emulator (default `https://gmail.googleapis.com`) |

**Service accounts cannot use Gmail on their own**: they have no mailbox. Use domain-wide delegation (a Workspace admin authorises your service account's client ID for the Gmail scopes) and impersonate a user with `google.impersonate`, or `subject` per call. See [domain-wide delegation setup](../google-core/README.md#domain-wide-delegation-setup).

Each function documents the scope it asks for. The defaults:

| Scope | Classification | Used for |
|---|---|---|
| `gmail.send` | Sensitive | `send`, `send-message`, `send-raw` (and the sending half of `reply`, `reply-all`, `forward`) |
| `gmail.readonly` | Restricted | Listing, search, `get-message`, threads, drafts listing, attachments, labels listing, profile, history, `watch` |
| `gmail.modify` | Restricted | Label changes, `batch-modify`, trash and untrash (messages and threads) |
| `gmail.compose` | Restricted | Creating, updating, sending and deleting drafts |
| `gmail.labels` | Sensitive | Creating, updating and deleting labels |

`reply`, `reply-all` and `forward` read the original first, so they need `gmail.readonly` as well as `gmail.send`. With access and refresh tokens the user's granted scopes apply; the defaults matter for service accounts, where the scopes requested must match what the admin authorised (set `google.scopes` to, say, `"gmail.modify"` if that is the one scope you authorised). Permanent deletion needs the full `https://mail.google.com/` scope and is deliberately not wrapped: use the trash.

Every function has an options arity. Gmail query parameters use Gmail's own names (`q`, `maxResults`, `pageToken`, `labelIds`, `format`, ...); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`.

### Per-call credentials

Credentials in `opts` override context for that call only, so one app can act for many users; see the [google-core README](../google-core/README.md#per-call-credentials-multi-tenant) for every key.

```hot
::messages ::google::gmail::messages

inbox ::messages/list-messages({q: "is:unread", token: "ya29.tenant-access-token"})
```

Per-call credentials are masked in call records and never written to context. A credential key that is present but blank or null is an error naming the key, never a fallback to context.

## Quick start

```hot
::messages ::google::gmail::messages

// Send: HTML with a plain-text alternative and an attachment
sent ::messages/send({
  to: ["Ada Lovelace <ada@example.com>"],
  subject: "Q3 report",
  text: "Hi Ada, the Q3 report is attached.",
  html: "<p>Hi Ada, the <b>Q3 report</b> is attached.</p>",
  attachments: [{filename: "q3.csv", content: "region,total\nwest,42\n", mime-type: "text/csv"}]
})

// Search and read as flat records
unread ::messages/search-emails("is:unread newer_than:2d", 10)
email first(unread)
summary {from: email.from-email, subject: email.subject, body: email.text}

// Reply in thread, then mark read and archive
::messages/reply(email.id, {text: "Thanks, on it."})
::messages/mark-read(email.id)
::messages/archive-message(email.id)
```

## Functions by area

**`::google::gmail::messages`**: `send` (a message Map), `send-message` (to, subject, text), `send-raw` (your own RFC 5322 bytes), `reply`, `reply-all`, `forward`, `list-messages` (one page), `list-all-messages` (all pages, optional limit), `search-emails` (search returning `Email`s), `get-message`, `get-email`, `modify-message`, `batch-modify` (any number of ids, 1000 per call), `mark-read`, `mark-unread`, `archive-message`, `trash-message`, `untrash-message`, `get-attachment`, `attachment-bytes`, plus the pure `reply-fields`, `reply-subject`, `forward-subject`, `label-changes`.

**`::google::gmail::mime`** (pure): `build-message`, `build-headers`, `build-body`, `text-part`, `attachment-part`, `multipart`, `build-related`, `to-raw`, `parse-addresses`, `format-addresses`, `emails`, `is-email`, `encode-words`, `encode-header-text`, `encode-phrase`, `clean-header-value`.

**`::google::gmail::parse`** (pure): `to-email`, `headers`, `header`, `header-values`, `text-body`, `html-body`, `charset`, `attachments`, `all-parts`, `walk-parts`, `decode-data`, `is-attachment`.

**`::google::gmail::threads`**: `list-threads`, `list-all-threads`, `get-thread`, `thread-emails`, `modify-thread`, `trash-thread`, `untrash-thread`.

**`::google::gmail::drafts`**: `create-draft`, `create-reply-draft`, `update-draft`, `send-draft`, `list-drafts`, `list-all-drafts`, `get-draft`, `delete-draft`.

**`::google::gmail::labels`**: `list-labels`, `get-label`, `find-label`, `create-label`, `ensure-label`, `update-label`, `delete-label`, `build-label`.

**`::google::gmail::sync`**: `get-profile`, `list-history`, `sync-history`, `is-history-expired`, `summarize-history`, `watch`, `stop-watch`, `build-watch`, `parse-push`, `verify-push-token`, `verify-push`.

The most used functions are also aliased at the top level (`::google::gmail/send`, `send-message`, `reply`, `reply-all`, `forward`, `list-messages`, `list-all-messages`, `search-emails`, `get-message`, `get-email`, `modify-message`, `trash-message`, `to-email`, `list-labels`, `get-profile`).

## Composing messages

`send`, `create-draft` and `::google::gmail::mime/build-message` take one message Map:

| Field | Value |
|---|---|
| `to`, `cc`, `bcc`, `from`, `reply-to` | `"ada@example.com"`, `"Ada <ada@example.com>, bob@example.com"`, a Vec of those, or `{name, email}` Maps |
| `subject` | Str (non-ASCII is RFC 2047 encoded) |
| `text`, `html` | Bodies; both together become `multipart/alternative` |
| `attachments` | Vec of `{filename, content, mime-type?, content-id?, inline?}`; `content` is Bytes or a Str, the type is guessed from the filename when unset |
| `in-reply-to`, `references` | Message-ID Str or Vec (set for you by `reply`) |
| `headers` | Extra headers, e.g. `{List-Unsubscribe: "<mailto:unsub@example.com>"}` |

Inputs from users or LLMs are safe to pass straight through:

- CR, LF and other control characters in the subject and header values are folded to a space, so `"Hi\r\nBcc: attacker@evil.com"` cannot add a header.
- Addresses containing line breaks, and malformed addresses, are rejected with an Err before anything is sent; so are bad header names and attempts to set `Content-Type`, `MIME-Version` or the address headers through `headers`.
- Non-ASCII subjects and display names are RFC 2047 encoded; non-ASCII attachment names use RFC 2231 `filename*`.
- ASCII text with short lines is sent `7bit`; everything else, and every attachment, is base64 in 76-character lines. The message is assembled as Bytes, so binary attachments arrive intact.

Gmail adds `Date` and `Message-ID`, sets `From` to the authenticated user unless you name one of their send-as aliases, and removes `Bcc` from delivered copies. Messages over 5 MiB go through the media-upload endpoint (`uploadType=multipart`) automatically, up to Gmail's 35 MB limit.

```hot
::messages ::google::gmail::messages
::drafts ::google::gmail::drafts

// Inline image referenced from the HTML
logo Bytes([137, 80, 78, 71])
::messages/send({
  to: "team@example.com",
  subject: "Launch day",
  html: "<p><img src=\"cid:logo\"> We are live.</p>",
  attachments: [{filename: "logo.png", content: logo, content-id: "logo"}]
})

// Let a person review an agent's answer before it goes out
draft ::drafts/create-reply-draft("18c2f0a1b2c3d4e5", {text: "Here is the summary you asked for..."})
```

## Reading messages

`get-message` returns Gmail's resource with base64url part data; `::google::gmail::parse` reads it, and `to-email` flattens it into an `Email`:

| `Email` field | From |
|---|---|
| `id`, `thread-id`, `history-id`, `label-ids`, `snippet` | the message resource |
| `message-id`, `from`, `to`, `cc`, `reply-to`, `subject`, `date` | headers |
| `from-email`, `to-emails`, `cc-emails` | addresses, lowercased |
| `internal-date` | when Gmail received it (epoch ms) |
| `unread`, `starred`, `in-inbox` | labels |
| `text`, `html` | the first plain-text and HTML bodies, decoded |
| `attachments` | `Attachment`s: `filename`, `mime-type`, `size`, `attachment-id`, `data` (when inline), `content-id`, `inline` |
| `raw` | the message resource |

```hot
::messages ::google::gmail::messages

email ::messages/get-email("18c2f0a1b2c3d4e5")
pdfs filter(email.attachments, (a) { eq(a.mime-type, "application/pdf") })
files map(pdfs, (a) { {name: a.filename, bytes: ::messages/attachment-bytes(a)} })
```

Bodies are decoded as UTF-8. Gmail removes the transfer encoding but not the charset, so a body in another charset (check `::google::gmail::parse/charset`) may contain replacement characters.

## Recipe: inbox triage agent

Label, star and archive messages matching a search, in one pass:

```hot
::messages ::google::gmail::messages
::labels ::google::gmail::labels

triage-invoices
meta {schedule: "every 30 minutes"}
fn (event) {
  label ::labels/ensure-label("Finance/Invoices")
  ids map(::messages/list-all-messages("in:inbox has:attachment subject:invoice", 500), (m) { m.id })
  cond {
    is-empty(ids) => { 0 }
    => {
      ::messages/batch-modify(ids, [label.id], ["INBOX"])
      length(ids)
    }
  }
}
```

## Recipe: push notifications and incremental sync

Gmail publishes mailbox changes to a Google Cloud Pub/Sub topic, which pushes them to your webhook. One-time setup:

1. Create a topic, e.g. `projects/my-project/topics/gmail`.
2. Grant `gmail-api-push@system.gserviceaccount.com` the **Pub/Sub Publisher** role on the topic.
3. Create a push subscription whose endpoint is your Hot webhook URL with `?token=<secret>` appended, and set the same secret as `google.gmail.push.token`.
4. Call `watch` for the mailbox, and renew it at least every 7 days (Google recommends daily).

```hot
::sync ::google::gmail::sync
::messages ::google::gmail::messages
::store ::hot::store

state ::store/Map({name: "gmail-sync"})

renew-gmail-watch
meta {schedule: "0 3 * * *"}
fn (event) {
  w ::sync/watch("projects/my-project/topics/gmail", {label-ids: ["INBOX"], label-filter-behavior: "include"})
  if(is-null(::store/get(state, "history-id")), ::store/put(state, "history-id", w.historyId), null)
}

gmail-push
meta {webhook: {service: "gmail", path: "/push"}}
fn (request) {
  note ::sync/verify-push(request)
  cond {
    is-err(note) => { ::hot::http/HttpResponse({status: 401, body: "bad token"}) }
    => {
      send("gmail:changed", {email: note.email-address})
      ::hot::http/HttpResponse({status: 204})
    }
  }
}

on-gmail-changed
meta {on-event: "gmail:changed"}
fn (event) {
  set ::sync/sync-history(::store/get(state, "history-id"))
  cond {
    ::sync/is-history-expired(set) => { ::store/put(state, "history-id", ::sync/get-profile().historyId) }
    => {
      map(set.added-message-ids, (id) { send("gmail:message", {id: id}) })
      ::store/put(state, "history-id", set.history-id)
    }
  }
}
```

Pushes only say that the mailbox changed: always sync from your stored history id. They can arrive late, twice or out of order, and Pub/Sub retries any non-2xx answer, so acknowledge quickly and do the work in an event handler. `verify-push` compares the `token` query parameter in constant time and fails closed when it or `google.gmail.push.token` is missing. When the stored id is too old (Gmail answers 404 after about a week), `is-history-expired` is true: do a full sync with `list-all-messages` and store a fresh `historyId`.

## Notes and limits

- `search-emails` makes one `get-message` call per result; keep `limit` modest, or use `list-all-messages` for ids only.
- `list-messages` returns only `{id, threadId}` per message; Gmail's search syntax (`q`) is the same as the Gmail search box.
- `maxResults` is at most 500 for messages, threads, drafts and history.
- `trash-message` is recoverable for 30 days; drafts are deleted immediately by `delete-draft`.
- Gmail meters each user in quota units (a send costs far more than a read). On 429 or 5xx, `::google::api/is-retryable(result)` is true: let a handler `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)). Never retry a send blindly after a connection failure, since the first attempt may have gone through.
- One response is capped by `http.max-response-bytes` (16 MiB by default), which bounds `format: "raw"` reads and attachment downloads.

## License

Apache-2.0 - see [LICENSE](LICENSE)
