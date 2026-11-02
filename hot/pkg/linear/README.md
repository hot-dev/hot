# linear

Linear API bindings for Hot: issues (full lifecycle through archive/delete), teams with workflow states and labels, comments, search, and webhook verification over Linear's GraphQL API — plus `::linear::api/graphql` for everything else.

## Setup

| Context Variable | Description |
|---|---|
| `linear.api.key` | Personal API key (`lin_api_...`) or OAuth token (`lin_oauth_...`) — the right Authorization form is chosen automatically |
| `linear.webhook.secret` | For `::linear::webhooks/verify-request` |

## Usage

```hot
::issues ::linear::issues
::teams ::linear::teams

team ::teams/get-team-by-key("ENG")
issue ::issues/create-issue(team.id, "Rate limit the export endpoint", {priority: 2})
::issues/create-comment(issue.id, "Seen 429s from the reporting job since Tuesday.")

// Move it through the team's workflow
started first(filter(::teams/list-states(team.id).nodes, (s) { eq(s.type, "started") }))
::issues/update-issue(issue.id, {stateId: started.id})

todo ::issues/list-issues({filter: {state: {type: {eq: "unstarted"}}}, first: 20})
thread ::issues/list-comments(issue.id, {first: 50})
hits ::issues/search-issues("export timeout")

::issues/delete-issue(issue.id)   // to trash; archive-issue keeps it queryable

// Webhooks: HMAC check plus replay protection on the signed
// webhookTimestamp (60s tolerance by default)
is-valid ::linear::webhooks/verify-request(request)

// Raw GraphQL escape hatch
me ::linear::api/graphql("query { viewer { id name email } }")
```

GraphQL errors come back as an Err carrying `{errors, data}`; transport errors as `{status, body}`. List surfaces (`list-issues`, `list-comments`) take `first`/`after` and return `pageInfo` for cursoring.

## Per-call credentials

Every function that calls Linear takes an optional trailing `opts` Map, so one run can act for many workspaces and parallel flows don't share credentials. Nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The personal API key or OAuth token for this call, instead of `linear.api.key` |
| `base-url` | Another GraphQL endpoint, such as a proxy. Needs an `api-key` in the same `opts`; `linear.api.key` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "lin_api_tenant_b"}
teams ::linear::teams/list-teams(tenant)
todo ::linear::issues/list-issues({first: 20}, tenant)
me ::linear::api/graphql("query { viewer { id } }", {}, tenant)

// Webhooks: one tenant's secret
is-valid fn (request: Map): Bool {
  ::linear::webhooks/verify-request(request, {webhook-secret: "lin_wh_tenant_b"})
}
```

An `api-key` or `webhook-secret` that is present but blank or null is an error naming the key, never a fallback to context. `::linear::api/resolve(opts)` is the one place credentials are decided.

## License

Apache-2.0 - see [LICENSE](LICENSE)
