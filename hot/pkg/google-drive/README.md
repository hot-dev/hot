# google-drive

Google Drive API bindings for Hot: search, upload, download, export, and manage files and folders. Auth via [`google-core`](../google-core).

```hot
::drive ::google::drive

folder ::drive/create-folder("Reports")
file ::drive/upload-file("report.csv", csv-text, "text/csv", folder.id)
page ::drive/list-files("name contains 'report' and trashed = false")
pdf ::drive/export-file(doc-id, "application/pdf")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
