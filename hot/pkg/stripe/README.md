# stripe

Stripe API bindings for Hot. Payments, customers, checkout, subscriptions, invoicing, refunds, and webhook signature verification.

## Setup

Add your Stripe secret key as a context variable:

| Context Variable | Required | Description |
|---|---|---|
| `stripe.api.key` | yes | Secret key (`sk_test_...` or `sk_live_...`) |
| `stripe.webhook.secret` | for webhooks | Endpoint signing secret (`whsec_...`) |

## Modules

| Namespace | Covers |
|---|---|
| `::stripe::customers` | Customers CRUD, list, search |
| `::stripe::payment-intents` | PaymentIntents create/confirm/capture/cancel |
| `::stripe::payment-methods` | Attach, detach, list payment methods |
| `::stripe::checkout` | Stripe-hosted Checkout Sessions |
| `::stripe::subscriptions` | Subscriptions lifecycle, search |
| `::stripe::products` | Products and Prices |
| `::stripe::invoices` | Invoices and invoice items |
| `::stripe::refunds` | Refunds |
| `::stripe::balance` | Account balance and balance transactions |
| `::stripe::events` | Event log |
| `::stripe::webhooks` | Signature verification + endpoint management |
| `::stripe::api` | Authenticated raw requests for anything not covered |

## Quick Start

### Accept a payment with Checkout

```hot
::checkout ::stripe::checkout

session ::checkout/create-checkout-session(::checkout/CreateCheckoutSessionRequest({
  mode: "payment",
  line_items: [{price: "price_...", quantity: 1}],
  success_url: "https://example.com/success?session_id={CHECKOUT_SESSION_ID}",
  cancel_url: "https://example.com/cancel"
}))
// Redirect the customer to session.url
```

### Create a customer and subscription

```hot
::customers ::stripe::customers
::subs ::stripe::subscriptions

customer ::customers/create-customer(::customers/CreateCustomerRequest({
  email: "alice@example.com",
  name: "Alice Smith"
}))

sub ::subs/create-subscription(::subs/CreateSubscriptionRequest({
  customer: customer.id,
  items: [{price: "price_..."}],
  payment_behavior: "default_incomplete",
  expand: ["latest_invoice.payment_intent"]
}))
```

### Handle webhooks (always verify!)

```hot
::webhooks ::stripe::webhooks

stripe-webhook
meta {webhook: {service: "stripe", path: "/stripe/webhook"}}
fn (request) {
  if(not(::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    handle-event(request.body)
  )
}

handle-event fn cond (event: Map): Any {
  eq(event.type, "payment_intent.succeeded") => { fulfill-order(event.data.object) }
  eq(event.type, "checkout.session.completed") => { activate(event.data.object) }
  => { "ignored" }
}
```

### Anything else

Every Stripe endpoint is reachable through the authenticated raw request helper. Bodies are form-encoded automatically using Stripe's bracket syntax (`metadata[key]=value`) — Stripe does not accept JSON bodies.

```hot
response ::stripe::api/request("POST", `${::stripe/BASE_URL}/v1/coupons`, {}, {
  percent_off: 25,
  duration: "once"
})
```

## Errors

Functions return the decoded response body on success, or an `err` wrapping `::stripe::api/HttpError` (`{status, headers, body}`) on failure. Stripe's structured error details are in `error.body.error`.

## Documentation

Full documentation available at [hot.dev/pkg/stripe](https://hot.dev/pkg/stripe)

## License

Apache-2.0 - see [LICENSE](LICENSE)
