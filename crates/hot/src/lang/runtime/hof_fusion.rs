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
use crate::val::Val;
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
    /// A pure op that exists in the registry but is not lowerable in Tier 1.
    UnsupportedOp(PureOp),
    /// Wrong operand arity for an op.
    BadArity(PureOp),
    /// A `LoadVar` of something that is not a known parameter (unknown capture).
    UnknownCapture(String),
    /// A constant kind that is not an `Int`/`Bool`.
    UnsupportedConst,
    /// A register was read before being defined within the lambda/region.
    UndefinedRegister(RegisterId),
    /// Operand types did not match the op (e.g. arithmetic on a Bool).
    TypeMismatch,
    /// The pipeline shape (sources/stages/uses) was not a fusible chain.
    PipelineShape,
}

impl Bailout {
    /// Stable telemetry label for this bailout.
    pub fn label(&self) -> &'static str {
        match self {
            Bailout::UnsupportedInstruction(_) => "unsupported.hof_instruction",
            Bailout::UnsupportedHotlib(_) => "unsupported.hof_hotlib",
            Bailout::UnsupportedOp(_) => "unsupported.hof_op",
            Bailout::BadArity(_) => "unsupported.hof_arity",
            Bailout::UnknownCapture(_) => "unsupported.hof_capture",
            Bailout::UnsupportedConst => "unsupported.hof_const",
            Bailout::UndefinedRegister(_) => "unsupported.hof_undef_reg",
            Bailout::TypeMismatch => "unsupported.hof_type",
            Bailout::PipelineShape => "unsupported.hof_pipeline_shape",
        }
    }
}

/// Inferred static type of a lowered expression. Tier 1 is closed over `Int`
/// and `Bool` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
    Int,
    Bool,
}

/// Pure expression IR for a lowered lambda body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PureExpr {
    /// Reference to lambda parameter by index.
    Param(usize),
    ConstInt(i64),
    ConstBool(bool),
    /// A pure op applied to operands.
    Op(PureOp, Vec<PureExpr>),
}

/// A lambda body lowered to pure expression IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoweredLambda {
    pub param_count: usize,
    pub expr: PureExpr,
    /// Static type of the result (`Int` for map/reduce, `Bool` for filter).
    pub result_type: ExprType,
}

/// A runtime scalar value handled by the Stage 1 fused evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureValue {
    Int(i64),
    Bool(bool),
}

impl PureValue {
    pub fn as_int(self) -> Option<i64> {
        match self {
            PureValue::Int(i) => Some(i),
            PureValue::Bool(_) => None,
        }
    }

    pub fn as_bool(self) -> Option<bool> {
        match self {
            PureValue::Bool(b) => Some(b),
            PureValue::Int(_) => None,
        }
    }

    pub fn to_val(self) -> Val {
        match self {
            PureValue::Int(i) => Val::Int(i),
            PureValue::Bool(b) => Val::Bool(b),
        }
    }
}

/// Outcome of evaluating the expression IR for a single element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Value(PureValue),
    /// A runtime condition (e.g. divide/modulo by zero) requires falling back
    /// to the interpreter so the exact error value/message is produced.
    DeoptToInterpreter,
}

// ---------------------------------------------------------------------------
// Lambda lowering
// ---------------------------------------------------------------------------

/// True if a pure op is lowerable by the Tier 1 evaluator.
///
/// `Div` is excluded because integer division can yield a `Dec` (non-whole
/// results), breaking the `Int`-closed assumption. `Abs`/`Min`/`Max` are
/// deferred.
fn op_is_tier1(op: PureOp) -> bool {
    matches!(
        op,
        PureOp::Add
            | PureOp::Sub
            | PureOp::Mul
            | PureOp::Mod
            | PureOp::IsZero
            | PureOp::Eq
            | PureOp::Ne
            | PureOp::Lt
            | PureOp::Lte
            | PureOp::Gt
            | PureOp::Gte
            | PureOp::Not
    )
}

/// Result type and operand-type expectation of an op.
fn op_types(op: PureOp) -> (ExprType, ExprType) {
    // (result_type, operand_type)
    match op {
        PureOp::Add | PureOp::Sub | PureOp::Mul | PureOp::Mod | PureOp::Div => {
            (ExprType::Int, ExprType::Int)
        }
        PureOp::Abs | PureOp::Min | PureOp::Max | PureOp::Neg => (ExprType::Int, ExprType::Int),
        PureOp::IsZero
        | PureOp::Eq
        | PureOp::Ne
        | PureOp::Lt
        | PureOp::Lte
        | PureOp::Gt
        | PureOp::Gte => (ExprType::Bool, ExprType::Int),
        PureOp::Not => (ExprType::Bool, ExprType::Bool),
    }
}

