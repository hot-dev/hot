---
description: "REST API reference for Hot projects, builds, runs, events, streams, files, secrets, agents, MCP services, and API keys."
---

# HOT API

The Hot API provides programmatic access to manage projects, builds, runs, events, and more.

> **Official SDKs** are available for JavaScript/TypeScript, Python, Go, Rust,
> and Java — see [SDKs](api/sdks). The examples on this page show the raw
> HTTP API; the SDKs wrap every endpoint below.

## Base URL

```
https://api.hot.dev/v1
```

For local development:

```
http://localhost:4681/v1
```

## Authentication

All API requests (except health checks) require a bearer token in the `Authorization` header:

```bash
curl https://api.hot.dev/v1/projects \
  -H "Authorization: Bearer <token>"
```

Hot supports three credential types — **API keys**, **service keys**, and **sessions** — all used the same way. All credentials are scoped to an environment, and resources are automatically filtered to that environment's context.

See the [Authentication](/docs/authentication) documentation for full details on credential types, the permissions model, and the permissions builder.

### Permissions Model {#permissions-model}

API keys, service keys, and sessions share a granular permission system. Permissions are a JSON map of resource URNs to action arrays:

```json
{
  "mcp:weather/get-forecast": ["execute"],
  "stream:*": ["read"],
  "event:user:*": ["create", "read"]
}
```

**Resource URN format:** `type:path`

- `type` is the resource category
- `path` is the resource identifier (`*` for wildcard)

**Resource types and valid actions:**

| Resource Type | Valid Actions | Description |
|--------------|-------------|-------------|
| `mcp` | `execute` | MCP tool invocation (e.g., `mcp:weather/get-forecast`) |
| `webhook` | `execute` | Webhook endpoint access (e.g., `webhook:payments/*`) |
| `stream` | `read` | Stream subscription (e.g., `stream:*` or `stream:<id>`) |
| `event` | `create`, `read` | Event publishing and reading |
| `run` | `read` | Run inspection |
| `project` | `create`, `read`, `update`, `delete` | Project management |
| `build` | `create`, `read`, `execute` | Build management and deployment |
| `context` | `create`, `read`, `update`, `delete` | Context variable management |
| `key` | `create`, `read`, `update`, `delete` | API key management |
| `session` | `create`, `read`, `delete` | Session management |
| `env` | `read` | Environment information |

The wildcard action `*` grants all valid actions for that resource type. The universal wildcard `*:*` with `["*"]` grants unrestricted access.

**Validation Rules:**

Permissions are validated when creating or updating API keys, sessions, and service keys. The following rules apply:

| Rule | Example (rejected) | Error |
|------|--------------------|-------|
| Resource must use `type:path` format | `"no-colon-here"` | Invalid resource |
| Resource key must not be empty | `""` | Invalid resource |
| Type must not be empty | `":path"` | Invalid resource |
| Path must not be empty | `"mcp:"` | Invalid resource |
| Bare `*` is not valid — use `*:*` | `"*"` | Invalid resource |
| `*` type only allows `*` path | `"*:foo"` | Invalid resource |
| Type must be alphanumeric/hyphens | `"mcp!:test"` | Invalid resource |
| Actions must not be empty | `{"mcp:*": []}` | Empty action list |
| Actions are lowercase only | `"Read"`, `"CREATE"` | Invalid action |
| Only valid actions: `create`, `read`, `update`, `delete`, `execute`, `*` | `"destroy"` | Invalid action |
| Action must be valid for the resource type | `"mcp:*": ["create"]` | Action not valid for resource |

Sessions and service keys must also be a **subset** of the parent API key's permissions — you cannot escalate permissions beyond what the issuing key allows.

### Rate Limiting

API requests are rate limited per organization. Limits are based on your subscription plan:

| Plan | Requests per Second |
|------|-------------------|
| Starter | 20 RPS |
| Pro | 100 RPS |
| Scale / Self-Host | Unlimited |

When the limit is exceeded, the API returns `429 Too Many Requests` with a `Retry-After` header indicating how many seconds to wait before retrying.

## Response Format

All responses use a consistent envelope format.

### Success Response (Single Item)

```json
{
  "data": {
    "project_id": "550e8400-e29b-41d4-a716-446655440000",
    "name": "my-project"
  },
  "meta": {
    "request_id": "123e4567-e89b-12d3-a456-426614174000",
    "timestamp": "2024-01-15T10:30:00Z"
  }
}
```

### Success Response (List)

```json
{
  "data": [...],
  "pagination": {
    "total": 42,
    "limit": 20,
    "offset": 0,
    "has_more": true
  },
  "meta": {
    "request_id": "123e4567-e89b-12d3-a456-426614174000",
    "timestamp": "2024-01-15T10:30:00Z"
  }
}
```

### Error Response

```json
{
  "error": {
    "code": "not_found",
    "message": "Project not found",
    "request_id": "123e4567-e89b-12d3-a456-426614174000"
  }
}
```

## Pagination

List endpoints support pagination via query parameters:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `limit` | int | 20 | Maximum results to return |
| `offset` | int | 0 | Number of results to skip |

---

## Endpoints

### Health & Status

#### Get API Status

```http
GET /status
```

Returns API server health information. No authentication required. Note that `/status` is served at the server root — it is the one endpoint **not** under the `/v1` prefix.

**Response:**

