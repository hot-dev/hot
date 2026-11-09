# box

Box Platform API v2.0 bindings for Hot, built for document workflows and agents: list folders with marker paging, search with typed filters, read any document as text through Box's `extracted_text` representation, binary-safe downloads with Range chunking past the response cap, multipart and chunked uploads, shared links and collaborations, versions, the Events API and enterprise admin logs, and webhooks v2 with signature verification.

Auth covers Client Credentials Grant, JWT server auth, OAuth 2.0 with a store-backed keeper for Box's **single-use** refresh tokens, and explicit access tokens, with `As-User` support and a per-run token cache.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/box": "0.1.2"
}
```

## Auth and context variables

Set credentials once in Context Variables, or pass them per call in `opts` for multi-tenant apps.

### Per-call credentials

Per-call credentials replace context for that call; your app's `box.client.id` / `box.client.secret` still fill in. They are never written to context, and they are masked in call records, including the secrets inside a `jwt-config`, as is every token Box returns.

```hot
tenant {account: `tenant:${tenant-id}`}
items ::box::folders/list-all-items("0", tenant, 200)
```

A credential key (`token`, `account`, `refresh-token`, `client-id`, `client-secret`, `jwt-config`, `private-key`) that is present but blank or null is an error naming the key, never a fallback to context. `::box::api/resolve(opts)` is the one place this is decided. The `box.api-root` and `box.upload-root` roots are context-only, and `::box::api/request` refuses any other host, and plain http unless the override is itself http, so a call cannot redirect or downgrade credentials.

### Context variables

| Context Variable | `opts` key | Description |
|---|---|---|
| `box.access.token` | `token` | A ready access token: a developer token (60 minutes) or one you manage |
| `box.client.id`, `box.client.secret` | `client-id`, `client-secret` | Your app's OAuth keys (Developer Console → app → Configuration) |
| `box.enterprise.id` | `enterprise-id` | CCG / JWT subject: act as the app's service account in this enterprise |
| `box.user.id` | `user-id` | CCG / JWT subject: act as this managed or app user |
| `box.jwt.config` | `jwt-config` | The app config JSON the Developer Console downloads (JWT server auth) |
| `box.jwt.private-key` | `private-key` | The **unencrypted** PKCS#8 private key for JWT (see below) |
| `box.oauth.account` | `account` | The keeper account whose OAuth tokens to use |
| `box.oauth.refresh-token` | `refresh-token` | A refresh token: in context it only seeds an empty keeper entry; in `opts` it needs `account` or `on-refresh` |
| `box.oauth.store` | `token-store` | The keeper's `::hot::store` name (default `box-oauth-tokens`) |
| `box.as-user` | `as-user` | Send `As-User: <id>` so an admin or service account acts as a managed user |
| `box.webhook.primary-key`, `box.webhook.secondary-key` | | Webhook signature keys for `verify-request` |
| `box.api-root`, `box.upload-root` | | Optional overrides of `https://api.box.com` / `https://upload.box.com` (proxy or test server) |

Within one source the order is: access token, OAuth (keeper or `on-refresh`), JWT, Client Credentials Grant. Every function has an options arity; Box query parameters use Box's own names (`fields`, `limit`, `marker`, `offset`, `sort`, `direction`, ...).

### Client Credentials Grant (simplest server auth)

Create a Custom App with "Server Authentication (Client Credentials Grant)", have an admin authorize it (Admin Console → Integrations → Platform Apps Manager; free developer accounts are authorized automatically), then set `box.client.id`, `box.client.secret` and `box.enterprise.id` (the service account) or `box.user.id`.

### JWT server auth

Set `box.jwt.config` to the downloaded config JSON. Box encrypts the `privateKey` in that file with a passphrase, and Hot cannot decrypt it, so decrypt it once and put the result in `box.jwt.private-key`:

```bash
jq -r .boxAppSettings.appAuth.privateKey config.json > encrypted.pem
openssl pkcs8 -in encrypted.pem -out box-key.pem   # enter boxAppSettings.appAuth.passphrase
```

`box-key.pem` starts with `BEGIN PRIVATE KEY`. Alternatively generate your own key pair, upload the public key in the Developer Console, and keep the private key unencrypted. Each token request signs a fresh RS256 assertion (`box_sub_type`, a unique `jti`, 45 s expiry) with the config's `publicKeyID` as `kid`. Treat the key like a password: store it as a secret Context Variable and never commit it.

### OAuth 2.0 and single-use refresh tokens

Box access tokens last about 60 minutes. A refresh token lasts 60 days and works **once**: every refresh returns a new pair and kills the old refresh token. Lose the new one and the user has to authorize again. The package offers two ways to keep it.

