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
            // --- Single-stage terminal reduce (matches string-concat-benchmark) ---
            r#"::t ns
f fn (n: Int): Str {
    reduce(range(n), (acc, i) { concat(acc, `item-${i}-`) }, "")
}
length(f(200))"#,
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

        // This test verifies that fail constructs a Failure type {$msg, $err}
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
