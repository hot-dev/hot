# crm

CRM-agnostic operations for Hot agents: `upsert-contact`, `find-contact`, `log-activity`, `upsert-company` — against whichever CRM the workspace has configured (HubSpot, Salesforce, or Attio; selection by credentials, in that order). Every normalized value carries `provider` and the `raw` payload; drop to the provider-native packages for anything deeper.

```hot
contact ::crm/upsert-contact({email: "ada@example.com", first-name: "Ada", last-name: "Lovelace"})
::crm/log-activity(contact, "Demo call went well; wants pricing for 50 seats.")
```

Activity maps to a HubSpot note, a Salesforce completed Task, or an Attio note.

## License

Apache-2.0 - see [LICENSE](LICENSE)
