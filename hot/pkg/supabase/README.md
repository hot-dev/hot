# supabase

Supabase client SDK for Hot. Query your database (PostgREST), authenticate users (GoTrue), manage file storage, and invoke edge functions.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/supabase": "1.1.0"
```

## Configuration

Set these context variables via the Hot app:

| Variable | Required | Description |
|----------|----------|-------------|
| `supabase.url` | Yes | Project URL (e.g. `https://xxxx.supabase.co`) |
| `supabase.anon.key` | Yes | Anon/public API key |
| `supabase.service.key` | For admin ops | Service role key (bypasses RLS) |

Find these in your Supabase Dashboard under **Settings > API**.

## Usage

### Database Queries

```hot
::db ::supabase::db
::f ::supabase::filters

// Select with filters
users ::db/select(::db/SelectRequest({
  table: "users",
  select: "id,name,email",
  filters: [::f/gte("age", 18), ::f/eq("active", true)],
  order: "created_at.desc",
  limit: 10
}))

// Select with relations
posts ::db/select(::db/SelectRequest({
  table: "posts",
  select: "title,content,author:users(name,email)"
}))

// Single row
user ::db/select(::db/SelectRequest({
  table: "users",
  filters: [::f/eq("id", "abc-123")],
  single: true
}))
```

### Insert, Update, Delete

```hot
::db ::supabase::db
::f ::supabase::filters

// Insert
row ::db/insert(::db/InsertRequest({
  table: "posts",
  data: {title: "Hello", body: "World", author_id: "abc-123"}
}))

// Update
::db/update(::db/UpdateRequest({
  table: "posts",
  data: {title: "Updated Title"},
  filters: [::f/eq("id", row[0].id)]
}))

// Delete
::db/delete(::db/DeleteRequest({
  table: "posts",
  filters: [::f/eq("id", row[0].id)]
}))

// Upsert (insert or update on conflict)
::db/upsert(::db/UpsertRequest({
  table: "products",
  data: [{sku: "X1", name: "Widget", price: 10}],
  on-conflict: "sku"
}))
```

### Stored Procedures (RPC)

```hot
::db ::supabase::db

result ::db/rpc(::db/RpcRequest({
  fn-name: "add_them",
  params: {a: 1, b: 2}
}))
// => 3
```

### Filter Helpers

```hot
::f ::supabase::filters

::f/eq("status", "active")        // status=eq.active
::f/neq("role", "guest")          // role=neq.guest
::f/gt("age", 21)                 // age=gt.21
::f/gte("age", 18)                // age=gte.18
::f/lt("price", 100)              // price=lt.100
::f/lte("price", 50)              // price=lte.50
::f/like("name", "John*")         // name=like.John*
::f/ilike("email", "*@gmail.com") // email=ilike.*@gmail.com
::f/in_("id", [1, 2, 3])          // id=in.(1,2,3)
::f/is_("deleted_at", "null")     // deleted_at=is.null
::f/not_("status", "eq.archived") // status=not.eq.archived
::f/or_(["age.lt.18", "age.gt.65"])  // or=(age.lt.18,age.gt.65)
```

### Authentication

```hot
::auth ::supabase::auth

// Sign up
result ::auth/sign-up(::auth/SignUpRequest({
  email: "alice@example.com",
  password: "securePassword123",
  data: {name: "Alice"}
}))

// Sign in with password
session ::auth/sign-in-password(::auth/SignInPasswordRequest({
  email: "alice@example.com",
  password: "securePassword123"
}))
session.access_token  // use this for authenticated requests

// Get current user
user ::auth/get-user(session.access_token)

// Update user metadata
::auth/update-user(::auth/UpdateUserRequest({
  data: {name: "Alice Smith"}
}), session.access_token)

// Refresh token
new-session ::auth/refresh-token(session.refresh_token)

// Sign out
::auth/sign-out(session.access_token)

// Password recovery
::auth/recover-password("alice@example.com")
```

### Admin Auth (Service Role Key)

