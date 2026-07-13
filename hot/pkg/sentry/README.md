# sentry

Sentry bindings: issues (search with Sentry query syntax, resolve/assign), latest events with stack traces, and projects — agents that triage their own production errors. Context variables: `sentry.token`, `sentry.org` (`sentry.url` for self-hosted).

```hot
fresh ::sentry/list-issues("my-api", "is:unresolved firstSeen:-24h")
::sentry/resolve-issue(first(fresh).id)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
