---
description: "See what Hot intentionally leaves out, including syntax and language features replaced by Hot-specific patterns."
---

# What Hot Doesn't Have

Hot intentionally omits certain syntax found in other languages. This isn't a limitation—it's a design choice that keeps the language simple and consistent.

## No Infix Operators

Hot has no `+`, `-`, `*`, `/`, `==`, `!=`, `<`, `>`, `&&`, `||`, etc.

| Instead of | Use |
|------------|-----|
| `a + b` | `add(a, b)` |
| `a - b` | `sub(a, b)` |
| `a * b` | `mul(a, b)` |
| `a / b` | `div(a, b)` |
| `a % b` | `mod(a, b)` |
| `a == b` | `eq(a, b)` |
| `a != b` | `ne(a, b)` |
| `a < b` | `lt(a, b)` |
| `a > b` | `gt(a, b)` |
| `a <= b` | `lte(a, b)` |
| `a >= b` | `gte(a, b)` |
| `a && b` | `and(a, b)` |
| `a \|\| b` | `or(a, b)` |
| `!a` | `not(a)` |

**Why?** Consistency. Everything is a function call, with predictable evaluation order and no operator precedence to remember.

> **Watch out:** `%` is not the modulo operator in Hot — it's the [lambda placeholder](/docs/language/functions#placeholder-lambdas). Use `mod(a, b)` for remainders.

## No Assignment Operator

Hot has no `=` for assignment.

| Instead of | Use |
|------------|-----|
| `name = "Alice"` | `name "Alice"` |
| `count = 42` | `count 42` |

Variables are declared by placing the name before the value.

## No If/Else Blocks

Hot has no `if`/`else` statement syntax.

| Instead of | Use |
|------------|-----|
| `if (x) { a } else { b }` | `if(x, a, b)` |
| `if (x) { a }` | `if(x, a)` |

Or use `cond` for multiple conditions:

```hot
cond {
  lt(x, 0) => "negative"
  eq(x, 0) => "zero"
  => "positive"
}
```

## No Loops

Hot has no `for`, `while`, `do-while`, or any loop constructs.

| Instead of | Use |
|------------|-----|
| `for (x of items)` | `map(items, (x) { ... })` |
| `items.filter(...)` | `filter(items, (x) { ... })` |
| `items.reduce(...)` | `reduce(items, (acc, x) { ... }, init)` |
| `items.forEach(...)` | `for-each(items, (x) { ... })` |
| `while (cond) { }` | Tail-recursive function |

**Why?** Loops imply mutation. Functional transformations are clearer and parallelize better.

### Tail Call Optimization (TCO)

Hot has automatic TCO for tail-recursive functions. Use the accumulator pattern for custom iteration:

```hot
// Tail-recursive - stack-safe for any list size
sum-list fn cond (xs: Vec, acc: Int): Int {
  is-empty(xs) => { acc }
  => { sum-list(rest(xs), add(acc, first(xs))) }
}

sum-list([1, 2, 3, 4, 5], 0)  // 15
```

This works for arbitrarily large collections without stack overflow.

## No Classes or Interfaces

Hot has no `class`, `interface`, `extends`, or `implements`.

| Instead of | Use |
|------------|-----|
| `class User { }` | `User type { name: Str }` |
| `new User()` | `User({name: "Alice"})` |
| `interface Printable` | Type coercion: `Type -> Str` |
| `extends BaseClass` | Composition |

Types in Hot are data definitions, not behavior containers. Add behavior with functions:

```hot
User type { name: Str, email: Str }

// Functions that work on User
greet-user fn (user: User): Str {
  `Hello, ${user.name}!`
}

// Type coercion for "interface-like" behavior
User -> Str fn (user: User): Str {
  `${user.name} <${user.email}>`
}
```

## No Exceptions

Hot has no `throw`, `catch`, `finally`, or `try { ... } catch { ... }` blocks —
and no `try(...)` function either. Expected failures are `Result.Err` values
you branch on; `fail()` signals a broken invariant and halts the run or task
(there is no catch). To supervise work that may halt, run it behind a task
boundary (`::hot::task/start` + `await`) — see
[Error Handling](/docs/language/errors).

| Instead of | Use |
|------------|-----|
| `throw new Error("msg")` | `err("msg")` or `Result.Err("msg")` |
| `try { } catch { }` | `if(is-ok(result), ..., ...)` or `match` |

Use `Result` types for error handling:

```hot
safe-divide fn (a: Int, b: Int): Int {
  if(eq(b, 0),
    err("Division by zero"),
    div(a, b))
}

result safe-divide(10, 0)

// Use match for pattern matching on Result
match result {
  Result.Ok => log(`Result: ${result}`)
  Result.Err => log(`Error: ${result}`)
}
```

## No Mutable Variables

Hot has no `let`, `var`, or reassignment.

```hot
count 1
count 2  // Creates a NEW binding, shadows the first
```

This isn't mutation—it's creating a new variable that shadows the old one. For accumulating values, use `reduce`:

```hot
// Instead of: let sum = 0; for (x of items) { sum += x; }
total reduce(items, (sum, x) { add(sum, x) }, 0)
```

## No Return Statement

The last expression in a function body is the return value:

```hot
sum fn (a: Int, b: Int): Int {
  add(a, b)  // This is returned
}
```

No `return` keyword exists.

## No Ternary Operator

Hot has no `? :` ternary.

| Instead of | Use |
|------------|-----|
| `x ? a : b` | `if(x, a, b)` |

## Summary: The Hot Way

| Concept | Other Languages | Hot |
|---------|----------------|-----|
| Math | `a + b * c` | `add(a, mul(b, c))` |
| Comparison | `a == b && c > d` | `and(eq(a, b), gt(c, d))` |
| Conditionals | `if/else` blocks | `if()` function or `cond` |
| Loops | `for`, `while` | `map`, `filter`, `reduce` |
| Objects | `class` + `new` | `type` + constructor |
| Errors | `throw`/`catch` | `Result.Err()`/`match` |
| Mutation | `x = x + 1` | `new-x add(x, 1)` |

The tradeoff: Hot code looks different from JavaScript/Python/etc. The benefit: Complete consistency, easier parallelization, and no hidden complexity.
