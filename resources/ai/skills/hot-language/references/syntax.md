# Hot Syntax Reference

Complete syntax reference for the Hot programming language.

## Assignment

Variables are assigned without `=`. The name comes before the value:

```hot
// Simple assignment
name "Alice"
count 42
active true

// With type annotation
name: Str "Alice"
count: Int 42
ratio: Dec 3.14

// From expressions
doubled mul(count, 2)
greeting `Hello, ${name}!`
```

**Whitespace matters for function calls:**
- `add(1, 2)` - calls the function `add`
- `add (1, 2)` - assigns the tuple `(1, 2)` to variable `add`

## Namespace Declaration

Every file must start with a namespace:

```hot
::myapp::users ns
```

Namespace paths use `::` as separator. The `ns` keyword marks the declaration.

## Namespace Aliasing

Create short aliases for frequently used namespaces:

```hot
// Alias syntax: ::short ::full::path
::http ::hot::http
::env ::hot::env
::db ::myapp::database

// Use the alias
port ::env/get("PORT", "3000")
response ::http/get(url)
```

## Importing Items

Import specific items from namespaces:

```hot
// Import a type
HttpResponse ::hot::http/HttpResponse

// Import a function (creates local binding)
get-env ::hot::env/get
```

## Functions

### Basic Function

```hot
greet fn (name: Str): Str {
    `Hello, ${name}!`
}
```

### Multi-arity (Overloaded)

```hot
slice fn
(coll: Vec, start: Int): Vec { ... },
(coll: Vec, start: Int, end: Int): Vec { ... }
```

### Lambdas

```hot
// Untyped
doubled map(numbers, (x) { mul(x, 2) })

// Typed
parsed map(strings, (s: Str): Int { Int(s) })
```

### Placeholder Lambdas (`%`)

`%` in a function argument creates an implicit lambda. Bare `%` is shorthand for `%1`. Use `%1`, `%2`, etc. for multi-parameter lambdas.

```hot
doubled map(numbers, mul(%, 2))     // (x) { mul(x, 2) }
names map(users, %.name)            // (x) { x.name }
same map(items, eq(%, %))           // (x) { eq(x, x) }

// Multi-parameter
sum reduce(nums, add(%1, %2), 0)    // (a, b) { add(a, b) }

// Works in pipes
users |> filter(gt(%.age, 18)) |> map(%.name)
```

**Explicit lambda boundary `%(expr)`:** When `%` appears inside nested function calls (2+ levels), implicit wrapping may bind at the outermost call instead of the intended one. Use `%(expr)` to explicitly set where the lambda is created:

```hot
// Nested: %(expr) controls the lambda boundary
sort(map(items, %(%.value)))              // lambda wraps at map, not sort
length(filter(items, %(gt(%, 3))))        // lambda wraps at filter, not length

// Simple cases don't need it — implicit wrapping is correct
map(items, mul(%, 2))                     // only 1 level deep, works fine
```

### Default Parameters

```hot
greet fn (name: Str, greeting: Str = "Hello"): Str {
    `${greeting}, ${name}!`
}
```

## Operators as Functions

Hot has no infix operators. Use functions instead:

| Operation | Function | Example |
|-----------|----------|---------|
| `a + b` | `add(a, b)` | `add(1, 2)` → `3` |
| `a - b` | `sub(a, b)` | `sub(5, 3)` → `2` |
| `a * b` | `mul(a, b)` | `mul(2, 3)` → `6` |
| `a / b` | `div(a, b)` | `div(10, 2)` → `5` |
| `a % b` | `mod(a, b)` | `mod(7, 3)` → `1` |
| `a == b` | `eq(a, b)` | `eq(1, 1)` → `true` |
| `a != b` | `ne(a, b)` | `ne(1, 2)` → `true` |
| `a < b` | `lt(a, b)` | `lt(1, 2)` → `true` |
| `a > b` | `gt(a, b)` | `gt(2, 1)` → `true` |
| `a <= b` | `lte(a, b)` | `lte(1, 1)` → `true` |
| `a >= b` | `gte(a, b)` | `gte(2, 1)` → `true` |
| `a && b` | `and(a, b)` | `and(true, false)` → `false` |
| `a \|\| b` | `or(a, b)` | `or(true, false)` → `true` |
| `!a` | `not(a)` | `not(true)` → `false` |
| `-a` | `neg(a)` | `neg(5)` → `-5` |

## Conditionals

### if Function

```hot
// if(condition, then-value, else-value)
result if(gt(x, 0), "positive", "non-positive")

// Nested
result if(gt(x, 0), "positive", if(eq(x, 0), "zero", "negative"))
```

