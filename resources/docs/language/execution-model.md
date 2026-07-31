---
description: "Understand how Hot evaluates code within a single platform run or task attempt."
---

# Language Evaluation Model

This page describes how Hot evaluates code **within one platform run or task
attempt**. Events, handler routing, retries, tasks, workers, and stream lineage
belong to the [Platform Execution Model](/docs/platform/execution-model).

Within Hot code, the key is to keep separate concepts separate: eager versus
lazy describes **when an argument is evaluated**, while serial versus parallel
describes **how a flow schedules its contents**.

## Model at a Glance

| Concern | Default | Explicit alternative | Detailed reference |
|---------|---------|----------------------|--------------------|
| Function arguments | Evaluated before the call | A `lazy` parameter defers one argument | [Functions](/docs/language/functions#lazy-arguments) |
| Function bodies | Implicit `serial` flow | `parallel`, `cond`, `match`, and other flows | [Flows](/docs/language/flows) |
| Bindings and values | Immutable lexical bindings | Name reuse and deep paths create later bindings | [Vars and Values](/docs/language/vars-and-values) |
| Failure | A bound `Result` remains intact; ordinary consumption propagates an `Err` | Lazy Result helpers or `match` inspect failure explicitly | [Error Handling](/docs/language/errors) |
| Types | Gradual checking with structural records and tagged constructed values | Annotations narrow known contracts; `Any` remains dynamic | [Types](/docs/language/types) |

## How the Rules Compose

For an ordinary call, Hot evaluates non-lazy argument expressions, enters an
implicit serial body, and uses the body's final expression as the function's
success value. There is no `return` statement. Flows such as `cond`, `match`,
and `parallel` are expressions too, so they can provide that final value or be
bound within a larger serial body.

A `lazy` parameter changes argument evaluation only. Each `do` forces its
deferred computation again; it does not turn the surrounding body into a
parallel flow. Likewise, `Iter` defers sequence-element production rather than
function-argument evaluation. See [Functions](/docs/language/functions#lazy-arguments)
for lazy parameters and the [`Iter` package](/pkg/hot-std/hot/iter) for sequence
laziness.

An explicit `parallel` flow changes scheduling within that flow. Hot derives
dependencies from binding references and can run independent bindings
concurrently. It does not parallelize ordinary serial code. Result collection,
dependency levels, failure behavior, and `All<Map>` / `All<Vec>` annotations
are defined in [Flows](/docs/language/flows#parallel-flow).

## Results at Evaluation Boundaries

Binding a `Result` does not consume it. Ordinary function arguments, template
interpolation, and ordinary field or index access are consumption boundaries:
an `Ok` supplies its payload and an `Err` propagates. Lazy Result-aware helpers
preserve the wrapper. A `match` selects an arm from the variant identity, and
the matched name exposes that variant's payload inside the arm.

Function return annotations name the successful value rather than a visible
`Result` wrapper. See [Error Handling](/docs/language/errors) for creation,
propagation, inspection, `err(...)`, and `fail(...)`.

## Values, Types, and Effects

Hot uses immutable value semantics and lexically scoped closures. Reusing a
name or assigning through a deep path produces a later binding rather than
mutating a shared object. [Vars and Values](/docs/language/vars-and-values)
defines those operations.

Hot is gradually typed: it checks statically visible information while allowing
dynamic boundaries through `Any`. [Types](/docs/language/types) owns the
detailed rules for structural records, nominal runtime tags, unions, generics,
and coercions.

Hot's value and control model is functional, but the language is not pure.
HTTP, files, stores, tasks, events, and other external effects can occur in
ordinary functions, and the type system does not track them. Parallel
dependency analysis sees Hot binding references, not conflicts in external
state; already-started sibling effects cannot be rolled back automatically.
See [Flows](/docs/language/flows#parallel-flow) for the resulting coordination
and idempotency guidance.
