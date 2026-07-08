// Bytecode instruction set for Hot.
//
// This module defines a register-based bytecode instruction set optimized for Hot's
// language features including flows, lazy evaluation, and type dispatch.

use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use std::fmt;
use std::sync::Arc;

/// Register identifier - uses u32 for large programs with many packages
pub type RegisterId = u32;

/// Constant pool index - references to literal values stored separately
pub type ConstantId = u32;

/// Function identifier - interned function references for fast dispatch
pub type FunctionId = u32;

/// Type identifier - interned type references for fast type checking
pub type TypeId = u16;

/// Jump offset for control flow instructions
pub type JumpOffset = i32;

/// Flow types for Hot language flows
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum FlowType {
    Serial,
    Parallel,
    Pipe,
    Cond,
    CondAll,
    Match,
    MatchAll,
}

/// Flow result modifiers
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum FlowResultModifier {
    One, // Return single value (tail for serial, tail for cond)
    Map, // Return all results as map
    Vec, // Return all results as vector of values
}

/// Scope types for lexical scoping
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ScopeType {
    /// Function scope (parameters and local variables)
    Function,
    /// Lambda scope (captures outer variables)
    Lambda,
    /// Flow scope (serial, parallel, pipe flows)
    Flow,
    /// Namespace scope (top-level variables)
    Namespace,
}