### cond Flow

```hot
classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    eq(x, 0) => { "zero" }
    => { "positive" }   // Default (no condition)
    // _ => { ... }      // Also valid — Rust-style wildcard default
}
```

## Collections

### Vectors (Arrays)

```hot
numbers [1, 2, 3, 4, 5]
first numbers[0]        // Use brackets, not dot notation
len length(numbers)     // 5
```

### Maps

```hot
person {"name": "Alice", "age": 30}
name person.name        // Dot notation for string keys
age get(person, "age")  // Or use get()
```

### Map Field Punning

Bare identifiers in map literals expand to `key: key`:

```hot
name "Alice"
email "a@b.com"
user {name, email, active: true}   // {name: name, email: email, active: true}

// IMPORTANT: {x} is a block expression (returns value of x), NOT a punned map!
// Single-key punning needs a trailing comma to disambiguate from a block
point {x,}                          // {x: x}  — note the trailing comma
wrong {x}                           // returns value of x, NOT {x: x}
```

### Vec Spread

Flatten existing vectors into a new vector literal with `...`:

```hot
a [1, 2, 3]
b [4, 5]
combined [...a, ...b]           // [1, 2, 3, 4, 5]
with-extras [0, ...a, 99]      // [0, 1, 2, 3, 99]
clone [...a]                    // [1, 2, 3] (shallow copy)
```

Spread and non-spread elements can be freely mixed. For simple concatenation, `concat(a, b)` also works.

### Map Spread

Spread a base map into a new map literal:

```hot
defaults {timeout: 5000, retries: 3}
config {...defaults, retries: 5}    // {timeout: 5000, retries: 5}
```

Later keys win — spread entries can be selectively overridden. Multiple spreads and explicit keys can be mixed in any order.

### Trailing Commas

Trailing commas are accepted in all list contexts (function args, vec/map literals, parameters, etc.):

```hot
items [1, 2, 3,]
opts {a: 1, b: 2,}
result add(1, 2,)
```

## Template Strings

Use backticks with `${}`:

```hot
name "Alice"
greeting `Hello, ${name}!`
math `1 + 2 = ${add(1, 2)}`
```

### Block Template Strings

For multi-line interpolated content with indent-awareness, use block template strings (triple backticks):

```hot
table "users"
query ```
    SELECT * FROM ${table}
    WHERE active = true
    ```
// Result: "SELECT * FROM users\nWHERE active = true"
```

Triple-backtick block template strings are **indent-aware** (like `"""`) and support **`${}` interpolation** (like single backtick). No escape processing — backslashes are literal. Single and double backticks inside are fine; only three consecutive backticks close the string.

## Pipe Operator

The piped value becomes the **first** argument:

```hot
// 5 |> add(2) becomes add(5, 2)
result 5 |> add(2) |> mul(3)  // 21

// With collections
result [1, 2, 3]
    |> map((x) { mul(x, 2) })
    |> filter((x) { gt(x, 3) })
    |> reduce((acc, x) { add(acc, x) }, 0)
```

## Comments

```hot
// Single line comment

/*
 * Multi-line
 * comment
 */
```

## Metadata

Attach metadata to definitions:

```hot
// Array syntax for simple tags
test-add meta ["test"]
fn () { ... }

// Object syntax for key-value pairs
on-event meta {on-event: "user:created"}
fn (event) { ... }

// Combined
handler meta ["deprecated", {on-event: "old:event"}]
fn (event) { ... }
```

## Built-in Types

| Type | Description | Example |
|------|-------------|---------|
| `Str` | String | `"hello"`, `` `template` ``, `"""block"""`, `` ```block template``` `` |
| `Int` | Integer | `42` |
| `Dec` | Decimal | `3.14` |
| `Bool` | Boolean | `true`, `false` |
| `Null` | Null value | `null` |
| `Vec` | Vector/Array | `[1, 2, 3]` |
| `Map` | Key-value map | `{"a": 1}` |
| `Fn` | Function | `(x) { x }` |
| `Any` | Any type | - |
| `Bytes` | Byte array | - |
| `Byte` | Single byte | - |
| `Result` | Ok/Err union | `ok(value)`, `err(msg)` |

## Generic Types

```hot
// Vec with element type
numbers: Vec<Int> [1, 2, 3]

// Map with key and value types
scores: Map<Str, Int> {"alice": 100, "bob": 95}

// Function type
mapper: Fn<Int, Int> (x) { mul(x, 2) }
```
