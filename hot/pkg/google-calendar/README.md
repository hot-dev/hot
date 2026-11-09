# google-calendar

Google Calendar API v3 bindings for Hot, built for scheduling workflows and agents: list events with every filter, create events with Google Meet links and attendee notifications, update, patch, move and delete them, expand recurring series, check free/busy across people, keep a copy in sync with sync tokens and push-notification webhooks, and manage calendars and sharing. Credentials, scopes and errors come from [`google-core`](../google-core).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/google-calendar": "0.1.2"
}
```

## Auth and scopes

Set credentials once in Context Variables (any `google-core` flow), or pass them per call in `opts` for multi-tenant apps:

| Context Variable | Description |
|---|---|
| `google.access.token` | A ready OAuth access token |
| `google.oauth.refresh-token` + `google.oauth.client-id` + `google.oauth.client-secret` | A user's refresh token (see the [consent flow](../google-core/README.md#consent-flow)) |
| `google.service.account` (+ `google.impersonate`) | Service-account key JSON, usually with domain-wide delegation |
| `google.scopes` | Scopes for service-account tokens, overriding the defaults below |
| `google.calendar.channel.token` | Shared secret checked by `::google::calendar::sync/verify-notification` |
| `google.calendar.api-root` | Optional API root override for a proxy or emulator (default `https://www.googleapis.com`) |

Every function asks for the narrowest scope that works and documents it:

| Scope | Default for |
|---|---|
| `calendar.events.readonly` | Listing and reading events, instances, `sync-events`, `watch-events`, `stop-channel` |
| `calendar.events` | Creating, updating, patching, deleting, moving and quick-adding events |
| `calendar.readonly` | The calendar list, `get-calendar`, `get-colors` |
| `calendar` | `create-calendar` |
| `calendar.freebusy` | `query-free-busy` |
| `calendar.acls.readonly`, `calendar.acls` | Listing, and adding or removing, sharing rules |

All Calendar scopes are sensitive (Google verifies public apps that request them). With access and refresh tokens the user's granted scopes apply. For service accounts, the requested scopes must match what a Workspace admin authorised for domain-wide delegation: if you authorised just `calendar`, set `google.scopes` to `"calendar"` so every call asks for it. A service account without delegation only sees calendars shared with its email address.

Every function has an options arity. Calendar parameters use the API's own names (`timeMin`, `singleEvents`, `orderBy`, `sendUpdates`, ...); credential keys (`token`, `refresh-token`, `service-account`, `subject`, `scopes`, `retry`) are handled by `google-core`. Calendar ids (`primary`, `team@group.calendar.google.com`, `en.usa#holiday@group.v.calendar.google.com`) and event ids are URL-encoded in every path.

### Per-call credentials

