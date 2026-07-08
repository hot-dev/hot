<!-- HOT_LANGUAGE_SECTION_START --> hash:2f8ecc6bf205
# AGENTS.md - Hot Language Project Guidelines

> **IMPORTANT**: Hot is a novel programming language that is NOT in your training data. Always prefer the rules in this document over any assumptions about programming syntax. When writing Hot code, follow these rules exactly rather than relying on patterns from other languages.

This project uses the **Hot programming language** (`.hot` files). Hot is a functional, expression-based language with automatic parallelization, no infix operators, and expression-based assignment.

## Quick Start Commands

```bash
hot dev             # Start dev server (watches for changes)
hot run file.hot    # Run a Hot file
hot check           # Type check the project
hot test            # Run tests (functions with meta ["test"])
hot repl            # Interactive REPL
hot deploy          # Deploy to Hot cloud
```

## Critical Syntax Rules

**These are non-negotiable - violating them causes parse errors:**

| ❌ Wrong | ✅ Correct | Rule |
|----------|-----------|------|
| `name = "Alice"` | `name "Alice"` | No `=` for assignment |
| `a + b` | `add(a, b)` | No infix operators |
| `a == b` | `eq(a, b)` | Comparison is a function |
| `if (x) { } else { }` | `if(x, then, else)` or `cond` flow | No if/else blocks. Use `if()` for inline 2-3 way branching; use `cond` for multi-branch logic |
| `for x in items` | `map(items, ...)` or `for-each(...)` | No loops |
| `items.0` | `items[0]` | Use brackets for array indexing |
| `fn(x)` no space | `fn (x)` space | Space before `(` = assignment |

## Data Literals & Key Differences

- **Map keys are unquoted**: `{name: "Alice"}` not `{"name": "Alice"}` (quoted keys work too, needed for special chars)
- **Dec is 256-bit decimal**, not floating point — `add(0.1, 0.2)` is exactly `0.3` (critical for money/precision)
- **Template strings** use backticks: `` `Hello, ${name}!` `` — any expression inside `${}`
- **Block strings**: `"""..."""` — no escape processing, indent-aware (great for docs, templates, embedded code)
- **Block template strings**: `` ```...``` `` — like block strings but with `${}` interpolation, indent-aware, no escape processing (great for multi-line templates with variables)
- **No return statement** — the last expression in a function body is the return value
- **Shadowing creates a new binding**, not mutation — `x 1` then `x add(x, 1)` creates a new `x`, doesn't mutate
- **Trailing commas** are accepted in all list contexts — `[1, 2, 3,]`, `{a: 1,}`, `add(1, 2,)`, `fn (a: Int,)`, `(x,) { ... }`
- **Map field punning** — bare identifiers desugar to key-value pairs: `{name, email}` → `{name: name, email: email}`. **IMPORTANT**: `{x}` is a block expression (returns the value of `x`), NOT a punned map. Use `{x,}` (trailing comma) for single-key punning, or `{x: x}` to be explicit
- **Vec spread** — `...` flattens vectors in a vec literal: `[...a, ...b]` concatenates, `[0, ...a, 99]` mixes spread and literals
- **Map spread** — `...` spreads an existing map: `{...base, c: 3}`. Later keys win: `{...base, a: 99}` overrides `a`

## File Structure

```hot
::myapp::users ns              // Namespace declaration (required first line)

// Namespace aliasing: create short aliases for namespaced functions
::http ::hot::http
::env ::hot::env

// Import specific items (var alias)
HttpResponse ::hot::http/HttpResponse

// Variables (no = sign)
api-url ::env/get("API_URL", "https://api.example.com")
max-retries: Int 3

// Core functions (add, eq, map, if, etc.) need no import—they're auto-imported.
// Namespaced functions (::hot::http, ::hot::env, etc.) need an alias or full path.

// Functions (return type is the success type, not Result)
get-user fn (id: Str): Map {
    response ::http/get(`${api-url}/users/${id}`)
    if(is-ok(response), response.body, err("Failed"))  // no ok() needed—auto-wrapped
}

// Conditional flow function
handle fn cond (status: Int): Str {
    eq(status, 200) => { "success" }
    lt(status, 500) => { "client error" }
    => { "server error" }  // Default case
}

// Event handler
on-user-created meta {on-event: "user:created"}
fn (event) {
    send-welcome-email(event.data.email)
}

// Scheduled function
daily-sync meta {schedule: "0 2 * * *", retry: 3}
fn () {
    sync-external-data()
}
```

