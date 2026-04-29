# Hot Type System Reference

Hot has a structural type system with type inference, custom types, enums, and type coercion.

## Built-in Types

| Type | Description | Literal Example |
|------|-------------|-----------------|
| `Str` | UTF-8 string | `"hello"`, `` `template` ``, `"""block"""`, `` ```block template``` `` |
| `Int` | 64-bit integer | `42`, `-17` |
| `Dec` | Decimal number | `3.14`, `-0.5` |
| `Bool` | Boolean | `true`, `false` |
| `Null` | Null value | `null` |
| `Vec` | Ordered collection | `[1, 2, 3]` |
| `Map` | Key-value pairs | `{"a": 1, "b": 2}` |
| `Fn` | Function | `(x) { x }` |
| `Any` | Any type | - |
| `Bytes` | Byte array | - |
| `Byte` | Single byte (0-255) | - |
| `Result` | Success/Error union | `ok(v)`, `err(e)` |

## Type Annotations

Annotations are optional but can be added for clarity:

```hot
// Variable with type
name: Str "Alice"
count: Int 42
ratio: Dec 3.14

// Function parameters and return type
add fn (a: Int, b: Int): Int {
    call-lib(::hot::math/add, [a, b])
}

// Generic types
numbers: Vec<Int> [1, 2, 3]
lookup: Map<Str, Int> {"a": 1, "b": 2}
```

## Struct Types

Define named record types with the `type` keyword:

```hot
// Define struct
User type {
    id: Str,
    email: Str,
    active: Bool
}

// Construct (type name IS the constructor)
user User({"id": "123", "email": "a@b.com", "active": true})

// Access fields with dot notation
email user.email
is-active user.active
```

### Custom Constructors

Override the default constructor with a function:

```hot
Point type fn (x: Int, y: Int): Point {
    Point({"x": x, "y": y})
}

// Now construct with positional args
p Point(10, 20)
p.x  // 10
p.y  // 20
```

### Nested Types

```hot
Address type {
    street: Str,
    city: Str,
    zip: Str
}

Person type {
    name: Str,
    address: Address
}

alice Person({
    "name": "Alice",
    "address": Address({
        "street": "123 Main St",
        "city": "Springfield",
        "zip": "12345"
    })
})

alice.address.city  // "Springfield"
```

## Literal Union Types

A **literal union** is a type made up of specific literal values — strings,
ints, decs, bools, or `null`. Use the `|` separator after the `type`
keyword:

```hot
// String literal union (enum-like, lowercase values)
Fruit type "apple" | "banana" | "orange"

// Numeric literal union
DiceRoll type 1 | 2 | 3 | 4 | 5 | 6

// Mixed with null (nullable literal union)
OptionalFruit type "apple" | "banana" | null

// Single-literal type (unit type)
A type "A"
```

Literal unions are values of their underlying primitive type — a `Fruit`
**is** a `Str`, a `DiceRoll` **is** an `Int`. They flow into and out of
plain `Str`/`Int` parameters without ceremony.

### Open Literal Unions

A literal union may be declared `open` to allow other namespaces (or
later top-level declarations in the same namespace) to add members:

```hot
// Seed the union as open
Fruit type open "apple" | "banana" | "orange"

// Extend with a leading `|` (single-member extension; the leading
// pipe makes the extension obvious at the call site)
Fruit type open | "kiwi"

// Multi-member extension reads naturally without a leading pipe
Fruit type open "mango" | "pineapple"
```

Re-declaring with an existing member is a silent no-op (deduped), not
an error — "include this member" should be safe to repeat.

**Rules:**

1. **Open ⇄ closed mismatch is an error**
   (`open-literal-union-mismatch`). Once a literal union is declared
   open it can only be extended `open`, and once declared closed it
   cannot be re-opened. Mixing the two raises a diagnostic that points
   at both spans.
2. **Extensions must be top-level.** A `Foo type open | "x"` inside a
   function body is rejected with `open-literal-union-mismatch` because
   nested extensions make the accumulated member set hard to reason
   about from the outside.
3. **`open` only applies to literal unions.** Adding `open` to a
   non-literal alias (e.g., `Foo type open Int`) is rejected with
   `open-literal-union-mismatch`.
