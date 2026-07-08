//! Compiler test suite (split out of mod.rs to keep the orchestrator file
//! navigable). Lives behind `#[cfg(test)] mod tests;` in compiler/mod.rs.

use super::*;
use crate::lang::parser::parse_hot;

#[test]
fn test_compiler_creation() {
    let compiler = Compiler::new();

    // Check initial state
    assert_eq!(compiler.next_register, 0);
    assert_eq!(compiler.function_mapping.len(), 0);
    assert_eq!(compiler.type_implementations.len(), 0);
    assert_eq!(compiler.current_namespace, "hot::main");

    // Check bytecode program is initialized
    let program = compiler.get_program();
    assert_eq!(program.constants.len(), 0);
    assert_eq!(program.functions.len(), 0);
}

#[test]
fn test_auto_generate_mcp_tool_name_uses_underscores() {
    let name = auto_generate_mcp_tool_name("::myapp::tools", "get-weather");
    assert_eq!(name, "myapp_tools_get_weather");
}

#[test]
fn test_auto_generate_mcp_tool_name_preserves_explicit_separators_as_underscores() {
    let name = auto_generate_mcp_tool_name("::hot::hi", "farewell");
    assert_eq!(name, "hot_hi_farewell");
}

#[test]
fn test_auto_generate_mcp_tool_name_handles_empty_namespace() {
    let name = auto_generate_mcp_tool_name("::", "ping-tool");
    assert_eq!(name, "ping_tool");
}

#[test]
fn test_auto_generate_mcp_tool_name_normalizes_namespace_hyphens() {
    let name = auto_generate_mcp_tool_name("::my-app::hi", "farewell");
    assert_eq!(name, "my_app_hi_farewell");
}

// ========================================================================
// URL-safe service name validation
// ========================================================================

#[test]
fn test_url_safe_service_name_valid() {
    assert!(is_url_safe_service_name("slack"));
    assert!(is_url_safe_service_name("my-service"));
    assert!(is_url_safe_service_name("my_service"));
    assert!(is_url_safe_service_name("my.service"));
    assert!(is_url_safe_service_name("github-webhooks"));
    assert!(is_url_safe_service_name("stripe_v2"));
    assert!(is_url_safe_service_name("a"));
    assert!(is_url_safe_service_name("A"));
    assert!(is_url_safe_service_name("service123"));
    assert!(is_url_safe_service_name("my-app.v2_beta"));
}

#[test]
fn test_url_safe_service_name_invalid_empty() {
    assert!(!is_url_safe_service_name(""));
}

#[test]
fn test_url_safe_service_name_invalid_leading_trailing() {
    assert!(!is_url_safe_service_name("-leading"));
    assert!(!is_url_safe_service_name("trailing-"));
    assert!(!is_url_safe_service_name(".leading"));
    assert!(!is_url_safe_service_name("trailing."));
    assert!(!is_url_safe_service_name("-"));
    assert!(!is_url_safe_service_name("."));
}

#[test]
fn test_url_safe_service_name_invalid_characters() {
    assert!(!is_url_safe_service_name("my service"));
    assert!(!is_url_safe_service_name("my/service"));
    assert!(!is_url_safe_service_name("my@service"));
    assert!(!is_url_safe_service_name("my#service"));
    assert!(!is_url_safe_service_name("my?service"));
    assert!(!is_url_safe_service_name("my&service"));
    assert!(!is_url_safe_service_name("my=service"));
    assert!(!is_url_safe_service_name("service!"));
    assert!(!is_url_safe_service_name("my%20service"));
}

// ========================================================================
// Webhook endpoint name generation
// ========================================================================

#[test]
fn test_auto_generate_webhook_name_basic() {
    let name = auto_generate_webhook_name("::myapp::webhooks", "on-payment");
    assert_eq!(name, "myapp_webhooks_on_payment");
}

#[test]
fn test_auto_generate_webhook_name_empty_namespace() {
    let name = auto_generate_webhook_name("::", "handle-event");
    assert_eq!(name, "handle_event");
}

#[test]
fn test_auto_generate_webhook_name_no_hyphens() {
    let name = auto_generate_webhook_name("::app::slack", "verify");
    assert_eq!(name, "app_slack_verify");
}

// ========================================================================
// Webhook extraction from compiled code
// ========================================================================

#[test]
fn test_extract_webhook_basic() {
    let source = r#"
        on-slack-event
        meta {webhook: {service: "slack", path: "/events"}}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract webhooks");

    assert_eq!(compiler.webhooks.len(), 1);
    assert!(compiler.webhooks.contains_key("slack"));

    let endpoints = &compiler.webhooks["slack"];
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].service, "slack");
    assert_eq!(endpoints[0].path, "/events");
    assert_eq!(endpoints[0].method, "POST"); // default
}

#[test]
fn test_extract_webhook_custom_method() {
    let source = r#"
        get-status
        meta {webhook: {service: "health", path: "/status", method: "GET"}}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    let endpoints = &compiler.webhooks["health"];
    assert_eq!(endpoints[0].method, "GET");
}