## Flows

Flows (`cond`, `cond-all`, `match`, `match-all`, `parallel`, `serial`) are **standalone constructs**. The `fn` keyword modifies a flow to become a function definition.

```hot
// Flow as function definition
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    eq(x, 0) => { "zero" }
    => { "positive" }
}

// Standalone flows within function bodies
process fn (order: Map): Map {
    // cond - first matching branch wins (errors propagate automatically)
    validated cond {
        is-empty(order.items) => { err("No items") }
        lt(order.total, 0) => { err("Invalid total") }
        => { order }
    }

    // parallel - concurrent execution, returns Map
    data parallel {
        customer fetch-customer(validated.customer-id)
        inventory fetch-inventory(validated.items)
    }

    // cond-all - ALL matching branches execute, returns Map
    effects cond-all {
        order.is-gift => gift { add-gift-wrap(order) }
        order.notify => notify { send-notification(order) }
        => standard { log-order(order) }
    }

    data
}
```

**Flow types:**
- `serial` - Sequential execution (default), returns last value
- `cond` - First matching branch wins
- `cond-all` - ALL matching branches execute, returns Map
- `match` - Pattern match on types/values, first match wins
- `match-all` - Pattern match, ALL matches execute, returns Map
- `parallel` - Concurrent execution, returns Map

**Default arms:** Both bare `=>` and `_ =>` can be used as the default/wildcard arm in `cond` and `match`:
```hot
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    _ => { "positive" }  // same as bare => { "positive" }
}
```

**Flow result shape** controls what a flow returns. Prefer annotations:
- plain/no annotation — return the winning/last value (default for `serial`, `cond`, `match`)
- `All<Vec>` — collect all results into a Vec
- `All<Map>` — collect results into a Map keyed by branch name (default for `parallel`, `cond-all`, `match-all`)
- Any other type on a collect-all flow — return only the single final value (`x: Int parallel { ... }`)

Bare `All` is only for natural collect-all forms (`parallel`, `cond-all`, and
`match-all`); use explicit `All<Vec>` or `All<Map>` elsewhere.

```hot
// cond-all defaults to All<Map> with named branches
effects cond-all {
    order.is-gift => gift { add-gift-wrap(order) }
    order.notify => notify { send-notification(order) }
}
// => {gift: ..., notify: ...}

// Override to collect as Vec
discounts: All<Vec> cond-all {
    is-member => { 0.10 }
    gt(total, 100) => { 0.05 }
}
// => [0.10, 0.05]

// parallel with All<Vec> instead of default All<Map>
values: All<Vec> parallel {
    a fetch-a()
    b fetch-b()
}
// => [<a-result>, <b-result>]
```

## Types

