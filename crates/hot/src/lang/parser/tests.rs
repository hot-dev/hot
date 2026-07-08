use super::*;
use ahash::AHashMap;

#[test]
fn test_removed_result_modifier_syntax_errors() {
    // |map/|vec/|one were removed in Hot 2.6.0; the parser gives a
    // targeted migration hint instead of a generic parse error.
    for src in [
        "x cond|map {\n    true => m { 1 }\n}",
        "x cond-all|vec {\n    true => m { 1 }\n}",
        "x parallel|one {\n    a 1\n}",
        "x 5 |> add(2) |one",
        "x match|map v {\n    _ => { 1 }\n}",
    ] {
        let result = parse_hot(src);
        let err = format!("{:?}", result.expect_err(src));
        assert!(
            err.contains("removed in Hot 2.6.0"),
            "expected migration hint for {:?}, got: {}",
            src,
            err
        );
    }
}

#[test]
fn test_typed_bindings_in_value_position_blocks() {
    // `name: Type value` must parse anywhere a block statement can
    // appear, not just in function bodies. The one exception is a
    // block's FIRST statement, where `name:` is a map literal by the
    // documented disambiguation rule.
    for src in [
        // cond arm block
        "r cond {\n    true => m {\n        y 1\n        x: Int 5\n        add(x, y)\n    }\n}",
        // value-position serial flow
        "v serial {\n    x: Int 5\n    x\n}",
        // nested flow-shape annotation inside an arm block
        "r: All<Map> cond {\n    true => m {\n        y 1\n        inner: All<Map> serial { a 1 b 2 }\n        inner\n    }\n}",
    ] {
        let result = parse_hot(src);
        assert!(
            result.is_ok(),
            "expected {:?} to parse: {:?}",
            src,
            result.err()
        );
    }

    // First statement `name:` stays a map literal (documented ambiguity)
    let result = parse_hot("m {x: 1}");
    assert!(
        result.is_ok(),
        "leading name: is a map literal: {:?}",
        result.err()
    );
}

#[test]
fn test_plain_annotation_takes_single_value_on_collect_all_flows() {
    // A flow annotation states the type of the flow's result: All<...>
    // collects, any other type opts a collect-all flow out of collection
    // and takes the single final value.
    for src in [
        "x: Int parallel {\n    a 1\n}",
        "x: Str cond-all {\n    true => m { \"a\" }\n}",
        "x: Map parallel {\n    a {k: 1}\n}",
    ] {
        let program = parse_hot(src).expect(src);
        let default_ns = &program.namespaces[&NsPath::new()];
        let (_, value) = default_ns
            .scope
            .vars
            .iter()
            .find(|(var, _)| var.sym.name() == "x")
            .expect("x should exist");
        match value {
            Value::Flow(flow) => assert_eq!(
                flow.result_modifier,
                Some(ResultModifier::One),
                "plain annotation should set One on collect-all flow: {}",
                src
            ),
            other => panic!("expected flow, got {:?}", other),
        }
    }

    // On naturally-single flows a plain annotation is an ordinary type
    // check and sets no result modifier.
    for src in [
        "x: Int cond {\n    true => m { 1 }\n}",
        "x: Int serial {\n    a 1\n}",
        "x: Int 5 |> add(2)",
    ] {
        let program = parse_hot(src).expect(src);
        let default_ns = &program.namespaces[&NsPath::new()];
        let (_, value) = default_ns
            .scope
            .vars
            .iter()
            .find(|(var, _)| var.sym.name() == "x")
            .expect("x should exist");
        match value {
            Value::Flow(flow) => assert_eq!(
                flow.result_modifier, None,
                "plain annotation should not set a modifier on single-value flow: {}",
                src
            ),
            other => panic!("expected flow, got {:?}", other),
        }
    }
}

#[test]
fn test_simple_variable() {
    let source = r#"x 42"#;
    let result = parse_hot(source);
    assert!(result.is_ok());
}

#[test]
fn test_function_definition() {
    let source = r#"add fn (a, b) { call-lib(::hot::math/add, [a, b]) }"#;
    let result = parse_hot(source);
    assert!(result.is_ok());
}

#[test]
fn test_metadata() {
    let source = r#"test_func meta ["test"] fn () 42"#;
    let result = parse_hot(source);
    assert!(result.is_ok());
}

