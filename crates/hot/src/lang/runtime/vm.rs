// Virtual machine for Hot bytecode.

use crate::lang::bytecode::{
    BytecodeProgram, Constant, ConstantId, FlowResultModifier, FlowType, FunctionId, Instruction,
    RegisterId, ScopeType,
};
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::runtime::error::RuntimeError;
use crate::lang::runtime::jit::{CodeMemoryStatus, JitConfig, JitFunctionProfile, JitRuntimeState};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

/// Default maximum Hot-level recursion depth. Each user-visible function call
/// adds one to the depth counter (dispatch wrappers like `call`/`call-lib` are
/// not counted). Hitting this limit returns a structured runtime error
/// instead of letting the OS stack overflow take down the process.
///
/// The host runs with a 64 MB Tokio thread stack, and a typical VM frame is on
/// the order of tens of KB, so 4096 is well within the stack budget while
/// still admitting the natural-recursion patterns user code tends to write
/// (tree walks, JSON traversal, parser combinators, etc.).
///
/// Override at runtime with `HOT_MAX_RECURSION_DEPTH=<n>`.
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 4096;

/// Strip the compiler's per-cond uniqueness suffix from a branch name so it can
/// be exposed as a user-visible map key (for `cond-all|map`, `match-all|map`,
/// etc.). The compiler appends `\0c<digits>` to every cond/match branch name
/// to avoid `ConstantId` collisions across nested or sibling flows; the NUL
/// byte cannot appear in a user-supplied label, so finding it is unambiguous.
fn user_facing_branch_name(internal: &str) -> &str {
    match internal.find('\0') {
        Some(idx) => &internal[..idx],
        None => internal,
    }
}

/// Resolve the active recursion-depth cap, honoring `HOT_MAX_RECURSION_DEPTH`.
/// Cached after first read so we don't re-parse the env on every call.
pub fn max_recursion_depth() -> usize {
    use std::sync::OnceLock;
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("HOT_MAX_RECURSION_DEPTH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_RECURSION_DEPTH)
    })
}

/// Failure state that can be shared across threads for parallel execution
#[derive(Debug, Clone)]
pub struct FailureState {
    pub msg: String,
    pub data: Val,
}

/// Thread-safe failure state holder
#[derive(Debug)]
pub struct VmFailureState {
    pub failed: AtomicBool,
    pub failure: RwLock<Option<FailureState>>,
}

/// Cancellation state that can be shared across threads for parallel execution
#[derive(Debug, Clone)]
pub struct CancellationState {
    pub msg: String,
    pub data: Val,
}

/// Thread-safe cancellation state holder
#[derive(Debug)]
pub struct VmCancellationState {
    pub cancelled: AtomicBool,
    pub cancellation: RwLock<Option<CancellationState>>,
}

/// Virtual machine execution error
#[derive(Debug, Clone)]
pub enum VmError {
    InvalidRegister(RegisterId),
    InvalidConstant(ConstantId),
    RuntimeError(RuntimeError),
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::InvalidRegister(reg) => write!(f, "Invalid register: r{}", reg),
            VmError::InvalidConstant(const_id) => write!(f, "Invalid constant: c{}", const_id),
            VmError::RuntimeError(runtime_error) => write!(f, "{}", runtime_error),
        }
    }
}

pub type VmResult<T> = Result<T, VmError>;

/// Result of executing function instructions.
/// Can either be a final value or a tail call request.
#[derive(Debug, Clone)]
enum FunctionExecutionResult {
    /// Function returned a value
    Value(Val),
    /// Function wants to perform a tail call - restart with new arguments
    TailCall {
        /// The function ID being tail-called (must match current function)
        function_id: FunctionId,
        /// New argument values to use
        args: Vec<Val>,
    },
}

impl VmError {
    /// Create a runtime error from a string message
    pub fn runtime(message: String) -> Self {
        VmError::RuntimeError(RuntimeError::new(message))
    }

    /// Create a runtime error from a string message with instruction pointer context
    pub fn runtime_with_ip(message: String, ip: usize) -> Self {
        VmError::RuntimeError(RuntimeError::new(message).with_instruction_pointer(ip))
    }
}

/// Lexical scope for proper variable resolution
#[derive(Debug, Clone)]
pub struct LexicalScope {
    /// Variables in this scope (ordered by insertion)
    variables: IndexMap<String, Val>,
    /// Local type constructors in this scope: type_name -> constructor_function_name
    local_types: IndexMap<String, String>,
    /// Local type implementations in this scope: (source_type, target_type) -> implementation_function_name
    local_implementations: IndexMap<(String, String), String>,
}

/// Flow execution context
#[derive(Debug, Clone)]
pub struct FlowContext {
    /// Unique identifier for this flow execution
    flow_id: uuid::Uuid,
    /// Call ID for inline flow tracking (used for call:start/call:stop events)
    call_id: Option<uuid::Uuid>,
    /// Start time for inline flow tracking
    start_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Type of flow being executed
    flow_type: FlowType,
    /// Result modifier for this flow
    _result_modifier: FlowResultModifier,
    /// Variables referenced in this flow (for result collection)
    _flow_variables: Vec<String>,
    /// Variables referenced during flow execution (name -> value)
    flow_variable_refs: Vec<(String, Val)>,

    /// Conditional branch results (for cond flows) — Arc<str> branch names avoid alloc
    cond_branch_results: Vec<(Arc<str>, Val)>,
    /// Whether any branch has executed (for cond short-circuiting)
    cond_branch_executed: bool,
    /// Current branch name (for tracking which branch calls are made in) — Arc<str> avoids alloc
    current_branch: Option<Arc<str>>,

    /// Pipe step results (for pipe flows with result modifiers)
    pipe_step_results: Vec<(String, Val)>,
}

/// Call execution context for tracking function invocations
#[derive(Debug, Clone)]
pub struct CallContext {
    /// Unique identifier for this call execution
    pub call_id: uuid::Uuid,
    /// Function name being called
    pub function_name: String,
    /// Static scope path from AST (e.g., "::demo::schedule/send-daily-newsletter")
    pub static_scope: String,
    /// Parent call ID (for nested calls)
    pub parent_call_id: Option<uuid::Uuid>,
    /// Call depth (0 = top-level, 1 = first nested call, etc.)
    pub call_depth: usize,
    /// Start time of this call (relative to run start)
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// Monotonic start instant for precise timing
    pub start_instant: std::time::Instant,
}

impl CallContext {
    pub fn new(
        function_name: String,
        static_scope: String,
        parent_call_id: Option<uuid::Uuid>,
        call_depth: usize,
        run_start_time: chrono::DateTime<chrono::Utc>,
        run_start_instant: std::time::Instant,
    ) -> Self {
        let now_instant = std::time::Instant::now();
        let elapsed = now_instant.duration_since(run_start_instant);
        let start_time = run_start_time
            + chrono::Duration::from_std(elapsed).unwrap_or(chrono::Duration::zero());

        Self {
            call_id: Uuid::now_v7(),
            function_name,
            static_scope,
            parent_call_id,
            call_depth,
            start_time,
            start_instant: now_instant,
        }
    }

    /// Generate runtime execution path for this call
    /// Format: "call_<call_id>" or "call_<parent_id>/call_<call_id>" for nested calls
    pub fn runtime_path(&self) -> String {
        format!("call_{}", self.call_id)
    }

    /// Calculate duration in microseconds from start_time to now
    pub fn duration_us(&self) -> i64 {
        let end_time = chrono::Utc::now();
        (end_time - self.start_time).num_microseconds().unwrap_or(0)
    }
}

/// Deferred variable thunk for TRUE parallel execution
#[derive(Debug, Clone)]
struct DeferredVarThunk {
    var_name: String,
    /// Thunk (zero-arg lambda) to evaluate - contains LambdaInfo
    thunk: Val,
}

/// Context for parallel flow execution
#[derive(Debug, Clone)]
struct ParallelFlowContext {
    /// Deferred variable thunks to be evaluated in parallel
    deferred_thunks: Vec<DeferredVarThunk>,
    /// Dependency levels: [[var_a, var_b], [var_c]]
    /// Variables in the same inner vec can execute in parallel
    dependency_levels: Vec<Vec<String>>,
    /// Whether we've captured the metadata yet
    metadata_captured: bool,
}

impl LexicalScope {
    pub fn new(_scope_type: ScopeType, _parent_depth: Option<usize>) -> Self {
        Self {
            variables: IndexMap::new(),
            local_types: IndexMap::new(),
            local_implementations: IndexMap::new(),
        }
    }

    /// Add a local type constructor to this scope
    pub fn add_local_type(&mut self, type_name: String, constructor_function_name: String) {
        self.local_types
            .insert(type_name, constructor_function_name);
    }

    /// Add a local type implementation to this scope
    pub fn add_local_implementation(
        &mut self,
        source_type: String,
        target_type: String,
        implementation_function_name: String,
    ) {
        self.local_implementations
            .insert((source_type, target_type), implementation_function_name);
    }

    /// Get a local type constructor from this scope
    pub fn get_local_type(&self, type_name: &str) -> Option<&String> {
        self.local_types.get(type_name)
    }

    /// Get a local type implementation from this scope
    pub fn get_local_implementation(
        &self,
        source_type: &str,
        target_type: &str,
    ) -> Option<&String> {
        self.local_implementations
            .get(&(source_type.to_string(), target_type.to_string()))
    }
}

/// Default number of engine threads for parallel execution
pub const DEFAULT_ENGINE_THREADS: usize = 4;

/// High-performance virtual machine for Hot bytecode
pub struct VirtualMachine {
    pub program: Arc<BytecodeProgram>,
    registers: Vec<Val>,
    /// Baseline register capacity for the entry context
    initial_register_capacity: usize,
    /// Instrumentation: peak registers vector length observed during execution
    peak_registers_len: usize,
    /// Instrumentation: peak total namespace variable key count observed
    peak_namespace_var_count: usize,
    instruction_pointer: usize,
    /// Lexical scope stack for proper variable resolution
    scope_stack: Vec<LexicalScope>,

    /// Flow context stack for nested flows
    flow_contexts: Vec<FlowContext>,
    /// Call context stack for tracking function invocations and runtime paths
    call_stack: Vec<CallContext>,
    /// Hot AST for namespace and metadata access
    hot_ast: Option<Arc<crate::lang::ast::HotAst>>,
    /// Debug: Current function name for correlation
    current_debug_function: Option<String>,
    /// Function name to ID mapping from compiler (ordered for consistent resolution)
    function_mapping: Arc<IndexMap<String, crate::lang::bytecode::FunctionId>>,
    /// Core functions registry for call-lib access (ordered)
    core_functions: Arc<IndexMap<String, crate::lang::bytecode::FunctionId>>,
    /// Core variables registry for metadata-driven auto-import
    core_variables: Arc<CoreVariableRegistry>,
    /// Type implementations for dispatch: (source_type, target_type) -> constructor_function_name (ordered)
    type_implementations: Arc<IndexMap<(String, String), String>>,
    /// Global namespace variables registry: namespace -> (variable_name -> value)
    pub(crate) namespace_variables: indexmap::IndexMap<String, indexmap::IndexMap<String, Val>>,
    /// Simple namespace registry for context variables: variable_name -> value
    pub namespace_registry: AHashMap<String, Val>,
    /// Current namespace for variable storage
    pub(crate) current_namespace: String,
    /// Engine event emitter for variable tracking
    emitter: Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>>,
    /// Execution context for emitter events
    execution_context: Option<crate::lang::event::ExecutionContext>,
    /// Database pool for file storage and other operations
    database_pool: Option<Arc<crate::db::DatabasePool>>,
    /// File storage backend for file operations
    file_storage: Option<Arc<dyn crate::file_storage::FileStorage>>,
    /// Persistent store backend for ::hot::store functions
    store: Option<Arc<dyn crate::store::Store>>,
    /// Embedding provider for ::hot::store search
    embedding_provider: Option<Arc<dyn crate::store::embedding::EmbeddingProvider>>,
    /// Run start time for precise timing calculations
    run_start_time: chrono::DateTime<chrono::Utc>,
    /// Run start instant for monotonic timing
    run_start_instant: std::time::Instant,
    /// Event publisher for user-defined events
    event_publisher: Option<Arc<dyn crate::lang::event::EventPublisher>>,
    /// Template literal recursion depth to prevent infinite loops
    template_recursion_depth: usize,
    /// Skip execution until we reach a CondBranchEnd with this constant ID
    /// Tuple: (branch_name, flow_stack_depth) to ensure we only skip within the correct flow
    skip_until_branch_end: Option<(ConstantId, usize)>,
    /// Skip all remaining branches in a cond/match flow after the first match.
    /// The counter tracks nested BeginFlow/EndFlow depth so we stop at the correct EndFlow.
    /// Set after a cond/match branch executes to skip condition evaluation code too.
    skip_remaining_cond_flow: Option<usize>,
    /// Function call recursion depth to prevent infinite loops
    function_call_depth: usize,
    /// Specific recursion depth tracking for the call function to prevent infinite recursion
    call_function_depth: usize,
    /// If true, convert runtime errors into Result-style maps instead of aborting
    pub(crate) error_capture_active: bool,
    /// Suppress automatic Result checking during lazy thunk evaluation
    suppress_result_checking: bool,
    /// User context storage for ctx-set/ctx-get functions (VM-specific)
    pub context_storage: AHashMap<String, Val>,
    /// Set of context keys that are secrets (should be masked in logs/db)
    pub secret_keys: AHashSet<String>,
    /// Set of secret value hashes that should be masked in call logs
    /// This is populated when ctx/get returns a secret value
    /// We store hashes (u64) for efficient comparison and to handle any Val type
    pub secret_value_hashes: AHashSet<u64>,
    /// Engine configuration (includes thread count, etc.)
    conf: Val,
    /// Thread count for parallel execution (cached from conf)
    thread_count: usize,
    /// Parallel flow execution context (active when executing parallel flows)
    parallel_flow_context: Option<ParallelFlowContext>,
    /// Function dispatch cache: maps (function_name_hash, arity) → FunctionId.
    /// Avoids repeated string formatting and multi-map probing in unified_function_lookup.
    /// Populated lazily on first resolution; valid for the lifetime of the program.
    dispatch_cache: AHashMap<u64, FunctionId>,
    /// Per-function-id cache of declared parameter type names, recovered from
    /// the typed signature key in `function_mapping` (`name/N:T1,T2,...`).
    /// `None` means the function is untyped (no typed signature was registered);
    /// populated lazily on first call so we don't pay the lookup cost twice.
    /// Used by the `CallUserFunction` opcode handler to apply implicit
    /// `Source -> Target` coercions when the runtime arg types differ from
    /// the declared param types (the type checker has already vetted the call).
    coercion_param_cache: AHashMap<FunctionId, Option<Vec<String>>>,
    /// JIT configuration, code-memory status, and per-function profiling state.
    jit: JitRuntimeState,
    /// Failure state (thread-safe for parallel execution)
    failure_state: Arc<VmFailureState>,
    /// Cancellation state (thread-safe for parallel execution)
    cancellation_state: Arc<VmCancellationState>,
    /// Isolation mode for multi-tenant environments (cached from conf)
    isolation_enabled: bool,
    /// Stream publisher for real-time stream data delivery
    stream_publisher: Option<Arc<crate::stream::StreamPubSub>>,
    /// Task queue for enqueuing task requests (set by worker infrastructure)
    task_queue: Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>>,
    /// Task receive channel (set by task worker when running inside a task)
    task_receiver: Option<Arc<parking_lot::Mutex<tokio::sync::mpsc::Receiver<Val>>>>,
    /// External cancellation token — checked periodically by the instruction loop.
    /// When set to `true`, the VM returns a cancellation error at the next check point.
    external_cancel: Option<Arc<AtomicBool>>,
    /// Task ID when this VM is executing inside a task worker.
    /// Used by `::hot::task/checkpoint` and `::hot::task/restore`.
    task_id: Option<uuid::Uuid>,
}

impl VirtualMachine {
    pub fn new(
        program: Arc<BytecodeProgram>,
        hot_ast: Option<Arc<crate::lang::ast::HotAst>>,
        function_mapping: Arc<IndexMap<String, crate::lang::bytecode::FunctionId>>,
        core_functions: Arc<IndexMap<String, crate::lang::bytecode::FunctionId>>,
        type_implementations: Arc<IndexMap<(String, String), String>>,
        core_variables: Arc<CoreVariableRegistry>,
        conf: Option<Val>,
    ) -> Self {
        let register_count = program.entry_register_count as usize;

        // Initialize with a global namespace scope
        let scope_stack = vec![LexicalScope::new(ScopeType::Namespace, None)];

        // Get conf with defaults
        let conf = conf.unwrap_or_else(Val::map_empty);

        // Extract thread count from conf (matches hot.engine.threads in hot.hot)
        let thread_count = conf
            .get_int_or_default("engine.threads", DEFAULT_ENGINE_THREADS as i64)
            .max(1) as usize;

        // Extract isolation mode from conf (matches hot.engine.isolation in hot.hot)
        let isolation_enabled = conf.get_bool_or_default("engine.isolation", false);

        tracing::debug!("VM: Initializing with thread_count = {}", thread_count);
        tracing::debug!("VM: Isolation mode enabled = {}", isolation_enabled);

        let jit = JitRuntimeState::new(program.functions.len(), &conf);

        let mut vm = Self {
            program,
            registers: vec![Val::Null; register_count.max(256)],
            initial_register_capacity: register_count.max(256),
            peak_registers_len: register_count.max(256),
            peak_namespace_var_count: 0,
            instruction_pointer: 0,
            scope_stack,

            flow_contexts: Vec::new(),
            call_stack: Vec::new(),
            hot_ast,
            current_debug_function: None,
            function_mapping,
            core_functions,
            core_variables,
            type_implementations,
            namespace_variables: IndexMap::new(),
            namespace_registry: AHashMap::new(),
            current_namespace: "::hot::main".to_string(),
            emitter: None,
            execution_context: None,
            database_pool: None,
            file_storage: None,
            store: None,
            embedding_provider: None,
            run_start_time: chrono::Utc::now(),
            run_start_instant: std::time::Instant::now(),
            event_publisher: None,
            template_recursion_depth: 0,
            skip_until_branch_end: None,
            skip_remaining_cond_flow: None,
            function_call_depth: 0,
            call_function_depth: 0,
            error_capture_active: false,
            suppress_result_checking: false,
            context_storage: AHashMap::new(),
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            conf,
            thread_count,
            parallel_flow_context: None,
            dispatch_cache: AHashMap::new(),
            coercion_param_cache: AHashMap::new(),
            jit,
            failure_state: Arc::new(VmFailureState {
                failed: AtomicBool::new(false),
                failure: RwLock::new(None),
            }),
            cancellation_state: Arc::new(VmCancellationState {
                cancelled: AtomicBool::new(false),
                cancellation: RwLock::new(None),
            }),
            isolation_enabled,
            stream_publisher: None,
            task_queue: None,
            task_receiver: None,
            external_cancel: None,
            task_id: None,
        };

        // Ensure the default namespace has a ns variable. Failure here would
        // indicate an internal setup bug, not user input — log and continue with
        // an empty namespace so we never panic during VM construction.
        if let Err(e) = vm.ensure_namespace_has_ns_variable("::hot::main") {
            tracing::error!(
                error = %e,
                "Failed to seed ::hot::main namespace during VM construction"
            );
        }

        vm
    }

    /// Get function mapping (for debugging/introspection)
    pub fn get_function_mapping(&self) -> &IndexMap<String, crate::lang::bytecode::FunctionId> {
        &self.function_mapping
    }

    /// Check if isolation mode is enabled (for multi-tenant environments)
    pub fn is_isolation_enabled(&self) -> bool {
        self.isolation_enabled
    }

    /// Build runtime execution path from current execution context
    /// Format: "run_<run_id>/call_<call_id>/flow_<flow_id>"
    /// Returns None if no execution context is set
    fn build_runtime_path(&self) -> Option<String> {
        let execution_context = self.execution_context.as_ref()?;

        let mut path = format!("run_{}", execution_context.run_id);

        // Add current call context if any
        if let Some(call_ctx) = self.call_stack.last() {
            path.push_str(&format!("/call_{}", call_ctx.call_id));
        }

        // Add current flow context if any
        if let Some(flow_ctx) = self.flow_contexts.last() {
            path.push_str(&format!("/flow_{}", flow_ctx.flow_id));
        }

        Some(path)
    }

    /// Get type implementations (for debugging/introspection)
    pub fn get_type_implementations(&self) -> &IndexMap<(String, String), String> {
        &self.type_implementations
    }

    /// Return the effective JIT configuration for this VM.
    pub fn jit_config(&self) -> &JitConfig {
        &self.jit.config
    }

    /// Return the detected code-memory status for this VM's current platform/runtime.
    pub fn jit_code_memory_status(&self) -> &CodeMemoryStatus {
        &self.jit.code_memory_status
    }

    /// Return a snapshot of per-function JIT profiling for introspection/debugging.
    pub fn jit_function_profile(&self, function_id: FunctionId) -> Option<&JitFunctionProfile> {
        self.jit.function_profile(function_id)
    }

    pub fn jit_has_compiled_function(&self, function_id: FunctionId) -> bool {
        self.jit.has_compiled_function(function_id)
    }

    /// Set the database pool for file storage and other operations
    pub fn set_database_pool(&mut self, pool: Arc<crate::db::DatabasePool>) {
        self.database_pool = Some(pool);
    }

    /// Set the file storage backend
    pub fn set_file_storage(&mut self, storage: Arc<dyn crate::file_storage::FileStorage>) {
        self.file_storage = Some(storage);
    }

    /// Get the database pool
    pub fn get_database_pool(&self) -> Option<Arc<crate::db::DatabasePool>> {
        self.database_pool.clone()
    }

    /// Get the file storage backend
    pub fn get_file_storage(&self) -> Option<Arc<dyn crate::file_storage::FileStorage>> {
        self.file_storage.clone()
    }

    /// Set the persistent store backend
    pub fn set_store(&mut self, store: Arc<dyn crate::store::Store>) {
        self.store = Some(store);
    }

    /// Get the persistent store backend
    pub fn get_store(&self) -> Option<Arc<dyn crate::store::Store>> {
        self.store.clone()
    }

    /// Set the embedding provider
    pub fn set_embedding_provider(
        &mut self,
        provider: Arc<dyn crate::store::embedding::EmbeddingProvider>,
    ) {
        self.embedding_provider = Some(provider);
    }

    /// Get the embedding provider
    pub fn get_embedding_provider(
        &self,
    ) -> Option<Arc<dyn crate::store::embedding::EmbeddingProvider>> {
        self.embedding_provider.clone()
    }

    /// Get the thread count for parallel execution
    pub fn get_thread_count(&self) -> usize {
        self.thread_count
    }

    /// Get the engine configuration
    pub fn get_conf(&self) -> &Val {
        &self.conf
    }

    /// Get the run start time
    pub fn get_run_start_time(&self) -> chrono::DateTime<chrono::Utc> {
        self.run_start_time
    }

    /// Get a clone of the hot_ast Arc (for parallel execution)
    pub fn get_hot_ast_arc(&self) -> Option<Arc<crate::lang::ast::HotAst>> {
        self.hot_ast.clone()
    }

    /// Get a clone of the function_mapping Arc (for parallel execution)
    pub fn get_function_mapping_arc(
        &self,
    ) -> Arc<IndexMap<String, crate::lang::bytecode::FunctionId>> {
        self.function_mapping.clone()
    }

    /// Get a clone of the core_functions Arc (for parallel execution)
    pub fn get_core_functions_arc(
        &self,
    ) -> Arc<IndexMap<String, crate::lang::bytecode::FunctionId>> {
        self.core_functions.clone()
    }

    /// Get a clone of the type_implementations Arc (for parallel execution)
    pub fn get_type_implementations_arc(&self) -> Arc<IndexMap<(String, String), String>> {
        self.type_implementations.clone()
    }

    /// Get a clone of the core_variables Arc (for parallel execution)
    pub fn get_core_variables_arc(&self) -> Arc<CoreVariableRegistry> {
        self.core_variables.clone()
    }

    /// Store a variable in the current namespace (for parallel execution setup)
    pub fn store_variable_public(&mut self, var_name: &str, value: Val) -> Result<(), VmError> {
        self.store_variable(var_name, value)
    }

    /// Resolve a type constructor using scope chain (local scope first, then global)
    pub fn resolve_type_constructor(&self, type_name: &str) -> Option<String> {
        // First, search in local scopes (from innermost to outermost)
        for scope in self.scope_stack.iter().rev() {
            if let Some(constructor_name) = scope.get_local_type(type_name) {
                tracing::trace!(
                    "VM: Found local type constructor for '{}': {}",
                    type_name,
                    constructor_name
                );
                return Some(constructor_name.clone());
            }
        }

        // If not found in local scopes, check global function mapping for namespace-level types
        // Try various forms of the type name
        let potential_names = vec![
            format!("{}/{}", self.current_namespace, type_name),
            format!("::hot::type/{}", type_name),
            type_name.to_string(),
        ];

        for name in potential_names {
            if self.function_mapping.contains_key(&name) {
                tracing::trace!(
                    "VM: Found global type constructor for '{}': {}",
                    type_name,
                    name
                );
                return Some(name);
            }
        }

        tracing::trace!("VM: No type constructor found for '{}'", type_name);
        None
    }

    /// Get the type name of a value for type dispatch
    fn get_value_type_name(&self, value: &Val) -> String {
        match value {
            Val::Map(m) => {
                // Check if this is a typed value with $type metadata
                if let Some(Val::Str(type_name)) = m.get(&Val::from("$type")) {
                    // Extract just the type name from the full path
                    if let Some(last_part) = type_name.split('/').next_back() {
                        last_part.to_string()
                    } else {
                        (**type_name).to_owned()
                    }
                } else {
                    "Map".to_string()
                }
            }
            Val::Str(_) => "Str".to_string(),
            Val::Int(_) => "Int".to_string(),
            Val::Dec(_) => "Dec".to_string(),
            Val::Bool(_) => "Bool".to_string(),
            Val::Vec(_) => "Vec".to_string(),
            Val::Null => "Null".to_string(),
            Val::Box(_) => "Box".to_string(),
            Val::Byte(_) => "Byte".to_string(),
            Val::Bytes(_) => "Bytes".to_string(),
        }
    }

    /// Resolve a type implementation using scope chain (local scope first, then global)
    pub fn resolve_type_implementation(
        &self,
        source_type: &str,
        target_type: &str,
    ) -> Option<String> {
        // First, search in local scopes (from innermost to outermost)
        for scope in self.scope_stack.iter().rev() {
            if let Some(impl_name) = scope.get_local_implementation(source_type, target_type) {
                return Some(impl_name.clone());
            }
        }

        // If not found in local scopes, check global type implementations
        let key = (source_type.to_string(), target_type.to_string());
        if let Some(impl_name) = self.type_implementations.get(&key) {
            return Some(impl_name.clone());
        }

        None
    }

    /// Get current namespace context
    pub fn get_current_namespace(&self) -> &str {
        &self.current_namespace
    }

    /// Set the current namespace (for REPL namespace restoration)
    pub fn set_current_namespace(&mut self, namespace: String) {
        self.current_namespace = namespace;
    }

    /// Restore namespace variables from a previous execution (for REPL state preservation)
    pub fn restore_namespace_variables(
        &mut self,
        state: &indexmap::IndexMap<String, indexmap::IndexMap<String, Val>>,
    ) {
        self.namespace_variables = state.clone();
    }

    /// Execute a range of instructions (for incremental REPL execution)
    /// This executes instructions from `start` (inclusive) to `end` (exclusive)
    pub fn execute_instruction_range(&mut self, start: usize, end: usize) -> VmResult<Val> {
        // Set instruction pointer to start
        self.instruction_pointer = start;

        let mut last_result_register = 0;
        let mut instruction_count = 0;

        tracing::trace!(
            "VM starting instruction range execution [{}, {})",
            start,
            end
        );

        let result = (|| -> VmResult<Val> {
            let program = Arc::clone(&self.program);
            let ext_cancel = self.external_cancel.clone();
            while self.instruction_pointer < end
                && self.instruction_pointer < program.entry_point.len()
            {
                instruction_count += 1;
                // High instruction limit to allow legitimate computation
                // (10 million instructions should handle most workloads)
                if instruction_count > 10_000_000 {
                    return Err(VmError::runtime_with_ip(
                        "Instruction limit reached - too many instructions executed".to_string(),
                        self.instruction_pointer,
                    ));
                }

                if instruction_count & 0x3FF == 0
                    && let Some(ref token) = ext_cancel
                    && token.load(Ordering::Relaxed)
                {
                    return Err(VmError::runtime_with_ip(
                        "Execution cancelled".to_string(),
                        self.instruction_pointer,
                    ));
                }

                let instruction = &program.entry_point[self.instruction_pointer];
                tracing::trace!(
                    "VM executing instruction {}: {:?}",
                    self.instruction_pointer,
                    instruction
                );

                // Track the destination register of the last instruction that produces a value
                match instruction {
                    Instruction::LoadConst { dest, .. }
                    | Instruction::Move { dest, .. }
                    | Instruction::Add { dest, .. }
                    | Instruction::Call { dest, .. }
                    | Instruction::CallWithSpread { dest, .. }
                    | Instruction::CallNative { dest, .. }
                    | Instruction::LoadVar { dest, .. }
                    | Instruction::LoadVarOrDefault { dest, .. }
                    | Instruction::LoadScoped { dest, .. }
                    | Instruction::CaptureVar { dest, .. }
                    | Instruction::LoadFunctionRef { dest, .. }
                    | Instruction::EndFlow { dest, .. }
                    | Instruction::Pipe { dest, .. }
                    | Instruction::DefineFunction { dest, .. }
                    | Instruction::DotAccess { dest, .. }
                    | Instruction::DotAccessOrDefault { dest, .. }
                    | Instruction::GetTypePath { dest, .. }
                    | Instruction::IsType { dest, .. }
                    | Instruction::TemplateInterpolate { dest, .. }
                    | Instruction::CallLibBuiltin { dest, .. }
                    | Instruction::CallUserFunction { dest, .. }
                    | Instruction::CallLambda { dest, .. }
                    | Instruction::ExtractInnerVal { dest, .. }
                    | Instruction::ConstructTyped { dest, .. } => {
                        last_result_register = *dest;
                    }
                    _ => {}
                }

                self.execute_instruction(instruction)?;
            }

            Ok(self.get_register(last_result_register)?.clone())
        })();

        tracing::trace!(
            "VM finished instruction range execution, executed {} instructions",
            instruction_count
        );

        result
    }

    /// Evaluate an AST expression with captured scope (for lazy thunks)
    /// This temporarily injects captured namespace variables to preserve lexical scope
    pub fn evaluate_with_captured_scope(
        &mut self,
        expr: &crate::lang::ast::Value,
        captured_namespace: &str,
        captured_namespace_vars: &indexmap::IndexMap<String, crate::val::Val>,
    ) -> Result<crate::val::Val, crate::lang::runtime::vm::VmError> {
        // Save current state
        let saved_namespace = self.current_namespace.clone();
        let saved_namespace_vars = self
            .namespace_variables
            .get(captured_namespace)
            .cloned()
            .unwrap_or_default();

        // Temporarily set the captured namespace as current
        self.current_namespace = captured_namespace.to_string();

        // Temporarily inject captured namespace variables
        self.namespace_variables
            .entry(captured_namespace.to_string())
            .or_default()
            .extend(captured_namespace_vars.clone());

        // Evaluate the expression
        let result = self.convert_ast_value_to_runtime_val(expr);

        // Restore original state
        self.current_namespace = saved_namespace;
        if saved_namespace_vars.is_empty() {
            self.namespace_variables.shift_remove(captured_namespace);
        } else {
            self.namespace_variables
                .insert(captured_namespace.to_string(), saved_namespace_vars);
        }

        result
    }

    /// Get Hot AST for metadata and namespace access
    pub fn get_hot_ast(&self) -> Option<&Arc<crate::lang::ast::HotAst>> {
        self.hot_ast.as_ref()
    }

    /// Set the emitter for variable tracking
    pub fn set_emitter(&mut self, emitter: Arc<dyn crate::lang::emitter::EngineEventEmitter>) {
        self.emitter = Some(emitter);
    }

    /// Get the emitter reference
    pub fn get_emitter(&self) -> &Option<Arc<dyn crate::lang::emitter::EngineEventEmitter>> {
        &self.emitter
    }

    /// Set the stream publisher for real-time stream data delivery
    pub fn set_stream_publisher(&mut self, publisher: Arc<crate::stream::StreamPubSub>) {
        self.stream_publisher = Some(publisher);
    }

    /// Get the stream publisher reference
    pub fn get_stream_publisher(&self) -> Option<Arc<crate::stream::StreamPubSub>> {
        self.stream_publisher.clone()
    }

    /// Set the task queue for enqueuing task requests
    pub fn set_task_queue(
        &mut self,
        queue: Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>,
    ) {
        self.task_queue = Some(queue);
    }

    /// Get the task queue
    pub fn get_task_queue(
        &self,
    ) -> Option<Arc<crate::queue::ProcessingQueue<crate::lang::hot::task::TaskRequest>>> {
        self.task_queue.clone()
    }

    /// Set the task receive channel (used by the task worker)
    pub fn set_task_receiver(
        &mut self,
        receiver: Arc<parking_lot::Mutex<tokio::sync::mpsc::Receiver<Val>>>,
    ) {
        self.task_receiver = Some(receiver);
    }

    /// Set an external cancellation token.
    /// When the flag is set to `true`, the VM will stop at the next check point.
    pub fn set_external_cancel(&mut self, token: Arc<AtomicBool>) {
        self.external_cancel = Some(token);
    }

    /// Set the task ID for checkpoint/restore support.
    pub fn set_task_id(&mut self, task_id: uuid::Uuid) {
        self.task_id = Some(task_id);
    }

    /// Get the task ID (set when running inside a task worker).
    pub fn get_task_id(&self) -> Option<uuid::Uuid> {
        self.task_id
    }

    /// Get the task receive channel
    pub fn get_task_receiver(
        &self,
    ) -> Option<Arc<parking_lot::Mutex<tokio::sync::mpsc::Receiver<Val>>>> {
        self.task_receiver.clone()
    }

    /// Get the suppress_result_checking flag
    pub fn get_suppress_result_checking(&self) -> bool {
        self.suppress_result_checking
    }

    /// Set the suppress_result_checking flag
    pub fn set_suppress_result_checking(&mut self, suppress: bool) {
        self.suppress_result_checking = suppress;
    }

    /// Set the execution context for emitter events
    pub fn set_execution_context(
        &mut self,
        execution_context: crate::lang::event::ExecutionContext,
    ) {
        self.execution_context = Some(execution_context);
    }

    /// Sync secret_keys from VM to execution_context for database masking
    pub fn sync_secret_keys_to_execution_context(&mut self) {
        if let Some(ref mut ctx) = self.execution_context {
            tracing::trace!(
                "sync_secret_keys_to_execution_context: syncing {} secret keys: {:?}",
                self.secret_keys.len(),
                self.secret_keys
            );
            ctx.secret_keys = self.secret_keys.clone();
        } else {
            tracing::trace!(
                "sync_secret_keys_to_execution_context: no execution_context to sync to"
            );
        }
    }

    /// Sync secret_value_hashes from VM to execution_context for database masking
    /// This should be called after any ctx/get call that might return a secret
    pub fn sync_secret_value_hashes_to_execution_context(&mut self) {
        if let Some(ref mut ctx) = self.execution_context {
            // Merge new secret value hashes into the execution context
            for hash in &self.secret_value_hashes {
                ctx.secret_value_hashes.insert(*hash);
            }
        }
    }

    /// Set the event publisher for user-defined events
    pub fn set_event_publisher(
        &mut self,
        event_publisher: Arc<dyn crate::lang::event::EventPublisher>,
    ) {
        self.event_publisher = Some(event_publisher);
    }

    /// Get the event publisher for the VM
    pub fn get_event_publisher(&self) -> &Option<Arc<dyn crate::lang::event::EventPublisher>> {
        &self.event_publisher
    }

    /// Get the execution context for the VM
    pub fn get_execution_context(&self) -> &Option<crate::lang::event::ExecutionContext> {
        &self.execution_context
    }

    /// Execute a list of additional instructions on the already-initialized VM (for cached bytecode + eval code)
    pub fn execute_instruction_list(&mut self, instructions: &[Instruction]) -> VmResult<Val> {
        let mut last_result_register = 0;

        for instruction in instructions.iter() {
            self.execute_instruction(instruction)?;

            // Track which register holds the result (for Return instructions)
            if let Instruction::Return { value } = instruction {
                last_result_register = *value;
            }
        }

        // Return the value from the last result register
        let reg_idx = last_result_register as usize;
        if reg_idx < self.registers.len() {
            Ok(self.registers[reg_idx].clone())
        } else {
            Ok(Val::Null)
        }
    }

