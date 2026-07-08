<!-- HOT_LANGUAGE_SECTION_START --> hash:1695348114829091571
# Hot Language Instructions for GitHub Copilot

This project uses the **Hot programming language** (`.hot` files). Hot is a functional, expression-based language with automatic parallelization.

## Critical Syntax Rules

**These are non-negotiable - violating them causes parse errors:**

| ❌ Wrong | ✅ Correct | Rule |
|----------|-----------|------|
| `name = "Alice"` | `name "Alice"` | No `=` for assignment |
| `a + b` | `add(a, b)` | No infix operators |
| `a == b` | `eq(a, b)` | Comparison is a function |
| `if (x) { } else { }` | `if(x, then, else)` | No if/else blocks |
| `for x in items` | `map(items, ...)` or `for-each(iter, ...)` | No loops |
| `fn (x)` with space | `fn(x)` no space | Space before `(` = assignment |
| `items.0` | `items[0]` | Array indexing uses brackets |

## File Structure

```hot
::myapp::module ns              // Namespace declaration (required)

// Namespace aliasing: create short aliases
::http ::hot::http
::env ::hot::env

// Import specific items
HttpResponse ::hot::http/HttpResponse

// Variables (no = sign)
api-url ::env/get("API_URL", "https://api.example.com")
max-retries: Int 3

// Functions
get-user fn (id: Str): Result {
    response http-get(`${api-url}/users/${id}`)
    if(is-ok(response), ok(response.body), err("Failed"))
}

// Conditional flow
handle fn cond (status: Int): Str {
    eq(status, 200) => { "success" }
    lt(status, 500) => { "client error" }
    => { "server error" }
}
```

## Function Patterns

```hot
// Overloaded by arity (comma-separated definitions)
process fn
(x: Int): Str { `Int: ${x}` },
(x: Str): Str { `Str: ${x}` }

// Parallel execution
fetch-all fn parallel (urls: Vec<Str>): All<Vec> {
    ::hot::http/get(urls[0])
    ::hot::http/get(urls[1])
}

// Pipe operator - piped value becomes first argument
result 5 |> add(2) |> mul(3)  // = 21
```

## Flows

Flows (`cond`, `cond-all`, `match`, `match-all`, `parallel`, `serial`) are **standalone constructs**. The `fn` keyword modifies a flow to become a function definition.

```hot
// Flow as function definition (fn modifies the flow)
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    => { "positive" }
}

// Standalone flows within function bodies (no fn needed)
process fn (data: Map): Result {
    // cond flow - conditional branching
    validated cond {
        is-empty(data) => { err("Empty") }
        => { ok(data) }
    }

    // parallel flow - concurrent execution
    results parallel {
        fetch-a(data)
        fetch-b(data)
    }

    // cond-all flow - runs ALL matching branches
    effects cond-all {
        data.notify => { send-notification(data) }
        => { log-event(data) }
    }

    ok(results)
}
```

**Flow types:**
- `serial` - Sequential execution (default)
- `cond` - Conditional, first matching branch wins
- `cond-all` - Conditional, ALL matching branches execute
- `match` - Pattern matching on variant types, first matching arm wins
- `match-all` - Pattern matching on variant types, ALL matching arms execute
- `parallel` - Concurrent execution

## Types

```hot
// Struct type (type name IS the constructor)
Person type { name: Str, age: Int }
alice Person({"name": "Alice", "age": 30})

// Custom constructor
Point type fn (x: Int, y: Int): Point {
    Point({"x": x, "y": y})
}

// Type coercion with ->
Date -> Str fn (d: Date): Str { `${d.year}-${d.month}-${d.day}` }

// Built-in types: Str, Int, Dec, Bool, Null, Vec, Map, Fn, Any, Result
```

## Common Functions (instead of operators)

- **Math**: `add(a,b)`, `sub(a,b)`, `mul(a,b)`, `div(a,b)`, `mod(a,b)`
- **Comparison**: `eq(a,b)`, `ne(a,b)`, `lt(a,b)`, `gt(a,b)`, `lte(a,b)`, `gte(a,b)`
- **Logic**: `and(a,b)`, `or(a,b)`, `not(a)`, `if(cond, then, else)`
- **Collections**: `map(coll,fn)`, `filter(coll,fn)`, `reduce(coll,fn,init)`

<!-- HOT_LANGUAGE_SECTION_END -->
