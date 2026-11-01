# google-drive

Google Drive API v3 bindings for Hot, built for document workflows and agents: search with a query builder, read any file as text (Google Docs as Markdown, Sheets as CSV, Slides as text), binary-safe and resumable uploads, chunked downloads past the response cap, shared drives, sharing, revisions, and change sync with push-notification webhooks. Credentials, scopes, retries and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-drive": "0.1.0"
}
```

## Auth and scopes

Set credentials once in Context Variables (any `google-core` flow), or pass them per call in `opts` for multi-tenant apps:

| Context Variable | Description |
|---|---|
| `google.access.token` | A ready OAuth access token |
| `google.oauth.refresh-token` + `google.oauth.client-id` + `google.oauth.client-secret` | A user's refresh token (see the [consent flow](../google-core/README.md#consent-flow)) |
| `google.service.account` (+ `google.impersonate`) | Service-account key JSON, optionally with domain-wide delegation |
| `google.scopes` | Scopes for service-account tokens, overriding the defaults below |
| `google.drive.channel.token` | Shared secret checked by `::google::drive::changes/verify-notification` |
| `google.drive.api-root` | Optional API root override for a proxy or emulator (default `https://www.googleapis.com`) |

| Scope | Classification | Enough for |
|---|---|---|
| `drive.file` | Non-sensitive | Everything, but only on files your app created or the user opened with it (e.g. via the Google Picker) |
| `drive.readonly` | Restricted | Reading every file: list, get, `read-document`, download, export, changes, revisions, permissions listing (the default for reads) |
| `drive` | Restricted | Everything (the default for writes: create, upload, update, move, trash, delete, share) |
| `drive.metadata.readonly` | Restricted | Listing and metadata only, no content |

`drive` and `drive.readonly` are **restricted** scopes: a public app that requests them must pass Google's OAuth verification and an annual third-party security assessment. `drive.file` is the non-sensitive alternative and works for most "pick a file, process it" apps. With access and refresh tokens the user's granted scopes apply; the defaults matter for service accounts, where `google.scopes` (or `opts.scopes`) must match what a Workspace admin authorised for domain-wide delegation.

Every function has an options arity. Drive query parameters use Drive's own names (`q`, `pageSize`, `pageToken`, `fields`, `orderBy`, `driveId`, ...); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`.

## Quick start

```hot
::files ::google::drive::files
::content ::google::drive::content
::up ::google::drive::upload
::q ::google::drive::query

// Search
hits ::files/list-all-files({
  q: ::q/all-of(::q/name-contains("roadmap"), ::q/not-folder(), ::q/not-trashed()),
  orderBy: "modifiedTime desc"
}, 20)

// Read any file as text: Docs export to Markdown, blobs download as stored
doc ::content/read-document(first(hits).id, "markdown")
doc.text
doc.file.web-url

// Upload (binary-safe; resumable above 5 MiB) into a folder
folder ::files/ensure-folder("Reports", "root")
report ::up/upload-file("q3.csv", "region,total\nwest,42\n", "text/csv", folder.id)

// Convert on upload: a CSV becomes a Google Sheet
sheet ::up/upload-file("Q3", "region,total\nwest,42\n", "text/csv", {parent: folder.id, convert-to: "spreadsheet"})

// Tidy up: trash is recoverable for 30 days
::files/trash-file(report.id)
```

## Functions by area

**`::google::drive::files`**: `list-files` (one page), `list-all-files` (all pages, optional limit), `list-folder`, `get-file`, `get-doc-file`, `resolve-shortcut`, `create-folder`, `ensure-folder`, `create-shortcut`, `update-file`, `rename-file`, `move-file`, `copy-file`, `trash-file`, `untrash-file`, `delete-file` (permanent; prefer `trash-file`).

