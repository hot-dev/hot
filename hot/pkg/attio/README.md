# attio

Attio CRM bindings for Hot: records (query, create, and `assert` upserts by matching attribute), notes, tasks, and lists on Attio's v2 API.

## Setup

Context variable `attio.api.key` (Workspace settings → Developers).

## Usage

```hot
::records ::attio::records
::notes ::attio::notes
::lists ::attio::lists

person ::records/assert-record("people", "email_addresses", {
  email_addresses: [{email_address: "ada@example.com"}],
  name: [{first_name: "Ada", last_name: "Lovelace", full_name: "Ada Lovelace"}]
})

::notes/create-note("people", person.id.record_id, "Intro call", "Wants the enterprise tier.")

::lists/add-entry("sales-pipeline", {
  parent_object: "people",
  parent_record_id: person.id.record_id
})

hits ::records/query-records("companies", {
  filter: {domains: {domain: {"$contains": "example.com"}}}
})
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
