# cipherowl

CipherOwl bindings for Hot: blockchain address compliance. Screen an address, or a batch of up to 50, for sanctions and illicit-activity exposure, then triage it with a 0-100 risk score, band and breakdown. Pull the evidence with exposure paths, and generate the risk-assessment narrative, a draft SAR narrative or a fund-flow graph. Keep addresses under continuous monitoring and poll their risk changes. Maintain your organization's allowlist and denylist overrides, and read balances and transactions from the Onchain Service. A decision helper turns any mix of results into `clear`, `review` or `block` under thresholds you set. `::cipherowl::api/request` covers any endpoint that isn't wrapped here.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/cipherowl": "0.1.1"
}
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `cipherowl.client.id` | yes, unless passed in `opts` | API client id from https://app.cipherowl.ai |
| `cipherowl.client.secret` | yes, unless passed in `opts` | API client secret |
| `cipherowl.base.url` | no | Overrides `https://svc.cipherowl.ai` (also used for `/oauth/token`). Not secret |

**Auth.** OAuth2 client credentials.
- **Minting:** the package posts `{client_id, client_secret, audience: "svc.cipherowl.ai", grant_type: "client_credentials"}` as JSON to `/oauth/token` and sends the token as `Authorization: Bearer`.
- **Caching:** tokens last 24 hours and there is no refresh token. A token is cached for the run with `::hot::ctx/set-secret`, so it is masked in run logs. The cache key is a SHA-256 of the credential identity, never the credential itself. The token is re-minted 60 seconds before it expires.
- **401s:** a 401 evicts the token, mints a new one and retries the request once.

## Per-call credentials

Every function that calls the API has a trailing `opts` arity.
- **Credentials:** pass `{client-id, client-secret}` to act for another tenant, or `{access-token}` with a token you minted yourself. Context credentials are then ignored for that call. A credential that is present but blank or null is an error naming the key, never a fallback to context.
- **Per-call URL:** `opts` can also carry `base-url`, which needs a credential in the same `opts`. Context credentials are never sent to a per-call URL.
- **Masking:** per-call credentials and minted tokens are masked in call records.
- **Resolver:** `::cipherowl::api/resolve(opts)` is the one resolver: opts first, then context, then defaults. Per-call credentials are never written to context.

```hot
tenant {client-id: "tenant-client-id", client-secret: "tenant-client-secret"}
out ::cipherowl::screening/screen-address("evm", "0x0000000000000000000000000000000000000001", {config: "co-vasp"}, tenant)
```

## Quick start

Screening is the cheap call and carries no severity. Pair a hit with the score to get a band, then decide:

```hot
::screening ::cipherowl::screening
::risk ::cipherowl::risk
::decisions ::cipherowl::decisions

address "0x93df2c9c8786242b8fdca3f96b3914b8b4a9f704"
screen ::screening/screen-address("evm", address, {config: "co-sandbox", trackAlert: false})
score ::risk/risk-score("evm", address, {config: "co-sandbox"})

decision ::decisions/decide([screen, score], {
  found-risk: "review",
  review-score: 40,
  block-score: 75,
  block-categories: ["ofac"],
  max-hops: 1
})
decision.decision
decision.triggers
```

## Functions

Every function below also takes a trailing `opts` Map (see Setup). `options` Maps hold request parameters.

**Request parameters**
- `chain`:
  - Pass a chain class: `bitcoin`, `evm`, `tron`, `solana`, `bitcoin_cash`, `bitcoin_gold`, `bitcoin_sv`, `dash`, `litecoin`, `monero`, `verge`, `xrp` or `zcash` (`::cipherowl/CHAINS`).
  - Or pass a specific chain from `chain-capabilities` (`ethereum`, `base`, ...). Responses echo the class (`evm`), so results also carry `requested-chain`.
  - Reports need the class: they translate a specific chain with `::cipherowl::risk/chain-class`.
- `config`:
  - `co-defi`, `co-vasp`, `co-institution`, your organization's own config, or `co-sandbox`.
  - Omitted, CipherOwl applies your organization's default; the response says which config was used.
- `hops`: 0 to 8.
- `reference`: your own attribution label, lowercased, dot-separated `[a-z0-9_]` segments up to 256 characters, e.g. `acme.withdrawals`.
- `trackAlert`: `false` keeps a risky screening from being recorded as a case.
- Validation:
  - EVM addresses must be `0x` plus 40 hex characters; other chains are checked only for being non-empty.
  - Base58 casing is kept.
  - Invalid input returns an Err before anything is sent.

