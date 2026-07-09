# notion

Notion API bindings for Hot: pages, databases with query filters, blocks, and search — plus rich-text and block builders so agents write readable Notion, not raw JSON.

## Setup

Context variable `notion.token`: an internal integration secret from [notion.so/my-integrations](https://notion.so/my-integrations), with target pages shared with the integration.

## Usage

```hot
::pages ::notion::pages
::databases ::notion::databases
::blocks ::notion::blocks

page ::pages/create-page(parent-id, "Meeting notes", [
  ::notion/heading(2, "Decisions"),
  ::notion/bullet("Ship the web-search package"),
  ::notion/to-do("Follow up on pinecone quota")
])

rows ::databases/query-database(db-id, {
  filter: {property: "Status", status: {equals: "In progress"}}
})

hits ::blocks/search("quarterly roadmap", "page")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