/// Bytecode instruction set optimized for Hot language features
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Instruction {
    // === Value Operations ===
    /// Load constant from constant pool into register
    LoadConst {
        dest: RegisterId,
        constant: ConstantId,
    },

    /// Move value from one register to another
    Move { dest: RegisterId, src: RegisterId },

    /// Load function reference into register
    LoadFunctionRef {
        dest: RegisterId,
        function_name: ConstantId,
    },

    /// Load type reference into register (for variant union types)
    LoadTypeRef {
        dest: RegisterId,
        type_ref: ConstantId,
    },

    // === Flow Control Instructions ===
    /// Begin a flow execution context
    BeginFlow {
        flow_type: FlowType,
        result_modifier: FlowResultModifier,
        /// Boxed: SourceLocation is ~56 bytes and would otherwise set the
        /// size of every Instruction in every program.
        source: Option<Box<SourceLocation>>,
    },

    /// End a flow execution context and collect results
    EndFlow { dest: RegisterId },

    /// Execute a branch in conditional flow
    CondBranch {
        condition: RegisterId,
        target: JumpOffset,
    },

    /// Start of a conditional branch - evaluate condition and skip if false
    CondBranchStart {
        branch_name: ConstantId,
        condition: RegisterId,
        skip_target: JumpOffset,
    },

    /// End of a conditional branch - collect result
    CondBranchEnd {
        branch_name: ConstantId,
        result: RegisterId,
    },

    /// Pipe value to next operation (for pipe flows)
    Pipe { dest: RegisterId, src: RegisterId },

    // === Function Definition Instructions ===
    /// Define a function and store it in a register
    DefineFunction {
        dest: RegisterId,
        function_id: FunctionId,
    },

    // === Arithmetic Operations ===
    /// Add two registers, store result in dest
    Add {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    /// Subtract right from left, store result in dest
    Sub {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    /// Multiply two registers, store result in dest
    Mul {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    // === Comparison Operations ===
    /// Compare equality, store boolean result in dest
    Eq {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    /// Check if a string ends with a suffix, store boolean result in dest
    StrEndsWith {
        dest: RegisterId,
        string: RegisterId,
        suffix: RegisterId,
    },

    /// Check if a string starts with a prefix, store boolean result in dest
    StrStartsWith {
        dest: RegisterId,
        string: RegisterId,
        prefix: RegisterId,
    },

    /// Compare greater than, store boolean result in dest
    Gt {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    /// Compare less than, store boolean result in dest
    Lt {
        dest: RegisterId,
        left: RegisterId,
        right: RegisterId,
    },

    // === Function Calls ===
    /// Call function with arguments from consecutive registers
    Call {
        dest: RegisterId,
        function: FunctionId,
        args_start: RegisterId,
        args_count: u8,
    },

    /// Call native library function (hotlib)
    CallNative {
        dest: RegisterId,
        function: FunctionId,
        args_start: RegisterId,
        args_count: u8,
    },

    /// Call lambda function with parameter binding
    CallLambda {
        dest: RegisterId,
        lambda: RegisterId,
        args_start: RegisterId,
        args_count: u8,
    },

    /// Call function with spread arguments
    /// At runtime, args marked in spread_mask are unpacked (they should be Vecs)
    CallWithSpread {
        dest: RegisterId,
        function: FunctionId,
        args_start: RegisterId,
        args_count: u8,
        spread_mask: u64, // Bitmask: bit i = 1 means arg i should be spread
    },

    /// Call user-defined function directly by function ID
    CallUserFunction {
        dest: RegisterId,
        function_id: FunctionId,
        args_start: RegisterId,
        args_count: u8,
    },

    /// Tail call optimization - reuse current stack frame for self-recursive calls.
    /// Instead of pushing a new stack frame, this instruction updates the current
    /// function's parameters with new argument values and jumps back to the start
    /// of the function. This prevents stack overflow for tail-recursive functions.
    TailCall {
        /// The function ID to tail-call (must be the same as the current function)
        function_id: FunctionId,
        /// Starting register for arguments
        args_start: RegisterId,
        /// Number of arguments
        args_count: u8,
    },

    /// Execute template literal with interpolation
    TemplateInterpolate {
        dest: RegisterId,
        parts_start: RegisterId,
        parts_count: u8,
    },

    /// Access property on object (dot access)
    DotAccess {
        dest: RegisterId,
        object: RegisterId,
        property: ConstantId,
    },

    /// Access property on object, or use default value if property doesn't exist
    DotAccessOrDefault {
        dest: RegisterId,
        object: RegisterId,
        property: ConstantId,
        default_value: ConstantId,
    },

    /// Access property on object with dynamic key from register
    /// Used for obj[var] where var is a runtime variable
    DynamicDotAccess {
        dest: RegisterId,
        object: RegisterId,
        property: RegisterId, // Register containing the key/index value
    },

    /// Set property on object with dynamic key from register
    /// Used for obj[var] = value where var is a runtime variable
    DynamicDotSet {
        object: RegisterId,
        property: RegisterId, // Register containing the key/index value
        value: RegisterId,
    },

    /// Set property on object (dot assignment)
    DotSet {
        object: RegisterId,
        property: ConstantId,
        value: RegisterId,
    },

    /// Append value to vector (items[] = value syntax)
    VecAppend { vec: RegisterId, value: RegisterId },

    /// Get the type path of any value (including primitives)
    /// For Int -> "::hot::type/Int", for custom types -> their $type field, etc.
    GetTypePath { dest: RegisterId, value: RegisterId },

    /// Call Rust hotlib function explicitly (call-lib in Hot code)
    CallLibBuiltin {
        dest: RegisterId,
        function: RegisterId,
        args: RegisterId,
    },

    /// Call Hot user-defined function explicitly (call in Hot code)

    // === Control Flow ===
    /// Unconditional jump
    Jump { offset: JumpOffset },

    /// Jump if register contains truthy value
    JumpIf {
        condition: RegisterId,
        offset: JumpOffset,
    },

    /// Jump if register contains falsy value
    JumpIfNot {
        condition: RegisterId,
        offset: JumpOffset,
    },

    /// Return from function with value in register
    Return { value: RegisterId },

    // === Type Operations ===
    /// Extract inner value from a typed object for variant construction.
    /// If the input has a $val field (is a typed object), extract that $val.
    /// Otherwise, return the input as-is.
    /// This allows both `Variant({...})` and `Variant(Type({...}))` to work equivalently.
    ExtractInnerVal { dest: RegisterId, src: RegisterId },

    /// Construct a typed object with recursive nested type construction.
    /// type_info is a constant containing: {$type: "TypeName", $fields: {field_name: "TypeName", ...}}
    /// For each field with a custom type, recursively constructs that type.
    /// Result is {$type: "TypeName", $val: {field1: typed_val1, ...}}
    ConstructTyped {
        dest: RegisterId,
        src: RegisterId,
        type_info: ConstantId,
    },

    /// Check if a value is of a given type (by type path string).
    /// This is the unified type checking used by `match` and `is-type`.
    /// Handles:
    /// - Primitives (Int, Str, Bool, etc.)
    /// - Custom types (User, Order, etc.)
    /// - Enum variants (Result.Ok, Result.Err, Direction.Up, etc.)
    /// - Type-level matching (Result matches Result.Ok and Result.Err)
    ///
    /// Returns true if:
    /// - Exact match: value's type path equals the expected type path
    /// - Type-level match: value's type path starts with expected + "."
    IsType {
        dest: RegisterId,
        value: RegisterId,
        type_path: RegisterId, // Register containing the type path string
    },

    /// Ensure a value is wrapped in a Result type for match flow pattern matching.
    /// If the value is already a Result (Ok or Err), it is left unchanged.
    /// If the value is any other type, it is wrapped in Result.Ok.
    /// This enables `match result { Result.Ok => ... Result.Err => ... }` to work
    /// even when the matched value comes from a function return (which is not
    /// explicitly wrapped in Result.Ok at the runtime level).
    EnsureResult { dest: RegisterId, value: RegisterId },

    // === Collection Operations ===
    /// Create vector from consecutive registers
    MakeVec {
        dest: RegisterId,
        elements_start: RegisterId,
        count: u16,
    },

    /// Push a value onto a vector (modifies vec in place)
    VecPush { vec: RegisterId, value: RegisterId },

    /// Create vector from consecutive registers with spread support
    /// Elements marked in spread_mask are Vec values that should be flattened into the result
    MakeVecWithSpread {
        dest: RegisterId,
        elements_start: RegisterId,
        count: u16,
        spread_mask: u64, // Bitmask: bit i = 1 means element i should be spread
    },

    /// Set element in vector/map
    SetElement {
        collection: RegisterId,
        index: RegisterId,
        value: RegisterId,
    },

    /// Merge source map into destination map
    MergeMaps {
        dest: RegisterId,
        source: RegisterId,
    },

    // === Variable Operations ===
    /// Load variable by name from current scope chain
    LoadVar {
        dest: RegisterId,
        var_name: ConstantId,
    },

    /// Load variable by name, or use default value if variable doesn't exist
    LoadVarOrDefault {
        dest: RegisterId,
        var_name: ConstantId,
        default_value: ConstantId,
    },

    /// Store value into variable in current scope
    StoreVar {
        var_name: ConstantId,
        value: RegisterId,
        /// Optional metadata for emitter events (boxed to reduce enum size)
        metadata: Option<Box<VariableMetadata>>,
    },

    /// Set current namespace for variable storage
    SetNamespace { namespace: ConstantId },

    /// Load variable from specific namespace
    LoadGlobal {
        dest: RegisterId,
        namespace: ConstantId,
        var_name: ConstantId,
    },

    /// Store value into global variable
    StoreGlobal {
        namespace: ConstantId,
        var_name: ConstantId,
        value: RegisterId,
    },

    /// Load variable from specific scope depth (for closures)
    LoadScoped {
        dest: RegisterId,
        var_name: ConstantId,
        scope_depth: u16,
    },

    /// Store variable at specific scope depth
    StoreScoped {
        var_name: ConstantId,
        value: RegisterId,
        scope_depth: u16,
    },

    /// Deferred variable expression for parallel flow execution
    /// The thunk register contains a LambdaInfo that will be evaluated in parallel
    DeferredVarExpr {
        var_name: ConstantId,
        /// Register containing the thunk (zero-arg lambda) to evaluate
        thunk: RegisterId,
    },

    // === Scope Management ===
    /// Enter a new lexical scope (function, lambda, flow)
    EnterScope { scope_type: ScopeType },

    /// Exit current lexical scope and restore previous scope
    ExitScope,

    /// Capture variables from outer scope (for closures)
    CaptureVar {
        dest: RegisterId,
        var_name: ConstantId,
        scope_depth: u16,
    },

    /// Return immediately if the source register contains an error value
    /// Used for error propagation in constructors
    ReturnIfErr { src: RegisterId },

    // === Error Capture (for `!` raw operator) ===
    /// Begin error capture mode: convert runtime errors to {"error": message}
    BeginErrorCapture,
    /// End error capture mode
    EndErrorCapture,
    /// Wrap a value as an Ok result map: {"ok": src}
    WrapOk { dest: RegisterId, src: RegisterId },

    // === Lexical Type System ===
    /// Register a local type implementation in the current scope
    RegisterLocalImplementation {
        source_type: ConstantId,
        target_type: ConstantId,
        implementation_function_name: ConstantId,
    },

    /// Register a local type constructor in the current scope
    RegisterLocalType {
        type_name: ConstantId,
        constructor_function_name: ConstantId,
    },
}

/// Constant pool entry for storing literal values
/// Uses Val directly to leverage Hot's rich type system
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Constant {
    /// Direct Val storage for maximum compatibility
    Val(crate::val::Val),
    /// Function reference (for deferred function calls) — Arc<str> for cheap cloning
    FunctionRef(Arc<str>),
    /// Type reference (for type checking) — Arc<str> for cheap cloning
    TypeRef(Arc<str>),
    /// Variant union type reference with variants
    VariantTypeRef(crate::lang::refs::TypeRef),
    /// String reference (for branch names, type names in instructions, etc.) — Arc<str> for cheap cloning
    StringRef(Arc<str>),
}

/// Function metadata for efficient dispatch
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunctionInfo {
    /// Function name for debugging
    pub name: String,
    /// Namespace where function is defined
    pub namespace: String,
    /// Parameter count
    pub arity: u8,
    /// Whether function has variadic parameters
    pub is_variadic: bool,
    /// Parameter names for proper variable binding
    pub param_names: Vec<String>,
    /// Parameter types for compile-time checking
    pub param_types: Vec<TypeId>,
    /// Return type
    pub return_type: TypeId,
    /// Whether parameters should be lazily evaluated
    pub lazy_params: Vec<bool>,
    /// Flow type if this function is defined as a flow (e.g., `fn cond`, `fn parallel`)
    /// None means serial (default), Some means a specific flow type
    pub flow_type: Option<FlowType>,
    /// Bytecode instructions for this function
    pub instructions: Vec<Instruction>,
    /// Number of registers needed for execution
    pub register_count: u32,
    /// Source location where function is defined
    pub source: Option<SourceLocation>,
}

/// Lazily-computed cache of a lambda's structural hash (parameters,
/// instructions, captures, ...). The JIT keys compiled lambdas by this hash;
/// computing it hashed the entire instruction list on every lambda call.
/// 0 means "not computed yet" (the hasher result is remapped away from 0).
#[derive(Debug, Default)]
pub struct LambdaHashCache(std::sync::atomic::AtomicU64);

impl Clone for LambdaHashCache {
    fn clone(&self) -> Self {
        // Structural fields are immutable after construction, so the cached
        // hash stays valid across clones (closure capture clones only mutate
        // `closure_env`, which is not part of the hash).
        Self(std::sync::atomic::AtomicU64::new(
            self.0.load(std::sync::atomic::Ordering::Relaxed),
        ))
    }
}

impl LambdaHashCache {
    pub fn get_or_compute(&self, compute: impl FnOnce() -> u64) -> u64 {
        let cached = self.0.load(std::sync::atomic::Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }
        // Reserve 0 as the "unset" sentinel.
        let hash = compute().max(1);
        self.0.store(hash, std::sync::atomic::Ordering::Relaxed);
        hash
    }
}

/// Lambda function information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LambdaInfo {
    /// Parameter names for binding
    pub parameters: Vec<String>,
    /// Bytecode instructions for lambda body
    pub instructions: Vec<Instruction>,
    /// Number of registers needed for execution
    pub register_count: u32,
    /// Variables captured from outer scope
    pub capture_vars: Vec<String>,
    /// Closure environment (captured variable values)
    pub closure_env: AHashMap<String, crate::val::Val>,
    /// Namespace where this lambda was defined (for lexical scoping)
    pub defining_namespace: String,
    /// Whether this lambda represents a lazy parameter (should not be auto-evaluated)
    #[serde(default)]
    pub is_lazy_param: bool,
    /// Pre-computed set of registers used by this lambda (for efficient save/restore)
    /// Computed at compile time to avoid O(N) scan on every lambda call
    #[serde(default)]
    pub used_registers: Vec<u32>,
    /// Lazily-computed structural hash (see `LambdaHashCache`). Not part of
    /// equality/ordering/hashing; skipped in serialization.
    #[serde(skip)]
    pub structural_hash_cache: LambdaHashCache,
}

impl PartialEq for LambdaInfo {
    fn eq(&self, other: &Self) -> bool {
        self.parameters == other.parameters
            && self.instructions == other.instructions
            && self.register_count == other.register_count
            && self.capture_vars == other.capture_vars
            && self.defining_namespace == other.defining_namespace
            && self.used_registers == other.used_registers
        // Skip closure_env comparison for now
    }
}

impl Eq for LambdaInfo {}

impl std::hash::Hash for LambdaInfo {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.parameters.hash(state);
        self.instructions.hash(state);
        self.register_count.hash(state);
        self.capture_vars.hash(state);
        // Skip closure_env for hashing
    }
}

impl PartialOrd for LambdaInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LambdaInfo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.parameters
            .cmp(&other.parameters)
            .then_with(|| self.instructions.cmp(&other.instructions))
            .then_with(|| self.register_count.cmp(&other.register_count))
            .then_with(|| self.capture_vars.cmp(&other.capture_vars))
        // Skip closure_env for ordering
    }
}

impl std::fmt::Display for LambdaInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lambda({} params, {} instructions, {} captured)",
            self.parameters.len(),
            self.instructions.len(),
            self.capture_vars.len()
        )
    }
}

