# github

GitHub API bindings for Hot: repos, issues, pull requests, Actions workflows,
logs, artifacts, and search — the developer-workflow surface agents and
automations actually use. Built on the `jwt` primitives.

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
::artifacts ::github::artifacts

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

## GitHub Actions evidence

The Actions bindings cover the investigation loop from workflow discovery
through exact attempt, job, step, log, and artifact evidence:

```hot
failed ::actions/list-workflow-runs("acme", "api", {
  branch: "main",
  status: "failure",
  per_page: 20
})

run first(failed.workflow_runs)
jobs ::actions/list-workflow-jobs("acme", "api", run.id, {filter: "all"})
job first(jobs.jobs)

// Ordered step conclusions distinguish checkout from later build/test failures.
checkout find-first(job.steps, (step) { eq(step.name, "Checkout") })
log ::actions/download-workflow-job-logs("acme", "api", job.id)

// Compare re-run attempts and inspect their archived outputs.
attempt ::actions/get-workflow-run-attempt("acme", "api", run.id, run.run_attempt)
archive ::actions/download-workflow-attempt-logs("acme", "api", run.id, attempt.run_attempt)
outputs ::artifacts/list-workflow-run-artifacts("acme", "api", run.id)
```

Workflow control is explicit: `dispatch-workflow`, `enable-workflow`,
`disable-workflow`, `cancel-workflow-run`, `rerun-workflow`,
`rerun-failed-workflow-jobs`, and `rerun-workflow-job`. Re-run functions accept
an optional `{enable_debug_logging: true}` options Map.

## License

Apache-2.0 - see [LICENSE](LICENSE)
