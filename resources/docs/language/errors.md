---
description: "Understand Hot Result values, automatic wrapping and unwrapping, lazy result checks, short-circuiting, and fail handling."
---

# Error Handling

Operations that can fail return `Result` values. Hot makes working with Results ergonomic through **automatic wrapping**, **automatic unwrapping**, and **lazy argument evaluation**.

## The Result Type

A `Result` represents either success (`Ok`) or failure (`Err`):

```hot
Result enum {
  Ok(Any),
  Err(Any)
}
```

**Return values are automatically wrapped** in `Result.Ok`, so you typically only need `err()` to signal failures:

{{snippet:errors#safe-divide}}

Many core functions return Results implicitly—HTTP calls, file operations, parsing, and other fallible operations.

## Automatic Unwrapping

When you use a `Result` value as a function argument or interpolate it in a template, Hot automatically handles it:

- **Ok Result**: Unwraps to the inner value
- **Err Result**: Immediately halts execution

```hot
// HTTP functions return Results automatically
response http-get("https://api.example.com/user/1")
name response.body.name  // Auto-unwraps the Result
greeting `Hello, ${name}!`
```

If the HTTP call failed, execution halts at the point of use—you don't need explicit error handling on every line. Errors automatically propagate up.

> **Note:** Function return type annotations specify the expected **success type**, not `Result`. The Result wrapper is implicit for any operation that can fail.

### Dot Access on Results

Auto-unwrapping extends to field access. Dot access on an `Ok` Result reads fields from the **payload**, so you never need to unwrap before drilling in:

```hot
response http-get("https://api.example.com/user/1")  // returns a Result
name response.body.name   // reads .body.name from the Ok payload
```

If the Result is an `Err`, the dot access halts execution at that point — the same propagation rule as passing an Err to a function.

## Checking Results Explicitly

Use `is-ok` and `is-err` to inspect Results without triggering automatic unwrapping:

{{snippet:errors#is-ok-check}}

{{result:errors#is-ok-check}}

These functions receive the Result as a **lazy argument**, which prevents automatic unwrapping during the check.

You can also use `match` for pattern matching on Result variants:

```hot
result safe-divide(20, 4)

message match result {
  Result.Ok => `Success: ${result}`
  Result.Err => `Error: ${result}`
}
```

## Lazy Arguments and Result Checking

When a function argument is marked `lazy`, it isn't evaluated until explicitly requested. This is how Hot enables safe Result inspection.

```hot
// The if function uses lazy arguments
if fn cond (pred: Any, lazy then: Any, lazy else: Any): Any {
  pred => { do then }
  => { do else }
}
```

For lazy arguments, Result checking is **suppressed** during evaluation. This means:

1. You can pass expressions that produce Results
2. The Result won't auto-unwrap (or fail) until `do` evaluates it
3. Functions like `is-ok` and `is-err` can safely receive and inspect Results

```hot
// Safe division that returns a Result
safe-divide fn (a, b) {
  if(eq(b, 0), err("Division by zero"), ok(div(a, b)))
}

// is-ok receives the Result without triggering auto-unwrap
result safe-divide(10, 0)
if(is-ok(result),
  `Result: ${result}`,
  "Cannot divide by zero")  // This branch runs
```

### Writing Your Own Result-Inspecting Functions

The same rule applies to your own functions. A regular parameter auto-unwraps its argument, so passing an `Err` Result to it triggers the halt **before your function body runs**. Mark the parameter `lazy` and evaluate it with `do` — inside a lazy context, `do` preserves the Result instead of unwrapping it:

```hot
// ❌ Regular parameter: an Err argument halts before the body executes
describe fn (r: Any): Str {
  if(is-err(r), "failed", "succeeded")   // never reached for Err values
}

describe(err("boom"))   // Runtime error: boom

// ✅ lazy parameter + do: receives and inspects the Result safely
describe fn (lazy r: Any): Str {
  v do r
  if(is-err(v), "failed", "succeeded")
}

describe(err("boom"))   // "failed"
describe(ok(42))        // "succeeded"
```

This composes with `OnErr.Preserve` (see Pattern 5) to classify per-item outcomes without halting the batch:

```hot
results map(items, process-item(%), OnErr.Preserve)  // Errs stay as values
labels map(results, describe(%))                     // ["succeeded", "failed", ...]
```

If a halt fires when you hand a preserved `Result.Err` to a helper function, the fix is almost always marking the receiving parameter `lazy`.

## Short-Circuit Evaluation

Lazy arguments also enable short-circuit evaluation for `and` and `or`:

{{snippet:errors#short-circuit}}

## Error Handling Patterns

### Pattern 1: Let It Fail

For many cases, just use Results directly. Errors propagate automatically:

```hot
main fn () {
  user fetch-user(id)        // Auto-unwraps or fails
  posts fetch-posts(user.id) // Auto-unwraps or fails
  render-page(user, posts)   // Only runs if both succeeded
}
```

### Pattern 2: Check and Handle

When you need to handle errors explicitly:

```hot
result fetch-user(id)
if(is-ok(result),
  render-profile(result),
  render-error-page(result))
```

Or use `match` for cleaner syntax:

```hot
result fetch-user(id)
match result {
  Result.Ok => render-profile(result)
  Result.Err => render-error-page(result)
}
```

### Pattern 3: Default Values

Provide fallbacks for failures:

{{snippet:errors#default-value}}

### Pattern 4: Fail with Context

Use `fail` to declare that the current run or task failed. This is different
from returning a normal domain `err(...)` value: `fail()` and `cancel()` stop
execution by default and are only caught at an explicit boundary such as `try`.

```hot
validate fn (data) {
  if(is-empty(data.email),
    fail("Email is required", {field: "email"}),
    data)
}
```

### Pattern 5: Preserve Domain Errors in Map-Shaped Calls

Eligible higher-order functions force a normal `Result.Err` by default. Pass
`OnErr.Preserve` when you intentionally want to keep per-item domain errors as
values in the result:

{{snippet:errors#preserve-domain-errors}}

`OnErr` applies only to normal `err(...)` / `Result.Err(...)` values. It does
not catch `fail()`, `cancel()`, or hard runtime errors.

### Pattern 6: Rare Halt Containment

Most Hot code should not use `try`. Normal error handling uses `Result` values
and Hot's auto-unwrapping rules, while `fail()` and `cancel()` usually mean the
current run or task should stop.

Use `::hot::lang/try` only at an intentional boundary where one failure must be
caught, such as a scheduled fan-out loop where one failed item must be recorded
without stopping the rest of the batch:

{{snippet:errors#try-contain-halt}}

`try` returns a normal `Result`: `Result.Ok(value)` on success and
`Result.Err(payload)` on failure. Because it catches halts that would otherwise
stop the run or task, it should be uncommon in application logic.

> **Note:** You may also encounter `::hot::lang/try-call` in compiler hints and
> older code — it's the legacy map-shaped variant of the same halt boundary.
> Prefer `try` in new code.

## Summary

- Use `Result.Ok(value)` or `ok(value)` and `Result.Err(message)` or `err(message)` to create Results
- Results **auto-unwrap** when passed to functions or used in templates
- Err Results **automatically fail** at point of use—no explicit handling needed
- Use `is-ok(result)` and `is-err(result)` to check without triggering auto-unwrap
- Use `OnErr.Preserve` with eligible map-shaped APIs when you intentionally want to keep domain errors as values
- Use `fail()` / `cancel()` for explicit run/task termination, not ordinary recoverable domain errors
- Reach for `try` only in rare cases where a `fail()`, `cancel()`, or runtime halt must be caught at a boundary
- Use `match` for pattern matching on `Result.Ok` and `Result.Err` variants
- Dot access on Results automatically accesses fields within the payload: `result.name`
- **Lazy arguments** suppress Result checking, enabling safe inspection and short-circuit evaluation
- Most code can ignore error handling; errors propagate automatically
