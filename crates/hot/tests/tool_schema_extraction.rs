//! Integration tests for the runtime tool-schema registry powered by
//! `Compiler::extract_tool_schemas` and consumed by
//! `::hot::internal::mcp/schema-from-fn`.
//!
//! These tests parse a tiny Hot program, run the compiler's tool-schema
//! extraction step, then call the `schema_from_fn` Hot binding directly
//! to verify that the registry was populated correctly and is reachable
//! through the same path Hot code would use at runtime.

use hot::lang::compiler::Compiler;
use hot::lang::hot::internal_mcp;
use hot::lang::hot::r#type::HotResult;
use hot::lang::parser;
use hot::lang::runtime::function_ref::function_ref;
use hot::val::Val;
use parking_lot::Mutex;

// The internal_mcp registry is process-global; serialize tests that
// touch it so concurrent execution doesn't cause flakes.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn compile_and_extract(source: &str) {
    let mut program = parser::parse_hot(source).expect("parse");
    let mut compiler = Compiler::new();
    // Use the unchecked path: these test fixtures are minimal Hot
    // programs without `hot-std` loaded, so they cannot satisfy name
    // resolution. Schema extraction walks the AST directly and does
    // not need the validation passes.
    compiler
        .compile_program_unchecked(&mut program)
        .expect("compile");
    compiler
        .extract_tool_specs(&program)
        .expect("extract_tool_specs");
}

#[test]
fn test_schema_from_fn_typed_args() {
    let _g = TEST_LOCK.lock();
    internal_mcp::clear_registry();

    let src = r#"
::demo ns

search meta {doc: "Search the index for matching docs."}
fn (query: Str, limit: Int): Vec<Str> {
    [query]
}
"#;
    compile_and_extract(src);

    let result = internal_mcp::schema_from_fn(&[function_ref("::demo/search".to_string())]);
    let map = match result {
        HotResult::Ok(Val::Map(m)) => m,
        other => panic!("expected Ok(Map), got: {:?}", other),
    };

    assert_eq!(
        map.get(&Val::from("name")),
        Some(&Val::from("::demo/search"))
    );
    assert_eq!(
        map.get(&Val::from("description")),
        Some(&Val::from("Search the index for matching docs."))
    );

    let input_schema = map
        .get(&Val::from("input-schema"))
        .expect("input-schema present");
    let Val::Map(input) = input_schema else {
        panic!("input-schema not a map: {:?}", input_schema);
    };
    assert_eq!(input.get(&Val::from("type")), Some(&Val::from("object")));
    let Some(Val::Map(props)) = input.get(&Val::from("properties")) else {
        panic!("properties missing or not a map");
    };
    assert!(props.contains_key(&Val::from("query")));
    assert!(props.contains_key(&Val::from("limit")));

    let output_schema = map
        .get(&Val::from("output-schema"))
        .expect("output-schema present");
    assert!(matches!(output_schema, Val::Map(_)));

    internal_mcp::clear_registry();
}

#[test]
fn test_schema_from_fn_unknown_function_returns_err() {
    let _g = TEST_LOCK.lock();
    internal_mcp::clear_registry();

    // Compile something unrelated so the registry is non-empty but
    // does not contain the function we ask for.
    let src = r#"
::demo ns

other fn (x: Int): Int { x }
"#;
    compile_and_extract(src);

    match internal_mcp::schema_from_fn(&[function_ref("::demo/missing".to_string())]) {
        HotResult::Err(_) => {}
        other => panic!("expected Err for unknown fn, got: {:?}", other),
    }

    internal_mcp::clear_registry();
}

#[test]
fn test_all_tool_schemas_lists_all_compiled_fns() {
    let _g = TEST_LOCK.lock();
    internal_mcp::clear_registry();

    let src = r#"
::demo ns

a fn (x: Int): Int { x }
b fn (y: Str): Str { y }
"#;
    compile_and_extract(src);

    let v = match internal_mcp::all_tool_schemas(&[]) {
        HotResult::Ok(Val::Vec(v)) => v,
        other => panic!("expected Ok(Vec), got: {:?}", other),
    };
    let names: Vec<String> = v
        .iter()
        .filter_map(|val| match val {
            Val::Map(m) => m.get(&Val::from("name")).and_then(|n| match n {
                Val::Str(s) => Some((**s).to_string()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n == "::demo/a"), "names: {:?}", names);
    assert!(names.iter().any(|n| n == "::demo/b"), "names: {:?}", names);

    internal_mcp::clear_registry();
}

#[test]
fn test_meta_tool_overrides_description_and_display_name() {
    let _g = TEST_LOCK.lock();
    internal_mcp::clear_registry();

    let src = r#"
::demo ns

search
meta {
    doc: "old doc",
    tool: {name: "search_index", description: "Override desc"}
}
fn (q: Str): Vec<Str> { [q] }
"#;
    compile_and_extract(src);

    let result = internal_mcp::schema_from_fn(&[function_ref("::demo/search".to_string())]);
    let map = match result {
        HotResult::Ok(Val::Map(m)) => m,
        other => panic!("expected Ok(Map), got: {:?}", other),
    };

    assert_eq!(
        map.get(&Val::from("description")),
        Some(&Val::from("Override desc")),
        "tool.description should win over doc"
    );
    assert_eq!(
        map.get(&Val::from("display-name")),
        Some(&Val::from("search_index")),
        "tool.name surfaces as display-name"
    );

    internal_mcp::clear_registry();
}

#[test]
fn test_meta_mcp_provides_fallback_description() {
    let _g = TEST_LOCK.lock();
    internal_mcp::clear_registry();

    let src = r#"
::demo ns

ping
meta {mcp: {service: "demo", name: "ping_tool", description: "MCP desc"}}
fn (msg: Str): Str { msg }
"#;
    compile_and_extract(src);

    let result = internal_mcp::schema_from_fn(&[function_ref("::demo/ping".to_string())]);
    let map = match result {
        HotResult::Ok(Val::Map(m)) => m,
        other => panic!("expected Ok(Map), got: {:?}", other),
    };

    assert_eq!(
        map.get(&Val::from("description")),
        Some(&Val::from("MCP desc"))
    );
    assert_eq!(
        map.get(&Val::from("display-name")),
        Some(&Val::from("ping_tool"))
    );

    internal_mcp::clear_registry();
}
