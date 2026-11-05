// Tests for run/exec control functionality (fail and cancel functions)

#[cfg(test)]
mod run_tests {
    use crate::lang::bytecode::Instruction;
    use crate::lang::compiler::Compiler;
    use crate::lang::parser;
    use crate::lang::runtime::jit_hof::{PipelinePlan, detect_compilable_pipelines};
    use crate::lang::runtime::vm::VirtualMachine;
    use crate::val;
    use crate::val::Val;
    use std::sync::Arc;

    #[test]
    fn collection_members_of_mixed_unions_are_checked_and_kept() {
        let source = r#"::t ns
Other type { name: Str }
Numbers type { values: Vec<Int> | Other }
Lookup type { values: Map<Str, Int> | Other }
EitherList type { values: Vec<Int> | Vec<Str> | Other }
probe fn (data: Any): Vec {
    [is-err(Numbers({values: data})), is-err(Lookup({values: data})),
     is-err(EitherList({values: data}))]
}
all fn (): Vec {
    [probe([1, "bad"]), probe([1, 2]), probe(["a", "b"]),
     probe({a: 1}), probe({name: "valid record"}),
     probe({a: "bad"}), probe(null),
     Numbers({values: [1.0, 2]}).values]
}
all()
all()"#;
        assert_same_with_and_without_jit(source, &val!([
            [true, true, true], [false, true, false], [true, true, false],
            [true, false, true], [false, false, false],
            [true, true, true], [true, true, true], [1, 2]
        ]));
    }

    #[test]
    fn generic_map_members_wait_for_record_members() {
        let source = r#"::t ns
Other type { name: Str }
Pet type { name: Str }
Strict type { values: Map<Str, Str> | Other }
Reversed type { values: Other | Map<Str, Str> }
Loose type { values: Map<Str, Any> | Other }
Nullable type { values: Map<Str, Str>? | Other }
Plain type { values: Map | Other }
Both type { values: Map<Str, Str> | Other | Pet }
all fn (): Vec {
    record {name: "x"}
    colors {color: "red"}
    [is-type(Strict({values: record}).values, Other),
     is-type(Reversed({values: record}).values, Other),
     is-type(Loose({values: record}).values, Other),
     is-type(Nullable({values: record}).values, Other),
     eq(Strict({values: colors}).values, colors),
     eq(Reversed({values: colors}).values, colors),
     eq(Loose({values: {a: 1}}).values, {a: 1}),
     is-err(Strict({values: {a: 1}})),
     is-err(Both({values: record})),
     eq(Plain({values: record}).values, record)]
}
all()
all()"#;
        assert_same_with_and_without_jit(
            source,
            &val!([true, true, true, true, true, true, true, true, true, true]),
        );
    }

    #[test]
    fn failed_map_keys_leave_the_check_path_as_they_found_it() {
        let cases = [
            // The first entry passes its second member after the first
            // member refused its key.
            (
                r#"::t ns
K type { m: Vec<Map<Str, Int> | Map<Int, Str>> }
K({m: [assoc({}, 7, "ok"), {a: "bad"}]})"#,
                val!(["m", 1, "a"]),
                "at `m[1].a` expected Int, got Str",
            ),
            (
                r#"::t ns
K type { m: Map<Str, Int> | Map<Int, Str> }
K({m: assoc({}, 7, 8)})"#,
                val!(["m", 7]),
                "at `m[7]` expected Str, got Int",
            ),
        ];
        for (source, path, message) in cases {
            for conf in [None, Some(crate::val!({"jit": {"threshold": 1}}))] {
                let out = compile_and_run_with_std_conf(source, conf).unwrap();
                assert!(out.is_err(), "{out:?}");
                let violation = out.get("$val").unwrap().get("$val").unwrap();
                assert_eq!(violation.get("path").unwrap(), path);
                let text = violation.get("message").unwrap().to_string();
                assert!(text.contains(message), "{text}");
            }
        }
    }

    #[test]
    fn number_keyed_maps_take_the_keys_json_gives_them() {
        let source = r#"::t ns
Counts type { counts: Map<Int, Int> }
Prices type { prices: Map<Dec, Str> }
Named type { counts: Map<Str, Int> }
Positive type Int where gt(%, 0)
PositiveKeys type { counts: Map<Positive, Int> }
all fn (): Vec {
    [keys(Counts({counts: from-json("{\"1\": 2}")}).counts),
     eq(keys(Prices({prices: {"1.5": "a"}}).prices), [1.5]),
     keys(Named({counts: {"1": 2}}).counts),
     keys(PositiveKeys({counts: {"1": 2}}).counts),
     is-err(PositiveKeys({counts: {"0": 2}})),
     is-err(Counts({counts: {"1.5": 2}})),
     is-err(Counts({counts: {"01": 2}})),
     is-err(Counts({counts: {"+1": 2}})),
     is-err(Counts({counts: {one: 2}})),
     is-err(Counts({counts: {"1": 2, "1.0": 3}}))]
}
all()
all()"#;
        assert_same_with_and_without_jit(
            source,
            &val!([[1], true, ["1"], [1], true, true, true, true, true, true]),
        );
    }

    #[test]
    fn maps_of_valid_keys_stay_on_the_fast_path_and_others_are_refused() {
        let source = r#"::t ns
Foo type { a: Int }
Any-values type { m: Map<Str, Any> }
Str-values type { m: Map<Str, Str> }
Int-keys type { m: Map<Int, Any> }
Dec-keys type { m: Map<Dec, Str> }
Custom type { m: Map<Any, Foo> }
Whatever type { m: Map<Any, Any> }
all fn (): Vec {
    numbered assoc({}, 7, "x")
    [eq(Any-values({m: {a: 1, b: "x"}}).m, {a: 1, b: "x"}),
     is-err(Any-values({m: numbered})),
     eq(Str-values({m: {a: "x"}}).m, {a: "x"}),
     is-err(Str-values({m: numbered})),
     is-err(Str-values({m: {a: 1}})),
     eq(keys(Int-keys({m: numbered}).m), [7]),
     eq(keys(Int-keys({m: {"7": "x"}}).m), [7]),
     eq(keys(Dec-keys({m: numbered}).m), [7.0]),
     eq(Custom({m: {x: {a: 1}}}).m, {x: Foo({a: 1})}),
     eq(Whatever({m: numbered}).m, numbered),
     is-err(Whatever({m: "x"}))]
}
all()
all()"#;
        assert_same_with_and_without_jit(
            source,
            &val!([
                true, true, true, true, true, true, true, true, true, true, true
            ]),
        );
    }

    #[test]
    fn converted_map_keys_collide_in_either_order() {
        let source = r#"::t ns
Counts type { m: Map<Int, Str> }
all fn (): Vec {
    [is-err(Counts({m: assoc({"1": "a"}, 1, "b")})),
     is-err(Counts({m: assoc(assoc({}, 1, "a"), "1", "b")})),
     get(Counts({m: assoc({"1": "a"}, 2, "b")}).m, 2)]
}
all()
all()"#;
        assert_same_with_and_without_jit(source, &val!([true, true, "b"]));
    }

    #[test]
    fn optional_collections_are_checked_once_and_still_converted() {
        let source = r#"::t ns
Other type { name: Str }
Field type { xs: Vec<Dec>? }
Member type { xs: Vec<Dec>? | Other }
Shape enum { Many(Vec<Dec>?) }
all fn (): Vec {
    [eq(Field({xs: [1]}).xs, [1.0]),
     eq(Field({xs: null}).xs, null),
     is-err(Field({xs: ["x"]})),
     eq(Member({xs: [1]}).xs, [1.0]),
     is-type(Member({xs: {name: "x"}}).xs, Other),
     is-err(Member({xs: ["x"]})),
     is-err(Shape.Many(["x"]))]
}
all()
all()"#;
        assert_same_with_and_without_jit(
            source,
            &val!([true, true, true, true, true, true, true]),
        );
    }

    #[test]
    fn map_fields_validate_keys_without_losing_converted_entries() {
        let source = r#"::t ns
Counts type { counts: Map<Str, Int> }
Numeric type { counts: Map<Int, Int> }
Loose type { counts: Map<Any, Int> }
Optional type { counts: Map<Str, Int>? }
Positive type Int where gt(%, 0)
PositiveKeys type { counts: Map<Positive, Int> }
probe fn (data: Any): Vec {
    [is-err(Counts({counts: data})), is-err(Numeric({counts: data})),
     is-err(Loose({counts: data})), is-err(Optional({counts: data}))]
}
all fn (): Vec {
    [probe({a: 2}), probe(assoc({}, 1, 2)), probe(assoc({}, 1.0, 2)),
     probe(assoc(assoc({}, 1, 2), 1.0, 3)), probe(null),
     keys(Numeric({counts: assoc({}, 1.0, 2)}).counts),
     is-err(PositiveKeys({counts: assoc({}, 0, 2)})),
     is-ok(PositiveKeys({counts: assoc({}, 1, 2)}))]
}
all()
all()"#;
        assert_same_with_and_without_jit(source, &val!([
            [false, true, false, false], [true, false, false, true],
            [true, false, false, true], [true, true, false, true],
                [true, true, true, false], [1], true, true
        ]));
    }

