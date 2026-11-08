# cal-com

Cal.com API v2 bindings for Hot: event types, availability schedules and slots, bookings through their whole lifecycle (paged and filtered lists, confirm, decline, cancel, reschedule, no-shows), the API key's own profile, and webhooks with `X-Cal-Signature-256` verification. It covers the scheduling half of a sales or assistant agent: find a lead, log it to the CRM, book the meeting. `::cal-com::api/request` covers any endpoint that isn't wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/cal-com": "0.1.1"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `cal-com.api.key` | yes, unless every call passes `api-key` | API key (`cal_live_...` / `cal_test_...`) from Settings → Developer → API keys, or a managed user's access token |
| `cal-com.url` | no | Base URL override for self-hosted Cal.com, e.g. `https://cal.example.com/api/v2`. Defaults to `https://api.cal.com/v2` |
| `cal-com.webhook.secret` | for webhooks | The secret you set on the webhook. Used by `::cal-com::webhooks/verify-request` |

Cal.com v2 versions each endpoint family separately with the `cal-api-version` header. Each call sends its family's version: event types `2024-06-14`, slots `2024-09-04`, bookings `2024-08-13`, schedules `2024-06-11`. Webhooks and `/me` take no version header. A self-hosted instance must be recent enough to serve those versions.

## Per-call credentials

Every function that calls Cal.com takes an optional trailing `opts` Map, so one run can act for many accounts and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and an `api-key` from `opts` is masked in call records.

| Key | Effect |
|---|---|
| `api-key` | The API key or managed user's access token for this call, instead of `cal-com.api.key` |
| `base-url` | Another API root, such as a self-hosted instance. Needs an `api-key` in the same `opts`; `cal-com.api.key` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
::bookings ::cal-com::bookings

tenant {api-key: "cal_live_tenant_b"}
page ::bookings/list-bookings({status: "upcoming", take: 50}, tenant)
booking ::bookings/get-booking("bk_123", tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `cal-com.api.key`. `::cal-com::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::types ::cal-com::event-types
::slots ::cal-com::slots
::bookings ::cal-com::bookings

intro first(filter(::types/list-event-types(), (t) { eq(t.slug, "intro-call") }))

open ::slots/get-slots(intro.id, "2026-07-10", "2026-07-14", "America/Chicago")

booking ::bookings/create-booking({
  eventTypeId: intro.id,
  start: "2026-07-10T15:00:00Z",
  attendee: {name: "Ada Lovelace", email: "ada@example.com", timeZone: "America/Chicago"},
  metadata: {crmId: "lead-42"}
})

::bookings/reschedule-booking(booking.uid, "2026-07-11T16:00:00Z", {reschedulingReason: "Customer asked for Friday"})
```

## Functions

**Bookings** (`::cal-com::bookings`)
- `create-booking(booking)`: book a slot by `eventTypeId` (or slug and `username`/`teamSlug`)
- `list-bookings(options)`: one page, returned as `{status, data, pagination}`. Filters use the API's own names: `status` (Str or Vec), `attendeeEmail`, `eventTypeId`, `afterStart`, `beforeEnd`, `afterCreatedAt`, `sortStart`, plus `take`/`skip`
- `list-all-bookings(options, limit)`: follows take/skip pages until `limit` or the last page
- `get-booking(uid)`, `confirm-booking(uid)`, `decline-booking(uid, reason?)`
- `cancel-booking(uid, reason)` or `cancel-booking(uid, {cancellationReason, cancelSubsequentBookings, seatUid})`
- `reschedule-booking(uid, new-start, options?)`
- `mark-absent(uid, ["ada@example.com"])` or `mark-absent(uid, {host: true, attendees: [{email, absent}]})`

**Availability**
- `::cal-com::slots/get-slots(event-type-id, start, end, time-zone, options?)`: `options` takes `duration`, `format: "range"` or `bookingUidToReschedule`
- `::cal-com::schedules`: `list-schedules`, `get-default-schedule`, `get-schedule`, `create-schedule(name, time-zone, is-default, {availability, overrides})`, `update-schedule`, `delete-schedule`

**Event types** (`::cal-com::event-types`)
- `list-event-types(options?)`, `get-event-type(id)`
- `create-event-type(title, slug, length-in-minutes, options?)`, `update-event-type(id, changes)`, `delete-event-type(id)`

**Profile** (`::cal-com::users`)
- `me()`: the cheapest API key check. `update-me(changes)` updates the profile

**Webhooks** (`::cal-com::webhooks`)
- `verify-request(request)` / `verify-request(request, secret)`
- User-level: `list-webhooks`, `get-webhook`, `create-webhook(url, triggers, options?)`, `update-webhook`, `delete-webhook`
- Per event type: `list-event-type-webhooks`, `get-event-type-webhook`, `create-event-type-webhook(event-type-id, url, triggers, options?)`, `update-event-type-webhook`, `delete-event-type-webhook`

**Escape hatch** (`::cal-com::api`)
- `request(method, path, body, api-version, opts?)` returns `data`. `request-envelope` returns the whole body. Pass `""` for no body or no version header

## Webhooks

Cal.com signs each delivery with `X-Cal-Signature-256`, the hex HMAC-SHA256 of the raw body keyed with the webhook's secret. `verify-request` checks it over `::hot::http/raw-body` in constant time. It fails closed: an unset `cal-com.webhook.secret` halts with a message, an empty explicit secret never verifies, and the `no-secret-provided` header that Cal.com sends for a webhook with no secret is rejected.

```hot
::webhooks ::cal-com::webhooks

hook ::webhooks/create-webhook(
  "https://example.com/cal/webhook",
  ["BOOKING_CREATED", "BOOKING_CANCELLED", "BOOKING_RESCHEDULED"],
  {secret: ::hot::ctx/get("cal-com.webhook.secret")}
)

cal-webhook
meta {webhook: {service: "cal-com", path: "/cal/webhook"}}
fn (request) {
  if(not(::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: request.body.triggerEvent}))
}
```

Cal.com doesn't sign a timestamp, so there is no replay window to enforce. If duplicate deliveries matter, deduplicate on `triggerEvent` and the booking `uid`.

## Notes and limits

- Every endpoint function returns the response's `data` and drops the `{status: "success"}` wrapper. `list-bookings` is the exception: it returns the whole envelope so you can page. Empty and 204 responses return null.
- Non-2xx responses return an Err carrying `HttpError {status, headers, body}`. Cal.com's error details are in `body.error`.
- Request bodies omit unset (null) fields instead of sending JSON `null`.
- `start` times for bookings and slots are UTC ISO 8601. `get-slots` returns times in the `time-zone` you pass.
- Confirm, decline and mark-absent must be called by the booking's owner.
- Bookings pages default to 100 items (`take`).

## License

Apache-2.0 - see [LICENSE](LICENSE)