#[test]
fn test_extract_webhook_with_all_fields() {
    let source = r#"
        payment-webhook
        meta {webhook: {
            service: "stripe",
            path: "/payment",
            method: "POST",
            name: "stripe_payment_handler",
            description: "Handle Stripe payment events",
            auth: "required"
        }}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    let endpoints = &compiler.webhooks["stripe"];
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].name, "stripe_payment_handler");

    // Check the webhook Val contains the expected fields
    let ep = &endpoints[0].webhook;
    assert_eq!(ep.get_str("description"), "Handle Stripe payment events");
    assert_eq!(ep.get_str("auth_mode"), "required");
}

#[test]
fn test_extract_webhook_missing_service_skipped() {
    let source = r#"
        bad-webhook
        meta {webhook: {path: "/events"}}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert!(
        compiler.webhooks.is_empty(),
        "Webhook missing 'service' should be skipped"
    );
}

#[test]
fn test_extract_webhook_missing_path_skipped() {
    let source = r#"
        bad-webhook
        meta {webhook: {service: "slack"}}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert!(
        compiler.webhooks.is_empty(),
        "Webhook missing 'path' should be skipped"
    );
}

#[test]
fn test_extract_webhook_invalid_service_name_skipped() {
    let source = r#"
        bad-webhook
        meta {webhook: {service: "my service", path: "/events"}}
        fn (request) {
            request
        }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert!(
        compiler.webhooks.is_empty(),
        "Webhook with invalid service name should be skipped"
    );
}

#[test]
fn test_extract_webhook_multiple_services() {
    let source = r#"
        slack-handler
        meta {webhook: {service: "slack", path: "/events"}}
        fn (request) { request }

        stripe-handler
        meta {webhook: {service: "stripe", path: "/payment"}}
        fn (request) { request }

        github-handler
        meta {webhook: {service: "github", path: "/push"}}
        fn (request) { request }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.webhooks.len(), 3);
    assert!(compiler.webhooks.contains_key("slack"));
    assert!(compiler.webhooks.contains_key("stripe"));
    assert!(compiler.webhooks.contains_key("github"));
}

#[test]
fn test_extract_webhook_multiple_per_service() {
    let source = r#"
        slack-events
        meta {webhook: {service: "slack", path: "/events"}}
        fn (request) { request }

        slack-commands
        meta {webhook: {service: "slack", path: "/commands"}}
        fn (request) { request }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.webhooks.len(), 1);

    let endpoints = &compiler.webhooks["slack"];
    assert_eq!(endpoints.len(), 2);

    let paths: Vec<&str> = endpoints.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"/events"));
    assert!(paths.contains(&"/commands"));
}

#[test]
fn test_extract_webhook_method_case_normalized() {
    let source = r#"
        handler
        meta {webhook: {service: "test", path: "/hook", method: "put"}}
        fn (request) { request }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    let endpoints = &compiler.webhooks["test"];
    assert_eq!(endpoints[0].method, "PUT");
}

#[test]
fn test_extract_webhook_non_webhook_meta_ignored() {
    let source = r#"
        scheduled-job
        meta {schedule: "0 * * * *"}
        fn () { 42 }

        event-handler
        meta {on-event: "user:created"}
        fn (event) { event }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_webhooks(&program)
        .expect("Failed to extract");

    assert!(
        compiler.webhooks.is_empty(),
        "Non-webhook meta should not create webhooks"
    );
}

#[test]
fn test_extract_workflow_type_definition() {
    let source = r#"
        ::demo::sales ns

        LeadQualification
        meta {
            doc: "Scores inbound leads",
            workflow: {
                name: "Lead Qualification",
                tags: ["sales", "ai"],
            },
        }
        type {}
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_workflows(&program)
        .expect("Failed to extract workflows");

    assert_eq!(compiler.workflows.len(), 1);
    let workflow = &compiler.workflows[0];
    assert_eq!(workflow.type_name, "LeadQualification");
    assert_eq!(workflow.namespace, "::demo::sales");
    assert_eq!(workflow.workflow_val.get_str("name"), "Lead Qualification");
    assert_eq!(
        workflow.workflow_val.get_str("description"),
        "Scores inbound leads"
    );
}

#[test]
fn test_extract_workflow_ignores_function_membership_meta() {
    let source = r#"
        ::demo::sales ns

        on-lead
        meta {workflow: LeadQualification, on-event: "lead:new"}
        fn (event) { event }
    "#;

    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_workflows(&program)
        .expect("Failed to extract workflows");

    assert!(
        compiler.workflows.is_empty(),
        "workflow membership on functions should not create workflow definitions"
    );
}

#[test]
fn test_dependency_analysis_simple() {
    let compiler = Compiler::new();

    // Parse a simple parallel flow: a 1  b 2  c a
    let source = r#"
    test parallel {
        a 1
        b 2
        c a
    }
    "#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    // Get the parallel flow
    if let Some((_, namespace)) = hot_program.namespaces.iter().next()
        && let Some((_, value)) = namespace.scope.vars.iter().next()
        && let Value::Flow(flow) = value
    {
        // Build dependency graph
        let result = compiler.build_dependency_graph(flow);
        assert!(result.is_ok(), "Dependency graph build failed");

        let (graph, _) = result.unwrap();

        // Check dependencies
        assert_eq!(graph.len(), 3, "Should have 3 variables");
        assert_eq!(
            graph.get("a").map(|s| s.len()),
            Some(0),
            "a has no dependencies"
        );
        assert_eq!(
            graph.get("b").map(|s| s.len()),
            Some(0),
            "b has no dependencies"
        );
        assert_eq!(
            graph.get("c").map(|s| s.len()),
            Some(1),
            "c has 1 dependency"
        );
        assert!(graph.get("c").unwrap().contains("a"), "c depends on a");

        // Test topological sort
        let order = compiler.topological_sort(&graph).unwrap();
        assert_eq!(order.len(), 3);

        // a and b should come before c
        let a_pos = order.iter().position(|s| s == "a").unwrap();
        let b_pos = order.iter().position(|s| s == "b").unwrap();
        let c_pos = order.iter().position(|s| s == "c").unwrap();
        assert!(a_pos < c_pos, "a must come before c");
        assert!(b_pos < c_pos, "b must come before c");
    }
}