4. **Extensions live in the same namespace as the seed.** The
   accumulated member set is per-namespace; two namespaces that
   declare `Fruit` with different members are unrelated types
   (cross-namespace short-name collisions are independent — use FQNs
   to disambiguate when imported).

#### Migration recipe: closed → open

To open a previously closed literal union for extension, prefix the
seed declaration with `open`:

```hot
// Before — closed (no extension allowed)
HttpMethod type "GET" | "POST" | "PUT" | "DELETE" | "PATCH"

// After — open (additional members can be added in the same namespace)
HttpMethod type open "GET" | "POST" | "PUT" | "DELETE" | "PATCH"
```

Existing call sites and pattern matches require no changes. New
members are added by re-declaring at the top level of the *same*
namespace:

```hot
// In the same namespace that declared the open seed
HttpMethod type open | "CONNECT"
HttpMethod type open | "TRACE"
```

## Enum Types

Define variant/union types with the `enum` keyword:

```hot
// Simple enum (no data)
Direction enum {
    Up,
    Down,
    Left,
    Right
}

// Construct
dir Direction.Up

// Compare
is-up eq(dir, Direction.Up)
```

### Enums with Data

```hot
// Define types for variants that carry data
Circle type { radius: Dec }
Rectangle type { width: Dec, height: Dec }

// Define enum with typed variants
Shape enum {
    Circle(Circle),
    Rectangle(Rectangle),
    Point  // No data
}

// Construct
circle Shape.Circle({radius: 5.0})
rect Shape.Rectangle({width: 10.0, height: 20.0})
point Shape.Point

// Access inner data (dot notation skips $val automatically)
circle.radius  // 5.0
rect.width     // 10.0
```

### Pattern Matching on Enums

Use `match` flow to handle enum variants. Access the matched value directly through the matched variable:

```hot
area fn match (shape: Shape): Dec {
    Shape.Circle => { mul(3.14159, mul(shape.radius, shape.radius)) }
    Shape.Rectangle => { mul(shape.width, shape.height) }
    Shape.Point => { 0.0 }
}
```

`match` on a closed enum must be **exhaustive**: every variant either has its
own arm or a `_` default arm catches the rest. The compiler rejects
non-exhaustive matches (`non-exhaustive-match`).

### Open Enums

Add the `open` modifier to make an enum extensible — any module may register
new variants by declaring an arrow `Source -> Enum.Variant`. An open enum
with no seed variants is a pure extension point:

```hot
// Pure extension point
Plugin enum open { }

// Open enum with initial variants
Animal enum open {
    Dog,
    Cat
}
```

Because the variant set of an open enum is unbounded, `match` on an open
enum **must** include a `_` default arm — the compiler reports
`open-enum-match-missing-default` otherwise.

```hot
describe-animal fn match (a: Animal): Str {
    Animal.Dog => { "dog" }
    Animal.Cat => { "cat" }
    _ => { "unknown" }     // required for open enums
}
```

### Arrow Enrollment

Arrows do double duty for open enums: they register a new variant **and**
synthesize the variant constructor. The bodyless form is the most concise:

```hot
Bird type { species: Str, wingspan: Dec }
Bird -> Animal.Bird       // enrolls Bird as Animal.Bird

eagle Animal.Bird({species: "Eagle", wingspan: 2.1})
```

The variant name can differ from the source type name — this is a
**role-shaped variant**:

```hot
Lizard type { length-cm: Dec }
Lizard -> Animal.Reptile  // variant is Animal.Reptile, payload is Lizard
```

Declaring the same arrow twice in different locations is an error
(`ambiguous-type-implementation`) and points at both source spans.

## Result Type

`Result` is a built-in enum for error handling:

```hot
// Create results
success ok("value")
failure err("something went wrong")

// Check result type
is-ok(success)   // true
is-err(failure)  // true

// Pattern match (access value through the matched variable)
handle fn match (r: Result): Str {
    Result.Ok => { `Got: ${r}` }
    Result.Err => { `Error: ${r}` }
}

// Common pattern (return type is the success type, not Result)
safe-divide fn (a: Int, b: Int): Int {
    if(eq(b, 0), err("Division by zero"), ok(div(a, b)))
}
```

