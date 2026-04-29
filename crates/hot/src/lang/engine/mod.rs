// High-performance execution engine for Hot.
//
// The engine sits *above* `compiler` and `runtime`: it discovers source
// files, drives parse/compile/execute pipelines, manages the bytecode
// cache, and exposes the high-level user-facing API (`eval_code`,
// `run_file`, `run_tests`, …).
//
// Submodules:
//   * `discover`     — file/dir discovery + parse cache
//   * `pipeline`     — run/eval/check pipeline implementations + handler
//                      extraction + per-project resolution helpers
//   * `cache`        — bytecode cache prewarm + cached-execution paths
//   * `incremental`  — REPL state-retained execution + var extraction

pub mod cache;
pub mod discover;
pub mod incremental;
pub mod pipeline;

use crate::lang::bytecode::BytecodeProgram;
use crate::lang::compiler::Compiler;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use ahash::AHashMap;
use indexmap::IndexMap;
use std::sync::Arc;

/// Extracted metadata from a compiled project: handlers, schedules, tools, webhooks, agents, workflows, send targets, context requirements, and box requirements.
pub struct ExtractedHandlers {
    pub event_handlers: crate::lang::compiler::EventHandlers,
    pub scheduled_functions: crate::lang::compiler::ScheduledFunctions,
    pub mcp_tools: crate::lang::compiler::McpTools,
    pub webhooks: crate::lang::compiler::Webhooks,
    pub agents: crate::lang::compiler::AgentDefs,
    pub workflows: crate::lang::compiler::WorkflowDefs,
    pub send_targets: crate::lang::compiler::SendTargets,
    pub ctx_requirements: crate::lang::compiler::ctx_checker::ProgramCtxRequirements,
    pub box_requirements: crate::lang::compiler::box_checker::ProgramBoxRequirements,
}

/// Pipeline execution mode
#[derive(Debug, Clone)]
pub enum PipelineMode {
    /// Check only - compile and validate without execution
    Check,
    /// Execute and return final result
    Execute,
    /// Run tests and return success/failure
    Test {
        pattern: Option<String>,
        capture_output: bool,
    },
}

/// Test execution result
#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub error: Option<String>,
}

/// Result from incremental execution (for REPL)
pub struct IncrementalExecutionResult {
    pub result: crate::val::Val,
    pub new_state: indexmap::IndexMap<String, indexmap::IndexMap<String, crate::val::Val>>,
    pub total_instruction_count: usize,
    pub current_namespace: String,
}

/// Compilation artifacts that can be cached for reuse
/// This eliminates the need for double-compilation on first run
pub struct CompilationArtifacts {
    /// Compiled bytecode program
    pub program: BytecodeProgram,
    /// Function name to ID mapping
    pub function_mapping: IndexMap<String, u32>,
    /// Core functions registry
    pub core_functions: IndexMap<String, u32>,
    /// Type implementations mapping
    pub type_implementations: IndexMap<(String, String), String>,
    /// Parsed AST program (for metadata enrichment)
    pub ast_program: crate::lang::ast::Program,
    /// Pre-built HotAst with variable index
    pub hot_ast: crate::lang::ast::HotAst,
}

/// Result from pipeline execution with cacheable artifacts
pub struct ExecutionWithArtifacts {
    /// Execution result value
    pub result: Val,
    /// Compilation artifacts for caching (None if loaded from cache)
    pub artifacts: Option<CompilationArtifacts>,
}

/// High-performance engine for Hot programs.
pub struct Engine {
    /// Bytecode compiler
    compiler: Compiler,
    /// Optional emitter for variable tracking
    emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
    /// Optional execution context for emitter events
    execution_context: Option<crate::lang::event::ExecutionContext>,
    /// Optional event publisher for user-defined events
    event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
}

/// An executed engine that retains VM state for variable extraction
pub struct ExecutedEngine {
    /// The virtual machine with executed state
    vm: VirtualMachine,
    /// The compiled program (for reference)
    program: Arc<BytecodeProgram>,
}

impl ExecutedEngine {
    /// Extract a variable from the executed VM state
    pub fn extract_var(&self, namespace_path: Option<&str>, var_name: &str) -> Option<Val> {
        self.vm.get_var(namespace_path, var_name)
    }

    /// Extract all variables from a specific namespace
    pub fn extract_namespace_vars(
        &self,
        namespace_path: Option<&str>,
    ) -> Option<&indexmap::IndexMap<String, Val>> {
        self.vm.get_namespace_vars(namespace_path)
    }

    /// Extract all variables from all namespaces (for debugging)
    pub fn extract_all_vars(&self) -> &indexmap::IndexMap<String, indexmap::IndexMap<String, Val>> {
        self.vm.get_all_namespace_vars()
    }

    /// Get the current namespace of the VM
    pub fn get_current_namespace(&self) -> &str {
        self.vm.get_current_namespace()
    }

    /// Get a list of all namespace names
    pub fn get_namespace_names(&self) -> Vec<String> {
        self.vm.get_all_namespace_vars().keys().cloned().collect()
    }

    /// Get the compiled program (for reference)
    pub fn get_program(&self) -> &Arc<BytecodeProgram> {
        &self.program
    }