```hot
// Struct type (type name IS constructor)
User type { id: Str, email: Str, active: Bool }
user User({id: "123", email: "a@b.com", active: true})
user.email  // "a@b.com"

// Enum (variant union) type
Direction enum { Up, Down, Left, Right }
dir Direction.Up

// Enum with data
Circle type { radius: Dec }
Shape enum { Circle(Circle), Point }
shape Shape.Circle({radius: 5.0})
shape.radius  // 5.0

// Match flow for enums (access matched value through original variable).
// Match on a closed enum must be exhaustive — every variant covered or
// a `_` default arm. Compiler reports `non-exhaustive-match` otherwise.
describe fn match (dir: Direction): Str {
    Direction.Up => { "Going up" }
    Direction.Down => { "Going down" }
    Direction.Left => { "Going left" }
    Direction.Right => { "Going right" }
}

// Open enums — extensible variant unions. New variants are added later
// via `Source -> Enum.Variant` arrows. Match on an open enum REQUIRES a
// `_` default arm (`open-enum-match-missing-default` otherwise).
Animal enum open { Dog, Cat }

Bird type { species: Str, wingspan: Dec }
Bird -> Animal.Bird               // bodyless arrow enrolls the variant
                                   // AND synthesizes the constructor

eagle Animal.Bird({species: "Eagle", wingspan: 2.1})

label-animal fn match (a: Animal): Str {
    Animal.Dog => { "dog" }
    Animal.Cat => { "cat" }
    Animal.Bird => { "bird" }
    _ => { "unknown" }            // required: variants are open-ended
}

// Role-shaped variant — variant name need not match the source type
Lizard type { length-cm: Dec }
Lizard -> Animal.Reptile          // tag is Reptile, payload is Lizard

// Match on literal values (Int, Str, Dec, Bool, Null, Vec, Map)
status-message fn match (status: Int): Str {
    200 => { "ok" }
    404 => { "not found" }
    500 => { "server error" }
    => { "unknown" }
}

// Match with mixed type and value arms (top-to-bottom, first wins)
describe-value fn match (value: Any): Str {
    null => { "null" }
    0 => { "zero" }
    "" => { "empty string" }
    Int => { "integer" }
    Str => { "string" }
    => { "other" }
}

// Match on expression subjects (computed values)
result match length(name) {
    0 => { "empty" }
    1 => { "single char" }
    => { "long" }
}

// Nullable types — Str? is shorthand for Str | Null
// ? means the TYPE accepts null, NOT that the parameter is omittable
find-user fn (id: Str): User? { /* returns User or null */ }
greet fn (name: Str, title: Str?): Str { /* title must be passed, but can be null */ }

// Literal unions — restrict to specific values
Fruit type "apple" | "banana" | "orange"
DiceRoll type 1 | 2 | 3 | 4 | 5 | 6

// Open literal unions — extensible by re-declaring at top level
HttpMethod type open "GET" | "POST" | "PUT" | "DELETE" | "PATCH"
HttpMethod type open | "CONNECT"            // single-member extension (leading |)
HttpMethod type open "TRACE" | "OPTIONS"    // multi-member extension
// Open ⇄ closed mismatch → `open-literal-union-mismatch`; extensions must be top-level

// Type coercion
Date -> Str fn (d: Date): Str { `${d.year}-${d.month}-${d.day}` }

// Runtime type checking
is-type(alice, Person)    // true for custom types
is-str(value)             // true if Str (also: is-int, is-dec, is-vec, is-map, is-fn, is-null)

// untype — strip $type/$val metadata for serialization
alice Person({name: "Alice", age: 30})
to-json(untype(alice))    // {"name":"Alice","age":30} (clean JSON, no $type/$val)

// Built-in: Str, Int, Dec, Bool, Null, Vec, Map, Fn, Any, Bytes, Result
```

## Standard Library Quick Reference

### Core Functions (auto-imported, no prefix needed)

Functions with `meta {core: true}` are available everywhere without namespace qualification. You can also mark your own functions as `core: true` to auto-import them across your project.

