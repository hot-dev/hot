# zendesk

Zendesk Support bindings: tickets (create/update/comment), search with Zendesk's query syntax, and users. Context variables: `zendesk.subdomain`, `zendesk.email`, `zendesk.api.token`.

```hot
urgent ::zendesk/search("type:ticket status:open priority:urgent")
::zendesk/add-comment(ticket-id, "Rolled back; monitoring.", false)  // internal note
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
