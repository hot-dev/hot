use crate::lang::bytecode::{
    BytecodeProgram, Constant, FlowResultModifier, FlowType, FunctionId, FunctionInfo, Instruction,
    LambdaInfo,
};
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use ahash::AHashMap;
use cranelift_codegen::ir::{
    AbiParam, FuncRef, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Type, UserFuncName,
    types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};
use fastnum::D256;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static GLOBAL_JIT_COMPILE_COUNT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_JIT_BAILOUT_COUNT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_LAMBDA_INTERPRETER_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_LAMBDA_JIT_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_LAMBDA_JIT_FALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static JIT_VM_PTR: Cell<*mut VirtualMachine> = const { Cell::new(std::ptr::null_mut()) };
    static JIT_STACK_LIMIT: Cell<u64> = const { Cell::new(0) };
}

const JIT_STACK_GUARD_BUDGET: u64 = 60_000_000;
const JIT_STACK_EXHAUSTED: i64 = 1;

fn ensure_jit_stack_limit() {
    JIT_STACK_LIMIT.with(|cell| {
        if cell.get() == 0 {
            let anchor: u64 = 0;
            let sp = std::ptr::addr_of!(anchor) as u64;
            cell.set(sp.saturating_sub(JIT_STACK_GUARD_BUDGET));
        }
    });
}

unsafe extern "C" fn hot_jit_get_stack_limit() -> i64 {
    JIT_STACK_LIMIT.with(|cell| cell.get() as i64)
}

/// Set the thread-local VM pointer before calling JIT code. Returns the previous
/// value so the caller can restore it (supports re-entrant JIT calls).
pub fn set_jit_vm_ptr(vm: *mut VirtualMachine) -> *mut VirtualMachine {
    JIT_VM_PTR.with(|cell| cell.replace(vm))
}

/// Retrieve the thread-local VM pointer (used by JIT helper functions).
fn get_jit_vm_ptr() -> *mut VirtualMachine {
    JIT_VM_PTR.with(|cell| cell.get())
}

/// Runtime mode for JIT behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitMode {
    Disabled,
    Enabled,
}

/// User-visible JIT configuration derived from conf values.
///
/// All settings flow through the standard conf system:
///   - `hot.hot` template (with `::env/get` for env var support)
///   - CLI flags (`--jit`, `--jit.threshold`)
///   - `apply_env_vars` auto-mapping (`HOT_JIT_MODE`, `HOT_JIT_THRESHOLD`)
///
/// The JIT code itself never reads environment variables directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitConfig {
    pub mode: JitMode,
    pub threshold: u32,
    /// Kill switch for higher-order-function pipeline fusion. Defaults on; can
    /// be disabled via conf `jit.hof.fusion`, CLI `--jit-hof-fusion`, or env
    /// `HOT_JIT_HOF_FUSION`. Disabling falls back to the per-element lambda-JIT
    /// / interpreter path with no behavior change.
    pub hof_fusion: bool,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            mode: JitMode::Enabled,
            threshold: 100,
            hof_fusion: true,
        }
    }
}

impl JitConfig {
    pub fn from_conf(conf: &Val) -> Self {
        let mut cfg = Self::default();

        // Read mode from conf key "jit.mode".
        // Falls back to checking plain "jit" key (supports HOT_JIT env var
        // auto-mapping which sets conf["jit"] = "value" directly).
        let mode_raw = {
            let from_mode_key = conf.get_str_or_default("jit.mode", "");
            if from_mode_key.is_empty() {
                match conf.get("jit") {
                    Some(Val::Str(s)) => s.to_string(),
                    _ => String::new(),
                }
            } else {
                from_mode_key
            }
        };

        cfg.mode = match mode_raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "disabled" => JitMode::Disabled,
            _ => JitMode::Enabled,
        };

        cfg.threshold = conf.get_int_or_default("jit.threshold", 100).max(1) as u32;
        cfg.hof_fusion = conf.get_bool_or_default("jit.hof.fusion", true);

        // Gate on platform availability — disable on unsupported targets
        // regardless of what conf says.
        let status = CodeMemoryStatus::detect();
        if status.availability == JitAvailability::Unsupported {
            cfg.mode = JitMode::Disabled;
        }

        cfg
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self.mode, JitMode::Enabled)
    }

    /// HOF pipeline fusion requires JIT enabled and the fusion kill switch on.
    pub fn hof_fusion_enabled(&self) -> bool {
        self.is_enabled() && self.hof_fusion
    }
}

/// High-level code-memory backend selection/reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeMemoryBackend {
    None,
    System,
    Arena,
    AppleMapJit,
}

/// Coarse-grained readiness for native code memory on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitAvailability {
    Unsupported,
    Experimental,
    Available,
}

/// Platform/code-memory capability report for diagnostics and gating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMemoryStatus {
    pub availability: JitAvailability,
    pub backend: CodeMemoryBackend,
    pub reason: Option<String>,
}

impl CodeMemoryStatus {
    pub fn detect() -> Self {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Self {
                availability: JitAvailability::Available,
                backend: CodeMemoryBackend::AppleMapJit,
                reason: None,
            };
        }

        #[cfg(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "windows", target_arch = "x86_64")
        ))]
        {
            return Self {
                availability: JitAvailability::Available,
                backend: CodeMemoryBackend::System,
                reason: None,
            };
        }

        #[allow(unreachable_code)]
        Self {
            availability: JitAvailability::Unsupported,
            backend: CodeMemoryBackend::None,
            reason: Some(
                "No JIT code-memory backend has been validated for this target".to_string(),
            ),
        }
    }
}

/// Placeholder abstraction for code-memory ownership/finalization.
///
/// This intentionally starts small: the first implementation goal is to make the
/// allocator/backend choice explicit in Hot before native code emission is added.
pub trait CodeMemoryProvider: Send + Sync {
    fn status(&self) -> &CodeMemoryStatus;
}

#[derive(Debug, Clone)]
pub struct DeferredCodeMemoryProvider {
    status: CodeMemoryStatus,
}

impl DeferredCodeMemoryProvider {
    pub fn new(status: CodeMemoryStatus) -> Self {
        Self { status }
    }
}

impl CodeMemoryProvider for DeferredCodeMemoryProvider {
    fn status(&self) -> &CodeMemoryStatus {
        &self.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JitTypeTag {
    Null,
    Bool,
    Int,
    Dec,
    Str,
    Vec,
    Map,
    TypedMap(String),
    Boxed(String),
    Byte,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeSig {
    pub arity: u8,
    pub args: Vec<JitTypeTag>,
}

impl TypeSig {
    pub fn from_args(args: &[Val]) -> Self {
        let args: Vec<JitTypeTag> = args.iter().map(JitTypeTag::from_val).collect();
        Self {
            arity: args.len() as u8,
            args,
        }
    }
}

impl JitTypeTag {
    pub fn from_val(val: &Val) -> Self {
        match val {
            Val::Null => Self::Null,
            Val::Bool(_) => Self::Bool,
            Val::Int(_) => Self::Int,
            Val::Dec(_) => Self::Dec,
            Val::Str(_) => Self::Str,
            Val::Vec(_) => Self::Vec,
            Val::Map(map) => map
                .get(&Val::from("$type"))
                .and_then(|v| match v {
                    Val::Str(s) => Some(Self::TypedMap(s.to_string())),
                    _ => None,
                })
                .unwrap_or(Self::Map),
            Val::Box(v) => Self::Boxed(v.type_name().to_string()),
            Val::Byte(_) => Self::Byte,
            Val::Bytes(_) => Self::Bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JitFunctionProfile {
    pub call_count: u32,
    pub stable_signature: Option<TypeSig>,
    pub observed_signatures: AHashMap<TypeSig, u32>,
    pub guard_failures: u32,
    pub deopts: u32,
    pub cumulative_compile_time_ms: u64,
    pub do_not_jit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LambdaJitKey {
    structural_hash: u64,
    register_count: u32,
    is_lazy_param: bool,
    args_signature: TypeSig,
    capture_signature: TypeSig,
    capture_function_names: Vec<Option<String>>,
}

impl LambdaJitKey {
    fn new(lambda: &LambdaInfo, args: &[Val], captures: &[Val]) -> Self {
        let mut hasher = DefaultHasher::new();
        lambda.parameters.hash(&mut hasher);
        lambda.instructions.hash(&mut hasher);
        lambda.register_count.hash(&mut hasher);
        lambda.capture_vars.hash(&mut hasher);
        lambda.defining_namespace.hash(&mut hasher);
        lambda.is_lazy_param.hash(&mut hasher);

        Self {
            structural_hash: hasher.finish(),
            register_count: lambda.register_count,
            is_lazy_param: lambda.is_lazy_param,
            args_signature: TypeSig::from_args(args),
            capture_signature: TypeSig::from_args(captures),
            capture_function_names: captures
                .iter()
                .map(capture_function_ref_name)
                .collect::<Vec<_>>(),
        }
    }
}

fn capture_function_ref_name(capture: &Val) -> Option<String> {
    if let Val::Box(boxed) = capture
        && let Some(function_ref) = boxed
            .as_any()
            .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
    {
        return Some(function_ref.name().to_string());
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitValueKind {
    Int,
    Bool,
    Dec,
    Null,
    OwnedVal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Int,
    Bool,
    Dec,
    Null,
    OwnedVal,
    TypeTag,
    StringConst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownCoreCall {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Not,
    IsZero,
    IsNull,
    IsSome,
    And,
    Or,
    Length,
}

#[derive(Debug, Clone)]
struct AbiLayout {
    offsets: Vec<usize>,
    size: usize,
}

#[derive(Debug, Clone, Copy)]
struct JitHelperRefs {
    get_stack_limit: FuncRef,
    copy_dec: FuncRef,
    add_numeric: FuncRef,
    sub_numeric: FuncRef,
    mul_numeric: FuncRef,
    div_numeric: FuncRef,
    mod_numeric: FuncRef,
    ne_numeric: FuncRef,
    eq_numeric: FuncRef,
    eq_general: FuncRef,
    gt_numeric: FuncRef,
    gte_numeric: FuncRef,
    lt_numeric: FuncRef,
    lte_numeric: FuncRef,
    truthy_general: FuncRef,
    clone_owned_val: FuncRef,
    drop_owned_val: FuncRef,
    call_vm: FuncRef,
    binop_general: FuncRef,
    cmp_general: FuncRef,
    is_err: FuncRef,
    call_vm_by_name: FuncRef,
    promote_to_owned: FuncRef,
    make_vec: FuncRef,
    call_lib: FuncRef,
    dot_access: FuncRef,
    merge_maps: FuncRef,
    template_interpolate: FuncRef,
    extract_inner_val: FuncRef,
    ensure_result: FuncRef,
    set_element: FuncRef,
    construct_typed: FuncRef,
    lookup_var: FuncRef,
    vec_push: FuncRef,
    get_type_path: FuncRef,
    is_type_check: FuncRef,
    str_ends_with: FuncRef,
    str_starts_with: FuncRef,
    lookup_var_or_default: FuncRef,
    call_with_spread: FuncRef,
    populate_closure: FuncRef,
    dynamic_dot_access: FuncRef,
    dynamic_dot_set: FuncRef,
    dot_set: FuncRef,
    make_vec_with_spread: FuncRef,
    store_global: FuncRef,
    set_namespace: FuncRef,
    call_native: FuncRef,
    define_function: FuncRef,
    load_scoped: FuncRef,
    capture_var: FuncRef,
    length: FuncRef,
    is_null: FuncRef,
}

#[derive(Debug, Clone)]
struct ActiveFlowState {
    flow_type: FlowType,
    result_modifier: FlowResultModifier,
    end_ip: usize,
    result_reg: Option<u32>,
    merged_result_kind: Option<RawKind>,
    pre_flow_owned: Vec<usize>,
    accumulator_var: Option<Variable>,
    /// Registers defined between BeginFlow and the first branch whose kinds
    /// must NOT be cleared after branch cleanup (e.g., GetTypePath subject).
    flow_header_regs: Vec<usize>,
    /// For Pipe flows: the register holding the last Pipe step result.
    last_pipe_reg: Option<u32>,
}

pub struct JitCompiledFunction {
    signature: TypeSig,
    result_kind: JitValueKind,
    module: JITModule,
    func_id: FuncId,
}

impl JitCompiledFunction {
    pub fn matches_args(&self, args: &[Val]) -> bool {
        self.signature == TypeSig::from_args(args)
    }

    pub fn call(&self, args: &[Val]) -> Result<Val, String> {
        ensure_jit_stack_limit();
        let raw_args = self.signature.encode_args(args)?;
        let mut raw_result = self.result_kind.make_buffer();
        let fn_ptr = self.module.get_finalized_function(self.func_id);
        let jit_fn: unsafe extern "C" fn(*const u8, *mut u8) -> i64 =
            unsafe { mem::transmute(fn_ptr) };
        let ret = unsafe {
            jit_fn(
                raw_args.as_ptr().cast::<u8>(),
                raw_result.as_mut_ptr().cast::<u8>(),
            )
        };
        if ret == JIT_STACK_EXHAUSTED {
            return Err("JIT_STACK_EXHAUSTED".to_string());
        }
        if ret != 0 {
            return unsafe { Ok(take_owned_val(ret)) };
        }
        self.result_kind.decode_result(&raw_result)
    }
}

fn synthetic_function_info_from_lambda(
    program: &mut BytecodeProgram,
    lambda: &LambdaInfo,
    captures: &[Val],
    effective_arity: usize,
) -> Result<FunctionInfo, String> {
    let arity = u8::try_from(effective_arity)
        .map_err(|_| unsupported_jit_bailout("lambda_arity", "lambda arity exceeds u8"))?;
    let mut param_names = Vec::with_capacity(lambda.capture_vars.len() + lambda.parameters.len());
    param_names.extend(lambda.capture_vars.iter().cloned());
    param_names.extend(lambda.parameters.iter().cloned());

    let mut captured_functions = AHashMap::new();
    for (capture_name, capture_value) in lambda.capture_vars.iter().zip(captures.iter()) {
        if let Val::Box(boxed) = capture_value
            && let Some(function_ref) = boxed
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
            )
        {
            captured_functions.insert(capture_name.clone(), function_ref.name().to_string());
        }
    }

    let mut instructions = lambda.instructions.clone();
    if !captured_functions.is_empty() {
        for instruction in &mut instructions {
            if let Instruction::LoadVar { dest, var_name } = instruction
                && let Some(Constant::Val(Val::Str(name))) =
                    program.constants.get(*var_name as usize)
                && let Some(function_name) = captured_functions.get(name.as_ref())
            {
                let function_const = program.constants.len() as u32;
                program
                    .constants
                    .push(Constant::FunctionRef(function_name.clone().into()));
                *instruction = Instruction::LoadConst {
                    dest: *dest,
                    constant: function_const,
                };
            }
        }
    }

    Ok(FunctionInfo {
        name: "<lambda>".to_string(),
        namespace: lambda.defining_namespace.clone(),
        arity,
        is_variadic: false,
        param_names,
        param_types: Vec::new(),
        return_type: 0,
        lazy_params: vec![false; effective_arity],
        flow_type: None,
        instructions,
        register_count: lambda.register_count,
        source: None,
    })
}

impl Default for JitFunctionProfile {
    fn default() -> Self {
        Self {
            call_count: 0,
            stable_signature: None,
            observed_signatures: AHashMap::new(),
            guard_failures: 0,
            deopts: 0,
            cumulative_compile_time_ms: 0,
            do_not_jit: false,
        }
    }
}

impl JitFunctionProfile {
    pub fn record_call(&mut self, args: &[Val]) {
        self.call_count = self.call_count.saturating_add(1);

        let sig = TypeSig::from_args(args);
        let count = self.observed_signatures.entry(sig.clone()).or_insert(0);
        *count = count.saturating_add(1);

        self.stable_signature = self
            .observed_signatures
            .iter()
            .max_by_key(|(_, seen)| *seen)
            .map(|(sig, _)| sig.clone());
    }
}

pub struct JitRuntimeState {
    pub config: JitConfig,
    pub code_memory_status: CodeMemoryStatus,
    pub function_profiles: Vec<JitFunctionProfile>,
    compiled_functions: Vec<Option<JitCompiledFunction>>,
    lambda_profiles: AHashMap<LambdaJitKey, JitFunctionProfile>,
    compiled_lambdas: AHashMap<LambdaJitKey, JitCompiledFunction>,
}

impl JitRuntimeState {
    pub fn new(function_count: usize, conf: &Val) -> Self {
        Self {
            config: JitConfig::from_conf(conf),
            code_memory_status: CodeMemoryStatus::detect(),
            function_profiles: vec![JitFunctionProfile::default(); function_count],
            compiled_functions: std::iter::repeat_with(|| None)
                .take(function_count)
                .collect(),
            lambda_profiles: AHashMap::new(),
            compiled_lambdas: AHashMap::new(),
        }
    }

    pub fn record_call(&mut self, function_id: u32, args: &[Val]) {
        if let Some(profile) = self.function_profiles.get_mut(function_id as usize) {
            if profile.do_not_jit || profile.call_count > self.config.threshold {
                return;
            }
            profile.record_call(args);
        }
    }

    pub fn function_profile(&self, function_id: u32) -> Option<&JitFunctionProfile> {
        self.function_profiles.get(function_id as usize)
    }

    pub fn has_compiled_function(&self, function_id: FunctionId) -> bool {
        self.compiled_functions
            .get(function_id as usize)
            .and_then(|entry| entry.as_ref())
            .is_some()
    }

    pub fn try_call_compiled(
        &mut self,
        function_id: FunctionId,
        args: &[Val],
    ) -> Result<Option<Val>, String> {
        let Some(entry) = self
            .compiled_functions
            .get(function_id as usize)
            .and_then(|entry| entry.as_ref())
        else {
            return Ok(None);
        };

        if entry.matches_args(args) {
            return match entry.call(args) {
                Ok(val) => Ok(Some(val)),
                Err(e) if e == "JIT_STACK_EXHAUSTED" => Ok(None),
                Err(e) => Err(e),
            };
        }

        if let Some(profile) = self.function_profiles.get_mut(function_id as usize) {
            profile.guard_failures = profile.guard_failures.saturating_add(1);
        }

        Ok(None)
    }

    pub fn should_compile_now(&self, function_id: FunctionId, args: &[Val]) -> bool {
        if !self.config.is_enabled() {
            return false;
        }
        if matches!(
            self.code_memory_status.availability,
            JitAvailability::Unsupported
        ) {
            return false;
        }

        let Some(profile) = self.function_profiles.get(function_id as usize) else {
            return false;
        };
        if profile.do_not_jit {
            return false;
        }
        if self
            .compiled_functions
            .get(function_id as usize)
            .and_then(|entry| entry.as_ref())
            .is_some()
        {
            return false;
        }

        let current_sig = TypeSig::from_args(args);
        profile.call_count >= self.config.threshold
            && profile.stable_signature.as_ref() == Some(&current_sig)
    }

    pub fn compile_function(
        &mut self,
        program: &BytecodeProgram,
        function_id: FunctionId,
        function_info: &FunctionInfo,
        args: &[Val],
    ) -> Result<(), String> {
        let start = std::time::Instant::now();
        let signature = TypeSig::from_args(args);
        tracing::trace!(
            "[JIT-BC] '{}' (id={}) sig={:?} body:\n{}",
            function_info.name,
            function_id,
            signature,
            function_info
                .instructions
                .iter()
                .enumerate()
                .map(|(i, inst)| format!("  [{:3}] {:?}", i, inst))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let compiled = compile_supported_function(program, function_id, function_info, &signature)?;

        if let Some(slot) = self.compiled_functions.get_mut(function_id as usize) {
            *slot = Some(compiled);
        }
        GLOBAL_JIT_COMPILE_COUNT.fetch_add(1, Ordering::Relaxed);

        if let Some(profile) = self.function_profiles.get_mut(function_id as usize) {
            profile.cumulative_compile_time_ms = profile
                .cumulative_compile_time_ms
                .saturating_add(start.elapsed().as_millis() as u64);
        }

        Ok(())
    }

    pub fn try_call_compiled_lambda(
        &mut self,
        program: &BytecodeProgram,
        lambda: &LambdaInfo,
        args: &[Val],
        captures: &[Val],
    ) -> Result<Option<Val>, String> {
        if !self.config.is_enabled()
            || matches!(
                self.code_memory_status.availability,
                JitAvailability::Unsupported
            )
            || lambda.is_lazy_param
        {
            GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }

        let mut effective_args = Vec::with_capacity(args.len() + captures.len());
        effective_args.extend_from_slice(captures);
        effective_args.extend_from_slice(args);
        let signature = TypeSig::from_args(&effective_args);
        let key = LambdaJitKey::new(lambda, args, captures);

        if let Some(compiled) = self.compiled_lambdas.get(&key) {
            if compiled.matches_args(&effective_args) {
                return match compiled.call(&effective_args) {
                    Ok(val) => {
                        GLOBAL_LAMBDA_JIT_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
                        Ok(Some(val))
                    }
                    Err(e) if e == "JIT_STACK_EXHAUSTED" => {
                        GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                        Ok(None)
                    }
                    Err(e) => Err(e),
                };
            }

            if let Some(profile) = self.lambda_profiles.get_mut(&key) {
                profile.guard_failures = profile.guard_failures.saturating_add(1);
            }
        }

        let profile = self.lambda_profiles.entry(key.clone()).or_default();
        if profile.do_not_jit || profile.call_count > self.config.threshold {
            GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        profile.record_call(&effective_args);

        if profile.call_count < self.config.threshold
            || profile.stable_signature.as_ref() != Some(&signature)
        {
            GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }

        let mut synthetic_program = program.clone();
        let function_info = synthetic_function_info_from_lambda(
            &mut synthetic_program,
            lambda,
            captures,
            effective_args.len(),
        )?;
        let start = std::time::Instant::now();
        match compile_supported_function(&synthetic_program, u32::MAX, &function_info, &signature) {
            Ok(compiled) => {
                GLOBAL_JIT_COMPILE_COUNT.fetch_add(1, Ordering::Relaxed);
                if let Some(profile) = self.lambda_profiles.get_mut(&key) {
                    profile.cumulative_compile_time_ms = profile
                        .cumulative_compile_time_ms
                        .saturating_add(start.elapsed().as_millis() as u64);
                }
                self.compiled_lambdas.insert(key.clone(), compiled);
                if let Some(compiled) = self.compiled_lambdas.get(&key) {
                    match compiled.call(&effective_args) {
                        Ok(val) => {
                            GLOBAL_LAMBDA_JIT_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
                            Ok(Some(val))
                        }
                        Err(e) if e == "JIT_STACK_EXHAUSTED" => {
                            GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                    Ok(None)
                }
            }
            Err(err) => {
                tracing::debug!(
                    "[JIT] lambda compilation skipped (reason={}): {}",
                    jit_bailout_reason(&err),
                    err
                );
                increment_jit_bailout_count();
                if !is_retryable_jit_bailout(&err)
                    && let Some(profile) = self.lambda_profiles.get_mut(&key)
                {
                    profile.do_not_jit = true;
                }
                GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
        }
    }

    pub fn mark_do_not_jit(&mut self, function_id: FunctionId) {
        if let Some(profile) = self.function_profiles.get_mut(function_id as usize) {
            profile.do_not_jit = true;
        }
    }
}

pub fn global_jit_compile_count() -> usize {
    GLOBAL_JIT_COMPILE_COUNT.load(Ordering::Relaxed)
}

pub fn global_jit_bailout_count() -> usize {
    GLOBAL_JIT_BAILOUT_COUNT.load(Ordering::Relaxed)
}

pub fn increment_jit_bailout_count() {
    GLOBAL_JIT_BAILOUT_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn increment_lambda_interpreter_call_count() {
    GLOBAL_LAMBDA_INTERPRETER_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn jit_bailout_reason(error: &str) -> &str {
    error
        .split_once(':')
        .map(|(reason, _)| reason)
        .unwrap_or("compile-error")
}

pub fn is_retryable_jit_bailout(error: &str) -> bool {
    jit_bailout_reason(error).starts_with("unsupported.")
}

fn unsupported_jit_bailout(reason: &'static str, message: impl Into<String>) -> String {
    format!("unsupported.{}: {}", reason, message.into())
}

pub struct JitStats {
    pub compilations: usize,
    pub bailouts: usize,
    pub lambda_interpreter_calls: usize,
    pub lambda_jit_calls: usize,
    pub lambda_jit_fallbacks: usize,
}

pub fn global_jit_stats() -> JitStats {
    JitStats {
        compilations: GLOBAL_JIT_COMPILE_COUNT.load(Ordering::Relaxed),
        bailouts: GLOBAL_JIT_BAILOUT_COUNT.load(Ordering::Relaxed),
        lambda_interpreter_calls: GLOBAL_LAMBDA_INTERPRETER_CALL_COUNT.load(Ordering::Relaxed),
        lambda_jit_calls: GLOBAL_LAMBDA_JIT_CALL_COUNT.load(Ordering::Relaxed),
        lambda_jit_fallbacks: GLOBAL_LAMBDA_JIT_FALLBACK_COUNT.load(Ordering::Relaxed),
    }
}

pub fn log_jit_stats_summary() {
    let stats = global_jit_stats();
    if stats.compilations > 0
        || stats.bailouts > 0
        || stats.lambda_interpreter_calls > 0
        || stats.lambda_jit_calls > 0
        || stats.lambda_jit_fallbacks > 0
    {
        tracing::debug!(
            "[JIT] stats: {} compiled, {} bailed out, lambda: {} jitted, {} interpreted, {} fallback probes",
            stats.compilations,
            stats.bailouts,
            stats.lambda_jit_calls,
            stats.lambda_interpreter_calls,
            stats.lambda_jit_fallbacks
        );
    }
}

impl TypeSig {
    fn encode_args(&self, args: &[Val]) -> Result<Vec<u64>, String> {
        if self != &TypeSig::from_args(args) {
            return Err("JIT type guard mismatch".to_string());
        }

        let layout = AbiLayout::for_args(&self.args)?;
        let mut words = vec![0u64; layout.word_len()];

        for (idx, (tag, arg)) in self.args.iter().zip(args.iter()).enumerate() {
            let offset = layout.offset(idx)?;
            match (tag, arg) {
                (JitTypeTag::Null, Val::Null) => write_i64(&mut words, offset, 0),
                (JitTypeTag::Int, Val::Int(n)) => write_i64(&mut words, offset, *n),
                (JitTypeTag::Bool, Val::Bool(b)) => {
                    write_i64(&mut words, offset, if *b { 1 } else { 0 })
                }
                (JitTypeTag::Dec, Val::Dec(d)) => write_dec(&mut words, offset, *d),
                (
                    JitTypeTag::Str
                    | JitTypeTag::Vec
                    | JitTypeTag::Map
                    | JitTypeTag::TypedMap(_)
                    | JitTypeTag::Boxed(_)
                    | JitTypeTag::Byte
                    | JitTypeTag::Bytes,
                    value,
                ) => write_i64(&mut words, offset, new_owned_val(value.clone())),
                _ => return Err(format!("Unsupported JIT argument type: {:?}", tag)),
            }
        }

        Ok(words)
    }
}

impl JitTypeTag {
    fn raw_kind(&self) -> Option<RawKind> {
        match self {
            JitTypeTag::Null => Some(RawKind::Null),
            JitTypeTag::Int => Some(RawKind::Int),
            JitTypeTag::Bool => Some(RawKind::Bool),
            JitTypeTag::Dec => Some(RawKind::Dec),
            JitTypeTag::Str
            | JitTypeTag::Vec
            | JitTypeTag::Map
            | JitTypeTag::TypedMap(_)
            | JitTypeTag::Boxed(_)
            | JitTypeTag::Byte
            | JitTypeTag::Bytes => Some(RawKind::OwnedVal),
        }
    }
}

impl AbiLayout {
    fn for_args(args: &[JitTypeTag]) -> Result<Self, String> {
        let kinds: Result<Vec<_>, _> = args
            .iter()
            .map(|tag| {
                tag.raw_kind()
                    .ok_or_else(|| format!("Unsupported JIT specialization type: {:?}", tag))
            })
            .collect();
        Self::for_raw_kinds(&kinds?)
    }

    fn for_raw_kinds(kinds: &[RawKind]) -> Result<Self, String> {
        let mut offsets = Vec::with_capacity(kinds.len());
        let mut size = 0usize;
        for kind in kinds {
            size = align_up(size, raw_kind_align(*kind));
            offsets.push(size);
            size += raw_kind_size(*kind);
        }
        Ok(Self {
            offsets,
            size: align_up(size, 8),
        })
    }

    fn for_result(kind: JitValueKind) -> Self {
        let raw_kind = kind.raw_kind();
        let size = align_up(raw_kind_size(raw_kind), 8);
        Self {
            offsets: vec![0],
            size,
        }
    }

    fn offset(&self, idx: usize) -> Result<usize, String> {
        self.offsets
            .get(idx)
            .copied()
            .ok_or_else(|| format!("Missing ABI slot {}", idx))
    }

    fn word_len(&self) -> usize {
        self.size.max(8) / 8
    }
}

impl JitValueKind {
    fn raw_kind(self) -> RawKind {
        match self {
            JitValueKind::Int => RawKind::Int,
            JitValueKind::Bool => RawKind::Bool,
            JitValueKind::Dec => RawKind::Dec,
            JitValueKind::Null => RawKind::Null,
            JitValueKind::OwnedVal => RawKind::OwnedVal,
        }
    }

    fn make_buffer(self) -> Vec<u64> {
        vec![0u64; AbiLayout::for_result(self).word_len()]
    }

    fn decode_result(self, words: &[u64]) -> Result<Val, String> {
        Ok(match self {
            JitValueKind::Int => Val::Int(read_i64(words, 0)),
            JitValueKind::Bool => Val::Bool(read_i64(words, 0) != 0),
            JitValueKind::Dec => Val::Dec(read_dec(words, 0)),
            JitValueKind::Null => Val::Null,
            JitValueKind::OwnedVal => unsafe { take_owned_val(read_i64(words, 0)) },
        })
    }
}

fn raw_kind_size(kind: RawKind) -> usize {
    match kind {
        RawKind::Int
        | RawKind::Bool
        | RawKind::Null
        | RawKind::OwnedVal
        | RawKind::TypeTag
        | RawKind::StringConst => mem::size_of::<i64>(),
        RawKind::Dec => mem::size_of::<D256>(),
    }
}

fn raw_kind_align(kind: RawKind) -> usize {
    match kind {
        RawKind::Int
        | RawKind::Bool
        | RawKind::Null
        | RawKind::OwnedVal
        | RawKind::TypeTag
        | RawKind::StringConst => mem::align_of::<i64>(),
        RawKind::Dec => mem::align_of::<D256>(),
    }
}

fn align_up(size: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (size + align - 1) & !(align - 1)
}

fn write_i64(words: &mut [u64], offset: usize, value: i64) {
    unsafe {
        let ptr = words.as_mut_ptr().cast::<u8>().add(offset).cast::<i64>();
        *ptr = value;
    }
}

fn read_i64(words: &[u64], offset: usize) -> i64 {
    unsafe {
        let ptr = words.as_ptr().cast::<u8>().add(offset).cast::<i64>();
        *ptr
    }
}

fn write_dec(words: &mut [u64], offset: usize, value: D256) {
    unsafe {
        let ptr = words.as_mut_ptr().cast::<u8>().add(offset).cast::<D256>();
        *ptr = value;
    }
}

fn read_dec(words: &[u64], offset: usize) -> D256 {
    unsafe {
        let ptr = words.as_ptr().cast::<u8>().add(offset).cast::<D256>();
        *ptr
    }
}

fn compile_supported_function(
    program: &BytecodeProgram,
    function_id: FunctionId,
    function_info: &FunctionInfo,
    signature: &TypeSig,
) -> Result<JitCompiledFunction, String> {
    if signature.arity != function_info.arity {
        return Err("JIT arity mismatch".to_string());
    }
    if function_info.is_variadic {
        // Variadic functions expect their last param (`...rest`) to be the packed
        // Vec of remaining args, but `try_call_compiled` passes raw unpacked args
        // to the JIT. Forcing `sig.args[last] = Vec` makes the type guard match
        // calls like `or(Null, [])` where the second raw arg happens to be a Vec,
        // but the JIT body then treats that arg as the rest vec directly (e.g.
        // `is-empty(rest)` returns true for `[]` even though the real rest is
        // `[[]]`), producing wrong results. Skip JIT for variadic functions
        // until we pack rest args before invocation.
        return Err(unsupported_jit_bailout(
            "variadic",
            "JIT does not yet compile variadic functions",
        ));
    }
    if function_info.lazy_params.iter().any(|&lp| lp) {
        return Err(unsupported_jit_bailout(
            "lazy_params",
            "JIT does not yet compile functions with lazy parameters (scope visibility gap)"
                .to_string(),
        ));
    }
    if function_has_unsupported_lazy_paths(program, function_info) {
        return Err(unsupported_jit_bailout(
            "lazy_thunk_shape",
            "JIT does not yet compile this lazy thunk bytecode shape safely",
        ));
    }
    if function_has_jit_bailout_hotlib_dependency(program, function_id, function_info) {
        return Err(unsupported_jit_bailout(
            "hotlib_policy",
            "JIT policy marks a hotlib dependency as interpreter-only",
        ));
    }
    for tag in &signature.args {
        if tag.raw_kind().is_none() {
            return Err(unsupported_jit_bailout(
                "specialization_type",
                format!("Unsupported JIT specialization type: {:?}", tag),
            ));
        }
    }

    let mut flag_builder = settings::builder();
    flag_builder
        .set("use_colocated_libcalls", "false")
        .map_err(|e| e.to_string())?;
    flag_builder
        .set("enable_verifier", "false")
        .map_err(|e| e.to_string())?;
    let isa_builder = cranelift_native::builder().map_err(|e| e.to_string())?;
    let isa = isa_builder
        .finish(settings::Flags::new(flag_builder))
        .map_err(|e| e.to_string())?;

    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    builder.symbol(
        "hot_jit_get_stack_limit",
        hot_jit_get_stack_limit as *const u8,
    );
    builder.symbol("hot_jit_copy_dec", hot_jit_copy_dec as *const u8);
    builder.symbol("hot_jit_add_numeric", hot_jit_add_numeric as *const u8);
    builder.symbol("hot_jit_sub_numeric", hot_jit_sub_numeric as *const u8);
    builder.symbol("hot_jit_mul_numeric", hot_jit_mul_numeric as *const u8);
    builder.symbol("hot_jit_div_numeric", hot_jit_div_numeric as *const u8);
    builder.symbol("hot_jit_mod_numeric", hot_jit_mod_numeric as *const u8);
    builder.symbol("hot_jit_ne_numeric", hot_jit_ne_numeric as *const u8);
    builder.symbol("hot_jit_eq_numeric", hot_jit_eq_numeric as *const u8);
    builder.symbol("hot_jit_eq_general", hot_jit_eq_general as *const u8);
    builder.symbol("hot_jit_gt_numeric", hot_jit_gt_numeric as *const u8);
    builder.symbol("hot_jit_gte_numeric", hot_jit_gte_numeric as *const u8);
    builder.symbol("hot_jit_lt_numeric", hot_jit_lt_numeric as *const u8);
    builder.symbol("hot_jit_lte_numeric", hot_jit_lte_numeric as *const u8);
    builder.symbol(
        "hot_jit_truthy_general",
        hot_jit_truthy_general as *const u8,
    );
    builder.symbol(
        "hot_jit_clone_owned_val",
        hot_jit_clone_owned_val as *const u8,
    );
    builder.symbol(
        "hot_jit_drop_owned_val",
        hot_jit_drop_owned_val as *const u8,
    );
    builder.symbol("hot_jit_call_vm", hot_jit_call_vm as *const u8);
    builder.symbol("hot_jit_binop_general", hot_jit_binop_general as *const u8);
    builder.symbol("hot_jit_cmp_general", hot_jit_cmp_general as *const u8);
    builder.symbol("hot_jit_is_err", hot_jit_is_err as *const u8);
    builder.symbol(
        "hot_jit_call_vm_by_name",
        hot_jit_call_vm_by_name as *const u8,
    );
    builder.symbol(
        "hot_jit_promote_to_owned",
        hot_jit_promote_to_owned as *const u8,
    );
    builder.symbol("hot_jit_make_vec", hot_jit_make_vec as *const u8);
    builder.symbol("hot_jit_call_lib", hot_jit_call_lib as *const u8);
    builder.symbol("hot_jit_dot_access", hot_jit_dot_access as *const u8);
    builder.symbol("hot_jit_merge_maps", hot_jit_merge_maps as *const u8);
    builder.symbol(
        "hot_jit_template_interpolate",
        hot_jit_template_interpolate as *const u8,
    );
    builder.symbol(
        "hot_jit_extract_inner_val",
        hot_jit_extract_inner_val as *const u8,
    );
    builder.symbol("hot_jit_ensure_result", hot_jit_ensure_result as *const u8);
    builder.symbol("hot_jit_set_element", hot_jit_set_element as *const u8);
    builder.symbol(
        "hot_jit_construct_typed",
        hot_jit_construct_typed as *const u8,
    );
    builder.symbol("hot_jit_lookup_var", hot_jit_lookup_var as *const u8);
    builder.symbol("hot_jit_vec_push", hot_jit_vec_push as *const u8);
    builder.symbol("hot_jit_get_type_path", hot_jit_get_type_path as *const u8);
    builder.symbol("hot_jit_is_type_check", hot_jit_is_type_check as *const u8);
    builder.symbol("hot_jit_str_ends_with", hot_jit_str_ends_with as *const u8);
    builder.symbol(
        "hot_jit_lookup_var_or_default",
        hot_jit_lookup_var_or_default as *const u8,
    );
    builder.symbol(
        "hot_jit_call_with_spread",
        hot_jit_call_with_spread as *const u8,
    );
    builder.symbol(
        "hot_jit_populate_closure",
        hot_jit_populate_closure as *const u8,
    );
    builder.symbol(
        "hot_jit_str_starts_with",
        hot_jit_str_starts_with as *const u8,
    );
    builder.symbol(
        "hot_jit_dynamic_dot_access",
        hot_jit_dynamic_dot_access as *const u8,
    );
    builder.symbol(
        "hot_jit_dynamic_dot_set",
        hot_jit_dynamic_dot_set as *const u8,
    );
    builder.symbol("hot_jit_dot_set", hot_jit_dot_set as *const u8);
    builder.symbol(
        "hot_jit_make_vec_with_spread",
        hot_jit_make_vec_with_spread as *const u8,
    );
    builder.symbol("hot_jit_store_global", hot_jit_store_global as *const u8);
    builder.symbol("hot_jit_set_namespace", hot_jit_set_namespace as *const u8);
    builder.symbol("hot_jit_call_native", hot_jit_call_native as *const u8);
    builder.symbol(
        "hot_jit_define_function",
        hot_jit_define_function as *const u8,
    );
    builder.symbol("hot_jit_load_scoped", hot_jit_load_scoped as *const u8);
    builder.symbol("hot_jit_capture_var", hot_jit_capture_var as *const u8);
    builder.symbol("hot_jit_length", hot_jit_length as *const u8);
    builder.symbol("hot_jit_is_null", hot_jit_is_null as *const u8);
    let mut module = JITModule::new(builder);

    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(ptr_ty));

    let name = format!(
        "hot_jit_{}_{}",
        function_id,
        function_info.name.replace(':', "_")
    );
    let func_id = module
        .declare_function(&name, Linkage::Local, &sig)
        .map_err(|e| e.to_string())?;

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    ctx.func.name = UserFuncName::user(0, func_id.as_u32());
    let self_func_ref = module.declare_func_in_func(func_id, &mut ctx.func);
    let helper_refs = declare_jit_helpers(&mut module, &mut ctx.func, ptr_ty)?;

    let expected_return_kind = prescan_return_kind(
        &function_info.instructions,
        program,
        function_id,
        signature,
        &function_info.param_names,
    );

    let mut fb_ctx = FunctionBuilderContext::new();
    let result_kind = {
        let mut fbx = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let result_kind = build_supported_body(
            &mut fbx,
            program,
            function_id,
            function_info,
            signature,
            ptr_ty,
            self_func_ref,
            helper_refs,
            expected_return_kind,
        )?;
        fbx.finalize();
        result_kind
    };

    tracing::trace!("[JIT-IR] {} ->\n{}", function_info.name, ctx.func.display());

    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| e.to_string())?;
    module.clear_context(&mut ctx);
    module.finalize_definitions().map_err(|e| e.to_string())?;

    Ok(JitCompiledFunction {
        signature: signature.clone(),
        result_kind,
        module,
        func_id,
    })
}

fn function_has_unsupported_lazy_paths(
    program: &BytecodeProgram,
    function_info: &FunctionInfo,
) -> bool {
    let mut lazy_root_by_reg: AHashMap<u32, u32> = AHashMap::new();
    let mut used_lazy_roots = std::collections::HashSet::new();

    for (ip, instruction) in function_info.instructions.iter().enumerate() {
        match instruction {
            Instruction::LoadConst { dest, constant } => {
                if matches!(
                    program.constants.get(*constant as usize),
                    Some(Constant::Val(Val::Box(b)))
                        if b.as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                            .is_some_and(|lambda| lambda.is_lazy_param)
                ) {
                    lazy_root_by_reg.insert(*dest, *dest);
                }
            }
            Instruction::Move { dest, src } => {
                if let Some(root) = lazy_root_by_reg.get(src).copied() {
                    lazy_root_by_reg.insert(*dest, root);
                }
            }
            Instruction::CallUserFunction {
                function_id,
                args_start,
                args_count,
                ..
            } => {
                let Some(callee) = program.functions.get(*function_id as usize) else {
                    continue;
                };
                if !callee.lazy_params.iter().any(|&lazy| lazy) {
                    continue;
                }
                if !lazy_call_args_match_params(callee, *args_start, *args_count, &lazy_root_by_reg)
                {
                    return true;
                }
                for idx in 0..usize::from(*args_count) {
                    if callee.lazy_params.get(idx).copied().unwrap_or(false) {
                        let reg = *args_start + idx as u32;
                        if let Some(root) = lazy_root_by_reg.get(&reg).copied() {
                            used_lazy_roots.insert(root);
                        }
                    }
                }
            }
            Instruction::Call {
                function: fn_reg,
                args_start,
                args_count,
                ..
            } => {
                let Some(fn_name) =
                    resolve_const_function_name(&function_info.instructions, program, ip, *fn_reg)
                else {
                    continue;
                };
                let callee_id = if fn_name.starts_with("::") {
                    find_function_exact(program, &fn_name, Some(*args_count))
                } else {
                    find_function_by_suffix(program, &fn_name, Some(*args_count))
                };
                let Some(callee_id) = callee_id else {
                    continue;
                };
                let Some(callee) = program.functions.get(callee_id) else {
                    continue;
                };
                if !callee.lazy_params.iter().any(|&lazy| lazy) {
                    continue;
                }
                if !lazy_call_args_match_params(callee, *args_start, *args_count, &lazy_root_by_reg)
                {
                    return true;
                }
                for idx in 0..usize::from(*args_count) {
                    if callee.lazy_params.get(idx).copied().unwrap_or(false) {
                        let reg = *args_start + idx as u32;
                        if let Some(root) = lazy_root_by_reg.get(&reg).copied() {
                            used_lazy_roots.insert(root);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    lazy_root_by_reg
        .values()
        .any(|root| !used_lazy_roots.contains(root))
}

fn lazy_call_args_match_params(
    callee: &FunctionInfo,
    args_start: u32,
    args_count: u8,
    lazy_root_by_reg: &AHashMap<u32, u32>,
) -> bool {
    let count = usize::from(args_count);
    if callee.lazy_params.len() < count {
        return false;
    }
    (0..count).all(|idx| {
        let reg = args_start + idx as u32;
        if callee.lazy_params[idx] {
            lazy_root_by_reg.contains_key(&reg)
        } else {
            !lazy_root_by_reg.contains_key(&reg)
        }
    })
}

fn function_has_jit_bailout_hotlib_dependency(
    program: &BytecodeProgram,
    function_id: FunctionId,
    function_info: &FunctionInfo,
) -> bool {
    let mut visited = std::collections::HashSet::new();
    function_has_jit_bailout_hotlib_dependency_inner(
        program,
        function_id as usize,
        function_info,
        &mut visited,
    )
}

fn function_has_jit_bailout_hotlib_dependency_inner(
    program: &BytecodeProgram,
    function_id: usize,
    function_info: &FunctionInfo,
    visited: &mut std::collections::HashSet<usize>,
) -> bool {
    if !visited.insert(function_id) {
        return false;
    }

    for (ip, instruction) in function_info.instructions.iter().enumerate() {
        match instruction {
            Instruction::CallLibBuiltin { function, .. }
                if resolve_const_function_name(
                    &function_info.instructions,
                    program,
                    ip,
                    *function,
                )
                .is_some_and(|name| hotlib_jit_policy_bails_out(&name)) =>
            {
                return true;
            }
            Instruction::CallUserFunction {
                function_id: callee_id,
                ..
            } => {
                let callee_id = *callee_id as usize;
                if let Some(callee) = program.functions.get(callee_id)
                    && function_has_jit_bailout_hotlib_dependency_inner(
                        program, callee_id, callee, visited,
                    )
                {
                    return true;
                }
            }
            Instruction::Call {
                function: fn_reg,
                args_count,
                ..
            } => {
                let Some(fn_name) =
                    resolve_const_function_name(&function_info.instructions, program, ip, *fn_reg)
                else {
                    continue;
                };
                if hotlib_jit_policy_bails_out(&fn_name) {
                    return true;
                }
                let callee_id = if fn_name.starts_with("::") {
                    find_function_exact(program, &fn_name, Some(*args_count))
                } else {
                    find_function_by_suffix(program, &fn_name, Some(*args_count))
                };
                if let Some(callee_id) = callee_id
                    && let Some(callee) = program.functions.get(callee_id)
                    && function_has_jit_bailout_hotlib_dependency_inner(
                        program, callee_id, callee, visited,
                    )
                {
                    return true;
                }
            }
            _ => {}
        }
    }

    false
}

fn hotlib_jit_policy_bails_out(name: &str) -> bool {
    matches!(
        crate::lang::hot::get_hotlib_map()
            .get(name)
            .map(crate::lang::hot::HotLibFn::jit_policy),
        Some(crate::lang::hot::libmap::HotLibJitPolicy::JitBailout)
    )
}

const NUMERIC_KIND_INT: i64 = 1;
const NUMERIC_KIND_DEC: i64 = 2;
const NUMERIC_KIND_OWNED: i64 = 3;
const TYPE_TOKEN_NULL: i64 = 10;
const TYPE_TOKEN_INT: i64 = 11;
const TYPE_TOKEN_BOOL: i64 = 12;
const TYPE_TOKEN_DEC: i64 = 13;
const HELPER_VAL_KIND_INT: i64 = 1;
const HELPER_VAL_KIND_BOOL: i64 = 2;
const HELPER_VAL_KIND_DEC: i64 = 3;
const HELPER_VAL_KIND_NULL: i64 = 4;
const HELPER_VAL_KIND_OWNED: i64 = 5;

unsafe extern "C" fn hot_jit_copy_dec(src: *const D256, dst: *mut D256) {
    unsafe { *dst = *src };
}

unsafe extern "C" fn hot_jit_add_numeric(
    out: *mut D256,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let result = crate::lang::hot::math::add(&[
        unsafe { decode_numeric_val(left_kind, left_raw) },
        unsafe { decode_numeric_val(right_kind, right_raw) },
    ]);
    write_numeric_result(out, result)
}

unsafe extern "C" fn hot_jit_sub_numeric(
    out: *mut D256,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let result = crate::lang::hot::math::sub(&[
        unsafe { decode_numeric_val(left_kind, left_raw) },
        unsafe { decode_numeric_val(right_kind, right_raw) },
    ]);
    write_numeric_result(out, result)
}

unsafe extern "C" fn hot_jit_mul_numeric(
    out: *mut D256,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let result = crate::lang::hot::math::mul(&[
        unsafe { decode_numeric_val(left_kind, left_raw) },
        unsafe { decode_numeric_val(right_kind, right_raw) },
    ]);
    write_numeric_result(out, result)
}

unsafe extern "C" fn hot_jit_div_numeric(
    out: *mut D256,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let result = crate::lang::hot::math::div(&[
        unsafe { decode_numeric_val(left_kind, left_raw) },
        unsafe { decode_numeric_val(right_kind, right_raw) },
    ]);
    write_numeric_result(out, result)
}

unsafe extern "C" fn hot_jit_mod_numeric(
    out: *mut D256,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let result = crate::lang::hot::math::modulo(&[
        unsafe { decode_numeric_val(left_kind, left_raw) },
        unsafe { decode_numeric_val(right_kind, right_raw) },
    ]);
    write_numeric_result(out, result)
}

unsafe extern "C" fn hot_jit_ne_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::ne,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_eq_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::eq,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_gt_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::gt,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_gte_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::gte,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_lt_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::lt,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_lte_numeric(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    eval_numeric_cmp(
        crate::lang::hot::cmp::lte,
        left_kind,
        left_raw,
        right_kind,
        right_raw,
    )
}

unsafe extern "C" fn hot_jit_eq_general(
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let left = unsafe { decode_helper_val(left_kind, left_raw) };
    let right = unsafe { decode_helper_val(right_kind, right_raw) };
    if left == right { 1 } else { 0 }
}

unsafe extern "C" fn hot_jit_truthy_general(kind: i64, raw: i64) -> i64 {
    let val = unsafe { decode_helper_val(kind, raw) };
    if val.is_truthy() { 1 } else { 0 }
}

unsafe extern "C" fn hot_jit_clone_owned_val(raw: i64) -> i64 {
    if raw == 0 {
        return 0;
    }
    let ptr = raw as *const Val;
    unsafe { Arc::increment_strong_count(ptr) };
    raw
}

/// Check whether an OwnedVal (Arc<Val> pointer) is a Result.Err variant.
/// Returns 1 if error, 0 otherwise. Does not modify reference counts.
unsafe extern "C" fn hot_jit_is_err(raw: i64) -> i64 {
    if raw == 0 {
        return 0;
    }
    let val = unsafe { &*(raw as *const Val) };
    if val.is_err() { 1 } else { 0 }
}

unsafe extern "C" fn hot_jit_drop_owned_val(raw: i64) {
    if raw == 0 {
        return;
    }
    let ptr = raw as *const Val;
    unsafe { Arc::decrement_strong_count(ptr) };
}

const BINOP_ADD: i64 = 0;
const BINOP_SUB: i64 = 1;
const BINOP_MUL: i64 = 2;
const BINOP_DIV: i64 = 3;
const BINOP_MOD: i64 = 4;

/// JIT helper: type-preserving binary arithmetic for operands that include OwnedVal.
/// Returns an OwnedVal handle wrapping the exact result type (Int stays Int, etc.).
unsafe extern "C" fn hot_jit_binop_general(
    op: i64,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let left = unsafe { decode_helper_val(left_kind, left_raw) };
    let right = unsafe { decode_helper_val(right_kind, right_raw) };
    let args = [left, right];
    use crate::lang::hot::r#type::HotResult;
    let result = match op {
        BINOP_ADD => crate::lang::hot::math::add(&args),
        BINOP_SUB => crate::lang::hot::math::sub(&args),
        BINOP_MUL => crate::lang::hot::math::mul(&args),
        BINOP_DIV => crate::lang::hot::math::div(&args),
        BINOP_MOD => crate::lang::hot::math::modulo(&args),
        _ => {
            return new_owned_val(Val::err(Val::Str(
                format!("unknown binop op {}", op).into(),
            )));
        }
    };
    match result {
        HotResult::Ok(val) => new_owned_val(val),
        HotResult::Err(err) => new_owned_val(Val::err(err)),
    }
}

/// JIT helper: type-preserving comparison for operands that include OwnedVal.
/// Returns 1 (true) or 0 (false).
unsafe extern "C" fn hot_jit_cmp_general(
    op: i64,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let left = unsafe { decode_helper_val(left_kind, left_raw) };
    let right = unsafe { decode_helper_val(right_kind, right_raw) };
    let args = [left, right];
    use crate::lang::hot::r#type::HotResult;
    let result = match op {
        0 => crate::lang::hot::cmp::lt(&args),
        1 => crate::lang::hot::cmp::gt(&args),
        2 => crate::lang::hot::cmp::lte(&args),
        3 => crate::lang::hot::cmp::gte(&args),
        4 => crate::lang::hot::cmp::eq(&args),
        5 => crate::lang::hot::cmp::ne(&args),
        _ => return 0,
    };
    match result {
        HotResult::Ok(Val::Bool(b)) => {
            if b {
                1
            } else {
                0
            }
        }
        HotResult::Ok(v) => {
            if v.is_truthy() {
                1
            } else {
                0
            }
        }
        HotResult::Err(_) => 0,
    }
}

/// JIT helper: call back into the VM to execute a user function.
/// `function_id` - the bytecode function ID to call
/// `args_ptr` - pointer to array of (kind: i64, raw: i64) pairs
/// `args_count` - number of arguments
/// Returns an OwnedVal handle to the result.
unsafe extern "C" fn hot_jit_call_vm(
    function_id: i64,
    args_ptr: *const i64,
    args_count: i64,
) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("JIT call_vm: no VM available".into())));
    }
    let vm = unsafe { &mut *vm_ptr };
    let count = args_count as usize;

    let mut args = Vec::with_capacity(count);
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        args.push(unsafe { decode_helper_val(kind, raw) });
    }

    let error_capture = vm.error_capture_active;
    match vm.execute_compiled_user_function(function_id as u32, &args) {
        Ok(result) => new_owned_val(result),
        Err(err) => vm_error_to_owned_val(err, error_capture),
    }
}

/// JIT helper: promote a typed value to an OwnedVal.
/// Takes (kind, raw) and returns an OwnedVal handle.
unsafe extern "C" fn hot_jit_promote_to_owned(kind: i64, raw: i64) -> i64 {
    let val = unsafe { decode_helper_val(kind, raw) };
    new_owned_val(val)
}

/// JIT helper: construct a Vec from (kind, raw) pairs.
/// `args_ptr` - pointer to array of (kind: i64, raw: i64) pairs
/// `args_count` - number of elements
/// Returns an OwnedVal handle to a Val::Vec.
unsafe extern "C" fn hot_jit_make_vec(args_ptr: *const i64, args_count: i64) -> i64 {
    let count = args_count as usize;
    let mut elements = Vec::with_capacity(count);
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        elements.push(unsafe { decode_helper_val(kind, raw) });
    }
    new_owned_val(Val::Vec(elements))
}

/// JIT helper: call a hotlib (Rust-native) function by name with a Vec of args.
/// `fn_name_ptr` - OwnedVal pointer to a Val::Str (the function name)
/// `args_ptr` - OwnedVal pointer to a Val::Vec (the arguments)
/// Returns an OwnedVal handle to the result.
unsafe extern "C" fn hot_jit_call_lib(fn_name_ptr: i64, args_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("JIT call_lib: no VM available".into())));
    }
    let vm = unsafe { &mut *vm_ptr };

    let fn_val = unsafe { &*(fn_name_ptr as *const Val) };
    let args_val = unsafe { &*(args_ptr as *const Val) };

    tracing::trace!("[JIT] call_lib: fn={:?}, args={:?}", fn_val, args_val);

    // Match the interpreter's CallLibBuiltin routing: when the function ref or
    // args contain a LambdaInfo (lazy thunk), route through execute_call_lib_builtin
    // which handles lazy evaluation, thunk preservation, and the call-lib(fn, args) pattern.
    let fn_has_thunk = matches!(fn_val, Val::Box(b) if b.as_any().downcast_ref::<crate::lang::bytecode::LambdaInfo>().is_some());
    let args_has_thunk = match args_val {
        Val::Box(b) => b
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            .is_some(),
        Val::Vec(vec) => vec.iter().any(|v| {
            matches!(v, Val::Box(b) if b.as_any().downcast_ref::<crate::lang::bytecode::LambdaInfo>().is_some())
        }),
        _ => false,
    };

    let error_capture = vm.error_capture_active;

    if fn_has_thunk || args_has_thunk {
        match vm.execute_call_lib_builtin(&[fn_val.clone(), args_val.clone()]) {
            Ok(result) => {
                tracing::trace!("[JIT] call_lib_builtin result: {:?}", result);
                new_owned_val(result)
            }
            Err(err) => {
                tracing::trace!("[JIT] call_lib_builtin error: {}", err);
                vm_error_to_owned_val(err, error_capture)
            }
        }
    } else {
        let function_name = vm.value_to_string(fn_val);
        let args_slice: &[Val] = match args_val {
            Val::Vec(vec) => vec.as_slice(),
            other => std::slice::from_ref(other),
        };

        match vm.execute_call_lib(&function_name, args_slice) {
            Ok(result) => {
                tracing::trace!("[JIT] call_lib result: {:?}", result);
                new_owned_val(result)
            }
            Err(err) => {
                tracing::trace!("[JIT] call_lib error: {}", err);
                vm_error_to_owned_val(err, error_capture)
            }
        }
    }
}

/// JIT helper: access a property on a value (dot access).
/// `obj_ptr` - OwnedVal pointer to the object
/// `prop_ptr` - pointer to a constant Val::Str with the property name
/// Returns an OwnedVal handle to the property value.
unsafe extern "C" fn hot_jit_dot_access(obj_ptr: i64, prop_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("JIT dot_access: no VM available".into())));
    }
    let vm = unsafe { &*vm_ptr };
    let obj_val = unsafe { &*(obj_ptr as *const Val) };
    let prop_val = unsafe { &*(prop_ptr as *const Val) };
    let prop_name = match prop_val {
        Val::Str(s) => s.as_ref(),
        _ => {
            return new_owned_val(Val::err(Val::Str(
                "JIT dot_access: expected Str for property name".into(),
            )));
        }
    };
    match vm.access_property(obj_val, prop_name) {
        Ok(result) => new_owned_val(result),
        Err(err) => new_owned_val(Val::err(Val::Str(err.to_string().into()))),
    }
}

/// JIT helper: merge source map into target map.
/// Both args are OwnedVal pointers to Val::Map values.
/// Returns an OwnedVal handle to the merged map.
unsafe extern "C" fn hot_jit_merge_maps(target_ptr: i64, source_ptr: i64) -> i64 {
    let target = unsafe { &*(target_ptr as *const Val) };
    let source = unsafe { &*(source_ptr as *const Val) };
    let result = match (target, source) {
        (Val::Map(t), Val::Map(s)) => {
            let mut merged = (**t).clone();
            for (k, v) in s.iter() {
                merged.insert(k.clone(), v.clone());
            }
            Val::Map(Box::new(merged))
        }
        _ => target.clone(),
    };
    new_owned_val(result)
}

/// JIT helper: template string interpolation.
/// `parts_ptr` - pointer to array of (kind: i64, raw: i64) pairs
/// `parts_count` - number of parts
/// Returns an OwnedVal handle to the concatenated Val::Str.
unsafe extern "C" fn hot_jit_template_interpolate(parts_ptr: *const i64, parts_count: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    let count = parts_count as usize;
    let mut result = String::new();
    for i in 0..count {
        let kind = unsafe { *parts_ptr.add(i * 2) };
        let raw = unsafe { *parts_ptr.add(i * 2 + 1) };
        let val = unsafe { decode_helper_val(kind, raw) };
        if !vm_ptr.is_null() {
            let vm = unsafe { &*vm_ptr };
            result.push_str(&vm.value_to_string(&val));
        } else if !matches!(val, Val::Null) {
            result.push_str(&val.to_string());
        }
    }
    new_owned_val(Val::Str(result.into()))
}

/// JIT helper: extract $val from a typed map (for match arms on typed values).
/// If the value is a Map with $type and $val, returns the $val.
/// Otherwise returns the value unchanged.
unsafe extern "C" fn hot_jit_extract_inner_val(src_ptr: i64) -> i64 {
    let src_val = unsafe { &*(src_ptr as *const Val) };
    let extracted = match src_val {
        Val::Map(map) => {
            if map.contains_key(&Val::from("$type")) {
                if let Some(inner_val) = map.get(&Val::from("$val")) {
                    inner_val.clone()
                } else {
                    src_val.clone()
                }
            } else {
                src_val.clone()
            }
        }
        _ => src_val.clone(),
    };
    new_owned_val(extracted)
}

/// JIT helper: wrap a value in Result.Ok if it isn't already a Result.
/// If the value is a lazy thunk, evaluates it first with result-checking suppressed.
unsafe extern "C" fn hot_jit_ensure_result(val_ptr: i64) -> i64 {
    let val = unsafe { &*(val_ptr as *const Val) };

    // Evaluate lazy thunks with result-checking suppressed
    let val = if let Val::Box(b) = val {
        if b.as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            .is_some()
        {
            let vm_ptr = get_jit_vm_ptr();
            if !vm_ptr.is_null() {
                let vm = unsafe { &mut *vm_ptr };
                let prev = vm.get_suppress_result_checking();
                vm.set_suppress_result_checking(true);
                let result = vm.execute_lambda(val, &[]);
                vm.set_suppress_result_checking(prev);
                match result {
                    Ok(v) => v,
                    Err(_) => val.clone(),
                }
            } else {
                val.clone()
            }
        } else {
            val.clone()
        }
    } else {
        val.clone()
    };

    // Check if already a Result type
    if let Val::Map(ref map) = val
        && let Some(Val::Str(s)) = map.get(&Val::from("$type"))
    {
        let s: &str = s;
        if s == "::hot::type/Result.Ok" || s == "::hot::type/Result.Err" {
            return new_owned_val(val);
        }
    }

    // Wrap in Result.Ok
    let mut map = indexmap::IndexMap::new();
    map.insert(Val::from("$type"), Val::from("::hot::type/Result.Ok"));
    map.insert(Val::from("$val"), val);
    new_owned_val(Val::Map(Box::new(map)))
}

/// JIT helper: set an element in a collection (map or vec).
/// `collection_ptr` - OwnedVal pointer to a Val::Map or Val::Vec
/// `index_ptr` - OwnedVal pointer to the key/index
/// `value_ptr` - OwnedVal pointer to the value to set
/// Returns an OwnedVal handle to the updated collection.
unsafe extern "C" fn hot_jit_set_element(
    collection_ptr: i64,
    index_ptr: i64,
    value_ptr: i64,
) -> i64 {
    let collection_val = unsafe { &*(collection_ptr as *const Val) };
    let index_val = unsafe { &*(index_ptr as *const Val) };
    let value_val = unsafe { &*(value_ptr as *const Val) };
    match collection_val {
        Val::Map(map) => {
            let mut new_map = (**map).clone();
            new_map.insert(index_val.clone(), value_val.clone());
            new_owned_val(Val::Map(Box::new(new_map)))
        }
        Val::Vec(vec) => {
            if let Val::Int(idx) = index_val {
                let idx = *idx;
                if idx >= 0 && (idx as usize) < vec.len() {
                    let mut new_vec = vec.clone();
                    new_vec[idx as usize] = value_val.clone();
                    new_owned_val(Val::Vec(new_vec))
                } else {
                    new_owned_val(Val::err(Val::Str(
                        format!("Vector index {} out of bounds (length: {})", idx, vec.len())
                            .into(),
                    )))
                }
            } else {
                new_owned_val(Val::err(Val::Str(
                    format!("Vector index must be an integer, got: {:?}", index_val).into(),
                )))
            }
        }
        _ => new_owned_val(Val::err(Val::Str(
            format!(
                "SetElement can only be used on maps or vectors, got: {:?}",
                collection_val
            )
            .into(),
        ))),
    }
}

/// JIT helper: construct a typed value.
/// `src_ptr` - OwnedVal pointer to the source data
/// `type_info_ptr` - OwnedVal pointer to the type info constant (a Map with $type, $fields, etc.)
/// Returns an OwnedVal handle to the typed value.
unsafe extern "C" fn hot_jit_construct_typed(src_ptr: i64, type_info_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str(
            "JIT construct_typed: no VM available".into(),
        )));
    }
    let vm = unsafe { &mut *vm_ptr };
    let src_val = unsafe { &*(src_ptr as *const Val) };
    let type_info_val = unsafe { &*(type_info_ptr as *const Val) };
    match vm.construct_typed_recursive(src_val, type_info_val) {
        Ok(result) => new_owned_val(result),
        Err(err) => new_owned_val(Val::err(Val::Str(err.to_string().into()))),
    }
}

/// JIT helper: append an element to a Vec, returning a new OwnedVal Vec.
/// `vec_ptr` - OwnedVal pointer to a Val::Vec
/// `elem_ptr` - OwnedVal pointer to the value to append
/// Returns an OwnedVal handle to the new Vec.
unsafe extern "C" fn hot_jit_vec_push(vec_ptr: i64, elem_ptr: i64) -> i64 {
    let vec_val = unsafe { &*(vec_ptr as *const Val) };
    let elem_val = unsafe { &*(elem_ptr as *const Val) };
    match vec_val {
        Val::Vec(vec) => {
            let mut new_vec = vec.clone();
            new_vec.push(elem_val.clone());
            new_owned_val(Val::Vec(new_vec))
        }
        _ => {
            let new_vec = vec![elem_val.clone()];
            new_owned_val(Val::Vec(new_vec))
        }
    }
}

/// JIT helper: string ends_with check for OwnedVal strings.
/// Both args are OwnedVal pointers to Val::Str values.
/// Returns 1 (true) or 0 (false) as i64.
unsafe extern "C" fn hot_jit_str_ends_with(str_ptr: i64, suffix_ptr: i64) -> i64 {
    let str_val = unsafe { &*(str_ptr as *const Val) };
    let suffix_val = unsafe { &*(suffix_ptr as *const Val) };
    match (str_val, suffix_val) {
        (Val::Str(s), Val::Str(suffix)) if s.ends_with(&**suffix) => 1,
        _ => 0,
    }
}

/// JIT helper: runtime type check for OwnedVal values.
/// `val_ptr` - OwnedVal pointer to the value to check
/// `type_path_ptr` - OwnedVal or string const pointer to the expected type path
/// Returns 1 (true) or 0 (false) as i64.
unsafe extern "C" fn hot_jit_is_type_check(val_ptr: i64, type_path_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return 0;
    }
    let vm = unsafe { &*vm_ptr };
    let val = unsafe { &*(val_ptr as *const Val) };
    let type_path_val = unsafe { &*(type_path_ptr as *const Val) };
    let expected_type = match type_path_val {
        Val::Str(s) => s.as_ref().to_string(),
        _ => return 0,
    };
    let actual_type = vm.get_value_type_path(val);
    if actual_type == expected_type || actual_type.starts_with(&format!("{}.", expected_type)) {
        1
    } else {
        0
    }
}

/// JIT helper: get the type path string for an OwnedVal.
/// Returns an OwnedVal handle to a Val::Str containing the type path.
unsafe extern "C" fn hot_jit_get_type_path(val_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::Str("".into()));
    }
    let vm = unsafe { &*vm_ptr };
    let val = unsafe { &*(val_ptr as *const Val) };
    let type_path = vm.get_value_type_path(val);
    new_owned_val(Val::Str(type_path.into()))
}

/// JIT helper: look up a variable by name through the VM.
/// Returns an OwnedVal handle to the resolved value, or Val::Null if not found.
unsafe extern "C" fn hot_jit_lookup_var(name_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::Null);
    }
    let vm = unsafe { &mut *vm_ptr };
    let name_val = unsafe { &*(name_ptr as *const Val) };
    let name = match name_val {
        Val::Str(s) => s.as_ref(),
        _ => return new_owned_val(Val::Null),
    };
    match vm.lookup_variable(name) {
        Ok(val) => new_owned_val(val),
        Err(_) => new_owned_val(Val::Null),
    }
}

