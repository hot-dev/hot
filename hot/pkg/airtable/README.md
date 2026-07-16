# airtable

Airtable bindings for Hot: records CRUD with `filterByFormula`, batch upsert and delete, record comments, plus the metadata API (`whoami`, `list-bases`, `get-base-schema`) so agents can read a base's shape before writing.

## Setup

Context variable `airtable.token`: a personal access token (`pat...`) with the scopes and base access it needs — `data.records:read/write`, `data.recordComments:read/write`, and `schema.bases:read` cover this package.

## Usage

```hot
::at ::airtable

rows ::at/list-records(base-id, "Leads", {
  filterByFormula: `{Status} = ${::at/escape-formula-value(status)}`,
  maxRecords: 20
})

::at/create-records(base-id, "Leads", [{fields: {Name: "Ada", Score: 99}}])

// Create-or-update keyed on a unique field (Airtable's performUpsert)
::at/upsert-records(base-id, "Leads", ["Email"], [
  {fields: {Email: "ada@example.com", Status: "Customer"}}
])

// Comments on a record
c ::at/create-comment(base-id, "Leads", record-id, "Signed the order form.")
::at/update-comment(base-id, "Leads", record-id, c.id, "Signed! PO attached in email.")

// Read the schema before writing to an unfamiliar base
schema ::at/get-base-schema(base-id)

::at/delete-records(base-id, "Leads", [rec-a, rec-b])
```

Always build `filterByFormula` values with `escape-formula-value` — unescaped interpolation lets crafted input rewrite the formula.

## License

Apache-2.0 - see [LICENSE](LICENSE)
