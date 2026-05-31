---
description: "Understand the Hot platform model for projects, functions, runs, events, streams, tasks, durability, and observability."
---

# Hot Platform

The Hot Platform is a complete backend workflow automation system. It combines a purpose-built programming language with managed infrastructure for running, monitoring, and scaling your workflows.

## Architecture Overview

<svg viewBox="0 0 600 480" class="w-full max-w-2xl mx-auto" style="font-family: system-ui, sans-serif;">
  <!-- Styles -->
  <style>
    .arch-box { fill: #f8fafc; stroke: #cbd5e1; stroke-width: 2; }
    .arch-box-accent { fill: #fef2f2; stroke: #ef4444; stroke-width: 2; }
    .arch-text { fill: #1e293b; font-size: 14px; font-weight: 600; }
    .arch-text-sm { fill: #475569; font-size: 11px; }
    .arch-arrow { stroke: #94a3b8; stroke-width: 2; fill: none; }
    @media (prefers-color-scheme: dark) {
      .arch-box { fill: #334155; stroke: #475569; }
      .arch-box-accent { fill: #451a1a; stroke: #f87171; }
      .arch-text { fill: #f1f5f9; }
      .arch-text-sm { fill: #cbd5e1; }
      .arch-arrow { stroke: #94a3b8; }
    }
  </style>

  <!-- Your Backend -->
  <rect x="20" y="10" width="560" height="70" rx="8" class="arch-box"/>
  <text x="300" y="35" text-anchor="middle" class="arch-text">Your Backend</text>
  <g transform="translate(80, 45)">
    <rect x="0" y="0" width="90" height="28" rx="4" class="arch-box"/>
    <text x="45" y="18" text-anchor="middle" class="arch-text-sm">Web Server</text>
  </g>
  <g transform="translate(190, 45)">
    <rect x="0" y="0" width="70" height="28" rx="4" class="arch-box"/>
    <text x="35" y="18" text-anchor="middle" class="arch-text-sm">CLI</text>
  </g>
  <g transform="translate(280, 45)">
    <rect x="0" y="0" width="70" height="28" rx="4" class="arch-box"/>
    <text x="35" y="18" text-anchor="middle" class="arch-text-sm">Scripts</text>
  </g>
  <g transform="translate(370, 45)">
    <rect x="0" y="0" width="90" height="28" rx="4" class="arch-box"/>
    <text x="45" y="18" text-anchor="middle" class="arch-text-sm">Integrations</text>
  </g>

  <!-- Arrow from backend to API -->
  <path d="M 300 80 L 300 110" class="arch-arrow" marker-end="url(#arrowhead)"/>

  <!-- Hot API -->
  <rect x="20" y="110" width="560" height="60" rx="8" class="arch-box-accent"/>
  <text x="300" y="135" text-anchor="middle" class="arch-text">Hot API</text>
  <text x="300" y="155" text-anchor="middle" class="arch-text-sm">Execute functions, send events, manage files</text>

  <!-- Arrow from API to Streams -->
  <path d="M 300 170 L 300 200" class="arch-arrow"/>

  <!-- Streams (container for Runs and Events) -->
  <rect x="20" y="200" width="560" height="100" rx="8" class="arch-box"/>
  <text x="300" y="222" text-anchor="middle" class="arch-text">Streams</text>
  <text x="300" y="238" text-anchor="middle" class="arch-text-sm">Real-time workflow execution</text>

  <!-- Runs (inside Streams) -->
  <g transform="translate(60, 250)">
    <rect x="0" y="0" width="220" height="40" rx="6" class="arch-box"/>
    <text x="110" y="26" text-anchor="middle" class="arch-text-sm">Runs — Function executions</text>
  </g>

  <!-- Events (inside Streams) -->
  <g transform="translate(320, 250)">
    <rect x="0" y="0" width="220" height="40" rx="6" class="arch-box"/>
    <text x="110" y="26" text-anchor="middle" class="arch-text-sm">Events — Async triggers</text>
  </g>

  <!-- Arrows from Streams to scheduler and workers -->
  <path d="M 150 300 L 150 320" class="arch-arrow"/>
  <path d="M 430 300 L 430 320" class="arch-arrow"/>

  <!-- Hot Scheduler -->
  <rect x="20" y="320" width="260" height="90" rx="8" class="arch-box-accent"/>
  <text x="150" y="355" text-anchor="middle" class="arch-text">Hot Scheduler</text>
  <text x="150" y="375" text-anchor="middle" class="arch-text-sm">Cron &amp; scheduled jobs</text>

  <!-- Hot Workers -->
  <rect x="300" y="320" width="280" height="90" rx="8" class="arch-box-accent"/>
  <text x="440" y="345" text-anchor="middle" class="arch-text">Hot Workers</text>
  <g transform="translate(320, 355)">
    <rect x="0" y="0" width="55" height="30" rx="4" class="arch-box"/>
    <text x="27" y="20" text-anchor="middle" class="arch-text-sm">Worker</text>
  </g>
  <g transform="translate(385, 355)">
    <rect x="0" y="0" width="55" height="30" rx="4" class="arch-box"/>
    <text x="27" y="20" text-anchor="middle" class="arch-text-sm">Worker</text>
  </g>
  <g transform="translate(450, 355)">
    <rect x="0" y="0" width="55" height="30" rx="4" class="arch-box"/>
    <text x="27" y="20" text-anchor="middle" class="arch-text-sm">Worker</text>
  </g>
  <text x="525" y="375" class="arch-text-sm">...</text>

  <!-- Hot App -->
  <rect x="20" y="430" width="560" height="50" rx="8" class="arch-box"/>
  <text x="300" y="455" text-anchor="middle" class="arch-text">Hot App</text>
  <text x="300" y="472" text-anchor="middle" class="arch-text-sm">Monitor runs, inspect events, debug workflows</text>

  <!-- Arrowhead marker -->
  <defs>
    <marker id="arrowhead" markerWidth="10" markerHeight="7" refX="9" refY="3.5" orient="auto">
      <polygon points="0 0, 10 3.5, 0 7" fill="#94a3b8"/>
    </marker>
  </defs>
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
