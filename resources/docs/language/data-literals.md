# Data Literals

Hot uses JavaScript-like syntax for data literals, making it instantly familiar if you've worked with JSON. There are a few important differences to be aware of.

## Strings

Double-quoted strings, just like JSON:

{{snippet:data-literals#strings-basic}}

### Template Strings

Use backticks for string interpolation:

{{snippet:data-literals#template-strings}}

{{result:data-literals#template-strings}}

Any expression can go inside `${}`.

### Block Strings

For content where you don't want escape processing, use block strings (`"""`):

{{snippet:data-literals#block-strings}}

Block strings have **no escape processing** — escape sequences (`\n`, `\"`, etc.) are not interpreted, so characters are treated as literal text. They're also **indent-aware**: the closing `"""` determines the base indentation, which is automatically stripped from all lines. This makes them ideal for documentation, SQL queries, HTML templates, and embedded code examples.

### Block Template Strings

For multi-line content that needs both indent-awareness and `${}` interpolation, use block template strings:

{{snippet:data-literals#block-template-strings}}

Block template strings combine the best of both worlds: **indent-aware** like block strings (the closing ` ``` ` determines the base indentation to strip) and **interpolation** like template strings (`${}` expressions are evaluated). Like block strings, they have **no escape processing**. This makes them ideal for shell scripts, HTML templates, and embedded code that needs Hot variables.

## Numbers

Hot has two number types: **Int** and **Dec**.

### Int (Integers)

Whole numbers without decimal points:

{{snippet:data-literals#integers}}

### Dec (Decimals)

Numbers with decimal points. Hot uses `Dec` instead of floating-point:

{{snippet:data-literals#decimals}}

**Why Dec instead of Float?**

Floating-point math has precision issues:

```javascript
// In JavaScript: 0.1 + 0.2 = 0.30000000000000004
```

Hot's `Dec` type uses 256-bit decimal arithmetic (via [fastnum D256](https://docs.rs/fastnum)) providing up to **76 digits of precision**. This means exact decimal arithmetic—critical for money, percentages, scientific calculations, and anywhere precision matters.

{{snippet:data-literals#dec-precision}}

{{result:data-literals#dec-precision}}

## Booleans

{{snippet:data-literals#booleans}}

## Null

The absence of a value:

{{snippet:data-literals#null-values}}

## Vectors

Ordered collections using square brackets:

{{snippet:data-literals#vectors}}

Nested vectors:

{{snippet:data-literals#vec-nested}}

Access elements with `first()`, `last()`, or index notation:

{{snippet:data-literals#vec-access}}

> **Note:** Hot uses `Vec` (vector) where other languages use "array". Same concept, different name.

### Vec Spread

Use `...` to flatten existing vectors into a new vector literal:

```hot
a [1, 2, 3]
b [4, 5]
combined [...a, ...b]           // [1, 2, 3, 4, 5]
with-extras [0, ...a, 99]      // [0, 1, 2, 3, 99]
clone [...a]                    // [1, 2, 3] (shallow copy)
```

Spread elements are flattened inline. Non-spread elements and spread elements can be freely mixed. For simple concatenation without extra elements, you can also use `concat(a, b)`.

## Maps (like Objects)

Key-value collections using curly braces:

{{snippet:data-literals#maps}}

Nested maps:

{{snippet:data-literals#maps-nested}}

Access properties with dot notation:

{{snippet:data-literals#map-access}}

{{result:data-literals#map-access}}

**Map vs Object**: Hot calls these `Map` instead of Object. Keys are always strings.

### Map Spread

Use `...` to merge existing maps into a new map literal:

```hot
defaults {timeout: 5000, retries: 3}
config {...defaults, retries: 5}    // {timeout: 5000, retries: 5}
```

Later keys win, so spread entries can be selectively overridden. Multiple spreads and explicit keys can be mixed in any order.

## Comparison to JavaScript/JSON

| Concept | JavaScript/JSON | Hot |
|---------|----------------|-----|
| Array | `[1, 2, 3]` | `[1, 2, 3]` (same syntax, called `Vec`) |
| Object | `{"a": 1}` | `{a: 1}` (unquoted keys, called `Map`) |
| String | `"hello"` | `"hello"` (same!) |
| Template string | `` `Hi ${name}` `` | `` `Hi ${name}` `` (same!) |
| Block string | N/A | `"""..."""` (no escaping, indent-aware) |
| Block template string | N/A | `` ```...``` `` (indent-aware `${}` interpolation) |
| Integer | `42` | `42` (same!) |
| Float | `3.14` | `3.14` (but it's `Dec`!) |
| Boolean | `true`/`false` | `true`/`false` (same!) |
| Null | `null` | `null` (same!) |

## Type Annotations on Literals

You can add types to your data for documentation and type checking:

{{snippet:data-literals#type-annotations}}

## Summary

- Hot's data literals are **nearly identical to JSON**
- `Vec` (vector) for ordered collections, `Map` for key-value collections
- Use `Dec` for decimals (not floating-point) — exact precision
- Template strings use backticks with `${expression}` interpolation
- Block strings (`"""..."""`) — no escape processing, indent-aware
- Block template strings (`` ```...``` ``) for indent-aware content with `${}` interpolation
- Access vectors with `[index]`, maps with `.property`
