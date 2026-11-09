# microsoft-excel

Excel workbook bindings for Hot over Microsoft Graph, built for data workflows and agents: read and write cells by A1 address, treat a worksheet or table as records, manage worksheets and tables, run several calls in one workbook session, and recalculate. Works on `.xlsx` files in OneDrive and SharePoint. Credentials, clouds and errors come from [`microsoft-graph`](../microsoft-graph), and drives and items are addressed as in [`microsoft-onedrive`](../microsoft-onedrive).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/microsoft-excel": "0.1.1"
}
```

## Auth and permissions

Microsoft documents the workbook API for **delegated work or school tokens**. Application (app-only) tokens are not supported, so a client secret or certificate credential won't work here. Personal Microsoft accounts have partial support at best, and sessions are not available to them.

| Context Variable | Description |
|---|---|
| `microsoft.oauth.refresh-token` + `microsoft.client.id` | A user's delegated refresh token (see [microsoft-graph](../microsoft-graph/README.md#authentication)) |
| `microsoft.access.token` | A ready delegated access token |
| `microsoft.onedrive.drive-id` | Default drive for every call; `opts.drive` overrides it |

The token needs `Files.ReadWrite`, even for reads. Credentials can also be passed per call in `opts`, which is how one app acts for many users.

## Addressing

- **Workbook:** an id, a path from the drive root (`"/Reports/Q3.xlsx"`), or a driveItem Map. `opts.drive` picks the drive: `"me"`, a drive id, or a base path from `microsoft-onedrive`.
- **Worksheet and table:** a name or an id.
- **Range:** an A1 address on a worksheet (`"A1:C10"`, `"A:D"`). `write-values` and `write-records` also take a single top-left cell and expand it to the data's size.

## Quick start

```hot
::ranges ::microsoft::excel::ranges
::tables ::microsoft::excel::tables
::sheets ::microsoft::excel::worksheets

book "/Reports/Q3.xlsx"

// A worksheet with a header row, as records
leads ::ranges/read-records(book, "Leads")

// Raw grids
totals ::ranges/read-values(book, "Summary", "A1:B10")
::ranges/write-values(book, "Summary", "B2", [["=SUM(Data!C:C)"]])

// A new worksheet with a table
::sheets/add-worksheet(book, "Pipeline")
::ranges/write-records(book, "Pipeline", "A1", [{name: "Ada", stage: "demo"}], ["name", "stage"])
table ::tables/create-table(book, "Pipeline", "A1:B2")
::tables/rename-table(book, table.id, "PipelineTable")

// Append and read back by column name
::tables/append-records(book, "PipelineTable", [{name: "Grace", stage: "trial"}])
::tables/read-table-records(book, "PipelineTable")
```

## Sessions

A session makes a run of calls faster and consistent. `with-session` opens one, passes it to your function in `opts`, and closes it afterwards:

```hot
::excel ::microsoft::excel
::ranges ::microsoft::excel::ranges

book "/Reports/Q3.xlsx"

::excel/with-session(book, (s) {
  ::ranges/write-values(book, "Inputs", "B1", [[1250]], s)
  ::ranges/read-values(book, "Summary", "A1:B5", s)
})
```

With `{persist: false}`, changes are discarded when the session ends, which suits what-if calculations: `with-session(book, {persist: false}, f)`. Excel expires idle sessions after a few minutes, and `refresh-session` keeps one alive.

## Functions by area

**`::microsoft::excel::workbooks`**: `with-session`, `create-session`, `close-session`, `refresh-session`, `calculate`, plus the pure `calculation-body`.

**`::microsoft::excel::worksheets`**: `list-worksheets`, `get-worksheet`, `add-worksheet`, `update-worksheet`, `rename-worksheet`, `delete-worksheet`.

**`::microsoft::excel::ranges`**: `get-range`, `read-values`, `read-formulas`, `read-formatted`, `used-range`, `update-range`, `write-values`, `write-formulas`, `clear-range`, `insert-cells`, `delete-cells`, `read-records`, `write-records`, plus the pure `used-range-path`, `clear-body`.

**`::microsoft::excel::tables`**: `list-tables`, `get-table`, `create-table`, `update-table`, `rename-table`, `delete-table`, `convert-to-range`, `column-names`, `list-rows`, `add-rows`, `delete-row`, `read-table-records`, `append-records`, plus the pure `create-table-body`, `add-rows-body`, `row-path`.

**`::microsoft::excel::records`** (pure): `rows-to-records`, `records-to-rows`, `records-to-grid`, `record-columns`, `cell-value`, `rectangular`, `column-letter`, `column-index`, `is-cell`, `expand-address`, `quote-sheet`, `a1`.

The most used functions are also aliased at the top level (`::microsoft::excel/read-values`, `write-values`, `read-records`, `write-records`, `list-worksheets`, `list-tables`, `read-table-records`, `append-records`, `with-session`).

## Values

| When | Value | Meaning |
|---|---|---|
| Reading | `""` | A blank cell (null in records) |
| Reading | a number | Dates are Excel serial numbers. `read-formatted` gives what the cell shows |
| Writing | `""` | Empties the cell |
| Writing | null | Leaves the cell unchanged |
| Writing | a string starting with `=` | A formula |

A write must match its range's size exactly. `write-values` handles that when you give a single top-left cell.

## Notes and limits

- Use a table to append. `add-rows` and `append-records` find the end for you. Neither is idempotent, so a blind retry can duplicate rows.
- Send many rows in one `add-rows` call. Adding them one at a time is slow.
- Very large ranges can fail or time out. Read them in smaller blocks, and pass `select` on `get-range` and `used-range` to trim responses.
- On 429 or 5xx, `::microsoft::graph::api/is-retryable(result)` is true: let a handler `retry` policy reschedule. Microsoft notes that adding rows can occasionally return a 504 that should be retried.
- Uploading, copying and deleting the workbook file are Drive operations: see [`microsoft-onedrive`](../microsoft-onedrive).
- Errors are `::microsoft::graph::api/HttpError` with Graph's `code` and `message`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
