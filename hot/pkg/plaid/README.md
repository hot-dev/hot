# plaid

Plaid bindings: Link token flow, account balances, identity, and incremental `transactions/sync`. Credentials are injected into request bodies per Plaid's convention. Context variables: `plaid.client-id`, `plaid.secret`, `plaid.env` ("sandbox" default).

```hot
link ::plaid/create-link-token("user-42", "My App")
tokens ::plaid/exchange-public-token(public-token)
sync ::plaid/sync-transactions(tokens.access_token, null)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
