# documents

One document layer for workflows and AI agents over Google Drive, OneDrive and SharePoint, Dropbox, Box and Amazon S3: a single `Document` record, source descriptors, `list` / `search` / `get` / `download`, `read-text` with each provider's native export or extraction, `upload`, change feeds behind opaque cursors, webhook normalising, and a capability map agents can plan with. Built for RAG ingestion and file-triggered workflows.

It sits on top of the provider packages ([`google-drive`](../google-drive), [`microsoft-onedrive`](../microsoft-onedrive), [`dropbox`](../dropbox), [`box`](../box), [`aws-s3`](../aws-s3)) and adds no auth of its own: credentials, token caches, retries and error types are theirs.

**Use a provider package directly** when you need something only that provider has: sharing and permissions, revisions and versions, Drive query syntax, Box metadata search or collaborations, Dropbox team spaces, Graph batch requests, watch-channel and subscription management, resumable-upload control. Everything here returns the provider's resource in `Document.raw`, so you can mix the two.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/documents": "0.1.0"
}
```

The provider packages come with it.

## Auth and ctx variables

This package reads and writes no context itself. Each source's `opts` go straight to its provider package, which takes credentials from `opts` first and its own context variables second. Set the provider's variables once for a single-tenant app, or put credentials in the source's `opts` for many tenants and parallel flows (nothing per-call is ever written to context).

| Provider | Context variables (see the provider README) | Per-source `opts` keys |
|---|---|---|
| Google Drive | `google.access.token`; `google.oauth.refresh-token` + `google.oauth.client-id` + `google.oauth.client-secret`; `google.service.account` (+ `google.impersonate`) | `token`, `refresh-token`, `client-id`, `client-secret`, `service-account`, `subject`, `scopes` |
| OneDrive / SharePoint | `microsoft.access.token`; `microsoft.client.id` + `microsoft.oauth.refresh-token`; `microsoft.client.id` + `microsoft.client.secret` + `microsoft.tenant.id`; `microsoft.onedrive.drive-id` | `token`, `refresh-token`, `client-id`, `client-secret`, `tenant-id`, `cloud`, ... |
| Dropbox | `dropbox.access.token`; `dropbox.refresh.token` + `dropbox.app.key` (+ `dropbox.app.secret`) | `token`, `refresh-token`, `app-key`, `app-secret`, `select-user`, `path-root` |
| Box | `box.access.token`; `box.client.id` + `box.client.secret` + `box.enterprise.id` / `box.user.id`; `box.jwt.config`; `box.oauth.account` | `token`, `account`, `client-id`, `client-secret`, `enterprise-id`, `user-id`, `as-user`, ... |
| Amazon S3 | `aws.access-key-id`, `aws.secret-access-key`, `aws.session-token`, `aws.region` | `access-key-id`, `secret-access-key`, `session-token`, `region`, `endpoint` |

Webhook secrets are the providers' too: `google.drive.channel.token`, `microsoft.graph.client-state`, `dropbox.app.secret`, `box.webhook.primary-key` / `box.webhook.secondary-key`.

## Quick start

```hot
::docs ::documents

src {provider: "google-drive"}

// One page of the root, and the next
page ::docs/list(src, null, {page-size: 20})
more ::docs/list(src, null, {cursor: page.cursor})

// Read the first file as text: Google Docs export to Markdown
doc first(filter(page.documents, (d) { eq(d.kind, "file") }))
text ::docs/read-text(src, doc.id, "auto")
text.status   // "ok", "pending" or "unsupported"
text.text

