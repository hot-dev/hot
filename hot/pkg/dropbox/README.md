# dropbox

Dropbox API v2 bindings for Hot, built for document workflows and agents: list and search files, read any file as text (Paper docs export to Markdown), binary-safe downloads with Range chunking past the response cap, uploads with chunked upload sessions and `content_hash` checks, folders, moves, copies, deletes, revisions, batch jobs, shared links, cursor sync with longpoll, and signed webhooks. OAuth with PKCE and offline refresh tokens is built in, with a per-run token cache and team (member and team-space) headers.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/dropbox": "0.1.0"
}
```

## Auth and ctx variables

Create an app in the [Dropbox App Console](https://www.dropbox.com/developers/apps) (scoped access; "App folder" or "Full Dropbox"), enable the scopes you need, and set credentials once in Context Variables, or pass them per call in `opts` for multi-tenant apps:

| Context Variable | `opts` key | Description |
|---|---|---|
| `dropbox.access.token` | `token` | A ready access token. Dropbox access tokens are short-lived (about 4 hours), so this suits tests and one-off runs |
| `dropbox.refresh.token` | `refresh-token` | An offline refresh token (from the [consent flow](#consent-flow)); access tokens are minted from it and cached for the run |
| `dropbox.app.key` | `app-key` | The app key (needed with a refresh token) |
| `dropbox.app.secret` | `app-secret` | The app secret: needed to refresh tokens of confidential apps (not for PKCE-issued tokens) and to verify webhooks |
| `dropbox.select.user` | `select-user` | Team apps: a `team_member_id` to act as (`Dropbox-API-Select-User`) |
| `dropbox.select.admin` | `select-admin` | Team apps: an admin's `team_member_id` for team-owned content (`Dropbox-API-Select-Admin`) |
| `dropbox.path.root` | `path-root` | `Dropbox-API-Path-Root`: a root namespace id for the team space, `ns:<id>` for one namespace, or `home` |
| `dropbox.api.root` | | Optional: send every route to a proxy or local mock (`{root}/2/{route}`) |

Per-call `opts` win over context: a `token` or `refresh-token` in `opts` replaces the context credentials for that call (a per-call refresh token without `app-key` borrows the context app key and secret). Every function has an options arity; Dropbox arguments use Dropbox's own snake_case names (`recursive`, `limit`, `include_deleted`, `autorename`, ...).

| Scope | Enough for |
|---|---|
| `account_info.read` | `::dropbox::users` |
| `files.metadata.read` | Listing, metadata, search, cursors, longpoll |
| `files.content.read` | Download, export, `read-document`, temporary links |
| `files.content.write` | Upload, create folder, move, copy, delete, restore, batches |
| `sharing.read` / `sharing.write` | Listing / creating and revoking shared links |

```hot
::files ::dropbox::files

// As one tenant's user, without touching context
page ::files/list-folder("/Inbox", {
  refresh-token: "stored-refresh-token",
  app-key: "abc123appkey",
  app-secret: "app-secret"
})
```

### Consent flow

```hot
::oauth ::dropbox::oauth

verifier ::oauth/pkce-verifier()
url ::oauth/authorize-url("abc123appkey", "https://app.example.com/oauth/dropbox", "csrf-123", {
  code-verifier: verifier,
  scope: ["account_info.read", "files.metadata.read", "files.content.read"]
})

// In the callback, with the same verifier and redirect URI:
tokens ::oauth/exchange-code("abc123appkey", "app-secret", "code-from-query",
  "https://app.example.com/oauth/dropbox", {code-verifier: verifier})
tokens.refresh_token   // store per user
tokens.account_id
```

`authorize-url` sends `token_access_type=offline` by default so Dropbox returns a refresh token; pass `token-access-type: "online"` for an access token only. Other options: `include-granted-scopes`, `force-reapprove`, `force-reauthentication`, `disable-signup`, `require-role`, `locale`, `extra`. A null redirect URI is the no-redirect flow (Dropbox shows the code to the user). `refresh-access-token` mints a fresh token without the cache, and `revoke-token(access-token)` disconnects the user: Dropbox also disables the refresh token behind it.

### Team apps

Team (Business) apps get a team token. Pick the member to act as with `select-user` (or an admin with `select-admin`). Members of teams using the team space only see team folders when calls carry their root namespace:

```hot
::users ::dropbox::users
::files ::dropbox::files

