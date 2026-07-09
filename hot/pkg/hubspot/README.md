# hubspot

HubSpot CRM bindings for Hot. One generic CRM v3 objects surface (CRUD, search, associations) that covers contacts, companies, deals, tickets, and custom objects — with typed conveniences, activity notes, and v3 webhook verification on the `hot.dev/webhooks` recipes.

## Setup

| Context Variable | Description |
|---|---|
| `hubspot.token` | Private-app access token (`pat-...`) |
| `hubspot.client.secret` | App client secret, for webhook verification |

## Usage

```hot
::contacts ::hubspot::contacts
::deals ::hubspot::deals
::notes ::hubspot::notes

contact ::contacts/create-contact({email: "ada@example.com", firstname: "Ada"})
found ::contacts/find-by-email("ada@example.com")

deal ::deals/create-deal({dealname: "Acme expansion", amount: "24000", dealstage: "qualifiedtobuy"})
::hubspot::objects/associate("deals", deal.id, "contacts", contact.id)

::notes/log-note("deals", deal.id, "Champion confirmed budget on today's call.")

// Any object type, uniformly
::hubspot::objects/search-objects("tickets", {
  filters: [{propertyName: "hs_pipeline_stage", operator: "EQ", value: "1"}]
})

// Webhooks (X-HubSpot-Signature-v3, 5-minute replay window, fail-closed)
is-valid ::hubspot::webhooks/verify-request(request)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