    #[test]
    fn inline_refinements_resolve_named_numeric_bases() {
        let source = r#"::numbers ns
Money type Dec
::t ns
::n ::numbers
Amount ::numbers/Money
positive fn (x: Amount where gt(%, 0)): Bool { true }
qualified fn (x: ::n/Money where gt(%, 0)): Bool { true }
probe fn (): Vec {
    [positive(::n/Money(5.0)), qualified(::n/Money(5.0)),
     is-err(positive(::n/Money(-5.0))), is-err(qualified(::n/Money(-5.0)))]
}
// Top-level calls must check too: body inference alone suppresses some errors.
positive(::n/Money(5.0))
qualified(::n/Money(5.0))
probe()
probe()"#;
        assert_same_with_and_without_jit(source, &val!([true, true, true, true]));
    }

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
            // --- Results that change kind between Int and Dec, per element ---
            r#"::t ns
f fn (n: Int): Dec {
    range(1, add(n, 1))
    |> map((x) { div(x, 2) })
    |> map((d) { mul(d, 3) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
f(50)"#,
            // --- A Str accumulator replaced for every element, newest first ---
            r#"::t ns
f fn (n: Int): Str {
    range(1, add(n, 1))
    |> map((i) { `item-${i}` })
    |> map((s) { concat(s, "!") })
    |> reduce((acc, s) { concat(s, acc) }, "")
}
f(10)"#,
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
            let compiled =
                compile_and_run_with_std_conf(src, Some(crate::val!({"jit": {"threshold": 1}})))
                    .expect("compiled run");
            assert_eq!(on, compiled, "compiled code differs for:\n{}", src);
            // sanity: never null/error
            assert!(
                !matches!(on, Val::Null) && !on.is_err(),
                "unexpected result {:?} for:\n{}",
                on,
                src
            );
        }
    }

    /// Reading a few characters from the start of a string costs the same at
    /// any length. A read that copied the string would take far longer on the
    /// large one, so the margin leaves only that able to fail this.
    #[test]
    fn reading_the_start_of_a_string_costs_the_same_at_any_length() {
        let src = r#"::t ns
fastest-ms fn (f: Fn): Int {
    tries map(range(3), (_try) {
        start ::hot::time/epoch-millis(::hot::time/now())
        done map(range(500), (i) { f(i) })
        sub(::hot::time/epoch-millis(::hot::time/now()), start)
    })
    reduce(tries, min(%1, %2), first(tries))
}
timings fn (s: Str): Map {
    {
        slice-start: fastest-ms((i) { slice(s, 0, 5) }),
        get: fastest-ms((i) { get(s, 3) }),
        nth: fastest-ms((i) { ::hot::coll/nth(s, 3) }),
        index: fastest-ms((i) { s[3] }),
    }
}
sixteen-mb reduce(range(20), (acc, _i) { concat(acc, acc) }, "0123456789abcdef")
{small: timings("0123456789"), large: timings(sixteen-mb)}"#;
        for conf in [
            Some(val!({"jit": {"mode": "off"}})),
            Some(val!({"jit": {"threshold": 1}})),
        ] {
            let label = format!("{conf:?}");
            let out = compile_and_run_with_std_conf(src, conf).expect("run");
            let (Some(Val::Map(small)), Some(Val::Map(large))) = (out.get("small"), out.get("large"))
            else {
                panic!("{label}: unexpected result {out:?}");
            };
            for (read, small_ms) in small.iter() {
                let (Val::Int(small_ms), Some(Val::Int(large_ms))) = (small_ms, large.get(read)) else {
                    panic!("{label}: {read} has no timing");
                };
                assert!(
                    *large_ms <= small_ms * 20 + 100,
                    "{label}: {read} took {small_ms}ms on 10 characters, {large_ms}ms on 16 MB"
                );
            }
        }
    }

    /// Reading a Map or Vec shares its storage rather than copying it, which
    /// is what makes a read cost the same at any size. Each entry of the result
    /// holds the same collection, reached by a different read: a copy on any of
    /// those paths gives that entry an allocation of its own. The JIT reads
    /// values through different code, so both modes run.
    #[test]
    fn reads_share_collection_storage() {
        let src = r#"::t ns
same fn (x) { x }
annotated fn (x: Map): Map { x }
annotated-bytes fn (x: Bytes): Bytes { x }
bound fn (x) {
    y x
    z y
    z
}
held fn (x) {
    box {x: x}
    box.x
}
reads fn (m: Map, v: Vec, b: Bytes) {
    reader () { m }
    bytes-reader () { b }
    called map([1, 2], (i) { same(m) })
    mapped map([1], (i) { v })
    bytes-mapped map([1], (i) { same(b) })
    listed [m, v, b]
    {
        m: [same(m), annotated(m), bound(m), reader(), called[0], called[1], listed[0], held(m)],
        v: [same(v), bound(v), mapped[0], listed[1], held(v)],
        child: [m.child, get(m, "child"), same(m).child],
        item: [v[0], first(v), get(v, 0), same(v)[0]],
        b: [same(b), annotated-bytes(b), bound(b), bytes-reader(), bytes-mapped[0], listed[2], held(b)],
        data: [m.data, get(m, "data"), same(m).data],
    }
}
run fn () {
    b Bytes([1, 2, 3])
    m {child: {a: 1}, other: 2, data: Bytes([4, 5])}
    v [{b: 1}, {b: 2}]
    {m, v, b, reads: reads(m, v, b)}
}
run()
run()
run()"#;
        for conf in [
            Some(val!({"jit": {"mode": "off"}})),
            Some(val!({"jit": {"threshold": 1}})),
        ] {
            let label = format!("{conf:?}");
            let out = compile_and_run_with_std_conf(src, conf).expect("run");
            let field = |value: &Val, name: &str| value.get(name).expect(name);
            let reads = field(&out, "reads");
            let m = field(&out, "m");
            let v = field(&out, "v");
            let b = field(&out, "b");
            let child = field(&m, "child");
            let data = field(&m, "data");
            let Val::Vec(items) = &v else {
                panic!("v is {v:?}");
            };
            for (expected, name) in [
                (&m, "m"),
                (&v, "v"),
                (&child, "child"),
                (&items[0], "item"),
                (&b, "b"),
                (&data, "data"),
            ] {
                let Val::Vec(held) = field(&reads, name) else {
                    panic!("{name} is not a Vec");
                };
                for (index, value) in held.iter().enumerate() {
                    let shared = match (expected, value) {
                        (Val::Map(a), Val::Map(b)) => Arc::ptr_eq(a, b),
                        (Val::Vec(a), Val::Vec(b)) => Arc::ptr_eq(a, b),
                        (Val::Bytes(a), Val::Bytes(b)) => Arc::ptr_eq(a, b),
                        _ => false,
                    };
                    assert!(shared, "{label}: read {index} of {name} was copied: {value:?}");
                }
            }
        }
    }

    /// The other half of sharing: writing one holder of a collection leaves
    /// every other holder as it was, in both JIT modes. `shared-values.hot`
    /// covers the language's writes one by one.
    #[test]
    fn writes_leave_the_other_holders_of_a_collection() {
        let src = r#"::t ns
write fn (m: Map): Map {
    m.child.a 2
    m
}
m {child: {a: 1}}
held [m]
written write(m)
grown assoc(m, "b", 1)
[m.child.a, held[0].child.a, written.child.a, length(m), length(grown)]"#;
        assert_same_with_and_without_jit(src, &val!([1, 1, 2, 1, 2]));
    }

    /// A read costs the same whatever the size of the collection. One that
    /// scaled would take far longer on the large collection, so the margin
    /// leaves only that able to fail this. `bench-large-reads.hot` prints the
    /// numbers.
    #[test]
    fn reads_cost_the_same_at_any_size() {
        let src = r#"::t ns
ignore-it fn (coll, i) { i }
fastest-ms fn (f: Fn): Int {
    tries map(range(3), (_try) {
        start ::hot::time/epoch-millis(::hot::time/now())
        done map(range(500), (i) { f(i) })
        sub(::hot::time/epoch-millis(::hot::time/now()), start)
    })
    reduce(tries, min(%1, %2), first(tries))
}
rows fn (n: Int): Vec {
    map(range(n), (i) { {k: `k-${i}`, v: "some text in the value"} })
}
timings fn (n: Int, b: Bytes): Map {
    v rows(n)
    m index-by(v, %.k)
    {
        map-pass: fastest-ms((i) { ignore-it(m, i) }),
        map-get: fastest-ms((i) { get(m, "k-0") }),
        map-dot: fastest-ms((i) { m.k-0 }),
        map-length: fastest-ms((i) { length(m) }),
        vec-pass: fastest-ms((i) { ignore-it(v, i) }),
        vec-index: fastest-ms((i) { v[0] }),
        vec-first: fastest-ms((i) { first(v) }),
        bytes-pass: fastest-ms((i) { ignore-it(b, i) }),
        bytes-length: fastest-ms((i) { length(b) }),
        bytes-get: fastest-ms((i) { get(b, 0) }),
    }
}
sixty-four-mb reduce(range(6), (acc, _i) { concat(acc, acc) }, random-bytes(1000000))
{small: timings(10, random-bytes(10)), large: timings(200000, sixty-four-mb)}"#;
        for conf in [
            Some(val!({"jit": {"mode": "off"}})),
            Some(val!({"jit": {"threshold": 1}})),
        ] {
            let label = format!("{conf:?}");
            let out = compile_and_run_with_std_conf(src, conf).expect("run");
            let (Some(Val::Map(small)), Some(Val::Map(large))) = (out.get("small"), out.get("large"))
            else {
                panic!("{label}: unexpected result {out:?}");
            };
            for (read, small_ms) in small.iter() {
                let (Val::Int(small_ms), Some(Val::Int(large_ms))) = (small_ms, large.get(read)) else {
                    panic!("{label}: {read} has no timing");
                };
                assert!(
                    *large_ms <= small_ms * 20 + 100,
                    "{label}: {read} took {small_ms}ms small, {large_ms}ms large"
                );
            }
        }
    }

    /// Meta keeps a call written in it unevaluated, and the call runs when the
    /// value read from meta is passed to a function. That needs a walk over the
    /// argument, which every other program is spared.
    #[test]
    fn a_call_in_meta_runs_when_its_value_is_passed() {
        let src = r#"::t ns
bump fn (n: Int): Int { add(n, 1) }
same fn (x) { x }
f meta {count: bump(1), list: [bump(2)], plain: 1} fn () { 1 }
g fn () {
    m ::hot::meta/get(f)
    out same(m)
    [out.count, out.list, out.plain, same(m.list)]
}
g()"#;
        let expected = val!([2, [3], 1, [3]]);
        let out = compile_and_run_with_std_ast(src).expect("run");
        assert_eq!(out, expected);
    }

    /// What `src` gives with `conf`, and how many fused runs compiled code made.
    fn run_counting_compiled_fused_runs(src: &str, conf: Option<Val>) -> (Val, u64) {
        let before = crate::lang::runtime::jit::fused_runs_for_test();
        let out = compile_and_run_with_std_conf(src, conf).expect("run");
        (out, crate::lang::runtime::jit::fused_runs_for_test() - before)
    }

    /// A function that crosses the JIT threshold keeps its pipeline fused.
    /// Compiled code that called each stage on its own ran this about 20x
    /// slower. A benchmark that stays under the threshold can't see that, so
    /// this runs past it, at the threshold's default.
    #[test]
    fn a_pipeline_stays_fused_once_its_function_is_compiled() {
        let src = r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> map((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
run fn (): Int {
    reduce(range(150), (_acc, _i) { f(20) }, 0)
}
run()"#;
        let (out, runs) = run_counting_compiled_fused_runs(src, None);
        assert_eq!(out, val!(1540));
        assert!(runs >= 40, "compiled code made {runs} fused runs");

        // The kill switch reaches compiled code too.
        let (out, runs) =
            run_counting_compiled_fused_runs(src, Some(val!({"jit": {"hof": {"fusion": false}}})));
        assert_eq!(out, val!(1540));
        assert_eq!(runs, 0, "fusion is off");
    }

    /// Fusion runs its stages and their operations natively, so a namespace
    /// that reaches another function by one of their names runs them as
    /// written, in compiled code as in the interpreter.
    #[test]
    fn compiled_code_does_not_fuse_over_a_shadowed_name() {
        let src = r#"::t ns
add fn (a: Int, b: Int): Int { 7 }
f fn (v: Vec): Int {
    reduce(v, (acc, x) { add(acc, x) }, 0)
}
[f([1, 2]), f([1, 2]), f([1, 2])]"#;
        let (out, runs) = run_counting_compiled_fused_runs(src, Some(val!({"jit": {"threshold": 1}})));
        assert_eq!(out, val!([7, 7, 7]));
        assert_eq!(runs, 0);
    }

    /// Compiled code tries a fused run and keeps the stages for a de-opt, and
    /// both paths meet with the same registers. So each of these runs with the
    /// JIT off, where only the stages run, and compiled from the first call:
    /// work around the pipeline has to read what it left, and a de-opt has to
    /// reach the stages. Failures have to match as well.
    #[test]
    fn compiled_pipelines_give_what_the_stages_give() {
        let programs = [
            // Values made before the pipeline, read after it.
            r#"::t ns
f fn (v: Vec, label: Str): Map {
    before {label: label, size: length(v)}
    total v
    |> filter((x) { gt(x, 1) })
    |> map((x) { mul(x, 2) })
    |> reduce((acc, x) { add(acc, x) }, 0)
    again v |> map((x) { add(x, 1) }) |> reduce((acc, x) { add(acc, x) }, 100)
    {before, total, again, label, v}
}
[f([1, 2, 3], "a"), f([1, 2, 3], "a"), f([4, 5], "b")]"#,
            // A count held raw, then used.
            r#"::t ns
f fn (v: Vec): Int {
    count v |> filter((x) { gt(x, 1) }) |> length()
    add(mul(count, 10), 1)
}
[f([1, 2, 3]), f([]), f([5])]"#,
            // A pipeline in each arm of a flow.
            r#"::t ns
f fn cond (n: Int): Int {
    gt(n, 5) => { range(n) |> map((x) { mul(x, 2) }) |> reduce((acc, x) { add(acc, x) }, 0) }
    eq(n, 5) => { range(n) |> filter((x) { gt(x, 1) }) |> length() }
    => { add(n, 1) }
}
[f(9), f(5), f(1), f(9), f(5)]"#,
            // Bools from `some` and `all`, branched on.
            r#"::t ns
f fn (v: Vec): Str {
    if(v |> map((x) { mul(x, 2) }) |> some((x) { gt(x, 4) }), "some", "none")
}
g fn (v: Vec): Str {
    if(v |> all((x) { gt(x, 0) }), "all", "not all")
}
[f([1, 2, 3]), f([1]), g([1, 2]), g([0, 1]), f([]), g([])]"#,
            // A pipeline among the arguments of a self tail call.
            r#"::t ns
f fn cond (i: Int, acc: Int): Int {
    eq(i, 0) => { acc }
    => {
        f(
            sub(i, 1),
            add(acc, range(i) |> map((x) { mul(x, 2) }) |> reduce((a, x) { add(a, x) }, 0)),
        )
    }
}
[f(5, 0), f(50, 1)]"#,
            // A pipeline with another call's register live around it.
            r#"::t ns
f fn (n: Int, s: Str): Str {
    `${s}:${add(mul(n, 2), range(n) |> reduce((a, x) { add(a, x) }, 0))}:${s}`
}
[f(3, "x"), f(4, "y"), f(5, "z")]"#,
            // A pipeline in a lambda that gets hot.
            r#"::t ns
f fn (ns: Vec): Vec {
    map(ns, (n) { range(n) |> map((x) { mul(x, 2) }) |> reduce((a, x) { add(a, x) }, 0) })
}
f([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])"#,
            // Results of other types, where a register holds them owned.
            r#"::t ns
f fn (n: Int): Vec {
    dec range(1, n) |> map((x) { div(x, 2) }) |> reduce((a, x) { add(a, x) }, 0)
    text reduce(range(n), (a, i) { concat(a, `i${i}`) }, "")
    [dec, text, dec]
}
[f(4), f(6), f(4)]"#,
            // De-opts: a predicate that isn't a Bool, and a source of another type.
            r#"::t ns
f fn (v: Vec): Int {
    v |> filter((x) { x }) |> reduce((a, x) { add(a, x) }, 0)
}
g fn (m: Any): Any {
    reduce(m, (a, x) { add(a, x) }, 0)
}
[f([1, 2]), f([]), f([3]), g([1, 2]), g({a: 1, b: 2}), g([3])]"#,
            // Overflow de-opts, and the stages report it.
            r#"::t ns
f fn (v: Vec): Int {
    reduce(v, (a, x) { mul(a, x) }, 1)
}
[f([2, 3]), f([9223372036854775807, 2]), f([4])]"#,
            // A range the fused run declines.
            r#"::t ns
f fn (n: Any): Int {
    range(n) |> reduce((a, x) { add(a, x) }, 0)
}
[f(3), f(1.5), f(3)]"#,
        ];
        // Where a failure is reported from differs between the tiers.
        let comparable = |result: Result<Val, String>| {
            result.map_err(|err| {
                err.split("instruction_pointer").next().unwrap().to_string()
            })
        };
        for src in programs {
            let stages = compile_and_run_with_std_conf(src, Some(val!({"jit": {"mode": "off"}})));
            let before = crate::lang::runtime::jit::fused_runs_for_test();
            let compiled =
                compile_and_run_with_std_conf(src, Some(val!({"jit": {"threshold": 1}})));
            let runs = crate::lang::runtime::jit::fused_runs_for_test() - before;
            assert!(runs > 0, "compiled code made no fused run of:\n{src}");
            assert_eq!(
                comparable(stages),
                comparable(compiled),
                "compiled code differs for:\n{src}"
            );
        }
    }

    /// Runs `src` compiled, with `edit` given the instructions of `::t/f` and
    /// its one pipeline. Returns the result and how many fused runs it made.
    fn run_with_pipeline_edited(
        src: &str,
        edit: impl FnOnce(&mut Vec<Instruction>, &PipelinePlan),
    ) -> (Val, u64) {
        let before = crate::lang::runtime::jit::fused_runs_for_test();
        let out = run_with_std_edited(
            src,
            Some(val!({"jit": {"threshold": 1}})),
            false,
            false,
            |program| {
                let id = program
                    .functions
                    .iter()
                    .position(|function| function.name == "::t/f")
                    .expect("the function exists");
                let plans = detect_compilable_pipelines(&program.functions[id].instructions, program);
                let [plan] = plans.as_slice() else {
                    panic!("expected one pipeline, found {plans:?}");
                };
                edit(&mut program.functions[id].instructions, plan);
            },
        )
        .expect("run");
        (out, crate::lang::runtime::jit::fused_runs_for_test() - before)
    }

    const READS_ITS_ARGUMENT_THROUGH_A_PIPELINE: &str = r#"::t ns
f fn (v: Vec): Int {
    v |> filter((x) { gt(x, 1) }) |> reduce((a, x) { add(a, x) }, 0)
}
run fn (): Map {
    v [1, 2, 3]
    total reduce(range(300), (acc, _i) { add(acc, f(v)) }, 0)
    {v, total}
}
run()"#;

    fn assert_read_argument_released(out: &Val, runs: u64) {
        assert!(runs >= 200, "compiled code made {runs} fused runs");
        assert_eq!(out.get("total"), Some(val!(1500)));
        let Some(Val::Vec(v)) = out.get("v") else {
            panic!("unexpected result {out:?}");
        };
        assert!(
            Arc::strong_count(&v) < 10,
            "{} references to the collection remain",
            Arc::strong_count(&v)
        );
    }

    // The compiler writes fresh registers in a pipeline's stages, so these
    // rewrite the bytecode for the cases it doesn't reach today. A fused run
    // skips those stages, and what follows treats the registers as they left
    // them, so it has to leave the same ownership.

    /// The register the source was loaded into, before the stages: the first
    /// instruction after the parameter checks loads it.
    fn loaded_source_register(instructions: &[Instruction]) -> u32 {
        let first = instructions
            .iter()
            .find(|ins| !matches!(ins, Instruction::CheckParam { .. }));
        let Some(Instruction::LoadVar { dest, .. }) = first else {
            panic!("expected the source to load first, found {instructions:?}");
        };
        *dest
    }

    /// A stage overwrites a register that owned a value: its value is released.
    #[test]
    fn a_fused_run_releases_what_its_stages_overwrite() {
        let (out, runs) = run_with_pipeline_edited(
            READS_ITS_ARGUMENT_THROUGH_A_PIPELINE,
            |instructions, plan| {
                let source = loaded_source_register(instructions);
                // The `reduce` initial value, the last argument it loads, goes
                // through the source's register.
                let Instruction::Call { args_start, .. } = instructions[plan.last_stage_ip] else {
                    panic!("expected the last stage's call");
                };
                let span = plan.first_stage_ip..plan.last_stage_ip;
                let initial_move = span
                    .clone()
                    .rev()
                    .find(|&ip| {
                        matches!(instructions[ip], Instruction::Move { dest, .. } if dest == args_start + 2)
                    })
                    .expect("the initial value moves into place");
                let Instruction::Move { src, .. } = &mut instructions[initial_move] else {
                    unreachable!();
                };
                let loaded = std::mem::replace(src, source);
                for ins in &mut instructions[span] {
                    if let Instruction::LoadConst { dest, .. } = ins
                        && *dest == loaded
                    {
                        *dest = source;
                    }
                }
            },
        );
        assert_read_argument_released(&out, runs);
    }

    /// The result lands in a register that owned a value: its value is
    /// released, and the result takes its place.
    #[test]
    fn a_fused_run_releases_what_its_result_replaces() {
        let (out, runs) = run_with_pipeline_edited(
            READS_ITS_ARGUMENT_THROUGH_A_PIPELINE,
            |instructions, plan| {
                // The first stage's argument register owns its source.
                let source = plan.source_reg;
                let Instruction::Call { dest, .. } = &mut instructions[plan.last_stage_ip] else {
                    panic!("expected the last stage's call");
                };
                let result = std::mem::replace(dest, source);
                for ins in &mut instructions[plan.last_stage_ip + 1..] {
                    if let Instruction::Return { value } = ins
                        && *value == result
                    {
                        *value = source;
                    }
                }
            },
        );
        assert_read_argument_released(&out, runs);
    }

    /// A stage leaves a value owned in a register that held a raw scalar. What
    /// follows releases that register, so a fused run mustn't leave the scalar
    /// there: released as a value, it is a wild pointer.
    #[test]
    fn a_fused_run_leaves_no_scalar_where_its_stages_leave_a_value() {
        let src = r#"::t ns
f fn (n: Int, v: Vec): Int {
    k add(n, 7)
    total v |> filter((x) { gt(x, 1) }) |> reduce((a, x) { add(a, x) }, 0)
    add(total, k)
}
run fn (): Int {
    v [1, 2, 3]
    reduce(range(300), (acc, _i) { add(acc, f(1, v)) }, 0)
}
run()"#;
        let (out, runs) = run_with_pipeline_edited(src, |instructions, plan| {
            // The first stage's result moves to the register that held the 7,
            // a constant Int, which is held raw.
            let scalar = instructions[..plan.first_stage_ip]
                .iter()
                .find_map(|ins| match ins {
                    Instruction::LoadConst { dest, .. } => Some(*dest),
                    _ => None,
                })
                .expect("the 7 loads before the pipeline");
            let Instruction::Call { dest, .. } = &mut instructions[plan.first_stage_ip] else {
                panic!("expected the first stage's call");
            };
            let result = std::mem::replace(dest, scalar);
            for ins in &mut instructions[plan.first_stage_ip + 1..=plan.last_stage_ip] {
                if let Instruction::Move { src, .. } = ins
                    && *src == result
                {
                    *src = scalar;
                }
            }
        });
        assert!(runs >= 200, "compiled code made {runs} fused runs");
        assert_eq!(out, val!(3900));
    }

    /// Compiled code owns references to the values it reads. A fused run
    /// leaves its registers holding what the stages would own, so it has to
    /// release the same references, on the fused path and on a de-opt's.
    #[test]
    fn compiled_pipelines_release_what_they_read() {
        let src = r#"::t ns
fused fn (v: Vec): Int {
    v |> filter((x) { gt(x, 1) }) |> map((x) { mul(x, 2) }) |> reduce((a, x) { add(a, x) }, 0)
}
deopts fn (v: Vec): Int {
    v |> filter((x) { x }) |> reduce((a, x) { add(a, x) }, 0)
}
run fn (): Map {
    v [1, 2, 3]
    total reduce(range(300), (acc, _i) { add(acc, add(fused(v), deopts(v))) }, 0)
    {v, total}
}
run()"#;
        let (out, runs) = run_counting_compiled_fused_runs(src, Some(val!({"jit": {"threshold": 1}})));
        assert!(runs >= 200, "compiled code made {runs} fused runs");
        assert_eq!(out.get("total"), Some(val!(4800)));
        let Some(Val::Vec(v)) = out.get("v") else {
            panic!("unexpected result {out:?}");
        };
        assert!(
            Arc::strong_count(&v) < 10,
            "{} references to the collection remain",
            Arc::strong_count(&v)
        );
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

    /// Run `src` interpreted and with the JIT compiling after one call; both
    /// must produce `expected`.
    fn assert_same_with_and_without_jit(src: &str, expected: &Val) {
        for conf in [None, Some(crate::val!({"jit": {"threshold": 1}}))] {
            let label = if conf.is_some() { "jit" } else { "interpreter" };
            let out = compile_and_run_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("{label} run failed: {e}\n{src}"));
            assert_eq!(&out, expected, "{label} result for:\n{src}");
        }
    }

    fn strs(values: &[&str]) -> Val {
        Val::vec(values.iter().map(|s| Val::from(*s)).collect())
    }

    /// A value imported from another namespace evaluates in its home
    /// namespace: its initializer finds that namespace's types, functions,
    /// and aliases, which the importer need not have.
    #[test]
    fn imported_value_evaluates_in_its_home_namespace() {
        let src = r#"::mp::duty ns
s4 ::mp::text/s4
s4-title ::mp::text/s4-title
s4-loud ::mp::text/s4-loud
shape ::mp::text/shape
heading ::mp::text/heading-of-s4()
probe fn (): Vec {
    [s4.section, s4-title, s4-loud, shape.section, heading]
}

::mp::text ns
::fmt ::mp::fmt
Clause type { section: Str, heading: Str, text: Str }
Shape enum { Circle(Clause), Dot }
title fn (c: Clause): Str { `${c.section} ${c.heading}` }
s4 Clause(section: "§4", heading: "Reporting", text: "Log it.")
s4-title title(s4)
s4-loud ::fmt/loud(s4.heading)
shape Shape.Circle({section: "§5", heading: "Holds", text: "Hold it."})
heading-of-s4 fn (): Str { s4.heading }

::mp::fmt ns
loud fn (s: Str): Str { uppercase(s) }

::mp::main ns
::mp::duty/probe()
::mp::duty/probe()"#;
        assert_same_with_and_without_jit(
            src,
            &strs(&["§4", "§4 Reporting", "REPORTING", "§5", "Reporting"]),
        );
        assert_eq!(
            compile_and_run_with_std_ast(src).as_ref(),
            Ok(&strs(&["§4", "§4 Reporting", "REPORTING", "§5", "Reporting"])),
        );
    }

    /// Namespaces that read each other's values run in source order, so the
    /// first reads a value its partner has not stored yet. The VM then
    /// evaluates that value's initializer from the AST, in its home
    /// namespace.
    #[test]
    fn value_read_before_its_namespace_runs_evaluates_in_its_home_namespace() {
        let src = r#"::mp::a ns
z "from a"
x ::mp::b/y

::mp::b ns
Note type { text: Str }
y Note(text: "from b")
w ::mp::a/z

::mp::c ns
probe fn (): Vec {
    x ::mp::a/x
    [x.text, ::mp::b/w]
}
probe()"#;
        assert_eq!(
            compile_and_run_with_std_ast(src).as_ref(),
            Ok(&strs(&["from b", "from a"])),
        );
    }

    /// A value can call a function before the function's namespace has run.
    /// The type aliases that function names are bound by then, so the run
    /// doesn't depend on which namespace sorts first.
    #[test]
    fn value_calling_a_function_finds_its_type_aliases() {
        let src = r#"::aa::c ns
made ::o::b/make()

::o::b ns
Thing ::o::a/Thing
make fn (): Thing { Thing(n: 1) }

::o::a ns
Thing type { n: Int }

::main ns
::aa::c/made.n"#;
        assert_same_with_and_without_jit(src, &Val::Int(1));
    }

    /// Every name for a type or variant is there before any namespace runs.
    /// These namespaces read each other's values, so no order would help:
    /// `::aa::c` runs first, and calls into `::o::b` before it has run. Each
    /// alias gets its own program, since a local alias is a value that would
    /// otherwise move `::o::b` first for the aliases beside it.
    #[test]
    fn type_aliases_are_bound_before_any_namespace_runs() {
        let cases = [
            ("record", "Thing ::o::a/Thing", "Thing(n: 1).n", val!(1)),
            ("enum", "Dir ::o::a/Dir", "eq(Dir.Up, ::o::a/Dir.Up)", val!(true)),
            (
                "record variant",
                "Round ::o::a/Shape.Circle",
                "Round(r: 2).r",
                val!(2),
            ),
            (
                "plain variant",
                "Pack ::o::a/Shape.Boxed",
                r#"eq(Pack("x"), ::o::a/Shape.Boxed("x"))"#,
                val!(true),
            ),
            (
                "unit variant",
                "Spot ::o::a/Shape.Dot",
                "eq(Spot, ::o::a/Shape.Dot)",
                val!(true),
            ),
            ("where type", "Port ::o::a/Port", "Port(80)", val!(80)),
            (
                "literal union",
                "Fruit ::o::a/Fruit",
                r#"Fruit("apple")"#,
                val!("apple"),
            ),
            (
                "alias of an alias",
                "Relayed ::o::d/Thing",
                "Relayed(n: 3).n",
                val!(3),
            ),
            (
                "local alias",
                "Thing ::o::a/Thing\nNear Thing",
                "Near({n: 4}).n",
                val!(4),
            ),
            (
                "alias of a local alias",
                "Thing ::o::a/Thing\nNear Thing\nNearer Near",
                "Nearer({n: 5}).n",
                val!(5),
            ),
            (
                "local variant",
                "Dir ::o::a/Dir\nDown Dir.Down",
                "eq(Down, ::o::a/Dir.Down)",
                val!(true),
            ),
        ];
        let mut failures = Vec::new();
        for (kind, aliases, expression, expected) in &cases {
            let src = format!(
                r#"::aa::c ns
base 5
made ::o::b/make()

::o::b ns
{aliases}
limit ::aa::c/base
make fn (): Any {{ {expression} }}

::o::d ns
Thing ::o::a/Thing

::o::a ns
Thing type {{ n: Int }}
Dir enum {{ Up, Down }}
Circle type {{ r: Int }}
Shape enum {{ Circle(Circle), Boxed(Str), Dot }}
Port type Int where between(%, 1, 65535)
Fruit type "apple" | "banana"

::main ns
::aa::c/made"#
            );
            for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
                let got = compile_and_run_with_std_conf(&src, conf);
                if got.as_ref() != Ok(expected) {
                    failures.push(format!("{kind}: {got:?}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A value whose Map or Vec literal calls into another namespace waits
    /// for that namespace to run, so the callee finds its own constants.
    /// Calls held inside a literal were invisible to the ordering, and the
    /// importer (first in source order) ran before `CONST` was stored.
    #[test]
    fn value_calling_another_namespace_inside_a_literal_runs_after_it() {
        let src = r#"::rdoc::a ns
VALUE {name: ::zz::b/get-name().name}
NAMES [::zz::b/get-name().name, "y"]
SPREAD {...::zz::b/get-name(), extra: ::zz::b/get-name().name}

::zz::b ns
CONST "x"
get-name fn (): Map { {name: CONST} }

::main ns
names ::rdoc::a/NAMES
[::rdoc::a/VALUE.name, names[0], ::rdoc::a/SPREAD.extra]"#;
        assert_same_with_and_without_jit(src, &strs(&["x", "x", "x"]));
    }

    /// An Err passed to a parameter not declared `Result` halts by design
    /// (the Result-boundary rule); the halt names that parameter and
    /// function, since the Err may have been made far from the call.
    #[test]
    fn consumed_err_argument_halt_names_parameter_and_function() {
        let src = r#"::t ns
takes fn (x: Int): Int { add(x, 1) }
probe fn (s: Str): Int {
    n Int(s)
    takes(n)
}
probe("1")
probe("2")
probe("3")
probe("abc")"#;
        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("an Err passed to an Int parameter halts");
            assert!(
                error.contains(
                    "Result error: Cannot convert 'abc' to Int (an Err passed to parameter `x` of ::t/takes; declare the parameter `Result` to receive Errs)"
                ),
                "{mode}: {error}"
            );
        }
    }

    /// Lambdas, placeholder lambdas and natives consume an Err as user
    /// functions do, and each halt names what consumed it.
    #[test]
    fn consumed_err_halt_names_lambda_placeholder_and_native() {
        let cases = [
            (
                "probe fn (): Any { f (x) { 1 }\n f(err(\"boom\")) }",
                "boom (an Err passed to parameter `x` of a lambda;",
            ),
            (
                "probe fn (): Any { map([err(\"boom\")], add(%, 1)) }",
                "boom (an Err passed to placeholder `%` of a lambda;",
            ),
            (
                "probe fn (): Any { is-null(err(\"boom\")) }",
                "boom (an Err passed as argument 1 of ::hot::type/is-null, which needs a value;",
            ),
            (
                "probe fn (n: Int): Any { add(n, err(\"boom\")) }",
                "boom (an Err passed as argument 2 of ::hot::math/add, which needs a value;",
            ),
            (
                "probe fn (): Any { length(err(\"boom\")) }",
                "boom (an Err passed as argument 1 of ::hot::coll/length, which needs a value;",
            ),
        ];
        for (body, expected) in cases {
            let call = if body.contains("(n: Int)") { "probe(1)" } else { "probe()" };
            let src = format!("::t ns\n{body}\n{call}");
            for (mode, conf) in [
                ("jit", crate::val!({"jit": {"threshold": 1}})),
                ("interpreter", crate::val!({"jit": {"mode": "off"}})),
            ] {
                let error = compile_and_run_with_std_conf(&src, Some(conf))
                    .expect_err("the Err is consumed");
                assert!(
                    error.contains(&format!("Result error: {expected}")),
                    "{mode}: {error}\n{src}"
                );
            }
        }
    }

    /// Compiled code halts on a numeric op's Err at the op when every path
    /// consumes it, so it has to name the consumer the interpreter names.
    #[test]
    fn numeric_err_consumed_in_compiled_code_names_its_consumer() {
        let cases = [
            (
                "takes fn (x: Int): Int { x }\nprobe fn (n: Int): Int { takes(add(n, 9223372036854775807)) }",
                "Integer overflow (an Err passed to parameter `x` of ::t/takes;",
            ),
            (
                "probe fn (n: Int): Int { mul(add(n, 9223372036854775807), 2) }",
                "Integer overflow (an Err passed as argument 1 of ::hot::math/mul, which needs a value;",
            ),
        ];
        for (body, expected) in cases {
            let src = format!("::t ns\n{body}\nprobe(0)\nprobe(0)\nprobe(1)");
            for (mode, conf) in [
                ("jit", crate::val!({"jit": {"threshold": 1}})),
                ("interpreter", crate::val!({"jit": {"mode": "off"}})),
            ] {
                let error = compile_and_run_with_std_conf(&src, Some(conf))
                    .expect_err("the overflow is consumed");
                assert!(
                    error.contains(&format!("Result error: {expected}")),
                    "{mode}: {error}\n{src}"
                );
            }
        }
    }

    /// A value of a declared type over a number compares as the number it
    /// holds, in both tiers, for `Dec` and `Int` alike. Compared any other way,
    /// 80 comes out above 100 and 200 below 80.
    #[test]
    fn named_numbers_compare_as_the_numbers_they_hold() {
        let mut cases = Vec::new();
        for (low, high) in [("Money(80.0)", "Money(200.0)"), ("Count(80)", "Count(200)")] {
            cases.extend([
                (format!("gt-any({low}, 100)"), false),
                (format!("lt-any({low}, 100)"), true),
                (format!("gte-any({low}, 100)"), false),
                (format!("lte-any({low}, 100)"), true),
                (format!("gt-any({high}, 100)"), true),
                (format!("lt-any({high}, 100)"), false),
                (format!("gte-any({high}, 100)"), true),
                (format!("lte-any({high}, 100)"), false),
                (format!("gt-any(100, {low})"), true),
                (format!("lt-any(100, {low})"), false),
                (format!("gt-any(100.5, {high})"), false),
                (format!("lt-any(100.5, {high})"), true),
                (format!("gt-any({high}, {low})"), true),
                (format!("lt-any({high}, {low})"), false),
                (format!("gt-any({low}, {low})"), false),
                (format!("gte-any({low}, {low})"), true),
                (format!("lte-any({low}, {low})"), true),
                // No `Any` needed: the checker reads the type as its number.
                (format!("gt({low}, 100)"), false),
                (format!("lt({low}, 100)"), true),
                (format!("gt({high}, {low})"), true),
            ]);
        }
        cases.extend([
            // Payloads of each kind inside one type.
            ("gt-any(Money(200), Money(80.5))".to_string(), true),
            ("lt-any(Money(200), Money(80.5))".to_string(), false),
            // A parameter or a field of the type, not only a constructor call.
            ("cost-over(Line(cost: Money(80.0)), 100.0)".to_string(), false),
            ("cost-over(Line(cost: Money(200.0)), 100.0)".to_string(), true),
            ("count-under(Count(80), 100)".to_string(), true),
            ("count-under(Count(200), 100)".to_string(), false),
            ("between-any(Money(80.0), 1, 100)".to_string(), true),
            ("between-any(Money(200.0), 1, 100)".to_string(), false),
            ("between-any(Count(80), 1, 100)".to_string(), true),
        ]);
        let exprs: Vec<&str> = cases.iter().map(|(expr, _)| expr.as_str()).collect();
        let src = format!(
            r#"::t ns
Money type Dec
Count type Int
Line type {{ cost: Money }}
gt-any fn (a: Any, b: Any): Any {{ gt(a, b) }}
gte-any fn (a: Any, b: Any): Any {{ gte(a, b) }}
lt-any fn (a: Any, b: Any): Any {{ lt(a, b) }}
lte-any fn (a: Any, b: Any): Any {{ lte(a, b) }}
between-any fn (x: Any, low: Any, high: Any): Any {{ between(x, low, high) }}
cost-over fn (line: Line, limit: Dec): Bool {{ gt(line.cost, limit) }}
count-under fn (count: Count, limit: Int): Bool {{ lt(count, limit) }}
[{}]"#,
            exprs.join(", ")
        );
        let expected = Val::vec(cases.iter().map(|(_, want)| Val::Bool(*want)).collect());
        assert_same_with_and_without_jit(&src, &expected);
    }

    /// The answer a comparison gives a named number decides a branch the same
    /// way in both tiers, and in a pipeline: an Err here would read as false.
    #[test]
    fn named_numbers_take_the_right_branch() {
        let src = r#"::t ns
Money type Dec
Count type Int
Line type { cost: Money }
by-if fn (cost: Any, limit: Any): Str { if(gt(cost, limit), "over", "within") }
by-cond fn (cost: Any, limit: Any): Str {
    cond {
        gt(cost, limit) => { "over" }
        => { "within" }
    }
}
by-not fn (cost: Any, limit: Any): Bool { not(gt(cost, limit)) }
by-and fn (cost: Any, limit: Any): Bool { and(gt(cost, 0), lt(cost, limit)) }
over fn (lines: Vec, limit: Any): Int { length(filter(lines, gt(%.cost, limit))) }
counted fn (items: Vec, limit: Any): Int {
    reduce(items, (total, item) { if(lt(item, limit), add(total, 1), total) }, 0)
}
[
    by-if(Money(200.0), 100), by-if(Money(80.0), 100),
    by-if(Count(200), 100), by-if(Count(80), 100),
    by-cond(Money(200.0), 100), by-cond(Money(80.0), 100),
    by-not(Money(80.0), 100), by-not(Money(200.0), 100),
    by-and(Money(80.0), 100), by-and(Money(200.0), 100),
    over([Line(cost: Money(80.0)), Line(cost: Money(200.0))], 100),
    length(filter([Money(80.0), Money(200.0)], gt(%, 100))),
    counted([Count(80), Count(200), Count(99)], 100),
]"#;
        let expected = val!([
            "over", "within", "over", "within", "over", "within", true, false, true, false, 1, 1, 2
        ]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// A `where` predicate compares the same way, so a constraint on a named
    /// number holds the number to its bound in both directions.
    #[test]
    fn constraints_compare_named_numbers_as_their_numbers() {
        let src = r#"::t ns
Money type Dec
Count type Int
Capped type { cost: Money where lte(%, 100) }
Owed type { amount: Money where gt(%, 0) }
Few type { count: Count where lt(%, 10) }
capped fn (cost: Any): Bool { is-err(Capped(cost: cost)) }
owed fn (amount: Any): Bool { is-err(Owed(amount: amount)) }
few fn (count: Any): Bool { is-err(Few(count: count)) }
[
    capped(Money(80.0)), capped(Money(200.0)), capped(Money(100.0)),
    owed(Money(5.0)), owed(Money(-5.0)),
    few(Count(9)), few(Count(10)),
]"#;
        let expected = val!([false, true, false, false, true, false, true]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// Operands with no order give an Err, as `add` does for a value that isn't
    /// a number, and never an answer read from how the values print. Compiled
    /// code keeps that Err a value, so each tier sees the same thing, including
    /// where a function has already run on numbers.
    #[test]
    fn a_comparison_with_no_order_gives_an_err_in_both_tiers() {
        let operands = [
            "Money(80.0), null",
            "null, Money(80.0)",
            "Count(80), \"100\"",
            "null, 1",
            "1, null",
            "\"b\", 1",
            "[10], [9]",
            "{a: 1}, {a: 2}",
            "Point(x: 1), Point(x: 2)",
            "Temp.Celsius(30.0), 100",
            "Temp.Celsius(30.0), Temp.Fahrenheit(80.0)",
        ];
        let calls: Vec<String> = operands
            .iter()
            .map(|args| {
                format!(
                    "[is-err(gt-any({args})), is-err(lt-any({args})), \
                     is-err(gte-any({args})), is-err(lte-any({args})), \
                     tested({args}), kept({args}), negated({args})]"
                )
            })
            .collect();
        let src = format!(
            r#"::t ns
Money type Dec
Count type Int
Point type {{ x: Int }}
Temp enum {{ Celsius(Dec), Fahrenheit(Dec) }}
gt-any fn (a: Any, b: Any): Bool {{ gt(a, b) }}
gte-any fn (a: Any, b: Any): Any {{ gte(a, b) }}
lt-any fn (a: Any, b: Any): Any {{ lt(a, b) }}
lte-any fn (a: Any, b: Any): Any {{ lte(a, b) }}
tested fn (a: Any, b: Any): Str {{ if(gt(a, b), "then", "else") }}
kept fn (a: Any, b: Any): Bool {{
    result gt(a, b)
    is-err(result)
}}
negated fn (a: Any, b: Any): Bool {{ not(lt(a, b)) }}
[gt-any(2, 1), lt-any(2, 1), gte-any(2, 1), lte-any(2, 1), tested(2, 1), kept(2, 1), negated(2, 1)]
[{}]"#,
            calls.join(", ")
        );
        let row = val!([true, true, true, true, "else", true, true]);
        let expected = Val::vec(vec![row; operands.len()]);
        assert_same_with_and_without_jit(&src, &expected);
    }

    /// A field's kind isn't known when a function compiles, so compiled code
    /// reaches the comparison that takes any value. That comparison has to keep
    /// an Err a value: a raw Bool would turn it into `false`, and where the
    /// interpreter halts or returns the Err, the run would go on with an answer.
    #[test]
    fn a_compiled_comparison_on_a_field_keeps_its_err() {
        let src = r#"::t ns
Money type Dec
kept fn (m: Map): Bool {
    result gt(m.a, m.b)
    is-err(result)
}
returned fn (m: Map): Bool { lt(m.a, m.b) }
returned-any fn (m: Map): Any { gte(m.a, m.b) }
tested fn (m: Map): Str { if(lte(m.a, m.b), "then", "else") }
negated fn (m: Map): Bool { not(gt(m.a, m.b)) }
both fn (m: Map): Bool { and(gt(m.a, 0), lt(m.a, m.b)) }
good {a: 2, b: 1}
named {a: Money(200.0), b: 100}
bad {a: Money(80.0), b: null}
[
    kept(good), kept(good), kept(good), kept(named), kept(bad),
    returned(good), returned(good), returned(good), returned(named), is-err(returned(bad)),
    returned-any(good), returned-any(good), returned-any(named), is-err(returned-any(bad)),
    tested(good), tested(good), tested(good), tested(named), tested(bad),
    negated(good), negated(good), negated(good), negated(named), negated(bad),
    both(good), both(good), both(good), both(named),
]"#;
        let expected = val!([
            false, false, false, false, true,
            false, false, false, false, true,
            true, true, true, true,
            "else", "else", "else", "else", "else",
            false, false, false, false, true,
            false, false, false, false
        ]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// A compiled order on a field is owned, so it meets raw values where
    /// flows and calls join: a raw `true` read as a pointer would crash here.
    #[test]
    fn a_compiled_order_on_a_field_mixes_with_raw_results() {
        let src = r#"::t ns
Money type Dec
joined fn (m: Map, known: Bool): Bool {
    cond {
        known => { true }
        => { gt(m.a, m.b) }
    }
}
joined-other-way fn (m: Map, known: Bool): Bool {
    cond {
        known => { lt(m.a, m.b) }
        => { false }
    }
}
by-if fn (m: Map, known: Bool): Bool { if(known, gte(m.a, m.b), eq(m.a, m.b)) }
counted fn cond (m: Map, n: Int, total: Int): Int {
    eq(n, 0) => { total }
    gt(m.a, m.b) => { counted(m, sub(n, 1), add(total, 1)) }
    => { counted(m, sub(n, 1), total) }
}
last-is-over fn cond (m: Map, n: Int): Bool {
    eq(n, 0) => { gt(m.a, m.b) }
    => { last-is-over(m, sub(n, 1)) }
}
over {a: Money(200.0), b: 100}
under {a: Money(80.0), b: 100}
[
    joined(over, true), joined(over, false), joined(under, false), joined(under, true),
    joined(under, false), joined(over, false),
    joined-other-way(under, true), joined-other-way(over, true), joined-other-way(over, false),
    joined-other-way(under, true),
    by-if(over, true), by-if(under, true), by-if(under, false), by-if(over, true),
    counted(over, 5, 0), counted(under, 5, 0), counted(over, 5, 0),
    last-is-over(over, 3), last-is-over(under, 3), last-is-over(over, 3),
]"#;
        let expected = val!([
            true, true, false, true, false, true, true, false, false, true, true, false, false,
            true, 5, 0, 5, true, false, true
        ]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// The same on a field, with the Err consumed after compiling.
    #[test]
    fn a_compiled_comparison_on_a_field_halts_when_its_err_is_consumed() {
        let cases = [
            (
                "takes fn (x: Bool): Bool { x }\nprobe fn (m: Map): Bool { takes(gt(m.a, m.b)) }",
                "gt requires numbers (an Err passed to parameter `x` of ::t/takes;",
            ),
            (
                "probe fn (m: Map): Any {\n    result lte(m.a, m.b)\n    Str(result)\n}",
                "lte requires numbers (an Err passed",
            ),
        ];
        for (body, expected) in cases {
            let src = format!(
                "::t ns\nMoney type Dec\n{body}\nprobe({{a: 1, b: 2}})\nprobe({{a: 1, b: 2}})\nprobe({{a: 1, b: 2}})\nprobe({{a: Money(1.0), b: null}})"
            );
            for (mode, conf) in [
                ("jit", crate::val!({"jit": {"threshold": 1}})),
                ("interpreter", crate::val!({"jit": {"mode": "off"}})),
            ] {
                let error = compile_and_run_with_std_conf(&src, Some(conf))
                    .expect_err("the Err is consumed");
                assert!(
                    error.contains(&format!("Result error: {expected}")),
                    "{mode}: {error}\n{src}"
                );
            }
        }
    }

    /// A pipeline's comparison on a value with no order stops the run in both
    /// tiers, fused or not, as `add` does there.
    #[test]
    fn a_pipeline_comparison_with_no_order_halts() {
        let src = "::t ns\nMoney type Dec\nprobe fn (xs: Vec): Int { length(filter(xs, gt(%, 0))) }\nprobe([1, 2])\nprobe([1, 2])\nprobe([1, 2])\nprobe([1, null])";
        for (mode, conf) in [
            ("jit", crate::val!({"jit": {"threshold": 1}})),
            ("interpreter", crate::val!({"jit": {"mode": "off"}})),
        ] {
            let error = compile_and_run_with_std_conf(src, Some(conf))
                .expect_err("the predicate's Err is consumed");
            assert!(error.contains("gt requires numbers"), "{mode}: {error}");
        }
    }

    /// Consuming that Err halts, and compiled code names what consumed it as
    /// the interpreter does, after running on numbers first.
    #[test]
    fn a_comparison_err_that_is_consumed_halts() {
        let cases = [
            (
                "takes fn (x: Bool): Bool { x }\nprobe fn (a: Any, b: Any): Bool { takes(gt(a, b)) }",
                "gt requires numbers (an Err passed to parameter `x` of ::t/takes;",
            ),
            (
                "probe fn (a: Any, b: Any): Any { concat(Str(lte(a, b)), \"x\") }",
                "lte requires numbers (an Err passed as argument 1 of ::hot::type/Str",
            ),
        ];
        for (body, expected) in cases {
            let src = format!("::t ns\nMoney type Dec\n{body}\nprobe(1, 2)\nprobe(1, 2)\nprobe(Money(1.0), null)");
            for (mode, conf) in [
                ("jit", crate::val!({"jit": {"threshold": 1}})),
                ("interpreter", crate::val!({"jit": {"mode": "off"}})),
            ] {
                let error = compile_and_run_with_std_conf(&src, Some(conf))
                    .expect_err("the Err is consumed");
                assert!(
                    error.contains(&format!("Result error: {expected}")),
                    "{mode}: {error}\n{src}"
                );
            }
        }
    }

    /// Values a comparison could already order keep their answers, read
    /// through a declared type as numbers are.
    #[test]
    fn strings_and_bools_keep_their_order() {
        let src = r#"::t ns
Code type Str
Flag type Bool
gt-any fn (a: Any, b: Any): Any { gt(a, b) }
lt-any fn (a: Any, b: Any): Any { lt(a, b) }
[
    gt-any("b", "a"), lt-any("b", "a"), gt-any(true, false), lt-any(true, false),
    gt-any(Code("b"), Code("a")), lt-any(Code("b"), "c"), gt-any(Flag(true), false),
]"#;
        let expected = val!([true, false, true, false, true, true, true]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// `sort` agrees with `lt`: named numbers go by the number they hold,
    /// whichever kind holds it, among plain numbers, and inside other values.
    #[test]
    fn sort_orders_named_numbers_by_the_number_they_hold() {
        let src = r#"::t ns
Money type Dec
Count type Int
shown fn (values: Vec): Vec { map(values, (value) { Str(untype(value)) }) }
costs fn (values: Vec): Vec { map(values, (value) { Str(untype(value.cost)) }) }
[
    shown(sort([Money(200), Money(80.5), Money(100.0), Money(9)])),
    shown(sort([Count(200), Count(80), Count(100)])),
    shown(sort([Money(200.0), 100, Money(80.0), 90.5, Count(95)])),
    shown(sort-by([Money(200), Money(80.5), Money(9)], (value) { value })),
    shown(sort-by([{n: 1}, {n: 2}], (value) { Money(sub(0, value.n)) }) |> map(%.n)),
    costs(sort([{cost: Money(200)}, {cost: Money(80.5)}])),
]"#;
        let expected = val!([
            ["9", "80.5", "100.0", "200"],
            ["80", "100", "200"],
            ["80.0", "90.5", "95", "100", "200.0"],
            ["9", "80.5", "200"],
            ["2", "1"],
            ["80.5", "200"]
        ]);
        assert_same_with_and_without_jit(src, &expected);
    }

    /// Serve `requests` HTTP requests on a local port with a fixed JSON reply,
    /// and return the port's base URL.
    fn serve_json(requests: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let Ok(mut stream) = stream else { return };
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let body = r#"{"secret":"account-123"}"#;
                let reply = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\nx-trace: abc\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        url
    }

    /// A real response has its declared type, so `Target(response)` runs the
    /// `HttpResponse -> Target` arrow instead of copying fields, here dropping
    /// a body the arrow redacts. It used to be an untagged Map, so the arrow
    /// only ran for values built with `HttpResponse(...)`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_http_response_runs_a_declared_arrow() {
        let src = |url: &str| {
            format!(
                r#"::t ns
::http ::hot::http
HttpResponse ::http/HttpResponse
HttpRequest ::http/HttpRequest
Target type {{ status: Int, body: Any, message: Str? }}
HttpResponse -> Target fn (r: HttpResponse): Target {{
    Target({{status: r.status, body: "redacted", message: `arrow ran ${{r.status}}`}})
}}
typed-param fn (x: HttpResponse): Target {{ Target(x) }}
describe fn match (r: Any): Str {{
    HttpResponse => {{ "a response" }}
    => {{ "something else" }}
}}
probe fn (): Vec {{
    r ::http/request("GET", "{url}", {{}}, "")
    fetched ::http/get("{url}")
    sent ::http/request(HttpRequest({{method: "GET", url: "{url}"}}))
    direct Target(r)
    via-param typed-param(r)
    via-get Target(fetched)
    via-request Target(sent)
    tagged from-json(to-json(r))
    plain from-json(to-json(untype(r)))
    spread {{...r, extra: 1}}
    [
        direct.message, direct.body, via-param.message,
        via-get.message, via-request.message,
        r.status, r.body.secret, get(r.headers, "x-trace"),
        get(r, "status"), is-type(r, HttpResponse), describe(r),
        ::http/is-ok-response(r), is-map(untype(r)),
        is-type(tagged, HttpResponse), is-type(plain, HttpResponse), plain.status,
        spread.status, get(assoc(r, "status", 201), "status"),
    ]
}}
probe()
probe()
probe()"#
            )
        };
        let expected = Val::vec(vec![
            Val::from("arrow ran 404"),
            Val::from("redacted"),
            Val::from("arrow ran 404"),
            Val::from("arrow ran 404"),
            Val::from("arrow ran 404"),
            Val::Int(404),
            Val::from("account-123"),
            Val::from("abc"),
            Val::Int(404),
            Val::Bool(true),
            Val::from("a response"),
            Val::Bool(false),
            Val::Bool(true),
            Val::Bool(true),
            Val::Bool(false),
            Val::Int(404),
            Val::Int(404),
            Val::Int(201),
        ]);
        for conf in [
            crate::val!({"jit": {"mode": "disabled"}}),
            crate::val!({"jit": {"threshold": 1}}),
        ] {
            // HTTP natives block on the runtime, so they run off its threads.
            let program = src(&serve_json(9));
            let run_conf = conf.clone();
            let out = tokio::task::spawn_blocking(move || {
                compile_and_run_with_std_conf(&program, Some(run_conf))
            })
            .await
            .expect("the run finishes")
            .unwrap_or_else(|e| panic!("{conf:?}: {e}"));
            assert_eq!(out, expected, "{conf:?}");
        }
    }

    /// A lambda bound at top level and called by name receives an Err in a
    /// `Result` parameter. The by-name call found only functions named `f`,
    /// not the lambda its dispatch runs, so the argument was consumed.
    #[test]
    fn top_level_lambda_with_result_parameter_receives_err() {
        let src = r#"::t ns
f (x: Result) { is-err(x) }
r err("boom")
[f(err("boom")), f(r), f(ok(1))]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![Val::Bool(true), Val::Bool(true), Val::Bool(false)]),
        );
    }

    /// `Name({...})` inside a custom constructor of a field-less `Name type fn`
    /// type reaches the default Map constructor. These types had no default
    /// constructor, so the inner call resolved back to a custom overload and
    /// never returned (the shipped `Email` and `Point` examples hung).
    #[test]
    fn field_less_type_fn_constructor_builds_through_default_map_constructor() {
        let email = r#"::t ns
Email type fn
(value: Str): Email { Email({value,}) },
(local: Str, domain: Str): Email { Email({value: `${local}@${domain}`}) }
a Email("a@b.com")
b Email("x", "y.com")
c Email({value: "m@b.com"})
[a.value, b.value, c.value, Str(is-type(a, Email))]"#;
        assert_same_with_and_without_jit(
            email,
            &strs(&["a@b.com", "x@y.com", "m@b.com", "true"]),
        );

        let point = r#"::t ns
Point type fn (x: Int, y: Int): Point { Point({x, y}) }
p Point(10, 20)
[p.x, p.y]"#;
        assert_same_with_and_without_jit(point, &Val::vec(vec![Val::Int(10), Val::Int(20)]));
    }

    /// A Map that is not a literal (a variable or a call result) selects the
    /// type's default Map constructor at runtime, as a literal does at compile
    /// time. It used to be coerced into the `(value: Str)` overload, which
    /// stored the stringified Map; inside the constructor's own body the
    /// by-name call re-entered that overload until the recursion limit.
    #[test]
    fn non_literal_map_arg_reaches_default_map_constructor() {
        let struct_form = r#"::t ns
Email type { value: Str }
Email fn (value: Str): Email { Email({value,}) }
m {value: "var@b.com"}
get-m fn (): Map { {value: "call@b.com"} }
[Email(m).value, Email(get-m()).value, Email("str@b.com").value]"#;
        assert_same_with_and_without_jit(
            struct_form,
            &strs(&["var@b.com", "call@b.com", "str@b.com"]),
        );

        let own_body = r#"::t ns
Slug type fn (value: Str): Slug {
    data {value: lowercase(value)}
    Slug(data)
}
[Slug("Hello").value, Slug("World").value, Slug("Again").value]"#;
        assert_same_with_and_without_jit(own_body, &strs(&["hello", "world", "again"]));
    }

    /// A Map reaches a one-parameter custom constructor whose type takes a
    /// Map wherever `Map` appears in it. Only `Map | Str` used to: the
    /// constructor was keyed by the first member of its union, so with
    /// `Str | Map`, `Map?` or `Map<Str, Any>` Maps went to the default
    /// constructor instead.
    #[test]
    fn map_args_reach_a_constructor_whose_type_takes_a_map() {
        for param in ["Map | Str", "Str | Map", "Map?", "Map<Str, Any>"] {
            let src = format!(
                r#"::t ns
Box type {{ value: Any }}
fn (m: {param}) {{ Box({{value: ["custom", m]}}) }}
m {{value: 2}}
[Box({{value: 1}}).value, Box(m).value, ::t/Box({{value: 3}}).value]"#
            );
            assert_same_with_and_without_jit(
                &src,
                &crate::val!([
                    ["custom", {"value": 1}],
                    ["custom", {"value": 2}],
                    ["custom", {"value": 3}]
                ]),
            );
        }

        // `Any` and untyped parameters leave Maps to the default constructor.
        for param in ["v: Any", "v"] {
            let src = format!(
                r#"::t ns
Box type {{ value: Any }}
fn ({param}) {{ Box({{value: ["custom", v]}}) }}
[Box(1).value, Box({{value: 2}}).value]"#
            );
            assert_same_with_and_without_jit(&src, &crate::val!([["custom", 1], 2]));
        }
    }

    /// Of two constructors that take a Map, the one typed `Map` gets Maps
    /// before a union, in either declaration order.
    #[test]
    fn a_map_constructor_takes_maps_before_a_union_constructor() {
        let constructors = [
            "(m: Map) { Box({value: [\"map\", m]}) }",
            "(m: Str | Map) { Box({value: [\"union\", m]}) }",
        ];
        for [first, second] in [constructors, [constructors[1], constructors[0]]] {
            let src = format!(
                r#"::t ns
Box type {{ value: Any }}
fn {first},
{second}
m {{value: 2}}
[Box({{value: 1}}).value, Box(m).value, Box("s").value]"#
            );
            assert_same_with_and_without_jit(
                &src,
                &crate::val!([["map", {"value": 1}], ["map", {"value": 2}], ["union", "s"]]),
            );
        }
    }

    /// Custom constructors still build through the default constructor when
    /// another one takes Maps. A Map literal in any constructor's body calls
    /// the default one, and the constructor that takes the Map can pass it on
    /// with `Name(map)`. Both used to depend on declaration order: a body
    /// reached a Map constructor declared before it, and passing the Map on
    /// after another one-parameter constructor recursed until the limit.
    #[test]
    fn constructors_build_through_the_default_when_another_takes_maps() {
        let literal_in_later_body = r#"::t ns
Box type { value: Any }
fn (m: Map) { Box({value: ["map", m]}) },
(s: Str) { Box({value: s}) }
[Box("s").value, Box({value: 1}).value]"#;
        assert_same_with_and_without_jit(
            literal_in_later_body,
            &crate::val!(["s", ["map", {"value": 1}]]),
        );

        let pass_the_map_on = r#"::t ns
Box type { value: Any }
fn (n: Int) { Box({value: n}) },
(m: Str | Map) { Box(merge(m, {value: ["seen", m.value]})) }
m {value: 3}
[Box(1).value, Box({value: 2}).value, Box(m).value]"#;
        assert_same_with_and_without_jit(
            pass_the_map_on,
            &crate::val!([1, ["seen", 2], ["seen", 3]]),
        );
    }

    /// A call defers its arguments for the overload it resolves to. With a
    /// lazy one-parameter constructor, `Box({value: 2})` resolves to the
    /// default constructor but deferred the Map for the lazy one, so the
    /// default constructor got a thunk. Plain functions whose overloads mix
    /// lazy and evaluated parameters had the same problem.
    #[test]
    fn calls_defer_arguments_for_the_overload_they_resolve_to() {
        let lazy_constructor = r#"::t ns
Box type { value: Any }
fn (lazy v: Str) { Box({value: `lazy:${do(v)}`}) }
[Box({value: 2}).value, Box({value: add(1, 1)}).value, ::t/Box({value: 3}).value, Box("s").value]"#;
        assert_same_with_and_without_jit(lazy_constructor, &crate::val!([2, 2, 3, "lazy:s"]));

        let overloads = [
            "(lazy x: Str): Str { `lazy:${do(x)}` }",
            "(x: Int): Str { Str(add(x, 1)) }",
        ];
        for [first, second] in [overloads, [overloads[1], overloads[0]]] {
            let src = format!(
                r#"::t ns
g fn
{first},
{second}
[g("a"), g(1), ::t/g("b"), ::t/g(2)]"#
            );
            assert_same_with_and_without_jit(&src, &strs(&["lazy:a", "2", "lazy:b", "3"]));
        }
    }

    /// Same-arity overloads dispatch on runtime argument types whichever
    /// overload is declared last (the compiler's arity-only fallback key), for
    /// direct calls, self tail calls, and calls made from JIT-compiled code.
    #[test]
    fn same_arity_overloads_dispatch_on_runtime_arg_types() {
        let direct = r#"::t ns
f fn
(x: Str): Str { "str" },
(x: Map): Str { "map" }
g fn
(x: Map): Str { "map" },
(x: Str): Str { "str" }
m {a: 1}
s "hi"
[f(m), f(s), g(m), g(s), f({a: 1}), g("lit")]"#;
        assert_same_with_and_without_jit(
            direct,
            &strs(&["map", "str", "map", "str", "map", "str"]),
        );

        // The Vec overload's `kind(first(x))` resolves statically to itself
        // and is a tail call; at runtime its Str argument selects the Str
        // overload. Compiled code would make that self call natively, so the
        // function must stay interpreted.
        let self_tail_call = r#"::t ns
kind fn
(x: Str): Str { `str:${x}` },
(x: Vec): Str {
    cond {
        is-vec(x) => { kind(first(x)) }
        => { "vec-overload-got-non-vec" }
    }
}
[kind(["abc"]), kind(["def"]), kind(["ghi"]), kind("jkl")]"#;
        assert_same_with_and_without_jit(
            self_tail_call,
            &strs(&["str:abc", "str:def", "str:ghi", "str:jkl"]),
        );

        // `pick` is compiled after one call; its call to `g` goes through the
        // JIT's VM trampoline, which must dispatch the same way.
        let from_compiled_caller = r#"::t ns
g fn
(x: Map): Str { "map" },
(x: Str): Str { "str" }
pick fn (v: Any): Str { g(v) }
m {a: 1}
[pick(m), pick(m), pick(m), pick(m)]"#;
        assert_same_with_and_without_jit(
            from_compiled_caller,
            &strs(&["map", "map", "map", "map"]),
        );
    }

    /// Same-arity overloads whose parameter types share a short name reach the
    /// overload of the argument's own type, whichever is declared last and
    /// however the type is written, on a direct call or as a callback. Dispatch
    /// used to compare short names, so every value tied and went to the overload
    /// declared last, and a callback didn't dispatch at all.
    #[test]
    fn same_named_parameter_types_dispatch_to_their_own_overload() {
        let src = r#"::a ns
E1 type { n: Int }
Kind enum open { Plain }

::b ns
E1 type { n: Int }
Kind enum open { Plain }

::one ns
E1 ::a/E1

::two ns
E1 ::one/E1

::imports ns
Ours ::a/E1
Theirs ::b/E1
imported fn
(x: Ours): Any { "a" },
(x: Theirs): Any { "b" }
imported-any fn (x: Any): Any { imported(x) }

::local ns
E1 type { n: Int }
mixed fn
(x: E1): Any { "local" },
(x: ::a/E1): Any { "a" },
(x: ::b/E1): Any { "b" }
mixed-any fn (x: Any): Any { mixed(x) }
make fn (): Any { E1({n: 1}) }

::c ns
::first ::a
::second ::b
a-last fn
(x: ::b/E1): Any { "b" },
(x: ::a/E1): Any { "a" }
b-last fn
(x: ::a/E1): Any { "a" },
(x: ::b/E1): Any { "b" }
aliased fn
(x: ::first/E1): Any { "a" },
(x: ::second/E1): Any { "b" }
nullable fn
(x: ::first/E1?): Any { "a" },
(x: ::second/E1?): Any { "b" }
union fn
(x: ::first/E1 | Str): Any { "a" },
(x: ::second/E1 | Int): Any { "b" }
two-args fn
(n: Int, x: ::a/E1): Any { "a" },
(n: Int, x: ::b/E1): Any { "b" }
variants fn
(x: ::a/Kind): Any { "a" },
(x: ::b/Kind): Any { "b" }
apply-all fn (x: Any): Vec {
    [
        a-last(x), b-last(x), aliased(x), nullable(x), union(x), two-args(1, x),
        ::imports/imported-any(x), ::local/mixed-any(x)
    ]
}
variants-any fn (x: Any): Any { variants(x) }
reexported fn
(x: ::two/E1): Any { "own type" },
(x: Any): Any { "any" }
reexported-any fn (x: Any): Any { reexported(x) }
by-name fn (xs: Vec): Vec { map(xs, b-last) }

::t ns
a-val ::a/E1({n: 1})
b-val ::b/E1({n: 1})
run fn (): Vec {
    from-a ::c/apply-all(a-val)
    from-b ::c/apply-all(b-val)
    named ::c/by-name([a-val, b-val, a-val])
    [
        ...from-a, ...from-b,
        ::c/variants-any(::a/Kind.Plain), ::c/variants-any(::b/Kind.Plain),
        ...named,
        ::local/mixed-any(::local/make()),
        ::c/reexported-any(a-val), ::c/reexported-any(b-val)
    ]
}
run()
run()
run()"#;
        let expected = strs(&[
            "a", "a", "a", "a", "a", "a", "a", "a", //
            "b", "b", "b", "b", "b", "b", "b", "b", //
            "a", "b", //
            "a", "b", "a", //
            "local", //
            "own type", "any",
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// An untyped (`Any`) overload is reachable whichever order it is
    /// declared in, and the most specific overload that accepts the arguments
    /// wins: a single type, then a union or nullable type, then `Any`. The
    /// untyped overload's key used to be taken over by a later same-arity
    /// overload, so `k("x")` reached `(x: Map)`.
    #[test]
    fn untyped_overload_is_reachable_and_most_specific_wins() {
        let src = r#"::t ns
k fn
(x: Any): Str { "any" },
(x: Map): Str { "map" }
k2 fn
(x: Map): Str { "map" },
(x: Any): Str { "any" }
u fn
(x: Any): Str { "any" },
(x: Int | Dec): Str { "num" }
n fn
(x: Str?): Str { "opt-str" },
(x: Any): Str { "any" }
h fn
(x: Fn): Str { "fn" },
(x: Any): Str { "any" }
w fn
(x: Map): Str { "map" },
(x: Any): Str { "any" }
Email type { value: Str }
s "hi"
m {a: 1}
i 1
f (v) { v }
e Email({value: "a@b.com"})
[
    k("x"), k(s), k(1), k({a: 1}), k(m),
    k2("x"), k2(s), k2(1), k2({a: 1}), k2(m),
    u(1), u(1.5), u(i), u("s"),
    n(null), n("a"), n(1),
    h(f), h((v) { v }), h(1),
    w(m), w(e), w(s),
]"#;
        assert_same_with_and_without_jit(
            src,
            &strs(&[
                "any", "any", "any", "map", "map", //
                "any", "any", "any", "map", "map", //
                "num", "num", "num", "any", //
                "opt-str", "opt-str", "any", //
                "fn", "fn", "any", //
                "map", "map", "any",
            ]),
        );
    }

    /// A namespaced call runs the overload of its arity. It used to run the
    /// first overload declared, whatever its arity, when the argument types
    /// weren't known: `::hot::time/PlainDate(s)` returned today's date
    /// through `PlainDate()`, and a predicate calling it read the clock.
    #[test]
    fn namespaced_calls_run_the_overload_of_their_arity() {
        let src = r#"::lib ns
Stamp type { label: Str }
fn (): Stamp { Stamp({label: "none"}) },
(label: Str): Stamp { Stamp({label: label}) }
pick fn (flag: Bool, lazy other: Any): Any { if(flag, "skipped", do(other)) }

::t ns
::time ::hot::time
::l ::lib
Day type Str where is-ok(::hot::time/PlainDate(%))
dynamic fn (x: Any): Any { x }
date-of fn (s: Str): Any { ::time/PlainDate(s) }
label-of fn (s): Str { ::lib/Stamp(s).label }
probe fn (): Vec {
    date date-of("2024-12-25")
    [::time/year(date), ::time/month(date), ::time/day(date),
     is-err(date-of("nope")),
     label-of("given"), ::l/Stamp().label,
     Day(dynamic("2024-12-25")), is-err(Day(dynamic("x"))),
     ::l/pick(true, fail("evaluated"))]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([2024, 12, 25, true, "given", "none", "2024-12-25", true, "skipped"]),
        );
    }

    /// A parameter or local binding shadows a function of the same name, for
    /// calls and references and whatever the declaration order. The compiler
    /// used to bind the name to a function registered before the body was
    /// compiled (a namespace or core function), so `before(x)` ignored the
    /// parameter while `after(x)` used it. A lazy parameter does not shadow.
    #[test]
    fn locals_shadow_functions_with_and_without_jit() {
        let src = r#"::t ns
before fn (x: Any): Int { 0 }
call-before fn (x: Int, before: Fn): Any { before(x) }
call-after fn (x: Int, after: Fn): Any { after(x) }
after fn (x: Any): Int { 0 }
local-call fn (x: Int): Int {
    before: Fn (v) { mul(v, 5) }
    before(x)
}
local-ref fn (xs: Vec): Vec {
    before: Fn (v) { mul(v, 5) }
    map(xs, before)
}
apply-first fn (xs: Vec, first: Fn): Any { first(xs) }
in-lambda fn (xs: Vec, before: Fn): Vec { map(xs, (v) { before(v) }) }
lazy-param fn (lazy before: Any): Int { before(7) }
[
    call-before(4, (v) { mul(v, 3) }),
    call-after(4, (v) { mul(v, 3) }),
    local-call(2),
    local-ref([1, 2]),
    apply-first([1, 2], (v) { "custom" }),
    in-lambda([1], (v) { add(v, 100) }),
    lazy-param(1),
    before(1),
]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::Int(12),
                Val::Int(12),
                Val::Int(10),
                Val::vec(vec![Val::Int(5), Val::Int(10)]),
                Val::from("custom"),
                Val::vec(vec![Val::Int(101)]),
                Val::Int(0),
                Val::Int(0),
            ]),
        );
    }

    /// `div` of two Ints is an Int when the quotient is whole, in compiled
    /// code as in the interpreter. The JIT used to always produce a Dec, so
    /// `eq(5, safe-divide(10, 2))` was false.
    #[test]
    fn int_division_matches_with_and_without_jit() {
        let src = r#"::t ns
direct fn (a: Int, b: Int): Int { div(a, b) }
in-branch fn (a: Int, b: Int): Any { if(eq(b, 0), 0, div(a, b)) }
safe-divide fn (a: Int, b: Int): Int {
    if(eq(b, 0), err("Division by zero"), ok(div(a, b)))
}
dec-div fn (a: Dec, b: Int): Dec { div(a, b) }
[
    Str(is-int(direct(10, 2))),
    Str(is-dec(direct(3, 2))),
    Str(is-int(in-branch(10, 2))),
    Str(is-dec(in-branch(3, 2))),
    Str(eq(5, safe-divide(10, 2))),
    Str(eq(10, mul(direct(20, 4), 2))),
    Str(is-dec(dec-div(4.0, 2))),
]"#;
        assert_same_with_and_without_jit(
            src,
            &strs(&["true", "true", "true", "true", "true", "true", "true"]),
        );
    }

    /// Calling a parameter or local binding by name (`f(x)` for `f: Fn`)
    /// calls that local in compiled code too. The JIT used to look the name
    /// up through the VM, which cannot see compiled locals (and found any
    /// unrelated function named `f` in another namespace instead).
    #[test]
    fn calls_through_local_fns_match_with_and_without_jit() {
        let src = r#"::t ns
apply-twice fn (x: Int, f: Fn): Int { f(f(x)) }
invoke fn (x: Int, f: Fn): Any { f(x) }
with-local fn (x: Int): Int {
    triple: Fn (v) { mul(v, 3) }
    triple(x)
}
sq %(mul(%, %))
[apply-twice(3, add(%, 2)), invoke(4, sq), invoke(4, (v) { mul(v, 3) }), with-local(5)]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![Val::Int(7), Val::Int(16), Val::Int(12), Val::Int(15)]),
        );
    }

    /// A null argument fails at a `Str` parameter, called directly or from a
    /// caller typed `Any`, and passes at `Str?`. `Null -> Str` is for `Str(null)`
    /// and interpolation, which still give "".
    #[test]
    fn null_fails_at_a_str_parameter_with_and_without_jit() {
        let src = r#"::t ns
f fn (x: Str): Any { [is-type(x, Null), x] }
via-any fn (a: Any): Any { f(a) }
nullable fn (x: Str?): Any { x }
via-any("warm")
via-any("warm")
via-any("warm")
[is-err(via-any(null)), is-err(f(null)), f("s"), nullable(null), Str(null), `[${null}]`]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::Bool(true),
                Val::Bool(true),
                Val::vec(vec![Val::Bool(false), Val::from("s")]),
                Val::Null,
                Val::from(""),
                Val::from("[]"),
            ]),
        );
    }

    /// A call by a name that reaches a value passes its arguments as values,
    /// even when a function of that name has lazy parameters: a parameter or
    /// local, a field of either, a piped call, and the namespace's own value.
    #[test]
    fn a_value_named_like_a_lazy_function_gets_values_with_and_without_jit() {
        let src = r#"::t ns
deferred fn (lazy x: Any): Any { "deferred" }
param-source fn (source): Any { source(null) }
param-assert fn (assert): Any { assert(null) }
param-assert-eq fn (assert-eq): Any { assert-eq(null, null) }
param-and fn (and): Any { and(null, null) }
param-or fn (or): Any { or(null, null) }
param-if fn (if): Any { if(null, null) }
param-deferred fn (deferred): Any { deferred(null) }
local-source fn (): Any {
    source (x: Str?) { "hit" }
    source(null)
}
field-of-param fn (source): Any { source.list(null) }
field-of-local fn (): Any {
    source {list: (x: Str?) { "hit" }}
    source.list(null)
}
piped fn (source): Any { null |> source() }
eager fn (source): Any {
    seen source(sub(1, 1))
    [seen]
}
one (x: Str?) { "hit" }
two (a: Str?, b: Str?) { "hit" }
int-arg (n: Int) { n }
[
    param-source(one), param-assert(one), param-assert-eq(two), param-and(two), param-or(two),
    param-if(two), param-deferred(one), local-source(), field-of-param({list: one}), field-of-local(),
    piped(one), eager(int-arg)
]"#;
        let mut expected = vec![Val::from("hit"); 11];
        expected.push(Val::vec(vec![Val::Int(0)]));
        assert_same_with_and_without_jit(src, &Val::vec(expected));
    }

    /// A namespace's own value or function of a lazy core function's name
    /// takes its arguments as values too.
    #[test]
    fn a_namespace_binding_named_like_a_lazy_core_function_gets_values() {
        let src = r#"::t ns
source (x: Str?) { "value" }
assert {check: (x: Str?) { "field" }}
assert-eq fn (a: Str?, b: Str?): Any { "function" }
[source(null), assert.check(null), assert-eq(null, null)]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::from("value"),
                Val::from("field"),
                Val::from("function"),
            ]),
        );
    }

    /// Functions with lazy parameters still defer: the core ones, a
    /// namespace's own, an import, and one defined in the body, which a local
    /// binding of the name doesn't hide.
    #[test]
    fn functions_with_lazy_parameters_still_defer_with_and_without_jit() {
        let src = r#"::t ns
deferred fn (lazy x: Any): Any { "deferred" }
imported ::t/deferred
in-body fn (): Any {
    local-lazy fn (lazy x: Any): Any { "local" }
    local-lazy(fail("never runs"))
}
[
    deferred(fail("never runs")), imported(fail("never runs")), in-body(),
    or("first", fail("never runs")), and(false, fail("never runs"))
]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::from("deferred"),
                Val::from("deferred"),
                Val::from("local"),
                Val::from("first"),
                Val::Bool(false),
            ]),
        );
    }

    /// A primitive parameter rejects a value of another type at the call, in
    /// both tiers, whether or not the JIT has seen a good value first.
    #[test]
    fn primitive_parameters_reject_other_types_with_and_without_jit() {
        let src = r#"::t ns
int fn (x: Int): Any { x }
dec fn (x: Dec): Any { x }
str fn (x: Str): Any { x }
bool fn (x: Bool): Any { x }
mapped fn (x: Map): Any { x }
vector fn (x: Vec): Any { x }
bytes fn (x: Bytes): Any { 1 }
either fn (x: Int | Dec): Any { x }
maybe fn (x: Int?): Any { x }
strs fn (x: Vec<Str>): Any { length(x) }
any fn (json: Str): Any { from-json(json) }
warm fn (): Any {
    [int(1), dec(1.5), str("s"), bool(true), mapped({}), vector([]), either(1), maybe(1)]
}
warm()
warm()
warm()
[
    is-err(int(any("\"443\""))), is-err(int(any("443.5"))), is-err(int(any("true"))),
    is-err(dec(any("\"1.5\""))), is-err(str(any("443"))), is-err(bool(any("\"x\""))),
    is-err(mapped(any("[]"))), is-err(vector(any("{}"))), is-err(bytes(any("\"x\""))),
    is-err(either(any("\"x\""))), is-err(maybe(any("\"x\""))), is-err(int(any("null"))),
    is-err(str(any("null"))), is-err(mapped(any("null"))),
    maybe(any("null")), either(any("2.0")), strs(any("[1, 2]"))
]"#;
        let mut expected = vec![Val::Bool(true); 14];
        expected.push(Val::Null);
        expected.push(Val::Dec(fastnum::D256::from(2)));
        expected.push(Val::Int(2));
        assert_same_with_and_without_jit(src, &Val::vec(expected));
    }

    /// An `Int` becomes a `Dec` and a whole `Dec` an `Int`, as at a `where`
    /// parameter, directly and when a higher-order function makes the call.
    #[test]
    fn primitive_parameters_convert_exact_numbers_with_and_without_jit() {
        let src = r#"::t ns
int fn (x: Int): Any { [is-int(x), x] }
dec fn (x: Dec): Any { [is-dec(x), x] }
maybe fn (x: Dec?): Any { is-dec(x) }
any fn (json: Str): Any { from-json(json) }
int(1)
int(1)
dec(1.5)
dec(1.5)
[int(any("5.0")), dec(any("5")), maybe(any("5")), first(map(any("[5.0]"), int)), first(map(any("[5]"), dec))]"#;
        let int_pair = Val::vec(vec![Val::Bool(true), Val::Int(5)]);
        let dec_pair = Val::vec(vec![Val::Bool(true), Val::Dec(fastnum::D256::from(5))]);
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                int_pair.clone(),
                dec_pair.clone(),
                Val::Bool(true),
                int_pair,
                dec_pair,
            ]),
        );
    }

