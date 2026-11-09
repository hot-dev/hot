# hubspot

HubSpot CRM bindings for Hot, built on HubSpot's date-versioned APIs (`2026-09`, the version HubSpot recommends for new integrations). One generic objects surface covers contacts, companies, deals, tickets, activities, and custom objects: CRUD, search, batch read/create/update/upsert, associations, and cursor pagination. On top of that come typed conveniences, activity logging (notes, tasks, calls, meetings, emails), pipelines and stages, owners, property definitions, lists, and v3 webhook verification.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/hubspot": "0.1.2"
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `hubspot.token` | yes, unless passed per call | A private-app or service key access token (`pat-...`) |
| `hubspot.api.version` | no | Another date-based API version, e.g. `"2026-03"` |
| `hubspot.client.secret` | for webhooks | The app's client secret, used by `::hubspot::webhooks/verify-request` |

## Functions by area

### Records (`::hubspot::objects`)

`object-type` is `contacts`, `companies`, `deals`, `tickets`, an activity type (`notes`, `tasks`, `calls`, `meetings`, `emails`), or a custom object's name or type id.

| Function | Does |
|---|---|
| `get-object` | Read one record by id, or by a unique `idProperty` such as `email`; select `properties` and `associations` |
| `create-object` | Create, optionally with associations (`association-input`) |
| `update-object`, `archive-object` | Patch properties (by id or unique property); archive to the recycle bin |
| `list-objects`, `list-all-objects` | One page, or every page up to a cap |
| `search-objects`, `search-all-objects` | Filter (AND) or `filterGroups` (OR), sorts, free-text `query` |
| `batch-read`, `batch-create`, `batch-update`, `batch-upsert`, `batch-archive` | Up to 100 records per call; `batch-upsert` keys on a unique property |
| `associate`, `associate-with-label`, `remove-association`, `list-associations` | Default or labeled associations |

The typed modules wrap the common cases: `::hubspot::contacts` (`create-contact`, `get-contact`, `update-contact`, `find-by-email`, `search-contacts`, `upsert-by-email`, `archive-contact`), `::hubspot::companies` (`find-by-domain`, ...), and `::hubspot::deals`.

```hot
::objects ::hubspot::objects
::contacts ::hubspot::contacts

// Idempotent sync keyed on email (no search-index delay)
::contacts/upsert-by-email([
  {email: "ada@example.com", lifecyclestage: "lead"},
  {email: "grace@example.com", firstname: "Grace"}
])

// Read many at once
found ::objects/batch-read("contacts", ["ada@example.com", "grace@example.com"], {
  idProperty: "email",
  properties: ["firstname", "lifecyclestage", "hubspot_owner_id"]
})

// Open deals over $10k, newest first, across pages
big ::objects/search-all-objects("deals", {
  filters: [
    {propertyName: "amount", operator: "GT", value: "10000"},
    {propertyName: "hs_is_closed", operator: "EQ", value: "false"}
  ],
  sorts: [{propertyName: "createdate", direction: "DESCENDING"}],
  properties: ["dealname", "amount", "dealstage"]
})
```

### Activities (`::hubspot::engagements`, `::hubspot::notes`)

`create-task`, `create-call`, `create-meeting`, `create-email`, and `create-note` take HubSpot properties plus a Vec of `{type, id}` targets, and associate the activity to each target in the same call. `hs_timestamp` defaults to now. `association-type-id` exposes HubSpot's activity-to-record type ids. `::hubspot::notes/log-note(object-type, id, body)` is the one-line note. Update or archive an activity with `::hubspot::objects` and its type (`"tasks"`, ...).

```hot
::engagements ::hubspot::engagements

::engagements/create-call({
  hs_call_title: "Discovery call",
  hs_call_body: "Wants SSO and audit logs.",
  hs_call_direction: "OUTBOUND",
  hs_call_duration: 1800000,
  hs_call_status: "COMPLETED"
}, [{type: "contacts", id: "12345"}])

::hubspot::notes/log-note("deals", "67890", "Champion confirmed budget on today's call.")
```

### Pipelines, owners, properties

- `::hubspot::pipelines`: `list-pipelines`, `get-pipeline`, `list-stages`, `get-stage`, and `find-stage(object-type, pipeline, stage)`, which matches ids or labels case-insensitively and returns `{pipeline_id, stage_id, stage}`.
- `::hubspot::owners`: `list-owners`, `get-owner` (by owner id or `userId`), and `find-owner-by-email`. Write the owner's `id` to `hubspot_owner_id`.
- `::hubspot::properties`: `list-properties`, `get-property` (enumeration `options` are the valid values), `create-property`, `update-property`, and `archive-property`.

