# quickbooks

QuickBooks Online Accounting API (v3) bindings for Hot, built for agentic finance ops: reconciliation, invoicing, AP/AR and reporting. OAuth 2.0 with rotating refresh tokens (a per-run token cache and an opt-in store-backed token keeper), sandbox and production companies, a pinned `minorversion`, CRUD with sparse updates for the core entities, invoice send and PDF, the SQL-like query endpoint with safe literals and paging, reports with a row flattener, batch, change data capture, readable `Fault` errors with `intuit_tid`, and webhook verification for both the legacy and the CloudEvents payloads.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/quickbooks": "0.1.0"
}
```

## Auth and context variables

Create an app at [developer.intuit.com](https://developer.intuit.com) with the `com.intuit.quickbooks.accounting` scope. Every Intuit developer account includes free sandbox companies for testing.

| Context Variable | Secret | Description |
|---|---|---|
| `quickbooks.client.id` | no | Your app's OAuth client id |
| `quickbooks.client.secret` | yes | Your app's OAuth client secret |
| `quickbooks.refresh.token` | yes | A company's refresh token (rotates; see below) |
| `quickbooks.access.token` | yes | A ready access token; skips refreshing |
| `quickbooks.realm.id` | no | The company id (`realmId`) |
| `quickbooks.environment` | no | `sandbox` or `production` (default). Anything else is an error |
| `quickbooks.minorversion` | no | Overrides the pinned `minorversion` (75) |
| `quickbooks.token.store` | no | Store name for the opt-in token keeper |
| `quickbooks.webhook.verifier` | yes | The webhook verifier token |

Every function that calls the API takes a trailing `opts` Map that overrides context for that call: `access-token`, `refresh-token`, `client-id`, `client-secret`, `token-store`, `token-store-key`, `realm-id`, `environment`, `minorversion`, `request-id` and `retry`. When `opts` carries a credential, context credentials are ignored, so one run can act for many companies (and parallel flows never share per-call values: nothing is written to context except the hashed, secret token cache). `::quickbooks::api/resolve(opts)` shows what a call would use.

### Connecting a company

```hot
::oauth ::quickbooks::oauth
::auth ::quickbooks::auth

connect-url fn (state: Str): Str {
  ::oauth/authorize-url("ABcd123", "https://app.example.com/oauth/quickbooks", state)
}

// The redirect handler: Intuit sends ?code=...&state=...&realmId=...
on-callback fn (request: Map): Map {
  cb ::oauth/callback-params(request)
  tokens ::oauth/exchange-code(cb.code, "https://app.example.com/oauth/quickbooks")
  // Persist the refresh token and realm id per company, e.g. in the keeper:
  ::auth/keeper-seed("quickbooks-tokens", cb.realm-id, tokens)
  {realm-id: cb.realm-id}
}
```

Check `cb.state` against the value you issued before exchanging the code. `revoke(refresh-token)` disconnects the app.

### Refresh tokens rotate

Access tokens last an hour. Refresh tokens last 100 days from their last use and at most five years in all (`refresh-token-hard-expires-in`). Intuit rotates them: `::quickbooks::oauth/refresh` may return a different refresh token, and once it does the old value stops working. Every `TokenSet` carries the token to keep in `refresh-token` (with `rotated: true` when it changed). Persist it every time.

The request helpers refresh for you and cache the access token in the run (as a secret, keyed by a hash of the app and refresh token), so a run that makes 200 calls refreshes once. Two ways to persist a rotated refresh token:

- **Token keeper (recommended).** Set `quickbooks.token.store` (or `opts.token-store`) to a store name. Entries are keyed by realm id. Each run reuses the stored access token while it is fresh, otherwise refreshes and writes the new tokens back with `::hot::store/put-if`.
- **Yourself.** Read `::quickbooks::auth/current-refresh-token(opts)` at the end of the run and save it.

*Race note:* two runs that find the same stale keeper entry both refresh. `put-if` lets exactly one write win; the loser re-reads and uses the winner's tokens. Intuit normally returns the same refresh token within a day, so both refreshes succeed; if many concurrent runs refresh the same company, route refreshing through one scheduled function.

## Quick start

```hot
::qb ::quickbooks::entities
::q ::quickbooks::query
::r ::quickbooks::reports