    /// The check leaves user types alone, keeps a tagged value over an accepted
    /// kind, and doesn't read a lazy parameter or a `fn match` that takes a
    /// `Result`.
    #[test]
    fn primitive_checks_leave_other_annotations_with_and_without_jit() {
        let src = r#"::t ns
User type { id: Str }
Port type Int
user fn (x: User): Any { x }
mapped fn (x: Map): Any { 1 }
int fn (x: Int): Any { 1 }
deferred fn (lazy x: Int): Any { 1 }
kept fn match (x: Str): Any {
    Result.Err => { "err" }
    "a" => { "a" }
    _ => { "other" }
}
any fn (json: Str): Any { from-json(json) }
fails fn (): Str { err("bad") }
[user(any("\"x\"")), mapped(User({id: "a"})), int(Port(80)), deferred("not an int"), kept(fails()), kept("a")]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::from("x"),
                Val::Int(1),
                Val::Int(1),
                Val::Int(1),
                Val::from("err"),
                Val::from("a"),
            ]),
        );
    }

    /// A failure names the parameter, its function and everything a union
    /// accepts, and a variadic parameter's failure names the value's position.
    #[test]
    fn primitive_check_failures_describe_the_parameter() {
        for (call, expected) in [
            ("int(any(\"\\\"x\\\"\"))", "parameter `x` of `::t/int` expected Int, got Str"),
            ("either(any(\"\\\"x\\\"\"))", "parameter `x` of `::t/either` expected Int | Dec, got Str"),
            ("rest(1, any(\"\\\"x\\\"\"))", "parameter `xs` of `::t/rest` at `[1]` expected Int, got Str"),
        ] {
            let src = format!(
                "::t ns\nint fn (x: Int): Any {{ x }}\neither fn (x: Int | Dec): Any {{ x }}\nrest fn (...xs: Int): Any {{ xs }}\nany fn (json: Str): Any {{ from-json(json) }}\nadd({call}, 1)"
            );
            for conf in [
                crate::val!({"jit": {"threshold": 1}}),
                crate::val!({"jit": {"mode": "off"}}),
            ] {
                let error = compile_and_run_with_std_conf(&src, Some(conf.clone()))
                    .expect_err("the Err is consumed");
                assert!(error.contains(expected), "with {conf:?}: {error}");
            }
        }
    }

    /// Native self calls, self tail calls and fused pipelines check too: a
    /// compiled body can't assume a later call's argument matches the first.
    #[test]
    fn primitive_checks_cover_compiled_call_paths() {
        let src = r#"::t ns
to-any fn (x: Any): Any { x }
count fn (s: Str, n: Int): Any {
    if(eq(n, 0), s, count(to-any(n), sub(n, 1)))
}
doubled fn (x: Int): Int { mul(x, 2) }
big fn (x: Int): Bool { gt(x, 2) }
total fn (acc: Int, x: Int): Int { add(acc, x) }
pipeline fn (xs: Vec): Int {
    xs |> map(doubled) |> filter(big) |> reduce(total, 0)
}
good [1, 2, 3]
[pipeline(good), pipeline(good), pipeline(good), is-err(count("s", 1))]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::Int(10),
                Val::Int(10),
                Val::Int(10),
                Val::Bool(true),
            ]),
        );
        let bad = r#"::t ns
