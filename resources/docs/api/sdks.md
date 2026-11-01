---
description: "Official Hot SDKs for JavaScript, Python, Go, Rust, and Java: install, quick start, streaming, and error handling."
---

# SDKs

Official client libraries for the Hot API, released in lockstep versions:

| Language | Package | Source | API Reference |
|----------|---------|--------|---------------|
| JavaScript / TypeScript | [`@hot-dev/sdk`](https://www.npmjs.com/package/@hot-dev/sdk) (npm) | [hot-dev/hot-js](https://github.com/hot-dev/hot-js) | [Package README](https://github.com/hot-dev/hot-js/tree/main/packages/sdk) |
| Python | [`hot-dev`](https://pypi.org/project/hot-dev/) (PyPI) | [hot-dev/hot-python](https://github.com/hot-dev/hot-python) | [README](https://github.com/hot-dev/hot-python) |
| Go | `github.com/hot-dev/hot-go` | [hot-dev/hot-go](https://github.com/hot-dev/hot-go) | [pkg.go.dev](https://pkg.go.dev/github.com/hot-dev/hot-go) |
| Rust | [`hot-dev`](https://crates.io/crates/hot-dev) (crates.io) | [hot-dev/hot-rust](https://github.com/hot-dev/hot-rust) | [docs.rs](https://docs.rs/hot-dev) |
| Java | `dev.hot:hot-sdk` (Maven Central) | [hot-dev/hot-java](https://github.com/hot-dev/hot-java) | [javadoc.io](https://javadoc.io/doc/dev.hot/hot-sdk) |

Every SDK covers the thirteen runtime API v1 resources ([Endpoints](/docs/api#endpoints)),
plus the [org API](/docs/api/management) (`orgs`, `apiKeys`) for org API keys,
SSE run-stream subscriptions with automatic reconnection across the API's
5-minute stream timeout, durable run and task waiters, structured API errors,
and escape hatches for endpoints that do not yet have a helper.

Authenticated clients should run server-side. Browser apps and untrusted
clients should call your own backend route instead of exposing a Hot API key.
The JavaScript SDK ships a `@hot-dev/sdk/proxy` helper for this. The helper
publishes with your server's credential, so it requires an `allowEventTypes`
list and rejects `hot:` types such as `hot:call` unless they are listed
exactly. It also takes an `authorize` hook, and ignores a browser `stream_id`
unless `allowStreamId` accepts it. Give it a per-user session or a service key
scoped to the events and streams that user may use, never a full-access API
key. See the
[package README](https://github.com/hot-dev/hot-js/tree/main/packages/sdk#nextjs-proxy).

## Install

<!-- tabs:start -->
#### **JavaScript**

```bash
npm install @hot-dev/sdk
```

Requires Node 20+. ESM-only.

#### **Python**

```bash
pip install hot-dev
```

Python 3.10+. Import as `hot`.

#### **Go**

```bash
go get github.com/hot-dev/hot-go@v1.3.0
```

Go 1.23+. Zero dependencies.

#### **Rust**

```bash
cargo add hot-dev tokio --features tokio/full
cargo add futures-util serde_json
```

Async on tokio; TLS via rustls. Import as `hot_dev`.

To pass your own HTTP client, build it with the re-exported
`hot_dev::reqwest::Client::builder()`, so it matches the SDK's `reqwest`.
After upgrading to 1.3.0, run
`cargo update -p rustls -p rustls-webpki -p webpki-roots` to pick up the TLS
security floors.

#### **Java**

```text
// Gradle
implementation("dev.hot:hot-sdk:1.3.0")
```

```xml
<!-- Maven -->
<dependency>
  <groupId>dev.hot</groupId>
  <artifactId>hot-sdk</artifactId>
  <version>1.3.0</version>
</dependency>
```

Java 17+ (tested on 17, 21 and 25). Jackson is the only runtime dependency.
<!-- tabs:end -->

The org API needs Hot 3.0.1; runtime calls also work with Hot 3.0.0.

## Quick Start

Publish an event and stream its run to completion. `base_url` defaults to
`https://api.hot.dev`; for local development with `hot dev`, point it at
`http://localhost:4681`.

<!-- tabs:start -->
#### **JavaScript**

```javascript
import { HotClient } from "@hot-dev/sdk";

const hot = new HotClient({ token: process.env.HOT_API_KEY });

for await (const event of hot.streams.subscribeWithEvent({
  event_type: "team-agent:ask",
  event_data: { question: "what is blocking launch?" },
})) {
  if (event.type === "stream:data") console.log(event.data_type, event.payload);
  if (event.type === "run:stop") {
    console.log(event.run?.result);
    break;
  }
}
```

#### **Python**

```python
import os
from hot import HotClient

hot = HotClient(token=os.environ["HOT_API_KEY"])

for event in hot.streams.subscribe_with_event(
    {"event_type": "team-agent:ask", "event_data": {"question": "what is blocking launch?"}}
):
    if event["type"] == "stream:data":
        print(event["data_type"], event.get("payload"))
    if event["type"] == "run:stop":
        print(event.get("run", {}).get("result"))
        break
```

#### **Go**

```go
client, err := hot.NewClient(hot.Config{Token: os.Getenv("HOT_API_KEY")})
if err != nil {
	log.Fatal(err)
}

ctx := context.Background()
for event, err := range client.Streams.SubscribeWithEvent(ctx, map[string]any{
	"event_type": "team-agent:ask",
	"event_data": map[string]any{"question": "what is blocking launch?"},
}, nil) {
	if err != nil {
		log.Fatal(err)
	}
	if event.Type() == "stream:data" {
		fmt.Println(event["data_type"], event["payload"])
	}
	if event.Type() == "run:stop" {
		fmt.Println(event.Run()["result"])
		break
	}
}
```

#### **Rust**

```rust
use futures_util::StreamExt;
use hot_dev::{HotClient, StreamEventExt, SubscribeWithEventOptions};
use serde_json::json;

let client = HotClient::builder(std::env::var("HOT_API_KEY").unwrap()).build();

let mut events = client.streams().subscribe_with_event(
    json!({
        "event_type": "team-agent:ask",
        "event_data": { "question": "what is blocking launch?" },
    }),
    SubscribeWithEventOptions::default(),
);
while let Some(event) = events.next().await {
    let event = event?;
    if event.event_type() == "stream:data" {
        println!("{:?} {:?}", event.get("data_type"), event.get("payload"));
    }
    if event.event_type() == "run:stop" {
        println!("{:?}", event.run().and_then(|run| run.get("result")));
        break;
    }
}
```

#### **Java**

```java
HotClient client = HotClient.builder(System.getenv("HOT_API_KEY")).build();

try (StreamEvents events = client.streams().subscribeWithEvent(Map.of(
    "event_type", "team-agent:ask",
    "event_data", Map.of("question", "what is blocking launch?")))) {
  while (events.hasNext()) {
    Map<String, Object> event = events.next();
    if (StreamEvents.typeOf(event).equals("stream:data")) {
      System.out.println(event.get("data_type") + " " + event.get("payload"));
    }
    if (StreamEvents.typeOf(event).equals("run:stop")) {
      System.out.println(StreamEvents.runOf(event).get("result"));
      break;
    }
  }
}
```
<!-- tabs:end -->

## Manage Sub-organizations

Every SDK exposes the [org API](/docs/api/management) on its normal client:
`orgs`, `apiKeys`, and `orgs.env(org, env)` for projects, context and builds
in a sub-organization's environment. Call it with an org API key. Calling a runtime route
with an org key, or an org route with an environment key, returns
`403 key_scope_mismatch`; each SDK raises a typed error that names the key the
call needs. Slugs must be 32 characters or fewer.

<!-- tabs:start -->
#### **JavaScript**

```typescript
import { HotClient } from "@hot-dev/sdk";

const org = new HotClient({ token: process.env.HOT_ORG_KEY });
const subOrg = await org.orgs.create({
  name: "Customer", slug: "customer-123", idempotency_key: "customer-123-v1",
});
const key = await org.orgs.createEnvApiKey(subOrg.org.org_id, subOrg.env_id, {
  description: "Customer backend", permissions: { "event:support:*": ["create"] },
});
await org.orgs.env(subOrg.org.org_id, subOrg.env_id).projects.list();
const customer = new HotClient({ token: key.token });
```

#### **Python**

```python
import os

from hot import HotClient

with HotClient(token=os.environ["HOT_ORG_KEY"]) as org:
    sub_org = org.orgs.create(
        {"name": "Customer", "slug": "customer-123", "idempotency_key": "customer-123-v1"}
    )
    org_id, env_id = sub_org["org"]["org_id"], sub_org["env_id"]
    key = org.orgs.create_env_api_key(
        org_id,
        env_id,
        {"description": "Customer backend", "permissions": {"event:support:*": ["create"]}},
    )
    org.orgs.env(org_id, env_id).projects.list()

customer = HotClient(token=key["token"])
```

#### **Go**

```go
orgs, err := hot.NewClient(hot.Config{Token: os.Getenv("HOT_ORG_KEY")})
if err != nil {
	log.Fatal(err)
}
subOrg, err := orgs.Orgs.Create(ctx, map[string]any{
	"name": "Customer", "slug": "customer-123", "idempotency_key": "customer-123-v1",
})
if err != nil {
	log.Fatal(err)
}
orgID := subOrg["org"].(map[string]any)["org_id"].(string)
envID := subOrg["env_id"].(string)
key, err := orgs.Orgs.CreateEnvAPIKey(ctx, orgID, envID, map[string]any{
	"description": "Customer backend",
	"permissions": map[string]any{"event:support:*": []string{"create"}},
})
if err != nil {
	log.Fatal(err)
}
if _, err := orgs.Orgs.Env(orgID, envID).Projects.List(ctx, nil); err != nil {
	log.Fatal(err)
}
// The minted key makes the customer's runtime calls.
customer, err := hot.NewClient(hot.Config{Token: key["token"].(string)})
if err != nil {
	log.Fatal(err)
}
if _, err := customer.Events.Publish(ctx, map[string]any{"event_type": "support:opened"}); err != nil {
	log.Fatal(err)
}
```

#### **Rust**

```rust
let org = HotClient::new(std::env::var("HOT_ORG_KEY")?);
let sub_org = org.orgs().create(json!({
    "name": "Customer", "slug": "customer-123", "idempotency_key": "customer-123-v1",
})).await?;
let (org_id, env_id) = (sub_org["org"]["org_id"].as_str().unwrap(), sub_org["env_id"].as_str().unwrap());
let key = org.orgs().create_env_api_key(org_id, env_id, json!({
    "description": "Customer backend",
    "permissions": {"event:support:*": ["create"]},
})).await?;
let projects = org.orgs().env(org_id, env_id).projects().list(&[]).await?;
let customer = HotClient::new(key["token"].as_str().unwrap());
```

#### **Java**

```java
HotClient org = HotClient.builder(System.getenv("HOT_ORG_KEY")).build();
Map<String, Object> subOrg = org.orgs().create(Map.of(
    "name", "Customer", "slug", "customer-123", "idempotency_key", "customer-123-v1"));
String orgId = (String) ((Map<?, ?>) subOrg.get("org")).get("org_id");
Map<String, Object> key = org.orgs().createEnvApiKey(orgId, (String) subOrg.get("env_id"), Map.of(
    "description", "Customer backend",
    "permissions", Map.of("event:support:*", List.of("create"))));
HotClient customer = HotClient.builder((String) key.get("token")).build();
```
<!-- tabs:end -->

Each SDK also has `apiKeys.get`, `update`, `revoke` and `rotate` (`rotate`
takes `overlap_seconds`), `orgs.usage(parentOrgId)`, and `purgeCalls` on env,
runs, streams and orgs. Each SDK's README documents these in its own idiom.

Over plain HTTP, the same calls look like this:

```bash
sub_org=$(curl -s https://api.hot.dev/v1/orgs \
  -H "Authorization: Bearer $HOT_ORG_KEY" -H "Content-Type: application/json" \
  -d '{"name":"Customer","slug":"customer-123","idempotency_key":"customer-123-v1"}')
org_id=$(echo "$sub_org" | jq -r .data.org.org_id)
env_id=$(echo "$sub_org" | jq -r .data.env_id)
curl -s "https://api.hot.dev/v1/orgs/$org_id/envs/$env_id/api-keys" \
  -H "Authorization: Bearer $HOT_ORG_KEY" -H "Content-Type: application/json" \
  -d '{"description":"Customer backend","permissions":{"event:support:*":["create"]}}'
```

## Call a Hot Function

Every SDK wraps the publish-and-wait flow for `hot:call` events. The helper
publishes through the atomic subscribe-with-event endpoint, correlates the
terminal run to the published event, and reconnects without publishing twice:

<!-- tabs:start -->
#### **JavaScript**

```javascript
const result = await hot.events.callHot("::myapp::math/add-nums", [2, 3]);
// result === 5
```

#### **Python**

```python
result = hot.events.call_hot("::myapp::math/add-nums", [2, 3])
# result == 5
```

#### **Go**

```go
result, err := client.Events.CallHot(ctx, "::myapp::math/add-nums", []any{2, 3}, nil)
if err != nil {
	log.Fatal(err)
}
fmt.Println(result) // 5 (a float64)
```

#### **Rust**

```rust
let result = client
    .events()
    .call_hot("::myapp::math/add-nums", vec![json!(2), json!(3)], CallOptions::default())
    .await?;
// result == json!(5)
```

#### **Java**

```java
Object result = client.events().callHot("::myapp::math/add-nums", List.of(2, 3));
// result equals 5
```
<!-- tabs:end -->

## Wait for a Run

If another API response or stored record gives your client a run ID, wait on
that run directly. The run subscription sends the latest persisted snapshot
first, reconnects after transport interruptions, and closes after a terminal
state. A failed or cancelled run raises a structured run error containing the
terminal run record.

| Language | Wait method |
|----------|-------------|
| JavaScript / TypeScript | `await hot.runs.wait(runId)` |
| Python | `hot.runs.wait(run_id)` or `await async_hot.runs.wait(run_id)` |
| Go | `client.Runs.Wait(ctx, runID, nil)` |
| Rust | `client.runs().wait(run_id, RunWaitOptions::default()).await` |
| Java | `client.runs().waitFor(runId)` |

`timeout` bounds the whole wait, including reconnects and backoff, even while
the server sends keepalives.

Use the stream subscription when you need live `stream:data` or several
related runs. Use the run waiter when one run's durable terminal record is the
only result you need.

## Wait for a Background Task

When a Hot run starts a task for a client, return the task id immediately and
let the SDK wait for its durable result. The task subscription sends the latest
persisted snapshot first, including a terminal snapshot when the task completed
before the client subscribed. Waiters reconnect automatically and return the
completed task record. Failed, cancelled, and timed-out tasks raise a structured
task error containing that record. `timeout` bounds the whole wait, including
reconnects and backoff, even while the server sends keepalives.

<!-- tabs:start -->
#### **JavaScript**

```javascript
const task = await hot.tasks.wait(taskId, { timeoutMs: 300_000 });
console.log(task.result);
```

#### **Python**

```python
task = hot.tasks.wait(task_id, timeout=300)
print(task["result"])

# AsyncHotClient exposes the same resource:
task = await async_hot.tasks.wait(task_id, timeout=300)
```

#### **Go**

```go
task, err := client.Tasks.Wait(ctx, taskID, &hot.WaitForTaskOptions{
	Timeout: 5 * time.Minute,
})
if err != nil {
	log.Fatal(err)
}
fmt.Println(task["result"])
```

#### **Rust**

```rust
let task = client
    .tasks()
    .wait(task_id, TaskWaitOptions {
        timeout: Duration::from_secs(300),
        ..TaskWaitOptions::default()
    })
    .await?;
println!("{:?}", task.get("result"));
```

#### **Java**

```java
Map<String, Object> task = client.tasks().waitFor(
    taskId,
    new TaskWaitOptions().timeout(Duration.ofMinutes(5)));
System.out.println(task.get("result"));
```
<!-- tabs:end -->

Use `::hot::task/await` inside Hot only when later Hot code in that same
execution depends on the task result. Keeping a run alive just so a client can
wait defeats the task's asynchronous lifecycle.

### Coordinate several tasks on one stream

The stream that contains the originating run also emits durable `task:update`
snapshots for every task on that stream. This keeps the event-handler result
fully user-defined: it can return one task ID, two task IDs, or a nested domain
object. The client interprets that result, then waits on each task:

```typescript
let taskIds: string[] = [];

for await (const event of hot.streams.subscribeWithEvent({
  event_type: "report:requested",
  event_data: { report_id: "report_123" },
})) {
  if (event.type === "run:stop") {
    // `task_ids` is application-defined, not imposed by Hot.
    const result = event.run?.result as { task_ids?: string[] } | undefined;
    taskIds = result?.task_ids ?? [];
    break;
  }
}

// Wait on each task. Every waiter reconnects across the 5-minute stream
// limit and cannot miss a task that finished before it subscribed.
const tasks = await Promise.all(taskIds.map((id) => hot.tasks.wait(id, { timeoutMs: 1_800_000 })));
```

A plain `streams.subscribe` is a single connection. The server ends it after
5 minutes with `stream:complete`, whether or not the tasks are done, so use it
to watch `task:update` snapshots, not to wait for them. In the other SDKs, wait
on each task the same way: with a thread pool or `asyncio.gather` in Python,
goroutines in Go, `join_all` in Rust, and `CompletableFuture` in Java.

## Errors

Non-2xx responses surface as structured errors with `status_code`, `code`,
`request_id`, and `retry_after`:

<!-- tabs:start -->
#### **JavaScript**

```javascript
import { HotApiError } from "@hot-dev/sdk";

try {
  await hot.projects.get("missing-project");
} catch (error) {
  if (error instanceof HotApiError) {
    console.log(error.status, error.code, error.requestId, error.retryAfter);
  }
}
```

#### **Python**

```python
from hot import HotApiError

try:
    hot.projects.get("missing-project")
except HotApiError as error:
    print(error.status_code, error.code, error.request_id, error.retry_after)
```

#### **Go**

```go
_, err := client.Projects.Get(ctx, "missing-project")
var apiErr *hot.APIError
if errors.As(err, &apiErr) {
	fmt.Println(apiErr.StatusCode, apiErr.Code, apiErr.RequestID, apiErr.RetryAfter)
}
```

#### **Rust**

```rust
match client.projects().get("missing-project").await {
    Err(hot_dev::Error::Api(error)) => {
        println!("{} {:?} {:?} {:?}", error.status_code, error.code, error.request_id, error.retry_after);
    }
    other => drop(other),
}
```

#### **Java**

```java
try {
  client.projects().get("missing-project");
} catch (HotApiException error) {
  System.out.println(error.statusCode() + " " + error.code() + " "
      + error.requestId() + " " + error.retryAfter());
}
```
<!-- tabs:end -->

A `403 key_scope_mismatch` (an org key on a runtime route, or an environment
key on an org route) raises a typed error that says which key the call needs:

- **JavaScript:** `HotKeyScopeError` (a `HotApiError`); `error.requiredScope`
  is `"org"` or `"env"`.
- **Python:** `except HotKeyScopeError as error:` then
  `error.required_scope`, which is `"org"` or `"env"`.
- **Go:** `var scopeErr *hot.KeyScopeError; if errors.As(err, &scopeErr) { scopeErr.RequiredScope }`.
- **Rust:** `if let Some(scope) = error.required_scope() { /* KeyScope::Org or KeyScope::Env */ }`.
- **Java:** `HotKeyScopeException` (a `HotApiException`); `requiredScope()`.

In Java, decimals decode as `Double` unless the client is built with
`.useBigDecimalForFloats(true)`, which keeps Hot `Dec` values and `event_data`
decimals exact.

## Shared Behavior

All five SDKs follow the same conventions:

- **Wire-format payloads.** Request and response payloads use the API wire
  format (`event_type`, `event_data`, `stream_id`). SDK-only options use each
  language's idiom (`baseUrl`, `base_url`, `BaseURL`). No SDK ever transforms
  user-owned payloads such as `event_data`.
- **Retries.** JSON requests retry automatically (at most twice) on 429 with a
  `retry_after`. Idempotent requests (GET, PUT, DELETE) also retry on 502, 503
  and 504. Raw requests, uploads and downloads are not retried. JSON requests
  time out after 60 seconds by default in JavaScript, Python, Rust and Java;
  Go uses context deadlines.
- **Reconnects.** Durable waiters (`runs.wait`, `tasks.wait`,
  `subscribeWithEvent`, `callHot`, `waitForRunResult`) stop at once on
  permanent errors, meaning any 4xx except 408 and 429 (including
  `key_scope_mismatch`). They reconnect after transport errors, 408, 429 and
  5xx with jittered exponential backoff that honours `retry_after` and never
  sleeps past the caller's timeout. The attempt limit counts consecutive failed
  connections only. The API closes every subscription 5 minutes after it opens
  (with `stream:complete`, not an idle timeout), and those rollovers reconnect
  at once without counting, so a run of any length can be awaited.
- **Streaming reconnection.** `subscribeWithEvent` resubscribes across the
  API's 5-minute SSE timeout, dedupes replayed `run:start` and terminal
  events by `run_id`, and ends after the terminal run correlated to the event
  it published. Unrelated runs on the same stream do not end the iterator. Use
  the plain `subscribe` when your app expects multiple independent runs on one
  stream. A plain stream subscription also carries durable `task:update`
  snapshots for tasks associated with that stream.
- **Retried handlers.** A `run:fail` with status `pending_retry` is not final.
  The run-result helpers follow the retry run, which gets a new `event_id` and
  links back through `origin_run_id`.
- **Chunks.** `onChunk` / `on_chunk` receives `stream:data` in order,
  including chunks that arrive before `run:start`.
- **Durable run waiting.** `runs.wait` (or the language-equivalent method)
  reads `run:update` snapshots and reconnects without missing a run that became
  terminal before subscription setup.
- **Durable task waiting.** `tasks.wait` (or the language-equivalent method)
  reads `task:update` snapshots, reconnects across transport interruptions, and
  cannot miss a task that became terminal before subscription setup.
- **Identification.** Each SDK sends `User-Agent: hot-sdk-<lang>/<version>`.
- **Escape hatches.** A `request(...)` method, plus a raw-response variant,
  covers endpoints that do not yet have a resource helper. In JavaScript these
  are `hot.request` and `hot.requestRaw`, with paths relative to `/v1`.
- **Large downloads.** In Python, `hot.files.download_stream(file_id)` and
  `hot.builds.download_stream(project, build_id)` stream large bodies;
  `download()` reads the whole body.

Building chat or agent frontends? The JavaScript SDK additionally ships
agent, webhook, and BFF proxy helpers as subpath exports
(`@hot-dev/sdk/agent`, `/webhook`, `/proxy`); the other SDKs intentionally
cover the core API.