```hot
owner ::hubspot::owners/find-owner-by-email("rep@example.com")
status ::hubspot::properties/get-property("contacts", "hs_lead_status")
valid-values map(status.options, (o) { o.value })
```

### Lists (`::hubspot::lists`)

`create-list` (`MANUAL`, `SNAPSHOT`, or `DYNAMIC` with a `filterBranch`), `get-list`, `find-list-by-name`, `search-lists`, `delete-list`, `add-members`, `remove-members`, and `list-members`. `object-type-id` maps `contacts` to `0-1`, `companies` to `0-2`, `deals` to `0-3`, and `tickets` to `0-5`.

```hot
::lists ::hubspot::lists

created ::lists/create-list("Webinar attendees", "contacts", "MANUAL")
::lists/add-members(created.list.listId, ["12345", "67890"])
```

### Per-call credentials

Every function that calls HubSpot takes an optional trailing `opts` Map, so one run can act for many portals and parallel flows don't share credentials. Per-call credentials are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `token` | The private app or OAuth access token for this call, instead of `hubspot.token` |
| `api-version` | The date-based API version for this call, instead of `hubspot.api.version` |

`opts` always comes last. Where a function also takes `params` or `options`, pass `{}` for those.

```hot
tenant {token: "pat-tenant-b"}

contact ::hubspot::contacts/get-contact("12345", tenant)
page ::hubspot::objects/list-objects("deals", {limit: 10}, tenant)
every ::hubspot::objects/list-all-objects("deals", {}, 5, tenant)
::hubspot::notes/log-note("contacts", "12345", "Demo went well.", tenant)
```

A `token` that is present but blank or null is an error naming the key, never a fallback to `hubspot.token`. `::hubspot::api/resolve(opts)` is the one place this is decided.

### Pagination and low level (`::hubspot::api`)

`paginate(fetch, max-pages)` follows `paging.next.after` for any list or search call and concatenates `results`. `next-after(page)` reads one cursor. `json` and `request` call any endpoint, and `crm-path("objects", "/contacts")` builds a versioned path.

```hot
::objects ::hubspot::objects

companies ::hubspot::api/paginate((after) {
  ::objects/list-objects("companies", {limit: 100, after: after, properties: ["name", "domain"]})
}, 3)
```

## Errors

A non-2xx response returns an Err carrying `::hubspot::api/HttpError` `{status, headers, body}`. HubSpot's body has `message`, `category` (e.g. `CONFLICT`, `VALIDATION_ERROR`, `OBJECT_NOT_FOUND`), and `correlationId`. `::hubspot::api/error-message` extracts the message. `find-by-email`, `find-by-domain`, `find-owner-by-email`, and `find-stage` return null when nothing matches.

```hot
created ::hubspot::contacts/create-contact({email: "ada@example.com"})
if-err(created, (e) { println(`HubSpot: ${::hubspot::api/error-message(e)}`) })
```

## Webhooks

`verify-request` implements HubSpot's v3 request validation. It computes a base64 HMAC-SHA256 with your client secret over `method + uri + body + timestamp` and compares it in constant time. The body comes from `::hot::http/raw-body`. Before signing, the URI's percent-escaped `: / ? @ ! $ ' ( ) * , ;` are decoded per HubSpot's spec, so URLs with query strings verify. Requests whose millisecond `X-HubSpot-Request-Timestamp` is more than 5 minutes off are rejected. A missing secret, header, or URL fails closed.

```hot
hubspot-webhook
meta {webhook: {service: "hubspot", path: "/hubspot/webhook"}}
fn (request) {
  if(not(::hubspot::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: "ok"}))
}
```

HubSpot signs the exact URL configured in your app. If a proxy rewrites it, pass that URL explicitly with `verify-request(request, secret, url)`.

## Notes and limits

- **Search is eventually consistent.** A record created moments ago may not match a search yet. Prefer `get-object` with `idProperty` or `batch-upsert` for read-after-write flows.
- **Limits.** Batch calls take 100 inputs. Search returns at most 200 per page and 10,000 in total. Private apps are rate limited per 10 seconds by tier, and 429 responses come back as `HttpError`.
- **Archive is soft.** Archived records stay in the recycle bin for 90 days.
- **Versions.** HubSpot's numbered `v3`/`v4` CRM paths are being replaced by date versions, and v4 associations move to unsupported status in March 2027. This package uses date versions throughout. Pin one with `hubspot.api.version`.

## Integration tests

`hot test -p pkg-integration-hubspot` lists one contact (read-only). It skips with a note unless `HUBSPOT_TOKEN` is set. HubSpot offers free developer test accounts.

## License

Apache-2.0 - see [LICENSE](LICENSE)
