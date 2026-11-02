# crm

CRM-agnostic operations for Hot agents and workflows. Contacts, companies, deals, and activities share one normalized surface over HubSpot, Salesforce, and Attio. Every normalized value carries `provider` and the provider's `raw` payload. For anything deeper, use the provider packages (`hot.dev/hubspot`, `hot.dev/salesforce`, `hot.dev/attio`) directly.

## Install

```hot
// hot.hot
hot.project.my-app.deps {
    "hot.dev/crm": "0.1.0"
}
```

## Setup

Configure at least one provider. Its own package documents its context variables.

| Context Variable | Description |
|---|---|
| `hubspot.token` | HubSpot private-app token (see `hot.dev/hubspot`) |
| `salesforce.*` | A complete Salesforce auth flow (see `hot.dev/salesforce`). A bare `salesforce.client-id` does not count. |
| `attio.api.key` | Attio API key (see `hot.dev/attio`) |
| `crm.provider` | `hubspot`, `salesforce`, or `attio`: which to use when several are configured |
| `crm.salesforce.domain-field` | Optional custom Account field holding a bare domain (e.g. `Domain__c`), matched exactly instead of `Website` |
| `crm.attio.deal-owner` | Default Attio deal owner (a workspace member email). Attio requires an owner on new deals. |

The provider is chosen per call. `options.provider` wins, then `crm.provider`, then the first configured of HubSpot, Salesforce, Attio. Every function takes an optional last `options` Map for this.

## Per-call credentials

`options` also takes a section per provider, so one run can act for many accounts and parallel flows don't share credentials. Each section is passed as that package's `opts`, so its keys are the package's own (see `hot.dev/hubspot`, `hot.dev/salesforce` and `hot.dev/attio`). Per-call credentials are masked in call records, and nothing from them is written to ctx.

```hot
tenant {
  provider: "hubspot",
  hubspot: {token: "pat-tenant-b"},
  salesforce: {access-token: "00D...", instance-url: "https://tenant-b.my.salesforce.com"},
  attio: {api-key: "tenant-b-attio-key"}
}

contact ::crm/find-contact("ada@example.com", tenant)
::crm/log-activity(contact, "Called back.", {...tenant, subject: "Call"})
same ::crm/find-contact("ada@example.com", {...tenant, provider: "salesforce"})
```

- The section of the provider in use applies. Without `provider`, the first section in HubSpot, Salesforce, Attio order picks the provider, ahead of `crm.provider`. A `Contact` or `Deal` you pass in still uses its own `provider`, with that provider's section.
- A section that is present but not a Map is an error, and so is any credential its package refuses, such as a blank `token`. Neither falls back to context.

## Quick start

```hot
::crm ::crm

contact ::crm/upsert-contact({email: "ada@example.com", first-name: "Ada", last-name: "Lovelace", company: "Analytical Engines"})
::crm/log-activity(contact, "Demo call went well; wants pricing for 50 seats.", {subject: "Demo call"})
recent ::crm/list-activities(contact, {limit: 5})

company ::crm/upsert-company({domain: "https://www.analytical-engines.example/", name: "Analytical Engines"})
deal ::crm/upsert-deal({
  name: "Analytical Engines: 50 seats",
  stage: "Prospecting",
  amount: 24000,
  close-date: "2026-12-31",
  company-id: company.id,
  contact-id: contact.id
})
::crm/move-stage(deal, "Negotiation/Review")

// Pick a provider for one call
same ::crm/find-contact("ada@example.com", {provider: "attio"})
```

## Functions

| Function | Returns | Notes |
|---|---|---|
| `upsert-contact(fields[, options])` | `Contact` | By email. `fields`: `email` (required), `first-name`, `last-name`, `phone`, `company` (a name), `company-id`. Only the given fields are written. |
| `find-contact(email[, options])` | `Contact` or null | Exact email |
| `log-activity(contact, body[, options])` | `Activity` | `contact` is a `Contact` or an email. `options.subject` |
| `list-activities(contact[, options])` | Vec of `Activity` | Newest first. `options.limit` (default 20) |
| `upsert-company(fields[, options])` | `Company` | By domain. `fields`: `domain` (required), `name` |
| `find-company(domain[, options])` | `Company` or null | The domain is normalized |
| `upsert-deal(fields[, options])` | `Deal` | `id` updates that deal. Otherwise the deal is matched by exact `name` (within `company-id`). |
| `find-deals(criteria[, options])` | Vec of `Deal` | `name`, `stage`, `company-id`, `contact-id`, `limit` |
| `move-stage(deal, stage[, options])` | `Deal` | `deal` is a `Deal` or an id |

Normalized types:
- `Contact`: `{id, email, first-name, last-name, phone, company-id, provider, raw}`
- `Company`: `{id, name, domain, provider, raw}`
- `Deal`: `{id, name, stage, amount, close-date, company-id, provider, raw}`
- `Activity`: `{id, subject, body, occurred-at, provider, raw}`

Building blocks such as `select-provider`, `normalize-domain`, and the per-provider builders in `::crm::hubspot`, `::crm::salesforce`, and `::crm::attio` are exposed for testing.

## How each provider maps

| | HubSpot | Salesforce | Attio |
|---|---|---|---|
| Contact upsert | `batch/upsert` keyed on email (no search lag) | Find by Email, then create or update | `assert` on `email_addresses` |
| `company` on a contact | The contact's `company` text property. `company-id` associates the company record. | The Account with that exact Name, created if missing, set as `AccountId` | The company with that exact name, created if missing, linked as `company` |
| Company match | `domain` property, via search | Accounts whose `Website` normalizes to the domain, or `crm.salesforce.domain-field` | `assert` on `domains` |
| Deal | Deal with `dealstage` and `pipeline` ids | Opportunity (`StageName`, `CloseDate`). `contact-id` adds an OpportunityContactRole. | `deals` record with a status `stage`, an `owner`, and `value` |
| Activity | Note (`subject` becomes a bold first line) | Completed Task (`Subject`, `Description`) | Note (`subject` becomes the title) |

## Notes and limits

- **Salesforce LastName.** Salesforce requires `LastName` on a new Contact, so it defaults to "Unknown" only when creating one. An update never writes a last name you did not pass.
- **Attio names.** Attio stores a person's name as one value that is written whole. When you pass only `first-name` or only `last-name`, the other part is read from the existing record first, so it is not blanked.
- **Domains** are normalized before matching and writing: lowercase, no scheme, `www.`, port, or path. `https://www.Acme.com/about` and `acme.com` are the same company. Salesforce matches `Website` with `LIKE '%acme.com%'` and then compares the normalized value exactly, so `notacme.com` does not match. Set `crm.salesforce.domain-field` for an exact indexed match on a custom field instead.
- **Deal stages** use the provider's own values: a HubSpot `dealstage` id (resolve labels with `hot.dev/hubspot`'s pipelines module), a Salesforce `StageName`, or an Attio status title. Creating a deal needs `name` and `stage` on every provider, `close-date` on Salesforce, and an owner on Attio. Attio deals have no close date.
- **Deal upserts** without `id` match on the exact name. Several matches is an Err. HubSpot search lags writes by a few seconds, so an immediate second upsert of a new HubSpot deal can create a duplicate. Pass the returned `id` to update.
- **HubSpot search lag.** Company matching and `find-deals` use HubSpot search, which lags writes by a few seconds. Contacts do not: lookups go by email directly, and upserts use batch upsert.
- Errors from the provider packages come back unchanged as Errs, such as their `HttpError` with status and body.

## License

Apache-2.0 - see [LICENSE](LICENSE)