#[test]
fn test_dependency_analysis_chain() {
    let compiler = Compiler::new();

    // Parse a chain dependency: a 1  b a  c b  d c
    let source = r#"
    test parallel {
        a 1
        b a
        c b
        d c
    }
    "#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    if let Some((_, namespace)) = hot_program.namespaces.iter().next()
        && let Some((_, value)) = namespace.scope.vars.iter().next()
        && let Value::Flow(flow) = value
    {
        let (graph, _) = compiler.build_dependency_graph(flow).unwrap();

        // Test dependency levels
        let order = compiler.topological_sort(&graph).unwrap();
        let levels = compiler.group_by_dependency_levels(&graph, &order);

        assert_eq!(levels.len(), 4, "Should have 4 dependency levels");
        assert_eq!(levels[0], vec!["a"], "Level 0: only a");
        assert_eq!(levels[1], vec!["b"], "Level 1: only b");
        assert_eq!(levels[2], vec!["c"], "Level 2: only c");
        assert_eq!(levels[3], vec!["d"], "Level 3: only d");
    }
}

#[test]
fn test_dependency_analysis_parallel_levels() {
    let compiler = Compiler::new();

    // Parse: a 1  b 2  c a  d b
    // Should create 2 levels: [a, b] then [c, d]
    let source = r#"
    test parallel {
        a 1
        b 2
        c a
        d b
    }
    "#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    if let Some((_, namespace)) = hot_program.namespaces.iter().next()
        && let Some((_, value)) = namespace.scope.vars.iter().next()
        && let Value::Flow(flow) = value
    {
        let (graph, _) = compiler.build_dependency_graph(flow).unwrap();
        let order = compiler.topological_sort(&graph).unwrap();
        let levels = compiler.group_by_dependency_levels(&graph, &order);

        assert_eq!(levels.len(), 2, "Should have 2 dependency levels");
        assert_eq!(levels[0].len(), 2, "Level 0: 2 variables");
        assert!(levels[0].contains(&"a".to_string()));
        assert!(levels[0].contains(&"b".to_string()));
        assert_eq!(levels[1].len(), 2, "Level 1: 2 variables");
        assert!(levels[1].contains(&"c".to_string()));
        assert!(levels[1].contains(&"d".to_string()));
    }
}

#[test]
fn test_dependency_analysis_circular() {
    let compiler = Compiler::new();

    // Parse a circular dependency: a b  b a (should fail)
    let source = r#"
    test parallel {
        a b
        b a
    }
    "#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    if let Some((_, namespace)) = hot_program.namespaces.iter().next()
        && let Some((_, value)) = namespace.scope.vars.iter().next()
        && let Value::Flow(flow) = value
    {
        let (graph, _) = compiler.build_dependency_graph(flow).unwrap();
        let result = compiler.topological_sort(&graph);

        assert!(result.is_err(), "Should detect circular dependency");
        assert!(result.unwrap_err().contains("Circular dependency"));
    }
}

#[test]
fn test_parallel_flow_compilation_and_execution() {
    let mut compiler = Compiler::new();

    // Parse and compile a parallel flow with dependencies
    let source = r#"
    result: All<Map> parallel {
        a 10
        b 20
        c ::hot::math/add(a, 5)
        d ::hot::math/add(b, 10)
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Create a VM and execute
    let program = compiler.get_program_arc();
    let hot_ast = std::sync::Arc::new(crate::lang::ast::HotAst::from_program(hot_program.clone()));
    let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
        program,
        Some(hot_ast),
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        compiler.get_core_variables_arc(),
        None,
    );

    // Execute the program
    let exec_result = vm.execute();
    assert!(
        exec_result.is_ok(),
        "Execution failed: {:?}",
        exec_result.err()
    );

    // Check that the result variable was set correctly
    let result_val = vm.get_var(None, "result");
    assert!(result_val.is_some(), "Result variable not found");

    // The result should be a map with all 4 variables
    if let Some(Val::Map(map)) = result_val {
        assert_eq!(map.len(), 4, "Result map should have 4 entries");
        assert_eq!(map.get(&Val::from("a")), Some(&Val::Int(10)));
        assert_eq!(map.get(&Val::from("b")), Some(&Val::Int(20)));
        assert_eq!(map.get(&Val::from("c")), Some(&Val::Int(15))); // 10 + 5
        assert_eq!(map.get(&Val::from("d")), Some(&Val::Int(30))); // 20 + 10
    } else {
        panic!("Result should be a map");
    }
}

