# cloudflare

Cloudflare bindings: zones, DNS records, and Workers KV. For R2 object storage use `aws-s3` against your R2 S3-compatible endpoint. Context variables: `cloudflare.token`, `cloudflare.account.id` (for KV).

```hot
zone first(::cloudflare/list-zones("example.com").result)
::cloudflare/create-dns-record(zone.id, {type: "A", name: "api", content: "203.0.113.7", proxied: true})
::cloudflare/kv-write(namespace-id, "flag", "on", 3600)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
