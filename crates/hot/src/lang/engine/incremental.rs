//! REPL/state-retained execution paths.
//!
//! These methods drive Hot programs in a way that *keeps* the resulting VM
//! state alive (rather than throwing it away after the run), so callers
//! can inspect variables, replay incremental snippets, or build a REPL on
//! top of the engine.
//!
//! Public surface:
//!   * `execute_and_retain_state` / `execute_eval_and_retain_state`
//!     — file/eval pipelines that return an [`super::ExecutedEngine`].
//!   * `execute_incremental` — REPL turn: load prior namespace state,
//!     compile additional code, run it, return result + new state.
//!   * `execute_code_and_retain_state` — internal worker shared by the
//!     above three.
//!   * `get_var_from_*` / `get_namespace_vars_from_vm` / `get_all_vars_from_vm`
//!     — convenience accessors over a running [`crate::lang::runtime::vm::VirtualMachine`].

use super::discover::{discover_compilation_units, parse_units_with_cache};
use super::{Engine, ExecutedEngine, IncrementalExecutionResult};
use ahash::AHashMap;
use std::sync::Arc;

impl Engine {
    /// Execute code and return an ExecutedEngine for variable extraction
    /// This allows extracting multiple variables from the same execution
    pub fn execute_and_retain_state(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        color: bool,
    ) -> Result<ExecutedEngine, String> {
        Self::execute_code_and_retain_state(None, src_paths, test_paths, conf, project_name, color)
    }

    /// Execute eval code and return an ExecutedEngine for variable extraction
    pub fn execute_eval_and_retain_state(
        code: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        color: bool,
    ) -> Result<ExecutedEngine, String> {
        Self::execute_code_and_retain_state(
            Some(code),
            src_paths,
            test_paths,
            conf,
            project_name,
            color,
        )
    }

    /// Execute code incrementally for REPL - compile all code but execute only new instructions
    /// This prevents re-execution of side effects while maintaining full program context
    #[allow(clippy::too_many_arguments)]
    pub fn execute_incremental(
        code: &str,
        src_paths: &[String],
        _test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        preserved_state: &indexmap::IndexMap<String, indexmap::IndexMap<String, crate::val::Val>>,
        executed_instruction_count: usize,
        preserved_namespace: Option<&str>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        color: bool,
    ) -> Result<IncrementalExecutionResult, String> {
        // Parse the REPL input code
        let eval_program =
            crate::lang::parser::parse_hot(code).map_err(|e| format!("Parse error: {}", e))?;

        // Discover compilation units (dependencies + source paths) - uses caching
        tracing::debug!(
            "execute_incremental: src_paths={:?}, conf={}, project_name={:?}",
            src_paths,
            conf.is_some(),
            project_name
        );
        let units = discover_compilation_units(conf, project_name, src_paths, &[])?;
        tracing::debug!("execute_incremental: discovered {} units", units.len());

        // Parse compilation units with caching - this is the key optimization
        // that prevents re-parsing all packages on every REPL input
        let (cached_namespaces, _parsed_files) = parse_units_with_cache(&units, color)?;

        // Build the combined program from cached namespaces
        // Use ::hot::main as default namespace to match run_unified_pipeline behavior
        let mut combined_program = crate::lang::ast::Program {
            namespaces: cached_namespaces,
            current_namespace: crate::lang::ast::NsPath::hot_main(),
        };

        // Merge eval program namespaces (REPL code)
        for (ns_path, namespace) in eval_program.namespaces {
            combined_program.namespaces.insert(ns_path, namespace);
        }

        // Resolve variable references
        crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

        // Compile the program
        let mut compiler = crate::lang::compiler::Compiler::new();

        compiler
            .compile_program(&mut combined_program)
            .map_err(|errors| format!("Compilation errors:\n{}", errors.format_error(color)))?;

        // Create VM with full AST from combined program (includes all dependency namespaces)
        let program_arc = compiler.get_program_arc();
        let total_instruction_count = program_arc.get_entry_instruction_count();
        let hot_ast = Arc::new(crate::lang::ast::HotAst::from_program(
            combined_program.clone(),
        ));
        let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
            program_arc.clone(),
            Some(hot_ast),
            compiler.get_function_mapping_arc(),
            compiler.get_core_functions_arc(),
            compiler.get_type_implementations_arc(),
            compiler.get_core_variables_arc(),
            None,
        );