#[test]
fn test_metadata_object_before_function() {
    let source = r#"add meta {core: true, doc: "Add two numbers"} fn (x, y) { call-lib(::hot::math/add, [x, y]) }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_exact_math_hot_syntax() {
    // This is the exact syntax from math.hot that's failing
    let source = r#"is-pos meta {core: true, doc: "Return true if `x` is greater than zero"}
fn (x: Int | Dec): Bool { call-lib(::hot::math/gt, [x, 0]) }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for math.hot syntax: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_metadata_with_newlines() {
    // Test the real Hot syntax with newlines between parts
    let source = r#"assertion-passed
meta {doc: "Increment passed assertion counters"}
fn () { 42 }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for metadata with newlines: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_multiline_metadata_map() {
    // Test the exact pattern from test.hot that was initially failing
    let source = r#"run-tests
meta {
    doc: "Discover and run all test functions."
}
fn (capture-output) { 42 }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        eprintln!("Parse error for multiline metadata: {:?}", e);
    }
    assert!(result.is_ok(), "Should parse multiline metadata map");
}

#[test]
fn test_multiline_metadata_map_with_file_context() {
    // Test using parse_hot_file like the engine does
    let source = r#"run-tests
meta {
    doc: "Discover and run all test functions."
}
fn (capture-output) { 42 }"#;
    let result = parse_hot_file(source, "test.hot");
    if let Err(e) = &result {
        eprintln!("Parse error for multiline metadata (file context): {:?}", e);
    }
    assert!(
        result.is_ok(),
        "Should parse multiline metadata map with file context"
    );
}

#[test]
fn test_metadata_with_comment() {
    // Test with comment before variable definition (like in test.hot)
    let source = r#"// Helper to increment a counter in context
assertion-passed
meta {doc: "Increment passed assertion counters"}
fn () { 42 }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for metadata with comment: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_metadata_with_value_same_line() {
    // Test metadata with value on same line
    let source = r#"is-pos meta {core: true} fn (x) { call-lib(::hot::math/gt, [x, 0]) }"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for metadata same line: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_exact_test_hot_syntax() {
    let source = r#"increment-passed meta {doc: "Increment passed assertion counters"}
fn () {
    passed-count add(get-passed-count(), 1)
}"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for test.hot syntax: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_pipe_chain_parsing() {
    // Test the pipe chain syntax - simplified version
    let source = r#"test-functions namespaces()
|> mapcat(get-functions)
|> filter(is-test)"#;
    let result = parse_hot(source);
    if let Err(e) = &result {
        tracing::debug!("Parse error for pipe chain: {:?}", e);
    }
    assert!(result.is_ok());
}

#[test]
fn test_flexible_type_meta_fn_ordering() {
    // Test that meta, type, and fn parts can appear in any order

    // Order: meta then type (with fields and constructor)
    let source1 = r#"Duration
meta {doc: "A length of time"}
type {
    hours: Dec,
    minutes: Dec
}
fn (components: Map): Duration { components }"#;
    let result1 = parse_hot(source1);
    assert!(
        result1.is_ok(),
        "meta then type+fn failed: {:?}",
        result1.err()
    );

    // Order: type then meta (simple type alias)
    let source2 = r#"Day type
meta {doc: "Day time unit"}"#;
    let result2 = parse_hot(source2);
    assert!(
        result2.is_ok(),
        "type then meta failed: {:?}",
        result2.err()
    );

    // Verify the trailing meta is merged onto var.meta
    let program2 = result2.unwrap();
    let default_ns = &program2.namespaces[&NsPath::new()];
    let day_var = default_ns
        .scope
        .vars
        .iter()
        .find(|(v, _)| v.sym.name() == "Day");
    assert!(day_var.is_some(), "Day var not found");
    let (var, day_value) = day_var.unwrap();
    assert!(
        matches!(day_value, Value::TypeDef(_)),
        "Day should be a TypeDef"
    );
    assert!(
        var.meta.is_some(),
        "Day var should have meta attached (trailing meta merged onto var)"
    );

    // Order: type with braces then meta
    let source3 = r#"Person type {
    name: Str,
    age: Int
}
meta {doc: "A person record"}"#;
    let result3 = parse_hot(source3);
    assert!(
        result3.is_ok(),
        "type{{}} then meta failed: {:?}",
        result3.err()
    );

    // Order: type then meta then fn
    let source4 = r#"UserId type
meta {doc: "User identifier", core: true}
fn (id: Str): UserId { id }"#;
    let result4 = parse_hot(source4);
    assert!(
        result4.is_ok(),
        "type then meta then fn failed: {:?}",
        result4.err()
    );
}

/// Comprehensive Hot syntax parsing tests.
#[test]
fn test_basic_literals_and_assignments() {
    let source = r#"
        a 1.0
        b 2.0
        x [1, 2, 3]
        y [4, 5, 6]
        greeting "Hello"
        flag true
        empty null
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse basic literals: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    // Check that all variables were parsed
    let var_names: Vec<String> = default_ns
        .scope
        .vars
        .keys()
        .map(|var| var.sym.name().to_string())
        .collect();

    assert!(var_names.contains(&"a".to_string()));
    assert!(var_names.contains(&"greeting".to_string()));
    assert!(var_names.contains(&"x".to_string()));
}

#[test]
fn test_namespace_definition_and_switching() {
    let source = r#"::hot::test ns

        // Switch to another namespace
        ::hot::math ns

        x 42

        // Switch back
        ::hot::test ns

        y "hello""#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse namespace definitions: {:?}",
        result.err()
    );

    let program = result.unwrap();

    // Should have both namespaces
    assert!(program.namespaces.contains_key(&NsPath(vec![
        NsPathPart::Sym(Sym::from("hot")),
        NsPathPart::Sym(Sym::from("test"))
    ])));
    assert!(program.namespaces.contains_key(&NsPath(vec![
        NsPathPart::Sym(Sym::from("hot")),
        NsPathPart::Sym(Sym::from("math"))
    ])));

    // Check variable assignments in correct namespaces
    let math_ns = &program.namespaces[&NsPath(vec![
        NsPathPart::Sym(Sym::from("hot")),
        NsPathPart::Sym(Sym::from("math")),
    ])];
    assert!(
        math_ns
            .scope
            .vars
            .iter()
            .any(|(var, _)| var.sym.name() == "x")
    );

    let test_ns = &program.namespaces[&NsPath(vec![
        NsPathPart::Sym(Sym::from("hot")),
        NsPathPart::Sym(Sym::from("test")),
    ])];
    assert!(
        test_ns
            .scope
            .vars
            .iter()
            .any(|(var, _)| var.sym.name() == "y")
    );
}

#[test]
fn test_deep_path_access() {
    // Bracket whitespace differentiates deep access from assignment.
    let source = r#"
        // Deep bracket access without whitespace
        a[0] 42
        nested[0][1] "value"

        // Vector literal with whitespace
        b [42, 43]

        // Deep path with dots
        config.database.host "localhost"
        config.database.port 5432
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse deep path access: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    // Check that deep path variables were parsed
    let has_deep_path_var = default_ns
        .scope
        .vars
        .keys()
        .any(|var| var.deep_set.is_some());
    assert!(has_deep_path_var, "Should have variables with deep paths");
}

