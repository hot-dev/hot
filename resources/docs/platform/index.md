---
description: "Understand Hot's platform model for projects, functions, runs, events, streams, tasks, durability, and observability."
---

# Hot Platform

The Hot Platform is a complete backend workflow automation system. It combines a purpose-built programming language with managed infrastructure for running, monitoring, and scaling your workflows.

## Architecture Overview

<svg viewBox="0 0 720 592" class="w-full max-w-2xl mx-auto" style="font-family: system-ui, sans-serif;">
  <!--
    Theming note: the Hot App toggles dark mode with a `dark` class on <html>
    (not the OS prefers-color-scheme), so these styles use `.dark` selectors.
  -->
  <!-- Neutral grays matching the app palette (input.css) — no blue tint. -->
  <style>
    .arch-box { fill: #f9f9f9; stroke: #cccccc; stroke-width: 1.5; }
    .arch-accent { fill: #fef2f2; stroke: #ef4444; stroke-width: 1.5; }
    .arch-chip { fill: #ffffff; stroke: #dddddd; stroke-width: 1; }
    .arch-title { fill: #111111; font-size: 15px; font-weight: 650; }
    .arch-sub { fill: #666666; font-size: 11.5px; }
    .arch-label { fill: #888888; font-size: 10.5px; font-style: italic; }
    .arch-arrow { stroke: #aaaaaa; stroke-width: 1.75; fill: none; }
    .arch-arrow-dashed { stroke: #aaaaaa; stroke-width: 1.75; fill: none; stroke-dasharray: 4 4; }
    .dark .arch-box { fill: #17171a; stroke: #2a2a30; }
    .dark .arch-accent { fill: #7f1d1d; stroke: #f87171; }
    .dark .arch-chip { fill: #212126; stroke: #2f2f36; }
    .dark .arch-title { fill: #f8f8f8; }
    .dark .arch-sub { fill: #aaaaaa; }
    .dark .arch-sub-accent { fill: #f0dcdc; }
    .dark .arch-label { fill: #aaaaaa; }
  </style>

  <defs>
    <marker id="arch-arrow-head" markerWidth="9" markerHeight="7" refX="7.5" refY="3.5" orient="auto-start-reverse">
      <polygon points="0 0, 9 3.5, 0 7" fill="#aaaaaa"/>
    </marker>
  </defs>

  <!-- Your Backend (producer: calls the API) -->
  <rect x="16" y="12" width="436" height="84" rx="10" class="arch-box"/>
  <text x="234" y="36" text-anchor="middle" class="arch-title">Your Backend</text>
  <rect x="46" y="50" width="96" height="30" rx="6" class="arch-chip"/>
  <text x="94" y="69" text-anchor="middle" class="arch-sub">Web Server</text>
  <rect x="154" y="50" width="64" height="30" rx="6" class="arch-chip"/>
  <text x="186" y="69" text-anchor="middle" class="arch-sub">CLI</text>
  <rect x="230" y="50" width="76" height="30" rx="6" class="arch-chip"/>
  <text x="268" y="69" text-anchor="middle" class="arch-sub">Scripts</text>
  <rect x="318" y="50" width="104" height="30" rx="6" class="arch-chip"/>
  <text x="370" y="69" text-anchor="middle" class="arch-sub">Integrations</text>

  <!-- Hot Scheduler (producer: cron fires emit events) -->
  <rect x="470" y="12" width="234" height="84" rx="10" class="arch-accent"/>
  <text x="587" y="44" text-anchor="middle" class="arch-title">Hot Scheduler</text>
  <text x="587" y="64" text-anchor="middle" class="arch-sub arch-sub-accent">Cron &amp; scheduled jobs</text>

  <!-- Backend <-> API (calls in, results and SSE back) -->
  <path d="M 234 101 L 234 121" class="arch-arrow" marker-start="url(#arch-arrow-head)" marker-end="url(#arch-arrow-head)"/>
  <text x="246" y="115" class="arch-label">REST calls &amp; SSE subscriptions</text>

  <!-- Scheduler -> Streams (emits schedule events) -->
  <path d="M 587 96 L 587 241" class="arch-arrow" marker-end="url(#arch-arrow-head)"/>
  <text x="599" y="172" class="arch-label">cron fires → emits events</text>

  <!-- Hot API -->
  <rect x="16" y="126" width="436" height="64" rx="10" class="arch-accent"/>
  <text x="234" y="152" text-anchor="middle" class="arch-title">Hot API</text>
  <text x="234" y="172" text-anchor="middle" class="arch-sub arch-sub-accent">Execute functions, send events, manage files</text>

  <!-- API -> Streams -->
  <path d="M 234 190 L 234 241" class="arch-arrow" marker-end="url(#arch-arrow-head)"/>
  <text x="246" y="220" class="arch-label">publishes events</text>

  <!-- Streams (grouping runs and events) -->
  <rect x="16" y="246" width="688" height="112" rx="10" class="arch-box"/>
  <text x="360" y="272" text-anchor="middle" class="arch-title">Streams</text>
  <text x="360" y="289" text-anchor="middle" class="arch-sub">Group related events &amp; runs for end-to-end tracing</text>
  <rect x="48" y="300" width="304" height="44" rx="6" class="arch-chip"/>
  <text x="200" y="326" text-anchor="middle" class="arch-sub">Events — async triggers</text>
  <rect x="368" y="300" width="304" height="44" rx="6" class="arch-chip"/>
  <text x="520" y="326" text-anchor="middle" class="arch-sub">Runs — function executions</text>

  <!-- Streams -> Workers -->
  <path d="M 360 358 L 360 387" class="arch-arrow" marker-end="url(#arch-arrow-head)"/>
  <text x="372" y="378" class="arch-label">workers pick up events &amp; execute runs</text>

  <!-- Hot Workers (consumer: executes Hot code) -->
  <rect x="16" y="392" width="688" height="96" rx="10" class="arch-accent"/>
  <text x="360" y="416" text-anchor="middle" class="arch-title">Hot Workers</text>
  <rect x="240" y="428" width="72" height="32" rx="6" class="arch-chip"/>
  <text x="276" y="448" text-anchor="middle" class="arch-sub">Worker</text>
  <rect x="324" y="428" width="72" height="32" rx="6" class="arch-chip"/>
  <text x="360" y="448" text-anchor="middle" class="arch-sub">Worker</text>
  <rect x="408" y="428" width="72" height="32" rx="6" class="arch-chip"/>
  <text x="444" y="448" text-anchor="middle" class="arch-sub">Worker</text>
  <text x="496" y="448" class="arch-sub">…</text>
  <text x="360" y="478" text-anchor="middle" class="arch-sub arch-sub-accent">Scale horizontally — each worker executes runs in isolation</text>

  <!-- Everything is recorded and observable in Hot App -->
  <path d="M 360 488 L 360 507" class="arch-arrow-dashed" marker-end="url(#arch-arrow-head)"/>
  <text x="372" y="502" class="arch-label">every run, event &amp; stream recorded</text>

  <!-- Hot App -->
  <rect x="16" y="512" width="688" height="64" rx="10" class="arch-box"/>
  <text x="360" y="538" text-anchor="middle" class="arch-title">Hot App</text>
  <text x="360" y="558" text-anchor="middle" class="arch-sub">Monitor runs, inspect events, debug workflows</text>
</svg>

## Core Concepts

### Runs

Every function call creates a run—a tracked execution with:

- Unique run ID for tracking
- Full execution trace
- Input parameters and return values
- Timing and performance data
- Error details if failed

[Learn more about Runs →](/docs/platform/runs-events-streams)

### Events

Events are the primary way to trigger asynchronous workflows. Emit events from your application or external systems, and Hot automatically routes them to registered handlers.

```hot
// Define an event handler
on-user-signup meta {on-event: "user:created"}
fn (event) {
  send-welcome-email(event.data.email)
  create-default-settings(event.data.id)
}
```

[Learn more about Events →](/docs/platform/runs-events-streams)

### Streams

Streams provide real-time data flow for long-running operations like AI responses, live updates, and bidirectional communication.

[Learn more about Streams →](/docs/platform/runs-events-streams)

### Workers

Workers are the execution engine of the Hot Platform. They pick up runs from the queue, execute your Hot code, and report results back.

- Scale horizontally by adding more workers
- Process event handlers and scheduled jobs
- Execute in isolated contexts for security

[Learn more about Workers →](/docs/events)

## Platform Components

| Component | Purpose |
|-----------|---------|
| [Hot API](/docs/api) | API for executing functions, sending events, and managing files |
| [Workers](/docs/events) | Execution engine for running Hot code |
| [Alerts](/docs/alerts) | Monitor your applications with notifications for run failures, deployments, and custom events |
| [MCP Services](/docs/mcp) | Expose Hot functions as MCP tools for AI agents |
| [Webhooks](/docs/webhooks) | Turn Hot functions into webhook endpoints for external services |
| [Custom Domains](/docs/domains) | Map your own domain names to your Hot Dev environment |
| [Hot App](/docs/app) | Real-time monitoring and debugging interface |

## Deployment Options

### Hot Cloud (Managed)

Deploy to the Hot Cloud with a single command:

```bash
hot deploy
```

Hot Cloud provides:
- Managed workers with auto-scaling
- Global edge deployment
- Built-in observability
- Zero infrastructure management

[See pricing →](/pricing)
