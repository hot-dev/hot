---
description: "Run Hot functions on a schedule with cron expressions, natural-language schedules, retries, and dynamic schedules."
---

# Schedules

The Hot Scheduler runs functions on a schedule using cron expressions or natural language.

## Scheduled Runs

Schedule functions to run at specific times using the `schedule` metadata:

```hot
::myapp::jobs ns

// Run every hour
hourly-cleanup
meta {schedule: "0 * * * *"}
fn () {
  cleanup-expired-sessions()
  prune-old-logs()
}

// Run daily at midnight UTC
daily-report
meta {schedule: "0 0 * * *"}
fn () {
  generate-daily-report()
  send-to-slack()
}

// Run every 5 minutes
health-check
meta {schedule: "*/5 * * * *"}
fn () {
  check-external-services()
}
```

Scheduled functions can be grouped under an [agent](/docs/agents) by adding `agent: TypeName` to the metadata. This enables per-agent run tracking, health metrics, and observability in the Hot App.

## Cron Expression Format

```
┌───────────── second (0-59, optional)
│ ┌───────────── minute (0-59)
│ │ ┌───────────── hour (0-23)
│ │ │ ┌───────────── day of month (1-31)
│ │ │ │ ┌───────────── month (1-12 or JAN-DEC)
│ │ │ │ │ ┌───────────── day of week (0-6 or SUN-SAT)
│ │ │ │ │ │ ┌───────────── year (optional)
│ │ │ │ │ │ │
* * * * * * *
```

Hot accepts common cron forms (5, 6, or 7 fields) plus nickname forms such as `@daily`.

Common patterns:

| Pattern | Description |
|---------|-------------|
| `* * * * *` | Every minute |
| `*/15 * * * * *` | Every 15 seconds |
| `*/5 * * * *` | Every 5 minutes |
| `0 * * * *` | Every hour |
| `0 0 * * *` | Daily at midnight |
| `0 0 * * 0` | Weekly on Sunday |
| `0 0 1 * *` | Monthly on the 1st |
| `@daily` | Daily (nickname form) |

## Natural Language Schedules

You can also use plain English to define schedules:

```hot
::myapp::jobs ns

// Natural language schedules
daily-digest
meta {schedule: "every day at 9:00 am"}
fn () {
  send-daily-digest()
}

weekly-review
meta {schedule: "on Sunday at 12:00"}
fn () {
  generate-weekly-review()
}

payroll
meta {schedule: "run at midnight on the 1st and 15th of the month"}
fn () {
  process-payroll()
}
```

Supported English patterns:

| English Phrase | Equivalent Cron |
|----------------|-----------------|
| `every minute` | `* * * * *` |
| `every 15 seconds` | `*/15 * * * * *` |
| `every day at 4:00 pm` | `0 0 16 */1 * ? *` (equivalent) |
| `at 10:00 am` | `0 0 10 * * ? *` (equivalent) |
| `run at midnight on the 1st and 15th of the month` | `0 0 0 1,15 * ? *` (equivalent) |
| `on Sunday at 12:00` | `0 0 12 ? * SUN *` (equivalent) |
| `7pm every Thursday` | `0 0 19 ? * THU *` (equivalent) |
| `midnight on Tuesdays` | `0 0 0 ? * TUE *` (equivalent) |

Natural language schedules are converted to cron expressions internally. Both formats are fully supported and can be mixed within the same project.

## Dynamic Schedules

In addition to metadata-driven schedules (defined at build time), you can create schedules dynamically at runtime using events. This is useful for:

- **One-time scheduled calls** - Execute a function once at a specific time
- **User-triggered scheduling** - Let users schedule tasks for later
- **Dynamic recurring jobs** - Create cron schedules based on runtime conditions

### Creating Schedules

Use the `hot:schedule:new` event to create a schedule:

```hot
// Schedule for a specific datetime
send("hot:schedule:new", {
  fn: "::myapp::orders/process-order",
  args: [{order_id: "12345"}],
  schedule: "2024-01-15T10:30:00Z"
})

// Schedule for 10 minutes from now
send("hot:schedule:new", {
  fn: "::myapp::reminders/send-reminder",
  args: [{user_id: user.id, message: "Time to check in!"}],
  schedule: "in 10 minutes"
})

// Schedule with natural language duration
send("hot:schedule:new", {
  fn: "::myapp::notifications/send-followup",
  args: [{email: customer.email}],
  schedule: "2 hours from now"
})

// Create a recurring schedule dynamically
send("hot:schedule:new", {
  fn: "::myapp::reports/generate",
  args: [{report_type: "daily"}],
  schedule: "every day at 9am"
})
```