```hot
::admin ::supabase::auth-admin

// List users
result ::admin/list-users(::admin/ListUsersRequest({page: 1, per_page: 50}))

// Create user (skipping email verification)
user ::admin/create-user(::admin/CreateUserRequest({
  email: "bob@example.com",
  password: "tempPassword123",
  email_confirm: true
}))

// Delete user
::admin/delete-user(user.id)
```

### File Storage

```hot
::storage ::supabase::storage

// Upload a file
::storage/upload("avatars", "users/alice.txt", "Hello World", "text/plain")

// Download from private bucket
content ::storage/download("documents", "report.pdf")

// List objects
objects ::storage/list-objects(::storage/ListObjectsRequest({
  bucket: "avatars",
  prefix: "users/",
  limit: 100
}))

// Public URL (no API call)
url ::storage/get-public-url("avatars", "users/alice.jpg")

// Signed URL for temporary access
result ::storage/create-signed-url("documents", "report.pdf", 3600)

// Bucket management
::storage/create-bucket(::storage/CreateBucketRequest({
  name: "uploads",
  public: false,
  file_size_limit: 10485760
}))
```

### Edge Functions

```hot
::functions ::supabase::functions

result ::functions/invoke(::functions/InvokeFunctionRequest({
  name: "process-order",
  body: {order_id: "abc-123"}
}))
```

## Modules

| Module | Description |
|--------|-------------|
| `::supabase::db` | Database CRUD via PostgREST (select, insert, update, delete, upsert, rpc) |
| `::supabase::filters` | PostgREST filter helpers (eq, gt, lt, in, like, or, and, etc.) |
| `::supabase::auth` | User authentication (sign up, sign in, OTP, refresh, password recovery) |
| `::supabase::auth-admin` | Admin user management with service role key |
| `::supabase::storage` | File storage (buckets, upload, download, signed URLs) |
| `::supabase::functions` | Edge function invocation |
| `::supabase::api` | Low-level HTTP client with dual-header auth |
| `::supabase::core` | Project URL and key helpers |

## Integration Tests

### 1. Create a Supabase Project

1. Go to [supabase.com](https://supabase.com) and create a project
2. In **Settings > API**, copy your **Project URL**, **anon key**, and **service_role key**

### 2. Create a Test Table

Run this SQL in the Supabase SQL Editor:

```sql
CREATE TABLE test_items (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  name text NOT NULL,
  value integer DEFAULT 0,
  created_at timestamptz DEFAULT now()
);

ALTER TABLE test_items ENABLE ROW LEVEL SECURITY;

CREATE POLICY "Allow all for anon" ON test_items
  FOR ALL USING (true) WITH CHECK (true);

CREATE UNIQUE INDEX test_items_name_idx ON test_items (name);
```

### 3. Create a Test Storage Bucket

In the Supabase Dashboard, go to **Storage** and create a bucket called `hot-test-bucket`.

### 4. Set Context Variables

```
supabase.url=https://xxxx.supabase.co
supabase.anon.key=eyJ...
supabase.service.key=eyJ...
```

### 5. Set Environment Variables

```
SUPABASE_TEST_TABLE=test_items
SUPABASE_TEST_BUCKET=hot-test-bucket
SUPABASE_TEST_FUNCTION=hello-world
```

`SUPABASE_TEST_FUNCTION` is optional -- only needed if you have an edge function deployed.

### 6. Run the Tests

```bash
hot test hot/pkg/supabase/integration-test/
```

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `db.hot` | Insert, select, update, delete, upsert, filters | Creates/deletes rows in test table |
| `auth.hot` | Sign up, sign in, get user, refresh, sign out, admin list | Creates/deletes a test user |
| `storage.hot` | Bucket create/delete, upload, list, signed URL, delete | Creates/deletes a temp bucket + file |
| `functions.hot` | Invoke edge function | Calls the test function |

All tests clean up after themselves.

## Documentation

- [Supabase Documentation](https://supabase.com/docs)
- [PostgREST API Reference](https://docs.postgrest.org/)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/supabase)

## License

Apache-2.0 - see [LICENSE](LICENSE)
