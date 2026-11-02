# stripe

Stripe API bindings for Hot: PaymentIntents and SetupIntents, Checkout and the customer portal, customers and payment methods, products and prices, subscriptions with items, invoices, coupons and promotion codes, refunds, disputes, payouts, usage meters, and webhook signature verification. Every request is pinned to Stripe API version `2026-10-28.endive`, writes accept idempotency keys, and `::stripe::api/list-all` follows pagination for you. Anything not wrapped is one `::stripe::api/call` away.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/stripe": "1.0.0"
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `stripe.api.key` | yes, unless passed per call | Secret or restricted key: `sk_test_...` / `rk_test_...` in test mode, `sk_live_...` / `rk_live_...` in live mode |
| `stripe.webhook.secret` | for webhooks | The endpoint's signing secret (`whsec_...`), used by `::stripe::webhooks/verify-request` and `construct-event` |
| `stripe.api.version` | no | Override the pinned `Stripe-Version` (default `::stripe/API_VERSION`, `2026-10-28.endive`) |

Test mode is free: use your account's test keys (or a [sandbox](https://docs.stripe.com/sandboxes)) and [test cards](https://docs.stripe.com/testing) such as `pm_card_visa`.

## Quick start

```hot
::customers ::stripe::customers
::pi ::stripe::payment-intents
::refunds ::stripe::refunds
::opts ::stripe::api

customer ::customers/create-customer(::customers/CreateCustomerRequest({
  email: "alice@example.com",
  name: "Alice Smith"
}))

// Charge in one call; the idempotency key makes a retry return the
// original PaymentIntent instead of charging again.
intent ::pi/create-payment-intent(::pi/CreatePaymentIntentRequest({
  amount: 2000,
  currency: "usd",
  customer: customer.id,
  payment_method: "pm_card_visa",
  confirm: true,
  automatic_payment_methods: {enabled: true, allow_redirects: "never"}
}), ::opts/RequestOptions({idempotency_key: "order-42-payment"}))
intent.status  // => "succeeded"

refund ::refunds/create-refund(::refunds/CreateRefundRequest({
  payment_intent: intent.id,
  amount: 500
}), ::opts/RequestOptions({idempotency_key: "order-42-refund-1"}))
```

### Accept a payment with Checkout

```hot
::checkout ::stripe::checkout

session ::checkout/create-checkout-session(::checkout/CreateCheckoutSessionRequest({
  mode: "payment",
  line_items: [{price: "price_...", quantity: 1}],
  success_url: "https://example.com/success?session_id={CHECKOUT_SESSION_ID}",
  cancel_url: "https://example.com/cancel"
}))
// Redirect the customer to session.url; fulfil on checkout.session.completed
```

### Start a subscription

```hot
::subs ::stripe::subscriptions

sub ::subs/create-subscription(::subs/CreateSubscriptionRequest({
  customer: "cus_...",
  items: [{price: "price_..."}],
  discounts: [{promotion_code: "promo_..."}],
  payment_behavior: "default_incomplete",
  payment_settings: {save_default_payment_method: "on_subscription"},
  expand: ["latest_invoice.confirmation_secret"]
}))
// Hand this to Stripe.js to collect the first payment
client-secret sub.latest_invoice.confirmation_secret.client_secret

// Later: cancel now, crediting unused time and recording why
::subs/cancel-subscription(sub.id, ::subs/CancelSubscriptionRequest({
  prorate: true,
  cancellation_details: {feedback: "too_expensive"}
}))
```

### Save a card for later, then let customers manage billing

```hot
::si ::stripe::setup-intents
::portal ::stripe::billing-portal

setup ::si/create-setup-intent(::si/CreateSetupIntentRequest({
  customer: "cus_...",
  automatic_payment_methods: {enabled: true}
}))
// setup.client_secret goes to Stripe.js

portal ::portal/create-portal-session(::portal/CreatePortalSessionRequest({
  customer: "cus_...",
  return_url: "https://example.com/account"
}))
// Redirect to portal.url
```

### Fetch every page

```hot
// Follows has_more / starting_after (or next_page for /search), 100 per page,
// capped at 1000 objects unless you pass a different max (0 = no cap).
active ::stripe::api/list-all("/v1/subscriptions", {status: "active"})
paid ::stripe::api/list-all("/v1/invoices/search", {query: `status:"paid"`}, 250)
```

## Functions