```json
{
  "status": "ok",
  "service": "hot.dev api server",
  "version": "1.0.0",
  "git_sha": "abc1234",
  "start_time": "2026-01-15T10:00:00Z"
}
```

---

### Projects

#### List Projects

```http
GET /v1/projects
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

**Response:**

```json
{
  "data": [
    {
      "project_id": "550e8400-e29b-41d4-a716-446655440000",
      "env_id": "660e8400-e29b-41d4-a716-446655440000",
      "name": "my-project",
      "active": true,
      "created_at": "2024-01-15T10:30:00Z",
      "updated_at": "2024-01-15T10:30:00Z"
    }
  ],
  "pagination": {...},
  "meta": {...}
}
```

#### Create Project

```http
POST /v1/projects
```

**Request Body:**

```json
{
  "name": "my-project"
}
```

**Response:** `201 Created` with project data.

#### Get Project

```http
GET /v1/projects/{project_id_or_slug}
```

Supports both UUID and project name (slug) in the URL.

#### Update Project

```http
PATCH /v1/projects/{project_id_or_slug}
```

**Request Body:**

```json
{
  "name": "new-project-name"
}
```

#### Delete Project

```http
DELETE /v1/projects/{project_id_or_slug}
```

**Response:** `204 No Content`

---

### Builds

#### List Builds (All in Environment)

```http
GET /v1/builds
```

Lists all builds across all projects in the environment.

**Response includes** `project_name` for each build.

#### List Builds (By Project)

```http
GET /v1/projects/{project_id_or_slug}/builds
```

#### Get Build

```http
GET /v1/projects/{project_id_or_slug}/builds/{build_id}
```

**Response:**

```json
{
  "data": {
    "build_id": "550e8400-e29b-41d4-a716-446655440000",
    "project_id": "660e8400-e29b-41d4-a716-446655440000",
    "hash": "abc123def456",
    "size": 102400,
    "build_type": "bundle",
    "deployed": false,
    "active": true,
    "created_at": "2024-01-15T10:30:00Z",
    "updated_at": "2024-01-15T10:30:00Z",
    "storage_path": "s3://builds/...",
    "storage_backend": "s3"
  },
  "meta": {...}
}
```

#### Get Deployed Build

```http
GET /v1/projects/{project_id_or_slug}/builds/deployed
```

Returns the currently deployed build for the project, or `404` if none.

#### Get Live Build

```http
GET /v1/projects/{project_id_or_slug}/builds/live
```

Returns the live (development) build for the project, or `404` if none.

#### Upload Build

```http
POST /v1/projects/{project_id_or_slug}/builds
Content-Type: multipart/form-data
```

**Form Fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `file` | Yes | The build zip file |
| `hash` | Yes | SHA hash of the build for validation |
| `build_id` | No | Optional UUID; if provided, enables idempotent uploads |

**Examples:**

<!-- tabs:start -->
#### **curl**

```bash
curl -X POST 'https://api.hot.dev/v1/projects/my-project/builds' \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -F "file=@build.hot.zip" \
  -F "hash=$(sha256sum build.hot.zip | cut -d' ' -f1)"
```

#### **JavaScript**

```javascript
const fs = require('fs');
const crypto = require('crypto');
const FormData = require('form-data');

const file = fs.readFileSync('build.hot.zip');
const hash = crypto.createHash('sha256').update(file).digest('hex');

const form = new FormData();
form.append('file', file, 'build.hot.zip');
form.append('hash', hash);

const response = await fetch(`${BASE_URL}/projects/my-project/builds`, {
  method: 'POST',
  headers: { 'Authorization': `Bearer ${HOT_API_KEY}` },
  body: form
});
```

#### **Python**

```python
import hashlib

with open('build.hot.zip', 'rb') as f:
    file_hash = hashlib.sha256(f.read()).hexdigest()

