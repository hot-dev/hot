# airtable

Airtable bindings: records CRUD with `filterByFormula`, plus the metadata API (`list-bases`, `get-base-schema`) so agents can read a base's shape before writing. Context variable: `airtable.token` (a PAT).

```hot
rows ::airtable/list-records(base-id, "Leads", {filterByFormula: "{Status} = 'New'"})
::airtable/create-records(base-id, "Leads", [{fields: {Name: "Ada", Score: 99}}])
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
