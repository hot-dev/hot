# middesk

[Middesk](https://www.middesk.com) business verification (KYB) bindings for Hot, built for agentic onboarding: create a business with the orders you need, wait for the `business.updated` webhook, summarize the report, then approve or reject. The package covers businesses and the full report (Secretary of State registrations, IRS TIN match, watchlists, liens, litigations, bankruptcies, people, website, review tasks), orders for additional products, documents and the PDF report as `Bytes`, monitoring, webhook management, `X-Middesk-Signature-256` verification, and a deterministic, LLM-friendly report summary with a `needs-review` flag. Anything not wrapped is one `::middesk::api/call` away.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/middesk": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `middesk.api.key` | yes, unless passed in options | A sandbox (`mk_test_...`) or production (`mk_live_...`) API key, from Dashboard > Settings > Developer > Credentials |
| `middesk.env` | no | `"sandbox"` or `"production"`, for a key without a recognizable prefix. The key's prefix normally decides: `mk_test_` goes to `https://api-sandbox.middesk.com/v1`, `mk_live_` to `https://api.middesk.com/v1`. A setting that contradicts the prefix is an error, since Middesk rejects a key on the other host |
| `middesk.webhook.secret` | for webhooks | The `secret` you set on the webhook endpoint |

Requests use HTTP Basic auth with the API key as the username and an empty password.

Middesk has no idempotency keys. Set `unique_external_id` on create: Middesk refuses a second business with the same value, so a retried create can't make a duplicate.

## Per-call credentials

Every function that calls the API takes an optional trailing options Map with `api-key` and `env` (a `::middesk::api/RequestOptions` works too), so one run can serve many Middesk accounts. Options always come last; where a function also takes `params`, pass `{}` for those.

```hot
tenant {api-key: "mk_test_tenant_b"}
business ::middesk::businesses/get-business("biz_123", tenant)
page ::middesk::businesses/list-businesses({per_page: 10}, tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `middesk.api.key`. `::middesk::api/resolve(opts)` merges options over context. Per-call keys are masked in call records and never written to ctx.

## Sandbox

Every Middesk account is provisioned with a sandbox key (`mk_test_`) alongside the production key, and sandbox requests are free. You get an account through Middesk's sales team (there's no public self-serve signup). Sandbox results are fake, even for real data, and businesses never enter `in_audit`. Documented test values drive outcomes:

| Input | Sandbox outcome |
|---|---|
| Business name `Unregistered Business` | No Secretary of State filings |
| Name containing `watchlist hit` (business or person) | Watchlist hit |
| Name containing `bankruptcy`, `liens found`, `highrisk` | Bankruptcy, blanket liens, high-risk industry |
| Name containing `short analyst review` / `long analyst review` | The business goes `pending` → `in_review` after 2 / 10 minutes |
| TIN `110000099` | TIN name mismatch |
| TIN `444555666` | TIN unknown and not issued |
| TIN `333444555` | IRS unavailable (retried) |
| TIN `123456789` | TIN not found |
| Address `123 Grand St., New York, NY 10013` | No address match |
| Address `223 Grand St., New York, NY 10013` | Address match |
| `address_line1` containing `PO Box`, `cmra`, `registered agent`, `residential` | The matching address flags |
| Person `J Doe` | Person unverified |
| Name `Watchlist Result Created Inc` | A `watchlist_result.created` monitoring event about a minute later |

Anything not listed verifies successfully. Sandbox business creation is limited to 120 per minute per account. See [Middesk's environments guide](https://docs.middesk.com/environments) for the full list.

## Quick start

```hot
::businesses ::middesk::businesses
::decisions ::middesk::decisions

// 1. Create the business; the default order is a Business Verification report
start-kyb fn (account-id: Str): Map {
  ::businesses/create-business({
    name: "Acme Robotics LLC",
    tin: {tin: "123410000"},
    addresses: [{address_line1: "123 Main St", city: "Austin", state: "TX", postal_code: "78701"}],
    people: [{name: "Jane Doe", title: "CEO"}],
    unique_external_id: account-id,
    orders: [{product: "business_verification_verify"}, {product: "website"}]
  })
}

// 2. When business.updated says it's in_review, summarize and decide
review-kyb fn (business-id: Str): Map {
  summary ::decisions/summarize(::businesses/get-business(business-id))
  cond {
    summary.needs-review => { summary }
    eq(summary.status, "in_review") => { ::businesses/set-status(business-id, "approved") }
    => { summary }
  }
}
```

## The onboarding loop

1. **Create.** `create-business` with `name`, `addresses`, and whatever else you have (`tin`, `website`, `people`, `phone_numbers`). Missing `name` or `addresses` is an error and nothing is sent. Pick products with `orders`; the default is a Business Verification report.
2. **Wait for the webhook.** Register a webhook for `business.updated` (and `order.updated` if you add orders later). Nothing polls or waits in a run; for a safety net, a schedule can check `::middesk::businesses/is-done` (status `in_review`, `approved` or `rejected`) and fail the run so its `retry` policy tries again later.
3. **Go deeper if needed.** `::middesk::orders/create-order(id, "liens")`, `"litigations"`, `"bankruptcies"`, `"website"` with `{subproducts: ["industry_classification"]}`, `"kyc"` for the owners, `"documents"`, `"adverse_media"`.
4. **Review.** `::middesk::decisions/summarize(business)` and `::middesk::decisions/reasons(summary)`.
5. **Decide.** `::middesk::businesses/set-status(id, "approved")` or `"rejected"`.

## Functions by area

**`::middesk::businesses`**: `create-business`, `verify-tin` (a TIN-only order), `get-business`, `list-businesses`, `collect-businesses`, `find-by-external-id`, `update-business`, `set-status`, `get-review`, `get-policy-results`, `get-timeline`, `get-latest-risk-assessment`, `get-pdf` (`Bytes`), `is-done`, `validate-create`.

**`::middesk::orders`**: `create-order`, `list-orders`, `get-order`, `update-order`, `is-done`, `order-body`.

**`::middesk::documents`**: `list-documents`, `get-document`, `get-download-url`, `download-document` (`Bytes`), `metadata` (without the signed URL).

**`::middesk::monitoring`**: `get-monitor`, `create-monitor`, `update-monitor`, `delete-monitor`.

**`::middesk::webhooks`**: `verify-request`, `verify-signature`, `construct-event`, `parse-event`, `list-webhooks`, `create-webhook`, `get-webhook`, `update-webhook`, `delete-webhook`.

**`::middesk::decisions`**: `summarize`, `reasons`, `status-decision`, `review-tasks`, `tin-result`, `watchlist-hits`, `registration-summary`.

**`::middesk::api`**: `call`, `send`, `get-bytes`, `list-page`, `collect`, `next-page`, `resolve`, `is-retryable`.

## Report summary

`summarize` turns a business into one deterministic record (lists sorted, so the same report always gives the same summary):

| Field | Meaning |
|---|---|
| `id`, `name`, `status` | The business |
| `decision` | `approved` (status `approved`), `declined` (`rejected`), `needs-review` (`in_review` or an unknown status), `pending` (`open`, `pending`, `in_audit`) |
| `final` | You (or Middesk) already approved or rejected it |
| `needs-review` | True when the status is `in_review`, or before a final decision when any review task failed or warned or a watchlist hit exists; false once approved or rejected |
| `passed` | Keys of `success` review tasks, e.g. `["name", "sos_active", "tin"]` |
| `failed`, `warnings` | `{key, label, sub_label, message}` for `failure` and `warning` tasks |
| `review-tasks` | Every task as `{key, category, label, sub_label, status, message}` |
| `tin` | `{verified, mismatch, unknown, issued, error}`, never the TIN itself |
| `watchlist-hits` | Number of watchlist results |
| `registrations` | `{count, active, inactive, states}` |
| `records` | Counts of `liens`, `litigations`, `bankruptcies` |
| `orders` | `[{product, status}]` |

`reasons(summary)` gives short sorted lines such as `"failure tin: TIN Match - Not Found"` and `"watchlist hits: 1"`.

## Webhooks

Middesk signs each delivery with HMAC-SHA256 of the raw body, keyed by the webhook's `secret`, and sends the hex digest in `X-Middesk-Signature-256`. `verify-request` hashes `::hot::http/raw-body`, compares in constant time, and fails closed on a missing or empty secret. Only the SHA-256 header is supported; an older Middesk doc described a SHA-1 `X-Middesk-Signature` header, which the current docs no longer mention.

**There is no signed timestamp**, so a captured delivery can be replayed. Record each event `id` once and ignore repeats. Middesk retries failed deliveries up to 10 times over about 3 days, so you need the dedupe anyway.

```hot
::store ::hot::store

seen ::store/Map({name: "middesk-events"})

middesk-webhook
meta {webhook: {service: "middesk", path: "/middesk/webhook"}, secret-headers: ["x-middesk-signature-256"]}
fn (request) {
  event ::middesk::webhooks/construct-event(request)
  cond {
    is-err(event) => { ::hot::http/HttpResponse({status: 401, body: "invalid signature"}) }
    is-some(::store/get(seen, event.id)) => { ::hot::http/HttpResponse({status: 200, body: {duplicate: true}}) }
    => {
      ::store/put(seen, event.id, {type: event.type, business: event.object-id})
      // On business.updated, fetch the business and summarize it for review
      ::hot::http/HttpResponse({status: 200, body: {received: true}})
    }
  }
}
```

Create the endpoint with a secret, or Middesk sends no signature at all:

```hot
register fn (): Map {
  secret ::hot::random/random-string(40)
  // Store `secret` as middesk.webhook.secret before deliveries start
  ::middesk::webhooks/create-webhook("https://myapp.hot.dev/middesk/webhook", ["business.updated", "order.updated"], {secret: secret})
}
```

Sandbox and production need separate webhook endpoints.

## Errors and pagination

A non-2xx response is `err(HttpError)` with `status`, `message` (one readable line, e.g. `"middesk: HTTP 422: Name is required (name)"`), `errors` (Middesk's `[{message, parameter}]`), `parameter` and `retry-after`. Nothing retries or waits: check `::middesk::api/is-retryable(e)` (408, 429, 5xx) and let a handler or schedule `retry` policy back off. The limit is about 20 requests per second per key, and `PATCH /businesses/{id}` allows 5 per 15 seconds per business.

Lists use page numbers: `page` (from 1) and `per_page`, with `has_more` and `total_count` in the response. `list-*` returns one page; `::middesk::api/next-page(page, n)` gives the next number, and `collect-businesses` / `::middesk::api/collect` follow `has_more` up to a limit.

## Data handling

A KYB request can carry PII: the business TIN (an SSN for some sole proprietors), and for KYC orders the owners' SSNs and birth dates.

- Errors never carry a TIN, SSN or birth date. Error bodies are redacted (values under `tin`, `ssn`, `dob`, `download_url` and similar keys) and digit runs and emails in error text are masked, so an `HttpError` is safe to log.
- Nothing is logged by the package. Don't log the business object or people records yourself.
- `summarize` never includes the TIN, people's SSNs or birth dates, or document links. It does include what Middesk wrote in task messages, which can name the business, its addresses and sometimes its officers.
- Document `download_url`s are signed links that need no key. `documents/metadata` leaves them out, `download-document` fetches a fresh one, and download errors carry only the HTTP status.
- `get-pdf` and downloaded documents can contain TINs and owner details: store them like the report.

## Notes and limits

- Middesk is sales-led: production and sandbox keys come with an account, and products such as KYC, liens filing, litigations, adverse media, risk, and international verification need to be enabled on your contract.
- Middesk's current OpenAPI spec lists no query filters for `GET /businesses`. The filters documented earlier (`status`, `external_id`, `unique_external_id`, `tags`, `start_date`, `end_date`) are passed through as given.
- Response shapes for orders, monitors and webhooks are untyped in Middesk's spec; they come back as decoded JSON.
- Not wrapped (use `::middesk::api/call`): lien filing and termination, Signal fraud scores, business batches, actions, agents, identity autocomplete and prefill, sub-accounts, OAuth clients, and webhook event schemas.
