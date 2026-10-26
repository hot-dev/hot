---
description: "Understand Hot Result values, automatic wrapping and unwrapping, Result parameters, lazy arguments, short-circuiting, and fail handling."
---

# Error Handling

Operations that can fail return `Result` values. Hot makes working with Results ergonomic through **automatic wrapping**, **automatic unwrapping**, and **`Result` parameters**.

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

### Binding vs Consuming a Result

Merely binding a `Result` preserves the tagged value. Propagation happens when
ordinary code consumes it:

```hot
result fetch-user(id) // Result remains intact in this binding
valid is-ok(result)   // inspects the Result without consuming it
page render(result)   // Ok unwraps; Err halts and propagates here
```

Ordinary function arguments, template interpolation, and ordinary field or
index access are consumption boundaries. A parameter declared `Result`
receives the Result without consuming it, which is how `is-ok` and `is-err`
inspect the wrapper (see [Accepting a Result](#accepting-a-result)). `if-ok` and `if-err` pass the selected variant's
unwrapped payload to a handler, then return a Result; the unmatched variant
passes through unchanged. `match` also handles Result variants explicitly, as
described in
[Checking Results Explicitly](#checking-results-explicitly).

### Dot Access on Results

Auto-unwrapping extends to field access. Dot access on an `Ok` Result reads fields from the **payload**, so you never need to unwrap before drilling in:

```hot
response http-get("https://api.example.com/user/1")  // returns a Result
name response.body.name   // reads .body.name from the Ok payload
```

If the Result is an `Err`, the dot access halts execution at that point — the same propagation rule as passing an Err to a function. Use `is-ok`, `is-err`, or `match` when code needs to inspect the Result without consuming it.

## Checking Results Explicitly

Use `is-ok` and `is-err` to inspect Results without triggering automatic unwrapping:

{{snippet:errors#is-ok-check}}

{{result:errors#is-ok-check}}

These functions declare a `Result` parameter, so the Result reaches them intact. They check the value their argument produces; they don't catch a halt while it is computed (see [Checks Never Catch Halts](#checks-never-catch-halts)).

You can also use `match` for pattern matching on Result variants:

```hot
result safe-divide(20, 4)

message match result {
  Result.Ok => `Success: ${result}`
  Result.Err => `Error: ${result}`
}
```

The `match` chooses an arm using the `Result.Ok` or `Result.Err` tag. Within the
selected arm, `result` evaluates to that variant's payload.

## Accepting a Result

A parameter declared `Result` receives its argument as it is, an `Err`
included. Any other parameter consumes its argument: an `Ok` unwraps, and an
`Err` halts before the function body runs. This holds however the function is
called: directly, through a higher-order function such as `map`, or with
`call`.

```hot
// ❌ An `Any` parameter consumes its argument: an Err halts at the call
describe fn (r: Any): Str {
  if(is-err(r), "failed", "succeeded")   // never reached for Err values
}

describe(err("boom"))   // Runtime error: Result error: boom

// ✅ A `Result` parameter receives the Result as it is
describe fn (r: Result): Str {
  if(is-err(r), "failed", "succeeded")
}

describe(err("boom"))   // "failed"
describe(ok(42))        // "succeeded"
describe(42)            // "succeeded": any value is an implicit Ok
```

This composes with `OnErr.Preserve` (see Pattern 5) to classify per-item outcomes without halting the batch:

```hot
results map(items, process-item(%), OnErr.Preserve)  // Errs stay as values
labels map(results, describe)                        // ["succeeded", "failed", ...]
```

A lambda that receives preserved Errs declares its parameter the same way:
`map(results, (r: Result) { ... })`. If a halt fires when you hand a preserved
`Result.Err` to a function, the fix is almost always declaring the receiving
parameter `Result`.

## Lazy Arguments

A parameter marked `lazy` defers its argument: the expression runs when the
function forces it with `do`, if it does, exactly as if it were written at
that point. Laziness changes only when an argument is evaluated. An `Err` value
comes back from `do` as a value, and an `Err` consumed while computing it halts
as it would anywhere else.

```hot
// The if function declares lazy parameters for its branches
if fn cond (pred: Result, lazy then: Any, lazy else: Any): Any {
  pred => { do then }
  => { do else }
}
```

`if` declares its condition `Result`, so an `Err` condition is falsy instead
of halting. Only the selected branch runs.

## Checks Never Catch Halts

`is-err`, `if-err`, and the other Result checks look at a value. They don't
catch a halt: `fail()`, `cancel()`, or an `Err` consumed while computing their
argument halts the same as anywhere else. So checking a result directly and
binding it first always agree:

```hot
is-err(load-user(42))   // true when load-user returns an Err

user load-user(42)      // the same Err, bound
is-err(user)            // true
```

When `load-user` halts instead, for example by reading a field of a failed
response, both forms halt. To make a failure checkable, return it as an `Err`
instead of consuming it, for example with `if-ok` or `match`:

```hot
load-user fn (id: Int): Any {
  if-ok(fetch-user(id), (response) { response.body.user })
}
```

To survive halts in code you don't control, run it as a task (Pattern 7).

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

### Pattern 4: Fail on Broken Invariants

Use `fail` to declare that the current run or task hit a bug or broken
invariant and must stop. This is different from returning a normal domain
`err(...)` value: expected failures — bad input, a refused connection, a
query error — should be `err(...)` values the caller can branch on, while
`fail()` and `cancel()` halt execution and surface at the run or task
boundary (the `run:fail` event, or `status: "failed"` on the `TaskResult`
returned by `::hot::task/await`).

```hot
apply-migration fn (db, version: Int) {
  if(lt(version, current-version(db)),
    fail("migration version went backwards", {version: version}),
    run-migration(db, version))
}
```

### Pattern 5: Preserve Domain Errors in Map-Shaped Calls

Eligible higher-order functions force a normal `Result.Err` by default. Pass
`OnErr.Preserve` when you intentionally want to keep per-item domain errors as
values in the result:

{{snippet:errors#preserve-domain-errors}}

`OnErr` applies only to normal `err(...)` / `Result.Err(...)` values. It does
not catch `fail()`, `cancel()`, or hard runtime errors.

### The Error Payload Convention

Keep payloads in one of two shapes so error text survives every hop:

- **Simple:** a plain `Str` — `err("connection refused")`.
- **Structured:** a Map with a `message` field plus any structured
  fields — `err({message: "pg: relation missing", code: "42P01"})`.

The auto-unwrap halt reads `message` (then `msg`) from Map payloads, and
`err-message(result)` extracts readable text from any shape — including
a halt's `Failure` payload — so handlers never hand-roll extraction:

```hot
conn ::pg/connect(opts)
if-err(conn, (e) { println(`db down: ${err-message(e)}`) })
```

### Pattern 6: Chain Fallible Steps

`if-ok` flat-maps: the handler receives the Ok value, its return passes
through unchanged, and an `Err` short-circuits out. Use it to chain steps
where each depends on the previous one succeeding:

{{snippet:errors#chain-if-ok}}

The first failing step becomes the whole chain's return value, as a single
well-formed `Err`.

### Pattern 7: Supervise Untrusted Work with Tasks

There is no `catch` in Hot. Code that must survive a `fail()` in work it
does not control — arbitrary user callbacks, independent jobs — runs that
work as a task. A halt inside the task never propagates to the caller; it
surfaces as data on the awaited result:

```hot
info ::hot::task/start(::myapp/risky-job, args)
result ::hot::task/await(info.id)
if(eq(result.status, "failed"),
  record-error("job", result.result),
  use-value(result.result))
```

This server-side wait is appropriate because the same Hot execution consumes
the result. When a client starts independent background work, return `info.id`
instead and use the SDK task waiter. That waiter follows the durable task state
without keeping the originating run alive.

> **Note:** `::hot::lang/try` and `::hot::lang/try-call` were removed in Hot
> 2.6.0. Old code that wrapped calls in `try` to detect failures should
> branch on the returned `Result` directly (Pattern 2); fan-out loops that
> used `try` for isolation should pass `OnErr.Preserve` (Pattern 5).

## Summary

- Use `Result.Ok(value)` or `ok(value)` and `Result.Err(message)` or `err(message)` to create Results
- Binding a Result preserves it; ordinary consumption triggers unwrapping or propagation
- Results **auto-unwrap** when passed to functions or used in templates
- Err Results **automatically fail** at point of use, carrying the payload's message—no explicit handling needed
- Use `is-ok(result)` and `is-err(result)` to check without triggering auto-unwrap
- Use `if-ok` to chain fallible steps; an `Err` short-circuits the chain
- Use `OnErr.Preserve` with eligible map-shaped APIs when you intentionally want to keep domain errors as values
- Use `fail()` / `cancel()` for bugs and broken invariants, not ordinary recoverable domain errors
- Supervise untrusted work with a task boundary; await it in Hot only when the same execution needs the result
- Return independent task ids to clients and wait with the SDK task resource
- Use `match` for pattern matching on `Result.Ok` and `Result.Err` variants
- Dot access on Results automatically accesses fields within the payload: `result.name`
- Declare a parameter `Result` to receive a Result without consuming it; any other parameter consumes it, however the function is called
- `lazy` only defers evaluation; it gives `if`, `and`, and `or` their short-circuiting
- Result checks look at values and never catch a halt: `fail()`, `cancel()`, or a consumed `Err`
- Most code can ignore error handling; errors propagate automatically
