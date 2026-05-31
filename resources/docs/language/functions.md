---
description: "Define and call Hot functions, including typed parameters, return values, metadata, handlers, and reusable logic."
---

# Functions

In Hot, everything is a [Var/Value pair](/docs/language/vars-and-values)—function names are Vars, and function definitions are Values.

## Defining Functions

Use `fn` followed by parameters and a body:

{{snippet:functions#basic-functions}}

{{result:functions#basic-functions}}

The last expression in the body is the return value—no `return` keyword needed.

### Functions are Flows

The `fn` keyword modifies a **flow** to turn it into a function definition. The default flow is `serial`, which can be omitted:

{{snippet:functions#fn-equivalent-forms}}

You can use other flow types to change how the function body executes:

{{snippet:functions#fn-cond-example}}

See [Flows](/docs/language/flows) for more on `serial`, `parallel`, `cond`, and `cond-all`.

## Calling Functions

Call functions with parentheses:

{{snippet:functions#calling-functions}}

## Qualified Calls

Call functions from other namespaces using the full path:

```hot
upper ::hot::str/uppercase("hello")   // "HELLO"
data ::hot::http/get("https://api.example.com")
```

Or create a function alias:

```hot
uppercase ::hot::str/uppercase

result uppercase("hello")   // "HELLO"
```

Or create a namespace alias:

```hot
::str ::hot::str

result ::str/uppercase("hello")   // "HELLO"
```

## Parameter Types

Types are optional but recommended:

{{snippet:functions#param-types}}

## Nullable Parameters

Use `?` for types that accept null (shorthand for `Type | Null`):

{{snippet:functions#nullable-params}}

{{result:functions#nullable-params}}

## Function Overloading

Define multiple versions of a function with different parameter counts (arity):

{{snippet:functions#function-overloading}}

Or different parameter types:

{{snippet:functions#overload-by-arity}}

Hot dispatches to the correct version based on arguments.

## Variadic Functions

Accept any number of arguments with `...`:

{{snippet:functions#variadic-functions}}

## Lambdas (Anonymous Functions)

Create inline functions with `(params) { body }`:

{{snippet:functions#lambdas}}

{{result:functions#lambdas}}

Lambdas are just values—assign them to vars:

{{snippet:functions#lambda-as-var}}

### Placeholder Lambdas (`%`)

For single-parameter lambdas, use `%` as shorthand. Hot automatically wraps the expression in a lambda:

```hot
// These are equivalent:
map([1, 2, 3], (x) { mul(x, 2) })
map([1, 2, 3], mul(%, 2))

// Property access:
filter(users, (u) { u.active })
filter(users, %.active)

// In pipelines:
names |> map(%.email) |> filter(ends-with(%, "@company.com"))
```

For multi-parameter lambdas, use `%1`, `%2`, etc. (bare `%` is the same as `%1`):

```hot
reduce([1, 2, 3], add(%1, %2), 0)  // 6
```

#### Explicit Lambda Boundary: `%(expr)`

When `%` appears inside nested function calls, the implicit lambda wraps at the outermost call boundary. If that's not what you want, use `%(expr)` to mark exactly where the lambda should be created:

```hot
// Without %(expr) — implicit wrapping binds at sort, not map (wrong)
// sort(map(items, %.value))

// With %(expr) — lambda wraps at map (correct)
sort(map(items, %(%.value)))
length(filter(items, %(gt(%, 3))))
```

Rule of thumb: if `%` is an argument to a function that is **itself** an argument to another function, use `%(...)`.

Use explicit `(params) { body }` when:
- The lambda has multiple statements
- The parameter is unused (side-effect only)
- Clarity is more important than brevity

## Lazy Arguments

Arguments marked `lazy` aren't evaluated until needed. This enables short-circuit evaluation:

```hot
if fn
cond (pred: Any, lazy then: Any): Any {
  pred => { do then }
},
cond (pred: Any, lazy then: Any, lazy else: Any): Any {
  pred => { do then }
  => { do else }
}
```

Use `do` to evaluate a lazy argument:

{{snippet:functions#lazy-arguments}}

This is how `if`, `and`, and `or` avoid evaluating unused branches.

## Metadata on Functions

Add documentation, test markers, or event handlers:

```hot
// Documentation
greet meta {doc: "Greets a user by name"}
fn (name: Str): Str {
  `Hello, ${name}!`
}

// Test function
test-greet meta ["test"]
fn () {
  assert-eq(greet("World"), "Hello, World!")
}

// Event handler
on-user-created meta {on-event: "user:created"}
fn (event) {
  send-welcome-email(event.data.email)
}

// Scheduled function
daily-cleanup meta {schedule: "@daily"}
fn (event) {
  cleanup-old-records()
}
```

## Core Functions

These functions are available everywhere without imports. See [hot-std](/pkg/hot-std) for full documentation.

**[Math](/pkg/hot-std/hot/math)**: `add`, `sub`, `mul`, `div`, `mod`, `pow`, `round`, `floor`, `ceil`, `rand`

**[Comparison](/pkg/hot-std/hot/cmp)**: `eq`, `ne`, `lt`, `gt`, `lte`, `gte`

**[Logic](/pkg/hot-std/hot/bool)**: `if`, `and`, `or`, `not`, `is-truthy`

**[Collections](/pkg/hot-std/hot/coll)** (eager): `map`, `filter`, `reduce`, `first`, `rest`, `last`, `length`, `concat`, `flatten`, `merge`, `keys`, `vals`, `some`, `all`, `range`, `sort`, `reverse`, `distinct`, `slice`

**[Iterators](/pkg/hot-std/hot/iter)** (lazy): `Iter`, `next`, `collect`, `for-each`, `take`, `range`

**[Strings](/pkg/hot-std/hot/str)**: `uppercase`, `lowercase`, `trim`, `split`, `join`, `starts-with`, `ends-with`, `contains`, `replace`

**[Results](/pkg/hot-std/hot/type)**: `ok`, `err`, `is-ok`, `is-err`, `Result`

**[Types](/pkg/hot-std/hot/type)**: `Str`, `Int`, `Dec`, `Bool`, `Vec`, `Map`, `Any`, `Null`, `is-null`, `is-some`

## Tail Call Optimization (TCO)

Hot automatically optimizes tail-recursive functions, enabling stack-safe recursion for any depth.

A call is in **tail position** when its result is returned directly without further processing:

{{snippet:functions#tco-factorial}}

{{result:functions#tco-factorial}}

Use the accumulator pattern to make functions tail-recursive:

```hot
// NOT tail-recursive (result passed to add, not returned)
sum fn (xs: Vec): Int {
  if(is-empty(xs), 0, add(first(xs), sum(rest(xs))))
}

// Tail-recursive with accumulator (stack-safe)
sum fn (xs: Vec): Int { sum-acc(xs, 0) }
sum-acc fn cond (xs: Vec, acc: Int): Int {
  is-empty(xs) => { acc }
  => { sum-acc(rest(xs), add(acc, first(xs))) }
}
```

## Summary

- `fn` modifies a flow to become a function (default is `serial`, omittable)
- Use `fn cond` for conditional functions, `fn parallel` for concurrent execution
- Call with no space before `(`: `func(args)`
- Overload by arity or type
- Use lambdas `(x) { body }` for inline functions, or `%` for concise single-param lambdas
- Mark args `lazy` for deferred evaluation
- Tail-recursive functions are automatically optimized (TCO)
