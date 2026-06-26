//! Bytecode-cache aware execution paths.
//!
//! Two complementary surfaces:
//!
//!   * `prewarm_package_cache` — discovers and parses every project
//!     dependency up front so REPL/test/eval first-input latency goes
//!     away. Also gates `cleanup_stale_test_artifacts`, which sweeps
//!     `.hot/test-*.db` files left behind by crashed test runs.
//!
//!   * `eval_code_with_cached_bytecode`,
//!     `call_function_with_cached_bytecode[_and_task]` — fast path that
//!     skips parse/compile by reusing a `CachedBytecode` blob produced by
//!     `compile_project_for_cache` / `compile_to_cache` /
//!     `artifacts_to_cached_bytecode`. Used by the worker pool when the
//!     bytecode cache hits.

use super::discover::{discover_compilation_units, parse_units_with_cache};
use super::{CompilationArtifacts, Engine, PipelineMode};
use ahash::AHashMap;
use std::sync::Arc;

impl Engine {
    /// Pre-warm the package cache by discovering and parsing all project dependencies.
    /// This is useful for REPL initialization to avoid parsing delays on first input.
    /// Returns the number of units cached.
    pub fn prewarm_package_cache(
        src_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<usize, String> {
        tracing::debug!(
            "Pre-warming package cache: src_paths={:?}, project_name={:?}",
            src_paths,
            project_name
        );

        // Discover all compilation units (dependencies + source paths)
        let units = discover_compilation_units(conf, project_name, src_paths, &[])?;
        tracing::debug!("Pre-warm: discovered {} units", units.len());

        if units.is_empty() {
            return Ok(0);
        }

        // Parse all units with caching - this populates the cache
        let (namespaces, _) = parse_units_with_cache(&units, false)?;
        tracing::debug!(
            "Pre-warm: cached {} namespaces from {} units",
            namespaces.len(),
            units.len()
        );

        Ok(units.len())
    }

    /// Run tests using unified AST with Hot test runner
    #[allow(clippy::too_many_arguments)]
    pub async fn run_tests_with_direct_discovery(
        pattern: Option<&str>,
        capture_output: bool,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        task_queue: Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
        color: bool,
    ) -> Result<bool, String> {
        // Integration mode: task_queue.is_some() means services are running and we must
        // share the same DB they're using (from conf). Otherwise create an ephemeral DB.
        let is_integration = task_queue.is_some();

        // Clean up stale test artifacts, but skip in integration mode to avoid
        // deleting the shared DB that was already set up by the integration orchestrator
        if !is_integration {
            Self::cleanup_stale_test_artifacts();
        }

        // Generate unique test ID for this test run
        let test_id = uuid::Uuid::now_v7().simple().to_string();

        let (temp_db_path, database_pool) = if is_integration {
            // Use the DB from conf — the same one the task worker is using
            let conf_val = conf.unwrap_or(&crate::val::Val::Null);
            tracing::debug!(
                "Integration mode: using shared database: {}",
                crate::db::get_db_uri_from_conf(conf_val)
            );
            let pool = match crate::db::create_db_pool(conf_val).await {
                Ok(db) => Some(Arc::new(db)),
                Err(e) => {
                    return Err(format!(
                        "Failed to create integration test database pool: {}",
                        e
                    ));
                }
            };
            (None, pool)
        } else {
            let path = format!(".hot/test-{}.db", test_id);
            tracing::debug!("Creating temporary test database: {}", path);

            let test_conf = crate::val!({
                "db": {
                    "uri": format!("sqlite:{}", path),
                    "schema": "hot"
                }
            });

            if let Err(e) = crate::db::run_migrations(&test_conf).await {
                return Err(format!("Failed to run migrations on test database: {}", e));
            }

            let pool = match crate::db::create_db_pool(&test_conf).await {
                Ok(db) => Some(Arc::new(db)),
                Err(e) => {
                    return Err(format!("Failed to create test database pool: {}", e));
                }
            };
            (Some(path), pool)
        };

        // File storage: in integration mode, use the path from conf; otherwise ephemeral
        let (temp_file_storage_path, file_storage): (
            Option<String>,
            Option<Arc<dyn crate::file_storage::FileStorage>>,
        ) = if is_integration {
            let storage_path_str = conf
                .and_then(|c| {
                    let s = c.get_str("file.storage.path");
                    if s.is_empty() || s == "null" {
                        None
                    } else {
                        Some(s)
                    }
                })
                .unwrap_or_else(|| format!(".hot/test-integration-files-{}", test_id));
            tracing::debug!(
                "Integration mode: using file storage at {}",
                storage_path_str
            );
            let storage_path = std::path::PathBuf::from(&storage_path_str);
            let storage: Arc<dyn crate::file_storage::FileStorage> =
                Arc::new(crate::file_storage::LocalFileStorage::new(storage_path));
            // Don't clean up integration file storage — the caller handles it
            (None, Some(storage))
        } else {
            let path = format!(".hot/test-files-{}", test_id);
            tracing::debug!("Creating temporary test file storage: {}", path);
            let storage_path = std::path::PathBuf::from(&path);
            let storage: Arc<dyn crate::file_storage::FileStorage> =
                Arc::new(crate::file_storage::LocalFileStorage::new(storage_path));
            (Some(path), Some(storage))
        };

        // Create a test execution context (needed for file operations with org/env isolation)
        // Use insert_test_data which creates user, org, env, project, build, and run
        let test_execution_context = if database_pool.is_some() && file_storage.is_some() {
            if let Some(ref db) = database_pool {
                match crate::db::insert_test_data(db).await {
                    Ok(test_data) => {
                        tracing::debug!(
                            "Test data created: run_id={}, env_id={}, build_id={}",
                            test_data.run_id,
                            test_data.env_id,
                            test_data.build_id
                        );

                        Some(crate::lang::event::ExecutionContext::new(
                            test_data.run_id,
                            test_data.stream_id,
                            crate::db::run::RunType::Run.as_id(),
                            Some(test_data.env_id),
                            Some(test_data.user_id),
                            Some(test_data.org_id),
                            Some(test_data.build_id),
                        ))
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create test data: {}", e);
                        return Err(format!("Test setup failed: {}", e));
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Create a temporary store for tests so ::hot::store functions work.
        // Scope to the test execution context's org/env so the local SQLite backend
        // matches the multi-tenant semantics of the Postgres backend. The store
        // shares the test database pool — its tables live alongside everything
        // else in the main migrations.
        let store: Option<Arc<dyn crate::store::Store>> =
            match (&database_pool, &test_execution_context) {
                (Some(pool), Some(ctx)) => {
                    let oid = ctx.org_id.unwrap_or_else(uuid::Uuid::new_v4);
                    let eid = ctx.env_id.unwrap_or_else(uuid::Uuid::new_v4);
                    Some(Arc::new(crate::store::sqlite::SqliteStore::new(
                        pool.clone(),
                        oid,
                        eid,
                    )))
                }
                _ => {
                    tracing::debug!("Test store not available: no db pool or execution context");
                    None
                }
            };

        // VM execution is CPU-intensive and must run on a blocking thread.
        // The runtime is configured with a 64 MB thread stack (see hot_cli
        // main) because the Hot VM is deeply recursive.
        let src_paths_clone = src_paths.to_vec();
        let test_paths_clone = test_paths.to_vec();
        let conf_clone = conf.cloned();
        let project_name_clone = project_name.map(|s| s.to_string());
        let pattern_clone = pattern.map(|s| s.to_string());

        let pipeline_result = tokio::task::spawn_blocking(move || {
            Self::run_unified_pipeline(
                &src_paths_clone,
                &test_paths_clone,
                conf_clone.as_ref(),
                project_name_clone.as_deref(),
                None,
                None,
                PipelineMode::Test {
                    pattern: pattern_clone,
                    capture_output,
                },
                None,
                test_execution_context,
                None,            // No event publisher
                context_storage, // Context storage from ctx.hot
                database_pool,
                file_storage,
                store,
                None, // No embedding provider
                task_queue,
                stream_publisher,
                color,
                None, // No warnings out
            )
        })
        .await
        .map_err(|e| format!("Task failed: {}", e))?;

        let result = match pipeline_result {
            Ok(crate::val::Val::Bool(success)) => Ok(success),
            Ok(_) => Ok(false),
            Err(e) => Err(e),
        };

        // Clean up ephemeral test artifacts (only for non-integration mode)
        if let Some(ref path) = temp_db_path {
            tracing::debug!("Cleaning up temporary test database: {}", path);
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(format!("{}-shm", path));
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }

        if let Some(ref path) = temp_file_storage_path {
            tracing::debug!("Cleaning up temporary test file storage: {}", path);
            let _ = std::fs::remove_dir_all(path);
        }

        // The ::hot::store backend now lives in the same SQLite file as the rest
        // of the test database, so cleanup of `temp_db_path` above already covers
        // its rows. There is no longer a separate `.hot/test-store-*` directory.

        result
    }

    /// Clean up stale test artifacts from previous failed/crashed test runs.
    /// This removes any `.hot/test-*.db` files and `.hot/test-files-*` directories
    /// that may have been left behind if the test runner crashed or was killed.
    fn cleanup_stale_test_artifacts() {
        let hot_dir = std::path::Path::new(".hot");
        if !hot_dir.exists() {
            return;
        }

        let entries = match std::fs::read_dir(hot_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };

            // Clean up stale test database files (test-*.db, test-*.db-shm, test-*.db-wal)
            if file_name.starts_with("test-") && file_name.contains(".db") {
                tracing::debug!("Cleaning up stale test database: {}", path.display());
                let _ = std::fs::remove_file(&path);
            }

            // Clean up stale test file storage directories (test-files-*)
            if file_name.starts_with("test-files-") && path.is_dir() {
                tracing::debug!("Cleaning up stale test file storage: {}", path.display());
                let _ = std::fs::remove_dir_all(&path);
            }

            // Clean up stale test store directories (test-store-*)
            if file_name.starts_with("test-store-") && path.is_dir() {
                tracing::debug!("Cleaning up stale test store: {}", path.display());
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// Execute eval code using a cached BytecodeProgram with registries and AST (for worker cache hits)
    /// Uses cached AST for full metadata enrichment!
    #[allow(clippy::too_many_arguments)]
    pub fn eval_code_with_cached_bytecode(
        code: &str,
        cached: crate::lang::cache::bytecode_cache::CachedBytecode,
        conf: Option<&crate::val::Val>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
    ) -> Result<crate::val::Val, String> {
        tracing::debug!("Using cached bytecode with AST (skipping parsing and compilation)");

        // Clone registries and program for extension
        let function_mapping = cached.function_mapping.clone();
        let core_functions = cached.core_functions.clone();
        let type_implementations = cached.type_implementations.clone();
        let mut cached_program = cached.program.clone();
        let cached_instruction_count = cached_program.entry_point.len();

        // Compile ONLY the eval code with inherited registries
        tracing::debug!("Compiling eval code with cached context: {}", code);

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

        let eval_code_with_ns = format!("{} ns\n{}", eval_ns, code);
        let mut eval_ast = crate::lang::parser::parse_hot(&eval_code_with_ns)
            .map_err(|e| format!("Failed to parse eval code: {}", e))?;

        // Compile just the eval code with the cached registries pre-loaded
        // This ensures the eval code can reference project functions and core functions
        let mut eval_compiler = crate::lang::compiler::Compiler::new();

        // Pre-populate the compiler with cached registries so eval code can reference them
        for (name, id) in &function_mapping {
            eval_compiler.register_existing_function(name.clone(), *id);
        }
        for (name, id) in &core_functions {
            eval_compiler.register_existing_core_function(name.clone(), *id);
        }
        for ((type_name, method_name), impl_name) in &type_implementations {
            eval_compiler.register_existing_type_implementation(
                type_name.clone(),
                method_name.clone(),
                impl_name.clone(),
            );
        }

        eval_compiler
            .compile_program(&mut eval_ast)
            .map_err(|e| format!("Failed to compile eval code: {}", e.format_error(false)))?;

        // Merge eval program into cached program
        let mut eval_program = eval_compiler.get_program().clone();

        // Merge constants and remap IDs in eval instructions
        let id_mapping = cached_program.merge_constants(eval_program.constants.clone());
        crate::lang::bytecode::BytecodeProgram::remap_constant_ids(
            &mut eval_program.entry_point,
            &id_mapping,
        );

        // Append eval instructions to cached program
        let _eval_start = cached_program.append_instructions(eval_program.entry_point);
        let total_instructions = cached_program.entry_point.len();

        tracing::debug!(
            "✓ Extended bytecode: {} cached + {} eval = {} total instructions",
            cached_instruction_count,
            total_instructions - cached_instruction_count,
            total_instructions
        );

        // Use the pre-built HotAst from cache (no expensive indexing needed!)
        // Just wrap in Arc for VM
        let hot_ast = Arc::new(cached.hot_ast.clone());

        // Extract core variables from the cached AST (critical for resolving types like Null, Vec, etc.)
        let core_variables = Arc::new(
            crate::lang::compiler::core_registry::extract_core_variables_from_ast(
                &cached.ast_program,
            ),
        );

        tracing::debug!(
            "✓ Using pre-built HotAst from cache ({} namespaces, {} core vars) - skipped expensive indexing!",
            cached.ast_program.namespaces.len(),
            core_variables.len()
        );

        // Create VM with the extended program and full AST
        let extended_program_arc = Arc::new(cached_program);
        let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
            extended_program_arc,
            Some(hot_ast), // Full AST from cache - rich metadata enrichment!
            Arc::new(function_mapping),
            Arc::new(core_functions),
            Arc::new(type_implementations),
            core_variables,
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
        if let Some(context_storage) = context_storage {
            vm.context_storage = context_storage;
        }

        // Extract ctx requirements via call graph and apply defaults + secret keys
        let ctx_requirements =
            crate::lang::compiler::ctx_checker::extract_ctx_requirements_via_call_graph(
                &cached.ast_program,
            );
        for (key, default_val) in ctx_requirements.all_defaults() {
            vm.context_storage.entry(key).or_insert(default_val);
        }
        vm.secret_keys = ctx_requirements.all_secret_keys();
        vm.sync_secret_keys_to_execution_context();

        // Set database pool if provided
        if let Some(db_pool) = database_pool {
            vm.set_database_pool(db_pool);
        }
        // Set stream publisher for real-time SSE updates
        if let Some(publisher) = stream_publisher {
            vm.set_stream_publisher(publisher);
        }

        // Install tool/skill spec registries from cached bytecode
        // before module-init runs. See `call_function_with_cached_bytecode`
        // for the full rationale.
        crate::lang::hot::internal_mcp::set_registry(cached.tool_specs.clone());
        crate::lang::hot::internal_skill::set_registry(cached.skill_specs.clone());

        // Execute the complete extended program (cached + eval)
        tracing::debug!(
            "Executing extended bytecode program ({} total instructions)",
            total_instructions
        );
        let result = vm
            .execute()
            .map_err(|e| format!("Failed to execute extended bytecode: {}", e))?;

        tracing::debug!("Cached bytecode + eval execution completed successfully");
        Ok(result)
    }

    /// Call a function directly with Val arguments using cached bytecode (no code parsing!)
    /// This is much faster than eval_code_with_cached_bytecode when you have Val arguments
    /// because it skips parsing and compilation entirely.
    /// Takes Arc<CachedBytecode> to avoid expensive clones on repeated calls.
    #[allow(clippy::too_many_arguments)]
    pub fn call_function_with_cached_bytecode(
        function_name: &str,
        args: &[crate::val::Val],
        cached: Arc<crate::lang::cache::bytecode_cache::CachedBytecode>,
        conf: Option<&crate::val::Val>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
        task_queue: Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>>,
        file_storage: Option<Arc<dyn crate::file_storage::FileStorage>>,
        store: Option<Arc<dyn crate::store::Store>>,
        embedding_provider: Option<Arc<dyn crate::store::embedding::EmbeddingProvider>>,
        external_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<crate::val::Val, String> {
        let timing_id = execution_context
            .as_ref()
            .map(|ctx| ctx.run_id.as_simple().to_string())
            .unwrap_or_else(|| function_name.to_string());
        let total_started = std::time::Instant::now();
        tracing::debug!(
            "Calling function '{}' directly with {} args (skipping parsing entirely!)",
            function_name,
            args.len()
        );

        let cached_artifacts_started = std::time::Instant::now();
        // Use runtime-ready shared artifacts directly (no extension needed).
        let function_mapping = Arc::clone(&cached.runtime_function_mapping);
        let core_functions = Arc::clone(&cached.runtime_core_functions);
        let type_implementations = Arc::clone(&cached.runtime_type_implementations);
        let program = Arc::clone(&cached.runtime_program);
        let instruction_count = program.entry_point.len();
        let hot_ast = Arc::clone(&cached.runtime_hot_ast);
        let core_variables = Arc::clone(&cached.runtime_core_variables);
        tracing::debug!(
            "TIMING [{}]: vm_cached_artifacts_setup: {:?}",
            timing_id,
            cached_artifacts_started.elapsed()
        );

        tracing::debug!(
            "✓ Using cached bytecode directly ({} instructions, {} namespaces, {} core vars)",
            instruction_count,
            cached.ast_program.namespaces.len(),
            core_variables.len()
        );

        // Create VM with the cached program
        let vm_create_started = std::time::Instant::now();
        let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
            program,
            Some(hot_ast),
            function_mapping,
            core_functions,
            type_implementations,
            core_variables,
            conf.cloned(),
        );
        if let Some(token) = external_cancel {
            vm.set_external_cancel(token);
        }
        tracing::debug!(
            "TIMING [{}]: vm_create: {:?}",
            timing_id,
            vm_create_started.elapsed()
        );

        let vm_services_started = std::time::Instant::now();
        // Set context storage, database, event publisher, and stream publisher BEFORE initialization
        // (these don't trigger run recording)
        if let Some(context_storage) = context_storage {
            vm.context_storage = context_storage;
        }

        // Extract ctx requirements via call graph and apply defaults + secret keys
        for (key, default_val) in cached.runtime_ctx_defaults.iter() {
            vm.context_storage
                .entry(key.clone())
                .or_insert_with(|| default_val.clone());
        }
        vm.secret_keys = cached.runtime_secret_keys.as_ref().clone();
        vm.sync_secret_keys_to_execution_context();

        if let Some(db_pool) = database_pool {
            vm.set_database_pool(db_pool);
        }
        if let Some(storage) = file_storage {
            vm.set_file_storage(storage);
        }
        if let Some(event_publisher) = &event_publisher {
            vm.set_event_publisher(Arc::clone(event_publisher));
        }
        if let Some(publisher) = &stream_publisher {
            vm.set_stream_publisher(Arc::clone(publisher));
        }
        if let Some(tq) = task_queue {
            vm.set_task_queue(tq);
        }
        if let Some(s) = store {
            vm.set_store(s);
        }
        if let Some(ep) = embedding_provider {
            vm.set_embedding_provider(ep);
        }
        tracing::debug!(
            "TIMING [{}]: vm_services_setup: {:?}",
            timing_id,
            vm_services_started.elapsed()
        );

        // Install tool/skill spec registries from the cached bytecode
        // BEFORE module-init runs. Module-level statements like
        // `agent-tools [::tool/from-fn(::ns/fn, ...)]` call
        // `::hot::internal::mcp/schema-from-fn` which reads the
        // process-global registry; without this hydration, cache
        // hits in fresh worker processes (and zip-build deployments)
        // would error out with
        // `no schema registered for '...'`.
        let registry_started = std::time::Instant::now();
        crate::lang::hot::internal_mcp::set_registry(cached.tool_specs.clone());
        crate::lang::hot::internal_skill::set_registry(cached.skill_specs.clone());
        tracing::debug!(
            "TIMING [{}]: vm_registry_install: {:?}",
            timing_id,
            registry_started.elapsed()
        );

        // Initialize global state WITHOUT emitter (don't record initialization as a run)
        if instruction_count > 0 {
            tracing::debug!(
                "Initializing program state ({} instructions) without run recording",
                instruction_count
            );
            let module_init_started = std::time::Instant::now();
            vm.execute()
                .map_err(|e| format!("Failed to initialize program: {}", e))?;
            tracing::debug!(
                "TIMING [{}]: vm_module_init_execute: {:?}",
                timing_id,
                module_init_started.elapsed()
            );
        }

        // NOW set up emitter and execution context for the function call
        // Keep local references for manual run event emission
        let emitter_for_run = emitter.clone();
        let execution_context_for_run = execution_context.clone();

        let run_context_started = std::time::Instant::now();
        if let Some(ref em) = emitter {
            vm.set_emitter(Arc::clone(em));
        }
        if let Some(ref ctx) = execution_context {
            vm.set_execution_context(ctx.clone());
        }

        // Sync secret keys to execution context NOW that it's been set
        // (the earlier sync_secret_keys_to_execution_context call happened before ctx was set)
        vm.sync_secret_keys_to_execution_context();
        tracing::debug!(
            "TIMING [{}]: vm_run_context_setup: {:?}",
            timing_id,
            run_context_started.elapsed()
        );

        // Call the function directly - this is the actual "run" we want to record
        tracing::debug!(
            "Calling function '{}' with Val arguments directly",
            function_name
        );

        // Emit run:start manually since call_function_bypassing_unified_lookup doesn't do it
        if let (Some(emitter), Some(execution_context)) =
            (&emitter_for_run, &execution_context_for_run)
        {
            tracing::debug!(
                "Engine: Emitting run:start for direct function call run_id={}",
                execution_context.run_id
            );
            let run_start_emit_started = std::time::Instant::now();
            let start_event = crate::lang::emitter::EngineEvent::run_start(execution_context);
            emitter.emit(start_event);
            tracing::debug!(
                "TIMING [{}]: vm_run_start_emit: {:?}",
                timing_id,
                run_start_emit_started.elapsed()
            );
        }

        let function_call_started = std::time::Instant::now();
        let result = vm.call_function_bypassing_unified_lookup(function_name, args);
        tracing::debug!(
            "TIMING [{}]: vm_function_call: {:?}",
            timing_id,
            function_call_started.elapsed()
        );

        // Emit run:stop, run:fail, or run:cancel manually based on result and VM state
        let terminal_emit_started = std::time::Instant::now();
        match &result {
            Ok(result_val) => {
                if vm.has_failed() {
                    if let (Some(failure_state), Some(emitter), Some(execution_context)) = (
                        vm.get_failure(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let event = crate::lang::emitter::EngineEvent::run_fail(
                            execution_context,
                            failure_state.data,
                        );
                        emitter.emit(event);
                    }
                } else if vm.has_cancelled() {
                    if let (Some(cancellation_state), Some(emitter), Some(execution_context)) = (
                        vm.get_cancellation(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            cancellation_state.data,
                        );
                        emitter.emit(event);
                    }
                } else if result_val.is_err() {
                    // Result is a Result.Err type - emit run:fail event
                    if let (Some(emitter), Some(execution_context)) =
                        (&emitter_for_run, &execution_context_for_run)
                    {
                        // Extract the error value from Result.Err
                        let failure = if let Some(err_val) = result_val.unwrap_err() {
                            // Check if err_val already has terminal Failure structure
                            if let crate::val::Val::Map(m) = err_val {
                                if m.contains_key(&crate::val::Val::from("msg"))
                                    || m.contains_key(&crate::val::Val::from("err"))
                                {
                                    err_val.clone()
                                } else {
                                    crate::val!({
                                        "msg": format!("{}", err_val),
                                        "err": err_val.clone()
                                    })
                                }
                            } else {
                                crate::val!({
                                    "msg": format!("{}", err_val),
                                    "err": err_val.clone()
                                })
                            }
                        } else {
                            crate::val!({
                                "msg": "Unknown error",
                                "err": result_val.clone()
                            })
                        };
                        let event =
                            crate::lang::emitter::EngineEvent::run_fail(execution_context, failure);
                        emitter.emit(event);
                        tracing::debug!(
                            "Engine: Emitting run:fail for Result.Err in direct function call run_id={}",
                            execution_context.run_id
                        );
                    }
                } else if result_val.is_cancelled() {
                    // Result is a Cancellation type - emit run:cancel event
                    if let (Some(emitter), Some(execution_context)) =
                        (&emitter_for_run, &execution_context_for_run)
                    {
                        let event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            result_val.clone(),
                        );
                        emitter.emit(event);
                        tracing::debug!(
                            "Engine: Emitting run:cancel for Cancellation in direct function call run_id={}",
                            execution_context.run_id
                        );
                    }
                } else {
                    // Success: emit run:stop event
                    if let (Some(emitter), Some(execution_context)) =
                        (&emitter_for_run, &execution_context_for_run)
                    {
                        // Unwrap ::hot::type/Result.Ok if present
                        let unwrapped_result = if result_val.is_ok() {
                            result_val
                                .unwrap_ok()
                                .cloned()
                                .unwrap_or_else(|| result_val.clone())
                        } else {
                            result_val.clone()
                        };
                        let stop_event = crate::lang::emitter::EngineEvent::run_stop(
                            execution_context,
                            unwrapped_result,
                        );
                        emitter.emit(stop_event);
                        tracing::debug!(
                            "Engine: Emitting run:stop for direct function call run_id={}",
                            execution_context.run_id
                        );
                    }
                }
            }
            Err(e) => {
                if vm.has_failed() {
                    if let (Some(failure_state), Some(emitter), Some(execution_context)) = (
                        vm.get_failure(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let fail_event = crate::lang::emitter::EngineEvent::run_fail(
                            execution_context,
                            failure_state.data,
                        );
                        emitter.emit(fail_event);
                    }
                } else if vm.has_cancelled() {
                    if let (Some(cancellation_state), Some(emitter), Some(execution_context)) = (
                        vm.get_cancellation(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let cancel_event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            cancellation_state.data,
                        );
                        emitter.emit(cancel_event);
                    }
                } else if let (Some(emitter), Some(execution_context)) =
                    (&emitter_for_run, &execution_context_for_run)
                {
                    let fail_event = crate::lang::emitter::EngineEvent::run_fail(
                        execution_context,
                        crate::val::Val::from(e.to_string()),
                    );
                    emitter.emit(fail_event);
                    tracing::debug!(
                        "Engine: Emitting run:fail for direct function call run_id={}",
                        execution_context.run_id
                    );
                }
            }
        }
        tracing::debug!(
            "TIMING [{}]: vm_terminal_emit: {:?}",
            timing_id,
            terminal_emit_started.elapsed()
        );

        let final_result_started = std::time::Instant::now();
        let final_result =
            result.map_err(|e| format!("Failed to call function '{}': {}", function_name, e))?;
        tracing::debug!(
            "TIMING [{}]: vm_final_result: {:?}",
            timing_id,
            final_result_started.elapsed()
        );

        tracing::debug!("Direct function call completed successfully (no parsing overhead!)");
        tracing::debug!(
            "TIMING [{}]: call_function_with_cached_bytecode_internal_total: {:?}",
            timing_id,
            total_started.elapsed()
        );
        Ok(final_result)
    }

    /// Like `call_function_with_cached_bytecode`, but also wires a task receive
    /// channel onto the VM for `::hot::task/receive()` support.
    #[allow(clippy::too_many_arguments)]
    pub fn call_function_with_cached_bytecode_and_task(
        function_name: &str,
        args: &[crate::val::Val],
        cached: Arc<crate::lang::cache::bytecode_cache::CachedBytecode>,
        conf: Option<&crate::val::Val>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
        task_receiver: Option<
            Arc<parking_lot::Mutex<tokio::sync::mpsc::Receiver<crate::val::Val>>>,
        >,
        task_queue: Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>>,
        file_storage: Option<Arc<dyn crate::file_storage::FileStorage>>,
        store: Option<Arc<dyn crate::store::Store>>,
        embedding_provider: Option<Arc<dyn crate::store::embedding::EmbeddingProvider>>,
        external_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
        task_id: Option<uuid::Uuid>,
    ) -> Result<crate::val::Val, String> {
        // Build VM identically to the base method
        let function_mapping = Arc::clone(&cached.runtime_function_mapping);
        let core_functions = Arc::clone(&cached.runtime_core_functions);
        let type_implementations = Arc::clone(&cached.runtime_type_implementations);
        let program = Arc::clone(&cached.runtime_program);
        let instruction_count = program.entry_point.len();
        let hot_ast = Arc::clone(&cached.runtime_hot_ast);
        let core_variables = Arc::clone(&cached.runtime_core_variables);

        let mut vm = crate::lang::runtime::vm::VirtualMachine::new(
            program,
            Some(hot_ast),
            function_mapping,
            core_functions,
            type_implementations,
            core_variables,
            conf.cloned(),
        );

        if let Some(context_storage) = context_storage {
            vm.context_storage = context_storage;
        }

        for (key, default_val) in cached.runtime_ctx_defaults.iter() {
            vm.context_storage
                .entry(key.clone())
                .or_insert_with(|| default_val.clone());
        }
        vm.secret_keys = cached.runtime_secret_keys.as_ref().clone();
        vm.sync_secret_keys_to_execution_context();

        if let Some(db_pool) = database_pool {
            vm.set_database_pool(db_pool);
        }
        if let Some(storage) = file_storage {
            vm.set_file_storage(storage);
        }
        if let Some(event_publisher) = &event_publisher {
            vm.set_event_publisher(Arc::clone(event_publisher));
        }
        if let Some(publisher) = &stream_publisher {
            vm.set_stream_publisher(Arc::clone(publisher));
        }

        // Wire up task receive channel
        if let Some(rx) = task_receiver {
            vm.set_task_receiver(rx);
        }
        if let Some(tq) = task_queue {
            vm.set_task_queue(tq);
        }
        if let Some(s) = store {
            vm.set_store(s);
        }
        if let Some(ep) = embedding_provider {
            vm.set_embedding_provider(ep);
        }
        if let Some(token) = external_cancel {
            vm.set_external_cancel(token);
        }
        if let Some(tid) = task_id {
            vm.set_task_id(tid);
        }

        // Install tool/skill spec registries from cached bytecode
        // before module-init runs. Same rationale as
        // `call_function_with_cached_bytecode`.
        crate::lang::hot::internal_mcp::set_registry(cached.tool_specs.clone());
        crate::lang::hot::internal_skill::set_registry(cached.skill_specs.clone());

        // Initialize global state WITHOUT emitter
        if instruction_count > 0 {
            vm.execute()
                .map_err(|e| format!("Failed to initialize program: {}", e))?;
        }

        // Set emitter and execution context for the function call
        let emitter_for_run = emitter.clone();
        let execution_context_for_run = execution_context.clone();

        if let Some(ref em) = emitter {
            vm.set_emitter(Arc::clone(em));
        }
        if let Some(ref ctx) = execution_context {
            vm.set_execution_context(ctx.clone());
        }
        vm.sync_secret_keys_to_execution_context();

        // Emit run:start
        if let (Some(emitter), Some(execution_context)) =
            (&emitter_for_run, &execution_context_for_run)
        {
            let start_event = crate::lang::emitter::EngineEvent::run_start(execution_context);
            emitter.emit(start_event);
        }

        let result = vm.call_function_bypassing_unified_lookup(function_name, args);

        // Emit run:stop/fail/cancel
        match &result {
            Ok(result_val) => {
                if vm.has_failed() {
                    if let (Some(failure_state), Some(emitter), Some(execution_context)) = (
                        vm.get_failure(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let fail_event = crate::lang::emitter::EngineEvent::run_fail(
                            execution_context,
                            failure_state.data,
                        );
                        emitter.emit(fail_event);
                    }
                } else if vm.has_cancelled() {
                    if let (Some(cancellation_state), Some(emitter), Some(execution_context)) = (
                        vm.get_cancellation(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let cancel_event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            cancellation_state.data,
                        );
                        emitter.emit(cancel_event);
                    }
                } else if result_val.is_err() {
                    if let (Some(emitter), Some(execution_context)) =
                        (&emitter_for_run, &execution_context_for_run)
                    {
                        let failure = if let Some(err_val) = result_val.unwrap_err() {
                            crate::val!({
                                "msg": format!("{}", err_val),
                                "err": err_val.clone()
                            })
                        } else {
                            crate::val!({"msg": "Unknown error"})
                        };
                        let fail_event =
                            crate::lang::emitter::EngineEvent::run_fail(execution_context, failure);
                        emitter.emit(fail_event);
                    }
                } else if let (Some(emitter), Some(execution_context)) =
                    (&emitter_for_run, &execution_context_for_run)
                {
                    let unwrapped_result = if result_val.is_ok() {
                        result_val
                            .unwrap_ok()
                            .cloned()
                            .unwrap_or_else(|| result_val.clone())
                    } else {
                        result_val.clone()
                    };
                    let stop_event = crate::lang::emitter::EngineEvent::run_stop(
                        execution_context,
                        unwrapped_result,
                    );
                    emitter.emit(stop_event);
                }
            }
            Err(e) => {
                if vm.has_failed() {
                    if let (Some(failure_state), Some(emitter), Some(execution_context)) = (
                        vm.get_failure(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let fail_event = crate::lang::emitter::EngineEvent::run_fail(
                            execution_context,
                            failure_state.data,
                        );
                        emitter.emit(fail_event);
                    }
                } else if vm.has_cancelled() {
                    if let (Some(cancellation_state), Some(emitter), Some(execution_context)) = (
                        vm.get_cancellation(),
                        &emitter_for_run,
                        &execution_context_for_run,
                    ) {
                        let cancel_event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            cancellation_state.data,
                        );
                        emitter.emit(cancel_event);
                    }
                } else if let (Some(emitter), Some(execution_context)) =
                    (&emitter_for_run, &execution_context_for_run)
                {
                    let fail_event = crate::lang::emitter::EngineEvent::run_fail(
                        execution_context,
                        crate::val::Val::from(e.to_string()),
                    );
                    emitter.emit(fail_event);
                }
            }
        }

        result.map_err(|e| format!("Failed to call function '{}': {}", function_name, e))
    }

    /// Compile project sources and return artifacts without executing any code
    /// This is useful for the worker cache miss path - compile once, then call functions directly
    #[allow(clippy::too_many_arguments)]
    pub fn compile_project_for_cache(
        src_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
        context_storage: Option<AHashMap<String, crate::val::Val>>,
        database_pool: Option<Arc<crate::db::DatabasePool>>,
        stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
    ) -> Result<(crate::val::Val, CompilationArtifacts), String> {
        tracing::debug!(
            "Compiling project for cache (no eval code): {:?}",
            project_name
        );

        // Use the unified pipeline with NO eval code - just compile project sources
        let result = Self::run_unified_pipeline_with_artifacts(
            src_paths,
            &[], // No test paths for event handlers
            conf,
            project_name,
            None, // No target file
            None, // NO eval code - this is the key difference!
            emitter,
            execution_context,
            event_publisher,
            context_storage,
            database_pool,
            None, // No file storage
            stream_publisher,
            None, // No store
            None, // No embedding provider
            false,
        )?;

        match result.artifacts {
            Some(artifacts) => {
                tracing::debug!(
                    "✓ Project compiled with {} functions",
                    artifacts.function_mapping.len()
                );
                Ok((result.result, artifacts))
            }
            None => Err("Failed to get compilation artifacts".to_string()),
        }
    }

    /// Compile source files and save bytecode to a specific cache location.
    /// This is used to pre-compile bundles after extraction so routing can find functions immediately.
    ///
    /// Parameters:
    /// - src_paths: Source paths to compile
    /// - cache: The BytecodeCache to save to
    /// - project_name: Project name for cache key calculation (should match bundle manifest)
    /// - manifest_cache_key: Optional cache key from manifest (if provided, use this instead of calculating)
    /// - manifest_file_hashes: Optional file hashes from manifest (if provided, use these for metadata)
    /// - conf: Optional project config for dependency resolution (needed for live builds)
    pub fn compile_to_cache(
        src_paths: &[String],
        cache: &crate::lang::cache::bytecode_cache::BytecodeCache,
        project_name: &str,
        manifest_cache_key: Option<&str>,
        manifest_file_hashes: Option<Vec<crate::lang::cache::bytecode_cache::FileHash>>,
        conf: Option<&crate::val::Val>,
    ) -> Result<(), String> {
        tracing::debug!(
            "Pre-compiling {} source paths to cache for project '{}'",
            src_paths.len(),
            project_name
        );

        // For live builds, pass conf and project_name so dependencies can be resolved.
        // For bundle builds, pass None (deps are pre-bundled in src_paths).
        let compile_project_name: Option<&str> = if conf.is_some() {
            Some(project_name)
        } else {
            None
        };

        // Compile the project
        let (_, artifacts) = Self::compile_project_for_cache(
            src_paths,
            conf,
            compile_project_name,
            None, // No emitter
            None, // No execution context
            None, // No event publisher
            None, // No context storage
            None, // No database
            None, // No stream publisher
        )?;

        // Use manifest cache key if provided, otherwise calculate
        let (cache_key, file_hashes) = if let Some(key) = manifest_cache_key {
            let hashes = manifest_file_hashes.unwrap_or_default();
            (key.to_string(), hashes)
        } else {
            // Calculate cache key from source files
            let mut all_source_files = Vec::new();
            for src_path in src_paths {
                if let Ok(files) = Self::discover_hot_files(src_path) {
                    all_source_files.extend(files);
                }
            }

            let hashes =
                crate::lang::cache::bytecode_cache::BytecodeCache::hash_files(&all_source_files)
                    .map_err(|e| format!("Failed to hash files: {}", e))?;

            let key = crate::lang::cache::bytecode_cache::BytecodeCache::calculate_cache_key(
                project_name,
                &hashes,
            )?;
            (key, hashes)
        };

        // Create metadata
        let metadata = crate::lang::cache::bytecode_cache::CacheMetadata {
            project_name: project_name.to_string(),
            hot_version: crate::build_info::VERSION.to_string(),
            git_sha: crate::build_info::GIT_SHA.to_string(),
            cache_format_version: crate::hasher::CacheType::Bytecode.format_version(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            file_hashes,
            cache_key: cache_key.clone(),
        };

        // Build tool/skill spec registries from the AST so they can be
        // baked into the cache file. This is what lets cross-process
        // cache hits and zip-build deployments serve
        // `::hot::internal::mcp/schema-from-fn` and
        // `::hot::internal::skill/meta-from-fn` without recompiling.
        let spec_compiler = crate::lang::compiler::Compiler::new();
        let tool_specs = spec_compiler.build_tool_specs(&artifacts.ast_program);
        let skill_specs = spec_compiler.build_skill_specs(&artifacts.ast_program);

        // Save to cache
        cache.save(
            &cache_key,
            &artifacts.program,
            metadata,
            &artifacts.function_mapping,
            &artifacts.core_functions,
            &artifacts.type_implementations,
            &artifacts.ast_program,
            &artifacts.hot_ast,
            &tool_specs,
            &skill_specs,
        )?;

        tracing::debug!(
            "Pre-compiled bytecode saved to cache with key {}",
            &cache_key[..12.min(cache_key.len())]
        );
        Ok(())
    }

    /// Convert CompilationArtifacts to CachedBytecode for use with call_function_with_cached_bytecode
    /// This creates a minimal CacheMetadata since we're not actually caching, just executing
    /// Returns Arc<CachedBytecode> to match the expected parameter type
    pub fn artifacts_to_cached_bytecode(
        artifacts: CompilationArtifacts,
    ) -> Arc<crate::lang::cache::bytecode_cache::CachedBytecode> {
        // Create minimal metadata for immediate execution (not for caching)
        let metadata = crate::lang::cache::bytecode_cache::CacheMetadata {
            project_name: String::new(),
            hot_version: crate::build_info::VERSION.to_string(),
            git_sha: crate::build_info::GIT_SHA.to_string(),
            cache_format_version: crate::hasher::CacheType::Bytecode.format_version(),
            created_at: 0,
            file_hashes: vec![],
            cache_key: String::new(),
        };

        // Same rationale as `compile_to_cache`: bake the
        // tool/skill spec registries onto the in-memory CachedBytecode
        // so that downstream `call_function_with_cached_bytecode` calls
        // can install them into the global registries before
        // `vm.execute()` runs module-level statements like
        // `agent-tools [::tool/from-fn(::ns/fn, ...)]`.
        let spec_compiler = crate::lang::compiler::Compiler::new();
        let tool_specs = spec_compiler.build_tool_specs(&artifacts.ast_program);
        let skill_specs = spec_compiler.build_skill_specs(&artifacts.ast_program);

        Arc::new(crate::lang::cache::bytecode_cache::CachedBytecode::new(
            metadata,
            crate::lang::cache::bytecode_cache::CachedBytecodeArtifacts {
                program: artifacts.program,
                function_mapping: artifacts.function_mapping,
                core_functions: artifacts.core_functions,
                type_implementations: artifacts.type_implementations,
                ast_program: artifacts.ast_program,
                hot_ast: artifacts.hot_ast,
                tool_specs,
                skill_specs,
            },
        ))
    }
}
