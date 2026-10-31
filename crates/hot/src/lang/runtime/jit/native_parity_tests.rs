//! Interpreter/JIT parity for the natives compiled code runs: `length`, map
//! spreads and appends, bare calls to core names a project rebinds, lambdas
//! reading locals bound after them, and calls no overload takes.
//!
//! Each call runs on a fresh VM, first interpreted, then compiled from its
//! first call, so a call that halts doesn't leave its failure to the next.

use crate::lang::bytecode::FunctionId;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val;
use crate::val::Val;
use std::sync::Arc;

fn source_vm(source: &str, jit: Val) -> VirtualMachine {
    let mut program = crate::lang::parser::parse_hot(source).expect("test source parses");
    let mut compiler = crate::lang::compiler::Compiler::new();
    compiler
        .compile_program_unchecked(&mut program)
        .expect("test source compiles");
    let mut vm = VirtualMachine::new(
        compiler.get_program_arc(),
        None,
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        Arc::new(CoreVariableRegistry::new()),
        Some(val!({ "jit": jit })),
    );
    vm.execute().expect("namespace initialization runs");
    vm
}

/// Each call's value, or the message of the error it stops with, and
/// whether `function` compiled.
fn outcomes(
    source: &str,
    function: &str,
    calls: &[Vec<Val>],
    jit: Val,
) -> (Vec<Result<Val, String>>, bool) {
    let mut compiled = false;
    let results = calls
        .iter()
        .map(|args| {
            let mut vm = source_vm(source, jit.clone());
            let result = vm
                .call_function_bypassing_unified_lookup(function, args)
                .map_err(|err| err.to_string());
            let id = vm
                .program
                .functions
                .iter()
                .position(|f| f.name == function)
                .expect("the function exists") as FunctionId;
            compiled |= vm.jit_has_compiled_function(id);
            result
        })
        .collect();
    (results, compiled)
}

/// The outcomes interpreted and compiled, after checking they are the same;
/// and whether the function compiled.
fn same_outcomes(
    source: &str,
    function: &str,
    calls: &[Vec<Val>],
) -> (Vec<Result<Val, String>>, bool) {
    let (interpreted, _) = outcomes(source, function, calls, val!({"mode": "disabled"}));
    let (compiled, was_compiled) = outcomes(
        source,
        function,
        calls,
        val!({"mode": "enabled", "threshold": 1}),
    );
    assert_eq!(compiled, interpreted, "{function}: compiled vs interpreted");
    (interpreted, was_compiled)
}

fn is_err_value(outcome: &Result<Val, String>) -> bool {
    matches!(outcome, Ok(value) if value.is_err())
}

// `length` of a value that isn't a collection is an Err in the interpreter.
// Compiled code counted it as 0.
#[test]
fn jit_length_of_a_non_collection_gives_the_interpreters_err() {
    let source = r#"
::t ns

returned fn (m: Map): Any { length(m.items) }

kept fn (m: Map): Map { {n: length(m.items)} }

consumed fn (m: Map): Int { add(length(m.items), 1) }

unread fn (m: Map): Int {
    length(m.items)
    5
}
"#;
    let calls = vec![
        vec![val!({"items": [1, 2]})],
        vec![val!({})],
        vec![val!({"items": 5})],
        vec![val!({"items": true})],
        vec![val!({"items": "abc"})],
    ];
    for function in ["::t/returned", "::t/kept", "::t/consumed", "::t/unread"] {
        let (results, was_compiled) = same_outcomes(source, function, &calls);
        assert!(was_compiled, "{function} compiles");
        match function {
            "::t/returned" => {
                assert_eq!(results[0], Ok(val!(2)));
                assert!(is_err_value(&results[1]), "{results:?}");
                assert!(is_err_value(&results[2]), "{results:?}");
            }
            "::t/kept" => {
                assert_eq!(results[0], Ok(val!({"n": 2})));
                let Ok(Val::Map(map)) = &results[2] else {
                    panic!("{results:?}");
                };
                assert!(map.get(&val!("n")).is_some_and(Val::is_err), "{results:?}");
            }
            "::t/consumed" => {
                assert_eq!(results[0], Ok(val!(3)));
                assert!(
                    results[1]
                        .as_ref()
                        .is_err_and(|message| message.contains("length expects collection")),
                    "{results:?}"
                );
            }
            _ => assert!(results.iter().all(|r| *r == Ok(val!(5))), "{results:?}"),
        }
    }
}

// Spreading a value that isn't a map, and appending to one that isn't a
// vector, halt in the interpreter. Compiled code dropped the spread source
// or replaced the target with a new vector.
#[test]
fn jit_map_spread_and_append_halt_as_the_interpreter_does() {
    let source = r#"
::t ns

spread fn (m: Any): Map { {...m, b: 2} }

append fn (v: Any): Any {
    out v
    out[] 1
    out
}
"#;
    let spreads = vec![
        vec![val!({"a": 1})],
        vec![Val::Null],
        vec![val!(5)],
        vec![val!("xyz")],
        vec![val!([1, 2])],
    ];
    let (results, was_compiled) = same_outcomes(source, "::t/spread", &spreads);
    assert!(was_compiled, "spread compiles");
    assert_eq!(results[0], Ok(val!({"a": 1, "b": 2})));
    assert!(
        results[1..].iter().all(|r| r
            .as_ref()
            .is_err_and(|m| m.contains("MergeMaps requires two maps"))),
        "{results:?}"
    );

    let appends = vec![
        vec![val!([0])],
        vec![Val::Null],
        vec![val!({"a": 1})],
        vec![val!(5)],
        vec![val!("xyz")],
    ];
    let (results, was_compiled) = same_outcomes(source, "::t/append", &appends);
    assert!(was_compiled, "append compiles");
    assert_eq!(results[0], Ok(val!([0, 1])));
    assert_eq!(results[1], Ok(val!([1])));
    assert!(
        results[2..]
            .iter()
            .all(|r| r.as_ref().is_err_and(|m| m.contains("Cannot append to"))),
        "{results:?}"
    );
}

