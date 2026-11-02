# persona

[Persona](https://withpersona.com) identity verification bindings for Hot, built for agentic KYC and KYB onboarding: create an inquiry (or a KYB transaction), wait for the webhook, review what Persona found, then decide. The package covers inquiries with hosted-flow, resume and one-time links, accounts, verifications, reports (watchlist, PEP, adverse media, business lookup and the other KYB reports), cases, documents, transactions, events, webhook management, `Persona-Signature` verification with a replay window, and decision helpers that normalize every status to `approved | declined | needs-review | pending | failed`. It is pinned to `Persona-Version: 2026-09-29`. Anything not wrapped is one `::persona::api/call` away.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/persona": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `persona.api.key` | yes | A sandbox (`persona_sandbox_...`) or production (`persona_production_...`) API key. Both use `https://api.withpersona.com/api/v1`; the key picks the environment |
| `persona.api.version` | no | Overrides the `Persona-Version` header (default `2026-09-29`, `::persona/API_VERSION`) |
| `persona.key-inflection` | no | `kebab` (Persona's default), `snake` or `camel` for response keys. The read helpers accept all three |
| `persona.webhook.secret` | for webhooks | The webhook's signing secret (`wbhsec_...`) |

Requests use `Authorization: Bearer <key>`. Every function that calls the API takes an optional trailing options argument, a `::persona::api/RequestOptions` or a plain Map:

| Option | Effect |
|---|---|
| `api-key` | Use this key instead of `persona.api.key` for one call (many tenants in one run, or parallel flows) |
| `persona-version` | `Persona-Version` for one call |
| `key-inflection` | `Key-Inflection` for one call |
| `idempotency-key` | Sent as `Idempotency-Key` on POSTs. Persona replays the first response for a repeated key (kept at least 24 hours), so a retried create never makes a second inquiry, report or transaction |

`::persona::api/resolve(opts)` is the one place options, context and defaults are merged. Per-call keys are never written to ctx.

**Sandbox.** Sign up at [withpersona.com](https://withpersona.com/dashboard/signup) for immediate, free access to a sandbox environment and its `persona_sandbox_` key; production needs a contract with Persona's sales team. Sandbox inquiries run no real checks: the hosted flow has a pass/fail toggle, `::persona::inquiries/simulate` drives an inquiry through its lifecycle (and fires real webhooks), and reports return canned hits for documented test names (for example "Kim Jong Un" on a watchlist report, "Boris Johnson" for PEP, "Globovision" for business watchlist and adverse media). KYB transactions accept `fields.debug: "passed"` or `"failed"`. Templates, report templates, case templates and transaction types are created in the dashboard, per environment.

## Quick start

```hot
::inquiries ::persona::inquiries
::decisions ::persona::decisions
::api ::persona::api

// 1. Create an inquiry and a one-time link for the user
start-kyc fn (user-id: Str): Str {
  inquiry ::inquiries/create-inquiry("itmpl_...", {
    reference-id: user-id,
    meta: {auto-create-one-time-link: true}
  }, ::api/RequestOptions({idempotency-key: `${user-id}-kyc`}))
  ::inquiries/one-time-link(inquiry)
}

// 2. After the inquiry webhook arrives, read it and decide
review-kyc fn (inquiry-id: Str): Map {
  doc ::inquiries/get-inquiry(inquiry-id, {include: ["verifications", "reports"]})
  summary ::decisions/inquiry-summary(doc)
  cond {
    eq(summary.decision, "needs-review") => { ::inquiries/mark-for-review(inquiry-id) }
    => { summary }
  }
}
```

## The onboarding loop

1. **Create.** `create-inquiry` (KYC) or `::persona::transactions/create-transaction` (API-first KYB). Pass an `idempotency-key`.
2. **Send the user to the hosted flow.** `generate-one-time-link` (single use, 24 hours by default) or `resume-inquiry` + `resume-url`. `hosted-flow-url` builds a link that creates a new inquiry when opened.
3. **Wait for the webhook.** There is no polling loop and nothing waits in a run. Register a webhook for `inquiry.completed`, `inquiry.approved`, `inquiry.declined`, `inquiry.marked-for-review`, `inquiry.failed`, `inquiry.expired`, and `report/<type>.ready` or `transaction.status-updated` as needed. For a safety net, a schedule can check `is-done` and fail the run so its `retry` policy tries again later.
4. **Review.** `get-inquiry(id, {include: ["verifications", "reports"]})`, then `::persona::decisions/inquiry-summary`: ids, types, statuses, failed check names and match flags, with no PII, ready to hand to a reviewer agent.
5. **Decide.** `approve-inquiry`, `decline-inquiry`, `mark-for-review`, or open a case with `::persona::cases/create-case` for a human.

## Functions by area

**`::persona::inquiries`**: `create-inquiry`, `get-inquiry` (`include`, sparse `fields`), `list-inquiries`, `collect-inquiries`, `search-inquiries`, `update-inquiry`, `approve-inquiry`, `decline-inquiry`, `mark-for-review`, `expire-inquiry`, `resume-inquiry`, `session-token`, `generate-one-time-link`, `one-time-link`, `redact-inquiry`, `add-tag`, `remove-tag`, `print-inquiry` (PDF `Bytes`), `simulate` (sandbox keys only), `hosted-flow-url`, `resume-url`, `status`, `is-done`.

**`::persona::accounts`**: `create-account`, `get-account`, `list-accounts`, `find-by-reference-id`, `update-account`, `consolidate-accounts`, `redact-account`, `add-tag`.

**`::persona::verifications`**: `get-verification`, `get-verification-by-type` (`"government-id"`, `"selfie"`, `"database"`, `"document"`, `"phone-number"`, `"email-address"`, ...), `list-for-inquiry` (Persona has no verification list endpoint; this reads the inquiry's included verifications), `failed-checks`, `redact-verification`.

**`::persona::reports`**: `create-report` (any type: `watchlist`, `politically-exposed-person`, `adverse-media`, `business-lookup`, `business-watchlist`, `business-adverse-media`, `business-search`, `email-address`, `phone-number`, ...), `create-watchlist-report`, `create-business-lookup-report`, `get-report`, `list-reports`, `rerun-report`, `redact-report`, `add-tag`, `print-report`, `is-done`, `has-match`.

**`::persona::cases`**: `create-case`, `get-case`, `list-cases`, `update-case`, `assign-case`, `set-status`, `add-objects`, `add-comment`, `redact-case`.

**`::persona::documents`**: `get-document`, `files` (metadata without signed URLs), `file-url`, `download-file` (`Bytes`; only `*.withpersona.com` URLs).

**`::persona::transactions`** (KYB): `create-transaction`, `get-transaction`, `list-transactions`, `update-transaction`, `label-transaction`, `redact-transaction`, `is-done`.

**`::persona::events`**: `list-events`, `get-event`.

**`::persona::webhooks`**: `verify-request`, `verify-signature`, `parse-signature-header`, `construct-event`, `parse-event`, `event-document`, `list-webhooks`, `create-webhook`, `get-webhook`, `update-webhook`, `enable-webhook`, `disable-webhook`, `archive-webhook`, `rotate-secret`.

**`::persona::decisions`**: `inquiry-decision`, `report-decision`, `verification-decision`, `transaction-decision`, `case-decision`, `decide` (by type), `inquiry-summary`, `included-reasons`.

**`::persona::api`**: `call`, `send`, `get-bytes`, `list-page`, `collect`, `next-cursor`, `resolve`, and the JSON:API read helpers `resource`, `attr` (any inflection), `relationship-ids`, `included`, `find-included`, `field-values`, `flatten`.

## KYB

Persona's API-first KYB is a **transaction** of a KYB transaction type you configure in the dashboard: you send the business and its associated people in `fields`, and Persona workflows run the business reports, verify the owners and open a case, then move the transaction to `approved`, `declined` or `needs_review`. You can also run the building blocks yourself: `create-business-lookup-report` and the other `business-*` reports, and business inquiry templates through the normal inquiry flow.

```hot
::txns ::persona::transactions

start-kyb fn (business-id: Str): Map {
  ::txns/create-transaction("txntp_...", {
    business_name: "Acme Robotics LLC",
    business_registered_address: {street_1: "1 Main St", city: "Austin", subdivision: "TX", postal_code: "78701", country_code: "US"}
  }, {reference-id: business-id}, ::persona::api/RequestOptions({idempotency-key: `${business-id}-kyb`}))
}
```

## Decisions

| Persona status | `decision` |
|---|---|
| inquiry `approved` / `declined` | `approved` / `declined` (`final: true`) |
| inquiry `needs_review` | `needs-review` |
| inquiry `completed` (no decision yet) | `needs-review`, or `opts.completed-as` |
| inquiry `created`, `pending` | `pending` |
| inquiry `failed`, `expired` | `failed` |
| report `ready` with `has-match` / without | `needs-review` / `approved` |
| report `pending` / `errored` | `pending` / `failed` |
| verification `passed`, `confirmed` / `failed` | `approved` / `declined` (reasons: failed check names) |
| transaction `approved`, `declined`, `needs_review`, `errored`, `created` | `approved`, `declined`, `needs-review`, `failed`, `pending` |
| case `Approved` / `Declined` / anything else | `approved` / `declined` / `pending` |
| anything unknown | `needs-review` (never an approval) |

Reasons are sorted and name checks and report types only, e.g. `"verification/government-id failed: id_expired"` or `"report/watchlist has a match"`.

## Webhooks

Persona signs every delivery with HMAC-SHA256 over `${t}.${raw_body}` and sends `Persona-Signature: t=<unix>,v1=<hex>`. During a secret rotation the header holds one space-separated set per active secret; any matching set verifies. `verify-request` compares in constant time, uses `::hot::http/raw-body`, fails closed on a missing or empty secret, and rejects timestamps more than 300 seconds from now (pass a third argument to change the window, 0 to skip it).

```hot
persona-webhook
meta {webhook: {service: "persona", path: "/persona/webhook"}, secret-headers: ["persona-signature"]}
fn (request) {
  event ::persona::webhooks/construct-event(request)
  cond {
    is-err(event) => { ::hot::http/HttpResponse({status: 401, body: "invalid signature"}) }
    => {
      // Deliveries can repeat and arrive out of order: dedupe on event.id,
      // order by event.created-at, then hand event.object-id to the reviewer.
      ::hot::http/HttpResponse({status: 200, body: {received: true}})
    }
  }
}
```

Reply within 5 seconds; Persona retries failed deliveries up to 7 times over about 2 days. Webhooks created after January 2026 send no `included` objects unless the webhook's included allowlist says so, so fetch the inquiry with `include` when you need its verifications. Create a webhook with `create-webhook` and `enable-webhook`; `rotate-secret` keeps the old secret signing for the overlap you choose.

## Errors and pagination

A non-2xx response is `err(HttpError)` with `status`, `message` (one readable line, e.g. `"persona: HTTP 404: Record not found [req_...]"`), `errors` (Persona's `[{title, details, source}]`), `title`, `details`, `pointer`, `request-id` (quote it to Persona support) and `retry-after`. Nothing retries or waits: check `::persona::api/is-retryable(e)` (408, 429, 5xx) and let a handler or schedule `retry` policy back off. Persona's default limit is 300 requests per minute per environment, plus creation quotas.

Lists are cursor-paginated (`page[size]` up to 100, `page[after]`, newest first). `list-*` functions return one page; read the next cursor with `::persona::api/next-cursor(page)` and pass it as `{after: ...}`, or use `collect-inquiries` / `::persona::api/collect` with a limit. `include` isn't allowed on list endpoints.

## Data handling

Inquiries, accounts, verifications and documents hold PII: names, addresses, birth dates, government id numbers, selfies and id images. The package keeps that out of places it shouldn't be:

- Errors never carry an SSN, birth date, id number or document image. Error bodies are redacted (values under keys such as `social-security-number`, `birthdate`, `identification-number`, `front-photo`, `session-token`) and digit runs and emails in error text are masked, so an `HttpError` is safe to log.
- Nothing is logged by the package. Don't log `field-values`, `flatten` output, verification attributes or report PDFs yourself.
- `hosted-flow-url` never puts field prefill in the URL, since URLs end up in browser history and logs. Prefill through `create-inquiry(..., {fields: ...})` in the request body.
- Document responses carry signed file URLs that grant access without an API key. `documents/files` leaves them out; `download-file` errors carry only the HTTP status.
- Session tokens, one-time links and signed file URLs are credentials. Send them only to the user they belong to.
- `inquiry-summary` and the decision helpers contain only ids, types, statuses, check names and match flags.
- `redact-inquiry`, `redact-account`, `redact-report`, `redact-case`, `redact-transaction` and `redact-verification` permanently remove PII at Persona when your retention period ends.

## Notes and limits

- Production access, production pricing, and some report types (and their data sources) need a contract with Persona. The sandbox is self-serve and free.
- Statuses, report types, verification types and event names are treated as open-ended; unknown values map to `needs-review`.
- `fields` keys follow your template's casing and are never inflected.
- Search endpoints are eventually consistent; don't use `search-inquiries` to read your own writes.
- Not wrapped (use `::persona::api/call`): inquiry sessions, lists and list items, importers, API key management, workflows, graph queries, and the account relation endpoints.
