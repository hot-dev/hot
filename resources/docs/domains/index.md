---
description: "Configure custom domains for Hot Cloud apps, including DNS, certificates, verification, and production routing."
---

# Custom Domains

**Custom Domains** let you map your own domain names (e.g., `mcp.example.com`, `webhook.example.com`) to your Hot Dev environment. Instead of your customers connecting to `api.hot.dev`, they connect to your branded domain.

> **Pro plan required.** Custom domains are available on Pro and Scale plans. If you're on the Starter plan, the Domains page shows an upgrade prompt.

Plan limits: Pro allows up to 5 custom domains per organization, Scale up to 25, and Self-Host is unlimited.

Multiple domains can be mapped to the same environment - for example, `mcp.example.com` and `webhook.example.com` can both route to the full API surface.

## Adding a Domain

Click **Add Domain** in the [Hot App](/docs/app) and enter your domain name. Hot Dev will request a TLS certificate from the configured domain provider and begin provisioning.

The domain detail page guides you through three steps:

1. **Request Certificate** - Hot Dev requests a TLS certificate automatically. DNS validation records appear within a few seconds.
2. **Validate & Issue** - Add the validation CNAME record shown on the detail page to prove domain ownership. Once DNS propagates, the certificate is issued.
3. **Domain CNAME** - Once the certificate is validated, a routing target is created automatically. Add a CNAME pointing your domain to the target shown in the app. This record appears once provisioning completes.

The detail page updates automatically - you can leave it open and watch each step progress without refreshing.

## Verification & Status

Click **Check Status** on the domain detail page to trigger an immediate recheck. When a routing target exists, Check Status also performs a live DNS lookup to verify your domain's CNAME record is pointing to the correct target.

You don't have to keep clicking Check Status - pending domains are also checked automatically in the background. Once DNS propagation completes, your domain will be validated and provisioned without any manual action.

| Status | Meaning |
|--------|---------|
| Pending Validation | Certificate validation CNAME not yet detected |
| Validated | Certificate issued, routing target provisioning in progress |
| Provisioning | Routing target being created or deployed |
| Active | HTTPS is active - domain CNAME is configured and routing traffic |
| Deleting | Domain removal in progress - provider resources are being cleaned up |

## Using Custom Domains

Once a domain is active, use it anywhere you'd use `api.hot.dev`:

- **MCP endpoints** - Point AI agents to `mcp.example.com` instead of `api.hot.dev`
- **Webhook URLs** - Give external services your branded webhook URL
- **API calls** - Use your domain for all Hot API requests

The domain routes to the same Hot API surface, so all existing [API keys](/docs/authentication#api-keys), [MCP services](/docs/mcp), and [webhooks](/docs/webhooks) work automatically.

## Removing a Domain

Click **Remove Domain** on the domain detail page to delete a custom domain. This soft-deletes the domain and queues cleanup of the associated provider resources.

While cleanup is in progress, the domain shows a **Deleting** status. You cannot re-add the same domain name until cleanup completes - attempting to do so will show a message asking you to wait.

When removing a domain, remember to also delete the DNS CNAME records you created for it (both the validation CNAME and the domain CNAME).

## API Access

Custom domains can also be managed programmatically via the [Custom Domains API](/docs/api#custom-domains).
