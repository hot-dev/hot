# Hot SDK Clients

## Contents

- [Resolve the installed SDK](#resolve-the-installed-sdk)
- [Shared API contract](#shared-api-contract)
- [Choose a stream lifecycle](#choose-a-stream-lifecycle)
- [JavaScript and TypeScript](#javascript-and-typescript)
- [Python](#python)
- [Go](#go)
- [Rust](#rust)
- [Java](#java)
- [Client review checklist](#client-review-checklist)

## Resolve the Installed SDK

The SDKs release independently. Inspect the application's dependency manifest,
lockfile, and installed package instead of assuming SDK source repositories are
present:

| Language | Package | Verify the selected version |
| --- | --- | --- |
| JavaScript/TypeScript | `@hot-dev/sdk` | `npm ls @hot-dev/sdk` or `pnpm why @hot-dev/sdk` |
| Python | distribution `hot-dev`, import `hot` | `python -m pip show hot-dev` |
| Go | `github.com/hot-dev/hot-go` | `go list -m github.com/hot-dev/hot-go` |
| Rust | crate `hot-dev`, import `hot_dev` | `cargo tree -i hot-dev` |
| Java | `dev.hot:hot-sdk` | Gradle `dependencyInsight` or Maven `dependency:tree` |

Use the methods and types shipped by that selected version. When adding or
upgrading an SDK, choose and pin a version through the language package manager
and verify the resulting lockfile. Do not require a clone of an SDK repository.

## Shared API Contract

Every SDK mirrors the Hot API v1 resources:

- events, streams, and runs;
- files, projects, and builds;
- context, domains, sessions, and service keys;
- organization and environment.

Authenticated clients belong on trusted servers. Do not expose a Hot API key
or provider key in a browser bundle.

Core request and response payloads preserve Hot wire keys:

```json
{
  "event_type": "support:ask",
  "event_data": {
    "session_id": "web:chat:demo",
    "message_id": "m1",
    "question": "Where is my order?"
  },
  "stream_id": "optional-client-supplied-uuid"
}
```

The SDK never rewrites keys inside `event_data`. SDK-only configuration uses
the target language's normal casing.

Recognize these stream events:

- `event:published`: contains the accepted event and stream ids;
- `stream:data`: application payload under `data_type` and `payload`;
- `run:stop`, `run:fail`, or `run:cancel`: inspect `run.result` or the SDK's
  structured run failure;
- `stream:complete`: the current SSE connection ended.

## Choose a Stream Lifecycle

For one request and one matching run, use `subscribe-with-event`. It atomically
opens the stream and publishes, reconnects across the server's connection cap
in current SDKs, and stops after the first terminal run event.

For a multi-turn browser session:

1. allocate and retain one UUID stream id;
2. open exactly one subscription with the first atomic
   subscribe-with-event call;
3. publish later events into that stream without opening another subscriber;
4. reattach to the same stream after the connection closes;
5. refresh canonical state because reattachment tails new events and does not
   replay missed data.

The high-level `subscribeWithEvent` / `subscribe_with_event` helpers are
one-run iterators: they stop after the first terminal run event. A persistent
multi-turn UI should consume the raw BFF subscribe-with-event response for its
first connection, publish later turns separately, and reattach through the
existing-stream subscribe endpoint.

Two simultaneous subscribers can cause the same reply to render twice.
Closing the subscription after one `reply:end` can miss later workflow output,
including approval-resume results.

## JavaScript and TypeScript

Install `@hot-dev/sdk`. Use subpath exports:

| Import | Purpose |
| --- | --- |
| `@hot-dev/sdk` | `HotClient`, resources, core types |
| `@hot-dev/sdk/streaming` | SSE parsing and stream event types |
| `@hot-dev/sdk/agent` | event payloads, slash commands, reply folding |
| `@hot-dev/sdk/proxy` | backend-for-frontend route helpers |

Keep the token in a server route:

```ts
import { HotClient } from "@hot-dev/sdk";
import { createHotProxyRoute } from "@hot-dev/sdk/proxy";

const hot = new HotClient({
  baseUrl: process.env.HOT_API_URL ?? "http://localhost:4681",
  token: process.env.HOT_API_KEY ?? "",
});

export const POST = createHotProxyRoute(hot);
```

Consume the proxied SSE response in browser code:

```ts
import {
  consumeSseResponse,
  type StreamEvent,
} from "@hot-dev/sdk/streaming";

export function ask(
  eventType: string,
  eventData: Record<string, unknown>,
  streamId: string,
  signal?: AbortSignal,
): AsyncGenerator<StreamEvent> {
  return consumeSseResponse(
    () => fetch("/api/hot", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "text/event-stream",
      },
      body: JSON.stringify({ eventType, eventData, streamId }),
      signal,
    }),
    { signal },
  );
}
```

For later turns on an already subscribed stream, publish from a server route:

```ts
await hot.events.publish({
  event_type: eventType,
  event_data: eventData,
  stream_id: streamId,
});
```

Use `buildWebMessageIds`, `buildAgentEventData`, and `parseSlashCommand` from
`@hot-dev/sdk/agent` for chat-style clients. Keep command maps and session-id
policy in the application.

## Python

Install `hot-dev`; import package `hot`:

```python
import os
from hot import HotClient

with HotClient(token=os.environ["HOT_API_KEY"]) as hot:
    for event in hot.streams.subscribe_with_event({
        "event_type": "support:ask",
        "event_data": {
            "session_id": "service:chat:demo",
            "question": "Where is my order?",
        },
    }):
        if event["type"] == "stream:data":
            print(event["data_type"], event.get("payload"))
        if event["type"] in {"run:stop", "run:fail", "run:cancel"}:
            break
```

Use `AsyncHotClient` and `async for` for asynchronous services. Use the client
as a context manager so its `httpx` connection closes.

## Go

Import `github.com/hot-dev/hot-go` as package `hot`:

```go
client, err := hot.NewClient(hot.Config{
    Token: os.Getenv("HOT_API_KEY"),
})
if err != nil {
    return err
}

for event, err := range client.Streams.SubscribeWithEvent(ctx, map[string]any{
    "event_type": "support:ask",
    "event_data": map[string]any{
        "session_id": "service:chat:demo",
        "question": "Where is my order?",
    },
}, nil) {
    if err != nil {
        return err
    }
    if event.Type() == "run:stop" ||
        event.Type() == "run:fail" ||
        event.Type() == "run:cancel" {
        break
    }
}
```

Stream methods return Go 1.23 `iter.Seq2` iterators. Cancel the context to stop
the connection. Do not set `http.Client.Timeout` on an SSE client.

## Rust

Use crate `hot-dev` and import `hot_dev`:

```rust
use futures_util::StreamExt;
use hot_dev::{HotClient, SubscribeWithEventOptions};
use serde_json::json;

let client = HotClient::builder(std::env::var("HOT_API_KEY")?).build();
let mut events = client.streams().subscribe_with_event(
    json!({
        "event_type": "support:ask",
        "event_data": {
            "session_id": "service:chat:demo",
            "question": "Where is my order?"
        }
    }),
    SubscribeWithEventOptions::default(),
);

while let Some(event) = events.next().await {
    let event = event?;
    if matches!(
        event.get("type").and_then(|v| v.as_str()),
        Some("run:stop" | "run:fail" | "run:cancel")
    ) {
        break;
    }
}
```

The SDK is async on Tokio. Use the builder's JSON request timeout rather than
a client-wide timeout that would sever SSE.

## Java

Use Maven coordinate `dev.hot:hot-sdk` and package `dev.hot.sdk`:

```java
HotClient client = HotClient.builder(System.getenv("HOT_API_KEY")).build();

try (StreamEvents events = client.streams().subscribeWithEvent(Map.of(
    "event_type", "support:ask",
    "event_data", Map.of(
        "session_id", "service:chat:demo",
        "question", "Where is my order?")))) {
  while (events.hasNext()) {
    Map<String, Object> event = events.next();
    String type = StreamEvents.typeOf(event);
    if (type.equals("run:stop") ||
        type.equals("run:fail") ||
        type.equals("run:cancel")) {
      break;
    }
  }
}
```

`HotClient` is canonical. `AsyncHotClient` is a `CompletableFuture` wrapper.
Close `StreamEvents` and avoid whole-request timeouts on SSE.

## Client Review Checklist

- Keep credentials server-side.
- Verify the installed SDK version and use its exact method names.
- Preserve wire-format payload keys.
- Use an atomic first publish and subscription.
- Maintain one subscriber per multi-turn stream.
- Correlate messages with stable session and message ids.
- Handle every terminal run event and structured run failure.
- Handle the server's SSE connection cap and cancellation.
- Do not assume reconnect replays data.
- Render structured state events independently from reply text.
- Reconcile state from durable storage after reconnect.
- Test missing token, non-2xx API errors, abort, duplicate delivery, and
  connection closure.