/// Expected operand count of an op.
fn op_arity(op: PureOp) -> usize {
    match op {
        PureOp::IsZero | PureOp::Not | PureOp::Abs | PureOp::Neg => 1,
        _ => 2,
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
    /// A loaded function name (used as a `Call` target).
    FnName(String),
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
                        regs.insert(*dest, RegContent::FnName(s.to_string()));
                    }
                    Some(Constant::StringRef(s)) | Some(Constant::FunctionRef(s)) => {
                        regs.insert(*dest, RegContent::FnName(s.to_string()));
                    }
                    _ => {
                        // Unsupported constant kind; only an error if later used,
                        // but recording nothing means a later use will bail with
                        // UndefinedRegister. Be explicit instead.
                        regs.remove(dest);
                    }
                }
            }
            Instruction::LoadVar { dest, var_name } => {
                let name = const_str(program, *var_name)
                    .ok_or(Bailout::UnsupportedInstruction("LoadVar:badname"))?;
                if let Some(&idx) = param_index.get(name.as_str()) {
                    // Parameters are assumed Int in Tier 1 (range elements / acc).
                    regs.insert(*dest, RegContent::Expr(PureExpr::Param(idx), ExprType::Int));
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
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let name = match regs.get(function) {
                    Some(RegContent::FnName(n)) => n.clone(),
                    _ => return Err(Bailout::UndefinedRegister(*function)),
                };
                let desc = libmap::pure_op(&name)
                    .ok_or_else(|| Bailout::UnsupportedHotlib(name.clone()))?;
                let op = desc.op;
                if !op_is_tier1(op) {
                    return Err(Bailout::UnsupportedOp(op));
                }
                if *args_count as usize != op_arity(op) {
                    return Err(Bailout::BadArity(op));
                }
                let mut operands = Vec::with_capacity(*args_count as usize);
                let (_res_ty, operand_ty) = op_types(op);
                for i in 0..*args_count as u32 {
                    let reg = args_start + i;
                    match regs.get(&reg) {
                        Some(RegContent::Expr(e, ty)) => {
                            if *ty != operand_ty {
                                return Err(Bailout::TypeMismatch);
                            }
                            operands.push(e.clone());
                        }
                        _ => return Err(Bailout::UndefinedRegister(reg)),
                    }
                }
                let (res_ty, _) = op_types(op);
                regs.insert(*dest, RegContent::Expr(PureExpr::Op(op, operands), res_ty));
            }
            Instruction::Return { value } => {
                match regs.get(value) {
                    Some(RegContent::Expr(e, ty)) => {
                        result = Some((e.clone(), *ty));
                    }
                    _ => return Err(Bailout::UndefinedRegister(*value)),
                }
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
    let (res_ty, operand_ty) = op_types(op);
    let l = match regs.get(&left) {
        Some(RegContent::Expr(e, ty)) if *ty == operand_ty => e.clone(),
        Some(RegContent::Expr(_, _)) => return Err(Bailout::TypeMismatch),
        _ => return Err(Bailout::UndefinedRegister(left)),
    };
    let r = match regs.get(&right) {
        Some(RegContent::Expr(e, ty)) if *ty == operand_ty => e.clone(),
        Some(RegContent::Expr(_, _)) => return Err(Bailout::TypeMismatch),
        _ => return Err(Bailout::UndefinedRegister(right)),
    };
    regs.insert(dest, RegContent::Expr(PureExpr::Op(op, vec![l, r]), res_ty));
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
            Some(v) => EvalOutcome::Value(*v),
            None => EvalOutcome::DeoptToInterpreter,
        },
        PureExpr::ConstInt(i) => EvalOutcome::Value(PureValue::Int(*i)),
        PureExpr::ConstBool(b) => EvalOutcome::Value(PureValue::Bool(*b)),
        PureExpr::Op(op, operands) => eval_op(*op, operands, args),
    }
}

