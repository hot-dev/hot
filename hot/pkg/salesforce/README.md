# salesforce

Salesforce REST API bindings for Hot: sObject CRUD and external-id upserts, SOQL with auto-paging and safe literal escaping, SOSL search, describe, sObject collections (200 records per call) and composite requests, Bulk API 2.0 ingest and query jobs, and org limits. Authentication covers access tokens, refresh tokens, JWT bearer and client credentials, built on the `jwt` and `oauth2` packages, and caches the session for the rest of the run.

Targets REST API **v68.0** (Winter '27) by default. Set `salesforce.api.version` to pin another version.

## Install

```hot
// hot.hot
hot.project.my-app.deps {
    "hot.dev/salesforce": "0.1.2"
}
```

## Auth

Set the context variables for ONE flow. When several are present, the first complete one in this order wins: access token, refresh token, JWT bearer, client credentials. Set `salesforce.auth.flow` to force one.

| Flow | Required context variables | Token endpoint | JWT `aud` |
|---|---|---|---|
| Access token | `salesforce.access.token`, `salesforce.instance.url` | none (used as is) | n/a |
| Refresh token | `salesforce.client-id`, `salesforce.refresh-token` (+ `salesforce.client-secret` for confidential apps) | My Domain if set, else login/test | n/a |
| JWT bearer | `salesforce.client-id`, `salesforce.username`, `salesforce.private.key` (PEM of the connected app's certificate key) | My Domain if set, else login/test | login/test, or `salesforce.jwt.audience` |
| Client credentials | `salesforce.client-id`, `salesforce.client-secret`, `salesforce.my-domain.url` | My Domain (required) | n/a |

"login/test" means `https://login.salesforce.com`, or `https://test.salesforce.com` when `salesforce.sandbox` is `"true"`.

The token host and the JWT audience are separate settings because Salesforce treats them differently:

- The JWT `aud` must be `https://login.salesforce.com`, `https://test.salesforce.com`, or an Experience Cloud site URL. A My Domain URL is rejected, so `aud` never follows `salesforce.my-domain.url`.
- The client-credentials grant is not supported on login or test. It must be posted to the org's My Domain, such as `https://acme.my.salesforce.com`. The connected app also needs "Enable Client Credentials Flow" turned on and a run-as user.

| Other context variables | Description |
|---|---|
| `salesforce.my-domain.url` | The org's My Domain login URL (Setup → My Domain). Required for client credentials; otherwise optional. |
| `salesforce.sandbox` | `"true"` to use `test.salesforce.com` for the login host and JWT audience |
| `salesforce.jwt.audience` | Override the JWT `aud`, e.g. an Experience Cloud site URL |
| `salesforce.auth.flow` | Force a flow: `access-token`, `refresh-token`, `jwt-bearer`, `client-credentials` |
| `salesforce.session.ttl` | Seconds to reuse a minted session when the token response has no `expires_in`. Default 900. |
| `salesforce.api.version` | REST API version, e.g. `"66.0"`. Default `v68.0`. |

**Sessions.** The first request in a run mints a session, and later requests reuse it. The cache is keyed on the credentials. A `401 INVALID_SESSION_ID` drops the cached session, mints a fresh one, and retries the request once. An explicit access token is never refreshed: its 401 comes back as an Err.

## Per-call credentials

Every function that calls Salesforce takes an optional trailing `opts` Map, so one run can act for many orgs and parallel flows don't share credentials. Per-call credentials are masked in call records, and nothing from `opts` is written to ctx except the minted session, cached under a hash of the credentials.

| Flow | Keys in `opts` |
|---|---|
| Access token | `access-token`, `instance-url` |
| Refresh token | `refresh-token`, `client-id` (+ `client-secret` for confidential apps, `my-domain-url` for the token host) |
| JWT bearer | `private-key`, `client-id`, `username` (+ `audience`, `my-domain-url`) |
| Client credentials | `client-secret`, `client-id`, `my-domain-url` |

`flow`, `sandbox` and `ttl` work like their context variables, and `api-version` picks the REST API version. `opts` always comes last. Where a function also takes `options`, pass `{}` for those.

```hot
tenant {access-token: "00D...", instance-url: "https://acme.my.salesforce.com"}

lead ::salesforce::sobjects/get-record("Lead", "00Q...", tenant)
rows ::salesforce::query/soql-records("SELECT Id FROM Contact", {max-records: 500}, tenant)
limits ::salesforce::org/limits(tenant)

other-org {refresh-token: "5Aep...", client-id: "3MVG...", client-secret: "..."}
page ::salesforce::query/soql("SELECT Id FROM Lead", {}, other-org)
```

Credentials in `opts` are a complete identity, and nothing in it falls back to context: `{access-token}` alone is an error, not a token sent to `salesforce.instance.url`. A key that is present but blank or null is an error naming it. `instance-url`, `my-domain-url` or `audience` without a credential in the same `opts` is an error too: context credentials are never sent to a per-call URL. `::salesforce::api/resolve(opts)` is the one place this is decided.

## Quick start

```hot
::sobjects ::salesforce::sobjects
::query ::salesforce::query

lead ::sobjects/create-record("Lead", {LastName: "Lovelace", Company: "Analytical Engines", Email: "ada@example.com"})
::sobjects/update-record("Lead", lead.id, {Rating: "Hot"})

// Idempotent sync by external id
synced ::sobjects/upsert-record("Contact", "External_Id__c", "u-42", {LastName: "Lovelace"})
synced.created

// Build SOQL from untrusted input with literal(), then collect every page
name "O'Brien"
contacts ::query/soql-records(`SELECT Id, Name FROM Contact WHERE LastName = ${::query/literal(name)}`, {max-records: 2000})
```

## Functions

**`::salesforce::sobjects`**: single records
- `create-record(type, fields)`, `get-record(type, id[, fields])`, `update-record(type, id, fields)`, `delete-record(type, id)`
- `upsert-record(type, external-id-field, value, fields)` returns `{id, success, created}`. `get-by-external-id(type, field, value[, fields])` fetches by an external id.
- `describe-global()`, `describe-object(type)`, `field-names(description)`

**`::salesforce::query`**: SOQL
- `soql(query[, options])` returns one page. `options`: `include-deleted` (uses `/queryAll`), `batch-size`.
- `query-all(query)` uses `/queryAll`, which includes deleted and archived rows.
- `next-page(nextRecordsUrl)`
- `soql-records(query[, options])` follows `nextRecordsUrl` and returns every record. `max-records` caps it.
- `escape(str)`, `escape-like(str)` (also escapes `%` and `_`), `literal(value)` (Str, number, Bool, null, or Vec for `IN`)

**`::salesforce::search`**: SOSL
- `sosl(statement)` runs a raw SOSL statement.
- `find(term[, options])` escapes the term and builds the SOSL. `options`: `in`, `returning`, `limit`. `escape-sosl(term)` and `build-sosl(term, options)` are also exposed.

**`::salesforce::composite`**: many records per call
- `create-records(type, records[, options])`, `update-records(type, records[, options])`, `upsert-records(type, external-id-field, records[, options])`, `delete-records(ids[, options])` handle 200 records per call. Larger inputs are split into chunks unless `all-or-none` is set.
- `get-records(type, ids, fields)` returns up to 2000 records per call.
- `composite(subrequests[, options])` and `subrequest(method, path, reference-id[, body])` send up to 25 chained subrequests.

**`::salesforce::bulk`**: Bulk API 2.0
- Ingest: `create-ingest-job(object, operation[, options])`, `upload-job-data(id, csv)`, `close-job(id)`, `ingest(object, operation, csv[, options])` (create, upload, and close in one call), `get-ingest-job`, `poll-ingest-job`, `abort-ingest-job`, `delete-ingest-job`, `successful-results`, `failed-results`, `unprocessed-records`
- Query: `create-query-job(soql[, options])`, `get-query-job`, `poll-query-job`, `query-results(id[, {locator, max-records}])` (returns `{csv, locator, number-of-records}`), `abort-query-job`, `delete-query-job`
- Helpers: `to-csv(records, columns)`, `is-job-finished(job)`

**`::salesforce::org`**
- `limits()`, `versions()`, `api-usage(sforce-limit-info-header)`

**`::salesforce::api`**: the escape hatch
- `json(method, path[, body[, session]])` calls any REST resource. `send(method, path, body, options)` returns the raw response.
- `HttpError` carries `{status, headers, body, message, error-code}`.

```hot
::composite ::salesforce::composite
::bulk ::salesforce::bulk

// One round trip: create an account and a contact that references it
out ::composite/composite([
  ::composite/subrequest("POST", "/sobjects/Account", "acct", {Name: "Acme"}),
  ::composite/subrequest("POST", "/sobjects/Contact", "ada", {LastName: "Lovelace", AccountId: "@{acct.id}"})
], {all-or-none: true})

// Bulk upsert from records
csv ::bulk/to-csv([
  {External_Id__c: "u-1", LastName: "Lovelace"},
  {External_Id__c: "u-2", LastName: "Hopper"}
], ["External_Id__c", "LastName"])
job ::bulk/ingest("Contact", "upsert", csv, {external-id-field: "External_Id__c"})
status ::bulk/get-ingest-job(job.id)
```

## Waiting on Bulk jobs

Bulk jobs finish asynchronously, and hot-std has no sleep. To wait for one, poll from a scheduled function or a later event. Or start a task whose retry policy supplies the delay: `poll-ingest-job` and `poll-query-job` return an Err until the job reaches `JobComplete`, `Failed`, or `Aborted`.

```hot
check-job fn (job-id: Str): Map { ::salesforce::bulk/poll-ingest-job(job-id) }

csv ::salesforce::bulk/to-csv([{LastName: "Lovelace"}], ["LastName"])
job ::salesforce::bulk/ingest("Contact", "insert", csv)
::hot::task/start(check-job, job.id, ::hot::task/TaskOptions({retry: {attempts: 20, delay: 5000, backoff: "exponential"}}))
```

## Errors

A non-2xx response returns `err(HttpError)`. Salesforce's error array `[{message, errorCode, fields}]`, or an OAuth `{error, error_description}` body, is flattened into a readable `message`, for example `salesforce 400: INVALID_FIELD: No such column 'Foo' on entity 'Lead' (fields: Foo)`. `error-code` holds the first code.

## Notes and limits

- Collections take 200 records per call, and `get-records` takes 2000. `all-or-none` collections must fit in one call. A composite request holds at most 25 subrequests.
- `soql-records` makes one API call per page, which is 2000 records by default. Set `max-records` on large objects.
- Bulk uploads are one CSV per job, up to 150 MB. On update, empty cells leave a field unchanged; write `#N/A` to clear it.
- `literal()` quotes strings. Date and datetime literals are unquoted in SOQL, so interpolate those directly.
- Salesforce reports daily API consumption in the `Sforce-Limit-Info` response header. Read it from `::salesforce::api/send` and parse it with `::salesforce::org/api-usage`, or call `::salesforce::org/limits()`.
- Free Developer Edition orgs work for testing: https://developer.salesforce.com/signup

## License

Apache-2.0 - see [LICENSE](LICENSE)
