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

## Per-call credentials

Every function that calls Notion takes an optional trailing `opts` Map, so one run can act for many workspaces and parallel flows don't share credentials. Nothing from `opts` is written to ctx.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

| Key | Effect |
|---|---|
| `token` | The integration secret or OAuth token for this call, instead of `notion.token` |
| `api-version` | `Notion-Version` for this call |
| `base-url` | Another API root, such as a proxy. Needs a `token` in the same `opts`; `notion.token` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options` or `children`, pass `{}` or `[]` for those.

```hot
tenant {token: "ntn_tenant_b"}
bot ::notion::users/me(tenant)
hits ::notion::blocks/search("roadmap", "page", tenant)
rows ::notion::databases/query-database("db-id", {}, tenant)
```

A `token` that is present but blank or null is an error naming the key, never a fallback to `notion.token`. `::notion::api/resolve(opts)` is the one place this is decided.

## License

Apache-2.0 - see [LICENSE](LICENSE)
