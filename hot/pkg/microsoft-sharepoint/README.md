# microsoft-sharepoint

SharePoint Online for Hot over Microsoft Graph: find sites (the root site, by URL or path, search, subsites), get their document libraries as drives for [`microsoft-onedrive`](../microsoft-onedrive), read and write lists and list items with the columns you choose, subscribe to list changes, and grant least-privilege app-only access with `Sites.Selected` and `Lists.SelectedOperations.Selected`. Credentials, clouds, retries and errors come from [`microsoft-graph`](../microsoft-graph).

## Install

Add it to your project's deps in `hot.hot` (it pulls in microsoft-graph and microsoft-onedrive):

```hot
hot.project.my-app.deps {
  "hot.dev/microsoft-sharepoint": "0.1.2"
}
```

## Auth and permissions

Set credentials in Context Variables (any [`microsoft-graph`](../microsoft-graph/README.md#authentication) flow), or pass them per call in `opts`:

| Context Variable | Secret | Description |
|---|---|---|
| `microsoft.client.id`, `microsoft.tenant.id` | no | The Entra app and the tenant |
| `microsoft.client.private-key` + `microsoft.client.certificate`, or `microsoft.client.secret` | yes | App-only credential (a certificate is recommended) |
| `microsoft.oauth.refresh-token` | yes | Or a delegated user's refresh token |

| Permission | Type | Enough for |
|---|---|---|
| `Sites.Read.All` / `Sites.ReadWrite.All` | Delegated or application | Every site the user (delegated) or the tenant (application) has |
| `Sites.Selected` | Application or delegated | Only the site collections an admin granted |
| `Lists.SelectedOperations.Selected` | Application or delegated | Only the lists granted |
| `Files.SelectedOperations.Selected` | Application or delegated | Only the files or library folders granted |
| `Sites.FullControl.All` | Application | Granting `*.Selected` access (an admin step), and permission scans |

Application permissions always need a tenant admin's consent. Every function has an options arity for OData options (`select`, `expand`, `filter`, `top`, `orderby`) and microsoft-graph credential keys.

### Per-call credentials

Credentials in `opts` replace context for that call. They are never written to context, and they are masked in call records.

```hot
tenant {client-id: "11111111-2222-3333-4444-555555555555", client-secret: stored-client-secret, tenant-id: "fabrikam.onmicrosoft.com"}
site ::microsoft::sharepoint::sites/get-root-site(tenant)
```

A credential key that is present but blank or null is an error naming the key, never a fallback to context, and `graph-url` or `login-url` needs a credential in the same `opts`. See [microsoft-graph](../microsoft-graph/README.md#per-call-credentials-multi-tenant) for the keys and rules.

## Quick start

```hot
::sites ::microsoft::sharepoint::sites
::lists ::microsoft::sharepoint::lists
::items ::microsoft::onedrive::items
::content ::microsoft::onedrive::content

site ::sites/get-site-by-url("https://contoso.sharepoint.com/sites/engineering")

// Document libraries are drives: use microsoft-onedrive with the library id
lib ::sites/find-drive(site.id, "Documents")
specs ::items/list-all-children("/Specs", ::sites/drive-opts(lib), 200)
doc ::content/read-document(first(specs), "pdf", ::sites/drive-opts(lib))

// Lists
open ::lists/list-all-items(site.id, "Tickets", {
  fields: ["Title", "Status", "AssignedTo"],
  filter: "fields/Status eq 'Open'",
  allow-non-indexed: true
}, 500)
created ::lists/create-item(site.id, "Tickets", {Title: "Printer jam on 3", Status: "Open"})
::lists/update-item(site.id, "Tickets", created.id, {Status: "Resolved"})
```

## Functions by area

**`::microsoft::sharepoint::sites`**: `get-root-site`, `get-site`, `get-site-by-url`, `get-site-by-path`, `parse-site-url`, `search-sites`, `get-all-sites` (app-only), `list-subsites`, `list-drives`, `get-default-drive`, `find-drive`, `drive-opts`.

**`::microsoft::sharepoint::lists`**: `list-lists`, `get-list`, `list-items` (one page), `list-all-items`, `get-item`, `create-item`, `update-item`, `delete-item`, `items-query`, `subscribe-list`.

**`::microsoft::sharepoint::permissions`**: `grant-site`, `list-site-grants`, `get-site-grant`, `update-site-grant`, `delete-site-grant`, `grant-list`, `list-list-grants`, `delete-list-grant`. File and folder grants are `::microsoft::onedrive::sharing/grant-app`.

## Sites

Site ids look like `contoso.sharepoint.com,{site-collection-guid},{web-guid}`; look one up once and keep it. `get-site-by-url` accepts the browser URL of a site, or of a library or page inside it (`parse-site-url` strips `/Shared Documents/...`, `/Lists/...`, `/SitePages/...`). `get-site-by-path("contoso.sharepoint.com", "/sites/eng")` is the explicit form. `search-sites("*")` lists what the caller can see.

## Document libraries

Every library is a drive. `list-drives(site-id)` returns them (`id`, `name`, `webUrl`); pass a library's id as `opts.drive` (or `drive-opts(lib)`) to any `microsoft-onedrive` function: listing, `read-document`, uploads, versions, sharing, and delta sync with `subscribe` for file-triggered workflows. A site's default library is `get-default-drive(site-id)`, or `::microsoft::onedrive/site-drive(site-id)` as a drive reference.

## Lists

Items keep their columns under `fields`. Choose columns with `fields` (internal names: `Title`, `Status`, `AssignedTo`, ...), which becomes `$expand=fields($select=...)`; without it every column comes back. Filtering on `fields/...` needs an indexed column, or `allow-non-indexed: true` to send `Prefer: HonorNonIndexedQueriesWarningMayFailRandomly` (fine below 5,000 items, may fail above). `update-item` PATCHes only the columns you pass. `subscribe-list` registers a webhook (`updated`, up to 42,300 minutes); notifications carry no item data.

## Least-privilege app-only access

`Sites.Selected` and the newer `Lists.SelectedOperations.Selected` / `Files.SelectedOperations.Selected` let an app reach only the sites, lists or files an admin chose. Access needs all three:

1. An admin consents the Selected permission for your app in Entra ID (API permissions, application type).
2. A grant on the resource, made with `grant-site` / `grant-list` (or `::microsoft::onedrive::sharing/grant-app` for a file or folder) and a role: `read`, `write`, `owner` or `fullcontrol`.
3. Your app's token carries the Selected scope (it does once 1 is done).

**Granting is an admin step:** the granting call needs `Sites.FullControl.All` (application, or delegated for a SharePoint Administrator who is a member of the site). Do it once with a separate admin app or an admin's sign-in; your production app holds only `Sites.Selected`.

```hot
::perms ::microsoft::sharepoint::permissions

INDEXER_APP "11111111-2222-3333-4444-555555555555"

// Run once, as the admin app
grant-indexer fn (site-id: Str): Map {
  ::perms/grant-site(site-id, INDEXER_APP, "Contoso Indexer", ["read"], {
    client-id: ::hot::ctx/get("admin.client.id"),
    client-secret: ::hot::ctx/get("admin.client.secret"),
    tenant-id: ::hot::ctx/get("microsoft.tenant.id")
  })
}

// Later: who has access, and revoke
audit fn (site-id: Str): Vec {
  ::perms/list-site-grants(site-id, {client-id: ::hot::ctx/get("admin.client.id"), client-secret: ::hot::ctx/get("admin.client.secret")})
}
```

Then the indexer app (app-only, `Sites.Selected`) reads that site's libraries through `microsoft-onedrive` by drive id. It cannot search or enumerate sites it has no grant on, so store the site id. Grants on lists, list items and files break permission inheritance on that resource; site grants do not. `update-site-grant` changes roles; `delete-site-grant` revokes.

## Notes and limits

- Use site ids, not URLs, in stored configuration: URLs change when sites are renamed.
- `get-all-sites` is app-only and includes OneDrive personal sites unless you filter them out.
- List item `$filter` needs indexed columns on large lists; `$orderby` and `$top` support varies by column type.
- Errors are `::microsoft::graph::api/HttpError` (`accessDenied` usually means a missing grant or consent). Throttling (429/503 with `Retry-After`) is reported, not waited out: check `::microsoft::graph::api/is-retryable(result)` and use a `retry` policy.
- SharePoint is part of Microsoft 365: no separate sales contract. A one-seat Business Basic tenant is enough to test everything here; see [test tenants](../microsoft-graph/README.md#test-tenants).

## License

Apache-2.0 - see [LICENSE](LICENSE)
