---
description: "Use Hot flows for sequential logic, conditionals, matches, parallel work, and branch-based execution."
---

# Flows

Flows are expressions that control how their contents execute and how their
results are collected. Ordinary function bodies use a serial flow. Conditional,
matching, and parallel flows make alternative execution strategies explicit.

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

**1. With `fn`** — defines a function whose body uses that flow's scheduling
and result rules:

{{snippet:flows#flow-as-modifier}}

**2. As an inline expression** — provides local control flow:

{{snippet:flows#flow-inline}}

The examples below show both approaches.

## Serial Flow (Default)

Without a flow specifier, functions execute sequentially, returning the last value:

{{snippet:flows#serial-basic}}

{{result:flows#serial-basic}}

You can make it explicit with `serial`:

{{snippet:flows#serial-explicit}}

## Parallel Flow

Request dependency-aware concurrency with `parallel`:

{{snippet:flows#parallel-function}}

{{result:flows#parallel-function}}

Parallelism is explicit: Hot never changes an ordinary serial body into a
parallel one. Within a `parallel` flow, dependency scheduling is automatic.

`parallel`, `cond-all`, and `match-all` naturally return all branch results.
Use `All<Map>` or `All<Vec>` when declaring that collected shape explicitly.
A plain annotation such as `: Map` or `: Int` opts a naturally collect-all flow
out of collection and describes its single final value instead.

### When to Use Parallel

Use `parallel` when:
- Operations involve I/O (HTTP, database, file system)
- You want to speed up multiple slow operations

Hot automatically analyzes dependencies and executes in "levels" - variables at the same level run concurrently, but levels execute in order:

```hot
// Parallel with automatic dependency resolution
enrich-user fn parallel (id: Str): All<Map> {
  user ::api/get-user(id)           // Level 0
  orders ::api/get-orders(user.id)  // Level 1 (depends on user)
  prefs ::api/get-prefs(user.id)    // Level 1 (depends on user)
  summary build-summary(orders, prefs) // Level 2 (depends on orders, prefs)
}
// user runs first, then orders+prefs run in parallel, then summary
```

Dependency analysis follows references between Hot bindings. It cannot infer
conflicts in external state. Independent branches that write the same database
row, file, or remote resource may race even when no Hot value connects them.

If one branch fails, the flow fails. Sibling work that already started cannot
be rolled back automatically, so independently scheduled side effects should
be idempotent or explicitly coordinated.

## Conditional Flow

Use `cond` for conditional branching. The first matching condition wins:

{{snippet:flows#cond-classify}}

{{result:flows#cond-classify}}

The `=>` arrow separates the condition from the result. A branch without a condition is the default case.

Conditions are checked for **truthiness**: any value that isn't `false`, `null`, or an Err is considered true — including `0`, `""`, `[]`, and `{}`. The same rule backs `and`, `or`, `not`, and `is-truthy`; test emptiness explicitly with `is-empty`. This means you can use values directly as conditions:

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
at compile time. Union arms (`A | B`) count every variant they name toward
coverage, and an `Any` arm covers everything (it also satisfies the
open-enum default requirement below).

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

### Union Arms

Combine several patterns in one arm with `|` — the arm matches if **any**
atom matches. Atoms are the same pattern forms as single arms: types,
enum variants, literal values, and fully qualified type paths
(`::hot::type/Str`), mixed freely:

```hot
describe fn match (value: Any): Str {
  "" | Null => { "blank" }
  Int | Dec => { "number" }
  "yes" | "y" | true => { "affirmative" }
  => { "other" }
}
```

Enum variants union the same way, and union arms count toward
exhaustiveness — this match is exhaustive without a default arm:

```hot
Shape enum { Circle, Square, Triangle }

classify fn match (s: Shape): Str {
  Shape.Circle | Shape.Square => { "round-ish" }
  Shape.Triangle => { "pointy" }
}
```

Bindings receive the matched value as usual: `Str | Null (v) => { ... }`.

### Optional-Type Sugar

`T?` in a match arm means `T | Null`, exactly as in signatures:

```hot
greet fn (name: Str?): Str {
  match name {
    Str? => { `Hello, ${or(name, "stranger")}` }  // same as Str | Null
    _ => { "Hello, whatever you are" }
  }
}
```

### The Any Pattern

`Any` is the top type: it matches every value. An `Any` arm acts like a
default arm (and satisfies exhaustiveness), but unlike `_` it can carry
a binding:

```hot
kind match value {
  Str => { "string" }
  Any (v) => { `something else: ${v}` }
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

Results are keyed by the arm's pattern; a union arm produces a single
key joining its atoms (e.g. `"Int | Dec"`).

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

Flows are expressions. Combining `fn` with a flow creates a callable function
whose body uses that flow's scheduling and result rules:

```hot
// Function with conditional flow
classify fn cond (x: Int): Str {
  lt(x, 0) => { "negative" }
  => { "positive" }
}

// Inline flow expression inside a function body
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

// Any other type opts a collect-all flow OUT of collection: the
// annotation states the type of the single final value. On
// single-value flows a plain annotation is an ordinary type check.
last: Int parallel {
  a 1
  b 2
}
```

Bare `All` is accepted only on natural collect-all flows (`parallel`,
`cond-all`, and `match-all`). Use `All<Vec>` or `All<Map>` on other flows.

Annotations are not enforced at runtime, but `hot check` reports an
`annotation-mismatch` warning when an annotation names a type the value
can never be (for example `x: Int parallel { ... }` whose final value is
a `Str`).

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

Parallel scheduling is defined by named bindings and their dependencies.
Standalone, unbound expressions do not become collected result slots; bind
every concurrent operation even when using `All<Vec>`. A `fn parallel`
definition can collect unbound body expressions into an explicit `All<Vec>`,
but named bindings work consistently in both forms. Branches complete
independently, so an `Err` remains in its branch's slot rather than cancelling
siblings. It propagates when ordinary code later consumes that result.

Because branches are independent slots, a deep-path assignment into an
existing binding (`st.a.b 99`) is rejected at compile time inside `parallel` —
both the standalone block and `fn parallel` forms: concurrent writes into a
shared root have no defined order. Bind a new name in the branch and merge
after the flow, or use a serial flow. The check does not look inside nested
flows: a `serial { }` block nested in a parallel branch can still write to an
outer binding, but its merge order across branches is undefined — treat that
pattern as unsupported.

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

Flows make Hot's scheduling and result behavior explicit. You always know
whether operations run in sequence, parallel, or conditionally.