**The keeper (recommended).** Tokens live in a `::hot::store` Map, one entry per account, as plain text: anyone who can read the store can read them, so restrict who can. Save the pair after the consent callback, then pass `{account}` per call (or set `box.oauth.account`):

```hot
::oauth ::box::oauth

box-connect fn (user-id: Str, state: Str): Str {
  ::oauth/authorize-url(::hot::ctx/get("box.client.id"), "https://app.example.com/oauth/box", state)
}

box-callback fn (user-id: Str, code: Str): Bool {
  // App keys come from opts or box.client.id / box.client.secret
  tokens ::oauth/exchange-code(code, "https://app.example.com/oauth/box", {})
  ::box::auth/save-tokens(`user:${user-id}`, tokens)
}

list-root fn (user-id: Str): Map {
  ::box::folders/list-items("0", {account: `user:${user-id}`})
}
```

A refresh first claims a 30-second lease on the account's entry with `::hot::store/put-if` (compare-and-set), so two concurrent runs never spend the same refresh token, then writes the new pair with `put-if` against the claimed value. The run that loses the race uses the winner's access token, or, while the winner is still refreshing, gets a `TokenError` with `error-code: "refresh_in_progress"` that `::box::api/is-retryable` treats as retryable. A failed refresh releases the lease and leaves the stored pair as it was. `on-refresh` (a Fn) is still called after each refresh, as a notification. `load-tokens` and `forget-tokens` read and delete an entry (pair `forget-tokens` with `::box::oauth/revoke-token` on disconnect).

One case the keeper cannot cover: if Box accepts a refresh but the response is lost (a dropped connection, a run killed mid-call), the old token is spent and the new one never arrived. The account then needs a new consent (`invalid_grant`).

**You persist.** Pass `refresh-token` with `on-refresh`, a Fn that saves the new pair before the run ends (a failing callback fails the call):

```hot
store ::hot::store/Map({name: "my-box-tokens"})

list-for fn (user-id: Str): Map {
  saved ::hot::store/get(store, user-id)
  ::box::folders/list-items("0", {
    refresh-token: saved.refresh_token,
    on-refresh: (tokens) { ::hot::store/put(store, user-id, tokens) }
  })
}
```

Or call `::box::oauth/refresh-tokens(refresh-token, opts)` yourself, save the result first, and pass `{token: tokens.access_token}`. With either of these, guarding against concurrent refreshes is up to you. A `refresh-token` in `opts` with neither `account` nor `on-refresh` is refused, because its replacement would be lost.

### Token cache

CCG, JWT and refreshed tokens are cached in the run's execution context (stored as secrets) until 60 seconds before they expire. A run making 200 calls mints one token; the next run mints again (or, with the keeper, reuses the stored access token while it is fresh). A 401 on a cached token evicts it and retries once with a fresh one.

## Quick start

```hot
::folders ::box::folders
::files ::box::files
::content ::box::content
::up ::box::upload
::search ::box::search

// Browse: every item in a folder (marker paging), as provider-neutral records
items ::folders/list-all-doc-files("0", {fields: "id,type,name,size,sha1,etag,file_version,parent,modified_at"}, 200)

// Search PDFs and Word files under one folder
hits ::search/search("quarterly report", {file_extensions: ["pdf", "docx"], ancestor_folder_ids: ["12345"], limit: 20})

// Read a document as text (extracted_text when ready, else the original bytes)
doc ::content/read-document(first(hits.entries).id, "text")
doc.text
doc.text-state

// Upload into a folder (binary-safe; chunked from 20 MB), then trash it
folder ::folders/ensure-folder("Reports", "0")
report ::up/upload-file("q3.csv", "region,total\nwest,42\n", folder.id)
::files/trash-file(report.id)
```

## Functions by area

**`::box::folders`**: `list-items` (one page; marker paging, or offset paging with `offset`), `list-all-items` (all pages, optional limit; `{paging: "offset"}` for offsets), `list-all-doc-files`, `get-folder`, `get-doc-file`, `create-folder`, `ensure-folder`, `conflict-item`, `update-folder`, `rename-folder`, `move-folder`, `copy-folder`, `trash-folder` (`{recursive: true}` for non-empty folders), `restore-folder`, `get-trashed-folder`.

**`::box::files`**: `get-file` (`fields`, `rep-hints`), `get-doc-file`, `update-file`, `rename-file`, `move-file`, `copy-file`, `trash-file`, `restore-file`, `get-trashed-file`, `list-versions`, `list-all-versions`, `promote-version`.