as-jane {select-user: "dbmid:AAHhy7WsR0x-u4ZCqiDl5Fz5zvuL3kmspwU"}
root ::users/team-path-root(::users/get-current-account(as-jane))
team-folders ::files/list-folder("", {...as-jane, path-root: root})
```

## Quick start

```hot
::files ::dropbox::files
::content ::dropbox::content
::up ::dropbox::upload

// List a folder (all pages) and search
entries ::files/list-all("/Reports", {recursive: true}, 500)
hits ::files/search-all("quarterly plan", {path: "/Reports", file_extensions: ["pdf", "paper"]}, 20)

// Read any file: Paper docs as Markdown, text files as text, others as bytes
doc ::content/read-document("/Reports/Q3 plan.paper", "markdown")
doc.text
doc.file.version

// Upload (binary-safe; a session above 32 MiB) and tidy up
saved ::up/upload("/Reports/q3.csv", "region,total\nwest,42\n", {mode: "overwrite"})
::files/move-path("/Reports/q3.csv", "/Archive/q3.csv")
::files/delete-path("/Archive/q3.csv")
```

## Functions by area

**`::dropbox::files`**: `list-folder` (one page), `list-folder-continue`, `list-all` (all pages, optional limit), `list-files` (files as `DocFile`s), `get-metadata`, `get-doc-file`, `exists`, `search` / `search-continue` (`search_v2`), `search-all`, `match-metadata`, `create-folder`, `ensure-folder`, `move-path`, `copy-path`, `delete-path`, `restore-file`, `list-revisions`, and the batch jobs `delete-batch`, `copy-batch`, `move-batch`, `create-folder-batch` with `check-delete-batch`, `check-copy-batch`, `check-move-batch`, `check-create-folder-batch`, `is-done`, `entry-metadata`, `batch-failures`.

**`::dropbox::content`**: `read-document`, `read-document-text`, `download`, `download-text`, `download-range`, `download-chunked`, `export`, `export-format`, `get-temporary-link`, `content-hash`, `matches-content-hash`.

**`::dropbox::upload`**: `upload` (picks single or session), `upload-single`, `upload-session`, `start-upload-session`, `append-upload-session`, `finish-upload-session`, `resume-upload`, `write-mode`, `commit-info`.

**`::dropbox::sharing`**: `create-shared-link`, `get-or-create-shared-link`, `list-shared-links`, `list-all-shared-links`, `revoke-shared-link`, `get-shared-link-metadata`, `link-settings`.

**`::dropbox::sync`**: `get-latest-cursor`, `start-sync`, `sync-changes`, `changed-files`, `deleted-paths`, `is-under`, `longpoll`.

**`::dropbox::webhooks`**: `challenge-response`, `verify-signature`, `verify-notification`, `parse-notification`.

**`::dropbox::users`**: `get-current-account`, `get-space-usage`, `team-path-root`.

**`::dropbox::api`**: `rpc`, `request`, `upload-call`, `notify`, `HttpError`, `http-error`, `is-retryable`, `has-error-tag`, `is-not-found`, `is-reset`, `is-transport-limit`, `error-node`, `collect-cursor`, `ascii-escape`, `header-json`, `path-root-team`, `path-root-namespace`, `path-root-home`.

**`::dropbox::oauth`** / **`::dropbox::auth`**: `authorize-url`, `exchange-code`, `refresh-access-token`, `revoke-token`, `pkce-verifier`, `granted-scopes`, `missing-scopes`; `access-token`, `resolve-credentials`.

The most used functions are also aliased at the top level: `::dropbox/list-folder`, `list-all`, `get-metadata`, `search`, `create-folder`, `delete-path`, `download`, `read-document`, `upload`, `sync-changes`.

Paths are Dropbox paths (`/Reports/Q3.pdf`, case-insensitive), ids (`id:...`) or `rev:...`; the root is `""`, and `"/"` or `"Reports/"` are normalised for you. Non-ASCII paths work everywhere: content routes send arguments in the `Dropbox-API-Arg` header with every character from U+007F up escaped as `\uXXXX`, as Dropbox requires.

## Results

List and metadata functions return Dropbox's entries as Maps (`.tag` is `file`, `folder` or `deleted`). `::dropbox/to-doc-file` gives the provider-neutral record, with the same field names as `google-drive`'s `DocFile` plus `path` and `content-hash`:

| `DocFile` field | From Dropbox |
|---|---|
| `provider` | `"dropbox"` |
| `id`, `name` | `id` (the lowercased path for deleted entries), `name` |
| `mime-type` | from the file extension (`inode/directory` for folders, `application/vnd.dropbox.paper` for Paper docs) |
| `path` | `path_display` |
| `size`, `modified` | `size`, `server_modified` |
| `created` | always null (Dropbox does not record it) |
| `parents` | the parent folder's display path |
| `drive-id` | the shared folder (namespace) id, when inside one |
| `web-url` | `preview_url` when present |
| `version`, `revision-id` | `rev` (use as an etag) |
| `content-hash` | `content_hash`; `md5` and `sha256` are always null |
| `is-folder`, `is-google-native`, `is-shortcut`, `shortcut-target-id`, `trashed` | derived (`is-google-native` marks export-only files such as Paper docs; `is-shortcut` a symlink; `trashed` a deleted entry) |
| `export-formats` | `export_info.export_options` |
| `raw` | the Dropbox entry |

Content functions return a `FileContent` (the same shape as `google-drive`'s): `bytes` (the exact response bytes), `mime-type`, `size`, `text` (set when the type is textual), `name`, `source` (`download`, `export`, `range`, `chunked`), `format` (the export format) and `file` (a `DocFile`, from the `Dropbox-API-Result` header).

## Paper docs and export

`read-document(path, format)` exports files that cannot be downloaded directly (`is_downloadable: false`: Paper docs, and cloud docs such as Google Docs stored in Dropbox) and downloads everything else as stored. `format` defaults to `"auto"`: Markdown when the file offers it, else the file's own default export (`export_info.export_as`). Names are checked against the file's `export_info.export_options`; `md`, `text`/`txt` (for `plain_text`) and `htm` are accepted as aliases. Paper docs offer `markdown` and `html`. Folders return an Err.

```hot
::content ::dropbox::content

