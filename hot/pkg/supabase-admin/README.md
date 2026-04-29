# supabase-admin

Supabase Management API bindings for Hot. Manage projects, organizations, databases, edge functions, preview branches, and custom domains programmatically.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/supabase-admin": "1.1.0"
```

## Configuration

Set the `supabase.access.token` context variable to a Personal Access Token (PAT) via the Hot app.

Generate a PAT at [supabase.com/dashboard/account/tokens](https://supabase.com/dashboard/account/tokens).

## Usage

### List Projects

```hot
::projects ::supabase::admin::projects

projects ::projects/list-projects()
// => [{id: "abc", name: "My App", region: "us-east-1", ...}, ...]
```

### Get Project API Keys

```hot
::projects ::supabase::admin::projects

keys ::projects/get-api-keys("my-project-ref")
// => [{name: "anon", api_key: "eyJ..."}, {name: "service_role", api_key: "eyJ..."}]
```

### Create a Project

```hot
::projects ::supabase::admin::projects

project ::projects/create-project(::projects/CreateProjectRequest({
  name: "My New App",
  organization_id: "org-abc-123",
  region: "us-east-1",
  db_pass: "secureDbPassword123!"
}))
```

### Pause / Restore a Project

```hot
::projects ::supabase::admin::projects

::projects/pause-project("my-project-ref")
::projects/restore-project("my-project-ref")
```

### List Organizations

```hot
::orgs ::supabase::admin::organizations

orgs ::orgs/list-organizations()
members ::orgs/list-members("my-org-slug")
```

### Run a SQL Query

```hot
::databases ::supabase::admin::databases

result ::databases/run-query(::databases/RunQueryRequest({
  project_ref: "my-project-ref",
  query: "SELECT count(*) FROM auth.users"
}))
```

### Manage Edge Functions

```hot
::functions ::supabase::admin::functions

// List functions
fns ::functions/list-functions("my-project-ref")

// Deploy a function
::functions/create-function(::functions/CreateFunctionRequest({
  project_ref: "my-project-ref",
  slug: "hello-world",
  name: "hello-world",
  body: "Deno.serve(() => new Response('Hello!'))",
  verify_jwt: true
}))

// Delete a function
::functions/delete-function("my-project-ref", "hello-world")
```

### Preview Branches

```hot
::branches ::supabase::admin::branches

// List branches
branches ::branches/list-branches("my-project-ref")

// Create a branch
::branches/create-branch(::branches/CreateBranchRequest({
  project_ref: "my-project-ref",
  branch_name: "feat-auth",
  git_branch: "feat/auth"
}))

// Merge back to parent
::branches/merge-branch("branch-id")
```

### Custom Domains

```hot
::domains ::supabase::admin::domains

// Configure custom hostname
::domains/update-custom-hostname(::domains/UpdateCustomHostnameRequest({
  project_ref: "my-project-ref",
  custom_hostname: "api.myapp.com"
}))

// Verify DNS and activate
::domains/verify-custom-hostname("my-project-ref")
::domains/activate-custom-hostname("my-project-ref")
```

## API Base URL

`https://api.supabase.com/v1`

## Modules

| Module | Description |
|--------|-------------|
| `::supabase::admin::projects` | Project CRUD, pause/restore, API keys, config, health, usage |
| `::supabase::admin::organizations` | Organization CRUD, members, org projects |
| `::supabase::admin::databases` | Postgres config, backups, migrations, SQL query, replicas, upgrades |
| `::supabase::admin::functions` | Edge function CRUD, deploy, get source |
| `::supabase::admin::branches` | Preview branch CRUD, push, reset, merge |
| `::supabase::admin::domains` | Custom hostnames, vanity subdomains |
| `::supabase::admin::api` | Low-level Management API client |
| `::supabase::admin::core` | Shared configuration (BASE_URL) |

## Integration Tests

### 1. Generate a Personal Access Token

1. Go to [supabase.com/dashboard/account/tokens](https://supabase.com/dashboard/account/tokens)
2. Click **Generate new token**
3. Give it a name and copy the token

### 2. Set Context Variables

```
supabase.access.token=sbp_your_personal_access_token
```

### 3. Run the Tests

```bash
hot test hot/pkg/supabase-admin/integration-test/
```

The tests are read-only -- they list projects, organizations, members, API keys, and health checks. No projects or resources are created or modified.

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `projects.hot` | List projects, get project, API keys, health | Read-only |
| `organizations.hot` | List orgs, get org, list members | Read-only |

## Documentation

- [Supabase Management API Reference](https://supabase.com/docs/reference/api/introduction)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/supabase-admin)

## License

Apache-2.0 - see [LICENSE](LICENSE)
