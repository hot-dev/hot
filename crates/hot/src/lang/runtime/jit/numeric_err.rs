//! Where the Err of a compiled numeric op goes.
//!
//! An Int `add`, `sub` or `mul` that overflows, and a `mod` or `div` by
//! zero, gives an Err value. Compiled code keeps the result of such an op in
//! a raw register, which can't hold an Err, so it routes the Err by what the
//! instructions after the op do with the result, as the interpreter would:
//!
//! - where every path consumes the result first, the Err halts at the op
//!   (`Consumed`);
//! - where every path returns it, the Err is returned at the op
//!   (`Returned`);
//! - where no path reads it, it is ignored (`Unread`).
//!
//! Any other use needs the Err as a value (`Kept`). Acting at the op instead
//! of where the result is used is the same only when nothing between them
//! can be observed, so those instructions must neither run other code nor
//! halt for another reason.

use super::{
    KnownCoreCall, find_function_by_suffix, find_function_exact, known_core_call,
    resolve_const_function_name,
};
use crate::lang::bytecode::{
    BytecodeProgram, Constant, FlowResultModifier, FlowType, FunctionId, Instruction, LambdaInfo,
};
use crate::val::Val;
use ahash::AHashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericErrUse {
    /// Every path consumes the result before anything else can be observed.
    Consumed,
    /// Every path returns the result from the function.
    Returned,
    /// No path reads the result.
    Unread,
    /// The result is used some other way, so it must be a real value.
    Kept,
}

/// What the analysis knows about the code around the op.
pub(super) struct ErrUseScope<'a> {
    pub program: &'a BytecodeProgram,
    pub instructions: &'a [Instruction],
    /// What returning the result amounts to: `Returned` in a function's
    /// body; in an inlined lazy argument, the use of the call it is inlined
    /// into.
    pub returned: NumericErrUse,
    /// Whether a register set before the op holds a value that can't be an
    /// Err.
    pub plain_register: &'a dyn Fn(u32) -> bool,
    /// For a parameter or local, whether its value can't be an Err; `None`
    /// for a name that isn't one.
    pub plain_local: &'a dyn Fn(&str) -> Option<bool>,
}

const MAX_PATHS: usize = 32;
const MAX_STEPS: usize = 512;

/// How the result of the numeric op at `ip`, written to `dest`, is used.
pub(super) fn numeric_err_use(scope: &ErrUseScope<'_>, ip: usize, dest: u32) -> NumericErrUse {
    let mut pending = vec![ErrPath::new(ip + 1, dest)];
    let mut outcome = None;
    let mut paths = 1;
    let mut steps = 0;
    while let Some(mut path) = pending.pop() {
        let path_use = loop {
            steps += 1;
            if steps > MAX_STEPS {
                return NumericErrUse::Kept;
            }
            match path.step(scope) {
                Step::Next => {}
                Step::Split(other) => {
                    paths += 1;
                    if paths > MAX_PATHS {
                        return NumericErrUse::Kept;
                    }
                    pending.push(other);
                }
                Step::Done(path_use) => break path_use,
            }
        };
        match outcome {
            _ if path_use == NumericErrUse::Kept => return NumericErrUse::Kept,
            None => outcome = Some(path_use),
            Some(previous) if previous == path_use => {}
            // Which path runs isn't known at the op.
            Some(_) => return NumericErrUse::Kept,
        }
    }
    outcome.unwrap_or(NumericErrUse::Kept)
}

enum Step {
    Next,
    /// Go on, and also follow this other path.
    Split(ErrPath),
    Done(NumericErrUse),
}

/// One path through the instructions after the op.
#[derive(Clone)]
struct ErrPath {
    ip: usize,
    /// Registers holding the result.
    regs: Vec<u32>,
    /// Variables bound to the result, with the depth of the scope that bound
    /// them.
    vars: Vec<(u32, i32)>,
    /// Registers set since the op, and whether they can't hold an Err.
    set: AHashMap<u32, bool>,
    /// Registers set since the op to `true`.
    truths: Vec<u32>,
    /// Variables bound to something else since the op.
    rebound: Vec<u32>,
    /// Flows open on this path: whether the branch that ended gave the result.
    flows: Vec<bool>,
    /// Scope depth relative to the op's.
    depth: i32,
}

