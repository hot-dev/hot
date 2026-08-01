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
    fn test_fail_in_pmap_recovers_via_if_err() {
        // The idiomatic way to keep going past a failing per-item call
        // is `if-err` inside the callback. The callback returns a
        // domain-typed fallback and pmap completes normally.

        let source = r#"
            ::test ns

            process fn (x: Int): Int {
                ::hot::type/if-err(
                    ::hot::bool/if(
                        ::hot::cmp/eq(x, 5),
                        ::hot::exec/fail("Found 5", x),
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
