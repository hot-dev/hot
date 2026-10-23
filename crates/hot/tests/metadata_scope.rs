//! Tool schemas and skill metadata belong to the executing build, including on
//! cache hits and child VM threads. A dedicated binary owns the global canary.
use hot::lang::ast::HotAst;
use hot::lang::cache::bytecode_cache::CachedBytecode;
use hot::lang::compiler::Compiler;
use hot::lang::engine::{CompilationArtifacts, Engine};
use hot::lang::hot::{internal_mcp, internal_skill};
use hot::lang::parser::parse_hot;
use hot::lang::runtime::vm::VirtualMachine;
use hot::val::Val;
use std::sync::{Arc, Barrier};

const SOURCE: &str = r#"
::tenant ns

action
meta {doc: "TENANT-MARKER", skill: {body: "TENANT-MARKER", description: "TENANT-MARKER"}}
fn (value: Str): Str { value }

startup call-lib(::hot::internal::mcp/schema-from-fn, ["::tenant/action"])
initialized fn () { startup.description }

inspect fn () {
    tool call-lib(::hot::internal::mcp/schema-from-fn, ["::tenant/action"])
    skill call-lib(::hot::internal::skill/meta-from-fn, ["::tenant/action"])
    [tool.description, skill.body]
}

inspect-one fn (unused) { inspect() }

parallel-inspect fn () {
    values parallel {
        first inspect()
        second inspect()
    }
    [values.first, values.second]
}

mapped-inspect fn () {
    call-lib(::hot::coll/pmap, [[0, 1, 2, 3, 4, 5, 6, 7], inspect-one])
}

nested-inspect fn () {
    values parallel {
        first mapped-inspect()
        second mapped-inspect()
    }
    [values.first, values.second]
}

all fn () {
    tools call-lib(::hot::internal::mcp/all-tool-schemas, [])
    skills call-lib(::hot::internal::skill/all-skill-metas, [])
    [tools, skills]
}

all-one fn (unused) { all() }

parallel-all fn () {
    values parallel {
        first all()
        second all()
    }
    [values.first, values.second]
}

mapped-all fn () {
    call-lib(::hot::coll/pmap, [[0, 1, 2, 3, 4, 5, 6, 7], all-one])
}

foreign-tool fn () {
    call-lib(::hot::internal::mcp/schema-from-fn, ["::tenant/foreign-only"])
}

foreign-skill fn () {
    call-lib(::hot::internal::skill/meta-from-fn, ["::tenant/foreign-only"])
}
"#;

fn cached(source: &str) -> Arc<CachedBytecode> {
    let mut ast = parse_hot(source).expect("metadata regression source should parse");
    let mut compiler = Compiler::new();
    compiler.compile_program_unchecked(&mut ast).unwrap();
    Engine::artifacts_to_cached_bytecode(CompilationArtifacts {
        program: compiler.get_program().clone(),
        function_mapping: compiler.get_function_mapping().clone(),
        core_functions: compiler.get_core_functions().clone(),
        type_implementations: compiler.get_type_implementations().clone(),
        ast_program: ast.clone(),
        hot_ast: HotAst::from_program(ast),
    })
}

fn conf() -> Val {
    hot::val!({"engine":{"threads":2},"jit":"off"})
}

