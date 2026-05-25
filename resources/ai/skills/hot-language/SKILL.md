---
name: hot-language
description: >
  Write code in the Hot programming language. Use when creating, editing,
  or reviewing .hot files. Hot is a functional, expression-based language
  with automatic parallelization, no infix operators, and expression-based
  assignment syntax. IMPORTANT: Hot syntax differs significantly from
  conventional languages - always check these references before writing Hot code.
metadata:
  author: hotdev
  version: "1.0"
  license: Apache-2.0
---

# Hot Language Skill

Hot is a functional, expression-based language with automatic parallelization and type inference. This skill provides detailed reference documentation for the Hot programming language.

> **Note**: For quick syntax rules, see AGENTS.md in the project root. This skill contains extended reference documentation.

## Critical Syntax Rules

**These are non-negotiable - violating them causes parse errors:**

| ❌ Wrong | ✅ Correct | Rule |
|----------|-----------|------|
| `name = "Alice"` | `name "Alice"` | No `=` for assignment |
| `a + b` | `add(a, b)` | No infix operators |
| `a == b` | `eq(a, b)` | Comparison is a function |
| `if (x) { } else { }` | `if(x, then, else)` or `cond` flow | No if/else blocks |
| `for x in items` | `map(items, ...)` or `for-each(iter, ...)` | No loops |
| `items.0` | `items[0]` | Use brackets for array indexing |
| `add (1, 2)` | `add(1, 2)` | No space before `(` when calling |
| `{x}` for a map | `{x,}` or `{x: x}` | Single-key punning needs comma |

## File Structure

Every Hot file must start with a namespace declaration:

```hot
::myapp::users ns

// Namespace aliasing: create short aliases
::http ::hot::http
::env ::hot::env

// Import specific items
HttpResponse ::hot::http/HttpResponse

// Variables (no = sign)
api-url ::env/get("API_URL", "https://api.example.com")
max-retries: Int 3

// Functions (return type is the success type, not Result)
get-user fn (id: Str): Map {
    response ::http/get(`${api-url}/users/${id}`)
    if(is-ok(response), ok(response.body), err("Failed"))
}
```

## Quick Reference

### Variables

```hot
name "Alice"              // Inferred type
count: Int 42             // Explicit type
config {"key": "value"}   // Map literal
query """                 // Block string (indent-aware, no interpolation)
    SELECT * FROM users
    WHERE active = true
    """
tpl ```                   // Block template string (indent-aware + interpolation)
    SELECT * FROM ${table}
    WHERE id = ${id}
    ```
```

### Deep Paths

```hot
user.name "Alice"
user.settings.theme "dark"
servers[0] "api.example.com"
items[] "first"           // append to vector
shopping.items[] "apple"  // append to nested vector
```

### Functions

```hot
// Basic function
greet fn (name: Str): Str {
    `Hello, ${name}!`
}

// Conditional flow function
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    eq(x, 0) => { "zero" }
    => { "positive" }  // Default case
}

// Parallel execution function
fetch-all fn parallel (id: Str): Map {
    user ::http/get(`/users/${id}`)
    posts ::http/get(`/users/${id}/posts`)
}
```

### Flows

Flows are standalone constructs. The `fn` keyword makes them function definitions.

- `serial` - Sequential execution (default)
- `cond` - First matching branch wins
- `cond-all` - ALL matching branches execute, returns Map
- `match` - Pattern match on types/values, first match wins
- `match-all` - Pattern match, ALL matches execute
- `parallel` - Concurrent execution, returns Map

```hot
process fn (data: Map): Map {
    // Standalone cond flow (errors propagate automatically)
    validated cond {
        is-empty(data) => { err("Empty") }
        => { data }
    }

    // Parallel flow
    results parallel {
        fetch-a(validated)
        fetch-b(validated)
    }

    results
}
```

### Types