#[test]
fn test_parallel_flow_respects_dependencies() {
    let mut compiler = Compiler::new();

    // Test that a parallel flow with chain dependencies works correctly
    let source = r#"
    result: All<Vec> parallel {
        a 5
        b ::hot::math/mul(a, 2)
        c ::hot::math/mul(b, 2)
        d ::hot::math/mul(c, 2)
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok());

    // Create VM and execute
    let program = compiler.get_program_arc();
    let hot_ast = std::sync::Arc::new(crate::lang::ast::HotAst::from_program(hot_program.clone()));
    let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
        program,
        Some(hot_ast),
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        compiler.get_core_variables_arc(),
        None,
    );

    let exec_result = vm.execute();
    assert!(exec_result.is_ok());

    // Check that the chain was executed in order
    let result_val = vm.get_var(None, "result");
    if let Some(Val::Vec(vec)) = result_val {
        // Should have values: 5, 10, 20, 40 (in some order)
        assert_eq!(vec.len(), 4);
        assert!(vec.contains(&Val::Int(5)));
        assert!(vec.contains(&Val::Int(10)));
        assert!(vec.contains(&Val::Int(20)));
        assert!(vec.contains(&Val::Int(40)));
    } else {
        panic!("Result should be a vector");
    }
}

#[test]
fn test_parallel_flow_worker_resolves_cross_namespace_global() {
    let mut compiler = Compiler::new();

    let source = r#"
    ::parallel::global::dep ns

    GLOBAL_VALUE "ok"

    read-global fn (): Str {
        GLOBAL_VALUE
    }

    ::parallel::global::test ns

    result: All<Map> parallel {
        value ::parallel::global::dep/read-global()
        other "side"
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    let program = compiler.get_program_arc();
    let hot_ast = std::sync::Arc::new(crate::lang::ast::HotAst::from_program(hot_program.clone()));
    let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
        program,
        Some(hot_ast),
        compiler.get_function_mapping_arc(),
        compiler.get_core_functions_arc(),
        compiler.get_type_implementations_arc(),
        compiler.get_core_variables_arc(),
        None,
    );

    let exec_result = vm.execute();
    assert!(
        exec_result.is_ok(),
        "Execution failed: {:?}",
        exec_result.err()
    );

    let result_val = vm.get_var(Some("::parallel::global::test"), "result");
    if let Some(Val::Map(map)) = result_val {
        assert_eq!(map.get(&Val::from("value")), Some(&Val::from("ok")));
        assert_eq!(map.get(&Val::from("other")), Some(&Val::from("side")));
    } else {
        panic!("Result should be a map");
    }
}

#[test]
fn test_compile_simple_variable() {
    let mut compiler = Compiler::new();

    // Parse simple variable assignment
    let source = r#"x 42"#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that bytecode was generated
    let program = compiler.get_program();
    assert!(!program.constants.is_empty(), "Should have constants");

    // Check that the integer constant was added
    let has_int_constant = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(42))));
    assert!(has_int_constant, "Should have integer constant 42");
}

#[test]
fn test_compile_function_definition() {
    let mut compiler = Compiler::new();

    // Parse function definition
    let source = r#"
    add fn (x, y) {
        call-lib(::hot::math/add, [x, y])
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that function was registered (may be with namespace prefix)
    let function_mapping = compiler.get_function_mapping();
    let has_add_function = function_mapping.keys().any(|k| k.contains("add"));
    assert!(has_add_function, "Should have 'add' function in mapping");

    // Check that bytecode program has functions
    let program = compiler.get_program();
    assert!(
        !program.functions.is_empty(),
        "Should have compiled functions"
    );
}

#[test]
fn test_compile_function_call() {
    let mut compiler = Compiler::new();

    // Parse function call
    let source = r#"
    result add(1, 2)
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that constants were created for arguments
    let program = compiler.get_program();
    let has_int_1 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(1))));
    let has_int_2 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(2))));
    assert!(has_int_1, "Should have constant 1");
    assert!(has_int_2, "Should have constant 2");
}

#[test]
fn test_compile_multiple_namespaces() {
    let mut compiler = Compiler::new();

    // Parse code with multiple namespaces
    let source = r#"
    ::hot::math ns
    x 10

    ::hot::test ns
    y 20
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that constants from both namespaces were compiled
    let program = compiler.get_program();
    let has_int_10 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(10))));
    let has_int_20 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(20))));
    assert!(has_int_10, "Should have constant 10");
    assert!(has_int_20, "Should have constant 20");
}

#[test]
fn test_compile_flow_definitions() {
    let mut compiler = Compiler::new();

    // Parse flow definitions
    let source = r#"
    serial_flow serial {
        step1 process(data)
        step2 validate(step1)
    }

    parallel_flow parallel {
        branch1 fast_process(data)
        branch2 slow_process(data)
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check compilation stats - compilation time is always valid (u64 type)
}

#[test]
fn test_compile_type_definitions() {
    let mut compiler = Compiler::new();

    // Parse type definitions (like from time.hot)
    let source = r#"
    Nanosecond type

    Instant type {epochNanoseconds: Int} fn
    (epoch-nanos: Nanosecond): Instant {
        call-lib(::hot::time/instant-from-nanos, [epoch-nanos])
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Compilation should succeed (type definitions may not register functions yet)
}

#[test]
fn test_compile_type_implementations() {
    let mut compiler = Compiler::new();

    // Parse type implementation
    let source = r#"
    TestType -> Int fn (x: TestType): Int { 42 }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Compilation should succeed (type implementations may not be fully implemented yet)
}

#[test]
fn test_compile_additional_code() {
    let mut compiler = Compiler::new();

    // Compile initial code
    let initial_code = r#"x 10"#;
    let result = compiler.compile_additional_code(initial_code);
    assert!(
        result.is_ok(),
        "Initial compilation failed: {:?}",
        result.err()
    );

    // Compile additional code
    let additional_code = r#"y 20"#;
    let result = compiler.compile_additional_code(additional_code);
    assert!(
        result.is_ok(),
        "Additional compilation failed: {:?}",
        result.err()
    );

    // Check that both constants exist
    let program = compiler.get_program();
    let has_int_10 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(10))));
    let has_int_20 = program
        .constants
        .iter()
        .any(|c| matches!(c, Constant::Val(Val::Int(20))));
    assert!(has_int_10, "Should have constant 10");
    assert!(has_int_20, "Should have constant 20");
}

