# xero

Xero Accounting API bindings for Hot, built for agentic finance ops: reconciliation, invoicing, AP/AR and reporting. OAuth 2.0 with rotating refresh tokens (PKCE, Custom Connections, tenant connections, a per-run token cache and an opt-in store-backed token keeper), the granular 2026 scopes, contacts, invoices (PDF and email), credit notes, payments, bank transactions and transfers, accounts, items, manual journals, purchase orders, quotes, tax rates, tracking categories and the organisation, attachments, safe `where` filters with paging and collect-all, `If-Modified-Since` and `Idempotency-Key`, reports with a row flattener, rate-limit headers, readable validation errors, and webhook verification with the intent-to-receive handshake.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/xero": "0.1.0"
}
```

## Auth and context variables

Create an app at [developer.xero.com](https://developer.xero.com/app/manage). Test against the Demo Company: a free organisation with sample data that every Xero account can add (and reset) from the developer portal.

| Context Variable | Secret | Description |
|---|---|---|
| `xero.client.id` | no | Your app's OAuth client id |
| `xero.client.secret` | yes | Your app's client secret (not needed for PKCE apps) |
| `xero.refresh.token` | yes | A user's refresh token (rotates on every refresh) |
| `xero.access.token` | yes | A ready access token (30 minutes); skips refreshing |
| `xero.tenant.id` | no | The organisation id sent as `xero-tenant-id` |
| `xero.custom-connection` | no | `true` to use a Custom Connection (client credentials) |
| `xero.scopes` | no | Scopes to request for a Custom Connection token |
| `xero.token.store` | no | Store name for the opt-in token keeper |
| `xero.token.store-key` | no | Keeper entry key (default `default`) |
| `xero.webhook.key` | yes | The webhook signing key |

Every function that calls the API takes a trailing `opts` Map that overrides context for that call: `access-token`, `refresh-token`, `client-id`, `client-secret`, `custom-connection`, `scopes`, `token-store`, `token-store-key`, `tenant-id`, `if-modified-since`, `idempotency-key` and `retry`. When `opts` carries a credential, context credentials are ignored, so one run can act for many users and organisations; per-call values are never written to context. `::xero::api/resolve(opts)` shows what a call would use.

### Scopes

Apps created on or after 2 March 2026 must use Xero's granular scopes; older apps can keep the broad `accounting.transactions` and `accounting.reports.read` until 13 September 2027. `::xero/SCOPES_FINANCE_OPS` covers everything this package does (`accounting.contacts`, `accounting.settings`, `accounting.invoices`, `accounting.payments`, `accounting.banktransactions`, `accounting.manualjournals`, `accounting.attachments`, and the `accounting.reports.*.read` scopes) plus `offline_access` for a refresh token; `::xero/SCOPES_READ_ONLY` is the `.read` set. `accounting.invoices` also covers credit notes, purchase orders, quotes and items.

### Connecting an organisation

```hot
::oauth ::xero::oauth
::auth ::xero::auth

connect-url fn (state: Str): Str {
  ::oauth/authorize-url("XERO-CLIENT-ID", "https://app.example.com/oauth/xero", ::xero/SCOPES_FINANCE_OPS, state)
}

on-callback fn (request: Map, user-id: Str): Vec {
  cb ::oauth/callback-params(request)
  tokens ::oauth/exchange-code(cb.code, "https://app.example.com/oauth/xero")
  ::auth/keeper-seed("xero-tokens", user-id, tokens)
  // The organisations this user authorised; send one tenantId per call
  ::oauth/connections(tokens.access-token)
}
```

Check `cb.state` before exchanging the code. PKCE apps pass `{code-verifier: verifier}` to both `authorize-url` and `exchange-code` and leave out the client secret. `revoke(refresh-token)` removes every connection the token authorised; `::xero::api/delete-connection(connection-id)` removes one.

### Refresh tokens rotate

Xero returns a new refresh token on **every** refresh; the old one keeps working for a 30-minute grace period and then stops. An unused refresh token expires after 60 days. Every `TokenSet` carries the token to keep in `refresh-token`. One refresh token covers all the organisations the user connected.

The request helpers refresh for you and cache the access token in the run (as a secret, keyed by a hash), so later calls in the run, and later refreshes, use the newest token. To persist it:

- **Token keeper (recommended).** Set `xero.token.store` (or `opts.token-store`) to a store name and key entries by user authorisation (`token-store-key`), not by tenant: two tenant entries holding copies of one refresh token would invalidate each other. The keeper reuses a fresh access token, otherwise refreshes and writes the new tokens back with `::hot::store/put-if`.
- **Yourself.** Read `::xero::auth/current-refresh-token(opts)` before the run ends and save it.

*Race note:* two runs that find the same stale keeper entry both refresh with the same token. Within the grace period both succeed with different new tokens; `put-if` keeps exactly one and the loser uses the winner's tokens. For heavy concurrency, refresh from one scheduled function.

### Custom Connections

A Custom Connection is Xero's paid machine-to-machine option for one organisation (no user consent flow; check availability for your region). Set `xero.custom-connection` to `true` with the client id and secret, or call `::xero::oauth/client-credentials(scopes)`. Tokens last 30 minutes, there is no refresh token, and the tenant header is optional.

## Quick start

```hot
::acc ::xero::accounting
::w ::xero::where
::r ::xero::reports