```hot
// Struct type
User type { id: Str, email: Str, active: Bool }
user User({"id": "1", "email": "a@b.com", "active": true})

// Enum type — `match` must cover every variant or use a `_` default arm
Direction enum { Up, Down, Left, Right }
dir Direction.Up

// Enum with data
Shape enum { Circle(Circle), Point }

// Open enum — extensible via `Source -> Enum.Variant` arrows.
// Match on an open enum REQUIRES a `_` default arm.
Animal enum open { Dog, Cat }
Bird type { species: Str }
Bird -> Animal.Bird               // bodyless arrow enrolls the variant

// Literal union (enum-like, raw values)
Fruit type "apple" | "banana" | "orange"

// Open literal union — extensible by re-declaring at top level.
// Leading `|` makes single-member extensions read naturally.
HttpMethod type open "GET" | "POST" | "PUT" | "DELETE" | "PATCH"
HttpMethod type open | "CONNECT"
HttpMethod type open | "TRACE"

// Type coercion
Date -> Str fn (d: Date): Str { `${d.year}-${d.month}-${d.day}` }
```

### Common Functions

- **Math**: `add`, `sub`, `mul`, `div`, `mod`, `abs`, `round`
- **Comparison**: `eq`, `ne`, `lt`, `gt`, `lte`, `gte`
- **Logic**: `and`, `or`, `not`, `if`
- **Collections**: `map`, `filter`, `reduce`, `first`, `rest`, `concat`, `length`, `get`
- **Strings**: `uppercase`, `lowercase`, `trim`, `split`, `join`
- **Results**: `ok`, `err`, `is-ok`, `is-err`

### Pipe Operator

Piped value becomes the **first** argument:

```hot
result 5 |> add(2) |> mul(3)  // add(5,2)=7, mul(7,3)=21
```

### App and Platform Patterns

```hot
// Function aliases can carry metadata and point at another function.
signup-webhook
meta {webhook: {service: "leads", path: "/signup"}}
handle-signup

// Use HttpRequest for headers/raw body; simple get/post helpers have fixed arity.
HttpRequest ::hot::http/HttpRequest
response ::hot::http/request(HttpRequest({
    method: "POST",
    url: "https://api.example.com/users",
    headers: {Authorization: `Bearer ${api-key}`},
    body: untype(user),
}))
```

## Event Handlers and Schedules

```hot
// Event handler
on-user-created meta {on-event: "user:created"}
fn (event) { send-email(event.data.email) }

// With retry
on-payment meta {on-event: "payment:received", retry: 3}
fn (event) { process(event.data) }

// Scheduled function
daily-sync meta {schedule: "0 2 * * *", retry: {attempts: 5, delay: 10000}}
fn () { sync-data() }
```

## Testing

```hot
test-add meta ["test"]
fn () {
    assert(eq(add(1, 2), 3), "1 + 2 should equal 3")
}
```

Run tests with `hot test`.

## Reference Documentation

This skill includes detailed reference files:

| File | Description |
|------|-------------|
| [references/syntax.md](references/syntax.md) | Complete syntax reference |
| [references/hot-std.md](references/hot-std.md) | Full standard library documentation |
| [references/flows.md](references/flows.md) | Flow patterns, `if()` vs `cond`, result modifiers |
| [references/types.md](references/types.md) | Type system, type coercion |
| [references/error-handling.md](references/error-handling.md) | Auto-unwrapping, lazy arguments, Result patterns |
| [references/sequences.md](references/sequences.md) | Eager collections, lazy iterators, the `Next` type |
| [references/meta.md](references/meta.md) | Metadata for tests, events, schedules, MCP tools, webhooks, and context |

## Code Examples

See the [examples/](examples/) directory for working Hot code:
- `basic.hot` - Variables, functions, conditionals, pipes
- `types-and-enums.hot` - Custom types, enums (closed + open), literal unions (closed + open), arrow enrollment, exhaustive match, type coercion
- `flows.hot` - Parallel, cond-all, match-all, result modifiers
- `error-handling.hot` - Auto-unwrapping, lazy arguments, Result patterns
- `sequences.hot` - Eager collections, lazy iterators, range functions
- `event-handlers.hot` - Events, schedules, retry patterns
- `http-service.hot` - HTTP requests, API integration
- `webhooks.hot` - Inbound webhooks, function aliases, and `HttpResponse`
- `stores.hot` - Persistent stores, list entry shape, and semantic search setup

## CLI Commands

```bash
hot dev             # Start dev server (watches for changes)
hot run file.hot    # Run a Hot file
hot check           # Type check the project
hot test            # Run tests
hot repl            # Interactive REPL
hot deploy          # Deploy to Hot cloud
```

## See Also

- **AGENTS.md** in project root - Quick syntax reference (passive context)
- **hot.dev/docs** - Full online documentation
