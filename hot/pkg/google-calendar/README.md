# google-calendar

Google Calendar API bindings for Hot: list calendars, and create, read, update, and delete events. Auth via [`google-core`](../google-core).

```hot
::cal ::google::calendar

events ::cal/list-events("primary", {timeMin: "2026-07-09T00:00:00Z", singleEvents: true})
::cal/quick-add("primary", "Lunch with Sam tomorrow at noon")
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