roadmap ::content/read-document("/Notes/Roadmap.paper")
roadmap.text          // Markdown
html ::content/export("/Notes/Roadmap.paper", "html")
pdf ::content/read-document("/Reports/Q3.pdf")
pdf.bytes             // exact bytes; pdf.text is null
```

`content-hash(bytes)` computes Dropbox's `content_hash` (SHA-256 over the SHA-256s of 4 MiB blocks), so you can skip re-downloading files you already have, or check a download with `matches-content-hash(bytes, entry.content_hash)`.

## Recipe: RAG ingestion

```hot
::files ::dropbox::files
::content ::dropbox::content
::store ::hot::store

kb ::store/Map({name: "dropbox-kb", embedding: ::store/EmbeddingOptions.Default})

index-file fn (file: Map): Bool {
  doc ::content/read-document(file.id)
  cond {
    is-err(doc) => { false }
    is-null(doc.text) => { false }   // PDFs, images: route to an OCR or PDF package
    => {
      ::store/put(kb, file.id, {content: doc.text, name: doc.file.name, path: doc.file.path, version: doc.file.version})
      true
    }
  }
}

index-folder fn (path: Str): Int {
  files filter(::files/list-all(path, {recursive: true}), ::dropbox/is-file)
  length(filter(map(files, index-file), (ok) { ok }))
}
```

## Recipe: change sync

Store a cursor once, then read every change since it on a schedule (or when a webhook fires):

```hot
::sync ::dropbox::sync
::content ::dropbox::content
::store ::hot::store

