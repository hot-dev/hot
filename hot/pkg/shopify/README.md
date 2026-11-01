# shopify

Shopify Admin bindings for Hot over the GraphQL Admin API, pinned to API version `2026-10`. It covers orders, fulfillments with tracking, inventory, products and variants, customers, draft orders, metafields and webhook subscriptions. List functions page with cursors, mutation `userErrors` come back as errs, and webhook deliveries can be verified. `::shopify::api/graphql` runs any other query or mutation.

Shopify is GraphQL-first: the REST Admin API is legacy for new apps, so this package doesn't wrap it.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/shopify": "0.1.0"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `shopify.shop` | yes | The shop, as `acme` or `acme.myshopify.com` (both work; `https://` and paths are stripped) |
| `shopify.access.token` | yes | An Admin API access token (`shpat_...`) from a custom app or your app's OAuth flow |
| `shopify.api.version` | no | Overrides the pinned `2026-10`, e.g. `"2027-01"` |
| `shopify.webhook.secret` | for webhooks | The app's client secret (or, for webhooks created in the Shopify admin, the store's webhook signing secret) |

The token needs the scopes for what you call: `read_orders`/`write_orders`, `read_products`/`write_products`, `read_inventory`/`write_inventory`, `read_customers`/`write_customers`, `write_draft_orders`, and one of the `write_*_fulfillment_orders` scopes. Orders older than 60 days also need `read_all_orders`, and customer names, emails and addresses are protected customer data that Shopify must approve.

## Quick start

```hot
::orders ::shopify::orders
::products ::shopify::products

// One page, newest first, then the next one
page ::orders/list-orders({first: 20, query: "financial_status:paid"})
more ::orders/list-orders({first: 20, query: "financial_status:paid", after: page.pageInfo.endCursor})

// Or let the package follow cursors, up to a limit
recent ::orders/collect-orders({query: "created_at:>=2026-10-01"}, 500)

hits ::products/search-products("title:*sticker* status:active", {first: 10})

// Anything else via raw GraphQL
shop ::shopify::api/graphql("{ shop { name currencyCode } }")
```

## Functions

Every `list-*` / `search-*` function returns one page as a connection, `{nodes, pageInfo: {hasNextPage, endCursor}}`, and takes `first` (default 50, max 250), `after`, `query` (Shopify search syntax), `sortKey` and `reverse`. Each `collect-*` function follows the cursors and returns a Vec of up to `max-items` nodes. `::shopify::api/collect(page-fn, options, max-items)` does the same for any page function.

| Namespace | Functions |
|---|---|
| `::shopify::orders` | `list-orders`, `collect-orders`, `get-order`, `cancel-order`, `close-order`, `reopen-order`, `mark-order-as-paid`, `add-tags`, `remove-tags` |
| `::shopify::fulfillments` | `list-fulfillment-orders`, `create-fulfillment`, `update-tracking` |
| `::shopify::inventory` | `get-inventory-levels`, `adjust-quantities`, `set-quantities`, `set-on-hand-quantities` |
| `::shopify::products` | `list-products`, `search-products`, `collect-products`, `get-product`, `create-product`, `update-product`, `update-variants`, `delete-product` |
| `::shopify::customers` | `list-customers`, `search-customers`, `collect-customers`, `find-customer-by-email`, `get-customer`, `create-customer`, `update-customer` |
| `::shopify::draft-orders` | `create-draft-order`, `get-draft-order`, `complete-draft-order` |
| `::shopify::metafields` | `set-metafields`, `set-metafield`, `get-metafield`, `list-metafields`, `delete-metafields` |
| `::shopify::webhooks` | `verify-request`, `topic`, `shop-domain`, `webhook-id`, `event-id`, `delivery`, `create-subscription`, `list-subscriptions`, `delete-subscription` |
| `::shopify::api` | `graphql`, `mutate`, `collect`, `graphql-url`, `normalize-shop`, `api-version` |

Mutation inputs are Shopify's own input types (`ProductCreateInput`, `CustomerInput`, `DraftOrderInput`, ...), so field names are camelCase as in the GraphQL docs.

### Orders and fulfillment

```hot
::orders ::shopify::orders
::fulfillments ::shopify::fulfillments

order ::orders/get-order("gid://shopify/Order/148977776")

// Ship everything still open on the order, with tracking
open ::fulfillments/list-fulfillment-orders(order.id, {query: "status:open"})
shipped ::fulfillments/create-fulfillment(first(open.nodes).id, {
  company: "UPS",
  number: "1Z001985YW99744790"
}, {notifyCustomer: true})

::orders/add-tags(order.id, ["shipped-by-hot"])

// Cancel, refund to the original payment method, restock
::orders/cancel-order(order.id, "CUSTOMER", true, {
  refundMethod: {originalPaymentMethodsRefund: true},
  notifyCustomer: true
})
```