    pub fn execute(&mut self) -> VmResult<Val> {
        let mut last_result_register = 0;
        let mut instruction_count = 0;
        tracing::trace!(
            "VM starting execution with {} instructions",
            self.program.entry_point.len()
        );

        // Emit run:start event if emitter and execution context are configured
        tracing::debug!(
            "VM: emitter={}, execution_context={}",
            if self.emitter.is_some() {
                "Some"
            } else {
                "None"
            },
            if self.execution_context.is_some() {
                "Some"
            } else {
                "None"
            }
        );
        if let (Some(emitter), Some(execution_context)) = (&self.emitter, &self.execution_context) {
            tracing::info!(
                "VM: Emitting run:start for run_id={}, event_id={:?}",
                execution_context.run_id,
                execution_context.event_id
            );
            let start_event = crate::lang::emitter::EngineEvent::run_start(execution_context);
            emitter.emit(start_event);
        } else {
            tracing::trace!(
                "VM: NOT emitting run:start - emitter={}, execution_context={}",
                if self.emitter.is_some() {
                    "Some"
                } else {
                    "None"
                },
                if self.execution_context.is_some() {
                    "Some"
                } else {
                    "None"
                }
            );
        }

        // Reset instrumentation at the start of a run
        self.peak_registers_len = self.registers.len();
        self.peak_namespace_var_count = self.namespace_variables.values().map(|ns| ns.len()).sum();

        // Execute instructions and capture the result or error
        // Arc::clone avoids cloning every instruction on every tick of the main loop.
        // The local `program` is not part of `self`, so borrowing instructions from it
        // does not conflict with `&mut self` calls.
        let execution_result = (|| -> VmResult<Val> {
            let program = Arc::clone(&self.program);
            let ext_cancel = self.external_cancel.clone();
            while self.instruction_pointer < program.entry_point.len() {
                instruction_count += 1;
                // High instruction limit to allow legitimate computation
                // (10 million instructions should handle most workloads)
                if instruction_count > 10_000_000 {
                    tracing::warn!(
                        "VM instruction limit reached after {} instructions at IP {}",
                        instruction_count,
                        self.instruction_pointer
                    );
                    tracing::trace!(
                        "Current instruction: {:?}",
                        program.entry_point.get(self.instruction_pointer)
                    );
                    return Err(VmError::runtime_with_ip(
                        "Instruction limit reached - too many instructions executed".to_string(),
                        self.instruction_pointer,
                    ));
                }

                // Check external cancellation every 1024 instructions (Relaxed load is ~free)
                if instruction_count & 0x3FF == 0
                    && let Some(ref token) = ext_cancel
                    && token.load(Ordering::Relaxed)
                {
                    return Err(VmError::runtime_with_ip(
                        "Execution cancelled".to_string(),
                        self.instruction_pointer,
                    ));
                }

                let instruction = &program.entry_point[self.instruction_pointer];
                tracing::trace!(
                    "VM executing instruction {}: {:?}",
                    self.instruction_pointer,
                    instruction
                );

                // Track the destination register of the last instruction that produces a value
                match instruction {
                    Instruction::LoadConst { dest, .. }
                    | Instruction::Move { dest, .. }
                    | Instruction::Add { dest, .. }
                    | Instruction::Call { dest, .. }
                    | Instruction::CallWithSpread { dest, .. }
                    | Instruction::CallNative { dest, .. }
                    | Instruction::LoadVar { dest, .. }
                    | Instruction::LoadVarOrDefault { dest, .. }
                    | Instruction::LoadScoped { dest, .. }
                    | Instruction::CaptureVar { dest, .. }
                    | Instruction::LoadFunctionRef { dest, .. }
                    | Instruction::EndFlow { dest, .. }
                    | Instruction::Pipe { dest, .. }
                    | Instruction::DefineFunction { dest, .. }
                    | Instruction::DotAccess { dest, .. }
                    | Instruction::DotAccessOrDefault { dest, .. }
                    | Instruction::GetTypePath { dest, .. }
                    | Instruction::IsType { dest, .. }
                    | Instruction::TemplateInterpolate { dest, .. }
                    | Instruction::CallLibBuiltin { dest, .. }
                    | Instruction::CallUserFunction { dest, .. }
                    | Instruction::CallLambda { dest, .. }
                    | Instruction::ExtractInnerVal { dest, .. }
                    | Instruction::ConstructTyped { dest, .. } => {
                        last_result_register = *dest;
                    }
                    _ => {}
                }

                self.execute_instruction(instruction)?;
            }

            self.get_register(last_result_register).cloned()
        })();

        // Emit appropriate run event (stop, fail, or cancel) before returning
        match &execution_result {
            Ok(result_val) => {
                // Check if VM has failed or cancelled (e.g., via fail()/cancel() functions) even though execution returned Ok
                // This happens because HotResult::Err from hotlib functions gets wrapped in Ok(Result{$err: ...})
                if self.has_failed() {
                    // VM failure state is set, don't emit run:stop (run:fail was already emitted)
                    tracing::trace!(
                        "VM: Execution returned Ok but VM has failure state - skipping run:stop event"
                    );
                } else if self.has_cancelled() {
                    // VM cancellation state is set, don't emit run:stop (run:cancel was already emitted)
                    tracing::trace!(
                        "VM: Execution returned Ok but VM has cancellation state - skipping run:stop event"
                    );
                } else if result_val.is_err() {
                    // Result is a Result.Err type - emit run:fail event
                    // This handles cases where a Result.Err was returned without calling fail()
                    if let (Some(emitter), Some(execution_context)) =
                        (&self.emitter, &self.execution_context)
                    {
                        // Extract the error value from Result.Err
                        let failure = if let Some(err_val) = result_val.unwrap_err() {
                            // Check if err_val already has $msg/$err structure
                            if let Val::Map(m) = err_val {
                                if m.contains_key(&Val::from("$msg"))
                                    || m.contains_key(&Val::from("$err"))
                                {
                                    err_val.clone()
                                } else {
                                    // Wrap in failure format
                                    crate::val!({
                                        "$msg": format!("{}", err_val),
                                        "$err": err_val.clone()
                                    })
                                }
                            } else {
                                crate::val!({
                                    "$msg": format!("{}", err_val),
                                    "$err": err_val.clone()
                                })
                            }
                        } else {
                            crate::val!({
                                "$msg": "Unknown error",
                                "$err": result_val.clone()
                            })
                        };
                        let event =
                            crate::lang::emitter::EngineEvent::run_fail(execution_context, failure);
                        emitter.emit(event);
                        tracing::trace!(
                            "VM: Execution returned Result.Err - emitting run:fail event"
                        );
                    }
                } else if result_val.is_cancelled() {
                    // Result is a Cancellation type - emit run:cancel event
                    // This handles cases where a Cancellation was returned without calling cancel()
                    if let (Some(emitter), Some(execution_context)) =
                        (&self.emitter, &self.execution_context)
                    {
                        let event = crate::lang::emitter::EngineEvent::run_cancel(
                            execution_context,
                            result_val.clone(),
                        );
                        emitter.emit(event);
                        tracing::trace!(
                            "VM: Execution returned Cancellation - emitting run:cancel event"
                        );
                    }
                } else {
                    // Success: emit run:stop event with result
                    if let (Some(emitter), Some(execution_context)) =
                        (&self.emitter, &self.execution_context)
                    {
                        // Unwrap ::hot::type/Result.Ok if present - extract the $val value
                        // since run status already indicates success
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
            }
            Err(e) => {
                // Failure: emit run:fail event (if not already emitted)
                if !self.has_failed()
                    && let (Some(emitter), Some(execution_context)) =
                        (&self.emitter, &self.execution_context)
                {
                    let failure = crate::val!({
                        "$msg": e.to_string(),
                        "$err": crate::val!({"error": e.to_string()})
                    });
                    let event =
                        crate::lang::emitter::EngineEvent::run_fail(execution_context, failure);
                    emitter.emit(event);
                }
            }
        }

        // Truncate registers to initial capacity to avoid accumulation across runs
        if self.registers.len() > self.initial_register_capacity {
            self.registers.truncate(self.initial_register_capacity);
        }
        // Log instrumentation snapshot at end of run
        let total_ns_vars: usize = self.namespace_variables.values().map(|ns| ns.len()).sum();
        if total_ns_vars > self.peak_namespace_var_count {
            self.peak_namespace_var_count = total_ns_vars;
        }
        tracing::trace!(
            peak_registers = self.peak_registers_len,
            current_registers = self.registers.len(),
            peak_namespace_vars = self.peak_namespace_var_count,
            current_namespace_vars = total_ns_vars,
            "VM instrumentation: resource usage snapshot"
        );

        execution_result
    }

    fn execute_instruction(&mut self, instruction: &Instruction) -> VmResult<()> {
        tracing::trace!(
            "VM: Executing instruction at {}: {:?}",
            self.instruction_pointer,
            instruction
        );

        // Check if we're skipping instructions until we reach a specific CondBranchEnd
        // The skip is tied to a specific flow context depth to prevent cross-flow interference
        if let Some((skip_branch_id, skip_flow_depth)) = &self.skip_until_branch_end {
            let current_depth = self.flow_contexts.len();
            // Only skip if we're still in the same flow context (same depth)
            // If we've exited to a lower depth, the skip was for a nested flow and should be cleared
            if current_depth < *skip_flow_depth {
                // We've exited the flow that set the skip - clear it
                tracing::trace!(
                    "VM: Clearing skip flag - exited flow (current_depth={}, skip_depth={})",
                    current_depth,
                    skip_flow_depth
                );
                self.skip_until_branch_end = None;
            } else if current_depth == *skip_flow_depth {
                // We're in the same flow - apply skip logic
                match instruction {
                    Instruction::CondBranchEnd { branch_name, .. } => {
                        if branch_name == skip_branch_id {
                            // We've reached the end of the branch we were skipping
                            tracing::trace!(
                                "VM: Reached end of skipped branch (id={})",
                                branch_name
                            );
                            self.skip_until_branch_end = None;
                            self.instruction_pointer += 1;
                            return Ok(());
                        }
                    }
                    _ => {
                        // Skip this instruction
                        tracing::trace!(
                            "VM: Skipping instruction (waiting for CondBranchEnd id={}, depth={})",
                            skip_branch_id,
                            skip_flow_depth
                        );
                        self.instruction_pointer += 1;
                        return Ok(());
                    }
                }
            }
            // If current_depth > skip_flow_depth, we're in a nested flow - don't skip
            // This allows nested flows to execute normally
        }

        // Check if we're skipping all remaining branches in a cond/match flow.
        // This skips condition evaluation code AND branch bodies until the EndFlow
        // for the current cond/match flow, using a nesting counter for nested flows.
        if let Some(nested_depth) = self.skip_remaining_cond_flow {
            match instruction {
                Instruction::BeginFlow { .. } => {
                    // Entering a nested flow - increment counter
                    self.skip_remaining_cond_flow = Some(nested_depth + 1);
                    self.instruction_pointer += 1;
                    return Ok(());
                }
                Instruction::EndFlow { .. } => {
                    if nested_depth > 0 {
                        // Leaving a nested flow - decrement counter
                        self.skip_remaining_cond_flow = Some(nested_depth - 1);
                        self.instruction_pointer += 1;
                        return Ok(());
                    } else {
                        // This is our target EndFlow - stop skipping and execute it
                        self.skip_remaining_cond_flow = None;
                        // Fall through to normal instruction execution
                    }
                }
                _ => {
                    // Skip this instruction
                    self.instruction_pointer += 1;
                    return Ok(());
                }
            }
        }

        match instruction {
            Instruction::LoadConst { dest, constant } => {
                let dest = *dest;
                let constant = *constant;
                let value = self.load_constant(constant)?;

                // If we're in a parallel flow and haven't captured metadata yet,
                // this LoadConst should contain the dependency metadata
                if let Some(ref mut parallel_ctx) = self.parallel_flow_context
                    && !parallel_ctx.metadata_captured
                {
                    // Extract dependency levels from the constant value
                    if let Val::Vec(levels_vec) = &value {
                        let mut dependency_levels = Vec::new();
                        for level_val in levels_vec {
                            if let Val::Vec(var_names_vec) = level_val {
                                let var_names: Vec<String> = var_names_vec
                                    .iter()
                                    .filter_map(|v| {
                                        if let Val::Str(s) = v {
                                            Some((**s).to_owned())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                dependency_levels.push(var_names);
                            }
                        }

                        parallel_ctx.dependency_levels = dependency_levels;
                        parallel_ctx.metadata_captured = true;

                        tracing::trace!(
                            "VM: Captured parallel flow dependency metadata: {} levels",
                            parallel_ctx.dependency_levels.len()
                        );
                    }
                }

                // If this is a lambda value, capture the closure variables NOW (at creation time)
                // rather than waiting until execution time.
                // EXCEPTION: If we're inside a parallel flow context, skip variable capture
                // so that thunks can capture variables from previous dependency levels at execution time.
                let in_parallel_flow = self.parallel_flow_context.is_some();

                let value = if let Val::Box(ref boxed) = value {
                    if let Some(lambda_info) = boxed
                        .as_any()
                        .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                    {
                        // Clone and populate the closure environment
                        let mut captured_lambda = lambda_info.clone();
                        captured_lambda.closure_env.clear();

                        for var_name in &captured_lambda.capture_vars {
                            // In parallel flow context, skip variable capture - defer to execution time
                            // This allows thunks to see computed_vars from previous dependency levels
                            if in_parallel_flow {
                                tracing::trace!(
                                    "VM: Deferring capture of '{}' in parallel flow context",
                                    var_name
                                );
                                // Only capture function references, not variables
                                if let Some(core_fn_name) = self.lookup_core_function_name(var_name)
                                {
                                    let fn_ref =
                                        crate::lang::runtime::function_ref::FunctionRef::new(
                                            core_fn_name.clone(),
                                        );
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                    tracing::trace!(
                                        "VM: Captured '{}' as core function reference in parallel context",
                                        var_name
                                    );
                                } else if let Some(function_id) =
                                    self.find_best_function_overload(var_name, &[])
                                    && let Some(fn_info) =
                                        self.program.functions.get(function_id as usize)
                                {
                                    let fn_ref =
                                        crate::lang::runtime::function_ref::FunctionRef::new(
                                            fn_info.name.clone(),
                                        );
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                    tracing::trace!(
                                        "VM: Captured '{}' as function reference in parallel context",
                                        var_name
                                    );
                                }
                                // Variable capture is deferred - will happen at execution time
                                continue;
                            }

                            // Normal (non-parallel) capture path
                            // Try to capture from current lexical scope
                            if let Ok(var_value) = self.lookup_variable(var_name) {
                                captured_lambda
                                    .closure_env
                                    .insert(var_name.clone(), var_value);
                                tracing::trace!(
                                    "VM: Captured '{}' at lambda creation time",
                                    var_name
                                );
                            } else if let Ok(var_value) = self.unified_variable_lookup(var_name) {
                                // Fall back to unified lookup
                                captured_lambda
                                    .closure_env
                                    .insert(var_name.clone(), var_value);
                                tracing::trace!(
                                    "VM: Captured '{}' at lambda creation time (unified)",
                                    var_name
                                );
                            } else if let Some(core_fn_name) =
                                self.lookup_core_function_name(var_name)
                            {
                                // If it's a core function, capture as a function reference
                                let fn_ref = crate::lang::runtime::function_ref::FunctionRef::new(
                                    core_fn_name.clone(),
                                );
                                captured_lambda
                                    .closure_env
                                    .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                if var_name == "concat" {
                                    tracing::info!(
                                        "VM: Captured 'concat' as core function reference '{}' at lambda creation time",
                                        core_fn_name
                                    );
                                } else {
                                    tracing::trace!(
                                        "VM: Captured '{}' as core function reference at lambda creation time",
                                        var_name
                                    );
                                }
                            } else if let Some(function_id) =
                                self.find_best_function_overload(var_name, &[])
                            {
                                // If it's any other user function, capture as a function reference
                                if let Some(fn_info) =
                                    self.program.functions.get(function_id as usize)
                                {
                                    let fn_ref =
                                        crate::lang::runtime::function_ref::FunctionRef::new(
                                            fn_info.name.clone(),
                                        );
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                    tracing::trace!(
                                        "VM: Captured '{}' as function reference at lambda creation time",
                                        var_name
                                    );
                                }
                            } else if var_name == "concat" {
                                // Special debug for concat
                                tracing::error!(
                                    "VM: FAILED to capture 'concat' at lambda creation time! \
                                     core_functions.len()={}, lookup_core_function_name returned None. \
                                     Lambda defined in namespace='{}', capture_vars={:?}",
                                    self.core_functions.len(),
                                    lambda_info.defining_namespace,
                                    captured_lambda.capture_vars
                                );
                                // List ALL entries in core_functions to debug
                                for (cf_name, cf_id) in self.core_functions.iter() {
                                    if cf_name.contains("concat") {
                                        tracing::error!(
                                            "  core_functions contains: {} -> {}",
                                            cf_name,
                                            cf_id
                                        );
                                    }
                                }
                                // If empty, say so
                                if self.core_functions.is_empty() {
                                    tracing::error!("  core_functions IS EMPTY!");
                                }
                            }
                            // If variable not found, leave it out - will error at execution time
                        }

                        Val::Box(Box::new(captured_lambda))
                    } else {
                        value
                    }
                } else {
                    value
                };

                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } => {
                let dest = *dest;
                let function_name = *function_name;
                use crate::lang::runtime::function_ref::function_ref;
                let fn_name_str = self.get_string_constant(function_name)?;
                let fn_ref = function_ref((*fn_name_str).to_owned());
                self.set_register(dest, fn_ref)?;
                self.instruction_pointer += 1;
            }

            Instruction::LoadTypeRef { dest, type_ref } => {
                let dest = *dest;
                let type_ref = *type_ref;
                // Load a TypeRef constant (for variant union types)
                let val = self.load_constant(type_ref)?;
                self.set_register(dest, val)?;
                self.instruction_pointer += 1;
            }

            Instruction::CondBranch { condition, target } => {
                let condition = *condition;
                let target = *target;
                // Legacy conditional branch - for backward compatibility
                let condition_val = self.get_register(condition)?;
                let is_truthy = self.is_truthy(condition_val);

                if is_truthy {
                    // Jump to target
                    self.instruction_pointer = (self.instruction_pointer as i32 + target) as usize;
                } else {
                    // Continue to next instruction
                    self.instruction_pointer += 1;
                }
            }

            Instruction::CondBranchStart {
                branch_name,
                condition,
                skip_target: _,
            } => {
                let branch_name = *branch_name;
                let condition = *condition;
                // Check for cond short-circuiting: if a branch has already executed, skip all remaining branches
                let should_skip_due_to_short_circuit =
                    if let Some(flow_context) = self.flow_contexts.last() {
                        // NOTE: Parallel execution for CondAll would require architectural changes:
                        // The current bytecode structure executes branch bodies inline (sequentially).
                        // To parallelize, we would need to:
                        // 1. Compiler: emit branch bodies as separate function-like units
                        // 2. VM: collect truthy branch IDs during first pass
                        // 3. VM: execute branch bodies in parallel (similar to pmap)
                        // This is a significant refactoring that changes how conditional flows compile.
                        // Current sequential execution is correct and respects all conditions.
                        matches!(flow_context.flow_type, FlowType::Cond | FlowType::Match)
                            && flow_context.cond_branch_executed
                    } else {
                        false
                    };

                if should_skip_due_to_short_circuit {
                    // cond short-circuiting: a branch has already executed, skip this one
                    tracing::trace!(
                        "VM: Skipping branch '{}' due to cond short-circuiting (previous branch already executed)",
                        branch_name
                    );
                    // Track both branch_name and flow depth to prevent cross-flow interference
                    self.skip_until_branch_end = Some((branch_name, self.flow_contexts.len()));
                    self.instruction_pointer += 1;
                } else {
                    // Normal conditional execution: only execute branch if condition is true
                    let condition_val = self.get_register(condition)?;
                    let is_truthy = self.is_truthy(condition_val);
                    let branch_name_str = self.get_string_constant(branch_name)?;

                    tracing::trace!(
                        "VM: CondBranchStart '{}' - condition: {:?}, is_truthy: {}",
                        branch_name_str,
                        condition_val,
                        is_truthy
                    );

                    if is_truthy {
                        // Condition is true, execute the branch
                        tracing::trace!(
                            "VM: Executing branch '{}' (condition is true)",
                            branch_name_str
                        );
                        // Track which branch we're currently in
                        if let Some(flow_context) = self.flow_contexts.last_mut() {
                            flow_context.current_branch = Some(branch_name_str);
                        }
                        self.instruction_pointer += 1;
                    } else {
                        // Condition is false, skip to the end of this branch
                        tracing::trace!(
                            "VM: Skipping branch '{}' (condition is false)",
                            branch_name_str
                        );

                        // We need to skip this branch, but we can't easily jump to the end
                        // because we don't know which instruction list we're executing from.
                        // Instead, we'll set a flag to skip execution until we reach the matching CondBranchEnd
                        // Track both branch_name and flow depth to prevent cross-flow interference
                        self.skip_until_branch_end = Some((branch_name, self.flow_contexts.len()));
                        self.instruction_pointer += 1;
                    }
                }
            }

            Instruction::CondBranchEnd {
                branch_name,
                result,
            } => {
                let branch_name = *branch_name;
                let result = *result;
                // End of conditional branch - collect result (only reached if branch was executed)
                let result_val = self.get_register(result)?.clone();
                let branch_name_str = self.get_string_constant(branch_name)?;

                tracing::trace!(
                    "VM: CondBranchEnd '{}' - collecting result: {:?}",
                    branch_name_str,
                    result_val
                );

                // Store the branch result in the flow context
                // CRITICAL: Only store if this CondBranchEnd belongs to the current flow.
                // Check that current_branch matches (set by CondBranchStart when branch executes).
                // This prevents inner functions' CondBranchEnd from storing into outer FlowContexts.
                let should_store = if let Some(flow_context) = self.flow_contexts.last() {
                    if let Some(ref current) = flow_context.current_branch {
                        current == &branch_name_str
                    } else {
                        // current_branch is None - this flow's branch may have already ended
                        // or we're in a default branch scenario. Allow storage.
                        true
                    }
                } else {
                    false
                };

                if !should_store {
                    // This CondBranchEnd is from an inner function, not the current flow
                    // Skip it - the inner function's flow already stored its result
                    self.instruction_pointer += 1;
                } else if let Some(flow_context) = self.flow_contexts.last_mut() {
                    flow_context
                        .cond_branch_results
                        .push((branch_name_str, result_val));

                    // Clear current branch tracking
                    flow_context.current_branch = None;

                    // Mark that a branch has executed (for cond/match short-circuiting)
                    if matches!(flow_context.flow_type, FlowType::Cond | FlowType::Match) {
                        flow_context.cond_branch_executed = true;
                        // Skip all remaining branches including their condition evaluation code.
                        // The nesting counter (0) means the next EndFlow at this level is ours.
                        self.skip_remaining_cond_flow = Some(0);
                        tracing::trace!(
                            "VM: cond/match flow - marking branch as executed, skipping remaining branches"
                        );
                    }
                }

                self.instruction_pointer += 1;
            }

            Instruction::Pipe { dest, src } => {
                let dest = *dest;
                let src = *src;
                // Pipe operation for pipe flows
                let value = self.get_register(src)?.clone();

                // Track pipe step results for result modifiers
                if let Some(flow_context) = self.flow_contexts.last_mut()
                    && flow_context.flow_type == FlowType::Pipe
                {
                    let step_name = format!("$pipe_{}", flow_context.pipe_step_results.len());
                    tracing::trace!("VM: Tracking pipe step '{}' = {:?}", step_name, value);
                    flow_context
                        .pipe_step_results
                        .push((step_name, value.clone()));
                }

                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::BeginFlow {
                flow_type,
                result_modifier,
                source,
            } => {
                let flow_type = *flow_type;
                let result_modifier = *result_modifier;
                let source = source.clone();
                // Begin a flow execution context
                tracing::trace!(
                    "VM: Beginning flow execution (type: {:?}, result_modifier: {:?})",
                    flow_type,
                    result_modifier
                );

                // Fast path: For Cond/Match flows without emitter, skip UUID generation
                // and use a nil UUID since flow_id is only needed for observability
                let flow_id = if self.emitter.is_none()
                    && matches!(
                        flow_type,
                        FlowType::Cond | FlowType::Match | FlowType::Serial
                    ) {
                    Uuid::nil()
                } else {
                    Uuid::now_v7()
                };

                // For parallel flows: Initialize parallel execution context
                if flow_type == FlowType::Parallel {
                    tracing::trace!("VM: Initializing TRUE parallel flow execution context");

                    // Initialize parallel flow context
                    // The next instruction should be LoadConst with dependency metadata
                    self.parallel_flow_context = Some(ParallelFlowContext {
                        deferred_thunks: Vec::new(),
                        dependency_levels: Vec::new(),
                        metadata_captured: false,
                    });
                }

                // For non-serial inline flows, emit a call:start event to track as a call record
                // Serial flows are the default and don't need tracking
                let (call_id, start_time) = if flow_type != FlowType::Serial {
                    if let (Some(emitter), Some(execution_context)) =
                        (&self.emitter, &self.execution_context)
                    {
                        let call_id = Uuid::now_v7();
                        let start_time = chrono::Utc::now();
                        let parent_call_id = self.call_stack.last().map(|ctx| ctx.call_id);
                        let call_depth = self.call_stack.len();
                        let runtime_path = self.build_runtime_path();

                        // Synthetic function name for inline flow. The outer
                        // `if flow_type != FlowType::Serial` makes Serial logically
                        // impossible here, but we use a string fallback rather than
                        // `unreachable!()` so a future refactor that changes the
                        // outer guard can't take the worker down.
                        let function_name = match flow_type {
                            FlowType::Serial => "<serial>".to_string(),
                            FlowType::Parallel => "<parallel>".to_string(),
                            FlowType::Cond => "<cond>".to_string(),
                            FlowType::CondAll => "<cond-all>".to_string(),
                            FlowType::Pipe => "<pipe>".to_string(),
                            FlowType::Match => "<match>".to_string(),
                            FlowType::MatchAll => "<match-all>".to_string(),
                        };

                        // Build flow info with inline marker
                        let flow_type_str = match flow_type {
                            FlowType::Serial => "serial",
                            FlowType::Parallel => "parallel",
                            FlowType::Cond => "cond",
                            FlowType::CondAll => "cond-all",
                            FlowType::Pipe => "pipe",
                            FlowType::Match => "match",
                            FlowType::MatchAll => "match-all",
                        };
                        let mut flow_map = IndexMap::new();
                        flow_map.insert(Val::from("type"), Val::from(flow_type_str.to_string()));
                        flow_map.insert(Val::from("flow_id"), Val::from(flow_id.to_string()));
                        flow_map.insert(Val::from("inline"), Val::Bool(true));

                        let start_event = crate::lang::emitter::EngineEvent::call_start(
                            execution_context,
                            call_id,
                            parent_call_id,
                            function_name.clone(),
                            function_name, // static_scope same as function_name for inline flows
                            runtime_path
                                .unwrap_or_else(|| format!("run_{}", execution_context.run_id)),
                            call_depth,
                            vec![], // No args for inline flows
                            source.as_ref(),
                            start_time,
                            Some(Val::Map(Box::new(flow_map))),
                        );
                        emitter.emit(start_event);

                        (Some(call_id), Some(start_time))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };

                let flow_context = FlowContext {
                    flow_id,
                    call_id,
                    start_time,
                    flow_type,
                    _result_modifier: result_modifier,
                    _flow_variables: Vec::new(),
                    flow_variable_refs: Vec::new(),

                    cond_branch_results: Vec::new(),
                    cond_branch_executed: false,
                    current_branch: None,
                    pipe_step_results: Vec::new(),
                };

                self.flow_contexts.push(flow_context);
                self.instruction_pointer += 1;
            }

            Instruction::EndFlow { dest } => {
                let dest = *dest;
                // Check if we're ending a parallel flow with deferred thunks
                if let Some(parallel_ctx) = self.parallel_flow_context.take() {
                    tracing::trace!(
                        "VM: EndFlow for parallel flow - executing {} deferred thunks in {} levels",
                        parallel_ctx.deferred_thunks.len(),
                        parallel_ctx.dependency_levels.len()
                    );

                    // Execute deferred thunks in TRUE parallel by dependency level
                    self.execute_parallel_flow(parallel_ctx)?;
                }

                // End flow execution and collect results
                if let Some(flow_context) = self.flow_contexts.pop() {
                    // For Cond/Match flows (short-circuiting) with |one modifier and no matching
                    // conditional branches, the default branch (if any) has already stored its
                    // result in `dest`. In this case, we should NOT overwrite it with Null.
                    // This doesn't apply to CondAll/MatchAll (non-short-circuiting) or other modifiers.
                    let should_preserve_existing =
                        matches!(flow_context.flow_type, FlowType::Cond | FlowType::Match)
                            && matches!(
                                flow_context._result_modifier,
                                crate::lang::bytecode::FlowResultModifier::One
                            )
                            && flow_context.cond_branch_results.is_empty();

                    let result = if should_preserve_existing {
                        // Keep the existing value in dest (from default branch)
                        self.get_register(dest)?.clone()
                    } else {
                        self.collect_flow_result(&flow_context)?
                    };

                    // Emit call:stop event for inline flows (those with a call_id)
                    if let Some(call_id) = flow_context.call_id
                        && let (Some(emitter), Some(execution_context)) =
                            (&self.emitter, &self.execution_context)
                    {
                        let end_time = chrono::Utc::now();
                        let duration_us = flow_context
                            .start_time
                            .map(|start| (end_time - start).num_microseconds().unwrap_or(0))
                            .unwrap_or(0);

                        let stop_event = crate::lang::emitter::EngineEvent::call_stop(
                            execution_context,
                            call_id,
                            Some(result.clone()),
                            None, // No error
                            end_time,
                            duration_us,
                        );
                        emitter.emit(stop_event);
                    }

                    self.set_register(dest, result)?;
                } else {
                    return Err(VmError::runtime("No flow context to end".to_string()));
                }
                self.instruction_pointer += 1;
            }

            Instruction::DefineFunction { dest, function_id } => {
                let dest = *dest;
                let function_id = *function_id;
                // Create a user-defined function value
                let function_value = self.create_user_function_value(&function_id.to_string());
                self.set_register(dest, function_value)?;
                self.instruction_pointer += 1;
            }

            Instruction::Move { dest, src } => {
                let dest = *dest;
                let src = *src;
                let value = self.get_register(src)?.clone();
                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::Add { dest, left, right } => {
                let dest = *dest;
                let left = *left;
                let right = *right;
                let left_val = self.get_register(left)?;
                let right_val = self.get_register(right)?;
                let result = match (left_val, right_val) {
                    (Val::Int(a), Val::Int(b)) => crate::lang::hot::math::fast_add_int(*a, *b),
                    _ => self.add_values(left_val, right_val)?,
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::Eq { dest, left, right } => {
                let dest = *dest;
                let left = *left;
                let right = *right;
                let left_val = self.get_register(left)?;
                let right_val = self.get_register(right)?;
                let result = Val::Bool(left_val == right_val);
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::StrEndsWith {
                dest,
                string,
                suffix,
            } => {
                let dest = *dest;
                let string = *string;
                let suffix = *suffix;
                let string_val = self.get_register(string)?;
                let suffix_val = self.get_register(suffix)?;
                let result = match (&string_val, &suffix_val) {
                    (Val::Str(s), Val::Str(suffix_s)) => Val::Bool(s.ends_with(&**suffix_s)),
                    _ => Val::Bool(false),
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::StrStartsWith {
                dest,
                string,
                prefix,
            } => {
                let dest = *dest;
                let string = *string;
                let prefix = *prefix;
                let string_val = self.get_register(string)?;
                let prefix_val = self.get_register(prefix)?;
                let result = match (&string_val, &prefix_val) {
                    (Val::Str(s), Val::Str(prefix_s)) => Val::Bool(s.starts_with(&**prefix_s)),
                    _ => Val::Bool(false),
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::Jump { offset } => {
                let offset = *offset;
                // Unconditional jump - offset is absolute instruction address
                self.instruction_pointer = offset as usize;
            }

            Instruction::JumpIf { condition, offset } => {
                let condition = *condition;
                let offset = *offset;
                let cond_val = self.get_register(condition)?;
                if cond_val.is_truthy() {
                    self.instruction_pointer = offset as usize;
                } else {
                    self.instruction_pointer += 1;
                }
            }

            Instruction::JumpIfNot { condition, offset } => {
                let condition = *condition;
                let offset = *offset;
                let cond_val = self.get_register(condition)?;
                if !cond_val.is_truthy() {
                    self.instruction_pointer = offset as usize;
                } else {
                    self.instruction_pointer += 1;
                }
            }

            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let dest = *dest;
                let function = *function;
                let args_start = *args_start;
                let args_count = *args_count;
                tracing::trace!(
                    "VM: Call instruction - dest: {}, function: {}, args_start: {}, args_count: {}",
                    dest,
                    function,
                    args_start,
                    args_count
                );

                // Collect arguments from consecutive registers and unwrap Results if they are "ok"
                let mut args = Vec::with_capacity(args_count as usize);
                for i in 0..args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    tracing::trace!("VM: Call arg {}: {:?}", i, arg_val);
                    args.push(self.prepare_call_arg(arg_val)?);
                }

                // Get function information from the program
                tracing::trace!(
                    "VM: About to call execute_function_call with function register: {}",
                    function
                );
                let result = self.execute_function_call(function, &args)?;
                tracing::trace!("VM: Call result: {:?}", result);
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::CallWithSpread {
                dest,
                function,
                args_start,
                args_count,
                spread_mask,
            } => {
                let dest = *dest;
                let function = *function;
                let args_start = *args_start;
                let args_count = *args_count;
                let spread_mask = *spread_mask;
                tracing::trace!(
                    "VM: CallWithSpread instruction - dest: {}, function: {}, args_start: {}, args_count: {}, spread_mask: {:b}",
                    dest,
                    function,
                    args_start,
                    args_count,
                    spread_mask
                );

                // Collect arguments, spreading Vecs where indicated by spread_mask
                let mut args = Vec::with_capacity(args_count as usize);
                for i in 0..args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    tracing::trace!("VM: CallWithSpread arg {}: {:?}", i, arg_val);

                    // Automatically unwrap "ok" Results for function arguments
                    let unwrapped_arg = self.unwrap_result_if_ok(&arg_val)?;
                    // Evaluate boxed AstNodes in the argument (e.g., in maps/vectors)
                    let resolved_arg = self.resolve_variable_references_in_val(&unwrapped_arg)?;

                    // Check if this argument should be spread
                    if (spread_mask >> i) & 1 == 1 {
                        // Spread this argument - it should be a Vec
                        match &resolved_arg {
                            Val::Vec(elements) => {
                                tracing::trace!(
                                    "VM: Spreading {} elements from arg {}",
                                    elements.len(),
                                    i
                                );
                                for elem in elements {
                                    args.push(elem.clone());
                                }
                            }
                            _ => {
                                return Err(VmError::runtime(format!(
                                    "Cannot spread non-vector value: {:?}",
                                    resolved_arg
                                )));
                            }
                        }
                    } else {
                        args.push(resolved_arg);
                    }
                }

                tracing::trace!("VM: CallWithSpread final args count: {}", args.len());

                // Execute the function call - `function` is a register like in `Call`
                let result = self.execute_function_call(function, &args)?;
                tracing::trace!("VM: CallWithSpread result: {:?}", result);
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::CallNative {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let dest = *dest;
                let function = *function;
                let args_start = *args_start;
                let args_count = *args_count;
                // Collect arguments from consecutive registers and unwrap Results if they are "ok"
                let mut args = Vec::with_capacity(args_count as usize);
                for i in 0..args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    // Automatically unwrap "ok" Results for function arguments
                    let unwrapped_arg = self.unwrap_result_if_ok(&arg_val)?;
                    // Evaluate boxed AstNodes in the argument (e.g., in maps/vectors)
                    let resolved_arg = self.resolve_variable_references_in_val(&unwrapped_arg)?;
                    args.push(resolved_arg);
                }

                // Get function information to determine if this is a variable lookup
                let function_info =
                    self.program
                        .functions
                        .get(function as usize)
                        .ok_or_else(|| {
                            VmError::runtime(format!("Invalid function ID: {}", function))
                        })?;

                let namespace = function_info.namespace.clone();
                let name = function_info.name.clone();

                // This is a direct native function call
                let function_name = format!("{}/{}", namespace, name);
                let result = match self.call_hotlib_function(&function_name, &args) {
                    Ok(v) => v,
                    Err(e) => {
                        if self.error_capture_active {
                            let mut m = indexmap::IndexMap::new();
                            m.insert(Val::from("error"), Val::from(e.to_string()));
                            Val::Map(Box::new(m))
                        } else {
                            return Err(e);
                        }
                    }
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::CallLambda {
                dest,
                lambda,
                args_start,
                args_count,
            } => {
                let dest = *dest;
                let lambda = *lambda;
                let args_start = *args_start;
                let args_count = *args_count;
                // Get the lambda from the register
                let lambda_val = self.get_register(lambda)?.clone();

                // Collect arguments from consecutive registers (no lazy metadata for lambdas yet)
                let mut args = Vec::with_capacity(args_count as usize);
                for i in 0..args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    // Automatically unwrap "ok" Results for function arguments
                    let unwrapped_arg = self.unwrap_result_if_ok(&arg_val)?;
                    // Evaluate boxed AstNodes in the argument (e.g., in maps/vectors)
                    let resolved_arg = self.resolve_variable_references_in_val(&unwrapped_arg)?;
                    args.push(resolved_arg);
                }

                // Execute the lambda
                let result = match self.execute_lambda(&lambda_val, &args) {
                    Ok(v) => v,
                    Err(e) => {
                        if self.error_capture_active {
                            let mut m = indexmap::IndexMap::new();
                            m.insert(Val::from("error"), Val::from(e.to_string()));
                            Val::Map(Box::new(m))
                        } else {
                            return Err(e);
                        }
                    }
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::CallUserFunction {
                dest,
                function_id,
                args_start,
                args_count,
            } => {
                let dest = *dest;
                let function_id = *function_id;
                let args_start = *args_start;
                let args_count = *args_count;
                tracing::trace!(
                    "VM: Executing CallUserFunction instruction (function_id: {}, args_count: {})",
                    function_id,
                    args_count
                );

                // Collect arguments from consecutive registers
                let mut args = Vec::with_capacity(args_count as usize);
                for i in 0..args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    // Automatically unwrap "ok" Results for function arguments
                    let unwrapped_arg = self.unwrap_result_if_ok(&arg_val)?;
                    // Evaluate boxed AstNodes in the argument (e.g., in maps/vectors)
                    let resolved_arg = self.resolve_variable_references_in_val(&unwrapped_arg)?;
                    args.push(resolved_arg);
                }

                // Implicit coercion at the static-dispatch boundary. The
                // compiler may have picked `function_id` from the arity-only
                // key (because compile-time arg types weren't known); if a
                // typed signature exists and the actual arg types differ but
                // a unique `Source -> Target` arrow exists, coerce in place.
                // The type checker has already vetted the call, so any miss
                // here just falls back to invoking the function with the
                // original args (preserves prior behavior).
                if let Some(expected) = self.lookup_typed_param_signature(function_id) {
                    let actual: Vec<String> =
                        args.iter().map(|a| self.get_val_type_name(a)).collect();
                    if actual != expected {
                        let mut plan: Vec<Option<String>> = Vec::with_capacity(args_count as usize);
                        let mut all_ok = true;
                        for (a, e) in actual.iter().zip(expected.iter()) {
                            if a == e {
                                plan.push(None);
                            } else if let Some(target) = self.find_coercion_target(a, e) {
                                plan.push(Some(target));
                            } else {
                                all_ok = false;
                                break;
                            }
                        }
                        if all_ok {
                            args = self.apply_coercion_plan(&args, &plan)?;
                        }
                    }
                }

                // Execute the user function directly
                let result = match self.execute_user_function(function_id, &args) {
                    Ok(v) => v,
                    Err(e) => {
                        if self.error_capture_active {
                            let mut m = indexmap::IndexMap::new();
                            m.insert(Val::from("error"), Val::from(e.to_string()));
                            Val::Map(Box::new(m))
                        } else {
                            return Err(e);
                        }
                    }
                };
                tracing::trace!("VM: CallUserFunction result: {:?}", result);
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::TemplateInterpolate {
                dest,
                parts_start,
                parts_count,
            } => {
                let dest = *dest;
                let parts_start = *parts_start;
                let parts_count = *parts_count;
                // Check for template literal recursion
                if self.template_recursion_depth > 15 {
                    tracing::warn!(
                        "Template recursion limit hit at depth {}",
                        self.template_recursion_depth
                    );
                    self.set_register(dest, Val::from("<template-recursion>"))?;
                    self.instruction_pointer += 1;
                    return Ok(());
                }

                // Execute template literal interpolation
                let mut result = String::new();
                let max_template_bytes = crate::lang::runtime::limits::max_string_bytes();

                // Temporarily increment recursion depth
                self.template_recursion_depth += 1;

                for i in 0..parts_count {
                    let part_reg = parts_start + i as RegisterId;
                    let part_val = self.get_register(part_reg)?.clone();

                    // Automatically unwrap Results in template literals
                    let unwrapped_val = self.unwrap_result_if_ok(&part_val)?;

                    // Convert value to string using Str constructor (supports type implementations)
                    let string_repr: String =
                        match crate::lang::hot::r#type::str_constructor(self, &[unwrapped_val]) {
                            crate::lang::hot::r#type::HotResult::Ok(Val::Str(s)) => (*s).to_owned(),
                            crate::lang::hot::r#type::HotResult::Ok(other) => other.to_string(),
                            crate::lang::hot::r#type::HotResult::Err(e) => e.to_string(),
                        };
                    // Bound the produced template length up-front using checked
                    // arithmetic so a single template can't drive an unbounded
                    // String allocation (which would call the allocator and
                    // potentially abort on OOM).
                    let projected_len =
                        result.len().checked_add(string_repr.len()).ok_or_else(|| {
                            VmError::runtime("Template literal length overflows usize".to_string())
                        })?;
                    if projected_len > max_template_bytes {
                        // Restore recursion depth before bailing out.
                        self.template_recursion_depth -= 1;
                        return Err(VmError::runtime(format!(
                            "Template literal too large: would produce {} bytes (limit {}). \
                             Raise HOT_MAX_STRING_BYTES if this is intentional.",
                            projected_len, max_template_bytes
                        )));
                    }
                    if let Err(e) = crate::lang::runtime::limits::try_reserve_string(
                        "template literal",
                        &mut result,
                        string_repr.len(),
                    ) {
                        self.template_recursion_depth -= 1;
                        return Err(VmError::runtime(e.to_string()));
                    }
                    result.push_str(&string_repr);
                }

                // Restore recursion depth
                self.template_recursion_depth -= 1;

                self.set_register(dest, Val::from(result))?;
                self.instruction_pointer += 1;
            }

            Instruction::LoadVar { dest, var_name } => {
                let dest = *dest;
                let var_name = *var_name;
                let name = self.get_constant_string(var_name)?;
                tracing::trace!(
                    "VM: LoadVar trying to load variable '{}' (constant ID {})",
                    name,
                    var_name
                );
                let value = self.lookup_variable(&name)?;

                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::LoadVarOrDefault {
                dest,
                var_name,
                default_value,
            } => {
                let dest = *dest;
                let var_name = *var_name;
                let default_value = *default_value;
                let name = self.get_constant_string(var_name)?;
                tracing::trace!(
                    "VM: LoadVarOrDefault trying to load variable '{}' (constant ID {})",
                    name,
                    var_name
                );

                // Try to load the variable, but use default if it doesn't exist
                let value = match self.lookup_variable(&name) {
                    Ok(val) => val,
                    Err(_) => {
                        // Variable doesn't exist, use default value
                        self.load_constant(default_value)?
                    }
                };

                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::StoreVar {
                var_name,
                value,
                metadata: _,
            } => {
                let var_name = *var_name;
                let value = *value;
                let name = self.get_constant_string(var_name)?;
                let val = self.get_register(value)?.clone();

                tracing::trace!("VM StoreVar - storing '{}' = {:?}", name, val);

                // Track variable assignments in flow contexts for result modifiers
                if let Some(flow_context) = self.flow_contexts.last_mut() {
                    // Only track for serial/parallel flows (all have result modifiers when using BeginFlow/EndFlow)
                    if matches!(
                        flow_context.flow_type,
                        FlowType::Serial | FlowType::Parallel
                    ) {
                        tracing::trace!(
                            "VM: Tracking variable assignment '{}' = {:?} in flow",
                            name,
                            val
                        );
                        flow_context
                            .flow_variable_refs
                            .push((name.to_string(), val.clone()));
                    }
                }

                self.store_variable(&name, val.clone())?;

                self.instruction_pointer += 1;
            }

            // Deferred variable expression for parallel flow execution
            Instruction::DeferredVarExpr { var_name, thunk } => {
                let var_name = *var_name;
                let thunk = *thunk;
                let name = self.get_constant_string(var_name)?;
                let thunk_val = self.get_register(thunk)?.clone();

                tracing::trace!(
                    "VM: DeferredVarExpr - buffering thunk for '{}' for parallel execution",
                    name
                );

                // Buffer the thunk for parallel execution at EndFlow
                if let Some(ref mut parallel_ctx) = self.parallel_flow_context {
                    parallel_ctx.deferred_thunks.push(DeferredVarThunk {
                        var_name: name.to_string(),
                        thunk: thunk_val,
                    });
                } else {
                    // If not in parallel context, evaluate immediately (fallback)
                    tracing::warn!(
                        "VM: DeferredVarExpr outside parallel flow, evaluating immediately"
                    );
                    let result = self.execute_thunk(&thunk_val)?;
                    self.store_variable(&name, result.clone())?;

                    // Track in flow context if present
                    if let Some(flow_context) = self.flow_contexts.last_mut() {
                        flow_context
                            .flow_variable_refs
                            .push((name.to_string(), result));
                    }
                }

                self.instruction_pointer += 1;
            }

            Instruction::SetNamespace { namespace } => {
                let namespace = *namespace;
                let namespace_name = self.get_constant_string(namespace)?;
                tracing::trace!(
                    "VM: Setting current namespace from '{}' to '{}'",
                    self.current_namespace,
                    namespace_name
                );
                self.current_namespace = namespace_name.to_string();

                // Ensure the new namespace has a ns variable pointing to itself
                self.ensure_namespace_has_ns_variable(&namespace_name)?;

                self.instruction_pointer += 1;
            }

            Instruction::StoreGlobal {
                namespace,
                var_name,
                value,
            } => {
                let namespace = *namespace;
                let var_name = *var_name;
                let value = *value;
                let namespace_name = self.get_constant_string(namespace)?;
                let name = self.get_constant_string(var_name)?;
                let val = self.get_register(value)?.clone();

                tracing::trace!(
                    "VM StoreGlobal - storing '{}' in namespace '{}'",
                    name,
                    namespace_name
                );

                // Store directly in the specified namespace
                self.namespace_variables
                    .entry(namespace_name.to_string())
                    .or_default()
                    .insert(name.to_string(), val);

                self.instruction_pointer += 1;
            }

            Instruction::LoadGlobal {
                dest,
                namespace,
                var_name,
            } => {
                let dest = *dest;
                let namespace = *namespace;
                let var_name = *var_name;
                let namespace_name = self.get_constant_string(namespace)?;
                let name = self.get_constant_string(var_name)?;

                let value = self
                    .namespace_variables
                    .get(namespace_name.as_ref() as &str)
                    .and_then(|vars| vars.get(name.as_ref() as &str))
                    .cloned()
                    .unwrap_or(Val::Null);

                tracing::trace!(
                    "VM LoadGlobal - loaded '{}' from namespace '{}': {:?}",
                    name,
                    namespace_name,
                    value
                );

                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::LoadScoped {
                dest,
                var_name,
                scope_depth,
            } => {
                let dest = *dest;
                let var_name = *var_name;
                let scope_depth = *scope_depth;
                let name = self.get_constant_string(var_name)?;
                let value = self
                    .lookup_variable_at_depth(&name, scope_depth as usize)
                    .ok_or_else(|| {
                        VmError::runtime(format!(
                            "Variable '{}' not found at depth {}",
                            name, scope_depth
                        ))
                    })?;
                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::StoreScoped {
                var_name,
                value,
                scope_depth,
            } => {
                let var_name = *var_name;
                let value = *value;
                let scope_depth = *scope_depth;
                let name = self.get_constant_string(var_name)?;
                let val = self.get_register(value)?.clone();
                self.store_variable_at_depth(&name, val, scope_depth as usize)?;
                self.instruction_pointer += 1;
            }

            Instruction::EnterScope { scope_type } => {
                let scope_type = *scope_type;
                let parent_depth = Some(self.scope_stack.len() - 1);
                self.scope_stack
                    .push(LexicalScope::new(scope_type, parent_depth));
                self.instruction_pointer += 1;
            }

            Instruction::ExitScope => {
                if self.scope_stack.len() > 1 {
                    // Keep at least the global scope
                    self.scope_stack.pop();
                }
                self.instruction_pointer += 1;
            }

            Instruction::CaptureVar {
                dest,
                var_name,
                scope_depth,
            } => {
                let dest = *dest;
                let var_name = *var_name;
                let scope_depth = *scope_depth;
                let name = self.get_constant_string(var_name)?;
                let value = self
                    .lookup_variable_at_depth(&name, scope_depth as usize)
                    .ok_or_else(|| {
                        VmError::runtime(format!(
                            "Variable '{}' not found at depth {}",
                            name, scope_depth
                        ))
                    })?;
                self.set_register(dest, value)?;
                self.instruction_pointer += 1;
            }

            Instruction::DotAccess {
                dest,
                object,
                property,
            } => {
                let dest = *dest;
                let object = *object;
                let property = *property;
                // Execute dot access (property access)
                let object_val = self.get_register(object)?.clone();
                let property_name = self.get_constant_string(property)?;

                let result = self.access_property(&object_val, &property_name)?;
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::DotAccessOrDefault {
                dest,
                object,
                property,
                default_value,
            } => {
                let dest = *dest;
                let object = *object;
                let property = *property;
                let default_value = *default_value;
                // Execute dot access with default value if property doesn't exist
                let object_val = self.get_register(object)?.clone();
                let property_name = self.get_constant_string(property)?;

                let result = match self.access_property(&object_val, &property_name) {
                    Ok(val) => val,
                    Err(_) => {
                        // Property doesn't exist, use default value
                        self.load_constant(default_value)?
                    }
                };

                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::DynamicDotAccess {
                dest,
                object,
                property,
            } => {
                let dest = *dest;
                let object = *object;
                let property = *property;
                // Execute dynamic dot access (property access with runtime key/index)
                let object_val = self.get_register(object)?.clone();
                let property_val = self.get_register(property)?.clone();

                let result = match &object_val {
                    Val::Map(m) => {
                        // For maps, use the property value as the key directly
                        match m.get(&property_val) {
                            Some(v) => v.clone(),
                            None => Val::Null,
                        }
                    }
                    Val::Vec(v) => {
                        // For vectors, property must be an integer index
                        match &property_val {
                            Val::Int(i) => {
                                if *i < 0 {
                                    Val::Null
                                } else {
                                    v.get(*i as usize).cloned().unwrap_or(Val::Null)
                                }
                            }
                            _ => {
                                return Err(VmError::runtime(format!(
                                    "Vector index must be an integer, got {:?}",
                                    property_val
                                )));
                            }
                        }
                    }
                    Val::Str(s) => {
                        // For strings, property must be an integer index
                        match &property_val {
                            Val::Int(i) => {
                                if *i < 0 {
                                    Val::Null
                                } else {
                                    s.chars()
                                        .nth(*i as usize)
                                        .map(|c| Val::from(c.to_string()))
                                        .unwrap_or(Val::Null)
                                }
                            }
                            _ => {
                                return Err(VmError::runtime(format!(
                                    "String index must be an integer, got {:?}",
                                    property_val
                                )));
                            }
                        }
                    }
                    Val::Bytes(b) => {
                        // For bytes, property must be an integer index
                        match &property_val {
                            Val::Int(i) => {
                                if *i < 0 {
                                    Val::Null
                                } else {
                                    b.get(*i as usize)
                                        .map(|byte| Val::Int(*byte as i64))
                                        .unwrap_or(Val::Null)
                                }
                            }
                            _ => {
                                return Err(VmError::runtime(format!(
                                    "Bytes index must be an integer, got {:?}",
                                    property_val
                                )));
                            }
                        }
                    }
                    _ => {
                        return Err(VmError::runtime(format!(
                            "Cannot access property on {:?}",
                            object_val
                        )));
                    }
                };

                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::DotSet {
                object,
                property,
                value,
            } => {
                let object = *object;
                let property = *property;
                let value = *value;
                // Execute dot assignment (property setting)
                let mut object_val = self.get_register(object)?.clone();
                let property_name = self.get_constant_string(property)?;
                let new_value = self.get_register(value)?.clone();

                // Set the property on the object
                self.set_property(&mut object_val, &property_name, new_value)?;

                // Store the modified object back to the register
                self.set_register(object, object_val)?;
                self.instruction_pointer += 1;
            }

            Instruction::DynamicDotSet {
                object,
                property,
                value,
            } => {
                let object = *object;
                let property = *property;
                let value = *value;
                // Execute dynamic dot assignment (property setting with runtime key/index)
                let mut object_val = self.get_register(object)?.clone();
                let property_val = self.get_register(property)?.clone();
                let new_value = self.get_register(value)?.clone();

                match &mut object_val {
                    Val::Map(m) => {
                        // For maps, use the property value as the key directly
                        m.insert(property_val, new_value);
                    }
                    Val::Vec(v) => {
                        // For vectors, property must be an integer index
                        match &property_val {
                            Val::Int(i) => {
                                if *i < 0 {
                                    return Err(VmError::runtime(format!(
                                        "Vector index cannot be negative: {}",
                                        i
                                    )));
                                }
                                let idx = usize::try_from(*i).map_err(|_| {
                                    VmError::runtime(format!(
                                        "Vector index {} is too large to address",
                                        i
                                    ))
                                })?;
                                // Growing the vector to `idx + 1` is a user-controlled
                                // allocation, so gate it on the configured collection
                                // size limit and use try_reserve to convert allocator
                                // failure into a recoverable runtime error.
                                if idx >= v.len() {
                                    let new_len = idx.checked_add(1).ok_or_else(|| {
                                        VmError::runtime(format!(
                                            "Vector index {} would overflow length",
                                            i
                                        ))
                                    })?;
                                    if let Err(e) =
                                        crate::lang::runtime::limits::check_collection_size(
                                            "dynamic vector assign",
                                            new_len,
                                        )
                                    {
                                        return Err(VmError::runtime(e.to_string()));
                                    }
                                    let additional = new_len - v.len();
                                    if let Err(e) = crate::lang::runtime::limits::try_reserve_vec(
                                        "dynamic vector assign",
                                        v,
                                        additional,
                                    ) {
                                        return Err(VmError::runtime(e.to_string()));
                                    }
                                    v.resize(new_len, Val::Null);
                                }
                                v[idx] = new_value;
                            }
                            _ => {
                                return Err(VmError::runtime(format!(
                                    "Vector index must be an integer, got {:?}",
                                    property_val
                                )));
                            }
                        }
                    }
                    _ => {
                        return Err(VmError::runtime(format!(
                            "Cannot set property on {:?}",
                            object_val
                        )));
                    }
                }

                // Store the modified object back to the register
                self.set_register(object, object_val)?;
                self.instruction_pointer += 1;
            }

            Instruction::VecAppend { vec, value } => {
                let vec = *vec;
                let value = *value;
                // Append value to vector (items[] = value syntax)
                let mut vec_val = self.get_register(vec)?.clone();
                let new_value = self.get_register(value)?.clone();

                match &mut vec_val {
                    Val::Vec(v) => {
                        v.push(new_value);
                    }
                    Val::Null => {
                        // If the vector doesn't exist yet, create it with the value
                        vec_val = Val::Vec(vec![new_value]);
                    }
                    _ => {
                        return Err(VmError::runtime(format!(
                            "Cannot append to {:?}, expected a vector",
                            vec_val
                        )));
                    }
                }

                // Store the modified vector back to the register
                self.set_register(vec, vec_val)?;
                self.instruction_pointer += 1;
            }

            Instruction::GetTypePath { dest, value } => {
                let dest = *dest;
                let value = *value;
                // Get the type path for any value (including primitives)
                let val = self.get_register(value)?.clone();
                let type_path = self.get_value_type_path(&val);
                self.set_register(dest, Val::from(type_path))?;
                self.instruction_pointer += 1;
            }

            Instruction::IsType {
                dest,
                value,
                type_path,
            } => {
                let dest = *dest;
                let value = *value;
                let type_path = *type_path;
                // Unified type checking for match and is-type function
                // Handles primitives, custom types, and enum variants
                // No auto-unwrapping - Result.Err values are checked directly
                let val = self.get_register(value)?.clone();
                let expected_type_path = match self.get_register(type_path)? {
                    Val::Str(s) => (**s).to_owned(),
                    other => {
                        return Err(VmError::runtime(format!(
                            "IsType expected string type path, got {:?}",
                            other
                        )));
                    }
                };

                // Get the actual type path of the value
                let actual_type_path = self.get_value_type_path(&val);

                // Check for match:
                // 1. Exact match: actual == expected (e.g., "Int" == "Int")
                // 2. Type-level match: actual starts with expected + "."
                //    (e.g., "Result.Ok" starts with "Result.")
                let is_match = actual_type_path == expected_type_path
                    || actual_type_path.starts_with(&format!("{}.", expected_type_path));

                self.set_register(dest, Val::Bool(is_match))?;
                self.instruction_pointer += 1;
            }

            Instruction::EnsureResult { dest, value } => {
                let dest = *dest;
                let value = *value;
                let val = self.get_register(value)?.clone();

                // If the value is a lazy thunk (LambdaInfo), force-evaluate it
                // with Result auto-unwrapping suppressed so the Result stays intact.
                // This handles `fn match (r: Result) { Result.Ok => ... Result.Err => ... }`
                // where the parameter was marked lazy to preserve the Result through
                // the call boundary.
                let val = if let Val::Box(ref b) = val {
                    if b.as_any()
                        .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                        .is_some()
                    {
                        let prev = self.suppress_result_checking;
                        self.suppress_result_checking = true;
                        let evaluated = self.execute_lambda(&val, &[]);
                        self.suppress_result_checking = prev;
                        evaluated?
                    } else {
                        val
                    }
                } else {
                    val
                };

                let type_path = self.get_value_type_path(&val);
                // If already a Result type, leave unchanged
                let result = if type_path == "::hot::type/Result.Ok"
                    || type_path == "::hot::type/Result.Err"
                {
                    val
                } else {
                    // Wrap in Result.Ok
                    let mut map = indexmap::IndexMap::new();
                    map.insert(Val::from("$type"), Val::from("::hot::type/Result.Ok"));
                    map.insert(Val::from("$val"), val);
                    Val::Map(Box::new(map))
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::CallLibBuiltin {
                dest,
                function,
                args,
            } => {
                let dest = *dest;
                let function = *function;
                let args = *args;
                // Call Rust hotlib function explicitly
                let function_val = self.get_register(function)?.clone();

                // LAZY ARGUMENT HANDLING: Check if args contain lazy thunks before evaluating
                let args_val = self.get_register(args)?.clone();

                let function_name = self.value_to_string(&function_val);
                // CallLibBuiltin instruction executing
                tracing::trace!("VM: CallLibBuiltin handling function: {}", function_name);

                tracing::trace!(
                    "VM: CallLibBuiltin - function: '{}', args_val: {:?}",
                    function_name,
                    args_val
                );

                // For call-lib, handle both normal arguments and lazy lambdas intelligently
                // Check if this is a call-lib invocation (function reference or args are lambdas)
                let is_call_lib = function_name == "<Lambda>"
                    || matches!(&function_val, Val::Box(b) if b.as_any().downcast_ref::<crate::lang::bytecode::LambdaInfo>().is_some())
                    || matches!(&args_val, Val::Box(b) if b.as_any().downcast_ref::<crate::lang::bytecode::LambdaInfo>().is_some());

                if is_call_lib {
                    // Detected call-lib invocation
                    let result = match self.execute_call_lib_builtin(&[function_val, args_val]) {
                        Ok(v) => v,
                        Err(e) => {
                            if self.error_capture_active {
                                let mut m = indexmap::IndexMap::new();
                                m.insert(Val::from("error"), Val::from(e.to_string()));
                                Val::Map(Box::new(m))
                            } else {
                                return Err(e);
                            }
                        }
                    };
                    self.set_register(dest, result)?;
                    self.instruction_pointer += 1;
                    return Ok(());
                }

                // Regular hotlib function call (not call-lib)
                let args_slice = match &args_val {
                    Val::Vec(vec) => {
                        tracing::trace!(
                            "VM: CallLibBuiltin - args is Vec with {} elements: {:?}",
                            vec.len(),
                            vec
                        );
                        vec.as_slice()
                    }
                    _ => {
                        tracing::trace!(
                            "VM: CallLibBuiltin - args is single value, wrapping in slice: {:?}",
                            args_val
                        );
                        // Single argument, wrap in slice
                        std::slice::from_ref(&args_val)
                    }
                };

                let result = match self.execute_call_lib(&function_name, args_slice) {
                    Ok(v) => v,
                    Err(e) => {
                        if self.error_capture_active {
                            let mut m = indexmap::IndexMap::new();
                            m.insert(Val::from("error"), Val::from(e.to_string()));
                            Val::Map(Box::new(m))
                        } else {
                            return Err(e);
                        }
                    }
                };
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::Return { value } => {
                let value = *value;
                // Return instruction - set the return value and exit function
                let return_val = self.get_register(value)?;
                tracing::trace!("VM: Return instruction - returning value: {:?}", return_val);

                // For now, we'll store the return value in a special register
                // In a full implementation, this would handle function call stack properly
                self.set_register(0, return_val.clone())?; // Use register 0 as return register

                // Set a flag to indicate function should exit
                // For now, we'll just continue execution - this is a simplified implementation
                self.instruction_pointer += 1;
            }

            Instruction::MakeVec {
                dest,
                elements_start,
                count,
            } => {
                let dest = *dest;
                let elements_start = *elements_start;
                let count = *count;
                // Create vector from consecutive registers
                let mut elements = Vec::new();
                for i in 0..count {
                    let element_reg = elements_start + i as RegisterId;
                    let element_val = self.get_register(element_reg)?.clone();
                    elements.push(element_val);
                }

                let vector_val = Val::Vec(elements);
                self.set_register(dest, vector_val)?;
                self.instruction_pointer += 1;
            }

            Instruction::VecPush { vec, value } => {
                let vec = *vec;
                let value = *value;
                // Push a value onto a vector (modifies vec in place)
                let value_to_push = self.get_register(value)?.clone();
                let vec_val = self.get_register(vec)?;

                if let Val::Vec(elements) = vec_val {
                    let mut new_elements = elements.clone();
                    new_elements.push(value_to_push);
                    self.set_register(vec, Val::Vec(new_elements))?;
                } else {
                    return Err(VmError::runtime(format!(
                        "VecPush: expected Vec, got {:?}",
                        vec_val
                    )));
                }
                self.instruction_pointer += 1;
            }

            Instruction::MakeVecWithSpread {
                dest,
                elements_start,
                count,
                spread_mask,
            } => {
                let dest = *dest;
                let elements_start = *elements_start;
                let count = *count;
                let spread_mask = *spread_mask;
                // Create vector from consecutive registers, spreading elements marked in spread_mask
                let mut elements = Vec::new();
                for i in 0..count {
                    let element_reg = elements_start + i as RegisterId;
                    let element_val = self.get_register(element_reg)?.clone();

                    // Check if this element should be spread
                    if (spread_mask >> i) & 1 == 1 {
                        // Spread this element - it should be a Vec
                        match element_val {
                            Val::Vec(inner_elements) => {
                                for inner_elem in inner_elements {
                                    elements.push(inner_elem);
                                }
                            }
                            Val::Null => {
                                // Null spreads to nothing
                            }
                            _ => {
                                // Non-Vec values are added as-is (graceful fallback)
                                tracing::warn!(
                                    "VM: MakeVecWithSpread expected Vec for spread element {}, got {:?}",
                                    i,
                                    element_val
                                );
                                elements.push(element_val);
                            }
                        }
                    } else {
                        // Regular element - add as-is
                        elements.push(element_val);
                    }
                }

                let vector_val = Val::Vec(elements);
                self.set_register(dest, vector_val)?;
                self.instruction_pointer += 1;
            }

            Instruction::ExtractInnerVal { dest, src } => {
                let dest = *dest;
                let src = *src;
                // Extract $val from a typed object for variant construction.
                // If the input has both $type and $val fields, extract the $val.
                // Otherwise, return the input as-is.
                // This allows both `Variant({...})` and `Variant(Type({...}))` to work equivalently.
                let src_val = self.get_register(src)?.clone();
                let extracted = match &src_val {
                    Val::Map(map) => {
                        if map.contains_key(&Val::from("$type")) {
                            if let Some(inner_val) = map.get(&Val::from("$val")) {
                                inner_val.clone()
                            } else {
                                src_val
                            }
                        } else {
                            src_val
                        }
                    }
                    _ => src_val,
                };
                self.set_register(dest, extracted)?;
                self.instruction_pointer += 1;
            }

            Instruction::ConstructTyped {
                dest,
                src,
                type_info,
            } => {
                let dest = *dest;
                let src = *src;
                let type_info = *type_info;
                // Construct a typed object with recursive nested type construction.
                // type_info is a map: {$type: "TypeName", $fields: {field_name: "TypeName", ...}}
                let src_val = self.get_register(src)?.clone();
                let type_info_val = self.get_constant(type_info)?.clone();

                let result = self.construct_typed_recursive(&src_val, &type_info_val)?;
                self.set_register(dest, result)?;
                self.instruction_pointer += 1;
            }

            Instruction::SetElement {
                collection,
                index,
                value,
            } => {
                let collection = *collection;
                let index = *index;
                let value = *value;
                let collection_val = self.get_register(collection)?.clone();
                let index_val = self.get_register(index)?.clone();
                let value_val = self.get_register(value)?.clone();

                match collection_val {
                    Val::Map(mut map) => {
                        // Set element in map
                        map.insert(index_val, value_val);
                        self.set_register(collection, Val::Map(map))?;
                    }
                    Val::Vec(mut vec) => {
                        // Set element in vector (if index is an integer)
                        if let Val::Int(idx) = index_val {
                            if idx >= 0 && (idx as usize) < vec.len() {
                                vec[idx as usize] = value_val;
                                self.set_register(collection, Val::Vec(vec))?;
                            } else {
                                return Err(VmError::runtime(format!(
                                    "Vector index {} out of bounds (length: {})",
                                    idx,
                                    vec.len()
                                )));
                            }
                        } else {
                            return Err(VmError::runtime(format!(
                                "Vector index must be an integer, got: {:?}",
                                index_val
                            )));
                        }
                    }
                    _ => {
                        return Err(VmError::runtime(format!(
                            "SetElement can only be used on maps or vectors, got: {:?}",
                            collection_val
                        )));
                    }
                }

                self.instruction_pointer += 1;
            }

            Instruction::MergeMaps { dest, source } => {
                let dest = *dest;
                let source = *source;
                let dest_val = self.get_register(dest)?.clone();
                let source_val = self.get_register(source)?.clone();

                match (dest_val, source_val) {
                    (Val::Map(mut dest_map), Val::Map(source_map)) => {
                        // Merge source map into destination map
                        for (k, v) in source_map.iter() {
                            dest_map.insert(k.clone(), v.clone());
                        }
                        self.set_register(dest, Val::Map(dest_map))?;
                    }
                    (dest, source) => {
                        return Err(VmError::runtime(format!(
                            "MergeMaps requires two maps, got: {:?} and {:?}",
                            dest, source
                        )));
                    }
                }

                self.instruction_pointer += 1;
            }

            Instruction::BeginErrorCapture => {
                self.error_capture_active = true;
                self.instruction_pointer += 1;
            }

            Instruction::EndErrorCapture => {
                self.error_capture_active = false;
                self.instruction_pointer += 1;
            }

            Instruction::ReturnIfErr { src } => {
                let src = *src;
                let val = self.get_register(src)?.clone();
                if val.is_err() {
                    // Value is an error - return it immediately
                    self.set_register(0, val)?; // Put in return register
                    self.instruction_pointer = usize::MAX; // Signal early return
                    return Ok(()); // Exit instruction execution
                }
                // Not an error - continue to next instruction
                self.instruction_pointer += 1;
            }

            Instruction::WrapOk { dest, src } => {
                let dest = *dest;
                let src = *src;
                let val = self.get_register(src)?.clone();
                let mut m = indexmap::IndexMap::new();
                m.insert(Val::from("ok"), val);
                self.set_register(dest, Val::Map(Box::new(m)))?;
                self.instruction_pointer += 1;
            }

            Instruction::RegisterLocalImplementation {
                source_type,
                target_type,
                implementation_function_name,
            } => {
                let source_type = *source_type;
                let target_type = *target_type;
                let implementation_function_name = *implementation_function_name;
                // Register a type implementation in the current lexical scope
                let source_type_str = self.get_string_constant(source_type)?;
                let target_type_str = self.get_string_constant(target_type)?;
                let impl_fn_str = self.get_string_constant(implementation_function_name)?;
                if let Some(current_scope) = self.scope_stack.last_mut() {
                    current_scope.add_local_implementation(
                        (*source_type_str).to_owned(),
                        (*target_type_str).to_owned(),
                        (*impl_fn_str).to_owned(),
                    );
                } else {
                    tracing::warn!("VM: No current scope for RegisterLocalImplementation");
                }
                self.instruction_pointer += 1;
            }

            Instruction::RegisterLocalType {
                type_name,
                constructor_function_name,
            } => {
                let type_name = *type_name;
                let constructor_function_name = *constructor_function_name;
                // Register a type constructor in the current lexical scope
                let type_name_str = self.get_string_constant(type_name)?;
                let constructor_fn_str = self.get_string_constant(constructor_function_name)?;
                if let Some(current_scope) = self.scope_stack.last_mut() {
                    tracing::trace!(
                        "VM: Registering local type constructor: {} (function: {})",
                        type_name_str,
                        constructor_fn_str
                    );
                    current_scope.add_local_type(
                        (*type_name_str).to_owned(),
                        (*constructor_fn_str).to_owned(),
                    );
                } else {
                    tracing::warn!("VM: No current scope for RegisterLocalType");
                }
                self.instruction_pointer += 1;
            }

            _ => {
                return Err(VmError::runtime(format!(
                    "Unimplemented: {:?}",
                    instruction
                )));
            }
        }

        Ok(())
    }

    fn load_constant(&mut self, constant_id: ConstantId) -> VmResult<Val> {
        let constant = self
            .program
            .constants
            .get(constant_id as usize)
            .ok_or(VmError::InvalidConstant(constant_id))?;

        let val = match constant {
            Constant::Val(v) => {
                // Clone to avoid aliasing immutable borrow while mutably resolving
                let v_clone = v.clone();

                let resolved = self.resolve_variable_references_in_val(&v_clone)?;

                tracing::trace!(
                    "VM: LoadConstant ID {} resolved Val({:?}) to {:?}",
                    constant_id,
                    v_clone,
                    resolved
                );
                resolved
            }
            Constant::FunctionRef(name) => {
                // Create a boxed function reference for deferred execution
                let boxed = Val::boxed((*name).to_owned());
                tracing::trace!(
                    "VM: LoadConstant ID {} resolved FunctionRef({}) to {:?}",
                    constant_id,
                    name,
                    boxed
                );
                boxed
            }
            Constant::TypeRef(type_name) => {
                // Type references are represented as strings — cheap Arc clone
                let str_val = Val::Str(Arc::clone(type_name));
                tracing::trace!(
                    "VM: LoadConstant ID {} resolved TypeRef({}) to {:?}",
                    constant_id,
                    type_name,
                    str_val
                );
                str_val
            }
            Constant::StringRef(s) => {
                // String references are represented as strings — cheap Arc clone
                let str_val = Val::Str(Arc::clone(s));
                tracing::trace!(
                    "VM: LoadConstant ID {} resolved StringRef({}) to {:?}",
                    constant_id,
                    s,
                    str_val
                );
                str_val
            }
            Constant::VariantTypeRef(type_ref) => {
                // Variant type references are boxed TypeRef objects
                let boxed = Val::Box(Box::new(type_ref.clone()));
                tracing::trace!(
                    "VM: LoadConstant ID {} resolved VariantTypeRef({}) to {:?}",
                    constant_id,
                    type_ref.name,
                    boxed
                );
                boxed
            }
        };

        Ok(val)
    }

    /// Get a string from the constant pool by ConstantId
    fn get_string_constant(&self, constant_id: ConstantId) -> VmResult<Arc<str>> {
        let constant = self
            .program
            .constants
            .get(constant_id as usize)
            .ok_or(VmError::InvalidConstant(constant_id))?;

        match constant {
            Constant::StringRef(s) => Ok(Arc::clone(s)),
            Constant::FunctionRef(s) => Ok(Arc::clone(s)),
            Constant::TypeRef(s) => Ok(Arc::clone(s)),
            Constant::VariantTypeRef(type_ref) => Ok(type_ref.name.as_str().into()),
            Constant::Val(Val::Str(s)) => Ok(Arc::clone(s)),
            _ => Err(VmError::runtime(format!(
                "Expected string constant at index {}, got {:?}",
                constant_id, constant
            ))),
        }
    }

    /// Resolve variable references in a Val (for legacy compatibility)
    fn resolve_variable_references_in_val(&mut self, val: &Val) -> VmResult<Val> {
        match val {
            Val::Map(map) => {
                // Recursively resolve variable references in maps
                let mut resolved_map = indexmap::IndexMap::new();
                for (key, value) in map.iter() {
                    // Execute boxed FnCall or boxed Value eagerly when found in map values
                    // BUT preserve lazy lambdas without evaluation
                    let resolved_value = if let Val::Box(b) = value {
                        // Check if this is a lazy lambda - if so, preserve it
                        if b.as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                            .is_some()
                        {
                            // This is a lazy lambda - preserve it without evaluation
                            value.clone()
                        } else if let Some(fn_call) =
                            b.as_any().downcast_ref::<crate::lang::ast::FnCall>()
                        {
                            // Execute the function call now
                            self.execute_fncall_ast(fn_call)?
                        } else if let Some(ast_node) =
                            b.as_any().downcast_ref::<crate::lang::ast::AstNode>()
                        {
                            match &ast_node.0 {
                                crate::lang::ast::Value::FnCall(fc) => {
                                    self.execute_fncall_ast(fc)?
                                }
                                other => self.convert_ast_value_to_runtime_val(other)?,
                            }
                        } else {
                            self.resolve_variable_references_in_val(value)?
                        }
                    } else {
                        // Default recursive resolution for non-box values
                        self.resolve_variable_references_in_val(value)?
                    };
                    resolved_map.insert(key.clone(), resolved_value);
                }
                Ok(Val::Map(Box::new(resolved_map)))
            }
            Val::Vec(vec) => {
                // Recursively resolve variable references in vectors
                let mut resolved_vec = Vec::new();
                for value in vec {
                    let resolved_value = if let Val::Box(b) = value {
                        // Check if this is a lazy lambda - if so, preserve it
                        if b.as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                            .is_some()
                        {
                            // This is a lazy lambda - preserve it without evaluation
                            value.clone()
                        } else if let Some(fn_call) =
                            b.as_any().downcast_ref::<crate::lang::ast::FnCall>()
                        {
                            self.execute_fncall_ast(fn_call)?
                        } else if let Some(ast_node) =
                            b.as_any().downcast_ref::<crate::lang::ast::AstNode>()
                        {
                            match &ast_node.0 {
                                crate::lang::ast::Value::FnCall(fc) => {
                                    self.execute_fncall_ast(fc)?
                                }
                                other => self.convert_ast_value_to_runtime_val(other)?,
                            }
                        } else {
                            self.resolve_variable_references_in_val(value)?
                        }
                    } else {
                        self.resolve_variable_references_in_val(value)?
                    };
                    resolved_vec.push(resolved_value);
                }
                Ok(Val::Vec(resolved_vec))
            }
            _ => Ok(val.clone()),
        }
    }

    /// Execute an AST-level function call directly to a runtime Val
    fn execute_fncall_ast(&mut self, fn_call: &crate::lang::ast::FnCall) -> VmResult<Val> {
        // Resolve the function reference from the AST Value
        let func_name = match fn_call.function.as_ref() {
            crate::lang::ast::Value::Ref(crate::lang::ast::Ref::Var(var_ref)) => {
                var_ref.var.sym.name().to_string()
            }
            crate::lang::ast::Value::Ref(crate::lang::ast::Ref::Ns(ns_ref)) => {
                if let Some(fname) = &ns_ref.function_name {
                    format!("{}/{}", ns_ref.ns, fname)
                } else {
                    return Err(VmError::runtime(
                        "Qualified reference missing function name".to_string(),
                    ));
                }
            }
            crate::lang::ast::Value::Val(Val::Str(name), _) => (**name).to_owned(),
            other => {
                return Err(VmError::runtime(format!(
                    "Unsupported function spec in boxed FnCall: {:?}",
                    other
                )));
            }
        };

        // Evaluate arguments (they are AST FnCallArg values)
        let mut arg_vals: Vec<Val> = Vec::with_capacity(fn_call.args.len());
        for arg in &fn_call.args {
            let v = self.convert_ast_value_to_runtime_val(&arg.value)?;
            arg_vals.push(v);
        }

        // Debug: log function name and argument types/values
        tracing::trace!(
            "AST FnCall dispatch: function='{}', args={:?}",
            func_name,
            arg_vals
        );

        // Dispatch to function
        if func_name.starts_with("::") && func_name.contains('/') {
            self.execute_function_call_by_qualified_name(&func_name, &arg_vals)
        } else {
            self.execute_function_call_by_name(&func_name, &arg_vals)
        }
    }

    /// Look up a qualified variable (immutable version for use in access_property)
    fn lookup_qualified_variable_immutable(&self, qualified_name: &str) -> Option<Val> {
        // Expect format ::namespace::path/var
        let stripped = qualified_name.trim_start_matches("::");
        if stripped.contains('/')
            && let Some((namespace_path, var_name)) = stripped.rsplit_once('/')
        {
            let full_namespace = format!("::{}", namespace_path);

            // Only try runtime namespace variables (no AST fallback to avoid mutability)
            if let Some(ns_map) = self.namespace_variables.get(&full_namespace)
                && let Some(val) = ns_map.get(var_name)
            {
                return Some(val.clone());
            }
        }

        None
    }

    fn get_register(&self, reg_id: RegisterId) -> VmResult<&Val> {
        self.registers
            .get(reg_id as usize)
            .ok_or(VmError::InvalidRegister(reg_id))
    }

    fn set_register(&mut self, reg_id: RegisterId, value: Val) -> VmResult<()> {
        if reg_id as usize >= self.registers.len() {
            self.registers.resize(reg_id as usize + 1, Val::Null);
        }

        self.registers[reg_id as usize] = value;
        // Instrumentation: track peak registers length
        if self.registers.len() > self.peak_registers_len {
            self.peak_registers_len = self.registers.len();
        }
        Ok(())
    }

    /// Ensure the VM has at least the specified number of registers
    fn ensure_register_capacity(&mut self, capacity: usize) {
        if self.registers.len() < capacity {
            self.registers.resize(capacity, Val::Null);
            // Instrumentation: track peak registers length
            if self.registers.len() > self.peak_registers_len {
                self.peak_registers_len = self.registers.len();
            }
        }
    }

    fn add_values(&self, left: &Val, right: &Val) -> VmResult<Val> {
        // Leverage Hot's native math operations through hotlib
        use crate::lang::hot::math;

        match math::add(&[left.clone(), right.clone()]) {
            crate::lang::hot::r#type::HotResult::Ok(result) => Ok(result),
            crate::lang::hot::r#type::HotResult::Err(error) => {
                Err(VmError::runtime(format!("Addition failed: {}", error)))
            }
        }
    }

    /// Get a constant value from the constant pool
    fn get_constant(&self, constant_id: ConstantId) -> VmResult<Val> {
        let constant = self
            .program
            .constants
            .get(constant_id as usize)
            .ok_or(VmError::InvalidConstant(constant_id))?;

        match constant {
            Constant::Val(v) => Ok(v.clone()),
            Constant::StringRef(s) => Ok(Val::Str(Arc::clone(s))),
            Constant::FunctionRef(s) => Ok(Val::Str(Arc::clone(s))),
            Constant::TypeRef(s) => Ok(Val::Str(Arc::clone(s))),
            Constant::VariantTypeRef(type_ref) => Ok(Val::from(type_ref.name.as_str())),
        }
    }

    /// Get a constant as a string (for variable names, etc.)
    fn get_constant_string(&self, constant_id: ConstantId) -> VmResult<Arc<str>> {
        let constant = self
            .program
            .constants
            .get(constant_id as usize)
            .ok_or(VmError::InvalidConstant(constant_id))?;

        match constant {
            Constant::Val(Val::Str(s)) => {
                tracing::trace!("VM: Resolved constant ID {} to string '{}'", constant_id, s);
                Ok(Arc::clone(s))
            }
            Constant::StringRef(s) => {
                tracing::trace!(
                    "VM: Resolved constant ID {} to StringRef '{}'",
                    constant_id,
                    s
                );
                Ok(Arc::clone(s))
            }
            Constant::FunctionRef(s) => {
                tracing::trace!(
                    "VM: Resolved constant ID {} to FunctionRef '{}'",
                    constant_id,
                    s
                );
                Ok(Arc::clone(s))
            }
            _ => {
                tracing::error!(
                    "VM: Expected string constant at ID {}, but found: {:?}",
                    constant_id,
                    constant
                );
                Err(VmError::runtime("Expected string constant".to_string()))
            }
        }
    }

    /// Recursively construct a typed object from raw data.
    /// type_info format: {
    ///   $type: "TypeName",
    ///   $fields: {field_name: {$type: "FieldType", ...}, ...},
    ///   $required: ["field1", "field2", ...],
    ///   $all_fields: ["field1", "field2", ...]
    /// }
    ///
    /// For nested type references (type_info with only $type, no $fields/$required),
    /// this method calls the nested type's constructor function to perform validation.
    ///
    /// Returns Result.Err if required fields are missing or data doesn't match expected structure.
    pub fn construct_typed_recursive(&mut self, data: &Val, type_info: &Val) -> VmResult<Val> {
        let type_info_map = match type_info {
            Val::Map(m) => m,
            _ => {
                // Not a type info map - return data as-is
                return Ok(data.clone());
            }
        };

        // Null short-circuit for nullable field coercion: if the
        // compiler tagged this field as `$optional` (i.e. declared as
        // `T?` on a struct field) and the supplied value is `null`,
        // preserve raw `Val::Null` instead of wrapping it in a typed
        // shell like `{$type: "::hot::type/Fn", $val: null}`. Without
        // this guard, `is-null` / `is-some` / pattern matches lie
        // about a field that was explicitly assigned `null`, because
        // the outer value becomes a `Map` rather than a `Null`.
        // Top-level constructor calls (e.g. `MyType()`) are unaffected
        // because they carry `$all_fields` / `$fields` / `$required`,
        // not `$optional`.
        if matches!(data, Val::Null)
            && matches!(
                type_info_map.get(&Val::from("$optional")),
                Some(Val::Bool(true))
            )
        {
            return Ok(Val::Null);
        }

        if let Some(Val::Vec(members)) = type_info_map.get(&Val::from("$union")) {
            return self.construct_union_typed(data, members);
        }

        // Get the type name
        let type_name = match type_info_map.get(&Val::from("$type")) {
            Some(Val::Str(s)) => (**s).to_string(),
            _ => {
                // No type info - return data as-is
                return Ok(data.clone());
            }
        };

        // Check if it's a builtin type (no construction needed)
        // BUT: Vec with $item_type needs special handling for nested custom types
        let has_item_type = type_info_map.contains_key(&Val::from("$item_type"));
        if Self::is_builtin_type(&type_name) && !has_item_type {
            return Ok(data.clone());
        }

        // Handle Vec with custom item type: process each item through its constructor
        if type_name == "Vec" && has_item_type {
            if let Val::Vec(vec) = data {
                let item_type_info = match type_info_map.get(&Val::from("$item_type")) {
                    Some(v) => v,
                    None => {
                        // contains_key said yes a moment ago; if we land here the
                        // map shape is broken — return data unchanged rather than
                        // panicking the worker.
                        return Ok(data.clone());
                    }
                };
                let mut new_vec = Vec::new();
                for (idx, item) in vec.iter().enumerate() {
                    let result = self.construct_typed_recursive(item, item_type_info)?;

                    // Check if item construction returned an error
                    if result.is_err() {
                        let inner_error = result.get("$val").unwrap_or(Val::Null);
                        let error_msg = format!("In Vec item {}: {}", idx, inner_error);
                        return Ok(Val::err(Val::from(error_msg)));
                    }
                    new_vec.push(result);
                }
                return Ok(Val::Vec(new_vec));
            } else {
                // Not a Vec - return as-is
                return Ok(data.clone());
            }
        }

        // Check if this is a nested type reference (has only $type, no $fields/$required/$all_fields)
        // In this case, we call the nested type's constructor function
        // Note: $all_fields indicates a top-level type_info from the compiler, not a nested reference
        let has_fields = type_info_map.contains_key(&Val::from("$fields"));
        let has_required = type_info_map.contains_key(&Val::from("$required"));
        let has_all_fields = type_info_map.contains_key(&Val::from("$all_fields"));

        let is_nested_reference =
            !has_fields && !has_required && !has_all_fields && type_name.starts_with("::");

        if is_nested_reference {
            // This is a reference to another custom type - call its constructor
            // If data is already typed with matching type (or enum variant), return as-is
            if let Val::Map(m) = data
                && let Some(Val::Str(existing_type)) = m.get(&Val::from("$type"))
            {
                let existing = &**existing_type;
                // Exact match (e.g., "::ns/Type" == "::ns/Type")
                // OR enum variant match (e.g., "::ns/Type.Variant" starts with "::ns/Type.")
                if existing == type_name || existing.starts_with(&format!("{}.", type_name)) {
                    return Ok(data.clone());
                }

                // Implicit coercion via a registered arrow. This is what makes
                // `Vec<Geom> [Triangle({...}), Square({...})]` work when
                // `Triangle -> Geom.Tri` and `Square -> Geom.Sq` are in scope:
                // construct_typed_recursive sees the Vec wants `Geom`, the
                // element is a tagged Triangle, and rather than failing variant
                // inference we look up the arrow and call it.
                let src_short = existing.rsplit('/').next().unwrap_or(existing);
                let tgt_short = type_name.rsplit('/').next().unwrap_or(&type_name);
                let candidates: Vec<String> = self
                    .type_implementations
                    .keys()
                    .filter_map(|(s, t)| {
                        if s != src_short {
                            return None;
                        }
                        if t == tgt_short || t.starts_with(&format!("{}.", tgt_short)) {
                            Some(t.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if candidates.len() == 1
                    && let Some(impl_name) =
                        self.resolve_type_implementation(src_short, &candidates[0])
                {
                    return self.execute_function_call_by_qualified_name(
                        &impl_name,
                        std::slice::from_ref(data),
                    );
                }
            }

            // For non-map data (strings, numbers, etc.), just wrap with $type/$val
            // This handles enum values, primitives, etc. that don't need constructor validation
            if !matches!(data, Val::Map(_)) {
                let mut result_map = indexmap::IndexMap::new();
                result_map.insert(Val::from("$type"), Val::from(type_name.clone()));
                result_map.insert(Val::from("$val"), data.clone());
                return Ok(Val::Map(Box::new(result_map)));
            }

            // Check if this type has a direct constructor (struct types do)
            let has_direct_constructor = self.function_mapping.contains_key(&type_name)
                || self
                    .function_mapping
                    .contains_key(&format!("{}/1", type_name));

            if has_direct_constructor {
                // Call the struct type's constructor
                return self.execute_function_call_by_qualified_name(
                    &type_name,
                    std::slice::from_ref(data),
                );
            }

            // No direct constructor - try enum variant inference
            // Discover variants by finding functions matching "{type_name}.{Variant}" or "{type_name}.{Variant}/N"
            let variant_prefix = format!("{}.", type_name);
            let mut seen_variants: AHashSet<String> = AHashSet::new();
            let variants: Vec<String> = self
                .function_mapping
                .keys()
                .filter_map(|k| {
                    if !k.starts_with(&variant_prefix) {
                        return None;
                    }
                    let after_prefix = &k[variant_prefix.len()..];
                    // Extract variant name (before any arity suffix)
                    let variant_name = if let Some(slash_pos) = after_prefix.find('/') {
                        &after_prefix[..slash_pos]
                    } else {
                        after_prefix
                    };
                    // Skip if variant name is empty or contains dots (nested)
                    if variant_name.is_empty() || variant_name.contains('.') {
                        return None;
                    }
                    // Return full constructor name (without arity suffix for dedup)
                    let full_name = format!("{}{}", variant_prefix, variant_name);
                    if seen_variants.insert(full_name.clone()) {
                        Some(full_name)
                    } else {
                        None
                    }
                })
                .collect();

            if !variants.is_empty() {
                tracing::trace!(
                    "VM: Found {} enum variants for {}: {:?}",
                    variants.len(),
                    type_name,
                    variants
                );

                // Try each variant constructor with the data
                // Track: (variant_name, constructed_value, error_count)
                let mut successful_constructions: Vec<(String, Val)> = Vec::new();

                for variant_constructor in &variants {
                    // Variant constructors now use ConstructTyped internally, which validates
                    // through the associated type's constructor. So we just call the variant
                    // constructor directly and check if it returns an error.
                    match self.execute_function_call_by_qualified_name(
                        variant_constructor,
                        std::slice::from_ref(data),
                    ) {
                        Ok(result) => {
                            // Check if construction succeeded (not an error result)
                            if !result.is_err() {
                                let variant_name =
                                    variant_constructor[variant_prefix.len()..].to_string();
                                successful_constructions.push((variant_name, result));
                            }
                        }
                        Err(_) => {
                            // Constructor threw - not a match
                        }
                    }
                }

                match successful_constructions.len() {
                    0 => {
                        // No variant matched - leave as raw map
                        tracing::trace!(
                            "VM: No enum variant matched for {}, leaving as raw map",
                            type_name
                        );
                        return Ok(data.clone());
                    }
                    1 => {
                        // Unique match - use it. The `len() == 1` arm guarantees
                        // exactly one element; we still match instead of unwrap so a
                        // future refactor of this control flow can't trip a panic.
                        let Some((variant_name, constructed)) =
                            successful_constructions.into_iter().next()
                        else {
                            return Ok(data.clone());
                        };
                        tracing::trace!(
                            "VM: Inferred enum variant {}.{} from successful construction",
                            type_name,
                            variant_name
                        );
                        return Ok(constructed);
                    }
                    _ => {
                        // Multiple matches (tie) - leave as raw map
                        tracing::trace!(
                            "VM: Ambiguous enum variant match for {} ({} matched), leaving as raw map",
                            type_name,
                            successful_constructions.len()
                        );
                        return Ok(data.clone());
                    }
                }
            }

            // Not an enum and no direct constructor - try anyway (might be defined elsewhere)
            tracing::trace!(
                "VM: construct_typed_recursive calling nested constructor '{}' for nested type",
                type_name
            );
            return self
                .execute_function_call_by_qualified_name(&type_name, std::slice::from_ref(data));
        }

        // Get all_fields list to check for newtypes
        let all_fields: Vec<String> = type_info_map
            .get(&Val::from("$all_fields"))
            .and_then(|v| match v {
                Val::Vec(fields) => Some(
                    fields
                        .iter()
                        .filter_map(|f| match f {
                            Val::Str(s) => Some((**s).to_string()),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        // Get required fields list
        let required_fields: Vec<String> = type_info_map
            .get(&Val::from("$required"))
            .and_then(|v| match v {
                Val::Vec(fields) => Some(
                    fields
                        .iter()
                        .filter_map(|f| match f {
                            Val::Str(s) => Some((**s).to_string()),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        // Get field type definitions (if any)
        let field_types = type_info_map
            .get(&Val::from("$fields"))
            .and_then(|v| match v {
                Val::Map(m) => Some(m.as_ref()),
                _ => None,
            });

        // Handle newtypes (types with no fields that wrap a single value)
        // If $all_fields is empty and data is not a map, just wrap with $type/$val
        if all_fields.is_empty() && !matches!(data, Val::Map(_)) {
            let mut result_map = indexmap::IndexMap::new();
            result_map.insert(Val::from("$type"), Val::from(type_name.clone()));
            result_map.insert(Val::from("$val"), data.clone());
            return Ok(Val::Map(Box::new(result_map)));
        }

        // If data is already typed (has $type), extract its $val for reconstruction
        let raw_data = match data {
            Val::Map(m) if m.contains_key(&Val::from("$type")) => {
                m.get(&Val::from("$val")).cloned().unwrap_or(data.clone())
            }
            _ => data.clone(),
        };

        // Process the data based on its type
        let processed_val = match &raw_data {
            Val::Map(data_map) => {
                // Validate required fields are present
                let mut missing_fields = Vec::new();
                for required_field in &required_fields {
                    if !data_map.contains_key(&Val::from(required_field.as_str())) {
                        missing_fields.push(required_field.clone());
                    }
                }

                if !missing_fields.is_empty() {
                    // Return Result.Err for missing required fields
                    let error_msg = format!(
                        "Type {} is missing required field(s): {}",
                        type_name,
                        missing_fields.join(", ")
                    );
                    return Ok(Val::err(Val::from(error_msg)));
                }

                // For map data, recursively construct typed fields
                let mut new_map = indexmap::IndexMap::new();

                for (key, value) in data_map.iter() {
                    let key_str = match key {
                        Val::Str(s) => (**s).to_string(),
                        _ => {
                            new_map.insert(key.clone(), value.clone());
                            continue;
                        }
                    };

                    // Check if this field has a type definition
                    let processed_value = if let Some(field_types_map) = field_types {
                        if let Some(field_type_info) =
                            field_types_map.get(&Val::from(key_str.as_str()))
                        {
                            // Recursively construct the field value
                            let result = self.construct_typed_recursive(value, field_type_info)?;

                            // Check if nested construction returned an error
                            if result.is_err()
                                && !Self::type_info_preserves_result_values(field_type_info)
                            {
                                // Propagate the error with context
                                let inner_error = result.get("$val").unwrap_or(Val::Null);
                                let error_msg = format!(
                                    "In field '{}' of type {}: {}",
                                    key_str, type_name, inner_error
                                );
                                return Ok(Val::err(Val::from(error_msg)));
                            }
                            result
                        } else {
                            value.clone()
                        }
                    } else {
                        value.clone()
                    };

                    new_map.insert(key.clone(), processed_value);
                }

                Val::Map(Box::new(new_map))
            }
            Val::Vec(vec) => {
                // For vector data, check if we have item type info
                let item_type_info = type_info_map.get(&Val::from("$item_type"));

                if let Some(item_info) = item_type_info {
                    // Recursively construct each item
                    let mut new_vec = Vec::new();
                    for (idx, item) in vec.iter().enumerate() {
                        let result = self.construct_typed_recursive(item, item_info)?;

                        // Check if item construction returned an error
                        if result.is_err() {
                            let inner_error = result.get("$val").unwrap_or(Val::Null);
                            let error_msg =
                                format!("In item {} of {}: {}", idx, type_name, inner_error);
                            return Ok(Val::err(Val::from(error_msg)));
                        }
                        new_vec.push(result);
                    }
                    Val::Vec(new_vec)
                } else {
                    raw_data.clone()
                }
            }
            _ => {
                // Expected a map for struct type construction
                let got_type = match &raw_data {
                    Val::Int(_) => "Int",
                    Val::Dec(_) => "Dec",
                    Val::Str(_) => "Str",
                    Val::Bool(_) => "Bool",
                    Val::Vec(_) => "Vec",
                    Val::Map(_) => "Map",
                    Val::Null => "Null",
                    Val::Byte(_) => "Byte",
                    Val::Bytes(_) => "Bytes",
                    Val::Box(_) => "Box",
                };
                let error_msg =
                    format!("Type {} expected a map/object, got {}", type_name, got_type);
                return Ok(Val::err(Val::from(error_msg)));
            }
        };

        // Wrap in typed structure: {$type: "...", $val: processed_val}
        let mut result_map = indexmap::IndexMap::new();
        result_map.insert(Val::from("$type"), Val::from(type_name));
        result_map.insert(Val::from("$val"), processed_val);

        Ok(Val::Map(Box::new(result_map)))
    }

    fn construct_union_typed(&mut self, data: &Val, members: &[Val]) -> VmResult<Val> {
        if matches!(data, Val::Null) && members.iter().any(Self::type_info_allows_null) {
            return Ok(Val::Null);
        }

        if let Some(existing_type) = Self::typed_value_type(data)
            && members
                .iter()
                .any(|member| Self::type_info_matches_type(member, existing_type))
        {
            return Ok(data.clone());
        }

        if let Some(matched) = members.iter().find(|member| {
            Self::value_matches_builtin_type(data, member)
                || Self::value_matches_literal_type(data, member)
        }) {
            if Self::value_matches_literal_type(data, matched) {
                return Ok(data.clone());
            }
            return self.construct_typed_recursive(data, matched);
        }

        let mut constructed_matches = Vec::new();
        for member in members {
            if Self::is_builtin_type_info(member) || Self::type_info_literal(member).is_some() {
                continue;
            }

            let constructed = self.construct_typed_recursive(data, member)?;
            if !constructed.is_err() && Self::constructed_value_matches_type(&constructed, member) {
                constructed_matches.push(constructed);
            }
        }

        match constructed_matches.len() {
            1 => Ok(constructed_matches.remove(0)),
            0 => Ok(Val::err(Val::from(format!(
                "Value does not match any member of union {}",
                Self::format_union_members(members)
            )))),
            _ => Ok(Val::err(Val::from(format!(
                "Value ambiguously matches multiple members of union {}",
                Self::format_union_members(members)
            )))),
        }
    }

    fn type_info_allows_null(type_info: &Val) -> bool {
        let Val::Map(map) = type_info else {
            return false;
        };

        matches!(map.get(&Val::from("$optional")), Some(Val::Bool(true)))
            || matches!(map.get(&Val::from("$type")), Some(Val::Str(t)) if &**t == "Null")
            || matches!(map.get(&Val::from("$literal")), Some(Val::Str(t)) if &**t == "null")
    }

    fn typed_value_type(data: &Val) -> Option<&str> {
        let Val::Map(map) = data else {
            return None;
        };
        let Some(Val::Str(type_name)) = map.get(&Val::from("$type")) else {
            return None;
        };
        Some(type_name)
    }

    fn type_info_matches_type(type_info: &Val, existing_type: &str) -> bool {
        let Some(type_name) = Self::type_info_type_name(type_info) else {
            return false;
        };
        existing_type == type_name || existing_type.starts_with(&format!("{}.", type_name))
    }

    fn constructed_value_matches_type(value: &Val, type_info: &Val) -> bool {
        if Self::value_matches_literal_type(value, type_info) {
            return true;
        }

        if Self::value_matches_builtin_type(value, type_info) {
            return true;
        }

        let Some(existing_type) = Self::typed_value_type(value) else {
            return false;
        };
        Self::type_info_matches_type(type_info, existing_type)
    }

    fn value_matches_builtin_type(value: &Val, type_info: &Val) -> bool {
        let Some(type_name) = Self::type_info_type_name(type_info) else {
            return false;
        };

        match type_name {
            "Any" => true,
            "Str" | "String" => matches!(value, Val::Str(_)),
            "Int" | "Integer" => matches!(value, Val::Int(_)),
            "Dec" | "Number" => matches!(value, Val::Dec(_) | Val::Int(_)),
            "Bool" | "Boolean" => matches!(value, Val::Bool(_)),
            "Null" => matches!(value, Val::Null),
            "Vec" => matches!(value, Val::Vec(_)),
            "Map" => matches!(value, Val::Map(_)),
            "Byte" => matches!(value, Val::Byte(_)),
            "Bytes" => matches!(value, Val::Bytes(_)),
            _ => false,
        }
    }

    fn value_matches_literal_type(value: &Val, type_info: &Val) -> bool {
        let Some(literal) = Self::type_info_literal(type_info) else {
            return false;
        };

        match value {
            Val::Str(s) => literal
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .is_some_and(|lit| lit == &**s),
            Val::Int(i) => literal == i.to_string(),
            Val::Dec(d) => literal == d.to_string(),
            Val::Bool(b) => literal == b.to_string(),
            Val::Null => literal == "null",
            _ => false,
        }
    }

    fn is_builtin_type_info(type_info: &Val) -> bool {
        let Some(type_name) = Self::type_info_type_name(type_info) else {
            return false;
        };
        Self::is_builtin_type(type_name)
            || matches!(
                type_name,
                "String" | "Integer" | "Number" | "Boolean" | "Byte" | "Bytes"
            )
    }

    fn type_info_type_name(type_info: &Val) -> Option<&str> {
        let Val::Map(map) = type_info else {
            return None;
        };
        let Some(Val::Str(type_name)) = map.get(&Val::from("$type")) else {
            return None;
        };
        Some(type_name)
    }

    fn type_info_literal(type_info: &Val) -> Option<&str> {
        let Val::Map(map) = type_info else {
            return None;
        };
        let Some(Val::Str(literal)) = map.get(&Val::from("$literal")) else {
            return None;
        };
        Some(literal)
    }

    fn type_info_preserves_result_values(type_info: &Val) -> bool {
        if let Some(type_name) = Self::type_info_type_name(type_info) {
            let normalized = type_name.trim_end_matches('?');
            if normalized == "Any" || normalized.ends_with("/Any") {
                return true;
            }
        }

        let Val::Map(map) = type_info else {
            return false;
        };
        let Some(Val::Vec(members)) = map.get(&Val::from("$union")) else {
            return false;
        };
        members.iter().any(Self::type_info_preserves_result_values)
    }

    fn format_union_members(members: &[Val]) -> String {
        members
            .iter()
            .filter_map(|member| {
                Self::type_info_type_name(member).or_else(|| Self::type_info_literal(member))
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Check if a type name is a builtin type (doesn't need typed construction)
    fn is_builtin_type(type_name: &str) -> bool {
        matches!(
            type_name,
            "Str"
                | "String"
                | "Int"
                | "Integer"
                | "Dec"
                | "Number"
                | "Bool"
                | "Boolean"
                | "Null"
                | "Vec"
                | "Map"
                | "Any"
        )
    }

    /// Look up a variable in the scope chain (lexical scoping)
    pub fn lookup_variable(&mut self, name: &str) -> VmResult<Val> {
        // Delegate to unified variable lookup to ensure strict, consistent ordering
        self.unified_variable_lookup(name)
    }

    /// Get a variable from the VM's namespace registry (similar to lang1's get_var)
    /// This is used to extract variables after execution is complete
    pub fn get_var(&self, namespace_path: Option<&str>, var_name: &str) -> Option<Val> {
        let namespace = match namespace_path {
            Some(ns) => {
                // Ensure namespace path starts with "::" if not already
                if ns.starts_with("::") {
                    ns.to_string()
                } else {
                    format!("::{}", ns)
                }
            }
            None => self.current_namespace.clone(),
        };

        // Look up the variable in the namespace registry
        self.namespace_variables
            .get(&namespace)
            .and_then(|ns_vars| ns_vars.get(var_name))
            .cloned()
    }

    /// Get all variables in a specific namespace
    pub fn get_namespace_vars<'a>(
        &'a self,
        namespace_path: Option<&str>,
    ) -> Option<&'a indexmap::IndexMap<String, Val>> {
        let namespace = match namespace_path {
            Some(ns) => {
                // Ensure namespace path starts with "::" if not already
                if ns.starts_with("::") {
                    ns.to_string()
                } else {
                    format!("::{}", ns)
                }
            }
            None => self.current_namespace.clone(),
        };

        self.namespace_variables.get(&namespace)
    }

    /// Get all namespaces and their variables (for debugging/inspection)
    pub fn get_all_namespace_vars(
        &self,
    ) -> &indexmap::IndexMap<String, indexmap::IndexMap<String, Val>> {
        &self.namespace_variables
    }

    /// Look up a qualified variable reference (::namespace/var)
    fn lookup_qualified_variable(&mut self, qualified_name: &str) -> VmResult<Val> {
        // Expect format ::namespace::path/var
        let stripped = qualified_name.trim_start_matches("::");
        if stripped.contains('/')
            && let Some((namespace_path, var_name)) = stripped.rsplit_once('/')
        {
            let full_namespace = format!("::{}", namespace_path);

            // 1) Try runtime namespace variables first (populated during entry execution)
            if let Some(ns_map) = self.namespace_variables.get(&full_namespace)
                && let Some(val) = ns_map.get(var_name)
            {
                return Ok(val.clone());
            }

            // 2) Fall back to Hot AST, if available
            if let Some(hot_ast) = &self.hot_ast {
                let ns_path = crate::lang::ast::NsPath::from_string(&full_namespace);
                if let Some(namespace) = hot_ast.namespaces.namespaces.get(&ns_path) {
                    for (var, value) in namespace.scope.vars.iter() {
                        if var.sym.name() == var_name {
                            let value_clone = value.clone();
                            return self.convert_ast_value_to_runtime_val(&value_clone);
                        }
                    }
                    // Variable not found in namespace - don't error yet, try function lookup below
                }
            }

            // 3) Check if this is a core function reference (e.g., ::hot::coll/concat)
            // This allows importing functions as variable aliases: `concat ::hot::coll/concat`
            if let Some(&_function_id) = self.core_functions.get(qualified_name) {
                // Return as a function reference that can be called
                let fn_ref = crate::lang::runtime::function_ref::FunctionRef::new(
                    qualified_name.to_string(),
                );
                return Ok(Val::Box(Box::new(fn_ref)));
            }

            // 4) Also check function_mapping for non-core functions
            if self.function_mapping.contains_key(qualified_name) {
                let fn_ref = crate::lang::runtime::function_ref::FunctionRef::new(
                    qualified_name.to_string(),
                );
                return Ok(Val::Box(Box::new(fn_ref)));
            }

            return Err(VmError::runtime(format!(
                "Namespace variable or function '{}/{}' not found",
                full_namespace, var_name
            )));
        }

        Err(VmError::runtime(format!(
            "Invalid qualified reference format: {}",
            qualified_name
        )))
    }

    /// Check if a value is truthy (used for conditional flows)
    /// Uses the Hot language semantics from val.rs
    fn is_truthy(&self, val: &Val) -> bool {
        val.is_truthy()
    }

    /// Collect flow result from flow context
    fn collect_flow_result(&mut self, flow_context: &FlowContext) -> VmResult<Val> {
        use crate::lang::bytecode::FlowType;

        tracing::trace!(
            "VM: collect_flow_result - flow_type: {:?}, result_modifier: {:?}, branch_results: {:?}",
            flow_context.flow_type,
            flow_context._result_modifier,
            flow_context.cond_branch_results
        );

        match flow_context.flow_type {
            FlowType::Cond | FlowType::CondAll | FlowType::Match | FlowType::MatchAll => {
                // Handle conditional/match flows
                // Apply result modifier
                match flow_context._result_modifier {
                    crate::lang::bytecode::FlowResultModifier::Map => {
                        // Create a map from branch names to results.
                        // Strip the per-cond uniqueness suffix from each branch
                        // name before exposing it as a user-visible map key.
                        let mut result_map = indexmap::IndexMap::new();

                        for (branch_name, branch_result) in &flow_context.cond_branch_results {
                            let key = user_facing_branch_name(branch_name);
                            result_map.insert(Val::from(key.to_string()), branch_result.clone());
                        }

                        tracing::trace!("VM: collect_flow_result |map -> {:?}", result_map);
                        Ok(Val::Map(Box::new(result_map)))
                    }
                    crate::lang::bytecode::FlowResultModifier::Vec => {
                        // Create a vector of results (ignore branch names)
                        let result_vec: Vec<Val> = flow_context
                            .cond_branch_results
                            .iter()
                            .map(|(_, result)| result.clone())
                            .collect();

                        tracing::trace!("VM: collect_flow_result |vec -> {:?}", result_vec);
                        Ok(Val::Vec(result_vec))
                    }
                    crate::lang::bytecode::FlowResultModifier::One => {
                        // For CondAll/MatchAll, return the last truthy result
                        // For Cond/Match, return the first (and only) truthy result
                        let result = if matches!(
                            flow_context.flow_type,
                            FlowType::CondAll | FlowType::MatchAll
                        ) {
                            flow_context
                                .cond_branch_results
                                .last()
                                .map(|(_, result)| result.clone())
                                .unwrap_or(Val::Null)
                        } else {
                            flow_context
                                .cond_branch_results
                                .first()
                                .map(|(_, result)| result.clone())
                                .unwrap_or(Val::Null)
                        };

                        tracing::trace!("VM: collect_flow_result |one -> {:?}", result);
                        Ok(result)
                    }
                }
            }
            FlowType::Serial | FlowType::Parallel => {
                // For serial/parallel flows, apply result modifiers based on referenced variables
                match flow_context._result_modifier {
                    crate::lang::bytecode::FlowResultModifier::Map => {
                        // Create a map from variable references
                        let mut result_map = indexmap::IndexMap::new();

                        for (var_name, var_value) in &flow_context.flow_variable_refs {
                            result_map.insert(Val::from(var_name.clone()), var_value.clone());
                        }

                        tracing::trace!("VM: Serial/Parallel flow |map -> {:?}", result_map);
                        Ok(Val::Map(Box::new(result_map)))
                    }
                    crate::lang::bytecode::FlowResultModifier::Vec => {
                        // Create a vector from variable references (values only)
                        let result_vec: Vec<Val> = flow_context
                            .flow_variable_refs
                            .iter()
                            .map(|(_, value)| value.clone())
                            .collect();

                        tracing::trace!("VM: Serial/Parallel flow |vec -> {:?}", result_vec);
                        Ok(Val::Vec(result_vec))
                    }
                    crate::lang::bytecode::FlowResultModifier::One => {
                        // For |one, return the last referenced variable value, or null if none
                        let result = flow_context
                            .flow_variable_refs
                            .last()
                            .map(|(_, value)| value.clone())
                            .unwrap_or(Val::Null);

                        tracing::trace!("VM: Serial/Parallel flow |one -> {:?}", result);
                        Ok(result)
                    }
                }
            }
            FlowType::Pipe => {
                // For pipe flows, apply result modifiers based on pipe step results
                match flow_context._result_modifier {
                    crate::lang::bytecode::FlowResultModifier::Map => {
                        // Create a map from pipe step results
                        let mut result_map = indexmap::IndexMap::new();

                        for (step_name, step_value) in &flow_context.pipe_step_results {
                            result_map.insert(Val::from(step_name.clone()), step_value.clone());
                        }

                        tracing::trace!("VM: Pipe flow |map -> {:?}", result_map);
                        Ok(Val::Map(Box::new(result_map)))
                    }
                    crate::lang::bytecode::FlowResultModifier::Vec => {
                        // Create a vector from pipe step results (values only)
                        let result_vec: Vec<Val> = flow_context
                            .pipe_step_results
                            .iter()
                            .map(|(_, value)| value.clone())
                            .collect();

                        tracing::trace!("VM: Pipe flow |vec -> {:?}", result_vec);
                        Ok(Val::Vec(result_vec))
                    }
                    crate::lang::bytecode::FlowResultModifier::One => {
                        // For |one, return the last pipe step result, or null if none
                        let result = flow_context
                            .pipe_step_results
                            .last()
                            .map(|(_, value)| value.clone())
                            .unwrap_or(Val::Null);

                        tracing::trace!("VM: Pipe flow |one -> {:?}", result);
                        Ok(result)
                    }
                }
            }
        }
    }

    /// Create a user function value
    pub(crate) fn create_user_function_value(&self, function_name: &str) -> Val {
        // Return a proper boxed FunctionRef so higher-level libs can call it
        crate::lang::runtime::function_ref::function_ref(function_name.to_string())
    }

    /// Unwrap result if it's an "ok" result, or fail if it's an error
    /// This implements Hot's automatic Result unwrapping behavior:
    /// - If val is a Result.Ok: return the $val value
    /// - If val is a Result.Err: call fail() with the error $val
    /// - Otherwise: return the value as-is
    ///
    /// NOTE: This should ONLY be called for non-lazy function arguments and template literal parts
    pub(crate) fn unwrap_result_if_ok(&mut self, val: &Val) -> VmResult<Val> {
        // Don't unwrap if this is a lambda - lazy values should not be unwrapped
        if let Val::Box(b) = val
            && b.as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
        {
            return Ok(val.clone());
        }

        // Skip Result checking if suppressed (e.g., during lazy thunk evaluation)
        if self.suppress_result_checking {
            return Ok(val.clone());
        }

        // Check if this is a Result type (variant union format)
        // Format: {$type: "::hot::type/Result.Ok", $val: ...} or {$type: "::hot::type/Result.Err", $val: ...}
        if let Val::Map(m) = val
            && let Some(Val::Str(type_str)) = m.get(&Val::from("$type"))
        {
            // Result.Ok - return the inner value
            if &**type_str == "::hot::type/Result.Ok" {
                return Ok(m.get(&Val::from("$val")).cloned().unwrap_or(Val::Null));
            }

            // Result.Err - fail with the error
            if &**type_str == "::hot::type/Result.Err" {
                let err_val = m.get(&Val::from("$val")).cloned().unwrap_or(Val::Null);

                // Extract message from error
                let msg = match &err_val {
                    Val::Str(s) => (**s).to_owned(),
                    Val::Map(err_map) => err_map
                        .get(&Val::from("$msg"))
                        .and_then(|v| match v {
                            Val::Str(s) => Some((**s).to_owned()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "Error Result encountered".to_string()),
                    _ => "Error Result encountered".to_string(),
                };

                // Set VM failure state and return error
                self.set_failure(msg.clone(), err_val.clone());

                // Emit run:fail event if emitter is available
                if let (Some(emitter), Some(execution_context)) =
                    (self.get_emitter().as_ref(), self.get_execution_context())
                {
                    let failure = crate::val!({
                        "$msg": msg.clone(),
                        "$err": err_val
                    });
                    let event =
                        crate::lang::emitter::EngineEvent::run_fail(execution_context, failure);
                    emitter.emit(event);
                }

                return Err(VmError::runtime(format!("Result error: {}", msg)));
            }
        }

        // Not a Result type, return as-is
        Ok(val.clone())
    }

    fn prepare_call_arg(&mut self, val: Val) -> VmResult<Val> {
        match val {
            Val::Int(_)
            | Val::Dec(_)
            | Val::Str(_)
            | Val::Bool(_)
            | Val::Byte(_)
            | Val::Bytes(_)
            | Val::Null => Ok(val),
            other => {
                // Preserve the slower semantic path for Results, boxed AST
                // references, and nested collections that may need resolution.
                let unwrapped_arg = self.unwrap_result_if_ok(&other)?;
                self.resolve_variable_references_in_val(&unwrapped_arg)
            }
        }
    }

    /// Helper to decrement function call depth only for non-dispatch functions
    fn decrement_function_call_depth(&mut self, function_name: &str) {
        let is_dispatch_function = function_name == "call-lib" || function_name == "call";
        if !is_dispatch_function {
            self.function_call_depth -= 1;
        }
    }

    /// Resolve a function name to its fully qualified name without executing it
    /// Uses the same resolution logic as unified_function_lookup but returns the resolved name
    pub fn resolve_function_name(&mut self, function_name: &str) -> Option<String> {
        tracing::trace!("VM: Resolving function name '{}'", function_name);

        // call-lib is always available
        if function_name == "call-lib" {
            return Some("call-lib".to_string());
        }

        // 1. FULLY QUALIFIED NAMES - Check if they exist
        if function_name.starts_with("::") {
            // Try exact match first
            if self.function_mapping.contains_key(function_name) {
                return Some(function_name.to_string());
            }

            // Try with different arities for overloaded functions
            for arity in 0..=5 {
                let arity_key = format!("{}/{}", function_name, arity);
                if self.function_mapping.contains_key(&arity_key) {
                    return Some(function_name.to_string());
                }
            }

            // Try hotlib lookup for qualified names
            let hotlib_map = crate::lang::hot::get_hotlib_map();
            if hotlib_map.contains_key(function_name) {
                return Some(function_name.to_string());
            }

            return None;
        }

        // 2. CORE FUNCTIONS - Check core function lookup
        if let Some(core_function_name) = self.lookup_core_function_name(function_name) {
            // Verify the core function actually exists
            if self.function_mapping.contains_key(&core_function_name) {
                return Some(core_function_name);
            }

            // Try with different arities
            for arity in 0..=5 {
                let arity_key = format!("{}/{}", core_function_name, arity);
                if self.function_mapping.contains_key(&arity_key) {
                    return Some(core_function_name);
                }
            }

            // Try hotlib lookup
            let hotlib_map = crate::lang::hot::get_hotlib_map();
            if hotlib_map.contains_key(&core_function_name) {
                return Some(core_function_name);
            }
        }

        // 3. CURRENT NAMESPACE - Try namespace qualification
        let qualified_name = format!("{}/{}", self.current_namespace, function_name);

        // Try exact match
        if self.function_mapping.contains_key(&qualified_name) {
            return Some(qualified_name);
        }

        // Try with different arities
        for arity in 0..=5 {
            let arity_key = format!("{}/{}", qualified_name, arity);
            if self.function_mapping.contains_key(&arity_key) {
                return Some(qualified_name);
            }
        }

        // 4. HOTLIB FUNCTIONS - Try unqualified hotlib lookup
        let hotlib_map = crate::lang::hot::get_hotlib_map();
        if hotlib_map.contains_key(function_name) {
            return Some(function_name.to_string());
        }

        None
    }

    /// Compute dispatch cache key from function name and argument type signature.
    ///
    /// Includes the Val discriminant of each argument so that overloads dispatched
    /// by type (e.g. `add(Int, Int)` vs `add(Str, Str)`) get distinct cache entries.
    /// For `Val::Map`, also includes the `$type` field (if present) so that custom
    /// typed maps (e.g. `User` vs `Shape`) produce distinct cache keys.
    #[inline(always)]
    fn dispatch_cache_key(name: &str, args: &[Val]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = ahash::AHasher::default();
        name.hash(&mut hasher);
        args.len().hash(&mut hasher);
        for arg in args {
            std::mem::discriminant(arg).hash(&mut hasher);
            // For maps with a $type field (custom types like User, Shape, etc.),
            // include the type name so different custom types get distinct cache entries.
            if let Val::Map(m) = arg
                && let Some(Val::Str(type_name)) = m.get(&Val::from("$type"))
            {
                type_name.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Execute a user function and cache the dispatch for future calls
    #[inline(always)]
    fn execute_user_function_cached(
        &mut self,
        cache_key: u64,
        function_id: FunctionId,
        args: &[Val],
    ) -> VmResult<Val> {
        self.dispatch_cache.insert(cache_key, function_id);
        self.execute_user_function(function_id, args)
    }

    /// UNIFIED VARIABLE/FUNCTION LOOKUP - Single canonical method for all resolution
    ///
    /// This handles both variables and functions since variables can contain any value,
    /// including functions. Core variables (with core: true metadata) are auto-imported.
    ///
    /// Resolution order:
    /// 1. Fully qualified names (::namespace/var/arity) -> Direct lookup
    /// 2. Unqualified names -> Check in order:
    ///    a) Lexical scope (local variables/functions)
    ///    b) Var alias/ref (follow variable references)
    ///    c) Core variables/functions (auto-imported)
    fn unified_function_lookup(&mut self, function_name: &str, args: &[Val]) -> VmResult<Val> {
        tracing::trace!(
            "VM: Unified lookup for '{}' with {} args",
            function_name,
            args.len()
        );

        // call-lib is the only built-in function and is always available
        if function_name == "call-lib" {
            return self.execute_call_lib_builtin(args);
        }

        // Compute cache key early (cheap hash), but don't check until after lexical scope.
        let cache_key = Self::dispatch_cache_key(function_name, args);

        // No special cases - let everything go through unified resolution
        // Vec, Map, etc. are core variables that contain type constructor functions

        // 1. FULLY QUALIFIED NAMES - Direct lookup in function_mapping
        if function_name.starts_with("::") {
            // Try exact match first
            if let Some(&function_id) = self.function_mapping.get(function_name) {
                tracing::trace!(
                    "✅ VM: Found fully qualified function '{}' with ID {}",
                    function_name,
                    function_id
                );
                return self.execute_user_function_cached(cache_key, function_id, args);
            } else {
                tracing::trace!(
                    "❌ VM: Fully qualified function '{}' not found in function_mapping",
                    function_name
                );
            }

            // Try with arity suffix for overloaded functions
            let arity = args.len();
            let arity_key = format!("{}/{}", function_name, arity);
            if let Some(&function_id) = self.function_mapping.get(&arity_key) {
                tracing::trace!(
                    "✅ VM: Found fully qualified function '{}' with arity {}",
                    function_name,
                    arity
                );
                return self.execute_user_function_cached(cache_key, function_id, args);
            }

            // Try variadic function lookup: check lower arities for variadic functions
            if arity > 1 {
                for try_arity in (1..arity).rev() {
                    let variadic_key = format!("{}/{}", function_name, try_arity);
                    if let Some(&function_id) = self.function_mapping.get(&variadic_key) {
                        // Verify this function is actually variadic
                        if let Some(fn_info) = self.program.functions.get(function_id as usize)
                            && fn_info.is_variadic
                        {
                            tracing::trace!(
                                "✅ VM: Found variadic qualified function '{}' with key '{}' (call arity: {})",
                                function_name,
                                variadic_key,
                                arity
                            );
                            return self.execute_user_function_cached(cache_key, function_id, args);
                        }
                    }
                }
            }

            // Try variadic function lookup: check higher arities for variadic functions called with fewer args
            for try_arity in (arity + 1)..=(arity + 10) {
                let variadic_key = format!("{}/{}", function_name, try_arity);
                if let Some(&function_id) = self.function_mapping.get(&variadic_key) {
                    // Verify this function is actually variadic
                    if let Some(fn_info) = self.program.functions.get(function_id as usize)
                        && fn_info.is_variadic
                    {
                        // Check that the call arity is at least the minimum required
                        let min_arity = fn_info.param_names.len().saturating_sub(1);
                        if arity >= min_arity {
                            tracing::trace!(
                                "✅ VM: Found variadic qualified function '{}' with key '{}' (call arity: {}, func arity: {}, min: {})",
                                function_name,
                                variadic_key,
                                arity,
                                try_arity,
                                min_arity
                            );
                            return self.execute_user_function_cached(cache_key, function_id, args);
                        }
                    }
                }
            }

            // Try hotlib lookup for qualified names
            let hotlib_map = crate::lang::hot::get_hotlib_map();
            if let Some(lib_fn) = hotlib_map.get(function_name) {
                tracing::trace!(
                    "✅ VM: Found fully qualified hotlib function '{}'",
                    function_name
                );
                return match lib_fn {
                    crate::lang::hot::libmap::HotLibFn::LibFn(f) => match f(args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err_val) => {
                            Err(VmError::runtime(err_val.to_string()))
                        }
                    },
                    crate::lang::hot::libmap::HotLibFn::VmAwareFn(f)
                    | crate::lang::hot::libmap::HotLibFn::VmAwareJitFn(f, _) => match f(self, args)
                    {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err_val) => {
                            Err(VmError::runtime(err_val.to_string()))
                        }
                    },
                };
            }

            // Check for type dispatch with fully qualified names
            if args.len() == 1 {
                // Extract the target type from the fully qualified name (e.g., "::hot::type/Str" -> "Str")
                let target_type = if let Some(last_part) = function_name.split('/').next_back() {
                    last_part
                } else {
                    function_name
                };

                let arg = &args[0];
                let source_type = self.get_value_type_name(arg);

                tracing::trace!(
                    "VM: Checking type dispatch for fully qualified '{}' (target: '{}') with source type '{}'",
                    function_name,
                    target_type,
                    source_type
                );

                if let Some(impl_function_name) =
                    self.resolve_type_implementation(&source_type, target_type)
                {
                    tracing::trace!(
                        "✅ VM: Found type implementation '{}' -> '{}' using function '{}'",
                        source_type,
                        target_type,
                        impl_function_name
                    );

                    // Call the implementation function
                    return self.unified_function_lookup(&impl_function_name, args);
                } else {
                    tracing::trace!(
                        "VM: No type implementation found for '{}' -> '{}'",
                        source_type,
                        target_type
                    );
                }
            }

            // Fully qualified name not found
            return Err(VmError::runtime(format!(
                "Fully qualified function '{}' not found in function registry or hotlib",
                function_name
            )));
        }

        // 2. UNQUALIFIED NAMES - Check in specified order

        // 2a. LEXICAL SCOPE - Check if it's a local variable/function
        match self.lookup_variable(function_name) {
            Ok(var_value) => {
                // Skip lazy parameter lambdas - they shouldn't shadow function names
                // If a lazy parameter has the same name as a function (e.g., 'err'),
                // we want to call the function, not the lazy lambda itself
                if let Val::Box(b) = &var_value {
                    if let Some(lambda_info) = b
                        .as_any()
                        .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                    {
                        if lambda_info.is_lazy_param {
                            tracing::trace!(
                                "VM: Skipping lazy parameter lambda '{}' in lexical scope, will check core functions",
                                function_name
                            );
                            // Continue to core function lookup
                        } else {
                            tracing::trace!(
                                "✅ VM: Found '{}' in lexical scope: {:?}",
                                function_name,
                                var_value
                            );
                            return self.call_function_value(&var_value, args);
                        }
                    } else {
                        tracing::trace!(
                            "✅ VM: Found '{}' in lexical scope: {:?}",
                            function_name,
                            var_value
                        );
                        return self.call_function_value(&var_value, args);
                    }
                } else {
                    tracing::trace!(
                        "✅ VM: Found '{}' in lexical scope: {:?}",
                        function_name,
                        var_value
                    );
                    return self.call_function_value(&var_value, args);
                }
            }
            Err(_) => {
                tracing::trace!("VM: '{}' not found in lexical scope", function_name);
            }
        }

        // 2b. VAR ALIAS/REF - Handled by lexical scope lookup above

        // DISPATCH CACHE: Fast path for previously resolved function calls.
        // Placed AFTER lexical scope check so local shadowing always wins.
        // This avoids repeated format string building and multi-map probing
        // for core functions, namespace-qualified functions, and hotlib lookups.
        if let Some(&cached_fn_id) = self.dispatch_cache.get(&cache_key) {
            tracing::trace!(
                "VM: Dispatch cache hit for '{}' (arity {}) -> fn_id {}",
                function_name,
                args.len(),
                cached_fn_id
            );
            return self.execute_user_function(cached_fn_id, args);
        }

        // 2c. CORE FUNCTIONS - Check if it's an auto-imported core function
        tracing::trace!("VM: Looking up core function for '{}'", function_name);
        if let Some(core_function_name) = self.lookup_core_function_name(function_name) {
            tracing::trace!(
                "✅ VM: Resolved '{}' to core function '{}'",
                function_name,
                core_function_name
            );

            // Try direct lookup first
            if let Some(&function_id) = self.function_mapping.get(&core_function_name) {
                return self.execute_user_function_cached(cache_key, function_id, args);
            }

            // Try with arity suffix
            let arity_key = format!("{}/{}", core_function_name, args.len());
            if let Some(&function_id) = self.function_mapping.get(&arity_key) {
                return self.execute_user_function_cached(cache_key, function_id, args);
            }

            // Try variadic function lookup (functions with fewer params that accept rest args)
            let arity = args.len();
            for try_arity in (1..arity).rev() {
                let variadic_key = format!("{}/{}", core_function_name, try_arity);
                if let Some(&function_id) = self.function_mapping.get(&variadic_key) {
                    // Verify this function is actually variadic
                    if let Some(fn_info) = self.program.functions.get(function_id as usize)
                        && fn_info.is_variadic
                    {
                        tracing::trace!(
                            "✅ VM: Found variadic core function '{}' with key '{}' (call arity: {})",
                            core_function_name,
                            variadic_key,
                            arity
                        );
                        return self.execute_user_function_cached(cache_key, function_id, args);
                    }
                }
            }

            // Try hotlib lookup
            let hotlib_map = crate::lang::hot::get_hotlib_map();
            if let Some(lib_fn) = hotlib_map.get(&core_function_name) {
                return match lib_fn {
                    crate::lang::hot::libmap::HotLibFn::LibFn(f) => match f(args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err_val) => {
                            Err(VmError::runtime(err_val.to_string()))
                        }
                    },
                    crate::lang::hot::libmap::HotLibFn::VmAwareFn(f)
                    | crate::lang::hot::libmap::HotLibFn::VmAwareJitFn(f, _) => match f(self, args)
                    {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err_val) => {
                            Err(VmError::runtime(err_val.to_string()))
                        }
                    },
                };
            }
        }

        // 3. FALLBACK - Try current namespace qualification
        let qualified_name = format!("{}/{}", self.current_namespace, function_name);

        // Try exact match
        if let Some(&function_id) = self.function_mapping.get(&qualified_name) {
            tracing::trace!(
                "✅ VM: Found namespace-qualified function '{}' with ID {}",
                qualified_name,
                function_id
            );
            return self.execute_user_function_cached(cache_key, function_id, args);
        }

        // Try with arity suffix
        let arity_key = format!("{}/{}", qualified_name, args.len());
        if let Some(&function_id) = self.function_mapping.get(&arity_key) {
            tracing::trace!(
                "✅ VM: Found namespace-qualified function '{}' with arity {}",
                qualified_name,
                args.len()
            );
            return self.execute_user_function_cached(cache_key, function_id, args);
        }

        // 4. FINAL FALLBACK - Try unqualified hotlib lookup
        let hotlib_map = crate::lang::hot::get_hotlib_map();
        if let Some(lib_fn) = hotlib_map.get(function_name) {
            tracing::trace!(
                "✅ VM: Found unqualified hotlib function '{}'",
                function_name
            );
            return match lib_fn {
                crate::lang::hot::libmap::HotLibFn::LibFn(f) => match f(args) {
                    crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                    crate::lang::hot::r#type::HotResult::Err(err_val) => {
                        Err(VmError::runtime(err_val.to_string()))
                    }
                },
                crate::lang::hot::libmap::HotLibFn::VmAwareFn(f)
                | crate::lang::hot::libmap::HotLibFn::VmAwareJitFn(f, _) => match f(self, args) {
                    crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                    crate::lang::hot::r#type::HotResult::Err(err_val) => {
                        Err(VmError::runtime(err_val.to_string()))
                    }
                },
            };
        }

        // 5. TYPE DISPATCH - Check if this is a type conversion call
        if args.len() == 1 {
            // Single argument - check for type implementation
            let arg = &args[0];
            let source_type = self.get_value_type_name(arg);

            tracing::trace!(
                "VM: Checking type dispatch for '{}' with source type '{}'",
                function_name,
                source_type
            );

            if let Some(impl_function_name) =
                self.resolve_type_implementation(&source_type, function_name)
            {
                tracing::trace!(
                    "✅ VM: Found type implementation '{}' -> '{}' using function '{}'",
                    source_type,
                    function_name,
                    impl_function_name
                );

                // Call the implementation function
                return self.unified_function_lookup(&impl_function_name, args);
            } else {
                tracing::trace!(
                    "VM: No type implementation found for '{}' -> '{}'",
                    source_type,
                    function_name
                );
            }
        }

        // Function not found anywhere - add detailed debugging
        tracing::error!(
            "VM: Function '{}' not found anywhere. Current namespace: '{}', IP: {}",
            function_name,
            self.current_namespace,
            self.instruction_pointer
        );

        // Add scope stack information for debugging
        tracing::error!("VM: Scope stack depth: {}", self.scope_stack.len());
        if let Some(current_function) = &self.current_debug_function {
            tracing::error!("VM: Current function: {}", current_function);
        }

        Err(VmError::runtime_with_ip(
            format!(
                "Function '{}' not found. Tried: lexical scope, core functions, namespace '{}', hotlib registry, and type dispatch.",
                function_name, self.current_namespace
            ),
            self.instruction_pointer,
        ))
    }

    /// UNIFIED VARIABLE LOOKUP - Single canonical method for all variable resolution
    ///
    /// This is the same as function lookup but returns the variable value directly
    /// instead of trying to call it as a function. Supports core variables with
    /// core: true metadata for auto-import (e.g., PI meta {core: true} 3.14159).
    ///
    /// Resolution order:
    /// 1. Fully qualified names (::namespace/var) -> Direct lookup
    /// 2. Unqualified names -> Check in order:
    ///    a) Lexical scope (local variables)
    ///    b) Var alias/ref (follow variable references)
    ///    c) Core variables (auto-imported variables with core: true)
    pub fn unified_variable_lookup(&mut self, var_name: &str) -> VmResult<Val> {
        // 1) Fully qualified: ::namespace/var
        if var_name.starts_with("::") && var_name.contains('/') {
            return self.lookup_qualified_variable(var_name);
        }

        // 2a) Lexical scope (innermost first)
        let mut lexical_found: Option<Val> = None;
        for scope in self.scope_stack.iter().rev() {
            if let Some(val) = scope.variables.get(var_name) {
                lexical_found = Some(val.clone());
                break;
            }
        }
        if let Some(val) = lexical_found {
            // Strings are just strings - no automatic resolution to function references
            return Ok(val);
        }

        // Current namespace only (no cross-namespace scanning per project rules)
        let ns_val = self
            .namespace_variables
            .get(&self.current_namespace)
            .and_then(|m| m.get(var_name).cloned());
        if let Some(val) = ns_val {
            // Strings are just strings - no automatic resolution
            return Ok(val);
        }

        // 2c) Core variables (auto-imported)
        if let Some(core_val) = self.lookup_core_variable(var_name) {
            return Ok(core_val);
        }

        // Check if we're in a lambda context and this might be a parameter that should have been bound
        let in_lambda_context = self.scope_stack.len() > 1 && self.function_call_depth > 0;

        if in_lambda_context {
            tracing::trace!(
                "VM: Variable '{}' not found in lambda context. Scope stack: {:?}",
                var_name,
                self.scope_stack
                    .iter()
                    .rev()
                    .map(|scope| {
                        format!("Scope: {:?}", scope.variables.keys().collect::<Vec<_>>())
                    })
                    .collect::<Vec<_>>()
            );
        } else {
            tracing::trace!(
                "VM: Variable '{}' not found in unified lookup. Scope stack depth: {}, Current namespace: '{}', Function call depth: {}",
                var_name,
                self.scope_stack.len(),
                self.current_namespace,
                self.function_call_depth
            );
        }

        Err(VmError::runtime(format!(
            "Variable '{}' not found (unified lookup)",
            var_name
        )))
    }

    // Removed try_resolve_varref_marker - strings are just strings, no automatic resolution

    /// Execute a function call with proper Hot semantics
    fn execute_function_call(&mut self, function_register: u32, args: &[Val]) -> VmResult<Val> {
        // Inspect without stringifying so lambdas and refs are preserved
        let mut function_val = self.get_register(function_register as RegisterId)?.clone();

        // Unwrap typed Fn maps: `{$type: "::hot::type/Fn", $val: <callable>}`.
        // These appear when a function value is stored in an `Fn`/`Fn?`-typed struct
        // field or constructor slot. The downstream resolver below only knows how to
        // extract a name from `Val::Str` / `Val::Box(FunctionRef)`, so without this
        // unwrap a typed-Fn value reaches the `_ => return Ok(Val::Null)` arm and
        // silently produces a null result. (`call_function_value` performs the same
        // unwrap for the lexical-scope path; this one covers direct-Call dispatch.)
        if let Val::Map(m) = &function_val
            && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
            && &**tn == "::hot::type/Fn"
            && let Some(inner) = m.get(&Val::from("$val"))
        {
            function_val = inner.clone();
        }

        // Determine if this is a dispatch function without stringifying lambdas
        let is_dispatch_function = match &function_val {
            Val::Str(name) => &**name == "call-lib" || &**name == "call",
            Val::Box(b) => {
                if let Some(fr) = b
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                {
                    fr.name == "call-lib" || fr.name == "call"
                } else {
                    false
                }
            }
            _ => false,
        };

        if !is_dispatch_function {
            // Cap Hot-level recursion to prevent OS stack overflow from
            // unbounded user recursion. The OS stack is generous (64 MB on
            // worker threads) but a stack overflow bypasses catch_unwind and
            // would crash the host; this check converts deep recursion into a
            // recoverable runtime error instead.
            let max_depth = max_recursion_depth();
            if self.function_call_depth >= max_depth {
                tracing::error!(
                    "Function call recursion limit reached at depth {} (max {})",
                    self.function_call_depth,
                    max_depth,
                );
                return Err(VmError::runtime(format!(
                    "Function call recursion limit reached (depth {}, max {}). \
                     This usually indicates unbounded recursion. \
                     Use tail recursion (TCO-eligible position), an iterative form, \
                     or raise HOT_MAX_RECURSION_DEPTH if your workload genuinely needs deeper calls.",
                    self.function_call_depth, max_depth,
                )));
            }
            self.function_call_depth += 1;
        }

        tracing::trace!(
            "VM: Function call - register {} contains: {:?}",
            function_register,
            function_val
        );

        // Directly execute lambdas without converting to names
        if let Val::Box(b) = &function_val
            && b.as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
        {
            let exec = self.execute_lambda(&function_val, args);
            if !is_dispatch_function {
                self.decrement_function_call_depth("<lambda>");
            }
            return exec;
        }

        // Resolve to a function name for normal dispatch paths
        let function_name: String = match function_val {
            Val::Str(name) => (*name).to_owned(),
            Val::Box(boxed_val) => {
                if let Some(function_ref) = boxed_val
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
                ) {
                    function_ref.name.clone()
                } else {
                    return Ok(Val::Null);
                }
            }
            _ => return Ok(Val::Null),
        };

        tracing::trace!(
            "VM: Calling function '{}' with {} args",
            function_name,
            args.len()
        );

        // Handle built-in functions first (call-lib only - call is now defined in Hot code)
        if function_name == "call-lib" {
            let result = self.execute_call_lib_builtin(args);
            if !is_dispatch_function {
                self.decrement_function_call_depth(&function_name);
            }
            return result;
        }

        // Fast paths — bypass unified_function_lookup overhead.
        // Tier 1: #[inline(always)] type-specialized helpers (zero call overhead)
        // Tier 2: Full library functions for remaining type combos (single source of truth)
        // Each library file owns both its fast-path specializations and general impls.
        {
            use crate::lang::hot::r#type::HotResult as HR;
            use crate::lang::hot::{bool, cmp, coll, math};

            // Helper: return a fast-path result, handling call depth
            macro_rules! fast_return {
                ($val:expr) => {{
                    if !is_dispatch_function {
                        self.decrement_function_call_depth(&function_name);
                    }
                    return Ok($val);
                }};
            }
            macro_rules! fast_return_hr {
                ($hr:expr) => {
                    match $hr {
                        HR::Ok(val) => fast_return!(val),
                        HR::Err(err_val) => {
                            if !is_dispatch_function {
                                self.decrement_function_call_depth(&function_name);
                            }
                            return Err(VmError::runtime(err_val.to_string()));
                        }
                    }
                };
            }

            match args.len() {
                // ── 1-arg fast paths ──────────────────────────────────────
                1 => match function_name.as_str() {
                    "not" => match &args[0] {
                        Val::Bool(b) => fast_return!(bool::fast_not_bool(*b)),
                        _ => fast_return_hr!(bool::not(args)),
                    },
                    "is-zero" => match &args[0] {
                        Val::Int(a) => fast_return!(math::fast_is_zero_int(*a)),
                        _ => fast_return_hr!(math::is_zero(args)),
                    },
                    "is-empty" => match &args[0] {
                        Val::Vec(v) => fast_return!(coll::fast_is_empty_vec(v)),
                        Val::Str(s) => fast_return!(coll::fast_is_empty_str(s)),
                        Val::Map(m) => fast_return!(coll::fast_is_empty_map(m)),
                        Val::Null => fast_return!(Val::Bool(true)),
                        _ => fast_return_hr!(coll::is_empty(args)),
                    },
                    "length" => match &args[0] {
                        Val::Vec(v) => fast_return!(coll::fast_length_vec(v)),
                        Val::Str(s) => fast_return!(coll::fast_length_str(s)),
                        Val::Map(m) => fast_return!(coll::fast_length_map(m)),
                        _ => fast_return_hr!(coll::length(args)),
                    },
                    "first" => match &args[0] {
                        Val::Vec(v) => fast_return!(coll::fast_first_vec(v)),
                        _ => fast_return_hr!(coll::first(args)),
                    },
                    "rest" => match &args[0] {
                        Val::Vec(v) => fast_return!(coll::fast_rest_vec(v)),
                        _ => fast_return_hr!(coll::rest(args)),
                    },
                    "range" => match &args[0] {
                        Val::Int(end) => {
                            // Gate the fast path on the configured collection
                            // size so user code can't allocate a multi-GB Vec
                            // by calling `range(1_000_000_000)`.
                            if let Some(n) =
                                crate::lang::runtime::limits::range_element_count(0, *end, 1)
                                && let Err(e) =
                                    crate::lang::runtime::limits::check_collection_size("range", n)
                            {
                                fast_return_hr!(HR::Err(e));
                            }
                            fast_return!(coll::fast_range_1_int(*end))
                        }
                        _ => fast_return_hr!(coll::range(args)),
                    },
                    _ => {}
                },

                // ── 2-arg fast paths ──────────────────────────────────────
                2 => {
                    // Tier 1: Int+Int arithmetic and comparison
                    if let (Val::Int(a), Val::Int(b)) = (&args[0], &args[1]) {
                        let result = match function_name.as_str() {
                            "add" => Some(math::fast_add_int(*a, *b)),
                            "sub" => Some(math::fast_sub_int(*a, *b)),
                            "mul" => Some(math::fast_mul_int(*a, *b)),
                            "mod" => math::fast_mod_int(*a, *b),
                            "div" => math::fast_div_int(*a, *b),
                            "pow" => math::fast_pow_int(*a, *b),
                            "lt" => Some(cmp::fast_lt_int(*a, *b)),
                            "gt" => Some(cmp::fast_gt_int(*a, *b)),
                            "lte" => Some(cmp::fast_lte_int(*a, *b)),
                            "gte" => Some(cmp::fast_gte_int(*a, *b)),
                            "eq" => Some(cmp::fast_eq_int(*a, *b)),
                            "ne" => Some(cmp::fast_ne_int(*a, *b)),
                            _ => None,
                        };
                        if let Some(result) = result {
                            fast_return!(result);
                        }
                    }

                    // Tier 1: collection operations with common type combos
                    match function_name.as_str() {
                        "get" => match (&args[0], &args[1]) {
                            (Val::Vec(v), Val::Int(i)) => {
                                fast_return!(coll::fast_get_vec_int(v, *i))
                            }
                            (Val::Map(m), key) => fast_return!(coll::fast_get_map(m, key)),
                            _ => fast_return_hr!(coll::get(args)),
                        },
                        "concat" => match (&args[0], &args[1]) {
                            (Val::Str(a), Val::Str(b)) => fast_return!(coll::fast_concat_str(a, b)),
                            (Val::Vec(a), Val::Vec(b)) => fast_return!(coll::fast_concat_vec(a, b)),
                            _ => fast_return_hr!(coll::concat(args)),
                        },
                        "range" => match (&args[0], &args[1]) {
                            (Val::Int(start), Val::Int(end)) => {
                                if let Some(n) = crate::lang::runtime::limits::range_element_count(
                                    *start, *end, 1,
                                ) && let Err(e) =
                                    crate::lang::runtime::limits::check_collection_size("range", n)
                                {
                                    fast_return_hr!(HR::Err(e));
                                }
                                fast_return!(coll::fast_range_2_int(*start, *end))
                            }
                            _ => fast_return_hr!(coll::range(args)),
                        },
                        _ => {}
                    }

                    // Tier 2: math/cmp for non-Int type combos (Dec, mixed, etc.)
                    let tier2 = match function_name.as_str() {
                        "add" => Some(math::add(args)),
                        "sub" => Some(math::sub(args)),
                        "mul" => Some(math::mul(args)),
                        "div" => Some(math::div(args)),
                        "mod" => Some(math::modulo(args)),
                        "pow" => Some(math::pow(args)),
                        "eq" => Some(cmp::eq(args)),
                        "ne" => Some(cmp::ne(args)),
                        "lt" => Some(cmp::lt(args)),
                        "gt" => Some(cmp::gt(args)),
                        "lte" => Some(cmp::lte(args)),
                        "gte" => Some(cmp::gte(args)),
                        _ => None,
                    };
                    if let Some(hr) = tier2 {
                        fast_return_hr!(hr);
                    }
                }

                // ── 3-arg fast paths ──────────────────────────────────────
                3 => match function_name.as_str() {
                    "range" => match (&args[0], &args[1], &args[2]) {
                        (Val::Int(start), Val::Int(end), Val::Int(step)) => {
                            if let Some(n) = crate::lang::runtime::limits::range_element_count(
                                *start, *end, *step,
                            ) && let Err(e) =
                                crate::lang::runtime::limits::check_collection_size("range", n)
                            {
                                fast_return_hr!(HR::Err(e));
                            }
                            match coll::fast_range_3_int(*start, *end, *step) {
                                Some(val) => fast_return!(val),
                                None => fast_return_hr!(coll::range(args)),
                            }
                        }
                        _ => fast_return_hr!(coll::range(args)),
                    },
                    "get" => fast_return_hr!(coll::get(args)),
                    _ => {}
                },

                _ => {}
            }
        }

        // Use unified function lookup for all other functions
        let result = self.unified_function_lookup(&function_name, args);
        if !is_dispatch_function {
            self.decrement_function_call_depth(&function_name);
        }
        result
    }

    /// Execute a function call by name (helper for variable function calls)
    pub fn execute_function_call_by_name(
        &mut self,
        function_name: &str,
        args: &[Val],
    ) -> VmResult<Val> {
        tracing::trace!(
            "VM: execute_function_call_by_name called with: '{}'",
            function_name
        );

        // Fast paths for common Int operations
        // NOTE: div is excluded because Hot returns Dec when result isn't exact (e.g., div(3,2) = 1.5)
        if args.len() == 2
            && let (Val::Int(a), Val::Int(b)) = (&args[0], &args[1])
        {
            let result = match function_name {
                "add" => Some(Val::Int(a.wrapping_add(*b))),
                "sub" => Some(Val::Int(a.wrapping_sub(*b))),
                "mul" => Some(Val::Int(a.wrapping_mul(*b))),
                "mod" => {
                    if *b == 0 {
                        None
                    } else {
                        Some(Val::Int(a % b))
                    }
                }
                "lt" => Some(Val::Bool(a < b)),
                "gt" => Some(Val::Bool(a > b)),
                "lte" => Some(Val::Bool(a <= b)),
                "gte" => Some(Val::Bool(a >= b)),
                "eq" => Some(Val::Bool(a == b)),
                "ne" => Some(Val::Bool(a != b)),
                _ => None,
            };
            if let Some(result) = result {
                return Ok(result);
            }
        }

        // Use unified function lookup - no more redundant fallback paths!
        self.unified_function_lookup(function_name, args)
    }

    /// Increment call function recursion depth and check for infinite recursion.
    /// Shares the same env-tunable limit as the general function-call cap.
    pub fn increment_call_depth(&mut self) -> VmResult<()> {
        let max_depth = max_recursion_depth();
        if self.call_function_depth >= max_depth {
            return Err(VmError::runtime(format!(
                "Call function recursion limit reached (depth {}, max {}). \
                 This indicates infinite recursion in the `call` dispatch path.",
                self.call_function_depth, max_depth,
            )));
        }
        self.call_function_depth += 1;
        tracing::trace!(
            "Call function depth incremented to {}",
            self.call_function_depth
        );
        Ok(())
    }

    /// Decrement call function recursion depth
    pub fn decrement_call_depth(&mut self) {
        if self.call_function_depth > 0 {
            self.call_function_depth -= 1;
            tracing::trace!(
                "Call function depth decremented to {}",
                self.call_function_depth
            );
        }
    }

    /// Public helper to find best user function overload for `call`
    pub fn find_best_user_function_overload(
        &self,
        function_name: &str,
        args: &[Val],
    ) -> Option<u32> {
        self.find_best_function_overload(function_name, args)
    }

    /// Public helper to execute a compiled user function by id for `call`
    pub fn execute_compiled_user_function(
        &mut self,
        function_id: u32,
        args: &[Val],
    ) -> VmResult<Val> {
        self.execute_user_function(function_id, args)
    }

    /// Call a function bypassing unified_function_lookup to avoid recursion
    /// This is specifically for the call_vm_aware function to avoid infinite recursion
    pub fn call_function_bypassing_unified_lookup(
        &mut self,
        function_name: &str,
        args: &[Val],
    ) -> VmResult<Val> {
        tracing::trace!(
            "VM: Bypassing unified lookup for '{}' with {} args",
            function_name,
            args.len()
        );

        // 1. Try user-defined functions first (compiled Hot functions)
        if let Some(function_id) = self.find_best_function_overload(function_name, args) {
            tracing::trace!(
                "Found user-defined function '{}' with ID {}",
                function_name,
                function_id
            );
            return self.execute_user_function(function_id, args);
        }

        // 2. Try hotlib functions, but SKIP the call function to avoid recursion
        if function_name != "::hot::lang/call" {
            let hotlib_map = crate::lang::hot::get_hotlib_map();
            if let Some(lib_fn) = hotlib_map.get(function_name) {
                tracing::trace!("Found hotlib function: '{}'", function_name);
                match lib_fn {
                    crate::lang::hot::HotLibFn::LibFn(func) => {
                        tracing::trace!("Calling regular hotlib function: '{}'", function_name);
                        match func(args) {
                            crate::lang::hot::r#type::HotResult::Ok(val) => return Ok(val),
                            crate::lang::hot::r#type::HotResult::Err(err) => {
                                return Err(VmError::runtime_with_ip(
                                    format!("Hotlib function '{}' error: {:?}", function_name, err),
                                    self.instruction_pointer,
                                ));
                            }
                        }
                    }
                    crate::lang::hot::HotLibFn::VmAwareFn(func)
                    | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                        tracing::trace!("Calling VM-aware hotlib function: '{}'", function_name);
                        match func(self, args) {
                            crate::lang::hot::r#type::HotResult::Ok(val) => return Ok(val),
                            crate::lang::hot::r#type::HotResult::Err(err) => {
                                return Err(VmError::runtime_with_ip(
                                    format!(
                                        "VM-aware hotlib function '{}' error: {:?}",
                                        function_name, err
                                    ),
                                    self.instruction_pointer,
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Function not found
        Err(VmError::runtime(format!(
            "Function '{}' not found in user-defined functions or hotlib functions",
            function_name
        )))
    }

    /// Call a function value (namespace reference, function reference, etc.)
    fn call_function_value(&mut self, function_val: &Val, args: &[Val]) -> VmResult<Val> {
        match function_val {
            Val::Box(boxed) => {
                // Handle boxed callable values
                // 1) LambdaInfo: execute directly
                if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                    .is_some()
                {
                    tracing::trace!("VM: Calling boxed lambda directly via execute_lambda");
                    return self.execute_lambda(function_val, args);
                }

                // 2) FunctionRef: dispatch by qualified/unqualified name
                if let Some(function_ref) = boxed
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
                ) {
                    let function_name = function_ref.name();
                    tracing::trace!("VM: Calling boxed function reference: {}", function_name);

                    // Prefer compiled Hot function overloads if available (e.g., type constructors)
                    if let Some(function_id) = self.find_best_function_overload(function_name, args)
                    {
                        tracing::trace!(
                            "VM: Dispatching to compiled user function for '{}' (ID: {})",
                            function_name,
                            function_id
                        );
                        return self.execute_user_function(function_id, args);
                    }

                    if function_name.starts_with("::") && function_name.contains('/') {
                        return self.execute_function_call_by_qualified_name(function_name, args);
                    } else {
                        return self.execute_function_call_by_name(function_name, args);
                    }
                }

                // 3) Fallback: stringify and attempt resolution (legacy behavior)
                let function_name = boxed.to_string();
                tracing::trace!(
                    "VM: Calling boxed value via stringified reference: {}",
                    function_name
                );
                if function_name.starts_with("::") && function_name.contains('/') {
                    self.execute_function_call_by_qualified_name(&function_name, args)
                } else {
                    self.execute_function_call_by_name(&function_name, args)
                }
            }
            Val::Map(map) => {
                // FunctionAlias: `{$type: "::hot::type/FunctionAlias", $target: <name>}`
                if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                    && &**type_name == "::hot::type/FunctionAlias"
                    && let Some(Val::Str(target_function)) = map.get(&Val::from("$target"))
                {
                    tracing::trace!("VM: Calling function alias -> {}", target_function);
                    if target_function.starts_with("::") && target_function.contains('/') {
                        return self.execute_function_call_by_qualified_name(target_function, args);
                    } else {
                        return self.execute_function_call_by_name(target_function, args);
                    }
                }

                // Typed `Fn` value: `{$type: "::hot::type/Fn", $val: <callable>}`. This is the
                // shape produced when a function value is stored in a struct field declared
                // `Fn` / `Fn?` (or any `Fn`-typed slot). Without this branch, calling a local
                // that was loaded from such a field — e.g. `f h.body; f(args)` where `body:
                // Fn?` — fell through to the "non-callable" default and returned `Val::Null`,
                // which presented to the user as a function silently producing null.
                if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                    && &**type_name == "::hot::type/Fn"
                    && let Some(inner) = map.get(&Val::from("$val"))
                {
                    tracing::trace!("VM: Unwrapping typed Fn value and recursing");
                    return self.call_function_value(&inner.clone(), args);
                }

                // Not a callable wrapper. Single-arg "construction" sugar: a non-callable
                // map called with one arg yields that arg unchanged. Multi-arg calls on a
                // non-callable map are a no-op returning Null.
                if args.len() == 1 {
                    tracing::trace!(
                        "VM: Non-callable map value with single arg - returning arg as constructed value"
                    );
                    return Ok(args[0].clone());
                }

                tracing::trace!("VM: Attempted to call non-callable map value; returning Null");
                Ok(Val::Null)
            }
            Val::Str(function_name) => {
                // String function name - call directly
                tracing::trace!("VM: Calling string function name: {}", function_name);
                if function_name.starts_with("::") && function_name.contains('/') {
                    self.execute_function_call_by_qualified_name(function_name, args)
                } else {
                    self.execute_function_call_by_name(function_name, args)
                }
            }
            _ => {
                // Non-callable values are returned unchanged when "called" with 1 arg,
                // enabling type constructor maps like `City({ ... })` to just yield
                // the provided map as the constructed value. This matches Hot semantics
                // for user-defined types where constructors simply tag values.
                if args.len() == 1 {
                    tracing::trace!(
                        "VM: Non-callable function value with single arg - returning arg as constructed value"
                    );
                    return Ok(args[0].clone());
                }

                // Otherwise, attempting to call a non-callable is a no-op -> null
                tracing::trace!("VM: Attempted to call non-callable value; returning Null");
                Ok(Val::Null)
            }
        }
    }

    /// Find the best matching function overload based on arity and type compatibility
    /// This implements Hot's function overload resolution algorithm
    fn find_best_function_overload(&self, function_name: &str, args: &[Val]) -> Option<FunctionId> {
        let arity = args.len();

        // Check if the function name already includes an arity suffix (e.g., "func/0")
        if function_name.matches('/').count() >= 2
            && function_name
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_digit())
        {
            // Function name already has arity suffix - try direct lookup first
            if let Some(&function_id) = self.function_mapping.get(function_name) {
                tracing::trace!(
                    "VM: Found direct match for arity-suffixed function '{}'",
                    function_name
                );
                return Some(function_id);
            }
        }

        // 1. Try exact signature match with type information (most specific)
        let arg_types: Vec<String> = args.iter().map(|arg| self.get_val_type_name(arg)).collect();
        let type_signature = format!("{}/{}:{}", function_name, arity, arg_types.join(","));
        tracing::trace!(
            "VM: Looking for exact type match with signature '{}', arg_types: {:?}",
            type_signature,
            arg_types
        );
        if let Some(&function_id) = self.function_mapping.get(&type_signature) {
            tracing::trace!(
                "VM: Found exact type match for '{}' with signature '{}'",
                function_name,
                type_signature
            );
            return Some(function_id);
        } else {
            tracing::trace!(
                "VM: No exact type match found for signature '{}'. Available function mappings with similar names:",
                type_signature
            );
            // Debug: show available function mappings that start with the function name
            for (key, _) in self.function_mapping.iter() {
                if key.starts_with(function_name) {
                    tracing::trace!("  Available: '{}'", key);
                }
            }
        }

        // 2. Try arity-only match (fallback for functions without specific type constraints)
        let arity_key = format!("{}/{}", function_name, arity);
        if let Some(&function_id) = self.function_mapping.get(&arity_key) {
            tracing::trace!(
                "VM: Found arity match for '{}' with key '{}'",
                function_name,
                arity_key
            );
            return Some(function_id);
        }

        // 3. For variadic functions: if called with more args than registered, try lower arities
        // E.g., calling or(a, b, c) with 3 args should match or/2 if it's variadic
        if arity > 1 {
            for try_arity in (1..arity).rev() {
                let variadic_key = format!("{}/{}", function_name, try_arity);
                if let Some(&function_id) = self.function_mapping.get(&variadic_key) {
                    // Verify this function is actually variadic
                    if let Some(fn_info) = self.program.functions.get(function_id as usize)
                        && fn_info.is_variadic
                    {
                        tracing::trace!(
                            "VM: Found variadic function for '{}' with key '{}' (call arity: {})",
                            function_name,
                            variadic_key,
                            arity
                        );
                        return Some(function_id);
                    }
                }
            }
        }

        // 4. For variadic functions: if called with fewer args, try higher arities
        // E.g., calling concat(a, b) with 2 args should match concat/3 if it's variadic
        // (the variadic parameter gets an empty vec or the remaining args)
        // Try a reasonable range of higher arities (up to 10)
        for try_arity in (arity + 1)..=(arity + 10) {
            let variadic_key = format!("{}/{}", function_name, try_arity);
            if let Some(&function_id) = self.function_mapping.get(&variadic_key) {
                // Verify this function is actually variadic
                if let Some(fn_info) = self.program.functions.get(function_id as usize)
                    && fn_info.is_variadic
                {
                    // Check that the call arity is at least the minimum required
                    // (all non-variadic params must be provided)
                    let min_arity = fn_info.param_names.len().saturating_sub(1);
                    if arity >= min_arity {
                        tracing::trace!(
                            "VM: Found variadic function for '{}' with key '{}' (call arity: {}, func arity: {}, min: {})",
                            function_name,
                            variadic_key,
                            arity,
                            try_arity,
                            min_arity
                        );
                        return Some(function_id);
                    }
                }
            }
        }

        // No name-only fallback - function overloading requires explicit arity/type matching
        // (Variadic functions are handled above; non-variadic must match exactly)
        None
    }

    /// Get the type name of a Val for function overload resolution
    fn get_val_type_name(&self, val: &Val) -> String {
        match val {
            Val::Str(_) => "Str".to_string(),
            Val::Int(_) => "Int".to_string(),
            Val::Dec(_) => "Dec".to_string(),
            Val::Bool(_) => "Bool".to_string(),
            Val::Vec(_) => "Vec".to_string(),
            Val::Map(map) => {
                // Check if this is a typed object with $type metadata
                if let Some(Val::Str(type_name)) = map.get(&Val::from("$type")) {
                    // Extract just the type name from "::namespace/TypeName"
                    if let Some(slash_pos) = type_name.rfind('/') {
                        type_name[slash_pos + 1..].to_string()
                    } else {
                        (**type_name).to_owned()
                    }
                } else {
                    "Map".to_string()
                }
            }
            Val::Bytes(_) => "Bytes".to_string(),
            Val::Byte(_) => "Byte".to_string(),
            Val::Null => "Null".to_string(),
            Val::Box(_) => "Any".to_string(), // Boxed values are treated as Any for now
        }
    }

    /// Get the full type path for a value (for match flow type checking)
    /// Returns paths like "::hot::type/Int", "::hot::type/Result.Ok", etc.
    pub fn get_value_type_path(&self, val: &Val) -> String {
        match val {
            Val::Str(_) => "::hot::type/Str".to_string(),
            Val::Int(_) => "::hot::type/Int".to_string(),
            Val::Dec(_) => "::hot::type/Dec".to_string(),
            Val::Bool(_) => "::hot::type/Bool".to_string(),
            Val::Null => "::hot::type/Null".to_string(),
            Val::Vec(_) => "::hot::type/Vec".to_string(),
            Val::Byte(_) => "::hot::type/Byte".to_string(),
            Val::Bytes(_) => "::hot::type/Bytes".to_string(),
            Val::Map(map) => {
                // Check for $type field for custom types (including Result.Ok/Result.Err variants)
                if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                    (**type_str).to_owned()
                } else {
                    // Plain map
                    "::hot::type/Map".to_string()
                }
            }
            Val::Box(boxed) => {
                // Check for function ref, namespace ref, lambda, etc.
                if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                    .is_some()
                {
                    "::hot::type/Fn".to_string()
                } else if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::refs::NamespaceRef>()
                    .is_some()
                {
                    "::hot::type/Namespace".to_string()
                } else if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                    .is_some()
                {
                    "::hot::type/Fn".to_string()
                } else if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
                    .is_some()
                {
                    "::hot::iter/Iter".to_string()
                } else {
                    "::hot::type/Any".to_string()
                }
            }
        }
    }

    /// Look up the declared parameter types for `function_id` by reverse-
    /// scanning `function_mapping` for a typed signature key (`name/N:T...`)
    /// that maps to this id. Caches the result (including `None` misses) so
    /// the O(N) scan runs at most once per function.
    ///
    /// Used by the `CallUserFunction` opcode handler to decide whether the
    /// pre-resolved static dispatch needs implicit coercion at runtime —
    /// the compiler picks `function_id` from the arity-only key when arg
    /// types aren't statically known, so the typed param info has to be
    /// recovered here.
    fn lookup_typed_param_signature(&mut self, function_id: FunctionId) -> Option<Vec<String>> {
        if let Some(cached) = self.coercion_param_cache.get(&function_id) {
            return cached.clone();
        }
        let mut found: Option<Vec<String>> = None;
        for (key, &fid) in self.function_mapping.iter() {
            if fid != function_id {
                continue;
            }
            // Function-signature keys look like `::ns/name/arity` (untyped)
            // or `::ns/name/arity:Type1,Type2` (typed). Names contain `:`
            // (namespaces use `::`), so we must locate the typed-signature
            // colon as the *first* colon after the last `/`.
            let last_slash = match key.rfind('/') {
                Some(i) => i,
                None => continue,
            };
            let after_slash = &key[last_slash + 1..];
            if let Some(colon_off) = after_slash.find(':') {
                let types_str = &after_slash[colon_off + 1..];
                let types: Vec<String> = types_str.split(',').map(|s| s.to_string()).collect();
                // Skip pure-`Any` signatures — they don't constrain dispatch
                // and would always "match" any arg, causing useless work.
                if types.iter().all(|t| t == "Any") {
                    continue;
                }
                found = Some(types);
                break;
            }
        }
        self.coercion_param_cache.insert(function_id, found.clone());
        found
    }

    /// Look up a coercion arrow whose source type matches `src` and whose
    /// target type is either `tgt` exactly or a variant of it (`tgt.X`).
    /// Returns the actual target key (used to resolve the impl function via
    /// `resolve_type_implementation`) when exactly one such arrow exists.
    /// Multiple matches return `None` so the dispatcher can fall through
    /// rather than silently picking one.
    fn find_coercion_target(&self, src: &str, tgt: &str) -> Option<String> {
        let dotted_prefix = format!("{}.", tgt);
        let candidates: Vec<String> = self
            .type_implementations
            .keys()
            .filter_map(|(s, t)| {
                if s != src {
                    return None;
                }
                if t == tgt || t.starts_with(&dotted_prefix) {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect();
        if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        }
    }

    /// Apply a coercion plan: for each arg whose entry in `plan` is `Some(target)`,
    /// invoke the registered `Source -> Target` arrow function and substitute the
    /// coerced value; pass `None` entries through unchanged. Errors from an arrow
    /// propagate to the caller — silent arrow-failure would surprise users who
    /// explicitly registered the conversion.
    fn apply_coercion_plan(&mut self, args: &[Val], plan: &[Option<String>]) -> VmResult<Vec<Val>> {
        let mut coerced: Vec<Val> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if let Some(target) = plan.get(i).and_then(|p| p.as_ref()) {
                let actual = self.get_val_type_name(arg);
                let Some(impl_name) = self.resolve_type_implementation(&actual, target) else {
                    return Err(VmError::runtime_with_ip(
                        format!(
                            "Implicit coercion plan referenced missing arrow {} -> {}",
                            actual, target
                        ),
                        self.instruction_pointer,
                    ));
                };
                let coerced_val = self.execute_function_call_by_qualified_name(
                    &impl_name,
                    std::slice::from_ref(arg),
                )?;
                coerced.push(coerced_val);
            } else {
                coerced.push(arg.clone());
            }
        }
        Ok(coerced)
    }

    /// Execute a function call by qualified name (`::namespace/function`).
    ///
    /// Resolution: typed-signature match → arity match → variadic/hotlib
    /// fallbacks (all delegated through `find_best_function_overload`).
    ///
    /// Implicit `Source -> Target` coercion is **not** applied here. The
    /// per-call coercion happens inside the `CallUserFunction` opcode
    /// handler so that compiler-resolved static dispatch (where
    /// `function_id` may have been picked from the arity-only key)
    /// gets the same treatment without touching this dynamic path —
    /// adding coercion to the dynamic path here previously regressed
    /// union-typed built-ins like `slice` whose dispatch is shape-sensitive.
    fn execute_function_call_by_qualified_name(
        &mut self,
        qualified_name: &str,
        args: &[Val],
    ) -> VmResult<Val> {
        tracing::trace!(
            "VM: execute_function_call_by_qualified_name called with '{}' and {} args",
            qualified_name,
            args.len()
        );

        // Try as compiled user function with signature-based resolution.
        // (Implicit coercion happens at the `CallUserFunction` opcode boundary
        // for compiled calls, and only there — see the opcode handler. This
        // path stays intentionally narrow to avoid regressing union-typed
        // built-in overloads like `slice` whose dispatch is shape-sensitive.)
        if let Some(function_id) = self.find_best_function_overload(qualified_name, args) {
            tracing::trace!(
                "VM: Arity/variadic match for '{}', function_id: {}",
                qualified_name,
                function_id
            );
            return self.execute_user_function(function_id, args);
        } else {
            tracing::trace!(
                "VM: No compiled user function overload found for '{}' with {} args",
                qualified_name,
                args.len()
            );
        }

        // Try as hotlib function via call-lib
        let hotlib_map = crate::lang::hot::get_hotlib_map();

        // Generic hotlib overload resolution: try type+arity and arity-specific keys first
        let arity = args.len();
        let arg_types: Vec<String> = args.iter().map(|a| self.get_val_type_name(a)).collect();
        let type_signature_key = format!("{}/{}:{}", qualified_name, arity, arg_types.join(","));
        if let Some(lib_fn) = hotlib_map.get(&type_signature_key) {
            return match lib_fn {
                crate::lang::hot::HotLibFn::LibFn(func) => {
                    tracing::trace!("VM: Calling hotlib overload: {}", type_signature_key);
                    match func(args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
                crate::lang::hot::HotLibFn::VmAwareFn(func)
                | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                    tracing::trace!(
                        "VM: Calling VM-aware hotlib overload: {}",
                        type_signature_key
                    );
                    match func(self, args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
            };
        }

        let arity_key = format!("{}/{}", qualified_name, arity);
        if let Some(lib_fn) = hotlib_map.get(&arity_key) {
            return match lib_fn {
                crate::lang::hot::HotLibFn::LibFn(func) => {
                    tracing::trace!("VM: Calling hotlib arity overload: {}", arity_key);
                    match func(args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
                crate::lang::hot::HotLibFn::VmAwareFn(func)
                | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                    tracing::trace!("VM: Calling VM-aware hotlib arity overload: {}", arity_key);
                    match func(self, args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
            };
        }

        if let Some(lib_fn) = hotlib_map.get(qualified_name) {
            match lib_fn {
                crate::lang::hot::HotLibFn::LibFn(func) => {
                    tracing::trace!("VM: Calling hotlib function: {}", qualified_name);
                    match func(args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
                crate::lang::hot::HotLibFn::VmAwareFn(func)
                | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                    tracing::trace!("VM: Calling VM-aware hotlib function: {}", qualified_name);
                    match func(self, args) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => Ok(val),
                        crate::lang::hot::r#type::HotResult::Err(err) => {
                            self.dispatch_hotlib_err(err)
                        }
                    }
                }
            }
        } else {
            Err(VmError::runtime_with_ip(
                format!(
                    "Qualified function '{}' not found in compiled functions or hotlib functions",
                    qualified_name
                ),
                self.instruction_pointer,
            ))
        }
    }

    /// Execute the call-lib built-in function with lazy argument handling
    pub fn execute_call_lib_builtin(&mut self, args: &[Val]) -> VmResult<Val> {
        if args.len() != 2 {
            return Err(VmError::runtime_with_ip(
                format!(
                    "call-lib expects 2 arguments (function_ref, args), got {}",
                    args.len()
                ),
                self.instruction_pointer,
            ));
        }

        let function_ref_lazy = &args[0];
        let function_args_lazy = &args[1];

        // Debug: Check if we received lazy thunks
        // eprintln!("CALL-LIB DEBUG: Received lazy args");

        // Evaluate the function reference (first argument)
        let function_ref = if let Val::Box(boxed) = function_ref_lazy {
            if let Some(_lazy_thunk) = boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                // eprintln!("CALL-LIB DEBUG: Evaluating function reference lazy thunk");
                match crate::lang::hot::core::do_eval(self, &[Val::Box(boxed.clone())]) {
                    crate::lang::hot::r#type::HotResult::Ok(val) => val,
                    crate::lang::hot::r#type::HotResult::Err(e) => {
                        return Err(VmError::runtime(format!(
                            "Failed to evaluate function reference: {:?}",
                            e
                        )));
                    }
                }
            } else {
                function_ref_lazy.clone()
            }
        } else {
            function_ref_lazy.clone()
        };

        // Extract function name from reference
        let function_name: String = match &function_ref {
            Val::Str(name) => (**name).to_owned(),
            Val::Box(boxed) => {
                // Check if this is a FunctionRef and extract the actual function name
                if let Some(function_ref) = boxed
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
                ) {
                    tracing::trace!(
                        "VM: call-lib extracting function name from FunctionRef: {}",
                        function_ref.name
                    );
                    function_ref.name.clone()
                } else {
                    // Fallback to string representation
                    boxed.to_string()
                }
            }
            _ => {
                return Err(VmError::runtime(format!(
                    "call-lib: first argument must be a function reference, got {:?}",
                    function_ref
                )));
            }
        };

        // Handle both lazy and normal arguments intelligently
        // Processing call-lib arguments

        let args_vec = if let Val::Box(boxed) = function_args_lazy {
            if let Some(_lazy_thunk) = boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                // Function args is a lazy thunk

                // DYNAMIC LAZY INSPECTION: Check if any arguments are actually lazy thunks
                // First, evaluate the args vector to inspect its contents
                let temp_evaluated_vec =
                    match crate::lang::hot::core::do_eval(self, &[Val::Box(boxed.clone())]) {
                        crate::lang::hot::r#type::HotResult::Ok(val) => val,
                        crate::lang::hot::r#type::HotResult::Err(e) => {
                            return Err(VmError::runtime(format!(
                                "Failed to evaluate args vector for inspection: {:?}",
                                e
                            )));
                        }
                    };

                let has_lazy_args = if let Val::Vec(ref vec) = temp_evaluated_vec {
                    vec.iter().any(|arg| {
                        if let Val::Box(arg_boxed) = arg {
                            arg_boxed
                                .as_any()
                                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                                .is_some()
                        } else {
                            false
                        }
                    })
                } else {
                    false
                };

                tracing::trace!(
                    "call-lib: Function '{}' has lazy args: {}",
                    function_name,
                    has_lazy_args
                );

                if has_lazy_args {
                    // Target function expects lazy args - preserving lazy thunks
                    // eprintln!("CALL-LIB DEBUG: Target function '{}' expects lazy args - preserving lazy thunks", function_name);
                    // For functions that expect lazy arguments, evaluate the vector but preserve inner lazy thunks
                    let evaluated_vec =
                        match crate::lang::hot::core::do_eval(self, &[Val::Box(boxed.clone())]) {
                            crate::lang::hot::r#type::HotResult::Ok(val) => val,
                            crate::lang::hot::r#type::HotResult::Err(e) => {
                                return Err(VmError::runtime(format!(
                                    "Failed to evaluate args vector: {:?}",
                                    e
                                )));
                            }
                        };

                    match evaluated_vec {
                        Val::Vec(vec) => vec,
                        _ => {
                            return Err(VmError::runtime(format!(
                                "call-lib: second argument must evaluate to an array, got {:?}",
                                evaluated_vec
                            )));
                        }
                    }
                } else {
                    // Target function doesn't expect lazy args - evaluating all
                    // For regular functions, evaluate everything
                    // Evaluating args vector lazy thunk
                    let evaluated_vec =
                        match crate::lang::hot::core::do_eval(self, &[Val::Box(boxed.clone())]) {
                            crate::lang::hot::r#type::HotResult::Ok(val) => {
                                // Args vector lazy thunk evaluated successfully
                                val
                            }
                            crate::lang::hot::r#type::HotResult::Err(e) => {
                                // Args vector lazy thunk evaluation failed
                                return Err(VmError::runtime(format!(
                                    "Failed to evaluate args vector: {:?}",
                                    e
                                )));
                            }
                        };

                    match evaluated_vec {
                        Val::Vec(vec) => {
                            // Evaluate any lazy thunks within the vector
                            let mut evaluated_args = Vec::new();
                            for arg in vec {
                                if let Val::Box(arg_boxed) = &arg {
                                    if arg_boxed
                                        .as_any()
                                        .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                                        .is_some()
                                    {
                                        tracing::trace!(
                                            "Evaluating lazy thunk argument: {:?}",
                                            arg
                                        );
                                        let evaluated_arg =
                                            match crate::lang::hot::core::do_eval(self, &[arg]) {
                                                crate::lang::hot::r#type::HotResult::Ok(val) => {
                                                    tracing::trace!(
                                                        "Lazy thunk evaluated to: {:?}",
                                                        val
                                                    );
                                                    val
                                                }
                                                crate::lang::hot::r#type::HotResult::Err(e) => {
                                                    tracing::trace!(
                                                        "Lazy thunk evaluation failed: {:?}",
                                                        e
                                                    );
                                                    return Err(VmError::runtime(format!(
                                                        "Failed to evaluate argument: {:?}",
                                                        e
                                                    )));
                                                }
                                            };
                                        evaluated_args.push(evaluated_arg);
                                    } else {
                                        evaluated_args.push(arg);
                                    }
                                } else {
                                    evaluated_args.push(arg);
                                }
                            }
                            evaluated_args
                        }
                        _ => {
                            return Err(VmError::runtime(format!(
                                "call-lib: second argument must evaluate to an array, got {:?}",
                                evaluated_vec
                            )));
                        }
                    }
                }
            } else {
                return Err(VmError::runtime(format!(
                    "call-lib: expected lazy thunk for args, got Box with other content: {:?}",
                    boxed
                )));
            }
        } else {
            // Handle normal arguments (Vec directly) - this is the new approach
            match function_args_lazy {
                Val::Vec(vec) => {
                    // Function args is a normal Vec

                    // DYNAMIC LAZY INSPECTION: Check if any arguments are actually lazy thunks
                    let has_lazy_args = vec.iter().any(|arg| {
                        if let Val::Box(arg_boxed) = arg {
                            arg_boxed
                                .as_any()
                                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                                .is_some()
                        } else {
                            false
                        }
                    });

                    tracing::trace!(
                        "call-lib: Function '{}' (Vec args) has lazy args: {}",
                        function_name,
                        has_lazy_args
                    );

                    // Pass arguments as-is (lazy thunk creation is handled at compile time)
                    vec.clone()
                }
                _ => {
                    return Err(VmError::runtime(format!(
                        "call-lib: expected Vec or lazy thunk for args, got: {:?}",
                        function_args_lazy
                    )));
                }
            }
        };

        tracing::trace!(
            "VM: call-lib calling '{}' with {} args: {:?}",
            function_name,
            args_vec.len(),
            args_vec
        );

        // Call the hotlib function
        self.execute_call_lib(&function_name, &args_vec)
    }

    /// Look up core function name by unqualified name
    /// Core functions are determined solely by having "core" metadata and being registered in core_functions
    pub fn lookup_core_function_name(&self, name: &str) -> Option<String> {
        tracing::trace!(
            "VM: Searching for core function '{}' in core functions registry ({} entries)",
            name,
            self.core_functions.len()
        );

        // Check the core functions registry - this contains all functions with "core" metadata
        // PRIORITIZE qualified names over unqualified names
        let mut unqualified_match = None;

        for (registered_name, &function_id) in self.core_functions.iter() {
            // Check if this is a qualified name that ends with our unqualified name (PREFERRED)
            if registered_name.contains('/') && registered_name.ends_with(&format!("/{}", name)) {
                tracing::trace!(
                    "VM: Found qualified core function '{}' -> '{}' (ID: {})",
                    name,
                    registered_name,
                    function_id
                );
                return Some(registered_name.clone());
            }

            // Check if this is an exact match for unqualified name (FALLBACK)
            if registered_name == name && unqualified_match.is_none() {
                tracing::trace!(
                    "VM: Found unqualified core function match '{}' (ID: {})",
                    name,
                    function_id
                );
                unqualified_match = Some(registered_name.clone());
            }
        }

        // Return unqualified match if no qualified match was found
        if let Some(unqualified) = unqualified_match {
            return Some(unqualified);
        }

        tracing::trace!(
            "VM: Core function '{}' not found in core functions registry or hotlib",
            name
        );

        // If the registry is empty, that's the problem
        if self.core_functions.is_empty() {
            tracing::error!(
                "VM: Core functions registry is empty! This indicates a compilation issue."
            );
        }

        None
    }

    /// Call a hotlib function
    pub(crate) fn call_hotlib_function(
        &mut self,
        function_name: &str,
        args: &[Val],
    ) -> VmResult<Val> {
        tracing::trace!(
            "VM: Calling hotlib function '{}' with {} args",
            function_name,
            args.len()
        );

        // Get the hotlib map
        let hotlib_map = crate::lang::hot::get_hotlib_map();

        // Try to find and call the function using enum-based dispatch
        if let Some(lib_fn) = hotlib_map.get(function_name) {
            tracing::trace!(
                "VM: Found hotlib function '{}', calling with {} args: {:?}",
                function_name,
                args.len(),
                args
            );
            let result = match lib_fn {
                crate::lang::hot::HotLibFn::LibFn(func) => {
                    // Regular function - call directly
                    tracing::trace!("VM: Calling regular hotlib function '{}'", function_name);
                    func(args)
                }
                crate::lang::hot::HotLibFn::VmAwareFn(func)
                | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                    // VM-aware function - pass mutable reference to VM
                    func(self, args)
                }
            };

            match result {
                crate::lang::hot::r#type::HotResult::Ok(val) => {
                    tracing::trace!(
                        "VM: Hotlib function '{}' returned: {:?}",
                        function_name,
                        val
                    );
                    Ok(val)
                }
                crate::lang::hot::r#type::HotResult::Err(err) => {
                    tracing::trace!("VM: Hotlib function '{}' failed: {:?}", function_name, err);

                    // Extract clean error message from Val::Str if possible
                    let clean_message: String = match &err {
                        Val::Str(msg) => (**msg).to_owned(),
                        _ => format!("{:?}", err),
                    };

                    // Create a RuntimeError with function context and source location if available
                    let mut runtime_error = RuntimeError::new(clean_message)
                        .with_function(function_name.to_string())
                        .with_instruction_pointer(self.instruction_pointer);

                    // Try to get source location from the source map
                    tracing::trace!(
                        "Looking for source location at instruction pointer {}",
                        self.instruction_pointer
                    );
                    if let Some(source_location) = self
                        .program
                        .source_map
                        .get_location(self.instruction_pointer)
                    {
                        tracing::trace!(
                            "Found source location: {}:{}:{}",
                            source_location
                                .file
                                .as_ref()
                                .unwrap_or(&"<unknown>".to_string()),
                            source_location.line,
                            source_location.column
                        );
                        let runtime_source_location = crate::lang::runtime::error::SourceLocation {
                            file: source_location.file.as_ref().map(std::path::PathBuf::from),
                            line: source_location.line,
                            column: source_location.column,
                            position: source_location.position,
                            length: source_location.length,
                        };
                        runtime_error.location = Some(runtime_source_location);
                    } else {
                        tracing::trace!(
                            "No source location found for instruction pointer {}",
                            self.instruction_pointer
                        );
                    }

                    Err(VmError::RuntimeError(runtime_error))
                }
            }
        } else {
            tracing::trace!(
                "VM: Unknown hotlib function '{}', returning null",
                function_name
            );
            Ok(Val::Null)
        }
    }

    /// Execute a lambda
    pub fn execute_lambda(&mut self, lambda_val: &Val, args: &[Val]) -> VmResult<Val> {
        super::jit::increment_lambda_interpreter_call_count();

        // Extract lambda information from the value - use Cow pattern to avoid clone when possible
        let lambda_info_ref = match lambda_val {
            Val::Box(boxed_val) => {
                // Try to downcast to LambdaInfo
                if let Some(lambda_info) = boxed_val
                    .as_any()
                    .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                {
                    lambda_info
                } else {
                    return Err(VmError::runtime(
                        "Lambda value does not contain LambdaInfo".to_string(),
                    ));
                }
            }
            _ => {
                return Err(VmError::runtime(format!(
                    "Invalid lambda value type: {:?}",
                    lambda_val
                )));
            }
        };

        tracing::trace!(
            "VM: Executing lambda with {} parameters, {} args, {} captured vars, is_lazy_param: {}",
            lambda_info_ref.parameters.len(),
            args.len(),
            lambda_info_ref.capture_vars.len(),
            lambda_info_ref.is_lazy_param
        );

        // Relaxed parameter count handling: bind up to min length; extra args ignored, missing args as Null
        if args.len() != lambda_info_ref.parameters.len() {
            tracing::trace!(
                "VM: Lambda parameter count mismatch (expected {}, got {}), proceeding with relaxed binding",
                lambda_info_ref.parameters.len(),
                args.len()
            );
        }

        // Check if closure environment was already populated at creation time.
        // We can't use "non-Null" as a sentinel for "captured" because a legitimately
        // captured value may itself be Null (e.g. `is-null(null)` capturing `value=Null`).
        // Instead, treat the closure as already-captured iff every declared capture_var
        // has a corresponding entry in closure_env. Lambdas with no capture_vars are
        // trivially "already captured" (nothing to populate).
        let already_captured = lambda_info_ref.capture_vars.is_empty()
            || lambda_info_ref
                .capture_vars
                .iter()
                .all(|name| lambda_info_ref.closure_env.contains_key(name));

        // Use Cow pattern: only clone if we need to mutate closure_env
        let lambda_info: std::borrow::Cow<'_, crate::lang::bytecode::LambdaInfo> =
            if already_captured {
                // Fast path: use reference, no clone needed
                tracing::trace!(
                    "VM: Using {} pre-captured closure variables (no clone)",
                    lambda_info_ref.closure_env.len()
                );
                std::borrow::Cow::Borrowed(lambda_info_ref)
            } else {
                // Slow path: need to populate closure_env, so clone
                let mut lambda_info_owned = lambda_info_ref.clone();

                // Clear any placeholder compile-time captures and populate runtime values
                // This handles lambdas that are executed immediately (not returned/stored)
                lambda_info_owned.closure_env.clear();

                // Populate closure environment with current variable values
                for var_name in &lambda_info_owned.capture_vars.clone() {
                    // Try to capture from current lexical scope first
                    if let Ok(var_value) = self.lookup_variable(var_name) {
                        lambda_info_owned
                            .closure_env
                            .insert(var_name.clone(), var_value);
                        tracing::trace!(
                            "VM: Captured variable '{}' from lexical scope = {:?}",
                            var_name,
                            lambda_info_owned.closure_env.get(var_name)
                        );
                        continue;
                    }

                    if let Ok(var_value) = self.unified_variable_lookup(var_name) {
                        // Fall back to unified lookup (namespace + core variables)
                        lambda_info_owned
                            .closure_env
                            .insert(var_name.clone(), var_value);
                        tracing::trace!(
                            "VM: Captured variable '{}' from unified lookup = {:?}",
                            var_name,
                            lambda_info_owned.closure_env.get(var_name)
                        );
                        continue;
                    }

                    // Check if it's a core function - capture as function reference
                    if let Some(core_fn_name) = self.lookup_core_function_name(var_name) {
                        let fn_ref = crate::lang::runtime::function_ref::FunctionRef::new(
                            core_fn_name.clone(),
                        );
                        lambda_info_owned
                            .closure_env
                            .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                        tracing::trace!(
                            "VM: Captured '{}' as core function reference '{}' at lambda execution",
                            var_name,
                            core_fn_name
                        );
                        continue;
                    }

                    // Check if it's any other user function
                    if let Some(function_id) = self.find_best_function_overload(var_name, &[])
                        && let Some(fn_info) = self.program.functions.get(function_id as usize)
                    {
                        let qualified_name = format!("{}/{}", fn_info.namespace, fn_info.name);
                        let fn_ref = crate::lang::runtime::function_ref::FunctionRef::new(
                            qualified_name.clone(),
                        );
                        lambda_info_owned
                            .closure_env
                            .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                        tracing::trace!(
                            "VM: Captured '{}' as function reference '{}' at lambda execution",
                            var_name,
                            qualified_name
                        );
                        continue;
                    }

                    // No fallback - if variable is not found in lexical scope or current namespace,
                    // it should be resolved during lambda execution using proper scoping rules
                    tracing::trace!(
                        "VM: Variable '{}' not found in lexical scope or current namespace - will resolve during lambda execution",
                        var_name
                    );
                }
                std::borrow::Cow::Owned(lambda_info_owned)
            };

        // Create a new lexical scope for lambda execution
        let mut lambda_scope = LexicalScope::new(ScopeType::Function, Some(self.scope_stack.len()));

        // Bind captured variables to closure environment FIRST
        // (so parameters can shadow them if names conflict)
        for (var_name, var_value) in &lambda_info.closure_env {
            tracing::trace!(
                "VM: Binding captured variable '{}' to {:?}",
                var_name,
                var_value
            );
            lambda_scope
                .variables
                .insert(var_name.clone(), var_value.clone());
        }

        // Bind lambda parameters to arguments SECOND
        // (parameters take precedence over captured variables with the same name)
        for (i, param_name) in lambda_info.parameters.iter().enumerate() {
            let arg_value = args.get(i).cloned().unwrap_or(Val::Null);
            tracing::trace!("VM: Binding lambda parameter '{}' to value", param_name);
            lambda_scope.variables.insert(param_name.clone(), arg_value);
        }

        // Push the lambda scope onto the scope stack
        self.scope_stack.push(lambda_scope);

        // Save current namespace and switch to lambda's defining namespace for proper lexical scoping
        let saved_namespace = self.current_namespace.clone();
        self.current_namespace = lambda_info.defining_namespace.clone();

        tracing::trace!(
            "VM: Switched namespace from '{}' to '{}' for lambda execution",
            saved_namespace,
            self.current_namespace
        );

        // Save current instruction pointer and register state
        let saved_instruction_pointer = self.instruction_pointer;
        let saved_register_count = self.registers.len();

        // Ensure we have enough registers for lambda execution
        self.ensure_register_capacity(lambda_info.register_count as usize);

        // Save registers that will be used by this lambda to prevent nested calls from overwriting them
        // This is critical for recursive lambdas that use the same register IDs
        // Use pre-computed used_registers to avoid O(N) instruction scan on every call
        let registers_to_save: Vec<(usize, Val)> = lambda_info
            .used_registers
            .iter()
            .filter_map(|&reg| {
                let reg_idx = reg as usize;
                if reg_idx < self.registers.len() {
                    Some((reg_idx, self.registers[reg_idx].clone()))
                } else {
                    None
                }
            })
            .collect();

        // Execute lambda instructions with proper instruction pointer management
        let mut lambda_ip = 0;
        let mut last_result = Val::Null;

        while lambda_ip < lambda_info.instructions.len() {
            let instruction = &lambda_info.instructions[lambda_ip];

            tracing::trace!("VM: Lambda instruction {}: {:?}", lambda_ip, instruction);

            // Respect branch skipping inside lambdas as well
            if let Some((ref skip_branch, skip_depth)) = self.skip_until_branch_end {
                let current_depth = self.flow_contexts.len();
                // Only apply skip if we're at the same flow depth
                if current_depth == skip_depth {
                    // Only let the matching CondBranchEnd pass through to clear the skip
                    if let Instruction::CondBranchEnd { branch_name, .. } = instruction {
                        if branch_name == skip_branch {
                            // Execute via the main instruction machinery to clear the skip flag
                            let saved_main_ip = self.instruction_pointer;
                            self.instruction_pointer = lambda_ip;
                            self.execute_instruction(instruction)?;
                            self.instruction_pointer = saved_main_ip;

                            // Track result for value-producing instructions
                            if let Some(dest) = self.get_instruction_dest(instruction)
                                && let Ok(val) = self.get_register(dest)
                            {
                                last_result = val.clone();
                            }

                            lambda_ip += 1;
                            continue;
                        } else {
                            // Skip all instructions until the matching CondBranchEnd
                            lambda_ip += 1;
                            continue;
                        }
                    } else {
                        // Skip non-CondBranchEnd instructions while waiting to reach the end of the branch
                        lambda_ip += 1;
                        continue;
                    }
                }
                // If at different depth, don't skip - let nested flows execute normally
            }

            // Skip remaining cond/match branches (conditions + bodies) until EndFlow
            if let Some(nested_depth) = self.skip_remaining_cond_flow {
                match instruction {
                    Instruction::BeginFlow { .. } => {
                        self.skip_remaining_cond_flow = Some(nested_depth + 1);
                        lambda_ip += 1;
                        continue;
                    }
                    Instruction::EndFlow { .. } => {
                        if nested_depth > 0 {
                            self.skip_remaining_cond_flow = Some(nested_depth - 1);
                            lambda_ip += 1;
                            continue;
                        } else {
                            // Target EndFlow - stop skipping, fall through to execute
                            self.skip_remaining_cond_flow = None;
                        }
                    }
                    _ => {
                        lambda_ip += 1;
                        continue;
                    }
                }
            }

            // Execute instruction directly without interfering with main VM instruction pointer
            match instruction {
                Instruction::LoadConst { dest, constant } => {
                    let constant_val = self.load_constant(*constant)?;

                    // If this is a lambda value, capture closure variables NOW (at creation time)
                    let constant_val = if let Val::Box(ref boxed) = constant_val {
                        if let Some(inner_lambda_info) = boxed
                            .as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>(
                        ) {
                            let mut captured_lambda = inner_lambda_info.clone();
                            captured_lambda.closure_env.clear();

                            for var_name in &captured_lambda.capture_vars {
                                if let Ok(var_value) = self.lookup_variable(var_name) {
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), var_value);
                                    tracing::trace!(
                                        "VM: Captured '{}' at inner lambda creation time",
                                        var_name
                                    );
                                } else if let Ok(var_value) = self.unified_variable_lookup(var_name)
                                {
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), var_value);
                                    tracing::trace!(
                                        "VM: Captured '{}' at inner lambda creation time (unified)",
                                        var_name
                                    );
                                } else if let Some(core_fn_name) =
                                    self.lookup_core_function_name(var_name)
                                {
                                    // If it's a core function, capture as a function reference
                                    let fn_ref =
                                        crate::lang::runtime::function_ref::FunctionRef::new(
                                            core_fn_name.clone(),
                                        );
                                    captured_lambda
                                        .closure_env
                                        .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                    tracing::trace!(
                                        "VM: Captured '{}' as core function reference '{}' at inner lambda creation",
                                        var_name,
                                        core_fn_name
                                    );
                                } else if let Some(function_id) =
                                    self.find_best_function_overload(var_name, &[])
                                {
                                    // If it's any other user function, capture as a function reference
                                    if let Some(fn_info) =
                                        self.program.functions.get(function_id as usize)
                                    {
                                        let qualified_name =
                                            format!("{}/{}", fn_info.namespace, fn_info.name);
                                        let fn_ref =
                                            crate::lang::runtime::function_ref::FunctionRef::new(
                                                qualified_name.clone(),
                                            );
                                        captured_lambda
                                            .closure_env
                                            .insert(var_name.clone(), Val::Box(Box::new(fn_ref)));
                                        tracing::trace!(
                                            "VM: Captured '{}' as function reference '{}' at inner lambda creation",
                                            var_name,
                                            qualified_name
                                        );
                                    }
                                }
                            }

                            Val::Box(Box::new(captured_lambda))
                        } else {
                            constant_val
                        }
                    } else {
                        constant_val
                    };

                    self.set_register(*dest, constant_val)?;
                    last_result = self.get_register(*dest)?.clone();
                }
                Instruction::Move { dest, src } => {
                    let value = self.get_register(*src)?.clone();
                    self.set_register(*dest, value)?;
                    last_result = self.get_register(*dest)?.clone();
                }
                Instruction::LoadVar { dest, var_name } => {
                    let name = self.get_constant_string(*var_name)?;
                    let value = self.lookup_variable(&name)?;
                    self.set_register(*dest, value)?;
                    last_result = self.get_register(*dest)?.clone();
                }
                Instruction::Call {
                    dest,
                    function,
                    args_start,
                    args_count,
                } => {
                    let mut args = Vec::new();
                    for i in 0..*args_count {
                        let arg_reg = args_start + i as RegisterId;
                        let arg_val = self.get_register(arg_reg)?.clone();
                        args.push(arg_val);
                    }
                    let result = self.execute_function_call(*function, &args)?;
                    self.set_register(*dest, result)?;
                    last_result = self.get_register(*dest)?.clone();
                }
                Instruction::Return { value } => {
                    last_result = self.get_register(*value)?.clone();
                    break; // Exit lambda execution
                }
                _ => {
                    // For other instructions, we need to temporarily set the main IP
                    // and restore it after execution
                    let saved_main_ip = self.instruction_pointer;
                    self.instruction_pointer = lambda_ip;
                    self.execute_instruction(instruction)?;
                    self.instruction_pointer = saved_main_ip;

                    // Track result for value-producing instructions
                    if let Some(dest) = self.get_instruction_dest(instruction)
                        && let Ok(val) = self.get_register(dest)
                    {
                        last_result = val.clone();
                    }
                }
            }

            lambda_ip += 1;
        }

        // Restore instruction pointer and register state
        self.instruction_pointer = saved_instruction_pointer;

        // Restore saved registers before truncating (for recursive lambdas)
        for (reg_idx, saved_val) in registers_to_save {
            if reg_idx < self.registers.len() {
                self.registers[reg_idx] = saved_val;
            }
        }

        self.registers.truncate(saved_register_count);

        // Pop the lambda scope
        self.scope_stack.pop();

        // Restore original namespace
        self.current_namespace = saved_namespace;
        tracing::trace!(
            "VM: Restored namespace to '{}' after lambda execution",
            self.current_namespace
        );

        tracing::trace!(
            "VM: Lambda execution completed with result: {:?}",
            last_result
        );
        Ok(last_result)
    }

    /// Execute a user function
    fn try_jit_call(&mut self, function_id: u32, args: &[Val]) -> Result<Option<Val>, String> {
        let prev = crate::lang::runtime::jit::set_jit_vm_ptr(self as *mut VirtualMachine);
        let result = self.jit.try_call_compiled(function_id, args);
        crate::lang::runtime::jit::set_jit_vm_ptr(prev);
        result
    }

    pub fn try_jit_lambda_call(&mut self, lambda_val: &Val, args: &[Val]) -> VmResult<Option<Val>> {
        let lambda_info = match lambda_val {
            Val::Box(boxed_val) => boxed_val
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>(),
            _ => None,
        };
        let Some(lambda_info) = lambda_info else {
            return Ok(None);
        };
        if lambda_info.is_lazy_param {
            return Ok(None);
        }

        let normalized_args: Vec<Val> = lambda_info
            .parameters
            .iter()
            .enumerate()
            .map(|(idx, _)| args.get(idx).cloned().unwrap_or(Val::Null))
            .collect();

        let mut captures = Vec::with_capacity(lambda_info.capture_vars.len());
        for name in &lambda_info.capture_vars {
            let Some(value) = lambda_info.closure_env.get(name) else {
                return Ok(None);
            };
            captures.push(value.clone());
        }

        let program = Arc::clone(&self.program);
        let saved_namespace = self.current_namespace.clone();
        self.current_namespace = lambda_info.defining_namespace.clone();
        let prev = crate::lang::runtime::jit::set_jit_vm_ptr(self as *mut VirtualMachine);
        let result =
            self.jit
                .try_call_compiled_lambda(&program, lambda_info, &normalized_args, &captures);
        crate::lang::runtime::jit::set_jit_vm_ptr(prev);
        self.current_namespace = saved_namespace;

        result.map_err(|msg| VmError::RuntimeError(RuntimeError::new(msg)))
    }

    fn execute_user_function(&mut self, function_id: u32, args: &[Val]) -> VmResult<Val> {
        if self.jit.config.is_enabled() {
            self.jit.record_call(function_id, args);

            if let Some(result) = self.try_jit_call(function_id, args).map_err(|msg| {
                VmError::RuntimeError(
                    RuntimeError::new(msg).with_instruction_pointer(self.instruction_pointer),
                )
            })? {
                return Ok(result);
            }

            let program = Arc::clone(&self.program);
            let function_info = program.functions.get(function_id as usize).ok_or_else(|| {
                VmError::runtime(format!("Function with ID {} not found", function_id))
            })?;
            let function_name = &function_info.name;

            if self.jit.should_compile_now(function_id, args) {
                let compile_result =
                    self.jit
                        .compile_function(&program, function_id, function_info, args);

                match compile_result {
                    Ok(()) => {
                        tracing::debug!("[JIT] compiled '{}' (id={})", function_name, function_id);
                        if let Some(result) =
                            self.try_jit_call(function_id, args).map_err(|msg| {
                                VmError::RuntimeError(
                                    RuntimeError::new(msg)
                                        .with_function(function_name.to_owned())
                                        .with_instruction_pointer(self.instruction_pointer),
                                )
                            })?
                        {
                            return Ok(result);
                        }
                    }
                    Err(err) => {
                        let reason = super::jit::jit_bailout_reason(&err);
                        tracing::debug!(
                            "[JIT] compilation skipped for '{}' (id={}, reason={}): {}",
                            function_name,
                            function_id,
                            reason,
                            err
                        );
                        super::jit::increment_jit_bailout_count();
                        if !super::jit::is_retryable_jit_bailout(&err) {
                            self.jit.mark_do_not_jit(function_id);
                        }
                    }
                }
            }
        }

        let program = Arc::clone(&self.program);
        let function_info = program.functions.get(function_id as usize).ok_or_else(|| {
            VmError::runtime(format!("Function with ID {} not found", function_id))
        })?;
        let function_name = &function_info.name;
        let function_arity = function_info.arity;
        let function_param_names = &function_info.param_names;
        let function_is_variadic = function_info.is_variadic;
        let function_has_instructions = !function_info.instructions.is_empty();
        let function_instructions_len = function_info.instructions.len();
        let function_source = &function_info.source;
        let function_flow_type = function_info.flow_type;

        // Fast path: Skip CallContext creation when no emitter is attached
        // This avoids UUID generation, timestamp calls, and call stack management
        let has_emitter = self.emitter.is_some();
        let call_id = if has_emitter {
            // Create call context for tracking (only when observability is needed)
            let parent_call_id = self.call_stack.last().map(|ctx| ctx.call_id);
            let call_depth = self.call_stack.len();
            let static_scope = function_name.to_owned();
            let call_context = CallContext::new(
                function_name.to_owned(),
                static_scope,
                parent_call_id,
                call_depth,
                self.run_start_time,
                self.run_start_instant,
            );
            let id = call_context.call_id;
            self.call_stack.push(call_context);
            Some(id)
        } else {
            None
        };

        // Emit call:start event (only when emitter is attached)
        if let (Some(emitter), Some(execution_context), Some(cid)) =
            (&self.emitter, &self.execution_context, call_id)
        {
            let runtime_path = self.build_runtime_path();
            let parent_call_id = self.call_stack.iter().rev().nth(1).map(|ctx| ctx.call_id);
            let call_depth = self.call_stack.len().saturating_sub(1);

            // Build flow value - prioritize function's defined flow type, fall back to flow context
            // Serial is the default, so we only emit flow info for parallel, cond, cond_all, and pipe
            let flow = if let Some(fn_flow_type) = function_flow_type {
                // Function is defined as a flow function (e.g., `fn cond`, `fn parallel`)
                let flow_type_str = match fn_flow_type {
                    FlowType::Serial => None, // Serial is default, no pill needed
                    FlowType::Parallel => Some("parallel"),
                    FlowType::Cond => Some("cond"),
                    FlowType::CondAll => Some("cond-all"),
                    FlowType::Pipe => Some("pipe"),
                    FlowType::Match => Some("match"),
                    FlowType::MatchAll => Some("match-all"),
                };
                flow_type_str.map(|flow_type_str| {
                    let mut map = IndexMap::new();
                    map.insert(Val::from("type"), Val::from(flow_type_str.to_string()));
                    map.insert(Val::from("fn"), Val::Bool(true)); // Mark as function-level flow
                    Val::Map(Box::new(map))
                })
            } else {
                // Check if we're inside an inline flow context
                self.flow_contexts.last().and_then(|ctx| {
                    let flow_type_str = match ctx.flow_type {
                        FlowType::Serial => return None, // Serial is default, no pill needed
                        FlowType::Parallel => "parallel",
                        FlowType::Cond => "cond",
                        FlowType::CondAll => "cond-all",
                        FlowType::Pipe => "pipe",
                        FlowType::Match => "match",
                        FlowType::MatchAll => "match-all",
                    };
                    let mut map = IndexMap::new();
                    map.insert(Val::from("type"), Val::from(flow_type_str.to_string()));
                    map.insert(Val::from("flow_id"), Val::from(ctx.flow_id.to_string()));
                    // Include branch name if we're inside a cond/match branch
                    if let Some(branch) = &ctx.current_branch {
                        map.insert(Val::from("branch"), Val::from(branch.clone()));
                    }
                    Some(Val::Map(Box::new(map)))
                })
            };

            let start_event = crate::lang::emitter::EngineEvent::call_start(
                execution_context,
                cid,
                parent_call_id,
                function_name.to_owned(),
                function_name.to_owned(), // static_scope
                runtime_path.unwrap_or_else(|| format!("run_{}", execution_context.run_id)),
                call_depth,
                args.to_vec(),
                function_source.as_ref(),
                self.call_stack
                    .last()
                    .map(|ctx| ctx.start_time)
                    .unwrap_or_else(chrono::Utc::now),
                flow,
            );
            emitter.emit(start_event);
        }

        {
            // Check arity (basic validation)
            if !function_is_variadic && args.len() != function_arity as usize {
                tracing::trace!(
                    "⚠️  VM: Argument mismatch for function '{}': expected {}, got {}",
                    function_name,
                    function_arity,
                    args.len()
                );

                // Be lenient with argument mismatches during test execution; the
                // adjustment below preserves current dispatch behavior.
                tracing::trace!(
                    "VM: Handling argument mismatch for function '{}' (expected {}, got {})",
                    function_name,
                    function_arity,
                    args.len()
                );
                // Continue with argument adjustment
            }

            // Ensure at least a global namespace scope exists (defensive)
            if self.scope_stack.is_empty() {
                self.scope_stack.push(LexicalScope::new(
                    crate::lang::bytecode::ScopeType::Namespace,
                    None,
                ));
                // Ensure ns exists for current namespace (avoid simultaneous borrow of self)
                let cur_ns = self.current_namespace.clone();
                let _ = self.ensure_namespace_has_ns_variable(&cur_ns);
            }

            // Create a new scope for function execution
            let mut function_scope = LexicalScope::new(
                crate::lang::bytecode::ScopeType::Function,
                Some(self.scope_stack.len()),
            );

            // Bind parameters to arguments (handle argument count mismatch gracefully)
            // For variadic functions, the last param collects all remaining args as a Vec
            let variadic_param_index = if function_is_variadic {
                function_param_names.len().saturating_sub(1)
            } else {
                usize::MAX // No variadic param
            };

            for (i, param_name) in function_param_names.iter().enumerate() {
                if function_is_variadic && i == variadic_param_index {
                    // This is the variadic parameter - collect all remaining args into a Vec
                    let variadic_args: Vec<Val> = if i < args.len() {
                        args[i..].to_vec()
                    } else {
                        vec![]
                    };
                    let variadic_len = variadic_args.len();
                    function_scope
                        .variables
                        .insert(param_name.clone(), Val::Vec(variadic_args));
                    tracing::trace!(
                        "VM: Bound variadic parameter '{}' with {} args (starting from index {})",
                        param_name,
                        variadic_len,
                        i
                    );
                } else if i < args.len() {
                    // Non-variadic parameter with a provided arg
                    function_scope
                        .variables
                        .insert(param_name.clone(), args[i].clone());
                } else {
                    // Missing argument - provide default value
                    function_scope
                        .variables
                        .insert(param_name.clone(), Val::Null);
                    tracing::trace!(
                        "VM: Using default value (Null) for missing parameter: {}",
                        param_name
                    );
                }
            }

            // Push the function scope
            let saved_scope_depth = self.scope_stack.len();
            self.scope_stack.push(function_scope);

            // Save the current namespace and switch to the function's namespace
            let saved_namespace = self.current_namespace.clone();
            let function_namespace = function_name
                .rfind('/')
                .map(|namespace_end| function_name[..namespace_end].to_string());

            // Snapshot of namespace-registry entries we shadow with the
            // function's parameters, so we can restore them after the call.
            // Without this, calling `::hot::coll/get(map: Map<K,V>, ...)`
            // leaves the *function* slot for `map` permanently overwritten
            // by the caller's Map literal, which then breaks any later
            // function in the same namespace that references `map`.
            let mut ns_param_shadows: Vec<(String, Option<Val>)> = Vec::new();

            if let Some(ref namespace) = function_namespace {
                tracing::trace!(
                    "VM: Switching to function namespace '{}' for function '{}'",
                    namespace,
                    function_name
                );
                self.current_namespace = namespace.clone();

                // Ensure the function's namespace has a ns variable pointing to itself
                self.ensure_namespace_has_ns_variable(namespace)?;

                // Mirror all current function parameters/locals into the namespace registry
                // so closures and VM-aware hotlibs can resolve them when needed.
                // We capture each prior value first so the call can be unwound
                // cleanly without leaking parameter bindings across invocations.
                if let Some(scope) = self.scope_stack.last() {
                    let ns_entry = self
                        .namespace_variables
                        .entry(self.current_namespace.clone())
                        .or_default();
                    for (k, v) in &scope.variables {
                        let prev = ns_entry.insert(k.clone(), v.clone());
                        ns_param_shadows.push((k.clone(), prev));
                    }
                }
            }

            // Execute function instructions
            // Snapshot register usage to avoid unbounded growth across many function calls
            let saved_register_count_for_call = self.registers.len();
            // Ensure capacity for this function
            let needed = function_info.register_count as usize;
            self.ensure_register_capacity(needed);
            // Prevent outer branch skipping from leaking into function execution
            let saved_skip_flag = self.skip_until_branch_end;
            self.skip_until_branch_end = None;
            let saved_skip_cond_flow = self.skip_remaining_cond_flow;
            self.skip_remaining_cond_flow = None;

            // Save flow context depth for TCO cleanup - tail calls may leave stale contexts
            let saved_flow_context_depth = self.flow_contexts.len();

            let result = if !function_has_instructions {
                // No instructions compiled yet - this is expected for now
                // Return a placeholder indicating execution occurred
                // Function has no instructions - return Null
                tracing::warn!("VM: Function '{}' has no instructions", function_name);
                Val::Null // Return Null instead of placeholder string
            } else {
                // Execute the actual function instructions
                // Function has instructions - execute them

                // Store function name for debug correlation
                self.current_debug_function = Some(function_name.to_owned());
                tracing::trace!(
                    "VM: Function '{}' has {} instructions",
                    function_name,
                    function_instructions_len
                );

                // TCO Loop: Execute instructions, and if we get a TailCall result,
                // update parameters and re-execute instead of recursing
                let mut current_args = args.to_vec();
                loop {
                    // Update parameter bindings with current args (for tail call iterations)
                    // On first iteration, this duplicates the initial binding, but that's okay
                    if let Some(scope) = self.scope_stack.last_mut() {
                        for (i, param_name) in function_param_names.iter().enumerate() {
                            if function_is_variadic && i == variadic_param_index {
                                // Variadic parameter - collect remaining args
                                let variadic_args: Vec<Val> = if i < current_args.len() {
                                    current_args[i..].to_vec()
                                } else {
                                    vec![]
                                };
                                scope
                                    .variables
                                    .insert(param_name.clone(), Val::Vec(variadic_args));
                            } else if i < current_args.len() {
                                scope
                                    .variables
                                    .insert(param_name.clone(), current_args[i].clone());
                            } else {
                                scope.variables.insert(param_name.clone(), Val::Null);
                            }
                        }
                    }

                    // Execute the function instructions - borrow from outer Arc clone
                    let instructions = &function_info.instructions;
                    match self.execute_function_instructions(instructions)? {
                        FunctionExecutionResult::Value(val) => {
                            // Normal return - exit the loop with the result
                            break val;
                        }
                        FunctionExecutionResult::TailCall {
                            function_id: tail_fn_id,
                            args: new_args,
                        } => {
                            // Tail call optimization: verify it's the same function
                            if tail_fn_id != function_id {
                                // Cross-function tail call not yet supported, fall back to regular call
                                tracing::trace!(
                                    "VM: Cross-function tail call from {} to {} - not yet optimized",
                                    function_id,
                                    tail_fn_id
                                );
                                // Execute as a regular call
                                break self.execute_user_function(tail_fn_id, &new_args)?;
                            }

                            // Same function - update args and loop (TCO!)
                            current_args = new_args;

                            // TCO cleanup: Reset flow contexts and skip flags from previous iteration
                            // The previous iteration may have left stale flow contexts if it exited
                            // via TailCall before EndFlow was executed (e.g., in cond flows)
                            self.flow_contexts.truncate(saved_flow_context_depth);
                            self.skip_until_branch_end = None;
                            self.skip_remaining_cond_flow = None;

                            // Continue the loop with updated arguments
                        }
                    }
                }
            };

            // Restore scope stack to pre-function depth.
            // This pops the function scope AND any scopes leaked by flow constructs
            // (cond, match, etc.) during instruction execution.
            self.scope_stack.truncate(saved_scope_depth);

            // Restore skip flags after function execution
            self.skip_until_branch_end = saved_skip_flag;
            self.skip_remaining_cond_flow = saved_skip_cond_flow;

            // Restore register usage to pre-call values to prevent accumulation
            if self.registers.len() > saved_register_count_for_call {
                self.registers.truncate(saved_register_count_for_call);
            }

            // Unwind the namespace-registry shadows installed for this
            // call's parameters. Restore prior bindings (or remove if there
            // was none) so caller-frame parameters never leak into siblings
            // or future calls in the same namespace.
            if let Some(ref namespace) = function_namespace
                && let Some(ns_entry) = self.namespace_variables.get_mut(namespace)
            {
                for (k, prev) in ns_param_shadows.into_iter().rev() {
                    match prev {
                        Some(v) => {
                            ns_entry.insert(k, v);
                        }
                        None => {
                            ns_entry.shift_remove(&k);
                        }
                    }
                }
            }

            // Restore the saved namespace
            tracing::trace!(
                "VM: Restoring namespace from '{}' to '{}'",
                self.current_namespace,
                saved_namespace
            );
            self.current_namespace = saved_namespace.clone();

            // Ensure the restored namespace has a ns variable pointing to itself
            self.ensure_namespace_has_ns_variable(&saved_namespace)?;

            // Only do duration calculation and event emission when emitter is attached
            if let (Some(emitter), Some(execution_context), Some(cid)) =
                (&self.emitter, &self.execution_context, call_id)
            {
                // Calculate duration before popping call context
                let (end_time, duration_us) = if let Some(call_ctx) = self.call_stack.last() {
                    let end = chrono::Utc::now();
                    let duration = (end - call_ctx.start_time).num_microseconds().unwrap_or(0);
                    (end, duration)
                } else {
                    (chrono::Utc::now(), 0)
                };

                // Pop call context from stack
                self.call_stack.pop();

                // Emit call:stop event
                let stop_event = crate::lang::emitter::EngineEvent::call_stop(
                    execution_context,
                    cid,
                    Some(result.clone()),
                    None, // No error since we're returning success
                    end_time,
                    duration_us,
                );
                emitter.emit(stop_event);
            }

            Ok(result)
        }
    }

    /// Convert value to string (safe for template literals to avoid recursion)
    pub fn value_to_string(&self, val: &Val) -> String {
        match val {
            Val::Str(s) => (**s).to_owned(),
            _ => val.to_string(),
        }
    }

    /// Look up a core variable by name (variables with core: true metadata)
    fn lookup_core_variable(&self, var_name: &str) -> Option<Val> {
        tracing::trace!(
            "VM: Looking up core variable '{}' in registry with {} entries",
            var_name,
            self.core_variables.len()
        );

        // Use the dedicated core variables registry for fast lookup
        if let Some(core_var_info) = self.core_variables.get(var_name) {
            tracing::trace!(
                "VM: Found core variable '{}' from namespace '{}': {:?}",
                var_name,
                core_var_info.namespace_path,
                core_var_info.variable_type
            );

            // Convert the AST Value to a runtime Val
            match &core_var_info.value {
                crate::lang::ast::Value::Val(val, _) => Some(val.clone()),
                crate::lang::ast::Value::Fn(_) => {
                    // For function values, return a function reference
                    // The actual function call will be handled by unified_function_lookup
                    Some(Val::from(format!(
                        "{}/{}",
                        core_var_info.namespace_path, var_name
                    )))
                }
                crate::lang::ast::Value::TypeDef(_) => {
                    // For TypeDef values (like Vec, Map, etc.), return a function reference
                    // that points to the hotlib implementation
                    Some(Val::from(format!(
                        "{}/{}",
                        core_var_info.namespace_path, var_name
                    )))
                }
                _ => {
                    tracing::trace!(
                        "VM: Core variable '{}' has unsupported value type: {:?}",
                        var_name,
                        core_var_info.value
                    );
                    None
                }
            }
        } else {
            tracing::trace!(
                "VM: Core variable '{}' not found. Registry has {} entries",
                var_name,
                self.core_variables.len()
            );
            None
        }
    }

    /// Convert AST Value to runtime Val
    pub fn convert_ast_value_to_runtime_val(
        &mut self,
        ast_value: &crate::lang::ast::Value,
    ) -> VmResult<Val> {
        match ast_value {
            crate::lang::ast::Value::Val(val, _metadata) => {
                // Direct Val conversion - resolve any variable references
                self.resolve_variable_references_in_val(val)
            }
            crate::lang::ast::Value::Fn(_function_defs) => Ok(Val::Null),
            crate::lang::ast::Value::FnCall(fn_call) => {
                // Execute the function call and return its result
                self.execute_fncall_ast(fn_call)
            }
            crate::lang::ast::Value::Ref(reference) => {
                // Reference - convert to appropriate runtime value
                match reference {
                    crate::lang::ast::Ref::Var(var_ref) => {
                        // Resolve variable value from VM and apply deep path if present
                        let name = var_ref.var.sym.name();
                        let mut value = match self.lookup_variable(name) {
                            Ok(v) => v,
                            Err(_) => self.unified_variable_lookup(name)?,
                        };

                        if let Some(deep_path) = &var_ref.var.deep_path {
                            let mut parts: Vec<crate::lang::ast::DeepPathPart> = Vec::new();
                            Self::collect_deep_path_parts(deep_path, &mut parts);
                            for part in parts {
                                match part {
                                    crate::lang::ast::DeepPathPart::Key(k) => {
                                        match value {
                                            Val::Map(ref m) => {
                                                value = m
                                                    .get(&Val::from(k.clone()))
                                                    .cloned()
                                                    .unwrap_or(Val::Null);
                                            }
                                            Val::Null => { /* remain null */ }
                                            _ => {
                                                return Err(VmError::runtime(format!(
                                                    "Cannot access key '{}' on non-map value",
                                                    k
                                                )));
                                            }
                                        }
                                    }
                                    crate::lang::ast::DeepPathPart::Index(i) => match value {
                                        Val::Vec(ref v) => {
                                            value = v.get(i).cloned().unwrap_or(Val::Null);
                                        }
                                        Val::Bytes(ref b) => {
                                            value = b
                                                .get(i)
                                                .map(|byte| Val::Int(*byte as i64))
                                                .unwrap_or(Val::Null);
                                        }
                                        _ => {
                                            return Err(VmError::runtime(format!(
                                                "Cannot access index {} on non-vector/bytes value",
                                                i
                                            )));
                                        }
                                    },
                                    crate::lang::ast::DeepPathPart::DynamicIndex(var_name) => {
                                        // Resolve the variable to get the actual index/key
                                        let index_val = match self.lookup_variable(&var_name) {
                                            Ok(v) => v,
                                            Err(_) => self.unified_variable_lookup(&var_name)?,
                                        };
                                        match (&value, &index_val) {
                                            (Val::Map(m), key) => {
                                                value = m.get(key).cloned().unwrap_or(Val::Null);
                                            }
                                            (Val::Vec(v), Val::Int(i)) => {
                                                if *i < 0 {
                                                    value = Val::Null;
                                                } else {
                                                    value = v
                                                        .get(*i as usize)
                                                        .cloned()
                                                        .unwrap_or(Val::Null);
                                                }
                                            }
                                            (Val::Str(s), Val::Int(i)) => {
                                                if *i < 0 {
                                                    value = Val::Null;
                                                } else {
                                                    value = s
                                                        .chars()
                                                        .nth(*i as usize)
                                                        .map(|c| Val::from(c.to_string()))
                                                        .unwrap_or(Val::Null);
                                                }
                                            }
                                            (Val::Bytes(b), Val::Int(i)) => {
                                                if *i < 0 {
                                                    value = Val::Null;
                                                } else {
                                                    value = b
                                                        .get(*i as usize)
                                                        .map(|byte| Val::Int(*byte as i64))
                                                        .unwrap_or(Val::Null);
                                                }
                                            }
                                            _ => {
                                                return Err(VmError::runtime(format!(
                                                    "Cannot access dynamic index {:?} on {:?}",
                                                    index_val, value
                                                )));
                                            }
                                        }
                                    }
                                    crate::lang::ast::DeepPathPart::Append => {
                                        // Append is only valid for setting, not for reading
                                        // This shouldn't normally be reached during read operations
                                    }
                                }
                            }
                        }
                        Ok(value)
                    }
                    crate::lang::ast::Ref::Ns(ns_ref) => {
                        // Namespace reference - try to resolve as a qualified variable first
                        let ns_path = ns_ref
                            .ns
                            .0
                            .iter()
                            .map(|part| match part {
                                crate::lang::ast::NsPathPart::Sym(sym) => match sym {
                                    crate::lang::ast::Sym::String(s) => s.clone(),
                                },
                            })
                            .collect::<Vec<_>>()
                            .join("::");
                        if let Some(name) = &ns_ref.function_name {
                            let qualified_var = format!("::{}/{}", ns_path, name);
                            if let Ok(val) = self.lookup_qualified_variable(&qualified_var) {
                                return Ok(val);
                            }
                            // If not a variable, return as string reference (e.g., function ref)
                            Ok(Val::from(qualified_var))
                        } else {
                            let qualified_ns = format!("::{}", ns_path);
                            Ok(Val::from(qualified_ns))
                        }
                    }
                }
            }
            _ => Ok(Val::Null),
        }
    }

    fn collect_deep_path_parts(
        dp: &crate::lang::ast::DeepPath,
        out: &mut Vec<crate::lang::ast::DeepPathPart>,
    ) {
        match dp {
            crate::lang::ast::DeepPath::Key(k) => {
                out.push(crate::lang::ast::DeepPathPart::Key(k.clone()))
            }
            crate::lang::ast::DeepPath::Index(i) => {
                out.push(crate::lang::ast::DeepPathPart::Index(*i))
            }
            crate::lang::ast::DeepPath::DynamicIndex(var) => {
                out.push(crate::lang::ast::DeepPathPart::DynamicIndex(var.clone()))
            }
            crate::lang::ast::DeepPath::Append => out.push(crate::lang::ast::DeepPathPart::Append),
            crate::lang::ast::DeepPath::Chain(a, b) => {
                Self::collect_deep_path_parts(a, out);
                Self::collect_deep_path_parts(b, out);
            }
        }
    }

    /// Get access to the namespace registry for VM-aware hotlib functions
    pub fn get_namespace_registry(&self) -> &crate::lang::bytecode::NamespaceRegistry {
        &self.program.namespaces
    }

    /// Get a snapshot of variables mirrored in the current namespace (for VM-aware libs)
    pub fn get_current_namespace_locals(&self) -> Option<&indexmap::IndexMap<String, Val>> {
        self.namespace_variables.get(&self.current_namespace)
    }

    /// Ensure a namespace has a ns variable pointing to itself
    pub(crate) fn ensure_namespace_has_ns_variable(
        &mut self,
        namespace_name: &str,
    ) -> VmResult<()> {
        // Check if the namespace already has a ns variable
        if let Some(namespace_vars) = self.namespace_variables.get(namespace_name)
            && namespace_vars.contains_key("ns")
        {
            // Already has ns variable, nothing to do
            return Ok(());
        }

        // Create a NamespaceRef for this namespace
        let namespace_ref = crate::lang::refs::NamespaceRef::new(namespace_name.to_string());
        let ns_value = Val::Box(Box::new(namespace_ref));

        // Store the ns variable in this namespace
        self.namespace_variables
            .entry(namespace_name.to_string())
            .or_default()
            .insert("ns".to_string(), ns_value.clone());

        Ok(())
    }

    /// Execute a parallel flow with TRUE parallel execution of thunks by dependency level
    fn execute_parallel_flow(&mut self, parallel_ctx: ParallelFlowContext) -> VmResult<()> {
        use parking_lot::Mutex;
        use std::sync::Arc as StdArc;
        use std::thread;

        let ParallelFlowContext {
            deferred_thunks,
            dependency_levels,
            metadata_captured: _,
        } = parallel_ctx;

        tracing::trace!(
            "VM: execute_parallel_flow starting with {} thunks across {} levels",
            deferred_thunks.len(),
            dependency_levels.len()
        );

        // Build a map of var_name -> thunk for quick lookup
        let thunk_map: AHashMap<String, Val> = deferred_thunks
            .into_iter()
            .map(|t| (t.var_name, t.thunk))
            .collect();

        // Clone VM state needed for parallel execution (same pattern as pmap)
        let program = self.program.clone();
        let hot_ast = self.get_hot_ast_arc();
        let function_mapping = self.get_function_mapping_arc();
        let core_functions = self.get_core_functions_arc();
        let type_implementations = self.get_type_implementations_arc();
        let core_variables = self.get_core_variables_arc();
        let conf = self.get_conf().clone();
        let shared_failure_state = self.get_failure_state_arc();

        // Track computed variables from previous levels to pass to task VMs
        let mut computed_vars: IndexMap<String, Val> = IndexMap::new();

        // Execute each dependency level in sequence, but variables within a level in parallel
        for (level_idx, level_vars) in dependency_levels.iter().enumerate() {
            if level_vars.is_empty() {
                continue;
            }

            tracing::trace!(
                "VM: Executing dependency level {} with {} variables: [{}]",
                level_idx,
                level_vars.len(),
                level_vars.join(", ")
            );

            // Collect thunks for this level that need execution
            let level_thunks: Vec<(String, Val)> = level_vars
                .iter()
                .filter_map(|var_name| {
                    thunk_map
                        .get(var_name)
                        .map(|thunk| (var_name.clone(), thunk.clone()))
                })
                .collect();

            if level_thunks.is_empty() {
                tracing::trace!(
                    "VM: No thunks to execute at level {} (variables from outer scope)",
                    level_idx
                );
                continue;
            }

            // If only one variable, execute sequentially (no thread overhead)
            if level_thunks.len() == 1 {
                let (var_name, thunk) = &level_thunks[0];
                tracing::trace!(
                    "VM: Single thunk at level {}, executing sequentially: '{}'",
                    level_idx,
                    var_name
                );

                // Push a temporary scope with computed_vars so thunks can find
                // variables from previous levels (shadowing outer scope variables)
                let mut temp_scope =
                    LexicalScope::new(ScopeType::Flow, Some(self.scope_stack.len()));
                for (name, val) in &computed_vars {
                    temp_scope.variables.insert(name.clone(), val.clone());
                    tracing::trace!(
                        "VM: Added computed_var '{}' = {:?} to temp_scope for parallel execution",
                        name,
                        val
                    );
                }
                tracing::trace!(
                    "VM: Pushed temp_scope with {} vars, scope_stack now has {} scopes",
                    temp_scope.variables.len(),
                    self.scope_stack.len() + 1
                );
                self.scope_stack.push(temp_scope);

                let result = self.execute_thunk(thunk)?;

                // Pop the temporary scope
                self.scope_stack.pop();

                self.store_variable(var_name, result.clone())?;

                // Track computed var for subsequent levels
                computed_vars.insert(var_name.clone(), result.clone());

                // Track in flow context
                if let Some(flow_context) = self.flow_contexts.last_mut() {
                    flow_context
                        .flow_variable_refs
                        .push((var_name.clone(), result));
                }
                continue;
            }

            // Multiple thunks - execute in TRUE parallel using thread pool pattern
            tracing::trace!(
                "VM: TRUE parallel execution of {} thunks using up to {} threads",
                level_thunks.len(),
                self.thread_count
            );

            // Get current namespace variables for task VMs
            let current_namespace_vars = self.get_namespace_vars(None).cloned().unwrap_or_default();

            // Get the current namespace so task VMs use the same one
            let main_namespace = self.current_namespace.clone();

            // Clone computed vars from previous levels for task VMs
            let computed_vars_snapshot = computed_vars.clone();

            // Prepare results storage - use AHashMap to preserve var names for ordered retrieval
            let results_mutex: StdArc<Mutex<AHashMap<String, Val>>> =
                StdArc::new(Mutex::new(AHashMap::with_capacity(level_thunks.len())));
            let error_mutex: StdArc<Mutex<Option<String>>> = StdArc::new(Mutex::new(None));

            // Keep track of definition order for this level
            let level_var_order: Vec<String> =
                level_thunks.iter().map(|(name, _)| name.clone()).collect();

            // Capture Tokio runtime handle so spawned threads can use async functions
            // (e.g. ::hot::box/run uses Handle::current().block_on())
            let tokio_handle = tokio::runtime::Handle::try_current().ok();

            // Spawn thread for each thunk in this level
            let mut join_handles = Vec::with_capacity(level_thunks.len());

            for (var_name, thunk) in level_thunks {
                let program_clone = program.clone();
                let hot_ast_clone = hot_ast.clone();
                let function_mapping_clone = function_mapping.clone();
                let core_functions_clone = core_functions.clone();
                let type_implementations_clone = type_implementations.clone();
                let core_variables_clone = core_variables.clone();
                let conf_clone = Some(conf.clone());
                let namespace_vars_clone = current_namespace_vars.clone();
                let namespace_clone = main_namespace.clone();
                let computed_vars_clone = computed_vars_snapshot.clone();
                let results_clone = results_mutex.clone();
                let error_clone = error_mutex.clone();
                let failure_state_clone = shared_failure_state.clone();
                let tokio_handle_clone = tokio_handle.clone();

                let var_name_for_label = var_name.clone();
                let spawn_result = thread::Builder::new()
                    .name(format!("parallel-{}", var_name))
                    .spawn(move || {
                    // Enter Tokio runtime context so async functions (e.g. ::hot::box/run) work
                    let _tokio_guard = tokio_handle_clone.as_ref().map(|h| h.enter());

                    // Wrap user-code execution in run_user_code so a panic from
                    // user-supplied Hot code becomes a structured UserCodePanic
                    // recorded in the shared error slot, rather than killing the
                    // worker process. Shared mutexes use parking_lot (no poisoning)
                    // so a panic never leaves them in an unusable state.
                    let label = format!("parallel-branch:{}", var_name_for_label);
                    let panic_result = crate::lang::user_code::run_user_code(&label, || {
                        if failure_state_clone
                            .failed
                            .load(std::sync::atomic::Ordering::SeqCst)
                        {
                            tracing::trace!(
                                "VM: Early exit for '{}' due to failure in another thread",
                                var_name
                            );
                            return;
                        }

                        tracing::trace!(
                            "VM: Thread starting TRUE execution of thunk for '{}' in namespace '{}'",
                            var_name,
                            namespace_clone
                        );

                        // Create a new VM for this thread
                        let mut task_vm = VirtualMachine::new(
                            program_clone,
                            hot_ast_clone,
                            function_mapping_clone,
                            core_functions_clone,
                            type_implementations_clone,
                            core_variables_clone,
                            conf_clone,
                        );

                        // Share the failure state
                        task_vm.failure_state = failure_state_clone;

                        // Set the same namespace as the main VM so variable lookups work
                        task_vm.current_namespace = namespace_clone;

                        // Restore namespace variables so thunks can access outer scope
                        for (name, val) in namespace_vars_clone {
                            task_vm.store_variable_public(&name, val).ok();
                        }

                        // Restore computed variables from previous levels
                        for (name, val) in computed_vars_clone {
                            task_vm.store_variable_public(&name, val).ok();
                        }

                        // Execute the thunk
                        match task_vm.execute_thunk(&thunk) {
                            Ok(result) => {
                                tracing::trace!(
                                    "VM: Thread completed thunk for '{}' = {:?}",
                                    var_name,
                                    result
                                );
                                results_clone.lock().insert(var_name, result);
                            }
                            Err(err) => {
                                tracing::error!(
                                    "VM: Thread failed executing thunk for '{}': {:?}",
                                    var_name,
                                    err
                                );
                                let mut error = error_clone.lock();
                                if error.is_none() {
                                    *error = Some(format!(
                                        "Parallel execution of '{}' failed: {}",
                                        var_name, err
                                    ));
                                }
                            }
                        }
                    });

                    // If user code panicked, surface the structured info as the
                    // branch's error (without re-panicking the thread).
                    if let Err(panic) = panic_result {
                        tracing::error!(
                            "VM: Parallel branch '{}' panicked: {}",
                            var_name_for_label,
                            panic.summary()
                        );
                        let mut error = error_clone.lock();
                        if error.is_none() {
                            *error = Some(format!(
                                "Parallel execution of '{}' panicked: {}",
                                var_name_for_label, panic
                            ));
                        }
                    }
                });

                match spawn_result {
                    Ok(handle) => join_handles.push(handle),
                    Err(e) => {
                        tracing::error!("VM: Failed to spawn parallel thread: {}", e);
                        let mut error = error_mutex.lock();
                        if error.is_none() {
                            *error = Some(format!("Failed to spawn parallel thread: {}", e));
                        }
                    }
                }
            }

            // Wait for all threads in this level to complete. Because the
            // closure no longer panics on user-code panics (they're captured
            // into error_mutex above), a JoinError here indicates a real bug
            // in the VM itself — surface it but don't crash.
            for handle in join_handles {
                if let Err(e) = handle.join() {
                    let panic_msg = if let Some(s) = e.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = e.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        format!("{:?}", e)
                    };
                    tracing::error!(
                        "VM: Thread panicked outside user-code boundary: {}",
                        panic_msg
                    );
                    return Err(VmError::runtime(format!(
                        "Parallel execution thread panicked: {}",
                        panic_msg
                    )));
                }
            }

            // Check for VM failure first (from fail() call)
            if let Some(failure) = self.get_failure() {
                return Err(VmError::runtime(format!(
                    "Parallel execution failed: {}",
                    failure.msg
                )));
            }

            // Check for errors
            if let Some(error_msg) = error_mutex.lock().as_ref() {
                return Err(VmError::runtime(error_msg.clone()));
            }

            // Merge results from this level into main VM in definition order
            let level_results = results_mutex.lock();
            for var_name in &level_var_order {
                if let Some(result) = level_results.get(var_name) {
                    self.store_variable(var_name, result.clone())?;

                    // Track computed var for subsequent levels
                    computed_vars.insert(var_name.clone(), result.clone());

                    // Track in flow context
                    if let Some(flow_context) = self.flow_contexts.last_mut() {
                        flow_context
                            .flow_variable_refs
                            .push((var_name.clone(), result.clone()));
                    }
                }
            }

            tracing::trace!(
                "VM: Completed TRUE parallel execution of dependency level {} ({} variables)",
                level_idx,
                level_vars.len()
            );
        }

        tracing::trace!("VM: execute_parallel_flow completed with TRUE parallel execution");
        Ok(())
    }

    /// Execute a thunk (zero-arg lambda) and return its result
    fn execute_thunk(&mut self, thunk: &Val) -> VmResult<Val> {
        // Execute the thunk (zero-arg lambda) directly
        // execute_lambda handles extracting LambdaInfo from the Val
        self.execute_lambda(thunk, &[])
    }

    /// Store a variable in the current scope or namespace registry
    fn store_variable(&mut self, name: &str, value: Val) -> VmResult<()> {
        tracing::trace!(
            "store_variable called - name='{}', scope_stack.len()={}, current_namespace='{}'",
            name,
            self.scope_stack.len(),
            self.current_namespace
        );
        // If we're at the top level (only one scope - the global namespace scope),
        // store in the namespace registry. Otherwise, store in lexical scope.
        self.current_namespace.contains("type-extensions");
        if self.scope_stack.is_empty() {
            // Extremely defensive: if no scope exists, treat as top-level namespace storage
            tracing::trace!(
                "VM: No scope available; storing '{}' in namespace '{}' as fallback",
                name,
                self.current_namespace
            );
            self.namespace_variables
                .entry(self.current_namespace.clone())
                .or_default()
                .insert(name.to_string(), value);
            Ok(())
        } else if self.scope_stack.len() == 1 {
            // Top-level namespace variable - store in global namespace registry
            tracing::trace!(
                "VM: Storing namespace variable '{}' in namespace '{}'",
                name,
                self.current_namespace
            );
            self.namespace_variables
                .entry(self.current_namespace.clone())
                .or_default()
                .insert(name.to_string(), value);
            Ok(())
        } else {
            // Lexical scope variable (function parameter, local variable)
            tracing::trace!("VM: Storing lexical variable '{}' in scope", name);
            if let Some(scope) = self.scope_stack.last_mut() {
                scope.variables.insert(name.to_string(), value);
                // Mirror local variable into current namespace registry for resilient lookups within flows
                self.namespace_variables
                    .entry(self.current_namespace.clone())
                    .or_default()
                    .insert(
                        name.to_string(),
                        scope.variables.get(name).cloned().unwrap_or(Val::Null),
                    );
                Ok(())
            } else {
                Err(VmError::runtime(
                    "No scope available for variable storage".to_string(),
                ))
            }
        }
    }

    /// Look up a variable at a specific depth
    pub(crate) fn lookup_variable_at_depth(&self, name: &str, _depth: usize) -> Option<Val> {
        // For now, just look up in current scope
        if let Some(scope) = self.scope_stack.last() {
            scope.variables.get(name).cloned()
        } else {
            None
        }
    }

    /// Get the destination register of an instruction if it produces a value
    fn get_instruction_dest(&self, instruction: &Instruction) -> Option<RegisterId> {
        match instruction {
            Instruction::LoadConst { dest, .. }
            | Instruction::Move { dest, .. }
            | Instruction::Add { dest, .. }
            | Instruction::Call { dest, .. }
            | Instruction::CallNative { dest, .. }
            | Instruction::LoadVar { dest, .. }
            | Instruction::LoadVarOrDefault { dest, .. }
            | Instruction::LoadScoped { dest, .. }
            | Instruction::CaptureVar { dest, .. }
            | Instruction::LoadFunctionRef { dest, .. }
            | Instruction::EndFlow { dest, .. }
            | Instruction::Pipe { dest, .. }
            | Instruction::DefineFunction { dest, .. }
            | Instruction::DotAccess { dest, .. }
            | Instruction::DotAccessOrDefault { dest, .. }
            | Instruction::GetTypePath { dest, .. }
            | Instruction::IsType { dest, .. }
            | Instruction::TemplateInterpolate { dest, .. }
            | Instruction::CallLibBuiltin { dest, .. }
            | Instruction::CallUserFunction { dest, .. }
            | Instruction::CallLambda { dest, .. }
            | Instruction::ExtractInnerVal { dest, .. }
            | Instruction::ConstructTyped { dest, .. } => Some(*dest),
            _ => None,
        }
    }

    /// Set the VM failure state (thread-safe, first failure wins)
    /// Returns true if this was the first failure, false if already failed
    pub fn set_failure(&self, msg: String, data: Val) -> bool {
        // Use compare_exchange to atomically check and set failed flag
        if self
            .failure_state
            .failed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // We're the first failure - store our failure state
            if let Ok(mut failure) = self.failure_state.failure.write() {
                *failure = Some(FailureState { msg, data });
            }
            tracing::trace!("VM: Failure state set (first failure)");
            true
        } else {
            // Already failed - log but don't override
            tracing::trace!("VM: Additional failure encountered (first failure takes precedence)");
            false
        }
    }

    /// Check if the VM has failed
    pub fn has_failed(&self) -> bool {
        self.failure_state.failed.load(Ordering::SeqCst)
    }

    /// Get the failure state if failed
    pub fn get_failure(&self) -> Option<FailureState> {
        if self.has_failed() {
            self.failure_state.failure.read().ok()?.clone()
        } else {
            None
        }
    }

    /// Clear the failure state (used between test runs to prevent state leakage)
    pub fn reset_failure_state(&self) {
        self.failure_state.failed.store(false, Ordering::SeqCst);
        if let Ok(mut failure) = self.failure_state.failure.write() {
            *failure = None;
        }
    }

    /// Get a clone of the failure state Arc (for parallel execution)
    pub fn get_failure_state_arc(&self) -> Arc<VmFailureState> {
        self.failure_state.clone()
    }

    /// Set the VM cancellation state (thread-safe, first cancellation wins)
    /// Returns true if this was the first cancellation, false if already cancelled
    pub fn set_cancellation(&self, msg: String, data: Val) -> bool {
        // Use compare_exchange to atomically check and set cancelled flag
        if self
            .cancellation_state
            .cancelled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // We're the first cancellation - store our cancellation state
            if let Ok(mut cancellation) = self.cancellation_state.cancellation.write() {
                *cancellation = Some(CancellationState { msg, data });
            }
            tracing::trace!("VM: Cancellation state set (first cancellation)");
            true
        } else {
            // Already cancelled - log but don't override
            tracing::warn!(
                "VM: Additional cancellation encountered (first cancellation takes precedence)"
            );
            false
        }
    }

    /// Clear the cancellation state (used between test runs to prevent state leakage)
    pub fn reset_cancellation_state(&self) {
        self.cancellation_state
            .cancelled
            .store(false, Ordering::SeqCst);
        if let Ok(mut cancellation) = self.cancellation_state.cancellation.write() {
            *cancellation = None;
        }
    }

    /// Check if the VM has been cancelled
    pub fn has_cancelled(&self) -> bool {
        self.cancellation_state.cancelled.load(Ordering::SeqCst)
    }

    /// Get the cancellation state if cancelled
    pub fn get_cancellation(&self) -> Option<CancellationState> {
        if self.has_cancelled() {
            self.cancellation_state.cancellation.read().ok()?.clone()
        } else {
            None
        }
    }

    /// Get a clone of the cancellation state Arc (for parallel execution)
    pub fn get_cancellation_state_arc(&self) -> Arc<VmCancellationState> {
        self.cancellation_state.clone()
    }

    /// Reset VM state for clean execution (useful between test runs)
    pub fn reset_state(&mut self) {
        // Clear accumulated namespace variables (but keep core ones)
        self.namespace_variables.clear();

        // Clear namespace registry
        self.namespace_registry.clear();

        // Reset scope stack to just the global namespace scope
        self.scope_stack = vec![LexicalScope::new(
            crate::lang::bytecode::ScopeType::Namespace,
            None,
        )];

        // Clear all registers to prevent accumulation, and shrink back to initial capacity
        if self.registers.len() > self.initial_register_capacity {
            self.registers.truncate(self.initial_register_capacity);
        }
        for register in &mut self.registers {
            *register = Val::Null;
        }

        // Reset execution state
        self.instruction_pointer = 0;
        self.function_call_depth = 0;
        self.call_function_depth = 0;
        self.template_recursion_depth = 0;
        self.skip_until_branch_end = None;
        self.skip_remaining_cond_flow = None;

        // Clear flow contexts
        self.flow_contexts.clear();

        // Dispatch cache is NOT cleared — it's valid across runs for the same program
        // (function mappings are immutable after compilation)

        // Clear context storage and secret keys
        self.context_storage.clear();
        self.secret_keys.clear();
        self.secret_value_hashes.clear();

        // Reset current namespace
        self.current_namespace = "::hot::main".to_string();

        // Reset failure state
        self.failure_state.failed.store(false, Ordering::SeqCst);
        if let Ok(mut failure) = self.failure_state.failure.write() {
            *failure = None;
        }

        // Reset cancellation state
        self.cancellation_state
            .cancelled
            .store(false, Ordering::SeqCst);
        if let Ok(mut cancellation) = self.cancellation_state.cancellation.write() {
            *cancellation = None;
        }

        // Re-ensure the default namespace has a ns variable
        let _ = self.ensure_namespace_has_ns_variable("::hot::main");

        tracing::trace!("VM state reset for clean execution");
    }

    /// Store a variable at a specific depth
    fn store_variable_at_depth(&mut self, name: &str, value: Val, _depth: usize) -> VmResult<()> {
        // For now, just store in current scope
        self.store_variable(name, value)
    }

    /// Set a property of a value (supports both map keys and vector indices)
    pub(crate) fn set_property(
        &self,
        value: &mut Val,
        property: &str,
        new_value: Val,
    ) -> VmResult<()> {
        match value {
            Val::Map(map) => {
                // Map assignment: set the property as a string key
                map.insert(Val::from(property.to_string()), new_value);
                Ok(())
            }
            Val::Vec(vec) => {
                // Vector assignment: try to parse property as index
                match property.parse::<usize>() {
                    Ok(index) => {
                        if index < vec.len() {
                            vec[index] = new_value;
                            Ok(())
                        } else {
                            Err(VmError::runtime(format!(
                                "Vector index {} out of bounds (length: {})",
                                index,
                                vec.len()
                            )))
                        }
                    }
                    Err(_) => Err(VmError::runtime(format!(
                        "Cannot set property '{}' on vector - property must be a valid index",
                        property
                    ))),
                }
            }
            Val::Null => {
                // Convert null to map and set property
                let mut new_map = indexmap::IndexMap::new();
                new_map.insert(Val::from(property.to_string()), new_value);
                *value = Val::Map(Box::new(new_map));
                Ok(())
            }
            _ => Err(VmError::runtime(format!(
                "Cannot set property '{}' on value of type {:?}",
                property,
                std::mem::discriminant(value)
            ))),
        }
    }

    /// Access a property of a value (supports both map keys and vector indices)
    pub fn access_property(&self, value: &Val, property: &str) -> VmResult<Val> {
        match value {
            Val::Map(map) => {
                // Check if this is a function alias that needs to be resolved first
                if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                    && &**type_name == "::hot::type/FunctionAlias"
                    && let Some(Val::Str(target)) = map.get(&Val::from("$target"))
                {
                    // Try to resolve the function alias target as a qualified variable first
                    let qualified_target = if target.starts_with("::") {
                        target.to_string()
                    } else {
                        format!("::{}", target)
                    };
                    match self.lookup_qualified_variable_immutable(&qualified_target) {
                        Some(resolved_value) => {
                            return self.access_property(&resolved_value, property);
                        }
                        None => {
                            // If not found as a variable, it might be a function
                            // For now, return an error - this case should be handled differently
                            return Err(VmError::runtime(format!(
                                "Function alias target '{}' not found as variable",
                                qualified_target
                            )));
                        }
                    }
                }

                // If this is a typed object with unified pattern, unwrap $val for property access
                // BUT: never unwrap for $type or $val - these should always be read from the outer wrapper
                if property != "$type"
                    && property != "$val"
                    && let Some(inner_val) = map.get(&Val::from("$val"))
                    && let Val::Map(inner_map) = inner_val
                    && let Some(prop_val) = inner_map.get(&Val::from(property.to_string()))
                {
                    return Ok(prop_val.clone());
                }

                // Map access: look up the property as a string key
                if let Some(prop_val) = map.get(&Val::from(property.to_string())) {
                    Ok(prop_val.clone())
                } else {
                    Ok(Val::Null)
                }
            }
            Val::Vec(vec) => {
                // Vector access: try to parse property as an index
                if let Ok(index) = property.parse::<usize>() {
                    if index < vec.len() {
                        Ok(vec[index].clone())
                    } else {
                        Ok(Val::Null) // Index out of bounds returns null
                    }
                } else {
                    // Property is not a valid index
                    Ok(Val::Null)
                }
            }
            Val::Str(s) => {
                // String access: try to parse property as an index for character access
                if let Ok(index) = property.parse::<usize>() {
                    let chars: Vec<char> = s.chars().collect();
                    if index < chars.len() {
                        Ok(Val::from(chars[index].to_string()))
                    } else {
                        Ok(Val::Null) // Index out of bounds returns null
                    }
                } else {
                    // Property is not a valid index
                    Ok(Val::Null)
                }
            }
            Val::Bytes(bytes) => {
                // Bytes access: try to parse property as an index
                if let Ok(index) = property.parse::<usize>() {
                    if index < bytes.len() {
                        Ok(Val::Int(bytes[index] as i64))
                    } else {
                        Ok(Val::Null) // Index out of bounds returns null
                    }
                } else {
                    // Property is not a valid index
                    Ok(Val::Null)
                }
            }
            Val::Null => {
                // Accessing properties on null returns null (like optional chaining)
                Ok(Val::Null)
            }
            Val::Box(boxed) => {
                // Check if this is a TypeRef (variant union type)
                if let Some(type_ref) = boxed.as_any().downcast_ref::<crate::lang::refs::TypeRef>()
                {
                    // Accessing a property on a TypeRef returns the variant value or constructor
                    if let Some(variant_type_ref) = type_ref.get_variant(property) {
                        if variant_type_ref.is_none() {
                            // Unit variant: return the constructed value directly
                            // {$type: "::namespace/Type.Variant"}
                            let full_type = format!("{}.{}", type_ref.name, property);
                            let mut result_map = indexmap::IndexMap::new();
                            result_map.insert(Val::from("$type"), Val::from(full_type));
                            return Ok(Val::Map(Box::new(result_map)));
                        } else {
                            // Type-reference variant: return a function reference to the constructor
                            let constructor_name = format!("{}.{}", type_ref.name, property);
                            let fn_ref =
                                crate::lang::runtime::function_ref::function_ref(constructor_name);
                            return Ok(fn_ref);
                        }
                    } else {
                        return Err(VmError::runtime(format!(
                            "Type '{}' does not have variant '{}'",
                            type_ref.name, property
                        )));
                    }
                }
                // Other boxed types don't support property access
                Ok(Val::Null)
            }
            _ => {
                // Other types don't support property access
                Ok(Val::Null)
            }
        }
    }

    /// Convert a hotlib `HotResult::Err` payload into a `VmResult` honoring
    /// fail/cancel halting semantics.
    ///
    /// - If the VM has entered fail or cancel state (i.e. `::hot::exec/fail`
    ///   or `::hot::exec/cancel` was just called), halt execution by
    ///   returning `Err(VmError)`. The state is left set so the top-level
    ///   run-loop sees `has_failed()`/`has_cancelled()` and avoids emitting
    ///   a duplicate `run:fail`/`run:cancel` event (the primitive already
    ///   emitted it). Boundary helpers like `::hot::lang/try-call` and
    ///   tool dispatch reset the state when they catch the halt.
    /// - Otherwise wrap the error as a `Result.Err` value so user code can
    ///   inspect it via `is-err`/`match`. This preserves the existing
    ///   contract for non-halting hotlib errors (e.g. parse errors, HTTP
    ///   non-2xx, etc.).
    fn dispatch_hotlib_err(&mut self, err: Val) -> VmResult<Val> {
        if self.has_failed() {
            let msg = self
                .get_failure()
                .map(|f| f.msg)
                .unwrap_or_else(|| match &err {
                    Val::Str(s) => (**s).to_owned(),
                    other => format!("{:?}", other),
                });
            return Err(VmError::runtime_with_ip(msg, self.instruction_pointer));
        }
        if self.has_cancelled() {
            let msg = self
                .get_cancellation()
                .map(|c| c.msg)
                .unwrap_or_else(|| match &err {
                    Val::Str(s) => (**s).to_owned(),
                    other => format!("{:?}", other),
                });
            return Err(VmError::runtime_with_ip(msg, self.instruction_pointer));
        }
        Ok(Val::err(err))
    }

    /// Execute a call-lib instruction using the new enum-based hotlib system
    pub fn execute_call_lib(&mut self, function_name: &str, args: &[Val]) -> VmResult<Val> {
        tracing::trace!(
            "VM: execute_call_lib calling '{}' with {} args: {:?}",
            function_name,
            args.len(),
            args
        );

        // Get the hotlib map
        let hotlib_map = crate::lang::hot::get_hotlib_map();

        // Try to find the function with namespace fallback
        let lib_fn = hotlib_map.get(function_name).or_else(|| {
            // Try with namespace prefix if not already present
            let namespaced_name = if !function_name.starts_with("::") {
                if function_name.contains("/") {
                    // Function already has namespace like "hot::lang/namespaces"
                    format!("::{}", function_name)
                } else {
                    // Simple function name, add default namespace
                    format!("::hot::{}", function_name)
                }
            } else {
                function_name.to_string()
            };
            hotlib_map.get(&namespaced_name)
        });

        if let Some(lib_fn) = lib_fn {
            // Dispatch based on function type encoded in the enum
            let result = match lib_fn {
                crate::lang::hot::HotLibFn::LibFn(func) => {
                    // Regular function - call directly
                    func(args)
                }
                crate::lang::hot::HotLibFn::VmAwareFn(func)
                | crate::lang::hot::HotLibFn::VmAwareJitFn(func, _) => {
                    // VM-aware function - pass mutable reference to VM
                    func(self, args)
                }
            };

            match result {
                crate::lang::hot::r#type::HotResult::Ok(val) => {
                    tracing::trace!(
                        "VM: Hotlib function '{}' returned: {:?}",
                        function_name,
                        val
                    );
                    return Ok(val);
                }
                crate::lang::hot::r#type::HotResult::Err(err) => {
                    tracing::trace!("VM: Hotlib function '{}' failed: {:?}", function_name, err);
                    return self.dispatch_hotlib_err(err);
                }
            }
        }

        // call-lib should ONLY call hotlib functions, never compiled user functions
        // This ensures proper separation between hotlib and compiled function namespaces

        tracing::trace!(
            "VM: Unknown lib function '{}', returning null",
            function_name
        );
        Ok(Val::Null)
    }

    /// Execute function instructions in a separate context
    /// Returns either a final value or a tail call request
    fn execute_function_instructions(
        &mut self,
        instructions: &[Instruction],
    ) -> Result<FunctionExecutionResult, VmError> {
        if instructions.is_empty() {
            return Ok(FunctionExecutionResult::Value(Val::Null));
        }

        // Execute function instructions
        tracing::trace!(
            "VM: execute_function_instructions called with {} instructions",
            instructions.len()
        );

        // Only show trace for test functions to reduce noise
        if instructions.len() == 1 {
            tracing::trace!("VM: Executing {} function instructions", instructions.len());
            for (i, instruction) in instructions.iter().enumerate() {
                tracing::trace!("VM: Instruction {}: {:?}", i, instruction);
                // Show what constant is being loaded
                if let Instruction::LoadConst { constant, .. } = instruction
                    && let Some(const_val) = self.program.constants.get(*constant as usize)
                {
                    tracing::trace!("VM: Loading constant {}: {:?}", constant, const_val);
                }
            }
        }

        // Save the current execution context
        let saved_instruction_pointer = self.instruction_pointer;

        // Execute each instruction in the function using a proper instruction pointer
        // This allows jump instructions to modify control flow
        let mut last_result = Val::Null;
        // Track base flow depth for this function to avoid cross-function mismatches
        let base_flow_depth = self.flow_contexts.len();

        // Detect fusible HOF pipelines once for this function body. Gated behind
        // the `jit.hof.fusion` kill switch; empty when disabled or none found.
        let fused_plans: Vec<crate::lang::runtime::jit_hof::PipelinePlan> =
            if self.jit.config.hof_fusion_enabled() {
                let program = self.program.clone();
                crate::lang::runtime::jit_hof::detect_pipelines(instructions, &program)
            } else {
                Vec::new()
            };

        // Use a local instruction pointer for function execution
        let mut func_ip: usize = 0;

        while func_ip < instructions.len() {
            let instruction = &instructions[func_ip];
            // Set instruction pointer for debugging
            self.instruction_pointer = func_ip;

            // Check if we're skipping instructions until we reach a specific CondBranchEnd
            // This is critical for TCO: we must not execute TailCall instructions in skipped branches
            // Track both branch_name and flow depth to prevent cross-flow interference
            if let Some((skip_branch_id, skip_flow_depth)) = self.skip_until_branch_end {
                let current_depth = self.flow_contexts.len();
                // Only skip if we're at the same flow depth
                if current_depth < skip_flow_depth {
                    // We've exited the flow that set the skip - clear it
                    self.skip_until_branch_end = None;
                } else if current_depth == skip_flow_depth {
                    match instruction {
                        Instruction::CondBranchEnd { branch_name, .. } => {
                            if *branch_name == skip_branch_id {
                                // We've reached the end of the branch we were skipping
                                tracing::trace!(
                                    "VM: [func] Reached end of skipped branch (id={})",
                                    branch_name
                                );
                                self.skip_until_branch_end = None;
                                // Continue to next instruction without executing this one
                                func_ip += 1;
                                continue;
                            }
                        }
                        _ => {
                            // Skip this instruction
                            func_ip += 1;
                            continue;
                        }
                    }
                }
                // If current_depth > skip_flow_depth, we're in a nested flow - don't skip
            }

            // Skip remaining cond/match branches (conditions + bodies) until EndFlow
            if let Some(nested_depth) = self.skip_remaining_cond_flow {
                match instruction {
                    Instruction::BeginFlow { .. } => {
                        self.skip_remaining_cond_flow = Some(nested_depth + 1);
                        func_ip += 1;
                        continue;
                    }
                    Instruction::EndFlow { .. } => {
                        if nested_depth > 0 {
                            self.skip_remaining_cond_flow = Some(nested_depth - 1);
                            func_ip += 1;
                            continue;
                        } else {
                            // Target EndFlow - stop skipping and execute it
                            self.skip_remaining_cond_flow = None;
                        }
                    }
                    _ => {
                        func_ip += 1;
                        continue;
                    }
                }
            }

            // Honor explicit function returns: stop executing further instructions
            if let Instruction::Return { value } = instruction {
                // Processing Return instruction
                tracing::trace!("VM: Processing Return instruction with register {}", value);
                match self.get_register(*value) {
                    Ok(val) => {
                        last_result = val.clone();
                        // Return value retrieved successfully
                        tracing::trace!(
                            "VM: Return value successfully retrieved: {:?}",
                            last_result
                        );
                    }
                    Err(e) => {
                        last_result = Val::Null;
                        // Failed to get return register
                        tracing::warn!("VM: Failed to get return register {}: {:?}", value, e);
                    }
                }
                // Restore instruction pointer before returning
                self.instruction_pointer = saved_instruction_pointer;
                return Ok(FunctionExecutionResult::Value(last_result));
            }

            // Handle conditional error return: if the value is an error, return it immediately
            if let Instruction::ReturnIfErr { src } = instruction {
                let val = self.get_register(*src)?.clone();
                if val.is_err() {
                    // Value is an error - return it immediately
                    tracing::trace!("VM: ReturnIfErr - value is error, returning immediately");
                    self.instruction_pointer = saved_instruction_pointer;
                    return Ok(FunctionExecutionResult::Value(val));
                }
                // Not an error - continue to next instruction
                func_ip += 1;
                continue;
            }

            // Handle tail call optimization: signal to caller to restart with new args
            if let Instruction::TailCall {
                function_id,
                args_start,
                args_count,
            } = instruction
            {
                // Collect new argument values from registers
                let mut new_args = Vec::with_capacity(*args_count as usize);
                for i in 0..*args_count {
                    let arg_reg = args_start + i as RegisterId;
                    let arg_val = self.get_register(arg_reg)?.clone();
                    // Unwrap Results if they are "ok"
                    let unwrapped_arg = self.unwrap_result_if_ok(&arg_val)?;
                    // Resolve variable references
                    let resolved_arg = self.resolve_variable_references_in_val(&unwrapped_arg)?;
                    new_args.push(resolved_arg);
                }

                // Restore instruction pointer before returning tail call request
                self.instruction_pointer = saved_instruction_pointer;
                return Ok(FunctionExecutionResult::TailCall {
                    function_id: *function_id,
                    args: new_args,
                });
            }

            // Honor skip-until-branch-end semantics inside function execution
            // Track both branch_name and flow depth to prevent cross-flow interference
            if let Some((skip_branch, skip_depth)) = &self.skip_until_branch_end {
                let current_depth = self.flow_contexts.len();
                // Only skip if we're at the same flow depth
                if current_depth == *skip_depth {
                    // If we're skipping, only process the matching CondBranchEnd; skip others
                    if let Instruction::CondBranchEnd { branch_name, .. } = instruction {
                        if branch_name == skip_branch {
                            // Execute this to clear the skip flag using normal machinery
                            match self.execute_instruction(instruction) {
                                Ok(()) => {
                                    if let Some(dest) = self.get_instruction_dest(instruction)
                                        && let Ok(val) = self.get_register(dest)
                                    {
                                        last_result = val.clone();
                                    }
                                }
                                Err(e) => {
                                    self.instruction_pointer = saved_instruction_pointer;
                                    return Err(e);
                                }
                            }
                        }
                        // Advance to next instruction
                        func_ip += 1;
                        continue;
                    } else {
                        // Skip non-CondBranchEnd instructions while waiting for the end of the branch
                        func_ip += 1;
                        continue;
                    }
                }
                // If at different depth, don't skip - let nested flows execute normally
            }

            // Skip remaining cond/match branches (conditions + bodies) until EndFlow
            if let Some(nested_depth) = self.skip_remaining_cond_flow {
                match instruction {
                    Instruction::BeginFlow { .. } => {
                        self.skip_remaining_cond_flow = Some(nested_depth + 1);
                        func_ip += 1;
                        continue;
                    }
                    Instruction::EndFlow { .. } => {
                        if nested_depth > 0 {
                            self.skip_remaining_cond_flow = Some(nested_depth - 1);
                            func_ip += 1;
                            continue;
                        } else {
                            // Target EndFlow - stop skipping, fall through to execute
                            self.skip_remaining_cond_flow = None;
                        }
                    }
                    _ => {
                        func_ip += 1;
                        continue;
                    }
                }
            }

            // Guard: if an EndFlow appears with no matching BeginFlow in this function context,
            // propagate the last_result to the dest register so that subsequent Return instructions
            // can pick up the correct value.
            if let Instruction::EndFlow { dest } = instruction
                && self.flow_contexts.len() <= base_flow_depth
            {
                // No flow context opened within this function – propagate last_result to dest
                // This handles the case where a previous EndFlow set the correct result,
                // but this orphan EndFlow's dest register needs to be updated for Return to work.
                self.set_register(*dest, last_result.clone())?;
                // last_result remains unchanged
                func_ip += 1;
                continue;
            }

            // HOF pipeline fusion: when execution reaches the first stage of a
            // recognized pure pipeline, run the whole chain in one fused pass and
            // skip the original stage instructions. The fused run is pure and
            // writes nothing until it succeeds, so a guard failure (`Deopt`)
            // safely falls through to normal instruction-by-instruction execution.
            if !fused_plans.is_empty()
                && let Some(plan) = fused_plans.iter().find(|p| p.first_stage_ip == func_ip)
            {
                let fused_result = if let Some(range) = plan.range_source {
                    let mut args = Vec::with_capacity(range.args_count as usize);
                    for i in 0..range.args_count {
                        args.push(self.get_register(range.args_start + u32::from(i))?.clone());
                    }
                    crate::lang::runtime::jit_hof::run_pipeline_range(plan, &args)
                } else {
                    let source = self.get_register(plan.source_reg)?.clone();
                    crate::lang::runtime::jit_hof::run_pipeline(plan, &source)
                };
                match fused_result {
                    crate::lang::runtime::jit_hof::FusedRun::Produced(result) => {
                        crate::lang::runtime::jit_hof::record_fused_run();
                        self.set_register(plan.result_reg, result.clone())?;
                        last_result = result;
                        func_ip = plan.last_stage_ip + 1;
                        continue;
                    }
                    crate::lang::runtime::jit_hof::FusedRun::Deopt => {
                        // Fall through to execute the original instructions.
                        crate::lang::runtime::jit_hof::record_fused_deopt();
                    }
                }
            }

            // Handle jump instructions specially - they control the instruction pointer
            match instruction {
                Instruction::Jump { offset } => {
                    func_ip = *offset as usize;
                    continue; // Skip the normal instruction pointer increment
                }
                Instruction::JumpIf { condition, offset } => {
                    let cond_val = self.get_register(*condition)?;
                    if cond_val.is_truthy() {
                        func_ip = *offset as usize;
                        continue;
                    } else {
                        func_ip += 1;
                        continue;
                    }
                }
                Instruction::JumpIfNot { condition, offset } => {
                    let cond_val = self.get_register(*condition)?;
                    if !cond_val.is_truthy() {
                        func_ip = *offset as usize;
                        continue;
                    } else {
                        func_ip += 1;
                        continue;
                    }
                }
                Instruction::Eq { dest, left, right } => {
                    let left_val = self.get_register(*left)?;
                    let right_val = self.get_register(*right)?;
                    let result = Val::Bool(left_val == right_val);
                    self.set_register(*dest, result.clone())?;
                    last_result = result;
                    func_ip += 1;
                    continue;
                }
                Instruction::StrEndsWith {
                    dest,
                    string,
                    suffix,
                } => {
                    let string_val = self.get_register(*string)?;
                    let suffix_val = self.get_register(*suffix)?;
                    let result = match (&string_val, &suffix_val) {
                        (Val::Str(s), Val::Str(suffix_s)) => Val::Bool(s.ends_with(&**suffix_s)),
                        _ => Val::Bool(false),
                    };
                    self.set_register(*dest, result.clone())?;
                    last_result = result;
                    func_ip += 1;
                    continue;
                }
                Instruction::StrStartsWith {
                    dest,
                    string,
                    prefix,
                } => {
                    let string_val = self.get_register(*string)?;
                    let prefix_val = self.get_register(*prefix)?;
                    let result = match (&string_val, &prefix_val) {
                        (Val::Str(s), Val::Str(prefix_s)) => Val::Bool(s.starts_with(&**prefix_s)),
                        _ => Val::Bool(false),
                    };
                    self.set_register(*dest, result.clone())?;
                    last_result = result;
                    func_ip += 1;
                    continue;
                }
                _ => {}
            }

            match self.execute_instruction(instruction) {
                Ok(()) => {
                    // For functions, we typically want the result of the last instruction
                    // For now, we'll use a simple heuristic to get the result
                    match instruction {
                        Instruction::LoadConst { dest, .. }
                        | Instruction::Move { dest, .. }
                        | Instruction::Add { dest, .. }
                        | Instruction::Call { dest, .. }
                        | Instruction::CallNative { dest, .. }
                        | Instruction::LoadVar { dest, .. }
                        | Instruction::LoadVarOrDefault { dest, .. }
                        | Instruction::LoadScoped { dest, .. }
                        | Instruction::CaptureVar { dest, .. }
                        | Instruction::LoadFunctionRef { dest, .. }
                        | Instruction::EndFlow { dest, .. }
                        | Instruction::Pipe { dest, .. }
                        | Instruction::DefineFunction { dest, .. }
                        | Instruction::DotAccess { dest, .. }
                        | Instruction::DotAccessOrDefault { dest, .. }
                        | Instruction::GetTypePath { dest, .. }
                        | Instruction::IsType { dest, .. }
                        | Instruction::TemplateInterpolate { dest, .. }
                        | Instruction::CallLibBuiltin { dest, .. }
                        | Instruction::CallUserFunction { dest, .. }
                        | Instruction::ExtractInnerVal { dest, .. }
                        | Instruction::ConstructTyped { dest, .. } => {
                            // Get the result from the destination register
                            if let Ok(val) = self.get_register(*dest) {
                                last_result = val.clone();
                            }
                        }
                        Instruction::Return { value } => {
                            // Return instruction - capture the return value
                            if let Ok(val) = self.get_register(*value) {
                                last_result = val.clone();
                                tracing::trace!("VM: Captured return value: {:?}", last_result);
                            }
                        }
                        _ => {
                            // Instructions that don't produce values
                        }
                    }
                }
                Err(e) => {
                    // Restore instruction pointer and return error
                    let func_name = self.current_debug_function.as_deref().unwrap_or("unknown");
                    tracing::trace!("[{}] Error during function execution: {:?}", func_name, e);
                    self.instruction_pointer = saved_instruction_pointer;
                    return Err(e);
                }
            }

            // Advance instruction pointer for normal instructions
            func_ip += 1;
        }

        // Restore the instruction pointer
        self.instruction_pointer = saved_instruction_pointer;

        // Function execution complete
        Ok(FunctionExecutionResult::Value(last_result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::BytecodeProgram;

    #[test]
    fn test_vm_basic_execution() {
        let program = Arc::new(BytecodeProgram::new());
        let mut vm = VirtualMachine::new(
            program,
            None,
            Arc::new(IndexMap::new()),
            Arc::new(IndexMap::new()),
            Arc::new(IndexMap::new()),
            Arc::new(CoreVariableRegistry::new()),
            None,
        );
        let result = vm.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_vm_with_hot_ast() {
        let bytecode_program = Arc::new(BytecodeProgram::new());
        let hot_ast = crate::lang::ast::HotAst::from_program(crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::default(),
        });

        let mut vm = VirtualMachine::new(
            bytecode_program,
            Some(Arc::new(hot_ast)),
            Arc::new(IndexMap::new()),
            Arc::new(IndexMap::new()),
            Arc::new(IndexMap::new()),
            Arc::new(CoreVariableRegistry::new()),
            None,
        );
        let result = vm.execute();

        assert!(
            result.is_ok(),
            "VM execution with AST failed: {:?}",
            result.err()
        );
    }
}