#[test]
fn test_variable_and_function_references() {
    let source = r#"
        // Variable references
        x 42
        y x  // Reference to x

        // Namespace references
        math_add ::hot::math/add
        run_tests ::hot::test/run-tests
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse references: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check variable reference
    if let Some(Value::Ref(Ref::Var(_))) = vars.get("y") {
        // Good - variable reference
    } else {
        panic!("y should be a variable reference");
    }

    // Check namespace reference
    if let Some(Value::Ref(Ref::Ns(_))) = vars.get("math_add") {
        // Good - namespace reference
    } else {
        panic!("math_add should be a namespace reference");
    }
}

#[test]
fn test_function_calls() {
    let source = r#"
        // Simple function call
        result1 add(1, 2)

        // Function call with namespace
        result2 ::hot::math/multiply(x, y)

        // Nested function calls
        result3 add(multiply(2, 3), divide(10, 2))

        // Function call with complex arguments
        result4 process({
            "data": [1, 2, 3],
            "config": {"mode": "fast"}
        })
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse function calls: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check simple function call
    if let Some(Value::FnCall(_)) = vars.get("result1") {
        // Good - function call
    } else {
        panic!("result1 should be a function call");
    }

    // Check namespace function call
    if let Some(Value::FnCall(_)) = vars.get("result2") {
        // Good - function call
    } else {
        panic!("result2 should be a function call");
    }

    // Check nested function calls
    if let Some(Value::FnCall(_)) = vars.get("result3") {
        // Good - function call
    } else {
        panic!("result3 should be a function call");
    }
}

#[test]
fn test_function_definitions_based_on_v1() {
    let source = r#"
        newUser fn(name) {
            {"name": name}
        }

        // Flow function definition.
        processOrder fn serial(order, customer, options) {
            validated validateOrder(order, options.rules)
        }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse function definitions: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let var_names: Vec<String> = default_ns
        .scope
        .vars
        .keys()
        .map(|var| var.sym.name().to_string())
        .collect();

    assert!(var_names.contains(&"newUser".to_string()));
    assert!(var_names.contains(&"processOrder".to_string()));
}

#[test]
fn test_flow_definitions() {
    let source = r#"
        // Serial flow
        serial_flow serial {
            step1 process_data(input)
            step2 validate(step1)
            step3 save(step2)
        }

        // Parallel flow
        parallel_flow parallel {
            branch1 fast_process(data)
            branch2 slow_process(data)
            branch3 backup_process(data)
        }

        // Simple pipe flow
        pipe_result data |> process |> validate

        // Flow function
        flow_fn fn serial (data) {
            cleaned clean_data(data)
            validated validate_data(cleaned)
            processed process_data(validated)
        }
        "#;

    let result = parse_hot(source);
    assert!(result.is_ok(), "Failed to parse flows: {:?}", result.err());

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check serial flow
    if let Some(Value::Flow(flow)) = vars.get("serial_flow") {
        assert_eq!(flow.flow_type, FlowType::Serial);
    } else {
        panic!("serial_flow should be a serial flow");
    }

    // Check parallel flow
    if let Some(Value::Flow(flow)) = vars.get("parallel_flow") {
        assert_eq!(flow.flow_type, FlowType::Parallel);
    } else {
        panic!("parallel_flow should be a parallel flow");
    }

    // Check pipe flow result
    if vars.contains_key("pipe_result") {
        // Good - pipe flow parsed
    } else {
        panic!("pipe_result should exist");
    }

    // Check flow function
    if let Some(Value::Fn(_)) = vars.get("flow_fn") {
        // Good - function with flow
    } else {
        panic!("flow_fn should be a function definition");
    }
}

#[test]
fn test_bare_all_only_infers_collect_all_flows() {
    let source = r#"
        result: All cond-all {
            true => a { 1 }
            false => b { 2 }
        }
        "#;

    let program = parse_hot(source).expect("bare All should parse on cond-all");
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "result")
        .expect("result should exist");

    match value {
        Value::Flow(flow) => assert_eq!(flow.result_modifier, Some(ResultModifier::Map)),
        other => panic!("expected flow, got {:?}", other),
    }
}

#[test]
fn test_bare_all_rejected_on_ambiguous_flows() {
    let source = r#"
        result: All cond {
            true => { 1 }
            => { 2 }
        }
        "#;

    assert!(parse_hot(source).is_err(), "bare All should fail on cond");
}

#[test]
fn test_explicit_all_still_allowed_on_ambiguous_flows() {
    let source = r#"
        result: All<Vec> cond {
            true => { 1 }
            => { 2 }
        }
        "#;

    let program = parse_hot(source).expect("explicit All<Vec> should parse on cond");
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "result")
        .expect("result should exist");

    match value {
        Value::Flow(flow) => assert_eq!(flow.result_modifier, Some(ResultModifier::Vec)),
        other => panic!("expected flow, got {:?}", other),
    }
}

#[test]
fn test_type_definitions_with_constructors() {
    let source = r#"
        // Simple type definitions (like time.hot)
        Nanosecond type

        Microsecond type

        // Type with struct and constructor functions (like Instant from time.hot)
        Instant type {epochNanoseconds: Int} fn
        (epoch-millis: Millisecond): Instant {
            call-lib(::hot::time/instant-from-millis, [epoch-millis])
        },
        (epoch-nanos: Nanosecond): Instant {
            call-lib(::hot::time/instant-from-nanos, [epoch-nanos])
        }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse type definitions: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check simple type definitions (like time.hot)
    if let Some(Value::TypeDef(_)) = vars.get("Nanosecond") {
        // Good - simple type definition
    } else {
        panic!("Nanosecond should be a type definition");
    }

    if let Some(Value::TypeDef(_)) = vars.get("Microsecond") {
        // Good - simple type definition
    } else {
        panic!("Microsecond should be a type definition");
    }

    // Check type with struct and constructor functions (like Instant)
    if let Some(Value::TypeDef(type_def)) = vars.get("Instant") {
        assert!(
            type_def.constructor_functions.is_some(),
            "Instant should have constructor functions"
        );
    } else {
        panic!("Instant should be a type definition");
    }
}

