//! Higher-order-function pipeline fusion (analysis core).
//!
//! This module recognizes pure collection pipelines such as
//!
//! ```text
//! range(1, n) |> filter((x) { is-zero(mod(x, 2)) })
//!             |> map((x) { mul(x, x) })
//!             |> reduce((acc, x) { add(acc, x) }, 0)
//! ```
//!
//! and lowers the lambda bodies into a small pure expression IR that can be
//! evaluated in one fused pass, avoiding intermediate `Vec` materialization and
//! per-element VM/lambda dispatch.
//!
//! This file is intentionally analysis-only: it builds and validates plans and
//! evaluates the expression IR, but performs no VM mutation. Detection runs once
//! per function at compile time; execution wiring lives in the VM and is gated
//! behind the `jit.hof.fusion` feature flag.
//!
//! Generality: operations and HOF roles are classified by the registry metadata
//! in [`crate::lang::hot::libmap`] (`pure_op`, `hof_role`), never by matching
//! function-name strings here. A literal name is only ever used as a registry
//! lookup key.

use crate::lang::bytecode::{BytecodeProgram, Constant, Instruction, LambdaInfo, RegisterId};
use crate::lang::hot::libmap::{self, HofExecution, HofStage, PureOp};
use crate::lang::hot::r#type::{HotResult, str_internal};
use crate::val::Val;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// Telemetry: count successful fused passes vs runtime de-opts (guard failures).
//
// These are process-global, monotonic, `Relaxed` counters intended only for
// diagnostics/observability. They are NOT a reliable signal for assertions:
// `cargo test` runs many fusion tests concurrently in one process, so a
// before/after delta around a single call can be polluted by other threads.
// Tests that need to prove fusion fired should assert on a deterministic,
// local signal instead (e.g. the detected `PipelinePlan`), not on these deltas.
static FUSED_RUNS: AtomicU64 = AtomicU64::new(0);
static FUSED_DEOPTS: AtomicU64 = AtomicU64::new(0);

/// Record a successful fused pipeline pass.
pub fn record_fused_run() {
    FUSED_RUNS.fetch_add(1, Ordering::Relaxed);
}

/// Record a runtime de-opt (a guard failed and execution fell back to the
/// interpreter).
pub fn record_fused_deopt() {
    FUSED_DEOPTS.fetch_add(1, Ordering::Relaxed);
}

/// Number of successful fused passes since process start.
pub fn fused_run_count() -> u64 {
    FUSED_RUNS.load(Ordering::Relaxed)
}

/// Number of runtime de-opts since process start.
pub fn fused_deopt_count() -> u64 {
    FUSED_DEOPTS.load(Ordering::Relaxed)
}

/// Reason a pipeline (or one of its lambdas) could not be fused. Reason codes
/// double as telemetry labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bailout {
    /// An instruction the lowering does not understand.
    UnsupportedInstruction(&'static str),
    /// A hotlib call whose op is not a fusible pure op.
    UnsupportedHotlib(String),
    /// Wrong operand arity for an op.
    BadArity(PureOp),
    /// A `LoadVar` of something that is not a known parameter (unknown capture).
    UnknownCapture(String),
    /// A constant kind that is not lowerable as a value.
    UnsupportedConst,
    /// A register was read before being defined within the lambda/region.
    UndefinedRegister(RegisterId),
    /// The pipeline shape (sources/stages/uses) was not a fusible chain.
    PipelineShape,
}

impl Bailout {
    /// Stable telemetry label for this bailout.
    pub fn label(&self) -> &'static str {
        match self {
            Bailout::UnsupportedInstruction(_) => "unsupported.hof_instruction",
            Bailout::UnsupportedHotlib(_) => "unsupported.hof_hotlib",
            Bailout::BadArity(_) => "unsupported.hof_arity",
            Bailout::UnknownCapture(_) => "unsupported.hof_capture",
            Bailout::UnsupportedConst => "unsupported.hof_const",
            Bailout::UndefinedRegister(_) => "unsupported.hof_undef_reg",
            Bailout::PipelineShape => "unsupported.hof_pipeline_shape",
        }
    }
}

/// Inferred static type of a lowered expression.
///
/// `Int`/`Bool` enable the inlined numeric/boolean fast path. `Other` covers
/// values whose static type the lowering does not track (parameters, `Dec`,
/// `Str`, records, and the results of arithmetic/concat/field ops); these are
/// evaluated dynamically and, when the inline path does not apply, delegated to
/// the exact registered hotlib for parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
    Int,
    Bool,
    Other,
}

/// Pure expression IR for a lowered lambda body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PureExpr {
    /// Reference to lambda parameter by index.
    Param(usize),
    ConstInt(i64),
    ConstBool(bool),
    /// Any other constant value (`Str`, `Dec`, `Null`, ...) used as a value.
    ConstVal(Val),
    /// A pure op applied to operands. `full_name` is the fully-qualified registry
    /// key of the backing hotlib `LibFn`, used for the parity fallback when the
    /// inline fast path does not apply.
    Op {
        op: PureOp,
        full_name: &'static str,
        args: Vec<PureExpr>,
    },
    /// Field/property projection from a record (`base.key`).
    Field {
        base: Box<PureExpr>,
        key: Arc<str>,
    },
    /// Template-literal interpolation: stringify each part and concatenate.
    Template(Vec<PureExpr>),
}

/// A lambda body lowered to pure expression IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoweredLambda {
    pub param_count: usize,
    pub expr: PureExpr,
    /// Flat postfix program compiled from `expr`, evaluated by the reusable
    /// stack machine in the hot loop.
    pub code: Vec<MicroOp>,
    /// Static type of the result where known (`Bool` for predicates, otherwise
    /// `Other`/`Int`). Used only as a hint; evaluation dispatches on the runtime
    /// value.
    pub result_type: ExprType,
}

impl LoweredLambda {
    fn new(param_count: usize, expr: PureExpr, result_type: ExprType) -> Self {
        let code = flatten_program(&expr);
        LoweredLambda {
            param_count,
            expr,
            code,
            result_type,
        }
    }
}

/// A runtime value handled by the Stage 1 fused evaluator. `Int`/`Bool` are kept
/// unboxed for the fast path; everything else (`Dec`, `Str`, records, ...) is
/// carried as an owned [`Val`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PureValue {
    Int(i64),
    Bool(bool),
    Other(Val),
}

impl PureValue {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            PureValue::Int(i) => Some(*i),
            // `from_val` normally canonicalizes `Int` to the unboxed variant,
            // but accept a boxed `Int` too so a hand-constructed `Other(Int)`
            // still takes the fast path instead of silently de-opting.
            PureValue::Other(Val::Int(i)) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            PureValue::Bool(b) => Some(*b),
            PureValue::Other(Val::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn into_val(self) -> Val {
        match self {
            PureValue::Int(i) => Val::Int(i),
            PureValue::Bool(b) => Val::Bool(b),
            PureValue::Other(v) => v,
        }
    }

    pub fn to_val(&self) -> Val {
        self.clone().into_val()
    }

    /// Wrap a `Val` produced by a source element or a hotlib call. Keeps `Int`
    /// and `Bool` on the fast path; everything else becomes `Other`.
    pub fn from_val(v: Val) -> PureValue {
        match v {
            Val::Int(i) => PureValue::Int(i),
            Val::Bool(b) => PureValue::Bool(b),
            other => PureValue::Other(other),
        }
    }
}

/// Outcome of evaluating the expression IR for a single element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Value(PureValue),
    /// A runtime condition (overflow into a different domain, divide/modulo by
    /// zero, an error value, a type the fast path does not handle, ...) requires
    /// falling back to the interpreter so exact semantics are reproduced.
    DeoptToInterpreter,
}

// ---------------------------------------------------------------------------
// Lambda lowering
// ---------------------------------------------------------------------------

/// Statically-known result type of an op. Predicates are `Bool`; numeric and
/// string ops are `Other` because the concrete runtime type (`Int`/`Dec`/`Str`)
/// is decided dynamically.
fn op_result_type(op: PureOp) -> ExprType {
    match op {
        PureOp::IsZero
        | PureOp::Eq
        | PureOp::Ne
        | PureOp::Lt
        | PureOp::Lte
        | PureOp::Gt
        | PureOp::Gte
        | PureOp::Not => ExprType::Bool,
        _ => ExprType::Other,
    }
}

/// Expected operand count of an op.
fn op_arity(op: PureOp) -> usize {
    match op {
        PureOp::IsZero | PureOp::Not | PureOp::Abs | PureOp::Neg => 1,
        _ => 2,
    }
}

/// Fully-qualified registry key for an op produced by a dedicated bytecode
/// instruction (operator forms compile to `Add`/`Eq`/... rather than a `Call`).
/// Used to fetch the backing `LibFn` for the parity fallback.
fn op_full_name(op: PureOp) -> &'static str {
    match op {
        PureOp::Add => "::hot::math/add",
        PureOp::Sub => "::hot::math/sub",
        PureOp::Mul => "::hot::math/mul",
        PureOp::Div => "::hot::math/div",
        PureOp::Mod => "::hot::math/mod",
        PureOp::Abs => "::hot::math/abs",
        PureOp::Min => "::hot::math/min",
        PureOp::Max => "::hot::math/max",
        PureOp::IsZero => "::hot::math/is-zero",
        PureOp::Eq => "::hot::cmp/eq",
        PureOp::Ne => "::hot::cmp/ne",
        PureOp::Lt => "::hot::cmp/lt",
        PureOp::Lte => "::hot::cmp/lte",
        PureOp::Gt => "::hot::cmp/gt",
        PureOp::Gte => "::hot::cmp/gte",
        PureOp::Not => "::hot::bool/not",
        PureOp::Neg => "::hot::math/neg",
        PureOp::Concat => "::hot::coll/concat",
    }
}

