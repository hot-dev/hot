# opensanctions

The [OpenSanctions](https://www.opensanctions.org) `/match` API for Hot, with your own key: the hosted API, or a self-hosted [yente](https://www.opensanctions.org/docs/yente/) server. It screens against OpenSanctions' merged dataset (hundreds of sources, plus PEPs in `default`). It does not use the local cache of `hot.dev/name-screening`, whose result shape it returns.

> **Screening never decides.** Every result is `"possible-match"` or `"no-match"`, with each match scored and explained. **A person must review every possible match** before anyone blocks, reports or clears a party. Names are shared by many people, lists carry several transliterations, and the score measures similarity, not probability. A `"no-match"` only means that OpenSanctions returned nothing at or above the threshold on the dataset searched. It is not a clearance, and it covers only that dataset, which `sources` names in every result.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/opensanctions": "0.1.0"
}
```

## Settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::opensanctions::config/resolve`). Per-call values are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `opensanctions.api-key` | `api-key` | none | OpenSanctions API key (secret) |
| `opensanctions.base-url` | `base-url` | `https://api.opensanctions.org` | Your own self-hosted yente server, if you run one. See Per-call credentials |
| `opensanctions.dataset` | `dataset` | `"default"` | `"default"` (sanctions plus PEPs and more) or `"sanctions"` |

## Per-call credentials

Every function that reads settings takes a trailing `opts` Map, so one run can use many OpenSanctions keys. `::opensanctions::config/resolve(opts)` is the one resolver.

```hot
tenant {api-key: "tenant-opensanctions-key", dataset: "sanctions"}
::opensanctions/screen("Aleksandr Lukashenko", {kind: "individual", ...tenant})
```

An `api-key` in `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to `opensanctions.api-key`. A per-call `base-url` only uses an `api-key` from the same `opts`: the context key is never sent there, and without a per-call key a self-hosted yente gets no key at all. The hosted API always needs one.

## Quick start

```hot
os ::opensanctions/screen("Aleksandr Lukashenko", {kind: "individual", dataset: "sanctions", api-key: "..."})
os.status                       // "possible-match"
os.review-required              // true
first(os.matches).explanation   // OpenSanctions' score and its per-feature explanations
os.sources                      // the dataset searched
```

`opts`: `dob`, `country`, `kind`, `threshold` (OpenSanctions' default 0.7), `limit`, `algorithm`, `topics` (such as `["sanction"]`), `include-datasets`, plus the settings above.

## Functions

| Namespace | Functions |
|---|---|
| `::opensanctions` | `screen`, `match-body`, `match-query`, `match-url`, `match-headers`, `parse-response`, `to-match` |
| `::opensanctions::config` | `resolve`, `require-api-key` |

## Errors

- `screen` returns an Err without a key on the hosted API, naming `opensanctions.api-key`, and for a blank or null `api-key` in `opts`.
- HTTP failures are `::name-screening::http/HttpError` (`status`, `url`, `message`). Messages never carry the names screened or API keys.
- Nothing is retried inside a run. Use the schedule's `retry` policy (see the [retries docs](https://hot.dev/docs/retries)).

## Data handling

- `::opensanctions/screen` sends the name, date of birth and country to OpenSanctions, or to your own yente server via `base-url`.
- The package logs nothing.
- Screening results contain personal data from public lists and the subject you screened. Store and retain them under your own compliance policy.

## Notes and limits

- **Licensing**: OpenSanctions data is licensed CC BY-NC 4.0. It is free for non-commercial use, and **commercial use needs a license**. The hosted API is a paid service with a free trial. See [opensanctions.org/licensing](https://www.opensanctions.org/licensing/).
- OpenSanctions offers API trial keys.

## License

Apache-2.0 - see [LICENSE](LICENSE)
