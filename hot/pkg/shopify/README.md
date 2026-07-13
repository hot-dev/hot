# shopify

Shopify Admin bindings over the GraphQL Admin API (the current surface; REST is legacy for new apps): curated orders/products/customers queries, raw `graphql` for everything else, and webhook verification. Context variables: `shopify.shop`, `shopify.access.token`, `shopify.webhook.secret`.

```hot
recent ::shopify/list-orders(10)
customer ::shopify/find-customer-by-email("ada@example.com")
is-valid ::shopify/verify-request(request)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