doubled fn (x: Int): Int { mul(x, 2) }
pipeline fn (xs: Vec): Any { xs |> map(doubled) |> reduce(add(%1, %2), 0) }
pipeline([1, 2, 3])
pipeline([1, 2, 3])
pipeline(from-json("[1, \"x\"]"))"#;
        for conf in [
            crate::val!({"jit": {"threshold": 1}}),
            crate::val!({"jit": {"mode": "off"}}),
        ] {
            let error = compile_and_run_with_std_conf(bad, Some(conf.clone()))
                .expect_err("the bad element fails");
            assert!(
                error.contains("parameter `x` of `::t/doubled` expected Int, got Str"),
                "with {conf:?}: {error}"
            );
        }
    }

    /// Overloads with a rest parameter dispatch on their arguments, whatever
    /// the number of them.
    #[test]
    fn variadic_overloads_dispatch_on_their_arguments_with_and_without_jit() {
        let src = r#"::t ns
join fn
(a: Int, ...rest: Int): Str { "ints" },
(a: Str, ...rest: Str): Str { "strs" },
(a: Vec, ...rest: Vec): Str { "vecs" }
[
    join(1), join("a"), join([]),
    join(1, 2, 3), join("a", "b", "c"), join([], [], []),
    concat("a", "b", "c", "d"), concat([1], [2], [3], [4]),
    concat("n: ", 42, ", ok: ", true), concat(1, ":", 2),
    slice([1, 2, 3], 1, 2), slice("abc", 1, 2), length([1, 2]), length("abc"),
    is-empty([]), is-empty(""), is-empty(null)
]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::from("ints"),
                Val::from("strs"),
                Val::from("vecs"),
                Val::from("ints"),
                Val::from("strs"),
                Val::from("vecs"),
                Val::from("abcd"),
                Val::vec(vec![Val::Int(1), Val::Int(2), Val::Int(3), Val::Int(4)]),
                Val::from("n: 42, ok: true"),
                Val::from("1:2"),
                Val::vec(vec![Val::Int(2)]),
                Val::from("b"),
                Val::Int(2),
                Val::Int(3),
                Val::Bool(true),
                Val::Bool(true),
                Val::Bool(true),
            ]),
        );
    }

    /// Implicit arrows fire the same way in both tiers: one involving a user
    /// type applies, a primitive to primitive one doesn't. One parameter
    /// without an arrow, such as a nullable one or a generic with a comma in
    /// its type, doesn't stop another parameter's arrow.
    #[test]
    fn implicit_arrows_match_with_and_without_jit() {
        let src = r#"::t ns
Tri type { s: Int }
Label type { s: Str }
Geom enum open { Dot }
Tri -> Geom.Tri
Label -> Str fn (l: Label): Str { l.s }
Str -> Label fn (s: Str): Label { Label({s: s}) }

kind fn match (g: Geom): Str {
    Geom.Tri => { "tri" }
    _ => { "other" }
}
take-str fn (x: Str): Any { x }
take-label fn (l: Label): Any { l.s }
typed-call fn (t: Tri): Str { kind(t) }
any-call fn (a: Any): Str { kind(a) }
any-str fn (a: Any): Any { take-str(a) }
any-label fn (a: Any): Any { take-label(a) }
through-map fn (t: Tri): Vec { map([t], kind) }
through-lambda fn (t: Tri): Str {
    call-kind (g: Geom) { kind(g) }
    call-kind(t)
}
beside-nullable fn (a: Str?, g: Geom): Str { kind(g) }
call-nullable fn (t: Tri): Str { beside-nullable("x", t) }
beside-generic fn (m: Map<Str, Int>, g: Geom, h: Geom): Str { kind(h) }
call-generic fn (t: Tri): Str { beside-generic({}, t, t) }
countdown fn cond (g: Geom, n: Int, t: Tri): Str {
    eq(n, 0) => { kind(g) }
    => {
        result countdown(t, sub(n, 1), t)
        result
    }
}
tail-countdown fn cond (g: Geom, n: Int, t: Tri): Str {
    eq(n, 0) => { kind(g) }
    => { tail-countdown(t, sub(n, 1), t) }
}
t Tri({s: 1})
run fn (): Vec {
    [
        typed-call(t), any-call(t),
        any-str(Label({s: "from-label"})), is-err(any-str(5)), is-err(any-str(true)),
        any-label("from-str"),
        call-nullable(t), call-generic(t),
        countdown(Geom.Dot, 2, t), tail-countdown(Geom.Dot, 2, t),
        through-map(t), through-lambda(t)
    ]
}
run()
run()
run()"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::from("tri"),
                Val::from("tri"),
                Val::from("from-label"),
                Val::Bool(true),
                Val::Bool(true),
                Val::from("from-str"),
                Val::from("tri"),
                Val::from("tri"),
                Val::from("tri"),
                Val::from("tri"),
                strs(&["tri"]),
                Val::from("tri"),
            ]),
        );
    }

    /// `All<Vec>` and `All<Map>` serial bodies collect their bindings, and an
    /// `All<Vec>` parallel flow collects values, in compiled code too. The JIT
    /// returned `[]` / `{}` for serial bodies and keyed a Vec by name.
    #[test]
    fn collecting_serial_and_parallel_bodies_match_with_and_without_jit() {
        let src = r#"::t ns
all-vec fn (): All<Vec> {
    a 1
    b 2
    c 3
}
all-map fn (): All<Map> {
    a 1
    b 2
}
par-vec fn (n: Int): Vec {
    data: All<Vec> parallel {
        a add(n, 1)
        b mul(n, 2)
    }
    data
}
ser-map fn (n: Int): Map {
    data: All<Map> serial {
        a add(n, 1)
        b mul(n, 2)
    }
    data
}
[all-vec(), all-map(), par-vec(3), ser-map(3)]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                Val::vec(vec![Val::Int(1), Val::Int(2), Val::Int(3)]),
                crate::val!({"a": 1, "b": 2}),
                Val::vec(vec![Val::Int(4), Val::Int(6)]),
                crate::val!({"a": 4, "b": 6}),
            ]),
        );
    }

    /// A collecting flow holds only its own bindings. A function or lambda
    /// called inside it binds its checked parameters and locals in its own
    /// frame: the interpreter added `r`, `k`, `z` and `w` to the caller's map.
    #[test]
    fn calls_add_nothing_to_a_callers_collected_flow() {
        let src = r#"::t ns
Refined type { n: Int where gt(%, 0) }
read-refined fn (r: Refined): Int { r.n }
read-positive fn (k: Int where gt(%, 0)): Int { k }
read-local fn (q: Int): Int {
    z add(q, 1)
    z
}
calls-refined fn serial (x: Refined): All<Map> {
    y read-refined(x)
    status y
}
calls-positive fn serial (x: Int): All<Map> {
    y read-positive(x)
    status y
}
calls-local fn (x: Int): All<Map> {
    y read-local(x)
    status y
}
calls-lambda fn (xs: Vec): All<Map> {
    doubled map(xs, (v) {
        w mul(v, 2)
        w
    })
    n length(doubled)
}
calls-positive-vec fn serial (x: Int): All<Vec> {
    y read-positive(x)
    status y
}
calls-in-parallel fn parallel (x: Int): All<Map> {
    a read-positive(x)
    b read-local(x)
}
[
    keys(calls-refined(Refined(n: 1))),
    keys(calls-positive(1)),
    keys(calls-local(1)),
    calls-lambda([1, 2]),
    calls-positive-vec(1),
    calls-in-parallel(1),
]"#;
        assert_same_with_and_without_jit(
            src,
            &crate::val!([
                ["y", "status"],
                ["y", "status"],
                ["y", "status"],
                {"doubled": [2, 4], "n": 2},
                [1, 1],
                {"a": 1, "b": 2},
            ]),
        );
    }

    /// `cond` and `match` collect only the first matching arm under
    /// `All<Vec>` or `All<Map>`, in compiled code too. Compiled code ran on
    /// into the later arms, so `match 1 { 1 => ... _ => ... }` gave both.
    #[test]
    fn collecting_cond_and_match_stop_at_the_first_match() {
        let src = r#"::t ns
first-vec fn (x: Int): Vec {
    v: All<Vec> match x {
        1 => { `one ${x}` }
        2 => { {two: x} }
        _ => { "other" }
    }
    v
}
first-map fn (x: Int): Map {
    v: All<Map> match x {
        1 => { `one ${x}` }
        _ => { "other" }
    }
    v
}
first-cond fn (x: Int): Vec {
    label `n${x}`
    v: All<Vec> cond {
        gt(x, 10) => { concat(label, " big") }
        gt(x, 0) => { concat(label, " positive") }
        => { concat(label, " other") }
    }
    v
}
body-match fn (x: Int): All<Vec> {
    match x {
        1 => { "one" }
        _ => { "other" }
    }
}
[
    map(range(0, 4), first-vec),
    first-map(1),
    first-map(5),
    map([20, 3, -1], first-cond),
    [body-match(1), body-match(2)],
]"#;
        assert_same_with_and_without_jit(
            src,
            &crate::val!([
                [["other"], ["one 1"], [{"two": 2}], ["other"]],
                {"1": "one 1"},
                {"_": "other"},
                [["n20 big"], ["n3 positive"], ["n-1 other"]],
                [["one"], ["other"]],
            ]),
        );
    }

    /// In a flow function's body, a line that pipes a name (`x |> add(1)`,
    /// `st.a |> add(1)`) is an expression, not a binding of that name. It
    /// was a parse error ("Expected value") after the name.
    #[test]
    fn flow_body_line_can_pipe_a_name() {
        let src = r#"::t ns
inc fn serial (x: Int): Int { x |> add(1) }
both fn serial (x: Int): All<Vec> {
    a x
    a |> add(1)
}
field fn serial (x: Int): Int {
    st {a: x}
    st.a |> mul(3)
}
fan fn parallel (x: Int): All<Vec> {
    x |> add(1)
    x |> mul(3)
}
[inc(1), both(1), field(2), fan(2)]"#;
        assert_same_with_and_without_jit(src, &crate::val!([2, [1, 2], 6, [3, 6]]));
    }

    /// `All<Map>` on a pipe keys each stage by position, with keys that show
    /// when the map is printed. They were `$pipe_0`, ..., which read as type
    /// tags and printed as `{}`, and compiled code keyed every stage
    /// `$pipe_0`, keeping only the last.
    #[test]
    fn pipe_all_map_keys_every_stage() {
        let src = r#"::t ns
stages fn (x: Int): Map {
    m: All<Map> x |> add(2) |> mul(3)
    m
}
staged fn (x: Int): All<Map> { x |> add(1) }
[map([5, 6], stages), Str(stages(5)), staged(1)]"#;
        assert_same_with_and_without_jit(
            src,
            &crate::val!([
                [
                    {"pipe_0": 5, "pipe_1": 7, "pipe_2": 21},
                    {"pipe_0": 6, "pipe_1": 8, "pipe_2": 24},
                ],
                "{pipe_0: 5, pipe_1: 7, pipe_2: 21}",
                {"pipe_0": 1, "pipe_1": 2},
            ]),
        );
    }

    /// A plain `fn` body that is one flow takes an `All` return annotation
    /// on that flow, as a binding of it does: a pipe collects its stages.
    /// The annotation used to shape the implicit serial body, which binds
    /// nothing, so these returned `[]`.
    #[test]
    fn single_flow_fn_body_takes_the_all_annotation() {
        let src = r#"::t ns
pipe-trace fn (x: Int): All<Vec> { x |> add(1) |> mul(2) |> sub(3) }
pipe-lines fn (x: Int): All<Vec> {
    x
        |> add(1)
        |> mul(2)
}
checks fn (x: Int): All<Vec> {
    cond-all {
        gt(x, 0) => a { "positive" }
        gt(x, 10) => b { "big" }
        => c { "any" }
    }
}
both fn (x: Int): All<Vec> {
    parallel {
        a add(x, 1)
        b add(x, 2)
    }
}
both-named fn (x: Int): All {
    parallel {
        a add(x, 1)
        b add(x, 2)
    }
}
bindings fn (x: Int): All<Vec> {
    a x
    x |> add(1)
}
[pipe-trace(5), pipe-lines(1), checks(5), both(1), both-named(1), bindings(1)]"#;
        assert_same_with_and_without_jit(
            src,
            &crate::val!([
                [5, 6, 12, 9],
                [1, 2, 4],
                ["positive", "any"],
                [2, 3],
                {"a": 2, "b": 3},
                [1],
            ]),
        );
    }

    /// A `fail()` inside a `pmap` callback stays a halt when the caller is
    /// compiled, so containment returns the worker's `Failure`. The JIT
    /// boundary used to clear the re-raised halt and pass on a plain error
    /// string instead.
    #[test]
    fn pmap_halt_is_contained_with_and_without_jit() {
        let src = r#"::t ns
pmap-halt fn (): Vec<Int> {
    inputs ::hot::coll/map(range(1, 41), (x: Int) { x })
    ::hot::coll/pmap(inputs, (x: Int) {
        cond {
            eq(x, 7) => { fail("pmap boom on 7") }
            => { mul(x, 10) }
        }
    })
}
attempt ::hot::internal::exec/contain(pmap-halt, [])
failure if-err(attempt, (failure) { failure })
[Str(is-err(attempt)), failure.msg]"#;
        assert_same_with_and_without_jit(src, &strs(&["true", "pmap boom on 7"]));
    }

    /// Compiled `match-all` and `cond-all` flows build their result maps with
    /// the same branch keys as the interpreter. The JIT used to keep the
    /// compiler's internal `\0c<id>` suffix, so `get(result, "Critter.Bird")`
    /// found nothing.
    #[test]
    fn collect_all_map_keys_match_with_and_without_jit() {
        let src = r#"::t ns
Critter enum open {
    Dog,
    Cat
}
Avian type {
    species: Str,
    wingspan: Dec
}
Avian -> Critter.Bird
categorize fn match-all (a: Critter): All<Map> {
    Critter.Bird => { "flies" }
    Critter.Dog => { "walks" }
}
sizes fn cond-all (n: Int): All<Map> {
    gt(n, 0) => positive { "positive" }
    gt(n, 10) => big { "big" }
}
[
    keys(categorize(Critter.Bird({species: "Eagle", wingspan: 2.1}))),
    keys(categorize(Critter.Dog)),
    keys(sizes(20)),
]"#;
        assert_same_with_and_without_jit(
            src,
            &Val::vec(vec![
                strs(&["Critter.Bird"]),
                strs(&["Critter.Dog"]),
                strs(&["positive", "big"]),
            ]),
        );
    }

    /// Core types keep their conversion constructors: a Map argument is not
    /// routed to a default Map constructor. `Map` is qualified because hot-std
    /// also defines a core `::hot::store/Map` type.
    #[test]
    fn core_type_constructors_still_convert_map_args() {
        let src = r#"::t ns
m {a: 1}
[Str(m), to-json(::hot::type/Map(m)), to-json(Vec(m)), Str(is-err(Int(m))), Str(is-err(Uuid(m)))]"#;
        assert_same_with_and_without_jit(
            src,
            &strs(&["{a: 1}", r#"{"a":1}"#, r#"[["a",1]]"#, "true", "true"]),
        );
    }

    #[test]
    fn named_arguments_build_records_and_variants() {
        let src = r#"::t ns
User type { name: Str, email: Str, age: Int? }
Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }
Bird type { species: Str }
Animal enum open { Dog }
Bird -> Animal.Bird
Handler type { on-value: Fn }
Address type { city: Str }
Person type { name: Str, address: Address }
Window type { start: Int, finish: Int } where lte(%.start, %.finish)

probe fn (): Vec {
    a User(name: "Ada", email: "ada@example.com")
    b User({name: "Ada", email: "ada@example.com"})
    c User(
        age: 30,
        email: "bo@example.com",
        name: "Bo",
    )
    circle Shape.Circle(radius: 2.5)
    bird Animal.Bird(species: "Eagle")
    handler Handler(on-value: add(%, 1))
    on-value handler.on-value
    person Person(name: "Cy", address: {city: "Paris"})
    qualified ::t/User(name: "Di", email: "di@example.com")
    ::here ::t
    aliased ::here/User(name: "Ev", email: "ev@example.com")
    Local type { n: Int }
    local Local(n: 7)
    names map(["x", "y"], User(name: %, email: "e@example.com")) |> map(%.name)
    [eq(a, b), is-type(a, User), a.age, c.age, c.name,
     eq(circle.radius, 2.5), is-type(circle, Shape), bird.species, on-value(1),
     is-type(person.address, Address), person.address.city, qualified.name, local.n,
     names, is-err(Window(start: 5, finish: add(0, 1))), aliased.name]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &crate::val!([
                true,
                true,
                null,
                30,
                "Bo",
                true,
                true,
                "Eagle",
                2,
                true,
                "Paris",
                "Di",
                7,
                ["x", "y"],
                true,
                "Ev"
            ]),
        );
    }

    /// An enum from another namespace can be named in full, as
    /// `::geo/Shape.Circle`, or through a namespace alias, without importing
    /// it: to construct a variant with a Map or named arguments, as a unit
    /// variant's value, in `match`, bound to a name, and as a variant's
    /// payload type (`Ring(::geo/Round)`). A unit variant bound at the top of
    /// a namespace is its value; a variant with a payload is its constructor.
    #[test]
    fn qualified_enum_variants_construct_and_match() {
        let src = r#"::geo ns
Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }
Round type { radius: Dec }
Bird type { species: Str }
Animal enum open { Dog }
Bird -> Animal.Bird

::geo::forms ns
::gt ::geo
Form enum { Ring(::geo/Round), Disk(::gt/Round), Flat }

::t ns
::g ::geo
::f ::geo::forms
Nothing ::geo/Shape.Dot
Round ::geo/Shape.Circle

describe fn (s: ::geo/Shape): Str {
    match s {
        ::geo/Shape.Circle => { `circle ${s.radius}` }
        ::geo/Shape.Dot => { "dot" }
    }
}

probe fn (): Vec {
    mapped ::geo/Shape.Circle({radius: 1.0})
    named ::g/Shape.Circle(radius: 2.0)
    dot ::geo/Shape.Dot
    make ::geo/Shape.Circle
    [mapped.radius, named.radius, is-type(named, ::geo/Shape),
     is-type(dot, ::geo/Shape), eq(dot, ::g/Shape.Dot), eq(dot, Nothing),
     describe(named), describe(dot), describe(Nothing),
     make({radius: 3.0}).radius, Round({radius: 4.0}).radius,
     ::geo/Animal.Bird(species: "Eagle").species, is-type(::geo/Animal.Dog, ::geo/Animal),
     ::f/Form.Ring(radius: 5.0).radius, ::geo::forms/Form.Disk({radius: 6.0}).radius,
     is-type(::f/Form.Flat, ::f/Form)]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([
                1.0, 2.0, true, true, true, true, "circle 2.0", "dot", "dot", 3.0, 4.0, "Eagle",
                true, 5.0, 6.0, true
            ]),
        );
    }

    /// A variant's payload can be any type expression. Its data is checked as
    /// a record field of that type would be: `Vec<Ask>` builds each item as
    /// an `Ask` and fails on an item that isn't one.
    #[test]
    fn enum_variants_carry_type_expression_payloads() {
        let src = r#"::t ns
Ask type { key: Str }
Status enum { One(Ask), Many(Vec<Ask>), Counts(Map<Str, Int>), Maybe(Ask?), Either(Ask | Str), Done }
Steps type
  | Each(Vec<Ask>)
  | Stop

describe fn match (s: Status): Str {
    Status.One => { "one" }
    Status.Many => { "many" }
    Status.Counts => { "counts" }
    Status.Maybe => { "maybe" }
    Status.Either => { "either" }
    Status.Done => { "done" }
}

probe fn (): Vec {
    many Status.Many([Ask(key: "a"), {key: "b"}])
    [is-type(many, Status), describe(many), map(untype(many).Many, %.key),
     is-err(Status.Many([{nope: 1}])),
     get(untype(Status.Counts({a: 1})).Counts, "a"), describe(Status.Counts({a: 1})),
     is-type(Status.Maybe(null), Status), Status.Maybe({key: "c"}).key,
     untype(Status.Either("s")).Either, Status.Either(Ask(key: "d")).key,
     map(untype(Steps.Each([{key: "e"}])).Each, %.key)]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([
                true,
                "many",
                ["a", "b"],
                true,
                1,
                "counts",
                true,
                "c",
                "s",
                "d",
                ["e"]
            ]),
        );
    }

    /// A refinement or literal union as an enum payload, named or an inline
    /// `where`, checks the payload as a record field of that type would.
    #[test]
    fn enum_payloads_check_their_refinements() {
        let src = r#"::t ns
Positive type Int where gt(%, 0)
Fruit type "apple" | "banana"
Due enum { Hours(Positive), Days(Int where gt(%, 0)), Pick(Fruit), Many(Vec<Positive>) }
dynamic fn (x: Any): Any { x }
probe fn (): Vec {
    [is-err(Due.Hours(dynamic(0))), untype(Due.Hours(dynamic(2))).Hours,
     is-err(Due.Days(dynamic(-1))), untype(Due.Days(dynamic(3))).Days,
     is-err(Due.Pick(dynamic("pear"))), untype(Due.Pick(dynamic("apple"))).Pick,
     is-err(Due.Many(dynamic([1, 0]))), untype(Due.Many(dynamic([1, 2]))).Many]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([true, 2, true, 3, true, "apple", true, [1, 2]]),
        );
    }

    /// A record field or enum payload whose type declares a constructor for
    /// the value's type builds through it, as a call would: a time type
    /// parses a Str, and a bad Str is an Err. A field used to tag the Str
    /// as it was, so `found-at: "nonsense"` passed and a ZonedDateTime field
    /// read as 1970. A constructor that doesn't convert (`Uuid(7)` makes a
    /// new id) is not called on the value.
    #[test]
    fn typed_fields_build_through_the_constructor_for_their_value() {
        let src = r#"::t ns
::time ::hot::time
Form type { found-at: ::time/PlainDate, seen: ::time/PlainDate?, days: Vec<::time/PlainDate> }
Policy type { effective: ::time/ZonedDateTime }
Found enum { At(::time/PlainDate) }
Keyed type { id: Uuid }
dynamic fn (x: Any): Any { x }
probe fn (): Vec {
    form Form({found-at: dynamic("2026-01-02"), seen: null, days: ["2026-01-03"]})
    named Form(found-at: "2026-02-03", seen: "2026-02-04", days: [])
    policy Policy({effective: dynamic("2027-01-01T05:00:00Z[UTC]")})
    id Uuid(7)
    [is-type(form.found-at, ::time/PlainDate), Str(form.found-at), is-null(form.seen),
     map(form.days, Str(%)), Str(named.seen),
     ::time/epoch-millis(::time/to-instant(policy.effective)),
     is-err(Form({found-at: dynamic("nonsense"), seen: null, days: []})),
     is-err(Form({found-at: "2026-01-02", seen: dynamic("nope"), days: []})),
     is-err(Form({found-at: "2026-01-02", seen: null, days: dynamic(["nope"])})),
     is-err(Policy({effective: dynamic("2027-01-01")})),
     get(untype(Found.At(dynamic("2026-05-06"))).At, "day"),
     is-err(Found.At(dynamic("nope"))),
     eq(Str(Keyed({id: id}).id), Str(id))]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([
                true,
                "2026-01-02",
                true,
                ["2026-01-03"],
                "2026-02-04",
                1_798_779_600_000_i64,
                true,
                true,
                true,
                true,
                6,
                true,
                true
            ]),
        );
    }

    /// An arrow may name its enum by a namespace alias or in full. The
    /// checker used to read `::k/Kind.Extra` as a type of its own, so the
    /// arrow failed with "Implementation body returns ::geo/Kind, but should
    /// return ::geo/Kind.Extra", and the compiler did not resolve the alias,
    /// so no constructor was emitted for the variant.
    #[test]
    fn arrows_name_their_enum_by_alias_or_in_full() {
        let src = r#"::geo ns
Kind enum open { A }

::ext ns
::k ::geo
Extra type { n: Int }
Extra -> ::k/Kind.Extra
Full type { s: Str }
Full -> ::geo/Kind.Full

::t ns
::k ::geo
label fn match (x: ::k/Kind): Str {
    ::k/Kind.A => { "a" }
    ::k/Kind.Extra => { Str(x.n) }
    ::k/Kind.Full => { x.s }
    _ => { "other" }
}
[label(::k/Kind.Extra({n: 2})), label(::geo/Kind.Full(s: "f")), label(::k/Kind.A)]"#;
        let expected = strs(&["2", "f", "a"]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// Two namespaces may each declare a same-named type and an arrow to it
    /// from one source. Each arrow answers for its own target, whether called
    /// as a constructor or implicitly at a parameter, and whichever namespace
    /// loads last. The registry used to key targets by short name, so the last
    /// arrow declared ran for both, and a redacting arrow never ran.
    #[test]
    fn same_named_arrow_targets_keep_to_their_namespaces() {
        let src = r#"::src ns
Payload type { secret: Str }
Tri type { s: Int }

::a ns
Payload ::src/Payload
Tri ::src/Tri
Fail type { from: Str }
Geom enum open { Dot }
Payload -> Fail fn (p: Payload): Fail { Fail({from: "a"}) }
Tri -> Geom.Tri
build fn (p: Any): Any { Fail(p) }
take fn (f: Fail): Any { f.from }
through-any fn (x: Any): Any { take(x) }
shape fn match (g: Geom): Str {
    Geom.Tri => { "a tri" }
    _ => { "a other" }
}
shape-any fn (x: Any): Str { shape(x) }

::b ns
Payload ::src/Payload
Tri ::src/Tri
Fail type { from: Str }
Geom enum open { Dot }
Payload -> Fail fn (p: Payload): Fail { Fail({from: "b"}) }
Tri -> Geom.Tri
build fn (p: Any): Any { Fail(p) }
take fn (f: Fail): Any { f.from }
through-any fn (x: Any): Any { take(x) }
shape fn match (g: Geom): Str {
    Geom.Tri => { "b tri" }
    _ => { "b other" }
}
shape-any fn (x: Any): Str { shape(x) }

::imports ns
Fail ::a/Fail
Other ::b/Fail
take fn (f: Fail): Any { f.from }
take-other fn (f: Other): Any { f.from }
through-any fn (x: Any): Any { take(x) }
through-any-other fn (x: Any): Any { take-other(x) }

::t ns
p ::src/Payload({secret: "s"})
t ::src/Tri({s: 1})
run fn (): Vec {
    [
        ::a/build(p).from, ::b/build(p).from,
        ::a/through-any(p), ::b/through-any(p),
        ::a/shape-any(t), ::b/shape-any(t),
        ::imports/through-any(p), ::imports/through-any-other(p),
        is-type(::a/build(p), ::a/Fail), is-type(::b/build(p), ::b/Fail),
        is-type(::a/build(p), ::b/Fail)
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("a"),
            Val::from("b"),
            Val::from("a"),
            Val::from("b"),
            Val::from("a tri"),
            Val::from("b tri"),
            Val::from("a"),
            Val::from("b"),
            Val::Bool(true),
            Val::Bool(true),
            Val::Bool(false),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// A parameter's type written in full, through a namespace alias, or by a
    /// namespace that only imports it gets the implicit arrow as a short name
    /// does, and same-named targets stay apart. The signature lookup read a
    /// slash in the type as the end of the function's name, and the alias
    /// existed only at compile time, so the function received the value
    /// unconverted.
    #[test]
    fn qualified_and_aliased_parameter_types_get_the_implicit_arrow() {
        let src = r#"::src ns
Payload type { secret: Str }

::b::api ns
Payload ::src/Payload
E1 type { from: Str }
Kind enum open { Plain }
Payload -> E1 fn (p: Payload): E1 { E1({from: "b"}) }
Payload -> Kind.Wrapped

::other ns
Payload ::src/Payload
E1 type { from: Str }
Payload -> E1 fn (p: Payload): E1 { E1({from: "other"}) }

::reexport ns
E1 ::b::api/E1

::c ns
::alias ::b::api
::second ::other
::again ::reexport
take-qualified fn (x: ::b::api/E1): Any { x.from }
take-alias fn (x: ::alias/E1): Any { x.from }
take-second fn (x: ::second/E1): Any { x.from }
take-reexport fn (x: ::reexport/E1): Any { x.from }
take-alias-reexport fn (x: ::again/E1): Any { x.from }
take-pair fn (n: Int, x: ::alias/E1, s: Str): Any { `${n} ${x.from} ${s}` }
take-enum fn match (x: ::alias/Kind): Str {
    ::alias/Kind.Wrapped => { "wrapped" }
    _ => { "other" }
}
take-nested fn (x: ::alias/E1): Any {
    inner fn (y: ::alias/E1): Any { y.from }
    inner(x)
}
qualified-any fn (x: Any): Any { take-qualified(x) }
alias-any fn (x: Any): Any { take-alias(x) }
second-any fn (x: Any): Any { take-second(x) }
reexport-any fn (x: Any): Any { take-reexport(x) }
alias-reexport-any fn (x: Any): Any { take-alias-reexport(x) }
pair-any fn (x: Any): Any { take-pair(1, x, "s") }
enum-any fn (x: Any): Any { take-enum(x) }
nested-any fn (x: Any): Any { take-nested(x) }

::t ns
p ::src/Payload({secret: "s"})
already ::b::api/E1({from: "kept"})
run fn (): Vec {
    [
        ::c/qualified-any(p), ::c/alias-any(p), ::c/second-any(p),
        ::c/reexport-any(p), ::c/alias-reexport-any(p),
        ::c/pair-any(p), ::c/enum-any(p), ::c/nested-any(p),
        ::c/qualified-any(already), ::c/alias-any(already)
    ]
}
run()
run()
run()"#;
        let expected = strs(&[
            "b", "b", "other", "b", "b", "1 b s", "wrapped", "b", "kept", "kept",
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// Runtime overload dispatch can pick a sibling whose custom parameter type
    /// is an arrow's target, and that sibling gets the converted value as a
    /// lone function does, whichever overload is declared last and however the
    /// type is written. A sibling that declares the argument's own type still
    /// takes it unconverted. Dispatch used to skip coercion altogether.
    #[test]
    fn dispatched_overloads_get_the_implicit_arrow() {
        let src = r#"::src ns
Payload type { secret: Str }

::b ns
Payload ::src/Payload
E1 type { from: Str }
Payload -> E1 fn (p: Payload): E1 { E1({from: "arrow"}) }

::c ns
::alias ::b
::payloads ::src
E1 ::b/E1
Payload ::src/Payload
arrow-first fn
(x: E1): Any { x.from },
(x: Str): Any { "str" }
arrow-last fn
(x: Str): Any { "str" },
(x: E1): Any { x.from }
qualified fn
(x: ::b/E1): Any { x.from },
(x: Str): Any { "str" }
aliased fn
(x: ::alias/E1): Any { x.from },
(x: Str): Any { "str" }
two-args fn
(n: Int, x: ::alias/E1): Any { `${n} ${x.from}` },
(n: Int, x: Str): Any { "str" }
own-type-first fn
(x: Payload): Any { "own" },
(x: E1): Any { x.from },
(x: Str): Any { "str" }
own-type-qualified fn
(x: ::payloads/Payload): Any { "own" },
(x: E1): Any { x.from },
(x: Str): Any { "str" }
tail fn
(x: ::alias/E1): Any { x.from },
(x: Vec): Any { tail(first(x)) }
apply-all fn (x: Any): Vec {
    [
        arrow-first(x), arrow-last(x), qualified(x), aliased(x),
        two-args(1, x), own-type-first(x), own-type-qualified(x)
    ]
}
tail-any fn (x: Any): Any { tail(x) }

::t ns
p ::src/Payload({secret: "s"})
already ::b/E1({from: "kept"})
run fn (): Vec {
    with-payload ::c/apply-all(p)
    with-str ::c/apply-all("text")
    [
        ...with-payload,
        ...with-str,
        ::c/arrow-first(already), ::c/aliased(already),
        ::c/tail-any([p])
    ]
}
run()
run()
run()"#;
        let expected = strs(&[
            "arrow", "arrow", "arrow", "arrow", "1 arrow", "own", "own",
            "str", "str", "str", "str", "str", "str", "str",
            "kept", "kept",
            "arrow",
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// A parameter type can name one variant, as `Kind.Plain` or `::a/Kind.Plain`,
    /// through an alias or a re-export too. Overloads dispatch on it, a sibling
    /// declaring the enum takes the other variants, and an arrow to the variant
    /// runs. Same-named enums in two namespaces stay apart.
    #[test]
    fn parameter_types_can_name_an_enum_variant() {
        let src = r#"::a ns
Inner type { text: Str }
Kind enum open { Plain, Other, Wrapped(Inner) }
Source type { text: Str }
Source -> Kind.Sourced fn (s: Source): Kind { Kind.Wrapped({text: `from ${s.text}`}) }
short fn
(x: Kind.Plain): Any { "plain" },
(x: Kind.Wrapped): Any { `wrapped ${x.text}` },
(x: Kind): Any { "kind" }
returns fn (): Kind.Plain { Kind.Plain }
Holder type { kind: Kind.Plain }

::b ns
Kind enum { Plain, Other }

::reexport ns
Kind ::a/Kind

::c ns
::alias ::a
::other ::b
qualified fn
(x: ::a/Kind.Plain): Any { "a plain" },
(x: ::b/Kind.Plain): Any { "b plain" },
(x: ::alias/Kind.Wrapped): Any { `a wrapped ${x.text}` },
(x: ::reexport/Kind.Other): Any { "a other" },
(x: ::other/Kind.Other): Any { "b other" }
variant-first fn
(x: ::a/Kind.Plain): Any { "plain" },
(x: ::a/Kind): Any { "kind" }
enum-first fn
(x: ::alias/Kind): Any { "kind" },
(x: ::alias/Kind.Plain): Any { "plain" }
nullable fn (x: ::a/Kind.Plain?): Any { if(is-null(x), "none", "plain") }
either fn (x: ::a/Kind.Plain | ::a/Kind.Other): Any { "either" }
lone fn (x: ::a/Kind.Plain): Any { "lone" }
arrow-target fn (x: ::a/Kind.Sourced): Any { `sourced ${x.text}` }
hold fn (x: Any): Any { x }

::t ns
run fn (): Vec {
    plain ::c/hold(::a/Kind.Plain)
    wrapped ::c/hold(::a/Kind.Wrapped({text: "w"}))
    other ::c/hold(::a/Kind.Other)
    [
        ::a/short(plain), ::a/short(wrapped), ::a/short(other),
        ::c/qualified(plain), ::c/qualified(::c/hold(::b/Kind.Plain)),
        ::c/qualified(wrapped), ::c/qualified(other),
        ::c/qualified(::c/hold(::b/Kind.Other)),
        ::c/qualified(::a/Kind.Plain), ::c/qualified(::b/Kind.Other), ::a/short(::a/Kind.Other),
        ::c/variant-first(plain), ::c/variant-first(other),
        ::c/enum-first(plain), ::c/enum-first(other),
        ::c/nullable(plain), ::c/nullable(null), ::c/either(other), ::c/lone(plain),
        ::c/arrow-target(::c/hold(::a/Source({text: "s"}))),
        first(map([wrapped], ::a/short)), first(map([plain], ::c/qualified)),
        is-type(::a/returns(), ::a/Kind),
        is-type(::a/Holder(kind: ::a/Kind.Plain).kind, ::a/Kind)
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("plain"),
            Val::from("wrapped w"),
            Val::from("kind"),
            Val::from("a plain"),
            Val::from("b plain"),
            Val::from("a wrapped w"),
            Val::from("a other"),
            Val::from("b other"),
            Val::from("a plain"),
            Val::from("b other"),
            Val::from("kind"),
            Val::from("plain"),
            Val::from("kind"),
            Val::from("plain"),
            Val::from("kind"),
            Val::from("plain"),
            Val::from("none"),
            Val::from("either"),
            Val::from("lone"),
            Val::from("sourced from s"),
            Val::from("wrapped w"),
            Val::from("a plain"),
            Val::Bool(true),
            Val::Bool(true),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// A namespace offers what it imports under its own name: its type's
    /// constructor, a variant, a function, and a function reference, through an
    /// alias or another re-export too. Its own definition of a name still wins.
    #[test]
    fn calls_reach_types_and_functions_through_a_reexport() {
        let src = r#"::b::api ns
E1 type { from: Str }
Kind enum { Plain, Wrapped(E1) }
describe fn (e: E1): Any { `described ${e.from}` }
own fn (): Any { "api" }

::reexport ns
E1 ::b::api/E1
Kind ::b::api/Kind
describe ::b::api/describe

::again ns
::up ::reexport
E1 ::up/E1
Kind ::up/Kind
describe ::up/describe

::shadowing ns
own fn (): Any { "shadowing" }

::t ns
::alias ::again
E1 ::again/E1
take fn (x: ::reexport/E1): Any { x.from }
run fn (): Vec {
    direct ::reexport/E1({from: "direct"})
    named E1(from: "named")
    chained ::again/E1(from: "chained")
    aliased ::alias/E1({from: "aliased"})
    [
        direct.from, named.from, chained.from, aliased.from,
        is-type(direct, ::b::api/E1),
        eq(::reexport/Kind.Plain, ::b::api/Kind.Plain),
        ::alias/Kind.Wrapped({from: "wrapped"}).from,
        ::reexport/describe(direct), ::alias/describe(chained),
        first(map([{from: "mapped"}], ::reexport/E1)).from,
        first(map([direct], ::alias/describe)),
        take(chained),
        ::shadowing/own()
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("direct"),
            Val::from("named"),
            Val::from("chained"),
            Val::from("aliased"),
            Val::Bool(true),
            Val::Bool(true),
            Val::from("wrapped"),
            Val::from("described direct"),
            Val::from("described chained"),
            Val::from("mapped"),
            Val::from("described direct"),
            Val::from("chained"),
            Val::from("shadowing"),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// An overload set routes a tagged argument to the overload it has an arrow
    /// to, built-in parameter types included, whichever overload is declared
    /// last. Overloads that take the argument as it is still win, and an
    /// unrelated custom type doesn't get in the way. Calls and `map` callbacks
    /// dispatch alike.
    #[test]
    fn overload_dispatch_routes_through_arrows() {
        let src = r#"::a ns
Label type { text: Str }
Other type { n: Int }
Point type { x: Int }
Tag type { name: Str }
Label -> Str fn (l: Label): Str { l.text }
Point -> Vec fn (p: Point): Vec { [p.x] }
Label -> Tag fn (l: Label): Tag { Tag({name: l.text}) }
str-first fn
(s: Str): Any { `str ${s}` },
(v: Vec): Any { "vec" }
str-last fn
(v: Vec): Any { "vec" },
(s: Str): Any { `str ${s}` }
past-others fn
(i: Int): Any { "int" },
(s: Str): Any { `str ${s}` },
(v: Vec): Any { "vec" },
(b: Bool): Any { "bool" }
two-args fn
(n: Int, s: Str): Any { `${n} ${s}` },
(n: Int, v: Vec): Any { "vec" }
past-custom fn
(s: Str): Any { `str ${s}` },
(o: Other): Any { "other" }
custom-target fn
(t: Tag): Any { `tag ${t.name}` },
(o: Other): Any { "other" }
own-type fn
(s: Str): Any { `str ${s}` },
(l: Label): Any { "own" },
(v: Vec): Any { "vec" }
any-sibling fn
(s: Str): Any { `str ${s}` },
(x: Any): Any { "any" },
(v: Vec): Any { "vec" }
union-sibling fn
(u: Label | Int): Any { "union" },
(s: Str): Any { `str ${s}` }
lone fn (s: Str): Any { `str ${s}` }

::t ns
l ::a/Label({text: "l"})
hold fn (x: Any): Any { x }
run fn (): Vec {
    label hold(l)
    point hold(::a/Point({x: 1}))
    by-value ::a/str-first
    callbacks flatten([
        map([label], ::a/str-first), map([label], ::a/str-last),
        map([label], ::a/past-custom), map(["s", label], ::a/past-others),
        map([label], ::a/lone), [by-value(label)], [call("::a/str-last", [label])]
    ])
    [
        ::a/str-first(label), ::a/str-last(label), ::a/past-others(label),
        ::a/two-args(1, label), ::a/past-custom(label), ::a/custom-target(label),
        ::a/own-type(label), ::a/any-sibling(label), ::a/union-sibling(label),
        ::a/str-first(point), ::a/str-first("s"), ::a/str-last([1]),
        ...callbacks
    ]
}
run()
run()
run()"#;
        let expected = strs(&[
            "str l", "str l", "str l", "1 l", "str l", "tag l", "own", "any", "union",
            "vec", "str s", "vec",
            "str l", "str l", "str l", "str s", "str l", "str l", "str l", "str l",
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// Arrows that reach two overloads don't choose between them: the call stays
    /// on the overload it resolved to, whose parameter check then reports the
    /// mismatch. Neither does an argument route where its arrow can't run: at a
    /// nullable type, or a built-in value to a built-in type.
    #[test]
    fn overload_dispatch_does_not_guess_between_arrows() {
        let src = r#"::a ns
Label type { text: Str }
Label -> Str fn (l: Label): Str { l.text }
Label -> Vec fn (l: Label): Vec { [l.text] }
two fn
(s: Str): Any { "str" },
(v: Vec): Any { "vec" },
(i: Int): Any { "int" }
keeps-resolved fn
(s: Str): Any { "str" },
(v: Vec): Any { "vec" }
nullable fn
(s: Str?): Any { "str" },
(i: Int): Any { "int" }
builtin-value fn
(s: Str): Any { "str" },
(v: Vec): Any { "vec" }

::t ns
l ::a/Label({text: "l"})
hold fn (x: Any): Any { x }
describe fn (r: Result): Any { if-err(r, (e) { e.message }) }
run fn (): Vec {
    label hold(l)
    [
        describe(::a/two(label)),
        ::a/keeps-resolved(label),
        describe(::a/nullable(label)),
        describe(::a/builtin-value(hold(1)))
    ]
}
run()
run()
run()"#;
        let expected = strs(&[
            "parameter `i` of `::a/two` expected Int, got Label",
            "vec",
            "parameter `i` of `::a/nullable` expected Int, got Label",
            "parameter `v` of `::a/builtin-value` expected Vec, got Int",
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// An arrow's failure halts the call to a dispatched overload, as it does
    /// for a lone function, on a call and on a self tail call.
    #[test]
    fn dispatched_overload_arrow_failure_halts() {
        let declarations = r#"::t ns
Bad type { n: Int }
E1 type { from: Str }
Bad -> E1 fn (b: Bad): E1 { err("arrow failed") }
take fn
(x: E1): Any { x.from },
(x: Str): Any { "str" }
tail fn
(x: E1): Any { x.from },
(x: Vec): Any { tail(first(x)) }
call-any fn (x: Any): Any { take(x) }
tail-any fn (x: Any): Any { tail(x) }
"#;
        for call in ["call-any(Bad({n: 1}))", "tail-any([Bad({n: 1})])"] {
            let src = format!("{declarations}{call}");
            for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
                let err = compile_and_run_project_with_std_conf(&src, conf)
                    .expect_err("the arrow's failure halts");
                assert!(err.contains("arrow failed"), "{call}: {err}");
            }
        }
    }

    /// A function's tail call to itself gets its implicit arrows as any call
    /// does, in both tiers, and goes on looping: a compiled loop that nested a
    /// call per conversion would hit the recursion limit here. The self call
    /// used to pass the value unconverted.
    #[test]
    fn self_tail_calls_get_the_implicit_arrow() {
        let src = r#"::t ns
Payload type { secret: Str }
Label type { text: Str }
Count type { n: Int }
E1 type { from: Str }
E2 type { from: Str }
Payload -> E1 fn (p: Payload): E1 { E1({from: "e1"}) }
Payload -> E2 fn (p: Payload): E2 { E2({from: "e2"}) }
Label -> Str fn (l: Label): Str { l.text }
Count -> Int fn (c: Count): Int { c.n }
walk fn (x: E1, n: Int, q: Any): Any {
    if(eq(n, 0), x.from, walk(q, sub(n, 1), q))
}
walk-cond fn cond (x: E1, n: Int, q: Any): Any {
    eq(n, 0) => { x.from }
    => { walk-cond(q, sub(n, 1), q) }
}
two fn (x: E1, y: E2, n: Int, q: Any): Any {
    if(eq(n, 0), `${x.from} ${y.from}`, two(q, q, sub(n, 1), q))
}
to-str fn (s: Str, n: Int, q: Any): Any {
    if(eq(n, 0), s, to-str(q, sub(n, 1), q))
}
to-int fn (i: Int, n: Int, q: Any): Any {
    if(eq(n, 0), i, to-int(q, sub(n, 1), q))
}
same-object fn (x: E1, n: Int): Any {
    if(eq(n, 0), x.from, same-object(x, sub(n, 1)))
}
wrapped fn (x: Any): Any { x }
through-ok fn (x: E1, n: Int, q: Any): Any {
    if(eq(n, 0), x.from, through-ok(wrapped(q), sub(n, 1), q))
}
run fn (): Vec {
    p Payload({secret: "s"})
    start E1({from: "start"})
    [
        walk(start, 10000, p), walk-cond(start, 10000, p), walk(start, 0, p),
        two(start, E2({from: "other"}), 3, p),
        to-str("start", 3, Label({text: "label"})),
        to-int(0, 3, Count({n: 7})),
        same-object(start, 1000),
        through-ok(start, 3, p)
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("e1"),
            Val::from("e1"),
            Val::from("start"),
            Val::from("e1 e2"),
            Val::from("label"),
            Val::Int(7),
            Val::from("start"),
            Val::from("e1"),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// A parameter's built-in type is checked after its arrow has run: a self
    /// tail call that converts the value passes the check, one with no arrow
    /// for the value fails it, in both tiers and on the first iteration or a
    /// later one.
    #[test]
    fn self_tail_call_arrows_run_before_the_parameter_check() {
        let src = r#"::t ns
Label type { text: Str }
Count type { n: Int }
Plain type { n: Int }
Label -> Str fn (l: Label): Str { l.text }
Count -> Int fn (c: Count): Int { c.n }
to-str fn (s: Str, n: Int, q: Any): Any {
    if(eq(n, 0), s, to-str(q, sub(n, 1), q))
}
to-int fn (i: Int, n: Int, q: Any): Any {
    if(eq(n, 0), i, to-int(q, sub(n, 1), q))
}
to-dec fn (d: Dec, n: Int, q: Any): Any {
    if(eq(n, 0), d, to-dec(q, sub(n, 1), q))
}
overloaded fn
(v: Vec, n: Int, q: Any): Any { "vec" },
(s: Str, n: Int, q: Any): Any { if(eq(n, 0), s, overloaded(q, sub(n, 1), q)) }
run fn (): Vec {
    plain Plain({n: 1})
    failed-str to-str("start", 3, plain)
    failed-int to-int(0, 3, plain)
    failed-overload overloaded("start", 3, plain)
    [
        to-str("start", 3, Label({text: "label"})),
        to-int(0, 3, Count({n: 7})),
        to-dec(0.5, 3, 2),
        overloaded("start", 3, Label({text: "label"})),
        is-err(failed-str), is-err(failed-int), is-err(failed-overload)
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("label"),
            Val::Int(7),
            Val::Dec(fastnum::D256::from(2)),
            Val::from("label"),
            Val::Bool(true),
            Val::Bool(true),
            Val::Bool(true),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// An arrow's failure on a self tail call halts the run, in both tiers:
    /// an Err once it's consumed, as on any call, and a `fail` at once, after
    /// an earlier argument's arrow has run.
    #[test]
    fn self_tail_call_arrow_failure_halts() {
        let declarations = r#"::t ns
Good type { n: Int }
Bad type { n: Int }
E1 type { from: Str }
E2 type { from: Str }
Good -> E1 fn (g: Good): E1 { E1({from: "good"}) }
Bad -> E1 fn (b: Bad): E1 { err("arrow failed") }
Bad -> E2 fn (b: Bad): E2 { fail("arrow failed") }
walk fn (x: E1, n: Int, q: Any): Any {
    if(eq(n, 0), x.from, walk(q, sub(n, 1), q))
}
two fn (x: E1, y: E2, n: Int, p: Any, q: Any): Any {
    if(eq(n, 0), x.from, two(p, q, sub(n, 1), p, q))
}
"#;
        for call in [
            r#"walk(E1({from: "start"}), 2, Bad({n: 1}))"#,
            r#"two(E1({from: "a"}), E2({from: "b"}), 2, Good({n: 1}), Bad({n: 1}))"#,
        ] {
            let src = format!("{declarations}{call}");
            for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
                let err = compile_and_run_project_with_std_conf(&src, conf)
                    .expect_err("the arrow's failure halts");
                assert!(err.contains("arrow failed"), "{call}: {err}");
            }
        }
    }

    /// An arrow's source is its qualified type too: a value doesn't run the
    /// arrow declared for a same-named type in another namespace, and a local
    /// type named like a core one has arrows of its own, wherever it's declared.
    #[test]
    fn same_named_arrow_sources_keep_to_their_namespaces() {
        let src = r#"::a ns
Secret type { value: Str }
Out type { kind: Str }
Secret -> Out fn (s: Secret): Out { Out({kind: "from a"}) }
Secret -> Str fn (s: Secret): Str { "core str" }
build fn (x: Any): Any { Out(x) }

::b ns
Secret type { kind: Str }
make fn (): Any { Secret({kind: "copied"}) }

::c ns
Reduced type { text: Str }
Holder type { n: Int }
Holder -> Reduced fn (h: Holder): Reduced { Reduced({text: "local type"}) }
local fn (h: Any): Any { Reduced(h).text }

::t ns
run fn (): Vec {
    [
        ::a/build(::a/Secret({value: "v"})).kind,
        ::a/build(::b/make()).kind,
        Str(::a/Secret({value: "v"})),
        ::c/local(::c/Holder({n: 1}))
    ]
}
run()
run()
run()"#;
        let expected = Val::vec(vec![
            Val::from("from a"),
            Val::from("copied"),
            Val::from("core str"),
            Val::from("local type"),
        ]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// A type from another namespace takes named arguments when it is
    /// imported in a body, as `Sh ::geo/Shape`, as well as at the top of the
    /// namespace. The local import names the type it is bound to, even over
    /// a type of the same name, rather than shadowing types as a local value.
    #[test]
    fn named_arguments_build_variants_of_imported_enums() {
        let src = r#"::geo ns
Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }
Bird type { species: Str }
Animal enum open { Dog }
Bird -> Animal.Bird
Label type { text: Str }

::t ns
Shape ::geo/Shape
::g ::geo
Label type { name: Str }

probe fn (): Vec {
    imported Shape.Circle(radius: 1.0)
    Sh ::geo/Shape
    local Sh.Circle(radius: 2.0)
    mapped Sh.Circle({radius: 3.0})
    Fauna ::g/Animal
    enrolled Fauna.Bird(species: "Eagle")
    outer Label(name: "outer")
    Label ::geo/Label
    inner Label(text: "inner")
    [imported.radius, local.radius, mapped.radius, is-type(local, ::geo/Shape),
     enrolled.species, outer.name, inner.text]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!([1.0, 2.0, 3.0, true, "Eagle", "outer", "inner"]),
        );
    }

    /// At the top of a namespace, the project pipeline points a reference to
    /// an import at what it imports. A reference with a path keeps it, so
    /// `Shape.Circle(...)` on an imported enum builds the variant, by name or
    /// from a Map, and `config.port` on an imported value reads the field.
    /// The rewrite used to drop the path, leaving `::geo/Shape(...)`.
    #[test]
    fn imported_enum_variants_at_namespace_scope() {
        let src = r#"::geo ns
Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }
config {port: 8080}

::t ns
Shape ::geo/Shape
config ::geo/config
named Shape.Circle(radius: 1.0)
mapped Shape.Circle({radius: 2.0})
dot Shape.Dot
port config.port
[named.radius, mapped.radius, is-type(named, ::geo/Shape), is-type(dot, ::geo/Shape), port]"#;
        let expected = val!([1.0, 2.0, true, true, 8080]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// Variants that arrows add to a core open enum construct and match from
    /// any namespace, whether the arrow is in the enum's namespace or in one
    /// before or after it. Another namespace builds the core enum from its
    /// declaration, which used to leave the arrow variants out, so
    /// `Heard.PeriodEnded(...)` failed there with "does not have variant".
    #[test]
    fn arrow_variants_of_core_enums_construct_in_other_namespaces() {
        let src = r#"::rl::a-ext ns
Missed type { by: Str }
Missed -> Heard.Missed

::rl::core ns
Heard meta {core: true} enum open { Changed, Deadline }
PeriodEnded type { period: Str }
PeriodEnded -> Heard.PeriodEnded
q3 Heard.PeriodEnded(period: "Q3")

::rl::z-ext ns
Lapsed type { days: Int }
Lapsed -> Heard.Lapsed

::rl::p ns
why fn match (h: Heard): Str {
    Heard.Changed => { "changed" }
    Heard.PeriodEnded => { h.period }
    Heard.Missed => { h.by }
    Heard.Lapsed => { Str(h.days) }
    _ => { "other" }
}

::rl::t ns
top Heard.PeriodEnded({period: "Q4"})
in-body fn (): Vec {
    [
        ::rl::p/why(Heard.PeriodEnded(period: "Q1")),
        ::rl::p/why(Heard.Missed({by: "ops"})),
        ::rl::p/why(Heard.Lapsed(days: 3)),
    ]
}
concat([::rl::p/why(::rl::core/q3), ::rl::p/why(top)], in-body(), [::rl::p/why(Heard.Changed)])"#;
        let expected = strs(&["Q3", "Q4", "Q1", "ops", "3", "changed"]);
        for conf in [None, Some(val!({"jit": {"threshold": 1}}))] {
            let out = compile_and_run_project_with_std_conf(src, conf)
                .unwrap_or_else(|e| panic!("run failed: {e}"));
            assert_eq!(out, expected);
        }
    }

    /// In a match whose arms name `Result`, the other arms test the value an
    /// `Ok` holds and never match an `Err`; `Any` and `_` see the Result.
    /// Those arms used to test the Result itself, so `"a" =>` never matched
    /// `Ok("a")`.
    #[test]
    fn match_arms_beside_result_arms_test_the_ok_value() {
        let src = r#"::t ns
S type "a" | "b" | "c"
get-s fn (x: Str): S { if(eq(x, "bad"), err("a"), x) }
pick fn (x: Str): Str {
    match get-s(x) {
        Result.Err => { "err" }
        "a" => { "A" }
        S (s) => { `S ${s}` }
        _ => { "default" }
    }
}
held fn (): Str {
    match get-s("bad") {
        "a" => { "held a" }
        Result.Ok => { "ok" }
        Any => { "any" }
    }
}
[pick("a"), pick("b"), pick("bad"), held()]"#;
        assert_same_with_and_without_jit(src, &strs(&["A", "S b", "err", "any"]));
    }

    /// A runtime error that a native meets while running Hot code reaches a
    /// containing boundary with its message. The halt used to carry `null`,
    /// so `hot test` printed "Test execution error: " and nothing more.
    #[test]
    fn contained_runtime_error_keeps_its_message() {
        let src = r#"::t ns
Color enum { Red, Green }
bogus fn (): Vec {
    c Color
    map([1], (x) { c.Blue })
}
attempt ::hot::internal::exec/contain(bogus, [])
[is-err(attempt), contains(err-message(attempt), "Type '::t/Color' does not have variant 'Blue'")]"#;
        assert_same_with_and_without_jit(src, &val!([true, true]));
    }

    /// Like the Map form, a named call on a type with custom constructors goes
    /// to the default constructor, which still checks `where` constraints.
    /// A type declared twice takes names in either order, and a one-parameter
    /// constructor taking `Any` leaves the Map to the default constructor.
    #[test]
    fn named_arguments_build_types_with_custom_constructors() {
        let src = r#"::t ns
Tag type { name: Str }
fn (name: Str): Tag { Tag({name: `ctor:${name}`}) }
Spot type { x: Int, y: Int }
Spot type fn (x: Int, y: Int): Spot { Spot({x: mul(x, 10), y}) }
Pair type fn (a: Int, b: Int): Pair { Pair({a: mul(a, 10), b}) }
Pair type { a: Int, b: Int }
Box type { value: Any }
fn (v: Any): Box { Box({value: [v]}) }
Age type { years: Int where gte(%, 0) }
fn (years: Int): Age { Age({years: add(years, 1)}) }

probe fn (): Vec {
    tag Tag(name: "a")
    spot Spot(x: 1, y: 2)
    pair Pair(a: 1, b: 2)
    box Box(value: 3)
    [tag.name, Tag("b").name, is-type(tag, Tag), spot.x, Spot(1, 2).x, pair.a,
     Pair(1, 2).a, box.value, Box(3).value, is-err(Age(years: -1))]
}
probe()
probe()"#;
        assert_same_with_and_without_jit(
            src,
            &val!(["a", "ctor:b", true, 1, 10, 1, 10, 3, [3], true]),
        );
    }

    #[test]
    fn named_arguments_must_fit_the_callee() {
        let types = r#"::t ns
User type { name: Str, email: Str, age: Int? }
Shape enum { Circle(Circle), Dot }
Circle type { radius: Dec }
Point type fn (x: Int, y: Int): Point { Point({x, y}) }
Temp type { celsius: Dec }
fn (m: Map): Temp { Temp({celsius: m.c}) }
Box type { value: Any }
fn (v: Str | Map): Box { Box({value: [v]}) }
greet fn (name: Str): Str { name }
"#;
        for (call, message) in [
            (
                r#"User(nme: "Ada", email: "a@b.c")"#,
                "`User` has no field `nme`. Did you mean `name`?",
            ),
            (r#"User(name: "Ada")"#, "Missing required field `email`."),
            (r#"User(age: 3)"#, "Missing required fields `name`, `email`."),
            (r#"User("Ada", email: "a@b.c")"#, "Pass every field by name"),
            (r#"greet(name: "Ada")"#, "`greet` isn't a record type."),
            (r#"Point(x: 1, y: 2)"#, "`Point` has no fields to name."),
            (
                r#"Temp(celsius: 1.0)"#,
                "`Temp` has a constructor that takes one Map, `(m: Map)`",
            ),
            (r#"Box(value: 1)"#, "takes one Map, `(v: Str | Map)`"),
            (r#"Shape(radius: 1.0)"#, "`Shape` is an enum."),
            (r#"Shape.Dot(radius: 1.0)"#, "`Shape.Dot` carries no data."),
            (r#"Shape.Square(side: 1.0)"#, "`Shape` has no variant `Square`."),
            (
                r#"::t/Shape.Square(side: 1.0)"#,
                "`::t/Shape` has no variant `Square`.",
            ),
            (r#"::t/Shape.Dot(radius: 1.0)"#, "`::t/Shape.Dot` carries no data."),
            (
                r#""Ada" |> User(email: "a@b.c")"#,
                "A piped value can't fill a named field.",
            ),
        ] {
            let src = format!("{types}probe fn () {{ {call} }}\nprobe()");
            let err = compile_and_run_with_std(&src).expect_err(call);
            assert!(err.contains(message), "{call}: {err}");
            assert!(err.contains("NamedArgument"), "{call}: {err}");
        }
    }

    /// A parameter or local named like a type shadows it, so a call by that
    /// name goes to the local value and takes no named arguments. A
    /// qualified call, a lazy parameter, and a call before the local is bound
    /// still build the type.
    #[test]
    fn named_arguments_respect_locals_that_shadow_a_type() {
        let types = "::t ns\nShape type { radius: Dec }\n";
        for body in [
            "draw fn (Shape: Fn) { Shape(radius: 1.0) }\ndraw((m) { m })",
            "probe fn () {\n    Shape (m) { m }\n    Shape(radius: 1.0)\n}\nprobe()",
            "map([1], (Shape) { Shape(radius: 1.0) })",
        ] {
            let err = compile_and_run_with_std(&format!("{types}{body}")).expect_err(body);
            assert!(err.contains("`Shape` is a local value here"), "{body}: {err}");
            assert!(err.contains("NamedArgument"), "{body}: {err}");
        }
        let src = [
            types,
            r#"draw fn (Shape: Fn) { ::t/Shape(radius: 1.0) }
late fn (lazy Shape: Any) { Shape(radius: 2.0) }
probe fn () {
    before Shape(radius: 3.0)
    Shape (m) { m }
    before
}
check fn (s: Any, radius: Dec): Vec { [is-type(s, ::t/Shape), eq(s.radius, radius)] }
[check(draw((m) { m }), 1.0), check(late(1), 2.0), check(probe(), 3.0)]"#,
        ]
        .concat();
        assert_same_with_and_without_jit(
            &src,
            &val!([[true, true], [true, true], [true, true]]),
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
        let expected = Val::vec(vec![
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
                    .is_err_and(|e| e.contains("first requires a collection")
                        || e.contains("of `::hot::coll/first` expected Str | Vec | Map")),
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
        let expected = Val::vec(vec![
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
        run_with_std(source, conf, false, false)
    }

    /// Like [`compile_and_run_with_std_conf`], but the VM also holds the
    /// program's AST, as it does when the CLI runs a project.
    fn compile_and_run_with_std_ast(source: &str) -> Result<Val, String> {
        run_with_std(source, None, false, true)
    }

    /// Like [`compile_and_run_with_std_conf`], with the project pipeline's
    /// namespace-import pass (`resolve_all_variable_references`), which
    /// rewrites namespace-scope references to imports.
    fn compile_and_run_project_with_std_conf(
        source: &str,
        conf: Option<Val>,
    ) -> Result<Val, String> {
        run_with_std(source, conf, true, false)
    }

    fn run_with_std(
        source: &str,
        conf: Option<Val>,
        resolve_imports: bool,
        with_ast: bool,
    ) -> Result<Val, String> {
        run_with_std_edited(source, conf, resolve_imports, with_ast, |_| {})
    }

    /// Like [`run_with_std`], with `edit` applied to the compiled program.
    fn run_with_std_edited(
        source: &str,
        conf: Option<Val>,
        resolve_imports: bool,
        with_ast: bool,
        edit: impl FnOnce(&mut crate::lang::bytecode::BytecodeProgram),
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
        if resolve_imports {
            crate::lang::compiler::resolver::resolve_all_variable_references(
                &mut combined_program,
            );
        }

        compiler
            .compile_program(&mut combined_program)
            .map_err(|e| format!("Compile error: {:?}", e))?;

        let hot_ast = with_ast
            .then(|| Arc::new(crate::lang::ast::HotAst::from_program(combined_program)));
        let mut program = compiler.get_program_arc();
        edit(Arc::make_mut(&mut program));
        let mut vm = VirtualMachine::new(
            program,
            hot_ast,
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

    /// A compiled self tail call loops, without leaking what each call made,
    /// whatever its arguments: numeric ops whose Errs halt at the op or are
    /// kept as values, a call in between, owned temporaries and locals made
    /// before or inside the flow.
    #[test]
    fn compiled_tail_calls_loop_whatever_their_arguments() {
        let src = r#"::t ns
sum-three fn (i: Int, acc: Int): Int {
    if(lte(i, 0), acc, sum-three(sub(i, 1), add(add(acc, i), 1)))
}
mixed fn (i: Int, acc: Int, m: Int): Int {
    if(lte(i, 0), acc, mixed(sub(i, 1), add(mod(acc, m), i), m))
}
score fn (i: Int): Int { mul(mod(i, 7), 3) }
with-call fn (i: Int, acc: Int): Int {
    if(lte(i, 0), acc, with-call(sub(i, 1), add(acc, score(i))))
}
pre-local fn (i: Int, acc: Int): Int {
    s `x${i}`
    if(lte(i, 0), acc, pre-local(sub(i, 1), add(acc, length(s))))
}
nested fn (i: Int, acc: Int): Int {
    cond {
        lte(i, 0) => acc
        => {
            t `z${i}`
            cond {
                eq(mod(i, 2), 0) => nested(sub(i, 1), add(acc, length(t)))
                => nested(sub(i, 1), add(mod(acc, 1000003), 1))
            }
        }
    }
}
str-acc fn (i: Int, acc: Str): Str {
    if(lte(i, 0), acc, str-acc(sub(i, 1), if(eq(mod(i, 2), 0), acc, `${acc}`)))
}
[
    sum-three(100000, 0), mixed(100000, 0, 7), with-call(100000, 0),
    pre-local(100000, 0), nested(100000, 0), str-acc(100000, "s"),
]"#;
        let expected = val!([5000150000_i64, 1, 900000, 588895, 344450, "s"]);
        for conf in [
            val!({"jit": {"mode": "disabled"}}),
            val!({"jit": {"threshold": 1}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone())).expect("run");
            assert_eq!(out, expected, "{conf:?}");
        }
    }

    /// A capture a local function couldn't resolve when it was created (a
    /// binding made after it) comes from the call that created it, while
    /// that call runs, and never from the frames of a later caller.
    #[test]
    fn unresolved_capture_comes_from_the_definer_only() {
        let in_definer = r#"::t ns
make fn () {
    h fn () { `${later} via h` }
    later "definer later"
    h()
}
caller fn () {
    later "caller later"
    make()
}
caller()"#;
        let escaped = r#"::t ns
make fn () {
    h fn () { `${later} via h` }
    later "definer later"
    h
}
caller fn () {
    later "caller later"
    f make()
    f()
}
caller()"#;
        for conf in [
            val!({"jit": {"mode": "disabled"}}),
            val!({"jit": {"threshold": 1}}),
        ] {
            let out = compile_and_run_with_std_conf(in_definer, Some(conf.clone())).expect("run");
            assert_eq!(out, Val::from("definer later via h"), "{conf:?}");
            let err = compile_and_run_with_std_conf(escaped, Some(conf.clone()))
                .expect_err("the definer's binding is gone");
            assert!(err.contains("Variable 'later' not found"), "{conf:?}: {err}");
        }
    }

    /// Overload dispatch chooses on the arguments as passed, then prepares
    /// them for the overload it calls: a sibling that declares a `Result`
    /// receives an Err, and an Ok is matched by its value. A sibling whose
    /// parameter is eager never receives a thunk built for a lazy one.
    #[test]
    fn overload_dispatch_prepares_args_for_the_overload_called() {
        let src = r#"::t ns
f fn
(x: Result): Str { if(is-err(x), "result-err", "result-ok") },
(x: Str): Str { "str" }
h fn
(x: Fn): Str { "fn" },
(lazy x: Int): Str { `lazy ${do x}` }
probe fn (): Str {
    r err("boom")
    o ok("hi")
    s "hi"
    `${f(r)} ${f(o)} ${f(s)} ${h(add(1, 2))}`
}
probe()"#;
        for conf in [
            val!({"jit": {"mode": "disabled"}}),
            val!({"jit": {"threshold": 1}}),
        ] {
            let out = compile_and_run_with_std_conf(src, Some(conf.clone())).expect("run");
            assert_eq!(out, Val::from("result-err str str lazy 3"), "{conf:?}");
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
