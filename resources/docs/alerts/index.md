---
description: "Set up alert channels, destinations, and subscriptions for run failures, deploy events, and custom Hot alert calls."
---

# Alerts

The Alerts system helps you monitor your Hot applications by automatically notifying you when important events occur, such as run failures or deployment issues. Alerts use a pub/sub model with three key components: **channels**, **destinations**, and **subscriptions**.

## How Alerts Work

1. An **alert event** fires (e.g., a run fails, or your code calls `alert()`)
2. The event name is matched against **channel** patterns
3. Matching channels trigger their **subscriptions**
4. Each subscription delivers the alert to one or more **destinations** (email, Slack, PagerDuty, webhook)

## Alert Channels

**Alert Channels** define the types of events that can trigger alerts. Channels use regex patterns to match alert event names.

### Built-in System Channels

Hot provides several built-in channels that are automatically available:

| Channel | Description |
|---------|-------------|
| `run:failed` | Triggered when a run fails (after all retries are exhausted). Payload includes `run_id`, `env_id`, `error`, and `timestamp`. |
| `run:cancelled` | Triggered when a run is cancelled. Payload includes `run_id`, `env_id`, and `timestamp`. |
| `deploy:failed` | Triggered when a deployment fails during worker processing (e.g., build extraction, storage retrieval, or handler loading). Payload includes `build_id`, `env_id`, `error`, and `timestamp`. |
| `deploy:succeeded` | Triggered when a deployment is fully processed and the build is ready to serve traffic. Payload includes `build_id`, `env_id`, and `timestamp`. |

These system channels are read-only and available to all organizations.

### Custom Channels

You can create custom channels with regex patterns to match specific alert types. For example:

- `run:.*` - Matches all run-related alerts
- `deploy:.*` - Matches all deployment-related alerts
- `payment:.*` - Matches custom payment-related alerts (triggered from Hot code via `alert()`)

Custom channels are scoped to your organization and can optionally be environment-specific. They can be created from the Channels tab by organization admins.

## Alert Destinations

**Alert Destinations** are the endpoints where alerts are delivered. Destinations are configured at the organization level and can be reused across multiple subscriptions and environments.

Hot supports four destination types:

| Type | Description | Configuration Fields |
|------|-------------|---------------------|
| **Email** | Send alerts to email addresses | Email address |
| **Slack** | Post alerts to Slack channels | Webhook URL, optional channel override |
| **PagerDuty** | Create PagerDuty incidents | Routing key (Integration Key), severity level |
| **Webhook** | POST alerts to custom HTTP endpoints | URL, optional custom headers (JSON) |

Each destination has a name, type, and can be enabled or disabled independently. All destination management requires organization admin permissions.

## Alert Subscriptions

**Alert Subscriptions** connect channels to destinations. When an alert event matches a channel pattern, all active subscriptions for that channel will deliver the alert to their configured destinations.

Subscriptions can be configured at two scopes:

- **Organization-wide** - Alerts are sent for all environments in the organization
- **Environment-specific** - Alerts are sent only for a specific environment

Each subscription can include multiple channels and multiple destinations, allowing you to route different types of alerts to different notification endpoints.

**Example workflow:**

1. Create an email destination: `ops-team@example.com`
2. Create a Slack destination: `#alerts` channel
3. Create a subscription that routes `run:failed` alerts to both destinations
4. When a run fails, both the email and Slack destinations receive the alert

Subscriptions can be enabled or disabled without deleting them, making it easy to temporarily pause notifications.

## Alert History

The **History** tab displays all triggered alerts with their delivery status. Each alert entry shows:

- The alert channel that was triggered
- When the alert was created
- A delivery summary (sent, pending, failed counts)

Click on an individual alert to see the full payload and detailed delivery information, including error messages for any failed deliveries.

## Sending Alerts from Hot Code

You can publish custom alerts from your Hot code using the `alert` function (auto-imported from `::hot::alert`):

```hot
// Send an alert with a payload
alert("payment:failed", {"order_id": order-id, "error": err-msg})

// Send an alert without a payload
alert("health:degraded")
```

Alerts published from code follow the same routing rules: matching channel patterns trigger deliveries to subscribed destinations.

## Managing Alerts in the Dashboard

Access alerts configuration from **Alerts** in the [Hot App](/docs/app) sidebar. The alerts interface has four tabs:

- **Destinations** - Configure where alerts are sent
- **Subscriptions** - Link alert channels to destinations
- **Channels** - View and manage alert event types
- **History** - View triggered alerts and their delivery status
