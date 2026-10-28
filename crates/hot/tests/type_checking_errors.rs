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

/// Helper function to compile Hot code and extract non-fatal compiler warnings
fn compile_and_get_warnings(source: &str) -> Vec<CompilerError> {
    let mut compiler = Compiler::new();

    let test_program = match parser::parse_hot(source) {
        Ok(program) => program,
        Err(parse_error) => {
            panic!("Parse error in test source: {}", parse_error);
        }
    };

    let mut comprehensive_program = build_program_with_hot_std(test_program);
    let _ = compiler.compile_program(&mut comprehensive_program);
    compiler.diagnostics().warnings.clone()
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

/// Compile `files` (path, source) after hot-std the way the engine does: each
/// file parsed with its path and merged member-wise, so namespaces can span
/// files. Returns the compile errors.
fn compile_files_and_get_errors(files: &[(&str, &str)]) -> Vec<CompilerError> {
    use hot::lang::engine::discover::merge_program_namespaces_strict;

    let hot_std_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg/hot-std/src");
    let hot_std_files =
        hot::lang::engine::Engine::discover_hot_files(&hot_std_src.to_string_lossy()).unwrap();
    assert!(
        !hot_std_files.is_empty(),
        "no hot-std files in {:?}",
        hot_std_src
    );

    let mut sources = Vec::new();
    for path in &hot_std_files {
        sources.push((path.clone(), std::fs::read_to_string(path).unwrap()));
    }
    for (path, source) in files {
        sources.push((path.to_string(), source.to_string()));
    }

    let mut program = hot::lang::ast::Program {
        namespaces: Default::default(),
        current_namespace: hot::lang::ast::NsPath::new(),
    };
    for (path, source) in &sources {
        let parsed = parser::parse_hot_file(source, path)
            .unwrap_or_else(|e| panic!("Parse error in {}: {}", path, e));
        merge_program_namespaces_strict(&mut program.namespaces, parsed.namespaces).unwrap();
    }

    match Compiler::new().compile_program(&mut program) {
        Ok(()) => vec![],
        Err(errors) => errors.errors,
    }
}

/// Assert `errors` has a nested-placeholder error on `nested_line` and a
/// no-`Fn`-slot `%` error at `loose_line:loose_column`, both in `file`.
fn assert_placeholder_errors_in(
    errors: &[CompilerError],
    file: &str,
    nested_line: usize,
    (loose_line, loose_column): (usize, usize),
) {
    let find = |predicate: fn(&CompilerError) -> bool| {
        errors
            .iter()
            .find(|e| predicate(e))
            .and_then(|e| e.location())
            .map(|l| (l.file.clone(), l.line, l.column))
    };
    let file = Some(std::path::PathBuf::from(file));

    let nested = find(|e| matches!(e, CompilerError::NestedPlaceholder { .. }));
    assert_eq!(
        nested.map(|(f, line, _)| (f, line)),
        Some((file.clone(), nested_line)),
        "nested-placeholder location. Errors: {:?}",
        errors
    );
    let loose =
        find(|e| matches!(e, CompilerError::CallLibError { func_name, .. } if func_name == "%"));
    assert_eq!(
        loose,
        Some((file, loose_line, loose_column)),
        "no-`Fn`-slot `%` location. Errors: {:?}",
        errors
    );
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
    fn test_type_implementation_accepts_struct_shaped_map_return() {
        let source = r#"
            ::test ns

            Source type { value: Int }
            Target type { value: Int }

            Source -> Target fn (s: Source): Target {
                {value: s.value}
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Implementation returning a matching record should compile. Got: {:?}",
            errors
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

    #[test]
    fn test_struct_constraint_preserves_known_extra_fields() {
        let source = r#"
            ::test ns

            A type { a: Int }
            B type { a: Int, b: Int }

            m {a: 1, b: 2}
            x: A m
            y: B x
            z x.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Known extra fields should be preserved through struct constraints. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Preserved known field access should not warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_constructor_preserves_known_extra_fields() {
        let source = r#"
            ::test ns

            A type { a: Int }

            x A({a: 1, b: 2})
            z x.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Constructor should compile: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Constructor should preserve known extra field b. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_unknown_field_on_narrow_struct_is_warning() {
        let source = r#"
            ::test ns

            A type { a: Int }

            read-b fn (value: A): Any {
                value.b
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Unknown field rollout should not fail compilation. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            has_error_type(&warnings, |e| matches!(
                e,
                CompilerError::UnknownField {
                    type_name,
                    field_name,
                    ..
                } if type_name == "A" && field_name == "b"
            )),
            "Expected unknown-field warning for A.b. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_runtime_metadata_fields_warn_as_unknown_fields() {
        let source = r#"
            ::test ns

            Point type { x: Int, y: Int }

            p Point({x: 1, y: 2})
            tag p.$type
            payload p.$val
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Unknown field access remains a warning. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            ["$type", "$val"]
                .iter()
                .all(|expected| warnings.iter().any(|warning| {
                    matches!(
                        warning,
                        CompilerError::UnknownField { field_name, .. } if field_name == expected
                    )
                })),
            "Runtime metadata names should be ordinary unknown fields. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_enum_variant_constructor_is_not_unknown_field() {
        let source = r#"
            ::test ns

            Status enum {
                Pending,
                Active,
                Closed
            }

            active Status.Active
            pending Status.Pending
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Enum variant constructor access should compile. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Enum variant constructors should not warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_enum_variant_constructor_survives_homonym_struct() {
        let source = r#"
            ::other ns

            Status type { code: Int }
            status Status({code: 200})

            ::test ns

            Status enum {
                Pending,
                Active,
                Closed
            }

            active Status.Active
            pending Status.Pending
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Enum variant access should compile despite homonym struct. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Homonym struct should not make enum variants warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_struct_field_still_warns_after_enum_homonym() {
        let source = r#"
            ::enum_ns ns

            Status enum {
                Active
            }

            ::struct_ns ns

            Status type { code: Int }
            status Status({code: 200})
            value status.Active
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Unknown field rollout should not fail compilation. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            has_error_type(&warnings, |e| matches!(
                e,
                CompilerError::UnknownField {
                    field_name,
                    ..
                } if field_name == "Active"
            )),
            "Expected struct Status.Active to remain an unknown field. Got: {:?}",
            warnings
        );
    }

    fn unknown_fields(warnings: &[CompilerError]) -> Vec<(String, String)> {
        warnings
            .iter()
            .filter_map(|w| match w {
                CompilerError::UnknownField {
                    type_name,
                    field_name,
                    ..
                } => Some((type_name.clone(), field_name.clone())),
                _ => None,
            })
            .collect()
    }

    fn duplicate_core_names(warnings: &[CompilerError]) -> Vec<(String, String, String)> {
        warnings
            .iter()
            .filter_map(|w| match w {
                CompilerError::DuplicateCoreName {
                    name,
                    shadowed,
                    winner,
                    ..
                } => Some((name.clone(), shadowed.clone(), winner.clone())),
                _ => None,
            })
            .collect()
    }

    // hot-std declares a core struct `Next { value, done }` (`::hot::iter`).
    // A project core enum of the same name overrides it at runtime (the last
    // core declaration wins), so the checker must read `Next` as the enum too,
    // and say that hot-std's `Next` is shadowed.
    #[test]
    fn test_project_core_enum_overriding_hot_std_type_resolves_as_enum() {
        let source = r#"
            ::a ns

            Ask meta {core: true} type { step: Str }
            Next meta {core: true} enum { Open(Ask), NotYet, Done(Str) }

            ::b ns

            wait fn (): Next { Next.NotYet }
            is-not-yet fn (n: Next): Bool { eq(n, Next.NotYet) }
            open-map fn (): Next { Next.Open({step: "review"}) }
            done fn (): Next { Next.Done("ok") }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert_eq!(
            unknown_fields(&warnings),
            vec![],
            "The overriding enum's variants should not warn. Got: {:?}",
            warnings
        );
        assert_eq!(
            duplicate_core_names(&warnings),
            vec![(
                "Next".to_string(),
                "::hot::iter".to_string(),
                "::a".to_string()
            )],
            "Overriding hot-std's core Next should warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_named_args_reach_project_core_enum_over_hot_std_homonym() {
        let source = r#"
            ::a ns

            Ask meta {core: true} type { step: Str }
            Next meta {core: true} enum { Open(Ask), NotYet, Done(Str) }

            ::b ns

            open-named fn (): Next { Next.Open(step: "review") }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Named arguments should reach the project's core enum. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert_eq!(
            unknown_fields(&warnings),
            vec![],
            "Named variant construction should not warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_core_type_declared_in_two_namespaces_warns() {
        let source = r#"
            ::first ns

            Verdict meta {core: true} type { ok: Bool }

            ::second ns

            Verdict meta {core: true} enum { Pass, Fail }

            ::third ns

            pass fn (): Verdict { Verdict.Pass }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert_eq!(
            duplicate_core_names(&warnings),
            vec![(
                "Verdict".to_string(),
                "::first".to_string(),
                "::second".to_string()
            )],
            "A second core Verdict should warn. Got: {:?}",
            warnings
        );
        assert_eq!(
            unknown_fields(&warnings),
            vec![],
            "The last core Verdict is the one used. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_single_core_type_does_not_warn_duplicate() {
        let source = r#"
            ::first ns

            Phase meta {core: true} enum { Draft, Final }

            ::second ns

            Phase enum { Open, Closed }
            open fn (): Phase { Phase.Open }
        "#;

        let warnings = compile_and_get_warnings(source);
        assert_eq!(
            duplicate_core_names(&warnings),
            vec![],
            "A non-core homonym is not a duplicate core name. Got: {:?}",
            warnings
        );
    }

    // A core type declared in two hot-std namespaces resolves by load order,
    // so only deliberate pairs may exist: `::hot::exec` re-declares `run`'s
    // `Failure` and `Cancellation` as its context-aware forms.
    #[test]
    fn test_hot_std_core_types_are_declared_once() {
        use hot::lang::compiler::core_registry::CoreVariableRegistry;
        use std::collections::{BTreeMap, BTreeSet};

        let program = build_program_with_hot_std(parser::parse_hot("::test ns").unwrap());
        let mut declared: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (ns_path, namespace) in &program.namespaces {
            for (var, value) in &namespace.scope.vars {
                if matches!(value, hot::lang::ast::Value::TypeDef(_))
                    && CoreVariableRegistry::has_core_metadata(var)
                {
                    declared
                        .entry(var.sym.name().to_string())
                        .or_default()
                        .insert(ns_path.to_string());
                }
            }
        }
        let duplicated: Vec<&str> = declared
            .iter()
            .filter(|(_, namespaces)| namespaces.len() > 1)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            duplicated,
            vec!["Cancellation", "Failure"],
            "hot-std core types declared in more than one namespace: {:?}",
            declared
        );
    }

    #[test]
    fn test_core_enum_variant_from_other_namespace_is_not_unknown_field() {
        let source = r#"
            ::a ns

            Ask meta {core: true} type { step: Str }
            Then meta {core: true} enum { Open(Ask), NotYet, Done(Str) }

            ::b ns

            wait fn (): Then { Then.NotYet }
            open-named fn (): Then { Then.Open(step: "review") }
            open-map fn (): Then { Then.Open({step: "review"}) }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Core enum variants should not warn. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_match_arm_exposes_variant_payload_fields() {
        let source = r#"
            ::a ns

            Ask meta {core: true} type { step: Str }
            Then meta {core: true} enum { Open(Ask), NotYet, Done(Str) }

            Gate type { level: Int }
            Check enum { Pass(Gate), Skip }

            local-level fn (c: Check): Int {
                match c {
                    Check.Pass => { c.level }
                    _ => { 0 }
                }
            }

            ::b ns

            opens fn (n: Then, step: Str): Bool {
                match n {
                    Then.Open => { eq(n.step, step) }
                    _ => { false }
                }
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Payload fields inside a variant arm should resolve. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_match_arm_misspelled_payload_field_still_warns() {
        let source = r#"
            ::a ns

            Ask meta {core: true} type { step: Str }
            Then meta {core: true} enum { Open(Ask), NotYet, Done(Str) }

            Gate type { level: Int }
            Check enum { Pass(Gate), Skip }

            local-level fn (c: Check): Int {
                match c {
                    Check.Pass => { c.levle }
                    _ => { 0 }
                }
            }

            ::b ns

            opens fn (n: Then, step: Str): Bool {
                match n {
                    Then.Open => { eq(n.stepp, step) }
                    _ => { false }
                }
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        let mut found = unknown_fields(&warnings);
        found.sort();
        assert_eq!(
            found,
            vec![
                ("Ask".to_string(), "stepp".to_string()),
                ("Gate".to_string(), "levle".to_string()),
            ],
            "Misspelled payload fields should warn against the payload type. Got: {:?}",
            warnings
        );
    }

    // The arm's binding holds the payload at run time, as the subject does,
    // so it is checked against the payload type too.
    #[test]
    fn test_match_arm_binding_holds_variant_payload() {
        let source = r#"
            ::a ns

            Gate type { level: Int }
            Check enum { Pass(Gate), Skip }

            level fn (c: Check): Int {
                match c {
                    Check.Pass (g) => { g.level }
                    _ => { 0 }
                }
            }

            misspelled fn (c: Check): Int {
                match c {
                    Check.Pass (g) => { g.levle }
                    _ => { 0 }
                }
            }
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Should compile. Got: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert_eq!(
            unknown_fields(&warnings),
            vec![("Gate".to_string(), "levle".to_string())],
            "The binding should be checked against the payload type. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_explicit_map_annotation_keeps_dynamic_field_access() {
        let source = r#"
            ::test ns

            m: Map {a: 1}
            value m.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Map access should compile: {:?}", errors);

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Explicit Map access should remain dynamic. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_known_fields_survive_local_var_alias() {
        let source = r#"
            ::test ns

            A type { a: Int }

            m {a: 1, b: 2}
            alias m
            x: A alias
            value x.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Local var alias should preserve known fields. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Local var alias should not warn for preserved b. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_known_fields_survive_namespace_var_import_and_alias_resolution() {
        let source = r#"
            ::producer ns

            A type { a: Int }
            m {a: 1, b: 2}

            ::consumer ns
            ::p ::producer

            A ::p/A
            imported ::p/m
            x: A imported
            value x.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Namespace var import through alias should preserve known fields. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Namespace var import through alias should not warn for b. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_struct_fields_resolve_namespace_local_type_alias_before_homonym() {
        let source = r#"
            ::ai::session ns

            Session type { id: Str }
            Identity type { id: Str }

            ::other ns

            Session type { name: Str }

            ::ai::message ns

            Session ::ai::session/Session
            Identity ::ai::session/Identity

            Message type {
                session: Session,
                sender: Identity
            }

            sess Session({id: "chan-1"})
            user Identity({id: "U1"})
            m Message({session: sess, sender: user})
            value m.session.id
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Namespace-local type aliases should resolve before homonym short names. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Alias-resolved field access should not warn for m.session.id. Got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_known_fields_survive_exported_same_namespace_alias() {
        let source = r#"
            ::producer ns

            A type { a: Int }
            m {a: 1, b: 2}
            exported m

            ::consumer ns
            ::p ::producer

            A ::p/A
            imported ::p/exported
            x: A imported
            value x.b
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Exported same-namespace alias should preserve known fields. Got: {:?}",
            errors
        );

        let warnings = compile_and_get_warnings(source);
        assert!(
            warnings.is_empty(),
            "Exported same-namespace alias should not warn for b. Got: {:?}",
            warnings
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

    /// An implicit `%` lambda may not contain another implicit `%` lambda:
    /// in `map(xs, filter(%, gt(%, 0)))` the two `%` would refer to
    /// different parameters.
    #[test]
    fn test_nested_implicit_placeholders_error() {
        for source in [
            r#"
            ::test ns

            nested map([[1, -2]], filter(%, gt(%, 0)))
            "#,
            r#"
            ::test ns

            piped [[1, -2]] |> map(filter(%, gt(%, 0)))
            "#,
            // A lambda written around the inner `%` doesn't separate them.
            r#"
            ::test ns

            grouped map([[1, -2]], apply(%, (g) { filter(g, gt(%, 0)) }))
            "#,
        ] {
            let errors = compile_and_get_type_errors(source);
            assert!(
                has_error_type(&errors, |e| matches!(
                    e,
                    CompilerError::NestedPlaceholder {
                        function,
                        parameter,
                        inner_function: Some(inner),
                        inner_location: Some(_),
                        ..
                    } if function == "map"
                        && parameter.as_deref() == Some("func")
                        && inner == "filter"
                )),
                "Expected a nested-placeholder error for {}. Got: {:?}",
                source,
                errors
            );
        }
    }

    /// Both placeholder errors name the file the `%` is in. Every file
    /// without an `ns` declaration shares the root namespace `::`, which a
    /// hot-std file creates first, so the namespace's own file is wrong.
    #[test]
    fn test_placeholder_errors_name_file_in_root_namespace() {
        let errors = compile_files_and_get_errors(&[(
            "probe/no-ns.hot",
            "nested map([[1, -2]], filter(%, gt(%, 0)))\nloose add(%, 1)\n",
        )]);
        assert_placeholder_errors_in(&errors, "probe/no-ns.hot", 1, (2, 11));
    }

    /// Same for a namespace whose code is split across several files.
    #[test]
    fn test_placeholder_errors_name_file_in_split_namespace() {
        let errors = compile_files_and_get_errors(&[
            ("probe/split-a.hot", "::probe::split ns\n\nfirst 1\n"),
            (
                "probe/split-b.hot",
                "::probe::split ns\n\nnested map([[1, -2]], filter(%, gt(%, 0)))\nloose add(%, 1)\n",
            ),
        ]);
        assert_placeholder_errors_in(&errors, "probe/split-b.hot", 3, (4, 11));
    }

    /// An unresolved name points at the name itself. A variable, a path's
    /// base and a call all used to take the position of the token after it.
    #[test]
    fn test_unresolved_names_point_at_the_name() {
        let errors = compile_files_and_get_errors(&[(
            "probe/names.hot",
            "::probe::names ns\n\nf fn (): Int {\n    add(missing-var, 1)\n}\n\ng fn (): Int {\n    missing-fn(1)\n}\n\nh fn (): Any {\n    missing-map.field\n}\n",
        )]);
        let at = |name: &str| {
            errors
                .iter()
                .find(|e| e.to_string().contains(name))
                .and_then(|e| e.location())
                .map(|l| (l.line, l.column, l.length))
        };
        assert_eq!(at("missing-var"), Some((4, 9, 11)), "errors: {errors:?}");
        assert_eq!(at("missing-fn"), Some((8, 5, 10)), "errors: {errors:?}");
        assert_eq!(at("missing-map"), Some((12, 5, 11)), "errors: {errors:?}");
    }

    /// An explicit inner function, `%(...)` or `(x) { ... }`, is allowed
    /// inside an implicit `%` lambda.
    #[test]
    fn test_explicit_inner_lambda_inside_placeholder_compiles() {
        let source = r#"
            ::test ns

            explicit-boundary map([[1, -2]], filter(%, %(gt(%, 0))))
            explicit-lambda map([[1, -2]], filter(%, (n) { gt(n, 0) }))
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Explicit inner lambdas should compile. Got: {:?}",
            errors
        );
    }

    /// Single-level placeholders keep working, including a `%` that passes
    /// through calls with no `Fn` slot and pipe stages.
    #[test]
    fn test_single_level_placeholders_compile() {
        let source = r#"
            ::test ns

            items [{value: 3}, {value: 1}]
            counted length(filter([1, 5, 9], gt(%, 3)))
            sorted sort(map(items, %.value))
            doubled map([1, 2], add(%, %))
            emails [{active: true, email: "a@x"}] |> filter(%.active) |> map(%.email)
        "#;

        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Single-level placeholders should compile. Got: {:?}",
            errors
        );
    }

    /// The code of a `where` constraint diagnostic, when `error` is one.
    fn constraint_code(error: &CompilerError) -> Option<&'static str> {
        match error {
            CompilerError::Constraint { code, .. } => Some(code),
            _ => None,
        }
    }

    fn has_constraint_code(errors: &[CompilerError], code: &str) -> bool {
        errors.iter().any(|e| constraint_code(e) == Some(code))
    }

    /// Every `where` form compiles cleanly when its predicates are pure,
    /// return a Bool, and no literal fails them.
    #[test]
    fn test_where_constraints_compile() {
        let source = r#"
            ::test ns

            Port type Int where between(%, 1, 65535)
            Cents type Int where gte(%, 0)
            Window type { start: Int, finish: Int } where lte(%.start, %.finish)
            Order type {
                items: Vec<Str> where between(length(%), 1, 100),
                total: Cents,
            }
            listen fn (port: Port): Str { "ok" }
            retry-delay fn (attempt: Int where between(%, 1, 10)): Int where lte(%, 30000) {
                mul(attempt, 1000)
            }
            withdraw fn (amount: Int, balance: Int)
                where and(gt(%.amount, 0), lte(%.amount, %.balance))
                : Int where gte(%, 0)
            {
                sub(balance, amount)
            }
            max-retries: Int 3 where between(%, 0, 10)
            a listen(8080)
            b Port(443)
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Expected no errors. Got: {:?}", errors);
    }

    /// A `where` predicate is an ordinary function argument under the `%`
    /// rule, so a nested implicit lambda is rejected.
    #[test]
    fn test_where_predicate_rejects_nested_placeholder() {
        let source = r#"
            ::test ns
            Positives type Vec<Int> where all(%, gt(%, 0))
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            has_error_type(&errors, |e| matches!(
                e,
                CompilerError::NestedPlaceholder { function, .. } if function == "::hot::type/predicate"
            )),
            "Expected nested-placeholder. Got: {:?}",
            errors
        );
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("a `where` predicate")),
            "The message should name the `where` predicate. Got: {:?}",
            errors
        );

        let source = r#"
            ::test ns
            Positives type Vec<Int> where all(%, (n) { gt(n, 0) })
            Small type Vec<Int> where all(%, %(lt(%, 10)))
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Explicit inner lambdas compile. Got: {:?}",
            errors
        );
    }

    /// A predicate that reaches an impure native, even through helpers, is
    /// rejected, with the call chain in the message.
    #[test]
    fn test_where_predicate_must_be_pure() {
        let source = r#"
            ::test ns
            sku-exists fn (sku: Str): Bool { is-ok(::hot::http/get(sku)) }
            KnownSku type Str where sku-exists(%)
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            has_constraint_code(&errors, "impure-constraint"),
            "Expected impure-constraint. Got: {:?}",
            errors
        );
        let message = errors
            .iter()
            .find(|e| constraint_code(e) == Some("impure-constraint"))
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("::hot::http") && message.contains("::test/sku-exists"),
            "The message should name the native and the path to it: {}",
            message
        );
    }

    /// A predicate whose result is definitely not a Bool is rejected.
    #[test]
    fn test_where_predicate_must_return_bool() {
        let source = r#"
            ::test ns
            Named type Str where length(%)
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            has_constraint_code(&errors, "constraint-not-bool"),
            "Expected constraint-not-bool. Got: {:?}",
            errors
        );

        // `and`, `or` and `if` of Bools are Bools.
        let source = r#"
            ::test ns
            Ranged type Int where and(gte(%, 0), or(lt(%, 10), eq(%, 99)))
            Picked type Int where if(gt(%, 0), lt(%, 10), eq(%, -1))
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Bool predicates compile. Got: {:?}",
            errors
        );
    }

    /// A refined lazy parameter, and a precondition on a signature with a
    /// lazy parameter, are unsupported.
    #[test]
    fn test_where_rejects_lazy_parameters() {
        for source in [
            r#"
            ::test ns
            f fn (lazy x: Int where gt(%, 0)): Int { do x }
            "#,
            r#"
            ::test ns
            g fn (lazy x: Int, y: Int) where gt(%.y, 0): Int { y }
            "#,
        ] {
            let errors = compile_and_get_type_errors(source);
            assert!(
                has_constraint_code(&errors, "unsupported-constraint"),
                "Expected unsupported-constraint for {}. Got: {:?}",
                source,
                errors
            );
        }
    }

    /// Bounded constant evaluation rejects literals that fail a constraint:
    /// constructor arguments, typed bindings and call arguments.
    #[test]
    fn test_where_rejects_failing_literals() {
        let source = r#"
            ::test ns
            Port type Int where between(%, 1, 65535)
            AboveTwo type Int where gt(%, 2)
            listen fn (port: Port): Str { "ok" }
            a Port(70000)
            three: AboveTwo 1
            b listen(0)
        "#;
        let errors = compile_and_get_type_errors(source);
        let violations: Vec<String> = errors
            .iter()
            .filter(|e| constraint_code(e) == Some("constraint-violation"))
            .map(|e| e.to_string())
            .collect();
        assert_eq!(
            violations.len(),
            3,
            "Expected three constraint-violation errors. Got: {:?}",
            errors
        );
        assert!(violations.iter().any(|m| m.contains("70000 fails `Port`")));
        assert!(violations.iter().any(|m| m.contains("1 fails `AboveTwo`")));
        assert!(violations.iter().any(|m| m.contains("0 fails `Port`")));
    }

    fn violations(source: &str) -> Vec<String> {
        compile_and_get_type_errors(source)
            .iter()
            .filter(|e| constraint_code(e) == Some("constraint-violation"))
            .map(|e| e.to_string())
            .collect()
    }

    /// A predicate that calls a pure function of the program is evaluated
    /// through it: a single-expression body, local bindings and `cond`.
    #[test]
    fn test_where_rejects_failing_literals_through_user_functions() {
        let source = r#"
            ::test ns
            short fn (s: Str): Bool { lte(length(s), 3) }
            max-len 3
            fits fn (s: Str): Bool {
                n length(s)
                lte(n, max-len)
            }
            sized fn cond (n: Int): Bool {
                lt(n, 0) => { false }
                => { short-int(n) }
            }
            short-int fn (n: Int): Bool { lt(n, 100) }
            Tiny type Str where short(%)
            Fitted type Str where fits(%)
            Sized type Int where sized(%)
            take fn (s: Str where short(%)): Str { s }
            t: Tiny "too long"
            f: Fitted "also too long"
            s: Sized 100
            a take("long again")
        "#;
        let found = violations(source);
        assert_eq!(found.len(), 4, "Expected four violations. Got: {:?}", found);
        assert!(
            found
                .iter()
                .any(|m| m.contains("\"too long\" fails `Tiny`"))
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("\"also too long\" fails `Fitted`"))
        );
        assert!(found.iter().any(|m| m.contains("100 fails `Sized`")));
        assert!(
            found
                .iter()
                .any(|m| m.contains("\"long again\" fails `short(%)`"))
        );

        let passing = r#"
            ::test ns
            short fn (s: Str): Bool { lte(length(s), 3) }
            sized fn cond (n: Int): Bool {
                lt(n, 0) => { false }
                => { true }
            }
            Tiny type Str where short(%)
            Sized type Int where sized(%)
            t: Tiny "abc"
            s: Sized 7
        "#;
        let errors = compile_and_get_type_errors(passing);
        assert!(
            errors.is_empty(),
            "Passing literals compile. Got: {:?}",
            errors
        );
    }

    /// Evaluation gives up, reporting nothing, on what it can't follow:
    /// recursion past its budget, a refined parameter, a parameter type the
    /// literal doesn't have, and a function it doesn't know.
    #[test]
    fn test_where_user_function_evaluation_gives_up_quietly() {
        let source = r#"
            ::test ns
            countdown fn cond (n: Int): Bool {
                lte(n, 0) => { false }
                => { countdown(sub(n, 1)) }
            }
            guarded fn (n: Int where gt(%, 0)): Bool { false }
            typed fn (n: Int): Bool { false }
            opaque fn (s: Str): Bool { eq(uppercase(s), "X") }
            Deep type Int where countdown(%)
            Guarded type Int where guarded(%)
            Typed type Str where typed(%)
            Opaque type Str where opaque(%)
            d: Deep 1000
            g: Guarded 0
            t: Typed "x"
            o: Opaque "y"
        "#;
        let found = violations(source);
        assert!(found.is_empty(), "Expected no violations. Got: {:?}", found);
    }

    /// Literal fields of a constructor call, named or in Map form, are
    /// checked against their field types, with the field's path.
    #[test]
    fn test_where_rejects_failing_constructor_fields() {
        let source = r#"
            ::test ns
            Pos type Int where gt(%, 0)
            Fruit type "apple" | "banana"
            Rec type { n: Int where gt(%, 0) }
            Typed type { p: Pos, f: Fruit, q: Pos? }
            Outer type { inner: Rec }
            Circle type { radius: Dec where gt(%, 0) }
            Shape enum { Circle(Circle), Point }
            a Rec(n: 0)
            b Rec({n: -1})
            c Typed(p: 0, f: "kiwi", q: 0)
            d Outer({inner: {n: 0}})
            e Shape.Circle(radius: 0.0)
        "#;
        let found = violations(source);
        assert_eq!(
            found.len(),
            7,
            "Expected seven violations. Got: {:?}",
            found
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("0 fails `gt(%, 0)` at `n` of `Rec`"))
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("-1 fails `gt(%, 0)` at `n` of `Rec`"))
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("0 fails `Pos` (gt(%, 0)) at `p` of `Typed`"))
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("0 fails `Pos` (gt(%, 0)) at `q` of `Typed`"))
        );
        assert!(found.iter().any(
            |m| m.contains("\"kiwi\" isn't one of the values of `Fruit`")
                && m.contains("at `f` of `Typed`")
        ));
        assert!(found.iter().any(|m| m.contains("at `inner.n` of `Outer`")));
        assert!(
            found
                .iter()
                .any(|m| m.contains("0.0 fails `gt(%, 0)` at `radius` of `Circle`"))
        );

        let passing = r#"
            ::test ns
            Pos type Int where gt(%, 0)
            Rec type { n: Int where gt(%, 0) }
            Typed type { p: Pos, q: Pos? }
            x 0
            a Rec(n: 1)
            b Rec({n: x})
            c Typed(p: 3, q: null)
        "#;
        let errors = compile_and_get_type_errors(passing);
        assert!(
            errors.is_empty(),
            "Passing fields compile. Got: {:?}",
            errors
        );
    }

    /// A record's invariant is checked when every field it reads is a literal.
    #[test]
    fn test_where_rejects_failing_record_invariants() {
        let source = r#"
            ::test ns
            Range type { low: Int, high: Int } where lte(%.low, %.high)
            a Range(low: 5, high: 1)
            b Range({low: 3, high: 2})
        "#;
        let found = violations(source);
        assert_eq!(found.len(), 2, "Expected two violations. Got: {:?}", found);
        assert!(
            found
                .iter()
                .all(|m| m.contains("fails `Range` (lte(%.low, %.high))"))
        );

        let passing = r#"
            ::test ns
            Range type { low: Int, high: Int } where lte(%.low, %.high)
            x 0
            a Range(low: 1, high: 5)
            b Range(low: 5, high: x)
        "#;
        let errors = compile_and_get_type_errors(passing);
        assert!(
            errors.is_empty(),
            "Passing records compile. Got: {:?}",
            errors
        );
    }

    /// Typed bindings inside function bodies are checked like top-level ones.
    #[test]
    fn test_where_rejects_failing_literals_in_bodies() {
        let source = r#"
            ::test ns
            Pos type Int where gt(%, 0)
            Fruit type "apple" | "banana"
            main fn (): Int {
                x: Pos 0
                f: Fruit "kiwi"
                limit: Int 0 where gt(%, 0)
                inner (y) {
                    z: Pos -2
                    z
                }
                x
            }
        "#;
        let found = violations(source);
        assert_eq!(found.len(), 4, "Expected four violations. Got: {:?}", found);
        assert!(found.iter().any(|m| m.contains("0 fails `Pos`")));
        assert!(found.iter().any(|m| m.contains("-2 fails `Pos`")));
        assert!(
            found
                .iter()
                .any(|m| m.contains("\"kiwi\" isn't one of the values of `Fruit`"))
        );
        assert!(found.iter().any(|m| m.contains("0 fails `gt(%, 0)`")));

        let passing = r#"
            ::test ns
            Pos type Int where gt(%, 0)
            main fn (): Int {
                x: Pos 1
                x
            }
        "#;
        let errors = compile_and_get_type_errors(passing);
        assert!(
            errors.is_empty(),
            "Passing bindings compile. Got: {:?}",
            errors
        );
    }

    /// A literal outside an open union's known values is a warning, and a
    /// refined primitive used with `is-type` warns that it is untagged.
    #[test]
    fn test_where_warnings() {
        let source = r#"
            ::test ns
            TrackLinks type open "None" | "HtmlAndText"
            describe fn (mode: TrackLinks): Str { mode }
            a describe("HtmlOnly")
            Port type Int where between(%, 1, 65535)
            b is-type(443, Port)
        "#;
        let warnings = compile_and_get_warnings(source);
        assert!(
            has_constraint_code(&warnings, "unknown-literal"),
            "Expected unknown-literal. Got: {:?}",
            warnings
        );
        assert!(
            has_constraint_code(&warnings, "where-changes-representation"),
            "Expected where-changes-representation. Got: {:?}",
            warnings
        );
        let errors = compile_and_get_type_errors(source);
        assert!(
            errors.is_empty(),
            "Warnings don't fail compilation. Got: {:?}",
            errors
        );
    }

    /// A parameter's predicate can't use another parameter; the error points
    /// to argument-list preconditions.
    #[test]
    fn test_where_parameter_predicate_cannot_use_other_parameters() {
        let source = r#"
            ::test ns
            clamp fn (low: Int, high: Int where gte(%, low)): Int { high }
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(
            errors
                .iter()
                .any(|e| constraint_code(e) == Some("invalid-constraint")
                    && e.to_string().contains("uses `low`, another parameter")),
            "Expected invalid-constraint. Got: {:?}",
            errors
        );
    }

    /// An open literal union doesn't open a closed enum that has the same
    /// short name in another namespace, so matching the enum still needs no
    /// default arm.
    #[test]
    fn test_open_union_does_not_open_a_same_named_enum() {
        let source = r#"
            ::a ns
            Role type open "user" | "assistant"

            ::b ns
            Role enum { User, Assistant }
            describe fn (r: Role): Str {
                match r {
                    Role.User => { "user" }
                    Role.Assistant => { "assistant" }
                }
            }
        "#;
        let errors = compile_and_get_type_errors(source);
        assert!(errors.is_empty(), "Expected no errors. Got: {:?}", errors);
    }

    /// `and`, `or`, `if` and `not` infer the union of their result types, so
    /// a definite mismatch with a binding's annotation is reported.
    #[test]
    fn test_logic_functions_infer_precise_types() {
        let source = r#"
            ::test ns
            a: Int and(true, false)
            b: Str not(true)
            c: Int if(true, "yes", "no")
            d: Bool or(true, false)
        "#;
        let warnings = compile_and_get_warnings(source);
        let mismatched: Vec<String> = warnings
            .iter()
            .filter_map(|w| match w {
                CompilerError::AnnotationMismatch { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        for name in ["a", "b", "c"] {
            assert!(
                mismatched.iter().any(|m| m == name),
                "Expected an annotation mismatch for `{}`. Got: {:?}",
                name,
                warnings
            );
        }
        assert!(
            !mismatched.iter().any(|m| m == "d"),
            "`or` of Bools is a Bool. Got: {:?}",
            warnings
        );
    }
}
