# sumsub

Sumsub bindings for Hot: identity verification (KYC), business verification (KYB) and transaction monitoring (KYT/AML). The package creates and reads applicants, uploads ID documents, moves applicants between levels, resets them and submits them for review, and manages tags and notes. It mints WebSDK access tokens and hosted verification links, creates company applicants and links their beneficiaries, and submits transactions (Travel Rule transfers included) for scoring and review. Webhook deliveries are verified against their digest. A decision helper turns any review into approved, declined or pending. Every request is signed with your App Token, and `::sumsub::api/request` covers any endpoint that isn't wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/sumsub": "0.1.1"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `sumsub.app.token` | yes, unless passed in `opts` | An App Token from the dashboard (Dev space → App tokens). Sandbox tokens start `sbx:`, production tokens `prd:`; a token works only in the environment it was made in |
| `sumsub.secret.key` | yes, unless passed in `opts` | The token's secret key, shown once when the token is created. It signs every request and is never sent |
| `sumsub.level.name` | no | Default verification level for `create-applicant` when you pass null. Not secret |
| `sumsub.base.url` | no | Overrides `https://api.sumsub.com` (sandbox and production share it). Not secret |
| `sumsub.webhook.secret` | for webhooks | The webhook's own secret key from the Webhook manager. It is not the App Token secret |

**Signing.** Each request carries `X-App-Token`, `X-App-Access-Ts` (Unix seconds) and `X-App-Access-Sig`. The signature is the lowercase hex HMAC-SHA256 of `ts + METHOD + path-with-query + body`, keyed with the secret key. The body is the exact bytes sent, so multipart uploads are signed byte for byte. Sumsub rejects a timestamp more than a minute from its clock (error 4004), so keep the host clock in sync. `::sumsub::api/sign` is the pure signing function.

## Per-call credentials

Every function that calls the API has a trailing `opts` arity. Pass `{app-token, secret-key}` to act for another tenant; context credentials are then ignored for that call. `opts` can also carry `level-name`, and `base-url`, which needs the pair in the same `opts`: context credentials are never sent to a per-call URL. A credential that is present but blank or null, or one of the pair without the other, is an error naming the key, never a fallback to context. `::sumsub::api/resolve(opts)` is the one resolver: opts first, then context, then defaults. Per-call credentials are masked in call records and never written to context, so they can't leak between tenants or parallel flows.

```hot
tenant {app-token: "sbx:tenant-token", secret-key: "tenant-secret"}
applicant ::sumsub::applicants/get-applicant-by-external-id("user-42", tenant)
```

## Quick start

```hot
::applicants ::sumsub::applicants
::sdk ::sumsub::sdk
::decisions ::sumsub::decisions

// 1. Create the applicant for your user
applicant ::applicants/create-applicant("basic-kyc-level", {
  externalUserId: "user-42",
  email: "ada@example.com",
  fixedInfo: {firstName: "Ada", lastName: "Lovelace", country: "GBR"}
})

// 2. Send them to Sumsub's hosted verification page
link ::sdk/create-websdk-link("basic-kyc-level", {userId: "user-42", ttlInSecs: 86400})

// 3. When the applicantReviewed webhook arrives (or from a schedule), read the outcome
outcome ::decisions/review-outcome(::applicants/get-status(applicant.id))
outcome.outcome
```

## Functions

Every function below also takes a trailing `opts` Map (see Setup).

**Applicants** (`::sumsub::applicants`)
- `create-applicant(level-name, applicant)`: takes `externalUserId` (required), `type`, `email`, `phone`, `lang`, `metadata` and `fixedInfo` (`firstName`, `lastName`, `dob`, `country`, `nationality`, `addresses`, `tin`, ...)
- `get-applicant(id)`, `get-applicant-by-external-id(external-user-id)`, `find-applicant-by-external-id(external-user-id)` (`{exists, applicant?}`, no 404)
- `get-status(id)`: `reviewStatus` and `reviewResult`. Also `get-required-docs-status(id)` (per step), `get-review-history(id)` and `list-levels()`
- `move-to-level(id, level-name, doc-sets?)`, `reset-applicant(id)`, `reset-step(id, step)`, `request-check(id, reason?)`
- `add-id-document(id, metadata, file)`: multipart upload. Returns `{document, image-id}`. `list-documents(id)` lists the uploads
- `update-fixed-info(id, fields)`, `update-applicant(id, fields)`, `deactivate-applicant(id)`, `activate-applicant(id)`
- `approve-applicant(id, {note, tags})` and `reject-applicant(id, {note, tags, reasons})`, for decisions made outside Sumsub
- Tags: `add-tags`, `set-tags`, `remove-tags` (read them from `get-applicant(...).tags`)
- Notes: `list-notes(id, offset?, limit?)`, `list-all-notes(id, limit)`, `add-note(id, note, tags?)`, `update-note`, `delete-note`
- `simulate-review(id, {reviewAnswer, reviewRejectType?, rejectLabels?})`: sandbox only

