# google-gmail

Gmail API bindings for Hot: send email, search and read messages, manage labels. Auth via [`google-core`](../google-core).

```hot
::gmail ::google::gmail

::gmail/send-message("alice@example.com", "Weekly report", "All green.")
inbox ::gmail/list-messages("is:unread newer_than:7d")
message ::gmail/get-message(inbox.messages[0].id)
::gmail/modify-labels(message.id, ["STARRED"], ["UNREAD"])
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
