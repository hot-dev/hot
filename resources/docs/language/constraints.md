---
description: "Constrain Hot types, parameters, return values, and bindings with `where` predicates that are checked at every boundary."
---

# Constraints

A `where` clause refines a type with a predicate. A value of the type must
pass the predicate wherever it enters the type: a typed binding, a function
parameter, a return value, a constructor, or an update. A value that fails
becomes an ordinary `Result.Err` holding a `ConstraintError`.

## Named Refinements

Add `where` and a predicate to a type declaration. `%` is the value being
checked:

{{snippet:constraints#named-refinements}}

{{result:constraints#named-refinements}}

A refinement's values are plain values of its base type. There is no type tag:
arithmetic, equality, and JSON work as they do for the base type. `is-type`
checks the base type and runs the predicate.

{{snippet:constraints#refinement-values}}

{{result:constraints#refinement-values}}

A struct keeps its type tag. For a constrained value with its own identity,
use a struct with a `where` clause.

## Fields and Invariants

A `where` can follow any field type, an element type, or a whole struct. A
`where` after the closing brace is an invariant of the whole record. It can go
on the same line as the brace or on the next one.

{{snippet:constraints#fields-and-invariants}}

{{result:constraints#fields-and-invariants}}

Checks run innermost first: each element and field, then the record's
invariant. The error's `path` leads to the part that failed.

An enum variant's payload is checked the same way. It can name a refinement or
a literal union, or refine its type inline, as in
`Due enum { Hours(Int where gt(%, 0)) }`. The error names the variant:
``constructor of `Due.Hours` failed `gt(%, 0)` ``.

## Bindings

A typed binding takes a `where` after its value. A line that starts with
`where` continues the binding above it.

{{snippet:constraints#bindings}}

The `where` can also follow the declared type, as in
`max-retries: Int where between(%, 0, 10) 3`. A binding takes one `where`
clause. Combine conditions with `and`. A binding needs a declared type to
take a `where`; use `: Any` for an untyped value.

## Parameters and Return Values

A `where` after a parameter's type checks that argument. A `where` after the
return type checks the value the function returns.

{{snippet:constraints#parameters-and-returns}}

{{result:constraints#parameters-and-returns}}

A failing argument returns its `Err` before the body runs. The return check
runs only when the body succeeds; an `Err` from the body passes through
unchanged. A function that calls itself in tail position keeps its return
check and its constant stack.

## Preconditions

A `where` after the parameter list checks the arguments together. Its `%` is a
record of the arguments, keyed by parameter name. A long signature can continue
on later lines:

{{snippet:constraints#preconditions}}

{{result:constraints#preconditions}}

Parameter checks run first, then the precondition, then the body.

## Rest Parameters

A `where` on a rest parameter checks each argument it collects. A failing
argument's index is the error's `path`. A precondition sees the rest parameter
as a `Vec`:

{{snippet:constraints#rest-parameters}}

{{result:constraints#rest-parameters}}

## Nullable Types

`Username?` accepts `null`. A non-null value must satisfy `Username`. A `where`
on a nullable type sees `null` too, so its predicate has to handle it:

{{snippet:constraints#nullable}}

## Writing Predicates

A predicate is either a `%` expression or a lambda with one parameter:
`Int where gt(%, 2)` and `Int where (x) { gt(x, 2) }` are the same. As with
any lambda, its `{` goes on the same line as its `)`. A predicate can call
your own functions:

{{snippet:constraints#helper-predicates}}

A predicate runs at namespace scope. It can use the functions and top-level
values of its namespace, but not a function's parameters or local bindings,
even when its `where` appears inside that function. To relate one parameter to
another, use a [precondition](#preconditions).

A predicate must:

- **Return a `Bool`.** Truthy values don't pass, and `null` doesn't pass.
- **Be pure.** It can't use the clock, randomness, environment, context,
  files, stores, HTTP, events, tasks, or sockets, directly or through the
  functions it calls.
- **Finish quickly.** Each check has an instruction and call-depth budget.

`length` of a `Str` counts UTF-8 bytes, not characters, so `length("café")` is
`5`. Keep that in mind when you bound text length.

A `where` predicate is an ordinary function argument under Hot's `%` rule, so
an implicit `%` lambda can't appear inside it. Write an inner function
explicitly:

{{snippet:constraints#inner-functions}}

## Handling Constraint Errors

A failed check is a `Result.Err` whose value is one of these:

| Variant | When |
| --- | --- |
| `ConstraintError.Violation` | The value failed the predicate, or isn't of the base type |
| `ConstraintError.EvaluationFailed` | The predicate errored, returned a non-`Bool`, or called a function a predicate can't use |
| `ConstraintError.BudgetExceeded` | The predicate ran out of its budget |

Each holds these fields:

| Field | Value |
| --- | --- |
| `message` | A summary of the failure |
| `constraint` | The named refinement, such as `::shop/Port`, or `<inline>` |
| `predicate` | The predicate's source text |
| `boundary` | Where the check ran: `binding`, `constructor`, `parameter`, `arguments`, `return`, or `update` |
| `path` | Steps from the checked value to the part that failed. Empty when the value itself failed |
| `declared-type` | The declared type at the boundary, when there is one |
| `value-type` | The type of the rejected value |
| `function` | The function, for parameter, argument, and return checks |
| `parameter` | The parameter name, for parameter checks |
| `location` | The source location of the `where` clause |

`ConstraintError.EvaluationFailed` adds `cause`: the underlying error, or a
description of what the predicate returned. `ConstraintError.BudgetExceeded`
adds `budget` (`"instructions"` or `"depth"`) and `limit`. The rejected value
itself is never included.

{{snippet:constraints#handling-errors}}

{{result:constraints#handling-errors}}

`err-message` returns the message.

## Literal Unions

A literal union is a constraint too. A closed union accepts only its members,
at every boundary: parameters, typed fields, constructors, return values, and
updates. Any other value is `Err(ConstraintError.Violation)` when it arrives
at run time. An `open` union also accepts unknown values of the same type,
which suits values an external service may add to over time. Use `open` for
values that come from outside your code, such as API responses.

{{snippet:constraints#literal-unions}}

{{result:constraints#literal-unions}}

## Updates

`assoc`, `assoc-in`, `update`, `update-in`, `merge`, and deep-path assignment
keep a typed value's type, so they check its constraints too:

{{snippet:constraints#updates}}

{{result:constraints#updates}}

The checks reach typed values nested inside the one you change. `assoc-in`,
`update-in`, and `merge` check every typed value along the way, not only the
outermost one. `postwalk` and `prewalk` check each typed value they rebuild.

Map spread (`{...server}`) and transforms like `map-vals` and `delete` return
plain maps, which claim no type and are not checked.

## Conversions

Where a value meets a constraint, Hot only converts numbers exactly: an `Int`
becomes a `Dec`, and a whole `Dec` such as `5.0` becomes an `Int`. `7.9` is
rejected rather than truncated, and `"5"` is rejected rather than parsed.
Convert explicitly first, as in `Port(Int(text))`. Parameters without a
constraint keep their usual conversions.

## Checked Before Running

`hot check` reports constraint problems it can find without running the
program:

| Code | Severity | When |
| --- | --- | --- |
| `constraint-violation` | Error | A literal fails its constraint, such as `Port(70000)` |
| `constraint-not-bool` | Error | A predicate definitely returns something other than a `Bool` |
| `impure-constraint` | Error | A predicate calls a function a predicate can't use |
| `unsupported-constraint` | Error | A `lazy` parameter has a constraint, or a function with `lazy` parameters has a precondition |
| `invalid-constraint` | Error | A predicate isn't a function of one value, or a parameter's predicate uses another parameter |
| `duplicate-where` | Error | A binding or type has more than one `where` clause |
| `where-requires-type` | Error | A `where` follows a binding without a declared type |
| `nested-placeholder` | Error | An implicit `%` lambda inside a predicate |
| `unknown-literal` | Warning | A literal outside an open union's known values |
| `impossible-comparison` | Warning | `eq` or `ne` between a closed union's value and a literal outside it, which is always false (`ne`: always true) |
| `where-changes-representation` | Warning | A refined primitive type used with `is-type` or as a `match` arm |

For `constraint-violation`, `hot check` evaluates a predicate on a literal
(a number, string, `true`, `false` or `null`) written as:

- the value of a typed binding, at top level or inside a body: `p: Port 0`;
- the argument of a refined type: `Port(70000)`;
- an argument to a constrained parameter: `listen(0)`;
- a field of a record constructor call, named or in Map form, including an
  enum variant's record payload and nested Map literals. The error names the
  field: `` 0 fails `Port` (between(%, 1, 65535)) at `port` of `Server` ``. The
  record's invariant is evaluated too, when every field it reads is a
  literal.
- the payload of an enum variant: `Due.Hours(0)`.

The evaluation follows comparisons, `between`, `in`, `and`, `or`, `not`, `if`,
`is-null`, `length`, `starts-with`, `ends-with`, `contains`, top-level literal
values, and calls to the program's own functions whose parameters are untyped
or typed `Str`, `Int`, `Dec`, `Bool` or `Any`, within a fixed budget. When it
can't decide, as for a computed value or any other function, `hot check`
reports nothing and the predicate runs when the program does.

## Tool Schemas

MCP and agent tool schemas include a constraint's common shapes:

- Comparisons with a literal bound, and `between`, become `minimum` and
  `maximum`.
- Length bounds on a `Vec` become `minItems` and `maxItems`.
- `::hot::regex/is-match` with a literal pattern becomes `pattern`.
- `in` with a literal list becomes `enum`.
- A literal union becomes a typed `enum`, such as
  `{"type": "string", "enum": ["USD", "EUR", "GBP"]}`.

Length bounds on a `Str` don't become `minLength` or `maxLength`, because
`length` counts bytes and JSON Schema counts characters. Like every other
predicate, they are still enforced, and the schema's description states them.
A precondition goes in the tool's description.

## Summary

- `Type where predicate` refines a type; `%` is the value being checked
- Refinements check bindings, parameters, return values, constructors, and
  updates
- A failed check is a `Result.Err` holding a `ConstraintError`
- Predicates are pure, return a `Bool`, and run within a budget
- Literal unions are constraints; `open` unions accept unknown values
- Predicates run at namespace scope and can't see parameters or locals