    /// Extract context storage (populated by ::hot::ctx/set calls)
    pub fn extract_context_storage(&self) -> AHashMap<String, Val> {
        self.vm.context_storage.clone()
    }
}

impl Engine {
    /// Create a new engine.
    pub fn new() -> Self {
        Self {
            compiler: Compiler::new(),
            emitter: None,
            execution_context: None,
            event_publisher: None,
        }
    }

    /// Create a new engine with an emitter.
    pub fn new_with_emitter(
        emitter: Arc<dyn crate::lang::emitter::EngineEventEmitter>,
        execution_context: crate::lang::event::ExecutionContext,
    ) -> Self {
        Self {
            compiler: Compiler::new(),
            emitter: Some(emitter),
            execution_context: Some(execution_context),
            event_publisher: None,
        }
    }

    /// Create a new engine with an emitter and event publisher.
    pub fn new_with_emitter_and_event_publisher(
        emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
        execution_context: Option<crate::lang::event::ExecutionContext>,
        event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
    ) -> Self {
        Self {
            compiler: Compiler::new(),
            emitter,
            execution_context,
            event_publisher,
        }
    }

    /// Get the event publisher for the engine
    pub fn get_event_publisher(&self) -> &Option<Arc<dyn crate::lang::event::EventPublisher>> {
        &self.event_publisher
    }

    /// Get the execution context for the engine
    pub fn get_execution_context(&self) -> &Option<crate::lang::event::ExecutionContext> {
        &self.execution_context
    }

    /// Set the emitter for variable tracking
    pub fn set_emitter(&mut self, emitter: Arc<dyn crate::lang::emitter::EngineEventEmitter>) {
        self.emitter = Some(emitter);
    }

    /// Set the execution context for emitter events
    pub fn set_execution_context(
        &mut self,
        execution_context: crate::lang::event::ExecutionContext,
    ) {
        self.execution_context = Some(execution_context);
    }

    /// Set the event publisher for user-defined events
    pub fn set_event_publisher(
        &mut self,
        event_publisher: Arc<dyn crate::lang::event::EventPublisher>,
    ) {
        self.event_publisher = Some(event_publisher);
    }

    /// Evaluate Hot code using this engine's configured emitter and event publisher
    pub fn eval_code(
        &self,
        code: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<crate::val::Val, String> {
        Self::eval_code_pipeline_with_deps_emitter_and_event_publisher(
            code,
            src_paths,
            test_paths,
            conf,
            project_name,
            self.emitter.clone(),
            self.execution_context.clone(),
            self.event_publisher.clone(),
        )
    }

    /// Simple evaluation for validation purposes (no dependencies loaded)
    /// This is used to validate Hot code strings (e.g., for context variable values)
    pub fn eval_simple(code: &str) -> Result<crate::val::Val, String> {
        // Use eval with no dependencies - just parse and execute the code
        Self::eval_code_pipeline_with_deps(code, &[], &[], None, None)
    }

    /// Run a file using this engine's configured emitter and event publisher
    pub fn run_file(
        &self,
        file_path: &str,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<crate::val::Val, String> {
        // Use the unified pipeline with file execution
        Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            Some(file_path),
            None,
            PipelineMode::Execute,
            self.emitter.clone(),
            self.execution_context.clone(),
            self.event_publisher.clone(),
            None,  // No context storage (would be set at worker level)
            None,  // No database pool
            None,  // No file storage
            None,  // No store
            None,  // No embedding provider
            None,  // No task queue
            None,  // No stream publisher
            false, // No color in server-side execution
        )
    }

    /// Check sources using this engine's configured emitter and event publisher
    pub fn check_sources(
        &self,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<(), String> {
        Self::check_sources_pipeline_with_deps(src_paths, test_paths, conf, project_name, false)
    }

    /// Run tests using this engine's configured emitter and event publisher
    pub fn run_tests(
        &self,
        pattern: Option<&str>,
        capture_output: bool,
        src_paths: &[String],
        test_paths: &[String],
        conf: Option<&crate::val::Val>,
        project_name: Option<&str>,
    ) -> Result<Vec<TestResult>, String> {
        // Use the unified pipeline with test mode
        match Self::run_unified_pipeline(
            src_paths,
            test_paths,
            conf,
            project_name,
            None,
            None,
            PipelineMode::Test {
                pattern: pattern.map(|s| s.to_string()),
                capture_output,
            },
            self.emitter.clone(),
            self.execution_context.clone(),
            self.event_publisher.clone(),
            None,  // No context storage (would be set at worker level)
            None,  // No database pool
            None,  // No file storage
            None,  // No store
            None,  // No embedding provider
            None,  // No task queue
            None,  // No stream publisher
            false, // No color in server-side test execution
        ) {
            Ok(crate::val::Val::Bool(passed)) => Ok(vec![TestResult {
                name: pattern.unwrap_or("all tests").to_string(),
                passed,
                error: if passed {
                    None
                } else {
                    Some("One or more tests failed".to_string())
                },
            }]),
            Ok(other) => Ok(vec![TestResult {
                name: pattern.unwrap_or("all tests").to_string(),
                passed: false,
                error: Some(format!(
                    "Test runner returned non-boolean result: {:?}",
                    other
                )),
            }]),
            Err(e) => Err(e),
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
