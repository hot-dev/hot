// Comprehensive Type Checking Error Tests for Hot Language
//
// This test suite validates that the type checker correctly identifies
// and reports various type errors with helpful error messages.

use hot::lang::compiler::Compiler;
use hot::lang::errors::CompilerError;
use hot::lang::parser;
use std::path::Path;

/// Helper function to compile Hot code and extract type errors
fn compile_and_get_type_errors(source: &str) -> Vec<CompilerError> {
    let mut compiler = Compiler::new();

    // Parse the test source code
    let test_program = match parser::parse_hot(source) {
        Ok(program) => program,
        Err(parse_error) => {
            panic!("Parse error in test source: {}", parse_error);
        }
    };

    // Build a comprehensive program that includes hot-std
    let mut comprehensive_program = build_program_with_hot_std(test_program);

    // Attempt compilation with validation (includes type checking)
    match compiler.compile_program(&mut comprehensive_program) {
        Ok(()) => vec![], // No errors
        Err(errors) => errors.errors,
    }
}

/// Build a program that includes hot-std for realistic testing
fn build_program_with_hot_std(test_program: hot::lang::ast::Program) -> hot::lang::ast::Program {
    use hot::lang::ast::Program;
    use std::path::Path;

    let mut comprehensive_program = Program {
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
            && let Ok(hot_std_program) = load_hot_std_from_path(hot_std_path)
        {
            // Add hot-std namespaces
            for (ns_path, namespace) in hot_std_program.namespaces {
                comprehensive_program.namespaces.insert(ns_path, namespace);
            }
            break;
        }
    }

    // Add test program namespaces
    for (ns_path, namespace) in test_program.namespaces {
        comprehensive_program.namespaces.insert(ns_path, namespace);
    }

    comprehensive_program
}

/// Load hot-std from a specific path
fn load_hot_std_from_path(
    hot_std_path: &str,
) -> Result<hot::lang::ast::Program, Box<dyn std::error::Error>> {
    use hot::lang::ast::Program;
    use std::fs;

    let mut program = Program {
        namespaces: Default::default(),
        current_namespace: hot::lang::ast::NsPath::new(),
    };

    // Recursively find all .hot files in hot-std
    fn find_hot_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
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
            && let Ok(file_program) = parser::parse_hot(&content)
        {
            for (ns_path, namespace) in file_program.namespaces {
                program.namespaces.insert(ns_path, namespace);
            }
        }
    }

    Ok(program)
}

/// Helper function to check if errors contain a specific error type
fn has_error_type<F>(errors: &[CompilerError], predicate: F) -> bool
where
    F: Fn(&CompilerError) -> bool,
{
    errors.iter().any(predicate)
}

#[cfg(test)]
mod type_checking_tests {
    use super::*;

