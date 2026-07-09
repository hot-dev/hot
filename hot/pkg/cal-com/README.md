# cal-com

Cal.com bindings: event types, availability slots, and bookings (create/cancel/reschedule) — scheduling for sales and assistant agents. Context variable: `cal-com.api.key` (`cal-com.url` for self-hosted).

```hot
slots ::cal-com/get-slots(event-type-id, "2026-07-10", "2026-07-14", "America/Chicago")
::cal-com/create-booking({eventTypeId: id, start: slot, attendee: {name: "Ada", email: "ada@example.com", timeZone: "America/Chicago"}})
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
