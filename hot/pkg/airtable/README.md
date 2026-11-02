# airtable

Airtable bindings for Hot: records CRUD with `filterByFormula`, batch upsert and delete, record comments, plus the metadata API (`whoami`, `list-bases`, `get-base-schema`) so agents can read a base's shape before writing.

## Setup

Context variable `airtable.token`: a personal access token (`pat...`) with the scopes and base access it needs — `data.records:read/write`, `data.recordComments:read/write`, and `schema.bases:read` cover this package.

## Per-call credentials

Every function that calls Airtable takes an optional trailing `opts` Map, so one run can use several tokens. `token` replaces `airtable.token` for that call only. Nothing is written to ctx, so parallel flows can't see each other's tokens. A null or blank `token` is an error, never a fallback to the context token. Where a function has an optional `params` Map (`list-records`, `list-comments`), `opts` comes after it, so pass `{}` when there are no params.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::at ::airtable

leads-for-tenant fn (tenant: Map): Map {
  ::at/list-records(tenant.base-id, "Leads", {}, {token: tenant.airtable-token})
}
```

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