**`::box::search`**: `search` (one page), `search-all` (offset paging up to Box's 10,000 cap), `search-params`, `md-filter`.

**`::box::content`**: `download-file`, `download-range`, `download-file-chunked`, `download-text`, `extracted-text`, `get-representations`, `is-ready`, `is-done`, `read-document`, `read-document-text`.

**`::box::upload`**: `upload-file`, `upload-version`, `preflight-check`, `preflight-version`, `create-upload-session`, `create-version-session`, `get-upload-session`, `upload-part`, `list-parts`, `list-all-parts`, `commit-upload-session`, `abort-upload-session`, `upload-chunked`, `upload-to-session`, `finish-upload`, `build-multipart`, `digest-header`, `sha1-hex`.

**`::box::sharing`**: `set-shared-link`, `get-shared-link`, `remove-shared-link`, `get-shared-item`, `boxapi-header`, `add-collaboration`, `list-collaborations`, `list-all-collaborations`, `update-collaboration`, `remove-collaboration`, `user-by-email`, `user-by-id`, `group-by-id`.

**`::box::events`**: `get-events`, `current-position`, `sync-events`, `dedupe-events`, `event-ids`, `admin-events`, `admin-events-all`, `long-poll-url`, `long-poll`.

**`::box::webhooks`**: `create-webhook`, `list-webhooks`, `list-all-webhooks`, `get-webhook`, `update-webhook`, `delete-webhook`, `verify-request`, `verification-failure`, `parse-delivery`.

**`::box::auth`** / **`::box::oauth`**: `access-token`, `save-tokens`, `load-tokens`, `forget-tokens`; `authorize-url`, `exchange-code`, `refresh-tokens`, `revoke-token`.

**`::box::api`**: `request`, `json`, `resolve`, `HttpError` (`HttpError(response)` builds it from a non-2xx response), `is-retryable`, `has-code`, `collect-marker`, `collect-offset`, `with-query`.

The most used functions are also aliased at the top level (`::box/list-items`, `list-all-items`, `get-folder`, `create-folder`, `get-file`, `search`, `download-file`, `read-document`, `upload-file`).

## Results

Endpoint functions return Box's JSON as Maps. Box returns only a mini representation of an item unless you name `fields`; the package asks for a useful default set (`::box/FILE_FIELDS`, `FOLDER_FIELDS`, `ITEM_FIELDS`). Use `::box/to-doc-file` for a provider-neutral record with the same field names as `google-drive`'s `DocFile`, plus `sha1`:

| `DocFile` field | From Box |
|---|---|
| `provider` | `"box"` |
| `id`, `name` | `id`, `name` |
| `mime-type` | from the name's extension (`application/octet-stream` when unknown); `inode/directory` for folders, `application/vnd.box.web-link` for web links |
| `size` | `size` as an Int (for folders, the size of the contents) |
| `modified`, `created` | `modified_at`, `created_at` |
| `parents` | `[parent.id]` (empty for the root) |
| `drive-id` | null (Box has no shared drives) |
| `web-url` | `https://app.box.com/file/{id}` (or `/folder/{id}`) |
| `version`, `revision-id` | `etag` (send as `if-match`), `file_version.id` |
| `md5`, `sha256` | null (Box hashes with SHA-1) |
| `sha1` | `sha1` (hex) |
| `is-folder` | `type == "folder"` |
| `is-google-native`, `is-shortcut`, `shortcut-target-id` | false, false, null |
| `trashed` | `trashed_at` set or `item_status` not active |
| `export-formats` | empty (see extracted text below) |
| `raw` | the Box item |

Content functions return a `FileContent`: `bytes` (the exact response bytes), `mime-type`, `size`, `text` (set for textual content), `source` (`download`, `range`, `chunked`, `extracted-text`), and from `read-document` also `name`, `format`, `file` (a `DocFile`) and `text-state`.

## Extracted text

Box renders documents (PDF, Office, Google, iWork and more) as plain text on request: the `extracted_text` representation. Generation is asynchronous and the package never waits for it. `extracted-text` returns a status with `state` `"success"` (with `text`), `"pending"`, `"none"` (generation was just requested), `"unavailable"` (no text rendition for this file type) or `"error"`. Poll again from a schedule or a later step until `is-done`:

```hot
::content ::box::content
::store ::hot::store

pending ::store/Map({name: "box-text-pending"})
kb ::store/Map({name: "box-kb"})

box-text-poll
meta {schedule: "every 5 minutes"}
fn (event) {
  map(::store/keys(pending), (file-id) {
    status ::content/extracted-text(file-id)
    cond {
      is-err(status) => { null }
      ::content/is-ready(status) => {
        ::store/put(kb, file-id, {content: status.text})
        ::store/delete(pending, file-id)
      }
      ::content/is-done(status) => { ::store/delete(pending, file-id) }
      => { null }
    }
  })
}
```

`read-document(id, "text")` does it in one call: textual files (txt, md, csv, json, ...) are downloaded as is; other documents use the extracted text when it is ready and otherwise request it and download the original (pass `{download-fallback: false}` to skip that download). `read-document-text` returns just the text, or an Err saying whether it is still being generated. `"original"` downloads the stored bytes.

## Large files

- **Downloads** follow Box's redirect to `dl.boxcloud.com` (the Authorization header is not forwarded across hosts; the redirect URL carries its own grant). Files larger than one response (`http.max-response-bytes`, 16 MiB by default) switch to Range requests automatically; `download-file-chunked` does it explicitly and `download-range` reads a slice. The whole file is still held in memory. A just-uploaded file can answer 202 with `Retry-After`; that comes back as a retryable `HttpError`.
- **Uploads** below 20 MB go in one `multipart/form-data` request (attributes first, then the file, built as Bytes, with the SHA-1 in `Content-MD5` so Box rejects a corrupted transfer). From 20 MB (or with `{chunked: true}`) `upload-file` and `upload-version` use an upload session: Box picks the part size, each part goes up with `Digest: sha=<base64 SHA-1>` and `Content-Range`, and the commit sends every part plus the whole file's SHA-1. A failed part aborts the session (unless `keep-session: true`). If Box answers the commit with 202 while it assembles the file, `upload-chunked` returns `state: "processing"` and `finish-upload(session-id, content)` commits again later; `upload-file` returns an Err that names the session. Sessions expire after 7 days.
- Simple uploads are limited to 50 MB; the largest file Box accepts depends on the plan (250 MB on the free developer plan, up to 150 GB on Enterprise).

## Events and sync

The user event stream records every change the user can see. Store a position once, then read everything since it on a schedule:

```hot
::events ::box::events
::store ::hot::store

state ::store/Map({name: "box-sync"})

box-sync
meta {schedule: "every 10 minutes"}
fn (event) {
  position or(::store/get(state, "position"), ::events/current-position())
  set ::events/sync-events(position, {stream_type: "changes", seen: or(::store/get(state, "seen"), [])})
  map(set.events, (e) { println(`${e.event_type} ${e.source.id}`) })
  ::store/put(state, "seen", ::events/event-ids(set.events))
  ::store/put(state, "position", set.next-stream-position)
}
```

Box may deliver an event twice or out of order: `sync-events` drops repeated `event_id`s within a read and any ids in `seen`, and handlers should be idempotent. `sync-events` reads at most 20 chunks of 500 per call (`max-pages`); `complete: false` means there is more.

**Admin logs** (`admin-events`, `admin-events-all`) read the enterprise stream: `stream_type: "admin_logs"` with `created_after` / `created_before` and `event_type` filters, or `"admin_logs_streaming"` for the low-latency last two weeks. They need an enterprise admin token with "Manage enterprise properties", which the free developer plan does not have.

**Long-poll.** `long-poll(url, position)` waits on a Box realtime server until it answers `new_change` or `reconnect`. It is a blocking HTTP call that holds the run for up to Hot's 120-second request timeout, then returns `{message: "timeout"}`. `long-poll-url` gets the realtime URL from `OPTIONS /events`. A schedule calling `sync-events`, or a webhook that triggers it, is the better fit for Hot.

## Webhooks

Create a webhook on a file or folder (folder webhooks cover everything inside):

```hot
hook ::box::webhooks/create-webhook("folder", "12345", "https://example.hot.dev/box/webhook", ["FILE.UPLOADED", "FILE.TRASHED", "FILE.RESTORED"])
```

Verify every delivery before trusting it. Box signs the raw body followed by the `BOX-DELIVERY-TIMESTAMP` value with HMAC-SHA256 under both of the app's signature keys (Developer Console → app → Webhooks → Manage signature keys) and sends `BOX-SIGNATURE-PRIMARY` and `BOX-SIGNATURE-SECONDARY`. `verify-request` accepts a delivery when either signature matches its own key (so you can rotate one key at a time), the signature version is `1` and the algorithm `HmacSHA256`, and the timestamp is at most 10 minutes old (and at most a minute in the future). Comparisons are constant time, and a missing key, header or timestamp fails closed.

```hot
box-webhook
meta {webhook: {service: "box", path: "/box/webhook"}}
fn (request) {
  cond {
    not(::box::webhooks/verify-request(request)) => { ::hot::http/HttpResponse({status: 401, body: "invalid signature"}) }
    => {
      delivery ::box::webhooks/parse-delivery(request)
      send("box:changed", {trigger: delivery.trigger, item: delivery.source.id})
      ::hot::http/HttpResponse({status: 200, body: ""})
    }
  }
}
```

The 1-arity `verify-request` reads `box.webhook.primary-key` / `box.webhook.secondary-key` and fails the run when neither is set; pass keys explicitly with `verify-request(request, primary, secondary)`. `verification-failure` returns the reason (`bad-signature`, `stale`, `missing-signature`, ...) for logging. Box retries a delivery that does not get a 2xx within 30 seconds, so answer quickly and do the work in an event handler.

## Errors and pagination

Non-2xx responses come back as `err(::box::api/HttpError)` with a readable `message` built from Box's error body, e.g. `box: HTTP 409 item_name_in_use: Item with the same name already exists (request_id 9f1c2a)`, plus `code`, `error-message`, `request-id`, `help-url`, `context-info` (such as the conflicting item, which `::box::folders/conflict-item` reads) and `retry-after`. Credential failures are `::box::auth/TokenError` with Box's OAuth `error` in `error-code`.

Nothing waits inside a run. A connection failure on an idempotent call (GET, PUT, DELETE) is retried once, immediately; 429 and 5xx come back with `retry-after` set. Check `::box::api/is-retryable(result)` and fail the run so a handler, schedule or task `retry` policy reschedules it (see [retries](https://hot.dev/docs/retries)):

```hot
box-index
meta {
  schedule: "every 15 minutes",
  retry: {attempts: 5, delay: 2000, backoff: "exponential", max_delay: 60000, jitter: true}
}
fn (event) {
  page ::box::folders/list-items("0")
  cond {
    ::box::api/is-retryable(page) => { fail(`box: retrying later: ${err-message(page)}`) }
    is-err(page) => { println(`box: giving up: ${err-message(page)}`) }
    => { length(page.entries) }
  }
}
```

Folder items, collaborations and webhooks use **marker** paging (`next_marker`); search, versions and upload parts use **offset** paging (`offset`, `total_count`). The `list-all-*` and `search-all` helpers follow either for you and take an optional limit; `::box::api/collect-marker` and `collect-offset` work with any list call.

## Notes and limits

- **Rate limits:** 1,000 API calls per minute per user; 240 uploads per minute per user; search 6 per second and 60 per minute per user, 12 per second per enterprise. Enterprises also have a monthly API allocation set by their plan.
- **Search** indexes new content after a few minutes, and offset paging stops at 10,000 results. `scope: "enterprise_content"` must be enabled by Box support.
- **Preflight:** `preflight-check` and `preflight-version` send `OPTIONS` and return `{upload_url, upload_token}`, or the 409 `item_name_in_use` / 403 `storage_limit_exceeded` an upload would hit, before any bytes move.
- **Shared links:** removing one sends `{"shared_link": null}`, the only JSON null this package sends (Box's documented way to remove a link). `company` access needs a paid plan, and enterprises may restrict shared links and external collaboration (403).
- **Trash:** `trash-file` and `trash-folder` move items to the trash, recoverable until the enterprise's retention period (30 days by default). Permanent deletion is not wrapped.
- **Plans:** version history, admin logs, `enterprise_content` search, Box Shield and similar features need a paid Business or Enterprise plan. Box documents a [free developer plan](https://developer.box.com/platform/free-developer-plan) (10 GB, 250 MB uploads, one file version, OAuth, JWT and CCG, with a pre-authorized default CCG app), but as of November 2026 its signup link redirects to Box's pricing page, so new signups may not be open; a free Individual account with a Developer Console app, or a Business trial, also works for testing.
- **Data handling:** tokens, refresh tokens, client secrets and private keys are never logged or put into error messages. Per-call credentials and every token Box returns are marked with `::hot::ctx/mark-secret`, so call records mask them, and the token cache stores them as secrets. The keeper stores live tokens in `::hot::store`; restrict who can read that store. Error messages include Box's `request_id` but not `context_info`, which can contain item names.

## Integration tests

`hot test -p pkg-integration-box` runs three read-only smoke tests (current user, folder listing, folder info and event position) and skips cleanly without credentials. Set `BOX_ACCESS_TOKEN` (a developer token), or `BOX_CLIENT_ID` + `BOX_CLIENT_SECRET` with `BOX_ENTERPRISE_ID` or `BOX_USER_ID`, and optionally `BOX_TEST_FOLDER_ID`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