// Find a customer safely: literals are escaped
name "O'Brien Supplies"
customer first(::q/query(`select * from Customer where DisplayName = ${::q/literal(name)}`).items)

// Invoice, send, PDF
invoice ::qb/create-entity("Invoice", {
  CustomerRef: {value: customer.Id},
  Line: [{DetailType: "SalesItemLineDetail", Amount: 1200.0, SalesItemLineDetail: {ItemRef: {value: "1"}}}]
}, {request-id: "order-4411-invoice"})
::qb/send-entity("Invoice", invoice.Id, "ap@obrien.example")
pdf ::qb/entity-pdf("Invoice", invoice.Id)

// AR: every open invoice
open ::q/query-all(::q/select-from("Invoice", [["Balance", ">", 0]], "DueDate"))

// Reporting
pnl ::r/profit-and-loss({start_date: "2026-01-01", end_date: "2026-09-30"})
rows ::r/flatten-report(pnl)
```

## Functions by area

**`::quickbooks::entities`**: `create-entity`, `get-entity`, `update-entity` (full), `sparse-update`, `delete-entity`, `void-entity`, `deactivate-entity`, `send-entity`, `entity-pdf`, `send-invoice`, `invoice-pdf`, plus `entity-info`, `canonical-name` and `ENTITIES`. Supported: Customer, Vendor, Item, Account, Class, Department, Employee (name lists: create, read, update, deactivate), TaxCode (read), Invoice, Estimate, SalesReceipt, CreditMemo, Payment (with send and PDF), Bill, BillPayment, Purchase, JournalEntry, Deposit, Transfer, and CompanyInfo. Voiding works for Invoice, SalesReceipt, Payment and BillPayment.

Updates need the entity's current `SyncToken` (optimistic locking); a stale one fails with code 5010, so read the entity again and retry. Use `sparse-update`: a full `update-entity` clears every writable field you leave out. Name-list entities cannot be deleted; `deactivate-entity` sets `Active: false`.

**`::quickbooks::query`**: `query` (one page), `query-page`, `query-all` (STARTPOSITION/MAXRESULTS paging up to 1000 per page, with an optional limit), `count-rows`, `select-from`, `condition`, `literal`, `field`, `to-page`. `literal` single-quotes strings and backslash-escapes `\` and `'`; `condition` also checks field names and operators.

**`::quickbooks::reports`**: `run-report`, `profit-and-loss`, `balance-sheet`, `cash-flow`, `aged-receivables`, `aged-payables`, `general-ledger`, `transaction-list`, `flatten-report`, `column-keys`, `parse-amount`. Params use Intuit's names (`start_date`, `end_date`, `date_macro`, `accounting_method`, `summarize_column_by`, `customer`, `vendor`, `columns`, ...). `flatten-report` returns one record per detail row and section total with `type`, `label`, `id`, `values` (keyed by column), `cells`, `path`, `group` and `depth`.

**`::quickbooks::batch`**: `batch` (up to 30 items), `create-item`, `update-item`, `delete-item`, `query-item`, `build-batch`, `parse-results`. Each result is `{bid, ok, entity, value}` or `{bid, ok: false, message, errors}`.

**`::quickbooks::cdc`**: `changes-since(entities, changed-since)` returns `{changed, deleted, time}`: rows per entity, deleted ids per entity, and the server time to use next. The look-back is at most 30 days and each entity type returns at most 1000 changes per call.

**`::quickbooks::company`**: `company-info`, `preferences`.

**`::quickbooks::oauth`**: `authorize-url`, `callback-params`, `exchange-code`, `refresh`, `revoke`, `token-set`, `basic-auth`.

**`::quickbooks::auth`**: `access-token`, `current-refresh-token`, `keeper-seed`, `keeper-entry`, `resolve-credentials`.

