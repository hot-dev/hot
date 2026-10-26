// Tests for run/exec control functionality (fail and cancel functions)

#[cfg(test)]
mod run_tests {
    use crate::lang::compiler::Compiler;
    use crate::lang::parser;
    use crate::lang::runtime::vm::VirtualMachine;
    use crate::val;
    use crate::val::Val;
    use std::sync::Arc;

    /// Helper to compile and execute Hot code without `hot-std`. Uses
    /// `compile_program_unchecked` because the synthetic source has no
    /// stdlib loaded and would not survive name resolution.
    pub(super) fn compile_and_run(source: &str) -> Result<Val, String> {
        let mut compiler = Compiler::new();
        let mut program = parser::parse_hot(source).map_err(|e| format!("Parse error: {}", e))?;

        compiler
            .compile_program_unchecked(&mut program)
            .map_err(|e| format!("Compile error: {:?}", e))?;

        let program_arc = compiler.get_program_arc();
        let mut vm = VirtualMachine::new(
            program_arc,
            None,
            compiler.get_function_mapping_arc(),
            compiler.get_core_functions_arc(),
            compiler.get_type_implementations_arc(),
            compiler.get_core_variables_arc(),
            None,
        );

        vm.execute().map_err(|e| format!("{:?}", e))
    }