// A project core function sharing a native name, and a namespace binding of
// one, are what a bare call reaches. The interpreter's fast paths ran the
// native `first` and `length` regardless, and compiled code the native
// `is-some` and `length`.
#[test]
fn jit_bare_calls_to_rebound_core_names_reach_the_binding() {
    let source = r#"
::app::util ns

first meta {core: true} fn (xs: Any): Any { "util-first" }

is-some meta {core: true} fn (x: Any): Bool { false }

length meta {core: true} fn (x: Any): Int { 7 }

tail-of fn (xs: Vec): Any { "util-tail" }

::app::main ns

head-of fn (xs: Vec): Any { first(xs) }

present fn (x: Any): Bool { is-some(x) }

size fn (xs: Vec): Int { length(xs) }

::app::aliased ns

first ::app::util/tail-of

head-of fn (xs: Vec): Any { first(xs) }

sum fn (a: Int, b: Int): Int { add(a, b) }
"#;
    let vecs = vec![vec![val!([1, 2])], vec![val!([3])]];
    let (results, _) = same_outcomes(source, "::app::main/head-of", &vecs);
    assert_eq!(
        results,
        vec![Ok(val!("util-first")), Ok(val!("util-first"))]
    );
    let (results, _) = same_outcomes(source, "::app::main/present", &[vec![val!(1)]]);
    assert_eq!(results, vec![Ok(val!(false))]);
    let (results, was_compiled) = same_outcomes(source, "::app::main/size", &vecs);
    assert_eq!(results, vec![Ok(val!(7)), Ok(val!(7))]);
    assert!(!was_compiled, "a rebound intrinsic name stays interpreted");
    let (results, _) = same_outcomes(source, "::app::aliased/head-of", &vecs);
    assert_eq!(results, vec![Ok(val!("util-tail")), Ok(val!("util-tail"))]);
    // A native name the namespace doesn't rebind still compiles inline.
    let (results, was_compiled) =
        same_outcomes(source, "::app::aliased/sum", &[vec![val!(1), val!(2)]]);
    assert_eq!(results, vec![Ok(val!(3))]);
    assert!(was_compiled, "sum compiles");
}

// A lambda reading a local bound after it, as mutually recursive and
// self-recursive local lambdas do, finds it in its definer's frame when it
// runs. Compiled frames keep their locals out of the VM, so the lookup
// failed.
#[test]
fn jit_lambdas_reading_later_locals_match_the_interpreter() {
    let source = r#"
::t ns

scaled fn (x: Int): Int {
    scale (y) { mul(y, factor) }
    factor cond {
        gt(x, 0) => { 2 }
        => { 3 }
    }
    scale(x)
}

parity fn (x: Int): Str {
    is-ev (n) {
        cond {
            eq(n, 0) => { true }
            => { is-od(sub(n, 1)) }
        }
    }
    is-od (n) {
        cond {
            eq(n, 0) => { false }
            => { is-ev(sub(n, 1)) }
        }
    }
    cond {
        is-ev(x) => { "even" }
        => { "odd" }
    }
}

fact-of fn (x: Int): Int {
    fact (n) {
        cond {
            eq(n, 0) => { 1 }
            => { mul(n, fact(sub(n, 1))) }
        }
    }
    fact(x)
}

offset fn (x: Int): Int {
    base 10
    shift (y) { add(y, base) }
    shift(x)
}
"#;
    let ints = vec![vec![val!(4)], vec![val!(7)], vec![val!(-2)]];
    let (results, _) = same_outcomes(source, "::t/scaled", &ints);
    assert_eq!(results, vec![Ok(val!(8)), Ok(val!(14)), Ok(val!(-6))]);
    let (results, _) = same_outcomes(source, "::t/parity", &ints[..2]);
    assert_eq!(results, vec![Ok(val!("even")), Ok(val!("odd"))]);
    let (results, _) = same_outcomes(source, "::t/fact-of", &[vec![val!(5)]]);
    assert_eq!(results, vec![Ok(val!(120))]);
    // A lambda reading only earlier bindings still compiles.
    let (results, was_compiled) = same_outcomes(source, "::t/offset", &ints[..1]);
    assert_eq!(results, vec![Ok(val!(14))]);
    assert!(was_compiled, "offset compiles");
}

// A qualified call no overload of the name takes fails as the interpreter's
// lookup by name does. Compiled code called the function anyway, with Null
// for the missing parameter.
#[test]
fn jit_call_with_a_wrong_argument_count_fails_as_interpreted() {
    let source = r#"
::t::lib ns

two fn (a: Int, b: Any): Str { `a=${a} b=${b}` }

::t ns

::l ::t::lib

direct fn (x: Int): Any { ::t::lib/two(x) }

aliased fn (x: Int): Any { ::l/two(x) }

templated fn (x: Int): Any { `one-arg=${::l/two(x)}` }
"#;
    for function in ["::t/direct", "::t/aliased", "::t/templated"] {
        let (results, _) = same_outcomes(source, function, &[vec![val!(5)]]);
        assert!(
            results[0]
                .as_ref()
                .is_err_and(|message| message.contains("not found")),
            "{function}: {results:?}"
        );
    }
}