#[test]
fn test_type_implementations() {
    let source = r#"
        // Simple type implementation test
        TestType -> Int fn (x: TestType): Int { 42 }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse type implementations: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check simple type implementation - it's stored with the naming
    // convention `$impl_SourceType_TargetType@<position>`. The position
    // suffix lets two declarations with the same source/target pair
    // coexist (so the type checker can detect duplicate arrows).
    let impl_var = vars
        .iter()
        .find(|(name, _)| name.starts_with("$impl_TestType_Int@"));
    if let Some((_, Value::TypeImplementation(type_impl))) = impl_var {
        assert_eq!(type_impl.source_type, "TestType");
        assert_eq!(type_impl.target_type, "Int");
    } else {
        panic!(
            "Should have type implementation $impl_TestType_Int@..., but found variables: {:?}",
            vars.keys().collect::<Vec<_>>()
        );
    }
}

#[test]
fn test_email_workflow_from_v1() {
    // Simplified email workflow.
    let source = r#"::acme::email ns
        sendEmail ::postmark/emailWithTemplate
        sendSimple fn(to) {
            result sendEmail({
                "To": to
            })
        }"#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse email workflow: {:?}",
        result.err()
    );

    let program = result.unwrap();

    // Should have the acme::email namespace
    let email_ns_path = NsPath::from_vec(vec![
        NsPathPart::Sym(Sym::from("acme")),
        NsPathPart::Sym(Sym::from("email")),
    ]);
    assert!(program.namespaces.contains_key(&email_ns_path));

    let email_ns = &program.namespaces[&email_ns_path];
    let var_names: Vec<String> = email_ns
        .scope
        .vars
        .keys()
        .map(|var| var.sym.name().to_string())
        .collect();

    assert!(var_names.contains(&"sendEmail".to_string()));
    assert!(var_names.contains(&"sendSimple".to_string()));
}

#[test]
fn test_function_call_with_map_from_v1() {
    // Function call with a map argument.
    let source = r#"
        result test({
            "key": "value"
        })
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse function call with map: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    // Should have the result variable
    let has_result = default_ns
        .scope
        .vars
        .keys()
        .any(|var| var.sym.name() == "result");
    assert!(has_result, "Should have result variable");
}

#[test]
fn test_empty_map_literal() {
    // Test that {} is parsed as an empty map, not a block expression
    let source = r#"
        empty-map {}
        also-empty {   }
        with-newlines {
        }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse empty map literals: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    // Should have all three variables
    let has_empty_map = default_ns
        .scope
        .vars
        .keys()
        .any(|var| var.sym.name() == "empty-map");
    let has_also_empty = default_ns
        .scope
        .vars
        .keys()
        .any(|var| var.sym.name() == "also-empty");
    let has_with_newlines = default_ns
        .scope
        .vars
        .keys()
        .any(|var| var.sym.name() == "with-newlines");

    assert!(has_empty_map, "Should have empty-map variable");
    assert!(has_also_empty, "Should have also-empty variable");
    assert!(has_with_newlines, "Should have with-newlines variable");

    // Verify they are actually empty maps (Val::Map with empty IndexMap)
    for var in default_ns.scope.vars.keys() {
        if matches!(var.sym.name(), "empty-map" | "also-empty" | "with-newlines") {
            let value = &default_ns.scope.vars[var];
            match value {
                crate::lang::ast::Value::Val(crate::val::Val::Map(map), _) => {
                    assert!(map.is_empty(), "Map for {} should be empty", var.sym.name());
                }
                _ => panic!(
                    "Expected empty Val::Map for {}, got {:?}",
                    var.sym.name(),
                    value
                ),
            }
        }
    }
}

#[test]
fn test_metadata_and_core_functions() {
    // Test metadata parsing and core function patterns
    let source = r#"
        // Core function with metadata
        add meta {core: true, doc: "Add two numbers"}
        fn (x: Int | Dec, y: Int | Dec): Int | Dec {
            call-lib(::hot::math/add, [x, y])
        }

        // Test function with array metadata
        test_func meta ["test", "unit"]
        fn () { 42 }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse metadata and core functions: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    // Check that functions with metadata were parsed
    let add_var = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "add")
        .expect("add function should exist");
    assert!(add_var.0.meta.is_some(), "add should have metadata");

    let test_var = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "test_func")
        .expect("test_func should exist");
    assert!(test_var.0.meta.is_some(), "test_func should have metadata");
}

#[test]
fn test_function_definitions_with_all_flow_types() {
    // Based on bool.hot - test functions with different flow types
    let source = r#"
        // Regular function (no flow type)
        regular_fn fn (x) { x }

        // Serial flow function
        serial_fn fn serial (data) {
            step1 process(data)
            step2 validate(step1)
        }

        // Parallel flow function
        parallel_fn fn parallel (data) {
            branch1 fast_process(data)
            branch2 slow_process(data)
        }

        // Conditional flow function (like in bool.hot)
        if_fn meta {core: true, doc: "Conditional function"}
        fn cond (pred: Any, lazy then: Any): Any {
            pred => { do then }
        }

        // Match flow function (fn match syntax, consistent with fn cond)
        Status enum { Active, Inactive }
        describe_status fn match (s: Status): Str {
            Status.Active => { "active" }
            Status.Inactive => { "inactive" }
        }

        // Match-all flow function
        describe_all fn match-all (s: Status): Map {
            Status.Active => { "is active" }
            Status.Inactive => { "is inactive" }
        }

        // Pipe flow function
        pipe_fn fn (data) {
            data |> clean |> validate |> save
        }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse functions with flow types: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let var_names: Vec<String> = default_ns
        .scope
        .vars
        .keys()
        .map(|var| var.sym.name().to_string())
        .collect();

    // Check all function types were parsed
    assert!(var_names.contains(&"regular_fn".to_string()));
    assert!(var_names.contains(&"serial_fn".to_string()));
    assert!(var_names.contains(&"parallel_fn".to_string()));
    assert!(var_names.contains(&"if_fn".to_string()));
    assert!(var_names.contains(&"pipe_fn".to_string()));
    assert!(var_names.contains(&"Status".to_string())); // enum for match tests
    assert!(var_names.contains(&"describe_status".to_string())); // fn match
    assert!(var_names.contains(&"describe_all".to_string())); // fn match-all

    // Check that if_fn has metadata (like in bool.hot)
    let if_fn_var = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "if_fn")
        .expect("if_fn should exist");
    assert!(if_fn_var.0.meta.is_some(), "if_fn should have metadata");
}

