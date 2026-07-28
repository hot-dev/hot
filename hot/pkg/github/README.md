# github

GitHub API bindings for Hot: repos, commits, checks, pull requests and reviews,
Actions workflows, logs, artifacts, and search — the developer-workflow
surface agents and automations actually use. Built on the `jwt` primitives
and pinned to GitHub REST API `2026-03-10`.

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
::checks ::github::checks
::commits ::github::commits

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

## Commit, check, and review evidence

The read path combines modern Checks, legacy commit statuses, commit diffs,
and pull-request discussion:

```hot
commit ::commits/get-commit("acme", "api", "main")
comparison ::commits/compare-commits("acme", "api", "last-known-good", commit.sha)

runs ::checks/list-check-runs-for-ref("acme", "api", commit.sha, {
  filter: "all",
  per_page: 100
})
failed filter(runs.check_runs, (run) { eq(run.conclusion, "failure") })
annotations ::checks/list-check-run-annotations(
  "acme",
  "api",
  first(failed).id,
  {per_page: 100}
)

// Some external CI still reports through commit statuses instead of Checks.
status ::commits/get-combined-status("acme", "api", commit.sha)

reviews ::pulls/list-reviews("acme", "api", 42, {per_page: 100})
inline ::pulls/list-review-comments("acme", "api", 42, {per_page: 100})
requested ::pulls/list-requested-reviewers("acme", "api", 42)
```

`create-review` accepts either `(event, body)` or a Map containing `commit_id`,
`body`, `event`, and inline `comments`.

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

## Pagination and rate limits

Endpoint wrappers return one GitHub page and accept `per_page`/`page` where
supported. For header-driven pagination or request diagnostics, use the
response-preserving API:

```hot
page ::github::api/json-response(
  "GET",
  "/repos/acme/api/issues?state=open&per_page=100"
)

page.data
page.headers.x-ratelimit-remaining
next ::github::api/next-page(page) // null after the last page
```

`::github::api/paginate-list(path)` collects Vec-bodied endpoints;
`paginate-key(path, key)` collects nested result Vecs such as `check_runs` or
search `items`. For large result sets, prefer `json-response` and `next-page`
so the caller controls memory and request volume.

Current budgets by category are available from `::github::rate-limit/get()`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
