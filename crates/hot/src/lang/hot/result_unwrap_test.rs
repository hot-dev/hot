// Tests for automatic Result unwrapping behavior

use crate::lang::ast::Program;
use crate::lang::compiler::Compiler;
use crate::lang::parser;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use std::path::Path;

/// Load hot-std from a specific path
fn load_hot_std_from_path(hot_std_path: &str) -> Result<Program, std::io::Error> {
    use std::fs;
    use std::path::Path;

    let mut program = Program {
        namespaces: Default::default(),
        current_namespace: crate::lang::ast::NsPath::new(),
    };

    // Recursively find all .hot files in hot-std (excluding test directories)
    fn find_hot_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let path_str = path.to_string_lossy();

            // Skip test directories and test.hot files
            if path_str.contains("/test/")
                || path_str.ends_with("/test")
                || path_str.ends_with("/test.hot")
            {
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

/// Helper to compile and execute Hot code with hot-std included
fn compile_and_run_with_std(source: &str) -> Result<Val, String> {
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
        None,
    );

    vm.execute().map_err(|e| format!("{:?}", e))
}

#[test]
fn test_ok_result_unwraps_in_function_call() {
    // Test that an "ok" Result automatically unwraps when passed to a function
    let source = r#"
        ::test ns

        make-result fn () {
            ::hot::type/ok(42)
        }

        add-ten fn (x: Int) {
            ::hot::math/add(x, 10)
        }

        result make-result()
        final add-ten(result)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Val::Int(52));
}

#[test]
fn test_err_result_fails_in_function_call() {
    // Test that an "err" Result automatically triggers fail() when passed to a function
    let source = r#"
        ::test ns

        make-result fn () {
            ::hot::type/err("Something went wrong")
        }

        add-ten fn (x: Int) {
            ::hot::math/add(x, 10)
        }

        result make-result()
        final add-ten(result)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_err(), "Should fail when unwrapping error Result: {:?}", result);
    let err = result.unwrap_err();
    assert!(
        err.contains("Result error") || err.contains("Something went wrong"),
        "Error should mention Result error, got: {}",
        err
    );
}

#[test]
fn test_ok_result_unwraps_in_template_literal() {
    // Test that an "ok" Result automatically unwraps in template literals
    let source = r#"
        ::test ns

        make-result fn () {
            ::hot::type/ok("world")
        }

        result make-result()
        message `Hello, ${result}!`
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Val::from("Hello, world!"));
}

#[test]
fn test_err_result_fails_in_template_literal() {
    // Test that an "err" Result automatically triggers fail() in template literals
    let source = r#"
        ::test ns

        make-result fn () {
            ::hot::type/err("template error")
        }

        result make-result()
        message `Hello, ${result}!`
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_err(), "Should fail when unwrapping error Result in template: {:?}", result);
    let err = result.unwrap_err();
    assert!(
        err.contains("Result error") || err.contains("template error"),
        "Error should mention Result error, got: {}",
        err
    );
}

#[test]
fn test_nested_result_unwrapping() {
    // Test that Results unwrap in nested function calls
    let source = r#"
        ::test ns

        make-result fn () {
            ::hot::type/ok(5)
        }

        double fn (x: Int) {
            ::hot::math/mul(x, 2)
        }

        add fn (x: Int, y: Int) {
            ::hot::math/add(x, y)
        }

        r1 make-result()
        r2 ::hot::type/ok(3)
        final add(double(r1), r2)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    // double(5) = 10, add(10, 3) = 13
    assert_eq!(result.unwrap(), Val::Int(13));
}

#[test]
fn test_non_result_values_pass_through() {
    // Test that non-Result values are not affected
    let source = r#"
        ::test ns

        add-ten fn (x: Int) {
            ::hot::math/add(x, 10)
        }

        value 42
        final add-ten(value)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Val::Int(52));
}

#[test]
fn test_result_in_map() {
    // Test Result unwrapping doesn't affect map values (only function args and template literals)
    let source = r#"
        ::test ns

        result ::hot::type/ok(42)
        data {
            $value: result
        }
        final data
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);

    // The Result should be stored as-is in the map (not unwrapped)
    if let Val::Map(m) = result.unwrap() {
        let value = m.get(&Val::from("$value")).unwrap();
        // It should be a Result type with $type field
        if let Val::Map(result_map) = value {
            assert!(
                result_map.contains_key(&Val::from("$type")),
                "Should contain $type field for Result variant union"
            );
        } else {
            panic!("Expected a Map for Result type");
        }
    } else {
        panic!("Expected a Map");
    }
}

#[test]
fn test_multiple_result_args() {
    // Test that multiple Result arguments all unwrap correctly
    let source = r#"
        ::test ns

        add fn (x: Int, y: Int) {
            ::hot::math/add(x, y)
        }

        r1 ::hot::type/ok(10)
        r2 ::hot::type/ok(20)
        final add(r1, r2)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Val::Int(30));
}

#[test]
fn test_err_result_fails_immediately() {
    // Test that the first error Result encountered causes immediate failure
    let source = r#"
        ::test ns

        add fn (x: Int, y: Int) {
            ::hot::math/add(x, y)
        }

        r1 ::hot::type/err("first error")
        r2 ::hot::type/ok(20)
        final add(r1, r2)
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_err(), "Should fail: {:?}", result);
    let err = result.unwrap_err();
    assert!(
        err.contains("first error") || err.contains("Result error"),
        "Error should mention the first error, got: {}",
        err
    );
}

#[test]
fn test_ok_result_in_chained_calls() {
    // Test Result unwrapping in a chain of function calls
    let source = r#"
        ::test ns

        make-result fn (x: Int) {
            ::hot::type/ok(x)
        }

        add-five fn (x: Int) {
            ::hot::math/add(x, 5)
        }

        final add-five(add-five(make-result(10)))
    "#;

    let result = compile_and_run_with_std(source);
    assert!(result.is_ok(), "Should succeed: {:?}", result);
    // make-result(10) = ok(10), add-five(10) = 15, add-five(15) = 20
    assert_eq!(result.unwrap(), Val::Int(20));
}
