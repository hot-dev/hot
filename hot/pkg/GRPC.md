# gRPC for Hot — design note (decided 2026-07, build deferred)

Status: **not building on spec**. Nothing in the current catalog is
blocked on gRPC; the strongest consumer — "my agents need to call my
company's internal gRPC services" — is a demand signal we will hear
directly when it arrives. This note pins the design so the thinking
survives until then, and so no step has to be re-litigated.

## The layered plan (three steps, each independently useful)

### 1. `hot.dev/protobuf` — pure Hot, no runtime release needed

Protobuf wire encode/decode over Maps, driven by descriptors: varints,
tags, length-delimited fields, nested messages — `::hot::bytes`
territory, simpler than the MySQL binary row protocol in `hot.dev/mysql`
(the reference for style and test discipline: independently generated
byte vectors, hermetic).

Build this the first time OTel export (OTLP/HTTP+protobuf), a GCP data
plane (Spanner/Bigtable/Firestore/Pub/Sub), or any binary-protocol
package justifies it. **The protobuf layer has more consumers than the
gRPC layer** — it must not be trapped inside a grpc package.

### 2. Runtime natives (rides the next release after the demand signal)

- `::hot::http2` — transport only: open a connection, open streams,
  send headers/data, read data **and trailers** (the piece `::hot::http`
  cannot express; gRPC status arrives in trailers). The `h2` crate is
  already in the default dependency graph. Value beyond gRPC:
  multiplexed fan-out for API-heavy agents.
- `::hot::protobuf/compile-proto` — `protox`-backed (pure-Rust protoc):
  `.proto` source in, descriptor-set bytes out. Parsing proto3 grammar
  in pure Hot is a project in itself; this native is small and keeps
  the no-external-protoc story.

Both follow the three-edit call-lib pattern; connection/stream handles
follow the `::hot::tcp` Val::Box ownership model.

### 3. `hot.dev/grpc` — thin pure-Hot composition

gRPC framing is a 5-byte prefix (compressed flag + u32 length) over
HTTP/2 DATA frames — trivial once 1 and 2 exist.

```hot
client ::grpc/client({
  url: "https://greeter.internal:443",
  proto: ::hot::file/read-file("protos/greeter.proto")
})
reply ::grpc/call(client, "helloworld.Greeter/SayHello", {name: "Ada"})
reply.message
```

Decided principles:
- **No codegen, ever** — dynamic descriptors at runtime (the OpenAPI
  generator purge applies here too). Messages are Maps in, Maps out;
  descriptors map onto Val the way JSON does.
- **`.proto` files are resources** — shipped in the build via the
  existing `resources.paths` mechanism, read at startup, parsed once,
  descriptors cached. No new machinery.
- Metadata (auth) is a headers Map; explicit-credential arity per the
  catalog convention; TLS through the existing rustls stack.
- Errors: `grpc-status`/`grpc-message` trailers → Err carrying
  `{code, status, message}` (the `SqlError` shape discipline).
- Scope order: unary first (80% of the value); server-streaming as a
  readable handle next; client/bidi streaming last and only with a
  concrete consumer.

## Demand inventory (why deferred)

| Consumer | Reality |
|---|---|
| Users' internal microservices | The genuine market; an enterprise-adoption gate. Arrives as a loud, direct signal. |
| Temporal | gRPC-first, philosophically adjacent to Hot agents — most interesting single package unlock. |
| etcd, Milvus | gRPC-only/-first infra; real but niche. |
| qdrant gRPC mode | Faster transport for an existing package — an upgrade, not an unlock. |
| GCP data planes | gRPC is the blessed path; only matters if google-* expands beyond Workspace. |
| OTel export | Needs the protobuf layer, not gRPC (OTLP/HTTP exists). |
| Rest of the catalog | REST won; nothing else touches gRPC. |

## Build triggers

Start step 1 when a protobuf consumer lands on the roadmap. Start
steps 2–3 when a user asks to call internal gRPC services, or when
Temporal/etcd/GCP-data packages are decided. Each step is at most a
mysql-driver-sized effort; the whole bet never has to be placed at once.
