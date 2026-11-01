# google-sheets

Google Sheets API v4 bindings for Hot, built for data workflows and agents: read and write cell values (single range, batch, append, clear) with every render and input option, treat a sheet as a table of records, create spreadsheets with tabs, and run `batchUpdate` with builders for adding, renaming and deleting tabs, freezing rows, resizing columns and basic formatting. Credentials, scopes and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-sheets": "0.1.0"
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
| `google.sheets.api-root` | Optional API root override for a proxy or emulator (default `https://sheets.googleapis.com`) |

| Scope | Classification | Enough for |
|---|---|---|
| `spreadsheets.readonly` | Sensitive | Reading every spreadsheet the user can see (the default for reads) |
| `spreadsheets` | Sensitive | Reading and writing every spreadsheet the user can see (the default for writes and `create-spreadsheet`) |
| `drive.file` | Non-sensitive | Everything, but only on spreadsheets your app created or the user opened with it (e.g. via the Google Picker) |

`drive.file` is the least-privilege choice for "create a report sheet" or "process the sheet the user picked" apps, and avoids Google's sensitive-scope verification. With access and refresh tokens the user's granted scopes apply; the defaults matter for service accounts, where `google.scopes` (or `opts.scopes`) must match what you authorised.

A service account can edit spreadsheets shared with its email address. Spreadsheets it creates are owned by it (share them with `google-drive`'s permissions functions before anyone else can open them), unless you impersonate a user with domain-wide delegation.

Every function has an options arity. Sheets parameters use the API's own names (`valueRenderOption`, `valueInputOption`, `insertDataOption`, `fields`, ...); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`.

## Quick start

```hot
::values ::google::sheets::values
::books ::google::sheets::spreadsheets

id "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdef"

// A tab with a header row, as records
leads ::values/read-records(id, "Leads!A:D")
fresh filter(leads, (l) { eq(l.status, "new") })

// Append rows in the header's column order
::values/append-records(id, "Leads!A:D", [{name: "Ada", email: "ada@example.com", status: "new"}], ["name", "email", "status", "owner"])

// Raw grids, with options
totals ::values/get-values(id, "Summary!A1:B10", {valueRenderOption: "UNFORMATTED_VALUE"})
::values/update-values(id, "Summary!B2", [["=SUM(Data!C:C)"]])

// A new spreadsheet with two tabs and a formatted header
book ::books/create-spreadsheet("Q3 pipeline", {sheets: ["Leads", "Summary"]})
::values/write-records(book.spreadsheetId, "Leads!A1", [{name: "Ada", stage: "demo"}])
::books/format-header(book.spreadsheetId, ::books/sheet-id(book.spreadsheetId, "Leads"))
```

## Functions by area

**`::google::sheets::values`**: `get-values`, `batch-get-values`, `update-values`, `batch-update-values`, `append-values`, `clear-values`, `batch-clear-values`, `read-records`, `write-records`, `append-records`, plus the pure `read-params`, `write-params`, `value-range`, `batch-update-body`.

**`::google::sheets::spreadsheets`**: `get-spreadsheet`, `create-spreadsheet`, `batch-update`, `list-sheets`, `find-sheet`, `sheet-id`, `add-sheet`, `rename-sheet`, `delete-sheet`, `duplicate-sheet`, `freeze-rows`, `auto-resize-columns`, `format-header`, plus the pure `get-params`, `build-spreadsheet`, `batch-update-body`.

**`::google::sheets::requests`** (pure `batchUpdate` builders): `add-sheet`, `rename-sheet`, `delete-sheet`, `duplicate-sheet`, `freeze`, `auto-resize`, `set-size`, `insert-dimension`, `delete-dimension`, `append-dimension`, `repeat-format`, `format-fields`, `bold`, `background`, `number-format`, `header-row`, `grid-range`, `dimension-range`, `rgb`, `hex-color`.

**`::google::sheets::records`** (pure): `rows-to-records`, `records-to-rows`, `records-to-grid`, `record-columns`, `cell-value`, `column-letter`, `quote-sheet`, `a1`, `grid-a1`.

The most used functions are also aliased at the top level (`::google::sheets/get-values`, `batch-get-values`, `update-values`, `append-values`, `clear-values`, `read-records`, `append-records`, `get-spreadsheet`, `create-spreadsheet`, `batch-update`).

## Values and options

Ranges are A1 notation: `"Sheet1!A1:C10"`, `"Leads!A:D"` (whole columns), `"Leads"` (a whole tab), or a named range. Quote tab names with spaces or punctuation (`::google::sheets::records/a1("Q3 Plan", "A:C")` gives `'Q3 Plan'!A:C`); ids and ranges are URL-encoded for you.

| Option | Calls | Values |
|---|---|---|
| `valueRenderOption` | reads | `FORMATTED_VALUE` (default: what the cell shows), `UNFORMATTED_VALUE` (numbers as numbers), `FORMULA` |
| `dateTimeRenderOption` | reads | `SERIAL_NUMBER` (default), `FORMATTED_STRING`; ignored with `FORMATTED_VALUE` |
| `majorDimension` | reads and writes | `ROWS` (default), `COLUMNS` |
| `valueInputOption` | writes | `USER_ENTERED` (the default here: parsed as if typed, so formulas and dates work), `RAW` (stored as is) |
| `insertDataOption` | `append-values` | `OVERWRITE` (default: fill empty rows below the table), `INSERT_ROWS` |
| `includeValuesInResponse`, `responseValueRenderOption`, `responseDateTimeRenderOption` | writes | Return the written values |
| `ranges`, `includeGridData`, `fields` | `get-spreadsheet` | Cell data and formatting for ranges; a field mask such as `"properties.title,sheets.properties"` |

Unset options are never sent. Responses drop trailing empty cells and rows, so a row can be shorter than the header; `rows-to-records` fills missing cells with null. `records-to-rows` writes missing values as empty cells and Maps or Vecs as JSON text.

## Formatting and structure

`batch-update` applies a Vec of requests atomically, in order; `::google::sheets::requests` builds them. Requests address tabs by numeric `sheetId` (`sheet-id(id, "Leads")`), and ranges are 0-based and end exclusive:

```hot
::books ::google::sheets::spreadsheets
::req ::google::sheets::requests

id "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdef"
sheet ::books/sheet-id(id, "Leads")
header ::req/grid-range(sheet, 0, 1)
amounts ::req/grid-range(sheet, 1, null, 3, 4)

::books/batch-update(id, [
  ::req/freeze(sheet, 1),
  ::req/bold(header),
  ::req/background(header, ::req/hex-color("#e8f0fe")),
  ::req/number-format(amounts, "CURRENCY", "$#,##0.00"),
  ::req/auto-resize(sheet, "COLUMNS")
])
```

`repeat-format` takes any `CellFormat` (`textFormat`, `backgroundColor`, `horizontalAlignment`, `wrapStrategy`, `numberFormat`, ...) and sets the field mask to the keys you pass, so other formatting is left alone. Requests not covered by a builder can be passed as plain Maps in the API's shape.

## Notes and limits

- Sheets allows up to 10 million cells per spreadsheet; one response is capped by `http.max-response-bytes` (16 MiB by default), so read very large tabs in ranges.
- Quotas are per minute per project and per user (read and write requests are counted separately). On 429 or 5xx, `::google::api/is-retryable(result)` is true: let a handler `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)). `append-values` is not idempotent, so a blind retry can duplicate rows.
- `append-values` finds "the table" at the range by looking for the last row with data; leave no blank rows inside a table you append to.
- Deleting a whole spreadsheet is a Drive operation: `::google::drive::files/trash-file(spreadsheet-id)` in [`google-drive`](../google-drive).
- Errors are `::google::api/HttpError` with Google's `status` and `message` (e.g. `INVALID_ARGUMENT`: `Unable to parse range: Sheet9!A1`).

## License

Apache-2.0 - see [LICENSE](LICENSE)
