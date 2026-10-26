# Cancelled execution quota recovery

A task becoming `cancelled` records the user's request. It does not establish
that its container has stopped. Claimed cancelled containers keep their reserved
compute, duration, and account concurrency slot until termination is confirmed,
or until long after their deadline when nothing shows them still running. Old
heartbeats alone never release these reservations.

The owning worker settles measured compute and duration after confirmed exit.
The recovery pass that every task worker runs can kill and verify a labelled
local container; when measurements were lost, it charges the reserved maximum
before releasing the slot. Repeated settlement cannot refund already charged
compute.

## Release after the deadline

A cancelled container's deadline is its start plus its timeout. A working
runtime stops a container at its timeout, so a reservation is released without
termination evidence 15 minutes after the deadline when nothing shows the
container still running:

- no runtime that the recovery pass can reach knows the container, because
  another worker generation ran it (a lost host, or a worker restart); or
- the owning runtime doesn't answer when asked (a Kata runtime after a timeout
  or an error, say).

The release charges the reserved maximum and records the task's result with
`termination-confirmed: false`, and the worker logs `Releasing a cancelled
execution's quota after its deadline without termination evidence`. A container
that its runtime reports running keeps its reservation however late it is.

The release trades a small risk for availability: a container that outlives its
deadline on a host the recovery pass can't see runs uncounted. Without it, one
such container blocks every container task of an account limited to one
concurrent container until an operator steps in.

## Before the deadline

If logs report `Cancelled quota needs recovery on its owning runtime` or
`Cancelled execution termination is unconfirmed` and the account can't wait for
the deadline, locate the original worker's runtime and the task/container
identifiers. Verify that the actual workload is stopped, including any
surviving Kata VM/shim processes, before changing quota state. If the runtime is
unreachable or termination cannot be established, keep the reservation and
recover the runtime first, or wait for the release after the deadline.

After verified termination, use a trusted recovery job with the task's exact
organization/environment scope and the saved worker fence. Read
`Task::quota_reservation`, then call `Task::complete` with `TaskStatus::Cancelled`,
the same non-null worker ID, a nonnegative measured duration, and a result with a
nonnegative top-level `compute-units` integer. If measurements are unavailable,
use the stored reserved compute and duration, not zero. This atomically preserves
cancelled status, persists the charge, and clears reservations. Record the task,
original worker, runtime termination evidence, and whether usage was estimated.
Do not clear reservation fields directly or change cancelled status to completed.

## Tests

The integration test `docker_cancellation_requires_exit_before_releasing_quota`
is ignored by default because it requires a Docker host and may pull
`alpine:3.21`. Run it explicitly on the deployed Docker connection path. Generic
unit tests verify that running/unknown runtime states and stale worker fences
cannot settle quota before the deadline, and that a release after the deadline
charges the reserved maximum; they do not qualify the deployed container
runtime.
