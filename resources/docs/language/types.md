---
description: "Define Hot types, structs, enums, optionals, collections, conversions, and type-safe data contracts."
---

# Types

Hot has an optional type system inspired by TypeScript. Add types where they help catch errors and document intent; skip them where they add noise.

## Built-in Types

| Type | Description | Example |
|------|-------------|---------|
| `Str` | Text strings | `"hello"`, `` `template` ``, `"""block"""`, `` ```block template``` `` |
| `Int` | Integers | `42` |
| `Dec` | Decimal numbers | `19.99` |
| `Bool` | Booleans | `true`, `false` |
| `Null` | Null value | `null` |
| `Vec` | Vectors (also known as arrays) | `[1, 2, 3]` |
| `Map` | Maps (objects) | `{a: 1}` |
| `Fn` | Functions | `(x) { x }` |
| `Any` | Any type | anything |
| `Bytes` | Binary data | — |

## Type Annotations

Add types to variables:

{{snippet:types#type-annotations-vars}}

Add types to function parameters and returns:

{{snippet:types#type-annotations-fn}}

{{result:types#type-annotations-fn}}

## Types are Optional

You can skip types entirely:

{{snippet:types#types-optional-simple}}

Hot will infer types where possible and allow `Any` elsewhere.

## Generic Types

Parameterize collection types:

{{snippet:types#generic-types}}

## Union Types

A value can be one of several types:

```hot
parse-number fn (input: Str): Int | Dec | Null {
  // Returns Int, Dec, or null
}

process fn (value: Str | Int): Str {
  Str(value)
}
```

### Literal Unions

Union types can include literal values:

```hot
// String literals
Fruit type "apple" | "banana" | "orange"

// Number literals
DiceRoll type 1 | 2 | 3 | 4 | 5 | 6

// Mixed literals
Status type "pending" | "active" | 0 | 1

// Use in functions
pick-fruit fn (fruit: Fruit): Str {
  `You picked a ${fruit}!`
}

apple: Fruit "apple"   // Valid
pick-fruit("banana")   // Valid
pick-fruit("grape")    // Type error
```

Literal unions let you restrict values to a specific set of allowed values at the type level.

## Nullable Types

Use `?` for values that might be null:

```hot
find-user fn (id: Str): User? {
  // Returns User or null
}

greet fn (name: Str, title: Str?): Str {
  // title can be a Str or null
}
```

`Str?` is shorthand for `Str | Null`.

> **Note:** The `?` syntax indicates the *type* accepts null—it does not make the parameter omittable. You must still pass an argument (either a value or `null`). For truly optional parameters, use function overloading.

> **Why no `Option` type?** Languages without null (like Rust) use `Option<T>` with `Some(value)` and `None` to represent optional values. Hot has null to align with JavaScript/JSON data types, so `T?` achieves the same thing more concisely. While `Option` can technically express one extra state (`Some(null)` vs `None`), this distinction is rarely needed in practice and adds complexity for everyone. If an `Option` type is needed, you can define a custom type and make it [core](/docs/language/meta#core-functions) to your codebase.

## Defining Custom Types

### Struct Types

Define a type with fields:

{{snippet:types#custom-type-struct}}

### Types Are Constructors

The type name is also its constructor function:

{{snippet:types#type-as-constructor}}

{{result:types#type-as-constructor}}

> **Key Concept:** In Hot, **types are functions that return values of that type**. When you define a type, you're also defining a constructor function with the same name. This unifies type definitions and value creation into a single concept.

### Types with Custom Constructors

Add a constructor function for validation or convenience by combining the struct definition with a function:

{{snippet:types#type-custom-constructor}}

{{result:types#type-custom-constructor}}

You can also define multiple constructor arities:

{{snippet:types#type-custom-constructor-overloaded}}

{{result:types#type-custom-constructor-overloaded}}

### Empty Types (Markers)

Types with no fields work as markers or tags:

{{snippet:types#empty-types}}

## Enums (Variant Unions)

Define a type with multiple named variants using the `enum` keyword:

{{snippet:types#enum-simple}}

### Variants with Data

Variants can carry data by referencing other types:

{{snippet:types#enum-with-data}}

### Built-in Variant Types

Hot uses variant unions for core types:

```hot
// Result has Ok and Err variants
success Result.Ok(42)
failure Result.Err("Not found")
```

### Type-Level and Variant-Level Matching

Use `match` to check variant types (preferred):

{{snippet:types#match-variant}}

Use `is-type` for dynamic type checking:

{{snippet:types#type-checking}}

### Exhaustive Matching

`match` on a closed enum must cover every variant or fall through to a `_`
default arm. The compiler reports `[non-exhaustive-match]` and lists
the missing variants if neither holds.

```hot
Direction enum { Up, Down, Left, Right }

// Exhaustive — every variant has an arm
travel fn match (d: Direction): Str {
  Direction.Up => { "north" }
  Direction.Down => { "south" }
  Direction.Left => { "west" }
  Direction.Right => { "east" }
}

// Also exhaustive — `_` catches the rest
classify fn match (d: Direction): Str {
  Direction.Up => { "vertical" }
  Direction.Down => { "vertical" }
  _ => { "horizontal" }
}
```

### Open Enums

Add the `open` modifier to declare an extensible enum. Other modules add
new variants by declaring an arrow `Source -> Enum.Variant`. An open enum
with no seed variants is a pure extension point:

```hot
// Pure extension point — no initial variants
Plugin enum open { }

// Open enum with some seed variants
Animal enum open {
  Dog,
  Cat
}
```

Because the variant set of an open enum is unbounded, `match` on an open
enum **must** include a `_` default arm — the compiler reports
`[open-enum-match-missing-default]` otherwise.

```hot
greet-animal fn match (a: Animal): Str {
  Animal.Dog => { "woof" }
  Animal.Cat => { "meow" }
  _ => { "hello, friend" }   // required for open enums
}
```

### Arrow Enrollment

A single arrow declaration both **enrolls a variant** in an open enum and
**synthesizes its constructor**. The bodyless form is the idiomatic
shorthand:

```hot
Bird type { species: Str, wingspan: Dec }
Bird -> Animal.Bird           // bodyless — wraps the source value as-is

eagle Animal.Bird({species: "Eagle", wingspan: 2.1})
```

The variant name does not need to match the source type name. This is a
**role-shaped variant**, useful when a domain-specific tag reads better
than the data type name:

```hot
Lizard type { length-cm: Dec }
Lizard -> Animal.Reptile      // tag is Reptile, payload is Lizard
```

Declaring the same arrow twice — even from different files — is a
compile error (`[ambiguous-type-implementation]`) and the message points
to both source spans.

## Type Coercion

Define how types convert to each other using `->`:

{{snippet:types#type-coercion}}

{{result:types#type-coercion}}

Multiple coercions:

```hot
Temperature type { celsius: Dec }

Temperature -> Str fn (temp: Temperature): Str {
  `${temp.celsius}°C`
}

Temperature -> Int fn (temp: Temperature): Int {
  round(temp.celsius)
}

temp Temperature({celsius: 23.7})
Str(temp)   // "23.7°C"
Int(temp)   // 24
```

Arrow declarations (`Source -> Target`) **must live at the top level of a
namespace** (`[nested-type-implementation]`). Arrows mutate the
global implementation registry, so a nested arrow inside a function body
would be a hidden global side effect — another part of the program could
pick up the implementation without ever seeing it referenced near where
it was declared. Local **type** definitions inside function bodies are
still allowed; the rule applies only to arrows.

## Result Types

Hot doesn't have exceptions. Instead, use `Result` for operations that can fail:

```hot
// Create results explicitly
success Result.Ok(42)
failure Result.Err("Not found")

// Using shorthand functions
success ok(42)
failure err("Not found")

// Check results
result safe-divide(10, 2)
if(is-ok(result),
  `Result: ${result}`,
  `Error: ${result}`)
```

The `Result` type is an enum with `Ok` and `Err` variants:

```hot
Result enum {
  Ok(Any),
  Err(Any)
}

success Result.Ok(42)
`Value: ${success}`   // "Value: 42" (auto-unwraps in templates)
```

Results automatically unwrap when used as function arguments—Ok values pass through, Err values halt execution. This makes error propagation seamless.

See **[Error Handling](/docs/language/errors)** for the full story on Result types, automatic unwrapping, and lazy evaluation.

## Type Checking

Use `is-*` functions to check built-in types at runtime:

```hot
is-str(value)    // true if Str
is-int(value)    // true if Int
is-vec(value)    // true if Vec
is-map(value)    // true if Map
is-fn(value)     // true if function
is-null(value)   // true if null
is-some(value)   // true if not null
```

For custom types, use `is-type`:

{{snippet:types#type-checking}}

## The `untype` Function

Internally, Hot types are represented as Maps with special `$type` and `$val` keys. Most of the time, you don't need to think about this—Hot handles it transparently.

However, when data leaves the Hot system (over the wire, to a database, etc.), you may want to strip this metadata using `untype`:

```hot
// Define a type
Person type { name: Str, age: Int }

// Create a typed value
alice Person({name: "Alice", age: 30})

// Internally, alice looks like:
// {$type: "Person", $val: {name: "Alice", age: 30}}

// Strip the type metadata
untype(alice)  // {name: "Alice", age: 30}
```

### When to Use `untype`

The most common use case is serializing typed data to JSON for HTTP requests:

```hot
// Without untype, the JSON would include $type/$val metadata
to-json(alice)  // {"$type":"Person","$val":{"name":"Alice","age":30}}

// With untype, you get clean JSON
to-json(untype(alice))  // {"name":"Alice","age":30}
```

This is especially important when calling external APIs that expect clean JSON payloads:

```hot
// Sending typed data to an external API
request-body ChatRequest({
  model: "gpt-4",
  messages: [{role: "user", content: "Hello"}]
})

// Untype before converting to JSON
::hot::http/request("POST", url, headers, to-json(untype(request-body)))
```

### Recursive Untyping

The `untype` function works recursively—it strips type metadata from nested types as well:

```hot
Order type { customer: Person, items: Vec<Item> }

order Order({
  customer: Person({name: "Bob", age: 25}),
  items: [Item({name: "Widget", price: 9.99})]
})

// Recursively removes all type metadata
untype(order)
// {customer: {name: "Bob", age: 25}, items: [{name: "Widget", price: 9.99}]}
```

## Summary

- Types are **optional** — add them where they help
- **Types are constructors**: `Person({name: "Alice"})` creates a Person
- Use `?` for optional/nullable types: `Str?`
- Use `|` for union types: `Int | Str`
- Use **literal unions** for exact value sets: `"apple" | "banana"`
- Use **enums** (variant unions) for discriminated types: `Direction enum { Up, Down }`
- Define type coercions with `Type -> OtherType fn`
- Use `Result` with `Result.Ok()`/`Result.Err()` instead of exceptions (see [Error Handling](/docs/language/errors))
- Use `untype` to strip type metadata when serializing data for external systems