// The same calls work for every provider
box {provider: "box", folder-id: "12345"}
hits ::docs/search(box, "quarterly report", {limit: 10})
```

## Source descriptors

A source names the provider, the scope inside it, and the `opts` for every call on it. A bare provider name (`"box"`) is a source with the default scope and context credentials.

| Provider | Descriptor | Scope |
|---|---|---|
| Google Drive | `{provider: "google-drive", drive-id?, opts}` | `drive-id`: one shared drive; default My Drive and everything shared |
| OneDrive / SharePoint | `{provider: "onedrive", drive?, opts}` | `drive`: `"me"`, a drive id (a SharePoint document library), `{drive-id}`, `{user}`, `{site}` (the site's default library) or `{group}`; app-only tokens need one |
| Dropbox | `{provider: "dropbox", root?, opts}` | `root`: a folder path; default the whole account |
| Box | `{provider: "box", folder-id?, opts}` | `folder-id`: default `"0"`, the root |
| Amazon S3 | `{provider: "s3", bucket, prefix?, opts}` | `prefix`: a key prefix such as `"inbox/"`; `opts.endpoint` for S3-compatible services (path style) |

Aliases are accepted: `google`, `drive`, `microsoft-onedrive`, `sharepoint`, `aws-s3`.

```hot
::docs ::documents

tenant-library fn (site-id: Str, tenant-token: Str): Map {
  {provider: "onedrive", drive: {site: site-id}, opts: {token: tenant-token}}
}

bucket {provider: "s3", bucket: "acme-docs", prefix: "inbox/", opts: {region: "eu-west-1"}}
r2 {provider: "s3", bucket: "acme", opts: {endpoint: "https://ACCOUNT.r2.cloudflarestorage.com", region: "auto"}}

