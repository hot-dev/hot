# intercom

Intercom bindings: contacts (create/search/update) and conversations (search with Intercom's query DSL, reply as an admin, internal notes, close). Context variables: `intercom.token`, `intercom.admin.id`.

```hot
open ::intercom/search-conversations({field: "open", operator: "=", value: true})
::intercom/add-note(conversation-id, "Enterprise customer — escalate.")
::intercom/reply(conversation-id, "A fix ships this week!")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
