# Vars and Values

In Hot, everything is either a **Var** (a named binding) or a **Value**. This simple model is the foundation of the language.

## Values

A Value is anything that can be bound to a Var. Values fall into three categories: **data**, **definitions**, and **references**.

### Data

The most common Values are literal data:

| Type | Example | Description |
|------|---------|-------------|
| `Str` | `"hello"`, `` `template` ``, `"""block"""`, `` ```block template``` `` | Text strings (template strings support `${}` interpolation, block strings are indent-aware) |
| `Int` | `42` | Whole numbers |
| `Dec` | `19.99` | Decimal numbers (not floats!) |
| `Bool` | `true`, `false` | Boolean values |
| `Null` | `null` | Absence of value |
| `Vec` | `[1, 2, 3]` | Ordered collections (vectors) |
| `Map` | `{a: 1}` | Key-value collections (like objects) |
| `Fn` | `(x) { x }` | Anonymous functions |

### Definitions

Values can also be **definitions** — they introduce new types or named functions:

```hot
// Function definition
greet fn (name: Str): Str {
    `Hello, ${name}!`
}

// Type definition
User type { id: Str, email: Str, active: Bool }

// Enum definition
Direction enum { Up, Down, Left, Right }
```

Definitions are still Values—a `fn` definition produces a function Value, and a `type` definition produces a type constructor.

### References

A Value can be a **reference** to a Var or namespace defined elsewhere:

```hot
// Namespace alias — binds a short name to an existing namespace
::http ::hot::http

// Var import — binds a local name to a Var from another namespace
send-email ::notifications::email/send

// Var reference — binds a new name to an existing Var's Value
role ::myapp::users/default-role
```

References are covered in more detail in the [Namespaces](#namespaces) section below.

All Values are **immutable**. You don't modify data; you create new data.

## Vars

A Var is a name bound to a Value. Unlike most languages, Hot uses **no `=` sign** for assignment:

{{snippet:vars#var-assignment}}

Think of it as "name is Value" rather than "name equals Value".

### With Type Annotations

You can optionally add a type between the name and Value:

{{snippet:vars#var-typed}}

### Deep Path Assignment

Assign to nested paths to build up Maps:

{{snippet:vars#deep-path-assign}}

Deep paths also work with Vec indices:

{{snippet:vars#deep-path-vec}}

### Appending to Vectors

Use empty brackets to append to a vector:

{{snippet:vars#vec-append}}

```hot
// list = ["first", "second", "third"]
```

Like all Hot operations, this follows immutable semantics. Each append creates a **new** vector and rebinds the variable—it doesn't mutate the original. The syntax is shorthand for:

```hot
list []                    // list = []
list concat(list, ["first"])   // list = ["first"] (new vector, rebind)
list concat(list, ["second"])  // list = ["first", "second"] (new vector, rebind)
```

This also works with nested paths:

{{snippet:vars#vec-append-nested}}

```hot
// shopping = {items: ["apple", "banana", "cherry"]}
```

Each nested append creates a new outer structure with the updated inner vector.

### Deep Path Access

Read from nested paths into a new Var:

{{snippet:vars#deep-path-access}}

{{result:vars#deep-path-access}}

This works with Vec indices too:

{{snippet:vars#vec-access}}

## Namespaces

Every Var lives in a namespace. Each Hot file declares its namespace with `ns` keyword:

```hot
::myapp::users ns

// These Vars are in the ::myapp::users namespace
default-role "member"
max-users 1000
```

### Referencing Vars

Use the full path `::namespace/var-name` to reference Vars from other namespaces:

```hot
// Reference a Var from another namespace
role ::myapp::users/default-role
```

### Namespace Aliases

Create shorter names for frequently-used namespaces:

```hot
// Create aliases
::http ::hot::http
::env ::hot::env

// Now use the short form
api-url ::env/get("API_URL")
response ::http/get(api-url)
```

### Importing Specific Items

Import individual Vars/functions into your namespace:

```hot
::myapp::handlers ns

// Import specific items
HttpResponse ::hot::http/HttpResponse
send-email ::notifications::email/send

// Use without namespace prefix
response HttpResponse({status: 200})
```

## Core Vars

Vars marked with `core: true` in their metadata are available everywhere without namespace qualification.

Hot's standard library uses this extensively:

```hot
::myapp::example ns

// These are core - no prefix needed
doubled map([1, 2, 3], mul(%, 2))
total add(1, 2)
name Str(42)

// Equivalent to:
// doubled ::hot::coll/map(...)
// total ::hot::math/add(...)
// name ::hot::type/Str(...)
```

Core functions from hot-std include: `map`, `filter`, `reduce`, `add`, `sub`, `mul`, `div`, `eq`, `lt`, `gt`, `if`, `and`, `or`, `not`, `Str`, `Int`, `Dec`, `ok`, `err`, and many more.

### Making Your Own Core Vars

You can mark your own Vars as core to make them available throughout your application without namespace prefixes. This is powerful for domain-specific languages or frequently-used utilities:

```hot
::myapp::domain ns

// Mark a function as core
send-notification
meta {
    core: true,
    doc: "Send a notification to a user."
}
fn (user-id: Str, message: Str): Result {
    // implementation
}

// Mark a type constructor as core
UserId
meta {core: true}
type fn (id: Str): UserId { id }
```

Now any file in your application can use these without imports:

```hot
::myapp::handlers ns

// No import needed - these are core
result send-notification("user-123", "Welcome!")
id UserId("user-456")
```

This lets you bend the language to your domain, making common operations feel built-in.

## Immutability

All bindings are immutable. You cannot reassign a Var:

```hot
count 1
count 2  // This creates a NEW binding, shadowing the first
```

This isn't "changing" count—it's creating a new Var that shadows the previous one. In most contexts, you'll work with transformations that produce new Values:

{{snippet:vars#immutability}}

{{result:vars#immutability}}

## Summary

- **Values** are data (strings, numbers, vectors, maps...), definitions (`fn`, `type`, `enum`), or references to other namespaces and Vars
- **Vars** bind names to Values using `name Value` syntax (no `=`)
- **Namespaces** organize Vars with `::path::to::namespace`
- **Core** functions are available everywhere without qualification
- Everything is **immutable** — create new Values, don't modify existing ones