**`::google::drive::query`**: `quote`, `term`, `name-is`, `name-contains`, `full-text-contains`, `mime-type-is`, `mime-type-is-not`, `is-folder`, `not-folder`, `in-folder`, `owned-by`, `modified-after`, `modified-before`, `created-after`, `not-trashed`, `trashed`, `starred`, `shared-with-me`, `shortcut-to`, `has-property`, `app-property`, and the combinators `all-of`, `any-of`, `negate`. Values are quoted with `'` and `\` escaped, so `::q/name-is("Valentine's Day")` is safe.

**`::google::drive::content`**: `read-document`, `read-document-text`, `download-file`, `download-text`, `download-range`, `download-file-chunked`, `export-file`, `export-text`, `export-mime-type`, `export-formats`, `EXPORT_FORMATS`.

**`::google::drive::upload`**: `upload-file`, `update-file-content`, `start-resumable-upload`, `upload-resumable`, `upload-status`, `resume-upload`, `build-multipart`.

**`::google::drive::permissions`**: `list-permissions`, `list-all-permissions`, `get-permission`, `create-permission`, `build-permission`, `share-with-user`, `share-with-group`, `share-with-domain`, `share-with-anyone`, `update-permission`, `delete-permission`.

**`::google::drive::drives`**: `list-drives`, `list-all-drives`, `get-drive`, `about`, `export-formats`.

**`::google::drive::revisions`**: `list-revisions`, `list-all-revisions`, `get-revision`, `download-revision`, `keep-forever`.

**`::google::drive::changes`**: `get-start-page-token`, `list-changes`, `sync-changes`, `changed-files`, `removed-file-ids`, `watch-changes`, `watch-file`, `stop-channel`, `renew-watch`, `build-channel`, `parse-notification`, `verify-channel-token`, `verify-notification`.

The most used functions are also aliased at the top level (`::google::drive/list-files`, `get-file`, `read-document`, `upload-file`, `download-file`, `export-file`, `create-folder`, `trash-file`, `delete-file`).

## Results

List functions return Drive's resources as Maps (`list-files` returns the raw page with `nextPageToken`). Use `::google::drive/to-doc-file` for a provider-neutral record:

| `DocFile` field | From Drive |
|---|---|
| `provider` | `"google-drive"` |
| `id`, `name`, `mime-type` | `id`, `name`, `mimeType` |
| `size` | `size` as an Int (null for Google-native files and folders) |
| `modified`, `created` | `modifiedTime`, `createdTime` |
| `parents`, `drive-id`, `web-url` | `parents`, `driveId`, `webViewLink` |
| `version`, `revision-id` | `version` (use as an etag), `headRevisionId` |
| `md5`, `sha256` | `md5Checksum`, `sha256Checksum` |
| `is-folder`, `is-google-native`, `is-shortcut`, `shortcut-target-id`, `trashed` | derived |
| `export-formats` | the MIME types in `exportLinks` |
| `raw` | the Drive resource |

Content functions return a `FileContent`: `bytes` (the exact response bytes, so a JSON file stays bytes and a file containing `123` stays text), `mime-type`, `size`, `text` (set when the type is textual), `source` (`download`, `export`, `export-link`, `range`, `chunked`), and from `read-document` also `name`, `format` and `file` (a `DocFile`).

## Shared drives

`supportsAllDrives=true` is sent on every call, and listings include shared-drive items (`includeItemsFromAllDrives=true`). To search one shared drive, pass its id; `corpora` then defaults to `"drive"`:

```hot
::files ::google::drive::files
::drives ::google::drive::drives

eng first(filter(::drives/list-all-drives(), (d) { eq(d.name, "Engineering") }))
specs ::files/list-all-files({driveId: eng.id, q: "trashed = false"}, 500)
```

Use `corpora: "allDrives"` to search everything the user can reach (slower; Drive may set `incompleteSearch`). Items in a shared drive have exactly one parent, so use `create-shortcut` to show them elsewhere.

## Google Docs export

`read-document(id, format)` exports Google-native files and downloads everything else as stored. `format` defaults to `"auto"`:

| Type | `"auto"` | Other formats |
|---|---|---|
| Google Docs | `markdown` (`text/markdown`) | `text`, `html`, `zip`, `pdf`, `docx`, `odt`, `rtf`, `epub` |
| Google Sheets | `csv` (first sheet only) | `tsv` (first sheet only), `xlsx`, `ods`, `pdf`, `zip` |
| Google Slides | `text` (`text/plain`) | `pdf`, `pptx`, `odp` |
| Google Drawings | `svg` | `png`, `jpeg`, `pdf` |
| Apps Script | `json` | |

A full MIME type also works as `format`. Forms, Sites and Vids cannot be exported (Vids need the long-running `files.download`, not wrapped here). `files.export` is limited to 10 MB; larger exports fall back to the file's `exportLinks` automatically. Shortcuts are followed to their target, and folders return an Err. Check the live list for an account with `::google::drive::drives/export-formats()`.

## Recipe: RAG ingestion

List a folder, read every document as text, and store it in a searchable Hot store:

```hot
::files ::google::drive::files
::content ::google::drive::content
::q ::google::drive::query
::store ::hot::store

kb ::store/Map({name: "drive-kb", embedding: ::store/EmbeddingOptions.Default})

index-file fn (file: Map): Bool {
  doc ::content/read-document(file.id, "auto")
  cond {
    is-err(doc) => { false }
    is-null(doc.text) => { false }   // PDFs, images: route to an OCR or PDF package
    => {
      ::store/put(kb, file.id, {
        content: doc.text,
        name: doc.file.name,
        url: doc.file.web-url,
        version: doc.file.version
      })
      true
    }
  }
}