| Namespace | Functions |
|---|---|
| `::stripe::customers` | `create-customer`, `get-customer`, `update-customer`, `delete-customer`, `list-customers`, `search-customers` |
| `::stripe::payment-intents` | `create-payment-intent`, `get-payment-intent`, `update-payment-intent`, `confirm-payment-intent`, `capture-payment-intent`, `cancel-payment-intent`, `list-payment-intents`, `search-payment-intents` |
| `::stripe::setup-intents` | `create-setup-intent`, `get-setup-intent`, `update-setup-intent`, `confirm-setup-intent`, `cancel-setup-intent`, `list-setup-intents` |
| `::stripe::payment-methods` | `get-payment-method`, `attach-payment-method`, `detach-payment-method`, `update-payment-method`, `list-payment-methods` |
| `::stripe::checkout` | `create-checkout-session`, `get-checkout-session`, `update-checkout-session`, `expire-checkout-session`, `list-checkout-sessions`, `list-checkout-session-line-items` |
| `::stripe::billing-portal` | `create-portal-session` |
| `::stripe::products` | Products: `create-product`, `get-product`, `update-product`, `delete-product`, `list-products`, `search-products`. Prices: `create-price`, `get-price`, `update-price`, `list-prices`, `search-prices` |
| `::stripe::subscriptions` | `create-subscription`, `get-subscription`, `update-subscription`, `cancel-subscription`, `pause-subscription`, `resume-subscription`, `list-subscriptions`, `search-subscriptions`; items: `create-subscription-item`, `get-subscription-item`, `update-subscription-item`, `delete-subscription-item`, `list-subscription-items` |
| `::stripe::invoices` | `create-invoice`, `get-invoice`, `update-invoice`, `delete-invoice`, `finalize-invoice`, `pay-invoice`, `send-invoice`, `void-invoice`, `mark-invoice-uncollectible`, `list-invoice-lines`, `create-invoice-preview`, `list-invoices`, `search-invoices`; items: `create-invoice-item`, `delete-invoice-item` |
| `::stripe::coupons` | `create-coupon`, `get-coupon`, `update-coupon`, `delete-coupon`, `list-coupons`; `create-promotion-code`, `get-promotion-code`, `update-promotion-code`, `list-promotion-codes` |
| `::stripe::meters` | `create-meter-event`, `create-meter`, `get-meter`, `list-meters`, `deactivate-meter`, `list-meter-event-summaries` |
| `::stripe::refunds` | `create-refund`, `get-refund`, `update-refund`, `cancel-refund`, `list-refunds` |
| `::stripe::charges` | `get-charge`, `list-charges`, `search-charges` (read-only) |
| `::stripe::disputes` | `list-disputes`, `get-dispute`, `update-dispute` (evidence), `close-dispute` |
| `::stripe::payouts` | `list-payouts`, `get-payout` |
| `::stripe::balance` | `get-balance`, `get-balance-transaction`, `list-balance-transactions` |
| `::stripe::events` | `get-event`, `list-events` |
| `::stripe::webhooks` | `verify-request`, `construct-event`, `parse-signature-header`; endpoints: `create-webhook-endpoint`, `get-webhook-endpoint`, `update-webhook-endpoint`, `delete-webhook-endpoint`, `list-webhook-endpoints` |
| `::stripe::api` | `call`, `list-all`, `request`, `build-request`, `RequestOptions`, `form-encode`, `query-string` |

Request types mirror Stripe's parameter names (`CreateSubscriptionRequest({customer, items, discounts, ...})`), and unset fields are never sent. Most `get-*` functions take an optional params map, e.g. `get-payment-intent(id, {expand: ["latest_charge"]})`.

### Request options