#[test]
fn test_multiple_function_definitions_with_arities_and_types() {
    // Test multiple function definitions with different arities and type annotations
    // This is a key Hot language feature for function overloading
    let source = r#"
        // Function with multiple arities - no types
        add fn
        (x) { x },
        (x, y) { call-lib(::hot::math/add, [x, y]) }

        // Function with type annotations and multiple arities
        convert meta {core: true, doc: "Convert between types"}
        fn
        (value: Str): Int { call-lib(::hot::convert/str-to-int, [value]) },
        (value: Int): Str { call-lib(::hot::convert/int-to-str, [value]) }

        // Function with basic types and no-arg version
        process fn
        (): Null { null },
        (data: Str): Str { call-lib(::hot::process/simple, [data]) }
        "#;

    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Failed to parse multiple function definitions: {:?}",
        result.err()
    );

    let program = result.unwrap();
    // Use the actual default namespace (empty path)
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];

    let vars: AHashMap<String, &Value> = default_ns
        .scope
        .vars
        .iter()
        .map(|(var, value)| (var.sym.name().to_string(), value))
        .collect();

    // Check add function with multiple arities
    if let Some(Value::Fn(definitions)) = vars.get("add") {
        assert_eq!(
            definitions.len(),
            2,
            "add should have 2 different arity definitions"
        );

        // Check that we have 1-arg and 2-arg versions
        let arities: Vec<usize> = definitions.iter().map(|def| def.args.args.len()).collect();
        assert!(arities.contains(&1), "Should have 1-arg version");
        assert!(arities.contains(&2), "Should have 2-arg version");
    } else {
        panic!("add should be a function with multiple definitions");
    }

    // Check convert function with type annotations and metadata
    if let Some(Value::Fn(definitions)) = vars.get("convert") {
        assert_eq!(
            definitions.len(),
            2,
            "convert should have 2 type-specific definitions"
        );
    } else {
        panic!("convert should be a function with multiple definitions");
    }

    // Check that convert has metadata
    let convert_var = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "convert")
        .expect("convert function should exist");
    assert!(convert_var.0.meta.is_some(), "convert should have metadata");

    // Check process function with basic types
    if let Some(Value::Fn(definitions)) = vars.get("process") {
        assert_eq!(
            definitions.len(),
            2,
            "process should have 2 definitions with different arities"
        );

        // Check for 0-arg and 1-arg versions
        let arities: Vec<usize> = definitions.iter().map(|def| def.args.args.len()).collect();
        assert!(arities.contains(&0), "Should have 0-arg version");
        assert!(arities.contains(&1), "Should have 1-arg version");
    } else {
        panic!("process should be a function with multiple definitions");
    }

    // Verify all functions were parsed
    let var_names: Vec<String> = default_ns
        .scope
        .vars
        .keys()
        .map(|var| var.sym.name().to_string())
        .collect();

    assert!(var_names.contains(&"add".to_string()));
    assert!(var_names.contains(&"convert".to_string()));
    assert!(var_names.contains(&"process".to_string()));
}

#[test]
fn test_variadic_function_parsing() {
    // Test basic variadic function
    let source = r#"
        concat fn (a, b, ...rest) {
            call-lib(::hot::coll/concat, [a, b, ...rest])
        }
        "#;
    let result = parse_hot(source);
    assert!(result.is_ok(), "Should parse variadic function");

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, concat_value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "concat")
        .expect("concat should exist");

    if let Value::Fn(fn_defs) = concat_value {
        assert_eq!(fn_defs.len(), 1);
        let fn_def = &fn_defs[0];
        assert!(fn_def.args.variadic, "Function should be variadic");
        assert_eq!(fn_def.args.args.len(), 3); // a, b, rest
        assert!(!fn_def.args.args[0].lazy, "First arg should not be lazy");
        assert!(!fn_def.args.args[1].lazy, "Second arg should not be lazy");
        assert!(
            !fn_def.args.args[2].lazy,
            "Variadic arg should not be lazy by default"
        );
    } else {
        panic!("concat should be a function definition");
    }
}

#[test]
fn test_lazy_variadic_function_parsing() {
    // Test lazy variadic function - the new syntax "lazy ...rest"
    let source = r#"
        or fn (a, lazy ...rest) {
            // Short-circuit or with lazy variadic args
            a
        }
        "#;
    let result = parse_hot(source);
    assert!(result.is_ok(), "Should parse lazy variadic function");

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, or_value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "or")
        .expect("or should exist");

    if let Value::Fn(fn_defs) = or_value {
        assert_eq!(fn_defs.len(), 1);
        let fn_def = &fn_defs[0];
        assert!(fn_def.args.variadic, "Function should be variadic");
        assert_eq!(fn_def.args.args.len(), 2); // a, rest
        assert!(!fn_def.args.args[0].lazy, "First arg should not be lazy");
        assert!(fn_def.args.args[1].lazy, "Variadic arg should be lazy");
    } else {
        panic!("or should be a function definition");
    }
}