**Hosted verification** (`::sumsub::sdk`)
- `create-access-token(user-id, level-name, {ttlInSecs, applicantIdentifiers, externalActionId})`: a WebSDK or MobileSDK token (`POST /resources/accessTokens/sdk`)
- `create-websdk-link(level-name, {userId, ttlInSecs, redirect, applicantIdentifiers, lang, ...})`: a shareable verification URL (`POST /resources/sdkIntegrations/levels/-/websdkLink`)

**Business verification** (`::sumsub::kyb`)
- `create-company(level-name, external-user-id, company-info, options)`: `company-info` needs `companyName` and `country` (alpha-3)
- `update-company-info(id, fields)`
- `add-beneficiary(company-id, {types, applicantId | beneficiaryInfo, shareSize, subtypes, natureOfControl})`, `link-applicant(company-id, applicant-id, types)`, `remove-beneficiary(company-id, beneficiary-id)`
- `beneficiaries(applicant)`: pure. `BENEFICIARY_TYPES` and `UBO_SUBTYPES` list the documented roles

**Transaction monitoring** (`::sumsub::kyt`)
- `submit-transaction(applicant-id, txn)` and `submit-transaction-for-user(level-name, txn)`; the second is keyed by `applicant.externalUserId` and creates the applicant if needed
- `get-transaction(id)`, `get-transaction-by-txn-id(txn-id)`, `review-transaction(id, "GREEN" | "RED")`, `rescore-transaction(id)`, `set-payment-txn-id(id, hash)`, `delete-transaction(id)`
- Travel Rule: submit with `type: "travelRule"`. Then use `travel-rule-status(txn)` and `confirm-wallet-ownership(id, confirmed)`

```hot
txn ::sumsub::kyt/submit-transaction("65f0c1e2a3b4c5d6e7f80912", {
  txnId: "wd-1001",
  type: "finance",
  txnDate: "2026-11-01 12:00:00+0000",
  info: {direction: "out", amount: 2500, currencyCode: "USD", currencyType: "fiat"},
  applicant: {},
  counterparty: {type: "individual", fullName: "Grace Hopper", paymentMethod: {type: "bankAccount", accountId: "acct-ref-77"}}
})
decision ::sumsub::decisions/review-outcome(txn)
```

**Decisions** (`::sumsub::decisions`)
- `review-outcome(source)` returns `{outcome, final, can-retry, on-hold, fraud, review-status, review-answer, reject-type, reject-labels, button-ids, moderation-comment, client-comment}`. It accepts a status, an applicant, a transaction, a raw webhook payload or a parsed event.
  - `outcome` is `"approved"` only for `completed` + `GREEN`, and `"declined"` for `completed` + `RED`.
  - Everything else is `"pending"`: `init`, `pending`, `queued`, `onHold`, `awaitingService`, `awaitingUser`, or an unknown answer.
  - `final` means no resubmission (`reviewRejectType` is not `RETRY`). `can-retry` means the user may upload again.
  - `fraud` flags Sumsub's automatic-action labels (`FORGERY`, `STOLEN_DOCS`, ...).
  - `moderation-comment` may be shown to the user. `client-comment` is internal.
- `is-approved(source)`

**Escape hatch** (`::sumsub::api`)
- `request(method, path, body, opts?)`, `send(...)` (the raw response, with headers), `sign(...)`, `resolve(opts)`, `page-items(page)`, `next-offset(page, offset)`

## Webhooks

Sumsub sends `X-Payload-Digest`, a hex HMAC of the raw body keyed with the webhook's secret key, and names the algorithm in `X-Payload-Digest-Alg`: `HMAC_SHA256_HEX` (the default), `HMAC_SHA512_HEX` or the legacy `HMAC_SHA1_HEX`.
- **Verification:** `verify-request` checks the digest over `::hot::http/raw-body` in constant time.
- **Failing closed:**
  - An unset `sumsub.webhook.secret` halts with a message.
  - An empty explicit secret never verifies.
  - A missing digest, or a missing or unknown algorithm, fails.
- **Algorithms:** pass `{algorithms: ["HMAC_SHA256_HEX", "HMAC_SHA512_HEX"]}` as the third argument to refuse SHA-1.
- **Replays:** Sumsub signs no timestamp, so there is no replay window to enforce. It retries failed deliveries up to five times over about a day, so make handlers idempotent and key them on `dedupe-key(event)`.
- **Sumsub's own example is mislabelled.** Its `testDigest` example (secret `SoMe_SeCrEt_KeY`, body `someText`, digest `f6e92ffe...`) is labelled SHA-256 but is actually HMAC-SHA1. The unit tests cover all three algorithms.

```hot
sumsub-webhook
meta {webhook: {service: "sumsub", path: "/sumsub/webhook"}}
fn (request) {
  cond {
    not(::sumsub::webhooks/verify-request(request)) => {
      ::hot::http/HttpResponse({status: 401, body: "invalid digest"})
    }
    => {
      event ::sumsub::webhooks/parse-event(request)
      outcome ::sumsub::decisions/review-outcome(event)
      ::hot::http/HttpResponse({status: 200, body: outcome.outcome})
    }
  }
}
```