fn eval_op(op: PureOp, operands: &[PureExpr], args: &[PureValue]) -> EvalOutcome {
    macro_rules! ev {
        ($idx:expr) => {
            match eval_expr(&operands[$idx], args) {
                EvalOutcome::Value(v) => v,
                deopt => return deopt,
            }
        };
    }
    macro_rules! int_of {
        ($v:expr) => {
            match $v.as_int() {
                Some(i) => i,
                None => return EvalOutcome::DeoptToInterpreter,
            }
        };
    }
    macro_rules! bool_of {
        ($v:expr) => {
            match $v.as_bool() {
                Some(b) => b,
                None => return EvalOutcome::DeoptToInterpreter,
            }
        };
    }

    match op {
        PureOp::Add => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Int(a.wrapping_add(b)))
        }
        PureOp::Sub => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Int(a.wrapping_sub(b)))
        }
        PureOp::Mul => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Int(a.wrapping_mul(b)))
        }
        PureOp::Mod => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            if b == 0 {
                // Interpreter returns a "Modulo by zero" error value; defer.
                return EvalOutcome::DeoptToInterpreter;
            }
            EvalOutcome::Value(PureValue::Int(a % b))
        }
        PureOp::IsZero => {
            let a = int_of!(ev!(0));
            EvalOutcome::Value(PureValue::Bool(a == 0))
        }
        PureOp::Eq => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a == b))
        }
        PureOp::Ne => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a != b))
        }
        PureOp::Lt => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a < b))
        }
        PureOp::Lte => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a <= b))
        }
        PureOp::Gt => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a > b))
        }
        PureOp::Gte => {
            let a = int_of!(ev!(0));
            let b = int_of!(ev!(1));
            EvalOutcome::Value(PureValue::Bool(a >= b))
        }
        PureOp::Not => {
            let a = bool_of!(ev!(0));
            EvalOutcome::Value(PureValue::Bool(!a))
        }
        // Not lowered in Tier 1; lowering rejects these, so this is unreachable
        // in practice. Defer defensively.
        PureOp::Div | PureOp::Abs | PureOp::Min | PureOp::Max | PureOp::Neg => {
            EvalOutcome::DeoptToInterpreter
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
    /// `reduce` initial accumulator value.
    pub initial: Option<PureValue>,
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
    initial: Option<PureValue>,
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
                    trace_int_const(instrs, program, ip, args_start + 2).map(PureValue::Int)
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

        // A useful pipeline has at least 2 stages and ends in a terminal stage.
        if chain.len() < 2 {
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
                initial: sc.initial,
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
/// Tier 1 supports `Int`-valued `Vec` sources, `filter`/`map` transform stages,
/// and `reduce`/`length` terminal stages.
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
        match terminal.initial {
            Some(v) => reduce_acc = Some(v),
            None => return FusedRun::Deopt,
        }
    }

    for item in items {
        let mut cur = match item {
            Val::Int(i) => PureValue::Int(*i),
            _ => return FusedRun::Deopt,
        };

        let mut keep = true;
        for t in transforms {
            match t.stage {
                HofStage::Filter => match eval_expr(&t.lambda.expr, &[cur]) {
                    EvalOutcome::Value(PureValue::Bool(b)) => {
                        if !b {
                            keep = false;
                            break;
                        }
                    }
                    _ => return FusedRun::Deopt,
                },
                HofStage::Map => match eval_expr(&t.lambda.expr, &[cur]) {
                    EvalOutcome::Value(v @ PureValue::Int(_)) => cur = v,
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
                let acc = reduce_acc.expect("reduce acc initialized");
                match eval_expr(&terminal.lambda.expr, &[acc, cur]) {
                    EvalOutcome::Value(v @ PureValue::Int(_)) => reduce_acc = Some(v),
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
        HofStage::Reduce => reduce_acc.expect("reduce acc").to_val(),
        HofStage::Length => Val::Int(count),
        _ => return FusedRun::Deopt,
    };
    FusedRun::Produced(result)
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

/// Walk back to find an Int constant loaded into `reg`.
fn trace_int_const(
    instrs: &[Instruction],
    program: &BytecodeProgram,
    call_ip: usize,
    reg: RegisterId,
) -> Option<i64> {
    let mut cur = reg;
    for i in (0..call_ip).rev() {
        match &instrs[i] {
            Instruction::Move { dest, src } if *dest == cur => {
                cur = *src;
            }
            Instruction::LoadConst { dest, constant } if *dest == cur => {
                match program.constants.get(*constant as usize) {
                    Some(Constant::Val(Val::Int(i))) => return Some(*i),
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
            PureExpr::Op(
                PureOp::IsZero,
                vec![PureExpr::Op(
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
        assert_eq!(lowered.result_type, ExprType::Int);
        assert_eq!(
            lowered.expr,
            PureExpr::Op(PureOp::Mul, vec![PureExpr::Param(0), PureExpr::Param(0)])
        );
    }

    #[test]
    fn eval_matches_expected() {
        // is-zero(mod(x,2))
        let pred = PureExpr::Op(
            PureOp::IsZero,
            vec![PureExpr::Op(
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
        let e = PureExpr::Op(PureOp::Mod, vec![PureExpr::Param(0), PureExpr::ConstInt(0)]);
        assert_eq!(
            eval_expr(&e, &[PureValue::Int(10)]),
            EvalOutcome::DeoptToInterpreter
        );
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
        assert_eq!(p.stages[2].initial, Some(PureValue::Int(0)));
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
}