#[test]
fn test_lazy_variadic_with_type_annotation() {
    // Test lazy variadic with type annotation
    let source = r#"
        and fn (a: Any, lazy ...rest: Any): Any {
            a
        }
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse lazy variadic with type annotation"
    );

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, and_value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "and")
        .expect("and should exist");

    if let Value::Fn(fn_defs) = and_value {
        assert_eq!(fn_defs.len(), 1);
        let fn_def = &fn_defs[0];
        assert!(fn_def.args.variadic, "Function should be variadic");
        assert!(fn_def.args.args[1].lazy, "Variadic arg should be lazy");
        assert!(
            fn_def.args.args[1].type_annotation.is_some(),
            "Variadic arg should have type annotation"
        );
    } else {
        panic!("and should be a function definition");
    }
}

#[test]
fn test_lazy_variadic_in_cond_flow() {
    // Test lazy variadic in cond flow type function
    let source = r#"
        or fn
        cond (a: Any, lazy ...rest: Any): Any {
            is-truthy(a) => { a }
            => { first(rest) }
        }
        "#;
    let result = parse_hot(source);
    assert!(result.is_ok(), "Should parse lazy variadic in cond flow");

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let default_ns = &program.namespaces[&default_ns_path];
    let (_, or_value) = default_ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "or")
        .expect("or should exist");

    if let Value::Fn(fn_defs) = or_value {
        let fn_def = &fn_defs[0];
        assert!(fn_def.args.variadic, "Function should be variadic");
        assert!(fn_def.args.args[1].lazy, "Variadic arg should be lazy");
    } else {
        panic!("or should be a function definition");
    }
}

// ============================================================================
// Namespace Aliasing Tests
// ============================================================================

#[test]
fn test_namespace_alias_simple() {
    // Test basic namespace alias syntax: ::alias ::source::path
    let source = r#"
::test::ns ns

::http ::hot::http

x 42
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse simple namespace alias: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // Check that the alias was captured
    assert!(
        ns.aliases.contains_key(&NsPath::from("::http")),
        "Alias ::http should exist in namespace aliases"
    );
    assert_eq!(
        ns.aliases.get(&NsPath::from("::http")),
        Some(&NsPath::from("::hot::http")),
        "::http should map to ::hot::http"
    );
}

#[test]
fn test_namespace_alias_multiple() {
    // Test multiple namespace aliases in same namespace
    let source = r#"
::test::ns ns

::http ::hot::http
::env ::hot::env
::ctx ::hot::ctx

x 42
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse multiple namespace aliases: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // Check all aliases
    assert_eq!(ns.aliases.len(), 3, "Should have 3 aliases");
    assert_eq!(
        ns.aliases.get(&NsPath::from("::http")),
        Some(&NsPath::from("::hot::http"))
    );
    assert_eq!(
        ns.aliases.get(&NsPath::from("::env")),
        Some(&NsPath::from("::hot::env"))
    );
    assert_eq!(
        ns.aliases.get(&NsPath::from("::ctx")),
        Some(&NsPath::from("::hot::ctx"))
    );
}

#[test]
fn test_namespace_alias_with_deeper_path() {
    // Test alias to deeply nested namespace
    let source = r#"
::test::ns ns

::time ::hot::time
::str ::hot::str

x 42
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse aliases to deeper paths: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    assert_eq!(
        ns.aliases.get(&NsPath::from("::time")),
        Some(&NsPath::from("::hot::time"))
    );
    assert_eq!(
        ns.aliases.get(&NsPath::from("::str")),
        Some(&NsPath::from("::hot::str"))
    );
}

#[test]
fn test_namespace_alias_in_default_namespace() {
    // Test alias in default (unnamed) namespace
    let source = r#"
::env ::hot::env

config ::env/get("CONFIG", "default")
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse alias in default namespace: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let default_ns_path = NsPath::new();
    let ns = program
        .namespaces
        .get(&default_ns_path)
        .expect("Default namespace should exist");

    assert_eq!(
        ns.aliases.get(&NsPath::from("::env")),
        Some(&NsPath::from("::hot::env"))
    );
}

#[test]
fn test_namespace_alias_with_function_call() {
    // Test that aliases work alongside function calls
    let source = r#"
::test::ns ns

::http ::hot::http

// This should parse correctly with the alias declared
result ::http/get("https://example.com")
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse alias with function call usage: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    assert!(
        ns.aliases.contains_key(&NsPath::from("::http")),
        "::http alias should be present"
    );
}

#[test]
fn test_namespace_alias_not_confused_with_ns_ref() {
    // Test that regular namespace references are not confused with alias definitions
    let source = r#"
::test::ns ns

// This is a function alias (variable assignment)
get-env ::hot::env/get

// This is a namespace alias (starts with ::, followed by another :: namespace)
::env ::hot::env

x 42
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should distinguish ns alias from function alias: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // Should have the namespace alias
    assert_eq!(
        ns.aliases.get(&NsPath::from("::env")),
        Some(&NsPath::from("::hot::env"))
    );

    // Should also have the function alias as a variable
    let has_get_env = ns
        .scope
        .vars
        .iter()
        .any(|(var, _)| var.sym.name() == "get-env");
    assert!(
        has_get_env,
        "get-env function alias should exist as variable"
    );
}

#[test]
fn test_namespace_alias_with_multi_segment_alias() {
    // Test alias that has multiple segments
    let source = r#"
::test::ns ns

::my::http ::hot::http

x 42
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse multi-segment alias: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    assert_eq!(
        ns.aliases.get(&NsPath::from("::my::http")),
        Some(&NsPath::from("::hot::http"))
    );
}