impl LambdaInfo {
    /// Compute the set of registers used by a list of instructions.
    /// This extracts destination registers from all instruction types.
    pub fn compute_used_registers(instructions: &[Instruction]) -> Vec<u32> {
        let mut used: AHashSet<u32> = AHashSet::new();
        for instr in instructions {
            match instr {
                // Value operations
                Instruction::LoadConst { dest, .. }
                | Instruction::Move { dest, .. }
                | Instruction::LoadFunctionRef { dest, .. }
                | Instruction::LoadTypeRef { dest, .. }
                // Flow control
                | Instruction::EndFlow { dest }
                | Instruction::Pipe { dest, .. }
                | Instruction::DefineFunction { dest, .. }
                // Arithmetic
                | Instruction::Add { dest, .. }
                | Instruction::Sub { dest, .. }
                | Instruction::Mul { dest, .. }
                // Comparison
                | Instruction::Eq { dest, .. }
                | Instruction::StrEndsWith { dest, .. }
                | Instruction::StrStartsWith { dest, .. }
                | Instruction::Gt { dest, .. }
                | Instruction::Lt { dest, .. }
                // Function calls
                | Instruction::Call { dest, .. }
                | Instruction::CallNative { dest, .. }
                | Instruction::CallLambda { dest, .. }
                | Instruction::CallWithSpread { dest, .. }
                | Instruction::CallUserFunction { dest, .. }
                | Instruction::TemplateInterpolate { dest, .. }
                | Instruction::CallLibBuiltin { dest, .. }
                // Property access
                | Instruction::DotAccess { dest, .. }
                | Instruction::DotAccessOrDefault { dest, .. }
                | Instruction::DynamicDotAccess { dest, .. }
                // Type operations
                | Instruction::ExtractInnerVal { dest, .. }
                | Instruction::ConstructTyped { dest, .. }
                // Collection operations
                | Instruction::MakeVec { dest, .. }
                | Instruction::MakeVecWithSpread { dest, .. }
                | Instruction::MergeMaps { dest, .. }
                // Variable operations
                | Instruction::LoadVar { dest, .. }
                | Instruction::LoadVarOrDefault { dest, .. }
                | Instruction::LoadGlobal { dest, .. }
                | Instruction::LoadScoped { dest, .. }
                | Instruction::CaptureVar { dest, .. }
                // Error handling
                | Instruction::WrapOk { dest, .. }
                // Type operations
                | Instruction::GetTypePath { dest, .. }
                | Instruction::IsType { dest, .. }
                | Instruction::EnsureResult { dest, .. } => {
                    used.insert(*dest);
                }
                // In-place mutations: the register's value is written through,
                // so it must be part of the save/restore set as well.
                Instruction::VecPush { vec, .. } | Instruction::VecAppend { vec, .. } => {
                    used.insert(*vec);
                }
                Instruction::SetElement { collection, .. } => {
                    used.insert(*collection);
                }
                Instruction::DotSet { object, .. }
                | Instruction::DynamicDotSet { object, .. } => {
                    used.insert(*object);
                }
                // Instructions without dest registers
                _ => {}
            }
        }
        let mut result: Vec<u32> = used.into_iter().collect();
        result.sort_unstable();
        result
    }
}

