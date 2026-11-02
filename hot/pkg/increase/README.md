# increase

[Increase](https://increase.com) banking API bindings for Hot. Hold deposits, issue account numbers and cards, and move money over ACH, Fedwire, Real-Time Payments, FedNow, checks and book transfers. The package covers entities, accounts, balances and statements, account numbers, external accounts, all six outbound transfer rails with approval and cancellation, inbound transfers with returns, declines and reversals, transactions, cards, events and event subscriptions, sandbox simulations, cursor pagination, and webhook verification with a reusable [Standard Webhooks](https://www.standardwebhooks.com) verifier. Anything not wrapped is one `::increase::api/call` away.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/increase": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `increase.api.key` | yes, unless every call passes `api-key` | An API key from the Increase dashboard. Sandbox and production keys differ |
| `increase.environment` | no | `"sandbox"` (default, `https://sandbox.increase.com`) or `"production"` (`https://api.increase.com`). Any other value is an error, so a typo can't send requests anywhere |
| `increase.webhook.secret` | for webhooks | The Event Subscription's `shared_secret` |
| `increase.transfers.require-approval` | no | `true` sends `require_approval: true` on every transfer that doesn't set it, so each one waits for a person |

Requests use `Authorization: Bearer <key>`. Every function returns the decoded JSON body.

The sandbox is self-serve: sign up in the dashboard, or create an empty sandbox with an unauthenticated `POST https://sandbox.increase.com/groups`, which returns a sandbox API key. No money moves in the sandbox. Production needs an agreement with Increase.

## Per-call credentials

Every function that calls the API takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share keys. Nothing from `opts` is written to ctx, and per-call credentials are masked in call records.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `increase.api.key` |
| `environment` | `"sandbox"` or `"production"` for this call. If it differs from `increase.environment`, it needs an `api-key` in the same `opts`: a context key never goes to another environment's host |
| `idempotency-key` | Sent as `Idempotency-Key` |

```hot
// Tenant B's sandbox, without touching context
page ::increase::accounts/list-accounts({limit: 10}, {api-key: "tenant_b_sandbox_key", environment: "sandbox"})
```

An `api-key` or `environment` that is present but blank or null is an error naming the key, never a fallback to context. There is no per-call URL: requests only go to Increase's two hosts. `::increase::api/resolve(opts)` is the one place this is decided.

## Moving money safely

These rules hold for every transfer, return, decline and reversal in the package:

- **An idempotency key is required.** `create-ach-transfer`, `create-wire-transfer` and the other transfer creates, plus `return-inbound-ach-transfer`, `decline-inbound-ach-transfer`, `reverse-inbound-wire-transfer` and `return-inbound-rtp-transfer`, take `{idempotency-key: ...}` as the trailing `opts` argument. Without one they return an Err and send nothing. Increase stores the result for the key, so resending the same call can never make a second transfer. Tie the key to your own record, such as `"invoice-42-payout"`.
- **Nothing is retried automatically.** A call is sent exactly once. If it fails in transit, look the transfer up with `::increase::api/find-by-idempotency-key`, and resend the same call with the same key only if it isn't there.
- **Agents should not approve transfers.** Create transfers with `require_approval: true`, or set `increase.transfers.require-approval`, so they wait in `pending_approval`. A person then approves them in the dashboard or through your own approval step that calls `approve-*`. A wire, RTP or FedNow transfer can't be recalled once it is sent.
- **Account and routing numbers stay out of errors and logs.** `HttpError` redacts them. Local validation errors name the missing fields, never their values. Don't log request bodies, Account Number objects or card details yourself.

## Quick start

```hot
::api ::increase::api
::accounts ::increase::accounts
::ach ::increase::ach-transfers

// Find an open account and its balance (cents)
page ::accounts/list-accounts({"status.in": ["open"], limit: 10})
account first(page.data)
balance ::accounts/get-balance(account.id)

// Save the counterparty's bank account once
vendor ::increase::external-accounts/create-external-account({
  routing_number: "101050001",
  account_number: "987654321",
  description: "Landlord",
  account_holder: "business"
}, {idempotency-key: "vendor-17-bank"})

// Pay it by ACH, held for a person to approve
transfer ::ach/create-ach-transfer({
  account_id: account.id,
  external_account_id: vendor.id,
  amount: 10000,
  statement_descriptor: "Invoice 42",
  require_approval: true
}, {idempotency-key: "invoice-42-payout"})
// transfer.status => "pending_approval"
```

In the sandbox, you can drive a transfer through its lifecycle:

```hot
::sim ::increase::simulations

transfer-id "ach_transfer_uoxatyh3lt5evrsdvo7q"
::increase::ach-transfers/approve-ach-transfer(transfer-id)
::sim/submit-ach-transfer(transfer-id)
::sim/settle-ach-transfer(transfer-id)

// Money arriving from another bank
::sim/create-inbound-ach-transfer({account_number_id: "account_number_v18nkfqm6afpsrvy82b2", amount: 2500})
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::increase::entities` | `create-entity`, `get-entity`, `list-entities`, `update-entity` |
| `::increase::accounts` | `create-account`, `get-account`, `list-accounts`, `update-account`, `close-account`, `get-balance`; `create-account-number`, `get-account-number`, `list-account-numbers`, `update-account-number`; `list-account-statements`, `get-account-statement` |
| `::increase::external-accounts` | `create-external-account`, `get-external-account`, `list-external-accounts`, `update-external-account`, `archive-external-account` |
| `::increase::ach-transfers` | `create-ach-transfer`, `get-ach-transfer`, `list-ach-transfers`, `approve-ach-transfer`, `cancel-ach-transfer` |
| `::increase::wire-transfers` | `create-wire-transfer`, `get-wire-transfer`, `list-wire-transfers`, `approve-wire-transfer`, `cancel-wire-transfer` |
| `::increase::rtp-transfers` | `create-rtp-transfer`, `get-rtp-transfer`, `list-rtp-transfers`, `approve-rtp-transfer`, `cancel-rtp-transfer` |
| `::increase::fednow-transfers` | `create-fednow-transfer`, `get-fednow-transfer`, `list-fednow-transfers`, `approve-fednow-transfer`, `cancel-fednow-transfer` |
| `::increase::check-transfers` | `create-check-transfer`, `get-check-transfer`, `list-check-transfers`, `approve-check-transfer`, `cancel-check-transfer`, `stop-payment` |
| `::increase::account-transfers` | `create-account-transfer`, `get-account-transfer`, `list-account-transfers`, `approve-account-transfer`, `cancel-account-transfer` |
| `::increase::inbound-transfers` | `list-`/`get-inbound-ach-transfer(s)`, `decline-inbound-ach-transfer`, `return-inbound-ach-transfer`, `create-notification-of-change`; `list-`/`get-inbound-wire-transfer(s)`, `reverse-inbound-wire-transfer`; `list-`/`get-inbound-rtp-transfer(s)`, `return-inbound-rtp-transfer`; `list-`/`get-inbound-fednow-transfer(s)` |
| `::increase::transactions` | `list-transactions`, `get-transaction`, `list-pending-transactions`, `get-pending-transaction`, `list-declined-transactions`, `get-declined-transaction` |
| `::increase::cards` | `create-card`, `get-card`, `list-cards`, `update-card`, `get-card-details`, `create-details-iframe`; `create-physical-card`, `get-physical-card`, `list-physical-cards`, `update-physical-card` |
| `::increase::events` | `list-events`, `get-event`, `parse-event`, `fetch-associated-object`; `create-event-subscription`, `get-event-subscription`, `list-event-subscriptions`, `update-event-subscription`, `delete-event-subscription` |
| `::increase::simulations` | `settle-ach-transfer`, `return-ach-transfer`, `submit-ach-transfer`, `submit-wire-transfer`, `complete-rtp-transfer`, `complete-fednow-transfer`, `mail-check-transfer`, `create-inbound-ach-transfer`, `create-inbound-wire-transfer`, `create-inbound-rtp-transfer`, `create-inbound-fednow-transfer`, `authorize-card`, `settle-card`, `submit-check-deposit`, `return-check-deposit`, `reject-check-deposit`, `simulate` |
| `::increase::webhooks` | `verify-request`, `construct-event`, `signing-key` |
| `::increase::standard-webhooks` | `verify`, `verify-request`, `verify-at`, `sign`, `decode-secret`, `endpoint-verification-response` |
| `::increase::api` | `call`, `resolve`, `list-page`, `collect-all`, `next-page-params`, `find-by-idempotency-key`, `HttpError`, `is-error-type`, `current-environment`, `is-sandbox` |

Create functions take a params Map with Increase's field names, plus the optional `opts`. Required fields are checked before anything is sent. Unset fields are never sent as JSON `null`. List filters use Increase's names: `{"status.in": ["open"]}` and `{status: {in: ["open"]}}` both become `status.in=open`, and `{created_at: {after: "2026-01-01T00:00:00Z"}}` becomes `created_at.after=...`.

Every simulation errs before sending anything unless the call's environment is `"sandbox"`, so a simulation can't reach production.

## Webhooks

Increase signs deliveries with Standard Webhooks. Each delivery has three headers: `webhook-id` (the Event id), `webhook-timestamp` (Unix seconds) and `webhook-signature` (`v1,<base64 HMAC-SHA256>`, space-separated while a secret rotates). The signed content is `{id}.{timestamp}.{raw body}`, keyed with the raw bytes of the subscription's shared secret. `verify-request` accepts a delivery when any `v1` signature matches, each compared in constant time, and the timestamp is within five minutes in either direction. It fails closed. Without `increase.webhook.secret` the 1-arity form fails the run. An empty explicit secret, a missing header or a non-numeric timestamp is a rejection.

```hot
increase-webhook
meta {webhook: {service: "increase", path: "/increase/webhook"}}
fn (request) {
  cond {
    not(::increase::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    => {
      event ::increase::events/parse-event(::hot::http/raw-body(request))
      // Events are thin: re-fetch the object and act on its current state.
      object ::increase::events/fetch-associated-object(event)
      ::hot::http/HttpResponse({status: 200, body: {received: event.id}})
    }
  }
}
```

Return a 2xx quickly and do the work asynchronously. Increase retries failed deliveries for about three days in production, and not at all in the sandbox, so deduplicate on the event `id`. Events stay listable for 30 days. A scheduled `list-events({order_by: {field: "created_at", direction: "ascending"}})` poll from a stored cursor catches anything a webhook missed.

`::increase::standard-webhooks` works for any Standard Webhooks sender. `decode-secret("whsec_...")` turns a standard secret into the HMAC key, and `verify(headers, raw-body, key)` checks a delivery. It is tested against the reference vectors from the Standard Webhooks repository, including multiple signatures, unknown versions, unpadded secrets and a binary body.

## Errors and pagination

A non-2xx response is an Err carrying `HttpError {status, headers, body, message, type, title, detail, reason, resource_id, errors, retry_after}`. `type` is Increase's error type, such as `invalid_parameters_error`, `invalid_operation_error`, `idempotency_key_already_used_error` (with `resource_id`), `object_not_found_error`, `insufficient_permissions_error`, `rate_limited_error` or `invalid_api_key_error` (with `reason`, for example `wrong_environment`). `message` is one readable line. Values under keys like `account_number` and `routing_number` are replaced with `"[redacted]"`, and runs of eight or more digits in text are masked to their last four, so an `HttpError` is safe to log.

```hot
out ::increase::ach-transfers/create-ach-transfer({account_id: "account_in71c4amph0vgo2qllky", external_account_id: "external_account_ukk55lr923a3ac0pp7iv", amount: 10000, statement_descriptor: "Invoice 42"},
  {idempotency-key: "invoice-42-payout"})
cond {
  ::increase::api/is-error-type(out, "idempotency_key_already_used_error") => { println("this key already made a different transfer") }
  is-err(out) => { println(err-message(out)) }
  => { println(out.status) }
}
```

List functions return one page: `{data, next_cursor}`. Pass `next_cursor` back as `cursor`, with the same filters (`next-page-params` does this). `::increase::api/collect-all(path, params, limit)` follows every page: by default it stops at 1000 objects, and `0` means no limit.

```hot
page ::increase::transactions/list-transactions({account_id: "account_in71c4amph0vgo2qllky", limit: 100})
everything ::increase::api/collect-all("/transactions", {account_id: "account_in71c4amph0vgo2qllky"}, 0)
```

## Data handling

- Account Number objects, External Account create bodies and inbound transfers carry full account and routing numbers. Don't log them, and show people only the last four digits.
- Entity bodies carry tax ids, birth dates and identification numbers.
- `get-card-details` returns the card number, verification code and PIN in the clear, which brings your system into PCI scope. Prefer `create-details-iframe`, which shows them to the cardholder without them passing through you.
- Keep `increase.api.key` and `increase.webhook.secret` in Context Variables. They are secret by default.

## Notes and limits

- Amounts are Ints in the minor unit (cents). ACH transfers with a negative `amount` are debits, and need ACH debit approval on your program.
- Statuses change asynchronously over hours or days (ACH returns can arrive days later). Follow `*.updated` events instead of polling in a loop, which Hot doesn't allow.
- Uploading files (check deposit images, documents) is not wrapped. Create the file in the dashboard or with your own multipart request, then pass its `file_id`.
- Not wrapped: card disputes, card push transfers, digital wallets, lockboxes, IntraFi, Swift transfers, wire drawdown requests, ACH prenotifications, programs, exports, OAuth and real-time decisions. Use `::increase::api/call`.
- Production access, ACH debits, card issuing and some rails need a contract and program setup with Increase.
- Sources: Increase's OpenAPI spec (`https://increase.com/openapi.json`) and its official [Node](https://github.com/Increase/increase-node) and [Python](https://github.com/Increase/increase-python) SDKs.

## License

Apache-2.0 - see [LICENSE](LICENSE)
