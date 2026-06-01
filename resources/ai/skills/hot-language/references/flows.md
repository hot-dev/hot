# Hot Flows Reference

Flows are control structures in Hot that define execution patterns. They can be standalone expressions or combined with `fn` to create function definitions.

## Flow Types

| Flow | Behavior | Default Return |
|------|----------|----------------|
| `serial` | Sequential execution | Last value |
| `cond` | First matching branch | Matched value |
| `cond-all` | ALL matching branches | Map of results |
| `match` | Pattern match, first | Matched value |
| `match-all` | Pattern match, all | Map of results |
| `parallel` | Concurrent execution | Map of results |

## Serial Flow

Sequential execution, returns last value. This is the implicit default for function bodies.

```hot
// Explicit serial flow
process fn serial (data: Map): Map {
    validated validate(data)
    enriched enrich(validated)
    save(enriched)  // Returns this
}

// Implicit (same behavior)
process fn (data: Map): Map {
    validated validate(data)
    enriched enrich(validated)
    save(enriched)
}
```

## if() vs cond

Hot provides two ways to handle conditionals:

**`if(condition, then, else)`** — Use for simple binary conditions:
```hot
status if(gt(count, 0), "positive", "non-positive")
```

**`cond` flow** — Use for multiple conditions or complex branching:
```hot
status cond {
    lt(count, 0) => { "negative" }
    eq(count, 0) => { "zero" }
    => { "positive" }
}
```

The `if()` function uses lazy arguments, so only the matching branch is evaluated. For more than two branches, use `cond`.

## Cond Flow

Conditional branching. Evaluates conditions top-to-bottom, executes first matching branch.

### As Function Definition

```hot
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    eq(x, 0) => { "zero" }
    => { "positive" }  // Default case (no condition)
}
```

### As Standalone Expression

```hot
process fn (order: Map): Str {
    // Standalone cond within a function body (errors propagate automatically)
    status cond {
        is-empty(order.items) => { err("No items") }
        lt(order.total, 0) => { err("Invalid total") }
        => { "valid" }
    }

    // Continue with status...
    status
}
```

### Branch Syntax

```hot
condition => { result }     // With condition
=> { result }               // Default (always matches)
_ => { result }             // Also valid as default (Rust-style wildcard)
```

`cond` conditions use Hot truthiness: `false`, `null`, and `Err` results are
falsy. Everything else — including `0`, `""`, `[]`, and `{}` — is truthy. An
`Ok(x)` is truthy iff `x` is truthy. This lets you fall through on missing or
failed values:

```hot
display-name cond {
    user.nickname => { user.nickname }     // skipped if null; "" still matches
    user.name => { user.name }
    => { "Anonymous" }
}
```

`cond` does not bind the condition's value. To act on a Result without
auto-unwrapping it, bind first and then check with `is-ok`/`is-err`:

```hot
config fetch-config()
result cond {
    is-ok(config) => { use-config(config) }
    => { use-defaults() }
}
```

When you specifically need to handle empty strings or empty collections, use
`is-empty(...)` rather than relying on truthiness.

## Cond-All Flow

Executes ALL branches whose conditions are true. Returns a Map keyed by branch names.

```hot
process fn (order: Map): Map {
    // All matching branches execute
    effects cond-all {
        order.is-gift => gift { add-gift-wrap(order) }
        order.priority => priority { flag-priority(order) }
        order.notify => notify { send-notification(order) }
        => standard { log-order(order) }
    }
    // Returns: {gift: ..., priority: ..., notify: ..., standard: ...}
    // Only keys for branches that matched
}
```

**Branch names are required** in `cond-all` to key the result map:

```hot
condition => name { result }
=> default-name { result }
```

## Match Flow

Pattern matching on types, enum variants, and literal values. First matching arm wins.

### Type/Variant Arms

```hot
describe fn match (shape: Shape): Str {
    Shape.Circle => { `Circle with radius ${shape.radius}` }
    Shape.Rectangle => { `Rectangle ${shape.width}x${shape.height}` }
    Shape.Point => { "A point" }
}
```