        // INJECT context storage if provided (from ctx.hot)
        if let Some(ctx_storage) = context_storage {
            tracing::debug!(
                "Engine: Setting context storage with {} variables",
                ctx_storage.len()
            );
            vm.context_storage = ctx_storage;
        }

        // Extract ctx requirements via call graph and apply defaults + secret keys
        let ctx_requirements =
            crate::lang::compiler::ctx_checker::extract_ctx_requirements_via_call_graph(
                &combined_program,
            );
        for (key, default_val) in ctx_requirements.all_defaults() {
            vm.context_storage.entry(key).or_insert(default_val);
        }
        vm.secret_keys = ctx_requirements.all_secret_keys();
        vm.sync_secret_keys_to_execution_context();

        // INJECT preserved state BEFORE execution
        tracing::debug!(
            "Engine: About to restore {} namespaces",
            preserved_state.len()
        );
        vm.restore_namespace_variables(preserved_state);
        tracing::debug!(
            "Engine: Restored state, VM now has {} namespaces",
            vm.get_all_namespace_vars().len()
        );

        // RESTORE current namespace from previous execution
        // This is critical because we skip the SetNamespace instruction when doing incremental execution
        if let Some(ns) = preserved_namespace {
            tracing::debug!("Engine: Setting current_namespace to '{}'", ns);
            vm.set_current_namespace(ns.to_string());
        }

        // Execute ONLY new instructions
        let result = if executed_instruction_count < total_instruction_count {
            tracing::debug!(
                "REPL incremental execution: executing instructions [{}..{})",
                executed_instruction_count,
                total_instruction_count
            );
            vm.execute_instruction_range(executed_instruction_count, total_instruction_count)
                .map_err(|e| format!("Execution failed: {}", e))?
        } else {
            tracing::debug!("REPL incremental execution: no new instructions");
            crate::val::Val::Null
        };

        // Extract updated state
        let new_state = vm.get_all_namespace_vars().clone();
        let current_namespace = vm.get_current_namespace().to_string();