// AR: every authorised sales invoice with money due, oldest first
unpaid ::acc/list-all("Invoices", {
  where: ::w/all-of([["Type", "==", "ACCREC"], ["Status", "==", "AUTHORISED"], ["AmountDue", ">", 0]]),
  order: "DueDate"
}, 1000)

// Invoice a customer, email it, keep the PDF
contact first(::acc/create-records("Contacts", {Name: "Acme Corp", EmailAddress: "ap@acme.example"}))
invoice first(::acc/create-records("Invoices", {
  Type: "ACCREC",
  Contact: {ContactID: contact.ContactID},
  LineItems: [{Description: "Consulting", Quantity: 10, UnitAmount: 150, AccountCode: "200"}],
  Date: "2026-11-01",
  DueDate: "2026-11-30",
  Status: "AUTHORISED"
}, {idempotency-key: "order-4411-invoice"}))
::acc/email-invoice(invoice.InvoiceID)
pdf ::acc/invoice-pdf(invoice.InvoiceID)

// Reconciliation inputs
bank ::r/flatten-report(::r/bank-summary({fromDate: "2026-10-01", toDate: "2026-10-31"}))
changed ::acc/list-all("BankTransactions", {}, null, {if-modified-since: "2026-10-31T00:00:00"})
```

## Functions by area

**`::xero::accounting`**: `list-page`, `list-all`, `get-record`, `create-records` (PUT), `upsert-records` (POST), `update-record`, `delete-record`, `get-organisation`, `void-invoice`, `delete-draft-invoice`, `archive-contact`, `delete-payment`, `record-pdf`, `invoice-pdf`, `email-invoice`, `online-invoice-url`, `list-attachments`, `upload-attachment` (Bytes), `get-attachment`, `element-errors`, plus `resource-info` and `RESOURCES`. Resources: Contacts, Invoices, CreditNotes, Payments, BankTransactions, BankTransfers, Accounts, Items, ManualJournals, PurchaseOrders, Quotes, TaxRates, TrackingCategories, Organisation.

List params use Xero's names: `where`, `order`, `page`, `pageSize` (up to 1000 where supported), `IDs`, `Statuses`, `ContactIDs`, `InvoiceNumbers`, `searchTerm`, `includeArchived`, `summaryOnly`, `unitdp`. Vec values are comma-joined and everything is percent-encoded. Writes take one record or a Vec; `opts.summarize-errors: false` returns per-record errors (read them with `element-errors`), and `opts.idempotency-key` makes a write safe to retry.

**`::xero::where`**: `condition`, `all-of`, `any-of` (parts are `[field, op, value]` Vecs or expression Strs), `quote`, `guid`, `date`, `raw`, `literal`, `field`. Field names and operators are checked; GUIDs and dates get Xero's `guid(...)` and `DateTime(y,mm,dd)` forms. Xero documents no escape for `"` inside a filter string, so such values are an error rather than a guess: use `searchTerm` or an `IDs`-style parameter, which Xero also recommends for speed.

**`::xero::reports`**: `run-report`, `profit-and-loss`, `balance-sheet`, `trial-balance`, `aged-receivables(contact-id, params)`, `aged-payables(contact-id, params)`, `bank-summary`, `flatten-report`, `column-keys`, `parse-amount`. `flatten-report` returns `{type, section, label, account-id, values, cells}` per row, keyed by the report's header row.

**`::xero::oauth`**: `authorize-url`, `callback-params`, `exchange-code`, `refresh`, `client-credentials`, `revoke`, `connections`, `delete-connection`, `pkce-verifier`, `pkce-challenge`, `token-set`.