fn const_str(program: &BytecodeProgram, id: u32) -> Option<String> {
    match program.constants.get(id as usize)? {
        Constant::Val(Val::Str(s)) => Some(s.to_string()),
        Constant::StringRef(s) => Some(s.to_string()),
        Constant::FunctionRef(s) => Some(s.to_string()),
        _ => None,
    }
}

fn lambda_from_const(program: &BytecodeProgram, id: u32) -> Option<&LambdaInfo> {
    match program.constants.get(id as usize)? {
        Constant::Val(Val::Box(b)) => b.as_any().downcast_ref::<LambdaInfo>(),
        _ => None,
    }
}

/// Intermediate per-register tracking during lowering.
#[derive(Clone)]
enum RegContent {
    Expr(PureExpr, ExprType),
    /// A loaded string constant. Depending on its use it is either a `Call`
    /// target (function name) or a string *value* (e.g. a `concat` operand or a
    /// template part).
    Str(String),
}

/// Resolve a register to a value expression. A bare string constant becomes a
/// `Str` value; an already-lowered expression is cloned through.
fn reg_as_expr(
    regs: &std::collections::HashMap<RegisterId, RegContent>,
    reg: RegisterId,
) -> Result<(PureExpr, ExprType), Bailout> {
    match regs.get(&reg) {
        Some(RegContent::Expr(e, ty)) => Ok((e.clone(), *ty)),
        Some(RegContent::Str(s)) => Ok((PureExpr::ConstVal(Val::from(s.clone())), ExprType::Other)),
        None => Err(Bailout::UndefinedRegister(reg)),
    }
}

/// Lower a lambda body to pure expression IR, or bail with a reason.
pub fn lower_lambda(
    program: &BytecodeProgram,
    lambda: &LambdaInfo,
) -> Result<LoweredLambda, Bailout> {
    lower_callable_body(program, &lambda.parameters, &lambda.instructions)
}

fn lower_callable_body(
    program: &BytecodeProgram,
    params: &[String],
    instructions: &[Instruction],
) -> Result<LoweredLambda, Bailout> {
    use std::collections::HashMap;

    // Parameter name -> index.
    let mut param_index: HashMap<&str, usize> = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        param_index.insert(p.as_str(), i);
    }

    let mut regs: HashMap<RegisterId, RegContent> = HashMap::new();
    let mut result: Option<(PureExpr, ExprType)> = None;

    for ins in instructions {
        match ins {
            Instruction::LoadConst { dest, constant } => {
                match program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Int(i))) => {
                        regs.insert(
                            *dest,
                            RegContent::Expr(PureExpr::ConstInt(*i), ExprType::Int),
                        );
                    }
                    Some(Constant::Val(Val::Bool(b))) => {
                        regs.insert(
                            *dest,
                            RegContent::Expr(PureExpr::ConstBool(*b), ExprType::Bool),
                        );
                    }
                    Some(Constant::Val(Val::Str(s))) => {
                        regs.insert(*dest, RegContent::Str(s.to_string()));
                    }
                    Some(Constant::StringRef(s)) | Some(Constant::FunctionRef(s)) => {
                        regs.insert(*dest, RegContent::Str(s.to_string()));
                    }
                    // Other constant values (`Dec`, `Null`, ...) are usable as
                    // values directly.
                    Some(Constant::Val(v)) => {
                        regs.insert(
                            *dest,
                            RegContent::Expr(PureExpr::ConstVal(v.clone()), ExprType::Other),
                        );
                    }
                    _ => {
                        // Unknown constant kind; drop so a later use bails as an
                        // undefined register.
                        regs.remove(dest);
                    }
                }
            }
            Instruction::LoadVar { dest, var_name } => {
                let name = const_str(program, *var_name)
                    .ok_or(Bailout::UnsupportedInstruction("LoadVar:badname"))?;
                if let Some(&idx) = param_index.get(name.as_str()) {
                    // A parameter's runtime type is unknown statically (could be
                    // an `Int` range element, a record, a `Str` accumulator, ...).
                    regs.insert(
                        *dest,
                        RegContent::Expr(PureExpr::Param(idx), ExprType::Other),
                    );
                } else {
                    return Err(Bailout::UnknownCapture(name));
                }
            }
            Instruction::Move { dest, src } => {
                if let Some(c) = regs.get(src).cloned() {
                    regs.insert(*dest, c);
                } else {
                    return Err(Bailout::UndefinedRegister(*src));
                }
            }
            Instruction::Add { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Add)?;
            }
            Instruction::Sub { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Sub)?;
            }
            Instruction::Mul { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Mul)?;
            }
            Instruction::Eq { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Eq)?;
            }
            Instruction::Gt { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Gt)?;
            }
            Instruction::Lt { dest, left, right } => {
                lower_binop(&mut regs, *dest, *left, *right, PureOp::Lt)?;
            }
            Instruction::DotAccess {
                dest,
                object,
                property,
            } => {
                let (base, _ty) = reg_as_expr(&regs, *object)?;
                let key = const_str(program, *property)
                    .ok_or(Bailout::UnsupportedInstruction("DotAccess:badkey"))?;
                regs.insert(
                    *dest,
                    RegContent::Expr(
                        PureExpr::Field {
                            base: Box::new(base),
                            key: Arc::from(key.as_str()),
                        },
                        ExprType::Other,
                    ),
                );
            }
            Instruction::TemplateInterpolate {
                dest,
                parts_start,
                parts_count,
            } => {
                let mut parts = Vec::with_capacity(*parts_count as usize);
                for i in 0..*parts_count as u32 {
                    let (e, _ty) = reg_as_expr(&regs, parts_start + i)?;
                    parts.push(e);
                }
                regs.insert(
                    *dest,
                    RegContent::Expr(PureExpr::Template(parts), ExprType::Other),
                );
            }
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let name = match regs.get(function) {
                    Some(RegContent::Str(n)) => n.clone(),
                    Some(RegContent::Expr(_, _)) => {
                        return Err(Bailout::UndefinedRegister(*function));
                    }
                    None => return Err(Bailout::UndefinedRegister(*function)),
                };
                let desc = libmap::pure_op(&name)
                    .ok_or_else(|| Bailout::UnsupportedHotlib(name.clone()))?;
                let op = desc.op;
                if *args_count as usize != op_arity(op) {
                    return Err(Bailout::BadArity(op));
                }
                let mut operands = Vec::with_capacity(*args_count as usize);
                for i in 0..*args_count as u32 {
                    let (e, _ty) = reg_as_expr(&regs, args_start + i)?;
                    operands.push(e);
                }
                regs.insert(
                    *dest,
                    RegContent::Expr(
                        PureExpr::Op {
                            op,
                            full_name: desc.full_name,
                            args: operands,
                        },
                        op_result_type(op),
                    ),
                );
            }
            Instruction::Return { value } => {
                let (e, ty) = reg_as_expr(&regs, *value)?;
                result = Some((e, ty));
                break;
            }
            other => {
                return Err(Bailout::UnsupportedInstruction(instruction_name(other)));
            }
        }
    }

    let (expr, result_type) = result.ok_or(Bailout::UnsupportedInstruction("no-return"))?;
    Ok(LoweredLambda::new(params.len(), expr, result_type))
}

fn lower_binop(
    regs: &mut std::collections::HashMap<RegisterId, RegContent>,
    dest: RegisterId,
    left: RegisterId,
    right: RegisterId,
    op: PureOp,
) -> Result<(), Bailout> {
    let (l, _) = reg_as_expr(regs, left)?;
    let (r, _) = reg_as_expr(regs, right)?;
    regs.insert(
        dest,
        RegContent::Expr(
            PureExpr::Op {
                op,
                full_name: op_full_name(op),
                args: vec![l, r],
            },
            op_result_type(op),
        ),
    );
    Ok(())
}