        // Return result with updated state, instruction count, and namespace
        Ok(IncrementalExecutionResult {
            result,
            new_state,
            total_instruction_count,
            current_namespace,
        })
    }

    /// Internal method to execute code and return ExecutedEngine
    fn execute_code_and_retain_state(
        eval_code: Option<&str>,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        color: bool,
    ) -> Result<ExecutedEngine, String> {
        // Collect all .hot files in the correct loading order
        let mut all_hot_files = Vec::new();

        // Load project dependencies if configuration is provided
        if let (Some(conf), Some(project_name)) = (conf, project_name) {
            match crate::project::get_resolved_project_dependencies(conf, project_name) {
                Ok(resolved_deps) => {
                    for dep in &resolved_deps {
                        // Use discover_dependency_source_files to only load src_paths, not test_paths
                        let files = Self::discover_dependency_source_files(&dep.resolved_path)?;
                        all_hot_files.extend(files);
                    }
                }
                Err(e) => {
                    tracing::warn!("Warning: Failed to load project dependencies: {}", e);
                }
            }
        } else {
            // If no project configuration is provided, still load hot-std for
            // configuration files (so core fns like env-get are available).
            // Use the canonical DependencyResolver — same source of truth as
            // `discover::discover_compilation_units`.
            let resolver = crate::lang::project::DependencyResolver::default();
            let hot_std = resolver.get_hot_std_dependency();
            let hot_std_src_path = hot_std.resolved_path.join("src");
            if hot_std_src_path.exists() {
                tracing::debug!(
                    "Loading hot-std for configuration execution from: {}",
                    hot_std.resolved_path.display()
                );
                let hot_std_files = Self::discover_hot_files(&hot_std_src_path.to_string_lossy())?;
                all_hot_files.extend(hot_std_files);
            } else {
                tracing::warn!(
                    "hot-std/src directory not found at: {}",
                    hot_std_src_path.display()
                );
            }
        }

        // Add source files
        for src_path in src_paths {
            let files = Self::discover_hot_files(src_path)?;
            all_hot_files.extend(files);
        }

        // Add test files
        for test_path in test_paths {
            let files = Self::discover_hot_files(test_path)?;
            all_hot_files.extend(files);
        }

        // Parse all files into a combined program
        let mut combined_program = crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::new(),
        };

        for file_path in &all_hot_files {
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    match crate::lang::parser::parse_hot_file(&content, file_path) {
                        Ok(program) => {
                            // Merge namespaces
                            for (ns_path, namespace) in program.namespaces {
                                combined_program.namespaces.insert(ns_path, namespace);
                            }
                        }
                        Err(e) => {
                            if let Some(formatted) = e.format_error(&content, color) {
                                return Err(format!("Parse errors:\n{}", formatted));
                            } else {
                                return Err(format!("Parse error in {}: {}", file_path, e));
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(format!("Failed to read file {}: {}", file_path, e));
                }
            }
        }

        // If eval_code is provided, parse and add it to the program
        if let Some(code) = eval_code {
            match crate::lang::parser::parse_hot(code) {
                Ok(eval_program) => {
                    // Merge eval program namespaces
                    for (ns_path, namespace) in eval_program.namespaces {
                        combined_program.namespaces.insert(ns_path, namespace);
                    }
                }
                Err(e) => {
                    if let Some(formatted) = e.format_error(code, color) {
                        return Err(format!("Parse errors:\n{}", formatted));
                    } else {
                        return Err(format!("Parse error in eval code: {}", e));
                    }
                }
            }
        }

        // Resolve variable references
        crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

        // Compile the program
        let mut compiler = crate::lang::compiler::Compiler::new();

        // Add source files for error reporting
        for file_path in &all_hot_files {
            if let Ok(content) = std::fs::read_to_string(file_path) {
                compiler.add_source_file(std::path::PathBuf::from(file_path), content);
            }
        }

        // Compile with validation
        match compiler.compile_program(&mut combined_program) {
            Ok(()) => {
                // Create VM and execute
                let program_arc = compiler.get_program_arc();
                let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
                    program_arc.clone(),
                    Some(Arc::new(
                        crate::lang::ast::HotAst::with_namespaces_and_core(),
                    )),
                    compiler.get_function_mapping_arc(),
                    compiler.get_core_functions_arc(),
                    compiler.get_type_implementations_arc(),
                    compiler.get_core_variables_arc(),
                    None,
                );

                // Execute the program
                vm.execute()
                    .map_err(|e| format!("Execution failed: {}", e))?;

                // Return ExecutedEngine with the VM state
                Ok(ExecutedEngine {
                    vm,
                    program: program_arc,
                })
            }
            Err(errors) => {
                let formatted = errors.format_error(color);
                Err(format!("Compilation errors:\n{}", formatted))
            }
        }
    }

    /// Extract a variable from the VM after execution (similar to lang1's get_var)
    /// This allows retrieving configuration variables and other results after program execution
    /// DEPRECATED: Use execute_and_retain_state() followed by extract_var() for better performance
    pub fn get_var_from_execution(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        namespace_path: Option<&str>,
        var_name: &str,
    ) -> Result<Option<crate::val::Val>, String> {
        let executed_engine =
            Self::execute_and_retain_state(src_paths, test_paths, conf, project_name, false)?;
        Ok(executed_engine.extract_var(namespace_path, var_name))
    }

    /// Get a variable from a running VM instance (for use within execution context)
    pub fn get_var_from_vm(
        vm: &crate::lang::runtime::vm::VirtualMachine,
        namespace_path: Option<&str>,
        var_name: &str,
    ) -> Option<crate::val::Val> {
        vm.get_var(namespace_path, var_name)
    }

    /// Get all variables from a specific namespace in a running VM
    pub fn get_namespace_vars_from_vm<'a>(
        vm: &'a crate::lang::runtime::vm::VirtualMachine,
        namespace_path: Option<&str>,
    ) -> Option<&'a indexmap::IndexMap<String, crate::val::Val>> {
        vm.get_namespace_vars(namespace_path)
    }

    /// Get all namespaces and their variables from a running VM (for debugging)
    pub fn get_all_vars_from_vm(
        vm: &crate::lang::runtime::vm::VirtualMachine,
    ) -> &indexmap::IndexMap<String, indexmap::IndexMap<String, crate::val::Val>> {
        vm.get_all_namespace_vars()
    }
}
