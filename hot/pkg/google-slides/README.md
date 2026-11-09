# google-slides

Google Slides API v1 bindings for Hot, built for reporting workflows and agents: read a presentation as slide text and speaker notes, create decks and text slides, fill `{{placeholder}}` templates, fetch page thumbnails, and run `batchUpdate` with builders for slides, shapes, images, tables, text and styles. Credentials, scopes and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-slides": "0.1.1"
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
| `google.slides.api-root` | Optional API root override for a proxy or emulator (default `https://slides.googleapis.com`) |

| Scope | Classification | Enough for |
|---|---|---|
| `presentations.readonly` | Sensitive | Reading every presentation the user can see (the default for reads) |
| `presentations` | Sensitive | Reading and writing every presentation the user can see (the default for writes and `create-presentation`) |
| `drive.file` | Non-sensitive | Everything, but only on presentations your app created or the user opened with it |

A service account can edit presentations shared with its email address. Presentations it creates are owned by it, so share them with `google-drive`'s permissions functions before anyone else can open them, or impersonate a user with domain-wide delegation.

Every function has an options arity. Slides parameters use the API's own names (`fields`, `writeControl`); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`. See [per-call credentials](../google-core/README.md#per-call-credentials-multi-tenant).

## Quick start

```hot
::decks ::google::slides::presentations
::req ::google::slides::requests

// Every slide's id, title, text and speaker notes
slides ::decks/outline("1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdef")

// A new deck with a text slide
deck ::decks/create-presentation("Q3 review")
id deck.presentationId
added ::decks/add-text-slide(id, "Highlights", "Revenue up 12%\nChurn down 2%")

// Anything else goes through batch-update
box ::req/new-object-id()
::decks/batch-update(id, [
  ::req/create-text-box(box, added.slide, {x: 40, y: 340, width: 400, height: 40}),
  ::req/insert-text(box, "Source: finance"),
  ::req/font-size(box, 12),
  ::req/bullets(added.body)
])
```

## Templates

Copy a template with `google-drive`, then fill the copy. Every `{{key}}` is replaced with its value, and `replace-shapes-with-image` swaps a placeholder shape for an image:

```hot
::decks ::google::slides::presentations
::req ::google::slides::requests

copy ::google::drive::files/copy-file("1TemplateId", {name: "Acme QBR"})
::decks/fill-template(copy.id, {customer: "Acme", arr: "$120k"})
::decks/batch-update(copy.id, [::req/replace-shapes-with-image("{{logo}}", "https://example.com/acme.png")])
```

## Functions by area

**`::google::slides::presentations`**: `get-presentation`, `create-presentation`, `batch-update`, `get-page`, `get-thumbnail`, `outline`, `add-text-slide`, `replace-text`, `fill-template`, plus the pure `build-presentation`, `batch-update-body`, `thumbnail-params`, `text-slide-requests`.

**`::google::slides::requests`** (pure `batchUpdate` builders): `create-slide`, `placeholder`, `create-shape`, `create-text-box`, `create-image`, `create-table`, `delete-object`, `duplicate-object`, `move-slides`, `insert-text`, `delete-text`, `replace-all-text`, `replace-shapes-with-image`, `fill-template`, `text-style`, `bold`, `font-size`, `text-color`, `paragraph-style`, `bullets`, `background`, `new-object-id`, `points`, `element-properties`, `rgb`, `hex-color`, `optional-color`, `field-mask`, `text-range`, `cell-location`.

**`::google::slides::content`** (pure): `outline`, `slide-title`, `slide-text`, `speaker-notes`, `element-text`, `text-content`, `find-placeholder`, `find-element`.

The most used functions are also aliased at the top level (`::google::slides/get-presentation`, `create-presentation`, `batch-update`, `outline`, `add-text-slide`, `replace-text`, `fill-template`).

## Layout

Positions and sizes are `frame`s in points: `{x, y, width, height}`. A default 16:9 slide is 720 by 405 points. Text requests take `opts.rowIndex` and `opts.columnIndex` to address a table cell, and `opts.textRange` (from `text-range`) to address part of the text.

Object ids you choose must be 5 to 50 characters. `new-object-id` makes a fresh one, and a later request in the same batch can use it.

## Notes and limits

- `get-thumbnail` returns a short-lived `contentUrl` and counts against the expensive-read quota.
- Images are fetched from a public URL once, and must be PNG, JPEG or GIF under 50 MB and at most 25 megapixels.
- On 429 or 5xx, `::google::api/is-retryable(result)` is true: let a handler `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)). `add-text-slide` is not idempotent, so a blind retry can add a second slide.
- `{writeControl: {requiredRevisionId: deck.revisionId}}` makes a `batch-update` fail if the presentation changed since you read it.
- Exporting (PDF, PowerPoint), copying and deleting are Drive operations: see [`google-drive`](../google-drive).
- Errors are `::google::api/HttpError` with Google's `status` and `message`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