`match` functions may accept extra arguments after the matched value:

```hot
render fn match (shape: Shape, color: Str): Str {
    Shape.Circle => { `${color} circle ${shape.radius}` }
    Shape.Rectangle => { `${color} rectangle ${shape.width}x${shape.height}` }
}
```

### Exhaustiveness

A `match` on a closed `enum` must cover every variant or include a `_` /
bare `=>` default arm. Missing variants produce **`non-exhaustive-match`**
at compile time.

A `match` on an `open enum` MUST include a `_` / bare `=>` default arm,
because new variants can be enrolled at any time via arrows. Missing the
default produces **`open-enum-match-missing-default`**.

```hot
Animal enum open { Dog, Cat }

label fn match (a: Animal): Str {
    Animal.Dog => { "dog" }
    Animal.Cat => { "cat" }
    _ => { "other" }              // required for open enums
}
```

The same exhaustiveness rules apply to **open literal unions** (`type
open "a" | "b"`) — the accumulated member set is unbounded across
declarations, so a `match` against an open literal union also requires
a default arm. The bare `=>` form reads naturally for catch-all fallbacks
in dispatch helpers; `::hot::media/is-image` is a canonical example, where
an open `Media` alias is matched and unknown subtypes fall through to the
default arm.

### Value Arms

Match against literal values: `Int`, `Dec`, `Str`, `Bool`, `Null`, `Vec`, `Map`. Uses exact equality.

```hot
status-message fn match (code: Int): Str {
    200 => { "ok" }
    404 => { "not found" }
    500 => { "server error" }
    _ => { "unknown" }  // bare => also works as default
}
```

### Mixed Arms

Type and value arms can coexist. Evaluated top-to-bottom; first match wins.

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

The match subject can be any expression — it is evaluated once.

```hot
result match length(name) {
    0 => { "empty" }
    5 => { "five chars" }
    => { "other" }
}

result match response.status {
    200 => { "ok" }
    404 => { "not found" }
    => { `error: ${response.status}` }
}
```

### Vec and Map Arms

Match collections by full structural equality. Partial matching is not supported.

```hot
result match coords {
    [0, 0] => { "origin" }
    [1, 0] => { "unit x" }
    => { "other" }
}

result match config {
    {mode: "debug"} => { "debug mode" }
    {mode: "release"} => { "release mode" }
    => { "unknown mode" }
}
```

### As Standalone Expression

```hot
process fn (result: Result): Str {
    message match result {
        Result.Ok => { `Success: ${result}` }
        Result.Err => { `Error: ${result}` }
        _ => { `Unknown: ${result}` }
    }
    message
}
```

### Accessing Matched Values

After matching, access the value directly through the matched variable:

```hot
handle fn (event: Event): Action {
    action match event {
        Event.Click => {
            log(`Clicked at ${event.x}, ${event.y}`)
            Action.Navigate(event.target)
        }
        Event.KeyPress => {
            Action.Input(event.key)
        }
    }
    action
}
```

## Match-All Flow

Pattern matching where ALL matching arms execute. Returns Map keyed by the arm's pattern.

For type arms, the key is the type string (e.g., `"Int"`, `"Shape.Circle"`). For value arms, the key is the value itself (e.g., `200`, `"hello"`).

```hot
Trait enum { Flying, Swimming, Walking }

describe fn match-all (creature: Trait): Str {
    Trait.Flying => { "can fly" }
    Trait.Swimming => { "can swim" }
    Trait.Walking => { "can walk" }
}
// describe(Trait.Flying) returns {"Trait.Flying": "can fly"}
```

### Match-All with Mixed Arms

```hot
x 200
results match-all x {
    200 => { "is 200" }
    Int => { "is integer" }
}
// results: {"200": "is 200", "Int": "is integer"}
```

## Parallel Flow

Executes all branches concurrently. Returns Map keyed by variable names.

