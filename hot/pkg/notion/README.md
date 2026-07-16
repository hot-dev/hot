# notion

Notion API bindings for Hot: pages, databases with query filters, blocks, comments, users, and search — plus rich-text and block builders so agents write readable Notion, not raw JSON.

## Setup

Context variable `notion.token`: an internal integration secret from [notion.so/my-integrations](https://notion.so/my-integrations), with target pages shared with the integration. Comments need the integration's comment capabilities; `::notion::users` needs a user-information capability (`users/me` works at any level).

## Usage

```hot
::pages ::notion::pages
::databases ::notion::databases
::blocks ::notion::blocks
::comments ::notion::comments

page ::pages/create-page(parent-id, "Meeting notes", [
  ::notion/heading(2, "Decisions"),
  ::notion/bullet("Ship the web-search package"),
  ::notion/to-do("Follow up on pinecone quota")
])

rows ::databases/query-database(db-id, {
  filter: {property: "Status", status: {equals: "In progress"}}
})

hits ::blocks/search("quarterly roadmap", "page")

// Comments: new discussion on a page, then a reply into it
comment ::comments/create-comment({page_id: page.id}, "Ready for review.")
::comments/create-comment({discussion_id: comment.discussion_id}, "Shipping today.")

// The token's own bot user — the cheapest token smoke test
bot ::notion::users/me()
```

Notion's API cannot delete or resolve comments — plan for them to persist.

## License

Apache-2.0 - see [LICENSE](LICENSE)