The `hot:schedule:new` event payload uses:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `fn` | `Str` or function reference | Yes | Target function (for example `::myapp::jobs/process`) |
| `args` | `Vec` | No | Function arguments. Defaults to `[]` when omitted/null. |
| `schedule` | `Str` or `DateTime` | Yes | When to run (one-time or recurring expression) |

### Schedule Formats

The `schedule` field supports multiple formats:

| Format | Example | Description |
|--------|---------|-------------|
| ISO 8601 datetime | `"2024-01-15T10:30:00Z"` | Execute at exact time |
| Duration | `"10 minutes"`, `"2h"`, `"1 day 3 hours"` | Execute after duration |
| Natural language | `"in 10 minutes"`, `"2 hours from now"` | Human-friendly durations |
| Cron expression | `"0 30 9 * * MON"` | Recurring schedule |
| English cron | `"every day at 9am"`, `"every Monday at 2 PM"` | Natural language recurring |

### Cancelling Schedules

Cancel pending schedules using the `hot:schedule:cancel` event:

```hot
// Cancel by schedule ID (returned from hot:schedule:new)
send("hot:schedule:cancel", {
  schedule-id: "01916d8a-9c12-7f00-8000-123456789abc"
})

// Cancel all schedules for a specific function
send("hot:schedule:cancel", {
  fn: "::myapp::jobs/heavy-process"
})
```

The `hot:schedule:cancel` event payload supports either:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schedule-id` | `Str` (UUID) | Conditionally | Cancel one schedule by ID |
| `fn` | `Str` or function reference | Conditionally | Cancel active schedules for a function |

At least one of `schedule-id` or `fn` must be provided.

Cancelling schedules is useful for:
- Removing pending one-time schedules that are no longer needed
- Disabling recurring schedules without redeploying
- Cleaning up after a user cancels an action

### Dynamic vs Metadata Schedules

| Feature | Metadata Schedule | Dynamic Schedule |
|---------|-------------------|------------------|
| Defined in | Hot code (`meta {schedule: ...}`) | Runtime via `hot:schedule:new` |
| Lifecycle | Tied to build/deployment | Can be created/cancelled anytime |
| One-time support | No | Yes |
| Visible in UI | Always | When active |
| Use case | Regular jobs (daily reports, cleanup) | User-triggered, conditional scheduling |

### Example: Scheduled Reminders

```hot
::myapp::reminders ns

// Schedule a reminder for later
schedule-reminder fn (user-id: Str, message: Str, delay: Str): Str {
  // Create a one-time schedule
  schedule-id send("hot:schedule:new", {
    fn: "::myapp::reminders/send-reminder",
    args: [{user-id: user-id, message: message}],
    schedule: delay
  })

  // Return the schedule ID so it can be cancelled if needed
  schedule-id
}

// Cancel a pending reminder
cancel-reminder fn (schedule-id: Str): Bool {
  send("hot:schedule:cancel", {schedule-id: schedule-id})
}

// The actual reminder function (called by scheduler)
send-reminder fn (data: Map) {
  user get-user(data.user-id)
  send-push-notification(user.device-token, data.message)
}
```

Usage:
```hot
// Schedule a reminder for 30 minutes from now
reminder-id schedule-reminder("user-123", "Time for your meeting!", "in 30 minutes")

// Later, cancel if user dismisses early
cancel-reminder(reminder-id)
```

## Retries

Scheduled functions can retry automatically when they fail:

```hot
import-data meta {
  schedule: "0 2 * * *",
  retry: 5
}
fn () {
  import-from-sftp()
}
```

For full retry configuration (attempts, delay, backoff, jitter, limits, and `pending_retry` behavior), see [Retries](/docs/retries).

## How It Works

When a scheduled function's time arrives, the scheduler sends a `hot:schedule` event with the function details. A worker picks up the event and executes the function.

Scheduled runs are tracked and visible in the Hot App alongside event-triggered and API-triggered runs.