state ::store/Map({name: "dropbox-sync"})
kb ::store/Map({name: "dropbox-kb"})

dropbox-sync
meta {schedule: "every 15 minutes", retry: {attempts: 5, delay: 5000, backoff: "exponential", max_delay: 300000}}
fn (event) {
  cursor or(::store/get(state, "cursor"), ::sync/get-latest-cursor("/Docs", {recursive: true}))
  set ::sync/sync-changes(cursor, {path: "/Docs", recursive: true})
  if(::dropbox::api/is-retryable(set), fail(`dropbox: retrying later: ${err-message(set)}`), null)
  map(::sync/changed-files(set), (f) {
    if(f.is-folder, null, ::store/put(kb, f.id, {name: f.name, content: ::content/read-document-text(f.id)}))
  })
  map(::sync/deleted-paths(set), (p) { ::store/delete(kb, p) })
  ::store/put(state, "cursor", set.cursor)
}
```

Deletions are reported as `deleted` entries without ids, so key a mirror by `path_lower` (or keep an id-to-path map); deleting a folder reports only the folder (`is-under` finds its descendants). When Dropbox invalidates a cursor (409 `reset`), `sync-changes` relists `opts.path` and returns `reset: true` with the full listing in `entries`: replace the mirror rather than merging. Use the same listing options (`recursive`, `include_deleted`, ...) for every sync of one cursor.

`longpoll(cursor, timeout)` waits on `notify.dropboxapi.com` (no auth) until something changes, for 30 to 480 seconds plus up to 90 s of jitter. It is one blocking HTTP call, not a sleep, but it holds the run open the whole time, and Hot's HTTP timeout (120 s by default) ends longer waits with a transport Err, so keep the default 30 s and treat a timeout as "no changes". When the result carries `backoff`, wait that many seconds (schedule the next poll). Webhooks scale better for many accounts.

## Webhooks

Add a webhook URI in the App Console. Dropbox first verifies it with a `GET ?challenge=...` that you echo back, then `POST`s notifications naming the accounts that changed. Register both methods on one path:

```hot
::hooks ::dropbox::webhooks

dropbox-verify
meta {webhook: {service: "dropbox", path: "/dropbox/hook", method: "GET"}}
fn (request) {
  ::hooks/challenge-response(request)
}

