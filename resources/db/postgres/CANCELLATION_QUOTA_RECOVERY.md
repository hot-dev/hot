# Cancelled execution quota recovery

A task becoming `cancelled` records the user's request. It does not establish
that its container has stopped. Claimed cancelled containers keep their reserved
compute, duration, and account concurrency slot until termination is confirmed.
Old heartbeats and elapsed deadlines never release these reservations.

The owning worker settles measured compute and duration after confirmed exit.
The Docker recovery pass can kill and verify a labelled local container; when
measurements were lost, it charges the reserved maximum before releasing the
slot. A task's absence on an unrelated worker host is not termination evidence.
Repeated settlement cannot refund already charged compute.

If logs report `Cancelled quota needs recovery on its owning runtime` or
`Cancelled execution termination is unconfirmed`, locate the original worker's
runtime and the task/container identifiers. Verify that the actual workload is
stopped, including any surviving Kata VM/shim processes, before changing quota
state. If the runtime is unreachable or termination cannot be established, keep
the reservation and recover the runtime first.

Kata normal non-timeout completion supplies exit evidence. The current generic
executor cannot independently inspect Kata termination after timeout/error or
orphan cleanup; those paths retain the reservation and require operator
verification. Docker tasks cancelled during setup before a durable container ID
may also need review if no labelled container can be found after worker restart.

After verified termination, use a trusted recovery job with the task's exact
organization/environment scope and the saved worker fence. Read
`Task::quota_reservation`, then call `Task::complete` with `TaskStatus::Cancelled`,
the same non-null worker ID, a nonnegative measured duration, and a result with a
nonnegative top-level `compute-units` integer. If measurements are unavailable,
use the stored reserved compute and duration, not zero. This atomically preserves
cancelled status, persists the charge, and clears reservations. Record the task,
original worker, runtime termination evidence, and whether usage was estimated.
Do not clear reservation fields directly or change cancelled status to completed.

The integration test `docker_cancellation_requires_exit_before_releasing_quota`
is ignored by default because it requires a Docker host and may pull
`alpine:3.21`. Run it explicitly on the deployed Docker connection path. Generic
unit tests verify that running/unknown runtime states and stale worker fences
cannot settle quota; they do not qualify the deployed container runtime.