### Inventory

Since API `2026-04`, Shopify requires an idempotency key and a `changeFromQuantity` on every inventory change. `changeFromQuantity` is the quantity you expect before the write, or null to skip the check. The package sends both: changes without `changeFromQuantity` get an explicit null, and each call gets a fresh UUID key unless you pass `idempotencyKey`. Pass your own key when the call might be retried.

```hot
::inventory ::shopify::inventory

levels ::inventory/get-inventory-levels("gid://shopify/InventoryItem/30322695")
location-id first(levels.inventoryLevels.nodes).location.id

::inventory/adjust-quantities([
  {inventoryItemId: levels.id, locationId: location-id, delta: -2}
], {reason: "damaged", idempotencyKey: "damage-ticket-881"})

::inventory/set-on-hand-quantities([
  {inventoryItemId: levels.id, locationId: location-id, quantity: 40, changeFromQuantity: 42}
], {reason: "cycle_count_available"})
```

### Products, customers, draft orders, metafields

```hot
::products ::shopify::products
::customers ::shopify::customers
::draft-orders ::shopify::draft-orders
::metafields ::shopify::metafields

product ::products/create-product({title: "Cool socks", status: "DRAFT", tags: ["socks"]})
variant first(product.variants.nodes)
::products/update-variants(product.id, [{id: variant.id, price: "19.00", inventoryItem: {sku: "SOCK-1"}}])
::products/update-product({id: product.id, status: "ACTIVE"})

customer ::customers/find-customer-by-email("ada@example.com")
ada if(is-null(customer), ::customers/create-customer({email: "ada@example.com", firstName: "Ada"}), customer)

draft ::draft-orders/create-draft-order({
  email: "ada@example.com",
  lineItems: [{variantId: variant.id, quantity: 2}]
})
completed ::draft-orders/complete-draft-order(draft.id)

::metafields/set-metafield(product.id, "custom", "care_guide", "multi_line_text_field", "Hand wash cold.")
care ::metafields/get-metafield(product.id, "custom", "care_guide")
```

## Webhooks

`verify-request` checks the base64 HMAC-SHA256 of the raw body in `X-Shopify-Hmac-Sha256`, with a constant-time comparison. It fails closed: a missing context secret fails the call, and an empty explicit secret or a missing header never verifies. Shopify signs no timestamp, and it retries a failed delivery with the same `X-Shopify-Webhook-Id`, so dedupe on `webhook-id`.

```hot
shopify-webhook
meta {webhook: {service: "shopify", path: "/shopify/webhook"}}
fn (request) {
  cond {
    not(::shopify::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    => {
      d ::shopify::webhooks/delivery(request)
      // d.topic "orders/create", d.shop-domain, d.webhook-id (dedupe key)
      ::hot::http/HttpResponse({status: 200, body: {received: d.webhook-id}})
    }
  }
}
```

Subscriptions can be managed from code too:

```hot
sub ::shopify::webhooks/create-subscription("ORDERS_CREATE", "https://example.com/shopify/webhook", {
  includeFields: ["id", "name", "total_price"]
})
subs ::shopify::webhooks/list-subscriptions({topics: ["ORDERS_CREATE"]})
::shopify::webhooks/delete-subscription(sub.id)
```

Apps that declare webhooks in `shopify.app.toml` get app-level subscriptions, which don't show up in `list-subscriptions`.

## Errors and limits

- A non-2xx response is an Err carrying `HttpError {status, headers, body}`: 401 for a bad token, 402 for a frozen shop, 404 for an unknown shop or retired version.
- Top-level GraphQL `errors` are an Err carrying `GraphQLError {errors, data, cost, throttled, retry-after-ms}`.
- Mutation `userErrors` (and `orderCancelUserErrors`) are an Err carrying `UserErrors {mutation, user-errors, payload}`.
- **Throttling.** The Admin API rations query cost per app and store with a leaky bucket. A throttled call returns `throttled: true`, and `retry-after-ms` estimates the wait from `extensions.cost`. Hot has no in-process sleep, so the package doesn't wait or retry. Back off with a handler's `retry` metadata (`retry: {attempts: 5, delay: 1000, backoff: "exponential"}`), or keep `first` small in large `collect-*` runs.
- **API versions.** Shopify supports each quarterly version for 12 months, and `2026-10` is supported until October 2027. Bump `shopify.api.version` to test the next one before this package moves its pin.
- Connections cap `first` at 250, and pagination stops at 25,000 objects. Use bulk operations through `graphql` for full exports.
- `metafieldsSet` takes at most 25 metafields per call.

## License

Apache-2.0 - see [LICENSE](LICENSE)
