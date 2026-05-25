# Hot Error Handling Reference

Hot makes error handling ergonomic through automatic wrapping, automatic unwrapping, and lazy argument evaluation.

## The Result Type

A `Result` represents either success (`Ok`) or failure (`Err`):

```hot
Result enum {
    Ok(Any),
    Err(Any)
}
```

## Creating Results

```hot
success ok(42)
failure err("Something went wrong")
```

## Return Type Annotations

**Important:** Function return type annotations specify the expected **success type**, not `Result`. The Result wrapper is implicit:

```hot
// Return type is Int (the Ok value), not Result
safe-divide fn (a: Int, b: Int): Int {
    if(eq(b, 0), err("Division by zero"), div(a, b))
}
```

Successful return values are automatically wrapped in `Result.Ok`. You usually
only need to write `err(...)` for expected failures; writing `ok(...)` is valid
but often unnecessary.

## Automatic Unwrapping

When you use a `Result` value as a function argument or in template interpolation, Hot automatically handles it:

- **Ok Result**: Unwraps to the inner value
- **Err Result**: Immediately halts execution and propagates the error

```hot
// HTTP functions return Results automatically
response ::http/get("https://api.example.com/user/1")
name response.body.name  // Auto-unwraps the Result
greeting `Hello, ${name}!`
```

If the HTTP call fails, execution halts at the point of use—no explicit error handling needed. Errors propagate automatically.

## Checking Results Explicitly

Use `is-ok` and `is-err` to inspect Results without triggering auto-unwrapping:

```hot
result safe-divide(10, 0)

if(is-ok(result),
    `Result: ${result}`,
    "Cannot divide by zero")
```

These functions receive the Result as a **lazy argument**, which prevents auto-unwrapping during the check.

## Pattern Matching on Results

Use `match` for cleaner handling:

```hot
result fetch-user(id)

message match result {
    Result.Ok => { `Found: ${result.name}` }
    Result.Err => { `Error: ${result}` }
    _ => { `Unknown: ${result}` }
}
```

Inside a `Result.Ok` or `Result.Err` match arm, using the matched variable reads
the payload. Dot access also reaches into Ok payloads (`result.name`) without
manual `$val` handling.

> Closed-enum exhaustiveness: a `match` typed against `Result` requires a `_`
> default arm because the runtime carries internal `OkVal`/`ErrVal` variants. If
> the inferred subject type is narrower (e.g., the value of a known
> Result-returning function), the `_` may not be required, but adding it is
> always safe.

## Lazy Arguments

Arguments marked `lazy` aren't evaluated until explicitly needed. This enables:

1. **Safe Result inspection** — `is-ok` and `is-err` can receive Results without triggering auto-unwrap
2. **Short-circuit evaluation** — `and` and `or` don't evaluate unused branches
3. **Deferred execution** — Expensive computations only run when needed

### How if() Works

The `if` function uses lazy arguments:

```hot
// Both 'then' and 'else' are lazy - only one is evaluated
if fn cond (pred: Any, lazy then: Any, lazy else: Any): Any {
    pred => { do then }
    => { do else }
}
```

Use `do` to evaluate a lazy argument:

```hot
maybe-run fn (should-run: Bool, lazy action: Any): Any {
    if(should-run, do action, null)
}
```

### Short-Circuit Evaluation

```hot
// The second condition only evaluates if the first is true
result and(is-valid(data), is-authorized(user))

// The second option only evaluates if the first is false/null
value or(cached-value, fetch-from-database())
```

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

Or with `match`:

```hot
result fetch-user(id)

match result {
    Result.Ok => { render-profile(result) }
    Result.Err => { render-error-page(result) }
    _ => { render-error-page(result) }
}
```

### Pattern 3: Default Values

Provide fallbacks for failures:

```hot
result fetch-config()

config if(is-ok(result), result, default-config())
```

### Pattern 4: Fail with Context

Use `fail` to halt execution with a custom error. Use `err` for expected,
recoverable failures that callers may inspect as values:

```hot
validate fn (data: Map): Map {
    if(is-empty(data.email),
        fail("Email is required", {field: "email"}),
        data)
}
```

`fail` propagates up until a halt boundary catches it. Most code should let
that happen — Hot's auto-unwrap is the idiomatic error path. Reach for
`::hot::lang/try-call` only as an **escape hatch** when a fan-out loop must
keep going after one item fails, or when you need to catch a runtime panic.
For HTTP-specific transport errors, prefer `::hot::http/try-request` over
wrapping `::hot::http/request` in `try-call`.

### Pattern 5: Result Combinators

Use `if-ok` and `if-err` to selectively transform Ok or Err results. Whichever variant doesn't match passes through unchanged:

```hot
// if-ok: transform the Ok value; Err passes through
fetch-user(id)
    |> if-ok(%.name)        // Ok("Alice") → "Alice's name"; Err → unchanged
    |> if-err("Anonymous")  // Err → "Anonymous"; Ok → unchanged

// Chain for full handling
result fetch-data(id)
    |> if-ok(process-data(%))
    |> if-err(log-and-default(%))
```

## Summary

- Return type annotations use the **success type**, not `Result`
- Results **auto-unwrap** when passed to functions or used in templates
- Err Results **automatically fail** at point of use
- Use `is-ok(result)` and `is-err(result)` to check without triggering auto-unwrap
- Use `if-ok` and `if-err` to selectively transform Ok or Err values
- Use `match` with `Result.Ok` and `Result.Err` patterns
- **Lazy arguments** suppress Result checking, enabling safe inspection
- Most code can ignore error handling; errors propagate automatically