fn instruction_name(ins: &Instruction) -> &'static str {
    match ins {
        Instruction::LoadConst { .. } => "LoadConst",
        Instruction::Move { .. } => "Move",
        Instruction::LoadVar { .. } => "LoadVar",
        Instruction::Call { .. } => "Call",
        Instruction::CallLibBuiltin { .. } => "CallLibBuiltin",
        Instruction::CallUserFunction { .. } => "CallUserFunction",
        Instruction::CallLambda { .. } => "CallLambda",
        Instruction::Return { .. } => "Return",
        Instruction::DotAccess { .. } => "DotAccess",
        Instruction::TemplateInterpolate { .. } => "TemplateInterpolate",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Expression evaluation (Stage 1 fused interpreter)
// ---------------------------------------------------------------------------

/// Evaluate a lowered expression with the given parameter values.
pub fn eval_expr(expr: &PureExpr, args: &[PureValue]) -> EvalOutcome {
    match expr {
        PureExpr::Param(i) => match args.get(*i) {
            Some(v) => EvalOutcome::Value(v.clone()),
            None => EvalOutcome::DeoptToInterpreter,
        },
        PureExpr::ConstInt(i) => EvalOutcome::Value(PureValue::Int(*i)),
        PureExpr::ConstBool(b) => EvalOutcome::Value(PureValue::Bool(*b)),
        PureExpr::ConstVal(v) => EvalOutcome::Value(PureValue::from_val(v.clone())),
        PureExpr::Op {
            op,
            full_name,
            args: operands,
        } => eval_op(*op, full_name, operands, args),
        PureExpr::Field { base, key } => eval_field(base, key, args),
        PureExpr::Template(parts) => eval_template(parts, args),
    }
}

/// Evaluate `base.key` given the already-evaluated `base` value. The base must
/// be a record (`Val::Map`); anything else (or a missing key) de-opts so the
/// interpreter reproduces exact access semantics.
fn field_of(base_val: Val, key: &str) -> EvalOutcome {
    match base_val {
        Val::Map(map) => match map.get(&Val::from(key)) {
            Some(v) => EvalOutcome::Value(PureValue::from_val(v.clone())),
            None => EvalOutcome::DeoptToInterpreter,
        },
        _ => EvalOutcome::DeoptToInterpreter,
    }
}

fn eval_field(base: &PureExpr, key: &str, args: &[PureValue]) -> EvalOutcome {
    match eval_expr(base, args) {
        EvalOutcome::Value(v) => field_of(v.into_val(), key),
        deopt => deopt,
    }
}

/// Interpolate a template from already-evaluated part values. Each part is
/// stringified exactly as the interpreter's non-typed path does
/// (`str_internal`); a record/typed `Map` part de-opts because it could
/// dispatch to a user-defined `Str` implementation.
fn template_of(parts: &[PureValue]) -> EvalOutcome {
    let max_bytes = crate::lang::runtime::limits::max_string_bytes();
    let mut out = String::new();
    for part in parts {
        if matches!(part, PureValue::Other(Val::Map(_))) {
            return EvalOutcome::DeoptToInterpreter;
        }
        if !push_template_part(&mut out, part, max_bytes) {
            return EvalOutcome::DeoptToInterpreter;
        }
    }
    EvalOutcome::Value(PureValue::Other(Val::from(out)))
}

fn push_checked(out: &mut String, piece: &str, max_bytes: usize) -> bool {
    match out.len().checked_add(piece.len()) {
        Some(total) if total <= max_bytes => {
            out.push_str(piece);
            true
        }
        _ => false,
    }
}

fn push_template_part(out: &mut String, part: &PureValue, max_bytes: usize) -> bool {
    match part {
        PureValue::Int(i) => push_checked(out, &i.to_string(), max_bytes),
        PureValue::Bool(b) => push_checked(out, &b.to_string(), max_bytes),
        PureValue::Other(Val::Str(s)) => push_checked(out, s, max_bytes),
        PureValue::Other(Val::Dec(d)) => push_checked(out, &d.to_string(), max_bytes),
        PureValue::Other(Val::Byte(b)) => push_checked(out, &b.to_string(), max_bytes),
        PureValue::Other(Val::Null) => push_checked(out, "null", max_bytes),
        PureValue::Other(v) => {
            let piece = match str_internal(std::slice::from_ref(v)) {
                HotResult::Ok(Val::Str(s)) => s.to_string(),
                HotResult::Ok(other) => other.to_string(),
                HotResult::Err(_) => return false,
            };
            push_checked(out, &piece, max_bytes)
        }
    }
}

fn eval_template(parts: &[PureExpr], args: &[PureValue]) -> EvalOutcome {
    let mut vals: Vec<PureValue> = Vec::with_capacity(parts.len());
    for part in parts {
        match eval_expr(part, args) {
            EvalOutcome::Value(v) => vals.push(v),
            deopt => return deopt,
        }
    }
    template_of(&vals)
}

fn eval_op(op: PureOp, full_name: &str, operands: &[PureExpr], args: &[PureValue]) -> EvalOutcome {
    let mut vals: Vec<PureValue> = Vec::with_capacity(operands.len());
    for o in operands {
        match eval_expr(o, args) {
            EvalOutcome::Value(v) => vals.push(v),
            deopt => return deopt,
        }
    }
    apply_op(op, full_name, &vals)
}

/// Apply an op to already-evaluated operand values. Uses an inlined fast path
/// for the common `Int`/`Bool` case and otherwise calls the exact registered
/// hotlib `LibFn`, guaranteeing parity (mixed/`Dec`/`Str` operands, integer
/// overflow, divide/modulo by zero, error values).
fn apply_op(op: PureOp, full_name: &str, vals: &[PureValue]) -> EvalOutcome {
    if let Some(out) = fast_op(op, vals) {
        return out;
    }
    let arg_vals: Vec<Val> = vals.iter().map(PureValue::to_val).collect();
    match libmap::call_pure_lib(full_name, &arg_vals) {
        Some(HotResult::Ok(v)) => {
            if v.is_err() {
                EvalOutcome::DeoptToInterpreter
            } else {
                EvalOutcome::Value(PureValue::from_val(v))
            }
        }
        _ => EvalOutcome::DeoptToInterpreter,
    }
}

/// Inlined evaluation for `Int`/`Bool` operands. Returns `None` when the inline
/// path cannot guarantee parity (non-`Int` operands, overflow, zero divisor) so
/// the caller falls back to the exact hotlib implementation.
fn fast_op(op: PureOp, vals: &[PureValue]) -> Option<EvalOutcome> {
    fn ok(v: PureValue) -> Option<EvalOutcome> {
        Some(EvalOutcome::Value(v))
    }

    match op {
        PureOp::Add => {
            let (a, b) = (vals[0].as_int()?, vals[1].as_int()?);
            // Overflow falls through to the hotlib for identical wrap/panic
            // behavior in the current build.
            a.checked_add(b)
                .map(|r| EvalOutcome::Value(PureValue::Int(r)))
        }
        PureOp::Sub => {
            let (a, b) = (vals[0].as_int()?, vals[1].as_int()?);
            a.checked_sub(b)
                .map(|r| EvalOutcome::Value(PureValue::Int(r)))
        }
        PureOp::Mul => {
            let (a, b) = (vals[0].as_int()?, vals[1].as_int()?);
            a.checked_mul(b)
                .map(|r| EvalOutcome::Value(PureValue::Int(r)))
        }
        PureOp::Mod => {
            let (a, b) = (vals[0].as_int()?, vals[1].as_int()?);
            if b == 0 {
                // Zero divisor produces an error value; defer to the hotlib.
                None
            } else {
                ok(PureValue::Int(a % b))
            }
        }
        PureOp::IsZero => ok(PureValue::Bool(vals[0].as_int()? == 0)),
        PureOp::Eq => ok(PureValue::Bool(vals[0].as_int()? == vals[1].as_int()?)),
        PureOp::Ne => ok(PureValue::Bool(vals[0].as_int()? != vals[1].as_int()?)),
        PureOp::Lt => ok(PureValue::Bool(vals[0].as_int()? < vals[1].as_int()?)),
        PureOp::Lte => ok(PureValue::Bool(vals[0].as_int()? <= vals[1].as_int()?)),
        PureOp::Gt => ok(PureValue::Bool(vals[0].as_int()? > vals[1].as_int()?)),
        PureOp::Gte => ok(PureValue::Bool(vals[0].as_int()? >= vals[1].as_int()?)),
        PureOp::Not => ok(PureValue::Bool(!vals[0].as_bool()?)),
        PureOp::Concat => match vals {
            [PureValue::Other(Val::Str(a)), PureValue::Other(Val::Str(b))] => {
                let mut s = String::with_capacity(a.len() + b.len());
                s.push_str(a);
                s.push_str(b);
                ok(PureValue::Other(Val::from(s)))
            }
            _ => None,
        },
        // `Div` can yield a `Dec`; `Abs`/`Min`/`Max`/`Neg` are handled by the
        // hotlib fallback for full parity.
        PureOp::Div | PureOp::Abs | PureOp::Min | PureOp::Max | PureOp::Neg => None,
    }
}

// ---------------------------------------------------------------------------
// Flat stack program
//
// The recursive `PureExpr` tree is flattened once (per pipeline build) into a
// postfix `MicroOp` sequence evaluated by [`run_program`] against a value stack
// that is reused across every element. This removes the per-element recursion
// and per-operator `Vec` allocation of the tree-walker while preserving exactly
// the same semantics (and de-opt points) as [`eval_expr`].
// ---------------------------------------------------------------------------

/// A single postfix instruction. `Apply`/`Template` consume their top-of-stack
/// operands and push one result; `Field` consumes one and pushes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicroOp {
    PushParam(usize),
    PushInt(i64),
    PushBool(bool),
    PushVal(Val),
    Apply {
        op: PureOp,
        full_name: &'static str,
        argc: usize,
    },
    Field(Arc<str>),
    Template(usize),
}

/// Flatten a lowered expression tree into a postfix `MicroOp` program.
pub fn flatten_program(expr: &PureExpr) -> Vec<MicroOp> {
    let mut out = Vec::new();
    flatten_into(expr, &mut out);
    out
}

fn expr_uses_param(expr: &PureExpr, param: usize) -> bool {
    match expr {
        PureExpr::Param(idx) => *idx == param,
        PureExpr::ConstInt(_) | PureExpr::ConstBool(_) | PureExpr::ConstVal(_) => false,
        PureExpr::Op { args, .. } => args.iter().any(|arg| expr_uses_param(arg, param)),
        PureExpr::Field { base, .. } => expr_uses_param(base, param),
        PureExpr::Template(parts) => parts.iter().any(|part| expr_uses_param(part, param)),
    }
}