### As Function Definition

```hot
fetch-user-data fn parallel (id: Str): Map {
    profile ::http/get(`/users/${id}`)
    posts ::http/get(`/users/${id}/posts`)
    friends ::http/get(`/users/${id}/friends`)
}
// Returns: {profile: ..., posts: ..., friends: ...}
```

Parallel flow respects dependencies. Independent bindings run concurrently, but
a binding that references an earlier value waits for that value:

```hot
enrich-user fn parallel (id: Str): Map {
    user fetch-user(id)
    orders fetch-orders(user.id)
    prefs fetch-preferences(user.id)
    summary build-summary(orders, prefs)
}
```

### As Standalone Expression

```hot
process fn (userId: Str): Map {
    // Parallel fetch within sequential function
    data parallel {
        user fetch-user(userId)
        permissions fetch-permissions(userId)
        preferences fetch-preferences(userId)
    }

    // data is {user: ..., permissions: ..., preferences: ...}
    process-user(data)
}
```

### Error Handling in Parallel

If any branch returns an error, the parallel flow short-circuits:

```hot
fetch-all fn parallel (ids: Vec<Str>): Map {
    a fetch(ids[0])  // If this returns err(...), flow stops
    b fetch(ids[1])
    c fetch(ids[2])
}
```

## Flow Result Shape

Use `All<Vec>` or `All<Map>` annotations to collect all flow results.
Bare `All` is allowed only on natural collect-all forms (`parallel`,
`cond-all`, and `match-all`); use explicit `All<Vec>` or `All<Map>` on
`serial`, `pipe`, `cond`, and `match`.

| Shape | Behavior |
|-------|----------|
| Plain/no annotation | Return single value (default for serial, cond, match) |
| `All<Vec>` | Return results as vector |
| `All<Map>` | Return results as map (default for parallel, cond-all, match-all) |

```hot
// Return parallel results as a vector instead of a map
results: All<Vec> parallel {
    fetch-a()
    fetch-b()
    fetch-c()
}
// Returns: [result-a, result-b, result-c]

// Explicit map shape for named parallel work
result: All<Map> parallel {
    a fetch-a()
    b fetch-b()
}
// Returns: {a: result-a, b: result-b}
```

## Nested Flows

Flows can be nested within each other:

```hot
process fn (request: Request): Response {
    // Outer: parallel for concurrent fetches
    data parallel {
        user fetch-user(request.user-id)
        config fetch-config()
    }

    // Inner: cond for validation
    validated cond {
        is-null(data.user) => { err("User not found") }
        not(data.config.enabled) => { err("Service disabled") }
        => { ok(data) }
    }

    // Inner: match for result handling
    response match validated {
        Result.Ok => { Response.success(validated) }
        Result.Err => { Response.error(validated) }
        _ => { Response.error(validated) }
    }

    response
}
```

## Common Patterns

### Early Return with Cond

```hot
validate fn cond (input: Map): Map {
    is-empty(input.name) => { err("Name required") }
    lt(length(input.name), 2) => { err("Name too short") }
    is-empty(input.email) => { err("Email required") }
    not(contains(input.email, "@")) => { err("Invalid email") }
    => { input }
}
```

### Parallel with Fallback

```hot
fetch-with-fallback fn (id: Str): Data {
    results parallel {
        primary fetch-primary(id)
        backup fetch-backup(id)
    }

    if(is-ok(results.primary), results.primary, results.backup)
}
```

### Match with Exhaustive Patterns

```hot
Direction enum { Up, Down, Left, Right }

move fn match (dir: Direction, pos: Point): Point {
    Direction.Up => { Point({x: pos.x, y: sub(pos.y, 1)}) }
    Direction.Down => { Point({x: pos.x, y: add(pos.y, 1)}) }
    Direction.Left => { Point({x: sub(pos.x, 1), y: pos.y}) }
    Direction.Right => { Point({x: add(pos.x, 1), y: pos.y}) }
}
```
