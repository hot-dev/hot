# google-sheets

Google Sheets API bindings for Hot: read, write, append, and clear values; create spreadsheets. Auth via [`google-core`](../google-core).

```hot
::sheets ::google::sheets

rows ::sheets/get-values(id, "Sheet1!A1:C10")
::sheets/append-values(id, "Log!A:C", [["2026-07-09", "deploy", "ok"]])
::sheets/update-values(id, "Summary!B2", [[42]])
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