impl ErrPath {
    fn new(ip: usize, dest: u32) -> Self {
        Self {
            ip,
            regs: vec![dest],
            vars: Vec::new(),
            set: AHashMap::new(),
            truths: Vec::new(),
            rebound: Vec::new(),
            flows: Vec::new(),
            depth: 0,
        }
    }

    fn holds_result(&self, reg: u32) -> bool {
        self.regs.contains(&reg)
    }

    /// Whether `reg` holds a value that can't be an Err.
    fn plain(&self, reg: u32, scope: &ErrUseScope<'_>) -> bool {
        !self.holds_result(reg)
            && self
                .set
                .get(&reg)
                .copied()
                .unwrap_or_else(|| (scope.plain_register)(reg))
    }

    fn set_register(&mut self, reg: u32, plain: bool) {
        self.regs.retain(|held| *held != reg);
        self.truths.retain(|held| *held != reg);
        self.set.insert(reg, plain);
    }

    fn set_result(&mut self, reg: u32) {
        self.truths.retain(|held| *held != reg);
        self.set.remove(&reg);
        if !self.regs.contains(&reg) {
            self.regs.push(reg);
        }
    }

    fn step(&mut self, scope: &ErrUseScope<'_>) -> Step {
        use NumericErrUse::*;
        // Nothing can read the result any more.
        if self.regs.is_empty() && self.vars.is_empty() {
            return Step::Done(Unread);
        }
        let ip = self.ip;
        let Some(instruction) = scope.instructions.get(ip) else {
            return Step::Done(Kept);
        };
        self.ip += 1;
        match instruction {
            Instruction::LoadConst { dest, constant } => {
                let value = match scope.program.constants.get(*constant as usize) {
                    Some(Constant::Val(value)) => Some(value),
                    Some(_) => None,
                    None => return Step::Done(Kept),
                };
                // A lambda or lazy argument made here could read it later.
                if value.is_some_and(is_lambda) {
                    return Step::Done(Kept);
                }
                self.set_register(*dest, !value.is_some_and(Val::is_err));
                if value == Some(&Val::Bool(true)) {
                    self.truths.push(*dest);
                }
                Step::Next
            }
            Instruction::LoadFunctionRef { dest, .. } | Instruction::LoadTypeRef { dest, .. } => {
                self.set_register(*dest, true);
                Step::Next
            }
            Instruction::Move { dest, src } | Instruction::Pipe { dest, src } => {
                if self.holds_result(*src) {
                    self.set_result(*dest);
                } else {
                    let plain = self.plain(*src, scope);
                    let truth = self.truths.contains(src);
                    self.set_register(*dest, plain);
                    if truth {
                        self.truths.push(*dest);
                    }
                }
                Step::Next
            }
            Instruction::LoadVar { dest, var_name } => {
                if self.vars.iter().any(|(name, _)| name == var_name) {
                    self.set_result(*dest);
                    return Step::Next;
                }
                if self.rebound.contains(var_name) {
                    self.set_register(*dest, false);
                    return Step::Next;
                }
                let Some(name) = const_str(scope.program, *var_name) else {
                    return Step::Done(Kept);
                };
                // Any other name is looked up when it runs, which can run code.
                match (scope.plain_local)(name) {
                    Some(plain) => {
                        self.set_register(*dest, plain);
                        Step::Next
                    }
                    None => Step::Done(Kept),
                }
            }
            Instruction::StoreVar {
                var_name, value, ..
            } => {
                if let Some(index) = self.vars.iter().position(|(name, _)| name == var_name) {
                    // Shadowed in an inner scope, the binding returns when
                    // that scope ends.
                    if self.vars[index].1 != self.depth {
                        return Step::Done(Kept);
                    }
                    self.vars.remove(index);
                }
                if self.holds_result(*value) {
                    self.vars.push((*var_name, self.depth));
                } else if !self.rebound.contains(var_name) {
                    self.rebound.push(*var_name);
                }
                Step::Next
            }
            Instruction::EnterScope { .. } => {
                self.depth += 1;
                Step::Next
            }
            Instruction::ExitScope => {
                self.depth -= 1;
                let depth = self.depth;
                self.vars.retain(|(_, bound)| *bound <= depth);
                Step::Next
            }
            Instruction::ProvenArgs { .. } => Step::Next,
            // A constraint check passes an Err through unchanged.
            Instruction::CheckConstraint {
                dest,
                src,
                test: false,
                ..
            } if self.holds_result(*src) => {
                self.set_result(*dest);
                Step::Next
            }
            Instruction::BeginFlow {
                flow_type: FlowType::Cond,
                result_modifier: FlowResultModifier::One,
                ..
            } => {
                self.flows.push(false);
                Step::Next
            }
            Instruction::CondBranchStart { condition, .. } => {
                // Testing it reads it as a Bool, where an Err is false.
                if self.holds_result(*condition) {
                    return Step::Done(Kept);
                }
                if self.truths.contains(condition) {
                    return Step::Next;
                }
                // The branch may be skipped, going on after its end.
                let Some(end) = branch_end(scope.instructions, ip) else {
                    return Step::Done(Kept);
                };
                let mut skipped = self.clone();
                skipped.ip = end + 1;
                Step::Split(skipped)
            }
            Instruction::CondBranchEnd { result, .. } => {
                // The branch's value is the flow's, and the flow ends.
                let Some(end) = flow_end(scope.instructions, ip) else {
                    return Step::Done(Kept);
                };
                let gives_result = self.holds_result(*result);
                match self.flows.last_mut() {
                    Some(last) => *last = gives_result,
                    None if single_value_cond(scope.instructions, ip) => {
                        self.flows.push(gives_result)
                    }
                    None => return Step::Done(Kept),
                }
                self.ip = end;
                Step::Next
            }
            Instruction::EndFlow { dest } => {
                let gives_result = match self.flows.pop() {
                    Some(gives_result) => gives_result,
                    // A flow open at the op, whose remaining branches were
                    // all skipped.
                    None if single_value_cond(scope.instructions, ip) => false,
                    None => return Step::Done(Kept),
                };
                if gives_result {
                    self.set_result(*dest);
                } else {
                    self.set_register(*dest, false);
                }
                Step::Next
            }
            Instruction::Return { value } => Step::Done(if self.holds_result(*value) {
                scope.returned
            } else {
                Unread
            }),
            Instruction::ReturnIfErr { src } => {
                if self.holds_result(*src) {
                    return Step::Done(scope.returned);
                }
                if self.plain(*src, scope) {
                    return Step::Next;
                }
                // Another Err may return first, dropping the result.
                let mut returned = self.clone();
                returned.regs.clear();
                returned.vars.clear();
                Step::Split(returned)
            }
            Instruction::Call {
                dest,
                function,
                args_start,
                args_count,
            } => {
                let Some(name) =
                    resolve_const_function_name(scope.instructions, scope.program, ip, *function)
                else {
                    return Step::Done(Kept);
                };
                // A call through a parameter or local runs what it holds.
                if !name.starts_with("::") && (scope.plain_local)(&name).is_some() {
                    return Step::Done(Kept);
                }
                if let Some(core) = known_core_call(&name) {
                    return self.core_call(scope, core, *dest, *args_start, *args_count);
                }
                let callee = if name.starts_with("::") {
                    find_function_exact(scope.program, &name, Some(*args_count))
                } else {
                    find_function_by_suffix(scope.program, &name, Some(*args_count))
                };
                match callee {
                    Some(callee) => {
                        self.call(scope, callee as FunctionId, *args_start, *args_count)
                    }
                    None => Step::Done(Kept),
                }
            }
            Instruction::CallUserFunction {
                function_id,
                args_start,
                args_count,
                ..
            }
            | Instruction::TailCall {
                function_id,
                args_start,
                args_count,
            } => self.call(scope, *function_id, *args_start, *args_count),
            // The VM's own arithmetic and comparisons halt on an Err operand.
            Instruction::Add { dest, left, right }
            | Instruction::Sub { dest, left, right }
            | Instruction::Mul { dest, left, right }
            | Instruction::Gt { dest, left, right }
            | Instruction::Lt { dest, left, right } => {
                self.operands(scope, *dest, &[*left, *right])
            }
            _ => Step::Done(Kept),
        }
    }