fn flatten_into(expr: &PureExpr, out: &mut Vec<MicroOp>) {
    match expr {
        PureExpr::Param(i) => out.push(MicroOp::PushParam(*i)),
        PureExpr::ConstInt(i) => out.push(MicroOp::PushInt(*i)),
        PureExpr::ConstBool(b) => out.push(MicroOp::PushBool(*b)),
        PureExpr::ConstVal(v) => out.push(MicroOp::PushVal(v.clone())),
        PureExpr::Op {
            op,
            full_name,
            args,
        } => {
            for a in args {
                flatten_into(a, out);
            }
            out.push(MicroOp::Apply {
                op: *op,
                full_name,
                argc: args.len(),
            });
        }
        PureExpr::Field { base, key } => {
            flatten_into(base, out);
            out.push(MicroOp::Field(key.clone()));
        }
        PureExpr::Template(parts) => {
            for p in parts {
                flatten_into(p, out);
            }
            out.push(MicroOp::Template(parts.len()));
        }
    }
}

/// Evaluate a flat program against `args`, reusing `stack` (cleared on entry).
/// Returns the single top-of-stack result, or a de-opt.
fn run_program(code: &[MicroOp], args: &[PureValue], stack: &mut Vec<PureValue>) -> EvalOutcome {
    stack.clear();
    for mop in code {
        match mop {
            MicroOp::PushParam(i) => match args.get(*i) {
                Some(v) => stack.push(v.clone()),
                None => return EvalOutcome::DeoptToInterpreter,
            },
            MicroOp::PushInt(i) => stack.push(PureValue::Int(*i)),
            MicroOp::PushBool(b) => stack.push(PureValue::Bool(*b)),
            MicroOp::PushVal(v) => stack.push(PureValue::from_val(v.clone())),
            MicroOp::Apply {
                op,
                full_name,
                argc,
            } => {
                if stack.len() < *argc {
                    return EvalOutcome::DeoptToInterpreter;
                }
                let base = stack.len() - *argc;
                let v = match apply_op(*op, full_name, &stack[base..]) {
                    EvalOutcome::Value(v) => v,
                    deopt => return deopt,
                };
                stack.truncate(base);
                stack.push(v);
            }
            MicroOp::Field(key) => {
                let Some(base_val) = stack.pop() else {
                    return EvalOutcome::DeoptToInterpreter;
                };
                let v = match field_of(base_val.into_val(), key) {
                    EvalOutcome::Value(v) => v,
                    deopt => return deopt,
                };
                stack.push(v);
            }
            MicroOp::Template(n) => {
                if stack.len() < *n {
                    return EvalOutcome::DeoptToInterpreter;
                }
                let base = stack.len() - *n;
                let v = match template_of(&stack[base..]) {
                    EvalOutcome::Value(v) => v,
                    deopt => return deopt,
                };
                stack.truncate(base);
                stack.push(v);
            }
        }
    }
    match stack.pop() {
        Some(v) => EvalOutcome::Value(v),
        None => EvalOutcome::DeoptToInterpreter,
    }
}

// ---------------------------------------------------------------------------
// Pipeline detection
// ---------------------------------------------------------------------------

/// A single fused stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusedStage {
    pub stage: HofStage,
    pub lambda: LoweredLambda,
    /// `reduce` initial accumulator value (any constant `Val`).
    pub initial: Option<Val>,
}

/// An eager `::hot::coll/range` call that can be replaced by direct integer
/// iteration when its result is consumed only by this fused pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSource {
    /// ip of the original range call.
    pub call_ip: usize,
    /// First contiguous argument register for the range call.
    pub args_start: RegisterId,
    /// Number of range arguments (1..=3).
    pub args_count: u8,
}

/// A recognized fusible pipeline within a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelinePlan {
    /// ip of the first stage `Call`.
    pub first_stage_ip: usize,
    /// ip of the last (terminal) stage `Call`.
    pub last_stage_ip: usize,
    /// Register holding the input collection at the first stage. This is the
    /// first stage call's `args_start` register, which is guaranteed live at
    /// `first_stage_ip` (the calling convention reads arguments from there).
    /// Note: a register traced *back* through `Move`s can be reused for an
    /// unrelated value before the stage executes, so the argument register —
    /// not the traced source — must be read at dispatch.
    pub source_reg: RegisterId,
    /// Optional eager range source that can be streamed without materializing a
    /// `Vec`. When present, `first_stage_ip` is the range call ip.
    pub range_source: Option<RangeSource>,
    /// Register receiving the final result.
    pub result_reg: RegisterId,
    pub stages: Vec<FusedStage>,
}

/// Raw stage call recovered from the bytecode before chaining/validation.
struct StageCall {
    ip: usize,
    stage: HofStage,
    dest: RegisterId,
    input_reg: RegisterId,
    input_arg_reg: RegisterId,
    callable: StageCallable,
    initial: Option<Val>,
}

#[derive(Debug, Clone, Copy)]
enum StageCallable {
    LambdaConst(u32),
    FunctionId(u32),
    None,
}

/// Detect fusible pipelines in a function's instruction stream. Returns a
/// (possibly empty) list of plans. This is the compile-time recognition pass; it
/// performs no mutation.
pub fn detect_pipelines(instrs: &[Instruction], program: &BytecodeProgram) -> Vec<PipelinePlan> {
    // First, recover candidate sequential stage calls.
    let mut stage_calls: Vec<StageCall> = Vec::new();
    for (ip, ins) in instrs.iter().enumerate() {
        if let Instruction::Call {
            dest,
            function: fn_reg,
            args_start,
            args_count,
        } = ins
        {
            let Some(name) = resolve_fn_name(instrs, program, ip, *fn_reg) else {
                continue;
            };
            let Some(role) = libmap::hof_role(&name) else {
                continue;
            };
            if role.execution != HofExecution::Sequential {
                continue; // never fuse pmap / parallel
            }
            // input collection comes from args_start; lambda from args_start+1;
            // reduce initial from args_start+2.
            let input_reg = match trace_move_src(instrs, ip, *args_start) {
                Some(r) => r,
                None => continue,
            };
            // `length` takes only the collection (no lambda).
            let (callable, initial) = if role.stage == HofStage::Length {
                (StageCallable::None, None)
            } else {
                if *args_count < 2 {
                    continue;
                }
                let lam_reg = args_start + 1;
                let Some(callable) = trace_stage_callable(instrs, program, ip, lam_reg, role.stage)
                else {
                    continue;
                };
                let initial = if role.stage == HofStage::Reduce && *args_count >= 3 {
                    trace_const_val(instrs, program, ip, args_start + 2)
                } else {
                    None
                };
                (callable, initial)
            };
            stage_calls.push(StageCall {
                ip,
                stage: role.stage,
                dest: *dest,
                input_reg,
                input_arg_reg: *args_start,
                callable,
                initial,
            });
        }
    }

    if stage_calls.is_empty() {
        return Vec::new();
    }

    // Chain stages: a stage whose input_reg equals a previous stage's dest
    // continues that chain. We only build a single linear chain per starting
    // stage and require linear (single) use of intermediate results.
    let mut plans = Vec::new();
    let mut consumed = vec![false; stage_calls.len()];

    for start in 0..stage_calls.len() {
        if consumed[start] {
            continue;
        }
        // A chain start is a stage whose input_reg is NOT the dest of another stage.
        if stage_calls
            .iter()
            .any(|s| s.dest == stage_calls[start].input_reg)
        {
            continue;
        }
        let mut chain = vec![start];
        let mut current = start;
        loop {
            let cur_dest = stage_calls[current].dest;
            // Find a following stage consuming cur_dest.
            let next = stage_calls
                .iter()
                .enumerate()
                .find(|(_, s)| s.input_reg == cur_dest && s.ip > stage_calls[current].ip);
            match next {
                Some((idx, _)) => {
                    // Require linear use: cur_dest must not be read by any other
                    // instruction besides the next stage's input Move.
                    if !is_single_use_between(
                        instrs,
                        stage_calls[current].ip,
                        cur_dest,
                        stage_calls[idx].ip,
                    ) {
                        break;
                    }
                    chain.push(idx);
                    current = idx;
                }
                None => break,
            }
        }

        // A fusible pipeline ends in a terminal stage (`reduce`/`length`). A
        // single terminal stage is allowed (a lone `reduce`/`length` over a
        // `Vec`): fusing still removes per-element lambda dispatch. Lone `map`/
        // `filter` chains are rejected because they produce a collection that
        // the scalar runner does not build.
        if chain.is_empty() {
            continue;
        }
        if !stage_calls[*chain.last().unwrap()].stage.is_terminal() {
            continue;
        }

        // Lower every stage lambda; bail (skip plan) on any failure.
        let mut stages = Vec::with_capacity(chain.len());
        let mut ok = true;
        for &ci in &chain {
            let sc = &stage_calls[ci];
            let lowered = if sc.stage == HofStage::Length {
                // length has no lambda; represent with an identity-less marker by
                // skipping lowering. We encode it as a Param(0) Int passthrough.
                LoweredLambda::new(1, PureExpr::Param(0), ExprType::Int)
            } else {
                match lower_stage_callable(program, sc.callable, sc.stage) {
                    Ok(l) => l,
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            };
            stages.push(FusedStage {
                stage: sc.stage,
                lambda: lowered,
                initial: sc.initial.clone(),
            });
        }
        if !ok {
            continue;
        }

        for &ci in &chain {
            consumed[ci] = true;
        }
        let start_stage = &stage_calls[chain[0]];
        let terminal_stage = &stage_calls[*chain.last().unwrap()];
        let range_source = trace_range_source(instrs, program, start_stage, terminal_stage.ip);
        plans.push(PipelinePlan {
            first_stage_ip: range_source
                .as_ref()
                .map(|s| s.call_ip)
                .unwrap_or(start_stage.ip),
            last_stage_ip: terminal_stage.ip,
            source_reg: start_stage.input_arg_reg,
            range_source,
            result_reg: terminal_stage.dest,
            stages,
        });
    }

    plans
}

fn trace_range_source(
    instrs: &[Instruction],
    program: &BytecodeProgram,
    start_stage: &StageCall,
    last_stage_ip: usize,
) -> Option<RangeSource> {
    let source_reg = start_stage.input_reg;
    for ip in (0..start_stage.ip).rev() {
        match &instrs[ip] {
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } if *dest == source_reg => {
                if !(1..=3).contains(args_count) {
                    return None;
                }
                let name = resolve_fn_name(instrs, program, ip, *function)?;
                if !libmap::is_fusible_range_source(&name) {
                    return None;
                }
                if !range_source_window_is_safe(instrs, ip, start_stage, last_stage_ip) {
                    return None;
                }
                return Some(RangeSource {
                    call_ip: ip,
                    args_start: *args_start,
                    args_count: *args_count,
                });
            }
            _ => {
                if writes_register(&instrs[ip], source_reg) {
                    return None;
                }
            }
        }
    }
    None
}

