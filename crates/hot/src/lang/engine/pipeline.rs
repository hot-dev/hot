//! Run / eval / check pipeline implementations.
//!
//! This is the bulk of what makes the engine "drive" Hot programs:
//!
//!   * `extract_handlers_and_scheduled_functions` — post-compile harvest
//!     of handlers, schedules, MCP tools, webhooks, agent definitions,
//!     send targets, context requirements, and box requirements.
//!   * `run_file_pipeline_with_deps` and the family of
//!     `eval_code_pipeline_with_*` variants — increasingly feature-rich
//!     wrappers around the unified pipeline (emitter, event publisher,
//!     execution context, artifact collection).
//!   * `check_sources_pipeline_*` — typecheck-only pipelines (with
//!     diagnostics, dependencies, or context).
//!   * `run_unified_pipeline[_with_artifacts]` —
//!     the actual unified pipeline that all of the above delegate to.
//!   * `call_test_runner[_with_pattern]` — invokes the in-language
//!     `::hot::test::run-tests` entry point.

use super::discover::{
    DiscoveredUnit, NsMergePolicy, discover_compilation_units, merge_namespace,
    parse_files_parallel, parse_units_with_cache,
};
use super::{
    CompilationArtifacts, ContextValidationPolicy, Engine, ExecutionWithArtifacts,
    ExtractedHandlers, PipelineMode,
};
use ahash::{AHashMap, AHashSet};
use std::sync::Arc;

impl Engine {
    /// Extract event handlers, scheduled functions, MCP tools, webhook endpoints, agent definitions, and context requirements from a compiled project
    pub fn extract_handlers_and_scheduled_functions(
        src_paths: &[String],
        project_name: Option<&str>,
        conf: Option<&crate::val::Val>,
        color: bool,
    ) -> Result<ExtractedHandlers, String> {
        tracing::debug!(
            "Starting event handler extraction from {} source paths",
            src_paths.len()
        );

        // Create a new engine for compilation
        let mut engine = Self::new();

        // Collect all .hot files in the correct loading order:
        // 1. hot-std package, 2. project deps packages, 3. src_paths
        let mut all_hot_files = Vec::new();

        // First, load project dependencies (including hot-std) if project name is provided
        if let Some(project_name) = project_name {
            // Use the provided config if available, otherwise create a minimal one
            let owned_conf;
            let effective_conf = if let Some(c) = conf {
                c
            } else {
                owned_conf = crate::val::val!({
                    "project": {
                        "name": project_name
                    }
                });
                &owned_conf
            };

            match crate::project::get_resolved_project_dependencies(effective_conf, project_name) {
                Ok(resolved_deps) => {
                    tracing::debug!(
                        "Loading {} project dependencies for event handler extraction...",
                        resolved_deps.len()
                    );
                    for dep in &resolved_deps {
                        tracing::debug!("Dependency {}: {}", dep.name, dep.resolved_path.display());
                        // Use discover_dependency_source_files to only load src_paths, not test_paths
                        let files = Self::discover_dependency_source_files(&dep.resolved_path)?;
                        tracing::debug!(
                            "Found {} dependency files in {}",
                            files.len(),
                            dep.resolved_path.display()
                        );
                        all_hot_files.extend(files);
                    }
                }
                Err(e) => {
                    tracing::warn!("Warning: Failed to load project dependencies: {}", e);
                }
            }
        }

        // Then, load source files
        for src_path in src_paths {
            tracing::debug!("Discovering files in: {}", src_path);
            let files = Self::discover_hot_files(src_path)?;
            tracing::debug!("Found {} files in {}", files.len(), src_path);
            all_hot_files.extend(files);
        }

        // Remove duplicates while preserving order
        let mut seen = AHashSet::new();
        all_hot_files.retain(|file| seen.insert(file.clone()));

        tracing::debug!("Total files to process: {}", all_hot_files.len());

        // Parse all files and build the program
        let mut program = crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath(vec![]),
        };

        for file_path in all_hot_files {
            let content = std::fs::read_to_string(&file_path)
                .map_err(|e| format!("Failed to read file {}: {}", file_path, e))?;

            // Store file contents for error reporting
            engine
                .compiler
                .add_file_content(std::path::PathBuf::from(&file_path), content.clone());

            let parsed_program = crate::lang::parser::parse_hot_file(&content, &file_path)
                .map_err(|e| {
                    if let Some(formatted) = e.format_error(&content, color) {
                        format!("Parse errors:\n{}", formatted)
                    } else {
                        format!("Failed to parse {}: {}", file_path, e)
                    }
                })?;

            // Member-merge namespaces; duplicate definitions across files
            // are compile errors.
            for (ns_path, namespace) in parsed_program.namespaces {
                merge_namespace(
                    &mut program.namespaces,
                    ns_path,
                    namespace,
                    NsMergePolicy::Strict,
                )?;
            }
        }

        // Extract core variables first (needed for send target detection of bare send() calls)
        engine
            .compiler
            .extract_core_variables_from_program(&program)
            .map_err(|e| e.format_error(color))?;