dropbox-hook
meta {webhook: {service: "dropbox", path: "/dropbox/hook"}, secret-headers: ["x-dropbox-signature"]}
fn (request) {
  note ::hooks/verify-notification(request)
  cond {
    is-err(note) => { ::hot::http/HttpResponse({status: 403, body: "bad signature"}) }
    => {
      map(note.accounts, (account) { send("dropbox:changed", {account: account}) })
      ::hot::http/HttpResponse({status: 200, body: ""})
    }
  }
}
```

- `challenge-response` answers with `Content-Type: text/plain` and `X-Content-Type-Options: nosniff`, as Dropbox requires.
- `verify-notification` checks `X-Dropbox-Signature` (hex HMAC-SHA256 of the raw body, keyed with the app secret from `dropbox.app.secret`) in constant time over `::hot::http/raw-body`, and fails closed: a missing, malformed or wrong signature is an Err, and an unset app secret halts the handler. Dropbox signs no timestamp, so there is no replay window; a replayed notification only triggers a harmless extra sync.
- The body has no file data: `accounts` (`list_folder.accounts`, `dbid:` ids) and `users` (`delta.users`, numeric ids, also used by team apps). Map the account to its stored cursor and run `sync-changes`.
- **Respond within 10 seconds.** Dropbox retries failed or slow deliveries with backoff for about ten minutes and disables webhooks that keep failing. Verify, `send` an event per account, return 200, and sync in the event handler. Notifications for one account can arrive close together, so keep the sync idempotent.

## Errors, retries and pagination

Failures are `Result.Err` values. HTTP errors carry `::dropbox::api/HttpError`: `status`, `error-tag` (`"path/not_found"`), `error-tags`, `error` (Dropbox's typed union), `error-summary`, `user-message` (safe to show users, when sent), `retry-after`, `route`, and a readable `message` such as `"dropbox: HTTP 409 path/not_found (files/get_metadata)"`. Token problems carry `::dropbox::auth/TokenError`. Messages never include tokens.

- 409 is an endpoint error with a typed union: test it with `has-error-tag(result, "not_found")`, `is-not-found`, `is-reset`, or read nested fields with `error-node`.
- 429 surfaces `retry-after` from `Retry-After` (or the body's `retry_after`). Nothing waits inside a run: `is-retryable` is true for 408, 429, 5xx and transient 409s (`too_many_write_operations`, `retry_error`), so fail the run and let a handler, schedule or task `retry` policy back off (see [retries](https://hot.dev/docs/retries)). A dropped connection on a read-only route is resent once, immediately (`retry: false` turns that off).
- A cached access token rejected with 401 is evicted and the call retried once with a fresh token.
- Cursor lists: `list-all`, `search-all` and `list-all-shared-links` follow `has_more` / `cursor` for you; `::dropbox::api/collect-cursor` does it for any route.
- Batch jobs never wait: `delete-batch` and friends return a `BatchJob` whose `state` is `complete` or `async`; poll `check-delete-batch(job.job-id)` (etc.) from a later run until `is-done`.

## Large files

- **Downloads** larger than one response (`http.max-response-bytes`, 16 MiB by default) switch to Range requests automatically, pinned to the file's `rev`; `download-chunked` does it explicitly and `download-range` reads a slice. Exports cannot be ranged. The whole file is held in memory.
- **Uploads** up to 32 MiB (`session-threshold`) go in one `files/upload` request; larger ones use an upload session in 16 MiB chunks (`chunk-size`, a multiple of 4 MiB, at most 148 MiB). Each request carries the `content_hash` of its bytes so Dropbox rejects corrupted data (`verify: false` to skip). A failed chunk returns an `UploadError` with `session-id` and `offset`; `resume-upload` continues it (sessions last 7 days).
- Use `mode: "update"` with `rev` to save edits only when nobody changed the file since you read it (`"overwrite"` always wins, `"add"`, the default, never overwrites).

## Notes and limits

- **Data transport limit (Business plans).** Uploads (`upload`, and session start, append and finish) count as data transport calls. Dropbox Business teams on plans with a monthly limit get 403 `invalid_account_type/feature` once it is used up; `::dropbox::api/is-transport-limit` detects it, and retrying will not help until the next month or a plan change. Larger session chunks mean fewer calls. Downloads and metadata calls do not count.
- Team features (`select-user`, `select-admin`, team space, team folders) need a Dropbox team (Business/Advanced) plan and a team-scoped app; everything else works with a free Basic account, which is enough for the integration tests.
- `search_v2` returns at most 10,000 matches in total and its index lags recent changes by a few minutes; use listing or sync when you need every file.
- `delete-path` moves files to Dropbox's deleted files (restore with `restore-file` within the plan's version-history window); permanent deletion is not wrapped.
- `get-temporary-link` links (4 hours) and public shared links let anyone holding them download the file: treat them as secrets.
- `longpoll` holds the run open (see the sync recipe); prefer webhooks or a schedule for many accounts.

## Data handling

Tokens and the app secret are read from secret context variables, cached access tokens are stored as run secrets, and none of them appear in error messages or logs (use `secret-headers: ["x-dropbox-signature"]` on webhook handlers). File content is held in memory for the run only; this package stores nothing itself.

## Integration tests

`hot test -p pkg-integration-dropbox` runs read-only smoke tests (account, listing and cursors, a download checked against its `content_hash`). Set `DROPBOX_ACCESS_TOKEN`, or `DROPBOX_REFRESH_TOKEN` + `DROPBOX_APP_KEY` (+ `DROPBOX_APP_SECRET`), and optionally `DROPBOX_TEST_PATH` (a folder to read). Without credentials every test skips.

## License

Apache-2.0 - see [LICENSE](LICENSE)
