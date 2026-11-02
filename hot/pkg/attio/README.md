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

## Per-call credentials

Every function that calls Attio takes an optional trailing `opts` Map, so one run can act for many workspaces and parallel flows don't share credentials. Nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The access token for this call, instead of `attio.api.key` |
| `base-url` | Another API root, such as a proxy. Needs an `api-key` in the same `opts`; `attio.api.key` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "tenant-b-token"}
me ::attio::workspace/get-self(tenant)
hits ::attio::records/query-records("companies", {limit: 10}, tenant)
note ::attio::notes/list-notes("people", "rec-123", tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `attio.api.key`. `::attio::api/resolve(opts)` is the one place this is decided.

## License

Apache-2.0 - see [LICENSE](LICENSE)
