# sec-iapd

[SEC Investment Adviser Public Disclosure](https://adviserinfo.sec.gov/) for Hot, with no key. Reads the monthly Form ADV Part 1 data files and ADV-W withdrawal files, gives one row per filing with regulatory assets and private fund totals, and looks firms up by name or CRD number.

Form ADV isn't filed on EDGAR, so [`sec-edgar`](../sec-edgar) can't read it. An adviser files through IARD, and the SEC publishes the result here. The two packages join on Form D file numbers and CIKs.

## Install

Add it to your project's deps in `hot.hot`:

```hot
hot.project.my-app.deps {
  "hot.dev/sec-iapd": "0.1.1"
}
```

## Auth and settings

There's no credential. The User-Agent comes from per-call `opts` first, then the Context Variable, then the default (`::sec::iapd::api/resolve`).

| Context Variable | opts key | Default | Description |
|---|---|---|---|
| `sec.iapd.user-agent` | `user-agent` | `hot-dev-sec-iapd-pkg/0.1 (+https://hot.dev)` | Sent with every request |

`user-agent` from `opts` is masked in call records and never written to ctx. Present but blank or null, it is an error naming the key, never a fallback to the Context Variable.

## Quick start

```hot
::iapd ::sec::iapd

// One month, one row per filing
rows ::iapd/filings(::iapd/filing-data(2026, 10))

// New SEC registrations
filter(rows, (r) { in("initial-registration", r.types) })

// Registered advisers near $100M
filter(rows, (r) {
  and(is-some(r.regulatory-aum), between(r.regulatory-aum, 90000000, 110000000))
})

// Exempt advisers near $150M in private funds
filter(rows, (r) {
  and(eq(r.adviser-type, "exempt"), between(r.private-fund-assets, 120000000, 150000000))
})

// Firms withdrawing
::iapd/withdrawals(2026, 10)

// One firm
res ::iapd/firm-search("Acme Capital", {limit: 5})
::iapd/firm(first(res.firms).crd).basicInformation
```

## Functions by area

| Namespace | Functions |
|---|---|
| `::sec::iapd` | Index: `reports`, `report-files`, `find-report`. Form ADV: `filing-data`, `parse-filing-data`, `table-names`, `table-name`, `filings`, `filing-type-labels`. ADV-W: `withdrawals`, `parse-withdrawals`. Firms: `firm-search`, `parse-firm-search`, `firm`, `parse-firm`. Cells: `amount`, `iso-date`, `repair-quotes`, `parse-table`, `table-text`. URLs: `reports-index-url`, `report-url`, `firm-feed-url`, `firm-url`, `firm-search-url`, `crd-digits` |
| `::sec::iapd::api` | `resolve`, `headers`, `request`, `get-json`, `json-body`, `query-string`, `http-error`; type `HttpError` |

## Form ADV data files

`filing-data(year, month)` reads what `filings` needs from the month's zip of 101 tables and returns `{year, month, url, ...tables}`, or null when that month isn't posted. Rows keep the file's column names, every value is a Str, and rows of one filing share `FilingID`. `opts.tables` reads whole tables by name instead; `table-names` lists them.

`filings(data)` joins the default tables into one Map per filing. The `filings` docs list every key.

**A monthly file holds that month's filings, not every adviser.** A firm appears when it files. Most annual updates land in March, so a change in a firm's assets can sit unseen for most of a year. This package remembers nothing. To watch for a change, store each firm's last value.

**Exempt reporting advisers have no regulatory assets.** They file Items 1, 2, 3, 6, 7, 10 and 11 only, so `regulatory-aum` is null for them. Use `private-fund-assets`, which sums the gross asset value of the funds in Schedule D 7.B.(1) and leaves out feeders, because a master fund holds the same money. It's a sum of what the funds report. It isn't the private fund adviser exemption's test, which also counts uncalled commitments.

**Form PF is confidential.** Nothing here reads it, and nothing can. Form ADV's assets are the nearest public sign.

### Sizes

Most months are 7 to 9 MB. **March is about 31 MB**, because most firms file their annual update then. That's past the default `http.max-response-bytes` of 16 MiB, so reading March needs this in `hot.hot`:

```hot
hot.http.max-response-bytes 67108864
```

Reading a large month takes far more memory on Hot 3.0.3 than the file's size: `filing-data` peaked at about 22 GB for March and about 3.5 GB for a 7 MB month. [hot-dev/hot#270](https://github.com/hot-dev/hot/pull/270) brings those to about 1.7 GB and 370 MB.

### People in the data

By default `filing-data` keeps only the columns `filings` reads. That leaves out the firm's address, which can be a home, and every table that names people: `IA_Schedule_A_B` and `ERA_Schedule_A_B` (owners and officers), the `*_DRP_*` tables (disciplinary events) and `IA_ADV_1J_1K` (the chief compliance officer). `opts.tables` reads whole tables by name. Hot records what a named function is passed, so ask for those only when a run needs them, and don't hand them on to other functions in a large run.

ADV-W files hold each firm's contact person, email and phone. `parse-withdrawals` reads them a row at a time and keeps firm fields only.

### Quotes and encoding in the files

The files don't escape quotes consistently: one row writes `"KF ("X") LP"`, another `"BOTH ""A"" AND ""B"""`, and a third ends a value with a stray quote. A table that parses as it stands is left alone. One that doesn't goes through `repair-quotes`, which fixes those spots. Tables are read with `strict: false`, because a few rows split wrongly: a row with too many cells keeps the first ones, and a short row is padded with null. March 2026's `IA_Schedule_D_7B1` has a handful in 68,256.

Everything `filings` reads parsed in the seven months tried between August 2025 and October 2026. Some tables stay unreadable and come back as an Err: `IA_Schedule_D_7B1A3b` in October 2026 and March 2026, and `IA_DRP_Civil_Judicial` and `ERA_DRP_Civil_Judicial` in March 2026.

Most files from January to July 2025 are Latin-1, not UTF-8, and hot-std can't read that. Those months come back as an Err naming the table. Later files are UTF-8.

## Firm lookups

`firm-search` and `firm` use the endpoint behind the IAPD site. The SEC doesn't document it as a public API, so its response shape can change. `firm` returns null when there's no such adviser.

## Daily firm lists

The SEC also publishes every firm's current record each day. `firm-feed-url(date)` builds the address, for `"SEC"` or `"STATE"`. This package doesn't read it: it's XML of about 79 MB unpacked, past the default `compression.max-output-bytes` of 64 MiB.

## Errors

Non-2xx responses come back as an Err carrying `::sec::iapd::api/HttpError`, with `source` (`"sec-iapd"`), `status`, `url` and `message`. A transport failure passes through as its own Err. Nothing retries inside a run. Use a handler or schedule `retry` policy.

## Notes and limits

- **Use of the data**: the IAPD site says its information "is considered public information and may be distributed or copied", and asks that the SEC seal and logos not be used. `IARD` is a registered service mark. FINRA's BrokerCheck has its own terms, which limit use and name unsolicited marketing, and this package doesn't touch it.
- **Accuracy**: the SEC says neither it nor the states approved what's filed on Form ADV, and that it can't guarantee its accuracy.
- The index lists files from January 2025. The SEC's site has older reports in other forms.
- This package isn't affiliated with or approved by the SEC.

## License

Apache-2.0 - see [LICENSE](LICENSE)