response = requests.post(
    f'{BASE_URL}/projects/my-project/builds',
    headers={'Authorization': f'Bearer {HOT_API_KEY}'},
    files={'file': open('build.hot.zip', 'rb')},
    data={'hash': file_hash}
)
```
<!-- tabs:end -->

**Response:** `201 Created`

```json
{
  "data": {
    "build_id": "550e8400-e29b-41d4-a716-446655440000",
    "project_id": "660e8400-e29b-41d4-a716-446655440000",
    "hash": "abc123def456",
    "size": 102400,
    "storage_path": "s3://builds/...",
    "storage_backend": "s3",
    "created_at": "2024-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

If the `build_id` already exists, returns `200 OK` with header `X-Build-Exists: true`.

#### Download Build

```http
GET /v1/projects/{project_id_or_slug}/builds/{build_id}/download
```

Returns the build as a zip file with `Content-Type: application/zip`.

#### Deploy Build

```http
POST /v1/projects/{project_id_or_slug}/builds/{build_id}/deploy
```

Marks the build as deployed and queues it for worker processing.

---

### Files

Files are stored per-environment and can be managed through the API. Small files (up to 300 MB) can be uploaded in a single request. For larger files, use the multipart upload flow.

**Per-plan size limits:**

| Plan | Max File Upload |
|------|-----------------|
| Free | 100 MB |
| Starter | 1 GB |
| Pro | 5 GB |
| Scale | 50 GB |
| Self-Host | 50 GB |

#### List Files

```http
GET /v1/files
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `prefix` | string | Filter files by path prefix |
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

**Response:**

```json
{
  "data": [
    {
      "file_id": "550e8400-e29b-41d4-a716-446655440000",
      "path": "uploads/report.pdf",
      "size": 102400,
      "etag": "\"d41d8cd98f00b204e9800998ecf8427e\"",
      "content_type": "application/pdf",
      "storage_backend": "s3",
      "created_by_run_id": null,
      "updated_by_run_id": null,
      "created_at": "2024-01-15T10:30:00Z",
      "updated_at": "2024-01-15T10:30:00Z"
    }
  ],
  "pagination": {"total": 1, "limit": 20, "offset": 0},
  "meta": {...}
}
```

#### Get File Metadata

```http
GET /v1/files/{file_id}
```

Returns metadata for a single file.

#### Download File

```http
GET /v1/files/{file_id}/download
```

Returns the file content as a binary download with appropriate `Content-Type` and `Content-Disposition` headers.

#### Upload File (Simple)

```http
PUT /v1/files/upload/{path}
Content-Type: application/octet-stream
```

Upload a file in a single request. The request body is the raw file content. Suitable for files up to 300 MB (the HTTP body limit). For larger files, use the multipart upload flow below.

<!-- tabs:start -->
#### **curl**

```bash
curl -X PUT 'https://api.hot.dev/v1/files/upload/data/report.csv' \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: text/csv" \
  --data-binary @report.csv
```

#### **JavaScript**

```javascript
const file = fs.readFileSync('report.csv');

const response = await fetch(`${BASE_URL}/files/upload/data/report.csv`, {
  method: 'PUT',
  headers: {
    'Authorization': `Bearer ${HOT_API_KEY}`,
    'Content-Type': 'text/csv'
  },
  body: file
});
```

#### **Python**

```python
with open('report.csv', 'rb') as f:
    response = requests.put(
        f'{BASE_URL}/files/upload/data/report.csv',
        headers={
            'Authorization': f'Bearer {HOT_API_KEY}',
            'Content-Type': 'text/csv'
        },
        data=f
    )
```
<!-- tabs:end -->

**Response:** `200 OK`

```json
{
  "data": {
    "file_id": "550e8400-e29b-41d4-a716-446655440000",
    "path": "data/report.csv",
    "size": 102400,
    "etag": "\"d41d8cd98f00b204e9800998ecf8427e\"",
    "content_type": "text/csv",
    "storage_backend": "s3",
    "created_at": "2024-01-15T10:30:00Z",
    "updated_at": "2024-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

#### Delete File

```http
DELETE /v1/files/{file_id}
```

Returns `204 No Content` on success.

#### Multipart Upload

For files larger than 300 MB (or when you want resumable uploads), use the three-step multipart flow: initiate, upload parts, then complete.

##### 1. Initiate Upload

```http
POST /v1/files/uploads
Content-Type: application/json
```

**Request Body:**

| Field | Required | Description |
|-------|----------|-------------|
| `path` | Yes | Destination file path |
| `expected_size` | No | Expected total size in bytes (enables quota pre-check and determines part count) |
| `content_type` | No | MIME type of the file |

**Example:**

```json
{
  "path": "data/large-dataset.parquet",
  "expected_size": 1073741824,
  "content_type": "application/octet-stream"
}
```

**Response:** `201 Created`

```json
{
  "data": {
    "upload_id": "660e8400-e29b-41d4-a716-446655440000",
    "path": "data/large-dataset.parquet",
    "part_size": 67108864,
    "parts_expected": 16,
    "expires_at": "2024-01-16T10:30:00Z"
  },
  "meta": {...}
}
```

The `part_size` (in bytes) is the recommended size for each part. Use this value when splitting your file.

##### 2. Upload Parts

```http
PUT /v1/files/uploads/{upload_id}/{part_number}
Content-Type: application/octet-stream
```

Upload each part as raw bytes. Part numbers are 1-based.

**Constraints:**
- Non-final parts must be at least 5 MB
- Maximum part size is 256 MB
- Maximum 10,000 parts per upload

**Response:**

```json
{
  "data": {
    "part_number": 1,
    "size": 67108864,
    "etag": "\"a54357aff0632cce46d942af68356b38\""
  },
  "meta": {...}
}
```

##### 3. Complete Upload

```http
POST /v1/files/uploads/{upload_id}/complete
```

Assembles all uploaded parts into the final file. Returns the file metadata (same shape as simple upload response).

##### Abort Upload

```http
DELETE /v1/files/uploads/{upload_id}
```

Cancels the upload and cleans up any uploaded parts. Returns `204 No Content`.

<!-- tabs:start -->
#### **curl**

```bash
# 1. Initiate
UPLOAD=$(curl -s -X POST 'https://api.hot.dev/v1/files/uploads' \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path": "data/large-file.bin", "expected_size": 134217728}')

UPLOAD_ID=$(echo $UPLOAD | jq -r '.data.upload_id')
PART_SIZE=$(echo $UPLOAD | jq -r '.data.part_size')

# 2. Upload parts (split file and upload each chunk)
split -b $PART_SIZE large-file.bin part_
PART_NUM=1
for part in part_*; do
  curl -X PUT "https://api.hot.dev/v1/files/uploads/$UPLOAD_ID/$PART_NUM" \
    -H "Authorization: Bearer $HOT_API_KEY" \
    -H "Content-Type: application/octet-stream" \
    --data-binary @$part
  PART_NUM=$((PART_NUM + 1))
done

# 3. Complete
curl -X POST "https://api.hot.dev/v1/files/uploads/$UPLOAD_ID/complete" \
  -H "Authorization: Bearer $HOT_API_KEY"
```

#### **JavaScript**

```javascript
// 1. Initiate
const initRes = await fetch(`${BASE_URL}/files/uploads`, {
  method: 'POST',
  headers: {
    'Authorization': `Bearer ${HOT_API_KEY}`,
    'Content-Type': 'application/json'
  },
  body: JSON.stringify({
    path: 'data/large-file.bin',
    expected_size: fileBuffer.length
  })
});
const { data: { upload_id, part_size } } = await initRes.json();

// 2. Upload parts
for (let i = 0; i < fileBuffer.length; i += part_size) {
  const partNum = Math.floor(i / part_size) + 1;
  const chunk = fileBuffer.slice(i, i + part_size);
  await fetch(`${BASE_URL}/files/uploads/${upload_id}/${partNum}`, {
    method: 'PUT',
    headers: {
      'Authorization': `Bearer ${HOT_API_KEY}`,
      'Content-Type': 'application/octet-stream'
    },
    body: chunk
  });
}

// 3. Complete
await fetch(`${BASE_URL}/files/uploads/${upload_id}/complete`, {
  method: 'POST',
  headers: { 'Authorization': `Bearer ${HOT_API_KEY}` }
});
```

#### **Python**

```python
import math

# 1. Initiate
file_size = os.path.getsize('large-file.bin')
init = requests.post(
    f'{BASE_URL}/files/uploads',
    headers={'Authorization': f'Bearer {HOT_API_KEY}'},
    json={'path': 'data/large-file.bin', 'expected_size': file_size}
).json()

upload_id = init['data']['upload_id']
part_size = init['data']['part_size']

# 2. Upload parts
with open('large-file.bin', 'rb') as f:
    for part_num in range(1, math.ceil(file_size / part_size) + 1):
        chunk = f.read(part_size)
        requests.put(
            f'{BASE_URL}/files/uploads/{upload_id}/{part_num}',
            headers={
                'Authorization': f'Bearer {HOT_API_KEY}',
                'Content-Type': 'application/octet-stream'
            },
            data=chunk
        )

# 3. Complete
requests.post(
    f'{BASE_URL}/files/uploads/{upload_id}/complete',
    headers={'Authorization': f'Bearer {HOT_API_KEY}'}
)
```
<!-- tabs:end -->

---

### Context Variables (Secrets)

Context variables are encrypted secrets stored per-project. Values are encrypted at rest using AES-256-GCM.

**Security:** Values are never returned via API—only metadata (key, description, timestamps).

#### List Context Variables

```http
GET /v1/projects/{project_id_or_slug}/context
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

**Response:**

```json
{
  "data": [
    {
      "key": "DATABASE_URL",
      "description": "Production database connection",
      "created_at": "2024-01-15T10:30:00Z",
      "updated_at": "2024-01-15T10:30:00Z"
    }
  ],
  "pagination": {...},
  "meta": {...}
}
```

#### Create Context Variable

```http
POST /v1/projects/{project_id_or_slug}/context
```

**Request Body:**

```json
{
  "key": "DATABASE_URL",
  "value": "postgres://user:pass@host/db",
  "description": "Production database connection"
}
```

**Response:** `201 Created` (value is not returned).

#### Update Context Variable

```http
PUT /v1/projects/{project_id_or_slug}/context/{key}
```

**Request Body:**

```json
{
  "value": "postgres://user:newpass@host/db",
  "description": "Updated description"
}
```

#### Delete Context Variable

```http
DELETE /v1/projects/{project_id_or_slug}/context/{key}
```

**Response:** `204 No Content`

---

### Events

#### Publish Event

```http
POST /v1/events
```

Publishes an event that can trigger event handlers.

`event_type` is an arbitrary string chosen by your application (for example `user:created`).
Hot does not enforce a specific naming pattern, but `:`-separated names are the recommended convention.

For comparison:
- Hot language `send(...)` uses `type` and `data`
- HTTP API `POST /v1/events` uses `event_type` and `event_data`

**Request Body:**

```json
{
  "event_type": "user:signup",
  "event_data": {
    "user_id": "123",
    "email": "alice@example.com"
  }
}
```

**Response:** `201 Created`

```json
{
  "data": {
    "event_id": "550e8400-e29b-41d4-a716-446655440000",
    "env_id": "660e8400-e29b-41d4-a716-446655440000",
    "stream_id": "770e8400-e29b-41d4-a716-446655440000",
    "event_type": "user:signup",
    "event_data": {...},
    "event_time": "2024-01-15T10:30:00Z",
    "created_at": "2024-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

#### List Events

```http
GET /v1/events
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

#### Get Event

```http
GET /v1/events/{event_id}
```

#### Get Runs for Event

```http
GET /v1/events/{event_id}/runs
```

Returns all runs triggered by this event.

---

### Runs

#### List Runs

```http
GET /v1/runs
```

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |
| `status` | string | Filter: `running`, `succeeded`, `failed`, `cancelled`, `pending_retry` |
| `type` | string | Filter: `call`, `event`, `schedule`, `run`, `eval`, `repl` |
| `time_range` | string | ISO 8601 duration: `P7D`, `P30D`, etc. |

**Response:**

```json
{
  "data": [
    {
      "run_id": "550e8400-e29b-41d4-a716-446655440000",
      "env_id": "660e8400-e29b-41d4-a716-446655440000",
      "stream_id": "770e8400-e29b-41d4-a716-446655440000",
      "build_id": "880e8400-e29b-41d4-a716-446655440000",
      "run_type": "event",
      "status": "succeeded",
      "start_time": "2024-01-15T10:30:00Z",
      "stop_time": "2024-01-15T10:30:45Z",
      "origin_run_id": null,
      "event_id": "990e8400-e29b-41d4-a716-446655440000",
      "result": {...},
      "project_id": "aa0e8400-e29b-41d4-a716-446655440000",
      "project_name": "my-project"
    }
  ],
  "pagination": {...},
  "meta": {...}
}
```

#### Get Run

```http
GET /v1/runs/{run_id}
```

#### Get Run Statistics

```http
GET /v1/runs/stats
```

**Response:**

```json
{
  "data": {
    "total_runs": 1234,
    "running": 5,
    "succeeded": 1200,
    "failed": 25,
    "cancelled": 4
  },
  "meta": {...}
}
```

---

### Event Handlers

Event handlers are registered in your Hot code and loaded when builds are uploaded.

#### List Event Handlers

```http
GET /v1/projects/{project_id_or_slug}/event-handlers
```

Returns event handlers from the project's deployed build.

**Response:**

```json
{
  "data": [
    {
      "event_handler_id": "550e8400-e29b-41d4-a716-446655440000",
      "build_id": "660e8400-e29b-41d4-a716-446655440000",
      "event_type": "user:signup",
      "ns": "::myapp::handlers",
      "var": "on-user-signup"
    }
  ],
  "pagination": {...},
  "meta": {...}
}
```

---

### Schedules

Schedules are cron-based triggers defined in your Hot code.

#### List Schedules

```http
GET /v1/projects/{project_id_or_slug}/schedules
```

Returns schedules from the project's deployed build.

**Response:**

```json
{
  "data": [
    {
      "schedule_id": "550e8400-e29b-41d4-a716-446655440000",
      "build_id": "660e8400-e29b-41d4-a716-446655440000",
      "cron": "0 0 * * *",
      "ns": "::myapp::tasks",
      "var": "daily-cleanup"
    }
  ],
  "pagination": {...},
  "meta": {...}
}
```

---

### Environment

#### Get Environment Info

```http
GET /v1/env
```

**Response:**

```json
{
  "data": {
    "env_id": "550e8400-e29b-41d4-a716-446655440000",
    "org_id": "660e8400-e29b-41d4-a716-446655440000",
    "name": "production",
    "active": true
  },
  "meta": {...}
}
```

#### Subscribe to Environment Events (SSE)

```http
GET /v1/env/subscribe
```

Subscribe to real-time events for the entire environment via Server-Sent Events (SSE). This endpoint streams all run, event, and stream activity for the environment associated with your API key.

**Response:** `text/event-stream`

**SSE Event Types:**

| Event | Description |
|-------|-------------|
| `run:start` | A new run has started |
| `run:stop` | A run completed successfully |
| `run:fail` | A run failed |
| `run:cancel` | A run was cancelled |
| `event:created` | A new event was created |
| `event:handled` | An event was handled |
| `stream:created` | A new stream was created |

**Example Events:**

```
event: run:start
data: {"run_id":"550e8400-...","stream_id":"660e8400-...","run_type":"event"}

event: run:stop
data: {"run_id":"550e8400-...","stream_id":"660e8400-..."}

event: event:created
data: {"event_id":"770e8400-...","stream_id":"880e8400-...","event_type":"user:signup"}
```

**Examples:**

<!-- tabs:start -->
#### **curl**

```bash
curl -N 'https://api.hot.dev/v1/env/subscribe' \
  -H "Authorization: Bearer $HOT_API_KEY"
```

> Note: `-N` disables buffering for real-time streaming output.

#### **JavaScript**

```javascript
// Note: the browser-native EventSource API can't send an Authorization
// header, so use fetch with a stream reader instead.
const response = await fetch('https://api.hot.dev/v1/env/subscribe', {
  headers: {
    'Authorization': `Bearer ${HOT_API_KEY}`,
    'Accept': 'text/event-stream'
  }
});

const reader = response.body.getReader();
const decoder = new TextDecoder();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;

  const text = decoder.decode(value);
  for (const line of text.split('\n')) {
    if (line.startsWith('data: ')) {
      const data = JSON.parse(line.slice(6));
      console.log('SSE event:', data);
    }
  }
}
```

#### **Python**

```python
import requests
import json

response = requests.get(
    'https://api.hot.dev/v1/env/subscribe',
    headers={'Authorization': f'Bearer {HOT_API_KEY}'},
    stream=True
)

for line in response.iter_lines():
    if line:
        line = line.decode('utf-8')
        if line.startswith('data: '):
            event = json.loads(line[6:])
            print(f"Event: {event}")
```
<!-- tabs:end -->

**Notes:**
- The stream automatically times out after 5 minutes. Reconnect to continue receiving events.
- Events are scoped to the environment associated with your API key.
- This endpoint requires pub/sub to be configured on the server.

---

### Organization

#### Get Usage & Limits

```http
GET /v1/org/usage
```

Returns current usage statistics, plan limits, and usage percentages for the organization.

**Response:**

```json
{
  "data": {
    "org_id": "660e8400-e29b-41d4-a716-446655440000",
    "usage": {
      "runs_this_period": 1250,
      "file_storage_bytes": 52428800,
      "team_members": 5,
      "call_storage_bytes": 104857600,
      "call_count": 15000
    },
    "limits": {
      "runs_per_month": 10000,
      "storage_bytes": 1073741824,
      "team_members": 10,
      "call_retention_days": 30,
      "call_storage_bytes": 5368709120
    },
    "usage_percent": {
      "runs": 12.5,
      "file_storage": 4.9,
      "team_members": 50.0,
      "call_storage": 1.95,
      "has_warning": false
    },
    "plan": {
      "name": "Pro",
      "period_start": "2024-01-01T00:00:00Z",
      "period_end": "2024-02-01T00:00:00Z"
    }
  },
  "meta": {...}
}
```

**Fields:**

| Field | Description |
|-------|-------------|
| `usage` | Current usage in the billing period |
| `limits` | Plan limits (-1 = unlimited) |
| `usage_percent` | Usage as percentage of limits (can exceed 100 if over limit) |
| `usage_percent.has_warning` | True if any usage exceeds 90% |
| `plan.period_start` | Start of current billing period |
| `plan.period_end` | End of current billing period |

For self-hosted/local deployments without a subscription, all limits are unlimited (-1).

---

### Streams (Server-Sent Events)

Streams provide real-time updates for run execution via Server-Sent Events (SSE). A stream groups related events and runs together, allowing you to track the full lifecycle of an operation.

#### Subscribe to Stream

```http
GET /v1/streams/{stream_id}/subscribe
```

Subscribe to an existing stream to receive real-time updates.

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `project` | string | Optional project filter |

**Response:** `text/event-stream`

**SSE Event Types:**

| Event | Description |
|-------|-------------|
| `run:start` | A new run has started |
| `run:stop` | A run completed successfully |
| `run:fail` | A run failed |
| `run:cancel` | A run was cancelled |
| `stream:data` | Real-time data from the run (e.g., AI tokens) |
| `stream:complete` | Stream subscription ended — the stream completed or the subscription timed out (5 minute default) |

**Example Event:**

```
event: run:start
data: {"type":"run:start","run":{"run_id":"...","status":"running",...}}

event: stream:data
data: {"type":"stream:data","run_id":"...","data_type":"ai:delta","payload":{"text":"Hello"}}

event: run:stop
data: {"type":"run:stop","run":{"run_id":"...","status":"succeeded","result":"Hello world"}}
```

#### Subscribe with Event (Atomic)

```http
POST /v1/streams/subscribe-with-event
Accept: text/event-stream
```

**Recommended for streaming use cases.** This endpoint atomically subscribes to a stream AND publishes an event in a single request, eliminating race conditions where events might be missed.

**Request Body:**

```json
{
  "event_type": "chat:message",
  "event_data": {
    "message": "Hello, world!",
    "history": []
  },
  "stream_id": "optional-existing-stream-uuid"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `event_type` | Yes | The event type to publish |
| `event_data` | Yes | Event payload (any JSON) |
| `stream_id` | No | Continue an existing stream; if omitted, creates a new stream |

**Response:** `text/event-stream`

The first event is always `event:published` confirming the event was queued:

```
event: event:published
data: {"type":"event:published","event_id":"...","stream_id":"...","event_type":"chat:message"}

event: run:start
data: {"type":"run:start","run":{...}}

event: stream:data
data: {"type":"stream:data","run_id":"...","data_type":"ai:delta","payload":{"text":"Hello"}}

event: run:stop
data: {"type":"run:stop","run":{...}}
```

**Examples:**

<!-- tabs:start -->
#### **curl**

```bash
curl -N -X POST 'https://api.hot.dev/v1/streams/subscribe-with-event' \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"event_type": "chat:message", "event_data": {"message": "Hello!"}}'
```

> Note: `-N` disables buffering for real-time streaming output.

#### **JavaScript**

```javascript
const response = await fetch('https://api.hot.dev/v1/streams/subscribe-with-event', {
  method: 'POST',
  headers: {
    'Authorization': `Bearer ${HOT_API_KEY}`,
    'Content-Type': 'application/json',
    'Accept': 'text/event-stream',
  },
  body: JSON.stringify({
    event_type: 'chat:message',
    event_data: { message: 'Hello!', history: [] }
  })
});

const reader = response.body.getReader();
const decoder = new TextDecoder();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;

  const chunk = decoder.decode(value);
  // Parse SSE events from chunk
  for (const line of chunk.split('\n')) {
    if (line.startsWith('data: ')) {
      const event = JSON.parse(line.slice(6));
      console.log(event.type, event);
    }
  }
}
```

#### **Python**

```python
import requests
import json

response = requests.post(
    'https://api.hot.dev/v1/streams/subscribe-with-event',
    headers={
        'Authorization': f'Bearer {HOT_API_KEY}',
        'Content-Type': 'application/json',
        'Accept': 'text/event-stream',
    },
    json={
        'event_type': 'chat:message',
        'event_data': {'message': 'Hello!', 'history': []}
    },
    stream=True  # Required for SSE
)

for line in response.iter_lines():
    if line:
        line = line.decode('utf-8')
        if line.startswith('data: '):
            event = json.loads(line[6:])
            print(event['type'], event)
```
<!-- tabs:end -->

---

### Sessions

Sessions are short-lived, permission-scoped tokens for ephemeral access. Only API keys can create sessions.

#### Create Session

```http
POST /v1/sessions
```

**Request Body:**

```json
{
  "permissions": {
    "stream:*": ["read"],
    "event:user:*": ["create"]
  },
  "metadata": {
    "user_id": "end-user-123",
    "purpose": "stream-subscription"
  },
  "expires_in": 3600
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `permissions` | Yes | Permission map (resource URN → action array). Must be a subset of the parent API key's permissions. |
| `metadata` | No | Arbitrary JSON metadata (user ID, purpose, etc.) |
| `expires_in` | No | TTL in seconds (default: 3600, max: 86400) |

**Response:** `201 Created`

```json
{
  "data": {
    "session_id": "550e8400-e29b-41d4-a716-446655440000",
    "token": "s_0193a7b212347def8abc123456789012_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
    "permissions": {"stream:*": ["read"], "event:user:*": ["create"]},
    "metadata": {"user_id": "end-user-123"},
    "expires_at": "2026-01-15T11:30:00Z",
    "created_at": "2026-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

> **Important:** The `token` field is only returned at creation time. Store it securely — it cannot be retrieved later.

**Errors:**

| Code | Status | Cause |
|------|--------|-------|
| `forbidden` | 403 | Non-API-key credential attempted to create a session |
| `permission_escalation` | 403 | Requested permissions exceed parent API key permissions |
| `session_limit_exceeded` | 429 | Maximum active sessions (1000) reached for this API key |

#### List Sessions

```http
GET /v1/sessions
```

Lists active (non-expired, non-revoked) sessions for the authenticated API key.

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

#### Revoke Session

```http
DELETE /v1/sessions/{session_id}
```

Revokes a specific session. The session must belong to the authenticated API key.

**Response:** `204 No Content`

#### Revoke All Sessions

```http
DELETE /v1/sessions
```

Revokes all active sessions for the authenticated API key.

**Response:**

```json
{
  "data": {
    "revoked_count": 5
  },
  "meta": {...}
}
```

---

### Service Keys

Service keys are long-lived, permission-scoped credentials you issue to your customers or external systems for access to MCP tools, webhooks, and other API resources. Only API keys can create service keys.

#### Create Service Key

```http
POST /v1/service-keys
```

**Request Body:**

```json
{
  "name": "Acme Corp Production Key",
  "description": "MCP and stream access for Acme Corp",
  "permissions": {
    "mcp:weather/*": ["execute"],
    "stream:*": ["read"]
  },
  "metadata": {
    "customer_id": "acme-123"
  },
  "expires_in": null
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | No | Human-readable name |
| `description` | No | Description of the key's purpose |
| `permissions` | Yes | Permission map. Must be a subset of the parent API key's permissions. |
| `metadata` | No | Arbitrary JSON metadata (encrypted at rest, available at runtime via `req.auth.service-key.meta`) |
| `expires_in` | No | TTL in seconds (`null` or omitted = never expires) |

**Response:** `201 Created`

```json
{
  "data": {
    "service_key_id": "550e8400-e29b-41d4-a716-446655440000",
    "name": "Acme Corp Production Key",
    "description": "MCP and stream access for Acme Corp",
    "token": "0193a7b212347def8abc123456789012_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
    "permissions": {"mcp:weather/*": ["execute"], "stream:*": ["read"]},
    "metadata": {"customer_id": "acme-123"},
    "created_at": "2026-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

> **Important:** The `token` field is only returned at creation time. Store it securely — it cannot be retrieved later. Note that service key tokens have no `hot_` prefix, making them suitable for white-label use.

**Errors:**

| Code | Status | Cause |
|------|--------|-------|
| `forbidden` | 403 | Non-API-key credential attempted to create a service key |
| `permission_escalation` | 403 | Requested permissions exceed parent API key permissions |

#### List Service Keys

```http
GET /v1/service-keys
```

Lists service keys for the authenticated API key.

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | int | Max results (default: 20) |
| `offset` | int | Pagination offset |

#### Get Service Key

```http
GET /v1/service-keys/{service_key_id}
```

Returns details for a specific service key. The key must belong to the authenticated API key.

#### Revoke Service Key

```http
DELETE /v1/service-keys/{service_key_id}
```

Revokes a specific service key. The key must belong to the authenticated API key.

**Response:** `204 No Content`

#### Revoke All Service Keys

```http
DELETE /v1/service-keys
```

Revokes all active service keys for the authenticated API key.

**Response:**

```json
{
  "data": {
    "revoked_count": 3
  },
  "meta": {...}
}
```

---

### Custom Domains

Custom domains map your own domain names (e.g., `mcp.example.com`) to your Hot Dev environment. This feature requires a **Pro or Scale** subscription plan.

#### Register Domain

```http
POST /v1/domains
```

**Request Body:**

```json
{
  "domain": "mcp.example.com"
}
```

**Response:** `201 Created`

```json
{
  "data": {
    "domain_id": "550e8400-e29b-41d4-a716-446655440000",
    "env_id": "660e8400-e29b-41d4-a716-446655440000",
    "domain": "mcp.example.com",
    "status": "pending_validation",
    "validation_cname_name": "_abc123.mcp.example.com",
    "validation_cname_value": "_xyz789.acm-validations.aws",
    "routing_domain": null,
    "created_at": "2026-01-15T10:30:00Z"
  },
  "meta": {...}
}
```

After creating a domain, add the **validation CNAME** record (using the `validation_cname_name` and `validation_cname_value` fields) to prove domain ownership. Once validated, the `routing_domain` field will be populated — add a domain CNAME pointing to that routing target to start routing traffic.

Domain statuses: `pending_validation`, `validated`, `provisioning`, `active`, `deleting`.

**Errors:**

| Code | Status | Cause |
|------|--------|-------|
| `plan_required` | 403 | Custom domains require Pro or Scale plan |
| `domain_limit_reached` | 403 | Domain count limit reached for current plan |
| `domain_exists` | 409 | Domain is already registered |

#### List Domains

```http
GET /v1/domains
```

Lists all custom domains for the environment.

#### Get Domain

```http
GET /v1/domains/{domain_id}
```

#### Verify Domain

```http
POST /v1/domains/{domain_id}/verify
```

Checks the current provisioning status of the domain. If the validation CNAME has propagated and the certificate is issued, the domain moves to `validated` status and routing provisioning begins. If not yet validated, returns the required DNS records.

Pending domains are also checked automatically in the background, so you don't need to call this endpoint repeatedly.

**Response (validated):**

```json
{
  "data": {
    "domain_id": "550e8400-e29b-41d4-a716-446655440000",
    "domain": "mcp.example.com",
    "status": "validated",
    "message": "Domain validated successfully — routing provisioning in progress"
  },
  "meta": {...}
}
```

**Response (pending):**

```json
{
  "data": {
    "domain_id": "550e8400-e29b-41d4-a716-446655440000",
    "domain": "mcp.example.com",
    "status": "pending_validation",
    "message": "Add a CNAME record: _abc123.mcp.example.com → _xyz789.acm-validations.aws"
  },
  "meta": {...}
}
```

#### Delete Domain

```http
DELETE /v1/domains/{domain_id}
```

Removes a custom domain. The domain must belong to the authenticated environment. Deletion is asynchronous — the domain enters a `deleting` state while its routing target and TLS certificate are cleaned up, then the record is removed.

**Response:** `204 No Content`

---

## Error Codes

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `unauthorized` | 401 | Invalid, missing, expired, or revoked credential |
| `forbidden` | 403 | Credential lacks required permissions |
| `permission_escalation` | 403 | Requested permissions exceed parent credential permissions |
| `plan_required` | 403 | Feature requires a higher subscription plan |
| `not_found` | 404 | Resource not found |
| `bad_request` | 400 | Invalid request body or parameters |
| `domain_exists` | 409 | Custom domain is already registered |
| `domain_limit_reached` | 403 | Domain count limit reached for current plan |
| `session_limit_exceeded` | 429 | Maximum active sessions reached for this API key |
| `rate_limit_exceeded` | 429 | Too many requests (see `Retry-After` header) |
| `internal_server_error` | 500 | Server error |

---

## Code Examples

The [official SDKs](api/sdks) cover every endpoint on this page. Listing
projects and publishing an event in each:

<!-- tabs:start -->
#### **curl**

```bash
# List projects
curl https://api.hot.dev/v1/projects \
  -H "Authorization: Bearer $HOT_API_KEY"

# Publish an event
curl -X POST https://api.hot.dev/v1/events \
  -H "Authorization: Bearer $HOT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"event_type": "user:signup", "event_data": {"user_id": "123"}}'
```

#### **JavaScript**

```javascript
import { HotClient } from "@hot-dev/sdk";

const hot = new HotClient({ token: process.env.HOT_API_KEY });

// List projects
const { data: projects } = await hot.projects.list();

// Publish an event
const event = await hot.events.publish({
  event_type: "user:signup",
  event_data: { user_id: "123", email: "alice@example.com" },
});
console.log(event.stream_id);
```

#### **Python**

```python
import os
from hot import HotClient

hot = HotClient(token=os.environ["HOT_API_KEY"])

# List projects
projects = hot.projects.list()["data"]

# Publish an event
event = hot.events.publish(
    {
        "event_type": "user:signup",
        "event_data": {"user_id": "123", "email": "alice@example.com"},
    }
)
print(event["stream_id"])
```

#### **Go**

```go
client, err := hot.NewClient(hot.Config{Token: os.Getenv("HOT_API_KEY")})
if err != nil {
	log.Fatal(err)
}
ctx := context.Background()

// List projects
projects, err := client.Projects.List(ctx, nil)

// Publish an event
event, err := client.Events.Publish(ctx, map[string]any{
	"event_type": "user:signup",
	"event_data": map[string]any{"user_id": "123", "email": "alice@example.com"},
})
fmt.Println(event["stream_id"])
```

#### **Rust**

```rust
use hot_dev::HotClient;
use serde_json::json;

let client = HotClient::builder(std::env::var("HOT_API_KEY").unwrap()).build();

// List projects
let projects = client.projects().list(&[]).await?;

// Publish an event
let event = client
    .events()
    .publish(json!({
        "event_type": "user:signup",
        "event_data": { "user_id": "123", "email": "alice@example.com" },
    }))
    .await?;
println!("{}", event["stream_id"]);
```

#### **Java**

```java
HotClient client = HotClient.builder(System.getenv("HOT_API_KEY")).build();

// List projects
Map<String, Object> projects = client.projects().list();

// Publish an event
Map<String, Object> event = client.events().publish(Map.of(
    "event_type", "user:signup",
    "event_data", Map.of("user_id", "123", "email", "alice@example.com")));
System.out.println(event.get("stream_id"));
```
<!-- tabs:end -->

See [SDKs](api/sdks) for installation, streaming, error handling, and the
full behavior shared across all five libraries.
