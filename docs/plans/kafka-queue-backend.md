# Kafka queue backend plan

Status: exploration. Written on 2026-10-30. Nothing is implemented.

Add Apache Kafka as a fourth queue backend beside Memory, SQLite, and Redis
Streams ([`crates/hot/src/queue`](../../crates/hot/src/queue)). Kafka is for the
`scalable` preset of the [OpenTofu self-hosting plan](opentofu-cloud-self-hosting.md)
and for `hot-cloud`. It never replaces SQLite on a single machine or in local
development.

The broker is not the hard part. Hot's queue contract is per message: a lease,
an ack, idle-based reclaim, a delivery count, and a dead-letter queue. Kafka
matches that only through share groups (KIP-932, "Queues for Kafka"), and the
Rust client and most managed services do not support them yet.

## Why Kafka

Settle this first. Each reason leads to a different plan.

| Reason | What it implies |
| --- | --- |
| Durability on failover | ElastiCache replicates asynchronously, so a failover can lose queue writes it acknowledged. The self-hosting plan already notes this. MemoryDB for Valkey on AWS fixes it with no code change. GCP has no equivalent: Memorystore for Valkey's AOF persistence survives a node restart, but replication is still asynchronous. Sending `WAIT` after each queue write narrows the gap without closing it. Kafka with `acks=all` fixes it on both clouds, at much higher cost. |
| Throughput per queue | One Redis stream cannot spread its writes across shards. Kafka partitions can. |
| Customers who already run Kafka | An "existing Kafka" data mode, like existing Valkey. This is the strongest case for the work. |
| Kafka as an event source for user code | A different feature: a hot-std package or an event trigger, not a queue backend. |

## The current contract

Every backend implements the same behavior through
[`ProcessingQueue`](../../crates/hot/src/queue/mod.rs). Redis Streams is the
production implementation ([`streams.rs`](../../crates/hot/src/queue/streams.rs)).

| Hot today (Redis Streams) | Share groups (Kafka 4.2) | Classic consumer groups |
| --- | --- | --- |
| Lease per message, `XACK` | Lock per record, `ACCEPT` | Commit an offset per partition, so a per-record ack layer is needed |
| Idle reclaim with `XAUTOCLAIM` (event 60 s, task 120 s) | The lock times out and the record is redelivered | Only on rebalance; everything after the committed offset replays |
| Delivery count, then DLQ after 3 tries (`MAX_PROCESSING_RETRIES`) | Delivery count limit built in | Retry topics and a DLQ topic we build |
| Infra requeue with backoff (`infra-retry-backoff-ms`) | `RELEASE`, with no delay | Retry topics with a "not before" header |
| Long tasks, with a heartbeat on the task lease | `RENEW` extends the lock | A slow record blocks its partition's commit |
| More consumers when scaling out | Can exceed the partition count | Capped at the partition count |
| Startup fast-forward window (`fast_forward_if_stale`) | Seek by time | Seek by time (`offsets_for_times`) |
| `hot queue status`: waiting, running, oldest wait | Share lag metrics (KIP-1226) | Consumer lag per partition |

Hot does not rely on ordering, so partitions can be assigned freely. A
rebalance replays more than Redis redelivers, so handlers must deduplicate on
`event_id`.

Valkey stays in every configuration. It holds the cross-pod task leases
([`task_lease.rs`](../../crates/hot_task_worker/src/task_lease.rs), `SET NX PX`)
and the stream pub/sub ([`crates/hot/src/stream`](../../crates/hot/src/stream)),
which the app, API, and worker require whenever `queue.type` is `redis`. Kafka
adds a data service; it does not remove one.

## Support as of 2026-10-30

- **Apache Kafka 4.2** (February 2026) marks share groups production-ready and
  adds `RENEW` (KIP-1222) and share lag metrics (KIP-1226).
- **Rust:** librdkafka 2.15 has a share consumer as a preview, not for
  production. rust-rdkafka 0.39 has no binding for it.
- **Amazon MSK:** Kafka 4.2 runs only on Express brokers, and AWS says Express
  does not support KIP-932 yet. Standard brokers run 4.1, where queues are a
  preview.
- **Google Cloud Managed Service for Apache Kafka:** the documentation still
  says clusters run Kafka 3.7.1. No share groups and no new consumer protocol.
- **Confluent Cloud:** queues are generally available on Enterprise and
  Dedicated clusters, on both clouds. Standard clusters are expected later.

## Service choices

| | AWS | GCP |
| --- | --- | --- |
| Primary | MSK Express brokers, Kafka 4.2, IAM authentication over SASL/OAUTHBEARER | Managed Service for Apache Kafka, OAUTHBEARER with Google tokens; reuse the `redis.auth = gcp-iam` token code |
| Share groups today | Only Confluent Cloud through AWS Marketplace | Only Confluent Cloud through Google Cloud Marketplace |
| Existing endpoint | Confluent Cloud, or self-run Kafka 4.2 | Same |
| Local and CI | `apache/kafka` 4.2 container, single node, KRaft | Same |