    /// A call to a core function compiled inline.
    fn core_call(
        &mut self,
        scope: &ErrUseScope<'_>,
        core: KnownCoreCall,
        dest: u32,
        args_start: u32,
        args_count: u8,
    ) -> Step {
        let args: Vec<u32> = (args_start..args_start + u32::from(args_count)).collect();
        // These take Results.
        if matches!(
            core,
            KnownCoreCall::Not | KnownCoreCall::And | KnownCoreCall::Or
        ) && args.iter().any(|reg| self.holds_result(*reg))
        {
            return Step::Done(NumericErrUse::Kept);
        }
        self.operands(scope, dest, &args)
    }

    /// An operation that consumes `operands` in order and writes `dest`.
    fn operands(&mut self, scope: &ErrUseScope<'_>, dest: u32, operands: &[u32]) -> Step {
        match operands.iter().position(|reg| self.holds_result(*reg)) {
            Some(first) => {
                // An earlier operand that may be an Err would halt first.
                if operands[..first].iter().any(|reg| !self.plain(*reg, scope)) {
                    return Step::Done(NumericErrUse::Kept);
                }
                Step::Done(NumericErrUse::Consumed)
            }
            None => {
                // Of plain values it can't halt or run code, though its own
                // result can be an Err.
                if operands.iter().any(|reg| !self.plain(*reg, scope)) {
                    return Step::Done(NumericErrUse::Kept);
                }
                self.set_register(dest, false);
                Step::Next
            }
        }
    }