#[test]
fn test_compilation_stats() {
    let mut compiler = Compiler::new();

    // Parse and compile some code
    let source = r#"
    add fn (x, y) { call-lib(::hot::math/add, [x, y]) }
    result add(1, 2)
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check compilation stats - compilation time is always valid (u64 type)

    // Stats should reflect the compiled content
    let program = compiler.get_program();
    assert!(!program.constants.is_empty(), "Should have constants");
    assert!(!program.functions.is_empty(), "Should have functions");
}

#[test]
fn test_compile_variadic_function() {
    let mut compiler = Compiler::new();

    let source = r#"
    concat fn (a, b, ...rest) {
        call-lib(::hot::coll/concat, [a, b, ...rest])
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that the function was compiled with variadic flag
    let program = compiler.get_program();
    let concat_func = program.functions.iter().find(|f| f.name.contains("concat"));
    assert!(concat_func.is_some(), "Should have concat function");
    let func = concat_func.unwrap();
    assert!(func.is_variadic, "Function should be marked as variadic");
    assert_eq!(func.param_names.len(), 3); // a, b, rest
}

#[test]
fn test_compile_lazy_variadic_function() {
    let mut compiler = Compiler::new();

    let source = r#"
    or_test fn (a, lazy ...rest) {
        a
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that the function was compiled with variadic flag and lazy params
    let program = compiler.get_program();
    let or_func = program
        .functions
        .iter()
        .find(|f| f.name.contains("or_test"));
    assert!(or_func.is_some(), "Should have or_test function");
    let func = or_func.unwrap();
    assert!(func.is_variadic, "Function should be marked as variadic");
    assert_eq!(func.lazy_params.len(), 2); // a, rest
    assert!(!func.lazy_params[0], "First param should not be lazy");
    assert!(func.lazy_params[1], "Variadic param (rest) should be lazy");
}

#[test]
fn test_get_function_lazy_params_with_variadic() {
    let mut compiler = Compiler::new();

    // First compile a function with lazy variadic params
    let source = r#"
    test_lazy_var fn (a: Any, lazy ...rest: Any): Any {
        a
    }
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");
    compiler
        .compile_program_unchecked(&mut hot_program)
        .expect("Compilation failed");

    // Now verify the function info is stored correctly
    let program = compiler.get_program();
    let func = program
        .functions
        .iter()
        .find(|f| f.name.contains("test_lazy_var"))
        .expect("Should have test_lazy_var function");

    assert!(func.is_variadic, "Should be variadic");
    assert_eq!(
        func.lazy_params,
        vec![false, true],
        "Should have correct lazy params"
    );
}

#[test]
fn test_namespace_alias_resolution() {
    let mut compiler = Compiler::new();

    // Parse code with namespace alias
    let source = r#"
::test::ns ns

::env ::hot::env

// This should compile with the alias resolved to ::hot::env/get
result ::env/get("TEST", "default")
    "#;
    let mut hot_program = parse_hot(source).expect("Failed to parse");

    // Debug: check that aliases were parsed correctly
    let ns = hot_program
        .namespaces
        .get(&crate::lang::ast::NsPath::from("::test::ns"))
        .expect("Namespace should exist");
    eprintln!("Parsed aliases: {:?}", ns.aliases);
    assert!(
        ns.aliases
            .contains_key(&crate::lang::ast::NsPath::from("::env")),
        "::env alias should exist in parsed namespace"
    );

    // Compile the program
    let result = compiler.compile_program_unchecked(&mut hot_program);
    assert!(result.is_ok(), "Compilation failed: {:?}", result.err());

    // Check that the bytecode contains the resolved function name
    let program = compiler.get_program();

    // Look for a constant that contains the resolved function name
    let has_resolved_name = program.constants.iter().any(|c| {
        if let Constant::Val(Val::Str(s)) = c {
            eprintln!("Found string constant: {}", s);
            s.contains("::hot::env/get")
        } else {
            false
        }
    });

    assert!(
        has_resolved_name,
        "Should have resolved function name ::hot::env/get in constants"
    );
}

/// Helper function to find a variable value by name in a namespace
fn find_var_by_name<'a>(ns: &'a crate::lang::ast::Namespace, name: &str) -> Option<&'a Value> {
    ns.scope.vars.iter().find_map(|(var, value)| {
        if var.sym.name() == name {
            Some(value)
        } else {
            None
        }
    })
}