**`::xero::auth`**: `access-token`, `current-refresh-token`, `keeper-seed`, `keeper-entry`, `resolve-credentials`.

**`::xero::api`**: `resolve`, `request`, `json`, `bytes`, `connections`, `delete-connection`, `http-error`, `is-retryable`, `rate-limits`, `last-rate-limits`, `parse-date`, `next-page`, `query-string`.

## Webhooks

Xero signs each delivery with your webhook key: `x-xero-signature` is the base64 HMAC-SHA256 of the raw body. When you save the webhook URL, Xero runs an *intent to receive* check, sending signed test payloads (some with a bad signature) and expecting 200 with an empty body for a valid signature and 401 with an empty body for an invalid one, within five seconds. `respond` returns exactly that, and `receive` also parses the events:

```hot
xero-webhook
meta {webhook: {service: "xero", path: "/xero/webhook"}}
fn (request) {
  received ::xero::webhooks/receive(request)
  if(received.verified,
    for-each(received.events, (e) { ::hot::event/send("xero:changed", e) }),
    null)
  received.response
}
```

Events are `{resource-url, resource-id, event-date, event-type, event-category, tenant-id, tenant-type}`; categories include `INVOICE`, `CONTACT` and `CREDITNOTE`. They carry ids only, so fetch the record before acting. Xero signs no timestamp: dedupe on `resource-id` plus `event-date`. Verification uses `::hot::http/raw-body`, compares in constant time and fails closed on an empty key.

## Errors, rate limits and paging

API errors come back as `err(::xero::api/HttpError)` with a readable `message`: validation errors from `Elements[].ValidationErrors` are joined in (`"xero: HTTP 400 ValidationException: A validation exception occurred: Email address must be valid."`), and `validation-errors`, `error-type`, `error-number` and `correlation-id` are separate fields. Token failures are `::xero::oauth/TokenError`s (`invalid_grant` means the user must reconnect).

Limits per organisation: 60 calls a minute, 5,000 a day (1,000 on the free Starter tier), 5 concurrent; 10,000 a minute across the app. `HttpError.rate-limits` and `::xero::api/last-rate-limits(opts)` report `X-MinLimit-Remaining`, `X-DayLimit-Remaining` and `X-AppMinLimit-Remaining`; a 429 also carries `retry-after` and `problem` (`minute`, `day`, `appminute`). This package never waits: check `is-retryable` and give the handler or schedule a Hot `retry` policy (a `day` problem needs the next day). A connection failure on a GET or a write with `idempotency-key` is retried once, immediately; a cached token rejected with 401 is evicted and the call retried once.

Paging: `list-all` walks `page` (1-based), with `pageSize: 1000` where Xero supports it, and stops on Xero's `pagination.pageCount` or a short page. Unpaged resources (Accounts, Items, TaxRates, ...) return in one call. With `If-Modified-Since`, an unchanged collection returns an empty page with `not-modified: true`.

Dates: Xero's JSON dates look like `/Date(1761955200000+0000)/`; `::xero::api/parse-date` turns one into epoch milliseconds.

## Notes and limits

- **Developer pricing (since 2 March 2026).** Apps are on monthly tiers by connected organisations: Starter (free, up to 5 connections, 1,000 calls a day per organisation), Core (A$35 a month, 50 connections, 10 GB egress), Plus (A$245, 1,000 connections, 50 GB), Advanced (A$1,445, 10,000 connections, 250 GB), and Enterprise (by arrangement). Paid tiers bill A$2.40 per GB of egress above the allowance; ingress is unlimited. Check [developer.xero.com/pricing](https://developer.xero.com/pricing) for current figures. Bulk reads cost egress, so prefer `If-Modified-Since`, `summaryOnly` and narrow filters.
- **Demo Company.** Use it for development and these integration tests; reset it from the developer portal when its data gets messy.
- **Statuses, not deletes.** Approved invoices are voided (`void-invoice`), drafts deleted by status, contacts archived; payments are removed with `delete-payment` before an invoice can be voided.
- **Sizes.** Up to 10 attachments of at most 25 MB per record; keep write batches to about 50 records (requests are capped at 3.5 MB).
- **Payroll, bank feeds, projects, assets and files** are separate Xero APIs not covered here.
- **Data handling.** Contacts hold personal data, and bank transactions hold account details. Tokens are cached only as run secrets, keeper entries live in your Hot store (restrict who can read it), and error messages never include tokens or request bodies.

## License

Apache-2.0 - see [LICENSE](LICENSE)