    /// A call to a function: it consumes the result unless that parameter
    /// keeps it.
    fn call(
        &mut self,
        scope: &ErrUseScope<'_>,
        callee: FunctionId,
        args_start: u32,
        args_count: u8,
    ) -> Step {
        let args = args_start..args_start + u32::from(args_count);
        // Any other call runs other code first.
        let Some(first) = args.clone().position(|reg| self.holds_result(reg)) else {
            return Step::Done(NumericErrUse::Kept);
        };
        if scope.program.functions.get(callee as usize).is_none()
            || scope.program.keeps_param(callee, first)
        {
            return Step::Done(NumericErrUse::Kept);
        }
        // Arguments before it are consumed first.
        let earlier_may_halt = args.take(first).enumerate().any(|(index, reg)| {
            !scope.program.keeps_param(callee, index) && !self.plain(reg, scope)
        });
        Step::Done(if earlier_may_halt {
            NumericErrUse::Kept
        } else {
            NumericErrUse::Consumed
        })
    }
}

fn is_lambda(value: &Val) -> bool {
    matches!(value, Val::Box(boxed) if boxed.as_any().downcast_ref::<LambdaInfo>().is_some())
}

fn const_str(program: &BytecodeProgram, id: u32) -> Option<&str> {
    match program.constants.get(id as usize)? {
        Constant::Val(Val::Str(name)) | Constant::StringRef(name) => Some(&**name),
        _ => None,
    }
}