**Screening** (`::cipherowl::screening`)
- `screen-address(chain, address, options?)`: returns `{foundRisk, config, chain, address, version, requested-chain}`.
- `batch-screen(chain, addresses, options)`: up to 50 addresses on one chain (`MAX_BATCH`). Returns `{results: [{address, foundRisk}], ...}`.
- `risky-addresses(batch-result)`

**Risk** (`::cipherowl::risk`)
- `risk-score(chain, address, options?)`: `riskScore` (0-100), `riskBand` and `breakdown`. `options.riskProfile` selects the profile.
- `parse-score(result)`: normalizes the band (`"Severe Risk"` becomes `"severe"`) and orders the breakdown by contribution.
- `risk-breakdown(chain, address, options?)`: the matched categories, `DIRECT` or `INDIRECT`.
- `risk-detail(chain, address, options?)`: the evidence.
  - Normalize it with `parse-detail`.
  - Each reason gets `first-exposure-at` and `last-exposure-at` in Unix seconds (they arrive as strings) and `total-usd-value` (null for CipherOwl's `"Infinity"`).
  - Each reason also gets `paths`, the address hops from the risk source, and `partial`.
- `chain-capabilities()`: every supported chain with its `chainClass` and its `screening`, `reporting` and `research` tiers.
- `networks-for-chain(capabilities, class)`, `chain-class(chain)`

**Reports** (`::cipherowl::reports`)
- `risk-assessment(chain, address, options?)`: the Markdown risk narrative.
- `sar-report(chain, address, options)`: a DRAFT suspicious-activity narrative. Nothing is filed.
- `graph(chain, address, options)`: the fund-flow graph as Mermaid (default), HTML, or with `output_format: "png"`, `{content-type, bytes}`.
- A report or graph that fails comes back with HTTP 200 and an `error` field: check `report-error(result)` before using it.
- Narrative wording varies between calls.
- `resolved-config` holds the config applied.

**Continuous monitoring** (`::cipherowl::monitoring`)
- Pools: `create-pool({name, riskConfigs, openCaseRiskLevels})`, `list-pools`, `list-all-pools`, `get-pool`, `update-pool`, `delete-pool`
- Members: `add-addresses(pool-id, [{chain, address}])` (at most 1000), `remove-addresses`, `rebaseline-addresses`, `list-addresses`, `list-all-addresses`
- Changes: `list-risk-changes(pool-id, options)`, `poll-risk-changes(pool-id, page-token, max-pages)`, `risk-history(pool-id, chain, address, options)`

**Private Data: allowlist and denylist** (`::cipherowl::private-data`)
- `list-overrides(network, {listType: "allowlist" | "denylist" | "both", pageSize, pageToken, includeDeleted, sinceTimestamp})`, `list-all-overrides`
- `get-override(network, address)` (a 404 Err when there is none), `delete-override(network, address)`
- `upsert-override(network, {listType, address, notes, addLabels, removeLabels, expectedVersion})`, `allowlist-address(network, address, notes)`, `denylist-address(network, address, notes)`
- Rules:
  - Overrides are scoped to your organization. They take a network key (`ethereum_mainnet`) but apply per chain.
  - `expectedVersion` is `"0"` for a new entry, or the entry's current `version` to update it.
  - A 409 means the entry exists or changed since you read it.

**Onchain Service** (`::cipherowl::onchain`)
- `list-networks()`, `get-balance(network, address)`, `batch-balances(network, addresses)` (at most 100)
- `list-address-transactions(network, address, {page, pageSize, opReturnOnly})` with `next-page(page)`, and `get-transaction-scripts(network, txid)` (Bitcoin family)
- Balances are strings in the smallest unit (wei, satoshi).

**Cases, read-only** (`::cipherowl::cases`)
- `list-cases(filters)`, `get-case(id)`, `list-case-screenings(id, options)`, `list-case-audit-events(id, options)`

**Escape hatch** (`::cipherowl::api`)
- `request(method, path, body, opts?)`, `send(...)` (the raw response), `resolve(opts)`, `next-page-token(page)`, `collect-pages(...)`, plus the validators

## Decisions

`::cipherowl::decisions/decide(results, policy)` takes one result or a Vec of them (screen, risk-score, risk-breakdown, detail). It returns:
- `decision`: `clear`, `review` or `block`
- `triggers`: the rules that fired, e.g. `block-score>=75`, `block-category:ofac`, `found-risk`
- `score` and `band`: `low`, `medium`, `high` or `severe`
- `found-risk` and `config`
- `reasons`: `{category, direction, hops, direct, source}`, with no paths or counterparty addresses

`decide-batch(batch-result, policy)` decides each address in a batch.

The thresholds are yours, and the code has no defaults: `found-risk` is required, and every other rule fires only when you set it.

| Policy key | Meaning | Suggested starting point |
|---|---|---|
| `found-risk` | Decision when screening finds risk and nothing stricter fires | `"review"` |
| `review-score` / `block-score` | `riskScore` at or above escalates (inclusive) | `40` / `75` |
| `review-bands` / `block-bands` | Bands that escalate (`"severe"` or `"Severe Risk"`) | `["high"]` / `["severe"]` |
| `review-categories` / `block-categories` | Categories that escalate within `max-hops` | `[]` / `["ofac"]` |
| `direct-block-categories` | Categories that block only on direct exposure | `["hack"]` |
| `max-hops` | Distance limit for the category rules (0 to 8) | `1` or `2` |

The starting points are illustrative. Calibrate them with your compliance team against your risk appetite and the config you screen with. If the results say nothing at all (no `foundRisk`, score or reasons), the decision is `review`, never `clear`.

## Webhooks and polling

CipherOwl documents no webhooks, monitoring alerts included. Poll risk changes from a schedule:
- Persist the returned `page-token`, for example in a Hot store, and pass it back next time; an empty page means you are caught up.
- Treat `changeId` as an idempotency key.
- Records appear a couple of minutes after detection.
- When a pool reaches its `openCaseRiskLevels`, CipherOwl also opens a case.

```hot
poll-pool fn (pool-id: Str, page-token: Str?): Map {
  out ::cipherowl::monitoring/poll-risk-changes(pool-id, page-token, 5)
  escalations filter(out.changes, (c) { in(c.currentRisk, ["HIGH", "SEVERE"]) })
  {page-token: out.page-token, escalations: escalations}
}
```

Run it from a `schedule` handler.

## Errors and pagination

- **Errors:** functions return the decoded JSON body. A non-2xx response returns an Err carrying `HttpError`:
  - `status`, `headers`, `body`
  - `message`: the status and its usual meaning, never request data
  - `detail`: CipherOwl's own text, which may echo the address
  - `retry-after`, on a 429
- **Testing a status:** `::cipherowl::api/is-status(e, 404)`.
- **Status meanings:**
  - 403 can mean a path or config your plan can't use.
  - Gateway errors are short plain text.
- **Retries:** 429s are not retried in a run (Hot has no sleep); let a handler or schedule `retry` policy re-run the work. A GET whose connection fails is retried once.
- **Pagination:**
  - Monitoring and Private Data page with `pageSize` and `pageToken`; read the next token with `next-page-token`. The `list-all-*` helpers follow it.
  - Onchain transactions and cases use 1-based `page` numbers.

## Sandbox

CipherOwl's sandbox is free with a free-tier account at https://app.cipherowl.ai.
- **How it works:** use your normal credentials and base URL, with `config: "co-sandbox"`.
- **Coverage:** the `evm` and `tron` chains, for screening, risk score, breakdown, detail and the risk-assessment report.
- **Results:** curated addresses return real risk data; every other address returns clean. The catalog may change; it is in `::cipherowl/SANDBOX_EVM_ADDRESSES`, `SANDBOX_TRON_ADDRESSES` and `SANDBOX_CLEAN_ADDRESS`.

| Chain | Address | Band | Scenario |
|---|---|---|---|
| evm | `0x93df2c9c8786242b8fdca3f96b3914b8b4a9f704` | Severe | Hacker; received from OFAC / Japan-sanctioned entities (1 hop) |
| evm | `0xab346efef1ffe377cdad686abeab62e83df1099f` | Severe | Hacker; OFAC / Japan-sanctioned exposure (1 hop) |
| evm | `0xf7b31119c2682c88d88d455dbb9d5932c65cf1be` | Severe | Directly flagged: hacker + OFAC + Japan-sanctioned |
| evm | `0xa0e1c89ef1a489c9c7de96311ed5ce5d32c20e4b` | Severe | Directly flagged: hacker + OFAC + Japan-sanctioned |
| evm | `0x6ac5251e07a9cafccec9af39f08222f52ec8c1bc` | Severe | Directly flagged: hacker + OFAC |
| evm | `0x4f47bc496083c727c5fbe3ce9cdf2b0f6496270c` | Severe | Directly flagged: OFAC + Japan-sanctioned |
| evm | `0x2ad9790b5116ce19b9cc59c7982a64df0fea84bc` | Severe | OFAC; received from a hacker / Japan-sanctioned entity (1 hop) |
| evm | `0x511b9ed0b7ef9dfad519f398fdfbdf6af8356780` | Severe | Directly flagged: hacker + OFAC |
| tron | `TEAqwfMhXLaomXhZ8KeMhx3njGmQEDnsUR` | Severe | Directly flagged: OFAC |
| tron | `THrqsvhL6hgMGcsNQibXUCAVXeExy3c1ip` | Severe | Directly flagged: hacker + OFAC |
| tron | `TV5Wn9L9T7CwrAZhrxyEGunkvQiSnsGy2H` | Severe | Directly flagged: hacker + OFAC |
| tron | `TCwrBc8fBnLTAQQENwkTLP4UEESM9Ksg6n` | Severe | Directly flagged: hacker + OFAC |
| tron | `TCSrVLXVvMtaFpE3xjUHveU4P6APan8uBy` | High | Sent funds to an OFAC address (1 hop) |
| tron | `TDt7rX3MjtknAxxYM9y6EowSdudCuuon4o` | High | Sent funds to an OFAC address (1 hop) |
| tron | `TF1SZkPg2vb3owTe7UMi3B7B5wkfquzj8p` | High | Sent funds to an OFAC address (1 hop) |
| tron | `TFa3VWLszsdj16euZ17wzQ63exBLHcorXp` | Medium | Sent funds to an OFAC address (1 hop) |
| tron | `TBW9RpX1hG9jVFRh4ooGBXSLFmMcAWz4pj` | Medium | Sent funds to an OFAC address (1 hop) |
| evm | `0x0000000000000000000000000000000000000001` | clean | Any address outside the catalog |

The integration suite (`hot test -p pkg-integration-cipherowl`) only reads:
- It lists chain capabilities, screens one curated address and the clean address under `co-sandbox` with `trackAlert: false`, and scores the curated address.
- It runs with `CIPHEROWL_CLIENT_ID` and `CIPHEROWL_CLIENT_SECRET`, and skips cleanly without them.

## Data handling

Addresses, risk verdicts, exposure paths and reports are wallet-risk data. They can identify a customer's funds and their counterparties.
- **Error messages:** this package never puts an address, a reference, a risk result or a credential in an error message. CipherOwl's own error text is kept in `HttpError.detail`.
- **Run traces:** Hot records function arguments and results in the run trace. Client credentials and minted tokens are masked there, whether they came from context or `opts`. Addresses and results are not masked, so restrict access to the run logs of flows that screen.
- **Evidence:** `decide` leaves exposure paths and counterparty addresses out of its result. Store reports and detail with the case file, not in logs.
- **Reference:** `reference` is your own label and is stamped on CipherOwl's screening logs. Don't put customer PII in it.

## Notes and limits

- **Plan:** production screening needs a paid plan. Configs, chains and tiers can be restricted by plan, and a config your plan can't use answers 403. The free tier covers the sandbox.
- **Chains:** screening and risk reasons accept specific chains; reports want the chain class. The package translates, at the cost of one `chain-capabilities` call when you pass a specific chain.
- **Batch size:** `batch-screen` takes at most 50 addresses (verified against the live API; the public reference states no limit).
- **Hops:** when you omit `hops`, the public reference says the config's own reach applies, while a production client verified against the live API observed single-hop behavior. Pass `hops` explicitly when depth matters.
- **Reports:** reports are synchronous and generated per call, so they take a few seconds. `risk_level` in reports is deprecated; use `found_risk`.
- **Monitoring enrolment:** `add-addresses` has a 25-second budget. On a 504, list the pool before retrying.
- **Rate limits:** no numeric rate limits are published.

## License

Apache-2.0 - see [LICENSE](LICENSE)