/// JIT helper: look up a variable by name through the VM (for LoadVarOrDefault).
/// Returns an OwnedVal handle to the resolved value, or 0 if not found.
unsafe extern "C" fn hot_jit_lookup_var_or_default(name_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return 0;
    }
    let vm = unsafe { &mut *vm_ptr };
    let name_val = unsafe { &*(name_ptr as *const Val) };
    let name = match name_val {
        Val::Str(s) => s.as_ref(),
        _ => return 0,
    };
    match vm.lookup_variable(name) {
        Ok(val) => new_owned_val(val),
        Err(_) => 0,
    }
}

/// JIT helper: clone a lambda and populate its closure_env with captured variable values.
/// `lambda_ptr` is an OwnedVal pointer to a Val::Box(LambdaInfo).
/// `names_ptr` points to an array of i64 OwnedVal pointers (each a Val::Str).
/// `values_ptr` points to an array of i64 OwnedVal pointers (the captured values).
/// `count` is the number of captured variables.
/// Returns an OwnedVal pointer to the new lambda with populated closure_env.
unsafe extern "C" fn hot_jit_populate_closure(
    lambda_ptr: i64,
    names_ptr: *const i64,
    values_ptr: *const i64,
    count: i64,
) -> i64 {
    let lambda_val = unsafe { &*(lambda_ptr as *const Val) };
    if let Val::Box(b) = lambda_val
        && let Some(lambda_info) = b
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
    {
        let mut captured = lambda_info.clone();
        let n = count as usize;
        for i in 0..n {
            let name_raw = unsafe { *names_ptr.add(i) };
            let value_raw = unsafe { *values_ptr.add(i) };
            let name_val = unsafe { &*(name_raw as *const Val) };
            let value_val = unsafe { &*(value_raw as *const Val) };
            if let Val::Str(name) = name_val {
                captured
                    .closure_env
                    .insert(name.to_string(), value_val.clone());
            }
        }
        return new_owned_val(Val::Box(Box::new(captured)));
    }
    lambda_ptr
}

/// JIT helper: string starts-with check (mirrors hot_jit_str_ends_with).
unsafe extern "C" fn hot_jit_str_starts_with(str_ptr: i64, prefix_ptr: i64) -> i64 {
    let str_val = unsafe { &*(str_ptr as *const Val) };
    let prefix_val = unsafe { &*(prefix_ptr as *const Val) };
    match (str_val, prefix_val) {
        (Val::Str(s), Val::Str(prefix)) if s.starts_with(&**prefix) => 1,
        _ => 0,
    }
}

/// JIT helper: dynamic dot access (property access with runtime key).
/// `obj_ptr` - OwnedVal pointer to the object
/// `key_ptr` - OwnedVal pointer to the key (Str for maps, Int for vecs)
/// Returns OwnedVal handle to the result.
unsafe extern "C" fn hot_jit_dynamic_dot_access(obj_ptr: i64, key_ptr: i64) -> i64 {
    let obj_val = unsafe { &*(obj_ptr as *const Val) };
    let key_val = unsafe { &*(key_ptr as *const Val) };
    let result = match obj_val {
        Val::Map(m) => m.get(key_val).cloned().unwrap_or(Val::Null),
        Val::Vec(v) => match key_val {
            Val::Int(i) => {
                if *i < 0 {
                    Val::Null
                } else {
                    v.get(*i as usize).cloned().unwrap_or(Val::Null)
                }
            }
            _ => Val::Null,
        },
        Val::Str(s) => match key_val {
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
            _ => Val::Null,
        },
        _ => Val::Null,
    };
    new_owned_val(result)
}

/// JIT helper: dynamic dot set (property assignment with runtime key).
/// `obj_ptr` - OwnedVal pointer to the object (Map or Vec)
/// `key_ptr` - OwnedVal pointer to the key
/// `val_ptr` - OwnedVal pointer to the value
/// Returns OwnedVal handle to the modified object.
unsafe extern "C" fn hot_jit_dynamic_dot_set(obj_ptr: i64, key_ptr: i64, val_ptr: i64) -> i64 {
    let obj_val = unsafe { &*(obj_ptr as *const Val) };
    let key_val = unsafe { &*(key_ptr as *const Val) };
    let new_value = unsafe { &*(val_ptr as *const Val) };
    let result = match obj_val {
        Val::Map(m) => {
            let mut new_map = (**m).clone();
            new_map.insert(key_val.clone(), new_value.clone());
            Val::Map(Box::new(new_map))
        }
        Val::Vec(v) => {
            let mut new_vec = v.clone();
            if let Val::Int(i) = key_val
                && *i >= 0
            {
                let idx = *i as usize;
                while new_vec.len() <= idx {
                    new_vec.push(Val::Null);
                }
                new_vec[idx] = new_value.clone();
            }
            Val::Vec(new_vec)
        }
        _ => obj_val.clone(),
    };
    new_owned_val(result)
}

/// JIT helper: static dot set (property assignment with constant key).
/// `obj_ptr` - OwnedVal pointer to the object
/// `key_ptr` - OwnedVal pointer to the key (Val::Str)
/// `val_ptr` - OwnedVal pointer to the value
/// Returns OwnedVal handle to the modified object.
unsafe extern "C" fn hot_jit_dot_set(obj_ptr: i64, key_ptr: i64, val_ptr: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return obj_ptr;
    }
    let vm = unsafe { &mut *vm_ptr };
    let mut obj_val = unsafe { &*(obj_ptr as *const Val) }.clone();
    let key_val = unsafe { &*(key_ptr as *const Val) };
    let new_value = unsafe { &*(val_ptr as *const Val) }.clone();
    let prop_name = match key_val {
        Val::Str(s) => s.as_ref().to_string(),
        _ => return new_owned_val(obj_val),
    };
    match vm.set_property(&mut obj_val, &prop_name, new_value) {
        Ok(()) => new_owned_val(obj_val),
        Err(_) => new_owned_val(obj_val),
    }
}

/// JIT helper: make vec with spread elements.
/// `args_ptr` - pointer to array of (kind: i64, raw: i64) pairs
/// `args_count` - number of elements
/// `spread_mask` - bitmask of which elements should be spread
/// Returns OwnedVal handle to the resulting Vec.
unsafe extern "C" fn hot_jit_make_vec_with_spread(
    args_ptr: *const i64,
    args_count: i64,
    spread_mask: i64,
) -> i64 {
    let count = args_count as usize;
    let mask = spread_mask as u64;
    let mut elements = Vec::new();
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        let val = unsafe { decode_helper_val(kind, raw) };
        if (mask >> i) & 1 == 1 {
            match val {
                Val::Vec(inner) => elements.extend(inner),
                Val::Null => {}
                other => elements.push(other),
            }
        } else {
            elements.push(val);
        }
    }
    new_owned_val(Val::Vec(elements))
}

/// JIT helper: store a value in a global namespace variable.
/// `ns_ptr` - OwnedVal pointer to namespace name (Val::Str)
/// `name_ptr` - OwnedVal pointer to variable name (Val::Str)
/// `val_ptr` - OwnedVal pointer to the value
unsafe extern "C" fn hot_jit_store_global(ns_ptr: i64, name_ptr: i64, val_ptr: i64) {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return;
    }
    let vm = unsafe { &mut *vm_ptr };
    let ns_val = unsafe { &*(ns_ptr as *const Val) };
    let name_val = unsafe { &*(name_ptr as *const Val) };
    let value = unsafe { &*(val_ptr as *const Val) }.clone();
    if let (Val::Str(ns), Val::Str(name)) = (ns_val, name_val) {
        vm.namespace_variables
            .entry(ns.to_string())
            .or_default()
            .insert(name.to_string(), value);
    }
}

/// JIT helper: set the VM's current namespace.
/// `ns_ptr` - OwnedVal pointer to namespace name (Val::Str)
unsafe extern "C" fn hot_jit_set_namespace(ns_ptr: i64) {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return;
    }
    let vm = unsafe { &mut *vm_ptr };
    let ns_val = unsafe { &*(ns_ptr as *const Val) };
    if let Val::Str(ns) = ns_val {
        vm.current_namespace = ns.to_string();
        let _ = vm.ensure_namespace_has_ns_variable(ns.as_ref());
    }
}

/// JIT helper: call a native/hotlib function by function_id.
/// Resolves the function name from the program and calls it through the VM.
/// Returns OwnedVal handle to the result.
unsafe extern "C" fn hot_jit_call_native(
    function_id: i64,
    args_ptr: *const i64,
    args_count: i64,
) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("no VM available".into())));
    }
    let vm = unsafe { &mut *vm_ptr };
    let count = args_count as usize;
    let mut args = Vec::with_capacity(count);
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        let val = unsafe { decode_helper_val(kind, raw) };
        let unwrapped = match vm.unwrap_result_if_ok(&val) {
            Ok(v) => v,
            Err(e) => {
                let msg: Arc<str> = e.to_string().into();
                return new_owned_val(Val::err(Val::Str(msg)));
            }
        };
        args.push(unwrapped);
    }
    let func_info = match vm.program.functions.get(function_id as usize) {
        Some(info) => info,
        None => {
            let msg: Arc<str> = format!("Invalid function ID: {}", function_id).into();
            return new_owned_val(Val::err(Val::Str(msg)));
        }
    };
    let function_name = format!("{}/{}", func_info.namespace, func_info.name);
    match vm.call_hotlib_function(&function_name, &args) {
        Ok(v) => new_owned_val(v),
        Err(e) => {
            if vm.error_capture_active {
                let mut m = indexmap::IndexMap::new();
                m.insert(Val::from("error"), Val::from(e.to_string()));
                new_owned_val(Val::Map(Box::new(m)))
            } else {
                let msg: Arc<str> = e.to_string().into();
                new_owned_val(Val::err(Val::Str(msg)))
            }
        }
    }
}