Credentials in `opts` override context for that call only, so one app can act for many users; see the [google-core README](../google-core/README.md#per-call-credentials-multi-tenant) for every key.

```hot
::events ::google::calendar::events

upcoming ::events/list-events("primary", {singleEvents: true, token: "ya29.tenant-access-token"})
```

Per-call credentials are masked in call records and never written to context. A credential key that is present but blank or null is an error naming the key, never a fallback to context.

## Quick start

```hot
::events ::google::calendar::events
::cals ::google::calendar::calendars

// This week's agenda, recurring events expanded, in start order
agenda ::events/list-all-events("primary", {
  timeMin: "2026-11-02T00:00:00-06:00",
  timeMax: "2026-11-09T00:00:00-06:00",
  singleEvents: true,
  orderBy: "startTime"
})

// Book a meeting with a Meet link and email the attendees
meeting ::events/create-event("primary", ::events/build-event({
  summary: "Design review",
  start: "2026-11-03T15:00:00",
  end: "2026-11-03T15:30:00",
  time-zone: "America/Chicago",
  attendees: ["ada@example.com", "bob@example.com"],
  meet: true
}), {sendUpdates: "all"})
link ::events/meet-link(meeting)

// When is everyone busy?
fb ::cals/query-free-busy("2026-11-04T14:00:00Z", "2026-11-04T23:00:00Z", ["ada@example.com", "bob@example.com"])
ada-busy ::cals/busy-periods(fb, "ada@example.com")

// Move it, quietly
::events/patch-event("primary", meeting.id, {start: {dateTime: "2026-11-03T16:00:00-06:00"}, end: {dateTime: "2026-11-03T16:30:00-06:00"}}, {sendUpdates: "none"})
```

## Functions by area

**`::google::calendar::events`**: `list-events` (one page), `list-all-events` (all pages, optional limit), `get-event`, `create-event`, `update-event` (full replace), `patch-event`, `delete-event`, `quick-add`, `move-event`, `list-instances`, `list-all-instances`, plus the pure `build-event`, `event-time`, `attendee`, `meet-request`, `with-meet`, `meet-link`, `write-params`, `notify-params`.

**`::google::calendar::sync`**: `sync-events`, `is-sync-token-expired`, `summarize-events`, `sync-params`, `watch-events`, `stop-channel`, `renew-watch`, `build-channel`, `parse-notification`, `verify-channel-token`, `verify-notification`.

**`::google::calendar::calendars`**: `list-calendars`, `list-all-calendars`, `get-calendar-list-entry`, `get-calendar`, `create-calendar`, `get-colors`, `query-free-busy`, `busy-periods`, plus the pure `build-calendar`, `build-free-busy`.

**`::google::calendar::acl`**: `list-acl`, `list-all-acl`, `insert-rule`, `share-calendar`, `delete-rule`, `build-rule`.

The most used functions are also aliased at the top level (`::google::calendar/list-events`, `list-all-events`, `get-event`, `create-event`, `update-event`, `patch-event`, `delete-event`, `quick-add`, `build-event`, `list-calendars`, `query-free-busy`, `sync-events`).

## Times and time zones

- **Timed events** use `{dateTime, timeZone?}`. `dateTime` is RFC 3339: with an offset (`2026-11-03T15:00:00-06:00`, `...Z`) it is an exact instant; without one (`2026-11-03T15:00:00`) it needs `timeZone`, an IANA name such as `America/Chicago`. Recurring events need `timeZone` so they follow daylight-saving changes.
- **All-day events** use `{date}` (`2026-11-03`); `end.date` is exclusive, so a one-day event on Nov 3 ends on `2026-11-04`.
- `event-time` (and `build-event`'s `start`/`end`) picks the right shape from a Str: a bare date gives `{date}`, anything else `{dateTime}`.
- List filters `timeMin`, `timeMax` and `updatedMin` must carry an offset. `timeZone` on `list-events` / `get-event` sets the zone of the returned times; the calendar's own zone is in `get-calendar("primary").timeZone`.
- `quick-add` parses text in the calendar's time zone.

## Write options

| Option | Calls | Values |
|---|---|---|
| `sendUpdates` | create, update, patch, delete, quick-add, move | `all`, `externalOnly`, `none` (Google's default when unset: no email) |
| `conferenceDataVersion` | create, update, patch | `1` is added automatically when the body has `conferenceData` (needed to create a Meet link) |
| `supportsAttachments` | create, update, patch | `true` is added automatically when the body has `attachments` (Drive file links) |
| `maxAttendees` | create, update, patch, get, list | Truncate the attendee list in the response |

`build-event` drops null fields, so unset values are never sent as JSON null. `update-event` replaces the whole event (fields you leave out are cleared); use `patch-event` to change a few. A Meet link is created asynchronously: `meet-link` reads `hangoutLink` once `conferenceData.createRequest.status` is `success`.

## Recipe: incremental sync with push notifications

`sync-events` with a stored sync token returns only what changed; a webhook tells you when to run it:

```hot
::sync ::google::calendar::sync
::store ::hot::store

state ::store/Map({name: "calendar-sync"})
HOOK_URL "https://api.example.com/calendar/hook"

calendar-sync
meta {on-event: "calendar:changed"}
fn (event) {
  set ::sync/sync-events("primary", ::store/get(state, "sync-token"))
  // set.full-sync: replace your copy; otherwise apply set.changed and remove set.cancelled-ids
  map(set.changed, (e) { ::store/put(state, `event:${e.id}`, e) })
  map(set.cancelled-ids, (id) { ::store/delete(state, `event:${id}`) })
  ::store/put(state, "sync-token", set.next-sync-token)
}

renew-calendar-watch
meta {schedule: "0 4 */5 * *"}
fn (event) {
  fresh ::sync/renew-watch(::store/get(state, "channel"), "primary", HOOK_URL, {
    channel-token: ::hot::ctx/get("google.calendar.channel.token")
  })
  ::store/put(state, "channel", fresh)
}

calendar-hook
meta {webhook: {service: "calendar", path: "/hook"}, secret-headers: ["x-goog-channel-token"]}
fn (request) {
  note ::sync/verify-notification(request)
  cond {
    is-err(note) => { ::hot::http/HttpResponse({status: 401, body: "bad channel token"}) }
    note.is-sync => { ::hot::http/HttpResponse({status: 200, body: ""}) }
    => {
      send("calendar:changed", {resource-id: note.resource-id})
      ::hot::http/HttpResponse({status: 200, body: ""})
    }
  }
}
```

The first `sync-events` (token null) lists everything, deleted events included. When Google expires a sync token it answers 410 Gone; `sync-events` then runs a full sync and sets `full-sync: true` (pass `{resync: false}` to get the Err instead). Filters that cannot be combined with a sync token (`q`, `timeMin`, `timeMax`, `orderBy`, `updatedMin`, `iCalUID`, extended properties) are dropped on incremental calls, so keep the remaining options the same across runs. Notifications carry only headers (`resource-state` is `sync` for the first message, then `exists`); channels expire, so renew them from a schedule. `verify-notification` compares `X-Goog-Channel-Token` in constant time and fails closed when it or `google.calendar.channel.token` is missing.

## Sharing

```hot
::acl ::google::calendar::acl
::cals ::google::calendar::calendars

ops ::cals/create-calendar("On-call", {timeZone: "UTC", description: "Who is on call"})
::acl/share-calendar(ops.id, "ada@example.com", "writer")
::acl/insert-rule(ops.id, "freeBusyReader", "domain", "example.com", {sendNotifications: false})
```

Roles are `freeBusyReader`, `reader`, `writer`, `owner` and `none`; scope types are `user`, `group`, `domain` and `default` (public). `delete-rule` takes the rule id from `list-acl` (e.g. `user:ada@example.com`).

## Notes and limits

- `list-events` returns at most 2500 events per page (250 by default); `list-all-events` follows `nextPageToken` for you. Without `singleEvents: true`, a recurring series is one event with `recurrence`; with it, each occurrence is listed and `orderBy: "startTime"` is allowed.
- Deleting one occurrence of a recurring event (an instance id from `list-instances`) cancels just that occurrence.
- `move-event` changes an event's organizer calendar and only works for `default` events (not birthdays or working-location events).
- Quotas are per minute per project and per user. On 429, 403 `rateLimitExceeded` or 5xx, `::google::api/is-retryable(result)` is true: let a handler `retry` policy reschedule (see the [google-core README](../google-core/README.md#retries)). Creating an event is not idempotent; give it your own `id` (base32hex, 5-1024 characters) to make a retry safe.
- Errors are `::google::api/HttpError` with Google's `reason` (e.g. `notFound`, `forbiddenForNonOrganizer`, `timeRangeEmpty`).

## License

Apache-2.0 - see [LICENSE](LICENSE)
