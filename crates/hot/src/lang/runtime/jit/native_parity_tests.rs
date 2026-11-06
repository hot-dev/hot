//! Interpreter/JIT parity for the natives compiled code runs: `length`, map
//! spreads and appends, bare calls to core names a project rebinds, lambdas
//! reading locals bound after them, calls no overload takes, and core
//! functions called with other than two arguments.
//!
//! Each call runs on a fresh VM, first interpreted, then compiled from its
//! first call, so a call that halts doesn't leave its failure to the next.

use crate::lang::bytecode::FunctionId;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val;
use crate::val::Val;
use fastnum::D256;
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

// `add` and its kin run natively for two arguments. Another count is an
// ordinary call: it must not stop the function around it from compiling, as
// the intrinsic's fixed arity once did.
#[test]
fn jit_compiles_functions_that_call_core_functions_with_more_arguments() {
    let source = &chain_source(
        r#"
::app::main ns

sum fn (a: Int, b: Int, c: Int): Int { add(a, b, c) }

negative-sum fn (a: Int, b: Int, c: Int): Int { sub(mul(a, b, c), add(a, b, c, 1)) }

quotient fn (a: Int, b: Int, c: Int): Any { div(a, b, c) }

rising fn (a: Int, b: Int, c: Int): Bool { lt(a, b, c) }

falling fn (a: Int, b: Int, c: Int): Bool { gt(a, b, c) }

not-rising fn (a: Int, b: Int, c: Int): Bool { gte(a, b, c) }

not-falling fn (a: Int, b: Int, c: Int): Bool { lte(a, b, c) }

level fn (a: Int, b: Int, c: Int): Bool { eq(a, b, c) }

mixed fn (a: Int, b: Int, c: Int): Bool { ne(a, b, c) }

spread fn (a: Int, b: Int, c: Int): Int { add(...[a, b, c]) }
"#,
    );
    let calls = vec![
        vec![val!(1), val!(2), val!(3)],
        vec![val!(3), val!(2), val!(1)],
        vec![val!(2), val!(2), val!(2)],
    ];
    for (function, expected) in [
        ("sum", vec![val!(6), val!(6), val!(6)]),
        ("negative-sum", vec![val!(-1), val!(-1), val!(1)]),
        ("rising", vec![val!(true), val!(false), val!(false)]),
        ("falling", vec![val!(false), val!(true), val!(false)]),
        ("not-rising", vec![val!(false), val!(true), val!(true)]),
        ("not-falling", vec![val!(true), val!(false), val!(true)]),
        ("level", vec![val!(false), val!(false), val!(true)]),
        ("mixed", vec![val!(true), val!(true), val!(false)]),
        ("spread", vec![val!(6), val!(6), val!(6)]),
    ] {
        let name = format!("::app::main/{function}");
        let (results, was_compiled) = same_outcomes(source, &name, &calls);
        let expected: Vec<Result<Val, String>> = expected.into_iter().map(Ok).collect();
        assert_eq!(results, expected, "{name}");
        assert!(was_compiled, "{name} stays compiled");
    }
    let (results, was_compiled) = same_outcomes(
        source,
        "::app::main/quotient",
        &[
            vec![val!(100), val!(2), val!(5)],
            vec![val!(1), val!(0), val!(5)],
        ],
    );
    assert_eq!(results[0], Ok(val!(10)));
    assert!(is_err_value(&results[1]), "a zero divisor is an Err");
    assert!(was_compiled, "quotient stays compiled");
}

/// The overloads hot-std declares for `names`, without the rest of hot-std.
fn hot_std_overloads(namespace: &str, names: &[&str]) -> String {
    let functions: Vec<String> = names
        .iter()
        .map(|name| {
            format!(
                "{name} meta {{core: true}} fn\n\
                 (x: Any, y: Any) {{ call-lib({namespace}/{name}, [x, y]) }},\n\
                 (x: Any, y: Any, z: Any, ...rest: Any) {{\n\
                 call-lib({namespace}/{name}, [x, y, z, ...rest])\n\
                 }}"
            )
        })
        .collect();
    format!("{namespace} ns\n\n{}\n", functions.join("\n\n"))
}

fn chain_source(app: &str) -> String {
    format!(
        "{}\n{}\n{app}",
        hot_std_overloads("::hot::math", &["add", "sub", "mul", "div", "min", "max"]),
        hot_std_overloads("::hot::cmp", &["eq", "ne", "lt", "lte", "gt", "gte"]),
    )
}

