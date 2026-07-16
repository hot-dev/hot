# attio

Attio CRM bindings for Hot: records (query, create, `assert` upserts by matching attribute, update, delete) with object/attribute configuration, notes, tasks, comment threads, lists, and workspace members on Attio's v2 API.

## Setup

Context variable `attio.api.key` (Workspace settings → Developers → access token), with scopes matching the modules you use — Records, Object Configuration, Notes, Tasks, Comments, User Management.

## Usage

```hot
::records ::attio::records
::notes ::attio::notes
::tasks ::attio::tasks
::comments ::attio::comments
::workspace ::attio::workspace

person ::records/assert-record("people", "email_addresses", {
  email_addresses: [{email_address: "ada@example.com"}],
  name: [{first_name: "Ada", last_name: "Lovelace", full_name: "Ada Lovelace"}]
})

::notes/create-note("people", person.id.record_id, "Intro call", "Wants the enterprise tier.")

::tasks/create-task("Send the proposal", {
  deadline_at: "2026-08-01T12:00:00Z",
  linked_records: [{target_object: "people", target_record_id: person.id.record_id}]
})

// Comment threads need an author: a workspace member id
me first(::workspace/list-members().data)
::comments/create-comment(
  {object: "people", record_id: person.id.record_id},
  "Followed up by email.",
  me.id.workspace_member_id)

hits ::records/query-records("companies", {
  filter: {domains: {domain: {"$contains": "example.com"}}}
})

// Read an object's attribute slugs before writing records to it
attrs ::records/list-attributes("people")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