/// JIT helper: create a function value from a function_id.
/// Returns OwnedVal handle to the function value.
unsafe extern "C" fn hot_jit_define_function(function_id: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::Null);
    }
    let vm = unsafe { &mut *vm_ptr };
    let func_val = vm.create_user_function_value(&function_id.to_string());
    new_owned_val(func_val)
}

/// JIT helper: load a scoped variable by name at a specific scope depth.
/// Returns OwnedVal handle to the value, or error.
unsafe extern "C" fn hot_jit_load_scoped(name_ptr: i64, scope_depth: i64) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::Null);
    }
    let vm = unsafe { &*vm_ptr };
    let name_val = unsafe { &*(name_ptr as *const Val) };
    let name = match name_val {
        Val::Str(s) => s.as_ref(),
        _ => return new_owned_val(Val::Null),
    };
    match vm.lookup_variable_at_depth(name, scope_depth as usize) {
        Some(val) => new_owned_val(val),
        None => new_owned_val(Val::Null),
    }
}

/// JIT helper: capture a variable from a specific scope depth (same as load_scoped).
unsafe extern "C" fn hot_jit_capture_var(name_ptr: i64, scope_depth: i64) -> i64 {
    unsafe { hot_jit_load_scoped(name_ptr, scope_depth) }
}

/// JIT helper: compute length of a Vec, Str, Map, or Bytes value.
/// Takes an OwnedVal pointer and returns the length as an i64 Int.
unsafe extern "C" fn hot_jit_length(val_ptr: i64) -> i64 {
    let val = unsafe { &*(val_ptr as *const Val) };
    match val {
        Val::Vec(v) => v.len() as i64,
        Val::Str(s) => s.len() as i64,
        Val::Map(m) => m.len() as i64,
        Val::Bytes(b) => b.len() as i64,
        _ => 0,
    }
}

/// JIT helper: check if an OwnedVal is Val::Null. Returns 1 if null, 0 otherwise.
unsafe extern "C" fn hot_jit_is_null(val_ptr: i64) -> i64 {
    if val_ptr == 0 {
        return 1;
    }
    let val = unsafe { &*(val_ptr as *const Val) };
    if matches!(val, Val::Null) { 1 } else { 0 }
}

/// Call a function by name through the VM. Used when the function isn't in
/// program.functions (e.g., built-in type constructors like Vec, Str, etc.).
/// `name_ptr` is an OwnedVal pointer to a Val::Str containing the function name.
unsafe extern "C" fn hot_jit_call_vm_by_name(
    name_ptr: i64,
    args_ptr: *const i64,
    args_count: i64,
) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("no VM available".into())));
    }
    let vm = unsafe { &mut *vm_ptr };
    let count = args_count as usize;

    let name_val_ptr = name_ptr as *const Val;
    let name_val = unsafe { &*name_val_ptr };

    let mut args = Vec::with_capacity(count);
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        args.push(unsafe { decode_helper_val(kind, raw) });
    }

    let error_capture = vm.error_capture_active;

    match name_val {
        Val::Str(s) => match vm.execute_function_call_by_name(s.as_ref(), &args) {
            Ok(result) => new_owned_val(result),
            Err(err) => vm_error_to_owned_val(err, error_capture),
        },
        Val::Box(b) => {
            if let Some(lambda) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                let lambda_val = Val::Box(Box::new(lambda.clone()));
                match vm.execute_lambda(&lambda_val, &args) {
                    Ok(result) => new_owned_val(result),
                    Err(err) => vm_error_to_owned_val(err, error_capture),
                }
            } else {
                new_owned_val(Val::err(Val::Str(
                    format!("Cannot call non-function value: {:?}", name_val).into(),
                )))
            }
        }
        _ => {
            let name_str = vm.value_to_string(name_val);
            match vm.execute_function_call_by_name(&name_str, &args) {
                Ok(result) => new_owned_val(result),
                Err(err) => vm_error_to_owned_val(err, error_capture),
            }
        }
    }
}

/// Trampoline for `CallWithSpread`: like `hot_jit_call_vm_by_name` but applies
/// a spread_mask to expand Vec arguments before calling.
unsafe extern "C" fn hot_jit_call_with_spread(
    name_ptr: i64,
    args_ptr: *const i64,
    args_count: i64,
    spread_mask: i64,
) -> i64 {
    let vm_ptr = get_jit_vm_ptr();
    if vm_ptr.is_null() {
        return new_owned_val(Val::err(Val::Str("no VM available".into())));
    }
    let vm = unsafe { &mut *vm_ptr };
    let count = args_count as usize;
    let mask = spread_mask as u64;

    let mut expanded_args = Vec::new();
    for i in 0..count {
        let kind = unsafe { *args_ptr.add(i * 2) };
        let raw = unsafe { *args_ptr.add(i * 2 + 1) };
        let val = unsafe { decode_helper_val(kind, raw) };
        if (mask >> i) & 1 == 1 {
            if let Val::Vec(elements) = &val {
                for elem in elements {
                    expanded_args.push(elem.clone());
                }
            } else {
                expanded_args.push(val);
            }
        } else {
            expanded_args.push(val);
        }
    }

    let error_capture = vm.error_capture_active;
    let name_val_ptr = name_ptr as *const Val;
    let name_val = unsafe { &*name_val_ptr };

    match name_val {
        Val::Str(s) => match vm.execute_function_call_by_name(s.as_ref(), &expanded_args) {
            Ok(result) => new_owned_val(result),
            Err(err) => vm_error_to_owned_val(err, error_capture),
        },
        _ => {
            let name_str = vm.value_to_string(name_val);
            match vm.execute_function_call_by_name(&name_str, &expanded_args) {
                Ok(result) => new_owned_val(result),
                Err(err) => vm_error_to_owned_val(err, error_capture),
            }
        }
    }
}

fn declare_jit_helpers(
    module: &mut JITModule,
    func: &mut cranelift_codegen::ir::Function,
    ptr_ty: Type,
) -> Result<JitHelperRefs, String> {
    let get_stack_limit = declare_helper_func(
        module,
        func,
        ptr_ty,
        "hot_jit_get_stack_limit",
        &[],
        &[types::I64],
    )?;
    let copy_dec = declare_helper_func(
        module,
        func,
        ptr_ty,
        "hot_jit_copy_dec",
        &[ptr_ty, ptr_ty],
        &[],
    )?;
    let numeric_out = &[ptr_ty, types::I64, types::I64, types::I64, types::I64];
    let numeric_cmp = &[types::I64, types::I64, types::I64, types::I64];
    let owned_arg = &[types::I64];
    Ok(JitHelperRefs {
        get_stack_limit,
        copy_dec,
        add_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_add_numeric",
            numeric_out,
            &[types::I64],
        )?,
        sub_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_sub_numeric",
            numeric_out,
            &[types::I64],
        )?,
        mul_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_mul_numeric",
            numeric_out,
            &[types::I64],
        )?,
        div_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_div_numeric",
            numeric_out,
            &[types::I64],
        )?,
        mod_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_mod_numeric",
            numeric_out,
            &[types::I64],
        )?,
        ne_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_ne_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        eq_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_eq_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        eq_general: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_eq_general",
            numeric_cmp,
            &[types::I64],
        )?,
        gt_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_gt_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        gte_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_gte_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        lt_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_lt_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        lte_numeric: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_lte_numeric",
            numeric_cmp,
            &[types::I64],
        )?,
        truthy_general: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_truthy_general",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        clone_owned_val: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_clone_owned_val",
            owned_arg,
            &[types::I64],
        )?,
        drop_owned_val: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_drop_owned_val",
            owned_arg,
            &[],
        )?,
        call_vm: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_call_vm",
            &[types::I64, ptr_ty, types::I64],
            &[types::I64],
        )?,
        binop_general: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_binop_general",
            &[types::I64, types::I64, types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        cmp_general: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_cmp_general",
            &[types::I64, types::I64, types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        is_err: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_is_err",
            &[types::I64],
            &[types::I64],
        )?,
        call_vm_by_name: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_call_vm_by_name",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        promote_to_owned: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_promote_to_owned",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        make_vec: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_make_vec",
            &[ptr_ty, types::I64],
            &[types::I64],
        )?,
        call_lib: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_call_lib",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        dot_access: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_dot_access",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        merge_maps: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_merge_maps",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        template_interpolate: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_template_interpolate",
            &[ptr_ty, types::I64],
            &[types::I64],
        )?,
        extract_inner_val: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_extract_inner_val",
            &[types::I64],
            &[types::I64],
        )?,
        ensure_result: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_ensure_result",
            &[types::I64],
            &[types::I64],
        )?,
        set_element: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_set_element",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        construct_typed: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_construct_typed",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        lookup_var: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_lookup_var",
            &[types::I64],
            &[types::I64],
        )?,
        vec_push: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_vec_push",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        get_type_path: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_get_type_path",
            &[types::I64],
            &[types::I64],
        )?,
        is_type_check: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_is_type_check",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        str_ends_with: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_str_ends_with",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        lookup_var_or_default: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_lookup_var_or_default",
            &[types::I64],
            &[types::I64],
        )?,
        call_with_spread: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_call_with_spread",
            &[types::I64, types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        populate_closure: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_populate_closure",
            &[types::I64, types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        dynamic_dot_access: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_dynamic_dot_access",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        dynamic_dot_set: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_dynamic_dot_set",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        dot_set: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_dot_set",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        make_vec_with_spread: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_make_vec_with_spread",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        store_global: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_store_global",
            &[types::I64, types::I64, types::I64],
            &[],
        )?,
        set_namespace: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_set_namespace",
            &[types::I64],
            &[],
        )?,
        str_starts_with: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_str_starts_with",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        call_native: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_call_native",
            &[types::I64, types::I64, types::I64],
            &[types::I64],
        )?,
        define_function: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_define_function",
            &[types::I64],
            &[types::I64],
        )?,
        load_scoped: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_load_scoped",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        capture_var: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_capture_var",
            &[types::I64, types::I64],
            &[types::I64],
        )?,
        length: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_length",
            &[types::I64],
            &[types::I64],
        )?,
        is_null: declare_helper_func(
            module,
            func,
            ptr_ty,
            "hot_jit_is_null",
            &[types::I64],
            &[types::I64],
        )?,
    })
}

fn declare_helper_func(
    module: &mut JITModule,
    func: &mut cranelift_codegen::ir::Function,
    _ptr_ty: Type,
    name: &str,
    params: &[Type],
    returns: &[Type],
) -> Result<FuncRef, String> {
    let mut sig = module.make_signature();
    for param in params {
        sig.params.push(AbiParam::new(*param));
    }
    for ret in returns {
        sig.returns.push(AbiParam::new(*ret));
    }
    let func_id = module
        .declare_function(name, Linkage::Import, &sig)
        .map_err(|e| e.to_string())?;
    Ok(module.declare_func_in_func(func_id, func))
}

unsafe fn decode_numeric_val(kind: i64, raw: i64) -> Val {
    match kind {
        NUMERIC_KIND_INT => Val::Int(raw),
        NUMERIC_KIND_DEC => Val::Dec(unsafe { *(raw as *const D256) }),
        NUMERIC_KIND_OWNED => {
            if raw == 0 {
                return Val::Null;
            }
            let ptr = raw as *const Val;
            unsafe { Arc::increment_strong_count(ptr) };
            let arc = unsafe { Arc::from_raw(ptr) };
            (*arc).clone()
        }
        _ => Val::Null,
    }
}

/// Write the result of a numeric operation to the output pointer.
/// Returns 0 on success, or a non-zero OwnedVal error pointer on failure.
fn write_numeric_result(
    result_ptr: *mut D256,
    result: crate::lang::hot::r#type::HotResult<Val>,
) -> i64 {
    match result {
        crate::lang::hot::r#type::HotResult::Ok(Val::Dec(dec)) => unsafe {
            *result_ptr = dec;
            0
        },
        crate::lang::hot::r#type::HotResult::Ok(Val::Int(value)) => unsafe {
            *result_ptr = D256::from(value);
            0
        },
        crate::lang::hot::r#type::HotResult::Ok(other) => new_owned_val(Val::err(Val::Str(
            format!("numeric operation returned unexpected type: {:?}", other).into(),
        ))),
        crate::lang::hot::r#type::HotResult::Err(err) => new_owned_val(Val::err(err)),
    }
}

fn eval_numeric_cmp(
    cmp: fn(&[Val]) -> crate::lang::hot::r#type::HotResult<Val>,
    left_kind: i64,
    left_raw: i64,
    right_kind: i64,
    right_raw: i64,
) -> i64 {
    let args = unsafe {
        [
            decode_numeric_val(left_kind, left_raw),
            decode_numeric_val(right_kind, right_raw),
        ]
    };
    match cmp(&args) {
        crate::lang::hot::r#type::HotResult::Ok(Val::Bool(result)) => {
            if result {
                1
            } else {
                0
            }
        }
        crate::lang::hot::r#type::HotResult::Ok(v) => {
            if v.is_truthy() {
                1
            } else {
                0
            }
        }
        crate::lang::hot::r#type::HotResult::Err(_) => 0,
    }
}

fn encode_special_string_const(s: &str) -> Option<i64> {
    match s {
        "::hot::type/Null" => Some(TYPE_TOKEN_NULL),
        "::hot::type/Int" => Some(TYPE_TOKEN_INT),
        "::hot::type/Bool" => Some(TYPE_TOKEN_BOOL),
        "::hot::type/Dec" => Some(TYPE_TOKEN_DEC),
        _ => None,
    }
}

fn raw_type_token(kind: RawKind) -> Result<i64, String> {
    match kind {
        RawKind::Null => Ok(TYPE_TOKEN_NULL),
        RawKind::Int => Ok(TYPE_TOKEN_INT),
        RawKind::Bool => Ok(TYPE_TOKEN_BOOL),
        RawKind::Dec => Ok(TYPE_TOKEN_DEC),
        RawKind::OwnedVal | RawKind::TypeTag | RawKind::StringConst => {
            Err(format!("JIT type tags are unsupported for {:?}", kind))
        }
    }
}

/// Convert a VM error to an OwnedVal, matching the VM's error_capture_active behavior.
/// When error capture is active, wraps as {error: "msg"} map (like the VM does).
/// Otherwise wraps as Val::err for ReturnIfErr to catch.
fn vm_error_to_owned_val(err: impl std::fmt::Display, error_capture_active: bool) -> i64 {
    if error_capture_active {
        let mut m = indexmap::IndexMap::new();
        m.insert(Val::from("error"), Val::from(err.to_string()));
        new_owned_val(Val::Map(Box::new(m)))
    } else {
        new_owned_val(Val::err(Val::Str(err.to_string().into())))
    }
}

fn new_owned_val(val: Val) -> i64 {
    Arc::into_raw(Arc::new(val)) as i64
}

unsafe fn take_owned_val(raw: i64) -> Val {
    if raw == 0 {
        return Val::Null;
    }
    let arc = unsafe { Arc::from_raw(raw as *const Val) };
    (*arc).clone()
}

unsafe fn decode_helper_val(kind: i64, raw: i64) -> Val {
    match kind {
        HELPER_VAL_KIND_INT => Val::Int(raw),
        HELPER_VAL_KIND_BOOL => Val::Bool(raw != 0),
        HELPER_VAL_KIND_DEC => Val::Dec(unsafe { *(raw as *const D256) }),
        HELPER_VAL_KIND_NULL => Val::Null,
        HELPER_VAL_KIND_OWNED => {
            let ptr = raw as *const Val;
            unsafe { Arc::increment_strong_count(ptr) };
            let arc = unsafe { Arc::from_raw(ptr) };
            (*arc).clone()
        }
        _ => Val::Null,
    }
}

fn helper_value_kind(kind: RawKind) -> i64 {
    match kind {
        RawKind::Int => HELPER_VAL_KIND_INT,
        RawKind::Bool => HELPER_VAL_KIND_BOOL,
        RawKind::Dec => HELPER_VAL_KIND_DEC,
        RawKind::Null => HELPER_VAL_KIND_NULL,
        RawKind::OwnedVal | RawKind::TypeTag | RawKind::StringConst => HELPER_VAL_KIND_OWNED,
    }
}

/// Normalize a register's kind+value for passing to trampolines.
///
/// `Int | Bool | Dec | Null | OwnedVal` pass through unchanged — the trampoline
/// reads them directly from the args buffer.
///
/// `TypeTag | StringConst` registers are *not* handled here because the token
/// → original-string mapping needs the source `instructions` slice (see
/// `materialize_string_const_for_trampoline`); the caller must materialize
/// those into `OwnedVal` *before* calling this helper. Otherwise we'd silently
/// pass `Val::Null` for a type reference, which previously broke
/// `is-type(value, Null)` (and therefore JIT-compiled `is-null`) by giving
/// the runtime `is_type` a non-string second arg, returning Err and ultimately
/// `false` from `is-null` for *every* value.
fn normalize_for_trampoline(
    _builder: &mut FunctionBuilder<'_>,
    kind: RawKind,
    raw: cranelift_codegen::ir::Value,
) -> (RawKind, cranelift_codegen::ir::Value) {
    debug_assert!(
        !matches!(kind, RawKind::TypeTag | RawKind::StringConst),
        "TypeTag/StringConst args must be materialized via \
         materialize_string_const_for_trampoline before normalize_for_trampoline; \
         see emit_vm_callback for the canonical pattern"
    );
    (kind, raw)
}

/// Materialize a StringConst type token to its OwnedVal string representation.
/// Used when we need to pass the actual type name string to a trampoline.
fn materialize_string_const_for_trampoline(
    program: &BytecodeProgram,
    instructions: &[Instruction],
    reg: u32,
    builder: &mut FunctionBuilder<'_>,
) -> cranelift_codegen::ir::Value {
    for inst in instructions.iter().rev() {
        if let Instruction::LoadConst { dest, constant } = inst
            && *dest == reg
            && let Some(s) = match program.constants.get(*constant as usize) {
                Some(Constant::Val(Val::Str(s))) => Some(s.as_ref()),
                Some(Constant::StringRef(s)) => Some(s.as_ref()),
                Some(Constant::FunctionRef(s)) => Some(s.as_ref()),
                _ => None,
            }
        {
            let val = new_owned_val(Val::Str(s.to_string().into()));
            return builder.ins().iconst(types::I64, val);
        }
    }
    let null_val = new_owned_val(Val::Null);
    builder.ins().iconst(types::I64, null_val)
}

/// Resolve a register's kind+value for passing to a VM trampoline.
///
/// Wraps `normalize_for_trampoline` with a pre-step that promotes the
/// only kinds it can't faithfully serialize — `StringConst` and `TypeTag` —
/// into a freshly materialized `OwnedVal(Val::Str(<original-string>))` by
/// scanning `instructions` for the LoadConst that produced this register.
///
/// Without this step, a `LoadConst` of `"::hot::type/Null"` (which
/// `encode_special_string_const` collapses to a 1-word token) would round-trip
/// through the trampoline as `Val::Null`, so any JIT-compiled function that
/// passes a primitive type reference (e.g. the body of `is-null`, which calls
/// `is-type(value, Null)`) silently called the runtime with the wrong arg.
fn resolve_trampoline_arg(
    program: &BytecodeProgram,
    instructions: &[Instruction],
    reg: u32,
    kind: RawKind,
    raw: cranelift_codegen::ir::Value,
    builder: &mut FunctionBuilder<'_>,
) -> (RawKind, cranelift_codegen::ir::Value) {
    if matches!(kind, RawKind::TypeTag | RawKind::StringConst) {
        let owned = materialize_string_const_for_trampoline(program, instructions, reg, builder);
        return (RawKind::OwnedVal, owned);
    }
    normalize_for_trampoline(builder, kind, raw)
}

fn clone_owned_raw(
    builder: &mut FunctionBuilder<'_>,
    helper_refs: &JitHelperRefs,
    raw: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let call = builder.ins().call(helper_refs.clone_owned_val, &[raw]);
    builder.inst_results(call)[0]
}

fn drop_owned_var(builder: &mut FunctionBuilder<'_>, helper_refs: &JitHelperRefs, var: Variable) {
    let raw = builder.use_var(var);
    builder.ins().call(helper_refs.drop_owned_val, &[raw]);
}