// An overflow at any step of a chain is the Err of the whole call, however
// its result is used: returned, kept in a local, read by another op, or unread.
#[test]
fn jit_int_chains_overflow_as_interpreted_wherever_the_result_goes() {
    let source = chain_source(
        r#"
::app::main ns

returned fn (a: Int, b: Int, c: Int): Any { add(a, b, c) }

returned-mul fn (a: Int, b: Int, c: Int): Any { mul(a, b, c) }

returned-sub fn (a: Int, b: Int, c: Int): Any { sub(a, b, c) }

kept fn (a: Int, b: Int, c: Int): Any {
  total add(a, b, c, 1)
  [total]
}

read-by-op fn (a: Int, b: Int, c: Int): Any { add(add(a, b, c), 1) }

unread fn (a: Int, b: Int, c: Int): Int {
  ignored add(a, b, c)
  7
}
"#,
    );
    let max = val!(i64::MAX);
    let min = val!(i64::MIN);
    let calls = vec![
        vec![val!(1), val!(2), val!(3)],
        // The first step overflows; the last would bring it back.
        vec![max.clone(), val!(1), val!(-5)],
        // The last step overflows.
        vec![max.clone(), val!(-1), val!(2)],
        vec![max.clone(), max.clone(), max.clone()],
        vec![min.clone(), val!(-1), val!(1)],
        vec![min.clone(), min.clone(), val!(0)],
    ];
    for function in [
        "returned",
        "returned-mul",
        "returned-sub",
        "kept",
        "read-by-op",
        "unread",
    ] {
        let name = format!("::app::main/{function}");
        let (results, was_compiled) = same_outcomes(&source, &name, &calls);
        assert!(was_compiled, "{name} stays compiled");
        let overflow = "Integer overflow";
        match function {
            "unread" => assert!(results.iter().all(|r| *r == Ok(val!(7))), "{results:?}"),
            "read-by-op" => {
                assert_eq!(results[0], Ok(val!(7)));
                // An Err read by another op halts there, in either tier.
                assert!(
                    matches!(&results[1], Err(e) if e.contains(overflow)),
                    "{results:?}"
                );
            }
            "kept" => {
                assert_eq!(results[0], Ok(val!([7])));
                let Ok(Val::Vec(kept)) = &results[1] else {
                    panic!("{results:?}");
                };
                assert!(kept[0].is_err(), "the local keeps the Err as a value");
            }
            _ => assert!(is_err_value(&results[1]), "{name}: {results:?}"),
        }
    }
    let (results, _) = same_outcomes(
        &source,
        "::app::main/returned",
        &[
            vec![max.clone(), val!(1), val!(-5)],
            vec![val!(1), val!(2), val!(3)],
        ],
    );
    assert!(is_err_value(&results[0]), "the overflow is the result");
    assert_eq!(results[1], Ok(val!(6)));
}

// Dec, mixed and untyped operands take the native function, not the inline
// Int path, and what isn't a number takes the full call.
#[test]
fn jit_chains_of_other_operands_match_the_interpreter() {
    let source = chain_source(
        r#"
::app::main ns

sum fn (a, b, c) { add(a, b, c) }

product fn (a, b, c) { mul(a, b, c) }

quotient fn (a, b, c) { div(a, b, c) }

least fn (a, b, c) { min(a, b, c) }

most fn (a, b, c) { max(a, b, c) }

least-of-two fn (a, b) { min(a, b) }

most-of-two fn (a, b) { max(a, b) }

rising fn (a, b, c) { lt(a, b, c) }

not-falling fn (a, b, c) { lte(a, b, c) }

all-equal fn (a, b, c) { eq(a, b, c) }

not-all-equal fn (a, b, c) { ne(a, b, c) }
"#,
    );
    let calls = vec![
        vec![val!(1), val!(2), val!(3)],
        vec![val!(3), val!(2), val!(1)],
        vec![Val::from(D256::from(1)), val!(2), Val::from(D256::from(3))],
        vec![Val::from(D256::from(2)), Val::from(D256::from(2)), val!(2)],
        vec![val!(1), val!(0), val!(2)],
        vec![val!(i64::MAX), val!(1), Val::from(D256::from(1))],
    ];
    for function in [
        "sum",
        "product",
        "quotient",
        "least",
        "most",
        "rising",
        "not-falling",
        "all-equal",
        "not-all-equal",
    ] {
        let name = format!("::app::main/{function}");
        same_outcomes(&source, &name, &calls);
        // Not a number: the native functions never see these.
        same_outcomes(&source, &name, &[vec![val!(1), val!(2), val!("a")]]);
        same_outcomes(&source, &name, &[vec![val!("a"), val!(2), val!(3)]]);
    }
    let dec = |n: i64| Val::from(D256::from(n));
    let check = |function: &str, args: Vec<Val>, expected: Val| {
        let name = format!("::app::main/{function}");
        let (results, _) = same_outcomes(&source, &name, &[args]);
        assert_eq!(results, vec![Ok(expected)], "{name}");
    };
    check("sum", vec![val!(1), dec(2), val!(3)], dec(6));
    check("product", vec![dec(2), val!(3), val!(4)], dec(24));
    check("quotient", vec![val!(100), val!(2), val!(5)], val!(10));
    check("least", vec![dec(3), val!(1), val!(2)], dec(1));
    check("most", vec![val!(3), dec(9), val!(2)], dec(9));
    check("rising", vec![val!(1), dec(2), val!(3)], val!(true));
    check("rising", vec![val!(1), dec(3), val!(2)], val!(false));
    check("not-falling", vec![dec(2), dec(2), dec(3)], val!(true));
    check("all-equal", vec![dec(2), dec(2), dec(2)], val!(true));
    check("not-all-equal", vec![dec(2), dec(2), dec(2)], val!(false));
    check("not-all-equal", vec![dec(2), dec(3), dec(2)], val!(true));
    let pairs = vec![
        vec![val!(1), val!(2)],
        vec![val!(2), val!(1)],
        vec![Val::from(D256::from(2)), val!(3)],
        vec![val!("a"), val!(3)],
    ];
    same_outcomes(&source, "::app::main/least-of-two", &pairs);
    same_outcomes(&source, "::app::main/most-of-two", &pairs);
}

