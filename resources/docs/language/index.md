---
description: "Learn how Hot programs are organized: functions, values, types, metadata, flows, errors, and core syntax."
---

# Hot Language

Hot is a functional, expression-based language designed for backend workflows. It combines familiar JavaScript-like syntax with powerful features for concurrent execution and data transformation.

## Core Philosophy

Hot is built around a few simple ideas:

1. **Everything is a Var or a Value** — Variables bind names to values, and all values are immutable
2. **Familiar data literals** — If you know JSON, you know Hot's data syntax
3. **Flows control execution** — Parallel, serial, and conditional execution patterns via function modifiers
4. **Types are optional but powerful** — Add types where they help, skip them where they don't
5. **Eager and lazy evaluation** — Collection functions for immediate results, iterators for streaming/large data

## Quick Example

```hot
::myapp::hi ns

// A simple function with no arguments
hello fn (): Str {
  "Things are heating up!"
}

// Functions can take arguments with type annotations
warm-welcome fn (name: Str): Str {
  `Welcome to the heat, ${name}!`  // Backticks for template strings (also ```...``` for indent-aware blocks)
}

// The `if` function has lazy branches, only executing the true branch
// Notice there are no infix operators in Hot--only functions
is-it-hot fn (temp: Int): Str {
  if(gte(temp, 100), "It's hot!", "Not hot yet")
}

// The `cond` flow returns the first true branch
check-heat fn cond (num: Int): Str {
  is-zero(mod(num, 2)) => `${num} is even`
                       => `${num} is odd`
}

// The pipe operator `|>` chains functions. Piped value becomes the first argument.
// Lambdas are written as `(param) { body }`.
double-positives fn (nums: Vec<Int>): Vec<Int> {
  nums
  |> filter((n) { gt(n, 0) })
  |> map((n) { mul(n, 2) })
  // Shorthand: `%` creates a lambda automatically.
  // The above is equivalent to:  filter(gt(%, 0)) |> map(mul(%, 2))
}
```

## Language Guide

- **[Vars and Values](/docs/language/vars-and-values)** — Understanding Hot's core building blocks
- **[Data Literals](/docs/language/data-literals)** — Strings, numbers, vectors, maps, and more
- **[Functions](/docs/language/functions)** — Defining and calling functions
- **[Types](/docs/language/types)** — Optional typing, type definitions, and constructors
- **[Error Handling](/docs/language/errors)** — Result types, automatic unwrapping, and lazy evaluation
- **[Flows](/docs/language/flows)** — Controlling execution with cond, parallel, and pipes
- **[What Hot Doesn't Have](/docs/language/not-supported)** — Syntax you won't find in Hot

## Key Differences from Other Languages

| Other Languages | Hot |
|----------------|-----|
| `a + b` | `add(a, b)` |
| `a == b` | `eq(a, b)` |
| `if (x) { } else { }` | `if(x, then, else)` or `cond { x => then => else }` |
| `for x in items` | `map(items, (x) { ... })` or `map(items, %)` |
| `while (cond)` | Tail-recursive function (TCO-enabled) |
| `array` / `object` | `Vec` (vector) / `Map` |
| `float` | `Dec` (decimal) |
| `throw Error` | `err("message")` |
| Eager streams | Lazy `Iter` with `next`, `collect` |

Hot trades familiar syntax for simplicity and power. Once you internalize the functional style, you'll find it remarkably consistent.

## Comments

Hot supports both single-line and multi-line comments:

```hot
// Single-line comment

/* Multi-line comment
   can span multiple lines */

name /* inline comment */ "Alice"
```
