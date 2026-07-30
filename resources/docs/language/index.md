---
description: "Get a quick introduction to Hot syntax and find the detailed language guides."
---

# Hot Language

Hot is an expression-oriented language for backend workflows. It keeps
JSON-shaped data familiar, uses functions instead of infix operators, and
provides flows for conditional and concurrent work.

## Quick Example

```hot
::myapp::hi ns

greet fn (name: Str): Str {
  `Hello, ${name}!`
}

message greet("Hot")
```

## What Looks Different

| Other Languages | Hot |
|----------------|-----|
| `name = "Ada"` | `name "Ada"` |
| `a + b` | `add(a, b)` |
| `if (x) { } else { }` | `if(x, then, else)` or `cond { x => then => else }` |
| `return value` | The final expression is the value |
| `for x in items` | `map(items, ...)` or `for-each(iter, ...)` |

## Language Guide

- **[Vars and Values](/docs/language/vars-and-values)** — Bindings, namespaces,
  immutable values, and deep paths
- **[Data Literals](/docs/language/data-literals)** — Strings, numbers, vectors,
  maps, templates, and comments
- **[Functions](/docs/language/functions)** — Functions, calls, lambdas, and
  lazy parameters
- **[Types](/docs/language/types)** — Gradual typing, constructors, enums,
  unions, generics, and coercions
- **[Error Handling](/docs/language/errors)** — Results, propagation, and
  explicit failure handling
- **[Flows](/docs/language/flows)** — Serial, conditional, matching, pipe, and
  parallel flows
- **[Language Evaluation Model](/docs/language/execution-model)** — How those
  rules compose while Hot code is running
- **[What Hot Doesn't Have](/docs/language/not-supported)** — Conventional
  syntax and constructs that Hot replaces
