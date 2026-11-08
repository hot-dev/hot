# google-docs

Google Docs API v1 bindings for Hot, built for document workflows and agents: read a document as plain text, paragraphs, headings and tables across every tab, create documents, fill `{{placeholder}}` templates, and run `batchUpdate` with builders for text, styles, bullets, tables, images, named ranges, headers and footers. Credentials, scopes and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-docs": "0.1.0"
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
| `google.docs.api-root` | Optional API root override for a proxy or emulator (default `https://docs.googleapis.com`) |

| Scope | Classification | Enough for |
|---|---|---|
| `documents.readonly` | Sensitive | Reading every document the user can see (the default for reads) |
| `documents` | Sensitive | Reading and writing every document the user can see (the default for writes and `create-document`) |
| `drive.file` | Non-sensitive | Everything, but only on documents your app created or the user opened with it |

A service account can edit documents shared with its email address. Documents it creates are owned by it, so share them with `google-drive`'s permissions functions before anyone else can open them, or impersonate a user with domain-wide delegation.

Every function has an options arity. Docs parameters use the API's own names (`includeTabsContent`, `suggestionsViewMode`, `fields`, `writeControl`); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`. See [per-call credentials](../google-core/README.md#per-call-credentials-multi-tenant).

## Quick start

```hot
::docs ::google::docs::documents
::req ::google::docs::requests
::content ::google::docs::content

id "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdef"

// Plain text, for an LLM or a search index
text ::docs/read-text(id)

// Structure
doc ::docs/get-document(id, {includeTabsContent: true})
outline map(::content/headings(doc), %.text)
tables ::content/tables(doc)

// Simple edits
::docs/append-text(id, "Shipped the importer.\n")
::docs/replace-text(id, "Q3", "Q4")

// Several edits at once
::docs/batch-update(id, [
  ::req/insert-text("Status\nAll green\n", 1),
  ::req/heading(1, 8, 1),
  ::req/bold(8, 17)
])
```

## Templates

Copy a template with `google-drive`, then fill the copy. Every `{{key}}` is replaced with its value:

```hot
copy ::google::drive::files/copy-file("1TemplateId", {name: "Offer for Ada"})
::google::docs::documents/fill-template(copy.id, {name: "Ada", plan: "Pro", seats: 12})
```

## Functions by area

**`::google::docs::documents`**: `get-document`, `create-document`, `batch-update`, `read-text`, `append-text`, `insert-text`, `replace-text`, `fill-template`, plus the pure `get-params`, `batch-update-body`.

**`::google::docs::requests`** (pure `batchUpdate` builders): `insert-text`, `append-text`, `delete-range`, `replace-all-text`, `fill-template`, `text-style`, `bold`, `italic`, `link`, `font-size`, `text-color`, `paragraph-style`, `named-style`, `heading`, `align`, `bullets`, `delete-bullets`, `insert-table`, `append-table`, `insert-table-row`, `insert-table-column`, `delete-table-row`, `delete-table-column`, `insert-image`, `append-image`, `insert-page-break`, `append-page-break`, `create-named-range`, `delete-named-range`, `replace-named-range`, `create-header`, `create-footer`, `location`, `end-of-segment`, `text-range`, `table-cell`, `points`, `rgb`, `hex-color`, `optional-color`, `field-mask`.

**`::google::docs::content`** (pure): `document-text`, `tab-text`, `body-text`, `content-text`, `element-text`, `paragraph-text`, `paragraphs`, `headings`, `tables`, `all-tabs`, `find-tab`, `tab-id`, `tab-title`, `tab-body`, `end-index`.

The most used functions are also aliased at the top level (`::google::docs/get-document`, `create-document`, `batch-update`, `read-text`, `append-text`, `replace-text`, `fill-template`).

## Indexes

Docs addresses content by UTF-16 code unit, starting at 1. Ranges are start inclusive and end exclusive. `::google::docs::content/paragraphs` returns the `start` and `end` of each paragraph, ready for the builders.

`batch-update` applies requests in order, and each one shifts the indexes after it. When a batch has several insertions or deletions, apply the highest index first. Requests that need no index (`append-text`, `replace-all-text`, `fill-template`) avoid the problem.

Every builder's `opts` accepts `tabId`, and `segmentId` for a header, footer or footnote. `replace-text`, `fill-template` and named-range calls take `tabIds` instead, and cover every tab by default.

## Notes and limits

- `get-document` without `includeTabsContent` returns only the first tab under `body`. `read-text` and the `content` readers handle both shapes, and `read-text` always reads every tab unless you pass `tabId`.
- Google's write quota is per minute per project and per user. On 429 or 5xx, `::google::api/is-retryable(result)` is true: let a handler `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)). `append-text` is not idempotent, so a blind retry can duplicate text.
- `{writeControl: {requiredRevisionId: doc.revisionId}}` makes a `batch-update` fail if the document changed since you read it.
- Exporting (PDF, Word, Markdown), copying and deleting are Drive operations: see [`google-drive`](../google-drive).
- Errors are `::google::api/HttpError` with Google's `status` and `message`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