#[test]
fn test_namespace_alias_in_function_body() {
    let source = r#"
::test::ns ns

my-fn fn () {
  ::env ::hot::env
  result ::env/get("HOME", "/default")
  result
}
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse namespace alias inside function body: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // The alias should NOT be on the namespace (it's scoped to the function)
    assert!(
        !ns.aliases.contains_key(&NsPath::from("::env")),
        "Alias ::env should NOT be on the namespace (it's flow-scoped)"
    );

    // The alias should be on the function body's flow
    let (_, fn_value) = ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "my-fn")
        .expect("my-fn should exist");
    if let Value::Fn(fn_defs) = fn_value {
        if let Value::Flow(flow) = &fn_defs[0].body {
            assert_eq!(flow.aliases.len(), 1, "Flow should have 1 alias");
            assert_eq!(flow.aliases[0].1, NsPath::from("::env"));
            assert_eq!(flow.aliases[0].2, NsPath::from("::hot::env"));
        } else {
            panic!("Function body should be a Flow");
        }
    } else {
        panic!("my-fn should be a function");
    }
}

#[test]
fn test_variant_union_type() {
    let source = r#"
::test::ns ns

Direction type
  | Up
  | Down
  | Left
  | Right
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse variant union type: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // Should have the Direction type
    let has_direction = ns
        .scope
        .vars
        .iter()
        .any(|(var, _)| var.sym.name() == "Direction");
    assert!(has_direction, "Direction type should exist as variable");

    // Find the Direction type and check its variants
    let direction_var = ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "Direction");
    if let Some((_, value)) = direction_var {
        if let Value::TypeDef(type_def) = value {
            assert!(
                type_def.variants.is_some(),
                "Direction should have variants: {:?}",
                type_def
            );
            let variants = type_def.variants.as_ref().unwrap();
            assert_eq!(variants.len(), 4, "Should have 4 variants");
            assert_eq!(variants[0].name.name(), "Up");
            assert_eq!(variants[1].name.name(), "Down");
            assert_eq!(variants[2].name.name(), "Left");
            assert_eq!(variants[3].name.name(), "Right");
        } else {
            panic!("Direction should be a TypeDef, got: {:?}", value);
        }
    }
}

#[test]
fn test_variant_union_type_with_type_ref() {
    let source = r#"
::test::ns ns

Circle type { radius: Dec }

Shape type
  | Circle(Circle)
  | Point
        "#;
    let result = parse_hot(source);
    assert!(
        result.is_ok(),
        "Should parse variant union type with type ref: {:?}",
        result.err()
    );

    let program = result.unwrap();
    let ns_path = NsPath::from("::test::ns");
    let ns = program
        .namespaces
        .get(&ns_path)
        .expect("Namespace should exist");

    // Find the Shape type
    let shape_var = ns
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == "Shape");
    if let Some((_, value)) = shape_var {
        if let Value::TypeDef(type_def) = value {
            assert!(
                type_def.variants.is_some(),
                "Shape should have variants: {:?}",
                type_def
            );
            let variants = type_def.variants.as_ref().unwrap();
            assert_eq!(variants.len(), 2, "Should have 2 variants");
            assert_eq!(variants[0].name.name(), "Circle");
            assert_eq!(variants[0].type_ref, Some("Circle".to_string()));
            assert_eq!(variants[1].name.name(), "Point");
            assert_eq!(variants[1].type_ref, None); // Unit variant
        } else {
            panic!("Shape should be a TypeDef, got: {:?}", value);
        }
    }
}

/// Helpers shared by the open-enum / arrow-enrollment parser tests.
fn find_type_def<'a>(program: &'a Program, name: &str) -> Option<&'a TypeDef> {
    for ns in program.namespaces.values() {
        for (var, value) in &ns.scope.vars {
            if var.sym.name() == name
                && let Value::TypeDef(td) = value
            {
                return Some(td);
            }
        }
    }
    None
}

fn find_type_implementations<'a>(
    program: &'a Program,
    source: &str,
    target: &str,
) -> Vec<&'a TypeImplementation> {
    let mut out = Vec::new();
    for ns in program.namespaces.values() {
        for (_var, value) in &ns.scope.vars {
            if let Value::TypeImplementation(ti) = value
                && ti.source_type == source
                && ti.target_type == target
            {
                out.push(ti.as_ref());
            }
        }
    }
    out
}

#[test]
fn test_parse_enum_open_marks_is_open() {
    let source = r#"
::test::ns ns

Shape enum open { Circle(Circle), Rectangle(Rectangle) }
        "#;
    let program = parse_hot(source).expect("should parse open enum");
    let td = find_type_def(&program, "Shape").expect("Shape TypeDef should exist");
    assert!(td.is_open, "open enum should set is_open = true");
    let variants = td.variants.as_ref().expect("should have variants");
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].name.name(), "Circle");
    assert_eq!(variants[1].name.name(), "Rectangle");
}

#[test]
fn test_parse_enum_open_empty_body_is_allowed() {
    // An open enum with no seed variants is meaningful — it's a pure
    // extension point that gets populated later via `Source -> Enum.X`.
    let source = r#"
::test::ns ns

Event enum open {}
        "#;
    let program = parse_hot(source).expect("should parse empty open enum");
    let td = find_type_def(&program, "Event").expect("Event TypeDef should exist");
    assert!(td.is_open);
    assert_eq!(
        td.variants.as_ref().map(|v| v.len()).unwrap_or(0),
        0,
        "empty open enum should have zero seed variants"
    );
}

#[test]
fn test_parse_closed_enum_is_not_open() {
    let source = r#"
::test::ns ns

Direction enum { Up, Down, Left, Right }
        "#;
    let program = parse_hot(source).expect("should parse closed enum");
    let td = find_type_def(&program, "Direction").expect("Direction TypeDef");
    assert!(!td.is_open, "closed enum must have is_open = false");
    assert_eq!(td.variants.as_ref().map(|v| v.len()), Some(4));
}

