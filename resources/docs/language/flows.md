---
description: "Use Hot flows for sequential logic, conditionals, matches, parallel work, and branch-based execution."
---

# Flows

Flows control how expressions execute. They're one of Hot's most powerful features, enabling parallel execution, conditional branching, and data pipelines.

## Flow Types

| Flow | Description |
|------|-------------|
| `serial` | Execute sequentially (default) |
| `parallel` | Execute concurrently |
| `cond` | First matching branch wins |
| `cond-all` | All matching branches execute |
| `match` | Pattern match on types and values |
| `match-all` | All matching type/value patterns execute |
| `\|>` | Pipe data through transformations |

## Two Ways to Use Flows

Every flow can be used in two ways:

**1. As a function modifier** — defines the entire function's execution model:

{{snippet:flows#flow-as-modifier}}

**2. Inline within any expression** — for local control flow:

{{snippet:flows#flow-inline}}

The examples below show both approaches.

## Serial Flow (Default)

Without a flow specifier, functions execute sequentially, returning the last value:

{{snippet:flows#serial-basic}}

{{result:flows#serial-basic}}

You can make it explicit with `serial`:

{{snippet:flows#serial-explicit}}

## Parallel Flow

Execute expressions concurrently with `parallel`:

{{snippet:flows#parallel-function}}

{{result:flows#parallel-function}}

This is much faster than sequential execution when operations are independent.

### When to Use Parallel

Use `parallel` when:
- Operations involve I/O (HTTP, database, file system)
- You want to speed up multiple slow operations

Hot automatically analyzes dependencies and executes in "levels" - variables at the same level run concurrently, but levels execute in order:

```hot
// Parallel with automatic dependency resolution
enrich-user fn parallel (id: Str): Map {
  user ::api/get-user(id)           // Level 0
  orders ::api/get-orders(user.id)  // Level 1 (depends on user)
  prefs ::api/get-prefs(user.id)    // Level 1 (depends on user)
  summary build-summary(orders, prefs) // Level 2 (depends on orders, prefs)
}
// user runs first, then orders+prefs run in parallel, then summary
```

## Conditional Flow

Use `cond` for conditional branching. The first matching condition wins:

{{snippet:flows#cond-classify}}

{{result:flows#cond-classify}}

The `=>` arrow separates the condition from the result. A branch without a condition is the default case.

Conditions are checked for **truthiness**: any value that isn't `false` or `null` is considered true. This means you can use values directly as conditions:

{{snippet:flows#cond-truthy}}

{{result:flows#cond-truthy}}

### Multiple Conditions

{{snippet:flows#cond-grade}}

{{result:flows#cond-grade}}

### Named Branches

Give branches names for debugging or result identification:

{{snippet:flows#cond-named-branches}}

{{result:flows#cond-named-branches}}

### Complex Conditions

Any expression that returns a boolean works:

```hot
validate fn cond (user: Map): Result {
  is-null(user.email) => { err("Email required") }
  not(valid-email(user.email)) => { err("Invalid email") }
  lt(length(user.password), 8) => { err("Password too short") }
  => { ok(user) }
}
```

## Conditional-All Flow

Use `cond-all` when you want **all** matching branches to execute:

{{snippet:flows#cond-all-discounts}}

{{result:flows#cond-all-discounts}}

### Use Cases for cond-all

- Applying multiple rules/transformations
- Collecting all matching categories
- Running side effects for all matches
- Validation that collects all errors

{{snippet:flows#cond-all-validate}}

{{result:flows#cond-all-validate}}

## Match Flow

Use `match` to pattern match on types and literal values. The first matching pattern wins:

{{snippet:flows#match-direction-enum}}

```hot
describe fn match (dir: Direction): Str {
  Direction.Up => "Going up"
  Direction.Down => "Going down"
  Direction.Left => "Going left"
  Direction.Right => "Going right"
}

up Direction.Up
describe(up)  // → "Going up"
```

### Exhaustiveness

A `match` on a closed `enum` must cover every variant or include a `_` /
bare `=>` default arm. Missing variants produce **`non-exhaustive-match`**
at compile time.

A `match` on an `open enum` MUST include a `_` / bare `=>` default arm,
because additional variants can be enrolled later via
`Source -> Enum.Variant` arrows. Missing the default produces
**`open-enum-match-missing-default`**.

```hot
Animal enum open { Dog, Cat }

label fn match (a: Animal): Str {
  Animal.Dog => { "dog" }
  Animal.Cat => { "cat" }
  _ => { "other" }              // required for open enums
}
```

### Value Matching

Match against literal values — `Int`, `Dec`, `Str`, `Bool`, `Null`, `Vec`, `Map`:

```hot
status-message fn match (code: Int): Str {
  200 => { "ok" }
  404 => { "not found" }
  500 => { "server error" }
  => { "unknown" }
}
```

### Mixed Type and Value Arms

Type and value arms can coexist. Arms are evaluated top-to-bottom; first match wins:

```hot
describe fn match (value: Any): Str {
  null => { "null" }
  0 => { "zero" }
  "" => { "empty string" }
  Int => { "integer" }
  Str => { "string" }
  => { "other" }
}
```

### Expression Subjects

The match subject can be any expression — it is evaluated once:

```hot
result match length(name) {
  0 => { "empty" }
  5 => { "five chars" }
  => { "other" }
}
```

### Vec and Map Arms

Match collections by full structural equality:

```hot
result match coords {
  [0, 0] => { "origin" }
  [1, 0] => { "unit x" }
  => { "other" }
}
```

### Inline Match

Use `match` inline to branch on a value:

```hot
result get-result()

message match result {
  Result.Ok => `Success: ${result}`
  Result.Err => `Error: ${result}`
}
```

### Type-Level Matching

Match any variant of a type:

```hot
// Matches any Result variant
is-result match value {
  Result => true
  => false
}
```

### Match Functions with Extra Arguments

Match flow functions can have additional arguments beyond the matched value:

{{snippet:flows#match-direction-enum}}

{{snippet:flows#match-describe-direction}}

{{result:flows#match-describe-direction}}

## Match-All Flow

Use `match-all` when you want **all** matching patterns to execute:

{{snippet:flows#match-all-trait-enum}}

{{snippet:flows#match-all-describe-traits}}

{{result:flows#match-all-describe-traits}}

### Match Result Shape

Like other flows, match supports `All` annotations to collect branch results.
Use plain return types for single values and `All<Vec>` / `All<Map>` for
collected results.

Bare `All` is allowed only where the language already has a natural collect-all
default: `parallel`, `cond-all`, and `match-all`. On `serial`, `pipe`, `cond`,
and `match`, use explicit `All<Vec>` or `All<Map>` to make the collection shape
clear.

```hot
// match defaults to one winning result
// match-all defaults to All<Map> (keyed by branch)

// Get results as vector
traits: All<Vec> match-all creature {
  Trait.Flying => "flies"
  Trait.Swimming => "swims"
}
```

## Pipe Flow

The pipe `|>` chains transformations. The piped value becomes the **first argument** of the next function:

```hot
result 5 |> add(2) |> mul(3)
// 5 |> add(2) → add(5, 2) → 7
// 7 |> mul(3) → mul(7, 3) → 21
```

### Collection Pipelines

Pipes shine with collection operations:

```hot
// Using % placeholder lambdas for concise single-param operations
result [1, 2, 3, 4, 5]
  |> map(mul(%, 2))                    // [2, 4, 6, 8, 10]
  |> filter(gt(%, 5))                  // [6, 8, 10]
  |> reduce((a, x) { add(a, x) }, 0)  // 24 (multi-param: use explicit lambda)
```

### Pipes and `%` — How They Compose

Two rules govern how pipes and `%` interact:

1. **The pipe supplies the piped value as the first argument.** Don't add `%` for that — a pipe stage is already a partial call:

```hot
result 10
  |> mul(2)     // mul(10, 2) → 20
  |> add(5)     // add(20, 5) → 25
```

2. **`%` creates a lambda only inside an argument that expects a function** — the higher-order arguments of `map`, `filter`, `reduce`, and friends:

```hot
[1, 2, 3] |> map(mul(%, 2))    // % is each element → [2, 4, 6]
```

A bare `%` in a pipe stage that isn't a function-typed argument is a compile error:

```hot
10 |> mul(%, 2)
// error: Placeholder `%` has no enclosing parameter slot of type `Fn`
// to bind to. The pipe already passes 10 as the first argument — write
// `10 |> mul(2)` instead.
```

When you need a lambda where Hot wouldn't create one automatically, mark the boundary explicitly with `%(expr)` — see [Explicit Lambda Boundary](/docs/language/functions#explicit-lambda-boundary).

### Real-World Pipeline

```hot
process-users fn (users: Vec<Map>): Vec<Str> {
  users
    |> filter(%.active)
    |> map(%.email)
    |> filter(ends-with(%, "@company.com"))
    |> map(lowercase(%))
}
```

## Combining Flows

Use flows within function bodies:

{{snippet:flows#combining-flows}}

## Flow vs Function

Flows are **part of functions**, not standalone. The `fn` keyword combined with a flow creates a function:

```hot
// Function with conditional flow
classify fn cond (x: Int): Str {
  lt(x, 0) => { "negative" }
  => { "positive" }
}

// Standalone flow (inside a function body)
process fn (data: Map): Result {
  result cond {
    is-null(data) => { err("No data") }
    => { ok(data) }
  }
  result
}
```

## Flow Result Shape

Flow result shape controls whether a flow returns its single produced value or
all produced values. Use a plain type annotation for the single value case and
`All<Vec>` / `All<Map>` when you want a collected result:

```hot
// Single value (the default for serial, cond, match, and pipe)
result: Int serial {
  a 1
  b 2
}

// All values as a vector
values: All<Vec> serial {
  a 1
  b 2
}

// All values as a map keyed by branch or variable name
data: All<Map> parallel {
  user ::api/get-user(id)
  orders ::api/get-orders(id)
}
```

Bare `All` is accepted only on natural collect-all flows (`parallel`,
`cond-all`, and `match-all`). Use `All<Vec>` or `All<Map>` on other flows.

### Default Flow Shapes

Each flow type has a sensible default:

| Flow | Default | Behavior |
|------|---------|----------|
| `serial` | Single value | Returns the last expression's value |
| `parallel` | `All<Map>` | Returns all results as a map keyed by variable name |
| `cond` | Single value | Returns the matching branch's value |
| `cond-all` | `All<Map>` | Returns all matching results as a map keyed by branch name |
| `match` | Single value | Returns the matching arm's value |
| `match-all` | `All<Map>` | Returns all matching results as a map keyed by pattern |
| `\|>` (pipe) | Single value | Returns the final piped value |

### Explicit Result Shapes

Override the default when you need different results:

```hot
// Parallel defaults to All<Map>
data parallel {
  user ::api/get-user(id)
  orders ::api/get-orders(id)
  prefs ::api/get-prefs(id)
}
// => {user: ..., orders: ..., prefs: ...}

// Bare All is accepted on collect-all flows and keeps the natural map shape
data: All parallel {
  user ::api/get-user(id)
  orders ::api/get-orders(id)
}
// => {user: ..., orders: ...}

// Parallel with All<Vec> - get results as a vector
values: All<Vec> parallel {
  a fetch-a()
  b fetch-b()
  c fetch-c()
}
// => [<a-result>, <b-result>, <c-result>]

// cond-all defaults to All<Map>
results cond-all {
  check-a() => a { "A passed" }
  check-b() => b { "B passed" }
  check-c() => c { "C passed" }
}
// => {a: "A passed", c: "C passed"} (if A and C pass)

// cond-all with All<Vec> - collect as vector (no branch names)
discounts: All<Vec> cond-all {
  is-member => { "10% off" }
  gt(total, 100) => { "Free shipping" }
  has-coupon => { "Coupon applied" }
}
// => ["10% off", "Free shipping"] (if member with $150 order, no coupon)

// Pipe with All<Vec> - collect all intermediate values
steps: All<Vec> 5 |> add(2) |> mul(3)
// => [5, 7, 21]
```

## Summary

| Flow | Use When |
|------|----------|
| `serial` | Sequential execution (default) |
| `parallel` | Concurrent execution with automatic dependency resolution |
| `cond` | Choose one branch based on conditions |
| `cond-all` | Execute all matching branches |
| `match` | Pattern match on types and values |
| `match-all` | Execute all matching type/value patterns |
| `\|>` | Chain transformations on data |

Flows make Hot's execution model explicit. You always know whether operations run in sequence, parallel, or conditionally.