/// Variable information in namespace registry
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VariableInfo {
    /// Variable name
    pub name: String,
    /// Variable type (Function, Value, Type, etc.)
    pub var_type: VariableType,
    /// Metadata associated with the variable
    pub metadata: Option<crate::val::Val>,
    /// Function ID if this is a function
    pub function_id: Option<FunctionId>,
}

/// Type of variable in the namespace
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum VariableType {
    /// Regular function
    Function,
    /// Type constructor
    TypeConstructor,
    /// Regular value/variable
    Value,
    /// Core function (auto-imported)
    CoreFunction,
}

/// Namespace information in registry
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NamespaceInfo {
    /// Namespace path (e.g., "::hot::test")
    pub path: String,
    /// Variables in this namespace
    pub variables: IndexMap<String, VariableInfo>,
}

/// Complete namespace registry for independence
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NamespaceRegistry {
    /// All namespaces indexed by path
    pub namespaces: IndexMap<String, NamespaceInfo>,
    /// Quick lookup for test functions
    pub test_functions: Vec<String>, // Fully qualified names like "::hot::test::walk/test-untype-simple"
}

impl Default for NamespaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            namespaces: IndexMap::new(),
            test_functions: Vec::new(),
        }
    }

    /// Add a namespace to the registry
    pub fn add_namespace(&mut self, path: String) {
        if !self.namespaces.contains_key(&path) {
            self.namespaces.insert(
                path.clone(),
                NamespaceInfo {
                    path: path.clone(),
                    variables: IndexMap::new(),
                },
            );
        }
    }

    /// Add a variable to a namespace
    pub fn add_variable(&mut self, namespace_path: &str, var_info: VariableInfo) {
        self.add_variable_with_key(namespace_path, &var_info.name.clone(), var_info);
    }

    /// Add a variable to a namespace with a specific lookup key
    /// This is used when the lookup key differs from the full path stored in var_info.name
    /// (e.g., for imports where "Media" should map to "::hot::media/Media")
    pub fn add_variable_with_key(
        &mut self,
        namespace_path: &str,
        key: &str,
        var_info: VariableInfo,
    ) {
        self.add_namespace(namespace_path.to_string());
        if let Some(namespace) = self.namespaces.get_mut(namespace_path) {
            // If this is a test function, add it to the quick lookup
            if matches!(var_info.var_type, VariableType::Function)
                && let Some(metadata) = &var_info.metadata
                && is_test_metadata(metadata)
            {
                let full_name = format!("{}/{}", namespace_path, key);
                self.test_functions.push(full_name);
            }
            namespace.variables.insert(key.to_string(), var_info);
        }
    }

    /// Get all namespace paths
    pub fn get_namespace_paths(&self) -> Vec<String> {
        self.namespaces.keys().cloned().collect()
    }

    /// Get all functions in a namespace
    pub fn get_functions_in_namespace(&self, namespace_path: &str) -> Vec<&VariableInfo> {
        if let Some(namespace) = self.namespaces.get(namespace_path) {
            namespace
                .variables
                .values()
                .filter(|var| {
                    matches!(
                        var.var_type,
                        VariableType::Function | VariableType::CoreFunction
                    )
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get all variables in a namespace
    pub fn get_variables(&self, namespace_path: &str) -> Option<Vec<&VariableInfo>> {
        self.namespaces
            .get(namespace_path)
            .map(|namespace| namespace.variables.values().collect())
    }

    /// Get all variables across all namespaces
    pub fn get_all_variables(&self) -> Vec<(&String, Vec<&VariableInfo>)> {
        self.namespaces
            .iter()
            .map(|(ns_path, namespace)| (ns_path, namespace.variables.values().collect()))
            .collect()
    }

    /// Get all test functions
    pub fn get_test_functions(&self) -> &Vec<String> {
        &self.test_functions
    }
}

/// Check if metadata indicates a test function
fn is_test_metadata(metadata: &crate::val::Val) -> bool {
    match metadata {
        crate::val::Val::Vec(vec) => vec
            .iter()
            .any(|item| matches!(item, crate::val::Val::Str(s) if &**s == "test")),
        crate::val::Val::Map(map) => map
            .get(&crate::val::Val::from("test"))
            .map(|v| matches!(v, crate::val::Val::Bool(true)))
            .unwrap_or(false),
        crate::val::Val::Str(s) => &**s == "test",
        _ => false,
    }
}

/// Source location mapping for runtime error reporting
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceMap {
    /// Maps instruction pointer to source location
    pub instruction_locations: AHashMap<usize, SourceLocation>,
    /// Source file contents for pretty error reporting
    pub file_contents: AHashMap<String, String>,
}

/// Source location information for bytecode instructions
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct SourceLocation {
    pub file: Option<String>,
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
}

/// Variable metadata for emitter events
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct VariableMetadata {
    /// Variable name (without $ prefix)
    pub name: String,
    /// Namespace path (for backward compatibility)
    pub namespace: String,
    /// Static scope path (hierarchical: "::ns/func/lambda") - for AST metadata lookup
    pub static_scope: Option<String>,
    /// Variable metadata (from AST)
    pub meta: Option<crate::val::Val>,
    /// Source location information
    pub source: Option<SourceLocation>,
}

impl SourceMap {
    /// Create a new empty source map
    pub fn new() -> Self {
        Self {
            instruction_locations: AHashMap::new(),
            file_contents: AHashMap::new(),
        }
    }

    /// Add source location for an instruction
    pub fn add_location(&mut self, instruction_pointer: usize, location: SourceLocation) {
        self.instruction_locations
            .insert(instruction_pointer, location);
    }

    /// Add source file content
    pub fn add_file_content(&mut self, file_path: String, content: String) {
        self.file_contents.insert(file_path, content);
    }

    /// Get source location for an instruction pointer
    pub fn get_location(&self, instruction_pointer: usize) -> Option<&SourceLocation> {
        self.instruction_locations.get(&instruction_pointer)
    }

    /// Get source file content
    pub fn get_file_content(&self, file_path: &str) -> Option<&String> {
        self.file_contents.get(file_path)
    }
}

impl Default for SourceMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Compiled bytecode program
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BytecodeProgram {
    /// Constant pool
    pub constants: Vec<Constant>,
    /// Function definitions
    pub functions: Vec<FunctionInfo>,
    /// Lambda definitions
    pub lambdas: Vec<LambdaInfo>,
    /// Global variable names (for debugging)
    pub global_names: Vec<String>,
    /// Entry point instructions (namespace initialization)
    pub entry_point: Vec<Instruction>,
    /// Number of registers needed for entry point
    pub entry_register_count: u32,
    /// Namespace registry for independence
    pub namespaces: NamespaceRegistry,
    /// Source map for runtime error reporting
    pub source_map: SourceMap,
    /// Variable metadata registry for emitter events
    pub variable_metadata: AHashMap<String, VariableMetadata>,
    /// Content-hash index over `constants` so `add_constant` dedup is O(1)
    /// instead of a linear scan (which made pool construction quadratic on
    /// large programs). Values are candidate ids sharing a hash; equality is
    /// verified before reuse. Skipped in serde and rebuilt lazily, so
    /// deserialized programs stay correct (worst case: a few duplicate
    /// constants, never a wrong id).
    #[serde(skip)]
    constant_index: AHashMap<u64, Vec<ConstantId>>,
}

impl fmt::Display for FlowType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowType::Serial => write!(f, "serial"),
            FlowType::Parallel => write!(f, "parallel"),
            FlowType::Pipe => write!(f, "pipe"),
            FlowType::Cond => write!(f, "cond"),
            FlowType::CondAll => write!(f, "cond-all"),
            FlowType::Match => write!(f, "match"),
            FlowType::MatchAll => write!(f, "match-all"),
        }
    }
}

