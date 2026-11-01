# microsoft-onedrive

OneDrive and SharePoint document libraries for Hot, over the Microsoft Graph `driveItem` API, built for document workflows and agents: resolve any drive (yours, a user's, a SharePoint library, a group's), address items by id or path, list and search, read any file for RAG (as stored, converted to PDF, or as text), Range downloads past the response cap, binary-safe simple and resumable uploads, folders, move, copy, sharing, versions, and delta sync with change-notification subscriptions. The same functions work for OneDrive (personal), OneDrive for work or school, and SharePoint libraries. Credentials, clouds, retries and errors come from [`microsoft-graph`](../microsoft-graph).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/microsoft-onedrive": "0.1.0"
}
```

## Auth and permissions

Set credentials once in Context Variables (any [`microsoft-graph`](../microsoft-graph/README.md#authentication) flow), or pass them per call in `opts`:

| Context Variable | Secret | Description |
|---|---|---|
| `microsoft.oauth.refresh-token` + `microsoft.client.id` | yes / no | A user's delegated refresh token |
| `microsoft.client.id` + `microsoft.tenant.id` + `microsoft.client.secret` or `microsoft.client.private-key` | | App-only (client secret or certificate) |
| `microsoft.access.token` | yes | A ready access token |
| `microsoft.onedrive.drive-id` | no | Default drive for every call (needed for app-only, which has no `/me`); `opts.drive` overrides it |
| `microsoft.graph.client-state` | yes | Shared secret for change-notification webhooks |

| Permission | Type | Enough for |
|---|---|---|
| `Files.Read` / `Files.ReadWrite` | Delegated | The signed-in user's own OneDrive (personal accounts too) |
| `Files.Read.All` / `Files.ReadWrite.All` | Delegated or application | Everything the user can reach (delegated) or every drive in the tenant (application, admin consent) |
| `Sites.Read.All` / `Sites.ReadWrite.All` | Delegated or application | SharePoint libraries |
| `Sites.Selected`, `Files.SelectedOperations.Selected` | Application (or delegated) | Only the sites or files an admin granted (see [microsoft-sharepoint](../microsoft-sharepoint/README.md#least-privilege-app-only-access)) |

Application permissions always need admin consent. Every function has an options arity: OData options (`select`, `expand`, `top`, `orderby`, `filter`), this package's own keys (`drive`, `conflict`, `if-match`, `chunk-size`, ...) and microsoft-graph credential keys (`token`, `refresh-token`, `client-id`, `tenant-id`, ...).

## Drives and items

`opts.drive` picks the drive: `"me"` (default), a drive id (any OneDrive or SharePoint library), or a base path from `::microsoft::onedrive/user-drive(id)`, `site-drive(site-id)`, `group-drive(id)`. An item is an id, `"root"`, a path from the drive root (`"/Reports/Q3 Plan.xlsx"`, each segment percent-encoded for you), or a driveItem / `DocFile` Map (which carries its own drive id, so results from delta, search and shared lists work across drives).

## Quick start

```hot
::items ::microsoft::onedrive::items
::content ::microsoft::onedrive::content
::up ::microsoft::onedrive::upload

// List and search
reports ::items/list-all-children("/Reports", {select: "id,name,size,file,folder,parentReference,eTag,cTag"}, 200)
hits ::items/search("quarterly plan", {}, 25)

// Read any file: Office files as stored, or as PDF when small enough to convert
doc ::content/read-document(first(hits), "pdf")
doc.format       // "pdf", or "original" when conversion did not apply
doc.file.name

// Upload (binary-safe; an upload session above 4 MiB)
item ::up/upload-file("/Reports", "q3.csv", "region,total\nwest,42\n", "text/csv")