    #[test]
    fn test_basic_type_inference() {
        let source = r#"
            ::test ns

            x 42
            y "hello"
            z true
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Basic type inference should not produce errors"
        );
    }

    #[test]
    fn test_function_call_type_checking() {
        let source = r#"
            ::test ns

            add fn (a: Int, b: Int): Int {
                ::hot::math/add(a, b)
            }

            result add(1, 2)
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Valid function call should not produce errors"
        );
    }

    #[test]
    fn test_type_mismatch_in_function_call() {
        let source = r#"
            ::test ns

            add fn (a: Int, b: Int): Int {
                ::hot::math/add(a, b)
            }

            result add("hello", "world")
        "#;

        let errors = compile_and_get_type_errors(source);

        // Should have type mismatch errors for string arguments to int parameters
        assert!(!errors.is_empty(), "Type mismatch should produce errors");
        assert!(
            has_error_type(&errors, |e| matches!(e, CompilerError::TypeMismatch { .. })),
            "Should contain type mismatch error"
        );
    }

    #[test]
    fn test_invalid_union_types() {
        let source = r#"
            ::test ns

            process fn (value: Int | Str | Bool): Any {
                value
            }

            result process(42)
        "#;

        let errors = compile_and_get_type_errors(source);
        // Union types should be valid in Hot
        assert!(
            errors.is_empty(),
            "Valid union types should not produce errors"
        );
    }

    #[test]
    fn test_optional_type_handling() {
        let source = r#"
            ::test ns

            maybe_process fn (value: Int?): Str {
                ::hot::bool/if(value, ::hot::type/Str(value), "null")
            }

            result1 maybe_process(42)
            result2 maybe_process(null)
        "#;

        let errors = compile_and_get_type_errors(source);
        // Optional types should be handled correctly
        assert!(
            errors.is_empty(),
            "Valid optional type usage should not produce errors"
        );
    }

    #[test]
    fn test_generic_type_validation() {
        let source = r#"
            ::test ns

            Container type {
                value: T
            }

            container Container({"value": 42})
        "#;

        let errors = compile_and_get_type_errors(source);

        // Should have generic arity error - T is not declared as a generic parameter
        assert!(
            !errors.is_empty(),
            "Undeclared generic parameter should produce errors"
        );
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::InvalidGenericArity { .. }
            )),
            "Should contain invalid generic arity error"
        );
    }

    #[test]
    fn test_type_implementation_validation() {
        let source = r#"
            ::test ns

            Point type {
                x: Int,
                y: Int
            }

            Point -> Str fn (p: Point): Int {
                ::hot::math/add(p.x, p.y)
            }
        "#;

        let errors = compile_and_get_type_errors(source);

        // Should have implementation error - return type should be Str, not Int
        assert!(
            !errors.is_empty(),
            "Invalid implementation should produce errors"
        );
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::InvalidImplementation { .. }
            )),
            "Should contain invalid implementation error"
        );
    }

    #[test]
    fn test_lazy_parameter_validation() {
        let source = r#"
            ::test ns

            conditional fn (pred: Bool, lazy then: Any, lazy else: Any): Any {
                if(pred, then, else)
            }

            result conditional(true, 42, "hello")
        "#;

        let errors = compile_and_get_type_errors(source);
        // Lazy parameters should be valid in conditional contexts
        assert!(
            errors.is_empty(),
            "Valid lazy parameter usage should not produce errors"
        );
    }

    #[test]
    fn test_variadic_parameter_validation() {
        let source = r#"
            ::test ns

            sum fn (first: Int, ...rest: Int): Int {
                ::hot::coll/reduce(rest, ::hot::math/add, first)
            }

            result sum(1, 2, 3, 4, 5)
        "#;

        let errors = compile_and_get_type_errors(source);
        // Variadic parameters should be valid
        assert!(
            errors.is_empty(),
            "Valid variadic parameter usage should not produce errors"
        );
    }

    #[test]
    fn test_flow_type_validation() {
        let source = r#"
            ::test ns

            process_data fn (data: Vec<Int>): Vec<Str> {
                data |> map((x) { ::hot::type/Str(x) })
            }

            result process_data([1, 2, 3])
        "#;

        let errors = compile_and_get_type_errors(source);
        // Flow with proper type transformations should be valid
        assert!(
            errors.is_empty(),
            "Valid flow type usage should not produce errors"
        );
    }

    #[test]
    fn test_incompatible_flow_branches() {
        let source = r#"
            ::test ns

            mixed_result fn (flag: Bool): Any {
                cond {
                    flag => { 42 }
                    => { "hello" }
                }
            }

            result mixed_result(true)
        "#;

        let errors = compile_and_get_type_errors(source);
        // Mixed return types in conditional flow should be handled as union types
        assert!(
            errors.is_empty(),
            "Mixed flow branches should be handled as union types"
        );
    }

    #[test]
    fn test_circular_type_reference() {
        let source = r#"
            ::test ns

            Node type {
                value: Int,
                next: Node?
            }

            node Node({"value": 1, "next": null})
        "#;

        let errors = compile_and_get_type_errors(source);
        // Self-referential types with optional should be valid
        assert!(
            errors.is_empty(),
            "Self-referential optional types should be valid"
        );
    }

    #[test]
    fn test_missing_type_implementation() {
        let source = r#"
            ::test ns

            CustomType type {
                value: Str
            }

            custom CustomType({"value": "test"})
            result ::hot::type/Str(custom)
        "#;

        let errors = compile_and_get_type_errors(source);

        // Should have missing implementation error - CustomType doesn't implement Str
        assert!(
            !errors.is_empty(),
            "Missing type implementation should produce errors"
        );
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::MissingTypeImplementation { .. }
            )),
            "Should contain missing type implementation error"
        );
    }

    #[test]
    fn test_namespace_qualified_types() {
        let source = r#"
            ::test ns

            use_core_types fn (): Any {
                int_val ::hot::type/Int(42)
                str_val ::hot::type/Str("hello")
                bool_val ::hot::type/Bool(true)
                [int_val, str_val, bool_val]
            }

            result use_core_types()
        "#;

        let errors = compile_and_get_type_errors(source);
        // Namespace-qualified core types should be valid
        assert!(
            errors.is_empty(),
            "Namespace-qualified core types should be valid"
        );
    }

    #[test]
    fn test_complex_generic_types() {
        let source = r#"
            ::test ns

            process_map fn (data: Map<Str, Int>): Vec<Str> {
                data
                |> ::hot::coll/keys
                |> ::hot::coll/map((key) { key })
            }

            result process_map({"a": 1, "b": 2})
        "#;

        let errors = compile_and_get_type_errors(source);
        // Complex generic type usage should be valid
        assert!(
            errors.is_empty(),
            "Complex generic type usage should be valid"
        );
    }

    #[test]
    fn test_type_annotation_validation() {
        let source = r#"
            ::test ns

            invalid_annotation fn (x: NonExistentType): Int {
                42
            }
        "#;

        let errors = compile_and_get_type_errors(source);

        // Type annotation validation is working correctly

        // Should have unresolved type error
        assert!(
            !errors.is_empty(),
            "Invalid type annotation should produce errors"
        );
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::UnresolvedType { .. }
            )),
            "Should contain unresolved type error"
        );
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_hot_std_type_compatibility() {
        let source = r#"
            ::test ns

            test_math fn (): Bool {
                x ::hot::math/add(1, 2)
                y ::hot::math/mul(x, 3)
                ::hot::cmp/eq(y, 9)
            }

            test_collections fn (): Vec<Str> {
                numbers [1, 2, 3, 4, 5]
                numbers
                |> ::hot::coll/map((n) { ::hot::type/Str(n) })
                |> ::hot::coll/filter((s) { ::hot::str/contains(s, "2") })
            }

            result1 test_math()
            result2 test_collections()
        "#;

        let errors = compile_and_get_type_errors(source);
        // Integration with hot-std should work correctly
        assert!(
            errors.is_empty(),
            "Integration with hot-std should not produce type errors"
        );
    }

    #[test]
    fn test_test_framework_compatibility() {
        let source = r#"
            ::test meta ["test"] ns

            test_assertions meta ["test"] fn () {
                ::hot::test/assert-eq(1, 1, "one equals one")
                ::hot::test/assert(true, "true is truthy")
                ::hot::test/assert-eq("hello", "hello")
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        // Test framework usage should be type-safe
        assert!(
            errors.is_empty(),
            "Test framework usage should not produce type errors"
        );
    }

    /// A bare `%` placeholder that cannot find an enclosing `Fn`-typed
    /// parameter slot is an Option B compile error. The user must wrap
    /// the expression with `%(expr)` to opt into a first-class lambda.
    #[test]
    fn test_placeholder_with_no_fn_slot_errors() {
        let source = r#"
            ::test ns

            // `add` parameters are Int, not Fn — the `%` has nowhere
            // to bind and should produce a CallLibError on `%`.
            broken add(%, 1)
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            !errors.is_empty(),
            "Bare % outside any Fn slot should produce a compile error"
        );
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::CallLibError { func_name, .. } if func_name == "%"
            )),
            "Should contain a CallLibError for `%` placeholder. Got: {:?}",
            errors
        );
    }

    /// Wrapping the same expression with `%(expr)` makes it a legal
    /// first-class lambda value and the error goes away.
    #[test]
    fn test_placeholder_explicit_boundary_compiles() {
        let source = r#"
            ::test ns

            invoke fn (x: Int, f: Fn): Int { f(x) }

            sq %(::hot::math/mul(%, %))
            result invoke(4, sq)
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "%(expr) should compile cleanly. Got: {:?}",
            errors
        );
    }

    /// User-defined HOFs (anything with a `Fn`-typed parameter) should
    /// drive placeholder wrapping just like built-in HOFs do — there is
    /// no hardcoded list of HOF names anymore.
    #[test]
    fn test_placeholder_user_defined_hof_compiles() {
        let source = r#"
            ::test ns

            apply-twice fn (x: Int, f: Fn): Int { f(f(x)) }

            result apply-twice(3, ::hot::math/add(%, 2))
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Placeholder inside a user-defined HOF should compile. Got: {:?}",
            errors
        );
    }
}