        // Extract event handlers, scheduled functions, and MCP tools
        tracing::debug!(
            "About to extract from program with {} namespaces",
            program.namespaces.len()
        );
        engine
            .compiler
            .extract_event_handlers(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_scheduled_functions(&program)
            .map_err(|e| e.format_error(color))?;
        if let Some(conf) = conf {
            let policy = crate::db::SchedulePolicy::from_conf(conf);
            for cron_expression in engine.compiler.get_scheduled_functions().keys() {
                crate::db::validate_recurring_schedule_interval(
                    cron_expression,
                    policy.min_interval_secs,
                )
                .map_err(|e| e.message())?;
            }
        }
        engine
            .compiler
            .extract_mcp_tools(&program)
            .map_err(|e| e.format_error(color))?;
        // Populate the runtime tool-schema registry so that
        // ::hot::internal::mcp/schema-from-fn (and ::ai::tool/schema-from-fn)
        // can resolve any Hot function to its {input-schema, output-schema}.
        engine
            .compiler
            .extract_tool_specs(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_skill_specs(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_webhooks(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_agents(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_workflows(&program)
            .map_err(|e| e.format_error(color))?;
        engine
            .compiler
            .extract_send_targets(&program)
            .map_err(|e| e.format_error(color))?;

        let event_handlers = engine.compiler.get_event_handlers().clone();
        let scheduled_functions = engine.compiler.get_scheduled_functions().clone();
        let mcp_tools = engine.compiler.get_mcp_tools().clone();
        let webhooks = engine.compiler.get_webhooks().clone();
        let agents = engine.compiler.get_agents().clone();
        let workflows = engine.compiler.get_workflows().clone();
        let send_targets = engine.compiler.get_send_targets().clone();

        // Extract context and box requirements using call graph resolution.
        // This only includes requirements reachable from user code,
        // avoiding unnecessary requirements from unused package functions.
        let call_graph = crate::lang::compiler::call_graph::CallGraph::build(&program);
        let ctx_requirements = call_graph.resolve_user_ctx_requirements(&program);
        let box_requirements = call_graph.resolve_user_box_requirements(&program);

        tracing::debug!(
            "Extracted {} event handler types, {} scheduled function types, {} MCP services, {} webhook services, {} agent types, {} workflow types, {} send target fns, {} ctx requirement keys, {} box requirements",
            event_handlers.len(),
            scheduled_functions.len(),
            mcp_tools.len(),
            webhooks.len(),
            agents.len(),
            workflows.len(),
            send_targets.len(),
            ctx_requirements.all_required_keys().len(),
            box_requirements.requirements.len()
        );

        Ok(ExtractedHandlers {
            event_handlers,
            scheduled_functions,
            mcp_tools,
            webhooks,
            agents,
            workflows,
            send_targets,
            ctx_requirements,
            box_requirements,
        })
    }

    /// Structurally validate a single file with full dependency loading.
    ///
    /// This is the build/compile entry point: it parses and compiles the
    /// project, validates handlers, schedules, and box metadata, and collects
    /// context requirements without requiring runtime context values or
    /// executing module code.
    pub fn run_file_pipeline_with_deps(
        target_file: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        color: bool,
    ) -> Result<crate::val::Val, String> {
        // Use the unified pipeline with target file
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            Some(target_file),
            None,
            PipelineMode::StructuralValidation,
            None,
            None,
            None, // No event publisher
            None, // No context storage
            None, // No database pool
            None, // No file storage
            None, // No store
            None, // No embedding provider
            None, // No task queue
            None, // No stream publisher
            color,
            None, // No warnings out
        )
    }

    /// Evaluate Hot code using the pipeline with full dependency loading
    pub fn eval_code_pipeline_with_deps(
        code: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<crate::val::Val, String> {
        // Use the unified pipeline with eval code
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
            Some(code),
            PipelineMode::Execute,
            None,
            None,
            None, // No event publisher
            None, // No context storage
            None, // No database pool
            None, // No file storage
            None, // No store
            None, // No embedding provider
            None, // No task queue
            None, // No stream publisher
            false,
            None, // No warnings out
        )
    }

    /// Evaluate Hot code using the pipeline with full dependency loading, emitter, and event publisher
    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_code_pipeline_with_deps_emitter_and_event_publisher(
        code: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
    ) -> Result<crate::val::Val, String> {
        // Use the unified pipeline with eval code, emitter, and event publisher
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
            Some(code),
            PipelineMode::Execute,
            emitter,
            execution_context,
            event_publisher,
            None, // No context storage
            None, // No database pool
            None, // No file storage
            None, // No store
            None, // No embedding provider
            None, // No task queue
            None, // No stream publisher
            false,
            None, // No warnings out
        )
    }

    /// Evaluate Hot code using the pipeline with full dependency loading, emitter, event publisher, and context storage
    #[allow(clippy::too_many_arguments)]
    pub fn eval_code_pipeline_with_all_features(
        code: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
    ) -> Result<crate::val::Val, String> {
        // Use the unified pipeline with eval code, emitter, event publisher, and context storage
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
            Some(code),
            PipelineMode::Execute,
            emitter,
            execution_context,
            event_publisher,
            context_storage,
            None, // No database pool
            None, // No file storage
            None, // No store
            None, // No embedding provider
            None, // No task queue
            None, // No stream publisher
            false,
            None, // No warnings out
        )
    }

    /// Structured-error variant of `check_sources_pipeline_with_deps`.
    ///
    /// Runs the same discover → parse → resolve → compile-with-validation
    /// pipeline as `check_sources_pipeline_with_deps`, but returns the raw
    /// `CompilerErrors` collection (with sources cached) instead of a
    /// pre-formatted string. Empty `errors` means the project type-checks.
    ///
    /// Parse errors are converted to `InvalidFunctionCall` diagnostics with
    /// `func_name = "<parse>"` carrying the file path, line, column, and
    /// length so editors can highlight the offending span. Errors that
    /// don't have any source location (e.g. dependency discovery failures)
    /// are surfaced as a single `InvalidFunctionCall` with `func_name =
    /// "<pipeline>"` and `location: None`.
    /// Collect the set of project-owned source file paths (i.e. files from
    /// `SourcePath` units, not dependency `Package` units). Used to scope
    /// deprecation warnings to first-party code so a deprecated call buried in
    /// a dependency or in `hot-std` never produces noise the user can't fix.
    fn project_source_files(units: &[DiscoveredUnit]) -> AHashSet<String> {
        use crate::lang::cache::unit_cache::CompilationUnit;
        let mut set = AHashSet::new();
        for u in units {
            if matches!(u.unit, CompilationUnit::SourcePath { .. }) {
                for f in &u.files {
                    // Store both the raw and canonicalized path. The discovered
                    // path (often relative, e.g. `./hot/src/app.hot`) may differ
                    // from the path stored in the cached AST (absolute /
                    // canonicalized), so we match against either form.
                    set.insert(f.clone());
                    if let Ok(canon) = std::fs::canonicalize(f) {
                        set.insert(canon.display().to_string());
                    }
                }
            }
        }
        set
    }

    /// Keep only warnings whose call site lives in a project-owned file.
    /// Preserves the source cache so the retained warnings can still render
    /// rich ariadne snippets.
    fn scope_warnings_to_project(
        diagnostics: crate::lang::errors::CompilerErrors,
        project_files: &AHashSet<String>,
    ) -> crate::lang::errors::CompilerErrors {
        let mut out = crate::lang::errors::CompilerErrors::new();
        out.source_cache = diagnostics.source_cache;
        for w in diagnostics.warnings {
            let file = w.location().and_then(|l| l.file.as_ref()).cloned();
            let keep = file
                .as_ref()
                .map(|f| {
                    let raw = f.display().to_string();
                    if project_files.contains(&raw) {
                        return true;
                    }
                    match std::fs::canonicalize(f) {
                        Ok(canon) => project_files.contains(&canon.display().to_string()),
                        Err(_) => false,
                    }
                })
                .unwrap_or(false);
            if keep {
                // Ensure the warning's source is cached so it renders with a
                // rich ariadne snippet rather than the plain Display fallback.
                if let Some(f) = &file {
                    let key = f.display().to_string();
                    if !out.source_cache.contains_key(&key)
                        && let Ok(content) = std::fs::read_to_string(f)
                    {
                        out.add_source(key, content);
                    }
                }
                out.warnings.push(w);
            }
        }
        out
    }

    pub fn check_sources_pipeline_diagnostics(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> crate::lang::errors::CompilerErrors {
        Self::check_sources_pipeline_diagnostics_with_ctx(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
        )
    }

    /// Like `check_sources_pipeline_diagnostics`, but also emits diagnostics
    /// for missing context variables when `available_ctx_keys` is provided.
    /// Each missing key becomes a `CallLibError` with `func_name = "<ctx>"`
    /// and a file-level location pointing at the namespace's source file
    /// (the call graph doesn't currently track per-`meta` line/column, so
    /// the location is best-effort).
    pub fn check_sources_pipeline_diagnostics_with_ctx(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        available_ctx_keys: Option<&AHashSet<String>>,
    ) -> crate::lang::errors::CompilerErrors {
        use crate::lang::errors::{CompilerError, CompilerErrors};

        fn synthesize(message: String) -> CompilerErrors {
            let mut errs = CompilerErrors::new();
            errs.add(CompilerError::InvalidFunctionCall {
                func_name: "<pipeline>".to_string(),
                message,
                location: None,
            });
            errs
        }

        // Phase 1: Discover compilation units
        let units = match discover_compilation_units(conf, project_name, src_paths, test_paths) {
            Ok(u) => u,
            Err(msg) => return synthesize(msg),
        };

        // Phase 2: Parse units (errors come back pre-formatted from the parser)
        let (namespaces, parsed_files) = match parse_units_with_cache(&units, false) {
            Ok(pair) => pair,
            Err(_msg) => {
                // Re-parse each file directly to recover structured per-file
                // errors with line/column information. Slower than the cached
                // pipeline but only runs on the failure path.
                let mut errs = CompilerErrors::new();
                for unit in &units {
                    for file_path in &unit.files {
                        let content = match std::fs::read_to_string(file_path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        if let Err(parse_err) =
                            crate::lang::parser::parse_hot_file(&content, file_path)
                        {
                            errs.add_source(file_path.clone(), content.clone());
                            // Use the raw message — the outer ariadne renderer
                            // in `to_diagnostics()` will wrap it with the
                            // snippet using the location we attach below, so
                            // we don't want to embed a second pre-rendered
                            // ariadne report inside it.
                            let location = crate::lang::errors::ErrorLocation {
                                line: parse_err.location.line,
                                column: parse_err.location.column,
                                position: parse_err.location.position,
                                length: parse_err.location.length.max(1),
                                file: parse_err
                                    .location
                                    .file
                                    .clone()
                                    .or_else(|| Some(std::path::PathBuf::from(file_path))),
                            };
                            errs.add(CompilerError::InvalidFunctionCall {
                                func_name: "<parse>".to_string(),
                                message: parse_err.message.clone(),
                                location: Some(location),
                            });
                        }
                    }
                }
                if errs.is_empty() {
                    // Couldn't pin down a specific file — fall back to the
                    // pre-formatted aggregate string.
                    return synthesize(_msg);
                }
                return errs;
            }
        };

        // Phase 1.5: Resolve variable references
        let mut combined_program = crate::lang::ast::Program {
            namespaces,
            current_namespace: crate::lang::ast::NsPath::hot_main(),
        };
        crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

        // Phase 2: Compile + validate
        let mut compiler = crate::lang::Compiler::new();
        for parsed in &parsed_files {
            compiler.add_source_file(
                std::path::PathBuf::from(&parsed.path),
                parsed.content.clone(),
            );
        }

        let compile_result = compiler.compile_program(&mut combined_program);
        // Capture non-fatal warnings (deprecated-API usage), scoped to
        // project-owned files so dependency/stdlib code never adds noise.
        let scoped_warnings = {
            let project_files = Self::project_source_files(&units);
            Self::scope_warnings_to_project(compiler.take_diagnostics(), &project_files)
        };
        let mut errors = match compile_result {
            Ok(()) => CompilerErrors::new(),
            Err(mut errors) => {
                // Make sure source content is attached so diagnostics can
                // render rich messages via ariadne in `to_diagnostics()`.
                // `parsed_files` only contains freshly-parsed (cache-miss)
                // files, so on subsequent runs cached source files won't be
                // in the cache. Lazy-load any file referenced by an error
                // location that isn't already cached.
                for parsed in &parsed_files {
                    errors.add_source(parsed.path.clone(), parsed.content.clone());
                }
                let referenced_paths: AHashSet<String> = errors
                    .errors
                    .iter()
                    .filter_map(|e| {
                        e.location()
                            .and_then(|l| l.file.as_ref())
                            .map(|f| f.display().to_string())
                    })
                    .collect();
                for path in referenced_paths {
                    if errors.source_cache.contains_key(&path) {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        errors.add_source(path, content);
                    }
                }
                errors
            }
        };

        // Optional ctx-requirements check. Each missing key becomes a
        // `CallLibError` with a file-level location (no line/column,
        // since meta declarations don't carry per-key spans yet).
        if let Some(available) = available_ctx_keys {
            let call_graph = crate::lang::compiler::call_graph::CallGraph::build(&combined_program);
            let ctx_requirements = call_graph.resolve_user_ctx_requirements(&combined_program);
            if ctx_requirements.has_requirements() {
                let result = crate::lang::compiler::ctx_checker::check_ctx_requirements(
                    &ctx_requirements,
                    available,
                );
                for missing in &result.missing {
                    let location =
                        missing
                            .source_file
                            .as_ref()
                            .map(|f| crate::lang::errors::ErrorLocation {
                                line: 1,
                                column: 1,
                                position: 0,
                                length: 1,
                                file: Some(std::path::PathBuf::from(f)),
                            });
                    errors.add(CompilerError::CallLibError {
                        func_name: format!("<ctx:{}>", missing.key),
                        message: format!(
                            "Missing required context variable '{}' (required by {})",
                            missing.key, missing.namespace
                        ),
                        location,
                    });
                }
            }
        }

        // Merge in the deprecation warnings. They ride alongside errors so
        // `to_diagnostics()` emits them (severity 2), but never affect
        // `is_empty()` / the success exit code.
        errors.warnings.extend(scoped_warnings.warnings);
        for (path, content) in scoped_warnings.source_cache {
            errors.add_source(path, content);
        }

        errors
    }

    /// Check project sources using the unified pipeline with full dependency loading
    /// This compiles all sources and reports any compilation errors without executing
    pub fn check_sources_pipeline_with_deps(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        color: bool,
    ) -> Result<(), String> {
        Self::check_sources_pipeline_with_context(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
            color,
        )
    }

    /// Check project sources with context requirements validation
    /// This compiles all sources, checks ctx requirements, and reports errors without executing
    pub fn check_sources_pipeline_with_context(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        context_storage: Option<&AHashMap<String, crate::val::Val>>,
        color: bool,
    ) -> Result<(), String> {
        Self::check_sources_pipeline_with_context_warnings(
            src_paths,
            test_paths,
            conf,
            project_name,
            context_storage,
            color,
            None,
        )
    }

    /// Like `check_sources_pipeline_with_context`, but additionally fills
    /// `warnings_out` with project-scoped, non-fatal warnings (e.g. deprecated
    /// API usage) collected during a successful check. Errors are still
    /// returned via `Err(String)`; warnings never affect the result.
    pub fn check_sources_pipeline_with_context_warnings(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        context_storage: Option<&AHashMap<String, crate::val::Val>>,
        color: bool,
        warnings_out: Option<&mut crate::lang::errors::CompilerErrors>,
    ) -> Result<(), String> {
        // Delegate to unified pipeline with Check mode
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            None, // target_file
            None, // eval_code
            PipelineMode::Check,
            None, // emitter
            None, // execution_context
            None, // event_publisher
            context_storage.cloned(),
            None, // database_pool
            None, // file_storage
            None, // store
            None, // embedding_provider
            None, // task_queue
            None, // stream_publisher
            color,
            warnings_out,
        )
        .map(|_| ())
    }

    pub(super) fn runtime_context_validation_policy(
        context_storage: Option<&AHashMap<String, crate::val::Val>>,
    ) -> ContextValidationPolicy {
        ContextValidationPolicy::RequireSatisfied(
            context_storage
                .map(|storage| storage.keys().cloned().collect())
                .unwrap_or_default(),
        )
    }

    /// Post-compile structural validations shared by build, check, and execute
    /// modes, with an explicit policy for runtime context satisfaction.
    ///
    /// These are cheap passes over the already-compiled program. Running them
    /// in Execute mode as well gives `run`/`eval` the same pre-flight failures
    /// `check` reports — before any user code executes — without paying for
    /// the separate check pipeline's second full compile.
    pub(super) fn validate_compiled_program(
        compiler: &mut crate::lang::Compiler,
        combined_program: &crate::lang::ast::Program,
        conf: Option<&crate::val::Val>,
        context_policy: ContextValidationPolicy,
        color: bool,
    ) -> Result<crate::lang::compiler::ctx_checker::ProgramCtxRequirements, String> {
        // Validate event handlers (event parameter requirement)
        if let Err(e) = compiler.extract_event_handlers(combined_program) {
            let formatted = e.format_error(color);
            return Err(String::from("Event handler validation errors:\n") + &formatted);
        }

        // Validate scheduled functions (event parameter requirement)
        if let Err(e) = compiler.extract_scheduled_functions(combined_program) {
            let formatted = e.format_error(color);
            return Err(String::from("Scheduled function validation errors:\n") + &formatted);
        }
        if let Some(conf) = conf {
            let policy = crate::db::SchedulePolicy::from_conf(conf);
            for cron_expression in compiler.get_scheduled_functions().keys() {
                crate::db::validate_recurring_schedule_interval(
                    cron_expression,
                    policy.min_interval_secs,
                )
                .map_err(|e| format!("Schedule policy error: {}", e.message()))?;
            }
        }

        // Build call graph once for both ctx and box requirement resolution
        let call_graph = crate::lang::compiler::call_graph::CallGraph::build(combined_program);

        // Check context requirements using call graph resolution
        // This only includes ctx requirements reachable from user code.
        let ctx_requirements = call_graph.resolve_user_ctx_requirements(combined_program);

        if ctx_requirements.has_requirements()
            && let ContextValidationPolicy::RequireSatisfied(available_keys) = context_policy
        {
            let ctx_result = crate::lang::compiler::ctx_checker::check_ctx_requirements(
                &ctx_requirements,
                &available_keys,
            );

            if !ctx_result.is_ok() {
                return Err(ctx_result.format_errors());
            }

            tracing::debug!(
                " Context requirements satisfied: {} required keys found",
                ctx_result.found_keys.len()
            );
        }

        // Check box requirements: validate size names and report
        let box_requirements = call_graph.resolve_user_box_requirements(combined_program);

        if box_requirements.has_requirements() {
            // Validate all declared size names are recognized
            for req in &box_requirements.requirements {
                if let Some(ref size) = req.requirement.min_size
                    && crate::lang::compiler::box_checker::size_memory_mb(size).is_none()
                {
                    return Err(format!(
                        "Invalid box size '{}' in meta {{box: {{min-size: \"{}\"}}}} on {}\n\
                         Valid sizes: nano, micro, small, medium, large, xlarge, 2xlarge, 4xlarge",
                        size, size, req.fqn
                    ));
                }
            }

            if let Some(min_size) = box_requirements.effective_min_size() {
                tracing::debug!(
                    " Box requirements: min-size={}, network={}, from {} function(s)",
                    min_size,
                    box_requirements.requires_network(),
                    box_requirements.requirements.len()
                );
            }
        }

        // Hand the resolved ctx requirements back so Execute mode can apply
        // defaults and secret keys without rebuilding the call graph.
        Ok(ctx_requirements)
    }

    /// Unified pipeline for all operations (run, eval, test, repl)
    #[allow(clippy::too_many_arguments)]
    pub fn run_unified_pipeline(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        target_file: Option<&str>,
        eval_code: Option<&str>,
        mode: PipelineMode,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        file_storage: Option<Arc<dyn crate::file_storage::FileStorage>>,
        store: Option<Arc<dyn crate::store::Store>>,
        embedding_provider: Option<Arc<dyn crate::store::embedding::EmbeddingProvider>>,
        task_queue: Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
        color: bool,
        warnings_out: Option<&mut crate::lang::errors::CompilerErrors>,
    ) -> Result<crate::val::Val, String> {
        tracing::debug!(" Unified Pipeline: Loading Hot dependencies...");
        tracing::debug!(" Source paths: {:?}", src_paths);
        tracing::debug!(" Test paths: {:?}", test_paths);
        tracing::debug!(" Mode: {:?}", mode);

        // Phase 1: Discover compilation units (packages and source paths)
        tracing::debug!(" Unified Pipeline: Phase 1 - Discovering compilation units...");
        let units = discover_compilation_units(conf, project_name, src_paths, test_paths)?;

        tracing::debug!(
            " Discovered {} compilation units: {}",
            units.len(),
            units
                .iter()
                .map(|u| u.unit.id())
                .collect::<Vec<_>>()
                .join(", ")
        );

        // NOTE: an ad hoc-eval fast path over the precompiled hot-std image
        // (lang::cache::std_artifact) lived here and was removed after
        // measurement: it ran ~191ms vs ~60ms for this classic path. The
        // image removes *compilation* of hot-std, but an eval snippet still
        // needs the full merged AST resolved and type-checked for
        // correctness (~110ms), so the image decode (~65ms) was pure
        // addition. Caching bytecode cannot skip semantic analysis of
        // arbitrary user code; the AST unit cache (6ms load) plus a normal
        // compile is strictly cheaper. The seeded compiler extension it used
        // (Compiler::seed_from_compiled / Engine::execute_eval_extension)
        // remains in use by the worker, where the build is already resident
        // in memory and no decode is paid.

        // Phase 2: Parse units with per-package caching
        tracing::debug!(" Unified Pipeline: Phase 2 - Parsing with per-package cache...");
        let (namespaces, parsed_files) = parse_units_with_cache(&units, color)?;

        tracing::debug!(
            " Loaded {} namespaces from {} units",
            namespaces.len(),
            units.len()
        );

        // Handle target file if provided (not cached since it may be an ad-hoc file)
        let mut extra_parsed_files = Vec::new();
        if let Some(target_file) = target_file {
            let (parsed, errors) = parse_files_parallel(&[target_file.to_string()], color);
            if !errors.is_empty() {
                return Err(format!(
                    "Parse errors in target file:\n{}",
                    errors.join("\n")
                ));
            }
            extra_parsed_files.extend(parsed);
        }

        // Create combined program by merging all namespaces
        let mut combined_program = crate::lang::ast::Program {
            namespaces,
            current_namespace: crate::lang::ast::NsPath::hot_main(),
        };

        // Add namespaces from target file. Member-merged: the target file is
        // usually also discovered as part of its source unit (run pushes the
        // file's directory onto src paths), and the same-file bindings are
        // deduplicated by merge_namespace.
        for parsed in &extra_parsed_files {
            for (ns_path, namespace) in &parsed.namespaces {
                tracing::debug!(
                    "  Found namespace '{}' with {} variables (from target file)",
                    ns_path,
                    namespace.scope.vars.len()
                );
                merge_namespace(
                    &mut combined_program.namespaces,
                    ns_path.clone(),
                    namespace.clone(),
                    NsMergePolicy::Strict,
                )?;
            }
        }

        // Combine all parsed files for source file tracking
        let all_parsed_files: Vec<_> = parsed_files.into_iter().chain(extra_parsed_files).collect();

        // Track if we have parse errors from eval code
        let mut has_parse_errors = false;
        let mut eval_parse_errors = Vec::new();
        // Store eval source content for pretty error reporting
        let mut eval_source_content: Option<String> = None;

        // Handle eval code if provided
        if let Some(code) = eval_code {
            // Use a unique namespace to prevent conflicts with user code
            // Format: ::hot::$run::run-<short-id> where short-id is last 12 chars of UUID without hyphens
            let eval_ns = if let Some(ctx) = &execution_context {
                let run_id_str = ctx.run_id.to_string();
                let run_id_no_hyphens = run_id_str.replace('-', "");
                let short_id = if run_id_no_hyphens.len() >= 12 {
                    &run_id_no_hyphens[run_id_no_hyphens.len() - 12..]
                } else {
                    &run_id_no_hyphens
                };
                format!("::hot::$run::run-{}", short_id)
            } else {
                let temp_uuid = uuid::Uuid::now_v7().to_string();
                let temp_no_hyphens = temp_uuid.replace('-', "");
                let short_id = if temp_no_hyphens.len() >= 12 {
                    &temp_no_hyphens[temp_no_hyphens.len() - 12..]
                } else {
                    &temp_no_hyphens
                };
                format!("::hot::$run::run-{}", short_id)
            };

            let eval_code_with_namespace = format!("{} ns\n{}", eval_ns, code);
            // Store eval code for pretty error reporting later
            eval_source_content = Some(eval_code_with_namespace.clone());
            tracing::debug!(" Parsing eval code with namespace: {}", eval_ns);
            match crate::lang::parser::parse_hot_file(&eval_code_with_namespace, "<eval>") {
                Ok(eval_program) => {
                    tracing::debug!(
                        "Eval program has {} namespaces",
                        eval_program.namespaces.len()
                    );
                    // Merge namespaces from eval code. Shadow policy: eval
                    // input may re-declare an existing name (REPL-style
                    // redefinition) and the newest binding wins, but other
                    // members of an existing namespace are preserved —
                    // previously an eval `::ns ns` declaration replaced the
                    // project's entire namespace.
                    for (ns_path, namespace) in eval_program.namespaces {
                        tracing::debug!(
                            "Merging eval namespace '{}' with {} variables",
                            ns_path,
                            namespace.scope.vars.len()
                        );
                        for (var, _value) in &namespace.scope.vars {
                            tracing::debug!("Eval variable: '{}'", var.sym.name());
                        }
                        merge_namespace(
                            &mut combined_program.namespaces,
                            ns_path,
                            namespace,
                            NsMergePolicy::Shadow,
                        )?;
                    }
                }
                Err(e) => {
                    has_parse_errors = true;
                    if let Some(formatted) = e.format_error(code, color) {
                        tracing::debug!("Parse error in eval code:\n{}", formatted);
                        eval_parse_errors.push(formatted);
                    } else {
                        let error_msg = format!("Parse error in eval code: {}", e);
                        eval_parse_errors.push(error_msg);
                    }
                }
            }
        }

        // Stop if ANY parse errors were found in eval code
        if has_parse_errors || !eval_parse_errors.is_empty() {
            return Err(String::from("Parse errors:\n") + &eval_parse_errors.join("\n"));
        }

        // Phase 1.5: Resolve variable references to namespace functions
        tracing::debug!(" Unified Pipeline: Phase 1.5 - Resolving variable references...");
        crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

        // Phase 2: Compile the combined program with two-pass approach
        tracing::debug!(" Unified Pipeline: Phase 2 - Compiling combined program...");
        let mut compiler = crate::lang::Compiler::new();

        // Set file information for all files (reuse already-parsed content for efficiency)
        for parsed in &all_parsed_files {
            compiler.add_source_file(
                std::path::PathBuf::from(&parsed.path),
                parsed.content.clone(),
            );
        }

        // Add eval source content for pretty error reporting
        if let Some(eval_content) = eval_source_content {
            compiler.add_source_file(std::path::PathBuf::from("<eval>"), eval_content);
        }

        // Set the primary file for the target file if available
        if let Some(target_file) = target_file {
            // Try to find content from already-parsed files first
            let content = all_parsed_files
                .iter()
                .find(|p| p.path == target_file)
                .map(|p| p.content.clone())
                .or_else(|| std::fs::read_to_string(target_file).ok());
            if let Some(content) = content {
                compiler.set_current_file(std::path::PathBuf::from(target_file), content);
            }
        }

        // Try comprehensive compilation with validation first
        match compiler.compile_program(&mut combined_program) {
            Ok(()) => {
                // Compilation with validation succeeded. Capture non-fatal
                // deprecation warnings for the caller (scoped to project files)
                // without failing the check.
                if let Some(out) = warnings_out {
                    let project_files = Self::project_source_files(&units);
                    *out = Self::scope_warnings_to_project(
                        compiler.take_diagnostics(),
                        &project_files,
                    );
                }
            }
            Err(errors) => {
                let formatted = errors.format_error(color);
                let error_message = String::from("Compile-time errors found:\n") + &formatted;
                // tracing::error!(" Unified Pipeline compilation failed: {}", error_message);
                return Err(error_message);
            }
        }

        // Also check for call-lib validation errors (legacy support)
        let call_lib_errors = compiler.get_call_lib_errors();
        if !call_lib_errors.is_empty() {
            // Show all missing functions at once!
            let error_message = if call_lib_errors.len() == 1 {
                String::from("Compile-time error: ") + &call_lib_errors[0]
            } else {
                let formatted_errors = call_lib_errors
                    .iter()
                    .enumerate()
                    .map(|(i, err)| format!("  {}. {}", i + 1, err))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Compile-time errors found ({} missing functions):\n{}",
                    call_lib_errors.len(),
                    formatted_errors
                )
            };
            // tracing::error!(" Unified Pipeline compilation failed: {}", error_message);
            return Err(error_message);
        }

        tracing::debug!(" Compilation successful!");

        // Populate the runtime tool-schema registry so that
        // ::hot::internal::mcp/schema-from-fn (and ::ai::tool/schema-from-fn)
        // resolve any Hot function to its {input-schema, output-schema}
        // during execution. Done after successful compilation so we have
        // the full combined program AST available.
        if let Err(e) = compiler.extract_tool_specs(&combined_program) {
            tracing::warn!(
                "Tool spec extraction failed (continuing): {}",
                e.format_error(color)
            );
        }
        if let Err(e) = compiler.extract_skill_specs(&combined_program) {
            tracing::warn!(
                "Skill spec extraction failed (continuing): {}",
                e.format_error(color)
            );
        }

        tracing::debug!(" Unified Pipeline: Phase 3 - Executing program...");

        // Phase 3: Execute based on mode
        match mode {
            PipelineMode::StructuralValidation => {
                Self::validate_compiled_program(
                    &mut compiler,
                    &combined_program,
                    conf,
                    ContextValidationPolicy::CollectOnly,
                    color,
                )?;

                tracing::debug!(" Unified Pipeline: structural validation completed successfully");
                Ok(crate::val::Val::Null)
            }
            PipelineMode::Check => {
                // Check mode: validate only, no VM execution
                tracing::debug!(" Unified Pipeline: Check mode - validating...");

                Self::validate_compiled_program(
                    &mut compiler,
                    &combined_program,
                    conf,
                    Self::runtime_context_validation_policy(context_storage.as_ref()),
                    color,
                )?;

                tracing::debug!(" Unified Pipeline: Check completed successfully");
                Ok(crate::val::Val::Null)
            }
            PipelineMode::Execute => {
                // Run the same post-compile validations as Check mode before
                // any user code executes. This preserves pre-flight failures
                // (e.g. missing required ctx keys) for run/eval without a
                // separate check pipeline pass. The returned ctx
                // requirements are reused below for defaults/secret keys.
                let ctx_requirements = Self::validate_compiled_program(
                    &mut compiler,
                    &combined_program,
                    conf,
                    Self::runtime_context_validation_policy(context_storage.as_ref()),
                    color,
                )?;

                // Execute and return final result using Arc for efficient sharing
                let program = compiler.get_program_arc();
                let hot_ast = Arc::new(crate::lang::ast::HotAst::from_program(
                    combined_program.clone(),
                ));
                let mut vm = crate::lang::VirtualMachine::new(
                    program,
                    Some(hot_ast),
                    compiler.get_function_mapping_arc(),
                    compiler.get_core_functions_arc(),
                    compiler.get_type_implementations_arc(),
                    compiler.get_core_variables_arc(),
                    conf.cloned(),
                );

                // Set emitter, execution context, and event publisher if provided
                if let Some(emitter) = emitter {
                    vm.set_emitter(emitter);
                }
                if let Some(execution_context) = execution_context {
                    vm.set_execution_context(execution_context);
                }
                if let Some(event_publisher) = event_publisher {
                    vm.set_event_publisher(event_publisher);
                }
                // Set context storage if provided (pre-loaded context variables from database)
                if let Some(context_storage) = context_storage.clone() {
                    vm.context_storage = context_storage;
                }

                // Apply defaults + secret keys from the ctx requirements
                // the validation pass already resolved (call graph built once).
                // Apply default values for keys not already in context_storage
                for (key, default_val) in ctx_requirements.all_defaults() {
                    vm.context_storage.entry(key).or_insert(default_val);
                }
                // Set secret keys (all keys that are not marked visible)
                vm.secret_keys = ctx_requirements.all_secret_keys();
                vm.sync_secret_keys_to_execution_context();

                // Set database pool and file storage if provided
                if let Some(db_pool) = database_pool {
                    vm.set_database_pool(db_pool);
                }
                if let Some(storage) = file_storage {
                    vm.set_file_storage(storage);
                }
                if let Some(s) = store {
                    vm.set_store(s);
                }
                if let Some(ep) = embedding_provider {
                    vm.set_embedding_provider(ep);
                }

                match vm.execute() {
                    Ok(result) => {
                        tracing::debug!(" Unified Pipeline execution completed successfully");

                        if eval_code.is_some() {
                            // The VM tracks the destination of the final value-producing
                            // instruction. That is the eval expression's value even when
                            // it is Null. Falling back to the namespace's last binding
                            // would incorrectly print a preceding key variable for a
                            // terminal missing-key lookup.
                            return Ok(result);
                        }

                        Ok(result)
                    }
                    Err(e) => {
                        tracing::error!(" Unified Pipeline execution failed: {}", e);
                        Err(format!("Execution failed: {}", e))
                    }
                }
            }
            PipelineMode::Test {
                pattern,
                capture_output,
            } => {
                // Execute the Hot test runner functions
                tracing::debug!(" Unified Pipeline: Running tests...");

                // Create VM for test execution using Arc for efficient sharing
                let program = compiler.get_program_arc();
                let hot_ast = Arc::new(crate::lang::ast::HotAst::from_program(
                    combined_program.clone(),
                ));
                let mut vm = crate::lang::VirtualMachine::new(
                    program,
                    Some(hot_ast.clone()),
                    compiler.get_function_mapping_arc(),
                    compiler.get_core_functions_arc(),
                    compiler.get_type_implementations_arc(),
                    compiler.get_core_variables_arc(),
                    conf.cloned(),
                );

                // Set emitter, execution context, and event publisher if provided
                if let Some(emitter) = emitter {
                    vm.set_emitter(emitter);
                }
                if let Some(execution_context) = execution_context {
                    vm.set_execution_context(execution_context);
                }
                if let Some(event_publisher) = event_publisher {
                    vm.set_event_publisher(event_publisher);
                }
                // Set context storage if provided (pre-loaded context variables from database)
                if let Some(context_storage) = context_storage.clone() {
                    vm.context_storage = context_storage;
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

                // Set database pool and file storage if provided
                if let Some(db_pool) = database_pool {
                    vm.set_database_pool(db_pool);
                }
                if let Some(storage) = file_storage {
                    vm.set_file_storage(storage);
                }
                if let Some(s) = store {
                    vm.set_store(s);
                }
                if let Some(ep) = embedding_provider {
                    vm.set_embedding_provider(ep);
                }
                if let Some(tq) = task_queue {
                    vm.set_task_queue(tq);
                }
                if let Some(sp) = stream_publisher {
                    vm.set_stream_publisher(sp);
                }

                match vm.execute() {
                    Ok(_) => {
                        // Call the Hot test runner once using the initialized VM
                        let test_result = if let Some(pattern) = pattern {
                            tracing::debug!(" Running tests matching pattern: {}", pattern);
                            Self::call_test_runner_with_pattern(
                                &mut vm,
                                &pattern,
                                capture_output,
                                color,
                            )
                        } else {
                            tracing::debug!(" Running all tests via Hot test runner");
                            Self::call_test_runner(&mut vm, capture_output, color)
                        };

                        match test_result {
                            Ok(val) => Ok(val),
                            Err(e) => {
                                // Fallback: if the Hot runner already set ::hot::test/test-run-success,
                                // use that value instead of failing the whole run.
                                // Debug-level only: the returned Err surfaces
                                // to the user via the CLI.
                                tracing::debug!(" Test execution failed: {}", e);
                                if let Ok(val) = vm.lookup_variable("::hot::test/test-run-success")
                                {
                                    // Accept Bool or Result.Ok variant ({$type: "::hot::type/Result.Ok", $val: ...})
                                    let success = match &val {
                                        crate::val::Val::Bool(b) => *b,
                                        crate::val::Val::Map(m) => {
                                            let v = m.get(&crate::val::Val::from("$type"));
                                            if let Some(crate::val::Val::Str(type_name)) = v
                                                && &**type_name == "::hot::type/Result.Ok"
                                            {
                                                // Extract $val field
                                                if let Some(ok_val) =
                                                    m.get(&crate::val::Val::from("$val"))
                                                {
                                                    matches!(ok_val, crate::val::Val::Bool(true))
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        _ => false,
                                    };
                                    return Ok(crate::val::Val::Bool(success));
                                }
                                // The message already says the test runner
                                // failed; don't stack another prefix on it.
                                Err(e)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(" Unified Pipeline execution failed before tests: {}", e);
                        Err(format!("Execution failed before tests: {}", e))
                    }
                }
            }
        }
    }

    /// Unified pipeline that returns both execution result and compilation artifacts
    /// This eliminates double-compilation on first run in the worker
    #[allow(clippy::too_many_arguments)]
    pub fn run_unified_pipeline_with_artifacts(
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        target_file: Option<&str>,
        eval_code: Option<&str>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        file_storage: Option<Arc<dyn crate::file_storage::FileStorage>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
        store: Option<Arc<dyn crate::store::Store>>,
        embedding_provider: Option<Arc<dyn crate::store::embedding::EmbeddingProvider>>,
        color: bool,
    ) -> Result<ExecutionWithArtifacts, String> {
        tracing::debug!(" Unified Pipeline (with artifacts): Loading Hot dependencies...");

        // Collect all .hot files in the correct loading order
        let mut all_hot_files = Vec::new();
        let mut loaded_packages = AHashSet::new();

        // Always inject hot-std FIRST from system location
        // hot-std is tied to the Hot runtime version, so bundles don't include it
        // and both live and bundle builds use the system hot-std
        let has_sources = !src_paths.is_empty() || !test_paths.is_empty() || conf.is_some();
        tracing::debug!(
            "run_unified_pipeline: has_sources={}, src_paths={}, test_paths={}, conf={}",
            has_sources,
            src_paths.len(),
            test_paths.len(),
            conf.is_some()
        );
        if has_sources {
            let resolver = crate::lang::project::DependencyResolver::default();
            let hot_std = resolver.get_hot_std_dependency();
            let hot_std_path = hot_std.resolved_path.to_string_lossy().to_string();

            match Self::discover_hot_files(&hot_std_path) {
                Ok(files) if !files.is_empty() => {
                    tracing::debug!(
                        "Injecting hot-std from system: {} ({} files)",
                        hot_std_path,
                        files.len()
                    );
                    all_hot_files.extend(files);
                    loaded_packages.insert("hot-std".to_string());
                }
                Ok(_) => {
                    tracing::error!(
                        "hot-std found at {} but contains no .hot files!",
                        hot_std_path
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to discover hot-std files at {}: {}",
                        hot_std_path,
                        e
                    );
                }
            }
        }

        // Load project dependencies in the correct order if configuration is provided
        if let (Some(conf), Some(project_name)) = (conf, project_name) {
            match crate::project::get_resolved_project_dependencies(conf, project_name) {
                Ok(resolved_deps) => {
                    for dep in &resolved_deps {
                        // Skip hot-std since we already loaded it above
                        if loaded_packages.contains(&dep.name) {
                            continue;
                        }
                        // Use discover_dependency_source_files to only load src_paths, not test_paths
                        match Self::discover_dependency_source_files(&dep.resolved_path) {
                            Ok(files) => {
                                all_hot_files.extend(files);
                                loaded_packages.insert(dep.name.clone());
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Warning: Failed to discover files in dependency '{}': {}",
                                    dep.name,
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Warning: Failed to load project dependencies: {}", e);
                }
            }
        }

        // Load source files
        for src_path in src_paths {
            match Self::discover_hot_files(src_path) {
                Ok(files) => all_hot_files.extend(files),
                Err(e) => {
                    tracing::warn!("Warning: Failed to discover files in {}: {}", src_path, e)
                }
            }
        }

        // Load test files
        for test_path in test_paths {
            match Self::discover_hot_files(test_path) {
                Ok(files) => all_hot_files.extend(files),
                Err(e) => {
                    tracing::warn!("Warning: Failed to discover files in {}: {}", test_path, e)
                }
            }
        }

        // Remove duplicates while preserving order
        let mut seen = AHashSet::new();
        all_hot_files.retain(|file| seen.insert(file.clone()));

        // Parse all files into a combined program
        let mut combined_program = crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::hot_main(),
        };

        // Parse all source files in parallel for better performance
        let (parsed_files, parse_errors) = parse_files_parallel(&all_hot_files, color);

        if !parse_errors.is_empty() {
            return Err(format!("Parse errors:\n{}", parse_errors.join("\n")));
        }

        // Merge parsed namespaces member-by-member using the same strict
        // policy as the regular pipeline. Whole-namespace replacement here
        // made worker/std artifacts silently drop declarations.
        for parsed in &parsed_files {
            super::discover::merge_program_namespaces_strict(
                &mut combined_program.namespaces,
                parsed.namespaces.clone(),
            )?;
        }

        // Resolve variable references on the BASE program (before eval code)
        crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

        // Compile the BASE program (without eval code) for caching
        let mut compiler = crate::lang::compiler::Compiler::new();

        // Set file information for error reporting (reuse already-loaded content)
        for parsed in &parsed_files {
            compiler.add_source_file(
                std::path::PathBuf::from(&parsed.path),
                parsed.content.clone(),
            );
        }

        if let Some(target_file) = target_file
            && let Ok(content) = std::fs::read_to_string(target_file)
        {
            compiler.set_current_file(std::path::PathBuf::from(target_file), content);
        }

        // Compile base program with validation
        compiler
            .compile_program(&mut combined_program)
            .map_err(|errors| format!("Compilation errors:\n{}", errors.format_error(color)))?;

        // Populate the runtime tool-schema and skill-spec registries so that
        // ::hot::internal::mcp/schema-from-fn (and ::ai::tool/schema-from-fn)
        // resolve any Hot function to its {input-schema, output-schema}
        // during phase-3 execution. Without this, module-level statements
        // like `agent-tools [::tool/from-fn(::ns/fn, ...)]` evaluated during
        // `vm.execute()` below would error out with
        // "no schema registered for ...". Mirrors the same step in
        // `run_unified_pipeline`.
        if let Err(e) = compiler.extract_tool_specs(&combined_program) {
            tracing::warn!(
                "Tool spec extraction failed (continuing): {}",
                e.format_error(color)
            );
        }
        if let Err(e) = compiler.extract_skill_specs(&combined_program) {
            tracing::warn!(
                "Skill spec extraction failed (continuing): {}",
                e.format_error(color)
            );
        }

        tracing::debug!(" Base compilation successful, extracting artifacts for caching...");

        // Extract compilation artifacts from BASE program (without eval code)
        // This is critical: cached artifacts must NOT include eval code
        let artifacts = CompilationArtifacts {
            program: compiler.get_program().clone(),
            function_mapping: compiler.get_function_mapping().clone(),
            core_functions: compiler.get_core_functions().clone(),
            type_implementations: compiler.get_type_implementations().clone(),
            ast_program: combined_program.clone(),
            hot_ast: crate::lang::ast::HotAst::from_program(combined_program.clone()),
        };

        // Eval extends the base through the same seeded compiler path used by
        // persisted bytecode cache hits. Cacheable artifacts remain base-only.
        if let Some(code) = eval_code {
            let result = Self::execute_eval_extension(
                code,
                compiler.seed(),
                &combined_program,
                super::cache::EvalRuntimeOptions {
                    conf,
                    emitter,
                    execution_context,
                    event_publisher,
                    context_storage,
                    database_pool,
                    file_storage,
                    store,
                    embedding_provider,
                    task_queue: None,
                    stream_publisher,
                    color,
                    warnings_out: None,
                },
            )?;
            return Ok(ExecutionWithArtifacts {
                result,
                artifacts: Some(artifacts),
            });
        }

        let final_program = compiler.get_program_arc();
        let final_combined_program = combined_program;

        // Extract ctx requirements via call graph before consuming final_combined_program
        let ctx_requirements =
            crate::lang::compiler::ctx_checker::extract_ctx_requirements_via_call_graph(
                &final_combined_program,
            );

        // Build HotAst from final combined program
        let hot_ast = Arc::new(crate::lang::ast::HotAst::from_program(
            final_combined_program,
        ));

        // Execute the program
        let mut vm = crate::lang::VirtualMachine::new(
            final_program,
            Some(hot_ast),
            compiler.get_function_mapping_arc(),
            compiler.get_core_functions_arc(),
            compiler.get_type_implementations_arc(),
            compiler.get_core_variables_arc(),
            conf.cloned(),
        );

        // Configure VM with optional features
        if let Some(emitter) = emitter {
            vm.set_emitter(emitter);
        }
        if let Some(execution_context) = execution_context {
            vm.set_execution_context(execution_context);
        }
        if let Some(event_publisher) = event_publisher {
            vm.set_event_publisher(event_publisher);
        }
        if let Some(context_storage) = context_storage {
            vm.context_storage = context_storage;
        }

        // Apply defaults + secret keys
        for (key, default_val) in ctx_requirements.all_defaults() {
            vm.context_storage.entry(key).or_insert(default_val);
        }
        vm.secret_keys = ctx_requirements.all_secret_keys();
        vm.sync_secret_keys_to_execution_context();

        if let Some(db_pool) = database_pool {
            vm.set_database_pool(db_pool);
        }
        if let Some(storage) = file_storage {
            vm.set_file_storage(storage);
        }
        if let Some(s) = store {
            vm.set_store(s);
        }
        if let Some(ep) = embedding_provider {
            vm.set_embedding_provider(ep);
        }
        if let Some(publisher) = stream_publisher {
            vm.set_stream_publisher(publisher);
        }

        // Execute
        let result = vm
            .execute()
            .map_err(|e| format!("Execution failed: {}", e))?;

        Ok(ExecutionWithArtifacts {
            result,
            artifacts: Some(artifacts),
        })
    }

    /// Call the Hot test runner function ::hot::test/run-tests(capture-output)
    fn call_test_runner(
        vm: &mut crate::lang::VirtualMachine,
        capture_output: bool,
        color: bool,
    ) -> Result<crate::val::Val, String> {
        tracing::debug!(" Calling ::hot::test/run-tests({})", capture_output);

        // Call the Hot function ::hot::test/run-tests with capture_output parameter
        let args = vec![crate::val::Val::Bool(capture_output)];

        tracing::debug!("Starting test execution...");

        let result = vm.execute_function_call_by_name("::hot::test/run-tests", &args);

        match result {
            Ok(result) => {
                tracing::debug!("Test execution completed successfully");
                // Bool means ran-and-passed/failed; the no-tests sentinel
                // travels through unchanged so the CLI can distinguish
                // "tests failed" from "nothing ran".
                match result {
                    crate::val::Val::Bool(_) => Ok(result),
                    crate::val::Val::Str(ref s)
                        if &**s == crate::lang::engine::NO_TESTS_SENTINEL =>
                    {
                        Ok(result)
                    }
                    _ => {
                        tracing::warn!(" Test runner returned non-boolean result: {:?}", result);
                        Ok(crate::val::Val::Bool(false))
                    }
                }
            }
            Err(e) => {
                // Debug-level only: the returned Err surfaces to the user via
                // the CLI, so an error-level log here would print the same
                // failure twice (once as a raw Debug struct).
                tracing::debug!(" Failed to call test runner: {:?}", e);

                // Use the enhanced RuntimeError formatting
                if let crate::lang::runtime::vm::VmError::RuntimeError(runtime_error) = &e {
                    // Try to create a pretty ariadne report if we have source location
                    if let Some(ref location) = runtime_error.location
                        && let Some(ref file_path) = location.file
                        && let Some(file_str) = file_path.to_str()
                        && let Some(source_content) =
                            vm.program.source_map.get_file_content(file_str)
                        && let Some(pretty_report) =
                            runtime_error.format_error(source_content, color)
                    {
                        return Err(format!("Failed to call test runner:\n{}", pretty_report));
                    }

                    // Fallback to the Display implementation which includes function context
                    Err(format!("Failed to call test runner: {}", runtime_error))
                } else {
                    Err(format!("Failed to call test runner: {:?}", e))
                }
            }
        }
    }

    /// Call the Hot test runner function ::hot::test/run-matching-tests(pattern, capture-output)
    fn call_test_runner_with_pattern(
        vm: &mut crate::lang::VirtualMachine,
        pattern: &str,
        capture_output: bool,
        _color: bool,
    ) -> Result<crate::val::Val, String> {
        tracing::debug!(
            " Calling ::hot::test/run-matching-tests({}, {})",
            pattern,
            capture_output
        );

        // Call the Hot function ::hot::test/run-matching-tests with pattern and capture_output parameters
        let args = vec![
            crate::val::Val::from(pattern.to_string()),
            crate::val::Val::Bool(capture_output),
        ];
        match vm.execute_function_call_by_name("::hot::test/run-matching-tests", &args) {
            Ok(result) => {
                // Bool means ran; the no-tests sentinel travels through so a
                // pattern matching nothing is distinguishable from failures.
                match result {
                    crate::val::Val::Bool(_) => Ok(result),
                    crate::val::Val::Str(ref s)
                        if &**s == crate::lang::engine::NO_TESTS_SENTINEL =>
                    {
                        Ok(result)
                    }
                    _ => {
                        tracing::warn!(" Test runner returned non-boolean result: {:?}", result);
                        Ok(crate::val::Val::Bool(false))
                    }
                }
            }
            Err(e) => {
                // Debug-level only: the returned Err surfaces to the user via
                // the CLI, so an error-level log here would print the same
                // failure twice (once as a raw Debug struct).
                tracing::debug!(" Failed to call test runner with pattern: {:?}", e);

                // If the error is about tests-failed variable not found, fall back to direct test discovery
                if e.to_string().contains("tests-failed") {
                    tracing::warn!(
                        "Hot test runner failed with scoping issue, falling back to direct test discovery"
                    );
                    Err(format!("Hot test runner scoping issue: {}", e))
                } else {
                    Err(format!("Failed to call test runner: {}", e))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_compile_allows_required_ctx_but_runtime_execute_rejects_empty_keys() {
        let temp = tempfile::tempdir().unwrap();
        let src_dir = temp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("required_ctx.hot"),
            r#"::required_ctx ns

needs-key
meta {ctx: {"api.key": {required: true}}}
fn (): Str { "unused" }
"#,
        )
        .unwrap();
        let target_file = temp.path().join("validation_target.hot");
        std::fs::write(
            &target_file,
            r#"::validation_target ns

value 1
"#,
        )
        .unwrap();

        let src_paths = vec![src_dir.to_string_lossy().to_string()];
        let target_file = target_file.to_string_lossy().to_string();

        // Structural validation collects the requirement without demanding a value.
        Engine::run_file_pipeline_with_deps(&target_file, &src_paths, &[], None, None, false)
            .expect("structural compile should collect, not require, context");

        // Check and Execute both require it.
        let empty_context = AHashMap::new();
        let check_error = Engine::check_sources_pipeline_with_context(
            &src_paths,
            &[],
            None,
            None,
            Some(&empty_context),
            false,
        )
        .unwrap_err();
        assert!(check_error.contains("Missing required context variable 'api.key'"));

        let error = execute_target(&src_paths, &target_file).unwrap_err();
        assert!(error.contains("Missing required context variable 'api.key'"));
    }

    fn execute_target(src_paths: &[String], target_file: &str) -> Result<crate::val::Val, String> {
        Engine::run_unified_pipeline(
            src_paths,
            &[],
            None,
            None,
            Some(target_file),
            None,
            PipelineMode::Execute,
            None,
            None,
            None,
            Some(AHashMap::new()),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
        )
    }
}
