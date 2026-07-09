# salesforce

Salesforce bindings for Hot: SObject CRUD (including external-id upserts) and SOQL queries, with connected-app authentication built on the `jwt` and `oauth2` primitives.

## Setup

Set ONE auth mode in context (checked in order):

| Context Variables | Flow |
|---|---|
| `salesforce.access.token` + `salesforce.instance.url` | A session you manage |
| `salesforce.client-id` + `salesforce.client-secret` | Client-credentials flow |
| `salesforce.client-id` + `salesforce.username` + `salesforce.private.key` | JWT-bearer (RS256 with the connected app's certificate key) |

Optional `salesforce.login.url` for My Domain or sandboxes (`https://test.salesforce.com`).

## Usage

```hot
::sobjects ::salesforce::sobjects
::query ::salesforce::query

lead ::sobjects/create-record("Lead", {LastName: "Lovelace", Company: "AE", Email: "ada@example.com"})
::sobjects/update-record("Lead", lead.id, {Rating: "Hot"})
::sobjects/upsert-record("Contact", "External_Id__c", "u-42", {LastName: "Lovelace"})

open ::query/soql("SELECT Id, Name, StageName FROM Opportunity WHERE IsClosed = false")
more if(open.done, null, ::query/next-page(open.nextRecordsUrl))
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
