---
description: "Authenticate with Hot Dev Cloud using the CLI, API keys, bearer tokens, project tokens, and scoped access controls."
---

# Authentication

The [Hot API](/docs/api) supports three credential types for authenticating requests. All are passed in the `Authorization` header as Bearer tokens and scoped to an environment.

| Credential | Token Format | Lifetime | Purpose |
|-----------|-------------|----------|---------|
| **API Key** | `hot_<uuid>_<secret>` | Long-lived | Full environment access for you and your team |
| **Service Key** | `<uuid>_<secret>` | Long-lived (optional expiry) | Permission-scoped access for your customers and integrations |
| **Session** | `s_<uuid>_<secret>` | Short-lived (1h default, 24h max) | Ephemeral, permission-scoped access for browser clients |

```bash
curl -H "Authorization: Bearer $TOKEN" https://api.hot.dev/api/v1/projects
```

## API Keys

**API Keys** are your primary credentials for accessing the Hot API. Create and manage them from the [Hot App](/docs/app) dashboard.

- Create keys with descriptive names
- Enable or disable keys without deleting them
- Keys are scoped to environments

### Access Levels

When creating or editing an API key, you choose between two access levels:

- **Full Access** — Unrestricted access to all API endpoints (the default)
- **Restricted** — Limit the key to specific capabilities using the [permissions model](#permissions-model)

## Service Keys

**Service Keys** are long-lived, permission-scoped credentials designed for your customers and external integrations. If you're building a platform on Hot Dev and need to give your customers direct API access (e.g., to [MCP tools](/docs/mcp) or streams), service keys let you issue narrowly scoped tokens under your API key with granular permissions.

Service keys can carry **customer metadata** — arbitrary JSON that is encrypted at rest and automatically available to your Hot functions at runtime. This lets you identify callers and pass customer context (e.g., account ID, plan tier) without requiring extra parameters in every request.

### Creating Service Keys

From the [Hot App](/docs/app), click **New Service Key**. You'll specify:

- **Name** — A human-readable label (e.g., "Acme Corp Production")
- **Description** — What the key is for
- **Metadata** — Optional JSON attached to the key (e.g., `{"customer_id": "acme-123"}`). Encrypted at rest and available to your Hot functions at runtime via `req.auth.service-key.meta`. Use this to pass customer context into your functions without requiring extra parameters. See [Caller Identity](/docs/mcp#caller-identity-hotrequest) for details.
- **Permissions** — A granular permission map using the [permissions model](#permissions-model) below.
- **Expiration** — Optional. Leave empty for a key that never expires.

The generated token is displayed **only once** at creation time. It has no `hot_` prefix, making it suitable for white-label integrations where your customers shouldn't see Hot branding.

### Managing Service Keys

From the detail view, you can:

- View the key's permissions, metadata, and timestamps
- See when the key was last used
- **Revoke** the key to immediately invalidate it

Revoked and expired keys remain visible for audit purposes. Service keys can also be managed programmatically via the [Service Keys API](/docs/api#service-keys).

## Sessions

**Sessions** are short-lived tokens with granular permissions. Use them when you need to grant temporary, narrowly scoped access — for example, giving a browser client read-only access to a specific stream.

Sessions can only be created by API keys (not by other sessions or service keys). The session's permissions must be a subset of the parent API key's permissions.

See the [Sessions API](/docs/api#sessions) for endpoints to create, list, and revoke sessions.

## Permissions Model

API keys, service keys, and sessions all share the same permissions model. Permissions are a JSON map of resource URNs to action arrays:

```json
{
  "mcp:weather": ["execute"],
  "event:*": ["create"]
}
```

| Permission | Format | Description |
|------------|--------|-------------|
| **Full Access** | `{"*:*": ["*"]}` | Unrestricted access to all API endpoints |
| **MCP** | `{"mcp:*": ["execute"]}` | Invoke MCP tools across all services |
| **MCP (specific)** | `{"mcp:weather": ["execute"]}` | Invoke MCP tools in a specific service only |
| **Events** | `{"event:*": ["create", "read"]}` | Publish and read events |
| **Builds** | `{"build:*": ["create", "read"]}` | Upload builds and deploy them |
| **Context Variables** | `{"ctx:*": ["create", "read", "update", "delete"]}` | Manage context variables |
| **Webhooks** | `{"webhook:*": ["execute"]}` | Access webhook endpoints that require API key authentication |
| **Webhooks (specific)** | `{"webhook:internal": ["execute"]}` | Webhook access for a specific service only |

This allows you to create credentials narrowly scoped to just the capabilities needed—useful for giving an AI agent access to specific MCP tools, or restricting a webhook caller to a single service.

### Permissions Builder

When creating or editing a restricted API key or service key, the [Hot App](/docs/app) dashboard provides an interactive **permissions builder** instead of requiring manual JSON editing.

**Quick Presets** — One-click buttons to add common permission rules:

| Preset | Adds |
|--------|------|
| Read Only | `run:*` → read, `stream:*` → read, `event:*` → read, `build:*` → read |
| MCP Tools | `mcp:*` → execute |
| Events | `event:*` → create, read |
| Builds | `build:*` → create, read |
| Context Vars | `ctx:*` → create, read, update, delete |
| Webhooks | `webhook:*` → execute |

Presets add rules to the builder — they don't replace existing rules.

**Rule Builder** — Each rule has three parts:

1. **Resource Type** — Select a type from the dropdown (`mcp`, `event`, `build`, `ctx`, `webhook`, `run`, `stream`, `call`), or `All (*)` for a wildcard that covers every type.
2. **Path** — The resource path, usually `*` (all resources of that type) or a specific service name (e.g., `weather`). When the resource type is `All (*)`, the path is locked to `*`.
3. **Actions** — Check one or more actions: `create`, `read`, `update`, `delete`, `execute`, or `*` (all actions). The available actions depend on the resource type.

Click **Add Rule** to add additional rules. Rules with no actions checked are silently excluded from the saved permissions.

### Stale Service Permissions

Because MCP tools and webhook endpoints are defined by metadata in your source code, a service can be removed when code is redeployed without those definitions. If a credential has a permission referencing a service that is no longer deployed, the key list shows an amber warning badge next to that permission. The edit page displays a banner listing the stale services.

Stale permissions are **preserved intentionally**—if the service is redeployed, the credential immediately works again without reconfiguration. You can remove stale permissions manually from the edit page if the service is permanently retired.

See the [Hot API](/docs/api) documentation for the full API reference.