index-folder fn (folder-id: Str): Int {
  files ::files/list-folder(folder-id, {q: ::q/not-folder()})
  length(filter(map(files, index-file), (ok) { ok }))
}
```

Re-index only what changed by comparing `doc.file.version` (or `modifiedTime`), or use the sync recipe below.

## Recipe: change sync

Keep an index current with the change feed. Store a start page token once, then read every change since it on a schedule (or when a webhook fires):

```hot
::changes ::google::drive::changes
::content ::google::drive::content
::store ::hot::store

state ::store/Map({name: "drive-sync"})
kb ::store/Map({name: "drive-kb"})

drive-sync
meta {schedule: "every 15 minutes"}
fn (event) {
  token or(::store/get(state, "page-token"), ::changes/get-start-page-token())
  set ::changes/sync-changes(token)
  map(::changes/changed-files(set), (f) {
    if(or(f.is-folder, f.is-shortcut), null, ::store/put(kb, f.id, {name: f.name, content: ::content/read-document-text(f.id)}))
  })
  map(::changes/removed-file-ids(set), (id) { ::store/delete(kb, id) })
  ::store/put(state, "page-token", set.new-start-page-token)
}
```

Pass `{driveId: "..."}` to `get-start-page-token` and `sync-changes` to follow one shared drive.

### Push notifications and watch renewal

`watch-changes` asks Drive to POST to your HTTPS endpoint whenever something changes. Notifications carry only headers, so treat one as "run the sync now". Channels last at most 7 days for changes (1 day for `watch-file`) and are never renewed by Drive, so renew from a Hot schedule:

```hot
::changes ::google::drive::changes
::store ::hot::store

state ::store/Map({name: "drive-sync"})
HOOK_URL "https://api.example.com/drive/hook"

renew-drive-watch
meta {schedule: "0 3 */6 * *"}
fn (event) {
  token or(::store/get(state, "page-token"), ::changes/get-start-page-token())
  fresh ::changes/renew-watch(::store/get(state, "channel"), token, HOOK_URL, {
    channel-token: ::hot::ctx/get("google.drive.channel.token"),
    expires-in-ms: 604800000
  })
  ::store/put(state, "channel", fresh)
}

drive-hook
meta {webhook: {service: "drive", path: "/drive/hook"}, secret-headers: ["x-goog-channel-token"]}
fn (request) {
  note ::changes/verify-notification(request)
  cond {
    is-err(note) => { ::hot::http/HttpResponse({status: 401, body: "bad channel token"}) }
    note.is-sync => { ::hot::http/HttpResponse({status: 200, body: ""}) }
    => {
      send("drive:changed", {state: note.resource-state, message: note.message-number})
      ::hot::http/HttpResponse({status: 200, body: ""})
    }
  }
}
```

`renew-watch` opens the new channel before stopping the old one, so no change is missed. `verify-notification` compares `X-Goog-Channel-Token` in constant time and fails closed when the header or the configured token is missing; `parse-notification` gives `channel-id`, `resource-id`, `resource-state` (`sync`, `change`, or for file channels `add`, `update`, `trash`, `untrash`, `remove`), `changed` and `message-number`. The first message on a channel is a `sync`.

## Large files

- **Uploads** over 5 MiB (or with `resumable: true`) use a resumable session, sent in 8 MiB chunks (`chunk-size`, rounded down to 256 KiB multiples); a failed chunk is resent once. `start-resumable-upload` + `upload-resumable` give you the session URI to keep; `upload-status` and `resume-upload` continue an interrupted one.
- **Downloads** larger than one response (`http.max-response-bytes`, 16 MiB by default) switch to Range requests automatically; `download-file-chunked` does it explicitly and `download-range` reads a slice. The whole file is still held in memory.
- **Exports** cannot be chunked (Drive does not support Range on exports), so a single export must fit in `http.max-response-bytes`.

## Notes and limits

- Drive includes trashed files in listings unless `q` says `trashed = false` (`list-folder` adds it for you).
- `delete-file` skips the trash and cannot be undone; `trash-file` is recoverable for 30 days.
- Sharing to users and groups emails them unless you pass `{sendNotificationEmail: false}`.
- Service accounts have no Drive storage: uploads to a service account's own Drive fail with `storageQuotaExceeded`. Upload into a shared drive or impersonate a user.
- Hot 3.0.x's HTTP client reports Drive's `308 Resume Incomplete` as a redirect error. Chunked uploads handle this by assuming each 256 KiB-aligned chunk was stored, but `upload-status` cannot read the stored offset, so `resume-upload` asks you to restart. Uploads that fit in one chunk are unaffected.
- Errors are `::google::api/HttpError` with Google's `reason` (e.g. `notFound`, `insufficientFilePermissions`, `storageQuotaExceeded`). Nothing is retried with backoff inside a run: check `::google::api/is-retryable(result)` and let a handler or task `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)).

## License

Apache-2.0 - see [LICENSE](LICENSE)