// A SharePoint library is just another drive
lib-files ::items/list-all-children("root", {drive: "b!Lw5bH0Xn9UKz3j8RvbcAtQ"}, 50)
```

## Functions by area

**`::microsoft::onedrive::drives`**: `get-drive`, `my-drive`, `user-drive`, `site-drives`, `group-drive`, `list-drives-at`.

**`::microsoft::onedrive::items`**: `get-item`, `get-doc-file`, `resolve-remote`, `list-children` (one page), `list-all-children`, `search`, `recent`, `shared-with-me`, `create-folder`, `ensure-folder`, `update-item`, `rename-item`, `move-item`, `copy-item`, `copy-status`, `is-copy-done`, `delete-item`, `permanent-delete`, `restore-item`.

**`::microsoft::onedrive::content`**: `read-document`, `read-document-text`, `read-plan`, `download`, `download-text`, `download-range`, `download-chunked`, `convert`, `list-thumbnails`, `thumbnail`.

**`::microsoft::onedrive::upload`**: `upload-file`, `upload-small`, `upload-large`, `replace-content`, `create-upload-session`, `create-replace-session`, `upload-to-session`, `upload-session-status`, `resume-upload`, `cancel-upload-session`.

**`::microsoft::onedrive::sharing`**: `create-link`, `invite`, `list-permissions`, `get-permission`, `update-permission`, `delete-permission`, `grant-app`.

**`::microsoft::onedrive::versions`**: `list-versions`, `get-version`, `download-version`, `restore-version`.

**`::microsoft::onedrive::delta`**: `delta-page`, `latest-delta-link`, `sync-changes`, `changed-items`, `changed-files`, `deleted-ids`, `stale-ids`, `subscribe`, `renew-subscription`.

**`::microsoft::onedrive`**: drive and item addressing (`me-drive`, `drive`, `user-drive`, `site-drive`, `group-drive`, `item-path`, `child-path`, `encode-path`), `to-doc-file`, `to-doc-files`, `content-text`, and aliases for the most used functions (`get-item`, `list-children`, `list-all-children`, `search`, `create-folder`, `delete-item`, `download`, `read-document`, `upload-file`, `sync-changes`).

## Results

Item functions return Graph's driveItems as Maps (`list-children` returns the raw page with `@odata.nextLink`). `::microsoft::onedrive/to-doc-file` gives a provider-neutral record with **the same fields as google-drive's `DocFile`, plus `hashes`**:

| `DocFile` field | From the driveItem |
|---|---|
| `provider` | `"microsoft-onedrive"` (OneDrive and SharePoint alike; `raw.parentReference.driveType` tells them apart) |
| `id`, `name`, `mime-type` | `id`, `name`, `file.mimeType` (`""` for folders and OneNote packages) |
| `size` | `size` as an Int (null for folders and packages) |
| `modified`, `created` | `lastModifiedDateTime`, `createdDateTime` |
| `parents`, `drive-id`, `web-url` | `[parentReference.id]`, `parentReference.driveId`, `webUrl` |
| `version` | `eTag` (changes on any change; use it for `if-match`) |
| `revision-id` | `cTag` (changes only when content changes) |
| `md5` | always null (Graph has no MD5) |
| `sha256` | `file.hashes.sha256Hash`, lower-cased, when present (Graph documents it as unsupported, so usually null) |
| `hashes` | `file.hashes` as returned: `quickXorHash` (work/school and SharePoint), `sha1Hash` and `crc32Hash` (personal) |
| `is-folder` | a `folder` or `package` facet (shared folders too) |
| `is-google-native` | always false |
| `is-shortcut`, `shortcut-target-id` | a `remoteItem` (shared with me, "Add to My files") and its id |
| `trashed` | a `deleted` facet (delta) |
| `export-formats` | MIME types Graph can convert this file to (`application/pdf`, `text/html`) |
| `raw` | the driveItem |

Content functions return a `FileContent` (the google-drive shape): `bytes` (the exact response bytes), `mime-type`, `size`, `text` (set when the file is text by MIME type or extension), `source` (`download`, `range`, `chunked`, `convert`, `version`, `thumbnail`), and from `read-document` also `name`, `format` and `file` (a `DocFile`).

## Reading documents

`read-document(item, format)` follows shared items to the owner's drive and returns folders as an Err:

| `format` | Result |
|---|---|
| `"auto"` (default), `"original"` | The stored bytes. Office files come as-is for a text extractor ([libreoffice](../libreoffice), [pandoc](../pandoc), ...); `text` is set for `.txt`, `.md`, `.csv`, `.json`, `.xml`, `.yaml`, `.html` and `text/*` |
| `"pdf"` | A `.pdf` as stored; Word, Excel, PowerPoint, RTF, ODF, HTML, Markdown, EML/MSG, TIFF, Loop and Whiteboard files converted by Graph when at most **2 MB**; otherwise, or when Graph refuses, the original (`format: "original"`) |
| `"html"` | Loop, Fluid and Whiteboard as HTML (at most 2 MB); others as stored |
| `"text"` | Text files only; an Err for binaries |

Check `doc.format` and `doc.mime-type` for what you got. `convert(item, "pdf" | "html" | "jpg")` calls the conversion directly (`jpg` needs `width` and `height`).

## Recipe: RAG ingestion

Read every document in a folder and store it in a searchable Hot store:

```hot
::items ::microsoft::onedrive::items
::content ::microsoft::onedrive::content
::store ::hot::store

kb ::store/Map({name: "onedrive-kb", embedding: ::store/EmbeddingOptions.Default})

index-file fn (item: Map): Bool {
  doc ::content/read-document(item, "auto")
  cond {
    is-err(doc) => { false }
    is-null(doc.text) => { false }   // Office files, PDFs: route to a text extractor
    => {
      ::store/put(kb, doc.file.id, {content: doc.text, name: doc.file.name, url: doc.file.web-url, version: doc.file.revision-id})
      true
    }
  }
}

index-folder fn (path: Str): Int {
  files filter(::items/list-all-children(path, {}, 5000), (i) { is-map(i.file) })
  length(filter(map(files, index-file), (ok) { ok }))
}
```

Re-index only what changed by comparing `revision-id` (the cTag), or use the sync recipe below.

## Recipe: delta sync

Store a delta link once, then read every change since it on a schedule or when a notification arrives:

```hot
::delta ::microsoft::onedrive::delta
::content ::microsoft::onedrive::content
::store ::hot::store

state ::store/Map({name: "onedrive-sync"})
kb ::store/Map({name: "onedrive-kb"})

onedrive-sync
meta {schedule: "every 15 minutes"}
fn (event) {
  link or(::store/get(state, "delta-link"), ::delta/latest-delta-link())
  set ::delta/sync-changes(link)
  map(::delta/changed-files(set), (f) {
    ::store/put(kb, f.id, {name: f.name, version: f.revision-id, content: ::content/read-document-text(f.raw)})
  })
  map(::delta/deleted-ids(set), (id) { ::store/delete(kb, id) })
  ::store/put(state, "delta-link", set.delta-link)
}
```

- Pass `{drive: library-id}` to `latest-delta-link` and `sync-changes` to follow a SharePoint library. On OneDrive for work or school and SharePoint, delta runs from the drive root only.
- Delta reports each item's latest state, possibly more than once (the last one wins), without paths: **track items by id**. Deleted items carry a `deleted` facet and may have no name.
- **410 Gone.** When Graph discards a delta link, `sync-changes` restarts from the `Location` it returns and marks the set `resync: true` with `resync-mode`. For `resyncChangesApplyDifferences`, the set is the full drive: replace your local state with it and delete `::delta/stale-ids(local-ids, set)`. For `resyncChangesUploadDifferences`, keep local-only items and reconcile them yourself.
- `latest-delta-link()` (`token=latest`) starts from now without enumerating the drive; `sync-changes(null)` enumerates everything first, which is the only reliable way to build a full mirror.

### Change notifications

`subscribe` asks Graph to POST to your HTTPS endpoint when anything in the drive changes (`updated` on the root is the only option for drives). Notifications carry no item data: run `sync-changes`. Subscriptions last at most 42,300 minutes (under 30 days) and are never renewed by Graph, so renew from a schedule:

```hot
::delta ::microsoft::onedrive::delta
::subs ::microsoft::graph::subscriptions
::store ::hot::store

state ::store/Map({name: "onedrive-sync"})

start-watching fn (): Map {
  sub ::delta/subscribe("https://api.example.com/onedrive/hook", {client-state: ::hot::ctx/get("microsoft.graph.client-state")})
  if(not(is-err(sub)), ::store/put(state, "subscription-id", sub.id))
  sub
}

renew-onedrive-subscription
meta {schedule: "0 5 * * 1"}
fn (event) {
  ::delta/renew-subscription(::store/get(state, "subscription-id"))
}

onedrive-hook
meta {webhook: {service: "onedrive", path: "/onedrive/hook"}}
fn (request) {
  handshake ::subs/validation-response(request)
  cond {
    is-some(handshake) => { handshake }
    is-err(::subs/verify-notifications(request)) => { ::hot::http/HttpResponse({status: 401, body: "bad clientState"}) }
    => {
      send("onedrive:changed", {})
      ::subs/accepted()
    }
  }
}
```

Reply within 3 seconds (`accepted()` is a `202`) and do the sync in the `onedrive:changed` handler. See [microsoft-graph](../microsoft-graph/README.md#change-notifications-webhooks) for validation, `clientState` verification and lifecycle notifications.

## Large files

- **Downloads.** `download` fetches `/content` (Graph redirects to a pre-authenticated URL). Files larger than `http.max-response-bytes` (16 MiB by default) switch to Range requests on the item's `@microsoft.graph.downloadUrl` (Graph ignores Range on `/content`), 8 MiB at a time (`chunk-size`); `download-range(item, start, end)` reads a slice. The whole file is still held in memory.
- **Uploads.** `upload-file` sends one `PUT` up to 4 MiB (Graph allows up to 250 MB, but one dropped connection loses it all) and an upload session above. Sessions upload in order in chunks that are multiples of 320 KiB and under 60 MiB, 10 MiB by default (Microsoft recommends 5-10 MiB). Chunk PUTs go to the pre-authenticated `upload-url` **without an Authorization header**. A dropped connection is retried once at once; 429, 5xx or 416 stop with an Err. Keep the session's `upload-url` (`create-upload-session` + `upload-to-session`) and call `resume-upload(upload-url, bytes)` from a later run: it reads `nextExpectedRanges` and sends the rest. `cancel-upload-session` discards one.
- **Conflicts.** `opts.conflict`: `replace` (the default for uploads; OneDrive keeps the old content as a version), `rename` or `fail` (the default for `create-folder`).
- **Copies** run asynchronously: `copy-item` returns `{monitor-url}` at once; check it later with `copy-status(monitor-url)` from a schedule or event (never in a loop). When personal OneDrive finishes by redirecting to the new item, `resource-id` is null: find the copy in the destination folder.

## Notes and limits

- `recent` and `shared-with-me` are **deprecated by Microsoft**: degraded until November 2026, after which they return no data, and never available app-only. Use `search` (`scope: "drive"` on `/me/drive` includes items shared with the user) or delta sync instead.
- `delete-item` moves to the recycle bin (restorable for 93 days on work or school, 30 on personal); `permanent-delete` (work/school and SharePoint only) cannot be undone. `restore-item` works on personal OneDrive only.
- `move-item` cannot cross drives: copy, then delete.
- Format conversion only accepts source files up to 2 MB; app-only conversion is documented as needing `Files.ReadWrite.All`.
- App-only cannot replace the content of a file protected by a sensitivity label.
- Sharing to people sends no email unless `send-invitation: true`; anonymous links may be disabled by tenant policy.
- Errors are `::microsoft::graph::api/HttpError` with Graph's `code` (`itemNotFound`, `accessDenied`, `nameAlreadyExists`, `resyncRequired`, ...). Nothing is retried with backoff inside a run: check `::microsoft::graph::api/is-retryable(result)` and let a `retry` policy reschedule.
- Download and upload URLs are pre-authenticated: treat them as secrets and never log them.

## License

Apache-2.0 - see [LICENSE](LICENSE)