// Per-call opts merge over the source's opts and win
docs ::docs/list-all(tenant-library("contoso.sharepoint.com,1111,2222", "tok"), null, 100, {select: "id,name,file,folder,size,eTag,parentReference"})
```

Call `opts` are merged over the source's `opts`; this package's own keys (`cursor`, `page-size`, `limit`, `mime-type`, `max-pages`) are removed and everything else goes to the provider, so provider options work too (Box `fields`, Dropbox `include_deleted`, OneDrive `select`, ...).

## The Document

| Field | Meaning |
|---|---|
| `provider` | `"google-drive"`, `"onedrive"`, `"dropbox"`, `"box"` or `"s3"` |
| `source` | the scope it came from, never with credentials (`{provider: "onedrive", drive-id: "b!..."}`) |
| `id` | what `get`, `download` and `read-text` take: Drive / Box / OneDrive ids, Dropbox `id:...`, S3 keys |
| `name`, `path` | display path where the provider has one: Dropbox and S3 always, OneDrive and Box when the listing includes it, Drive never |
| `parents` | parent folder ids (Dropbox: the parent path; S3: the parent prefix) |
| `kind` | `"file"` or `"folder"` |
| `mime-type` | never empty: from the extension when the provider stores none (Dropbox, Box, S3 listings) |
| `is-native` | no stored bytes, must be exported: Google Docs, Sheets, Slides, Drawings; Dropbox Paper |
| `export-formats` | MIME types the provider exports or converts it to |
| `size` | bytes; null for folders and native documents |
| `created`, `modified` | RFC 3339; Dropbox and S3 record no creation time |
| `version` | opaque etag / rev: changes on every change, use it to skip re-indexing |
| `hashes` | by algorithm: `md5`, `sha256` (Drive), `quickXorHash`, `sha1`, `crc32` (OneDrive), `dropbox-content-hash`, `sha1` (Box); S3 ETags are not reported as MD5s |
| `web-url` | browser link, or null (S3) |
| `deleted` | trashed or deleted |
| `raw` | the provider's resource as returned |

Each provider has exactly one adapter in `::documents::adapters` (`from-google`, `from-onedrive`, `from-dropbox`, `from-box`, `from-s3`) that maps the provider package's `DocFile` into a `Document`.

## Functions

**`::documents`**

| Function | Returns |
|---|---|
| `list(source, folder?, opts?)` | `Page` `{documents, cursor, has-more}`: one page of a folder's direct children; pass `{cursor}` for the next |
| `list-all(source, folder?, limit?, opts?)` | Vec of `Document`: every page, or at most `limit` |
| `search(source, query, opts?)` | Vec of `Document`: the provider's native search, at most `opts.limit` (default 50) |
| `get(source, id, opts?)` | `Document` (also `get-document`) |
| `download(source, id, opts?)` | `FileContent` `{document-id, name, mime-type, size, bytes, text, method, document}` |
| `read-text(source, id, format?, opts?)` | `TextResult` (see below) |
| `upload(source, parent?, name, bytes, opts?)` | `Document`; chunked or resumable uploads are chosen by the provider package; `opts.mime-type` |
| `start-cursor(source, opts?)` | a change cursor for "from now on" |
| `changes(source, cursor, opts?)` | `ChangeSet` `{documents, deleted-ids, deleted-paths, next-cursor, reset-required, has-more}` |
| `capabilities(provider?)` | the capability Map |
| `normalize-webhook(provider, request, secret?)` | `WebhookEvent` |

`folder` is a folder id (Drive, OneDrive, Box), a path (Dropbox, OneDrive `/path`) or a key prefix ending in `/` (S3); null means the source's root.

**Search differences.** Drive searches names, descriptions and indexed content (`fullText contains`), not trashed, within `drive-id` when set. OneDrive and SharePoint use Graph `search(q=...)` over names, metadata and content from the drive root. Dropbox `search_v2` covers names, and content on plans with content search, under `root`; its index lags a few minutes and stops at 10,000 matches. Box searches names, descriptions, content and tags under `folder-id` and allows 6 searches per second per user. S3 has no search: `search` returns an Err, so list a prefix instead.

## Reading text

`read-text` never waits. Check `status`: `"ok"` (`text`, or `bytes` for PDF), `"pending"` (Box is still extracting: call again later from a schedule or a run `retry`), or `"unsupported"` (no native text: route `mime-type` to an extraction tool). `format` on the result says what you got.

| Provider | `"auto"` / `"text"` / `"markdown"` | `"pdf"` |
|---|---|---|
| Google Drive | Docs export as Markdown (`text`: plain text), Sheets as CSV (first sheet), Slides as text, Drawings as SVG, Apps Script as JSON; text files download | native documents export to PDF; stored PDFs download |
| OneDrive / SharePoint | text files download; Loop and Whiteboard convert to HTML (up to 2 MB); Office files are `unsupported` | Office, ODF, RTF, HTML, Markdown, EML/MSG, TIFF and Loop files up to 2 MB convert; stored PDFs download |
| Dropbox | Paper exports as Markdown (`text`: `plain_text` when offered); text files download | stored PDFs download |
| Box | text files download; everything else uses Box's `extracted_text` (may be `pending`) | stored PDFs download |
| Amazon S3 | text files (`text/*`, JSON, CSV, Markdown, ... by type or extension) download | stored PDFs download |

`::documents::text/plan(document, format)` shows the decision without making a request.

**Extraction for the rest.** An `unsupported` result carries the file's `mime-type`, so a workflow can send the bytes from `download` to a Hot Box package: [`pandoc`](../pandoc) for DOCX, ODT, HTML and EPUB to Markdown, [`libreoffice`](../libreoffice) for legacy Office formats and PDF conversion, and [`tesseract`](../tesseract) for OCR of scans and images.

```hot
::docs ::documents

route-text fn (src: Map, id: Str): Any {
  r ::docs/read-text(src, id, "auto")
  cond {
    is-err(r) => { r }
    eq(r.status, "ok") => { r.text }
    eq(r.status, "pending") => { send("docs:retry-later", {id: id}) }
    => { send("docs:extract", {id: id, mime-type: r.mime-type}) }
  }
}
```

## Recipe: RAG ingestion

Take a change cursor first (so nothing changed during the backfill is missed), backfill the tree, then keep the index current from `changes`. Key entries by provider and id.

```hot
::docs ::documents
::store ::hot::store

SOURCE {provider: "dropbox", root: "/Knowledge"}

kb ::store/Map({name: "docs-kb", embedding: ::store/Embedding({field: "content", text-search: true})})
state ::store/Map({name: "docs-sync"})

index-document fn (doc: Map): Bool {
  r ::docs/read-text(SOURCE, doc.id, "auto")
  cond {
    is-err(r) => { false }
    eq(r.status, "ok") => {
      ::store/put(kb, `${doc.provider}:${doc.id}`, {content: r.text, name: doc.name, path: doc.path, url: doc.web-url, version: doc.version})
      true
    }
    => {
      send("docs:extract", {id: doc.id, status: r.status, mime-type: r.mime-type})
      false
    }
  }
}

backfill fn (folder: Str?): Int {
  docs ::docs/list-all(SOURCE, folder, 5000)
  files filter(docs, (d) { and(eq(d.kind, "file"), not(d.deleted)) })
  folders filter(docs, (d) { eq(d.kind, "folder") })
  here length(filter(map(files, index-document), (ok) { ok }))
  reduce(map(folders, (f) { backfill(f.path) }), (total, n) { add(total, n) }, here)
}

start-ingestion fn (): Int {
  ::store/put(state, "cursor", ::docs/start-cursor(SOURCE))
  backfill(null)
}

answer fn (question: Str): Vec {
  ::store/search(kb, question, {limit: 5})
}
```

Dropbox folders are listed by `path`; for other providers pass `f.id`. Skip unchanged documents by comparing `doc.version` with what you stored.

## Recipe: schedule-driven sync

```hot
::docs ::documents
::store ::hot::store

SOURCE {provider: "box", folder-id: "12345"}

kb ::store/Map({name: "docs-kb"})
state ::store/Map({name: "docs-sync"})

apply-changes fn (set: Map): Int {
  live filter(set.documents, (d) { and(eq(d.kind, "file"), not(d.deleted)) })
  map(live, (d) {
    r ::docs/read-text(SOURCE, d.id, "auto")
    if(and(not(is-err(r)), eq(r.status, "ok")), ::store/put(kb, `box:${d.id}`, {content: r.text, name: d.name, version: d.version}), null)
  })
  map(set.deleted-ids, (id) { ::store/delete(kb, `box:${id}`) })
  length(live)
}

docs-sync
meta {schedule: "every 15 minutes"}
fn (event) {
  cursor or(::store/get(state, "cursor"), ::docs/start-cursor(SOURCE))
  set ::docs/changes(SOURCE, cursor)
  apply-changes(set)
  ::store/put(state, "cursor", set.next-cursor)
  // Box stopped at max-pages: there is more right now
  if(set.has-more, send("docs:sync-again", {}), null)
}
```

When `set.reset-required` is true (an OneDrive `410` resync or a Dropbox cursor reset), `documents` is a full listing: replace the index with it (delete keys not in it) rather than merging. Store `next-cursor` only after the changes are applied, so a failed run repeats them; handlers should be idempotent.

## Webhooks

`normalize-webhook` (`::documents::webhooks/normalize`) verifies a notification with the provider package's verifier and turns it into "something changed in this source": `{provider, kind, changed, resync, reauthorize, hint, response}`. Notifications carry no file data, so react by calling `changes` with your stored cursor, and return `event.response` (Graph wants `202`, the others `200`). Handshakes (Graph `validationToken`, Dropbox `challenge`) come back as `kind: "handshake"` with the reply to send.

| Provider | Verifier | Secret (2-arity reads it from context; 3-arity takes it) |
|---|---|---|
| Google Drive | channel token, constant time | `google.drive.channel.token` |
| OneDrive / SharePoint | `clientState` on every notification, constant time | `microsoft.graph.client-state` |
| Dropbox | `X-Dropbox-Signature`, HMAC-SHA256 of the raw body | `dropbox.app.secret` |
| Box | primary or secondary HMAC-SHA256 of body + timestamp, 10-minute window | `box.webhook.primary-key`, `box.webhook.secondary-key` (3-arity: `{primary, secondary}`) |

The 2-arity form fails the run when the secret is unset, so an unconfigured endpoint never accepts traffic. S3 has no webhooks of its own: route S3 Event Notifications through SQS, SNS or EventBridge (`hot.dev/aws-sqs`, `hot.dev/aws-sns`).

```hot
::docs ::documents
::store ::hot::store

SOURCE {provider: "onedrive", drive: "b!abc123"}
state ::store/Map({name: "docs-sync"})

onedrive-hook
meta {webhook: {service: "docs", path: "/docs/onedrive"}}
fn (request) {
  event ::docs/normalize-webhook("onedrive", request)
  cond {
    is-err(event) => { ::hot::http/HttpResponse({status: 401, body: "invalid clientState"}) }
    => {
      if(event.changed, send("docs:changed", {provider: event.provider, resync: event.resync}), null)
      event.response
    }
  }
}

on-docs-changed
meta {on-event: "docs:changed"}
fn (event) {
  cursor or(::store/get(state, "cursor"), ::docs/start-cursor(SOURCE))
  set ::docs/changes(SOURCE, cursor)
  ::store/put(state, "cursor", set.next-cursor)
  length(set.documents)
}
```

Creating and renewing the subscriptions stays with the provider packages: `::google::drive::changes/watch-changes` and `renew-watch` (7-day channels), `::microsoft::onedrive::delta/subscribe` and `renew-subscription` (under 30 days), the Dropbox App Console webhook, and `::box::webhooks/create-webhook`.

## Capabilities

`capabilities(provider)` returns plain data so an agent can plan before calling (`::documents::capabilities/supports("box", "extracted-text")` for a yes/no):

| | Google Drive | OneDrive / SharePoint | Dropbox | Box | S3 |
|---|---|---|---|---|---|
| Native export | Docs, Sheets, Slides, Drawings, Apps Script | | Paper (Markdown, HTML) | | |
| Conversion | | PDF, HTML (sources up to 2 MB) | | | |
| Extracted text | | | | yes (may be pending) | |
| Search | full text | full text | names (+ content on paid plans) | full text | no |
| Change feed | `changes` page token | `delta` link | `list_folder` cursor | event stream | watermark listing |
| Deletions in changes | yes | yes | yes (by path) | yes | no |
| Push notifications | watch channels (7 days) | subscriptions (under 30 days) | app webhook | webhooks v2 | via S3 Event Notifications only |
| Path addressing | no | yes | yes | no | yes (keys) |
| Single-request upload up to | 5 MiB (then resumable) | 4 MiB (then upload session) | 32 MiB (then upload session) | 20 MB (then chunked) | 5 GB (single PUT only) |
| Hashes | `md5`, `sha256` | `quickXorHash`, `sha1`, `crc32` | `dropbox-content-hash` | `sha1` | none |

## Errors and pagination

Errors are the provider packages' own (`::google::api/HttpError`, `::microsoft::graph::api/HttpError`, `::dropbox::api/HttpError`, `::box::api/HttpError`) with a readable `message`; S3 errors are `{status, code, message, request-id}` (`documents: s3 HTTP 404 NoSuchKey: ...`). Nothing is retried with backoff inside a run: use each provider's `is-retryable` and give the handler, schedule or task a `retry` policy (see https://hot.dev/docs/retries). Bad sources, formats and cursors are Errs before any request is made.

Page and change cursors are opaque Strs (`doc1.` + base64url JSON of the provider token). They hold no credentials, but Graph links and Dropbox cursors name drives and folders, so store them like other metadata. A cursor from another provider or verb is an Err.

## Per-provider caveats

- **Google Drive.** `drive` and `drive.readonly` are restricted scopes: a public app needs Google's OAuth verification and an annual security assessment. `drive.file` (non-sensitive) only sees files your app created or the user picked. Exports are capped at 10 MB (larger ones fall back to `exportLinks`), Sheets export only the first sheet as CSV, and service accounts have no storage of their own (upload into a shared drive or impersonate a user). Drive has no paths: `Document.path` is null.
- **OneDrive and SharePoint.** Graph converts to PDF or HTML only for sources up to 2 MB, and Office files have no native text. App-only tokens have no `/me`: give the source a `drive`. On work/school drives and SharePoint, delta runs from the drive root only, so `changes` covers the whole drive. `recent` and `sharedWithMe` are deprecated by Microsoft and not used here.
- **Dropbox.** Deletions are reported by path only, so `deleted-ids` holds lower-cased paths; a deleted folder removes everything below it. Dropbox records no creation time. `files/list_folder/longpoll` (in the dropbox package) holds the run open for up to its timeout; webhooks or a schedule scale better. Content search needs a plan that includes it.
- **Box.** OAuth refresh tokens are single-use: every refresh returns a new pair, which the box package's keeper stores (use `opts.account`, never a bare refresh token per call). Extracted text is generated on demand and can be `pending`. The event stream is account-wide: with a `folder-id`, items under other folders are filtered out and items moved away are reported as deleted. Box keeps user events for about two weeks, so sync at least weekly.
- **Amazon S3.** `aws-s3` has no per-call credentials, folder listing, key encoding or `DocFile`, so `::documents::s3` builds its ListObjectsV2 / HEAD / GET / PUT requests itself and signs them with `aws-core`. `changes` lists the whole prefix every time and never sees deletions; S3 Event Notifications are the better route. Uploads are a single PUT (no multipart), bounded by the run's memory and the 5 GB PutObject limit.

## Notes and limits

- Every read holds the whole file in memory for the run; single responses are capped by `http.max-response-bytes` (16 MiB by default). Drive, OneDrive, Dropbox and Box downloads switch to Range chunks past it; exports, conversions and S3 GETs cannot.
- `list` returns direct children only. Walk folders yourself (see the RAG recipe), or use `changes` for "everything under the source".
- Sales contracts: Google domain-wide delegation needs Google Workspace; SharePoint needs Microsoft 365 for business; Dropbox team spaces need a team plan; Box admin event logs need a paid enterprise and an admin token, and `enterprise_content` search must be enabled by Box support (user events, used here, work on every plan). Personal accounts work for everything else, and S3 is pay as you go.

## Data handling

This package stores nothing and writes no context. Credentials stay in the provider packages (secret context variables, or per-call `opts`); they are never put in a `Document`, a cursor or an error message. Document content and extracted text exist only in the run's memory unless you store them. Mark webhook signature headers as `secret-headers` on your handlers (`x-goog-channel-token`, `x-dropbox-signature`).

## Integration tests

`hot test -p pkg-integration-documents` runs one read-only smoke test per provider: list one page of the source root and `get` the first item. Each provider uses its own package's variables: `GOOGLE_ACCESS_TOKEN` or `GOOGLE_SERVICE_ACCOUNT` (+ `GOOGLE_IMPERSONATE`); `MS_ACCESS_TOKEN`, or `MS_CLIENT_ID` + `MS_REFRESH_TOKEN`, or `MS_CLIENT_ID` + `MS_CLIENT_SECRET` + `MS_TENANT_ID` + `MS_TEST_DRIVE_ID`; `DROPBOX_ACCESS_TOKEN` or `DROPBOX_REFRESH_TOKEN` + `DROPBOX_APP_KEY` (+ `DROPBOX_TEST_PATH`); `BOX_ACCESS_TOKEN` or `BOX_CLIENT_ID` + `BOX_CLIENT_SECRET` + `BOX_ENTERPRISE_ID` / `BOX_USER_ID` (+ `BOX_TEST_FOLDER_ID`); `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` + `AWS_S3_TEST_BUCKET` (+ `AWS_REGION`, `AWS_SESSION_TOKEN`, `AWS_S3_TEST_PREFIX`). A provider without credentials records a skip.

## License

Apache-2.0 - see [LICENSE](LICENSE)