/// The `CondBranchEnd` of the branch begun at `start`.
fn branch_end(instructions: &[Instruction], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, instruction) in instructions.iter().enumerate().skip(start + 1) {
        match instruction {
            Instruction::BeginFlow { .. } => depth += 1,
            Instruction::EndFlow { .. } => depth = depth.checked_sub(1)?,
            Instruction::CondBranchEnd { .. } if depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

/// The `EndFlow` of the flow that `ip` is in.
fn flow_end(instructions: &[Instruction], ip: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, instruction) in instructions.iter().enumerate().skip(ip + 1) {
        match instruction {
            Instruction::BeginFlow { .. } => depth += 1,
            Instruction::EndFlow { .. } if depth == 0 => return Some(index),
            Instruction::EndFlow { .. } => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether the flow that `ip` is in is a `cond` giving one value.
fn single_value_cond(instructions: &[Instruction], ip: usize) -> bool {
    let mut depth = 0usize;
    for instruction in instructions[..ip].iter().rev() {
        match instruction {
            Instruction::EndFlow { .. } => depth += 1,
            Instruction::BeginFlow {
                flow_type,
                result_modifier,
                ..
            } => {
                if depth == 0 {
                    return *flow_type == FlowType::Cond
                        && *result_modifier == FlowResultModifier::One;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::FunctionInfo;

    /// A program with the names the cases use, and `::t/f(x)` taking `x`
    /// lazily when `lazy`.
    fn program(lazy: bool) -> BytecodeProgram {
        let mut program = BytecodeProgram::new();
        for name in ["x", "mul", "is-err", "::t/f", "y", "n"] {
            program.constants.push(Constant::Val(Val::Str(name.into())));
        }
        program.constants.push(Constant::Val(Val::Bool(true)));
        program.functions.push(FunctionInfo {
            name: "::t/f".to_string(),
            namespace: "::t".to_string(),
            arity: 1,
            is_variadic: false,
            param_names: vec!["v".to_string()],
            param_types: vec![],
            return_type: 0,
            lazy_params: vec![lazy],
            flow_type: None,
            instructions: vec![],
            register_count: 0,
            source: None,
        });
        program
    }

    const X: u32 = 0;
    const MUL: u32 = 1;
    const IS_ERR: u32 = 2;
    const F: u32 = 3;
    const Y: u32 = 4;
    const N: u32 = 5;
    const TRUE: u32 = 6;

    /// The use of the result of the op at 0, writing register 0, followed by
    /// `rest`. Registers set before the op are plain unless listed in
    /// `may_err`; `n` is a plain local.
    fn use_of(program: &BytecodeProgram, rest: Vec<Instruction>, may_err: &[u32]) -> NumericErrUse {
        let mut instructions = vec![Instruction::Move { dest: 0, src: 99 }];
        instructions.extend(rest);
        let plain_register = |reg: u32| !may_err.contains(&reg);
        let plain_local = |name: &str| (name == "n").then_some(true);
        numeric_err_use(
            &ErrUseScope {
                program,
                instructions: &instructions,
                returned: NumericErrUse::Returned,
                plain_register: &plain_register,
                plain_local: &plain_local,
            },
            0,
            0,
        )
    }

    fn call(dest: u32, function: u32, args_start: u32, args_count: u8) -> Instruction {
        Instruction::Call {
            dest,
            function,
            args_start,
            args_count,
        }
    }

    fn load(dest: u32, constant: u32) -> Instruction {
        Instruction::LoadConst { dest, constant }
    }

    #[test]
    fn returned_result() {
        let program = program(false);
        let rest = vec![
            Instruction::Move { dest: 1, src: 0 },
            Instruction::Return { value: 1 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Returned);
    }

    #[test]
    fn unread_result() {
        let program = program(false);
        let rest = vec![
            Instruction::StoreVar {
                var_name: X,
                value: 0,
                metadata: None,
            },
            load(1, TRUE),
            Instruction::Return { value: 1 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Unread);
    }

    #[test]
    fn consumed_through_a_binding() {
        let program = program(false);
        // x <op>; mul(x, n)
        let rest = vec![
            Instruction::StoreVar {
                var_name: X,
                value: 0,
                metadata: None,
            },
            Instruction::LoadVar {
                dest: 2,
                var_name: X,
            },
            Instruction::LoadVar {
                dest: 3,
                var_name: N,
            },
            load(4, MUL),
            call(5, 4, 2, 2),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Consumed);
    }

    #[test]
    fn an_earlier_argument_that_may_be_an_err_keeps_the_result() {
        let program = program(false);
        // mul(<may be Err>, <op>)
        let rest = vec![
            Instruction::Move { dest: 8, src: 0 },
            load(4, MUL),
            call(5, 4, 7, 2),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest.clone(), &[7]), NumericErrUse::Kept);
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Consumed);
    }

    #[test]
    fn a_call_that_keeps_the_argument_keeps_the_result() {
        let program = program(true);
        let rest = vec![
            load(4, F),
            call(5, 4, 0, 1),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
        let program = super::tests::program(false);
        let rest = vec![
            load(4, F),
            call(5, 4, 0, 1),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Consumed);
    }

    #[test]
    fn an_unknown_callee_keeps_the_result() {
        let program = program(false);
        let rest = vec![
            load(4, IS_ERR),
            call(5, 4, 0, 1),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
    }

    #[test]
    fn other_code_before_the_use_keeps_the_result() {
        let program = program(false);
        // f(n), then mul(<op>, n)
        let rest = vec![
            Instruction::LoadVar {
                dest: 2,
                var_name: N,
            },
            load(4, F),
            call(3, 4, 2, 1),
            Instruction::Move { dest: 6, src: 0 },
            Instruction::Move { dest: 7, src: 2 },
            load(4, MUL),
            call(5, 4, 6, 2),
            Instruction::Return { value: 5 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
    }

    #[test]
    fn a_name_looked_up_at_run_time_keeps_the_result() {
        let program = program(false);
        let rest = vec![
            Instruction::LoadVar {
                dest: 2,
                var_name: Y,
            },
            Instruction::Return { value: 0 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
    }

    #[test]
    fn a_collection_keeps_the_result() {
        let program = program(false);
        let rest = vec![
            Instruction::MakeVec {
                dest: 1,
                elements_start: 0,
                count: 1,
            },
            Instruction::Return { value: 1 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
    }

    #[test]
    fn a_result_read_on_one_branch_only_is_kept() {
        let program = program(false);
        // x <op>; cond { n => { x } => { true } }
        let begin = Instruction::BeginFlow {
            flow_type: FlowType::Cond,
            result_modifier: FlowResultModifier::One,
            source: None,
            origin_name: None,
        };
        let rest = vec![
            Instruction::StoreVar {
                var_name: X,
                value: 0,
                metadata: None,
            },
            begin,
            Instruction::LoadVar {
                dest: 2,
                var_name: N,
            },
            Instruction::CondBranchStart {
                branch_name: X,
                condition: 2,
                skip_target: 0,
            },
            Instruction::LoadVar {
                dest: 3,
                var_name: X,
            },
            Instruction::CondBranchEnd {
                branch_name: X,
                result: 3,
            },
            load(4, TRUE),
            Instruction::CondBranchStart {
                branch_name: X,
                condition: 4,
                skip_target: 0,
            },
            load(5, TRUE),
            Instruction::CondBranchEnd {
                branch_name: X,
                result: 5,
            },
            Instruction::EndFlow { dest: 6 },
            Instruction::Return { value: 6 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Kept);
    }

    #[test]
    fn a_branch_result_returned_is_returned() {
        let program = program(false);
        // The op is a branch's value; the flow's value is returned.
        let mut instructions = vec![
            Instruction::BeginFlow {
                flow_type: FlowType::Cond,
                result_modifier: FlowResultModifier::One,
                source: None,
                origin_name: None,
            },
            load(1, TRUE),
            Instruction::CondBranchStart {
                branch_name: X,
                condition: 1,
                skip_target: 0,
            },
            Instruction::EnterScope {
                scope_type: crate::lang::bytecode::ScopeType::Flow,
            },
        ];
        let op = instructions.len();
        instructions.extend([
            Instruction::Move { dest: 0, src: 99 },
            Instruction::ExitScope,
            Instruction::CondBranchEnd {
                branch_name: X,
                result: 0,
            },
            Instruction::EndFlow { dest: 2 },
            Instruction::Return { value: 2 },
        ]);
        let plain_register = |_: u32| true;
        let plain_local = |_: &str| None;
        let scope = ErrUseScope {
            program: &program,
            instructions: &instructions,
            returned: NumericErrUse::Returned,
            plain_register: &plain_register,
            plain_local: &plain_local,
        };
        assert_eq!(numeric_err_use(&scope, op, 0), NumericErrUse::Returned);
        // In an inlined lazy argument, returning it is the call's use.
        let scope = ErrUseScope {
            returned: NumericErrUse::Consumed,
            ..scope
        };
        assert_eq!(numeric_err_use(&scope, op, 0), NumericErrUse::Consumed);
    }

    #[test]
    fn a_constraint_check_passes_the_err_through() {
        let program = program(false);
        let rest = vec![
            Instruction::CheckConstraint {
                dest: 1,
                src: 0,
                spec: X,
                test: false,
            },
            Instruction::Return { value: 1 },
        ];
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Returned);
    }

    #[test]
    fn a_returned_err_before_the_use_keeps_the_result() {
        let program = program(false);
        let rest = vec![
            Instruction::ReturnIfErr { src: 7 },
            Instruction::Return { value: 0 },
        ];
        assert_eq!(use_of(&program, rest.clone(), &[7]), NumericErrUse::Kept);
        assert_eq!(use_of(&program, rest, &[]), NumericErrUse::Returned);
    }
}
