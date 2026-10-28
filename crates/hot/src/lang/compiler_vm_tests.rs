//! Compiler tests that execute the compiled bytecode on the VM. The rest of
//! the compiler's tests live with it in `hot_lang`.

use crate::lang::compiler::Compiler;
use crate::lang::parser::parse_hot;
use crate::val::Val;

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
    assert!(exec_result.is_ok(), "{exec_result:?}");

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