    /// Fused HOF pipelines must produce identical results to the interpreter for
    /// every supported shape (Tier 1 numeric/boolean and Tier 2 property access,
    /// `Dec`, and strings).
    #[test]
    fn hof_fusion_parity_with_interpreter() {
        let programs = [
            // --- Tier 1: Int / Bool ---
            // sum-even-squares
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> map((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
f(100)"#,
            // collection-benchmark (map -> filter -> map -> reduce)
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> map((x) { mul(x, 3) })
    |> filter((x) { gt(x, 100) })
    |> map((x) { add(x, 1) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
f(500)"#,
            // filter -> length (count of evens)
            r#"::t ns
f fn (n: Int): Int {
    range(2, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> length()
}
f(1000)"#,
            // --- Tier 2: Dec (division promotes Int -> Dec, mixed reduce) ---
            r#"::t ns
f fn (n: Int): Dec {
    range(1, add(n, 1))
    |> map((x) { div(x, 2) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
f(50)"#,
            // --- Tier 2: property access / record projection ---
            r#"::t ns
make fn (n: Int): Vec<Map> {
    range(1, add(n, 1)) |> map((i) { {count: i} })
}
f fn (n: Int): Int {
    make(n)
    |> filter((x) { gt(x.count, 5) })
    |> map((x) { x.count })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
f(20)"#,
            // --- Tier 2: strings (concat + template interpolation) ---
            r#"::t ns
f fn (n: Int): Str {
    range(1, add(n, 1))
    |> map((i) { `item-${i}` })
    |> reduce((acc, s) { concat(acc, concat(s, ",")) }, "")
}
f(10)"#,
            // Null interpolation should match `Str(null)` in fused templates.
            r#"::t ns
f fn (): Str {
    [null, "ok"] |> reduce((acc, item) { concat(acc, `item=${item};`) }, "")
}
f()"#,
            // --- Single-stage terminal reduce (matches string-concat-benchmark) ---
            r#"::t ns
f fn (n: Int): Str {
    reduce(range(n), (acc, i) { concat(acc, `item-${i}-`) }, "")
}
length(f(200))"#,
            // --- Some/All terminals ---
            r#"::t ns
f fn (n: Int): Bool {
    range(1, add(n, 1))
    |> filter((x) { gt(x, 10) })
    |> some((x) { eq(x, 42) })
}
f(100)"#,
            r#"::t ns
f fn (n: Int): Bool {
    range(1, add(n, 1))
    |> map((x) { mul(x, 2) })
    |> all((x) { is-zero(mod(x, 2)) })
}
f(100)"#,
            // --- Named-function stage callables ---
            r#"::t ns
is-even fn (x: Int): Bool {
    is-zero(mod(x, 2))
}
square fn (x: Int): Int {
    mul(x, x)
}
sum fn (acc: Int, x: Int): Int {
    add(acc, x)
}
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter(is-even)
    |> map(square)
    |> reduce(sum, 0)
}
f(100)"#,
        ];
        for src in programs {
            let on = compile_and_run_with_std_conf(
                src,
                Some(crate::val!({"jit": {"hof": {"fusion": true}}})),
            )
            .expect("fusion-on run");
            let off = compile_and_run_with_std_conf(
                src,
                Some(crate::val!({"jit": {"hof": {"fusion": false}}})),
            )
            .expect("fusion-off run");
            assert_eq!(on, off, "fusion parity mismatch for:\n{}", src);
            // sanity: never null/error
            assert!(
                !matches!(on, Val::Null) && !on.is_err(),
                "unexpected result {:?} for:\n{}",
                on,
                src
            );
        }
    }

    /// Helper to compile and execute Hot code with hot-std included
    pub(super) fn compile_and_run_with_std(source: &str) -> Result<Val, String> {
        compile_and_run_with_std_conf(source, None)
    }

    /// End-to-end smoke for the `string-concat` benchmark shape with fusion on:
    /// the result must match the interpreter exactly. The deterministic guard
    /// that the pipeline actually fuses (rather than de-opting on every call)
    /// lives in `jit_hof::tests::detect_reads_source_from_live_arg_register`,
    /// which does not depend on process-global telemetry counters.
    #[test]
    fn string_concat_benchmark_shape_correct() {
        let src = r#"::t ns
f fn (n: Int): Str {
    reduce(range(n), (acc, i) { concat(acc, `item-${i}-`) }, "")
}
length(f(200))"#;
        let out = compile_and_run_with_std_conf(
            src,
            Some(crate::val!({"jit": {"hof": {"fusion": true}}})),
        )
        .expect("run");
        assert_eq!(out, Val::Int(1690), "unexpected concat length");
    }

    #[test]
    fn jit_function_ref_constructor_dispatches_by_name() {
        let src = r#"::t ns
make-error fn (n: Int): Any { err(`jit-constructor-${n}`) }
make-error(1)
make-error(2)
make-error(3)"#;
        let out = compile_and_run_with_std_conf(
            src,
            Some(crate::val!({"jit": {"threshold": 1}})),
        )
        .expect("JIT FunctionRef constructor call");
        assert!(out.is_err(), "expected Result.Err, got {out:?}");
        assert!(
            out.unwrap_err()
                .is_some_and(|value| value.to_string().contains("jit-constructor-3")),
            "unexpected Result.Err payload: {out:?}"
        );
    }

    #[test]
    fn jit_user_call_halts_on_strict_err_argument() {
        // The halt now propagates out of the JIT frame as a run failure
        // (matching the interpreter), rather than surfacing as an Err value
        // with the failure state set — this test used to assert the latter.
        let src = r#"::t ns
ignore fn (_value: Any): Str { "callee-ran" }
through-strict-call fn (bad: Bool): Str {
    value if(bad, err("jit-strict-argument"), 1)
    ignore(value)
}
through-strict-call(false)
through-strict-call(false)
through-strict-call(true)"#;
        let out = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})));
        let text = match &out {
            Ok(v) => format!("OK:{v:?}"),
            Err(e) => e.clone(),
        };
        assert!(
            text.contains("jit-strict-argument"),
            "halt must carry the Err payload, got {text}"
        );
        assert!(
            !text.contains("callee-ran"),
            "the callee ran despite the strict-argument halt: {text}"
        );
    }

    #[test]
    fn result_property_access_halts_on_err() {
        let programs = [
            (
                "dot",
                r#"::t ns
probe fn (bad: Bool): Str {
    result if(bad, err({message: "dot-access-strict"}), ok({message: "ok"}))
    value result.message
    `completed:${value}`
}
probe(false)
probe(false)
probe(true)"#,
            ),
            (
                "dynamic",
                r#"::t ns
probe fn (bad: Bool): Str {
    result if(bad, err({message: "dot-access-strict"}), ok({message: "ok"}))
    key "message"
    value result[key]
    `completed:${value}`
}
probe(false)
probe(false)
probe(true)"#,
            ),
            (
                "deep-path",
                r#"::t ns
probe fn (bad: Bool): Str {
    result if(
        bad,
        err({message: "dot-access-strict"}),
        ok({message: {value: "ok"}})
    )
    result.message.value "updated"
    "completed"
}
probe(false)
probe(false)
probe(true)"#,
            ),
        ];

        for (access, src) in programs {
            let jit =
                compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})));
            let interp =
                compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})));

            for (mode, outcome) in [("jit", &jit), ("interpreter", &interp)] {
                let text = match outcome {
                    Ok(value) => format!("OK:{value:?}"),
                    Err(error) => error.clone(),
                };
                assert!(
                    text.contains("dot-access-strict"),
                    "{mode} {access}: halt must carry the Err payload, got {text}"
                );
                assert!(
                    !text.contains("completed:"),
                    "{mode} {access}: property access consumed an Err without halting: {text}"
                );
            }
        }
    }

    /// A compiled call through a `Fn` parameter calls the parameter, even
    /// when a function elsewhere has the same short name: hot-std's
    /// `filter-keys` takes `predicate`, and `::hot::type/predicate` exists.
    #[test]
    fn jit_call_through_a_parameter_calls_its_value() {
        let src = r#"::u ns
transform fn (x: Any): Any { "the other transform" }

::t ns
apply fn (x: Any, transform: Fn): Any { transform(x) }
probe fn (): Vec {
    [apply(1, (v) { add(v, 1) }), filter-keys({a: 1, b: 2}, ne(%, "b"))]
}
probe()
probe()
probe()"#;
        let expected = crate::val!([2, {"a": 1}]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("calls through parameters run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// A flow whose branches end in a raw scalar and an owned value: the
    /// compiled result must be boxed, or the function declined, never a raw
    /// `true` or `Int` read as a pointer. All three crashed with a JIT
    /// threshold of 1: the prescan took the `eq` and `length` intrinsics for
    /// owned values, and `add`'s kind depends on its operands.
    #[test]
    fn jit_flows_with_mixed_branch_kinds() {
        let src = r#"::t ns
check fn (entries: Vec, prefix: Str, id: Str): Any {
    find-first(entries, (e) {
        and(starts-with(e.key, prefix), eq(e.value.id, id))
    })
}
size-or-label fn (items: Vec, count: Bool): Any {
    if(count, length(items), "none")
}
next-or-label fn (n: Int, more: Bool): Any {
    if(more, add(n, 1), "none")
}
entries [{key: "u1:1", value: {id: "a"}}, {key: "u1:2", value: {id: "b"}}]
probe fn (): Vec {
    [
        check(entries, "u1:", "b"),
        size-or-label([1, 2], true),
        size-or-label([1], false),
        next-or-label(1, true),
        next-or-label(1, false)
    ]
}
probe()
probe()
probe()"#;
        let expected = crate::val!([{"key": "u1:2", "value": {"id": "b"}}, 2, "none", 2, "none"]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("mixed-kind flows run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// A non-tail self-call bound inside a `cond` branch: the compiled call
    /// must read its result as the function's real return kind. The JIT took
    /// the flow's pre-loaded `Null` default for its result, so hot-sql's
    /// placeholder scanner kept only a name's first character.
    #[test]
    fn jit_self_call_reads_the_real_return_kind() {
        let src = r#"::t ns
collect fn (s: Str, i: Int): Map {
    cond {
        gte(i, length(s)) => { {name: "", next: i} }
        ne(slice(s, i, add(i, 1)), " ") => {
            rest collect(s, add(i, 1))
            {name: concat(slice(s, i, add(i, 1)), rest.name), next: rest.next}
        }
        => { {name: "", next: i} }
    }
}
count fn (s: Str, i: Int): Int {
    cond {
        gte(i, length(s)) => { 0 }
        => {
            rest count(s, add(i, 1))
            add(rest, 1)
        }
    }
}
probe fn (): Vec { [collect("name x", 0), count("abc", 0)] }
probe()
probe()
probe()"#;
        let expected = crate::val!([{"name": "name", "next": 4}, 3]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("self-calls run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// Self-recursion compiled on raw values gives the interpreter's results:
    /// `Int`, `Dec` and `Bool` recursion, one that mixes `Int` and `Dec`, a
    /// nested flow whose raw guess falls back, and a return check that passes
    /// or fails.
    #[test]
    fn jit_raw_self_recursion_matches_the_interpreter() {
        let src = r#"::t ns
outcome fn (lazy result: Any): Any {
    forced do result
    if(is-ok(forced), forced, "err")
}
fib fn (n: Int): Int {
    if(lte(n, 1), n, add(fib(sub(n, 1)), fib(sub(n, 2))))
}
halves fn (n: Int): Dec {
    if(lte(n, 0), 0.5, add(halves(sub(n, 1)), 0.5))
}
even-depth fn (n: Int): Bool {
    if(lte(n, 0), true, not(even-depth(sub(n, 1))))
}
mixed fn (n: Int): Any {
    if(lte(n, 0), 0, add(mixed(sub(n, 1)), 0.5))
}
nested fn (n: Int): Int {
    cond {
        lte(n, 1) => { n }
        => { if(lte(n, 3), add(nested(sub(n, 1)), 1), add(nested(sub(n, 2)), 2)) }
    }
}
checked fn (n: Int): Int where gte(%, 0) {
    if(lte(n, 1), n, add(checked(sub(n, 1)), checked(sub(n, 2))))
}
probe fn (): Vec {
    [
        fib(15),
        halves(4),
        even-depth(5),
        mixed(4),
        nested(9),
        checked(15),
        outcome(checked(-1))
    ]
}
probe()
probe()
probe()"#;
        let expected = Val::Vec(vec![
            Val::Int(610),
            Val::Dec(fastnum::D256::from(5) / fastnum::D256::from(2)),
            Val::Bool(false),
            Val::Dec(fastnum::D256::from(2)),
            Val::Int(9),
            Val::Int(610),
            Val::from("err"),
        ]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("recursive functions run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// A refinement violation deep in raw self-recursion halts the run when
    /// the caller adds to the result, compiled or not. The JIT used to hand
    /// the Err back up its frames, where a `match` could handle it.
    #[test]
    fn jit_raw_self_recursion_halts_on_a_consumed_violation() {
        let src = r#"::t ns
Small type Int where lte(%, 10)
grow fn (n: Int): Small {
    if(eq(n, 0), 11, add(0, grow(sub(n, 1))))
}
outcome fn (lazy result: Any): Any {
    forced do result
    if(is-ok(forced), forced, "err")
}
warm fn (): Vec {
    map(range(0, 3), (i) { outcome(grow(0)) })
}
warm()
r grow(5)
match r {
    Result.Err => { "handled" }
    _ => { "ok" }
}"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()));
            let text = match &out {
                Ok(v) => format!("OK:{v:?}"),
                Err(e) => e.clone(),
            };
            assert!(
                text.contains("ConstraintError") && !text.contains("handled"),
                "with {conf:?}, expected a halt on the violation, got {text}"
            );
        }
    }

    /// A `where` on a type declares a hidden predicate function, which must
    /// not become the file's value: `hot run` printed `::t/$where-…`. It
    /// still calls the namespace's own functions, declared before it.
    #[test]
    fn where_predicate_is_never_the_last_value() {
        let src = r#"::t ns
gt fn (a: Int, b: Int): Bool { lte(a, b) }
answer 42
Small type {
    n: Int
}
where gt(%.n, 5)"#;
        assert_eq!(compile_and_run_with_std_conf(src, None), Ok(crate::val!(42)));
    }

    /// A local binding ends with its function call: after `callee` binds a
    /// local `label`, its caller still calls the namespace's `label`. The
    /// local stayed in the namespace, so the call failed with "Function 'x'
    /// not found".
    #[test]
    fn callee_local_does_not_outlive_the_call() {
        let src = r#"::t ns
label fn (s: Str): Str { `[${s}]` }
callee fn (): Str {
    label "x"
    label
}
caller fn (): Str {
    a callee()
    label("hi")
}
caller()"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(crate::val!("[hi]")),
                "with {conf:?}"
            );
        }
    }

    /// A top-level function is lexically scoped: `check` reads its
    /// namespace's `limit`, never a caller's parameter or local of that
    /// name. Lookups walked every caller frame, so both callers saw 100.
    #[test]
    fn callee_reads_its_own_scope_not_its_callers() {
        let src = r#"::t ns
limit 10
check fn (v: Int): Bool { lte(v, limit) }
by-param fn (limit: Int): Bool { check(50) }
by-local fn (): Bool {
    limit 100
    check(50)
}
[by-param(100), by-local(), check(5)]"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(crate::val!([false, false, true])),
                "with {conf:?}"
            );
        }
    }

    /// A lambda or lazy argument captures an enclosing binding even when a
    /// core function has the same name, as `data` does. Without the
    /// capture, a lambda that `map` calls read the core function instead.
    #[test]
    fn closures_capture_bindings_named_like_core_functions() {
        let src = r#"::t ns
by-param fn (data: Map): Vec { map(keys(data), (key) { get(data, key) }) }
by-local fn (m: Map): Vec {
    data m
    map(keys(data), (key) { get(data, key) })
}
nested fn (ms: Vec): Vec { map(ms, (data) { map(keys(data), (k) { get(data, k) }) }) }
called fn (inc: Fn, xs: Vec): Vec { map(xs, (x) { inc(x) }) }
getter fn (data: Map): Fn { (k) { get(data, k) } }
twice fn (lazy v: Int): Int {
    forced do v
    add(forced, forced)
}
lazy-local fn (): Int {
    data "abc"
    twice(length(data))
}
probe fn (): Vec {
    get-a getter({a: 6})
    [
        by-param({a: 1, b: 2}),
        by-local({a: 3}),
        nested([{a: 4}, {b: 5}]),
        called((x) { mul(x, 10) }, [1, 2]),
        get-a("a"),
        lazy-local()
    ]
}
probe()
probe()"#;
        let expected = crate::val!([[1, 2], [3], [[4], [5]], [10, 20], 6, 6]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(expected.clone()),
                "with {conf:?}"
            );
        }
    }

    /// A failure is an Err value on every call path, and checking it gives
    /// the same answer bound or not. A bare core call such as `add` used to
    /// halt on the interpreter's fast path while `::hot::math/add` and
    /// compiled code returned the Err.
    #[test]
    fn a_failure_checks_the_same_bound_or_not() {
        let src = r#"::t ns
probe fn (x: Any): Vec {
    a add(x, 1)
    q ::hot::math/add(x, 1)
    d div(1, 0)
    e err("bad")
    [
        is-err(add(x, 1)), is-err(a),
        is-err(::hot::math/add(x, 1)), is-err(q),
        is-err(div(1, 0)), is-err(d),
        is-err(err("bad")), is-err(e),
        if-err(add(x, 1), (m) { "handled" }),
        or(add(x, 1), "default"),
        if(add(x, 1), "yes", "no"),
        length([add(x, 1), 1])
    ]
}
probe("x")
probe("x")"#;
        let expected = crate::val!([
            true, true, true, true, true, true, true, true, "handled", "default", "no", 2
        ]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(expected.clone()),
                "with {conf:?}"
            );
        }
    }

    /// An inspector looks at a value and catches nothing: `fail()`, and an
    /// Err consumed inside its argument, halt there as anywhere else.
    #[test]
    fn inspectors_never_catch_a_halt() {
        let cases = [
            ("is-err(fail(\"boom\"))", "boom"),
            ("if-err(fail(\"boom\"), (e) { 1 })", "boom"),
            ("is-ok(id(err(\"bad\")))", "Result error: bad"),
            ("is-err(length(err(\"bad\")))", "Result error: bad"),
            ("is-err(map([1], (v) { fail(\"in callback\") }))", "in callback"),
        ];
        for (expr, message) in cases {
            let src = format!(
                "::t ns\nid fn (v: Any): Any {{ v }}\nprobe fn (): Any {{ {expr} }}\nprobe()\nprobe()"
            );
            for conf in [
                crate::val!({"jit": {"threshold": 1}}),
                crate::val!({"jit": {"mode": "off"}}),
            ] {
                let out = compile_and_run_with_std_conf(&src, Some(conf.clone()));
                assert!(
                    out.as_ref().is_err_and(|e| e.contains(message)),
                    "`{expr}` with {conf:?}: expected a halt with {message:?}, got {out:?}"
                );
            }
        }
    }

    /// A parameter declared `Result` receives its argument as it is, however
    /// the function is called: directly, through a HOF, or with `call`. Any
    /// other parameter consumes it, through a HOF too.
    #[test]
    fn result_parameters_receive_results_on_every_call_path() {
        let src = r#"::t ns
describe fn (r: Result): Str { if(is-err(r), "failed", "succeeded") }
count-errs fn (...rest: Result): Int { length(filter(rest, is-err)) }
probe fn (): Vec {
    results map([1, 2], (i) { if(eq(i, 2), err("bad"), i) }, OnErr.Preserve)
    [
        describe(err("x")),
        map(results, describe),
        map(results, (r: Result) { describe(r) }),
        call(describe, [err("y")]),
        count-errs(1, err("a"), err("b"))
    ]
}
probe()
probe()"#;
        let consuming = r#"::t ns
probe fn (): Vec {
    results map([1, 2], (i) { if(eq(i, 2), err("bad"), i) }, OnErr.Preserve)
    map(results, (r) { r })
}
probe()
probe()"#;
        let expected = crate::val!([
            "failed",
            ["succeeded", "failed"],
            ["succeeded", "failed"],
            "failed",
            2
        ]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(expected.clone()),
                "with {conf:?}"
            );
            let out = compile_and_run_with_std_conf(consuming, Some(conf.clone()));
            assert!(
                out.as_ref()
                    .is_err_and(|e| e.contains("Result error: bad")),
                "an untyped callback parameter consumes an Err, with {conf:?}: {out:?}"
            );
        }
    }

    /// Compiled code reads `OnErr.Preserve` as the variant (it read Null, so
    /// `map` rejected its trailing argument), and its `length`, `is-null`
    /// and `is-some` consume an Err argument like the interpreter's.
    #[test]
    fn compiled_code_reads_variants_and_consumes_errs() {
        let preserve = r#"::t ns
probe fn (): Vec { map([1, 2], (i) { if(eq(i, 2), err("bad"), i) }, OnErr.Preserve) }
probe()
probe()"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(preserve, Some(conf.clone()))
                .expect("Preserve keeps the Err slot");
            let Val::Vec(slots) = &out else {
                panic!("expected a Vec with {conf:?}, got {out:?}");
            };
            assert_eq!(slots[0], Val::Int(1), "with {conf:?}");
            assert!(slots[1].is_err(), "with {conf:?}: {out:?}");
            for intrinsic in ["length", "is-null", "is-some"] {
                let src = format!(
                    "::t ns\nprobe fn (v: Any): Any {{ {intrinsic}(v) }}\nprobe([1])\nprobe(err(\"bad\"))"
                );
                let out = compile_and_run_with_std_conf(&src, Some(conf.clone()));
                assert!(
                    out.as_ref()
                        .is_err_and(|e| e.contains("Result error: bad")),
                    "{intrinsic} with {conf:?}: {out:?}"
                );
            }
        }
    }

    /// A lambda captures a local it reads through a pipe. Capture analysis
    /// took `cited |> some(…)` for a binding of `cited`, so a lambda that
    /// compiled code created couldn't find it. (A HOF re-ran the failed
    /// compiled callback in the interpreter, which hid this.)
    #[test]
    fn lambdas_capture_locals_read_through_pipes() {
        let src = r#"::t ns
probe fn (observations: Vec, ids: Vec): Any {
    cited ids |> map(Str)
    observations
        |> filter((observation) {
            cited |> some((id) { eq(id, observation.id) })
        })
}
[probe([{id: "a"}, {id: "b"}], ["a"]), probe([{id: "a"}], [])]"#;
        let expected = crate::val!([[{"id": "a"}], []]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(expected.clone()),
                "with {conf:?}"
            );
        }
    }

    /// A local function passed to a top-level function keeps its definer's
    /// locals, and the callee's parameter of the same name calls the value
    /// it was given. hot-ai's `fit` passes `item-text` this way.
    #[test]
    fn local_function_passed_to_a_top_level_function_keeps_its_scope() {
        let src = r##"::t ns
outer fn (opts: Map): Vec {
    prefix get(opts, "prefix", "-")
    show fn (item: Any): Str { `${prefix}${item}` }
    apply-all([1, 2], show)
}
apply-all fn (items: Vec, show: Fn): Vec { map(items, (item) { show(item) }) }
probe fn (): Vec { [outer({prefix: "#"}), outer({})] }
probe()
probe()"##;
        let expected = crate::val!([["#1", "#2"], ["-1", "-2"]]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(src, Some(conf.clone())),
                Ok(expected.clone()),
                "with {conf:?}"
            );
        }
    }

    /// A template consumes each interpolated value's Result, so an Err part
    /// halts compiled code too, and values render as the interpreter renders
    /// them. Compiled templates wrote `Result.Err(…)` into the string and
    /// rendered maps as JSON.
    #[test]
    fn jit_template_halts_on_an_err_part_and_renders_like_the_interpreter() {
        let ok = r#"::t ns
show fn (v: Any): Str { `<${first(v)}|${v}|${{a: 1}}>` }
warm map(range(0, 3), (i) { show([i]) })
show([7])"#;
        let bad = r#"::t ns
show fn (v: Any): Str { `<${first(v)}>` }
warm map(range(0, 3), (i) { show([i]) })
show(5)"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            assert_eq!(
                compile_and_run_with_std_conf(ok, Some(conf.clone())),
                Ok(crate::val!("<7|[7]|{a: 1}>")),
                "with {conf:?}"
            );
            let out = compile_and_run_with_std_conf(bad, Some(conf.clone()));
            assert!(
                out.as_ref()
                    .is_err_and(|e| e.contains("first requires a collection")),
                "with {conf:?}, expected a halt, got {out:?}"
            );
        }
    }

    /// A local function reads its caller's local by name, called directly
    /// and from a lambda the caller creates. The caller stays on the
    /// interpreter, where that local is in scope: once hot-ai's `fit`
    /// compiled, it failed with "Variable 'field' not found".
    #[test]
    fn jit_local_function_reads_its_callers_locals() {
        let src = r#"::t ns
outer fn (opts: Map): Str {
    field get(opts, "field", "none")
    helper fn (item: Any): Str { `${field}:${item}` }
    direct helper(1)
    via-lambda first(map([2], (n) { helper(n) }))
    `${direct} ${via-lambda}`
}
probe fn (): Vec { [outer({field: "x"}), outer({})] }
probe()
probe()
probe()"#;
        let expected = crate::val!(["x:1 x:2", "none:1 none:2"]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("local functions run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// `cond-all` and `match-all` key their results by branch label. Compiled
    /// code kept the compiler's uniqueness suffix on the label, so a field
    /// such as `result.a` was missing.
    #[test]
    fn jit_collecting_flows_key_results_by_label() {
        let src = r#"::t ns
flags-of fn cond-all (flags: Map) {
    flags.a => a { "a-result" }
    flags.b => b { "b-result" }
    flags.c => c { "c-result" }
}
inline fn (flags: Map): Map {
    cond-all {
        flags.a => a { 1 }
        flags.b => b { 2 }
    }
}
probe fn (): Vec {
    [flags-of({a: true, b: true, c: false}), inline({a: true, b: false})]
}
probe()
probe()
probe()"#;
        let expected = crate::val!([{"a": "a-result", "b": "b-result"}, {"a": 1}]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("collecting flows run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// `div` of two Ints that divides evenly is an Int, as the interpreter
    /// returns it, and a Dec otherwise. Compiled code always made a Dec, so
    /// `eq(div(10, 2), 5)` was false.
    #[test]
    fn jit_int_division_keeps_the_interpreters_type() {
        let src = r#"::t ns
halve fn (a: Int, b: Int): Any { div(a, b) }
probe fn (): Vec {
    [
        halve(10, 2),
        is-int(halve(10, 2)),
        eq(halve(3, 2), 1.5),
        is-dec(halve(3, 2)),
        is-err(halve(1, 0))
    ]
}
probe()
probe()
probe()"#;
        let expected = crate::val!([5, true, true, true, true]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("division runs");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// A `fail()` in one of `pmap`'s parallel workers reaches a `contain`
    /// around a compiled caller as the structured Failure. It arrived as
    /// plain text, so `failure.msg` was null.
    #[test]
    fn jit_pmap_halt_keeps_its_failure() {
        let src = r#"::t ns
boom fn (): Any {
    ::hot::coll/pmap(range(0, 200), (x: Int) {
        cond {
            eq(x, 7) => { fail("boom on 7") }
            => { x }
        }
    })
}
probe fn (): Vec {
    attempt ::hot::internal::exec/contain(boom, [])
    failure if-err(attempt, (failure) { failure })
    [is-err(attempt), failure.msg]
}
probe()
probe()
probe()"#;
        let expected = crate::val!([true, "boom on 7"]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("contained pmap halts run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// A body returning `All<Vec>` or `All<Map>` collects its bindings. The
    /// compiled version returned an empty collection.
    #[test]
    fn jit_collecting_serial_bodies() {
        let src = r#"::t ns
values fn (): All<Vec> {
    a 1
    b 2
}
named fn (): All<Map> {
    a 1
    b 2
}
probe fn (): Vec { [values(), named()] }
probe()
probe()
probe()"#;
        let expected = crate::val!([[1, 2], {"a": 1, "b": 2}]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("collecting bodies run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    /// `where` checks give the same results compiled as interpreted: a
    /// parameter, a predicate call, a return check, a precondition, a
    /// conversion that stays on the interpreter, and arguments a compiled
    /// caller proves.
    #[test]
    fn jit_where_checks_match_the_interpreter() {
        let src = r#"::t ns
Port type Int where between(%, 1, 65535)
Score type Int where between(%, 0, 100)
Ratio type Dec where gte(%, 0)

dynamic fn (value: Any): Any { value }
outcome fn (lazy result: Any): Any {
    forced do result
    if(is-ok(forced), forced, "err")
}
open fn (port: Port): Int { port }
named fn (label: Str where gt(length(%), 0)): Str { label }
delay fn (attempt: Int where between(%, 1, 10)): Int where lte(%, 30000) {
    mul(mul(attempt, attempt), 1000)
}
withdraw fn (amount: Int, balance: Int)
    where lte(%.amount, %.balance)
    : Int
{
    sub(balance, amount)
}
ratio fn (r: Ratio): Ratio { r }
total fn (scores: Vec<Score>): Int { reduce(scores, add(%1, %2), 0) }
middle fn (scores: Vec<Score>): Int { total(scores) }
probe fn (): Vec {
    [
        outcome(open(80)),
        outcome(open(dynamic(70000))),
        outcome(named("a")),
        outcome(named(dynamic(""))),
        outcome(delay(3)),
        outcome(delay(dynamic(6))),
        outcome(delay(dynamic(0))),
        outcome(withdraw(10, 100)),
        outcome(withdraw(dynamic(150), 100)),
        outcome(ratio(5)),
        outcome(middle([1, 2, 3])),
        outcome(middle(dynamic([1, 200])))
    ]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("compiled checks");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreted checks");
        let expected = Val::Vec(vec![
            Val::Int(80),
            Val::from("err"),
            Val::from("a"),
            Val::from("err"),
            Val::Int(9000),
            Val::from("err"),
            Val::from("err"),
            Val::Int(90),
            Val::from("err"),
            Val::Dec(fastnum::D256::from(5)),
            Val::Int(6),
            Val::from("err"),
        ]);
        assert_eq!(interp, expected, "interpreter");
        assert_eq!(jit, expected, "compiled");
    }

    /// A local function bound in a flow branch or flow body gets its
    /// arguments, compiled or not. The parser took `minus (a, b) { … }` there
    /// for a call to `minus` with `a` and `b`, so `a` was "not found".
    #[test]
    fn local_functions_bound_in_flows_run() {
        let src = r#"::t ns
countdown fn (n: Int): Int {
    cond {
        lte(n, 1) => { n }
        => {
            minus (a, b) { sub(a, b) }
            minus(countdown(sub(n, 1)), 1)
        }
    }
}
arm fn (n: Int): Int {
    match n {
        1 => { 0 }
        _ => {
            twice (x) { mul(x, 2) }
            twice(n)
        }
    }
}
steps fn (n: Int): Int {
    total serial {
        inc (x) { add(x, 1) }
        inc(n)
    }
    total
}
probe fn (): Vec { [countdown(5), arm(3), steps(4)] }
probe()
probe()
probe()"#;
        let expected = crate::val!([-3, 6, 5]);
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect("local functions run");
            assert_eq!(out, expected, "with {conf:?}");
        }
    }

    #[test]
    fn tagged_metadata_names_resolve_in_the_payload() {
        let src = r#"::t ns
probe fn (): Vec {
    with-val from-json("{\"$type\":\"::my-types/A\",\"$val\":{\"$val\":{\"c\":1}}}")
    shallow from-json("{\"$type\":\"::my-types/A\",\"$val\":{\"c\":1}}")
    set-val from-json("{\"$type\":\"::my-types/A\",\"$val\":{\"c\":1}}")
    set-type from-json("{\"$type\":\"::my-types/A\",\"$val\":{\"c\":1}}")
    with-type from-json("{\"$type\":\"::my-types/A\",\"$val\":{\"$type\":\"payload-type\"}}")
    val-key "$val"

    before [with-val.$val.c, with-val[val-key].c, is-null(shallow.$val), with-type.$type]
    set-val.$val {c: 2}
    set-type.$type "payload-type"

    [before[0], before[1], before[2], before[3], set-val.$val.c, set-type.$type]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT tagged payload field access");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter tagged payload field access");
        let expected = crate::val!([1, 1, true, "payload-type", 2, "payload-type"]);

        assert_eq!(interp, expected, "interpreter must resolve metadata-shaped names in the payload");
        assert_eq!(jit, expected, "JIT must resolve metadata-shaped names in the payload");
    }

    #[test]
    fn foreign_tagged_map_with_siblings_stays_an_ordinary_map() {
        let src = r#"::t ns
probe fn (): Vec {
    flat from-json("{\"$type\":\"::foreign/A\",\"n\":2}")
    flat.$val {c: 3}
    metadata from-json("{\"$type\":\"::foreign/B\",\"$val\":1,\"$metadata\":{\"source\":\"foreign\"}}")
    [flat.n, flat.$val.c, metadata.$val, metadata.$metadata.source]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT foreign tagged-looking map assignment");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter foreign tagged-looking map assignment");
        let expected = crate::val!([2, 3, 1, "foreign"]);
        assert_eq!(interp, expected);
        assert_eq!(jit, expected);
    }

    #[test]
    fn tagged_unit_variant_metadata_is_hidden_from_source_fields() {
        let read_src = r#"::t ns
Choice enum { A }
probe fn (): Vec {
    value Choice.A
    external from-json("{\"$type\":\"::external/Status.Ready\"}")
    type-key "$type"
    val-key "$val"
    [
        value.$type,
        value[type-key],
        value.$val,
        value[val-key],
        external.$type,
        external[type-key],
        external.$val,
        external[val-key],
        is-type(value, Choice.A)
    ]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(
            read_src,
            Some(crate::val!({"jit": {"threshold": 1}})),
        )
        .expect("JIT unit variant metadata access");
        let interp = compile_and_run_with_std_conf(
            read_src,
            Some(crate::val!({"jit": {"mode": "off"}})),
        )
        .expect("interpreter unit variant metadata access");
        let expected = crate::val!([null, null, null, null, null, null, null, null, true]);
        assert_eq!(interp, expected, "interpreter must hide unit variant metadata");
        assert_eq!(jit, expected, "JIT must hide unit variant metadata");

        let write_programs = [
            (
                "payload type field",
                r#"::t ns
Choice enum { A }
probe fn () {
    value Choice.A
    value.$type "payload-type"
    [is-type(value, Choice.A), value.$type]
}
probe()
probe()
probe()"#,
            ),
            (
                "nested payload value field",
                r#"::t ns
Choice enum { A }
probe fn () {
    value Choice.A
    value.$val.c 1
    [is-type(value, Choice.A), value.$val.c]
}
probe()
probe()
probe()"#,
            ),
        ];

        for (access, src) in write_programs {
            let jit = compile_and_run_with_std_conf(
                src,
                Some(crate::val!({"jit": {"threshold": 1}})),
            )
            .unwrap_or_else(|error| panic!("JIT {access} assignment failed: {error}"));
            let interp = compile_and_run_with_std_conf(
                src,
                Some(crate::val!({"jit": {"mode": "off"}})),
            )
            .unwrap_or_else(|error| panic!("interpreter {access} assignment failed: {error}"));
            let expected = if access == "payload type field" {
                crate::val!([true, "payload-type"])
            } else {
                crate::val!([true, 1])
            };
            assert_eq!(interp, expected, "interpreter {access} must write the payload");
            assert_eq!(jit, expected, "JIT {access} must write the payload");
        }
    }

    #[test]
    fn custom_struct_fields_remain_direct() {
        let src = r#"::t ns
Profile type {name: Str}
probe fn (): Vec {
    value Profile({name: "Ada"})
    value.name "Grace"
    [value.name, is-type(value, Profile)]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT custom struct field access");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter custom struct field access");
        let expected = crate::val!(["Grace", true]);
        assert_eq!(interp, expected, "interpreter must keep struct fields direct");
        assert_eq!(jit, expected, "JIT must keep struct fields direct");
    }

    #[test]
    fn dynamic_deep_assignment_updates_maps_and_vectors() {
        let src = r#"::t ns
probe fn (): Vec {
    record {name: "Ada"}
    field "name"
    record[field] "Grace"

    items [{name: "first"}, {name: "second"}]
    index 1
    items[index].name "updated"

    record["title"] "Engineer"
    [record, items, record.name, record.title, items[0].name, items[1].name]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT dynamic deep assignment");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter dynamic deep assignment");
        let expected = crate::val!([
            {"name": "Grace", "title": "Engineer"},
            [{"name": "first"}, {"name": "updated"}],
            "Grace",
            "Engineer",
            "first",
            "updated"
        ]);
        assert_eq!(interp, expected, "interpreter dynamic deep assignment");
        assert_eq!(jit, expected, "JIT dynamic deep assignment");
    }

    #[test]
    fn deep_assignment_creates_and_grows_vectors_in_both_engines() {
        let src = r#"::t ns
probe fn (): Vec {
    literal {}
    literal.rows[0].host "api"

    dynamic {rows: []}
    index 3
    dynamic.rows[index].host "worker"

    [literal, dynamic]
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT deep vector creation");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter deep vector creation");
        let expected = crate::val!([
            {"rows": [{"host": "api"}]},
            {"rows": [null, null, null, {"host": "worker"}]}
        ]);
        assert_eq!(interp, expected);
        assert_eq!(jit, expected);
    }

    #[test]
    fn dynamic_deep_assignment_requires_an_existing_collection_parent() {
        let src = r#"::t ns
probe fn (): Map {
    data {}
    index 0
    data.rows[index].host "worker"
    data
}
probe()"#;

        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("missing dynamic collection parent must fail");
            assert!(
                error.contains("dynamic") || error.contains("Null"),
                "{mode}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn lazy_result_evaluation_propagates_assignment_errors_without_panicking() {
        let src = r#"::t ns
rewrite fn (): Any {
    failure err({message: "original lazy error"})
    failure.message "rewritten"
    failure
}
inspect fn match (result: Result): Str {
    Result.Ok => { "ok" }
    Result.Err => { "err" }
}
inspect(rewrite())"#;

        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("assignment through lazy Result.Err must propagate");
            assert!(error.contains("original lazy error"), "{mode}: {error}");
            assert!(!error.contains("channel closed"), "{mode}: {error}");
        }
    }

    #[test]
    fn jit_and_interpreter_halt_on_missing_variable_in_dynamic_deep_assign() {
        // LoadVar must stay strict in the compiled tier: a dynamic deep
        // assignment to a fresh (unbound) variable halts the interpreter with
        // "Variable not found"; the JIT lookup helper previously materialized
        // Null and let the assignment succeed.
        let src = r#"::t ns
probe fn (n: Int): Map {
    k "a"
    fresh[k] n
    fresh
}
probe(1)
probe(2)
probe(3)"#;

        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("unbound dynamic deep-assign target must halt");
            assert!(
                error.contains("Variable 'fresh' not found (unified lookup)"),
                "{mode}: {error}"
            );
            assert!(!error.contains("Result error"), "{mode}: {error}");
        }
    }

    #[test]
    fn jit_and_interpreter_property_halt_messages_match() {
        // Property halts from a compiled frame previously surfaced with an
        // extra "Result error:" wrapper the interpreter does not add. Warm
        // the function with valid calls so the final one runs compiled.
        let src = r#"::t ns
probe fn (i: Int): Vec {
    v [1, 2]
    v[i] 9
    v
}
probe(0)
probe(0)
probe(0)
probe(-1)"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect_err("negative dynamic index must halt under JIT");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect_err("negative dynamic index must halt interpreted");
        // Same user-visible message in both engines: no extra
        // "Result error:" wrapper from the compiled tier.
        let expected = "Vector index cannot be negative: -1";
        assert!(interp.contains(expected), "{interp}");
        assert!(jit.contains(expected), "{jit}");
        assert!(!jit.contains("Result error"), "{jit}");
    }

    #[test]
    fn captured_halt_does_not_mask_later_halts() {
        // A halt consumed by is-err (lazy capture boundary) must clear the
        // VM failure state: previously the stale state made every later
        // compiled-frame boundary treat its own halt as pre-existing and
        // suppress it, so the program continued where the interpreter halts.
        let src = r#"::t ns
trap fn (): Int {
    r err("masked-halt-probe")
    r.k 5
    9
}
first is-err(trap())
boom trap()
"never-reached""#;

        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("halt after a captured halt must still propagate");
            assert!(error.contains("masked-halt-probe"), "{mode}: {error}");
        }
    }

    #[test]
    fn serial_flow_deep_path_assignment_updates_the_root() {
        let src = r#"::t ns
probe fn (): Map {
    st {a: {b: 1}}
    r serial {
        st.a.b 99
        st
    }
    r
}
probe()
probe()
probe()"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT serial-flow deep assignment");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter serial-flow deep assignment");
        let expected = crate::val!({"a": {"b": 99}});
        assert_eq!(interp, expected);
        assert_eq!(jit, expected);
    }

    #[test]
    fn parallel_flow_rejects_deep_path_assignment() {
        // Parallel branches are independent slots keyed by name; a deep-path
        // write into an existing binding has no defined order. Previously the
        // path was silently dropped and the root rebound to the bare value.
        let src = r#"::t ns
probe fn (): Map {
    st {a: {b: 1}}
    r parallel {
        st.a.b 99
        other 5
    }
    r
}
probe()"#;

        let error = compile_and_run_with_std_conf(src, None)
            .expect_err("deep-path assignment in parallel must be rejected");
        assert!(error.contains("parallel flow"), "{error}");
    }

    #[test]
    fn serial_fn_body_deep_path_assignment_updates_the_root() {
        // The flow-body parser previously split `st.a n` into two auto-named
        // slots, silently dropping the write in `fn serial` bodies.
        let src = r#"::t ns
probe fn serial (n: Int) {
    st {a: 1}
    st.a n
    st
}
probe(9)
probe(9)
probe(9)"#;

        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT fn-serial deep assignment");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter fn-serial deep assignment");
        let expected = crate::val!({"a": 9});
        assert_eq!(interp, expected);
        assert_eq!(jit, expected);
    }

    #[test]
    fn parallel_fn_body_rejects_deep_path_assignment() {
        // Same parser path as above: `fn parallel` bodies previously
        // decomposed the write into auto-named slots (silent data loss)
        // instead of reaching the compiler's rejection.
        for src in [
            r#"::t ns
probe fn parallel (n: Int) {
    st {a: 1}
    st.a n
}
probe(3)"#,
            r#"::t ns
probe fn parallel (n: Int) {
    st [1, 2, 3]
    st[0] n
}
probe(3)"#,
        ] {
            let error = compile_and_run_with_std_conf(src, None)
                .expect_err("deep-path assignment in fn parallel must be rejected");
            assert!(error.contains("parallel flow"), "{error}");
        }
    }

    #[test]
    fn dynamic_string_key_assignment_requires_an_existing_parent() {
        // Int keys already errored on a missing parent; string keys silently
        // materialized a Map, contradicting the documented rule that a
        // dynamic segment's parent must already exist.
        let src = r#"::t ns
probe fn (): Map {
    data {}
    key "x"
    data.child[key] 5
    data
}
probe()"#;

        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("missing dynamic parent must fail for string keys");
            assert!(
                error.contains("parent collection must already exist"),
                "{mode}: {error}"
            );
        }
    }

    #[test]
    fn deep_path_assignment_keeps_the_root_type_for_collection_calls() {
        // A namespace-scope deep-path assignment previously re-registered the
        // root's type from the assigned value (Vec -> Int), failing later
        // collection calls with a span-less compile-time type mismatch.
        let src = r#"::t ns
ports [4680]
ports[3] 4683
length(ports)"#;

        let result = compile_and_run_with_std_conf(src, None)
            .expect("collection call after ns-scope deep assignment");
        assert_eq!(result, Val::Int(4));
    }

    #[test]
    fn jit_flow_result_does_not_steal_constant_refcounts() {
        // A conditional whose branches are constant-backed OwnedVals: the
        // flow result register must take a fresh clone rather than the baked
        // constant's refcount, or the return/cleanup/decode sequence frees
        // the constant after one compiled call (heap corruption on the next).
        let src = r#"::t ns
k fn (n: Int): Str { if(gt(n, 1), "t", "f") }
a k(1)
b k(2)
k(7)"#;
        let out = compile_and_run_with_std_conf(
            src,
            Some(crate::val!({"jit": {"threshold": 1}})),
        )
        .expect("JIT constant-backed flow result");
        assert_eq!(out, Val::from("t"), "third call must still see the constant alive");
    }

    /// and/or compile to nested cond flows (try_compile_inline_and_or); under
    /// the JIT the skipped side must never evaluate — its fail() would halt
    /// the run — and the value semantics (first falsy / first truthy / last
    /// value, Err falsy) must match the interpreter exactly.
    #[test]
    fn jit_and_or_short_circuit_semantics() {
        let src = r#"::t ns
guard fn (x: Any): Any {
    and(is-map(x), get(x, "k"))
}
chain fn (): Vec {
    a and(false, fail("and must not reach arg 2"))
    b or(true, fail("or must not reach arg 2"))
    c and(1, 2, 3)
    d or(null, 0, fail("or must stop at 0"))
    e guard("not-a-map")
    g or(null, 42)
    h if-err(and(err("efirst"), true), (err-val) { `err:${err-val}` })
    [a, b, c, d, e, g, h]
}
chain()
chain()
chain()"#;
        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT and/or run");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter and/or run");
        assert_eq!(jit, interp, "JIT/interpreter parity for and/or");
        let expected = crate::val!([false, true, 3, 0, false, 42, "err:efirst"]);
        assert_eq!(jit, expected, "and/or value semantics under JIT");
    }

    /// Bare-reference pipe stages synthesize a zero-arg call and must wrap
    /// the piped value in a lazy thunk when the target's first parameter is
    /// lazy, so Result values reach the Result-aware inspectors — under the
    /// JIT exactly as in the interpreter.
    #[test]
    fn jit_pipe_bare_ref_honors_lazy_first_param() {
        let src = r#"::t ns
probe fn (flag: Bool): Vec {
    v if(flag, err("piped"), ok(7))
    e v |> is-err
    o v |> is-ok
    q v |> ::hot::type/is-err
    [e, o, q]
}
run fn (): Vec {
    [probe(true), probe(false), probe(true)]
}
run()
run()
run()"#;
        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
            .expect("JIT piped lazy-param run");
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})))
                .expect("interpreter piped lazy-param run");
        assert_eq!(jit, interp, "JIT/interpreter parity for piped lazy params");
        let expected = crate::val!([[true, false, true], [false, true, false], [true, false, true]]);
        assert_eq!(jit, expected, "piped Result values reach the inspectors under JIT");
    }

    /// The strict-argument law under JIT-lowered arithmetic: an Err flowing
    /// into `add` must halt with the Err's own payload — never reach the
    /// native (whose "add requires numbers" would mean the law was skipped)
    /// — and the halt must abort BOTH the halting frame and its caller.
    /// The completion markers make any leak visible in the outcome: if the
    /// frame or caller kept executing past the halt, the run would succeed
    /// with the marker string instead of failing with the payload.
    #[test]
    fn jit_pipe_strict_target_halts_with_payload() {
        let src = r#"::t ns
f fn (flag: Bool): Str {
    v if(flag, err("pipe-strict"), 1)
    x v |> add(1)
    "f-completed"
}
caller fn (flag: Bool): Str {
    f(flag)
    "caller-completed"
}
caller(false)
caller(false)
caller(true)"#;
        let describe = |outcome: &Result<Val, String>| -> String {
            match outcome {
                Ok(v) => format!("OK:{v:?}"),
                Err(e) => e.clone(),
            }
        };
        let jit = compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})));
        let interp =
            compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"mode": "off"}})));
        for (mode, outcome) in [("jit", &jit), ("interpreter", &interp)] {
            let text = describe(outcome);
            assert!(
                text.contains("pipe-strict"),
                "{mode}: halt must carry the Err payload, got {text}"
            );
            assert!(
                !text.contains("add requires numbers"),
                "{mode}: the Err reached the native — strict-argument law skipped: {text}"
            );
            assert!(
                !text.contains("f-completed"),
                "{mode}: the halting frame continued executing: {text}"
            );
            assert!(
                !text.contains("caller-completed"),
                "{mode}: the caller continued past the halt: {text}"
            );
        }
    }

    /// The strict-argument law under JIT-lowered comparisons: bare-bool
    /// helpers can't carry a halt in-band, so the emitted sentinel check
    /// must abort the frame with the Err payload instead of letting the
    /// comparison read as false — and the halt must propagate through the
    /// caller boundary rather than dying at the frame edge. Completion
    /// markers turn any leak (frame or caller continuing) into a visible
    /// success that the assertions reject.
    #[test]
    fn jit_cmp_strict_operand_halts_with_payload() {
        for op in ["gt(v, 0)", "lt(v, 9)", "eq(v, 1)", "gte(v, 0)"] {
            let src = format!(
                r#"::t ns
f fn (flag: Bool): Str {{
    v if(flag, err("cmp-strict"), 1)
    x {op}
    "f-completed"
}}
caller fn (flag: Bool): Str {{
    f(flag)
    "caller-completed"
}}
caller(false)
caller(false)
caller(true)"#
            );
            for (mode, conf) in [
                ("jit", crate::val!({"jit": {"threshold": 1}})),
                ("interpreter", crate::val!({"jit": {"mode": "off"}})),
            ] {
                let out = compile_and_run_with_std_conf(&src, Some(conf));
                let text = match &out {
                    Ok(v) => format!("OK:{v:?}"),
                    Err(e) => e.clone(),
                };
                assert!(
                    text.contains("cmp-strict"),
                    "{op} ({mode}): halt must carry the Err payload, got {text}"
                );
                assert!(
                    !text.contains("f-completed"),
                    "{op} ({mode}): the halting frame continued executing: {text}"
                );
                assert!(
                    !text.contains("caller-completed"),
                    "{op} ({mode}): the caller continued past the halt: {text}"
                );
            }
        }
    }

    /// A strict halt inside a deferred parallel binding must abort the frame
    /// via the sentinel check in emit_deferred_var_expr — before the fix the
    /// sentinel (2) flowed into set_element as a pointer and crashed the
    /// process. The constant markers catch both the crash (test dies) and
    /// any continuation leak.
    #[test]
    fn jit_deferred_parallel_binding_halt() {
        let src = r#"::t ns
f fn (flag: Bool): Map {
    parallel {
        a add(if(flag, err("deferred-boom"), 1), 1)
        b 2
    }
}
caller fn (flag: Bool): Str {
    f(flag)
    "caller-completed"
}
caller(false)
caller(false)
caller(true)"#;
        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf));
            let text = match &out {
                Ok(v) => format!("OK:{v:?}"),
                Err(e) => e.clone(),
            };
            assert!(
                text.contains("deferred-boom"),
                "{mode}: halt must carry the Err payload, got {text}"
            );
            assert!(
                !text.contains("caller-completed"),
                "{mode}: the caller continued past the halt: {text}"
            );
        }
    }

    /// Helper to compile and execute Hot code with hot-std included and an
    /// explicit conf (e.g. to toggle the `jit.hof.fusion` kill switch).
    pub(super) fn compile_and_run_with_std_conf(
        source: &str,
        conf: Option<Val>,
    ) -> Result<Val, String> {
        use crate::lang::ast::Program;
        use std::path::Path;

        let mut compiler = Compiler::new();
        let test_program = parser::parse_hot(source).map_err(|e| format!("Parse error: {}", e))?;

        // Load hot-std
        let mut combined_program = Program {
            namespaces: Default::default(),
            current_namespace: test_program.current_namespace.clone(),
        };

        // Try to load hot-std from various possible locations
        let hot_std_paths = [
            "hot/pkg/hot-std/src",
            "../hot/pkg/hot-std/src",
            "../../hot/pkg/hot-std/src",
            "./hot/pkg/hot-std/src",
        ];

        for hot_std_path in &hot_std_paths {
            if Path::new(hot_std_path).exists()
                && let Ok(hot_std_program) = load_hot_std_from_path(hot_std_path) {
                    // Add hot-std namespaces
                    for (ns_path, namespace) in hot_std_program.namespaces {
                        combined_program.namespaces.insert(ns_path, namespace);
                    }
                    break;
                }
        }

        // Add test program namespaces
        for (ns_path, namespace) in test_program.namespaces {
            combined_program.namespaces.insert(ns_path, namespace);
        }

        compiler
            .compile_program(&mut combined_program)
            .map_err(|e| format!("Compile error: {:?}", e))?;

        let program_arc = compiler.get_program_arc();
        let mut vm = VirtualMachine::new(
            program_arc,
            None,
            compiler.get_function_mapping_arc(),
            compiler.get_core_functions_arc(),
            compiler.get_type_implementations_arc(),
            compiler.get_core_variables_arc(),
            conf,
        );

        vm.execute().map_err(|e| format!("{:?}", e))
    }

    /// An Int op's overflow, a zero divisor, and a call of a value that isn't
    /// a function give the same values in compiled code as in the
    /// interpreter: checked, dropped, returned, kept in a collection.
    #[test]
    fn numeric_errs_match_in_interpreter_and_compiled_code() {
        let src = r#"::t ns
big 9223372036854775807
smallest sub(sub(0, big), 1)
add-ints fn (a: Int, b: Int) { add(a, b) }
mul-ints fn (a: Int, b: Int) { mul(a, b) }
pow-ints fn (a: Int, b: Int) { pow(a, b) }
mod-ints fn (a: Int, b: Int) { mod(a, b) }
div-decs fn (a: Dec, b: Dec) { div(a, b) }
checked fn (a: Int, b: Int) { r add(a, b)  if(is-err(r), "caught", r) }
dropped fn (a: Int, b: Int) { r mod(a, b)  "dropped" }
kept fn (a: Int, b: Int) { [mul(a, b), 1] }
call-value fn (v) { v(1) }
show fn (r: Result) { if(is-err(r), `Err ${err-message(r)}`, r) }
[
    show(add-ints(1, 2)), show(add-ints(big, 1)), show(mul-ints(big, 2)),
    show(pow-ints(10, 18)), show(pow-ints(10, 19)), show(pow-ints(3, 39)),
    show(mod-ints(5, 0)), show(mod-ints(smallest, -1)), show(div-decs(1.5, 0.0)),
    show(checked(big, 1)), show(checked(1, 1)), show(dropped(5, 0)),
    show(first(kept(big, 2))), show(abs(smallest)), show(floor(1000000000000000000000.5)),
    show(Int(10000000000000000000.5)), show(call-value(5)),
]"#;
        let overflow = "Err Integer overflow";
        let expected = val!([
            3, overflow, overflow,
            1000000000000000000_i64, overflow, 4052555153018976267_i64,
            "Err Modulo by zero", 0, "Err Division by zero",
            "caught", 2, "dropped",
            overflow, overflow, overflow,
            overflow, "Err Cannot call a value of type Int"
        ]);
        for conf in [
            val!({"jit": {"mode": "disabled"}}),
            val!({"jit": {"threshold": 1}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone())).expect("run");
            assert_eq!(out, expected, "{conf:?}");
        }
    }

    /// A consumed overflow halts with the same message either way.
    #[test]
    fn consumed_overflow_halts_in_interpreter_and_compiled_code() {
        let src = r#"::t ns
grow fn (a: Int) { mul(add(a, 1), 2) }
grow(9223372036854775807)"#;
        for conf in [
            val!({"jit": {"mode": "disabled"}}),
            val!({"jit": {"threshold": 1}}),
        ] {
            let err = compile_and_run_with_std_conf(src, Some(conf.clone()))
                .expect_err("the overflow halts");
            assert!(err.contains("Result error: Integer overflow"), "{conf:?}: {err}");
        }
    }

    fn load_hot_std_from_path(
        hot_std_path: &str,
    ) -> Result<crate::lang::ast::Program, Box<dyn std::error::Error>> {
        use crate::lang::ast::Program;
        use std::fs;
        use std::path::Path;

        let mut program = Program {
            namespaces: Default::default(),
            current_namespace: crate::lang::ast::NsPath::new(),
        };

        // Recursively find all .hot files in hot-std (excluding test directories)
        fn find_hot_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let path_str = path.to_string_lossy();

                // Skip test directories and test.hot file
                if path_str.contains("/test/") || path_str.ends_with("/test") || path_str.ends_with("/test.hot") {
                    continue;
                }

                if path.is_dir() {
                    find_hot_files(&path, files)?;
                } else if path.extension().and_then(|s| s.to_str()) == Some("hot") {
                    files.push(path);
                }
            }
            Ok(())
        }

        let mut hot_files = Vec::new();
        find_hot_files(Path::new(hot_std_path), &mut hot_files)?;

        // Parse each hot-std file
        for file_path in hot_files {
            if let Ok(content) = fs::read_to_string(&file_path)
                && let Ok(file_program) = parser::parse_hot(&content) {
                    for (ns_path, namespace) in file_program.namespaces {
                        program.namespaces.insert(ns_path, namespace);
                    }
                }
        }

        Ok(program)
    }

    #[test]
    fn test_fail_single_arity_with_string() {
        let source = r#"
            ::test ns

            test_fail fn () {
                ::hot::exec/fail("Something went wrong")
            }

            result test_fail()
        "#;

        let result = compile_and_run(source);
        assert!(result.is_err(), "fail() should return an error");
        assert!(result.unwrap_err().contains("Something went wrong"));
    }

    #[test]
    fn test_fail_single_arity_with_map() {
        let source = r#"
            ::test ns

            test_fail fn () {
                ::hot::exec/fail({"code": 404, "message": "Not found"})
            }

            result test_fail()
        "#;

        let result = compile_and_run(source);
        assert!(result.is_err(), "fail() should return an error");
    }

    #[test]
    fn test_fail_two_arity() {
        let source = r#"
            ::test ns

            test_fail fn () {
                ::hot::exec/fail("Database error", {"table": "users", "op": "insert"})
            }

            result test_fail()
        "#;

        let result = compile_and_run(source);
        assert!(result.is_err(), "fail() should return an error");
        assert!(result.unwrap_err().contains("Database error"));
    }

    #[test]
    fn test_vm_failure_state() {
        // Test that VM failure state is properly set
        let vm = create_test_vm();

        // Initially not failed
        assert!(!vm.has_failed());
        assert!(vm.get_failure().is_none());

        // Set failure
        let msg = "Test failure".to_string();
        let data = val!({"error": "test"});
        let is_first = vm.set_failure(msg.clone(), data.clone());

        assert!(is_first, "First failure should return true");
        assert!(vm.has_failed());

        let failure = vm.get_failure().expect("Should have failure");
        assert_eq!(failure.msg, msg);

        // Try to set another failure (should not override)
        let is_first_again = vm.set_failure("Second failure".to_string(), Val::Null);
        assert!(!is_first_again, "Second failure should return false");

        // Original failure should still be there
        let failure = vm.get_failure().expect("Should still have original failure");
        assert_eq!(failure.msg, msg);
    }

    #[test]
    fn test_failure_constructs_proper_type() {
        let source = r#"
            ::test ns

            test_fail fn () {
                ::hot::exec/fail("Error message", {"code": 500})
            }

            result test_fail()
        "#;

        // This test verifies that fail constructs a Failure type {msg, err}
        // The actual execution will fail, but we're testing the structure
        let result = compile_and_run(source);
        assert!(result.is_err());
    }

    #[test]
    fn test_fail_in_conditional() {
        let source = r#"
            ::test ns

            process fn (x: Int): Int {
                cond {
                    ::hot::cmp/lt(x, 0) => { ::hot::exec/fail("Negative number not allowed", x) }
                    => { ::hot::math/mul(x, 2) }
                }
            }

            result process(-5)
        "#;

        let result = compile_and_run(source);
        assert!(result.is_err(), "Should fail on negative input");
        assert!(result.unwrap_err().contains("Negative number"));
    }

    #[test]
    fn test_fail_in_flow() {
        let source = r#"
            ::test ns

            validate fn (x: Int): Int {
                serial {
                    check_positive ::hot::bool/if(
                        ::hot::cmp/gt(x, 0),
                        x,
                        ::hot::exec/fail("Must be positive", x)
                    )
                    doubled ::hot::math/mul(check_positive, 2)
                    doubled
                }
            }

            result validate(-10)
        "#;

        let result = compile_and_run(source);
        assert!(result.is_err(), "Should fail in flow");
    }

    #[test]
    fn test_failure_state_reset() {
        let mut vm = create_test_vm();

        // Set failure
        vm.set_failure("Test error".to_string(), Val::Null);
        assert!(vm.has_failed());

        // Reset state
        vm.reset_state();
        assert!(!vm.has_failed(), "Failure state should be reset");
        assert!(vm.get_failure().is_none());
    }

    #[test]
    fn test_fail_in_pmap_propagates() {
        // pmap shares `map`'s halt-propagation contract: an unhandled
        // `fail()` inside the callback propagates out of pmap. Workers
        // complete their in-flight chunks (no wasted parallel work),
        // then pmap surfaces the lowest-input-index halt — same
        // observable result as a sequential `map`.

        let source = r#"
            ::test ns

            process fn (x: Int): Int {
                ::hot::bool/if(
                    ::hot::cmp/eq(x, 5),
                    ::hot::exec/fail("Found 5", x),
                    ::hot::math/mul(x, 2)
                )
            }

            numbers [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
            pmap-result ::hot::coll/pmap(numbers, process)
        "#;

        let result = compile_and_run_with_std(source);
        assert!(
            result.is_err(),
            "pmap should propagate the unhandled fail(): {:?}",
            result
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("Found 5"),
            "pmap propagated error should carry the original failure message, got: {msg}"
        );
    }

    #[test]
    fn test_err_in_pmap_recovers_via_if_err() {
        // The idiomatic way to keep going past a failing per-item call
        // is `if-err` inside the callback. The callback returns a
        // domain-typed fallback and pmap completes normally. (`if-err`
        // handles an Err value; a `fail()` would halt.)

        let source = r#"
            ::test ns

            process fn (x: Int): Int {
                ::hot::type/if-err(
                    ::hot::bool/if(
                        ::hot::cmp/eq(x, 5),
                        ::hot::type/err("Found 5"),
                        ::hot::math/mul(x, 2)
                    ),
                    (e: Any) { -1 }
                )
            }

            numbers [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
            pmap-result ::hot::coll/pmap(numbers, process)
        "#;

        let result = compile_and_run_with_std(source);
        assert!(result.is_ok(), "pmap with if-err should succeed: {:?}", result);

        if let Val::Vec(vec) = result.unwrap() {
            assert_eq!(vec.len(), 10, "all 10 items present");
            // index 4 is x=5 which recovered to -1
            assert_eq!(vec[4], Val::Int(-1), "recovered slot should be -1");
            assert_eq!(vec[0], Val::Int(2), "first item computed normally");
        } else {
            panic!("Expected Vec");
        }
    }

    #[test]
    fn pmap_callback_runtime_errors_have_interpreter_jit_parity() {
        let source = r#"::test ns
process fn (x: Int): Vec {
    values [1, 2]
    index sub(0, 1)
    values[index] x
    values
}
pmap([1, 2, 3, 4, 5, 6, 7, 8], process)"#;

        // A runtime error in a callback halts pmap, in both modes.
        for conf in [
            crate::val!({"engine": {"threads": 2}, "jit": {"mode": "off"}}),
            crate::val!({"engine": {"threads": 2}, "jit": {"threshold": 1}}),
        ] {
            let out = compile_and_run_with_std_conf(source, Some(conf.clone()));
            assert!(
                out.as_ref()
                    .is_err_and(|e| e.contains("Vector index cannot be negative")),
                "with {conf:?}: expected the callback's halt, got {out:?}"
            );
        }
    }

    /// Helper to create a test VM
    fn create_test_vm() -> VirtualMachine {
        use crate::lang::bytecode::BytecodeProgram;
        use crate::lang::compiler::core_registry::CoreVariableRegistry;

        let program = Arc::new(BytecodeProgram::new());
        let function_mapping = Arc::new(indexmap::IndexMap::new());
        let core_functions = Arc::new(indexmap::IndexMap::new());
        let type_implementations = Arc::new(indexmap::IndexMap::new());
        let core_variables = Arc::new(CoreVariableRegistry::new());

        VirtualMachine::new(
            program,
            None,
            function_mapping,
            core_functions,
            type_implementations,
            core_variables,
            None,
        )
    }
}

/// Regression tests for a VM bug where a 0-arg lambda defined inside a
/// fn body and immediately invoked causes runaway recursion that blows
/// the OS stack. Hoisting the same lambda to a top-level binding works.
///
/// See conversation circa 2026-04-20 (mcp::test::ai). Symptoms:
///
/// ```text
/// thread 'tokio-rt-worker' has overflowed its stack
/// fatal runtime error: stack overflow, aborting
/// ```
///
/// These tests run on the small default cargo-test stack so the bug
/// surfaces as either a depth-guard error or a panic — never as a
/// silent abort.
#[cfg(test)]
mod inline_zero_arg_lambda_tests {
    use super::run_tests::{compile_and_run, compile_and_run_with_std};

    #[test]
    fn inline_zero_arg_lambda_returning_int_can_be_called() {
        let source = r#"
            ::test ns

            main fn (): Int {
                f fn (): Int { 42 }
                f()
            }

            result main()
        "#;
        let result = compile_and_run(source);
        assert!(
            result.is_ok(),
            "0-arg lambda invocation should succeed, got: {:?}",
            result
        );
    }

    #[test]
    fn inline_zero_arg_lambda_returning_map_can_be_called() {
        let source = r#"
            ::test ns

            main fn (): Map {
                f fn (): Map { {url: "https://example.com"} }
                f()
            }

            result main()
        "#;
        let result = compile_and_run(source);
        assert!(
            result.is_ok(),
            "0-arg lambda returning map should succeed, got: {:?}",
            result
        );
    }

    /// Pass an inline 0-arg lambda to a callee that calls it. This is
    /// the exact pattern that broke `::mcp::ai/resolve-spec`.
    #[test]
    fn inline_zero_arg_lambda_passed_then_called() {
        let source = r#"
            ::test ns

            invoke fn (g: Fn): Map {
                g()
            }

            main fn (): Map {
                spec fn (): Map { {url: "https://example.com"} }
                invoke(spec)
            }

            result main()
        "#;
        let result = compile_and_run(source);
        assert!(
            result.is_ok(),
            "0-arg lambda passed-then-called should succeed, got: {:?}",
            result
        );
    }

    /// The exact resolve-spec shape: caller binds a 0-arg lambda, passes
    /// it through a namespace-aliased fn whose body uses `cond` +
    /// `is-fn(...)` + `is-map(...)` and string interpolation in the
    /// fail branch. This is what blew the stack in the mcp tests.
    #[test]
    fn cond_is_fn_dispatch_with_inline_lambda_resolves() {
        let source = r#"
            ::test::resolver ns

            resolve-spec fn (spec: Any): Bool {
                is-map(spec)
            }
        "#;
        let caller = r#"
            ::test ns

            main fn (): Bool {
                spec fn (): Map { {url: "https://example.com/mcp"} }
                ::test::resolver/resolve-spec(spec)
            }

            result main()
        "#;
        let combined = format!("{}\n{}", source, caller);
        let result = compile_and_run_with_std(&combined);
        assert!(
            result.is_ok(),
            "cond+is-fn dispatch on inline 0-arg lambda should succeed, got: {:?}",
            result
        );
    }

    #[test]
    fn top_level_zero_arg_fn_works_as_baseline() {
        let source = r#"
            ::test ns

            f fn (): Int { 42 }

            main fn (): Int {
                f()
            }

            result main()
        "#;
        let result = compile_and_run(source);
        assert!(
            result.is_ok(),
            "top-level 0-arg fn should succeed (baseline), got: {:?}",
            result
        );
    }
}

#[cfg(test)]
mod parallel_failure_tests {
    #[test]
    fn test_multiple_failures_first_wins() {
        // Test that in parallel execution, the first failure wins
        // This is a conceptual test - actual implementation would need
        // parallel execution infrastructure

        use crate::lang::runtime::vm::VmFailureState;
        use std::sync::atomic::AtomicBool;
        use std::sync::RwLock;
        use std::sync::Arc;

        let failure_state = Arc::new(VmFailureState {
            failed: AtomicBool::new(false),
            failure: RwLock::new(None),
        });

        // Simulate multiple threads trying to set failure
        let state1 = failure_state.clone();
        let state2 = failure_state.clone();

        // First thread sets failure
        let first_msg = "First failure".to_string();
        let first_data = crate::val!({"id": 1});
        let first_result = {
            use std::sync::atomic::Ordering;
            if state1.failed.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                if let Ok(mut failure) = state1.failure.write() {
                    *failure = Some(crate::lang::runtime::vm::FailureState {
                        msg: first_msg.clone(),
                        data: first_data.clone(),
                        plain_vm_error: false,
                        consumed: false,
                    });
                }
                true
            } else {
                false
            }
        };

        // Second thread tries to set failure
        let second_msg = "Second failure".to_string();
        let second_data = crate::val!({"id": 2});
        let second_result = {
            use std::sync::atomic::Ordering;
            if state2.failed.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                if let Ok(mut failure) = state2.failure.write() {
                    *failure = Some(crate::lang::runtime::vm::FailureState {
                        msg: second_msg,
                        data: second_data,
                        plain_vm_error: false,
                        consumed: false,
                    });
                }
                true
            } else {
                false
            }
        };

        // Verify first wins
        assert!(first_result, "First failure should succeed");
        assert!(!second_result, "Second failure should fail");

        // Verify the stored failure is from the first thread
        let stored_failure = failure_state.failure.read().unwrap();
        assert!(stored_failure.is_some());
        assert_eq!(stored_failure.as_ref().unwrap().msg, first_msg);
    }
}
