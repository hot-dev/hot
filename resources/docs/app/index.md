---
description: "Use the Hot App dashboard to inspect projects, runs, events, traces, streams, logs, agents, and deployment state."
---

# Hot App

The Hot App is a web-based management and observability platform for your Hot projects. It provides visibility into executions, events, and streams, along with project configuration and team management.

**Access the Hot App:**

- **Local development**: Run `hot dev` and open [http://localhost:4680](http://localhost:4680)
- **Hot Cloud**: Sign up at [app.hot.dev](https://app.hot.dev)

## Navigation & Scope

The sidebar provides navigation to all features of the Hot App. At the top are two key selectors:

- **Organization** - Select which organization to view
- **Environment** - Select which environment within that organization (e.g., development, staging, production)

**These selectors control the scope of everything below.** When you select an organization and environment, all data throughout the app—runs, events, streams, projects, files, and more—is filtered to that specific context.

From each selector dropdown, you can also access management screens:

- **Organizations** - Create new organizations, manage users, configure teams, and handle billing
- **Environments** - Create new environments, edit environment settings

This scoping model keeps your data organized and allows you to easily switch between different projects or deployment stages.

## Dashboard

The **Dashboard** is your home screen, providing an at-a-glance overview of your Hot environment:

- **Issues banner** - A compact alert bar linking directly to failed runs, failed tasks, and unhandled events
- **Hero metrics** - Four cards showing totals and status breakdowns for Runs, Tasks (including CUS), Events, and Streams
- **Activity charts** - A scrollable grid of charts: run activity, run type distribution, task activity, CUS over time, event activity, event type distribution, stream activity, and stream composition
- **Issues** - Expanded tables for failed runs, failed tasks, and unhandled events

Use the filters at the top (project, time range, granularity) to scope the data. The dashboard auto-refreshes via server-sent events (SSE), and all charts and metrics update when filters change.

## Runs

The **Runs** view shows all executions of your Hot code:

- **Status** - Running, succeeded, failed, or cancelled
- **Type** - How it was triggered (call, event, schedule, run, eval, repl)
- **Duration** - How long the run took
- **Timestamp** - When the run started

Click any run to see the full execution trace, including:

- Input parameters
- Each expression evaluated with timing
- Return values or error details
- File attachments (if any)
- Parent/child run hierarchy

You can **retry** failed runs or **rerun** any completed run directly from the detail view.

### Run states

| State | Description |
|-------|-------------|
| `running` | Run is currently executing |
| `succeeded` | Run completed successfully |
| `failed` | Run failed with an error |
| `cancelled` | Run was cancelled |

## Tasks

The **Tasks** view shows asynchronous and container-based jobs:

- **Status** - Queued, running, completed, or failed
- **Duration** - How long the task ran
- **CUS** - Compute units consumed by the task
- **Timestamp** - When the task was created

Click any task to see:

- Task configuration and metadata
- Timing breakdown (queue time, container pull, execution)
- Call data from the task's execution
- Container logs (for `::hot::box` tasks)
- Stream graph showing the task in context with its originating run

### Task states

| State | Description |
|-------|-------------|
| `queued` | Task is waiting to be picked up |
| `running` | Task is currently executing |
| `completed` | Task finished successfully |
| `failed` | Task failed with an error |

## Events

The **Events** view shows all events received by your Hot application:

- **Event type** - The event name (e.g., `user:created`)
- **Payload** - The event data
- **Handled status** - Whether the event triggered a handler
- **Timestamp** - When the event was received

Click any event to see:

- Full event payload
- Which runs were triggered by this event
- Event metadata

Events are the primary way to trigger Hot functions. See [Events & Handlers](/docs/events) for more on defining event handlers.

## Streams

**Streams** group related runs and events together, providing a unified view of a logical workflow or request. When a run triggers other runs or emits events, they're all linked under the same stream.

- **Stream ID** - Unique identifier for the stream
- **Run count** - Number of runs in this stream
- **Event count** - Number of events in this stream
- **Timeline** - Visual flow of runs and events

Streams are especially useful for tracing complex workflows that span multiple function calls and events.

## Agents

The **Agents** view shows all deployed [agents](/docs/agents) in your environment. Agents are typed groups of event handlers, schedules, and webhooks that share identity.

- **Agents list** — Card grid showing each agent's name, namespace, description, tags, handler count, and project. Search by name, namespace, or project. A topology graph spanning all agents is shown at the top of the page.
- **Agent Dashboard** — Click an agent to open its dashboard. The default **Graph** tab shows an interactive topology visualization of the agent's handlers, triggers, and event sends. Click any node to open an inspector sidebar with full details (namespace, source location, description, retry config, handled/sent events). Use the toolbar to toggle horizontal/vertical layout, open/close the inspector, zoom, or download the graph as a PNG. Additional tabs show **Handlers** (all linked handlers with trigger details), **Runs** (paginated run history), and **Streams** (related streams).
- **Dashboard health widget** — The main Dashboard shows an Agent Health section with a health indicator per agent (green >95%, yellow 80–95%, red <80% success rate) and agent vs. non-agent run breakdown.

Agents are defined using `agent` metadata on types and `meta {agent: TypeName}` on handler functions. See [Agents](/docs/agents) for the full definition and patterns.

## Files

The **Files** view lets you browse and download files stored by your Hot functions:

- View file metadata (size, content type, timestamps)
- Download files directly
- See which run created each file

Files can be created in your Hot code using functions in the [`::hot::file`](https://hot.dev/pkg/hot.dev/hot-std/hot/file) namespace.

## Scheduled Runs

The **Scheduled Runs** view shows all functions with schedule metadata:

- **Schedule** - Cron expression defining when the function runs
- **Next run** - When the function will next execute
- **Recent runs** - History of scheduled executions

See [Schedules](/docs/schedules) for more on defining scheduled functions.

## Event Handlers

The **Event Handlers** view lists all functions that handle events:

- **Event pattern** - Which events this handler responds to
- **Function** - The handler function name
- **Project** - Which project contains this handler

## MCP Services

The **MCP Services** view lists all functions exposed as [Model Context Protocol](/docs/mcp) tools. Tools are grouped by **service**, showing tool name, description, file location, and project. Use the service filter to narrow the list.

MCP tools are driven by `mcp` metadata in your source code — they're automatically registered on deploy and unregistered when removed. See [MCP Services](/docs/mcp) for how to define tools and configure the MCP endpoint.

## Webhooks

The **Webhooks** view lists all functions exposed as [webhook endpoints](/docs/webhooks). Endpoints are grouped by **service**, showing method, path, function, auth mode, and description. Each service detail page shows the base URL for the webhook.

Webhook endpoints are driven by `webhook` metadata in your source code — they're automatically registered on deploy and unregistered when removed. See [Webhooks](/docs/webhooks) for how to define endpoints and configure authentication.

## Alerts

The **Alerts** view lets you configure monitoring and notifications for your Hot applications — run failures, deployment issues, and custom alerts from your code. Configure destinations (email, Slack, PagerDuty, webhook), channels, and subscriptions.

See [Alerts](/docs/alerts) for the full documentation on channels, destinations, subscriptions, and sending alerts from code.

## Projects

**Projects** represent deployed Hot applications. Each project shows:

- **Active status** - Whether the project is currently active
- **Builds** - Deployment history with the ability to deploy previous builds
- **Documentation** - Auto-generated docs from your Hot code

## Context Variables

**Context Variables** store configuration values accessible to your Hot code at runtime—things like API keys, feature flags, and environment-specific settings.

Context variables can be defined at two levels:

- **Environment level** - Shared across all projects in that environment
- **Project level** - Specific to a single project, overrides environment-level values

This hierarchy lets you set common defaults at the environment level while allowing individual projects to override specific values when needed.

Access context values in your Hot code:

```hot
api-key ::hot::ctx/get("API_KEY")
debug-mode ::hot::ctx/get("DEBUG", false)
```

## Docs

The **Docs** section provides auto-generated documentation for your deployed Hot projects:

- Browse namespaces and functions
- View function signatures and types
- Search across your codebase
- Explore package dependencies

Each build includes documentation for both your project's source code and the specific package dependencies included in that build. This ensures the docs always match the exact versions deployed.

The [hot-std](https://hot.dev/pkg/hot.dev/hot-std) standard library documentation is always available publicly at hot.dev, independent of any specific build.

## API Keys

**API Keys** authenticate your requests to the Hot API with configurable permissions and fine-grained resource/action scoping.

## Service Keys

**Service Keys** are customer-facing credentials with granular permissions, designed for external integrations and white-label use.

See [Authentication](/docs/authentication) for full documentation on API keys, service keys, sessions, and the permissions model.

## Custom Domains

**Custom Domains** let you map your own domain names to your Hot Dev environment for branded MCP, webhook, and API endpoints.

See [Custom Domains](/docs/domains) for setup instructions, DNS configuration, and verification.

## Access Attribution

Run and event detail pages display **access attribution** when the action was initiated via the API. This shows which credential (API key, service key, or session) made the request, along with the IP address, user agent, and HTTP request details.

For actions initiated from the dashboard (e.g., re-running a function), the originating user is shown instead.

## Organizations & Teams

Hot supports multi-tenant organization structures:

- **Organizations** - Top-level accounts with billing and user management
- **Teams** - Groups within an organization for access control
- **Environments** - Isolated execution contexts (dev, staging, production)

### User Roles

| Role | Permissions |
|------|-------------|
| Admin | Full access, manage users, billing, projects, settings |
| Member | View and execute, limited configuration |
