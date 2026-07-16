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

Every SDK covers the full API v1 surface: the twelve resources ([Endpoints](api)),
SSE run-stream subscriptions with automatic reconnection across the API's
5-minute stream timeout, structured API errors, and escape hatches for
endpoints that do not yet have a helper.

Authenticated clients should run server-side. Browser apps and untrusted
clients should call your own backend route instead of exposing a Hot API key
(the JavaScript SDK ships a `@hot-dev/sdk/proxy` helper for this).

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
go get github.com/hot-dev/hot-go
```

Go 1.23+. Zero dependencies.

#### **Rust**

```bash
cargo add hot-dev tokio --features tokio/full
cargo add futures-util serde_json
```

Async on tokio; TLS via rustls. Import as `hot_dev`.

#### **Java**

```kotlin
// Gradle
implementation("dev.hot:hot-sdk:1.1.3")
```

```xml
<!-- Maven -->
<dependency>
  <groupId>dev.hot</groupId>
  <artifactId>hot-sdk</artifactId>
  <version>1.1.3</version>
</dependency>
```

Java 17+. Jackson is the only runtime dependency.
<!-- tabs:end -->

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

## Call a Hot Function

Every SDK wraps the publish-and-wait flow for `hot:call` events:

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
// result == float64(5)
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

## Shared Behavior

All five SDKs follow the same conventions:

- **Wire-format payloads.** Request and response payloads use the API wire
  format (`event_type`, `event_data`, `stream_id`). SDK-only options use each
  language's idiom (`baseUrl`, `base_url`, `BaseURL`). No SDK ever transforms
  user-owned payloads such as `event_data`.
- **Retries.** JSON requests retry automatically (at most twice) when the API
  responds 429 with a `retry_after`. Streaming and raw requests never retry.
- **Streaming reconnection.** `subscribeWithEvent` resubscribes across the
  API's 5-minute SSE timeout, dedupes replayed `run:start` and terminal
  events by `run_id`, and ends after the first terminal `run:*` event. Use
  the plain `subscribe` when your app expects multiple independent runs on
  one stream.
- **Identification.** Each SDK sends `User-Agent: hot-sdk-<lang>/<version>`.
- **Escape hatches.** A `request(...)` method (plus a raw-response variant)
  covers endpoints that do not yet have a resource helper.

Building chat or agent frontends? The JavaScript SDK additionally ships
agent, webhook, and BFF proxy helpers as subpath exports
(`@hot-dev/sdk/agent`, `/webhook`, `/proxy`); the other SDKs intentionally
cover the core API.