#[test]
fn test_free_variable_analysis_flow_assignment() {
    let compiler = Compiler::new();

    // Parse a flow with variable assignment pattern: { a 1  a }
    // Hot uses newlines or spaces, not semicolons
    let source = r#"
::test::ns ns

result {
  a 1
  a
}
"#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    // Get the flow from the parsed program
    if let Some(ns) = hot_program
        .namespaces
        .get(&crate::lang::ast::NsPath::from("::test::ns"))
        && let Some(Value::Flow(flow)) = find_var_by_name(ns, "result")
    {
        let free_vars = compiler.analyze_free_variables(&Value::Flow(flow.clone()), &[]);

        // 'a' is defined inside the flow, so it should NOT be a free variable
        assert!(
            !free_vars.contains(&"a".to_string()),
            "Variable 'a' should be bound (defined) inside flow, not free. Got: {:?}",
            free_vars
        );
    } else {
        panic!("Failed to find flow in parsed program");
    }
}

#[test]
fn test_free_variable_analysis_nested_flow_assignment() {
    let compiler = Compiler::new();

    // Parse a flow with multiple variable assignments: { x 1  y x  y }
    let source = r#"
::test::ns ns

result {
  x 1
  y x
  y
}
"#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    if let Some(ns) = hot_program
        .namespaces
        .get(&crate::lang::ast::NsPath::from("::test::ns"))
        && let Some(Value::Flow(flow)) = find_var_by_name(ns, "result")
    {
        let free_vars = compiler.analyze_free_variables(&Value::Flow(flow.clone()), &[]);

        // Both 'x' and 'y' are defined inside the flow
        assert!(
            !free_vars.contains(&"x".to_string()),
            "Variable 'x' should be bound inside flow, not free. Got: {:?}",
            free_vars
        );
        assert!(
            !free_vars.contains(&"y".to_string()),
            "Variable 'y' should be bound inside flow, not free. Got: {:?}",
            free_vars
        );
    } else {
        panic!("Failed to find flow in parsed program");
    }
}

#[test]
fn test_free_variable_analysis_outer_reference() {
    let compiler = Compiler::new();

    // Parse a flow that references an outer variable: { a outer  a }
    // 'outer' should be free, 'a' should be bound
    let source = r#"
::test::ns ns

outer 42

result {
  a outer
  a
}
"#;
    let hot_program = parse_hot(source).expect("Failed to parse");

    if let Some(ns) = hot_program
        .namespaces
        .get(&crate::lang::ast::NsPath::from("::test::ns"))
        && let Some(Value::Flow(flow)) = find_var_by_name(ns, "result")
    {
        let free_vars = compiler.analyze_free_variables(&Value::Flow(flow.clone()), &[]);

        // 'outer' is referenced but not defined in flow - should be free
        assert!(
            free_vars.contains(&"outer".to_string()),
            "Variable 'outer' should be free (referenced from outer scope). Got: {:?}",
            free_vars
        );
        // 'a' is defined inside the flow - should NOT be free
        assert!(
            !free_vars.contains(&"a".to_string()),
            "Variable 'a' should be bound inside flow, not free. Got: {:?}",
            free_vars
        );
    } else {
        panic!("Failed to find flow in parsed program");
    }
}

// ========================================================================
// Send target extraction tests
// ========================================================================

fn compiler_with_core_send() -> Compiler {
    use crate::lang::compiler::core_registry::CoreVariableType;
    let mut compiler = Compiler::new();
    compiler.core_variables.add_core_variable(
        "send".to_string(),
        Value::Val(Val::Null, None),
        "::hot::event".to_string(),
        CoreVariableType::Function,
    );
    compiler
}

