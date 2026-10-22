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

## Nullable Types

`Username?` accepts `null`. A non-null value must satisfy `Username`. A `where`
on a nullable type sees `null` too, so its predicate has to handle it:

{{snippet:constraints#nullable}}

## Writing Predicates

A predicate is either a `%` expression or a lambda with one parameter:
`Int where gt(%, 2)` and `Int where (x) { gt(x, 2) }` are the same. It can
call your own functions:

{{snippet:constraints#helper-predicates}}

A predicate must:

- **Return a `Bool`.** Truthy values don't pass, and `null` doesn't pass.
- **Be pure.** It can't use the clock, randomness, environment, context,
  files, stores, HTTP, events, tasks, or sockets, directly or through the
  functions it calls.
- **Finish quickly.** Each check has an instruction and call-depth budget.

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

Each holds a `message`, the `constraint` and `predicate`, the `boundary`
(`binding`, `constructor`, `parameter`, `arguments`, `return`, or `update`),
the `path` to the part that failed, and the `function`, `parameter` and
`location` where they apply. The rejected value itself is never included.

{{snippet:constraints#handling-errors}}

{{result:constraints#handling-errors}}

`err-message` returns the message.

## Literal Unions

A literal union is a constraint too. A closed union accepts only its members.
An `open` union also accepts unknown values of the same type, which suits
values an external service may add to over time.

{{snippet:constraints#literal-unions}}

{{result:constraints#literal-unions}}

## Updates

`assoc`, `assoc-in`, `update`, `update-in`, `merge`, and deep-path assignment
keep a typed value's type, so they check its constraints too:

{{snippet:constraints#updates}}

{{result:constraints#updates}}

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
| `duplicate-where` | Error | A binding or type has more than one `where` clause |
| `where-requires-type` | Error | A `where` follows a binding without a declared type |
| `nested-placeholder` | Error | An implicit `%` lambda inside a predicate |
| `unknown-literal` | Warning | A literal outside an open union's known values |
| `where-changes-representation` | Warning | A refined primitive type used with `is-type` or as a `match` arm |

## Tool Schemas

MCP and agent tool schemas include a constraint's common shapes. Comparisons
with a literal bound and `between` become `minimum` and `maximum`, length
bounds become `minLength` and `maxLength` (or `minItems` and `maxItems`),
`::hot::regex/is-match` with a literal pattern becomes `pattern`, and `in` with
a literal list becomes `enum`. Other predicates are still enforced; the schema
describes them. A precondition goes in the tool's description.

## Summary

- `Type where predicate` refines a type; `%` is the value being checked
- Refinements check bindings, parameters, return values, constructors, and
  updates
- A failed check is a `Result.Err` holding a `ConstraintError`
- Predicates are pure, return a `Bool`, and run within a budget
- Literal unions are constraints; `open` unions accept unknown values