To check against the OpenTofu stacks:

- [ ] Whether MSK Express clusters require three Availability Zones. The AWS
      scalable stack builds two.
- [ ] Cost. Each cluster needs at least three brokers, which likely makes Kafka
      the most expensive data service in a scalable installation.

## Phase 0 Decide the reason

- [ ] Pick the reason from [Why Kafka](#why-kafka). If it is durability, qualify
      MemoryDB for Valkey on AWS before any Kafka work.

## Phase 1 Backend-neutral groundwork

Worth doing whatever Phase 0 decides.

- [ ] Write a conformance suite that runs against every backend. Cover:
      - lease and ack;
      - redelivery after a dropped lease;
      - orphan reclaim;
      - retry, then DLQ;
      - infra backoff;
      - the fast-forward window;
      - purge;
      - status counts and oldest wait.

      The SQLite panic-redelivery loop is the kind of defect it would catch.
- [ ] Remove Redis-specific pieces from the public queue API:
      `with_consumer_name`, `cleanup_streams`, and the `RedisQueueAdmin` behind
      `hot queue status` and `hot queue clear`. Replace them with a
      `QueueAdmin` trait.

## Phase 2 Share-group spike

- [ ] Prototype raw FFI to librdkafka's `rd_kafka_share_*` API.
- [ ] Run it against Kafka 4.2 locally and against Confluent Cloud.
- [ ] Measure lock renewal for long tasks and redelivery after a crash.

## Phase 3 Kafka backend on classic consumer groups

- [ ] Start with `hot:event`, `hot:alert`, and `hot:email`. Keep `hot:task`
      (runs for hours) and `hot:request`/`hot:response` (request/response
      calls) on Valkey.
- [ ] Build:
      - a per-partition tracker for out-of-order acks;
      - retry topics and a DLQ topic;
      - an idempotent producer with `acks=all`;
      - deduplication on `event_id`;
      - lease timing from record timestamps;
      - consumer lag in `hot queue status --json`.
- [ ] Put it behind a `kafka` Cargo feature. librdkafka brings in OpenSSL
      while the rest of Hot uses rustls, and it complicates the Windows and
      macOS builds.
- [ ] Add configuration following the flags-mirror-conf-keys convention:
      `queue.type = "kafka"`, `kafka.bootstrap`, `kafka.auth` (`aws-iam`,
      `gcp-iam`, `scram`, or `mtls`), and `kafka.topic-prefix`, with `_FILE`
      secrets.

## Phase 4 OpenTofu

Builds on the self-hosting work in #189.

- [ ] Add `data.kafka.mode` (`none`, `managed`, or `existing`) beside
      `data.postgres.mode` and `data.valkey.mode`. Validation rejects it on
      `single-machine`.
- [ ] Provision MSK Express in `stacks/aws/scalable` and
      `google_managed_kafka_cluster` in `stacks/gcp/scalable`, with private
      networking. Grant each role identity only the Kafka permissions it needs.
- [ ] Have Hot create its topics idempotently at startup, so managed and
      existing clusters behave the same.
- [ ] Add preflight checks: authentication, partition count,
      `min.insync.replicas`, and retention.
- [ ] Add lag and oldest-wait alerts.
- [ ] Bound worker autoscaling by partition count, the way the connection
      budget bounds it today.

## Phase 5 Move to share groups

Start when librdkafka's share consumer is generally available, rust-rdkafka
binds it, and MSK Express or Google Cloud supports KIP-932.

- [ ] Add `kafka-share`, which replaces the ack tracker and retry topics.
- [ ] Move `hot:task` off Valkey.

## Phase 6 hot-cloud

- [ ] Adopt only if Phase 0 points that way, paired through `hot-public.ref`
      as usual.

## References

- [Apache Kafka 4.2.0 release](https://kafka.apache.org/blog/2026/02/17/apache-kafka-4.2.0-release-announcement/)
- [librdkafka v2.15.0](https://github.com/confluentinc/librdkafka/releases/tag/v2.15.0)
- [rust-rdkafka changelog](https://github.com/fede1024/rust-rdkafka/blob/master/changelog.md)
- [Amazon MSK supported versions](https://docs.aws.amazon.com/msk/latest/developerguide/supported-kafka-versions.html)
- [MSK Express adds Kafka 4.2](https://aws.amazon.com/about-aws/whats-new/2026/07/aws-msk-express-version-42/)
- [Managed Service for Apache Kafka release notes](https://docs.cloud.google.com/managed-service-for-apache-kafka/docs/release-notes)
- [Managed Service for Apache Kafka authentication](https://docs.cloud.google.com/managed-service-for-apache-kafka/docs/authentication-kafka)
- [Confluent Cloud queues GA](https://www.confluent.io/blog/kafka-queue-semantics-share-consumer-ga/)
- [Confluent Cloud Q1 2026 launch](https://www.confluent.io/blog/2026-q1-confluent-cloud-launch/)