```hot
// Math — accept Int | Dec, mixing types produces Dec
add fn (x: Int | Dec, y: Int | Dec): Int | Dec
sub fn (x: Int | Dec, y: Int | Dec): Int | Dec
mul fn (x: Int | Dec, y: Int | Dec): Int | Dec
div fn (x: Int | Dec, y: Int | Dec): Int | Dec
mod fn (x: Int | Dec, y: Int | Dec): Int | Dec
pow fn (base: Int | Dec, exp: Int | Dec): Int | Dec
abs fn (x: Int | Dec): Int | Dec
min fn (x: Int | Dec, y: Int | Dec): Int | Dec
max fn (x: Int | Dec, y: Int | Dec): Int | Dec
round fn (x: Int | Dec): Int
floor fn (x: Int | Dec): Int
ceil fn (x: Int | Dec): Int
rand fn (): Dec
rand fn (n: Int): Dec
rand-int fn (): Int
rand-int fn (n: Int): Int

// Comparison
eq fn (a: Any, b: Any): Bool
ne fn (a: Any, b: Any): Bool
lt fn (a: Int | Dec, b: Int | Dec): Bool
gt fn (a: Int | Dec, b: Int | Dec): Bool
lte fn (a: Int | Dec, b: Int | Dec): Bool
gte fn (a: Int | Dec, b: Int | Dec): Bool

// Logic — and/or short-circuit, if branches are lazy
is-truthy fn (value: Any): Bool
not fn cond (x: Any): Bool
and fn (x: Any, ...rest): Any
or fn (x: Any, ...rest): Any
if fn cond (pred: Any, lazy then: Any): Any
if fn cond (pred: Any, lazy then: Any, lazy else: Any): Any

// Collections — most accept Vec, Map, and Str
map fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map
filter fn (coll: Str | Vec | Map, predicate: Fn): Str | Vec | Map
reduce fn (coll: Str | Vec | Map, reducer: Fn, initial: Any): Any // return reduced(val) to short-circuit
first fn (coll: Str | Vec | Map): Any
rest fn (coll: Str | Vec | Map): Str | Vec | Map
last fn (coll: Str | Vec | Map): Any
get fn (coll: Map | Vec | Str | Bytes, key: Any): Any
get fn (coll: Map | Vec | Str | Bytes, key: Any, not-found: Any): Any
length fn (coll: Str | Vec | Map | Bytes): Int
is-empty fn (coll: Str | Vec | Map): Bool
concat fn (a: Vec, b: Vec, ...rest: Vec): Vec
concat fn (a: Str, b: Str, ...rest: Str): Str
flatten fn (coll: Str | Vec | Map): Str | Vec | Map
merge fn (a: Map, b: Map): Map
assoc fn (map: Map, key: Any, value: Any): Map
assoc-some fn (map: Map, key: Any, value: Any): Map // assoc only if value is non-null
update fn (map: Map, key: Any, func: Fn): Map // apply func to value at key
keys fn (coll: Str | Vec | Map): Str | Vec | Map
vals fn (coll: Str | Vec | Map): Str | Vec | Map
map-keys fn (m: Map, func: Fn): Map // transform all keys
map-vals fn (m: Map, func: Fn): Map // transform all values
filter-keys fn (m: Map, predicate: Fn): Map // keep entries where key matches
filter-vals fn (m: Map, predicate: Fn): Map // keep entries where value matches
index-by fn (coll: Vec, func: Fn): Map // index collection by key function
group-by fn (coll: Vec, func: Fn): Map // group elements by key function
frequencies fn (coll: Vec): Map // count occurrences of each element
some fn (coll: Str | Vec | Map, predicate: Fn): Bool
all fn (coll: Str | Vec | Map, predicate: Fn): Bool
find-first fn (coll: Vec, predicate: Fn): Any // first match or null, short-circuits
sort fn (coll: Str | Vec | Map): Str | Vec | Map
reverse fn (coll: Str | Vec | Map): Str | Vec | Map
distinct fn (coll: Str | Vec | Map): Str | Vec | Map
slice fn (coll: Str | Vec | Map | Bytes, start: Int): Str | Vec | Map | Bytes
slice fn (coll: Str | Vec | Map | Bytes, start: Int, end: Int): Str | Vec | Map | Bytes
range fn (end: Int | Dec): Vec
range fn (start: Int | Dec, end: Int | Dec): Vec
range fn (start: Int | Dec, end: Int | Dec, step: Int | Dec): Vec

// Strings
split fn (s: Str, delim: Str): Vec<Str>
join fn (values: Vec<Str>, delim: Str): Str
trim fn (s: Str): Str
replace fn (s: Str, from: Str, to: Str): Str
uppercase fn (s: Str): Str
lowercase fn (s: Str): Str
contains fn (s: Str, substring: Str): Bool
starts-with fn (s: Str, prefix: Str): Bool
ends-with fn (s: Str, suffix: Str): Bool

// Results
ok fn (value: Any): Result
err fn (value: Any): Result
is-ok fn (lazy result: Any): Bool
is-err fn (lazy result: Any): Bool
if-ok fn (lazy result, handler): Any    // Ok → apply fn/use value; Err → pass through
if-err fn (lazy result, handler): Any   // Err → apply fn/use value; Ok → pass through

// Short-circuit
Reduced type { value: Any } // wrapper signaling early termination from reduce
reduced fn (value: Any): Reduced // wrap value to short-circuit reduce

// Types — constructors double as coercion functions
Str type fn (value: Any): Str
Int type fn (value: Int | Dec): Int
Dec type fn (value: Int | Dec): Dec
Bool type fn (value: Any): Bool
Vec type fn (value: Any): Vec<Any>
Map type fn (value: Any): Map<Any, Any>
is-null fn (value: Any): Bool
is-some fn (value: Any): Bool
Uuid fn (): Str
is-uuid fn (value: Any): Bool

// JSON
from-json fn (s: Str): Any
to-json fn (value: Any): Str

// XML
from-xml fn (s: Str): Map
to-xml fn (value: Map): Str
child fn (node: Map, name: Str): Map
children fn (node: Map, name: Str): Vec<Map>
text fn (node: Map): Str
attr fn (node: Map, name: Str): Str

// File I/O
read-file fn (path: Str): Str
write-file fn (path: Str, content: Str): Bool
delete-file fn (path: Str): Bool
file-exists fn (path: Str): Bool
list-files fn (path: Str): Vec<Str>

// Events
send fn (event-name: Str, data: Any): Map

// Random
random-bytes fn (n: Int): Bytes
random-string fn (n: Int): Str
secure-compare fn (a: Str, b: Str): Bool

// Deep path operations
get-in fn (coll: Map | Vec, path: Vec): Any
get-in fn (coll: Map | Vec, path: Vec, default: Any): Any
assoc-in fn (coll: Map | Vec, path: Vec, value: Any): Map | Vec
update-in fn (coll: Map | Vec, path: Vec, func: Fn): Map | Vec

// Predicates
between fn (x: Int | Dec, low: Int | Dec, high: Int | Dec): Bool  // inclusive range check
in fn (value: Any, coll: Vec): Bool    // membership test

// Utility
tap fn (value: Any): Any               // print value to stderr, return unchanged
tap fn (value: Any, label: Str): Any   // print "label: value" to stderr
print fn (val: Any): Str
println fn (val: Any): Str
assert fn (actual: Any): Bool
assert fn (actual: Any, msg: Str): Bool
assert-eq fn (expected: Any, actual: Any): Bool
assert-eq fn (expected: Any, actual: Any, msg: Str): Bool
fail fn (msg: Str): Failure
fail fn (msg: Str, data: Any): Failure
```