fn range_source_window_is_safe(
    instrs: &[Instruction],
    range_ip: usize,
    start_stage: &StageCall,
    last_stage_ip: usize,
) -> bool {
    let source_reg = start_stage.input_reg;

    for (ip, ins) in instrs
        .iter()
        .enumerate()
        .take(start_stage.ip)
        .skip(range_ip + 1)
    {
        match ins {
            // These are pure setup instructions for the original HOF call chain.
            Instruction::LoadConst { .. }
            | Instruction::LoadFunctionRef { .. }
            | Instruction::Move { .. } => {}
            // A range error check may be skipped only because the fused range
            // runner de-opts on every range shape that the eager call would turn
            // into an error; the interpreter then executes this check normally.
            Instruction::ReturnIfErr { src } if *src == source_reg => {}
            _ => return false,
        }

        if reads_register(ins, source_reg) {
            match ins {
                Instruction::Move { dest, src }
                    if *src == source_reg && *dest == start_stage.input_arg_reg => {}
                Instruction::ReturnIfErr { src } if *src == source_reg => {}
                _ => return false,
            }
        }

        if let Some(written) = single_written_register(ins)
            && written != source_reg
            && register_read_after(instrs, last_stage_ip + 1, written)
        {
            return false;
        }

        // Keep the loop variable used in debug assertions if this grows.
        let _ = ip;
    }

    !register_read_between_except_range_setup(
        instrs,
        source_reg,
        range_ip + 1,
        last_stage_ip + 1,
        start_stage.input_arg_reg,
    ) && !register_read_after(instrs, last_stage_ip + 1, source_reg)
}

fn register_read_between_except_range_setup(
    instrs: &[Instruction],
    reg: RegisterId,
    start: usize,
    end: usize,
    stage_input_arg: RegisterId,
) -> bool {
    instrs
        .iter()
        .take(end.min(instrs.len()))
        .skip(start)
        .any(|ins| {
            if !reads_register(ins, reg) {
                return false;
            }
            !matches!(
                ins,
                Instruction::Move { dest, src } if *src == reg && *dest == stage_input_arg
            ) && !matches!(ins, Instruction::ReturnIfErr { src } if *src == reg)
        })
}

fn register_read_after(instrs: &[Instruction], start: usize, reg: RegisterId) -> bool {
    instrs
        .iter()
        .skip(start)
        .any(|ins| reads_register(ins, reg))
}

fn single_written_register(ins: &Instruction) -> Option<RegisterId> {
    match ins {
        Instruction::LoadConst { dest, .. }
        | Instruction::LoadFunctionRef { dest, .. }
        | Instruction::Move { dest, .. } => Some(*dest),
        _ => None,
    }
}

/// Outcome of attempting to run a fused pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FusedRun {
    /// The pipeline produced this result value.
    Produced(Val),
    /// A guard/condition required falling back to the interpreter. No state was
    /// mutated; the caller should execute the original instructions instead.
    Deopt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StringAppendPlan {
    Program(Vec<MicroOp>),
    Template(Vec<Vec<MicroOp>>),
}

/// Run a fused pipeline in a single pass over `source`. Pure: reads `source`,
/// writes nothing. Returns `Deopt` (discarding any partial work) whenever a
/// runtime guard fails so the interpreter can reproduce exact semantics.
///
/// Supports `Vec` sources of any element type (`Int`/`Bool` on the fast path,
/// `Dec`/`Str`/records via the hotlib parity fallback), `filter`/`map`
/// transform stages, and `reduce`/`length`/`some`/`all` terminal stages.
pub fn run_pipeline(plan: &PipelinePlan, source: &Val) -> FusedRun {
    let items = match source {
        Val::Vec(items) => items,
        _ => return FusedRun::Deopt,
    };
    run_pipeline_items(plan, items.iter().cloned())
}

/// Run a fused pipeline over an eager `range(...)` source without materializing
/// the intermediate `Vec`. Invalid/non-Int range args de-opt so the interpreter
/// can reproduce the eager `::hot::coll/range` result or error exactly.
pub fn run_pipeline_range(plan: &PipelinePlan, args: &[Val]) -> FusedRun {
    let Some(iter) = range_iter_from_args(args) else {
        return FusedRun::Deopt;
    };
    run_pipeline_items(plan, iter)
}

fn run_pipeline_items<I>(plan: &PipelinePlan, items: I) -> FusedRun
where
    I: IntoIterator<Item = Val>,
{
    let Some((terminal, transforms)) = plan.stages.split_last() else {
        return FusedRun::Deopt;
    };

    // Validate stage roles up front.
    for t in transforms {
        if !matches!(t.stage, HofStage::Filter | HofStage::Map) {
            return FusedRun::Deopt;
        }
    }
    match terminal.stage {
        HofStage::Reduce | HofStage::Length | HofStage::Some | HofStage::All => {}
        _ => return FusedRun::Deopt,
    }

    // Terminal accumulators.
    let mut reduce_acc: Option<PureValue> = None;
    let mut count: i64 = 0;
    let mut some_result = false;
    let mut all_result = true;
    let mut string_reduce = string_reduce_builder(terminal);
    if terminal.stage == HofStage::Reduce && string_reduce.is_none() {
        match &terminal.initial {
            Some(v) => reduce_acc = Some(PureValue::from_val(v.clone())),
            None => return FusedRun::Deopt,
        }
    }

    // Value stack reused across every element/stage (no per-element allocation).
    let mut stack: Vec<PureValue> = Vec::with_capacity(8);

    for item in items {
        let mut cur = PureValue::from_val(item);

        let mut keep = true;
        for t in transforms {
            match t.stage {
                HofStage::Filter => {
                    match run_program(&t.lambda.code, std::slice::from_ref(&cur), &mut stack) {
                        EvalOutcome::Value(PureValue::Bool(b)) => {
                            if !b {
                                keep = false;
                                break;
                            }
                        }
                        // A non-`Bool` predicate result or a de-opt means the
                        // interpreter must decide truthiness/errors.
                        _ => return FusedRun::Deopt,
                    }
                }
                HofStage::Map => {
                    match run_program(&t.lambda.code, std::slice::from_ref(&cur), &mut stack) {
                        EvalOutcome::Value(v) if !value_is_err(&v) => cur = v,
                        _ => return FusedRun::Deopt,
                    }
                }
                _ => return FusedRun::Deopt,
            }
        }

        if !keep {
            continue;
        }

        match terminal.stage {
            HofStage::Reduce => {
                if let Some((out, append_plan)) = &mut string_reduce {
                    let args = [PureValue::Other(Val::from("")), cur];
                    if !push_string_append(out, append_plan, &args, &mut stack) {
                        return FusedRun::Deopt;
                    }
                } else {
                    // Invariant: `reduce_acc` is `Some` here (initialized before
                    // the loop and re-set every iteration). De-opt rather than
                    // panic if a future refactor ever breaks that invariant.
                    let Some(acc) = reduce_acc.take() else {
                        return FusedRun::Deopt;
                    };
                    let pair = [acc, cur];
                    match run_program(&terminal.lambda.code, &pair, &mut stack) {
                        EvalOutcome::Value(v) if !value_is_err(&v) => reduce_acc = Some(v),
                        _ => return FusedRun::Deopt,
                    }
                }
            }
            HofStage::Length => {
                count += 1;
            }
            HofStage::Some => {
                match run_program(
                    &terminal.lambda.code,
                    std::slice::from_ref(&cur),
                    &mut stack,
                ) {
                    EvalOutcome::Value(PureValue::Bool(true)) => {
                        some_result = true;
                        break;
                    }
                    EvalOutcome::Value(PureValue::Bool(false)) => {}
                    _ => return FusedRun::Deopt,
                }
            }
            HofStage::All => {
                match run_program(
                    &terminal.lambda.code,
                    std::slice::from_ref(&cur),
                    &mut stack,
                ) {
                    EvalOutcome::Value(PureValue::Bool(true)) => {}
                    EvalOutcome::Value(PureValue::Bool(false)) => {
                        all_result = false;
                        break;
                    }
                    _ => return FusedRun::Deopt,
                }
            }
            _ => return FusedRun::Deopt,
        }
    }

    let result = match terminal.stage {
        HofStage::Reduce => match string_reduce {
            Some((out, _)) => Val::from(out),
            // `reduce_acc` is always `Some` for a non-string reduce (initialized
            // before the loop, re-set each iteration). De-opt instead of
            // panicking if that ever fails to hold.
            None => match reduce_acc {
                Some(acc) => acc.into_val(),
                None => return FusedRun::Deopt,
            },
        },
        HofStage::Length => Val::Int(count),
        HofStage::Some => Val::Bool(some_result),
        HofStage::All => Val::Bool(all_result),
        _ => return FusedRun::Deopt,
    };
    FusedRun::Produced(result)
}

