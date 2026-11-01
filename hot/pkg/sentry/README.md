# sentry

Sentry API bindings for Hot: issue search and triage (single and bulk updates, events, the latest stack trace, comments), releases with commits and deploys for CI, projects and teams, and integration-platform webhook verification. Works with sentry.io, its `us`/`de` region hosts, and self-hosted Sentry. Cursor pagination follows Sentry's `Link` header, with collect-all helpers, and `::sentry::api/json` covers any endpoint not wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/sentry": "0.1.0"
}
```

## Setup

| Context Variable | Description |
|---|---|
| `sentry.token` | An organization auth token (`sntrys_...`, Settings → Developer Settings → Organization Tokens) or a user auth token. Issue comments need a user token |
| `sentry.org` | The organization slug |
| `sentry.base.url` | Optional. A self-hosted instance (`https://sentry.example.com`) or an explicit host. Default `https://sentry.io` |
| `sentry.region` | Optional. `us` or `de`, to call `https://{region}.sentry.io` directly (lower latency). Ignored when `sentry.base.url` is set |
| `sentry.webhook.secret` | An integration's client secret, for `::sentry::webhooks/verify-request` |

A missing token or organization is an Err carrying `ConfigError` that names the variable.

## Quick start

```hot
::issues ::sentry::issues

fresh ::issues/list-issues({project: "my-api", query: "is:unresolved firstSeen:-24h", statsPeriod: "24h", sort: "freq"})

issue first(fresh.items)
latest ::issues/get-latest-event(issue.id)
frames latest.entries

::issues/assign-issue(issue.id, "team:12")
::issues/resolve-issue(issue.id, "my-api@1.4.1")   // reopens if it regresses after 1.4.1
```

## Functions

| Module | Functions |
|---|---|
| `::sentry::issues` | `list-issues`, `list-all-issues`, `get-issue`, `update-issue`, `resolve-issue`, `unresolve-issue`, `ignore-issue`, `assign-issue`, `bulk-update-issues`, `list-issue-events`, `get-event`, `get-latest-event`, `list-comments`, `add-comment` |
| `::sentry::releases` | `create-release`, `list-releases`, `list-all-releases`, `get-release`, `update-release`, `finalize-release`, `set-commits`, `list-release-commits`, `delete-release`, `create-deploy`, `list-deploys` |
| `::sentry::projects` | `list-projects`, `list-all-projects`, `get-project`, `list-teams`, `list-all-teams` |
| `::sentry::webhooks` | `verify-request`, `hook-resource`, `hook-timestamp`, `request-id` |
| `::sentry::api` | `json`, `request`, `org-json`, `get-page`, `org-page`, `collect-all`, `collect-pages`, `next-cursor`, `parse-link-header`, `query`, `base-url` |

Issues use the organization-scoped endpoints (`/organizations/{org}/issues/...`); the deprecated per-project issue list and the legacy `/issues/{id}/` paths are not used.

### Pagination

List functions return a `Page`: `.items`, plus `.next_cursor` (null on the last page, when Sentry reports `results="false"`). Pass the cursor back to continue, or use a `list-all-*` helper, which follows cursors up to a limit (1000 items by default).

```hot
::issues ::sentry::issues

page ::issues/list-issues({query: "is:unresolved level:fatal", limit: 100})
more ::issues/list-issues({query: "is:unresolved level:fatal", limit: 100, cursor: page.next_cursor})

everything ::issues/list-all-issues({project: ["my-api", "web"], environment: "production"}, 5000)
```

### Bulk updates

```hot
::issues ::sentry::issues

stale ::issues/list-all-issues({project: "my-api", query: "is:unresolved lastSeen:-30d"}, 500)
::issues/bulk-update-issues(map(stale, %.id), {status: "ignored", substatus: "archived_forever"})
```

`bulk-update-issues` refuses an empty id list (Sentry would otherwise update every issue matching the filters) and more than 1000 ids per call.

### Releases and deploys from CI

```hot
::releases ::sentry::releases

version "my-api@1.4.0"
::releases/create-release(version, "my-api", {ref: "v1.4.0", url: "https://github.com/acme/api/releases/tag/v1.4.0"})
::releases/set-commits(version, [{repository: "acme/api", commit: "9f1c2d3", previousCommit: "4b5a6e7"}])
::releases/finalize-release(version)
::releases/create-deploy(version, "production", {url: "https://github.com/acme/api/actions/runs/123"})
```

`set-commits` uses Sentry's repository integration (GitHub, GitLab, …) to fetch the commit range. Without one, pass explicit commits: `update-release(version, {commits: [{id, repository, message, author_email}]})`.

Source maps and debug files: use [`sentry-cli`](https://docs.sentry.io/cli/) (`sentry-cli sourcemaps inject ./dist` then `sentry-cli sourcemaps upload --release my-api@1.4.0 ./dist`), which builds artifact bundles with debug IDs.

## Webhooks

Sentry integrations sign each delivery with `Sentry-Hook-Signature`, the hex HMAC-SHA256 of the raw body under the integration's client secret. `verify-request` checks it in constant time over the raw body and fails closed on a missing signature or empty secret.

```hot
sentry-webhook
meta {webhook: {service: "sentry", path: "/sentry/webhook"}}
fn (request) {
  cond {
    not(::sentry::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid signature"})
    }
    eq(::sentry::webhooks/hook-resource(request), "issue") => {
      println(`issue ${request.body.action}: ${request.body.data.issue.title}`)
      ::hot::http/HttpResponse({status: 200, body: "ok"})
    }
    => { ::hot::http/HttpResponse({status: 200, body: "ignored"}) }
  }
}
```

`hook-resource` returns `Sentry-Hook-Resource` (`issue`, `error`, `event_alert`, `metric_alert`, `comment`, `installation`, …), `hook-timestamp` the Unix-seconds `Sentry-Hook-Timestamp`, and `request-id` the delivery's `Request-ID` (for de-duplicating retries). The timestamp is not part of the signature, so there is no replay window by default; `verify-request(request, secret, max-age-seconds)` adds one if you want it.

## Errors

A non-2xx response is an Err carrying `HttpError` `{status, headers, body, message}`; `message` is Sentry's `detail`, or its field errors joined into one line (`"version: This field is required."`).

```hot
out ::sentry::issues/get-issue("4501")
if-err(out, (e) { println(`Sentry said ${e.status}: ${e.message}`) })
```

## Notes and limits

- **Issue comments** are attributed to a user, so `add-comment` needs a user auth token; organization tokens get 403. The comments endpoint isn't in Sentry's published API reference, though the Sentry UI uses it.
- **Project filters** accept slugs or numeric ids; `project` and `environment` take a single value or a Vec.
- **Rate limits** come back as 429 with `X-Sentry-Rate-Limit-*` headers on the `HttpError`.
- Collect-all helpers fetch pages serially; use `cursor` with the single-page functions for very large result sets.

## License

Apache-2.0 - see [LICENSE](LICENSE)