### Namespaced Functions (require prefix or alias)

These need the full `::namespace/function` path, a namespace alias, or a var import:

```hot
// Namespace alias (most common)
::http ::hot::http
response ::http/get("https://api.example.com")

// Var import (import a single function)
sha256 ::hot::hash/sha256
hash sha256("data")

// Full qualified path (no alias)
hash ::hot::hash/sha256("data")
```

- `::hot::http` — `get`, `post`, `put`, `patch`, `delete`, `request`, `request-stream`, `is-ok-response`
- `::hot::env` — `get(name, default)`, `get-all`
- `::hot::ctx` — `get(key)`, `set(key, value)`, `set(map)`, `set-secret(key, value)`
- `::hot::hash` — `sha256`, `blake3`, `sha384`, `sha512`
- `::hot::hmac` — `hmac-sha512`, `hmac-sha1`
- `::hot::time` — `now`, `now-zoned`, `parse`, `format`, `add`, `subtract`, `year`, `month`, `day`, `days`, `hours`, `minutes`, `seconds`, `with-timezone`, `to-plain-date-time`, `to-plain-date`, `to-plain-time`, `to-instant`
- `::hot::base64` — `encode`, `decode`, `encode-url`, `decode-url`
- `::hot::uri` — `Uri` type, `encode`, `decode`, `encode-query`, `decode-query`, `parse`, `build`, `join`, `is-valid`
- `::hot::regex` — `first-match`, `find`, `find-all`, `replace`, `replace-all`, `split`, `is-match`
- `::hot::bytes` — `to-int`, `to-uint`, `from-int`, `crc32`, `to-vec`
- `::hot::bit` — `and`, `or`, `xor`, `not`, `shift-left`, `shift-right`
- `::hot::run` — `fail`, `cancel`, `exit`, `info`, `is-inline-run`
- `::hot::meta` — `get(var)`
- `::hot::task` — `start`, `cancel`, `await`, `send`, `receive`, `checkpoint`, `restore`
- `::hot::store` — `put`, `get`, `delete`, `keys`, `vals`, `length`, `is-empty`, `list`, `search`, `filter`, `find-first`, `clear`, `destroy`