impl fmt::Display for FlowResultModifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowResultModifier::One => write!(f, "one"),
            FlowResultModifier::Map => write!(f, "map"),
            FlowResultModifier::Vec => write!(f, "vec"),
        }
    }
}

impl fmt::Display for ScopeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeType::Function => write!(f, "function"),
            ScopeType::Lambda => write!(f, "lambda"),
            ScopeType::Flow => write!(f, "flow"),
            ScopeType::Namespace => write!(f, "namespace"),
        }
    }
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instruction::LoadConst { dest, constant } => {
                write!(f, "LOAD_CONST r{}, c{}", dest, constant)
            }
            Instruction::Move { dest, src } => write!(f, "MOVE r{}, r{}", dest, src),
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } => write!(f, "LOAD_FUNCTION_REF r{}, c{}", dest, function_name),
            Instruction::LoadTypeRef { dest, type_ref } => {
                write!(f, "LOAD_TYPE_REF r{}, c{}", dest, type_ref)
            }
            Instruction::BeginFlow {
                flow_type,
                result_modifier,
                source,
            } => {
                if source.is_some() {
                    write!(f, "BEGIN_FLOW {}|{} @source", flow_type, result_modifier)
                } else {
                    write!(f, "BEGIN_FLOW {}|{}", flow_type, result_modifier)
                }
            }
            Instruction::EndFlow { dest } => write!(f, "END_FLOW r{}", dest),
            Instruction::CondBranch { condition, target } => {
                write!(f, "COND_BRANCH r{}, {}", condition, target)
            }
            Instruction::CondBranchStart {
                branch_name,
                condition,
                skip_target,
            } => write!(
                f,
                "COND_BRANCH_START c{}, r{}, {}",
                branch_name, condition, skip_target
            ),
            Instruction::CondBranchEnd {
                branch_name,
                result,
            } => write!(f, "COND_BRANCH_END c{}, r{}", branch_name, result),
            Instruction::Pipe { dest, src } => write!(f, "PIPE r{}, r{}", dest, src),
            Instruction::DefineFunction { dest, function_id } => {
                write!(f, "DEFINE_FUNCTION r{}, f{}", dest, function_id)
            }
            Instruction::Add { dest, left, right } => {
                write!(f, "ADD r{}, r{}, r{}", dest, left, right)
            }
            Instruction::Sub { dest, left, right } => {
                write!(f, "SUB r{}, r{}, r{}", dest, left, right)
            }
            Instruction::Mul { dest, left, right } => {
                write!(f, "MUL r{}, r{}, r{}", dest, left, right)
            }
            Instruction::Eq { dest, left, right } => {
                write!(f, "EQ r{}, r{}, r{}", dest, left, right)
            }
            Instruction::StrEndsWith {
                dest,
                string,
                suffix,
            } => write!(f, "STR_ENDS_WITH r{}, r{}, r{}", dest, string, suffix),
            Instruction::StrStartsWith {
                dest,
                string,
                prefix,
            } => write!(f, "STR_STARTS_WITH r{}, r{}, r{}", dest, string, prefix),
            Instruction::Gt { dest, left, right } => {
                write!(f, "GT r{}, r{}, r{}", dest, left, right)
            }
            Instruction::Lt { dest, left, right } => {
                write!(f, "LT r{}, r{}, r{}", dest, left, right)
            }
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => write!(
                f,
                "CALL r{}, f{}, r{}..{}",
                dest,
                function,
                args_start,
                args_start + *args_count as u32
            ),
            Instruction::CallNative {
                dest,
                function,
                args_start,
                args_count,
            } => write!(
                f,
                "CALL_NATIVE r{}, f{}, r{}..{}",
                dest,
                function,
                args_start,
                args_start + *args_count as u32
            ),
            Instruction::CallLambda {
                dest,
                lambda,
                args_start,
                args_count,
            } => write!(
                f,
                "CALL_LAMBDA r{}, r{}, r{}..{}",
                dest,
                lambda,
                args_start,
                args_start + *args_count as u32
            ),
            Instruction::CallWithSpread {
                dest,
                function,
                args_start,
                args_count,
                spread_mask,
            } => write!(
                f,
                "CALL_WITH_SPREAD r{}, f{}, r{}..{}, mask={:#x}",
                dest,
                function,
                args_start,
                args_start + *args_count as u32,
                spread_mask
            ),
            Instruction::CallUserFunction {
                dest,
                function_id,
                args_start,
                args_count,
            } => write!(
                f,
                "CALL_USER_FUNCTION r{}, f{}, r{}..{}",
                dest,
                function_id,
                args_start,
                args_start + *args_count as u32
            ),
            Instruction::TailCall {
                function_id,
                args_start,
                args_count,
            } => write!(
                f,
                "TAILCALL f{}, r{}..{}",
                function_id,
                args_start,
                args_start + *args_count as u32
            ),
            Instruction::TemplateInterpolate {
                dest,
                parts_start,
                parts_count,
            } => write!(
                f,
                "TEMPLATE_INTERPOLATE r{}, r{}..{}",
                dest,
                parts_start,
                parts_start + *parts_count as u32
            ),
            Instruction::DotAccess {
                dest,
                object,
                property,
            } => write!(f, "DOT_ACCESS r{}, r{}, c{}", dest, object, property),
            Instruction::DotAccessOrDefault {
                dest,
                object,
                property,
                default_value,
            } => write!(
                f,
                "DOT_ACCESS_OR_DEFAULT r{}, r{}, c{}, c{}",
                dest, object, property, default_value
            ),
            Instruction::DynamicDotAccess {
                dest,
                object,
                property,
            } => write!(
                f,
                "DYNAMIC_DOT_ACCESS r{}, r{}, r{}",
                dest, object, property
            ),
            Instruction::DynamicDotSet {
                object,
                property,
                value,
            } => write!(f, "DYNAMIC_DOT_SET r{}, r{}, r{}", object, property, value),
            Instruction::DotSet {
                object,
                property,
                value,
            } => write!(f, "DOT_SET r{}, c{}, r{}", object, property, value),
            Instruction::VecAppend { vec, value } => {
                write!(f, "VEC_APPEND r{}, r{}", vec, value)
            }
            Instruction::GetTypePath { dest, value } => {
                write!(f, "GET_TYPE_PATH r{}, r{}", dest, value)
            }
            Instruction::CallLibBuiltin {
                dest,
                function,
                args,
            } => write!(f, "CALL_LIB_BUILTIN r{}, r{}, r{}", dest, function, args),
            Instruction::Jump { offset } => write!(f, "JUMP {}", offset),
            Instruction::JumpIf { condition, offset } => {
                write!(f, "JUMP_IF r{}, {}", condition, offset)
            }
            Instruction::JumpIfNot { condition, offset } => {
                write!(f, "JUMP_IF_NOT r{}, {}", condition, offset)
            }
            Instruction::Return { value } => write!(f, "RETURN r{}", value),
            Instruction::ExtractInnerVal { dest, src } => {
                write!(f, "EXTRACT_INNER_VAL r{}, r{}", dest, src)
            }
            Instruction::ConstructTyped {
                dest,
                src,
                type_info,
            } => write!(f, "CONSTRUCT_TYPED r{}, r{}, c{}", dest, src, type_info),
            Instruction::IsType {
                dest,
                value,
                type_path,
            } => write!(f, "IS_TYPE r{}, r{}, r{}", dest, value, type_path),
            Instruction::EnsureResult { dest, value } => {
                write!(f, "ENSURE_RESULT r{}, r{}", dest, value)
            }
            Instruction::MakeVec {
                dest,
                elements_start,
                count,
            } => write!(
                f,
                "MAKE_VEC r{}, r{}..{}",
                dest,
                elements_start,
                elements_start + *count as u32
            ),
            Instruction::VecPush { vec, value } => {
                write!(f, "VEC_PUSH r{}, r{}", vec, value)
            }
            Instruction::MakeVecWithSpread {
                dest,
                elements_start,
                count,
                spread_mask,
            } => write!(
                f,
                "MAKE_VEC_WITH_SPREAD r{}, r{}..{}, mask={:#x}",
                dest,
                elements_start,
                elements_start + *count as u32,
                spread_mask
            ),
            Instruction::SetElement {
                collection,
                index,
                value,
            } => write!(f, "SET_ELEMENT r{}, r{}, r{}", collection, index, value),
            Instruction::MergeMaps { dest, source } => {
                write!(f, "MERGE_MAPS r{}, r{}", dest, source)
            }
            Instruction::LoadVar { dest, var_name } => {
                write!(f, "LOAD_VAR r{}, c{}", dest, var_name)
            }
            Instruction::LoadVarOrDefault {
                dest,
                var_name,
                default_value,
            } => write!(
                f,
                "LOAD_VAR_OR_DEFAULT r{}, c{}, c{}",
                dest, var_name, default_value
            ),
            Instruction::StoreVar {
                var_name,
                value,
                metadata,
            } => {
                if let Some(metadata) = metadata {
                    write!(
                        f,
                        "STORE_VAR c{}, r{} metadata={}",
                        var_name, value, metadata.name
                    )
                } else {
                    write!(f, "STORE_VAR c{}, r{}", var_name, value)
                }
            }
            Instruction::SetNamespace { namespace } => {
                write!(f, "SET_NAMESPACE c{}", namespace)
            }
            Instruction::LoadGlobal {
                dest,
                namespace,
                var_name,
            } => write!(f, "LOAD_GLOBAL r{}, c{}, c{}", dest, namespace, var_name),
            Instruction::StoreGlobal {
                namespace,
                var_name,
                value,
            } => write!(f, "STORE_GLOBAL c{}, c{}, r{}", namespace, var_name, value),
            Instruction::LoadScoped {
                dest,
                var_name,
                scope_depth,
            } => write!(
                f,
                "LOAD_SCOPED r{}, c{}, depth={}",
                dest, var_name, scope_depth
            ),
            Instruction::StoreScoped {
                var_name,
                value,
                scope_depth,
            } => write!(
                f,
                "STORE_SCOPED c{}, r{}, depth={}",
                var_name, value, scope_depth
            ),
            Instruction::DeferredVarExpr { var_name, thunk } => {
                write!(f, "DEFERRED_VAR_EXPR c{}, r{}", var_name, thunk)
            }
            Instruction::EnterScope { scope_type } => write!(f, "ENTER_SCOPE {}", scope_type),
            Instruction::ExitScope => write!(f, "EXIT_SCOPE"),
            Instruction::CaptureVar {
                dest,
                var_name,
                scope_depth,
            } => write!(
                f,
                "CAPTURE_VAR r{}, c{}, depth={}",
                dest, var_name, scope_depth
            ),
            Instruction::ReturnIfErr { src } => write!(f, "RETURN_IF_ERR r{}", src),
            Instruction::BeginErrorCapture => write!(f, "BEGIN_ERROR_CAPTURE"),
            Instruction::EndErrorCapture => write!(f, "END_ERROR_CAPTURE"),
            Instruction::WrapOk { dest, src } => write!(f, "WRAP_OK r{}, r{}", dest, src),
            Instruction::RegisterLocalImplementation {
                source_type,
                target_type,
                implementation_function_name,
            } => write!(
                f,
                "REGISTER_LOCAL_IMPLEMENTATION c{}, c{}, c{}",
                source_type, target_type, implementation_function_name
            ),
            Instruction::RegisterLocalType {
                type_name,
                constructor_function_name,
            } => write!(
                f,
                "REGISTER_LOCAL_TYPE c{}, c{}",
                type_name, constructor_function_name
            ),
        }
    }
}