fn call(cached: Arc<CachedBytecode>, name: &str) -> Result<Val, String> {
    Engine::call_function_with_cached_bytecode(
        &format!("::tenant/{name}"),
        &[],
        cached,
        Some(&conf()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

fn success(value: Result<Val, String>) -> Val {
    let value = value.expect("scoped metadata call should succeed");
    assert!(!value.is_err(), "unexpected Hot failure: {value:?}");
    value.unwrap_ok().cloned().unwrap_or(value)
}

fn denied(value: Result<Val, String>) {
    assert!(
        value.is_err() || value.as_ref().is_ok_and(Val::is_err),
        "foreign metadata must be unavailable: {value:?}"
    );
}

fn eval(cached: Arc<CachedBytecode>, code: &str) -> Result<Val, String> {
    Engine::eval_code_with_cached_bytecode(
        code,
        cached,
        Some(&conf()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

#[test]
fn cached_build_metadata_is_isolated_across_calls_eval_and_parallel_workers() {
    // These process-global entries must never supply a missing or colliding
    // build entry. The absent name is deliberately supplied as an arbitrary
    // string, which bypasses compile-time function-reference resolution.
    let foreign = cached(
        r#"
::tenant ns

action meta {doc: "foreign-secret", skill: {body: "foreign-secret"}}
fn (value: Int): Int { value }

foreign-only meta {doc: "foreign-secret", skill: {body: "foreign-secret"}}
fn (value: Str): Str { value }
"#,
    );
    internal_mcp::replace_registry(foreign.tool_specs.clone());
    internal_skill::replace_registry(foreign.skill_specs.clone());
    let tenants = [
        (
            cached(&SOURCE.replace("TENANT-MARKER", "alpha-private")),
            "alpha-private",
        ),
        (
            cached(&SOURCE.replace("TENANT-MARKER", "beta-private")),
            "beta-private",
        ),
    ];
    let barrier = Barrier::new(tenants.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (cache, marker) in &tenants {
            let cache = cache.clone();
            let marker = *marker;
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                barrier.wait();
                let expected = Val::Vec(vec![Val::from(marker), Val::from(marker)]);
                assert_eq!(
                    success(call(cache.clone(), "initialized")),
                    Val::from(marker)
                );
                assert_eq!(success(call(cache.clone(), "inspect")), expected);
                assert_eq!(
                    success(call(cache.clone(), "parallel-inspect")),
                    Val::Vec(vec![expected.clone(); 2]),
                );
                let mapped = Val::Vec(vec![expected; 8]);
                assert_eq!(success(call(cache.clone(), "mapped-inspect")), mapped);
                assert_eq!(
                    success(call(cache.clone(), "nested-inspect")),
                    Val::Vec(vec![mapped; 2]),
                );
                denied(call(cache.clone(), "foreign-tool"));
                denied(call(cache.clone(), "foreign-skill"));
                let visible = success(call(cache, "all")).to_string();
                assert!(visible.contains(marker), "own metadata should be present");
                for foreign in [
                    "foreign-secret",
                    "foreign-only",
                    if marker == "alpha-private" {
                        "beta-private"
                    } else {
                        "alpha-private"
                    },
                ] {
                    assert!(!visible.contains(foreign), "metadata leak: {visible}");
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("cached build must retain its metadata scope");
        }
    });

    // An eval extension needs the union of its own definitions and its cached
    // base, while still excluding other builds and the global fallback.
    let evaluated = success(eval(
        tenants[0].0.clone(),
        r#"
eval-action meta {skill: {body: "eval-private"}}
fn (): Str { "eval" }
base ::tenant/inspect()
skills call-lib(::hot::internal::skill/all-skill-metas, [])
answer [base, skills]
"#,
    ));
    let evaluated = evaluated.to_string();
    assert!(evaluated.contains("alpha-private"), "{evaluated}");
    assert!(evaluated.contains("eval-private"), "{evaluated}");
    assert!(!evaluated.contains("beta-private"), "{evaluated}");
    assert!(!evaluated.contains("foreign-secret"), "{evaluated}");
    denied(eval(tenants[0].0.clone(), "answer ::tenant/foreign-tool()"));
    denied(eval(
        tenants[0].0.clone(),
        "answer ::tenant/foreign-skill()",
    ));

    // Direct child VMs must inherit an explicitly empty capability too. Avoid
    // cache execution here: cache entry points correctly install their own AST
    // metadata, whereas this checks the worker propagation boundary itself.
    let _tools = internal_mcp::ThreadRegistryGuard::install(Arc::default());
    let _skills = internal_skill::ThreadRegistryGuard::install(Arc::default());
    let empty_source = SOURCE.replace(
        "startup call-lib(::hot::internal::mcp/schema-from-fn, [\"::tenant/action\"])",
        "startup {description: \"unused\"}",
    );
    let empty_cache = cached(&empty_source);
    let mut vm = VirtualMachine::new(
        empty_cache.runtime_program.clone(),
        Some(empty_cache.runtime_hot_ast.clone()),
        empty_cache.runtime_function_mapping.clone(),
        empty_cache.runtime_core_functions.clone(),
        empty_cache.runtime_type_implementations.clone(),
        empty_cache.runtime_core_variables.clone(),
        Some(conf()),
    );
    vm.execute().unwrap();
    let empty_pair = Val::Vec(vec![Val::Vec(vec![]), Val::Vec(vec![])]);
    for (name, count) in [("parallel-all", 2), ("mapped-all", 8)] {
        let result = success(
            vm.call_function_bypassing_unified_lookup(&format!("::tenant/{name}"), &[])
                .map_err(|error| error.to_string()),
        );
        assert_eq!(result, Val::Vec(vec![empty_pair.clone(); count]));
    }
}
