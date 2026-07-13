# github

GitHub API bindings for Hot: repos, issues, pull requests, Actions, and search — the developer-workflow surface agents and automations actually use. Built on the `jwt` primitives.

## Setup

| Context Variable | Description |
|---|---|
| `github.token` | Fine-grained/classic PAT, or an App installation token |
| `github.webhook.secret` | For `::github::webhooks/verify-request` |

GitHub Apps: mint installation tokens with `::github::app/installation-token(app-id, private-key-pem, installation-id)` (RS256 App JWT under the hood).

## Usage

```hot
::issues ::github::issues
::pulls ::github::pulls
::actions ::github::actions

issue ::issues/create-issue("acme", "api", "Timeout on /v1/users", {labels: ["bug"]})
::issues/create-comment("acme", "api", issue.number, "Triaged — looks like the pool limit.")

pr ::pulls/create-pull("acme", "api", "Add rate limiting", "feature/rl", "main", {draft: true})
::pulls/request-reviewers("acme", "api", pr.number, ["curt"])

::actions/dispatch-workflow("acme", "api", "deploy.yml", "main", {env: "staging"})

// Files via the contents API (decoded for you)
version ::github::repos/get-content("acme", "api", "VERSION")

// Webhooks (X-Hub-Signature-256, fail-closed)
is-valid ::github::webhooks/verify-request(request)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