`parse-event` flattens a payload:
- `type`, `applicant-id`, `external-user-id`, `level-name`
- `review-status`, `review-answer`, `reject-type`, `reject-labels`
- `sandbox`, `test`
- the transaction fields `kyt-txn-id` and `kyt-data-txn-id`
- the raw `payload`

`APPLICANT_EVENTS` and `KYT_EVENTS` list the documented types. The transaction on-hold event is `applicantKytOnHold`, not `applicantKytTxnOnHold`.

## Errors and pagination

- Functions return the decoded JSON body. Simple actions return `{ok: 1}`.
- A non-2xx response returns an Err carrying `HttpError`:
  - `status`, `headers` and `body`
  - Sumsub's `error-code` (e.g. 4003 signature mismatch, 4004 expired timestamp, 6008 sandbox applicant limit), `error-name` and `correlation-id`
  - `retry-after`, on the temporary 409 from approve and reject
  - `description`, Sumsub's own text
  - `message`, a readable summary that never includes request data or the description
- `::sumsub::api/is-error-code(e, 4004)` tests a code. Match on `error-code`, not `error-name`: Sumsub spells some names inconsistently.
- Invalid input (no `externalUserId`, a transaction without `txnId`, ...) returns an Err before anything is sent.
- A GET whose connection fails is retried once, freshly signed. Nothing else is retried in a run: Hot has no sleep. For a 429 or a temporary 409, let a handler or schedule `retry` policy re-run the work.
- Rate limits are 300 GET and 50 POST requests per 5 seconds.
- List endpoints page by `offset` and `limit` and return `{items, totalItems}`, sometimes wrapped in `list`. `page-items` and `next-offset` read either shape, and `list-all-notes` follows the pages.
- Request bodies omit unset (null) fields instead of sending JSON `null`.

## Async work

Reviews take time, and nothing in this package waits or polls in a loop.
- `request-check` submits the applicant. `get-status` reads the review.
- `::sumsub::decisions/review-outcome` tells you whether the review is done.
- The result arrives on the `applicantReviewed` webhook (preferred). Otherwise poll `get-status` from a `schedule` handler.
- Transactions are scored synchronously when you submit them, and later review changes arrive on the `applicantKytTxn*` webhooks.

## Sandbox

Create a sandbox App Token (Dev space, sandbox mode) to test without real checks.
- **Results:** sandbox runs no real verification. Results come from the "verification test modes" you attach to a level, and `simulate-review` completes a review with the answer you choose and fires `applicantReviewed`.
- **Limits:** 500 applicants (error 6008) and 1000 transactions (error 6014) per 24 hours.
- **Mock companies:** KYB registry checks return mock companies for the GBR registration numbers `99999999`, `88888888` and `77777777`.
- **Webhooks:** sandbox payloads carry `sandboxMode: true`, and test sends from the dashboard carry `testMode: true`.

The integration suite (`hot test -p pkg-integration-sumsub`) only reads: it lists levels and looks up a random unknown `externalUserId`. It runs with `SUMSUB_APP_TOKEN`, `SUMSUB_SECRET_KEY` and optionally `SUMSUB_LEVEL_NAME`, and skips cleanly without them.

## Data handling

Sumsub handles personal data: names, dates of birth, addresses, ID numbers, document images, selfies, and the counterparties and accounts in transactions.
- **Error messages:** this package never puts that data, or any credential, in an error message. `HttpError.message` carries only the status, Sumsub's numeric code and the correlation id. Sumsub's own `description` (which can echo submitted values) stays in its own field.
- **Run traces:** Hot records function arguments and results in the run trace. The App Token, secret key and webhook secret are context secrets and are masked there. Applicant data you pass to these functions, or receive from them, is not masked, so treat the run logs of flows that call this package as containing PII and restrict access to them. Pass only what Sumsub needs, and prefer `externalUserId`s that are opaque ids, not emails.
- **Webhooks:** webhook payloads carry ids and review results, not personal data. Fetch the applicant only when you need more.
- **Bearer credentials:** WebSDK links and access tokens let whoever holds them continue that user's verification. Send them only to that user.
- **Notes and tags:** these are visible to everyone on your Sumsub dashboard.

## Notes and limits

- **Contract:** Sumsub needs a contract for production (sandbox is self-serve after sign-up). Some features are enabled per account: Transaction Monitoring and Travel Rule, KYB registry checks, AML screening, video identification. Calls to a feature your account lacks return 403.
- **Not wrapped:** applicant actions, the audit trail, case management, wallet attribution and the Travel Rule VASP directory and SDK. Use `::sumsub::api/request` for them.
- **Legacy endpoints:** the older `POST /resources/accessTokens?userId=...` and `.../levels/{levelName}/websdkLink` still appear in places. This package uses the current `/resources/accessTokens/sdk` and `/resources/sdkIntegrations/levels/-/websdkLink`.
- **Changing level:** `move-to-level` sends the level as `?name=`, as the API requires (not `levelName`).
- **Document parts:** `add-id-document` sends only the `metadata` and `content` parts; Sumsub rejects any other part.

## License

Apache-2.0 - see [LICENSE](LICENSE)
