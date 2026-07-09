# hot-api

Hot platform API client — projects, events (publish/list), runs (list/get/stats), context variables, event handlers, schedules, environment info, and usage. Write operators, deploy hooks, and self-managing agents in Hot itself. Context variables: `hot-api.key`, optional `hot-api.url` (local: `http://localhost:4681/v1`).

```hot
::hot-api/publish-event("deploy:requested", {service: "api", ref: "main"})
failed ::hot-api/list-runs({status: "failed", limit: 10})
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
