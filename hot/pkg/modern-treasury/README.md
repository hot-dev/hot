# modern-treasury

[Modern Treasury](https://www.moderntreasury.com) API bindings for Hot. Run payment operations on your own bank accounts, and keep a double-entry ledger. The package covers counterparties and external accounts with micro-deposit verification, internal accounts and balance reports, and payment orders over ACH, wire, RTP, check, book and other rails, with approval, cancellation and reversals. It also covers expected payments and reconciliation, bank transactions, ledgers (ledger accounts, balanced ledger transactions, entries and balances), returns, incoming payment details, events, a sandbox simulation, header-cursor pagination, and `X-Signature` webhook verification. Anything not wrapped is one `::modern-treasury::api/call` away.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/modern-treasury": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `modern-treasury.organization.id` | yes, unless every call passes it | Your Organization ID (Developers → API Keys) |
| `modern-treasury.api.key` | yes, unless every call passes it | An API key for that organization |
| `modern-treasury.environment` | no | `"sandbox"` or `"production"`. Sandbox and live organizations share the host `https://app.moderntreasury.com`; this setting only unlocks the sandbox simulation. Unset counts as production, and any other value is an error |
| `modern-treasury.webhook.key` | for webhooks | The webhook key (Developers → Webhooks) |

Requests use HTTP Basic auth, with the organization id as the user and the API key as the password. Every function returns the decoded JSON body.

The sandbox is self-serve: sign up free at [app.moderntreasury.com/sign_up](https://app.moderntreasury.com/sign_up), and the new organization starts in sandbox. No money moves there. Going live with a bank needs a contract with Modern Treasury.

## Per-call credentials

Every function that calls the API takes an optional trailing `opts` Map, so one run can act for many organizations and parallel flows don't share credentials. Nothing from `opts` is written to ctx, and per-call credentials are masked in call records.

| Key | Effect |
|---|---|
| `organization-id`, `api-key` | The credentials for this call, instead of the context ones. Pass both: together they are one Basic auth credential |
| `environment` | Overrides `modern-treasury.environment`. It only unlocks the sandbox simulation; all environments share one host |
| `idempotency-key` | Sent as `Idempotency-Key` |

```hot
// Another organization's sandbox, without touching context
page ::modern-treasury::payment-orders/list-payment-orders({per_page: 10}, {organization-id: "org_b", api-key: "key_b"})
```

An `organization-id` or `api-key` that is present but blank or null is an error naming the key, never a fallback to context. There is no per-call URL. `::modern-treasury::api/resolve(opts)` is the one place this is decided.

## Moving money safely

- **An idempotency key is required.** `create-payment-order`, `create-payment-order-async`, `create-reversal`, `::modern-treasury::returns/create-return` and `verify-external-account` (which sends micro-deposits) take `{idempotency-key: ...}` as the trailing `opts` argument. Without a key they return an Err and send nothing. Modern Treasury keeps a successful POST or PATCH result for 24 hours and returns it for a repeated key. Tie the key to your own record, such as `"invoice-42-payout"`.
- **Nothing is retried automatically.** A call is sent exactly once. After a transport failure, resend the same call with the same key; a new key could pay twice.
- **Agents should not approve payment orders.** Configure approval rules in the Modern Treasury dashboard so that orders an agent creates wait in `needs_approval`. A person then approves them in the dashboard, or through your own approval step that calls `approve-payment-order`.
- **Account and routing numbers stay out of errors and logs.** `HttpError` redacts them. Local validation errors name the missing fields, never their values. Don't log request bodies or counterparty objects yourself.

## Quick start

```hot
::api ::modern-treasury::api
::po ::modern-treasury::payment-orders

// Your bank accounts
accounts ::modern-treasury::internal-accounts/list-internal-accounts({currency: "USD"})
from first(accounts.items)

// Who you're paying, with their bank account
vendor ::modern-treasury::counterparties/create-counterparty({
  name: "Kubernetes Supply Co",
  accounts: [{
    account_type: "checking",
    party_name: "Kubernetes Supply Co",
    account_details: [{account_number: "123456789"}],
    routing_details: [{routing_number: "021000021", routing_number_type: "aba"}]
  }]
}, {idempotency-key: "vendor-17"})

// Pay them by ACH
order ::po/create-payment-order({
  type: "ach",
  direction: "credit",
  amount: 10000,
  originating_account_id: from.id,
  receiving_account_id: first(vendor.accounts).id,
  description: "Invoice 42",
  metadata: {invoice_id: "42"}
}, {idempotency-key: "invoice-42-payout"})
// order.status => "needs_approval" with an approval rule, else "approved"
```

Ledgers record your own balances, such as user wallets:

```hot
::api ::modern-treasury::api
::ledgers ::modern-treasury::ledgers

ledger ::ledgers/create-ledger({name: "Wallets"})
cash ::ledgers/create-ledger-account({ledger_id: ledger.id, name: "Cash", currency: "USD", normal_balance: "debit"})
wallet ::ledgers/create-ledger-account({ledger_id: ledger.id, name: "User 42", currency: "USD", normal_balance: "credit"})

::ledgers/create-ledger-transaction({
  description: "User 42 top-up",
  status: "posted",
  external_id: "topup-42-1",
  ledger_entries: [
    {ledger_account_id: cash.id, direction: "debit", amount: 1000},
    {ledger_account_id: wallet.id, direction: "credit", amount: 1000}
  ]
}, {idempotency-key: "topup-42-1"})

balances ::ledgers/get-ledger-account-balances(wallet.id, {})
// balances.posted_balance.amount => 1000
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::modern-treasury::counterparties` | `create-counterparty`, `get-counterparty`, `list-counterparties`, `update-counterparty`, `delete-counterparty`, `collect-account-details` |
| `::modern-treasury::external-accounts` | `create-external-account`, `get-external-account`, `list-external-accounts`, `update-external-account`, `delete-external-account`, `verify-external-account`, `complete-verification`, `validate-routing-number` |
| `::modern-treasury::internal-accounts` | `list-internal-accounts`, `get-internal-account`, `list-balance-reports`, `get-balance-report`, `latest-balance-report` |
| `::modern-treasury::payment-orders` | `create-payment-order`, `create-payment-order-async`, `get-payment-order`, `list-payment-orders`, `update-payment-order`, `approve-payment-order`, `cancel-payment-order`, `create-reversal`, `get-reversal`, `list-reversals` |
| `::modern-treasury::expected-payments` | `create-expected-payment`, `get-expected-payment`, `list-expected-payments`, `update-expected-payment`, `reconcile-expected-payment`, `delete-expected-payment` |
| `::modern-treasury::transactions` | `list-transactions`, `get-transaction`, `list-line-items` |
| `::modern-treasury::ledgers` | `create-ledger`, `get-ledger`, `list-ledgers`, `update-ledger`; `create-ledger-account`, `get-ledger-account`, `get-ledger-account-balances`, `list-ledger-accounts`, `update-ledger-account`; `create-ledger-transaction`, `get-ledger-transaction`, `list-ledger-transactions`, `update-ledger-transaction`, `reverse-ledger-transaction`, `list-ledger-entries`, `is-balanced` |
| `::modern-treasury::returns` | `list-returns`, `get-return`, `create-return` |
| `::modern-treasury::incoming-payment-details` | `list-incoming-payment-details`, `get-incoming-payment-detail`, `update-incoming-payment-detail` |
| `::modern-treasury::events` | `list-events`, `get-event` |
| `::modern-treasury::simulations` | `create-incoming-payment-detail` (sandbox only) |
| `::modern-treasury::webhooks` | `verify-request`, `verify-signature`, `parse-event`, `construct-event`, `sign` |
| `::modern-treasury::api` | `call`, `resolve`, `ping`, `list-page`, `collect-all`, `next-page-params`, `HttpError`, `is-error-code` |

Create functions take a params Map with Modern Treasury's field names, plus the optional `opts`. Required fields are checked before anything is sent. Unset fields are never sent as JSON `null`. Query params use Rails brackets: `{metadata: {Type: "Loan"}}` becomes `metadata[Type]=Loan`, and `{id: ["a", "b"]}` becomes `id[]=a&id[]=b`.

Reconciliation is automatic. Create an expected payment with amount and date bounds, and Modern Treasury marks it `reconciled` when a bank transaction matches, then sends an `expected_payment.reconciled` webhook. Use `reconcile-expected-payment` only when you matched it yourself.

## Webhooks

Modern Treasury sends `X-Signature`, the hex HMAC-SHA256 of the raw body keyed with your webhook key. `verify-request` compares it in constant time and fails closed. Without `modern-treasury.webhook.key`, the 1-arity form fails the run. An empty explicit key or a missing signature is a rejection.

```hot
mt-webhook
meta {webhook: {service: "modern-treasury", path: "/modern-treasury/webhook"}}
fn (request) {
  cond {
    not(::modern-treasury::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    => {
      e ::modern-treasury::webhooks/parse-event(request)
      // e.topic "payment_order", e.event "completed", e.id, e.webhook_id
      ::hot::http/HttpResponse({status: 200, body: {received: e.webhook_id}})
    }
  }
}
```

Only the body is signed. `X-Event-Time` and the other headers are not, so there is no signed timestamp to enforce a replay window with. Deduplicate on `X-Webhook-ID`, which is stable across retries, and re-fetch the object (for example `get-payment-order(e.id)`) before acting on it. Modern Treasury waits five seconds for a 2xx. It retries 15 times over about a day and a half, then pauses the endpoint. A scheduled `list-events` poll catches anything missed.

## Errors and pagination

A non-2xx response is an Err carrying `HttpError {status, headers, body, message, code, detail, parameter, retry_after}`. `code` is Modern Treasury's error code, such as `parameter_invalid`, `parameter_missing`, `resource_not_found`, `invalid_key`, `conflict` (409, for example a reused idempotency key still in flight) or `too_many_requests`. `parameter` names the offending field. Account and routing numbers are redacted from the body and the message, so an `HttpError` is safe to log.

```hot
out ::modern-treasury::payment-orders/get-payment-order("8c0f4c1e-2b6f-4d7e-9b1a-3f2e8d4c5b6a")
if(::modern-treasury::api/is-error-code(out, "resource_not_found"), println("no such payment order"), println(out.status))
```

List endpoints return bare arrays, with the next cursor in the `X-After-Cursor` header. List functions return one page as `{items, after_cursor, per_page}`. Pass `after_cursor` back with the same filters (`next-page-params` does this). `per_page` defaults to 25, with a maximum of 100. `::modern-treasury::api/collect-all(path, params, limit)` follows every page: by default it stops at 1000 objects, and `0` means no limit.

```hot
waiting ::modern-treasury::api/collect-all("/api/payment_orders", {status: "needs_approval"}, 0)
```

## Data handling

- Counterparty, external account, return and incoming payment detail objects can carry account and routing numbers. Don't log them. Prefer `collect-account-details`, where the counterparty enters their own bank details in a Modern Treasury form.
- Legal entity data (tax ids, birth dates) is sensitive in the same way.
- Keep `modern-treasury.api.key` and `modern-treasury.webhook.key` in Context Variables. They are secret by default.

## Notes and limits

- Amounts are Ints in the currency's smallest unit (cents for USD).
- Payment order statuses change asynchronously, over hours or days. Follow `payment_order.*` webhooks instead of polling in a loop, which Hot doesn't allow.
- Document uploads (multipart) are not wrapped. Neither are legal entities, virtual accounts, invoices, account collection flows, bulk requests, account and routing details sub-resources, ledger account categories, settlements, statements or balance monitors. Use `::modern-treasury::api/call`.
- Live payments need a bank connection, which Modern Treasury sets up under contract. Reversals follow NACHA rules (ACH, within five banking days, for limited reasons).
- Sources: Modern Treasury's [OpenAPI spec](https://github.com/Modern-Treasury/modern-treasury-openapi) and its official [Node](https://github.com/Modern-Treasury/modern-treasury-node) and [Python](https://github.com/Modern-Treasury/modern-treasury-python) SDKs.

## License

Apache-2.0 - see [LICENSE](LICENSE)