Every write function takes an optional last argument: a `::stripe::api/RequestOptions`, or the same options as a Map (see [Per-call credentials](#per-call-credentials)).

| Field | Map key | Header | Use |
|---|---|---|---|
| `idempotency_key` | `idempotency-key` | `Idempotency-Key` | Safe retries of creates, confirms, captures, refunds, and invoice payments: Stripe replays the first result for 24 hours |
| `stripe_account` | `stripe-account` | `Stripe-Account` | Act on a Connect connected account |
| `stripe_version` | `api-version` | `Stripe-Version` | Use a different API version for this one call |
| `api_key` | `api-key` | `Authorization` | Use another Stripe account's key for this one call |

### Anything else

```hot
// Any endpoint: path relative to https://api.stripe.com; params go in the
// query string for GET/DELETE and the form body for POST.
clock ::stripe::api/call("POST", "/v1/test_helpers/test_clocks", {frozen_time: 1767225600})
::stripe::api/call("POST", `/v1/test_helpers/test_clocks/${clock.id}/advance`, {frozen_time: 1769904000})
::stripe::api/call("DELETE", `/v1/test_helpers/test_clocks/${clock.id}`)
```

## Per-call credentials

Every function that calls Stripe takes an optional last argument, so one run can act for many Stripe accounts and parallel flows don't share credentials. Per-call credentials are masked in call records, and nothing from them is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The secret or restricted key for this call, instead of `stripe.api.key` |
| `api-version` | The `Stripe-Version` for this call, instead of `stripe.api.version` |
| `stripe-account` | The connected account to act for |
| `idempotency-key` | The `Idempotency-Key` for this write |

It always comes last. Where a function also takes params or a request, pass `{}` for those.

```hot
tenant {api-key: "sk_test_tenant_b"}

balance ::stripe::balance/get-balance(tenant)
customer ::stripe::customers/get-customer("cus_123", {}, tenant)
page ::stripe::customers/list-customers(::stripe::customers/ListCustomersRequest({limit: 10}), tenant)
everyone ::stripe::api/list-all("/v1/customers", {}, 1000, tenant)

::stripe::customers/create-customer(
  ::stripe::customers/CreateCustomerRequest({email: "alice@example.com"}),
  {...tenant, idempotency-key: "signup-alice"}
)
```

An `api-key`, `stripe-account` or `idempotency-key` that is present but blank or null is an error naming the key, never a fallback. A `url` outside `https://api.stripe.com` given to `::stripe::api/request` needs an `api-key` in the call: `stripe.api.key` is only ever sent to Stripe. `::stripe::api/resolve(options)` is the one place this is decided.

## Webhooks

`verify-request` checks the `Stripe-Signature` HMAC-SHA256 over the raw request body with a constant-time comparison, accepts any `v1` signature (so secret rolls work), and rejects timestamps more than 300 seconds from now. It fails if `stripe.webhook.secret` is unset, and an empty explicit secret never verifies. An explicit secret is masked in call records. `construct-event` verifies and returns the event, or an Err.

```hot
stripe-webhook
meta {webhook: {service: "stripe", path: "/stripe/webhook"}}
fn (request) {
  event ::stripe::webhooks/construct-event(request)
  cond {
    is-err(event) => { ::hot::http/HttpResponse({status: 400, body: "invalid signature"}) }
    eq(event.type, "checkout.session.completed") => {
      // fulfil event.data.object, keyed on event.id so redeliveries are no-ops
      ::hot::http/HttpResponse({status: 200, body: {received: true}})
    }
    => { ::hot::http/HttpResponse({status: 200, body: {received: true}}) }
  }
}
```

Events are rendered in the webhook endpoint's API version, not the version this package sends. Create endpoints with `api_version: ::stripe/API_VERSION` so event payloads match API responses.

## Errors

Functions return the decoded response body on success, or an `err` wrapping `::stripe::api/HttpError` (`{status, headers, body}`) on a non-2xx response. Stripe's details are in `body.error`: `type` (`card_error`, `invalid_request_error`, `idempotency_error`, ...), `code`, `decline_code`, `message`, and `param`. `::stripe/Error` types that body.

## Notes and limits

- **API version.** Requests send `Stripe-Version: 2026-10-28.endive`, so shapes don't change when someone upgrades the account's default version. Changes since older versions that this package already follows: subscriptions and invoices take `discounts: [{coupon}]` or `[{promotion_code}]` instead of `coupon` (2025-03-31.basil); the invoice's client secret is `confirmation_secret` instead of `payment_intent` (basil); invoice items take `pricing: {price}` instead of `price` (basil); promotion codes take `promotion: {type: "coupon", coupon}` (2025-09-30.clover); PaymentIntents, SetupIntents, and Checkout take `allowed_payment_method_types` instead of `payment_method_types` (2026-09-30.endive); `billing_cycle_anchor` on subscription update and resume is `{type: "now" | "unchanged"}` (endive). If you set `stripe.api.version` to something older, use that version's parameter names with `::stripe::api/call`.
- **Idempotency is opt-in.** Stripe's SDKs generate a random key for every POST so their automatic retries are safe. This package does not retry, so it sends a key only when you pass one. Use a key tied to your own record (an order or job ID) so that a re-run workflow step reuses it.
- **Pagination.** List calls return one page (`limit` up to 100). `list-all` fetches up to 1000 objects by default; raise or remove the cap deliberately on large accounts. Search results can lag writes by about a minute, so don't use search to read back something you just wrote.
- **Amounts** are integers in the currency's smallest unit (cents for USD; whole yen for JPY).
- **Test mode** keys only see test-mode data, and webhook endpoints, products, and customers are separate per mode.
- **Integration tests** (`hot test --project pkg-integration-stripe`) are read-only list calls. They run only when `STRIPE_API_KEY` holds a test-mode key (`sk_test_` or `rk_test_`). Unset or live keys skip every test.

## Documentation

- Stripe API reference: https://docs.stripe.com/api
- API versioning and upgrades: https://docs.stripe.com/upgrades
- Package docs: https://hot.dev/pkg/stripe

## License

Apache-2.0 - see [LICENSE](LICENSE)