impl BytecodeProgram {
    /// Create a new empty bytecode program
    pub fn new() -> Self {
        Self {
            constants: Vec::new(),
            functions: Vec::new(),
            lambdas: Vec::new(),
            global_names: Vec::new(),
            entry_point: Vec::new(),
            entry_register_count: 0,
            namespaces: NamespaceRegistry::new(),
            source_map: SourceMap::new(),
            variable_metadata: AHashMap::new(),
            constant_index: AHashMap::new(),
        }
    }

    /// Serialize the bytecode program to JSON
    pub fn serialize(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("JSON serialization failed: {}", e))
    }

    /// Deserialize the bytecode program from JSON
    pub fn deserialize(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("JSON deserialization failed: {}", e))
    }

    /// Add variable metadata for emitter events
    pub fn add_variable_metadata(&mut self, key: String, metadata: VariableMetadata) {
        self.variable_metadata.insert(key, metadata);
    }

    /// Get variable metadata by key (namespace::variable_name)
    pub fn get_variable_metadata(&self, key: &str) -> Option<&VariableMetadata> {
        self.variable_metadata.get(key)
    }

    /// Hash a constant for the dedup index. Distinct-but-equal constants can
    /// hash apart (Val's cross-type equality), which only costs a duplicate
    /// pool entry; equal hashes are always verified with `==` before reuse.
    fn constant_hash(constant: &Constant) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = ahash::AHasher::default();
        match constant {
            Constant::Val(v) => {
                0u8.hash(&mut hasher);
                v.hash(&mut hasher);
            }
            Constant::FunctionRef(s) => {
                1u8.hash(&mut hasher);
                s.hash(&mut hasher);
            }
            Constant::TypeRef(s) => {
                2u8.hash(&mut hasher);
                s.hash(&mut hasher);
            }
            Constant::VariantTypeRef(tr) => {
                3u8.hash(&mut hasher);
                tr.name.hash(&mut hasher);
            }
            Constant::StringRef(s) => {
                4u8.hash(&mut hasher);
                s.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Rebuild the constant index from the pool (after deserialization or
    /// direct pushes to `constants`).
    fn rebuild_constant_index(&mut self) {
        self.constant_index.clear();
        for (i, c) in self.constants.iter().enumerate() {
            self.constant_index
                .entry(Self::constant_hash(c))
                .or_default()
                .push(i as ConstantId);
        }
    }

    /// Find an existing constant equal to `constant`, if any.
    fn find_constant(&mut self, constant: &Constant) -> Option<ConstantId> {
        if self.constant_index.is_empty() && !self.constants.is_empty() {
            self.rebuild_constant_index();
        }
        let hash = Self::constant_hash(constant);
        self.constant_index.get(&hash).and_then(|candidates| {
            candidates
                .iter()
                .copied()
                .find(|&id| self.constants.get(id as usize) == Some(constant))
        })
    }

    /// Add a constant to the pool, return its index
    pub fn add_constant(&mut self, constant: Constant) -> ConstantId {
        // Check if constant already exists to avoid duplicates
        if let Some(existing) = self.find_constant(&constant) {
            return existing;
        }

        let hash = Self::constant_hash(&constant);
        let id = self.constants.len() as ConstantId;
        self.constants.push(constant);
        self.constant_index.entry(hash).or_default().push(id);
        id
    }

    /// Add a Val as a constant, return its index
    pub fn add_val_constant(&mut self, val: crate::val::Val) -> ConstantId {
        self.add_constant(Constant::Val(val))
    }

    /// Add a function reference as a constant, return its index
    pub fn add_function_ref(&mut self, func_name: String) -> ConstantId {
        self.add_constant(Constant::FunctionRef(func_name.into()))
    }

    /// Add a type reference as a constant, return its index
    pub fn add_type_ref(&mut self, type_name: String) -> ConstantId {
        self.add_constant(Constant::TypeRef(type_name.into()))
    }

    /// Add a string reference as a constant, return its index
    pub fn add_string_ref(&mut self, s: String) -> ConstantId {
        self.add_constant(Constant::StringRef(s.into()))
    }

    /// Add a function to the program, return its index
    pub fn add_function(&mut self, function: FunctionInfo) -> FunctionId {
        let id = self.functions.len() as FunctionId;
        self.functions.push(function);
        id
    }

    /// Get the size in bytes of the bytecode program
    pub fn size_bytes(&self) -> usize {
        // Rough estimate - could be more precise
        std::mem::size_of::<Self>()
            + self.constants.len() * std::mem::size_of::<Constant>()
            + self.functions.len() * std::mem::size_of::<FunctionInfo>()
            + self.entry_point.len() * std::mem::size_of::<Instruction>()
    }

    /// Get the total number of instructions in the entry point (for REPL incremental execution)
    pub fn get_entry_instruction_count(&self) -> usize {
        self.entry_point.len()
    }

    /// Append instructions from another program (for cached bytecode + eval code)
    /// Returns the instruction index where the new instructions start
    pub fn append_instructions(&mut self, instructions: Vec<Instruction>) -> usize {
        let start_index = self.entry_point.len();
        self.entry_point.extend(instructions);
        start_index
    }

    /// Merge constants from another program and return a mapping of old IDs to new IDs
    pub fn merge_constants(&mut self, new_constants: Vec<Constant>) -> Vec<ConstantId> {
        let mut id_mapping = Vec::new();
        for constant in new_constants {
            // add_constant deduplicates via the constant index
            id_mapping.push(self.add_constant(constant));
        }
        id_mapping
    }

    /// Remap constant IDs in instructions based on a mapping
    pub fn remap_constant_ids(instructions: &mut [Instruction], id_mapping: &[ConstantId]) {
        for instruction in instructions.iter_mut() {
            match instruction {
                Instruction::LoadConst { constant, .. }
                    if (*constant as usize) < id_mapping.len() =>
                {
                    *constant = id_mapping[*constant as usize];
                }
                Instruction::LoadVar { var_name, .. }
                | Instruction::LoadVarOrDefault { var_name, .. }
                | Instruction::StoreVar { var_name, .. }
                | Instruction::SetNamespace {
                    namespace: var_name,
                }
                | Instruction::LoadGlobal { var_name, .. }
                | Instruction::StoreGlobal { var_name, .. }
                    if (*var_name as usize) < id_mapping.len() =>
                {
                    *var_name = id_mapping[*var_name as usize];
                }
                _ => {}
            }
        }
    }
}

impl Default for BytecodeProgram {
    fn default() -> Self {
        Self::new()
    }
}

/// Bytecode compilation statistics
#[derive(Debug, Clone, Default)]
pub struct CompilationStats {
    /// Number of instructions generated
    pub instruction_count: usize,
    /// Number of constants in pool
    pub constant_count: usize,
    /// Number of functions compiled
    pub function_count: usize,
    /// Compilation time in milliseconds
    pub compile_time_ms: u64,
    /// Memory usage in bytes
    pub memory_usage: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytecode_program_creation() {
        let mut program = BytecodeProgram::new();

        // Add some constants
        let str_const = program.add_constant(Constant::Val(crate::val::Val::from("hello")));
        let int_const = program.add_constant(Constant::Val(crate::val::Val::Int(42)));

        assert_eq!(str_const, 0);
        assert_eq!(int_const, 1);
        assert_eq!(program.constants.len(), 2);
    }

    #[test]
    fn test_constant_deduplication() {
        let mut program = BytecodeProgram::new();

        // Add the same constant twice
        let id1 = program.add_constant(Constant::Val(crate::val::Val::Int(42)));
        let id2 = program.add_constant(Constant::Val(crate::val::Val::Int(42)));

        // Should return the same ID
        assert_eq!(id1, id2);
        assert_eq!(program.constants.len(), 1);
    }

    #[test]
    fn test_instruction_display() {
        let inst = Instruction::LoadConst {
            dest: 0,
            constant: 5,
        };
        assert_eq!(format!("{}", inst), "LOAD_CONST r0, c5");

        let inst = Instruction::Add {
            dest: 2,
            left: 0,
            right: 1,
        };
        assert_eq!(format!("{}", inst), "ADD r2, r0, r1");

        let inst = Instruction::LoadVarOrDefault {
            dest: 1,
            var_name: 2,
            default_value: 3,
        };
        assert_eq!(format!("{}", inst), "LOAD_VAR_OR_DEFAULT r1, c2, c3");
        assert!(!format!("{}", inst).contains("LoadVarOrDefault"));

        let inst = Instruction::BeginErrorCapture;
        assert_eq!(format!("{}", inst), "BEGIN_ERROR_CAPTURE");
    }
}
