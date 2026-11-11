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
fn (event) {
  cleanup-expired-sessions()
  prune-old-logs()
}

// Run daily at midnight UTC
daily-report
meta {schedule: "0 0 * * *"}
fn (event) {
  generate-daily-report()
  send-to-slack()
}

// Run every 5 minutes
health-check
meta {schedule: "*/5 * * * *"}
fn (event) {
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

Cron expressions and English schedules are evaluated in UTC. A schedule has no
time zone setting, so write local times as UTC: `0 9 * * *` runs at 09:00 UTC.

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
fn (event) {
  send-daily-digest()
}

weekly-review
meta {schedule: "on Sunday at 12:00"}
fn (event) {
  generate-weekly-review()
}

payroll
meta {schedule: "run at midnight on the 1st and 15th of the month"}
fn (event) {
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

A metadata `schedule` must repeat. A schedule that names a single run, such as
`"2026-11-01"`, `"in 10 minutes"`, `"2h"` or `"tomorrow at 9am"`, is an
`invalid-schedule` error in `hot check`, builds and deploys. Hot 3.0.0
accepted some of these and ran them repeatedly. Write `every …` for an
interval (`"every 2 hours"`), or send a `hot:schedule:new` event to run a
function once (see [Dynamic Schedules](#dynamic-schedules)).

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
| `args` | `Vec` | No | Arguments the function is called with. Defaults to `[]` when omitted/null. |
| `schedule` | `Str` or `DateTime` | Yes | When to run (one-time or recurring expression) |

A schedule is one call: a function, a time and its arguments. Two schedules of
one function at one time both fire, as long as their `args` differ. Sending the
same call again doesn't add a schedule: the handler's run returns the ID of the
one it repeats, so a redelivered event or a retried run can't arm it twice. A
cancelled schedule is armed again this way. A one-time schedule that has already
fired isn't, so a late repeat can't run it a second time.

`send` returns an `EventInfo` for the event it published, not a schedule ID. The
schedule is created later, by the run that handles the event, and its ID is that
run's result. Read it from `GET /v1/events/{event_id}/runs` (the run's `result`,
with `event_id` from the `EventInfo`'s `event.id`) or from the schedule's page
in the Hot App. If the schedule can't be created, that run fails with the reason.

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
// Cancel by schedule ID (the result of the run that handled hot:schedule:new)
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

At least one of `schedule-id` or `fn` must be provided. When both are present,
`schedule-id` is used. A `schedule-id` names a schedule in the sender's
environment. `fn` cancels every active schedule of that function on the build
whose run handles the event, metadata schedules included. The handler's run
returns `true` if it cancelled a schedule. `send` itself returns an `EventInfo`.

Cancelling schedules is useful for:
- Removing pending one-time schedules that are no longer needed
- Disabling recurring schedules without redeploying
- Cleaning up after a user cancels an action

### Dynamic vs Metadata Schedules

| Feature | Metadata Schedule | Dynamic Schedule |
|---------|-------------------|------------------|
| Defined in | Hot code (`meta {schedule: ...}`) | Runtime via `hot:schedule:new` |
| Lifecycle | Tied to build/deployment | Created and cancelled at runtime, and cancelled when a new build is activated |
| One-time support | No | Yes |
| Function called with | One argument describing the schedule that fired | The schedule's `args` |
| Visible in UI | Always | When active |
| Use case | Regular jobs (daily reports, cleanup) | User-triggered, conditional scheduling |

### Dynamic Schedules and Deploys

A dynamic schedule belongs to the build whose run created it. Its arguments were
written for that program, so activating a new build of the project cancels every
active dynamic schedule on the project's other builds, one-time and recurring.
The cancelled schedule records `build replaced by deployment` as its
`deactivation-reason`, and the deploy logs how many it cancelled.

A schedule that has to outlive a deploy needs a record in your own application
state. After each activation, Hot sends a
[`hot:deploy:activated`](/docs/events#deploy-events) event: arm your schedules
again from that record.

```hot
on-deployed meta {on-event: "hot:deploy:activated"}
fn (event) {
  if(eq(event.data.project, "reminders"), rearm-pending-reminders())
}
```

### Example: Scheduled Reminders

Code that sends `hot:schedule:new` doesn't get the schedule's ID back, so this
example keeps its own record of each pending reminder in a
[store](/pkg/hot.dev/hot-std/hot/store). Cancelling deletes the record. The
schedule still fires, finds no record, and sends nothing. The same record is
what a [`hot:deploy:activated`](#dynamic-schedules-and-deploys) handler needs to
arm the reminders again after a deploy.

```hot
::myapp::reminders ns

::store ::hot::store

// Pending reminders, keyed by reminder ID
pending ::store/Map({name: "reminders:pending"})

// Schedule a reminder for later. Returns the reminder's ID.
schedule-reminder fn (user-id: Str, message: Str, delay: Str): Str {
  reminder-id Uuid()
  ::store/put(pending, reminder-id, {user-id: user-id, message: message})

  // Create a one-time schedule. send returns an EventInfo, not a schedule ID.
  send("hot:schedule:new", {
    fn: "::myapp::reminders/send-reminder",
    args: [reminder-id],
    schedule: delay
  })

  reminder-id
}

// Cancel a pending reminder. Returns true if it was still pending.
cancel-reminder fn (reminder-id: Str): Bool {
  ::store/delete(pending, reminder-id)
}

// The actual reminder function (called by the scheduler with the schedule's args)
send-reminder fn (reminder-id: Str) {
  reminder ::store/get(pending, reminder-id)
  if(is-some(reminder), deliver-reminder(reminder-id, reminder))
}

deliver-reminder fn (reminder-id: Str, reminder: Map) {
  user get-user(reminder.user-id)
  send-push-notification(user.device-token, reminder.message)
  ::store/delete(pending, reminder-id)
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
fn (event) {
  import-from-sftp()
}
```

For full retry configuration (attempts, delay, backoff, jitter, limits, and `pending_retry` behavior), see [Retries](/docs/retries).

## How It Works

When a scheduled function's time arrives, the scheduler sends a `hot:schedule` event with the function details. A worker picks up the event and executes the function.

A [dynamic schedule](#dynamic-schedules) calls its function with the `args` it
was created with: `args: ["s-1"]` calls `fn (id: Str)` with `"s-1"`, and
`args: []` calls a function that takes no arguments.

A metadata schedule calls its function with a single argument describing the
schedule that fired:

```hot
nightly-sync meta {schedule: "0 2 * * *"}
fn (event) {
  // event.type          → "hot:schedule"
  // event.schedule_id   → UUID of the schedule that fired
  // event.scheduled_at  → RFC 3339 timestamp of the trigger
  // event.is_backfill   → true when the run makes up for a missed time
  // event.backfilled_at → RFC 3339 timestamp of the backfill, or null
  sync-data()
}
```

Declare the parameter even if you don't use it — the scheduler always passes it.

Scheduled runs are tracked and visible in the Hot App alongside event-triggered and API-triggered runs.

## Missed Runs and Backfill

A recurring schedule's times that pass while the scheduler is down are skipped
by default. With `scheduler.backfill` set to `true` (`--scheduler.backfill true`
on `hot scheduler` or `hot dev`), the scheduler queues the missed times when it
syncs again:

- It counts from the later of the schedule's last run and the scheduler's last
  sync, up to now.
- It queues at most 100 missed times for each schedule. The rest are dropped.
- A schedule that has never run isn't backfilled in its first 5 minutes.
- Each backfilled run of a metadata schedule gets `is_backfill: true`, the
  missed time as `scheduled_at`, and the time it was queued as `backfilled_at`.
  A dynamic recurring schedule is called with its `args` as usual.

One-time schedules don't depend on this setting. One that came due while the
scheduler was down runs when the scheduler is back.

Backfilled runs arrive together, so a handler that must not repeat work should
check `event.is_backfill` or `event.scheduled_at`.

## Schedule Limits

Three settings limit schedules. A plan can make them stricter for an
organization.

| Setting | Default | Limits |
|---------|---------|--------|
| `schedule.min-interval-seconds` | `1` | Shortest gap between two runs of a recurring schedule |
| `schedule.min-delay-seconds` | `0` | Shortest delay before a one-time schedule runs |
| `schedule.max-active-per-org` | `-1` (no limit) | Active schedules in one organization |

A build with a metadata schedule that breaks a limit fails to deploy. A
`hot:schedule:new` that breaks one creates no schedule, and the run that
handled it fails with the reason.

## Testing Schedules

In `hot test`, a delivered `hot:schedule:new` arms the schedule in the test,
and `::hot::test/run-due` calls what is due by the test's clock. See
[Events and schedules](/docs/cli#events-and-schedules) under `hot test`.