fn collect_used_registers(instructions: &[Instruction]) -> std::collections::BTreeSet<u32> {
    let mut regs = std::collections::BTreeSet::new();
    for inst in instructions {
        match inst {
            Instruction::LoadConst { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::LoadVar { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::StoreVar { value, .. } => {
                regs.insert(*value);
            }
            Instruction::Move { dest, src } => {
                regs.insert(*dest);
                regs.insert(*src);
            }
            Instruction::Add { dest, left, right }
            | Instruction::Sub { dest, left, right }
            | Instruction::Mul { dest, left, right } => {
                regs.insert(*dest);
                regs.insert(*left);
                regs.insert(*right);
            }
            Instruction::Eq { dest, left, right } => {
                regs.insert(*dest);
                regs.insert(*left);
                regs.insert(*right);
            }
            Instruction::GetTypePath { dest, value } => {
                regs.insert(*dest);
                regs.insert(*value);
            }
            Instruction::IsType { dest, value, .. } => {
                regs.insert(*dest);
                regs.insert(*value);
            }
            Instruction::StrEndsWith {
                dest,
                string,
                suffix,
            } => {
                regs.insert(*dest);
                regs.insert(*string);
                regs.insert(*suffix);
            }
            Instruction::LoadFunctionRef { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::JumpIf { condition, .. } | Instruction::JumpIfNot { condition, .. } => {
                regs.insert(*condition);
            }
            Instruction::CondBranchStart { condition, .. } => {
                regs.insert(*condition);
            }
            Instruction::CondBranchEnd { result, .. } => {
                regs.insert(*result);
            }
            Instruction::EndFlow { dest } => {
                regs.insert(*dest);
            }
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => {
                regs.insert(*dest);
                regs.insert(*function);
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::CallUserFunction {
                dest,
                args_start,
                args_count,
                ..
            } => {
                regs.insert(*dest);
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::TailCall {
                args_start,
                args_count,
                ..
            } => {
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::Return { value } => {
                regs.insert(*value);
            }
            Instruction::ReturnIfErr { src } => {
                regs.insert(*src);
            }
            Instruction::MakeVec {
                dest,
                elements_start,
                count,
            } => {
                regs.insert(*dest);
                for i in 0..(*count as u32) {
                    regs.insert(*elements_start + i);
                }
            }
            Instruction::CallLibBuiltin {
                dest,
                function,
                args,
            } => {
                regs.insert(*dest);
                regs.insert(*function);
                regs.insert(*args);
            }
            Instruction::DotAccess {
                dest,
                object,
                property: _,
            } => {
                regs.insert(*dest);
                regs.insert(*object);
            }
            Instruction::MergeMaps { dest, source } => {
                regs.insert(*dest);
                regs.insert(*source);
            }
            Instruction::TemplateInterpolate {
                dest,
                parts_start,
                parts_count,
            } => {
                regs.insert(*dest);
                for i in 0..(*parts_count as u32) {
                    regs.insert(*parts_start + i);
                }
            }
            Instruction::ExtractInnerVal { dest, src } => {
                regs.insert(*dest);
                regs.insert(*src);
            }
            Instruction::EnsureResult { dest, value } => {
                regs.insert(*dest);
                regs.insert(*value);
            }
            Instruction::SetElement {
                collection,
                index,
                value,
            } => {
                regs.insert(*collection);
                regs.insert(*index);
                regs.insert(*value);
            }
            Instruction::ConstructTyped { dest, src, .. } => {
                regs.insert(*dest);
                regs.insert(*src);
            }
            Instruction::LoadVarOrDefault { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::LoadTypeRef { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::Gt { dest, left, right } | Instruction::Lt { dest, left, right } => {
                regs.insert(*dest);
                regs.insert(*left);
                regs.insert(*right);
            }
            Instruction::DotAccessOrDefault {
                dest,
                object,
                default_value,
                ..
            } => {
                regs.insert(*dest);
                regs.insert(*object);
                regs.insert(*default_value);
            }
            Instruction::VecAppend { vec, value } => {
                regs.insert(*vec);
                regs.insert(*value);
            }
            Instruction::CallLambda {
                dest,
                lambda,
                args_start,
                args_count,
            } => {
                regs.insert(*dest);
                regs.insert(*lambda);
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::Pipe { dest, src } => {
                regs.insert(*dest);
                regs.insert(*src);
            }
            Instruction::WrapOk { dest, src } => {
                regs.insert(*dest);
                regs.insert(*src);
            }
            Instruction::DeferredVarExpr { thunk, .. } => {
                regs.insert(*thunk);
            }
            Instruction::CallWithSpread {
                dest,
                function,
                args_start,
                args_count,
                ..
            } => {
                regs.insert(*dest);
                regs.insert(*function);
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::DynamicDotAccess {
                dest,
                object,
                property,
            } => {
                regs.insert(*dest);
                regs.insert(*object);
                regs.insert(*property);
            }
            Instruction::DynamicDotSet {
                object,
                property,
                value,
            } => {
                regs.insert(*object);
                regs.insert(*property);
                regs.insert(*value);
            }
            Instruction::DotSet { object, value, .. } => {
                regs.insert(*object);
                regs.insert(*value);
            }
            Instruction::MakeVecWithSpread {
                dest,
                elements_start,
                count,
                ..
            } => {
                regs.insert(*dest);
                for i in 0..(*count as u32) {
                    regs.insert(*elements_start + i);
                }
            }
            Instruction::StoreGlobal { value, .. } => {
                regs.insert(*value);
            }
            Instruction::StrStartsWith {
                dest,
                string,
                prefix,
            } => {
                regs.insert(*dest);
                regs.insert(*string);
                regs.insert(*prefix);
            }
            Instruction::CallNative {
                dest,
                args_start,
                args_count,
                ..
            } => {
                regs.insert(*dest);
                for i in 0..(*args_count as u32) {
                    regs.insert(*args_start + i);
                }
            }
            Instruction::DefineFunction { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::LoadScoped { dest, .. } => {
                regs.insert(*dest);
            }
            Instruction::CaptureVar { dest, .. } => {
                regs.insert(*dest);
            }
            // Instructions not listed above either have no register operands
            // (EnterScope, ExitScope, BeginErrorCapture, EndErrorCapture,
            // SetNamespace, BeginFlow) or are VM-only (LoadGlobal, StoreScoped,
            // VecPush) and will bail out before register mapping is consulted.
            _ => {}
        }
    }
    regs
}

/// Pre-scan instructions to determine the function's return kind without generating IR.
/// Tracks register kinds through the instruction stream and returns the kind of
/// Return instruction values. Returns None if the return kind cannot be determined.
fn prescan_return_kind(
    instructions: &[Instruction],
    program: &BytecodeProgram,
    function_id: FunctionId,
    signature: &TypeSig,
    param_names: &[String],
) -> Option<JitValueKind> {
    let mut reg_kinds: AHashMap<u32, RawKind> = AHashMap::new();
    let mut var_kinds: AHashMap<u32, RawKind> = AHashMap::new();
    let mut prescan_thunks: AHashMap<u32, crate::lang::bytecode::LambdaInfo> = AHashMap::new();

    // Map param names (as constant indices) to their signature kinds
    let mut param_kind_by_name: AHashMap<String, RawKind> = AHashMap::new();
    for (idx, name) in param_names.iter().enumerate() {
        if let Some(tag) = signature.args.get(idx)
            && let Some(kind) = tag.raw_kind()
        {
            param_kind_by_name.insert(name.clone(), kind);
        }
    }

    fn resolve_var_name(program: &BytecodeProgram, var_name: u32) -> Option<&str> {
        match program.constants.get(var_name as usize)? {
            Constant::Val(Val::Str(s)) => Some(s),
            Constant::StringRef(s) => Some(s),
            _ => None,
        }
    }

    let mut return_kind: Option<JitValueKind> = None;

    for (inst_ip, inst) in instructions.iter().enumerate() {
        match inst {
            Instruction::LoadConst { dest, constant } => {
                let kind = match program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Int(_))) => Some(RawKind::Int),
                    Some(Constant::Val(Val::Bool(_))) => Some(RawKind::Bool),
                    Some(Constant::Val(Val::Null)) => Some(RawKind::Null),
                    Some(Constant::Val(Val::Dec(_))) => Some(RawKind::Dec),
                    Some(Constant::Val(Val::Str(_)))
                    | Some(Constant::StringRef(_))
                    | Some(Constant::FunctionRef(_)) => Some(RawKind::OwnedVal),
                    Some(Constant::Val(Val::Box(b))) => {
                        if let Some(lambda) = b
                            .as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                        {
                            prescan_thunks.insert(*dest, lambda.clone());
                        }
                        Some(RawKind::OwnedVal)
                    }
                    _ => None,
                };
                if let Some(k) = kind {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::LoadVar { dest, var_name } => {
                if let Some(k) = var_kinds.get(var_name).copied() {
                    reg_kinds.insert(*dest, k);
                } else if let Some(name) = resolve_var_name(program, *var_name)
                    && let Some(k) = param_kind_by_name.get(name).copied()
                {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::StoreVar {
                var_name, value, ..
            } => {
                if let Some(k) = reg_kinds.get(value).copied() {
                    var_kinds.insert(*var_name, k);
                }
            }
            Instruction::Move { dest, src } => {
                if let Some(thunk) = prescan_thunks.get(src).cloned() {
                    prescan_thunks.insert(*dest, thunk);
                }
                if let Some(k) = reg_kinds.get(src).copied() {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::Add { dest, left, right }
            | Instruction::Sub { dest, left, right }
            | Instruction::Mul { dest, left, right } => {
                let lk = reg_kinds.get(left).copied();
                let rk = reg_kinds.get(right).copied();
                let kind = match (lk, rk) {
                    (Some(RawKind::Dec), _) | (_, Some(RawKind::Dec)) => RawKind::Dec,
                    (Some(RawKind::Int), Some(RawKind::Int)) => RawKind::Int,
                    _ => RawKind::OwnedVal,
                };
                reg_kinds.insert(*dest, kind);
            }
            Instruction::Eq { dest, .. }
            | Instruction::Lt { dest, .. }
            | Instruction::Gt { dest, .. }
            | Instruction::IsType { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::Bool);
            }
            Instruction::CallUserFunction {
                dest,
                function_id: fid,
                args_start,
                args_count,
            } => {
                if *fid == function_id {
                    if let Some(k) = return_kind {
                        reg_kinds.insert(*dest, k.raw_kind());
                    }
                } else {
                    // Check if this is a call with thunk args that we can analyze
                    let callee = program.functions.get(*fid as usize);
                    let has_lazy = callee.is_some_and(|f| f.lazy_params.iter().any(|&lp| lp));
                    if has_lazy {
                        let count = usize::from(*args_count);
                        // Phase 1: resolve thunks that don't depend on self-recursive return kind
                        let mut inferred_kind: Option<RawKind> = None;
                        for i in 0..count {
                            let arg_reg = *args_start + i as u32;
                            if let Some(lambda) = prescan_thunks.get(&arg_reg)
                                && let Some(k) = prescan_thunk_return_kind(
                                    &lambda.instructions,
                                    program,
                                    function_id,
                                    signature,
                                    param_names,
                                    return_kind,
                                )
                            {
                                inferred_kind = Some(k);
                                break;
                            }
                        }
                        // Phase 2: if we got a kind, use it as provisional return_kind
                        // and re-check unresolved thunks
                        if let Some(k) = inferred_kind {
                            let provisional = match k {
                                RawKind::Int => Some(JitValueKind::Int),
                                RawKind::Bool => Some(JitValueKind::Bool),
                                RawKind::Dec => Some(JitValueKind::Dec),
                                RawKind::Null => Some(JitValueKind::Null),
                                RawKind::OwnedVal => Some(JitValueKind::OwnedVal),
                                RawKind::TypeTag | RawKind::StringConst => None,
                            };
                            let mut consistent = true;
                            for i in 0..count {
                                let arg_reg = *args_start + i as u32;
                                if let Some(lambda) = prescan_thunks.get(&arg_reg)
                                    && let Some(k2) = prescan_thunk_return_kind(
                                        &lambda.instructions,
                                        program,
                                        function_id,
                                        signature,
                                        param_names,
                                        return_kind.or(provisional),
                                    )
                                    && k2 != k
                                {
                                    consistent = false;
                                }
                            }
                            if consistent {
                                reg_kinds.insert(*dest, k);
                            } else {
                                reg_kinds.insert(*dest, RawKind::OwnedVal);
                            }
                        } else {
                            reg_kinds.insert(*dest, RawKind::OwnedVal);
                        }
                    } else {
                        reg_kinds.insert(*dest, RawKind::OwnedVal);
                    }
                }
            }
            Instruction::Call {
                dest,
                function: fn_reg,
                args_start,
                args_count,
            } => {
                let fn_name = resolve_const_function_name(instructions, program, inst_ip, *fn_reg);
                let callee_idx = fn_name
                    .as_ref()
                    .filter(|n| n.starts_with("::"))
                    .and_then(|name| find_function_exact(program, name, Some(*args_count)));
                let callee = callee_idx.and_then(|idx| program.functions.get(idx));
                let has_lazy = callee.is_some_and(|f| f.lazy_params.iter().any(|&lp| lp));
                if has_lazy {
                    let count = usize::from(*args_count);
                    let mut inferred_kind: Option<RawKind> = None;
                    for i in 0..count {
                        let arg_reg = *args_start + i as u32;
                        if let Some(lambda) = prescan_thunks.get(&arg_reg)
                            && let Some(k) = prescan_thunk_return_kind(
                                &lambda.instructions,
                                program,
                                function_id,
                                signature,
                                param_names,
                                return_kind,
                            )
                        {
                            inferred_kind = Some(k);
                            break;
                        }
                    }
                    if let Some(k) = inferred_kind {
                        let provisional = match k {
                            RawKind::Int => Some(JitValueKind::Int),
                            RawKind::Bool => Some(JitValueKind::Bool),
                            RawKind::Dec => Some(JitValueKind::Dec),
                            RawKind::Null => Some(JitValueKind::Null),
                            RawKind::OwnedVal => Some(JitValueKind::OwnedVal),
                            RawKind::TypeTag | RawKind::StringConst => None,
                        };
                        let mut consistent = true;
                        for i in 0..count {
                            let arg_reg = *args_start + i as u32;
                            if let Some(lambda) = prescan_thunks.get(&arg_reg)
                                && let Some(k2) = prescan_thunk_return_kind(
                                    &lambda.instructions,
                                    program,
                                    function_id,
                                    signature,
                                    param_names,
                                    return_kind.or(provisional),
                                )
                                && k2 != k
                            {
                                consistent = false;
                            }
                        }
                        if consistent {
                            reg_kinds.insert(*dest, k);
                        } else {
                            reg_kinds.insert(*dest, RawKind::OwnedVal);
                        }
                    } else {
                        reg_kinds.insert(*dest, RawKind::OwnedVal);
                    }
                } else {
                    reg_kinds.insert(*dest, RawKind::OwnedVal);
                }
            }
            Instruction::MakeVec { dest, .. }
            | Instruction::CallLibBuiltin { dest, .. }
            | Instruction::DotAccess { dest, .. }
            | Instruction::TemplateInterpolate { dest, .. }
            | Instruction::DynamicDotAccess { dest, .. }
            | Instruction::MakeVecWithSpread { dest, .. }
            | Instruction::CallNative { dest, .. }
            | Instruction::DefineFunction { dest, .. }
            | Instruction::LoadScoped { dest, .. }
            | Instruction::CaptureVar { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::OwnedVal);
            }
            Instruction::StrStartsWith { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::Bool);
            }
            Instruction::MergeMaps { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::OwnedVal);
            }
            Instruction::EndFlow { dest } if !reg_kinds.contains_key(dest) => {
                reg_kinds.insert(*dest, RawKind::OwnedVal);
            }
            Instruction::Return { value } => {
                let kind = reg_kinds.get(value).copied()?;
                let jvk = match kind {
                    RawKind::Int => JitValueKind::Int,
                    RawKind::Bool => JitValueKind::Bool,
                    RawKind::Dec => JitValueKind::Dec,
                    RawKind::Null => JitValueKind::Null,
                    RawKind::OwnedVal => JitValueKind::OwnedVal,
                    RawKind::TypeTag | RawKind::StringConst => return None,
                };
                if let Some(existing) = return_kind {
                    if existing != jvk {
                        return None;
                    }
                } else {
                    return_kind = Some(jvk);
                }
            }
            // Instructions not listed above don't produce register results that affect
            // return-kind inference. This includes control flow (Jump, JumpIf, JumpIfNot,
            // BeginFlow, EndFlow, CondBranch*, Pipe), scope management (EnterScope,
            // ExitScope, StoreVar), error handling (ReturnIfErr, BeginErrorCapture,
            // EndErrorCapture, WrapOk), and instructions not yet JIT-compiled.
            _ => {}
        }
    }

    return_kind
}

/// Determine the return kind of a thunk's instructions during prescan.
fn prescan_thunk_return_kind(
    instructions: &[Instruction],
    program: &BytecodeProgram,
    parent_function_id: FunctionId,
    parent_signature: &TypeSig,
    parent_param_names: &[String],
    parent_return_kind: Option<JitValueKind>,
) -> Option<RawKind> {
    let mut reg_kinds: AHashMap<u32, RawKind> = AHashMap::new();

    let mut param_kind_by_name: AHashMap<String, RawKind> = AHashMap::new();
    for (idx, name) in parent_param_names.iter().enumerate() {
        if let Some(tag) = parent_signature.args.get(idx)
            && let Some(k) = tag.raw_kind()
        {
            param_kind_by_name.insert(name.clone(), k);
        }
    }

    let mut thunk_return_kind: Option<RawKind> = None;

    for (inst_ip, inst) in instructions.iter().enumerate() {
        match inst {
            Instruction::LoadConst { dest, constant } => {
                let kind = match program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Int(_))) => Some(RawKind::Int),
                    Some(Constant::Val(Val::Bool(_))) => Some(RawKind::Bool),
                    Some(Constant::Val(Val::Null)) => Some(RawKind::Null),
                    Some(Constant::Val(Val::Dec(_))) => Some(RawKind::Dec),
                    _ => Some(RawKind::OwnedVal),
                };
                if let Some(k) = kind {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::LoadVar { dest, var_name } => {
                if let Some(name) =
                    program
                        .constants
                        .get(*var_name as usize)
                        .and_then(|c| match c {
                            Constant::Val(Val::Str(s)) => Some(&**s),
                            Constant::StringRef(s) | Constant::FunctionRef(s) => Some(&**s),
                            _ => None,
                        })
                    && let Some(k) = param_kind_by_name.get(name).copied()
                {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::Move { dest, src } => {
                if let Some(k) = reg_kinds.get(src).copied() {
                    reg_kinds.insert(*dest, k);
                }
            }
            Instruction::Add { dest, left, right }
            | Instruction::Sub { dest, left, right }
            | Instruction::Mul { dest, left, right } => {
                let lk = reg_kinds.get(left).copied();
                let rk = reg_kinds.get(right).copied();
                let kind = match (lk, rk) {
                    (Some(RawKind::Dec), _) | (_, Some(RawKind::Dec)) => RawKind::Dec,
                    (Some(RawKind::Int), Some(RawKind::Int)) => RawKind::Int,
                    _ => RawKind::OwnedVal,
                };
                reg_kinds.insert(*dest, kind);
            }
            Instruction::Lt { dest, .. } | Instruction::Gt { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::Bool);
            }
            Instruction::CallUserFunction {
                dest,
                function_id: fid,
                ..
            } => {
                if *fid == parent_function_id {
                    if let Some(k) = parent_return_kind {
                        reg_kinds.insert(*dest, k.raw_kind());
                    }
                } else {
                    reg_kinds.insert(*dest, RawKind::OwnedVal);
                }
            }
            Instruction::Call {
                dest,
                function: fn_reg,
                args_start,
                args_count,
            } => {
                let fn_name = resolve_const_function_name(instructions, program, inst_ip, *fn_reg);
                let name_str = fn_name.as_deref().unwrap_or("");
                let is_known_arith = matches!(
                    name_str,
                    "add"
                        | "sub"
                        | "mul"
                        | "div"
                        | "mod"
                        | "::hot::math/add"
                        | "::hot::math/sub"
                        | "::hot::math/mul"
                        | "::hot::math/div"
                        | "::hot::math/mod"
                );
                let is_known_cmp = matches!(
                    name_str,
                    "lt" | "gt"
                        | "lte"
                        | "gte"
                        | "eq"
                        | "ne"
                        | "::hot::cmp/lt"
                        | "::hot::cmp/gt"
                        | "::hot::cmp/lte"
                        | "::hot::cmp/gte"
                        | "::hot::cmp/eq"
                        | "::hot::cmp/ne"
                );
                if is_known_cmp {
                    reg_kinds.insert(*dest, RawKind::Bool);
                } else if is_known_arith && usize::from(*args_count) == 2 {
                    let lk = reg_kinds.get(args_start).copied();
                    let rk = reg_kinds.get(&(args_start + 1)).copied();
                    let kind = match (lk, rk) {
                        (Some(RawKind::Dec), _) | (_, Some(RawKind::Dec)) => RawKind::Dec,
                        (Some(RawKind::Int), Some(RawKind::Int)) => RawKind::Int,
                        _ => RawKind::OwnedVal,
                    };
                    reg_kinds.insert(*dest, kind);
                } else {
                    // Detect self-recursive calls by name (qualified names only)
                    let resolved_fid = fn_name
                        .as_ref()
                        .filter(|n| n.starts_with("::"))
                        .and_then(|n| find_function_exact(program, n, Some(*args_count)));
                    if resolved_fid == Some(parent_function_id as usize) {
                        if let Some(k) = parent_return_kind {
                            reg_kinds.insert(*dest, k.raw_kind());
                        } else {
                            reg_kinds.insert(*dest, RawKind::OwnedVal);
                        }
                    } else {
                        reg_kinds.insert(*dest, RawKind::OwnedVal);
                    }
                }
            }
            Instruction::MakeVec { dest, .. }
            | Instruction::CallLibBuiltin { dest, .. }
            | Instruction::DotAccess { dest, .. }
            | Instruction::MergeMaps { dest, .. }
            | Instruction::TemplateInterpolate { dest, .. }
            | Instruction::DynamicDotAccess { dest, .. }
            | Instruction::MakeVecWithSpread { dest, .. }
            | Instruction::CallNative { dest, .. }
            | Instruction::DefineFunction { dest, .. }
            | Instruction::LoadScoped { dest, .. }
            | Instruction::CaptureVar { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::OwnedVal);
            }
            Instruction::StrStartsWith { dest, .. } => {
                reg_kinds.insert(*dest, RawKind::Bool);
            }
            Instruction::Return { value } => {
                if let Some(k) = reg_kinds.get(value).copied() {
                    if let Some(existing) = thunk_return_kind {
                        if existing != k {
                            return None;
                        }
                    } else {
                        thunk_return_kind = Some(k);
                    }
                }
            }
            // Thunk instructions not listed above (control flow, scope, error handling)
            // don't produce register results that affect thunk return-kind inference.
            _ => {}
        }
    }

    thunk_return_kind
}

/// Shared emission context for JIT instruction compilation.
/// Holds register state, locals, and program metadata so that both
/// `build_supported_body` and `compile_thunk_inline` can share instruction
/// handlers without duplicating logic.
struct EmitCtx<'a> {
    program: &'a BytecodeProgram,
    function_id: FunctionId,
    signature: &'a TypeSig,
    ptr_ty: Type,
    self_func_ref: FuncRef,
    helper_refs: JitHelperRefs,
    arg_layout: AbiLayout,
    expected_return_kind: Option<JitValueKind>,
    blocks: Vec<cranelift_codegen::ir::Block>,
    remap: AHashMap<u32, usize>,
    registers: Vec<Variable>,
    register_kinds: Vec<Option<RawKind>>,
    compile_time_owned: std::collections::HashSet<usize>,
    locals: AHashMap<String, (Variable, RawKind)>,
    thunk_registry: AHashMap<u32, crate::lang::bytecode::LambdaInfo>,
    scope_snapshots: Vec<AHashMap<String, (Variable, RawKind)>>,
    error_capture_active: bool,
}

enum EmitResult {
    Handled,
    Unhandled,
}

impl<'a> EmitCtx<'a> {
    fn register_kind(&self, reg: u32) -> Result<RawKind, String> {
        register_kind(&self.remap, &self.register_kinds, reg)
    }

    fn remap_reg(&self, reg: u32) -> Result<usize, String> {
        remap_reg(&self.remap, reg)
    }

    fn define_register(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        dest: u32,
        kind: RawKind,
        value: cranelift_codegen::ir::Value,
    ) -> Result<(), String> {
        define_register(
            builder,
            &self.helper_refs,
            &self.remap,
            &self.registers,
            &mut self.register_kinds,
            &mut self.compile_time_owned,
            dest,
            kind,
            value,
        )
    }

    fn jump_to_next(&self, builder: &mut FunctionBuilder<'_>, ip: usize) -> Result<(), String> {
        let next = self
            .blocks
            .get(ip + 1)
            .copied()
            .ok_or_else(|| "Instruction past end of block list".to_string())?;
        builder.ins().jump(next, &[]);
        Ok(())
    }

    fn reg_value(
        &self,
        builder: &mut FunctionBuilder<'_>,
        reg: u32,
    ) -> Result<cranelift_codegen::ir::Value, String> {
        Ok(builder.use_var(self.registers[self.remap_reg(reg)?]))
    }

    /// Build arg pairs buffer and emit a call_vm_by_name trampoline invocation
    /// for functions not found in program.functions (builtins like Vec, Str, etc.).
    fn emit_vm_callback_by_name(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        dest: u32,
        fn_name: &str,
        args_start: u32,
        args_count: u8,
        instructions: &[Instruction],
    ) -> Result<(), String> {
        let count = usize::from(args_count);
        let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (count * 2 * 8) as u32,
            3,
        ));
        for i in 0..count {
            let reg = args_start + i as u32;
            let kind = self.register_kind(reg)?;
            let raw = self.reg_value(builder, reg)?;
            let (eff_kind, eff_raw) =
                resolve_trampoline_arg(self.program, instructions, reg, kind, raw, builder);
            let kind_val = builder
                .ins()
                .iconst(types::I64, helper_value_kind(eff_kind));
            builder
                .ins()
                .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
            let raw_val = if eff_kind == RawKind::OwnedVal {
                clone_owned_raw(builder, &self.helper_refs, eff_raw)
            } else {
                eff_raw
            };
            builder
                .ins()
                .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
        }
        let args_buf_ptr = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
        let name_val = new_owned_val(Val::Str(fn_name.to_string().into()));
        let name_ptr = builder.ins().iconst(types::I64, name_val);
        let count_val = builder.ins().iconst(types::I64, count as i64);
        let call = builder.ins().call(
            self.helper_refs.call_vm_by_name,
            &[name_ptr, args_buf_ptr, count_val],
        );
        let result_handle = builder.inst_results(call)[0];
        self.define_register(builder, dest, RawKind::OwnedVal, result_handle)
    }

    /// Like emit_vm_callback_by_name but takes a runtime OwnedVal pointer for the function name.
    fn emit_vm_callback_by_name_raw(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        dest: u32,
        fn_name_ptr: cranelift_codegen::ir::Value,
        args_start: u32,
        args_count: u8,
        instructions: &[Instruction],
    ) -> Result<(), String> {
        let count = usize::from(args_count);
        let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (count * 2 * 8) as u32,
            3,
        ));
        for i in 0..count {
            let reg = args_start + i as u32;
            let kind = self.register_kind(reg)?;
            let raw = self.reg_value(builder, reg)?;
            let (eff_kind, eff_raw) =
                resolve_trampoline_arg(self.program, instructions, reg, kind, raw, builder);
            let kind_val = builder
                .ins()
                .iconst(types::I64, helper_value_kind(eff_kind));
            builder
                .ins()
                .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
            let raw_val = if eff_kind == RawKind::OwnedVal {
                clone_owned_raw(builder, &self.helper_refs, eff_raw)
            } else {
                eff_raw
            };
            builder
                .ins()
                .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
        }
        let args_buf_ptr = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
        let count_val = builder.ins().iconst(types::I64, count as i64);
        let call = builder.ins().call(
            self.helper_refs.call_vm_by_name,
            &[fn_name_ptr, args_buf_ptr, count_val],
        );
        let result_handle = builder.inst_results(call)[0];
        self.define_register(builder, dest, RawKind::OwnedVal, result_handle)
    }

    /// Build arg pairs buffer and emit a call_vm trampoline invocation.
    fn emit_vm_callback(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        dest: u32,
        called_function_id: u32,
        args_start: u32,
        args_count: u8,
        instructions: &[Instruction],
    ) -> Result<(), String> {
        let count = usize::from(args_count);
        let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (count * 2 * 8) as u32,
            3,
        ));
        for i in 0..count {
            let reg = args_start + i as u32;
            let kind = self.register_kind(reg)?;
            let raw = self.reg_value(builder, reg)?;
            let (eff_kind, eff_raw) =
                resolve_trampoline_arg(self.program, instructions, reg, kind, raw, builder);
            let kind_val = builder
                .ins()
                .iconst(types::I64, helper_value_kind(eff_kind));
            builder
                .ins()
                .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
            let raw_val = if eff_kind == RawKind::OwnedVal {
                clone_owned_raw(builder, &self.helper_refs, eff_raw)
            } else {
                eff_raw
            };
            builder
                .ins()
                .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
        }
        let args_buf_ptr = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
        let fn_id_val = builder.ins().iconst(types::I64, called_function_id as i64);
        let count_val = builder.ins().iconst(types::I64, count as i64);
        let call = builder.ins().call(
            self.helper_refs.call_vm,
            &[fn_id_val, args_buf_ptr, count_val],
        );
        let result_handle = builder.inst_results(call)[0];
        self.define_register(builder, dest, RawKind::OwnedVal, result_handle)
    }

    /// Try all call resolution strategies for a named function call (Call instruction).
    /// Returns Ok(true) if handled, Ok(false) if unhandled.
    #[allow(clippy::too_many_arguments)]
    fn emit_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        dest: u32,
        fn_reg: u32,
        args_start: u32,
        args_count: u8,
        instructions: &[Instruction],
    ) -> Result<bool, String> {
        let fn_name = match resolve_const_function_name(instructions, self.program, ip, fn_reg) {
            Some(name) => name,
            None => {
                let fn_kind = register_kind(&self.remap, &self.register_kinds, fn_reg)?;
                let fn_raw = builder.use_var(self.registers[remap_reg(&self.remap, fn_reg)?]);
                let fn_owned = if fn_kind == RawKind::OwnedVal {
                    fn_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, fn_kind, fn_raw)?
                };
                self.emit_vm_callback_by_name_raw(
                    builder,
                    dest,
                    fn_owned,
                    args_start,
                    args_count,
                    instructions,
                )?;
                self.jump_to_next(builder, ip)?;
                return Ok(true);
            }
        };
        // 1. Try core intrinsic (explicit allowlist — deterministic)
        if let Some((result_kind, raw)) = try_lower_known_core_call(
            builder,
            self.ptr_ty,
            self.helper_refs,
            &self.remap,
            self.program,
            &self.registers,
            &self.register_kinds,
            &fn_name,
            args_start,
            args_count,
        )? {
            self.define_register(builder, dest, result_kind, raw)?;
            self.jump_to_next(builder, ip)?;
            return Ok(true);
        }
        // For qualified names (e.g. "::hot::core/do"), resolve deterministically
        // by exact match. For unqualified names (e.g. "if", "and"), also try
        // resolving via suffix match so that thunk inlining can kick in for
        // core functions whose qualified name differs from the call-site name.
        let resolved_fid = if fn_name.starts_with("::") {
            find_function_exact(self.program, &fn_name, Some(args_count))
        } else {
            find_function_by_suffix(self.program, &fn_name, Some(args_count))
        };
        // 2. Try lazy branch inlining (only with deterministically resolved ID)
        if let Some(called_fid) = resolved_fid
            && let Some((result_kind, result_val)) = try_inline_lazy_branch_call(
                builder,
                self.program,
                self.function_id,
                self.signature,
                self.ptr_ty,
                self.self_func_ref,
                &self.helper_refs,
                &self.arg_layout,
                self.expected_return_kind,
                called_fid as u32,
                args_start,
                args_count,
                &self.locals,
                &self.remap,
                &self.registers,
                &self.register_kinds,
                &self.thunk_registry,
            )?
        {
            self.define_register(builder, dest, result_kind, result_val)?;
            self.jump_to_next(builder, ip)?;
            return Ok(true);
        }
        // 3. Try self-recursive by name (only with deterministically resolved ID)
        if let Some(called_fid) = resolved_fid
            && called_fid as u32 == self.function_id
            && let Some(ret_kind) = self.expected_return_kind
        {
            let (result_kind, raw, error_info) = emit_self_recursive_call(
                builder,
                self.ptr_ty,
                &self.helper_refs,
                self.self_func_ref,
                self.signature,
                &self.arg_layout,
                ret_kind,
                &self.remap,
                &self.registers,
                &self.register_kinds,
                args_start,
                args_count,
            )?;
            self.define_register(builder, dest, result_kind, raw)?;
            self.jump_to_next(builder, ip)?;
            if let Some((error_block, error_val)) = error_info {
                self.handle_recursive_call_error(builder, error_block, error_val);
            }
            return Ok(true);
        }
        // 4. VM callback — deterministic resolution by the VM's unified_function_lookup
        if let Some(called_fid) = resolved_fid {
            self.emit_vm_callback(
                builder,
                dest,
                called_fid as u32,
                args_start,
                args_count,
                instructions,
            )?;
        } else {
            self.emit_vm_callback_by_name(
                builder,
                dest,
                &fn_name,
                args_start,
                args_count,
                instructions,
            )?;
        }
        self.jump_to_next(builder, ip)?;
        Ok(true)
    }

    /// Try all call resolution strategies for a CallUserFunction instruction.
    /// Returns Ok(true) if handled, Ok(false) if it's a self-recursive call
    /// that the caller should handle (e.g. TailCall in body).
    #[allow(clippy::too_many_arguments)]
    fn emit_call_user_function(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        dest: u32,
        called_function_id: u32,
        args_start: u32,
        args_count: u8,
        instructions: &[Instruction],
    ) -> Result<bool, String> {
        if called_function_id == self.function_id {
            return Ok(false);
        }
        // Try lazy branch inlining
        if let Some((result_kind, result_val)) = try_inline_lazy_branch_call(
            builder,
            self.program,
            self.function_id,
            self.signature,
            self.ptr_ty,
            self.self_func_ref,
            &self.helper_refs,
            &self.arg_layout,
            self.expected_return_kind,
            called_function_id,
            args_start,
            args_count,
            &self.locals,
            &self.remap,
            &self.registers,
            &self.register_kinds,
            &self.thunk_registry,
        )? {
            self.define_register(builder, dest, result_kind, result_val)?;
            self.jump_to_next(builder, ip)?;
            return Ok(true);
        }
        // Try core intrinsic
        let callee_fn_name = self
            .program
            .functions
            .get(called_function_id as usize)
            .map(|f| f.name.as_str())
            .unwrap_or("");
        if let Some((result_kind, raw)) = try_lower_known_core_call(
            builder,
            self.ptr_ty,
            self.helper_refs,
            &self.remap,
            self.program,
            &self.registers,
            &self.register_kinds,
            callee_fn_name,
            args_start,
            args_count,
        )? {
            self.define_register(builder, dest, result_kind, raw)?;
            self.jump_to_next(builder, ip)?;
            return Ok(true);
        }
        // Fall back to VM callback
        self.emit_vm_callback(
            builder,
            dest,
            called_function_id,
            args_start,
            args_count,
            instructions,
        )?;
        self.jump_to_next(builder, ip)?;
        Ok(true)
    }

    /// Handle a self-recursive CallUserFunction (used by both body and thunk).
    fn emit_self_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        dest: u32,
        args_start: u32,
        args_count: u8,
    ) -> Result<(), String> {
        let ret_kind = self.expected_return_kind.ok_or_else(|| {
            "JIT cannot determine return kind for self-recursive call".to_string()
        })?;
        let (result_kind, raw, error_info) = emit_self_recursive_call(
            builder,
            self.ptr_ty,
            &self.helper_refs,
            self.self_func_ref,
            self.signature,
            &self.arg_layout,
            ret_kind,
            &self.remap,
            &self.registers,
            &self.register_kinds,
            args_start,
            args_count,
        )?;
        self.define_register(builder, dest, result_kind, raw)?;
        self.jump_to_next(builder, ip)?;
        if let Some((error_block, error_val)) = error_info {
            self.handle_recursive_call_error(builder, error_block, error_val);
        }
        Ok(())
    }

    /// Handle the error block from a self-recursive call by cleaning up owned
    /// values and returning the error from the current JIT function.
    fn handle_recursive_call_error(
        &self,
        builder: &mut FunctionBuilder<'_>,
        error_block: cranelift_codegen::ir::Block,
        error_val: cranelift_codegen::ir::Value,
    ) {
        builder.switch_to_block(error_block);
        builder.seal_block(error_block);
        self.emit_cleanup_owned(builder);
        builder.ins().return_(&[error_val]);
    }

    /// Emit an error check for ReturnIfErr. If the source register is not OwnedVal,
    /// emits a jump to the next instruction and returns None. Otherwise, calls
    /// `hot_jit_is_err` and branches: error goes to a new error block, ok goes to next.
    /// Returns `Some((error_block, raw_value))` when the caller must fill in the error block.
    fn emit_err_check(
        &self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        src: u32,
    ) -> Result<Option<(cranelift_codegen::ir::Block, cranelift_codegen::ir::Value)>, String> {
        let kind = self.register_kind(src)?;
        if kind != RawKind::OwnedVal {
            self.jump_to_next(builder, ip)?;
            return Ok(None);
        }
        let raw = self.reg_value(builder, src)?;
        let call = builder.ins().call(self.helper_refs.is_err, &[raw]);
        let is_err_result = builder.inst_results(call)[0];
        let zero = builder.ins().iconst(types::I64, 0);
        let is_err_flag = builder.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::NotEqual,
            is_err_result,
            zero,
        );
        let error_block = builder.create_block();
        let next = self
            .blocks
            .get(ip + 1)
            .copied()
            .ok_or_else(|| "ReturnIfErr at end of block list".to_string())?;
        builder.ins().brif(is_err_flag, error_block, &[], next, &[]);
        Ok(Some((error_block, raw)))
    }

    /// Drop all owned registers and locals. Used for cleanup before return/error paths.
    fn emit_cleanup_owned(&self, builder: &mut FunctionBuilder<'_>) {
        for (idx, existing_kind) in self.register_kinds.iter().enumerate() {
            if *existing_kind == Some(RawKind::OwnedVal) && !self.compile_time_owned.contains(&idx)
            {
                drop_owned_var(builder, &self.helper_refs, self.registers[idx]);
            }
        }
        for (var, local_kind) in self.locals.values() {
            if *local_kind == RawKind::OwnedVal {
                drop_owned_var(builder, &self.helper_refs, *var);
            }
        }
    }

    /// Emit shared instructions that are identical between body and thunk.
    /// Returns `EmitResult::Handled` if the instruction was compiled,
    /// `EmitResult::Unhandled` for caller-specific instructions.
    fn emit_instruction(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        instruction: &Instruction,
        instructions: &[Instruction],
    ) -> Result<EmitResult, String> {
        match instruction {
            Instruction::LoadConst { dest, constant } => {
                let (kind, value) = match self.program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Int(n))) => {
                        (RawKind::Int, builder.ins().iconst(types::I64, *n))
                    }
                    Some(Constant::Val(Val::Bool(b))) => (
                        RawKind::Bool,
                        builder.ins().iconst(types::I64, if *b { 1 } else { 0 }),
                    ),
                    Some(Constant::Val(Val::Null)) => {
                        (RawKind::Null, builder.ins().iconst(types::I64, 0))
                    }
                    Some(Constant::Val(Val::Str(s))) => {
                        if let Some(token) = encode_special_string_const(s) {
                            (
                                RawKind::StringConst,
                                builder.ins().iconst(types::I64, token),
                            )
                        } else {
                            (
                                RawKind::OwnedVal,
                                builder
                                    .ins()
                                    .iconst(types::I64, new_owned_val(Val::Str(s.clone()))),
                            )
                        }
                    }
                    Some(Constant::Val(Val::Dec(d))) => {
                        let slot = builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            mem::size_of::<D256>() as u32,
                            3,
                        ));
                        let slot_ptr = builder.ins().stack_addr(self.ptr_ty, slot, 0);
                        let const_ptr = builder.ins().iconst(types::I64, d as *const D256 as i64);
                        builder
                            .ins()
                            .call(self.helper_refs.copy_dec, &[const_ptr, slot_ptr]);
                        (RawKind::Dec, slot_ptr)
                    }
                    Some(Constant::StringRef(s)) | Some(Constant::FunctionRef(s)) => {
                        if let Some(token) = encode_special_string_const(s) {
                            (
                                RawKind::StringConst,
                                builder.ins().iconst(types::I64, token),
                            )
                        } else {
                            (
                                RawKind::OwnedVal,
                                builder
                                    .ins()
                                    .iconst(types::I64, new_owned_val(Val::Str(s.clone()))),
                            )
                        }
                    }
                    Some(Constant::Val(Val::Box(b)))
                        if b.as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                            .is_some() =>
                    {
                        let lambda_info = b
                            .as_any()
                            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                            .unwrap()
                            .clone();
                        self.thunk_registry.insert(*dest, lambda_info.clone());
                        let base_lambda = builder
                            .ins()
                            .iconst(types::I64, new_owned_val(Val::Box(b.clone())));

                        let mut capture_names: Vec<cranelift_codegen::ir::Value> = Vec::new();
                        let mut capture_values: Vec<cranelift_codegen::ir::Value> = Vec::new();
                        for var_name in &lambda_info.capture_vars {
                            let owned = if let Some(&(var, kind)) =
                                self.locals.get(var_name.as_str())
                            {
                                let raw = builder.use_var(var);
                                if kind == RawKind::OwnedVal {
                                    clone_owned_raw(builder, &self.helper_refs, raw)
                                } else {
                                    promote_to_owned(builder, &self.helper_refs, kind, raw)?
                                }
                            } else {
                                let name_val = new_owned_val(Val::Str(var_name.to_string().into()));
                                let name_ptr = builder.ins().iconst(types::I64, name_val);
                                let call =
                                    builder.ins().call(self.helper_refs.lookup_var, &[name_ptr]);
                                builder.inst_results(call)[0]
                            };
                            let name_val = new_owned_val(Val::Str(var_name.to_string().into()));
                            let name_ptr = builder.ins().iconst(types::I64, name_val);
                            capture_names.push(name_ptr);
                            capture_values.push(owned);
                        }

                        if capture_names.is_empty() {
                            (RawKind::OwnedVal, base_lambda)
                        } else {
                            let n = capture_names.len();
                            let names_slot = builder.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                (n * 8) as u32,
                                3,
                            ));
                            let values_slot = builder.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                (n * 8) as u32,
                                3,
                            ));
                            for (i, (name_v, val_v)) in
                                capture_names.iter().zip(capture_values.iter()).enumerate()
                            {
                                builder
                                    .ins()
                                    .stack_store(*name_v, names_slot, (i * 8) as i32);
                                builder
                                    .ins()
                                    .stack_store(*val_v, values_slot, (i * 8) as i32);
                            }
                            let names_ptr = builder.ins().stack_addr(self.ptr_ty, names_slot, 0);
                            let values_ptr = builder.ins().stack_addr(self.ptr_ty, values_slot, 0);
                            let count_val = builder.ins().iconst(types::I64, n as i64);
                            let call = builder.ins().call(
                                self.helper_refs.populate_closure,
                                &[base_lambda, names_ptr, values_ptr, count_val],
                            );
                            let captured = builder.inst_results(call)[0];
                            (RawKind::OwnedVal, captured)
                        }
                    }
                    Some(Constant::Val(v)) => (
                        RawKind::OwnedVal,
                        builder.ins().iconst(types::I64, new_owned_val(v.clone())),
                    ),
                    other => {
                        return Err(format!(
                            "Unsupported constant in JIT: {:?}",
                            other.map(std::mem::discriminant)
                        ));
                    }
                };
                self.define_register(builder, *dest, kind, value)?;
                if kind == RawKind::OwnedVal
                    && let Ok(idx) = self.remap_reg(*dest)
                {
                    self.compile_time_owned.insert(idx);
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::LoadVar { dest, var_name } => {
                let name = constant_string(self.program, *var_name)?;
                if let Some(&(var, kind)) = self.locals.get(name) {
                    let mut value = builder.use_var(var);
                    if kind == RawKind::OwnedVal {
                        value = clone_owned_raw(builder, &self.helper_refs, value);
                    }
                    self.define_register(builder, *dest, kind, value)?;
                } else {
                    let name_val = new_owned_val(Val::Str(name.to_string().into()));
                    let name_raw = builder.ins().iconst(types::I64, name_val);
                    let call = builder.ins().call(self.helper_refs.lookup_var, &[name_raw]);
                    let result = builder.inst_results(call)[0];
                    self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                    if let Ok(idx) = self.remap_reg(*dest) {
                        self.compile_time_owned.insert(idx);
                    }
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } => {
                let name = constant_string(self.program, *function_name)?;
                let val = new_owned_val(Val::Str(name.to_string().into()));
                let value = builder.ins().iconst(types::I64, val);
                self.define_register(builder, *dest, RawKind::OwnedVal, value)?;
                if let Ok(idx) = self.remap_reg(*dest) {
                    self.compile_time_owned.insert(idx);
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::StoreVar {
                var_name, value, ..
            } => {
                let name = constant_string(self.program, *var_name)?.to_string();
                let kind = self.register_kind(*value)?;
                let mut raw = self.reg_value(builder, *value)?;
                if kind == RawKind::OwnedVal {
                    raw = clone_owned_raw(builder, &self.helper_refs, raw);
                }
                if let Some((var, existing_kind)) = self.locals.get(&name).copied() {
                    if existing_kind != kind {
                        return Err(format!("JIT local '{}' changes type", name));
                    }
                    if existing_kind == RawKind::OwnedVal {
                        drop_owned_var(builder, &self.helper_refs, var);
                    }
                    builder.def_var(var, raw);
                } else {
                    let var = builder.declare_var(types::I64);
                    builder.def_var(var, raw);
                    self.locals.insert(name, (var, kind));
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Move { dest, src } => {
                if let Some(thunk) = self.thunk_registry.get(src).cloned() {
                    self.thunk_registry.insert(*dest, thunk);
                }
                let kind = self.register_kind(*src)?;
                let mut value = self.reg_value(builder, *src)?;
                if kind == RawKind::OwnedVal {
                    value = clone_owned_raw(builder, &self.helper_refs, value);
                }
                self.define_register(builder, *dest, kind, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Add { dest, left, right } => {
                let (kind, value) = numeric_binop(
                    builder,
                    self.ptr_ty,
                    self.helper_refs.add_numeric,
                    &self.helper_refs,
                    BINOP_ADD,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                    |b, l, r| b.ins().iadd(l, r),
                )?;
                self.define_register(builder, *dest, kind, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Sub { dest, left, right } => {
                let (kind, value) = numeric_binop(
                    builder,
                    self.ptr_ty,
                    self.helper_refs.sub_numeric,
                    &self.helper_refs,
                    BINOP_SUB,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                    |b, l, r| b.ins().isub(l, r),
                )?;
                self.define_register(builder, *dest, kind, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Mul { dest, left, right } => {
                let (kind, value) = numeric_binop(
                    builder,
                    self.ptr_ty,
                    self.helper_refs.mul_numeric,
                    &self.helper_refs,
                    BINOP_MUL,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                    |b, l, r| b.ins().imul(l, r),
                )?;
                self.define_register(builder, *dest, kind, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Eq { dest, left, right } => {
                let value = numeric_eq(
                    builder,
                    self.helper_refs.eq_numeric,
                    self.helper_refs.eq_general,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                )?;
                self.define_register(builder, *dest, RawKind::Bool, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Gt { dest, left, right } => {
                let value = numeric_compare(
                    builder,
                    self.helper_refs.gt_numeric,
                    &self.helper_refs,
                    1,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                    cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThan,
                )?;
                self.define_register(builder, *dest, RawKind::Bool, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Lt { dest, left, right } => {
                let value = numeric_compare(
                    builder,
                    self.helper_refs.lt_numeric,
                    &self.helper_refs,
                    0,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *left,
                    *right,
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                )?;
                self.define_register(builder, *dest, RawKind::Bool, value)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::GetTypePath { dest, value } => {
                let source_kind = self.register_kind(*value)?;
                if let Ok(token) = raw_type_token(source_kind) {
                    let raw = builder.ins().iconst(types::I64, token);
                    self.define_register(builder, *dest, RawKind::TypeTag, raw)?;
                } else {
                    let val_raw = self.reg_value(builder, *value)?;
                    let call = builder
                        .ins()
                        .call(self.helper_refs.get_type_path, &[val_raw]);
                    let result = builder.inst_results(call)[0];
                    self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::IsType {
                dest,
                value,
                type_path,
            } => {
                let value_kind = self.register_kind(*value)?;
                let type_kind = self.register_kind(*type_path)?;
                if matches!(type_kind, RawKind::StringConst | RawKind::TypeTag)
                    && matches!(
                        value_kind,
                        RawKind::Int | RawKind::Bool | RawKind::Dec | RawKind::Null
                    )
                {
                    if let Ok(token) = raw_type_token(value_kind) {
                        let actual = builder.ins().iconst(types::I64, token);
                        let expected = self.reg_value(builder, *type_path)?;
                        let cmp = builder.ins().icmp(
                            cranelift_codegen::ir::condcodes::IntCC::Equal,
                            actual,
                            expected,
                        );
                        let one = builder.ins().iconst(types::I64, 1);
                        let zero_val = builder.ins().iconst(types::I64, 0);
                        let raw = builder.ins().select(cmp, one, zero_val);
                        self.define_register(builder, *dest, RawKind::Bool, raw)?;
                    } else {
                        let zero_val = builder.ins().iconst(types::I64, 0);
                        self.define_register(builder, *dest, RawKind::Bool, zero_val)?;
                    }
                } else {
                    let val_raw = self.reg_value(builder, *value)?;
                    let val_owned = if value_kind == RawKind::OwnedVal {
                        val_raw
                    } else {
                        promote_to_owned(builder, &self.helper_refs, value_kind, val_raw)?
                    };
                    let type_owned = if matches!(type_kind, RawKind::OwnedVal) {
                        self.reg_value(builder, *type_path)?
                    } else if matches!(type_kind, RawKind::StringConst | RawKind::TypeTag) {
                        materialize_string_const_for_trampoline(
                            self.program,
                            instructions,
                            *type_path,
                            builder,
                        )
                    } else {
                        let type_raw = self.reg_value(builder, *type_path)?;
                        promote_to_owned(builder, &self.helper_refs, type_kind, type_raw)?
                    };
                    let call = builder
                        .ins()
                        .call(self.helper_refs.is_type_check, &[val_owned, type_owned]);
                    let result = builder.inst_results(call)[0];
                    self.define_register(builder, *dest, RawKind::Bool, result)?;
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::StrEndsWith {
                dest,
                string,
                suffix,
            } => {
                let string_kind = self.register_kind(*string)?;
                let suffix_kind = self.register_kind(*suffix)?;
                if string_kind == RawKind::TypeTag && suffix_kind == RawKind::StringConst {
                    let raw = builder.ins().iconst(types::I64, 0);
                    self.define_register(builder, *dest, RawKind::Bool, raw)?;
                } else if string_kind == RawKind::OwnedVal || suffix_kind == RawKind::OwnedVal {
                    let to_owned =
                        |b: &mut FunctionBuilder<'_>,
                         kind: RawKind,
                         raw: cranelift_codegen::ir::Value,
                         refs: &JitHelperRefs|
                         -> Result<cranelift_codegen::ir::Value, String> {
                            match kind {
                                RawKind::OwnedVal => Ok(raw),
                                RawKind::Int | RawKind::Bool | RawKind::Dec | RawKind::Null => {
                                    promote_to_owned(b, refs, kind, raw)
                                }
                                RawKind::TypeTag | RawKind::StringConst => Err(format!(
                                    "JIT StrEndsWith cannot promote {:?} to OwnedVal",
                                    kind
                                )),
                            }
                        };
                    let str_raw = self.reg_value(builder, *string)?;
                    let str_owned = to_owned(builder, string_kind, str_raw, &self.helper_refs)?;
                    let suffix_raw = self.reg_value(builder, *suffix)?;
                    let suffix_owned =
                        to_owned(builder, suffix_kind, suffix_raw, &self.helper_refs)?;
                    let call = builder
                        .ins()
                        .call(self.helper_refs.str_ends_with, &[str_owned, suffix_owned]);
                    let result = builder.inst_results(call)[0];
                    self.define_register(builder, *dest, RawKind::Bool, result)?;
                } else {
                    return Err(format!(
                        "JIT StrEndsWith unsupported operand kinds: {:?}, {:?}",
                        string_kind, suffix_kind
                    ));
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Jump { offset } => {
                let target = jump_target(&self.blocks, *offset)?;
                builder.ins().jump(target, &[]);
                Ok(EmitResult::Handled)
            }
            Instruction::JumpIf { condition, offset } => {
                let cond = truthy_value(
                    builder,
                    &self.helper_refs,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *condition,
                )?;
                let target = jump_target(&self.blocks, *offset)?;
                let next = next_block(&self.blocks, ip)?;
                builder.ins().brif(cond, target, &[], next, &[]);
                Ok(EmitResult::Handled)
            }
            Instruction::JumpIfNot { condition, offset } => {
                let cond = truthy_value(
                    builder,
                    &self.helper_refs,
                    &self.remap,
                    &self.registers,
                    &self.register_kinds,
                    *condition,
                )?;
                let target = jump_target(&self.blocks, *offset)?;
                let next = next_block(&self.blocks, ip)?;
                builder.ins().brif(cond, next, &[], target, &[]);
                Ok(EmitResult::Handled)
            }
            Instruction::EnterScope { .. } => {
                self.scope_snapshots.push(self.locals.clone());
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::ExitScope => {
                self.locals = self
                    .scope_snapshots
                    .pop()
                    .ok_or_else(|| "JIT ExitScope without matching EnterScope".to_string())?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::Call {
                dest,
                function: fn_reg,
                args_start,
                args_count,
            } => {
                self.emit_call(
                    builder,
                    ip,
                    *dest,
                    *fn_reg,
                    *args_start,
                    *args_count,
                    instructions,
                )?;
                Ok(EmitResult::Handled)
            }
            Instruction::CallUserFunction {
                dest,
                function_id: called_function_id,
                args_start,
                args_count,
            } => {
                if self.emit_call_user_function(
                    builder,
                    ip,
                    *dest,
                    *called_function_id,
                    *args_start,
                    *args_count,
                    instructions,
                )? {
                    Ok(EmitResult::Handled)
                } else {
                    Ok(EmitResult::Unhandled)
                }
            }
            Instruction::MakeVec {
                dest,
                elements_start,
                count,
            } => {
                let count_usize = *count as usize;
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (count_usize * 2 * 8) as u32,
                    3,
                ));
                for i in 0..count_usize {
                    let reg = elements_start + i as u32;
                    let (tag, raw) = encode_helper_val_operand(
                        builder,
                        &self.remap,
                        &self.registers,
                        &self.register_kinds,
                        reg,
                    )?;
                    builder.ins().stack_store(tag, slot, (i * 2 * 8) as i32);
                    builder.ins().stack_store(raw, slot, (i * 2 * 8 + 8) as i32);
                }
                let args_ptr = builder.ins().stack_addr(self.ptr_ty, slot, 0);
                let count_val = builder.ins().iconst(types::I64, count_usize as i64);
                let call = builder
                    .ins()
                    .call(self.helper_refs.make_vec, &[args_ptr, count_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::CallLibBuiltin {
                dest,
                function,
                args,
            } => {
                let fn_kind = register_kind(&self.remap, &self.register_kinds, *function)?;
                let fn_raw = builder.use_var(self.registers[remap_reg(&self.remap, *function)?]);
                let fn_val = if fn_kind == RawKind::OwnedVal {
                    fn_raw
                } else {
                    let (tag, raw) = encode_helper_val_operand(
                        builder,
                        &self.remap,
                        &self.registers,
                        &self.register_kinds,
                        *function,
                    )?;
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    builder.ins().stack_store(tag, slot, 0);
                    builder.ins().stack_store(raw, slot, 8);
                    let ptr = builder.ins().stack_addr(self.ptr_ty, slot, 0);
                    let one = builder.ins().iconst(types::I64, 1);
                    let call = builder.ins().call(self.helper_refs.make_vec, &[ptr, one]);
                    builder.inst_results(call)[0]
                };
                let args_raw = builder.use_var(self.registers[remap_reg(&self.remap, *args)?]);
                let call = builder
                    .ins()
                    .call(self.helper_refs.call_lib, &[fn_val, args_raw]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DotAccess {
                dest,
                object,
                property,
            } => {
                let obj_kind = register_kind(&self.remap, &self.register_kinds, *object)?;
                let obj_raw = builder.use_var(self.registers[remap_reg(&self.remap, *object)?]);
                let obj_val = if obj_kind == RawKind::OwnedVal {
                    obj_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, obj_kind, obj_raw)?
                };
                let prop_name = constant_string(self.program, *property)?;
                let prop_val = new_owned_val(Val::Str(prop_name.to_string().into()));
                let prop_const = builder.ins().iconst(types::I64, prop_val);
                if let Ok(idx) = self.remap_reg(*dest) {
                    self.compile_time_owned.insert(idx);
                }
                let call = builder
                    .ins()
                    .call(self.helper_refs.dot_access, &[obj_val, prop_const]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::MergeMaps { dest, source } => {
                let dest_kind = register_kind(&self.remap, &self.register_kinds, *dest)?;
                let dest_raw = builder.use_var(self.registers[remap_reg(&self.remap, *dest)?]);
                let dest_val = if dest_kind == RawKind::OwnedVal {
                    dest_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, dest_kind, dest_raw)?
                };
                let src_kind = register_kind(&self.remap, &self.register_kinds, *source)?;
                let src_raw = builder.use_var(self.registers[remap_reg(&self.remap, *source)?]);
                let src_val = if src_kind == RawKind::OwnedVal {
                    src_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, src_kind, src_raw)?
                };
                let call = builder
                    .ins()
                    .call(self.helper_refs.merge_maps, &[dest_val, src_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::TemplateInterpolate {
                dest,
                parts_start,
                parts_count,
            } => {
                let count = *parts_count as usize;
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (count * 2 * 8) as u32,
                    3,
                ));
                for i in 0..count {
                    let reg = parts_start + i as u32;
                    let (tag, raw) = encode_helper_val_operand(
                        builder,
                        &self.remap,
                        &self.registers,
                        &self.register_kinds,
                        reg,
                    )?;
                    builder.ins().stack_store(tag, slot, (i * 2 * 8) as i32);
                    builder.ins().stack_store(raw, slot, (i * 2 * 8 + 8) as i32);
                }
                let parts_ptr = builder.ins().stack_addr(self.ptr_ty, slot, 0);
                let count_val = builder.ins().iconst(types::I64, count as i64);
                let call = builder.ins().call(
                    self.helper_refs.template_interpolate,
                    &[parts_ptr, count_val],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::ExtractInnerVal { dest, src } => {
                let src_kind = self.register_kind(*src)?;
                let src_raw = self.reg_value(builder, *src)?;
                let src_val = if src_kind == RawKind::OwnedVal {
                    src_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, src_kind, src_raw)?
                };
                let call = builder
                    .ins()
                    .call(self.helper_refs.extract_inner_val, &[src_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::EnsureResult { dest, value } => {
                let val_kind = self.register_kind(*value)?;
                let val_raw = self.reg_value(builder, *value)?;
                let val_ptr = if val_kind == RawKind::OwnedVal {
                    val_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                let call = builder
                    .ins()
                    .call(self.helper_refs.ensure_result, &[val_ptr]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::SetElement {
                collection,
                index,
                value,
            } => {
                let coll_kind = register_kind(&self.remap, &self.register_kinds, *collection)?;
                let coll_raw =
                    builder.use_var(self.registers[remap_reg(&self.remap, *collection)?]);
                let coll_val = if coll_kind == RawKind::OwnedVal {
                    coll_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, coll_kind, coll_raw)?
                };
                let idx_kind = register_kind(&self.remap, &self.register_kinds, *index)?;
                let idx_raw = builder.use_var(self.registers[remap_reg(&self.remap, *index)?]);
                let idx_val = if idx_kind == RawKind::OwnedVal {
                    idx_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, idx_kind, idx_raw)?
                };
                let val_kind = register_kind(&self.remap, &self.register_kinds, *value)?;
                let val_raw = builder.use_var(self.registers[remap_reg(&self.remap, *value)?]);
                let val_owned = if val_kind == RawKind::OwnedVal {
                    val_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                let call = builder.ins().call(
                    self.helper_refs.set_element,
                    &[coll_val, idx_val, val_owned],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *collection, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::ConstructTyped {
                dest,
                src,
                type_info,
            } => {
                let src_kind = self.register_kind(*src)?;
                let src_raw = self.reg_value(builder, *src)?;
                let src_val = if src_kind == RawKind::OwnedVal {
                    src_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, src_kind, src_raw)?
                };
                let type_info_val = match self.program.constants.get(*type_info as usize) {
                    Some(Constant::Val(v)) => new_owned_val(v.clone()),
                    _ => {
                        return Err(format!(
                            "ConstructTyped: invalid type_info constant {}",
                            type_info
                        ));
                    }
                };
                let type_info_raw = builder.ins().iconst(types::I64, type_info_val);
                let call = builder
                    .ins()
                    .call(self.helper_refs.construct_typed, &[src_val, type_info_raw]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::RegisterLocalType { .. }
            | Instruction::RegisterLocalImplementation { .. } => {
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::LoadVarOrDefault {
                dest,
                var_name,
                default_value,
            } => {
                let name = constant_string(self.program, *var_name)?;
                if let Some(&(var, kind)) = self.locals.get(name) {
                    let mut value = builder.use_var(var);
                    if kind == RawKind::OwnedVal {
                        value = clone_owned_raw(builder, &self.helper_refs, value);
                    }
                    self.define_register(builder, *dest, kind, value)?;
                } else {
                    let name_val = new_owned_val(Val::Str(name.to_string().into()));
                    let name_raw = builder.ins().iconst(types::I64, name_val);
                    let call = builder
                        .ins()
                        .call(self.helper_refs.lookup_var_or_default, &[name_raw]);
                    let resolved = builder.inst_results(call)[0];
                    let is_found = builder.ins().icmp_imm(
                        cranelift_codegen::ir::condcodes::IntCC::NotEqual,
                        resolved,
                        0,
                    );
                    let default_val = match self.program.constants.get(*default_value as usize) {
                        Some(Constant::Val(v)) => new_owned_val(v.clone()),
                        _ => new_owned_val(Val::Null),
                    };
                    let default_raw = builder.ins().iconst(types::I64, default_val);
                    let result = builder.ins().select(is_found, resolved, default_raw);
                    self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                    if let Ok(idx) = self.remap_reg(*dest) {
                        self.compile_time_owned.insert(idx);
                    }
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::LoadTypeRef { dest, type_ref } => {
                let val = match self.program.constants.get(*type_ref as usize) {
                    Some(Constant::Val(v)) => new_owned_val(v.clone()),
                    _ => new_owned_val(Val::Null),
                };
                let value = builder.ins().iconst(types::I64, val);
                self.define_register(builder, *dest, RawKind::OwnedVal, value)?;
                if let Ok(idx) = self.remap_reg(*dest) {
                    self.compile_time_owned.insert(idx);
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DotAccessOrDefault {
                dest,
                object,
                property,
                default_value,
            } => {
                let obj_kind = self.register_kind(*object)?;
                let obj_raw = self.reg_value(builder, *object)?;
                let obj_ptr = if obj_kind == RawKind::OwnedVal {
                    obj_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, obj_kind, obj_raw)?
                };
                let prop = constant_string(self.program, *property)?;
                let prop_val = new_owned_val(Val::Str(prop.to_string().into()));
                let prop_ptr = builder.ins().iconst(types::I64, prop_val);
                let call = builder
                    .ins()
                    .call(self.helper_refs.dot_access, &[obj_ptr, prop_ptr]);
                let result = builder.inst_results(call)[0];
                let null_sentinel = builder.ins().iconst(types::I64, 0);
                let is_null = builder.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    result,
                    null_sentinel,
                );
                let merge_var = builder.declare_var(types::I64);
                let use_result_block = builder.create_block();
                let use_default_block = builder.create_block();
                let merge_block = builder.create_block();
                builder
                    .ins()
                    .brif(is_null, use_default_block, &[], use_result_block, &[]);

                builder.switch_to_block(use_result_block);
                builder.seal_block(use_result_block);
                builder.def_var(merge_var, result);
                builder.ins().jump(merge_block, &[]);

                builder.switch_to_block(use_default_block);
                builder.seal_block(use_default_block);
                let default_kind = self.register_kind(*default_value)?;
                let default_raw = self.reg_value(builder, *default_value)?;
                let default_ptr = if default_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &self.helper_refs, default_raw)
                } else {
                    promote_to_owned(builder, &self.helper_refs, default_kind, default_raw)?
                };
                builder.def_var(merge_var, default_ptr);
                builder.ins().jump(merge_block, &[]);

                builder.switch_to_block(merge_block);
                builder.seal_block(merge_block);
                let merged = builder.use_var(merge_var);
                self.define_register(builder, *dest, RawKind::OwnedVal, merged)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::VecAppend { vec, value } => {
                let vec_kind = self.register_kind(*vec)?;
                let vec_raw = self.reg_value(builder, *vec)?;
                let vec_ptr = if vec_kind == RawKind::OwnedVal {
                    vec_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, vec_kind, vec_raw)?
                };
                let val_kind = self.register_kind(*value)?;
                let val_raw = self.reg_value(builder, *value)?;
                let val_ptr = if val_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &self.helper_refs, val_raw)
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                let call = builder
                    .ins()
                    .call(self.helper_refs.vec_push, &[vec_ptr, val_ptr]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *vec, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::CallLambda {
                dest,
                lambda,
                args_start,
                args_count,
            } => {
                self.emit_call(
                    builder,
                    ip,
                    *dest,
                    *lambda,
                    *args_start,
                    *args_count,
                    instructions,
                )?;
                Ok(EmitResult::Handled)
            }
            Instruction::Pipe { .. } => Ok(EmitResult::Unhandled),
            Instruction::BeginErrorCapture => {
                self.error_capture_active = true;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::EndErrorCapture => {
                self.error_capture_active = false;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::WrapOk { dest, src } => {
                let src_kind = self.register_kind(*src)?;
                let src_raw = self.reg_value(builder, *src)?;
                let src_ptr = if src_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &self.helper_refs, src_raw)
                } else {
                    promote_to_owned(builder, &self.helper_refs, src_kind, src_raw)?
                };
                let ok_key = new_owned_val(Val::Str("ok".into()));
                let key_ptr = builder.ins().iconst(types::I64, ok_key);
                let empty_map = new_owned_val(Val::Map(Box::new(indexmap::IndexMap::new())));
                let map_ptr = builder.ins().iconst(types::I64, empty_map);
                if let Ok(idx) = self.remap_reg(*dest) {
                    self.compile_time_owned.insert(idx);
                }
                let call = builder
                    .ins()
                    .call(self.helper_refs.set_element, &[map_ptr, key_ptr, src_ptr]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::CallWithSpread {
                dest,
                function,
                args_start,
                args_count,
                spread_mask,
            } => {
                let fn_kind = self.register_kind(*function)?;
                let fn_raw = self.reg_value(builder, *function)?;
                let fn_ptr = if fn_kind == RawKind::OwnedVal {
                    fn_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, fn_kind, fn_raw)?
                };
                let count = usize::from(*args_count);
                let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (count * 2 * 8) as u32,
                    3,
                ));
                for i in 0..count {
                    let reg = *args_start + i as u32;
                    let kind = self.register_kind(reg)?;
                    let raw = self.reg_value(builder, reg)?;
                    let (eff_kind, eff_raw) = normalize_for_trampoline(builder, kind, raw);
                    let kind_val = builder
                        .ins()
                        .iconst(types::I64, helper_value_kind(eff_kind));
                    builder
                        .ins()
                        .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
                    let raw_val = if eff_kind == RawKind::OwnedVal {
                        clone_owned_raw(builder, &self.helper_refs, eff_raw)
                    } else {
                        eff_raw
                    };
                    builder
                        .ins()
                        .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
                }
                let args_buf = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
                let count_val = builder.ins().iconst(types::I64, count as i64);
                let mask_val = builder.ins().iconst(types::I64, *spread_mask as i64);
                let call = builder.ins().call(
                    self.helper_refs.call_with_spread,
                    &[fn_ptr, args_buf, count_val, mask_val],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::StrStartsWith {
                dest,
                string,
                prefix,
            } => {
                let string_kind = self.register_kind(*string)?;
                let prefix_kind = self.register_kind(*prefix)?;
                if string_kind == RawKind::TypeTag && prefix_kind == RawKind::StringConst {
                    let raw = builder.ins().iconst(types::I64, 0);
                    self.define_register(builder, *dest, RawKind::Bool, raw)?;
                } else if string_kind == RawKind::OwnedVal || prefix_kind == RawKind::OwnedVal {
                    let to_owned =
                        |b: &mut FunctionBuilder<'_>,
                         kind: RawKind,
                         raw: cranelift_codegen::ir::Value,
                         refs: &JitHelperRefs|
                         -> Result<cranelift_codegen::ir::Value, String> {
                            match kind {
                                RawKind::OwnedVal => Ok(raw),
                                RawKind::Int | RawKind::Bool | RawKind::Dec | RawKind::Null => {
                                    promote_to_owned(b, refs, kind, raw)
                                }
                                RawKind::TypeTag | RawKind::StringConst => Err(format!(
                                    "JIT StrStartsWith cannot promote {:?} to OwnedVal",
                                    kind
                                )),
                            }
                        };
                    let str_raw = self.reg_value(builder, *string)?;
                    let prefix_raw = self.reg_value(builder, *prefix)?;
                    let str_ptr = to_owned(builder, string_kind, str_raw, &self.helper_refs)?;
                    let prefix_ptr = to_owned(builder, prefix_kind, prefix_raw, &self.helper_refs)?;
                    let call = builder
                        .ins()
                        .call(self.helper_refs.str_starts_with, &[str_ptr, prefix_ptr]);
                    let result = builder.inst_results(call)[0];
                    self.define_register(builder, *dest, RawKind::Bool, result)?;
                } else {
                    let raw = builder.ins().iconst(types::I64, 0);
                    self.define_register(builder, *dest, RawKind::Bool, raw)?;
                }
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DynamicDotAccess {
                dest,
                object,
                property,
            } => {
                let obj_kind = self.register_kind(*object)?;
                let obj_raw = self.reg_value(builder, *object)?;
                let obj_ptr = if obj_kind == RawKind::OwnedVal {
                    obj_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, obj_kind, obj_raw)?
                };
                let prop_kind = self.register_kind(*property)?;
                let prop_raw = self.reg_value(builder, *property)?;
                let prop_ptr = if prop_kind == RawKind::OwnedVal {
                    prop_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, prop_kind, prop_raw)?
                };
                if let Ok(idx) = self.remap_reg(*dest) {
                    self.compile_time_owned.insert(idx);
                }
                let call = builder
                    .ins()
                    .call(self.helper_refs.dynamic_dot_access, &[obj_ptr, prop_ptr]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DynamicDotSet {
                object,
                property,
                value,
            } => {
                let obj_kind = self.register_kind(*object)?;
                let obj_raw = self.reg_value(builder, *object)?;
                let obj_ptr = if obj_kind == RawKind::OwnedVal {
                    obj_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, obj_kind, obj_raw)?
                };
                let prop_kind = self.register_kind(*property)?;
                let prop_raw = self.reg_value(builder, *property)?;
                let prop_ptr = if prop_kind == RawKind::OwnedVal {
                    prop_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, prop_kind, prop_raw)?
                };
                let val_kind = self.register_kind(*value)?;
                let val_raw = self.reg_value(builder, *value)?;
                let val_ptr = if val_kind == RawKind::OwnedVal {
                    val_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                let call = builder.ins().call(
                    self.helper_refs.dynamic_dot_set,
                    &[obj_ptr, prop_ptr, val_ptr],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *object, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DotSet {
                object,
                property,
                value,
            } => {
                let obj_kind = self.register_kind(*object)?;
                let obj_raw = self.reg_value(builder, *object)?;
                let obj_ptr = if obj_kind == RawKind::OwnedVal {
                    obj_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, obj_kind, obj_raw)?
                };
                let prop_name = constant_string(self.program, *property)?;
                let prop_val = new_owned_val(Val::Str(prop_name.to_string().into()));
                let prop_ptr = builder.ins().iconst(types::I64, prop_val);
                let val_kind = self.register_kind(*value)?;
                let val_raw = self.reg_value(builder, *value)?;
                let val_ptr = if val_kind == RawKind::OwnedVal {
                    val_raw
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                let call = builder
                    .ins()
                    .call(self.helper_refs.dot_set, &[obj_ptr, prop_ptr, val_ptr]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *object, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::MakeVecWithSpread {
                dest,
                elements_start,
                count,
                spread_mask,
            } => {
                let n = *count as usize;
                let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (n * 2 * 8) as u32,
                    3,
                ));
                for i in 0..n {
                    let reg = *elements_start + i as u32;
                    let kind = self.register_kind(reg)?;
                    let raw = self.reg_value(builder, reg)?;
                    let (eff_kind, eff_raw) = normalize_for_trampoline(builder, kind, raw);
                    let kind_val = builder
                        .ins()
                        .iconst(types::I64, helper_value_kind(eff_kind));
                    builder
                        .ins()
                        .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
                    let raw_val = if eff_kind == RawKind::OwnedVal {
                        clone_owned_raw(builder, &self.helper_refs, eff_raw)
                    } else {
                        eff_raw
                    };
                    builder
                        .ins()
                        .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
                }
                let args_buf = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
                let count_val = builder.ins().iconst(types::I64, n as i64);
                let mask_val = builder.ins().iconst(types::I64, *spread_mask as i64);
                let call = builder.ins().call(
                    self.helper_refs.make_vec_with_spread,
                    &[args_buf, count_val, mask_val],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::StoreGlobal {
                namespace,
                var_name,
                value,
            } => {
                let ns_name = constant_string(self.program, *namespace)?;
                let ns_val = new_owned_val(Val::Str(ns_name.to_string().into()));
                let ns_ptr = builder.ins().iconst(types::I64, ns_val);
                let name = constant_string(self.program, *var_name)?;
                let name_val = new_owned_val(Val::Str(name.to_string().into()));
                let name_ptr = builder.ins().iconst(types::I64, name_val);
                let val_kind = self.register_kind(*value)?;
                let val_raw = self.reg_value(builder, *value)?;
                let val_ptr = if val_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &self.helper_refs, val_raw)
                } else {
                    promote_to_owned(builder, &self.helper_refs, val_kind, val_raw)?
                };
                builder
                    .ins()
                    .call(self.helper_refs.store_global, &[ns_ptr, name_ptr, val_ptr]);
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::SetNamespace { namespace } => {
                let ns_name = constant_string(self.program, *namespace)?;
                let ns_val = new_owned_val(Val::Str(ns_name.to_string().into()));
                let ns_ptr = builder.ins().iconst(types::I64, ns_val);
                builder
                    .ins()
                    .call(self.helper_refs.set_namespace, &[ns_ptr]);
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::CallNative {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let count = *args_count as usize;
                let pairs_slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (count * 2 * 8) as u32,
                    3,
                ));
                for i in 0..count {
                    let reg = *args_start + i as u32;
                    let kind = self.register_kind(reg)?;
                    let raw = self.reg_value(builder, reg)?;
                    let (eff_kind, eff_raw) = normalize_for_trampoline(builder, kind, raw);
                    let kind_val = builder
                        .ins()
                        .iconst(types::I64, helper_value_kind(eff_kind));
                    builder
                        .ins()
                        .stack_store(kind_val, pairs_slot, (i * 2 * 8) as i32);
                    let raw_val = if eff_kind == RawKind::OwnedVal {
                        clone_owned_raw(builder, &self.helper_refs, eff_raw)
                    } else {
                        eff_raw
                    };
                    builder
                        .ins()
                        .stack_store(raw_val, pairs_slot, (i * 2 * 8 + 8) as i32);
                }
                let args_buf = builder.ins().stack_addr(self.ptr_ty, pairs_slot, 0);
                let func_id_val = builder.ins().iconst(types::I64, *function as i64);
                let count_val = builder.ins().iconst(types::I64, count as i64);
                let call = builder.ins().call(
                    self.helper_refs.call_native,
                    &[func_id_val, args_buf, count_val],
                );
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::DefineFunction { dest, function_id } => {
                let fid_val = builder.ins().iconst(types::I64, *function_id as i64);
                let call = builder
                    .ins()
                    .call(self.helper_refs.define_function, &[fid_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::LoadScoped {
                dest,
                var_name,
                scope_depth,
            } => {
                let name = constant_string(self.program, *var_name)?;
                let name_val = new_owned_val(Val::Str(name.to_string().into()));
                let name_ptr = builder.ins().iconst(types::I64, name_val);
                let depth_val = builder.ins().iconst(types::I64, *scope_depth as i64);
                let call = builder
                    .ins()
                    .call(self.helper_refs.load_scoped, &[name_ptr, depth_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            Instruction::CaptureVar {
                dest,
                var_name,
                scope_depth,
            } => {
                let name = constant_string(self.program, *var_name)?;
                let name_val = new_owned_val(Val::Str(name.to_string().into()));
                let name_ptr = builder.ins().iconst(types::I64, name_val);
                let depth_val = builder.ins().iconst(types::I64, *scope_depth as i64);
                let call = builder
                    .ins()
                    .call(self.helper_refs.capture_var, &[name_ptr, depth_val]);
                let result = builder.inst_results(call)[0];
                self.define_register(builder, *dest, RawKind::OwnedVal, result)?;
                self.jump_to_next(builder, ip)?;
                Ok(EmitResult::Handled)
            }
            // Flow-level instructions (BeginFlow, EndFlow, CondBranch*, Pipe,
            // DeferredVarExpr, Return, ReturnIfErr, TailCall) are handled by the
            // second-phase dispatch in build_supported_body. Unrecognized instructions
            // also flow through here and cause a bail-out in that second phase.
            _ => Ok(EmitResult::Unhandled),
        }
    }

    fn emit_deferred_var_expr(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        ip: usize,
        var_name: u32,
        thunk: u32,
        accumulator_var: Option<Variable>,
        flow_result_reg: Option<u32>,
    ) -> Result<EmitResult, String> {
        let thunk_kind = self.register_kind(thunk)?;
        let thunk_raw = self.reg_value(builder, thunk)?;
        let thunk_ptr = if thunk_kind == RawKind::OwnedVal {
            thunk_raw
        } else {
            promote_to_owned(builder, &self.helper_refs, thunk_kind, thunk_raw)?
        };
        let zero_a = builder.ins().iconst(types::I64, 0);
        let zero_b = builder.ins().iconst(types::I64, 0);
        let call = builder.ins().call(
            self.helper_refs.call_vm_by_name,
            &[thunk_ptr, zero_a, zero_b],
        );
        let result = builder.inst_results(call)[0];

        if let Some(acc_var) = accumulator_var {
            let name_str = constant_string(self.program, var_name)?;
            let key_val = new_owned_val(Val::Str(name_str.to_string().into()));
            let key_raw = builder.ins().iconst(types::I64, key_val);
            let acc_val = builder.use_var(acc_var);
            let call = builder
                .ins()
                .call(self.helper_refs.set_element, &[acc_val, key_raw, result]);
            let new_acc = builder.inst_results(call)[0];
            builder.def_var(acc_var, new_acc);
        } else if let Some(reg) = flow_result_reg {
            self.define_register(builder, reg, RawKind::OwnedVal, result)?;
        }

        self.jump_to_next(builder, ip)?;
        Ok(EmitResult::Handled)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_supported_body(
    builder: &mut FunctionBuilder<'_>,
    program: &BytecodeProgram,
    function_id: FunctionId,
    function_info: &FunctionInfo,
    signature: &TypeSig,
    ptr_ty: Type,
    self_func_ref: FuncRef,
    helper_refs: JitHelperRefs,
    expected_return_kind: Option<JitValueKind>,
) -> Result<JitValueKind, String> {
    let instructions = &function_info.instructions;
    if instructions.is_empty() {
        return Err("Cannot JIT empty function".to_string());
    }

    let used_regs = collect_used_registers(instructions);
    let remap: AHashMap<u32, usize> = used_regs
        .iter()
        .enumerate()
        .map(|(idx, &reg)| (reg, idx))
        .collect();
    let register_count = remap.len();

    let entry_block = builder.create_block();
    let blocks: Vec<_> = (0..instructions.len())
        .map(|_| builder.create_block())
        .collect();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);

    let zero = builder.ins().iconst(types::I64, 0);
    let mut registers = Vec::with_capacity(register_count);
    let register_kinds = vec![None; register_count];

    for _ in 0..register_count {
        let var = builder.declare_var(types::I64);
        builder.def_var(var, zero);
        registers.push(var);
    }

    let arg_ptr = builder.block_params(entry_block)[0];
    let ret_ptr = builder.block_params(entry_block)[1];
    let arg_layout = AbiLayout::for_args(&signature.args)?;
    let mut locals: AHashMap<String, (Variable, RawKind)> = AHashMap::new();
    let mut param_locals: Vec<(Variable, RawKind)> =
        Vec::with_capacity(function_info.param_names.len());
    for (idx, param_name) in function_info.param_names.iter().enumerate() {
        let tag = signature
            .args
            .get(idx)
            .ok_or_else(|| "Missing JIT signature arg".to_string())?;
        let kind = tag
            .raw_kind()
            .ok_or_else(|| format!("Unsupported parameter type for JIT: {:?}", tag))?;
        let var = builder.declare_var(types::I64);
        let offset = arg_layout.offset(idx)? as i64;
        let value = match kind {
            RawKind::Int | RawKind::Bool => {
                builder
                    .ins()
                    .load(types::I64, MemFlags::new(), arg_ptr, offset as i32)
            }
            RawKind::Null => builder.ins().iconst(types::I64, 0),
            RawKind::OwnedVal => {
                builder
                    .ins()
                    .load(types::I64, MemFlags::new(), arg_ptr, offset as i32)
            }
            RawKind::Dec => {
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    mem::size_of::<D256>() as u32,
                    3,
                ));
                let slot_ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
                let src_ptr = builder.ins().iadd_imm(arg_ptr, offset);
                builder
                    .ins()
                    .call(helper_refs.copy_dec, &[src_ptr, slot_ptr]);
                slot_ptr
            }
            RawKind::TypeTag | RawKind::StringConst => {
                return Err("Internal JIT token parameters are not supported".to_string());
            }
        };
        builder.def_var(var, value);
        locals.insert(param_name.clone(), (var, kind));
        param_locals.push((var, kind));
    }

    let mut ctx = EmitCtx {
        program,
        function_id,
        signature,
        ptr_ty,
        self_func_ref,
        helper_refs,
        arg_layout,
        expected_return_kind,
        blocks,
        remap,
        registers,
        register_kinds,
        compile_time_owned: std::collections::HashSet::new(),
        locals,
        thunk_registry: AHashMap::new(),
        scope_snapshots: Vec::new(),
        error_capture_active: false,
    };

    // Stack guard: compare a stack slot address (proxy for current SP) against
    // the thread-local stack limit retrieved via helper call.
    let guard_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 0));
    let sp_proxy = builder.ins().stack_addr(ptr_ty, guard_slot, 0);
    let limit_call = builder.ins().call(helper_refs.get_stack_limit, &[]);
    let limit_val = builder.inst_results(limit_call)[0];
    let stack_ok = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::UnsignedGreaterThan,
        sp_proxy,
        limit_val,
    );
    let body_block = builder.create_block();
    let overflow_block = builder.create_block();
    builder
        .ins()
        .brif(stack_ok, body_block, &[], overflow_block, &[]);

    builder.switch_to_block(overflow_block);
    builder.seal_block(overflow_block);
    let sentinel = builder.ins().iconst(types::I64, JIT_STACK_EXHAUSTED);
    builder.ins().return_(&[sentinel]);

    builder.switch_to_block(body_block);
    builder.seal_block(body_block);
    builder.ins().jump(ctx.blocks[0], &[]);

    let mut active_flows: Vec<ActiveFlowState> = Vec::new();
    let mut return_kind: Option<JitValueKind> = None;

    for (ip, instruction) in instructions.iter().enumerate() {
        builder.switch_to_block(ctx.blocks[ip]);

        match ctx.emit_instruction(builder, ip, instruction, instructions)? {
            EmitResult::Handled => {
                continue;
            }
            EmitResult::Unhandled => {}
        }

        match instruction {
            Instruction::BeginFlow {
                flow_type,
                result_modifier,
                ..
            } => {
                let flow_type = *flow_type;
                let result_modifier = *result_modifier;
                if !matches!(
                    flow_type,
                    FlowType::Cond
                        | FlowType::Match
                        | FlowType::CondAll
                        | FlowType::MatchAll
                        | FlowType::Serial
                        | FlowType::Pipe
                        | FlowType::Parallel
                ) {
                    return Err(format!("Unsupported flow type for JIT: {:?}", flow_type));
                }
                let pre_flow_owned: Vec<usize> = ctx
                    .register_kinds
                    .iter()
                    .enumerate()
                    .filter(|(idx, k)| {
                        **k == Some(RawKind::OwnedVal) && !ctx.compile_time_owned.contains(idx)
                    })
                    .map(|(idx, _)| idx)
                    .collect();
                let accumulator_var = if matches!(
                    result_modifier,
                    FlowResultModifier::Map | FlowResultModifier::Vec
                ) {
                    let empty_val = match result_modifier {
                        FlowResultModifier::Map => {
                            new_owned_val(Val::Map(Box::new(indexmap::IndexMap::new())))
                        }
                        FlowResultModifier::Vec => new_owned_val(Val::Vec(Vec::new())),
                        FlowResultModifier::One => unreachable!("guarded by outer if"),
                    };
                    let init = builder.ins().iconst(types::I64, empty_val);
                    let var = builder.declare_var(types::I64);
                    builder.def_var(var, init);
                    Some(var)
                } else {
                    None
                };
                let end_ip = find_matching_end_flow(instructions, ip)?;
                let result_reg = if let Instruction::EndFlow { dest } = &instructions[end_ip] {
                    Some(*dest)
                } else {
                    None
                };
                active_flows.push(ActiveFlowState {
                    flow_type,
                    result_modifier,
                    end_ip,
                    result_reg,
                    merged_result_kind: None,
                    pre_flow_owned,
                    accumulator_var,
                    flow_header_regs: Vec::new(),
                    last_pipe_reg: None,
                });
                ctx.jump_to_next(builder, ip)?;
            }
            Instruction::CondBranchStart { condition, .. } => {
                let Some(flow) = active_flows.last_mut() else {
                    return Err("JIT CondBranchStart without active flow".to_string());
                };
                if flow.flow_header_regs.is_empty() && flow.merged_result_kind.is_none() {
                    for (idx, k) in ctx.register_kinds.iter().enumerate() {
                        if k.is_some() && !flow.pre_flow_owned.contains(&idx) {
                            flow.flow_header_regs.push(idx);
                        }
                    }
                }
                let branch_end_ip = find_matching_cond_branch_end(instructions, ip)?;
                let skip_block = next_block(&ctx.blocks, branch_end_ip)?;
                let execute_block = next_block(&ctx.blocks, ip)?;
                let cond = truthy_value(
                    builder,
                    &ctx.helper_refs,
                    &ctx.remap,
                    &ctx.registers,
                    &ctx.register_kinds,
                    *condition,
                )?;
                builder
                    .ins()
                    .brif(cond, execute_block, &[], skip_block, &[]);
            }
            Instruction::CondBranchEnd {
                result,
                branch_name,
            } => {
                let Some(flow) = active_flows.last_mut() else {
                    return Err("JIT CondBranchEnd without active flow".to_string());
                };
                let kind_opt = ctx.register_kinds[ctx.remap_reg(*result)?];
                if kind_opt.is_none() {
                    ctx.jump_to_next(builder, ip)?;
                    continue;
                }
                let mut kind = kind_opt.unwrap();
                let mut raw = ctx.reg_value(builder, *result)?;

                if let Some(acc_var) = flow.accumulator_var {
                    let result_owned = if kind == RawKind::OwnedVal {
                        clone_owned_raw(builder, &ctx.helper_refs, raw)
                    } else {
                        promote_to_owned(builder, &ctx.helper_refs, kind, raw)?
                    };
                    let acc_val = builder.use_var(acc_var);
                    let new_acc = match flow.result_modifier {
                        FlowResultModifier::Map => {
                            let branch_name_str = constant_string(ctx.program, *branch_name)?;
                            let key_val =
                                new_owned_val(Val::Str(branch_name_str.to_string().into()));
                            let key_raw = builder.ins().iconst(types::I64, key_val);
                            let call = builder.ins().call(
                                ctx.helper_refs.set_element,
                                &[acc_val, key_raw, result_owned],
                            );
                            builder.inst_results(call)[0]
                        }
                        FlowResultModifier::Vec => {
                            let call = builder
                                .ins()
                                .call(ctx.helper_refs.vec_push, &[acc_val, result_owned]);
                            builder.inst_results(call)[0]
                        }
                        FlowResultModifier::One => {
                            unreachable!(
                                "CondBranchEnd accumulator only created for Map/Vec modifiers"
                            )
                        }
                    };
                    builder.def_var(acc_var, new_acc);
                    flow.merged_result_kind = Some(RawKind::OwnedVal);
                    ctx.jump_to_next(builder, ip)?;
                } else {
                    let result_reg = flow.result_reg.ok_or_else(|| {
                        "JIT flow result register was not initialized".to_string()
                    })?;
                    if let Some(existing) = flow.merged_result_kind {
                        if existing != kind {
                            if kind != RawKind::OwnedVal {
                                raw = promote_to_owned(builder, &ctx.helper_refs, kind, raw)?;
                            }
                            kind = RawKind::OwnedVal;
                            flow.merged_result_kind = Some(RawKind::OwnedVal);
                        }
                    } else {
                        flow.merged_result_kind = Some(kind);
                    }
                    let result_reg_idx = ctx.remap_reg(result_reg)?;
                    let result_src_idx = ctx.remap_reg(*result)?;
                    for (idx, existing_kind) in ctx.register_kinds.iter().enumerate() {
                        if *existing_kind == Some(RawKind::OwnedVal)
                            && !ctx.compile_time_owned.contains(&idx)
                            && !flow.pre_flow_owned.contains(&idx)
                            && !flow.flow_header_regs.contains(&idx)
                            && idx != result_reg_idx
                            && idx != result_src_idx
                        {
                            drop_owned_var(builder, &ctx.helper_refs, ctx.registers[idx]);
                        }
                    }
                    ctx.define_register(builder, result_reg, kind, raw)?;
                    for (idx, existing_kind) in ctx.register_kinds.iter_mut().enumerate() {
                        if *existing_kind == Some(RawKind::OwnedVal)
                            && !ctx.compile_time_owned.contains(&idx)
                            && !flow.pre_flow_owned.contains(&idx)
                            && !flow.flow_header_regs.contains(&idx)
                            && idx != result_reg_idx
                        {
                            *existing_kind = None;
                        }
                    }
                    if matches!(
                        flow.flow_type,
                        FlowType::Cond | FlowType::Match | FlowType::Serial | FlowType::Pipe
                    ) {
                        let target = ctx.blocks[flow.end_ip];
                        builder.ins().jump(target, &[]);
                    } else {
                        ctx.jump_to_next(builder, ip)?;
                    }
                }
            }
            Instruction::EndFlow { dest } => {
                let flow = active_flows
                    .pop()
                    .ok_or_else(|| "JIT EndFlow without active flow".to_string())?;
                if let Some(acc_var) = flow.accumulator_var {
                    let acc_val = builder.use_var(acc_var);
                    ctx.define_register(builder, *dest, RawKind::OwnedVal, acc_val)?;
                } else if let Some(pipe_reg) = flow.last_pipe_reg {
                    let kind = ctx.register_kind(pipe_reg)?;
                    let raw = ctx.reg_value(builder, pipe_reg)?;
                    ctx.define_register(builder, *dest, kind, raw)?;
                } else {
                    let result_reg = flow.result_reg.ok_or_else(|| {
                        "JIT flow result register was not initialized".to_string()
                    })?;
                    let kind = flow
                        .merged_result_kind
                        .unwrap_or(ctx.register_kind(result_reg)?);
                    if *dest != result_reg {
                        let raw = ctx.reg_value(builder, result_reg)?;
                        ctx.define_register(builder, *dest, kind, raw)?;
                    }
                }
                ctx.jump_to_next(builder, ip)?;
            }
            Instruction::CallUserFunction {
                dest,
                args_start,
                args_count,
                ..
            } => {
                ctx.emit_self_call(builder, ip, *dest, *args_start, *args_count)?;
            }
            Instruction::TailCall {
                function_id: called_function_id,
                args_start,
                args_count,
            } => {
                if *called_function_id != function_id {
                    return Err("JIT only supports self tail calls".to_string());
                }
                if usize::from(*args_count) != param_locals.len() {
                    return Err("JIT tail-call arity mismatch".to_string());
                }

                let mut kind_mismatch = false;
                for (idx, &(_, param_kind)) in param_locals
                    .iter()
                    .enumerate()
                    .take(usize::from(*args_count))
                {
                    let reg = *args_start + idx as u32;
                    let actual_kind = ctx.register_kind(reg)?;
                    if actual_kind != param_kind {
                        kind_mismatch = true;
                        break;
                    }
                }
                if kind_mismatch {
                    let flow = active_flows
                        .last()
                        .ok_or_else(|| "JIT TailCall outside flow".to_string())?;
                    let result_reg = flow
                        .result_reg
                        .ok_or_else(|| "JIT flow result register not set".to_string())?;
                    let end_ip = flow.end_ip;
                    ctx.emit_vm_callback(
                        builder,
                        result_reg,
                        function_id,
                        *args_start,
                        *args_count,
                        instructions,
                    )?;
                    let end_target = ctx.blocks[end_ip];
                    builder.ins().jump(end_target, &[]);
                    continue;
                }
                let mut new_values = Vec::with_capacity(param_locals.len());
                for (idx, &(param_var, param_kind)) in param_locals
                    .iter()
                    .enumerate()
                    .take(usize::from(*args_count))
                {
                    let reg = *args_start + idx as u32;
                    let raw = ctx.reg_value(builder, reg)?;
                    let new_val = match param_kind {
                        RawKind::OwnedVal => clone_owned_raw(builder, &ctx.helper_refs, raw),
                        RawKind::Dec => {
                            let tmp_slot = builder.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                mem::size_of::<D256>() as u32,
                                3,
                            ));
                            let tmp_ptr = builder.ins().stack_addr(ptr_ty, tmp_slot, 0);
                            builder
                                .ins()
                                .call(ctx.helper_refs.copy_dec, &[raw, tmp_ptr]);
                            tmp_ptr
                        }
                        RawKind::Int | RawKind::Bool | RawKind::Null => raw,
                        RawKind::TypeTag | RawKind::StringConst => {
                            return Err(
                                "JIT TailCall: internal token kinds cannot be function parameters"
                                    .to_string(),
                            );
                        }
                    };
                    new_values.push((param_var, param_kind, new_val));
                }

                for &(param_var, param_kind, _) in &new_values {
                    if param_kind == RawKind::OwnedVal {
                        drop_owned_var(builder, &ctx.helper_refs, param_var);
                    }
                }

                for (param_var, _, new_val) in new_values {
                    builder.def_var(param_var, new_val);
                }
                builder.ins().jump(ctx.blocks[0], &[]);
            }
            Instruction::ReturnIfErr { src } => {
                if ctx.error_capture_active {
                    ctx.jump_to_next(builder, ip)?;
                } else {
                    match ctx.emit_err_check(builder, ip, *src)? {
                        None => {}
                        Some((error_block, error_raw)) => {
                            builder.switch_to_block(error_block);
                            builder.seal_block(error_block);
                            let error_val = clone_owned_raw(builder, &ctx.helper_refs, error_raw);
                            ctx.emit_cleanup_owned(builder);
                            builder.ins().return_(&[error_val]);
                        }
                    }
                }
            }
            Instruction::Return { value } => {
                let kind = ctx.register_kind(*value)?;
                let raw = ctx.reg_value(builder, *value)?;
                let current_return_kind = match kind {
                    RawKind::Int => JitValueKind::Int,
                    RawKind::Bool => JitValueKind::Bool,
                    RawKind::Dec => JitValueKind::Dec,
                    RawKind::Null => JitValueKind::Null,
                    RawKind::OwnedVal => JitValueKind::OwnedVal,
                    RawKind::TypeTag | RawKind::StringConst => {
                        return Err("JIT cannot return internal type/string tokens".to_string());
                    }
                };
                if let Some(existing) = return_kind {
                    if existing != current_return_kind {
                        return Err("JIT function has mixed return kinds".to_string());
                    }
                } else {
                    return_kind = Some(current_return_kind);
                }
                match kind {
                    RawKind::Int | RawKind::Bool => {
                        builder.ins().store(MemFlags::new(), raw, ret_ptr, 0);
                    }
                    RawKind::Dec => {
                        builder
                            .ins()
                            .call(ctx.helper_refs.copy_dec, &[raw, ret_ptr]);
                    }
                    RawKind::Null => {
                        let zero_val = builder.ins().iconst(types::I64, 0);
                        builder.ins().store(MemFlags::new(), zero_val, ret_ptr, 0);
                    }
                    RawKind::OwnedVal => {
                        let returned = clone_owned_raw(builder, &ctx.helper_refs, raw);
                        builder.ins().store(MemFlags::new(), returned, ret_ptr, 0);
                    }
                    RawKind::TypeTag | RawKind::StringConst => {
                        return Err(
                            "JIT cannot persist internal type/string tokens as return values"
                                .to_string(),
                        );
                    }
                }
                ctx.emit_cleanup_owned(builder);
                let zero_ret = builder.ins().iconst(types::I64, 0);
                builder.ins().return_(&[zero_ret]);
            }
            Instruction::Pipe { dest, src } => {
                let kind = ctx.register_kind(*src)?;
                let raw = ctx.reg_value(builder, *src)?;
                ctx.define_register(builder, *dest, kind, raw)?;

                if let Some(flow) = active_flows.last_mut()
                    && flow.flow_type == FlowType::Pipe
                {
                    flow.last_pipe_reg = Some(*dest);
                    if let Some(acc_var) = flow.accumulator_var {
                        let result_owned = if kind == RawKind::OwnedVal {
                            clone_owned_raw(builder, &ctx.helper_refs, raw)
                        } else {
                            promote_to_owned(builder, &ctx.helper_refs, kind, raw)?
                        };
                        let acc_val = builder.use_var(acc_var);
                        let new_acc = match flow.result_modifier {
                            FlowResultModifier::Map => {
                                let step_name =
                                    format!("$pipe_{}", flow.last_pipe_reg.map(|_| 0).unwrap_or(0));
                                let key_val = new_owned_val(Val::Str(step_name.into()));
                                let key_raw = builder.ins().iconst(types::I64, key_val);
                                let call = builder.ins().call(
                                    ctx.helper_refs.set_element,
                                    &[acc_val, key_raw, result_owned],
                                );
                                builder.inst_results(call)[0]
                            }
                            FlowResultModifier::Vec => {
                                let call = builder
                                    .ins()
                                    .call(ctx.helper_refs.vec_push, &[acc_val, result_owned]);
                                builder.inst_results(call)[0]
                            }
                            FlowResultModifier::One => {
                                unreachable!("Pipe accumulator only created for Map/Vec modifiers")
                            }
                        };
                        builder.def_var(acc_var, new_acc);
                    }
                }
                ctx.jump_to_next(builder, ip)?;
            }
            Instruction::DeferredVarExpr { var_name, thunk } => {
                let (acc, result_reg) = if let Some(flow) = active_flows.last() {
                    (flow.accumulator_var, flow.result_reg)
                } else {
                    (None, None)
                };
                ctx.emit_deferred_var_expr(builder, ip, *var_name, *thunk, acc, result_reg)?;
            }
            // Remaining unhandled instructions are VM-only (LoadGlobal,
            // StoreScoped, VecPush) — not emitted by the compiler.
            _ => {
                return Err(format!(
                    "Unsupported instruction for JIT: {:?}",
                    instruction
                ));
            }
        }
    }

    builder.seal_all_blocks();
    return_kind.ok_or_else(|| "JIT function has no return".to_string())
}

fn constant_string(program: &BytecodeProgram, constant: u32) -> Result<&str, String> {
    match program.constants.get(constant as usize) {
        Some(Constant::Val(Val::Str(s))) => Ok(s),
        Some(Constant::StringRef(s)) => Ok(s),
        Some(Constant::FunctionRef(s)) => Ok(s),
        _ => Err(format!("Expected string constant c{}", constant)),
    }
}

fn remap_reg(remap: &AHashMap<u32, usize>, global_id: u32) -> Result<usize, String> {
    remap
        .get(&global_id)
        .copied()
        .ok_or_else(|| format!("Unmapped JIT register r{}", global_id))
}

#[allow(clippy::too_many_arguments)]
fn define_register(
    builder: &mut FunctionBuilder<'_>,
    helper_refs: &JitHelperRefs,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &mut [Option<RawKind>],
    compile_time_owned: &mut std::collections::HashSet<usize>,
    dest: u32,
    kind: RawKind,
    value: cranelift_codegen::ir::Value,
) -> Result<(), String> {
    let idx = remap_reg(remap, dest)?;
    let slot = registers[idx];
    if register_kinds[idx] == Some(RawKind::OwnedVal) && !compile_time_owned.contains(&idx) {
        drop_owned_var(builder, helper_refs, slot);
    }
    compile_time_owned.remove(&idx);
    builder.def_var(slot, value);
    register_kinds[idx] = Some(kind);
    Ok(())
}

fn register_kind(
    remap: &AHashMap<u32, usize>,
    register_kinds: &[Option<RawKind>],
    reg: u32,
) -> Result<RawKind, String> {
    let idx = remap_reg(remap, reg)?;
    register_kinds[idx].ok_or_else(|| format!("JIT register r{} has unknown type", reg))
}

fn next_block(
    blocks: &[cranelift_codegen::ir::Block],
    ip: usize,
) -> Result<cranelift_codegen::ir::Block, String> {
    blocks
        .get(ip + 1)
        .copied()
        .ok_or_else(|| "JIT function falls off the end without a return".to_string())
}

fn jump_target(
    blocks: &[cranelift_codegen::ir::Block],
    offset: i32,
) -> Result<cranelift_codegen::ir::Block, String> {
    let idx =
        usize::try_from(offset).map_err(|_| format!("Invalid negative jump target {}", offset))?;
    blocks
        .get(idx)
        .copied()
        .ok_or_else(|| format!("Invalid jump target {}", offset))
}

fn find_matching_end_flow(instructions: &[Instruction], begin_ip: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    for (idx, instruction) in instructions.iter().enumerate().skip(begin_ip) {
        match instruction {
            Instruction::BeginFlow { .. } => depth += 1,
            Instruction::EndFlow { .. } => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(idx);
                }
            }
            _ => {}
        }
    }
    Err(format!(
        "No matching EndFlow found for BeginFlow at {}",
        begin_ip
    ))
}

fn find_matching_cond_branch_end(
    instructions: &[Instruction],
    branch_start_ip: usize,
) -> Result<usize, String> {
    let mut flow_depth = 0usize;
    for (idx, instruction) in instructions.iter().enumerate().skip(branch_start_ip + 1) {
        match instruction {
            Instruction::BeginFlow { .. } => flow_depth += 1,
            Instruction::EndFlow { .. } => {
                if flow_depth == 0 {
                    return Err(format!(
                        "No matching CondBranchEnd found for CondBranchStart at {}",
                        branch_start_ip
                    ));
                }
                flow_depth -= 1;
            }
            Instruction::CondBranchEnd { .. } if flow_depth == 0 => return Ok(idx),
            _ => {}
        }
    }
    Err(format!(
        "No matching CondBranchEnd found for CondBranchStart at {}",
        branch_start_ip
    ))
}

fn resolve_const_function_name(
    instructions: &[Instruction],
    program: &BytecodeProgram,
    call_ip: usize,
    fn_reg: u32,
) -> Option<String> {
    for i in (0..call_ip).rev() {
        match &instructions[i] {
            Instruction::LoadConst { dest, constant } if *dest == fn_reg => {
                return match program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Str(s))) => Some(s.to_string()),
                    Some(Constant::StringRef(s)) => Some(s.to_string()),
                    Some(Constant::FunctionRef(s)) => Some(s.to_string()),
                    _ => None,
                };
            }
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } if *dest == fn_reg => {
                return match program.constants.get(*function_name as usize) {
                    Some(Constant::Val(Val::Str(s))) => Some(s.to_string()),
                    Some(Constant::StringRef(s)) => Some(s.to_string()),
                    Some(Constant::FunctionRef(s)) => Some(s.to_string()),
                    _ => None,
                };
            }
            Instruction::Move { dest, src } if *dest == fn_reg => {
                return resolve_const_function_name(instructions, program, i, *src);
            }
            Instruction::LoadVar { dest, .. }
            | Instruction::Add { dest, .. }
            | Instruction::Sub { dest, .. }
            | Instruction::Mul { dest, .. }
            | Instruction::Eq { dest, .. }
            | Instruction::Call { dest, .. }
            | Instruction::CallUserFunction { dest, .. }
            | Instruction::EndFlow { dest, .. }
            | Instruction::GetTypePath { dest, .. }
            | Instruction::IsType { dest, .. }
                if *dest == fn_reg =>
            {
                return None;
            }
            _ => {}
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn try_lower_known_core_call(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: Type,
    helper_refs: JitHelperRefs,
    remap: &AHashMap<u32, usize>,
    _program: &BytecodeProgram,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    callee_name: &str,
    args_start: u32,
    args_count: u8,
) -> Result<Option<(RawKind, cranelift_codegen::ir::Value)>, String> {
    let Some(core_call) = known_core_call(callee_name) else {
        return Ok(None);
    };

    let arg = |idx: usize| args_start + idx as u32;
    let lowered = match core_call {
        KnownCoreCall::Add => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            numeric_binop(
                builder,
                ptr_ty,
                helper_refs.add_numeric,
                &helper_refs,
                BINOP_ADD,
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                |b, l, r| b.ins().iadd(l, r),
            )?
        }
        KnownCoreCall::Sub => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            numeric_binop(
                builder,
                ptr_ty,
                helper_refs.sub_numeric,
                &helper_refs,
                BINOP_SUB,
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                |b, l, r| b.ins().isub(l, r),
            )?
        }
        KnownCoreCall::Mul => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            numeric_binop(
                builder,
                ptr_ty,
                helper_refs.mul_numeric,
                &helper_refs,
                BINOP_MUL,
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                |b, l, r| b.ins().imul(l, r),
            )?
        }
        KnownCoreCall::Eq => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            (
                RawKind::Bool,
                numeric_eq(
                    builder,
                    helper_refs.eq_numeric,
                    helper_refs.eq_general,
                    remap,
                    registers,
                    register_kinds,
                    arg(0),
                    arg(1),
                )?,
            )
        }
        KnownCoreCall::Gt => (
            RawKind::Bool,
            numeric_compare(
                builder,
                helper_refs.gt_numeric,
                &helper_refs,
                1, // gt
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThan,
            )?,
        ),
        KnownCoreCall::Gte => (
            RawKind::Bool,
            numeric_compare(
                builder,
                helper_refs.gte_numeric,
                &helper_refs,
                3, // gte
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThanOrEqual,
            )?,
        ),
        KnownCoreCall::Lt => (
            RawKind::Bool,
            numeric_compare(
                builder,
                helper_refs.lt_numeric,
                &helper_refs,
                0, // lt
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
            )?,
        ),
        KnownCoreCall::Lte => (
            RawKind::Bool,
            numeric_compare(
                builder,
                helper_refs.lte_numeric,
                &helper_refs,
                2, // lte
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                cranelift_codegen::ir::condcodes::IntCC::SignedLessThanOrEqual,
            )?,
        ),
        KnownCoreCall::Div => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            // Division always goes through the Dec trampoline because:
            // 1. Int / Int can produce Dec (e.g. 3/2 = 1.5)
            // 2. Division by zero must return an error, not trap
            numeric_binop_dec_only(
                builder,
                ptr_ty,
                helper_refs.div_numeric,
                &helper_refs,
                BINOP_DIV,
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
            )?
        }
        KnownCoreCall::Mod => {
            if args_count != 2 {
                return Err(format!("JIT intrinsic '{}' expects 2 args", callee_name));
            }
            // Mod uses a zero-guarded Int path so Int%Int returns Int
            // (not Dec). The guard branches to the trampoline on zero
            // to produce a proper error instead of trapping.
            numeric_binop_int_guarded(
                builder,
                ptr_ty,
                helper_refs.mod_numeric,
                &helper_refs,
                BINOP_MOD,
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                |builder, l, r| builder.ins().srem(l, r),
            )?
        }
        KnownCoreCall::Ne => (
            RawKind::Bool,
            numeric_compare(
                builder,
                helper_refs.ne_numeric,
                &helper_refs,
                5, // ne
                remap,
                registers,
                register_kinds,
                arg(0),
                arg(1),
                cranelift_codegen::ir::condcodes::IntCC::NotEqual,
            )?,
        ),
        KnownCoreCall::Not => {
            if args_count != 1 {
                return Err(format!("JIT intrinsic '{}' expects 1 arg", callee_name));
            }
            let truthy = truthy_value(
                builder,
                &helper_refs,
                remap,
                registers,
                register_kinds,
                arg(0),
            )?;
            let one = builder.ins().iconst(types::I64, 1);
            let zero = builder.ins().iconst(types::I64, 0);
            (RawKind::Bool, builder.ins().select(truthy, zero, one))
        }
        KnownCoreCall::IsZero => {
            if args_count != 1 {
                return Err(format!("JIT intrinsic '{}' expects 1 arg", callee_name));
            }
            let kind = register_kind(remap, register_kinds, arg(0))?;
            if kind != RawKind::Int {
                return Ok(None);
            }
            let val = builder.use_var(registers[remap_reg(remap, arg(0))?]);
            let cmp =
                builder
                    .ins()
                    .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, val, 0);
            let one = builder.ins().iconst(types::I64, 1);
            let zero = builder.ins().iconst(types::I64, 0);
            (RawKind::Bool, builder.ins().select(cmp, one, zero))
        }
        KnownCoreCall::IsNull => {
            if args_count != 1 {
                return Err(format!("JIT intrinsic '{}' expects 1 arg", callee_name));
            }
            let kind = register_kind(remap, register_kinds, arg(0))?;
            let one = builder.ins().iconst(types::I64, 1);
            let zero = builder.ins().iconst(types::I64, 0);
            let result = match kind {
                RawKind::Null => one,
                RawKind::Int
                | RawKind::Bool
                | RawKind::Dec
                | RawKind::StringConst
                | RawKind::TypeTag => zero,
                RawKind::OwnedVal => {
                    let raw = builder.use_var(registers[remap_reg(remap, arg(0))?]);
                    let call = builder.ins().call(helper_refs.is_null, &[raw]);
                    builder.inst_results(call)[0]
                }
            };
            (RawKind::Bool, result)
        }
        KnownCoreCall::IsSome => {
            if args_count != 1 {
                return Err(format!("JIT intrinsic '{}' expects 1 arg", callee_name));
            }
            let kind = register_kind(remap, register_kinds, arg(0))?;
            let one = builder.ins().iconst(types::I64, 1);
            let zero = builder.ins().iconst(types::I64, 0);
            let result = match kind {
                RawKind::Null => zero,
                RawKind::Int
                | RawKind::Bool
                | RawKind::Dec
                | RawKind::StringConst
                | RawKind::TypeTag => one,
                RawKind::OwnedVal => {
                    let raw = builder.use_var(registers[remap_reg(remap, arg(0))?]);
                    let call = builder.ins().call(helper_refs.is_null, &[raw]);
                    let is_null = builder.inst_results(call)[0];
                    let cmp = builder.ins().icmp_imm(
                        cranelift_codegen::ir::condcodes::IntCC::Equal,
                        is_null,
                        0,
                    );
                    builder.ins().select(cmp, one, zero)
                }
            };
            (RawKind::Bool, result)
        }
        KnownCoreCall::And => {
            if args_count < 2 {
                return Err(format!(
                    "JIT intrinsic '{}' expects at least 2 args",
                    callee_name
                ));
            }
            if args_count != 2 {
                return Ok(None);
            }
            let truthy_a = truthy_value(
                builder,
                &helper_refs,
                remap,
                registers,
                register_kinds,
                arg(0),
            )?;
            let a_raw = builder.use_var(registers[remap_reg(remap, arg(0))?]);
            let b_raw = builder.use_var(registers[remap_reg(remap, arg(1))?]);
            let a_kind = register_kind(remap, register_kinds, arg(0))?;
            let b_kind = register_kind(remap, register_kinds, arg(1))?;
            if a_kind == b_kind {
                let selected = builder.ins().select(truthy_a, b_raw, a_raw);
                // For OwnedVal, the source registers still own their Arc<Val>
                // pointers. The destination needs an additional ref count;
                // otherwise the Arc may be freed while still referenced by a
                // source slot, leading to a non-deterministic use-after-free.
                let value = if a_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &helper_refs, selected)
                } else {
                    selected
                };
                (a_kind, value)
            } else {
                return Ok(None);
            }
        }
        KnownCoreCall::Or => {
            if args_count < 2 {
                return Err(format!(
                    "JIT intrinsic '{}' expects at least 2 args",
                    callee_name
                ));
            }
            if args_count != 2 {
                return Ok(None);
            }
            let truthy_a = truthy_value(
                builder,
                &helper_refs,
                remap,
                registers,
                register_kinds,
                arg(0),
            )?;
            let a_raw = builder.use_var(registers[remap_reg(remap, arg(0))?]);
            let b_raw = builder.use_var(registers[remap_reg(remap, arg(1))?]);
            let a_kind = register_kind(remap, register_kinds, arg(0))?;
            let b_kind = register_kind(remap, register_kinds, arg(1))?;
            if a_kind == b_kind {
                let selected = builder.ins().select(truthy_a, a_raw, b_raw);
                // See KnownCoreCall::And — both source registers own their
                // Arc<Val> pointers, so the destination must clone the
                // selected value to take its own reference.
                let value = if a_kind == RawKind::OwnedVal {
                    clone_owned_raw(builder, &helper_refs, selected)
                } else {
                    selected
                };
                (a_kind, value)
            } else {
                return Ok(None);
            }
        }
        KnownCoreCall::Length => {
            if args_count != 1 {
                return Err(format!("JIT intrinsic '{}' expects 1 arg", callee_name));
            }
            let kind = register_kind(remap, register_kinds, arg(0))?;
            if kind != RawKind::OwnedVal {
                return Ok(None);
            }
            let raw = builder.use_var(registers[remap_reg(remap, arg(0))?]);
            let call = builder.ins().call(helper_refs.length, &[raw]);
            let result = builder.inst_results(call)[0];
            (RawKind::Int, result)
        }
    };

    Ok(Some(lowered))
}

fn known_core_call(name: &str) -> Option<KnownCoreCall> {
    // Bare names (e.g., "add") appear in Call instructions when the compiler
    // couldn't resolve the function locally — meaning it IS the core function.
    // A user-defined function with the same name would have been resolved to
    // CallUserFunction with its specific ID, so bare names here are safe.
    match name {
        "::hot::math/add" | "add" => Some(KnownCoreCall::Add),
        "::hot::math/sub" | "sub" => Some(KnownCoreCall::Sub),
        "::hot::math/mul" | "mul" => Some(KnownCoreCall::Mul),
        "::hot::math/div" | "div" => Some(KnownCoreCall::Div),
        "::hot::math/mod" | "mod" => Some(KnownCoreCall::Mod),
        "::hot::cmp/eq" | "eq" => Some(KnownCoreCall::Eq),
        "::hot::cmp/ne" | "ne" => Some(KnownCoreCall::Ne),
        "::hot::cmp/gt" | "gt" => Some(KnownCoreCall::Gt),
        "::hot::cmp/gte" | "gte" => Some(KnownCoreCall::Gte),
        "::hot::cmp/lt" | "lt" => Some(KnownCoreCall::Lt),
        "::hot::cmp/lte" | "lte" => Some(KnownCoreCall::Lte),
        "::hot::bool/not" | "not" => Some(KnownCoreCall::Not),
        "::hot::math/is-zero" | "is-zero" => Some(KnownCoreCall::IsZero),
        "::hot::null/is-null" | "is-null" => Some(KnownCoreCall::IsNull),
        "::hot::null/is-some" | "is-some" => Some(KnownCoreCall::IsSome),
        "::hot::bool/and" | "and" => Some(KnownCoreCall::And),
        "::hot::bool/or" | "or" => Some(KnownCoreCall::Or),
        "::hot::coll/length" | "length" => Some(KnownCoreCall::Length),
        _ => None,
    }
}

fn int_binop(
    builder: &mut FunctionBuilder<'_>,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
    op: impl FnOnce(
        &mut FunctionBuilder<'_>,
        cranelift_codegen::ir::Value,
        cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value,
) -> Result<cranelift_codegen::ir::Value, String> {
    if register_kind(remap, register_kinds, left)? != RawKind::Int
        || register_kind(remap, register_kinds, right)? != RawKind::Int
    {
        return Err("JIT arithmetic currently supports Int operands only".to_string());
    }
    let left_val = builder.use_var(registers[remap_reg(remap, left)?]);
    let right_val = builder.use_var(registers[remap_reg(remap, right)?]);
    Ok(op(builder, left_val, right_val))
}

#[allow(clippy::too_many_arguments)]
fn numeric_binop(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: Type,
    helper: FuncRef,
    helper_refs: &JitHelperRefs,
    binop_op_code: i64,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
    int_op: impl FnOnce(
        &mut FunctionBuilder<'_>,
        cranelift_codegen::ir::Value,
        cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value,
) -> Result<(RawKind, cranelift_codegen::ir::Value), String> {
    let left_kind = register_kind(remap, register_kinds, left)?;
    let right_kind = register_kind(remap, register_kinds, right)?;
    if left_kind == RawKind::Int && right_kind == RawKind::Int {
        return Ok((
            RawKind::Int,
            int_binop(
                builder,
                remap,
                registers,
                register_kinds,
                left,
                right,
                int_op,
            )?,
        ));
    }
    if !matches!(left_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
        || !matches!(right_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
    {
        return Err("JIT arithmetic supports Int/Dec operands only".to_string());
    }

    // When OwnedVal is involved, use the general helper that preserves result types
    if left_kind == RawKind::OwnedVal || right_kind == RawKind::OwnedVal {
        let (left_tag, left_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, left)?;
        let (right_tag, right_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, right)?;
        let op_val = builder.ins().iconst(types::I64, binop_op_code);
        let call = builder.ins().call(
            helper_refs.binop_general,
            &[op_val, left_tag, left_raw, right_tag, right_raw],
        );
        let result_handle = builder.inst_results(call)[0];
        return Ok((RawKind::OwnedVal, result_handle));
    }

    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        mem::size_of::<D256>() as u32,
        3,
    ));
    let out_ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
    let (left_tag, left_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
    let (right_tag, right_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
    let call = builder
        .ins()
        .call(helper, &[out_ptr, left_tag, left_raw, right_tag, right_raw]);
    let err_code = builder.inst_results(call)[0];

    let zero = builder.ins().iconst(types::I64, 0);
    let is_err = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::NotEqual,
        err_code,
        zero,
    );
    let ok_block = builder.create_block();
    let err_block = builder.create_block();
    builder.ins().brif(is_err, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    builder.ins().return_(&[err_code]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    Ok((RawKind::Dec, out_ptr))
}

/// Like `numeric_binop` but for Int/Int, guards against a zero right operand
/// by branching to the trampoline instead of executing the raw Cranelift
/// instruction (which would trap on srem/sdiv by zero). Returns `Int` when
/// both operands are `Int` (unlike `numeric_binop_dec_only` which always
/// returns `Dec`).
#[allow(clippy::too_many_arguments)]
fn numeric_binop_int_guarded(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: Type,
    helper: FuncRef,
    helper_refs: &JitHelperRefs,
    binop_op_code: i64,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
    int_op: impl FnOnce(
        &mut FunctionBuilder<'_>,
        cranelift_codegen::ir::Value,
        cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value,
) -> Result<(RawKind, cranelift_codegen::ir::Value), String> {
    let left_kind = register_kind(remap, register_kinds, left)?;
    let right_kind = register_kind(remap, register_kinds, right)?;

    if left_kind == RawKind::Int && right_kind == RawKind::Int {
        let left_val = builder.use_var(registers[remap_reg(remap, left)?]);
        let right_val = builder.use_var(registers[remap_reg(remap, right)?]);

        let zero = builder.ins().iconst(types::I64, 0);
        let is_zero_rhs = builder.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            right_val,
            zero,
        );

        let safe_block = builder.create_block();
        let err_block = builder.create_block();
        builder
            .ins()
            .brif(is_zero_rhs, err_block, &[], safe_block, &[]);

        // Zero case: delegate to the trampoline which returns a proper error
        builder.switch_to_block(err_block);
        builder.seal_block(err_block);
        let out_addr = builder.ins().iconst(ptr_ty, 0); // dummy; trampoline will return error
        let (lt, lr) = encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
        let (rt, rr) = encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
        let call = builder.ins().call(helper, &[out_addr, lt, lr, rt, rr]);
        let err_code = builder.inst_results(call)[0];
        builder.ins().return_(&[err_code]);

        // Non-zero case: safe to use the native instruction
        builder.switch_to_block(safe_block);
        builder.seal_block(safe_block);
        let result = int_op(builder, left_val, right_val);
        return Ok((RawKind::Int, result));
    }

    // Non Int/Int: delegate to the Dec trampoline path (same as numeric_binop)
    if !matches!(left_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
        || !matches!(right_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
    {
        return Err("JIT arithmetic supports Int/Dec operands only".to_string());
    }
    if left_kind == RawKind::OwnedVal || right_kind == RawKind::OwnedVal {
        let (left_tag, left_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, left)?;
        let (right_tag, right_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, right)?;
        let op_val = builder.ins().iconst(types::I64, binop_op_code);
        let call = builder.ins().call(
            helper_refs.binop_general,
            &[op_val, left_tag, left_raw, right_tag, right_raw],
        );
        let result_handle = builder.inst_results(call)[0];
        return Ok((RawKind::OwnedVal, result_handle));
    }
    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        mem::size_of::<D256>() as u32,
        3,
    ));
    let out_ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
    let (left_tag, left_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
    let (right_tag, right_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
    let call = builder
        .ins()
        .call(helper, &[out_ptr, left_tag, left_raw, right_tag, right_raw]);
    let err_code = builder.inst_results(call)[0];
    let z = builder.ins().iconst(types::I64, 0);
    let is_err = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::NotEqual,
        err_code,
        z,
    );
    let ok_block = builder.create_block();
    let err_ret_block = builder.create_block();
    builder
        .ins()
        .brif(is_err, err_ret_block, &[], ok_block, &[]);
    builder.switch_to_block(err_ret_block);
    builder.seal_block(err_ret_block);
    builder.ins().return_(&[err_code]);
    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    Ok((RawKind::Dec, out_ptr))
}

/// Like `numeric_binop` but always routes through the Dec trampoline,
/// even for Int/Int operands. Used for division where Int/Int can produce
/// Dec results (e.g. 3/2 = 1.5) and div-by-zero must return an error.
#[allow(clippy::too_many_arguments)]
fn numeric_binop_dec_only(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: Type,
    helper: FuncRef,
    helper_refs: &JitHelperRefs,
    binop_op_code: i64,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
) -> Result<(RawKind, cranelift_codegen::ir::Value), String> {
    let left_kind = register_kind(remap, register_kinds, left)?;
    let right_kind = register_kind(remap, register_kinds, right)?;
    if !matches!(left_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
        || !matches!(right_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
    {
        return Err("JIT arithmetic supports Int/Dec operands only".to_string());
    }

    if left_kind == RawKind::OwnedVal || right_kind == RawKind::OwnedVal {
        let (left_tag, left_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, left)?;
        let (right_tag, right_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, right)?;
        let op_val = builder.ins().iconst(types::I64, binop_op_code);
        let call = builder.ins().call(
            helper_refs.binop_general,
            &[op_val, left_tag, left_raw, right_tag, right_raw],
        );
        let result_handle = builder.inst_results(call)[0];
        return Ok((RawKind::OwnedVal, result_handle));
    }

    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        mem::size_of::<D256>() as u32,
        3,
    ));
    let out_ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
    let (left_tag, left_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
    let (right_tag, right_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
    let call = builder
        .ins()
        .call(helper, &[out_ptr, left_tag, left_raw, right_tag, right_raw]);
    let err_code = builder.inst_results(call)[0];

    let zero = builder.ins().iconst(types::I64, 0);
    let is_err = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::NotEqual,
        err_code,
        zero,
    );
    let ok_block = builder.create_block();
    let err_block = builder.create_block();
    builder.ins().brif(is_err, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    builder.ins().return_(&[err_code]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    Ok((RawKind::Dec, out_ptr))
}

fn int_compare(
    builder: &mut FunctionBuilder<'_>,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
    cc: cranelift_codegen::ir::condcodes::IntCC,
) -> Result<cranelift_codegen::ir::Value, String> {
    if register_kind(remap, register_kinds, left)? != RawKind::Int
        || register_kind(remap, register_kinds, right)? != RawKind::Int
    {
        return Err("JIT comparisons currently support Int operands only".to_string());
    }
    let left_val = builder.use_var(registers[remap_reg(remap, left)?]);
    let right_val = builder.use_var(registers[remap_reg(remap, right)?]);
    let cmp = builder.ins().icmp(cc, left_val, right_val);
    let one = builder.ins().iconst(types::I64, 1);
    let zero = builder.ins().iconst(types::I64, 0);
    Ok(builder.ins().select(cmp, one, zero))
}

#[allow(clippy::too_many_arguments)]
fn numeric_compare(
    builder: &mut FunctionBuilder<'_>,
    helper: FuncRef,
    helper_refs: &JitHelperRefs,
    cmp_op_code: i64,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
    cc: cranelift_codegen::ir::condcodes::IntCC,
) -> Result<cranelift_codegen::ir::Value, String> {
    let left_kind = register_kind(remap, register_kinds, left)?;
    let right_kind = register_kind(remap, register_kinds, right)?;
    if left_kind == RawKind::Int && right_kind == RawKind::Int {
        return int_compare(builder, remap, registers, register_kinds, left, right, cc);
    }
    if !matches!(left_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
        || !matches!(right_kind, RawKind::Int | RawKind::Dec | RawKind::OwnedVal)
    {
        return Err("JIT comparisons support Int/Dec operands only".to_string());
    }

    if left_kind == RawKind::OwnedVal || right_kind == RawKind::OwnedVal {
        let (left_tag, left_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, left)?;
        let (right_tag, right_raw) =
            encode_helper_val_operand(builder, remap, registers, register_kinds, right)?;
        let op_val = builder.ins().iconst(types::I64, cmp_op_code);
        let call = builder.ins().call(
            helper_refs.cmp_general,
            &[op_val, left_tag, left_raw, right_tag, right_raw],
        );
        return Ok(builder.inst_results(call)[0]);
    }

    let (left_tag, left_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
    let (right_tag, right_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
    let call = builder
        .ins()
        .call(helper, &[left_tag, left_raw, right_tag, right_raw]);
    Ok(builder.inst_results(call)[0])
}

#[allow(clippy::too_many_arguments)]
fn numeric_eq(
    builder: &mut FunctionBuilder<'_>,
    helper: FuncRef,
    general_helper: FuncRef,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    left: u32,
    right: u32,
) -> Result<cranelift_codegen::ir::Value, String> {
    let left_kind = register_kind(remap, register_kinds, left)?;
    let right_kind = register_kind(remap, register_kinds, right)?;
    if (left_kind == right_kind && !matches!(left_kind, RawKind::Dec | RawKind::OwnedVal))
        || matches!(
            (left_kind, right_kind),
            (RawKind::TypeTag, RawKind::StringConst) | (RawKind::StringConst, RawKind::TypeTag)
        )
    {
        let left_val = builder.use_var(registers[remap_reg(remap, left)?]);
        let right_val = builder.use_var(registers[remap_reg(remap, right)?]);
        let cmp = builder.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            left_val,
            right_val,
        );
        let one = builder.ins().iconst(types::I64, 1);
        let zero = builder.ins().iconst(types::I64, 0);
        return Ok(builder.ins().select(cmp, one, zero));
    }
    if !matches!(left_kind, RawKind::Int | RawKind::Dec)
        || !matches!(right_kind, RawKind::Int | RawKind::Dec)
    {
        let left_raw_v = builder.use_var(registers[remap_reg(remap, left)?]);
        let right_raw_v = builder.use_var(registers[remap_reg(remap, right)?]);
        let (eff_left_kind, eff_left_raw) =
            normalize_for_trampoline(builder, left_kind, left_raw_v);
        let (eff_right_kind, eff_right_raw) =
            normalize_for_trampoline(builder, right_kind, right_raw_v);
        let left_tag = builder
            .ins()
            .iconst(types::I64, helper_value_kind(eff_left_kind));
        let right_tag = builder
            .ins()
            .iconst(types::I64, helper_value_kind(eff_right_kind));
        let left_raw = eff_left_raw;
        let right_raw = eff_right_raw;
        let call = builder
            .ins()
            .call(general_helper, &[left_tag, left_raw, right_tag, right_raw]);
        return Ok(builder.inst_results(call)[0]);
    }
    let (left_tag, left_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, left)?;
    let (right_tag, right_raw) =
        encode_numeric_operand(builder, remap, registers, register_kinds, right)?;
    let call = builder
        .ins()
        .call(helper, &[left_tag, left_raw, right_tag, right_raw]);
    Ok(builder.inst_results(call)[0])
}

/// Promote a typed JIT value to an OwnedVal via the promote_to_owned trampoline.
fn promote_to_owned(
    builder: &mut FunctionBuilder<'_>,
    helper_refs: &JitHelperRefs,
    kind: RawKind,
    raw: cranelift_codegen::ir::Value,
) -> Result<cranelift_codegen::ir::Value, String> {
    if kind == RawKind::OwnedVal {
        return Ok(raw);
    }
    let tag = builder.ins().iconst(types::I64, helper_value_kind(kind));
    let call = builder
        .ins()
        .call(helper_refs.promote_to_owned, &[tag, raw]);
    Ok(builder.inst_results(call)[0])
}

fn encode_helper_val_operand(
    builder: &mut FunctionBuilder<'_>,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    reg: u32,
) -> Result<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value), String> {
    let kind = register_kind(remap, register_kinds, reg)?;
    let raw = builder.use_var(registers[remap_reg(remap, reg)?]);
    let tag = helper_value_kind(kind);
    Ok((builder.ins().iconst(types::I64, tag), raw))
}

fn encode_numeric_operand(
    builder: &mut FunctionBuilder<'_>,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    reg: u32,
) -> Result<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value), String> {
    let kind = register_kind(remap, register_kinds, reg)?;
    let raw = builder.use_var(registers[remap_reg(remap, reg)?]);
    match kind {
        RawKind::Int => Ok((builder.ins().iconst(types::I64, NUMERIC_KIND_INT), raw)),
        RawKind::Dec => Ok((builder.ins().iconst(types::I64, NUMERIC_KIND_DEC), raw)),
        RawKind::OwnedVal => Ok((builder.ins().iconst(types::I64, NUMERIC_KIND_OWNED), raw)),
        RawKind::Bool => Err("Boolean values are not valid numeric operands".to_string()),
        RawKind::Null => Err("Null values are not valid numeric operands".to_string()),
        RawKind::TypeTag | RawKind::StringConst => {
            Err("Type-tag values are not valid numeric operands".to_string())
        }
    }
}

fn truthy_value(
    builder: &mut FunctionBuilder<'_>,
    helper_refs: &JitHelperRefs,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    reg: u32,
) -> Result<cranelift_codegen::ir::Value, String> {
    let raw = builder.use_var(registers[remap_reg(remap, reg)?]);
    match register_kind(remap, register_kinds, reg)? {
        RawKind::Bool | RawKind::Int | RawKind::TypeTag | RawKind::StringConst => Ok(builder
            .ins()
            .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::NotEqual, raw, 0)),
        RawKind::Dec => {
            let one = builder.ins().iconst(types::I64, 1);
            Ok(builder
                .ins()
                .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::NotEqual, one, 0))
        }
        RawKind::Null => {
            Ok(builder
                .ins()
                .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::NotEqual, raw, 0))
        }
        RawKind::OwnedVal => {
            let kind = builder.ins().iconst(types::I64, HELPER_VAL_KIND_OWNED);
            let call = builder.ins().call(helper_refs.truthy_general, &[kind, raw]);
            let truthy = builder.inst_results(call)[0];
            Ok(builder
                .ins()
                .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::NotEqual, truthy, 0))
        }
    }
}

/// Compile a LambdaInfo thunk's instructions inline into the current function.
/// The thunk's `Return` instruction writes to `result_var` and jumps to `merge_block`.
/// Capture variables are resolved from the caller's `locals` map.
/// Returns the RawKind of the thunk's result, or Err if the thunk body can't be compiled.
#[allow(clippy::too_many_arguments)]
fn compile_thunk_inline(
    builder: &mut FunctionBuilder<'_>,
    program: &BytecodeProgram,
    function_id: FunctionId,
    signature: &TypeSig,
    ptr_ty: Type,
    self_func_ref: FuncRef,
    helper_refs: &JitHelperRefs,
    arg_layout: &AbiLayout,
    expected_return_kind: Option<JitValueKind>,
    lambda: &crate::lang::bytecode::LambdaInfo,
    parent_locals: &AHashMap<String, (Variable, RawKind)>,
    _parent_thunk_registry: &AHashMap<u32, crate::lang::bytecode::LambdaInfo>,
    result_var: Variable,
    merge_block: cranelift_codegen::ir::Block,
) -> Result<RawKind, String> {
    let instructions = &lambda.instructions;
    if instructions.is_empty() {
        return Err("Empty thunk body".to_string());
    }

    let used_regs = collect_used_registers(instructions);
    let remap: AHashMap<u32, usize> = used_regs
        .iter()
        .enumerate()
        .map(|(idx, &reg)| (reg, idx))
        .collect();
    let register_count = remap.len();

    let thunk_entry = builder.create_block();
    let blocks: Vec<cranelift_codegen::ir::Block> = (0..instructions.len())
        .map(|_| builder.create_block())
        .collect();

    builder.ins().jump(thunk_entry, &[]);
    builder.switch_to_block(thunk_entry);
    builder.seal_block(thunk_entry);

    let zero = builder.ins().iconst(types::I64, 0);
    let mut registers = Vec::with_capacity(register_count);
    let register_kinds: Vec<Option<RawKind>> = vec![None; register_count];
    for _ in 0..register_count {
        let var = builder.declare_var(types::I64);
        builder.def_var(var, zero);
        registers.push(var);
    }

    let mut locals: AHashMap<String, (Variable, RawKind)> = AHashMap::new();
    for (name, var_and_kind) in parent_locals.iter() {
        locals.insert(name.clone(), *var_and_kind);
    }

    let mut ctx = EmitCtx {
        program,
        function_id,
        signature,
        ptr_ty,
        self_func_ref,
        helper_refs: *helper_refs,
        arg_layout: arg_layout.clone(),
        expected_return_kind,
        blocks,
        remap,
        registers,
        register_kinds,
        compile_time_owned: std::collections::HashSet::new(),
        locals,
        thunk_registry: AHashMap::new(),
        scope_snapshots: Vec::new(),
        error_capture_active: false,
    };

    let mut return_kind: Option<RawKind> = None;

    builder.ins().jump(ctx.blocks[0], &[]);

    for (ip, instruction) in instructions.iter().enumerate() {
        builder.switch_to_block(ctx.blocks[ip]);

        match ctx.emit_instruction(builder, ip, instruction, instructions)? {
            EmitResult::Handled => continue,
            EmitResult::Unhandled => {}
        }

        match instruction {
            Instruction::CallUserFunction {
                dest,
                args_start,
                args_count,
                ..
            } => {
                ctx.emit_self_call(builder, ip, *dest, *args_start, *args_count)?;
            }
            Instruction::ReturnIfErr { src } => match ctx.emit_err_check(builder, ip, *src)? {
                None => {}
                Some((error_block, error_raw)) => {
                    builder.switch_to_block(error_block);
                    builder.seal_block(error_block);
                    let error_kind = ctx.register_kind(*src)?;
                    let owned_error = if error_kind == RawKind::OwnedVal {
                        clone_owned_raw(builder, helper_refs, error_raw)
                    } else {
                        error_raw
                    };
                    builder.def_var(result_var, owned_error);
                    return_kind = Some(RawKind::OwnedVal);
                    builder.ins().jump(merge_block, &[]);
                }
            },
            Instruction::Return { value } => {
                let kind = ctx.register_kind(*value)?;
                let val = ctx.reg_value(builder, *value)?;
                let (kind, val) = match kind {
                    RawKind::OwnedVal => (
                        RawKind::OwnedVal,
                        clone_owned_raw(builder, helper_refs, val),
                    ),
                    RawKind::TypeTag | RawKind::StringConst => (
                        RawKind::OwnedVal,
                        materialize_string_const_for_trampoline(
                            program,
                            instructions,
                            *value,
                            builder,
                        ),
                    ),
                    _ => (kind, val),
                };
                builder.def_var(result_var, val);
                return_kind = Some(kind);
                builder.ins().jump(merge_block, &[]);
            }
            _ => {
                return Err(format!(
                    "Unsupported instruction in thunk: {:?}",
                    std::mem::discriminant(instruction)
                ));
            }
        }
    }

    for block in &ctx.blocks {
        if !builder.func.stencil.layout.is_block_inserted(*block) {
            builder.switch_to_block(*block);
            builder.ins().jump(merge_block, &[]);
        }
        builder.seal_block(*block);
    }

    return_kind.ok_or_else(|| "Thunk body has no Return instruction".to_string())
}

/// Result from `emit_self_recursive_call`: (result_kind, result_value, error_info).
/// When error_info is Some, the caller must switch to the unsealed error block,
/// seal it, clean up owned values, and return the error value from the function.
type SelfRecursiveCallResult = Result<
    (
        RawKind,
        cranelift_codegen::ir::Value,
        Option<(cranelift_codegen::ir::Block, cranelift_codegen::ir::Value)>,
    ),
    String,
>;

/// Emit a native self-recursive call via Cranelift.
/// Encodes args into the ABI layout, calls self_func_ref, decodes the result.
/// On success the builder is positioned in the ok-path block. If the callee
/// returned an error, the error block and error pointer are returned so the
/// caller can emit cleanup + return.
#[allow(clippy::too_many_arguments)]
fn emit_self_recursive_call(
    builder: &mut FunctionBuilder<'_>,
    ptr_ty: Type,
    helper_refs: &JitHelperRefs,
    self_func_ref: FuncRef,
    signature: &TypeSig,
    arg_layout: &AbiLayout,
    expected_return_kind: JitValueKind,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    args_start: u32,
    args_count: u8,
) -> SelfRecursiveCallResult {
    if usize::from(args_count) != signature.args.len() {
        return Err("JIT self-recursive call arity mismatch".to_string());
    }

    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        arg_layout.size as u32,
        3,
    ));

    for idx in 0..usize::from(args_count) {
        let reg = args_start + idx as u32;
        let expected_kind = signature.args[idx]
            .raw_kind()
            .ok_or_else(|| "Unsupported recursive JIT arg kind".to_string())?;
        let actual_kind = register_kind(remap, register_kinds, reg)?;
        if expected_kind != actual_kind {
            return Err(format!(
                "JIT recursive call argument kind mismatch: expected {:?}, got {:?}",
                expected_kind, actual_kind
            ));
        }
        let raw = builder.use_var(registers[remap_reg(remap, reg)?]);
        match expected_kind {
            RawKind::Int | RawKind::Bool => {
                builder
                    .ins()
                    .stack_store(raw, slot, arg_layout.offset(idx)? as i32);
            }
            RawKind::Dec => {
                let dst_ptr =
                    builder
                        .ins()
                        .stack_addr(ptr_ty, slot, arg_layout.offset(idx)? as i32);
                builder.ins().call(helper_refs.copy_dec, &[raw, dst_ptr]);
            }
            RawKind::Null => {
                let zero = builder.ins().iconst(types::I64, 0);
                builder
                    .ins()
                    .stack_store(zero, slot, arg_layout.offset(idx)? as i32);
            }
            RawKind::OwnedVal => {
                let cloned = clone_owned_raw(builder, helper_refs, raw);
                builder
                    .ins()
                    .stack_store(cloned, slot, arg_layout.offset(idx)? as i32);
            }
            RawKind::TypeTag | RawKind::StringConst => {
                return Err("JIT self-recursive internal tokens not supported".to_string());
            }
        };
    }

    let self_ret_raw = expected_return_kind.raw_kind();
    let result_layout = AbiLayout::for_result(expected_return_kind);
    let arg_ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
    let result_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        result_layout.size as u32,
        3,
    ));
    let result_ptr = builder.ins().stack_addr(ptr_ty, result_slot, 0);
    let call_inst = builder.ins().call(self_func_ref, &[arg_ptr, result_ptr]);
    let ret_flag = builder.inst_results(call_inst)[0];

    let zero = builder.ins().iconst(types::I64, 0);
    let is_err = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::NotEqual,
        ret_flag,
        zero,
    );
    let error_block = builder.create_block();
    let ok_block = builder.create_block();
    builder.ins().brif(is_err, error_block, &[], ok_block, &[]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);

    match self_ret_raw {
        RawKind::Int | RawKind::Bool | RawKind::Null | RawKind::OwnedVal => {
            let v = builder
                .ins()
                .load(types::I64, MemFlags::new(), result_ptr, 0);
            Ok((self_ret_raw, v, Some((error_block, ret_flag))))
        }
        RawKind::Dec => {
            let dec_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                mem::size_of::<D256>() as u32,
                3,
            ));
            let dec_ptr = builder.ins().stack_addr(ptr_ty, dec_slot, 0);
            builder
                .ins()
                .call(helper_refs.copy_dec, &[result_ptr, dec_ptr]);
            Ok((RawKind::Dec, dec_ptr, Some((error_block, ret_flag))))
        }
        RawKind::TypeTag | RawKind::StringConst => {
            Err("JIT unsupported self-recursive return kind: internal tokens".to_string())
        }
    }
}

/// Look up a function by name, matching both qualified ("::hot::bool/if") and
/// unqualified ("if") names against the program's function list.
/// Find a function by unqualified name suffix (e.g. "if" matches "::hot::bool/if").
/// Used for core functions referenced without full qualification in bytecode.
/// Only returns a match if exactly one namespace contains the name at the given arity.
fn find_function_by_suffix(
    program: &BytecodeProgram,
    short_name: &str,
    arity: Option<u8>,
) -> Option<usize> {
    let suffix = format!("/{}", short_name);
    let mut found = None;
    for (i, f) in program.functions.iter().enumerate() {
        if f.name.ends_with(&suffix) {
            if let Some(a) = arity
                && f.arity != a
                && !f.is_variadic
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// When `arity` is provided, prefer the overload that matches the arg count.
/// Find a function by exact qualified name match, optionally filtering by arity.
/// Only matches functions whose name is exactly `name`. No fuzzy suffix matching.
fn find_function_exact(program: &BytecodeProgram, name: &str, arity: Option<u8>) -> Option<usize> {
    let mut first_match = None;
    for (i, f) in program.functions.iter().enumerate() {
        if f.name == name {
            if let Some(a) = arity {
                if f.arity == a || f.is_variadic {
                    return Some(i);
                }
            } else {
                return Some(i);
            }
            if first_match.is_none() {
                first_match = Some(i);
            }
        }
    }
    first_match
}

/// Try to inline a lazy branch-selector call.
///
/// This is intentionally keyed to bytecode shape rather than function name:
/// one eager selector parameter followed by one or two lazy thunk parameters.
/// Returns Some((kind, value)) if inlining succeeded, None if not applicable.
#[allow(clippy::too_many_arguments)]
fn try_inline_lazy_branch_call(
    builder: &mut FunctionBuilder<'_>,
    program: &BytecodeProgram,
    parent_function_id: FunctionId,
    parent_signature: &TypeSig,
    ptr_ty: Type,
    self_func_ref: FuncRef,
    helper_refs: &JitHelperRefs,
    arg_layout: &AbiLayout,
    expected_return_kind: Option<JitValueKind>,
    called_function_id: u32,
    args_start: u32,
    args_count: u8,
    locals: &AHashMap<String, (Variable, RawKind)>,
    remap: &AHashMap<u32, usize>,
    registers: &[Variable],
    register_kinds: &[Option<RawKind>],
    thunk_registry: &AHashMap<u32, crate::lang::bytecode::LambdaInfo>,
) -> Result<Option<(RawKind, cranelift_codegen::ir::Value)>, String> {
    let callee = match program.functions.get(called_function_id as usize) {
        Some(f) => f,
        None => return Ok(None),
    };
    let count = usize::from(args_count);

    // Check if this function has lazy params with thunk args in the registry.
    let lazy_params = &callee.lazy_params;
    if lazy_params.is_empty() || !lazy_params.iter().any(|&lp| lp) {
        return Ok(None);
    }
    if !is_known_lazy_branch_selector(callee) {
        return Ok(None);
    }

    // Identify: branch-selector pattern (selector, lazy then) or
    // (selector, lazy then, lazy else). The selector must not be a thunk;
    // lazy branches must be thunk registers. This does not require the
    // callee to be named `if`.
    if (2..=3).contains(&count)
        && (lazy_params.len() >= count)
        && !lazy_params[0]
        && lazy_params[1]
        && (count < 3 || lazy_params[2])
    {
        let pred_reg = args_start;
        let then_reg = args_start + 1;
        let else_reg = if count == 3 {
            Some(args_start + 2)
        } else {
            None
        };

        let then_thunk = match thunk_registry.get(&then_reg) {
            Some(t) => t,
            None => return Ok(None),
        };
        let else_thunk = match else_reg {
            Some(er) => match thunk_registry.get(&er) {
                Some(t) => Some(t),
                None => return Ok(None),
            },
            None => None,
        };

        let cond = truthy_value(
            builder,
            helper_refs,
            remap,
            registers,
            register_kinds,
            pred_reg,
        )?;

        // Use intermediate merge blocks so we can promote types if branches differ.
        let then_block = builder.create_block();
        let else_block = builder.create_block();
        let then_merge = builder.create_block();
        let else_merge = builder.create_block();
        let final_merge = builder.create_block();

        let result_var = builder.declare_var(types::I64);
        let zero = builder.ins().iconst(types::I64, 0);
        builder.def_var(result_var, zero);

        builder.ins().brif(cond, then_block, &[], else_block, &[]);

        // Compile then-branch (writes to result_var, jumps to then_merge)
        builder.switch_to_block(then_block);
        let then_kind = compile_thunk_inline(
            builder,
            program,
            parent_function_id,
            parent_signature,
            ptr_ty,
            self_func_ref,
            helper_refs,
            arg_layout,
            expected_return_kind,
            then_thunk,
            locals,
            thunk_registry,
            result_var,
            then_merge,
        )?;

        // Compile else-branch (writes to result_var, jumps to else_merge)
        builder.switch_to_block(else_block);
        let else_kind = if let Some(else_t) = else_thunk {
            compile_thunk_inline(
                builder,
                program,
                parent_function_id,
                parent_signature,
                ptr_ty,
                self_func_ref,
                helper_refs,
                arg_layout,
                expected_return_kind,
                else_t,
                locals,
                thunk_registry,
                result_var,
                else_merge,
            )?
        } else {
            builder.def_var(result_var, zero);
            builder.ins().jump(else_merge, &[]);
            RawKind::Null
        };

        let merged_kind = if then_kind == else_kind {
            then_kind
        } else {
            RawKind::OwnedVal
        };

        // then_merge: promote if needed, then jump to final_merge
        builder.seal_block(then_block);
        builder.switch_to_block(then_merge);
        builder.seal_block(then_merge);
        if merged_kind == RawKind::OwnedVal && then_kind != RawKind::OwnedVal {
            let v = builder.use_var(result_var);
            let promoted = promote_to_owned(builder, helper_refs, then_kind, v)?;
            builder.def_var(result_var, promoted);
        }
        builder.ins().jump(final_merge, &[]);

        // else_merge: promote if needed, then jump to final_merge
        builder.seal_block(else_block);
        builder.switch_to_block(else_merge);
        builder.seal_block(else_merge);
        if merged_kind == RawKind::OwnedVal && else_kind != RawKind::OwnedVal {
            let v = builder.use_var(result_var);
            let promoted = promote_to_owned(builder, helper_refs, else_kind, v)?;
            builder.def_var(result_var, promoted);
        }
        builder.ins().jump(final_merge, &[]);

        builder.switch_to_block(final_merge);
        builder.seal_block(final_merge);

        let result_val = builder.use_var(result_var);
        return Ok(Some((merged_kind, result_val)));
    }

    Ok(None)
}

fn is_known_lazy_branch_selector(callee: &FunctionInfo) -> bool {
    callee.name == "::hot::bool/if"
        && callee.arity == 3
        && callee.lazy_params == [false, true, true]
}

#[cfg(test)]
mod tests {
    use super::{
        JitAvailability, JitConfig, JitMode, JitRuntimeState, JitTypeTag, TypeSig,
        compile_supported_function,
    };
    use crate::lang::Engine;
    use crate::lang::ast::HotAst;
    use crate::lang::ast::Program;
    use crate::lang::bytecode::FunctionId;
    use crate::lang::bytecode::{
        BytecodeProgram, Constant, FlowResultModifier, FlowType, FunctionInfo, Instruction,
    };
    use crate::lang::compiler::core_registry::CoreVariableRegistry;
    use crate::lang::engine::CompilationArtifacts;
    use crate::lang::runtime::vm::VirtualMachine;
    use crate::val;
    use crate::val::Val;
    use ahash::AHashMap;
    use fastnum::{D256, decimal::Context};
    use indexmap::IndexMap;
    use std::sync::Arc;

    #[test]
    fn type_sig_tracks_typed_maps() {
        let args = vec![
            val!(1),
            val!({"$type": "::app/User", "name": "A"}),
            val!(true),
        ];

        let sig = TypeSig::from_args(&args);
        assert_eq!(sig.arity, 3);
        assert_eq!(sig.args[0], JitTypeTag::Int);
        assert_eq!(sig.args[1], JitTypeTag::TypedMap("::app/User".to_string()));
        assert_eq!(sig.args[2], JitTypeTag::Bool);
    }

    #[test]
    fn jit_config_defaults_to_enabled_on_supported_platform() {
        let conf = val!({});
        let cfg = JitConfig::from_conf(&conf);
        let status = super::CodeMemoryStatus::detect();
        if status.availability == JitAvailability::Unsupported {
            assert_eq!(cfg.mode, JitMode::Disabled);
        } else {
            assert_eq!(cfg.mode, JitMode::Enabled);
        }
        assert_eq!(cfg.threshold, 100);
    }

    #[test]
    fn jit_config_disabled_via_conf() {
        let conf = val!({"jit": {"mode": "disabled"}});
        let cfg = JitConfig::from_conf(&conf);
        assert_eq!(cfg.mode, JitMode::Disabled);
    }

    #[test]
    fn jit_config_respects_threshold_from_conf() {
        let conf = val!({"jit": {"mode": "enabled", "threshold": 50}});
        let cfg = JitConfig::from_conf(&conf);
        assert_eq!(cfg.threshold, 50);
    }

    #[test]
    fn jit_config_fallback_from_plain_jit_key() {
        let conf = val!({"jit": "disabled"});
        let cfg = JitConfig::from_conf(&conf);
        assert_eq!(cfg.mode, JitMode::Disabled);
    }

    #[test]
    fn code_memory_status_detects_known_targets() {
        let status = super::CodeMemoryStatus::detect();
        assert!(matches!(
            status.availability,
            JitAvailability::Unsupported
                | JitAvailability::Experimental
                | JitAvailability::Available
        ));
    }

    #[test]
    fn jit_compiles_capture_free_lambda_callback() {
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));

        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec!["x".to_string()],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: false,
            used_registers: vec![0, 1, 2],
        };

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit = JitRuntimeState::new(program.functions.len(), &conf);
        let result = jit
            .try_call_compiled_lambda(&program, &lambda, &[val!(41)], &[])
            .unwrap();

        assert_eq!(result, Some(val!(42)));
    }

    #[test]
    fn jit_compiles_lambda_callback_with_captures() {
        let mut program = BytecodeProgram::new();
        let offset_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("offset")));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        let mut closure_env = AHashMap::new();
        closure_env.insert("offset".to_string(), val!(10));

        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec!["x".to_string()],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: offset_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            capture_vars: vec!["offset".to_string()],
            closure_env,
            defining_namespace: "::test".to_string(),
            is_lazy_param: false,
            used_registers: vec![0, 1, 2],
        };

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit = JitRuntimeState::new(program.functions.len(), &conf);
        let result = jit
            .try_call_compiled_lambda(&program, &lambda, &[val!(5)], &[val!(10)])
            .unwrap();

        assert_eq!(result, Some(val!(15)));
    }

    #[test]
    fn jit_compiles_lambda_callback_with_captured_core_function_ref() {
        let mut program = BytecodeProgram::new();
        let add_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("add")));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));

        let add_ref =
            crate::lang::runtime::function_ref::function_ref("::hot::math/add".to_string());
        let mut closure_env = AHashMap::new();
        closure_env.insert("add".to_string(), add_ref.clone());

        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec!["x".to_string()],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: add_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::CallLambda {
                    dest: 3,
                    lambda: 0,
                    args_start: 1,
                    args_count: 2,
                },
                Instruction::Return { value: 3 },
            ],
            register_count: 4,
            capture_vars: vec!["add".to_string()],
            closure_env,
            defining_namespace: "::test".to_string(),
            is_lazy_param: false,
            used_registers: vec![0, 1, 2, 3],
        };

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit = JitRuntimeState::new(program.functions.len(), &conf);
        let result = jit
            .try_call_compiled_lambda(&program, &lambda, &[val!(41)], &[add_ref])
            .unwrap();

        assert_eq!(result, Some(val!(42)));
    }

    #[test]
    fn jit_does_not_compile_lazy_param_lambda_callback() {
        let program = BytecodeProgram::new();
        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec![],
            instructions: vec![Instruction::Return { value: 0 }],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit = JitRuntimeState::new(program.functions.len(), &conf);
        let result = jit
            .try_call_compiled_lambda(&program, &lambda, &[], &[])
            .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn jit_compiles_and_executes_simple_int_function() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        let function = FunctionInfo {
            name: "::test/add_one".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        };

        let sig = TypeSig::from_args(&[val!(41)]);
        let compiled = compile_supported_function(&program, 0, &function, &sig).unwrap();
        let result = compiled.call(&[val!(41)]).unwrap();
        assert_eq!(result, val!(42));
    }

    #[test]
    fn jit_runtime_state_records_and_compiles() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/add_one".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit = JitRuntimeState::new(program.functions.len(), &conf);
        let args = [val!(10)];
        jit.record_call(0, &args);
        assert!(jit.should_compile_now(0, &args));
        jit.compile_function(&program, 0, &program.functions[0], &args)
            .unwrap();
        let result = jit.try_call_compiled(0, &args).unwrap().unwrap();
        assert_eq!(result, val!(11));
    }

    #[test]
    fn jit_compiles_and_executes_simple_dec_function() {
        let mut program = BytecodeProgram::new();
        let delta_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("1.5", Context::default()).unwrap(),
        )));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        let function = FunctionInfo {
            name: "::test/add_dec".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: delta_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        };

        let input = Val::Dec(D256::from_str("2.5", Context::default()).unwrap());
        let expected = Val::Dec(D256::from_str("4.0", Context::default()).unwrap());
        let sig = TypeSig::from_args(std::slice::from_ref(&input));
        let compiled = compile_supported_function(&program, 0, &function, &sig).unwrap();
        let result = compiled.call(std::slice::from_ref(&input)).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn jitted_cond_flow_executes_first_matching_branch() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let ten_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(10)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let two_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(2)));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(true)));
        let fallback_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        let b0 = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("b0".into()));
        let b1 = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("b1".into()));
        let default_branch = program.constants.len() as u32;
        program
            .constants
            .push(Constant::StringRef("cond_default".into()));

        program.functions.push(FunctionInfo {
            name: "::test/classify".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::Cond,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::Lt {
                    dest: 3,
                    left: 1,
                    right: 2,
                },
                Instruction::CondBranchStart {
                    branch_name: b0,
                    condition: 3,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 4,
                    constant: zero_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: b0,
                    result: 4,
                },
                Instruction::LoadVar {
                    dest: 5,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: ten_const,
                },
                Instruction::Gt {
                    dest: 7,
                    left: 5,
                    right: 6,
                },
                Instruction::CondBranchStart {
                    branch_name: b1,
                    condition: 7,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 8,
                    constant: two_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: b1,
                    result: 8,
                },
                Instruction::LoadConst {
                    dest: 9,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: default_branch,
                    condition: 9,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 10,
                    constant: fallback_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: default_branch,
                    result: 10,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 11,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(-1)]).unwrap(),
            val!(0)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(5)]).unwrap(),
            val!(1)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(11)]).unwrap(),
            val!(2)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_cond_flow_skips_unselected_branch_work() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let false_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(false)));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(true)));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let ok_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(42)));
        let skipped_branch = program.constants.len() as u32;
        program
            .constants
            .push(Constant::StringRef("skipped".into()));
        let selected_branch = program.constants.len() as u32;
        program
            .constants
            .push(Constant::StringRef("selected".into()));

        program.functions.push(FunctionInfo {
            name: "::hot::math/div".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/skips_branch".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::Cond,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: false_const,
                },
                Instruction::CondBranchStart {
                    branch_name: skipped_branch,
                    condition: 1,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: zero_const,
                },
                Instruction::CallUserFunction {
                    dest: 4,
                    function_id: 0,
                    args_start: 2,
                    args_count: 2,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: skipped_branch,
                    result: 4,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: selected_branch,
                    condition: 5,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: ok_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: selected_branch,
                    result: 6,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 7,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(vm.execute_compiled_user_function(1, &[]).unwrap(), val!(42));
        assert!(vm.jit_has_compiled_function(1));
    }

    #[test]
    fn jitted_match_flow_executes_value_literal_arm() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let two_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(2)));
        let ten_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(10)));
        let fallback_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let arm_one = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("one".into()));
        let arm_two = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("two".into()));
        let default_branch = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("_".into()));

        program.functions.push(FunctionInfo {
            name: "::test/match_value".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::Match,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::GetTypePath { dest: 2, value: 1 },
                Instruction::LoadConst {
                    dest: 3,
                    constant: one_const,
                },
                Instruction::Eq {
                    dest: 4,
                    left: 1,
                    right: 3,
                },
                Instruction::CondBranchStart {
                    branch_name: arm_one,
                    condition: 4,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: ten_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: arm_one,
                    result: 5,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: two_const,
                },
                Instruction::Eq {
                    dest: 7,
                    left: 1,
                    right: 6,
                },
                Instruction::CondBranchStart {
                    branch_name: arm_two,
                    condition: 7,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 8,
                    constant: two_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: arm_two,
                    result: 8,
                },
                Instruction::LoadConst {
                    dest: 9,
                    constant: one_const,
                },
                Instruction::CondBranchStart {
                    branch_name: default_branch,
                    condition: 9,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 10,
                    constant: fallback_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: default_branch,
                    result: 10,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 11,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(1)]).unwrap(),
            val!(10)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(2)]).unwrap(),
            val!(2)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(7)]).unwrap(),
            val!(0)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_match_flow_executes_primitive_type_arm() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let int_type_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!("::hot::type/Int")));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(true)));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let int_arm = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("Int".into()));
        let default_branch = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("_".into()));

        program.functions.push(FunctionInfo {
            name: "::test/match_type".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::Match,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::GetTypePath { dest: 2, value: 1 },
                Instruction::LoadConst {
                    dest: 3,
                    constant: int_type_const,
                },
                Instruction::IsType {
                    dest: 4,
                    value: 1,
                    type_path: 3,
                },
                Instruction::CondBranchStart {
                    branch_name: int_arm,
                    condition: 4,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: one_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: int_arm,
                    result: 5,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: default_branch,
                    condition: 6,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 7,
                    constant: zero_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: default_branch,
                    result: 7,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 8,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(9)]).unwrap(),
            val!(1)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_cond_all_flow_returns_last_matching_result_for_one_modifier() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(true)));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let two_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(2)));
        let three_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(3)));
        let b0 = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("b0".into()));
        let b1 = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("b1".into()));
        let default_branch = program.constants.len() as u32;
        program
            .constants
            .push(Constant::StringRef("cond_default".into()));

        program.functions.push(FunctionInfo {
            name: "::test/cond_all".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::CondAll,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: b0,
                    condition: 1,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: b0,
                    result: 2,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: b1,
                    condition: 3,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 4,
                    constant: two_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: b1,
                    result: 4,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: default_branch,
                    condition: 5,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: three_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: default_branch,
                    result: 6,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 7,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(vm.execute_compiled_user_function(0, &[]).unwrap(), val!(3));
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_match_all_flow_returns_last_matching_result_for_one_modifier() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let int_type_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!("::hot::type/Int")));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(true)));
        let ten_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(10)));
        let twenty_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(20)));
        let thirty_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(30)));
        let arm_one = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("one".into()));
        let arm_int = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("Int".into()));
        let default_branch = program.constants.len() as u32;
        program.constants.push(Constant::StringRef("_".into()));

        program.functions.push(FunctionInfo {
            name: "::test/match_all".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::BeginFlow {
                    flow_type: FlowType::MatchAll,
                    result_modifier: FlowResultModifier::One,
                    source: None,
                },
                Instruction::LoadConst {
                    dest: 0,
                    constant: null_const,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::GetTypePath { dest: 2, value: 1 },
                Instruction::LoadConst {
                    dest: 3,
                    constant: one_const,
                },
                Instruction::Eq {
                    dest: 4,
                    left: 1,
                    right: 3,
                },
                Instruction::CondBranchStart {
                    branch_name: arm_one,
                    condition: 4,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: ten_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: arm_one,
                    result: 5,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: int_type_const,
                },
                Instruction::IsType {
                    dest: 7,
                    value: 1,
                    type_path: 6,
                },
                Instruction::CondBranchStart {
                    branch_name: arm_int,
                    condition: 7,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 8,
                    constant: twenty_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: arm_int,
                    result: 8,
                },
                Instruction::LoadConst {
                    dest: 9,
                    constant: true_const,
                },
                Instruction::CondBranchStart {
                    branch_name: default_branch,
                    condition: 9,
                    skip_target: 0,
                },
                Instruction::EnterScope {
                    scope_type: crate::lang::bytecode::ScopeType::Flow,
                },
                Instruction::LoadConst {
                    dest: 10,
                    constant: thirty_const,
                },
                Instruction::ExitScope,
                Instruction::CondBranchEnd {
                    branch_name: default_branch,
                    result: 10,
                },
                Instruction::EndFlow { dest: 0 },
                Instruction::Return { value: 0 },
            ],
            register_count: 11,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(1)]).unwrap(),
            val!(30)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    // (Aggregate flow modifier tests removed)

    #[test]
    fn jitted_function_roundtrips_owned_vec_param() {
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/echo_vec".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let input = Val::Vec(vec![val!(1), val!(2), val!(3)]);

        assert_eq!(
            vm.execute_compiled_user_function(0, std::slice::from_ref(&input))
                .unwrap(),
            input
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_function_compares_owned_vec_args_for_equality() {
        let mut program = BytecodeProgram::new();
        let left_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("left")));
        let right_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("right")));

        program.functions.push(FunctionInfo {
            name: "::test/vec_eq".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["left".to_string(), "right".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: left_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: right_name,
                },
                Instruction::Eq {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(
                0,
                &[
                    Val::Vec(vec![val!(1), val!(2)]),
                    Val::Vec(vec![val!(1), val!(2)])
                ]
            )
            .unwrap(),
            val!(true)
        );
        assert_eq!(
            vm.execute_compiled_user_function(
                0,
                &[
                    Val::Vec(vec![val!(1), val!(2)]),
                    Val::Vec(vec![val!(1), val!(3)])
                ]
            )
            .unwrap(),
            val!(false)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jitted_function_uses_owned_value_truthiness() {
        let mut program = BytecodeProgram::new();
        let value_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("value")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));

        program.functions.push(FunctionInfo {
            name: "::test/owned_truthy".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["value".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: value_name,
                },
                Instruction::JumpIf {
                    condition: 0,
                    offset: 4,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: zero_const,
                },
                Instruction::Return { value: 1 },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[Val::Vec(vec![])])
                .unwrap(),
            val!(1)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    fn make_test_vm(program: BytecodeProgram, conf: crate::val::Val) -> VirtualMachine {
        VirtualMachine::new(
            Arc::new(program),
            None,
            Arc::new(IndexMap::<String, FunctionId>::new()),
            Arc::new(IndexMap::<String, FunctionId>::new()),
            Arc::new(IndexMap::<(String, String), String>::new()),
            Arc::new(CoreVariableRegistry::new()),
            Some(conf),
        )
    }

    #[test]
    fn jitted_function_uses_defining_namespace_for_alias_lookup() {
        let mut program = BytecodeProgram::new();
        let ok_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("ok")));
        let alias_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("http-request")));

        program.functions.push(FunctionInfo {
            name: "::dep/target".to_string(),
            namespace: "::dep".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: ok_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });
        program.functions.push(FunctionInfo {
            name: "::lib/call-alias".to_string(),
            namespace: "::lib".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: alias_name,
                },
                Instruction::Call {
                    dest: 1,
                    function: 0,
                    args_start: 2,
                    args_count: 0,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let mut function_mapping = IndexMap::new();
        function_mapping.insert("::dep/target".to_string(), 0);
        function_mapping.insert("::dep/target/0".to_string(), 0);
        function_mapping.insert("::lib/call-alias".to_string(), 1);
        function_mapping.insert("::lib/call-alias/0".to_string(), 1);

        let make_vm = |conf| {
            let mut vm = VirtualMachine::new(
                Arc::new(program.clone()),
                None,
                Arc::new(function_mapping.clone()),
                Arc::new(IndexMap::<String, FunctionId>::new()),
                Arc::new(IndexMap::<(String, String), String>::new()),
                Arc::new(CoreVariableRegistry::new()),
                Some(conf),
            );
            vm.namespace_variables
                .entry("::lib".to_string())
                .or_default()
                .insert("http-request".to_string(), val!("::dep/target"));
            vm.set_current_namespace("::live".to_string());
            vm
        };

        let mut interp_vm = make_vm(val!({"jit": {"mode": "disabled"}}));
        assert_eq!(
            interp_vm.execute_compiled_user_function(1, &[]).unwrap(),
            val!("ok")
        );

        let mut jit_vm = make_vm(val!({"jit": {"mode": "enabled", "threshold": 1}}));
        assert_eq!(
            jit_vm.execute_compiled_user_function(1, &[]).unwrap(),
            val!("ok")
        );
        assert!(jit_vm.jit_has_compiled_function(1));
        assert_eq!(jit_vm.get_current_namespace(), "::live");
    }

    #[test]
    fn jitted_function_preserves_param_scope_for_lazy_core_lookup() {
        let mut program = BytecodeProgram::new();
        let predicate_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("predicate")));
        let true_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Bool(true)));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));

        program.functions.push(FunctionInfo {
            name: "::hot::coll/call-predicate".to_string(),
            namespace: "::hot::coll".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["predicate".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: predicate_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Call {
                    dest: 2,
                    function: 0,
                    args_start: 1,
                    args_count: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let mut function_mapping = IndexMap::new();
        function_mapping.insert("::hot::coll/call-predicate".to_string(), 0);
        function_mapping.insert("::hot::coll/call-predicate/1".to_string(), 0);

        let predicate = Val::Box(Box::new(crate::lang::bytecode::LambdaInfo {
            parameters: vec!["x".to_string()],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: true_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: false,
            used_registers: vec![0],
        }));

        let make_vm = |conf| {
            VirtualMachine::new(
                Arc::new(program.clone()),
                None,
                Arc::new(function_mapping.clone()),
                Arc::new(IndexMap::<String, FunctionId>::new()),
                Arc::new(IndexMap::<(String, String), String>::new()),
                Arc::new(CoreVariableRegistry::new()),
                Some(conf),
            )
        };

        let mut interp_vm = make_vm(val!({"jit": {"mode": "disabled", "threshold": 1}}));
        let interp = interp_vm
            .execute_compiled_user_function(0, std::slice::from_ref(&predicate))
            .expect("interpreter should resolve predicate from probe scope");
        assert_eq!(interp, Val::Bool(true));

        let mut jit_vm = make_vm(val!({"jit": {"mode": "enabled", "threshold": 1}}));
        let jitted = jit_vm
            .execute_compiled_user_function(0, &[predicate])
            .expect("JIT should preserve predicate parameter scope for VM callbacks");
        assert_eq!(jitted, Val::Bool(true));
        assert!(jit_vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_function_with_threshold_1() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/add_one".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[val!(9)]).unwrap();
        assert_eq!(result, val!(10));
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_branching_function() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/is_positive".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: zero_const,
                },
                Instruction::Gt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: one_const,
                },
                Instruction::Return { value: 3 },
                Instruction::LoadConst {
                    dest: 4,
                    constant: zero_const,
                },
                Instruction::Return { value: 4 },
            ],
            register_count: 5,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let positive = vm.execute_compiled_user_function(0, &[val!(7)]).unwrap();
        let non_positive = vm.execute_compiled_user_function(0, &[val!(0)]).unwrap();
        assert_eq!(positive, val!(1));
        assert_eq!(non_positive, val!(0));
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_self_recursive_int_function() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_down".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: zero_const,
                },
                Instruction::Return { value: 3 },
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadVar {
                    dest: 5,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 7,
                    left: 5,
                    right: 6,
                },
                Instruction::CallUserFunction {
                    dest: 8,
                    function_id: 0,
                    args_start: 7,
                    args_count: 1,
                },
                Instruction::Add {
                    dest: 9,
                    left: 4,
                    right: 8,
                },
                Instruction::Return { value: 9 },
            ],
            register_count: 10,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[val!(5)]).unwrap();
        assert_eq!(result, val!(15));
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_self_tail_recursive_function() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        let acc_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("acc")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_tail".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["n".to_string(), "acc".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                Instruction::LoadVar {
                    dest: 3,
                    var_name: acc_name,
                },
                Instruction::Return { value: 3 },
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 6,
                    left: 4,
                    right: 5,
                },
                Instruction::LoadVar {
                    dest: 7,
                    var_name: acc_name,
                },
                Instruction::LoadVar {
                    dest: 8,
                    var_name: n_name,
                },
                Instruction::Add {
                    dest: 7,
                    left: 7,
                    right: 8,
                },
                Instruction::TailCall {
                    function_id: 0,
                    args_start: 6,
                    args_count: 2,
                },
            ],
            register_count: 9,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!(5), val!(0)])
            .unwrap();
        assert_eq!(result, val!(15));
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn cached_bytecode_engine_path_triggers_jit() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/add-one".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });
        let mut function_mapping = IndexMap::new();
        function_mapping.insert("::test/add-one".to_string(), 0);
        function_mapping.insert("::test/add-one/1".to_string(), 0);
        let ast_program = Program {
            namespaces: IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::new(),
        };
        let artifacts = CompilationArtifacts {
            program,
            function_mapping,
            core_functions: IndexMap::new(),
            type_implementations: IndexMap::new(),
            ast_program: ast_program.clone(),
            hot_ast: HotAst::from_program(ast_program),
        };
        let cached = Engine::artifacts_to_cached_bytecode(artifacts);
        let before = super::global_jit_compile_count();
        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});

        let result = Engine::call_function_with_cached_bytecode(
            "::test/add-one",
            &[val!(41)],
            cached,
            Some(&conf),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("cached-bytecode call");

        assert_eq!(result, val!(42));
        assert!(super::global_jit_compile_count() > before);
    }

    #[test]
    fn task_cached_bytecode_engine_path_triggers_jit() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/add-one".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });
        let mut function_mapping = IndexMap::new();
        function_mapping.insert("::test/add-one".to_string(), 0);
        function_mapping.insert("::test/add-one/1".to_string(), 0);
        let ast_program = Program {
            namespaces: IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::new(),
        };
        let artifacts = CompilationArtifacts {
            program,
            function_mapping,
            core_functions: IndexMap::new(),
            type_implementations: IndexMap::new(),
            ast_program: ast_program.clone(),
            hot_ast: HotAst::from_program(ast_program),
        };
        let cached = Engine::artifacts_to_cached_bytecode(artifacts);
        let before = super::global_jit_compile_count();
        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let (_tx, rx) = tokio::sync::mpsc::channel(1);

        let result = Engine::call_function_with_cached_bytecode_and_task(
            "::test/add-one",
            &[val!(9)],
            cached,
            Some(&conf),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Arc::new(parking_lot::Mutex::new(rx))),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("task cached-bytecode call");

        assert_eq!(result, val!(10));
        assert!(super::global_jit_compile_count() > before);
    }

    #[test]
    fn jitted_function_can_call_known_core_wrappers() {
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let two_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(2)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/add".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["x".to_string(), "y".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/add_three".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: two_const,
                },
                Instruction::CallUserFunction {
                    dest: 4,
                    function_id: 0,
                    args_start: 2,
                    args_count: 2,
                },
                Instruction::Return { value: 4 },
            ],
            register_count: 5,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(1, &[val!(5)]).unwrap();

        assert_eq!(result, val!(8));
        assert!(vm.jit_has_compiled_function(1));
    }

    #[test]
    fn jitted_function_can_call_known_core_wrappers_with_dec() {
        let mut program = BytecodeProgram::new();
        let delta_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("1.25", Context::default()).unwrap(),
        )));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/add".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["x".to_string(), "y".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/add_dec_wrapper".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: delta_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let input = Val::Dec(D256::from_str("2.75", Context::default()).unwrap());
        let expected = Val::Dec(D256::from_str("4.00", Context::default()).unwrap());
        let result = vm.execute_compiled_user_function(1, &[input]).unwrap();

        assert_eq!(result, expected);
        assert!(vm.jit_has_compiled_function(1));
    }

    #[test]
    fn unsupported_function_falls_back_when_not_forced() {
        let mut program = BytecodeProgram::new();
        let ns_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("::test")));
        let var_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/load-global".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadGlobal {
                    dest: 0,
                    namespace: ns_const,
                    var_name: var_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        vm.namespace_variables
            .entry("::test".to_string())
            .or_default()
            .insert("x".to_string(), val!(42));
        let result = vm.execute_compiled_user_function(0, &[]).unwrap();
        assert_eq!(result, val!(42));
        assert!(!vm.jit_has_compiled_function(0));
    }

    #[test]
    fn unsupported_function_falls_back_with_low_threshold() {
        let mut program = BytecodeProgram::new();
        let ns_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("::test")));
        let var_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        program.functions.push(FunctionInfo {
            name: "::test/load-global".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadGlobal {
                    dest: 0,
                    namespace: ns_const,
                    var_name: var_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        vm.namespace_variables
            .entry("::test".to_string())
            .or_default()
            .insert("x".to_string(), val!(42));
        let result = vm.execute_compiled_user_function(0, &[]).unwrap();
        assert_eq!(result, val!(42));
        assert!(!vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_self_recursive_dec_function() {
        // sum_down(n: Dec): Dec = if n < 1.0 then 0.0 else n + sum_down(n - 1.0)
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("1.0", Context::default()).unwrap(),
        )));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("0.0", Context::default()).unwrap(),
        )));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_down_dec".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                // r0 = n
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                // r1 = 1.0
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                // r2 = n < 1.0
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                // if not (n < 1.0) goto 6
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                // base case: return 0.0
                Instruction::LoadConst {
                    dest: 3,
                    constant: zero_const,
                },
                Instruction::Return { value: 3 },
                // recursive case: r4 = n, r5 = n - 1.0
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadVar {
                    dest: 5,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 7,
                    left: 5,
                    right: 6,
                },
                // r8 = sum_down_dec(n - 1.0)
                Instruction::CallUserFunction {
                    dest: 8,
                    function_id: 0,
                    args_start: 7,
                    args_count: 1,
                },
                // r9 = n + sum_down_dec(n - 1.0)
                Instruction::Add {
                    dest: 9,
                    left: 4,
                    right: 8,
                },
                Instruction::Return { value: 9 },
            ],
            register_count: 10,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let input = Val::Dec(D256::from_str("5.0", Context::default()).unwrap());
        let result = vm.execute_compiled_user_function(0, &[input]).unwrap();
        let expected = Val::Dec(D256::from_str("15.0", Context::default()).unwrap());
        assert_eq!(result, expected);
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_self_tail_recursive_dec_function() {
        // sum_tail(n: Dec, acc: Dec): Dec = if n < 1.0 then acc else sum_tail(n - 1.0, acc + n)
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("1.0", Context::default()).unwrap(),
        )));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        let acc_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("acc")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_tail_dec".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["n".to_string(), "acc".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                // r0 = n
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                // r1 = 1.0
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                // r2 = n < 1.0
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                // if not (n < 1.0) goto 6
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                // base case: return acc
                Instruction::LoadVar {
                    dest: 3,
                    var_name: acc_name,
                },
                Instruction::Return { value: 3 },
                // recursive case: r4 = n - 1.0, r5 = acc + n
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 6,
                    left: 4,
                    right: 5,
                },
                Instruction::LoadVar {
                    dest: 7,
                    var_name: acc_name,
                },
                Instruction::LoadVar {
                    dest: 8,
                    var_name: n_name,
                },
                Instruction::Add {
                    dest: 7,
                    left: 7,
                    right: 8,
                },
                // tail call: sum_tail_dec(n - 1.0, acc + n)
                Instruction::TailCall {
                    function_id: 0,
                    args_start: 6,
                    args_count: 2,
                },
            ],
            register_count: 9,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let n = Val::Dec(D256::from_str("5.0", Context::default()).unwrap());
        let acc = Val::Dec(D256::from_str("0.0", Context::default()).unwrap());
        let result = vm.execute_compiled_user_function(0, &[n, acc]).unwrap();
        let expected = Val::Dec(D256::from_str("15.0", Context::default()).unwrap());
        assert_eq!(result, expected);
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn vm_executes_jitted_cross_function_call() {
        // fn#0: double(x) = x + x
        // fn#1: main(x)   = double(x)   (calls fn#0 via VM callback)
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        // fn#0: double(x) = x + x
        program.functions.push(FunctionInfo {
            name: "::test/double".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        // fn#1: main(x) = double(x)
        program.functions.push(FunctionInfo {
            name: "::test/main".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::CallUserFunction {
                    dest: 1,
                    function_id: 0,
                    args_start: 0,
                    args_count: 1,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        // main(5) = double(5) = 10
        let result = vm.execute_compiled_user_function(1, &[val!(5)]).unwrap();
        assert_eq!(result, val!(10));
        assert!(vm.jit_has_compiled_function(1));
    }

    #[test]
    fn vm_executes_jitted_cross_function_with_arithmetic() {
        // fn#0: double(x) = x + x
        // fn#1: main(x)   = double(x) + 1   (arithmetic on VM callback result)
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/double".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: x_name,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/main".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::CallUserFunction {
                    dest: 1,
                    function_id: 0,
                    args_start: 0,
                    args_count: 1,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: one_const,
                },
                Instruction::Add {
                    dest: 3,
                    left: 1,
                    right: 2,
                },
                Instruction::Return { value: 3 },
            ],
            register_count: 4,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        // double(5) + 1 = 11 (preserves Int type through OwnedVal general helper)
        let result = vm.execute_compiled_user_function(1, &[val!(5)]).unwrap();
        assert_eq!(result, val!(11));
    }

    #[test]
    fn benchmark_jit_tail_recursive_sum() {
        // sum_tail(n, acc): if n < 1 then acc else sum_tail(n-1, acc+n)
        // Uses JIT-only instructions (Lt, Sub) so this only tests JIT performance
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        let acc_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("acc")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_tail".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["n".to_string(), "acc".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                Instruction::LoadVar {
                    dest: 3,
                    var_name: acc_name,
                },
                Instruction::Return { value: 3 },
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 5,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 6,
                    left: 4,
                    right: 5,
                },
                Instruction::LoadVar {
                    dest: 7,
                    var_name: acc_name,
                },
                Instruction::LoadVar {
                    dest: 8,
                    var_name: n_name,
                },
                Instruction::Add {
                    dest: 7,
                    left: 7,
                    right: 8,
                },
                Instruction::TailCall {
                    function_id: 0,
                    args_start: 6,
                    args_count: 2,
                },
            ],
            register_count: 9,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        // Warmup
        let _ = vm
            .execute_compiled_user_function(0, &[val!(1000i64), val!(0)])
            .unwrap();

        for n in [10_000i64, 100_000, 1_000_000] {
            let start = std::time::Instant::now();
            let result = vm
                .execute_compiled_user_function(0, &[val!(n), val!(0)])
                .unwrap();
            let elapsed = start.elapsed();
            let expected = n * (n + 1) / 2;
            assert_eq!(result, val!(expected));
            eprintln!("  sum_tail({}): {:?}", n, elapsed);
        }
        eprintln!("  (compare: VM sum-tail(100000) = ~1259ms from Hot CLI)");
    }

    #[test]
    fn benchmark_jit_self_recursive_sum() {
        // sum_down(n): if n < 1 then 0 else n + sum_down(n-1)
        let mut program = BytecodeProgram::new();
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        program.functions.push(FunctionInfo {
            name: "::test/sum_down".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                },
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::JumpIfNot {
                    condition: 2,
                    offset: 6,
                },
                Instruction::LoadConst {
                    dest: 3,
                    constant: zero_const,
                },
                Instruction::Return { value: 3 },
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                },
                Instruction::LoadVar {
                    dest: 5,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 6,
                    constant: one_const,
                },
                Instruction::Sub {
                    dest: 7,
                    left: 5,
                    right: 6,
                },
                Instruction::CallUserFunction {
                    dest: 8,
                    function_id: 0,
                    args_start: 7,
                    args_count: 1,
                },
                Instruction::Add {
                    dest: 9,
                    left: 4,
                    right: 8,
                },
                Instruction::Return { value: 9 },
            ],
            register_count: 10,
            source: None,
        });

        let n = 20_000i64;
        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        // Warmup
        let _ = vm
            .execute_compiled_user_function(0, &[val!(100i64)])
            .unwrap();

        let start = std::time::Instant::now();
        let result = vm.execute_compiled_user_function(0, &[val!(n)]).unwrap();
        let elapsed = start.elapsed();

        let expected = n * (n + 1) / 2;
        assert_eq!(result, val!(expected));

        eprintln!("\n=== JIT self-recursive sum_down({}) ===", n);
        eprintln!("  Time: {:?}", elapsed);
        eprintln!("  Calls: ~{}", n);
    }

    #[test]
    fn jit_inlines_known_lazy_branch_call() {
        use crate::lang::bytecode::LambdaInfo;

        // Test: max(a, b) = if(gt(a, b), a, b) — using thunks for the lazy branches.
        // Generic lazy calls can compile through the VM callback, but inlining
        // a branch selector is only sound for a function whose semantics are known.
        let mut program = BytecodeProgram::new();
        let a_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("a")));
        let b_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("b")));
        let if_name = program.constants.len() as u32;
        program.constants.push(Constant::FunctionRef("if".into()));

        // Thunk for "then" branch: { a }  (just returns captured var a)
        let then_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: a_name,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec!["a".to_string()],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };

        // Thunk for "else" branch: { b }
        let else_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: b_name,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec!["b".to_string()],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };

        let then_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(then_thunk))));
        let else_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(else_thunk))));

        // Function 0: max(a, b)
        program.functions.push(FunctionInfo {
            name: "::test/max".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                // r0 = a, r1 = b
                Instruction::LoadVar {
                    dest: 0,
                    var_name: a_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: b_name,
                },
                // r2 = gt(a, b)
                Instruction::Gt {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                // r3 = then thunk, r4 = else thunk
                Instruction::LoadConst {
                    dest: 3,
                    constant: then_const,
                },
                Instruction::LoadConst {
                    dest: 4,
                    constant: else_const,
                },
                // r6 = if(r2, r3, r4) — named call that resolves to function 1.
                Instruction::LoadConst {
                    dest: 5,
                    constant: if_name,
                },
                Instruction::Call {
                    dest: 6,
                    function: 5,
                    args_start: 2,
                    args_count: 3,
                },
                Instruction::Return { value: 6 },
            ],
            register_count: 7,
            source: None,
        });

        // Function 1: if(pred, lazy then, lazy else)
        // This needs to exist in the program so the JIT can read lazy_params.
        program.functions.push(FunctionInfo {
            name: "::hot::bool/if".to_string(),
            namespace: "::hot::bool".to_string(),
            arity: 3,
            is_variadic: false,
            param_names: vec!["pred".to_string(), "then".to_string(), "else".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, true, true],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(10), val!(5)])
                .unwrap(),
            val!(10)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(3), val!(7)])
                .unwrap(),
            val!(7)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_inlines_fib_recursive_with_if_thunks() {
        use crate::lang::bytecode::LambdaInfo;

        // fib(n) = if(lte(n, 1), n, add(fib(sub(n,1)), fib(sub(n,2))))
        let mut program = BytecodeProgram::new();
        let n_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("n")));
        let one_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(1)));
        let two_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(2)));

        // "then" thunk: { n }
        let then_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec!["n".to_string()],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };

        // "else" thunk: { add(fib(sub(n, 1)), fib(sub(n, 2))) }
        let else_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                }, // r0 = n
                Instruction::LoadConst {
                    dest: 1,
                    constant: one_const,
                }, // r1 = 1
                Instruction::Sub {
                    dest: 2,
                    left: 0,
                    right: 1,
                }, // r2 = n - 1
                Instruction::CallUserFunction {
                    dest: 3,
                    function_id: 0,
                    args_start: 2,
                    args_count: 1,
                }, // r3 = fib(n-1)
                Instruction::LoadVar {
                    dest: 4,
                    var_name: n_name,
                }, // r4 = n
                Instruction::LoadConst {
                    dest: 5,
                    constant: two_const,
                }, // r5 = 2
                Instruction::Sub {
                    dest: 6,
                    left: 4,
                    right: 5,
                }, // r6 = n - 2
                Instruction::CallUserFunction {
                    dest: 7,
                    function_id: 0,
                    args_start: 6,
                    args_count: 1,
                }, // r7 = fib(n-2)
                Instruction::Add {
                    dest: 8,
                    left: 3,
                    right: 7,
                }, // r8 = fib(n-1) + fib(n-2)
                Instruction::Return { value: 8 },
            ],
            register_count: 9,
            capture_vars: vec!["n".to_string()],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
        };

        let then_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(then_thunk))));
        let else_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(else_thunk))));

        // Function 0: fib(n: Int) -> Int
        // lte(n, 1) is done inline with Lt + Not (lte = not(gt))
        // Actually let's use Gt for simplicity: if(gt(n, 1), else_thunk, then_thunk) -- swap order
        // Or better: just use Gt(1, n) which is lt(n, 1)... no.
        // Let's use: r0=n, r1=1, r2=Gt(r0, r1) → n > 1
        // if(n > 1, else_thunk, then_thunk)
        // Wait, we need lte(n,1) as the pred. lte(n,1) = !gt(n,1).
        // Simpler: Lt(n, 2) means n < 2, which for Int is equivalent to n <= 1.
        program.functions.push(FunctionInfo {
            name: "::test/fib".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["n".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: n_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: two_const,
                },
                Instruction::Lt {
                    dest: 2,
                    left: 0,
                    right: 1,
                }, // n < 2 (i.e., n <= 1)
                Instruction::LoadConst {
                    dest: 3,
                    constant: then_const,
                }, // then thunk: { n }
                Instruction::LoadConst {
                    dest: 4,
                    constant: else_const,
                }, // else thunk: { fib(n-1)+fib(n-2) }
                Instruction::CallUserFunction {
                    dest: 5,
                    function_id: 1,
                    args_start: 2,
                    args_count: 3,
                },
                Instruction::Return { value: 5 },
            ],
            register_count: 6,
            source: None,
        });

        // Function 1: if(pred, lazy then, lazy else)
        program.functions.push(FunctionInfo {
            name: "::hot::bool/if".to_string(),
            namespace: "::hot::bool".to_string(),
            arity: 3,
            is_variadic: false,
            param_names: vec!["pred".to_string(), "then".to_string(), "else".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, true, true],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(0)]).unwrap(),
            val!(0)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(1)]).unwrap(),
            val!(1)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(5)]).unwrap(),
            val!(5)
        );
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(10)]).unwrap(),
            val!(55)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_returns_owned_values_for_lazy_branch_results() {
        use crate::lang::bytecode::LambdaInfo;

        let mut program = BytecodeProgram::new();
        let flag_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("flag")));
        let admin_value = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!({"role": "admin"})));
        let member_value = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!({"role": "member"})));

        let then_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: admin_value,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };
        let else_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: member_value,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };
        let then_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(then_thunk))));
        let else_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(else_thunk))));

        program.functions.push(FunctionInfo {
            name: "::test/choose-role".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["flag".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: flag_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: then_const,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: else_const,
                },
                Instruction::CallUserFunction {
                    dest: 3,
                    function_id: 1,
                    args_start: 0,
                    args_count: 3,
                },
                Instruction::Return { value: 3 },
            ],
            register_count: 4,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::hot::bool/if".to_string(),
            namespace: "::hot::bool".to_string(),
            arity: 3,
            is_variadic: false,
            param_names: vec!["pred".to_string(), "then".to_string(), "else".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, true, true],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        let jit_conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut jit_vm = make_test_vm(program, jit_conf);

        let admin = jit_vm
            .execute_compiled_user_function(0, &[val!(true)])
            .unwrap();
        assert_eq!(admin, val!({"role": "admin"}));

        let member = jit_vm
            .execute_compiled_user_function(0, &[val!(false)])
            .unwrap();
        assert_eq!(member, val!({"role": "member"}));
        assert!(jit_vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_compiles_generic_lazy_call_via_vm_callback() {
        use crate::lang::bytecode::LambdaInfo;

        let mut program = BytecodeProgram::new();
        let pred_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("pred")));
        let ignored_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("ignored")));
        let result_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(99)));

        let lazy_arg = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: ignored_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };
        let lazy_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(lazy_arg))));

        program.functions.push(FunctionInfo {
            name: "::test/calls-lazy-helper".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["pred".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: pred_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: lazy_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 1,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/lazy-helper".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["pred".to_string(), "value".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, true],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: result_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(0, &[val!(true)]).unwrap(),
            val!(99)
        );
        assert!(vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_bails_on_unconsumed_lazy_thunk_shape() {
        use crate::lang::bytecode::LambdaInfo;

        let mut program = BytecodeProgram::new();
        let value_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("unused")));
        let thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: value_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };
        let thunk_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(thunk))));
        program.functions.push(FunctionInfo {
            name: "::test/returns-thunk".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: thunk_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let sig = TypeSig::from_args(&[]);
        let err = match compile_supported_function(&program, 0, &program.functions[0], &sig) {
            Ok(_) => panic!("unconsumed lazy thunk should not compile"),
            Err(err) => err,
        };
        assert!(
            err.contains("lazy thunk bytecode shape"),
            "unexpected bailout: {err}"
        );
    }

    #[test]
    fn jit_bails_when_lazy_thunk_escapes_into_vec() {
        use crate::lang::bytecode::LambdaInfo;

        let mut program = BytecodeProgram::new();
        let value_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("escaped")));
        let thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: value_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec![],
            closure_env: AHashMap::new(),
            defining_namespace: "::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![0],
        };
        let thunk_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(Val::Box(Box::new(thunk))));
        program.functions.push(FunctionInfo {
            name: "::test/vec-with-thunk".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: thunk_const,
                },
                Instruction::MakeVec {
                    dest: 1,
                    elements_start: 0,
                    count: 1,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let sig = TypeSig::from_args(&[]);
        let err = match compile_supported_function(&program, 0, &program.functions[0], &sig) {
            Ok(_) => panic!("lazy thunk escape should not compile"),
            Err(err) => err,
        };
        assert!(
            err.contains("lazy thunk bytecode shape"),
            "unexpected bailout: {err}"
        );
    }

    #[test]
    fn jit_matches_interpreter_for_owned_const_return() {
        let mut program = BytecodeProgram::new();
        let value_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!({"kind": "owned"})));
        program.functions.push(FunctionInfo {
            name: "::test/owned-return".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: value_const,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let interp_conf = val!({"jit": {"mode": "disabled", "threshold": 1}});
        let jit_conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut interp_vm = make_test_vm(program.clone(), interp_conf);
        let mut jit_vm = make_test_vm(program, jit_conf);

        let interp = interp_vm.execute_compiled_user_function(0, &[]).unwrap();
        let jitted = jit_vm.execute_compiled_user_function(0, &[]).unwrap();
        assert_eq!(jitted, interp);
        assert!(jit_vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_matches_interpreter_for_non_lazy_hotlib_call() {
        let mut program = BytecodeProgram::new();
        let function_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!("::hot::coll/length")));
        let xs_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("xs")));
        program.functions.push(FunctionInfo {
            name: "::test/length-of".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["xs".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: function_const,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: xs_name,
                },
                Instruction::MakeVec {
                    dest: 2,
                    elements_start: 1,
                    count: 1,
                },
                Instruction::CallLibBuiltin {
                    dest: 3,
                    function: 0,
                    args: 2,
                },
                Instruction::Return { value: 3 },
            ],
            register_count: 4,
            source: None,
        });

        let interp_conf = val!({"jit": {"mode": "disabled", "threshold": 1}});
        let jit_conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut interp_vm = make_test_vm(program.clone(), interp_conf);
        let mut jit_vm = make_test_vm(program, jit_conf);
        let args = [val!([1, 2, 3, 4])];

        let interp = interp_vm.execute_compiled_user_function(0, &args).unwrap();
        let jitted = jit_vm.execute_compiled_user_function(0, &args).unwrap();
        assert_eq!(jitted, interp);
        assert_eq!(jitted, val!(4));
        assert!(jit_vm.jit_has_compiled_function(0));
    }

    #[test]
    fn jit_bails_on_jit_bailout_hotlib_policy() {
        let mut program = BytecodeProgram::new();
        let function_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!("::hot::store/put")));
        program.functions.push(FunctionInfo {
            name: "::test/store-put-wrapper".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: function_const,
                },
                Instruction::MakeVec {
                    dest: 1,
                    elements_start: 0,
                    count: 0,
                },
                Instruction::CallLibBuiltin {
                    dest: 2,
                    function: 0,
                    args: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let sig = TypeSig::from_args(&[]);
        let err = match compile_supported_function(&program, 0, &program.functions[0], &sig) {
            Ok(_) => panic!("JIT bailout hotlib wrapper should not compile"),
            Err(err) => err,
        };
        assert!(err.contains("hotlib_policy"), "unexpected bailout: {err}");

        program.functions.push(FunctionInfo {
            name: "::test/calls-vm-aware-hotlib-wrapper".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::CallUserFunction {
                    dest: 0,
                    function_id: 0,
                    args_start: 0,
                    args_count: 0,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });
        let err = match compile_supported_function(&program, 1, &program.functions[1], &sig) {
            Ok(_) => panic!("caller of JIT bailout hotlib wrapper should not compile"),
            Err(err) => err,
        };
        assert!(err.contains("hotlib_policy"), "unexpected bailout: {err}");
    }

    #[test]
    fn jit_allows_vm_callback_hotlib_policy() {
        let mut program = BytecodeProgram::new();
        let function_const = program.constants.len() as u32;
        program
            .constants
            .push(Constant::Val(val!("::hot::coll/reduce")));
        program.functions.push(FunctionInfo {
            name: "::test/reduce-wrapper".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: function_const,
                },
                Instruction::MakeVec {
                    dest: 1,
                    elements_start: 0,
                    count: 0,
                },
                Instruction::CallLibBuiltin {
                    dest: 2,
                    function: 0,
                    args: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let sig = TypeSig::from_args(&[]);
        let result = compile_supported_function(&program, 0, &program.functions[0], &sig);
        assert!(
            result.is_ok(),
            "VM callback hotlib policy should compile: {:?}",
            result.err()
        );
    }

    #[test]
    fn jit_compiles_str_starts_with() {
        let mut program = BytecodeProgram::new();
        let hello_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("hello world")));
        let prefix_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("hello")));
        let bad_prefix_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("world")));

        program.functions.push(FunctionInfo {
            name: "::test/starts-with-hello".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: hello_const,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: prefix_const,
                },
                Instruction::StrStartsWith {
                    dest: 2,
                    string: 0,
                    prefix: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/starts-with-world".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: hello_const,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: bad_prefix_const,
                },
                Instruction::StrStartsWith {
                    dest: 2,
                    string: 0,
                    prefix: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(0, &[]).unwrap(),
            val!(true)
        );
        assert_eq!(
            vm.execute_compiled_user_function(1, &[]).unwrap(),
            val!(false)
        );
    }

    #[test]
    fn jit_div_by_zero_propagates_error_not_panic() {
        let mut program = BytecodeProgram::new();
        let zero_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(0)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/div".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/div_by_zero".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: zero_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0, // ::hot::math/div
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        let result = vm.execute_compiled_user_function(1, &[val!(10)]).unwrap();
        assert!(
            result.is_err(),
            "div(10, 0) should return Result.Err, got {:?}",
            result
        );
    }

    #[test]
    fn jit_mixed_int_dec_arithmetic() {
        let mut program = BytecodeProgram::new();
        let dec_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Dec(
            D256::from_str("1.5", Context::default()).unwrap(),
        )));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/add_mixed".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: dec_const,
                },
                Instruction::Add {
                    dest: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let sig = TypeSig::from_args(&[val!(10)]);
        let compiled =
            compile_supported_function(&program, 0, &program.functions[0], &sig).unwrap();
        let result = compiled.call(&[val!(10)]).unwrap();
        let expected = Val::Dec(D256::from_str("11.5", Context::default()).unwrap());
        assert_eq!(
            result, expected,
            "Int(10) + Dec(1.5) should produce Dec(11.5)"
        );
    }

    #[test]
    fn jit_general_binop_non_numeric_falls_back_to_vm() {
        let mut program = BytecodeProgram::new();
        let null_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(Val::Null));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/add".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/add_null".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: null_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        // Non-force mode: JIT should bail out gracefully and fall back to VM
        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);

        let result = vm.execute_compiled_user_function(1, &[val!("hello")]);
        assert!(
            result.is_ok(),
            "add(Str, Null) should fall back to VM, not panic; got {:?}",
            result
        );
        assert!(
            !vm.jit_has_compiled_function(1),
            "function should not be JIT-compiled"
        );
    }

    // --- KnownCoreCall tests ---

    #[test]
    fn jit_known_core_sub() {
        let mut program = BytecodeProgram::new();
        let three_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(3)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/sub".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/sub_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: three_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(10)]).unwrap(),
            val!(7)
        );
    }

    #[test]
    fn jit_known_core_mul() {
        let mut program = BytecodeProgram::new();
        let seven_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(7)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/mul".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/mul_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: seven_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(6)]).unwrap(),
            val!(42)
        );
    }

    #[test]
    fn jit_known_core_div() {
        let mut program = BytecodeProgram::new();
        let four_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(4)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/div".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/div_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: four_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(1, &[val!(10)]).unwrap();
        let expected = Val::Dec(D256::from_str("2.5", Context::default()).unwrap());
        assert_eq!(result, expected);
    }

    #[test]
    fn jit_known_core_mod() {
        let mut program = BytecodeProgram::new();
        let three_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(3)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/mod".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/mod_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: three_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(10)]).unwrap(),
            val!(1)
        );
    }

    #[test]
    fn jit_known_core_eq() {
        let mut program = BytecodeProgram::new();
        let five_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(5)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/eq".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/eq_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: five_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(5)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_ne() {
        let mut program = BytecodeProgram::new();
        let three_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(3)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/ne".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/ne_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: three_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(5)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_gt() {
        let mut program = BytecodeProgram::new();
        let three_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(3)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/gt".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/gt_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: three_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(5)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_gte() {
        let mut program = BytecodeProgram::new();
        let five_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(5)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/gte".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/gte_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: five_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(5)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_lt() {
        let mut program = BytecodeProgram::new();
        let five_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(5)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/lt".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/lt_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: five_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(3)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_lte() {
        let mut program = BytecodeProgram::new();
        let five_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(5)));
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::cmp/lte".to_string(),
            namespace: "::hot::cmp".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/lte_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: five_const,
                },
                Instruction::CallUserFunction {
                    dest: 2,
                    function_id: 0,
                    args_start: 0,
                    args_count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(5)]).unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_not() {
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::bool/not".to_string(),
            namespace: "::hot::bool".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/not_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::CallUserFunction {
                    dest: 1,
                    function_id: 0,
                    args_start: 0,
                    args_count: 1,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(false)])
                .unwrap(),
            val!(true)
        );
    }

    #[test]
    fn jit_known_core_is_zero() {
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::hot::math/is-zero".to_string(),
            namespace: "::hot::math".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });

        program.functions.push(FunctionInfo {
            name: "::test/is_zero_test".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::CallUserFunction {
                    dest: 1,
                    function_id: 0,
                    args_start: 0,
                    args_count: 1,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        assert_eq!(
            vm.execute_compiled_user_function(1, &[val!(0)]).unwrap(),
            val!(true)
        );
    }

    // --- DotAccess, MakeVec, MergeMaps, SetElement, VecAppend tests ---

    #[test]
    fn jit_dot_access_on_map() {
        let mut program = BytecodeProgram::new();
        let m_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("m")));
        let x_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/dot_access".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["m".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: m_name,
                },
                Instruction::DotAccess {
                    dest: 1,
                    object: 0,
                    property: x_const,
                },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!({"x": 42})])
            .unwrap();
        assert_eq!(result, val!(42));
    }

    #[test]
    fn jit_make_vec() {
        let mut program = BytecodeProgram::new();
        let a_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("a")));
        let b_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("b")));

        program.functions.push(FunctionInfo {
            name: "::test/make_vec".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: a_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: b_name,
                },
                Instruction::MakeVec {
                    dest: 2,
                    elements_start: 0,
                    count: 2,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!(1), val!(2)])
            .unwrap();
        assert_eq!(result, val!([1, 2]));
    }

    #[test]
    fn jit_merge_maps() {
        let mut program = BytecodeProgram::new();
        let a_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("a")));
        let b_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("b")));

        program.functions.push(FunctionInfo {
            name: "::test/merge_maps".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: a_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: b_name,
                },
                Instruction::MergeMaps { dest: 0, source: 1 },
                Instruction::Return { value: 0 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let a = val!({"x": 1});
        let b = val!({"y": 2});
        let result = vm.execute_compiled_user_function(0, &[a, b]).unwrap();
        assert_eq!(result, val!({"x": 1, "y": 2}));
    }

    #[test]
    fn jit_set_element_on_map() {
        let mut program = BytecodeProgram::new();
        let m_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("m")));
        let key_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("key")));
        let val_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(99)));

        program.functions.push(FunctionInfo {
            name: "::test/set_element".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["m".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: m_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: key_const,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: val_const,
                },
                Instruction::SetElement {
                    collection: 0,
                    index: 1,
                    value: 2,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[val!({})]).unwrap();
        assert_eq!(result, val!({"key": 99}));
    }

    #[test]
    fn jit_vec_append() {
        let mut program = BytecodeProgram::new();
        let v_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("v")));
        let val_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(42)));

        program.functions.push(FunctionInfo {
            name: "::test/vec_append".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["v".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: v_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: val_const,
                },
                Instruction::VecAppend { vec: 0, value: 1 },
                Instruction::Return { value: 0 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!([1, 2])])
            .unwrap();
        assert_eq!(result, val!([1, 2, 42]));
    }

    // --- Error propagation tests ---

    #[test]
    fn jit_wrap_ok_and_ensure_result() {
        // Function: fn(x) { WrapOk(x) then EnsureResult }
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/wrap-ok".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::WrapOk { dest: 1, src: 0 },
                Instruction::EnsureResult { dest: 2, value: 1 },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[val!(42)]).unwrap();
        // WrapOk wraps into an Ok result; EnsureResult should pass it through
        // The exact shape depends on VM semantics; verify it doesn't crash
        assert!(
            result != Val::Null,
            "WrapOk + EnsureResult should produce a non-null result"
        );
    }

    #[test]
    fn jit_begin_end_error_capture() {
        // Function that uses BeginErrorCapture / EndErrorCapture around a simple op
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/capture-error".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::BeginErrorCapture,
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::EndErrorCapture,
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[val!(99)]).unwrap();
        assert_eq!(result, val!(99));
    }

    // --- TemplateInterpolate test ---

    #[test]
    fn jit_template_interpolate() {
        // Function: fn(name) { `Hello, ${name}!` }
        let mut program = BytecodeProgram::new();
        let name_var = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("name")));
        let hello_part = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("Hello, ")));
        let excl_part = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("!")));

        program.functions.push(FunctionInfo {
            name: "::test/greet".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["name".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: hello_part,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: name_var,
                },
                Instruction::LoadConst {
                    dest: 2,
                    constant: excl_part,
                },
                Instruction::TemplateInterpolate {
                    dest: 3,
                    parts_start: 0,
                    parts_count: 3,
                },
                Instruction::Return { value: 3 },
            ],
            register_count: 4,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!("World")])
            .unwrap();
        assert_eq!(result, val!("Hello, World!"));

        let null_result = vm.execute_compiled_user_function(0, &[Val::Null]).unwrap();
        assert_eq!(null_result, val!("Hello, !"));
    }

    // --- DynamicDotAccess test ---

    #[test]
    fn jit_dynamic_dot_access() {
        // Function: fn(m, key) { m[key] } using DynamicDotAccess
        let mut program = BytecodeProgram::new();
        let m_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("m")));
        let key_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("key")));

        program.functions.push(FunctionInfo {
            name: "::test/dyn-access".to_string(),
            namespace: "::test".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["m".to_string(), "key".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false, false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: m_name,
                },
                Instruction::LoadVar {
                    dest: 1,
                    var_name: key_name,
                },
                Instruction::DynamicDotAccess {
                    dest: 2,
                    object: 0,
                    property: 1,
                },
                Instruction::Return { value: 2 },
            ],
            register_count: 3,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!({"x": 7, "y": 8}), val!("y")])
            .unwrap();
        assert_eq!(result, val!(8));
    }

    // --- DotSet test ---

    #[test]
    fn jit_dot_set() {
        // Function: fn(m) { m.x = 99; m }
        // DotSet mutates the object register in-place (no separate dest)
        let mut program = BytecodeProgram::new();
        let m_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("m")));
        let prop_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));
        let val_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(99)));

        program.functions.push(FunctionInfo {
            name: "::test/dot-set".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["m".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: m_name,
                },
                Instruction::LoadConst {
                    dest: 1,
                    constant: val_const,
                },
                Instruction::DotSet {
                    object: 0,
                    property: prop_name,
                    value: 1,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm
            .execute_compiled_user_function(0, &[val!({"x": 1})])
            .unwrap();
        assert_eq!(result, val!({"x": 99}));
    }

    // --- StoreGlobal test ---

    #[test]
    fn jit_store_global() {
        let mut program = BytecodeProgram::new();
        let ns_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("::test")));
        let var_name_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("my-var")));
        let val_const = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!(42)));

        program.functions.push(FunctionInfo {
            name: "::test/store-and-return".to_string(),
            namespace: "::test".to_string(),
            arity: 0,
            is_variadic: false,
            param_names: vec![],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![],
            flow_type: None,
            instructions: vec![
                Instruction::LoadConst {
                    dest: 0,
                    constant: val_const,
                },
                Instruction::StoreGlobal {
                    namespace: ns_const,
                    var_name: var_name_const,
                    value: 0,
                },
                Instruction::Return { value: 0 },
            ],
            register_count: 1,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        let result = vm.execute_compiled_user_function(0, &[]).unwrap();
        assert_eq!(result, val!(42));
    }

    // --- ExtractInnerVal test ---

    #[test]
    fn jit_extract_inner_val() {
        let mut program = BytecodeProgram::new();
        let x_name = program.constants.len() as u32;
        program.constants.push(Constant::Val(val!("x")));

        program.functions.push(FunctionInfo {
            name: "::test/extract-inner".to_string(),
            namespace: "::test".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["x".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![false],
            flow_type: None,
            instructions: vec![
                Instruction::LoadVar {
                    dest: 0,
                    var_name: x_name,
                },
                Instruction::ExtractInnerVal { dest: 1, src: 0 },
                Instruction::Return { value: 1 },
            ],
            register_count: 2,
            source: None,
        });

        let conf = val!({"jit": {"mode": "enabled", "threshold": 1}});
        let mut vm = make_test_vm(program, conf);
        // ExtractInnerVal on a plain value should pass it through
        let result = vm.execute_compiled_user_function(0, &[val!(42)]).unwrap();
        assert_eq!(result, val!(42));
    }
}
