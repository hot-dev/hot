# linear

Linear API bindings for Hot: issues, teams, comments, and search over Linear's GraphQL API — plus `::linear::api/graphql` for everything else.

## Setup

Context variable `linear.api.key`: a personal API key (`lin_api_...`) or OAuth token (`lin_oauth_...`) — the right Authorization form is chosen automatically.

## Usage

```hot
::issues ::linear::issues
::teams ::linear::teams

team ::teams/get-team-by-key("ENG")
issue ::issues/create-issue(team.id, "Rate limit the export endpoint", {priority: 2})
::issues/create-comment(issue.id, "Seen 429s from the reporting job since Tuesday.")

todo ::issues/list-issues({filter: {state: {type: {eq: "unstarted"}}}, first: 20})
hits ::issues/search-issues("export timeout")

// Raw GraphQL escape hatch
me ::linear::api/graphql("query { viewer { id name email } }")
```

GraphQL errors come back as an Err carrying `{errors, data}`; transport errors as `{status, body}`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
