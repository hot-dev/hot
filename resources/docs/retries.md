# Retries

Hot supports automatic retries for:

- **Event handlers** (`meta {on-event: "...", retry: ...}`)
- **Scheduled functions** (`meta {schedule: "...", retry: ...}`)

Retries are configured with the `retry` metadata field.

> `retry` does **not** apply to synchronous request/response surfaces like MCP tool calls and webhook invocations.

## Retry Metadata

The `retry` field supports two forms.

Simple form:

```hot
retry: 3
```

Full form:

```hot
retry: {
  attempts: 5,
  delay: 5000,
  backoff: "exponential",
  max_delay: 120000,
  jitter: true
}
```

| Field | Description | Default |
|-------|-------------|---------|
| `attempts` (or simple number) | Maximum retry attempts | `0` (disabled) |
| `delay` | Base delay between retries (ms) | `1000` |
| `backoff` | Delay strategy: `"fixed"`, `"exponential"`, `"linear"` | `"fixed"` |
| `max_delay` | Delay cap for backoff strategies (ms) | `300000` |
| `jitter` | Add random jitter (about +/-10%) | `false` |

Backoff formulas:

- `"fixed"`: `delay`
- `"exponential"`: `delay * 2^attempt`
- `"linear"`: `delay * (attempt + 1)`

## How Retries Execute

When a retryable run fails:

1. Hot reads retry config from function metadata.
2. If attempts remain, the run is updated to `pending_retry`.
3. A new run is scheduled at `next_retry_at`.
4. The retry run is linked to the original via `origin_run_id`.
5. Retries stop on success or when attempts are exhausted.

In Hot App, retries are shown with retry badges (for example `↻1`, `↻2`) and linked run history.

## Limits and Clamping

Retry values are clamped to configured platform limits:

| Setting | Environment Variable | Default |
|---------|----------------------|---------|
| Max attempts | `HOT_RETRY_MAX_ATTEMPTS` | `10` |
| Max delay | `HOT_RETRY_MAX_DELAY_MS` | `3600000` (1 hour) |
| Default delay | `HOT_RETRY_DEFAULT_DELAY_MS` | `1000` |

Values below minimum delay are raised, and values above configured limits are capped.