// Comparison chains read every pair, and `ne` isn't "all different".
#[test]
fn jit_int_comparison_chains_read_every_pair() {
    let source = chain_source(
        r#"
::app::main ns

rising fn (a: Int, b: Int, c: Int, d: Int): Bool { lt(a, b, c, d) }

not-falling fn (a: Int, b: Int, c: Int, d: Int): Bool { lte(a, b, c, d) }

falling fn (a: Int, b: Int, c: Int, d: Int): Bool { gt(a, b, c, d) }

not-rising fn (a: Int, b: Int, c: Int, d: Int): Bool { gte(a, b, c, d) }

all-equal fn (a: Int, b: Int, c: Int, d: Int): Bool { eq(a, b, c, d) }

not-all-equal fn (a: Int, b: Int, c: Int, d: Int): Bool { ne(a, b, c, d) }

least fn (a: Int, b: Int, c: Int, d: Int): Int { min(a, b, c, d) }

most fn (a: Int, b: Int, c: Int, d: Int): Int { max(a, b, c, d) }
"#,
    );
    let four = |a: i64, b: i64, c: i64, d: i64| vec![val!(a), val!(b), val!(c), val!(d)];
    let calls = vec![
        four(1, 2, 3, 4),
        four(4, 3, 2, 1),
        four(1, 2, 2, 3),
        four(3, 2, 2, 1),
        four(1, 1, 1, 1),
        // Only the last pair is out of order, then only the first.
        four(1, 2, 3, 1),
        four(5, 2, 3, 4),
        // `ne` of values that match at either end.
        four(1, 2, 2, 1),
        four(i64::MAX, i64::MIN, 0, -1),
    ];
    for (function, expected) in [
        (
            "rising",
            [true, false, false, false, false, false, false, false, false],
        ),
        (
            "not-falling",
            [true, false, true, false, true, false, false, false, false],
        ),
        (
            "falling",
            [false, true, false, false, false, false, false, false, false],
        ),
        (
            "not-rising",
            [false, true, false, true, true, false, false, false, false],
        ),
        (
            "all-equal",
            [false, false, false, false, true, false, false, false, false],
        ),
        (
            "not-all-equal",
            [true, true, true, true, false, true, true, true, true],
        ),
    ] {
        let name = format!("::app::main/{function}");
        let (results, was_compiled) = same_outcomes(&source, &name, &calls);
        let expected: Vec<Result<Val, String>> =
            expected.into_iter().map(|b| Ok(val!(b))).collect();
        assert_eq!(results, expected, "{name}");
        assert!(was_compiled, "{name} stays compiled");
    }
    let (results, was_compiled) = same_outcomes(&source, "::app::main/least", &calls);
    assert_eq!(results[8], Ok(val!(i64::MIN)));
    assert!(was_compiled, "least stays compiled");
    let (results, was_compiled) = same_outcomes(&source, "::app::main/most", &calls);
    assert_eq!(results[8], Ok(val!(i64::MAX)));
    assert!(was_compiled, "most stays compiled");
}