#### Reserved namespace prefixes

- `::hot::internal::*` — host-provided primitives intended to be wrapped by `hot-std` itself or by other packages (e.g. `::hot::internal::tokenizer` is wrapped by `::ai::tokenizer` from `hot.dev/hot-ai`). Always carry `no-doc: true` and may change between releases — never call directly from user code.
- `::hot::ext::*` — reserved for a future package-supplied native extension story. Not in use today.

Full signatures for namespaced functions are in `references/hot-std.md`.

## Error Handling & Results

Hot uses `Result` types instead of exceptions. Results are ergonomic thanks to **auto-wrapping**, **auto-unwrapping**, and **lazy arguments**.

### Auto-Wrapping & Auto-Unwrapping

```hot
// Return values are automatically wrapped in Result.Ok
// Return type annotation is the SUCCESS type, not Result
fetch-user fn (id: Str): Map {
    ::http/get(`${api-url}/users/${id}`).body  // auto-wrapped in Ok
}

// Ok results auto-unwrap when used; Err results halt execution
main fn () {
    user fetch-user("123")        // auto-unwraps Ok, or halts on Err
    posts fetch-posts(user.id)    // only runs if above succeeded
    render-page(user, posts)      // errors propagate automatically
}
```

### Checking Results Without Unwrapping

`is-ok` and `is-err` receive the result as a **lazy argument**, which suppresses auto-unwrapping:

```hot
result safe-divide(10, 0)
if(is-ok(result), `Result: ${result}`, "Division failed")

// Pattern matching on Result variants
message match result {
    Result.Ok => { `Success: ${result}` }
    Result.Err => { `Error: ${result}` }
}
```

### Dot Access on Results

Result-returning calls can be used directly when you expect success:

```hot
response ::http/get("https://api.example.com/user/1")
name response.body.name
```

### Common Error Patterns

```hot
// Pattern 1: Let it fail — errors propagate automatically
user fetch-user(id)
posts fetch-posts(user.id)

// Pattern 2: Check and handle
result fetch-user(id)
if(is-ok(result), render-profile(result), render-error-page())

// Pattern 3: Default values
name or(get(config, "name"), "Anonymous")

// Pattern 4: Expected failures are err(...) values; fail() is for
// broken invariants only — it halts the run/task (there is no catch)
if(is-empty(data.email), err({field: "email", message: "Email required"}), data)
if(lt(version, current-version(db)), fail("migration went backwards"), data)

// Pattern 5: Result combinators — transform Ok or Err selectively
fetch-user(id)
    |> if-ok(%.name)
    |> if-err("Anonymous")

// Pattern 6: Fan-out isolation — OnErr.Preserve keeps per-item Errs
// in their slots instead of halting the loop
results map(items, process-item, OnErr.Preserve)
failures filter(results, is-err)
```

## Common Patterns

### Pipe Operator
Piped value becomes **first** argument:
```hot
result 5 |> add(2) |> mul(3)  // add(5,2)=7, mul(7,3)=21
data |> map((x) { mul(x, 2) }) |> filter((x) { gt(x, 5) })
```