#[test]
fn test_parse_arrow_dotted_target() {
    // Bodyless dotted-target arrow exercises the parser path most
    // directly without depending on whether the rest of the function
    // grammar (return-type annotations on dotted targets, body parsing
    // of `Shape.Tri(t)`) has been updated. The bodyless form shows
    // that the *target* parser accepts the dotted name and produces a
    // registered implementation under the dotted key.
    let source = r#"
::test::ns ns

Shape enum open { Circle(Circle) }
Triangle type { base: Dec, height: Dec }
Triangle -> Shape.Tri
        "#;
    let program = parse_hot(source).expect("should parse dotted-target arrow");
    let impls = find_type_implementations(&program, "Triangle", "Shape.Tri");
    assert_eq!(
        impls.len(),
        1,
        "should register one Triangle -> Shape.Tri implementation"
    );
}

#[test]
fn test_parse_bodyless_arrow_synthesizes_wrap() {
    let source = r#"
::test::ns ns

Shape enum open { Circle(Circle) }
Triangle type { base: Dec, height: Dec }
Triangle -> Shape.Tri
        "#;
    let program = parse_hot(source).expect("should parse bodyless arrow");
    let impls = find_type_implementations(&program, "Triangle", "Shape.Tri");
    assert_eq!(impls.len(), 1, "bodyless arrow should produce one impl");
    let impl_value = &impls[0].implementation;
    // The synthesized implementation should be a Value::Fn with one
    // FnDef whose return type matches the dotted target.
    match impl_value {
        Value::Fn(defs) => {
            assert_eq!(defs.len(), 1, "synthesized fn has exactly one arity");
            let def = &defs[0];
            assert_eq!(def.return_type.as_deref(), Some("Shape.Tri"));
            assert_eq!(def.args.args.len(), 1);
            assert_eq!(
                def.args.args[0].type_annotation.as_deref(),
                Some("Triangle")
            );
        }
        other => panic!("expected Value::Fn for synthesized wrap, got {:?}", other),
    }
}

#[test]
fn test_parse_bodyless_arrow_namespaced_target() {
    let source = r#"
::test::ns ns

Triangle type { base: Dec, height: Dec }
Triangle -> ::other/Shape.Tri
        "#;
    let program = parse_hot(source).expect("should parse namespaced bodyless arrow");
    let impls = find_type_implementations(&program, "Triangle", "::other/Shape.Tri");
    assert_eq!(
        impls.len(),
        1,
        "should accept namespaced dotted bodyless target"
    );
}

#[test]
fn test_parse_open_literal_union_marks_is_open() {
    let source = r#"
::test::ns ns

Fruit type open "apple" | "banana" | "orange"
        "#;
    let program = parse_hot(source).expect("should parse open literal union");
    let td = find_type_def(&program, "Fruit").expect("Fruit TypeDef should exist");
    assert!(td.is_open, "open literal union should set is_open = true");
    let alias = td.type_alias.as_ref().expect("should have type alias");
    match alias {
        crate::lang::ast::TypeExpr::Union(members) => {
            assert_eq!(members.len(), 3);
        }
        other => panic!("expected Union, got {:?}", other),
    }
}

#[test]
fn test_parse_open_literal_union_with_leading_pipe() {
    let source = r#"
::test::ns ns

Fruit type open | "kiwi"
        "#;
    let program = parse_hot(source).expect("should parse open literal union with leading |");
    let td = find_type_def(&program, "Fruit").expect("Fruit TypeDef should exist");
    assert!(
        td.is_open,
        "open literal union with leading | should set is_open = true"
    );
    let alias = td.type_alias.as_ref().expect("should have type alias");
    // Single-member alias is not wrapped in Union — `is_open` is the
    // discriminator.
    match alias {
        crate::lang::ast::TypeExpr::Literal(crate::lang::ast::LiteralType::Str(s)) => {
            assert_eq!(s, "kiwi")
        }
        other => panic!("expected Literal Str(\"kiwi\"), got {:?}", other),
    }
}

#[test]
fn test_parse_open_literal_union_with_leading_pipe_multi() {
    let source = r#"
::test::ns ns

Fruit type open | "kiwi" | "mango"
        "#;
    let program = parse_hot(source).expect("should parse open literal union with leading |");
    let td = find_type_def(&program, "Fruit").expect("Fruit TypeDef should exist");
    assert!(td.is_open);
    match td.type_alias.as_ref().expect("alias") {
        crate::lang::ast::TypeExpr::Union(members) => assert_eq!(members.len(), 2),
        other => panic!("expected Union, got {:?}", other),
    }
}

#[test]
fn test_parse_closed_literal_union_is_not_open() {
    let source = r#"
::test::ns ns

Fruit type "apple" | "banana"
        "#;
    let program = parse_hot(source).expect("should parse closed literal union");
    let td = find_type_def(&program, "Fruit").expect("Fruit TypeDef should exist");
    assert!(
        !td.is_open,
        "closed literal union must keep is_open = false"
    );
}

#[test]
fn test_parse_open_keyword_not_consumed_when_no_literal_follows() {
    // `Foo type open` (with nothing or a non-literal after `open`) is
    // not a valid open-literal-union declaration. The parser must NOT
    // greedily consume `open` here — instead it falls through to the
    // existing identifier-alias path so `open` is treated as a regular
    // (unknown) type name. The type checker handles the diagnostic.
    let source = r#"
::test::ns ns

Foo type open
        "#;
    let program = parse_hot(source).expect("should parse without panicking");
    let td = find_type_def(&program, "Foo").expect("Foo TypeDef should exist");
    assert!(
        !td.is_open,
        "bare `type open` (no literals/leading pipe) must not set is_open"
    );
}

#[test]
fn test_parse_open_literal_union_with_int_members() {
    let source = r#"
::test::ns ns

Roll type open 1 | 2 | 3
        "#;
    let program = parse_hot(source).expect("should parse open literal union of ints");
    let td = find_type_def(&program, "Roll").expect("Roll TypeDef should exist");
    assert!(td.is_open);
    match td.type_alias.as_ref().expect("alias") {
        crate::lang::ast::TypeExpr::Union(members) => assert_eq!(members.len(), 3),
        other => panic!("expected Union of ints, got {:?}", other),
    }
}
