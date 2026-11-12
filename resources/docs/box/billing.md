---
description: "Understand Hot Box billing with Compute Unit Seconds (CUS), plan quotas, overages, and organization budgets."
---

# Container Billing (CUS)

Container tasks are billed using **Compute Unit Seconds** (CUS), a metric that combines wall-clock time with container size.

## What Are Compute Unit Seconds (CUS)

CUS measure container resource usage for billing. The formula:

```
CUS = ceil(wall_clock_seconds × size_multiplier)
```

- **wall_clock_seconds** — Billable execution time (see [What Counts as Billable Time](#what-counts-as-billable-time))
- **size_multiplier** — Multiplier from the container size preset
- **ceil** — Rounds up (e.g. 0.5 seconds at 1x = 1 CUS)

A 10-second run at `small` (1.0x) consumes 10 CUS. The same run at `nano` (0.25x) consumes 3 CUS.

## What Counts as Billable Time

Billable time measures your workload's execution window, not the platform's overhead. The clock starts when your container begins booting and stops when your command exits.

**Billed:**

- Container boot and runtime initialization
- Your command's execution, from entrypoint to exit

**Never billed:**

- **Image pull** — transferring your image to the worker, even on a cold worker that has never seen it. Identical tasks cost the same whether or not the worker that picked them up already had the image cached.
- **Capacity waits** — time spent queued for an execution slot.
- **Platform cleanup** — log collection and container removal after your command exits.
- **Preparation** — bundle download and extraction for `mounts` entries, and other worker-side setup.

The `duration-ms` field on task results and completion events reflects billable time. The `TaskResult` from `::hot::task/await` reports the phases separately as `image-pull-ms`, `slot-wait-ms`, `execution-ms` and `logs-collect-ms`, so you can see what was excluded. The task's stored result also records `runtime-start-ms`, the time the container took to start. `TaskResult` doesn't carry it: read it from the `result` of the task record the API returns.

**Timeouts are separate from billing.** A container task's `timeout` budget is measured from when a worker claims the task, so it covers preparation phases as well as execution: a slow image pull consumes your task's time budget, but never your CUS.

**Cancelling stops the clock.** `::hot::task/cancel` on a running container task stops its container, and the task is charged CUS for the time it ran until then.

## CUS Multiplier by Size

| Size | Multiplier |
|------|------------|
| `nano` | 0.25x |
| `micro` | 0.5x |
| `small` | 1.0x |
| `medium` | 2.0x |
| `large` | 4.0x |
| `xlarge` | 8.0x |
| `2xlarge` | 16.0x |
| `4xlarge` | 32.0x |

A task that names no `size` counts as the largest preset with no more memory than it has. See [Without a Size](/docs/box#without-a-size).

Get multipliers programmatically with `::hot::box/sizes()`:

```hot
all-sizes ::hot::box/sizes()
// [{name: "nano", memory-mb: 64, cus-multiplier: 0.25, ...}, ...]
```

## Included CUS per Plan

| Plan | Included CUS per Month |
|------|------------------------|
| Free | 5,000 |
| Starter | 50,000 |
| Pro | 500,000 |
| Scale | 5,000,000 |

## Overage

Usage beyond included CUS is handled by plan:

- **Free plan** — Hard cap. When CUS are exhausted, new container tasks are blocked until the next billing period.
- **Paid plans** (Starter, Pro, Scale) — Overage is billed at the per-CUS rate on your next invoice.

## Org Budget

Organizations can set an optional **spending cap** (`compute_units_budget`). When reached, container tasks are hard-blocked regardless of plan. This prevents unexpected overage charges.

## Checking Quota

Use `::hot::box/quota()` to check remaining CUS before starting containers:

```hot
q ::hot::box/quota()
q.compute-units-limit       // The plan's CUS per period (-1 = unlimited)
q.compute-units-remaining   // CUS left this period
q.compute-units-used        // CUS consumed this period
q.minutes-limit             // The plan's container minutes per period (-1 = unlimited)
q.minutes-remaining         // Container minutes left this period
q.concurrent-remaining      // Containers that can start now (-1 = unlimited)
q.overage                   // true if usage exceeds included (paid plans)
```

On the Free plan, `compute-units-remaining` and `minutes-remaining` stop at `0`.
On a paid plan they go below `0` as overage accrues, and `overage` is `true`.
With no limit they read `-1`, which a paid plan one unit over also reads: check
the limit for `-1` to tell an unlimited plan from overage.
`concurrent-remaining` counts the organization's running containers against
the plan's limit, and never goes below `0`.

Example: check quota before running a container:

```hot
q ::hot::box/quota()
// 0 means the Free plan's cap is reached. A paid plan goes negative instead.
if(eq(q.compute-units-remaining, 0),
  fail("CUS quota exhausted"),
  ::hot::box/start(::hot::box/BoxConf({image: "alpine:3.21", cmd: ["echo", "hello"], size: "nano"})))
```