### Placeholder Lambdas (`%`)
`%` in function call arguments creates an implicit lambda. Bare `%` is shorthand for `%1` (first arg). Use `%1`, `%2`, etc. for multi-parameter lambdas:
```hot
map(items, mul(%, 2))       // desugars to: map(items, (__0) { mul(__0, 2) })
filter(items, gt(%, 3))     // desugars to: filter(items, (__0) { gt(__0, 3) })
map(items, %.name)          // desugars to: map(items, (__0) { __0.name })
map(items, add(%, %))       // both % reference the same parameter (%1)

// Multi-parameter: %1 and %2
reduce(nums, add(%1, %2), 0) // desugars to: (__0, __1) { add(__0, __1) }

// Collection pipelines become very concise
users
    |> filter(%.active)
    |> map(%.email)
    |> filter(contains(%, "@company.com"))
```

**Implicit lambda binding (the rule):** A `%` placeholder is bound by the **nearest enclosing parameter slot whose declared type contains `Fn`** (e.g. `Fn`, `Fn | Null`, `Lazy Fn`, `Vec<Fn>`). The compiler wraps the smallest expression containing `%` into a lambda at that slot. This works uniformly for built-in HOFs *and* user-defined HOFs — there is no hardcoded list of HOF names.

```hot
length(filter(items, gt(%, 3)))      // wraps at filter's Fn slot
sort(map(items, %.value))            // wraps at map's Fn slot
not(is-empty(filter(xs, eq(%, x))))  // wraps at filter's Fn slot

// User-defined HOF: any parameter declared `Fn` is a binding site.
apply-twice fn (x: Int, f: Fn): Int { f(f(x)) }
apply-twice(3, mul(%, 2))            // wraps at apply-twice's f
```

**No `Fn` slot ⇒ compile error.** If a `%` does not have an enclosing `Fn`-typed parameter slot to bind to, compilation fails with a clear error pointing at the placeholder. This makes the wrapping behavior local and predictable: `%` never silently bubbles past a non-HOF call.

**Explicit lambda boundary `%(expr)`:** Use `%(expr)` when you need a first-class lambda value where no `Fn` slot exists, or when you want to force the lambda boundary at a specific inner point:
```hot
// Build a lambda value to store / pass around.
sq %(mul(%, %))
invoke-fn(4, sq)

// Force the wrap at the inner expression.
sort(map(data, %(%.value)))
```

### Validation with cond
```hot
validate fn cond (user: Map): Map {
    is-null(user.email) => { err("Email required") }
    not(contains(user.email, "@")) => { err("Invalid email") }
    lt(length(user.password), 8) => { err("Password too short") }
    => { user }  // all checks passed
}
```

### Default Values
```hot
// With get (3-arity)
port get(config, "port", 3000)

// With or (first truthy value)
name or(user.nickname, user.email, "Anonymous")
```

### Map Building
```hot
// Deep path assignment
config.database.host "localhost"
config.database.port 5432

// Field punning — bare identifiers become key: value pairs
name "Alice"
email "alice@example.com"
user {name, email}  // {name: "Alice", email: "alice@example.com"}
// CAUTION: {name} is a block (returns "Alice"), NOT {name: "Alice"}
// Use {name,} (trailing comma) for single-key punned maps

// Map spread — merge and override
defaults {timeout: 5000, retries: 3}
config {...defaults, retries: 5}  // {timeout: 5000, retries: 5}

// Vec spread — flatten vectors inline
a [1, 2, 3]
b [4, 5]
combined [...a, ...b]     // [1, 2, 3, 4, 5]
padded [0, ...a, 99]      // [0, 1, 2, 3, 99]

// Dynamic keys with assoc
headers assoc({}, "Authorization", `Bearer ${token}`)

// Deep path operations
config {db: {host: "localhost", port: 5432}}
get-in(config, ["db", "port"])              // 5432
assoc-in(config, ["db", "port"], 5433)     // new config with updated port
update-in(config, ["db", "port"], add(%, 1))
```

