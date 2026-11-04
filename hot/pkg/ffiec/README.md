# ffiec

The [FFIEC Central Data Repository](https://cdr.ffiec.gov/public/) Public Web Service for Hot, with a free PWS account. Covers Call Report and UBPR reporting periods, the panel of reporters, recent filers and facsimiles. This is the REST service in [CDR-PDD-SIS-611 v1.10](https://cdr.ffiec.gov/Public/Files/SIS611_-_Retrieve_Public_Data_via_Web_Service.pdf).

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/ffiec": "0.1.0"
}
```

## Auth and settings

Each setting comes from per-call `opts` first, then the Context Variable, then the default (`::ffiec::api/resolve`). Every API function takes a trailing `opts` Map, so one run can serve many tenants. Per-call credentials are never written to context.

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `ffiec.user-id` | `user-id` | none | **Required** (secret). Your PWS username, sent in the `UserID` header |
| `ffiec.token` | `token` | none | **Required** (secret). Your PWS security token, sent in the `Authentication` header as `Bearer {token}` |
| `ffiec.base-url` | `base-url` | `https://ffieccdr.azure-api.us/public` | Service root |

Create an account through [PDD Web Services](https://cdr.ffiec.gov/public/PWS/PWSPage.aspx) and generate the token after logging in. Note the header is `Authentication`, not `Authorization`.

`user-id` and `token` from `opts` are masked in call records, and they go together: one without the other is an error, so a call never pairs one tenant's user with another's token. Present but blank or null, either is an error naming the key, never a fallback to the Context Variable. A per-call `base-url` needs both in the same `opts`: context credentials are never sent to a per-call URL.

## Quick start

```hot
::ffiec ::ffiec

// What's available
periods ::ffiec/reporting-periods()
period last(periods)

// Who should file, and who has filed since your last run
panel ::ffiec/panel-of-reporters(period)
filers ::ffiec/filers-since-date(period, "04/15/2025 9:00 PM")

// One bank's report
report ::ffiec/facsimile(period, first(filers), {format: "PDF"})
report.bytes
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::ffiec` | Periods: `reporting-periods`, `ubpr-reporting-periods`. Filers: `panel-of-reporters`, `filers-since-date`, `filers-submission-date-time`. Files: `facsimile`, `ubpr-xbrl-facsimile`, `facsimile-bytes`, `is-missing-facsimile`. Helpers: `period-date`, `filer-headers` |
| `::ffiec::api` | `resolve`, `auth-headers`, `request`, `get-list`, `decode-list`, `http-error`; type `HttpError` |

Periods can be `MM/DD/YYYY` or `YYYY-MM-DD`. `facsimile` takes `format` (`"XBRL"` by default, `"PDF"` or `"SDF"`) and `fi-id-type` (`"ID_RSSD"` by default, `"FDICCertNumber"`, `"OCCChartNumber"` or `"OTSDockNumber"`).

## Pagination

The service doesn't paginate: each list comes back whole. To read only new filings, follow [the spec's process](https://cdr.ffiec.gov/Public/Files/SIS611_-_Retrieve_Public_Data_via_Web_Service.pdf): `reporting-periods` about once a quarter, `filers-since-date` daily or weekly with the time of your last run, then `facsimile` for each filer. The package keeps no state, so the caller holds that time.

## Errors

Non-2xx responses come back as an Err carrying `::ffiec::api/HttpError`:

- `source`: `"ffiec"`
- `status`, `url`, `headers`, `body`
- `retry-after`: seconds, parsed from `Retry-After` when the response has one
- `message`: a one-line summary built from the service's own error text, never a credential

The service reports an expired or invalid token as a 400, a missing or bad JWT as a 401, an exhausted quota as a 403, and a missing or invalid header as a 500 with `Error Code 5001`. `facsimile` and `ubpr-xbrl-facsimile` return null rather than an Err when the service has no such file (a 204, or the 400 that says so), and the list calls return `[]` for a 204. Missing credentials are an Err before any request. Nothing retries inside a run. Use a handler or schedule `retry` policy for 5xx.

## Notes and limits

- **Call limit**: about 2,500 calls per hour. Past it the service answers 403 until the next hour. The spec suggests spacing requests 1.5 seconds apart, which is 2,400 an hour.
- **Tokens** expire: generate a new one at least every 90 days. An account is removed after a year of inactivity.
- **Facsimile encoding**: the spec calls the body "a byte stream array" and doesn't say how it's encoded. `facsimile-bytes` reads a JSON string as base64, a JSON list as byte values, and anything else as the raw file, and is an Err on other JSON. It has not been run against the live service.
- **Header names**: the spec writes `UserID` and `dataSeries` in mixed case. HTTP treats header names as case-insensitive, and Hot sends them in lower case.
- **Panel rows**: the spec's sample prints `ZIP` with a leading zero, which JSON can't hold, and lists it as an Int. Treat it as possibly a number with its leading zero lost.
- **XBRL** files need the taxonomy from the PDD site to interpret them. SDF files are text.

## License

Apache-2.0 - see [LICENSE](LICENSE)