#[test]
fn test_extract_send_literal_string() {
    let source = r#"
        handler fn (event) {
            ::hot::event/send("order:created", event)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract send targets");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].event_name, "order:created");
}

#[test]
fn test_extract_send_single_arg() {
    let source = r#"
        handler fn (event) {
            ::hot::event/send("user:signup")
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets[0].event_name, "user:signup");
}

#[test]
fn test_extract_send_with_namespace_alias() {
    let source = r#"
        ::myapp ns
        ::ev ::hot::event

        handler fn (data) {
            ::ev/send("email:queued", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets[0].event_name, "email:queued");
}

#[test]
fn test_extract_send_var_import_alias() {
    let source = r#"
        ::myapp ns
        emit ::hot::event/send

        handler fn (data) {
            emit("order:shipped", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets[0].event_name, "order:shipped");
}

#[test]
fn test_extract_send_variable_resolution() {
    let source = r#"
        event-name "payment:received"
        handler fn (data) {
            ::hot::event/send(event-name, data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets[0].event_name, "payment:received");
}

#[test]
fn test_extract_send_no_sends() {
    let source = r#"
        handler fn (event) {
            event
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(compiler.send_targets.is_empty());
}

#[test]
fn test_extract_send_multiple_in_one_fn() {
    let source = r#"
        handler fn (order) {
            ::hot::event/send("order:validated", order)
            ::hot::event/send("audit:log", order)
            ::hot::event/send("metrics:increment", order)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets.len(), 3);
    let names: Vec<&str> = targets.iter().map(|t| t.event_name.as_str()).collect();
    assert!(names.contains(&"order:validated"));
    assert!(names.contains(&"audit:log"));
    assert!(names.contains(&"metrics:increment"));
}

#[test]
fn test_extract_send_deduplicates() {
    let source = r#"
        handler fn (order) {
            ::hot::event/send("order:created", order)
            ::hot::event/send("order:created", order)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets.len(), 1, "Duplicate sends should be deduped");
}

#[test]
fn test_extract_send_non_send_call_ignored() {
    let source = r#"
        handler fn (event) {
            ::hot::http/get("https://api.example.com")
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(
        compiler.send_targets.is_empty(),
        "Non-send calls should not produce targets"
    );
}

#[test]
fn test_extract_send_dynamic_arg_skipped() {
    let source = r#"
        handler fn (event) {
            ::hot::event/send(event.type, event.data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(
        compiler.send_targets.is_empty(),
        "Dynamic event names should be skipped (not resolvable)"
    );
}

#[test]
fn test_extract_send_multiple_namespaces() {
    let source = r#"
        ::app::orders ns
        on-order fn (event) {
            ::hot::event/send("inventory:reserve", event)
        }

        ::app::payments ns
        on-payment fn (event) {
            ::hot::event/send("receipt:generate", event)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 2);
    let keys: Vec<&String> = compiler.send_targets.keys().collect();
    assert!(keys.iter().any(|k| k.contains("on-order")));
    assert!(keys.iter().any(|k| k.contains("on-payment")));
}

#[test]
fn test_extract_send_in_cond_branches() {
    let source = r#"
        handler fn (order) {
            status cond {
                order.rush => {
                    ::hot::event/send("priority:escalate", order)
                }
                => {
                    ::hot::event/send("queue:standard", order)
                }
            }
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets.len(), 2);
    let names: Vec<&str> = targets.iter().map(|t| t.event_name.as_str()).collect();
    assert!(names.contains(&"priority:escalate"));
    assert!(names.contains(&"queue:standard"));
}

#[test]
fn test_extract_send_wrong_namespace_ignored() {
    let source = r#"
        handler fn (data) {
            ::my::custom/send("not-an-event", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(
        compiler.send_targets.is_empty(),
        "send() from non-event namespace should be ignored"
    );
}

#[test]
fn test_extract_send_empty_program() {
    let source = "";
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(compiler.send_targets.is_empty());
}

#[test]
fn test_extract_send_local_shadow_ignored() {
    let source = r#"
        ::myapp ns

        send fn (event-name: Str, data: Any): Map {
            {sent: event-name, data: data}
        }

        handler fn (data) {
            send("not-a-real-event", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = compiler_with_core_send();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(
        compiler.send_targets.is_empty(),
        "User-defined `send` should shadow core send, producing no send targets"
    );
}

#[test]
fn test_extract_send_local_shadow_qualified_still_works() {
    let source = r#"
        ::myapp ns

        send fn (event-name: Str, data: Any): Map {
            {sent: event-name, data: data}
        }

        handler fn (data) {
            send("ignored", data)
            ::hot::event/send("real:event", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = compiler_with_core_send();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].event_name, "real:event",
        "Qualified ::hot::event/send should still be detected even when unqualified send is shadowed"
    );
}

#[test]
fn test_extract_send_param_named_send_no_false_shadow() {
    let source = r#"
        handler fn (send: Str) {
            ::hot::event/send("real:event", send)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = compiler_with_core_send();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(
        targets[0].event_name, "real:event",
        "A parameter named 'send' should not shadow the core function at namespace scope"
    );
}

#[test]
fn test_extract_send_bare_in_namespace() {
    let source = r#"
        ::myapp ns

        handler fn (data) {
            send("user:created", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = compiler_with_core_send();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(
        targets[0].event_name, "user:created",
        "Bare send() in a namespace should be detected via core registry"
    );
}

#[test]
fn test_extract_send_bare_without_core_registry_not_detected() {
    let source = r#"
        ::myapp ns

        handler fn (data) {
            send("user:created", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert!(
        compiler.send_targets.is_empty(),
        "Without core registry, bare send() as Ref::Var cannot be resolved"
    );
}

#[test]
fn test_extract_send_definition_order_irrelevant() {
    let source = r#"
        ::myapp ns

        handler fn (data) {
            send("event:first", data)
        }

        other fn (data) {
            send("event:second", data)
        }
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = compiler_with_core_send();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(
        compiler.send_targets.len(),
        2,
        "Both functions should have send targets detected"
    );
    let all_events: Vec<&str> = compiler
        .send_targets
        .values()
        .flat_map(|targets| targets.iter().map(|t| t.event_name.as_str()))
        .collect();
    assert!(all_events.contains(&"event:first"));
    assert!(all_events.contains(&"event:second"));
}

#[test]
fn test_summarize_mcp_servers_map_with_url() {
    let mut entry = indexmap::IndexMap::new();
    entry.insert(Val::from("url"), Val::from("https://mcp.example.com/mcp"));
    let servers = Val::Vec(vec![Val::Map(Box::new(entry))]);

    let summary = AgentDef::summarize_mcp_servers(&servers).expect("summary");
    let Val::Vec(items) = summary else {
        panic!("expected Vec, got {:?}", summary);
    };
    assert_eq!(items.len(), 1);
    let Val::Map(first) = &items[0] else {
        panic!("expected Map");
    };
    assert_eq!(first.get(&Val::from("kind")), Some(&Val::from("map")));
    assert_eq!(
        first.get(&Val::from("url")),
        Some(&Val::from("https://mcp.example.com/mcp"))
    );
}

#[test]
fn test_summarize_mcp_servers_named_wrapper() {
    let mut inner = indexmap::IndexMap::new();
    inner.insert(Val::from("url"), Val::from("https://t.example.com/mcp"));

    let mut wrapper = indexmap::IndexMap::new();
    wrapper.insert(Val::from("time"), Val::Map(Box::new(inner)));

    let servers = Val::Vec(vec![Val::Map(Box::new(wrapper))]);
    let summary = AgentDef::summarize_mcp_servers(&servers).expect("summary");
    let Val::Vec(items) = summary else {
        panic!("expected Vec");
    };
    let Val::Map(first) = &items[0] else {
        panic!("expected Map");
    };
    assert_eq!(first.get(&Val::from("kind")), Some(&Val::from("named")));
    assert_eq!(first.get(&Val::from("name")), Some(&Val::from("time")));
    let Val::Map(inner_summary) = first.get(&Val::from("inner")).expect("inner") else {
        panic!("expected inner Map");
    };
    assert_eq!(
        inner_summary.get(&Val::from("kind")),
        Some(&Val::from("map"))
    );
    assert_eq!(
        inner_summary.get(&Val::from("url")),
        Some(&Val::from("https://t.example.com/mcp"))
    );
}

#[test]
fn test_summarize_mcp_servers_qualified_str_treated_as_ref() {
    let servers = Val::Vec(vec![Val::from("::env/weather-config")]);
    let summary = AgentDef::summarize_mcp_servers(&servers).expect("summary");
    let Val::Vec(items) = summary else {
        panic!("expected Vec");
    };
    let Val::Map(first) = &items[0] else {
        panic!("expected Map");
    };
    assert_eq!(first.get(&Val::from("kind")), Some(&Val::from("ref")));
    assert_eq!(
        first.get(&Val::from("target")),
        Some(&Val::from("::env/weather-config"))
    );
}

#[test]
fn test_summarize_mcp_servers_non_vec_returns_none() {
    let servers = Val::from("not a vec");
    assert!(AgentDef::summarize_mcp_servers(&servers).is_none());
}

#[test]
fn test_summarize_mcp_servers_empty_vec() {
    let servers = Val::Vec(vec![]);
    let summary = AgentDef::summarize_mcp_servers(&servers).expect("summary");
    let Val::Vec(items) = summary else {
        panic!("expected Vec");
    };
    assert!(items.is_empty());
}

#[test]
fn test_extract_send_non_function_vars_scanned() {
    let source = r#"
        result ::hot::event/send("startup:init")
    "#;
    let program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .extract_send_targets(&program)
        .expect("Failed to extract");

    assert_eq!(compiler.send_targets.len(), 1);
    let targets = compiler.send_targets.values().next().unwrap();
    assert_eq!(targets[0].event_name, "startup:init");
}

// ========================================================================
// Inline `if()` compilation
// ========================================================================

/// `if(pred, then, else)` calls that statically resolve to a core lazy `if`
/// must compile as an inline cond flow whose BeginFlow carries the origin
/// name (used by the emitter so call-trace search for "if" still works),
/// and the tail-position self-call in a branch must compile to TailCall.
#[test]
fn test_inline_if_emits_origin_named_cond_flow_and_tco() {
    // ::my::bool/if mirrors hot-std's core `if` (`fn cond` with lazy branches);
    // the inline transform keys on a core function whose qualified name ends
    // with "::bool/if".
    let source = r#"
::my::bool ns

if
meta { core: true }
fn
cond (pred: Any, lazy then: Any): Any {
    pred => { do then }
},
cond (pred: Any, lazy then: Any, lazy else: Any): Any {
    pred => { do then }
    => { do else }
}

::main ns

sum-to fn (i: Int, acc: Int): Int {
    if(lte(i, 0), acc, sum-to(sub(i, 1), add(acc, i)))
}
"#;

    let mut program = parse_hot(source).expect("Failed to parse");
    let mut compiler = Compiler::new();
    compiler
        .compile_program_unchecked(&mut program)
        .expect("Failed to compile");

    let bytecode = compiler.get_program();
    let sum_to = bytecode
        .functions
        .iter()
        .find(|f| f.name.ends_with("/sum-to"))
        .expect("sum-to not compiled");

    // The if() must have become an inline cond flow with an origin name...
    let origin_id = sum_to
        .instructions
        .iter()
        .find_map(|i| match i {
            crate::lang::bytecode::Instruction::BeginFlow {
                origin_name: Some(id),
                ..
            } => Some(*id),
            _ => None,
        })
        .expect("expected an origin-named BeginFlow from inline if()");
    match bytecode.constants.get(origin_id as usize) {
        Some(crate::lang::bytecode::Constant::StringRef(s)) => {
            assert!(
                s.ends_with("::bool/if"),
                "origin name should be the core if, got {s}"
            );
        }
        other => panic!("origin_name should be a StringRef, got {other:?}"),
    }

    // ...and the tail-position self-call must be a TailCall (TCO through if).
    assert!(
        sum_to
            .instructions
            .iter()
            .any(|i| matches!(i, crate::lang::bytecode::Instruction::TailCall { .. })),
        "expected TailCall for tail recursion through inline if()"
    );
}