### Collection Pipelines
```hot
// With placeholder lambdas (concise)
process-users fn (users: Vec<Map>): Vec<Str> {
    users
    |> filter(%.active)
    |> map(lowercase(%.email))
    |> filter(ends-with(%, "@company.com"))
}

// With explicit lambdas (equivalent)
process-users fn (users: Vec<Map>): Vec<Str> {
    users
    |> filter((u) { u.active })
    |> map((u) { lowercase(u.email) })
    |> filter((e) { ends-with(e, "@company.com") })
}
```

### Tail Recursion (TCO)

Hot interpreter frames are heavy — prefer collection ops (`map`, `filter`, `reduce`, `for-each`) for iteration. When you do recurse, the recursive call **must be in tail position** (the entire body of its branch, not bound to a name, not consumed by another expression). Non-tail recursion can blow the OS stack at surprisingly shallow depths.

```hot
// GOOD - accumulator pattern, recursive call is the whole branch body
sum-list fn cond (xs: Vec, acc: Int): Int {
    is-empty(xs) => { acc }
    => { sum-list(rest(xs), add(acc, first(xs))) }
}

// BAD - recursive call result is bound, then returned
//   the let-binding keeps the frame live; not TCO'd
sum-list-bad fn cond (xs: Vec, acc: Int): Int {
    is-empty(xs) => { acc }
    => {
        r sum-list-bad(rest(xs), add(acc, first(xs)))   // not tail
        r
    }
}

// BAD - recursive call result is consumed by another expression
//   (here `.value` access), so the frame must persist
collect-bad fn cond (xs: Vec): Vec {
    is-empty(xs) => { [] }
    => {
        rest-result collect-bad(rest(xs))   // not tail - we read .value below
        concat([first(xs).value], rest-result)
    }
}
```

If you hit `Function call recursion limit reached`, that's the soft cap (`HOT_MAX_RECURSION_DEPTH=4096`) telling you a function isn't tail-recursive. Rewrite to the accumulator form, or convert to `reduce` / `for-each`. Raising the cap is almost never the right answer.

### Event Handlers with Retry
```hot
// Simple retry: 3 attempts with default 1s delay
on-payment meta {on-event: "payment:received", retry: 3}
fn (event) { process-payment(event.data) }

// Full retry: custom attempts and delay
daily-sync meta {schedule: "0 2 * * *", retry: {attempts: 5, delay: 10000}}
fn () { sync-external-data() }
```

### HTTP Requests
```hot
::http ::hot::http

response ::http/get("https://api.example.com/users")
response ::http/post("https://api.example.com/users", {name: "Alice"})
// Response: {status: Int, headers: Map, body: Any}
```

## Testing

```hot
test-add meta ["test"]
fn () {
    assert(eq(add(1, 2), 3), "1 + 2 should equal 3")
}
```

Run tests with `hot test`.

## Project Structure

```
project/
├── hot.hot              # Project configuration
├── hot/
│   ├── src/             # Source files
│   │   └── myapp/
│   │       └── module.hot
│   └── test/            # Test files
└── .hot/                # Local data (gitignored)
```

## Additional Resources

**Online Documentation:** https://hot.dev/docs/language

For local reference, see the `.skills/hot-language/` directory:

**References:**
- `references/syntax.md` - Complete syntax reference
- `references/hot-std.md` - Full standard library documentation
- `references/types.md` - Type system and type coercion
- `references/flows.md` - Flow patterns, `if()` vs `cond`, flow result shapes
- `references/error-handling.md` - Auto-unwrapping, lazy arguments, Result patterns
- `references/sequences.md` - Eager collections, lazy iterators, range functions

**Examples:**
- `examples/basic.hot` - Variables, functions, conditionals, pipes
- `examples/types-and-enums.hot` - Custom types, enums (closed + open), literal unions (closed + open), arrow enrollment, exhaustive match
- `examples/flows.hot` - Parallel, cond-all, match-all, flow result shapes
- `examples/error-handling.hot` - Auto-unwrapping, lazy arguments
- `examples/sequences.hot` - Collections, iterators, range
- `examples/event-handlers.hot` - Events, schedules, retry patterns
- `examples/http-service.hot` - HTTP requests, API integration

<!-- HOT_LANGUAGE_SECTION_END -->