## Type Coercion

Define conversions between types with the `->` syntax:

```hot
// Define how Date converts to Str
Date type { year: Int, month: Int, day: Int }

Date -> Str fn (d: Date): Str {
    `${d.year}-${pad(d.month)}-${pad(d.day)}`
}

// Now Date automatically converts where Str is expected
today Date({year: 2024, month: 1, day: 15})
message `Today is ${today}`  // Uses the coercion
```

### Explicit Coercion

Use the type name as a function to explicitly convert:

```hot
num 42
text Str(num)  // "42"

str "123"
n Int(str)     // 123
```

### Multiple Coercions

A type can have multiple coercion targets:

```hot
User type { id: Str, name: Str, email: Str }

User -> Str fn (u: User): Str {
    `${u.name} <${u.email}>`
}

User -> Map fn (u: User): Map {
    {"id": u.id, "name": u.name, "email": u.email}
}
```

### Top-Level Only

Arrow declarations (`Source -> Target`) **must live at the top level of a
namespace** (`nested-type-implementation`). A nested arrow inside a
function body is rejected by the type checker because arrows mutate the
global implementation registry, so a nested arrow is a hidden global side
effect: another part of the program could pick up the implementation
without the user ever seeing it referenced near where it was declared.

```hot
::myapp::shipping ns

// Top level — OK
Inches type { value: Dec }
Centimeters type { value: Dec }

Inches -> Centimeters fn (i: Inches): Centimeters {
    Centimeters({value: mul(i.value, 2.54)})
}

normalize fn (i: Inches): Centimeters {
    // Inside a function body — using the arrow is fine.
    Centimeters(i)

    // But declaring one here would be nested-type-implementation:
    //   Inches -> Str fn (i: Inches): Str { `${i.value}in` }   // ❌ nested-type-implementation
}
```

Local **type** definitions (struct/enum/literal aliases) inside function
bodies are still allowed — those are scoped name resolutions and don't
mutate any cross-function registry. The rule is symmetrical with the
top-level-only rule for [open literal union extensions](#open-literal-unions):
every form of type-system declaration that has *global* effects (open
literal-union extensions, arrows) must be top-level.

If you find yourself wanting a nested arrow in a test for hygiene, lift
the (type, arrow) pair to the top level of the test's namespace and rely
on per-test type renames (e.g., `OrderForRecursionTest`) to keep them
distinct.

## Generic Types

Parameterize types with type variables:

```hot
// Generic function
identity fn (x: T): T { x }

// Constrained to specific types
first fn (items: Vec<T>): T {
    items[0]
}

// Multiple type parameters
pair fn (a: A, b: B): Map {
    {"first": a, "second": b}
}
```

### Generic Type Declarations

```hot
// Vec with element type
numbers: Vec<Int> [1, 2, 3]
names: Vec<Str> ["alice", "bob"]

// Map with key and value types
scores: Map<Str, Int> {"alice": 100, "bob": 95}

// Nested generics
matrix: Vec<Vec<Int>> [[1, 2], [3, 4]]

// Function types
mapper: Fn<Int, Str> (n) { Str(n) }
predicate: Fn<Str, Bool> (s) { gt(length(s), 0) }
```

## Type Inference

Hot infers types from usage:

```hot
// Inferred as Int
count 42

// Inferred as Vec<Int>
numbers [1, 2, 3]

// Inferred from function return
result add(1, 2)  // Int

// Inferred from map literal
config {"debug": true, "port": 8080}  // Map<Str, Any>
```

## Optional and Nullable Types

Use `?` suffix for optional types, or explicit union with `Null`:

```hot
// Optional field in struct
Config type {
    host: Str,
    port: Int?,      // Optional Int
    timeout: Int?
}

// Check for null
process fn (cfg: Config): Int {
    if(is-null(cfg.port), 8080, cfg.port)
}
```

## Type Aliases

Create aliases for complex types:

```hot
// Alias for a complex type
UserMap Map<Str, User>

// Use the alias
users: UserMap {}
```
