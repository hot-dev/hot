---
description: "Understand Container Usage Seconds billing for Hot Box workloads, including measurement, examples, and cost controls."
---

# Container Billing (CUS)

Container tasks are billed using **Compute Unit Seconds** (CUS), a metric that combines wall-clock time with container size.

## What Are Compute Unit Seconds (CUS)

CUS measure container resource usage for billing. The formula:

```
CUS = ceil(wall_clock_seconds × size_multiplier)
```

- **wall_clock_seconds** — Actual execution time
- **size_multiplier** — Multiplier from the container size preset
- **ceil** — Rounds up (e.g. 0.5 seconds at 1x = 1 CUS)

A 10-second run at `small` (1.0x) consumes 10 CUS. The same run at `nano` (0.25x) consumes 3 CUS.

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
q.compute-units-remaining   // CUS left this period (-1 = unlimited)
q.compute-units-used       // CUS consumed this period
q.tasks-remaining          // Tasks left (-1 = unlimited, 0 = exhausted)
q.overage                  // true if usage exceeds included (paid plans)
```

Example: check quota before running a container:

```hot
q ::hot::box/quota()
if(eq(q.tasks-remaining, 0), fail("No container tasks remaining"), "OK")
if(or(eq(q.compute-units-remaining, -1), gt(q.compute-units-remaining, 0)),
  ::hot::box/start(BoxConf({image: "alpine", cmd: ["echo", "hello"], size: "nano"})),
  fail("CUS quota exhausted"))
```