fn string_reduce_builder(terminal: &FusedStage) -> Option<(String, StringAppendPlan)> {
    if terminal.stage != HofStage::Reduce {
        return None;
    }

    let seed = match terminal.initial.as_ref()? {
        Val::Str(s) => s.to_string(),
        _ => return None,
    };

    let PureExpr::Op {
        op: PureOp::Concat,
        args,
        ..
    } = &terminal.lambda.expr
    else {
        return None;
    };

    match args.as_slice() {
        [PureExpr::Param(0), append_expr] if !expr_uses_param(append_expr, 0) => {
            let append = match append_expr {
                PureExpr::Template(parts) => {
                    StringAppendPlan::Template(parts.iter().map(flatten_program).collect())
                }
                _ => StringAppendPlan::Program(flatten_program(append_expr)),
            };
            Some((seed, append))
        }
        _ => None,
    }
}

fn push_string_append(
    out: &mut String,
    append: &StringAppendPlan,
    args: &[PureValue],
    stack: &mut Vec<PureValue>,
) -> bool {
    match append {
        StringAppendPlan::Program(code) => match run_program(code, args, stack) {
            EvalOutcome::Value(v) if !value_is_err(&v) => push_concat_part(out, &v),
            _ => false,
        },
        StringAppendPlan::Template(parts) => {
            let max_bytes = crate::lang::runtime::limits::max_string_bytes();
            for code in parts {
                match run_program(code, args, stack) {
                    EvalOutcome::Value(v) if !value_is_err(&v) => {
                        if matches!(v, PureValue::Other(Val::Map(_))) {
                            return false;
                        }
                        if !push_template_part(out, &v, max_bytes) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
    }
}

fn push_concat_part(out: &mut String, value: &PureValue) -> bool {
    match value {
        PureValue::Int(i) => out.push_str(&i.to_string()),
        PureValue::Bool(b) => out.push_str(&b.to_string()),
        PureValue::Other(Val::Int(i)) => out.push_str(&i.to_string()),
        PureValue::Other(Val::Bool(b)) => out.push_str(&b.to_string()),
        PureValue::Other(Val::Str(s)) => out.push_str(s),
        PureValue::Other(Val::Dec(d)) => out.push_str(&d.to_string()),
        PureValue::Other(Val::Null) => {}
        PureValue::Other(Val::Byte(b)) => out.push_str(&b.to_string()),
        PureValue::Other(Val::Bytes(bytes)) => out.push_str(&format!("{:?}", bytes)),
        PureValue::Other(Val::Vec(v)) => out.push_str(&format!("{:?}", v)),
        PureValue::Other(Val::Map(m)) => out.push_str(&format!("{:?}", m)),
        PureValue::Other(Val::Box(b)) => out.push_str(&b.to_string()),
    }
    true
}

struct RangeValueIter {
    current: i64,
    step: i64,
    remaining: usize,
}

impl Iterator for RangeValueIter {
    type Item = Val;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let value = self.current;
        self.remaining -= 1;
        if self.remaining > 0 {
            self.current = self.current.checked_add(self.step)?;
        }
        Some(Val::Int(value))
    }
}

fn range_iter_from_args(args: &[Val]) -> Option<RangeValueIter> {
    let (start, end, step) = match args {
        [Val::Int(end)] => (0, *end, 1),
        [Val::Int(start), Val::Int(end)] => (*start, *end, 1),
        [Val::Int(start), Val::Int(end), Val::Int(step)] => (*start, *end, *step),
        _ => return None,
    };
    if step == 0 {
        return None;
    }
    let count = crate::lang::runtime::limits::range_element_count(start, end, step)?;
    if crate::lang::runtime::limits::check_collection_size("range", count).is_err() {
        return None;
    }
    Some(RangeValueIter {
        current: start,
        step,
        remaining: count,
    })
}

/// True if a produced value is an `Result.Err` error value, which carries
/// interpreter-specific propagation semantics and must de-opt.
fn value_is_err(v: &PureValue) -> bool {
    matches!(v, PureValue::Other(val) if val.is_err())
}

/// Resolve the function name a `Call` targets by walking back through `Move` /
/// `LoadConst`. Mirrors `jit::resolve_const_function_name` but local to keep the
/// analysis module self-contained.
fn resolve_fn_name(
    instrs: &[Instruction],
    program: &BytecodeProgram,
    call_ip: usize,
    fn_reg: RegisterId,
) -> Option<String> {
    for i in (0..call_ip).rev() {
        match &instrs[i] {
            Instruction::LoadConst { dest, constant } if *dest == fn_reg => {
                return const_str(program, *constant);
            }
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } if *dest == fn_reg => {
                return const_str(program, *function_name);
            }
            Instruction::Move { dest, src } if *dest == fn_reg => {
                return resolve_fn_name(instrs, program, i, *src);
            }
            _ => {
                if writes_register(&instrs[i], fn_reg) {
                    return None;
                }
            }
        }
    }
    None
}

/// Walk back to find the source register that was `Move`d into `reg` just
/// before `call_ip`.
fn trace_move_src(instrs: &[Instruction], call_ip: usize, reg: RegisterId) -> Option<RegisterId> {
    for i in (0..call_ip).rev() {
        if let Instruction::Move { dest, src } = &instrs[i]
            && *dest == reg
        {
            return Some(*src);
        }
        if writes_register(&instrs[i], reg) {
            return None;
        }
    }
    None
}

fn trace_stage_callable(
    instrs: &[Instruction],
    program: &BytecodeProgram,
    call_ip: usize,
    reg: RegisterId,
    stage: HofStage,
) -> Option<StageCallable> {
    let mut cur = reg;
    for i in (0..call_ip).rev() {
        match &instrs[i] {
            Instruction::Move { dest, src } if *dest == cur => {
                cur = *src;
            }
            Instruction::LoadConst { dest, constant } if *dest == cur => {
                if lambda_from_const(program, *constant).is_some() {
                    return Some(StageCallable::LambdaConst(*constant));
                }
                let name = const_str(program, *constant)?;
                let arity = stage_callable_arity(stage)?;
                return find_unambiguous_function(program, &name, arity)
                    .map(|id| StageCallable::FunctionId(id as u32));
            }
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } if *dest == cur => {
                let name = const_str(program, *function_name)?;
                let arity = stage_callable_arity(stage)?;
                return find_unambiguous_function(program, &name, arity)
                    .map(|id| StageCallable::FunctionId(id as u32));
            }
            Instruction::LoadVar { dest, var_name } if *dest == cur => {
                let name = const_str(program, *var_name)?;
                let arity = stage_callable_arity(stage)?;
                return find_unambiguous_function(program, &name, arity)
                    .map(|id| StageCallable::FunctionId(id as u32));
            }
            _ => {
                if writes_register(&instrs[i], cur) {
                    return None;
                }
            }
        }
    }
    None
}

fn stage_callable_arity(stage: HofStage) -> Option<u8> {
    match stage {
        HofStage::Map | HofStage::Filter | HofStage::Some | HofStage::All => Some(1),
        HofStage::Reduce => Some(2),
        HofStage::Length => None,
    }
}

fn find_unambiguous_function(program: &BytecodeProgram, name: &str, arity: u8) -> Option<usize> {
    let suffix = format!("/{}", name);
    let mut matches = program.functions.iter().enumerate().filter(|(_, f)| {
        f.arity == arity
            && !f.is_variadic
            && (f.name == name || (!name.starts_with("::") && f.name.ends_with(&suffix)))
    });
    let (id, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(id)
}

fn lower_stage_callable(
    program: &BytecodeProgram,
    callable: StageCallable,
    stage: HofStage,
) -> Result<LoweredLambda, Bailout> {
    match callable {
        StageCallable::LambdaConst(id) => lambda_from_const(program, id)
            .ok_or(Bailout::PipelineShape)
            .and_then(|l| lower_lambda(program, l)),
        StageCallable::FunctionId(id) => {
            let func = program
                .functions
                .get(id as usize)
                .ok_or(Bailout::PipelineShape)?;
            let expected_arity = stage_callable_arity(stage).ok_or(Bailout::PipelineShape)?;
            if func.arity != expected_arity
                || func.is_variadic
                || func.flow_type.is_some()
                || func.lazy_params.iter().any(|lazy| *lazy)
            {
                return Err(Bailout::PipelineShape);
            }
            lower_callable_body(program, &func.param_names, &func.instructions)
        }
        StageCallable::None => Err(Bailout::PipelineShape),
    }
}

/// Walk back to find a constant `Val` loaded into `reg` (used for the `reduce`
/// initial accumulator, which may be any literal: `0`, `""`, a `Dec`, ...).
fn trace_const_val(
    instrs: &[Instruction],
    program: &BytecodeProgram,
    call_ip: usize,
    reg: RegisterId,
) -> Option<Val> {
    let mut cur = reg;
    for i in (0..call_ip).rev() {
        match &instrs[i] {
            Instruction::Move { dest, src } if *dest == cur => {
                cur = *src;
            }
            Instruction::LoadConst { dest, constant } if *dest == cur => {
                match program.constants.get(*constant as usize) {
                    Some(Constant::Val(v)) => return Some(v.clone()),
                    _ => return None,
                }
            }
            _ => {
                if writes_register(&instrs[i], cur) {
                    return None;
                }
            }
        }
    }
    None
}

/// Whether `reg` is read by any instruction strictly between `def_ip` and
/// `use_ip`, other than the `Move` at `use_ip - k` that feeds the next stage.
/// Conservative: returns false (i.e. not single-use) if `reg` appears as a read
/// operand anywhere in `(def_ip, use_ip)` except the feeding Move.
fn is_single_use_between(
    instrs: &[Instruction],
    def_ip: usize,
    reg: RegisterId,
    use_ip: usize,
) -> bool {
    let mut feeding_move_seen = false;
    for ins in instrs.iter().take(use_ip).skip(def_ip + 1) {
        if let Instruction::Move { src, .. } = ins
            && *src == reg
        {
            feeding_move_seen = true;
            continue;
        }
        if reads_register(ins, reg) {
            return false;
        }
        if writes_register(ins, reg) {
            // Redefined before use; the chain is broken/observable.
            return false;
        }
    }
    feeding_move_seen
}

/// Does this instruction write `reg` as a destination?
fn writes_register(ins: &Instruction, reg: RegisterId) -> bool {
    matches!(ins,
        Instruction::LoadConst { dest, .. }
        | Instruction::Move { dest, .. }
        | Instruction::LoadVar { dest, .. }
        | Instruction::LoadFunctionRef { dest, .. }
        | Instruction::Add { dest, .. }
        | Instruction::Sub { dest, .. }
        | Instruction::Mul { dest, .. }
        | Instruction::Eq { dest, .. }
        | Instruction::Gt { dest, .. }
        | Instruction::Lt { dest, .. }
        | Instruction::Call { dest, .. }
        | Instruction::CallUserFunction { dest, .. }
        | Instruction::CallLambda { dest, .. }
        | Instruction::CallLibBuiltin { dest, .. }
        | Instruction::DotAccess { dest, .. }
        | Instruction::TemplateInterpolate { dest, .. }
        if *dest == reg)
}

/// Does this instruction read `reg` as an operand? Conservative best-effort over
/// the instruction forms relevant to pipeline regions.
fn reads_register(ins: &Instruction, reg: RegisterId) -> bool {
    match ins {
        Instruction::Move { src, .. } => *src == reg,
        Instruction::Return { value } => *value == reg,
        Instruction::Add { left, right, .. }
        | Instruction::Sub { left, right, .. }
        | Instruction::Mul { left, right, .. }
        | Instruction::Eq { left, right, .. }
        | Instruction::Gt { left, right, .. }
        | Instruction::Lt { left, right, .. } => *left == reg || *right == reg,
        Instruction::Call {
            function,
            args_start,
            args_count,
            ..
        } => *function == reg || (reg >= *args_start && reg < *args_start + *args_count as u32),
        Instruction::DotAccess { object, .. } => *object == reg,
        Instruction::TemplateInterpolate {
            parts_start,
            parts_count,
            ..
        } => reg >= *parts_start && reg < *parts_start + *parts_count as u32,
        Instruction::JumpIf { condition, .. } | Instruction::JumpIfNot { condition, .. } => {
            *condition == reg
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::compiler::Compiler;
    use crate::lang::parser;

    fn compile(src: &str) -> std::sync::Arc<BytecodeProgram> {
        let mut compiler = Compiler::new();
        let mut program = parser::parse_hot(src).expect("parse");
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        compiler.get_program_arc()
    }

    fn find_lambda<'a>(prog: &'a BytecodeProgram, params: &[&str]) -> &'a LambdaInfo {
        for c in &prog.constants {
            if let Constant::Val(Val::Box(b)) = c
                && let Some(l) = b.as_any().downcast_ref::<LambdaInfo>()
                && l.parameters
                    .iter()
                    .map(|s| s.as_str())
                    .eq(params.iter().copied())
            {
                return l;
            }
        }
        panic!("lambda with params {:?} not found", params);
    }

    /// Build an `Op` node (full_name resolved from the op) for terse assertions.
    fn op(op: PureOp, args: Vec<PureExpr>) -> PureExpr {
        PureExpr::Op {
            op,
            full_name: op_full_name(op),
            args,
        }
    }

    #[test]
    fn lowers_filter_predicate() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1)) |> filter((x) { is-zero(mod(x, 2)) }) |> length()
}
"#,
        );
        let lam = find_lambda(&prog, &["x"]);
        let lowered = lower_lambda(&prog, lam).expect("lower");
        assert_eq!(lowered.result_type, ExprType::Bool);
        // is-zero(mod(Param0, 2))
        assert_eq!(
            lowered.expr,
            op(
                PureOp::IsZero,
                vec![op(
                    PureOp::Mod,
                    vec![PureExpr::Param(0), PureExpr::ConstInt(2)]
                )]
            )
        );
    }

    #[test]
    fn lowers_map_square() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1)) |> map((x) { mul(x, x) }) |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let lam = find_lambda(&prog, &["x"]);
        let lowered = lower_lambda(&prog, lam).expect("lower");
        assert_eq!(
            lowered.expr,
            op(PureOp::Mul, vec![PureExpr::Param(0), PureExpr::Param(0)])
        );
    }

    #[test]
    fn lowers_property_access() {
        // filter((x) { gt(x.count, 0) }) -> Field projection.
        let prog = compile(
            r#"::t ns
f fn (rows: Vec<Map>): Int {
    rows |> filter((x) { gt(x.count, 0) }) |> map((x) { x.count }) |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let lam = find_lambda(&prog, &["x"]);
        let lowered = lower_lambda(&prog, lam).expect("lower predicate");
        let field = PureExpr::Field {
            base: Box::new(PureExpr::Param(0)),
            key: Arc::from("count"),
        };
        assert_eq!(
            lowered.expr,
            op(PureOp::Gt, vec![field, PureExpr::ConstInt(0)])
        );
    }

    #[test]
    fn lowers_string_template_concat() {
        // (acc, i) { concat(acc, `item-${i}-`) }
        let prog = compile(
            r#"::t ns
f fn (n: Int): Str {
    reduce(range(n), (acc, i) { concat(acc, `item-${i}-`) }, "")
}
"#,
        );
        let lam = find_lambda(&prog, &["acc", "i"]);
        let lowered = lower_lambda(&prog, lam).expect("lower");
        let template = PureExpr::Template(vec![
            PureExpr::ConstVal(Val::from("item-")),
            PureExpr::Param(1),
            PureExpr::ConstVal(Val::from("-")),
        ]);
        assert_eq!(
            lowered.expr,
            op(PureOp::Concat, vec![PureExpr::Param(0), template])
        );
    }

    #[test]
    fn eval_matches_expected() {
        // is-zero(mod(x,2))
        let pred = op(
            PureOp::IsZero,
            vec![op(
                PureOp::Mod,
                vec![PureExpr::Param(0), PureExpr::ConstInt(2)],
            )],
        );
        assert_eq!(
            eval_expr(&pred, &[PureValue::Int(4)]),
            EvalOutcome::Value(PureValue::Bool(true))
        );
        assert_eq!(
            eval_expr(&pred, &[PureValue::Int(5)]),
            EvalOutcome::Value(PureValue::Bool(false))
        );
    }

    #[test]
    fn mod_by_zero_deopts() {
        let e = op(PureOp::Mod, vec![PureExpr::Param(0), PureExpr::ConstInt(0)]);
        assert_eq!(
            eval_expr(&e, &[PureValue::Int(10)]),
            EvalOutcome::DeoptToInterpreter
        );
    }

    #[test]
    fn eval_field_projection() {
        let field = PureExpr::Field {
            base: Box::new(PureExpr::Param(0)),
            key: Arc::from("count"),
        };
        let mut m = indexmap::IndexMap::new();
        m.insert(Val::from("count"), Val::Int(7));
        let record = PureValue::Other(Val::Map(Box::new(m)));
        assert_eq!(
            eval_expr(&field, std::slice::from_ref(&record)),
            EvalOutcome::Value(PureValue::Int(7))
        );
        // Missing key de-opts.
        let missing = PureExpr::Field {
            base: Box::new(PureExpr::Param(0)),
            key: Arc::from("nope"),
        };
        assert_eq!(
            eval_expr(&missing, &[record]),
            EvalOutcome::DeoptToInterpreter
        );
    }

    #[test]
    fn eval_template_stringifies_primitives() {
        let template = PureExpr::Template(vec![
            PureExpr::ConstVal(Val::from("item-")),
            PureExpr::Param(0),
            PureExpr::ConstVal(Val::from("-")),
        ]);
        assert_eq!(
            eval_expr(&template, &[PureValue::Int(3)]),
            EvalOutcome::Value(PureValue::Other(Val::from("item-3-")))
        );
    }

    #[test]
    fn flat_program_matches_tree_walker() {
        // is-zero(mod(add(x, 1), 2)) over a range of inputs, plus a field+template.
        let expr = op(
            PureOp::IsZero,
            vec![op(
                PureOp::Mod,
                vec![
                    op(PureOp::Add, vec![PureExpr::Param(0), PureExpr::ConstInt(1)]),
                    PureExpr::ConstInt(2),
                ],
            )],
        );
        let code = flatten_program(&expr);
        let mut stack = Vec::new();
        for x in -5..=5 {
            let args = [PureValue::Int(x)];
            assert_eq!(
                run_program(&code, &args, &mut stack),
                eval_expr(&expr, &args),
                "mismatch at x={}",
                x
            );
        }
        // A template program reused across the same stack must stay correct.
        let tmpl = PureExpr::Template(vec![
            PureExpr::ConstVal(Val::from("n=")),
            PureExpr::Param(0),
        ]);
        let tcode = flatten_program(&tmpl);
        for x in 0..3 {
            let args = [PureValue::Int(x)];
            assert_eq!(
                run_program(&tcode, &args, &mut stack),
                eval_expr(&tmpl, &args)
            );
        }
    }

    #[test]
    fn eval_div_falls_back_to_dec() {
        // div(3, 2) -> Dec(1.5) via the hotlib parity fallback.
        let e = op(
            PureOp::Div,
            vec![PureExpr::ConstInt(3), PureExpr::ConstInt(2)],
        );
        match eval_expr(&e, &[]) {
            EvalOutcome::Value(PureValue::Other(Val::Dec(_))) => {}
            other => panic!("expected Dec, got {:?}", other),
        }
        // div by zero -> error value -> de-opt.
        let z = op(
            PureOp::Div,
            vec![PureExpr::ConstInt(3), PureExpr::ConstInt(0)],
        );
        assert_eq!(eval_expr(&z, &[]), EvalOutcome::DeoptToInterpreter);
    }

    #[test]
    fn detects_sum_even_squares_pipeline() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> map((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plans = detect_pipelines(&func.instructions, &prog);
        assert_eq!(plans.len(), 1, "expected one pipeline plan");
        let p = &plans[0];
        assert_eq!(p.stages.len(), 3);
        assert_eq!(p.stages[0].stage, HofStage::Filter);
        assert_eq!(p.stages[1].stage, HofStage::Map);
        assert_eq!(p.stages[2].stage, HofStage::Reduce);
        assert_eq!(p.stages[2].initial, Some(Val::Int(0)));
    }

    #[test]
    fn run_pipeline_sum_even_squares() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> map((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        // range(1, 11) = [1..10]; evens 2,4,6,8,10; squares 4,16,36,64,100; sum=220.
        let source = Val::Vec((1..=10).map(Val::Int).collect());
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Int(220))
        );
    }

    #[test]
    fn detects_range_source_for_pipeline() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plans = detect_pipelines(&func.instructions, &prog);
        assert!(
            !plans.is_empty(),
            "expected named-function pipeline; functions={:#?}; instrs={:#?}",
            prog.functions,
            func.instructions
        );
        let plan = &plans[0];
        let range = plan.range_source.expect("range source");
        assert_eq!(plan.first_stage_ip, range.call_ip);
        assert!(range.args_count >= 2);
    }

    #[test]
    fn run_pipeline_range_source_sum_even_squares() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> map((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        assert!(plan.range_source.is_some());
        assert_eq!(
            run_pipeline_range(plan, &[Val::Int(1), Val::Int(11)]),
            FusedRun::Produced(Val::Int(220))
        );
        assert_eq!(
            run_pipeline_range(plan, &[Val::Int(11), Val::Int(1), Val::Int(-1)]),
            FusedRun::Produced(Val::Int(220))
        );
        assert_eq!(
            run_pipeline_range(plan, &[Val::Int(1), Val::Int(11), Val::Int(0)]),
            FusedRun::Deopt
        );
    }

    #[test]
    fn run_pipeline_named_function_stages() {
        let prog = compile(
            r#"::t ns
is-even fn (x: Int): Bool {
    is-zero(mod(x, 2))
}
square fn (x: Int): Int {
    mul(x, x)
}
sum fn (acc: Int, x: Int): Int {
    add(acc, x)
}
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter(is-even)
    |> map(square)
    |> reduce(sum, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plans = detect_pipelines(&func.instructions, &prog);
        assert!(
            !plans.is_empty(),
            "expected named-function pipeline; functions={:#?}; instrs={:#?}",
            prog.functions,
            func.instructions
        );
        let plan = &plans[0];
        assert_eq!(plan.stages.len(), 3);
        let source = Val::Vec((1..=10).map(Val::Int).collect());
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Int(220))
        );
    }

    #[test]
    fn run_pipeline_deopts_on_non_int_source() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> filter((x) { is-zero(mod(x, 2)) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        let source = Val::Vec(vec![Val::Int(2), Val::from("oops"), Val::Int(4)]);
        assert_eq!(run_pipeline(plan, &source), FusedRun::Deopt);
    }

    #[test]
    fn run_pipeline_some_terminal() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Bool {
    range(1, add(n, 1))
    |> filter((x) { gt(x, 2) })
    |> some((x) { eq(x, 4) })
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[1].stage, HofStage::Some);
        let source = Val::Vec((1..=5).map(Val::Int).collect());
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Bool(true))
        );

        let source = Val::Vec((1..=3).map(Val::Int).collect());
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Bool(false))
        );
    }

    #[test]
    fn run_pipeline_all_terminal() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Bool {
    range(1, add(n, 1))
    |> map((x) { mul(x, 2) })
    |> all((x) { gt(x, 0) })
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[1].stage, HofStage::All);
        let source = Val::Vec((1..=5).map(Val::Int).collect());
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Bool(true))
        );

        let source = Val::Vec(vec![Val::Int(1), Val::Int(-1), Val::Int(3)]);
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::Bool(false))
        );
    }

    #[test]
    fn does_not_detect_pmap() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Int {
    range(1, add(n, 1))
    |> pmap((x) { mul(x, x) })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plans = detect_pipelines(&func.instructions, &prog);
        // pmap is parallel; the reduce alone is < 2 sequential stages chained
        // from a fusible source, so no plan should form through pmap.
        assert!(
            plans
                .iter()
                .all(|p| p.stages.iter().all(|s| s.stage != HofStage::Map)),
            "pmap must never be fused"
        );
    }

    fn record(count: i64) -> Val {
        let mut m = indexmap::IndexMap::new();
        m.insert(Val::from("count"), Val::Int(count));
        Val::Map(Box::new(m))
    }

    #[test]
    fn run_pipeline_property_access() {
        let prog = compile(
            r#"::t ns
f fn (rows: Vec<Map>): Int {
    rows
    |> filter((x) { gt(x.count, 0) })
    |> map((x) { x.count })
    |> reduce((acc, x) { add(acc, x) }, 0)
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        // counts: 3, -1 (dropped), 0 (dropped), 5  -> 3 + 5 = 8
        let source = Val::Vec(vec![record(3), record(-1), record(0), record(5)]);
        assert_eq!(run_pipeline(plan, &source), FusedRun::Produced(Val::Int(8)));
    }

    /// Register reuse around a direct `reduce(range(n), lambda, "")` call must
    /// not leave `source_reg` pointing at a register that is clobbered before
    /// the call executes. For a single-stage pipeline the runtime source is the
    /// terminal call's `args_start` (guaranteed live at dispatch); tracing the
    /// argument *back* through `Move`s can land on a register later reused for
    /// the lambda, which previously forced a de-opt on every invocation.
    #[test]
    fn detect_reads_source_from_live_arg_register() {
        let prog = compile(
            r#"::t ns
f fn (n: Int): Str {
    reduce(range(n), (acc, i) { concat(acc, `item-${i}-`) }, "")
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plans = detect_pipelines(&func.instructions, &prog);
        assert_eq!(plans.len(), 1, "expected exactly one pipeline");
        let plan = &plans[0];
        let terminal_args_start = match &func.instructions[plan.last_stage_ip] {
            Instruction::Call { args_start, .. } => *args_start,
            other => panic!("expected terminal Call, got {other:?}"),
        };
        assert_eq!(
            plan.source_reg, terminal_args_start,
            "source must be read from the live args_start register"
        );
    }

    #[test]
    fn run_pipeline_string_concat() {
        let prog = compile(
            r#"::t ns
f fn (items: Vec<Str>): Str {
    items |> map((s) { concat(s, "!") }) |> reduce((acc, s) { concat(acc, s) }, "")
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        assert!(
            string_reduce_builder(plan.stages.last().expect("terminal")).is_some(),
            "string concat reduce should use the builder specialization"
        );
        let source = Val::Vec(vec![Val::from("a"), Val::from("b"), Val::from("c")]);
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::from("a!b!c!"))
        );
    }

    #[test]
    fn run_pipeline_string_concat_mixed_append_values() {
        let prog = compile(
            r#"::t ns
f fn (items: Vec<Int>): Str {
    items |> reduce((acc, i) { concat(acc, i) }, "")
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        let source = Val::Vec(vec![Val::Int(1), Val::Int(2), Val::Int(3)]);
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::from("123"))
        );
    }

    #[test]
    fn run_pipeline_template_map_part_deopts() {
        let prog = compile(
            r#"::t ns
f fn (items: Vec<Map>): Str {
    items |> reduce((acc, row) { concat(acc, `count=${row}`) }, "")
}
"#,
        );
        let func = prog
            .functions
            .iter()
            .find(|f| f.name.ends_with("/f"))
            .expect("fn f");
        let plan = &detect_pipelines(&func.instructions, &prog)[0];
        let source = Val::Vec(vec![record(1)]);
        assert_eq!(run_pipeline(plan, &source), FusedRun::Deopt);
    }
}