**`::quickbooks::api`**: `resolve`, `request`, `json`, `bytes`, `http-error`, `fault-details`, `is-retryable`, `company-url`, `query-string`.

## Webhooks

Intuit signs each delivery with your app's verifier token: `intuit-signature` is the base64 HMAC-SHA256 of the raw body. `verify-request` checks it over `::hot::http/raw-body` in constant time and fails closed on an empty verifier; `receive` verifies, parses and gives you the response to return.

```hot
quickbooks-webhook
meta {webhook: {service: "quickbooks", path: "/quickbooks/webhook"}}
fn (request) {
  received ::quickbooks::webhooks/receive(request)
  if(received.verified,
    for-each(received.events, (e) { ::hot::event/send("quickbooks:changed", e) }),
    null)
  received.response
}
```

Intuit moved webhooks to the CloudEvents format in 2026 (a JSON array of events with `type: "qbo.invoice.created.v1"`, `intuitaccountid` and `intuitentityid`); the migration deadline was May 2026. `parse-events` reads that format and the legacy `eventNotifications` payload into the same records: `{format, realm-id, entity, id, operation, last-updated}` plus `event-id`, `type` and `data` for CloudEvents and `deleted-id` for legacy merges. Operations use the legacy names (`Create`, `Update`, `Delete`, `Merge`, `Void`, `Emailed`).

Intuit signs no timestamp, so deliveries can be replayed: dedupe on `event-id` (or entity id plus `last-updated`) and re-read the entity before acting. Acknowledge quickly and do the work in an event handler; use `::quickbooks::cdc` to catch up on missed deliveries.

## Errors, retries and paging

API errors come back as `err(::quickbooks::api/HttpError)` with a readable `message` built from Intuit's `Fault` (`"quickbooks: HTTP 400 ValidationFault 6240: Duplicate Name Exists Error: ... (intuit_tid: 1-6730...)"`), plus `fault-type`, `code`, `detail`, `element`, every error in `errors`, and `intuit-tid` to quote to Intuit support. A 2xx response whose body is a `Fault` is also an error. Token failures are `::quickbooks::oauth/TokenError`s (`invalid_grant` means the company must reconnect).

This package never waits. A connection failure on a GET, a query, or a write with `request-id` is retried once, immediately. 429 throttling (about 500 requests a minute and 10 concurrent requests per company; 40 batch requests a minute) and 5xx are errors: check `is-retryable` and give the handler or schedule a Hot `retry` policy. A cached token rejected with 401 is evicted and the call retried once with a fresh one.

Paging: `query-page` and `query-all` add `STARTPOSITION`/`MAXRESULTS` (1000 per page); a short page ends the loop. Don't put those keywords in a query passed to them.

## Notes and limits

- **minorversion.** Intuit retired minor versions 1-74 on 1 August 2025; requests without one, or with an older one, are served as 75. This package pins `75` and rejects lower values. Override with `quickbooks.minorversion` or `opts.minorversion` when you need a newer schema.
- **API metering.** Under the Intuit App Partner Program, calls are classed as Core (writes such as creating invoices; free and unmetered) or CorePlus (most reads: queries, reads, reports, CompanyInfo), which are metered. The free Builder tier includes 500,000 CorePlus credits a month and blocks CorePlus calls beyond that; paid tiers (Silver, Gold, Platinum) raise the allowance and add Premium APIs. Prefer CDC and narrow `where` clauses over repeated full queries.
- **Sandbox.** Sandbox companies are free and come with sample data. Set `quickbooks.environment` to `sandbox`; sandbox tokens and realm ids do not work against production.
- **Payments.** Card and ACH processing (the QuickBooks Payments API, scope `com.intuit.quickbooks.payment`) is a separate API not covered here; `Payment` entities here record payments against invoices.
- **Data handling.** Customer, vendor and employee records hold personal data, and payment records may hold bank details. Tokens are cached only as run secrets, keeper entries live in your Hot store (restrict who can read it), and error messages never include tokens or request bodies. Intuit's error `Detail` can echo field values, so treat error messages as account data.

## License

Apache-2.0 - see [LICENSE](LICENSE)
