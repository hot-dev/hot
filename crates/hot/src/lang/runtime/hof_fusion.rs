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
    /// Static type of the result where known (`Bool` for predicates, otherwise
    /// `Other`/`Int`). Used only as a hint; evaluation dispatches on the runtime
    /// value.
    pub result_type: ExprType,
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
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            PureValue::Bool(b) => Some(*b),
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
    use std::collections::HashMap;

    // Parameter name -> index.
    let mut param_index: HashMap<&str, usize> = HashMap::new();
    for (i, p) in lambda.parameters.iter().enumerate() {
        param_index.insert(p.as_str(), i);
    }

    let mut regs: HashMap<RegisterId, RegContent> = HashMap::new();
    let mut result: Option<(PureExpr, ExprType)> = None;

    for ins in &lambda.instructions {
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
    Ok(LoweredLambda {
        param_count: lambda.parameters.len(),
        expr,
        result_type,
    })
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

/// Field/property projection. The base must evaluate to a record (`Val::Map`);
/// anything else (or a missing key) de-opts so the interpreter reproduces exact
/// access semantics.
fn eval_field(base: &PureExpr, key: &str, args: &[PureValue]) -> EvalOutcome {
    let base_val = match eval_expr(base, args) {
        EvalOutcome::Value(v) => v.into_val(),
        deopt => return deopt,
    };
    match base_val {
        Val::Map(map) => match map.get(&Val::from(key)) {
            Some(v) => EvalOutcome::Value(PureValue::from_val(v.clone())),
            None => EvalOutcome::DeoptToInterpreter,
        },
        _ => EvalOutcome::DeoptToInterpreter,
    }
}

/// Template interpolation. Each part is stringified exactly as the interpreter's
/// non-typed path does (`str_internal`); a record/typed `Map` part de-opts
/// because it could dispatch to a user-defined `Str` implementation.
fn eval_template(parts: &[PureExpr], args: &[PureValue]) -> EvalOutcome {
    let max_bytes = crate::lang::runtime::limits::max_string_bytes();
    let mut out = String::new();
    for part in parts {
        let v = match eval_expr(part, args) {
            EvalOutcome::Value(v) => v.into_val(),
            deopt => return deopt,
        };
        if matches!(v, Val::Map(_)) {
            return EvalOutcome::DeoptToInterpreter;
        }
        let piece = match str_internal(std::slice::from_ref(&v)) {
            HotResult::Ok(Val::Str(s)) => s.to_string(),
            HotResult::Ok(other) => other.to_string(),
            HotResult::Err(_) => return EvalOutcome::DeoptToInterpreter,
        };
        match out.len().checked_add(piece.len()) {
            Some(total) if total <= max_bytes => out.push_str(&piece),
            _ => return EvalOutcome::DeoptToInterpreter,
        }
    }
    EvalOutcome::Value(PureValue::Other(Val::from(out)))
}

fn eval_op(op: PureOp, full_name: &str, operands: &[PureExpr], args: &[PureValue]) -> EvalOutcome {
    // Evaluate operands first.
    let mut vals: Vec<PureValue> = Vec::with_capacity(operands.len());
    for o in operands {
        match eval_expr(o, args) {
            EvalOutcome::Value(v) => vals.push(v),
            deopt => return deopt,
        }
    }

    // Inlined fast path for the common `Int`/`Bool` case. Returns `None` to fall
    // through to the exact hotlib implementation (mixed/`Dec`/`Str` operands,
    // integer overflow, divide/modulo by zero, ...), guaranteeing parity.
    if let Some(out) = fast_op(op, &vals) {
        return out;
    }

    // Parity fallback: call the registered pure `LibFn` with the operand values.
    let arg_vals: Vec<Val> = vals.iter().map(PureValue::to_val).collect();
    match libmap::call_pure_lib(full_name, &arg_vals) {
        Some(HotResult::Ok(v)) => {
            // Error values (divide/modulo by zero, type errors, ...) carry
            // interpreter-specific propagation semantics; de-opt so they are
            // reproduced exactly.
            if v.is_err() {
                EvalOutcome::DeoptToInterpreter
            } else {
                EvalOutcome::Value(PureValue::from_val(v))
            }
        }
        // `HotResult::Err` or a non-pure/unknown function: defer to the
        // interpreter.
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
        // `Div` can yield a `Dec`; `Abs`/`Min`/`Max`/`Neg`/`Concat` are handled
        // by the hotlib fallback for full parity.
        PureOp::Div | PureOp::Abs | PureOp::Min | PureOp::Max | PureOp::Neg | PureOp::Concat => {
            None
        }
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

/// A recognized fusible pipeline within a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelinePlan {
    /// ip of the first stage `Call`.
    pub first_stage_ip: usize,
    /// ip of the last (terminal) stage `Call`.
    pub last_stage_ip: usize,
    /// Register holding the input collection at the first stage.
    pub source_reg: RegisterId,
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
    lambda_id: u32,
    initial: Option<Val>,
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
            let (lambda_id, initial) = if role.stage == HofStage::Length {
                (u32::MAX, None)
            } else {
                if *args_count < 2 {
                    continue;
                }
                let lam_reg = args_start + 1;
                let Some(id) = trace_lambda_const(instrs, ip, lam_reg) else {
                    continue;
                };
                let initial = if role.stage == HofStage::Reduce && *args_count >= 3 {
                    trace_const_val(instrs, program, ip, args_start + 2)
                } else {
                    None
                };
                (id, initial)
            };
            stage_calls.push(StageCall {
                ip,
                stage: role.stage,
                dest: *dest,
                input_reg,
                lambda_id,
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
                LoweredLambda {
                    param_count: 1,
                    expr: PureExpr::Param(0),
                    result_type: ExprType::Int,
                }
            } else {
                match lambda_from_const(program, sc.lambda_id)
                    .ok_or(Bailout::PipelineShape)
                    .and_then(|l| lower_lambda(program, l))
                {
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
        plans.push(PipelinePlan {
            first_stage_ip: stage_calls[chain[0]].ip,
            last_stage_ip: stage_calls[*chain.last().unwrap()].ip,
            source_reg: stage_calls[chain[0]].input_reg,
            result_reg: stage_calls[*chain.last().unwrap()].dest,
            stages,
        });
    }

    plans
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

/// Run a fused pipeline in a single pass over `source`. Pure: reads `source`,
/// writes nothing. Returns `Deopt` (discarding any partial work) whenever a
/// runtime guard fails so the interpreter can reproduce exact semantics.
///
/// Supports `Vec` sources of any element type (`Int`/`Bool` on the fast path,
/// `Dec`/`Str`/records via the hotlib parity fallback), `filter`/`map`
/// transform stages, and `reduce`/`length` terminal stages.
pub fn run_pipeline(plan: &PipelinePlan, source: &Val) -> FusedRun {
    let items = match source {
        Val::Vec(items) => items,
        _ => return FusedRun::Deopt,
    };

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
        HofStage::Reduce | HofStage::Length => {}
        _ => return FusedRun::Deopt,
    }

    // Terminal accumulators.
    let mut reduce_acc: Option<PureValue> = None;
    let mut count: i64 = 0;
    if terminal.stage == HofStage::Reduce {
        match &terminal.initial {
            Some(v) => reduce_acc = Some(PureValue::from_val(v.clone())),
            None => return FusedRun::Deopt,
        }
    }

    for item in items {
        let mut cur = PureValue::from_val(item.clone());

        let mut keep = true;
        for t in transforms {
            match t.stage {
                HofStage::Filter => match eval_expr(&t.lambda.expr, std::slice::from_ref(&cur)) {
                    EvalOutcome::Value(PureValue::Bool(b)) => {
                        if !b {
                            keep = false;
                            break;
                        }
                    }
                    // A non-`Bool` predicate result or a de-opt means the
                    // interpreter must decide truthiness/errors.
                    _ => return FusedRun::Deopt,
                },
                HofStage::Map => match eval_expr(&t.lambda.expr, std::slice::from_ref(&cur)) {
                    EvalOutcome::Value(v) if !value_is_err(&v) => cur = v,
                    _ => return FusedRun::Deopt,
                },
                _ => return FusedRun::Deopt,
            }
        }

        if !keep {
            continue;
        }

        match terminal.stage {
            HofStage::Reduce => {
                let acc = reduce_acc.take().expect("reduce acc initialized");
                match eval_expr(&terminal.lambda.expr, &[acc, cur]) {
                    EvalOutcome::Value(v) if !value_is_err(&v) => reduce_acc = Some(v),
                    _ => return FusedRun::Deopt,
                }
            }
            HofStage::Length => {
                count += 1;
            }
            _ => return FusedRun::Deopt,
        }
    }

    let result = match terminal.stage {
        HofStage::Reduce => reduce_acc.expect("reduce acc").into_val(),
        HofStage::Length => Val::Int(count),
        _ => return FusedRun::Deopt,
    };
    FusedRun::Produced(result)
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

/// Walk back to find the LambdaInfo constant id loaded into `reg`.
fn trace_lambda_const(instrs: &[Instruction], call_ip: usize, reg: RegisterId) -> Option<u32> {
    let mut cur = reg;
    for i in (0..call_ip).rev() {
        match &instrs[i] {
            Instruction::Move { dest, src } if *dest == cur => {
                cur = *src;
            }
            Instruction::LoadConst { dest, constant } if *dest == cur => {
                return Some(*constant);
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
        let source = Val::Vec(vec![Val::from("a"), Val::from("b"), Val::from("c")]);
        assert_eq!(
            run_pipeline(plan, &source),
            FusedRun::Produced(Val::from("a!b!c!"))
        );
    }
}
