//! Which values an instruction may move instead of copy.
//!
//! A Vec or Map is shared storage, and a write copies it while anything else
//! still holds it. A value on its way to a writer is held by the variable it
//! was read from, the register it was loaded into, the registers it was moved
//! through and the argument list of the call, so the writer always found it
//! shared and building a collection one element at a time was quadratic.
//!
//! A [`plan`] marks, for each instruction of one function or lambda body,
//! the values that instruction is the last to read: registers whose value
//! nothing reads again, and variables whose binding nothing reads again. The
//! interpreter hands such a value on instead of copying it. An unmarked
//! operand is copied as before, so a plan only ever removes copies; an empty
//! plan (or one whose length doesn't match its instructions) removes none.
//!
//! What the plan relies on:
//!
//! - The compiler emits no jumps. Control leaves the straight line only at
//!   the flow instructions (`BeginFlow`, `EndFlow`, `CondBranchStart`,
//!   `CondBranchEnd`), which skip forward to another flow instruction, and
//!   by leaving the body. So the instructions between two flow instructions
//!   run in order, all or none (an error or a return ends the body), and
//!   nothing runs twice. A body that holds a jump gets an empty plan.
//! - A register is read only by instructions of the body that wrote it, as
//!   [`operands`] lists them. It is dead after a read when no later
//!   instruction reads it, or when the next instruction to touch it writes
//!   it and is certain to run before any later read: one in the same run,
//!   or one directly in a block that encloses the read (the body itself, a
//!   branch, a scope), where the instructions run in order once a flow
//!   nested in them has ended. A write between the branches of a flow is
//!   not certain: the flow skips to its end once a branch has run.
//! - A variable is read by name, and not only by the body's own
//!   instructions: the lambdas, thunks and local functions created in the
//!   body read its frame for as long as it is live, and a call through a
//!   bare name calls a local of that name before any function. So every
//!   string the body's instructions name counts as a read of the variable
//!   of that name, and a name anything nested in the body can read is never
//!   taken. Nor is a name the frame doesn't bind itself on every run of the
//!   body (a capture, or a local bound in one branch): another run of the
//!   same body can sit above this one and read such a name from this frame.

use crate::lang::bytecode::{
    Constant, ConstantId, FlowType, FunctionId, Instruction, LambdaInfo, RegisterId,
};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};

/// The bits of one instruction's entry in a plan.
pub mod flag {
    /// The instruction's first single-register operand, in the order
    /// [`super::operands`] lists them, is dead after it.
    pub const FIRST: u8 = 1;
    /// Its second single-register operand is dead after it.
    pub const SECOND: u8 = 1 << 1;
    /// Its third single-register operand is dead after it.
    pub const THIRD: u8 = 1 << 2;
    /// Every register of its operand range (a call's arguments, a Vec
    /// literal's elements) is dead after it.
    pub const RANGE: u8 = 1 << 3;
    /// On a `LoadVar`: this is the last read of a binding of the frame the
    /// body runs in, so the load may take the value out of the binding.
    pub const VAR: u8 = 1 << 4;
    /// On a `LoadVar`: the value it loads goes on, through moves alone, to
    /// an instruction that writes it as its collection (a writer's first
    /// argument, a leading spread, a deep-path write), and by then the
    /// variable has given the value up (this load, or a later one before
    /// the write, is its `VAR` load). A caller that gives such a parameter
    /// a large collection nothing else holds is better served by the
    /// interpreter, which writes it in place, than by compiled code, which
    /// still copies it (see [`super::writer_parameters`]).
    pub const WRITER: u8 = 1 << 5;
}

/// The registers one instruction touches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Operands {
    /// Registers read one at a time; position `n` goes with flag bit `n`.
    pub reads: [Option<RegisterId>; 3],
    /// A run of consecutive registers read: first register and count.
    pub range: Option<(RegisterId, u32)>,
    /// The register written.
    pub write: Option<RegisterId>,
    /// A register read and then written through (the target of an in-place
    /// write). It is never moved from: its value stays where it is.
    pub update: Option<RegisterId>,
    /// Bit `n` set: the read at position `n` is never marked dead. It holds
    /// what there is nothing to gain by moving (a callee's name, a
    /// condition, a number), and giving the register up after every such
    /// read would cost every call something.
    pub kept: u8,
}

impl Operands {
    fn reads(a: RegisterId) -> Self {
        Self {
            reads: [Some(a), None, None],
            ..Self::default()
        }
    }
    fn reads2(a: RegisterId, b: RegisterId) -> Self {
        Self {
            reads: [Some(a), Some(b), None],
            ..Self::default()
        }
    }
    fn range(start: RegisterId, count: u32) -> Self {
        Self {
            range: Some((start, count)),
            ..Self::default()
        }
    }
    fn writes(mut self, dest: RegisterId) -> Self {
        self.write = Some(dest);
        self
    }
    fn updates(mut self, target: RegisterId) -> Self {
        self.update = Some(target);
        self
    }
    /// Also read `register`, without ever moving it (see `kept`).
    fn also_keeps(mut self, register: RegisterId) -> Self {
        if let Some(position) = self.reads.iter().position(Option::is_none) {
            self.reads[position] = Some(register);
            self.kept |= 1 << position;
        }
        self
    }
    /// Never move the reads at `positions` (see `kept`).
    fn keeps(mut self, positions: u8) -> Self {
        self.kept |= positions;
        self
    }

    /// Whether the instruction reads or writes `register` in any way.
    fn mentions(&self, register: RegisterId) -> bool {
        self.write == Some(register) || self.all_reads().any(|read| read == register)
    }

    /// Every register the instruction reads, the updated one included.
    fn all_reads(&self) -> impl Iterator<Item = RegisterId> + '_ {
        let range = self
            .range
            .into_iter()
            .flat_map(|(start, count)| (0..count).map(move |offset| start + offset));
        self.reads
            .iter()
            .flatten()
            .copied()
            .chain(range)
            .chain(self.update)
    }
}

/// The registers `instruction` reads and writes, or `None` for a jump, whose
/// control flow a plan does not model.
///
/// The match has no wildcard arm: a new instruction must say here what it
/// reads, or a register it reads could be taken for dead.
pub fn operands(instruction: &Instruction) -> Option<Operands> {
    use Instruction as I;
    let none = Operands::default();
    Some(match instruction {
        I::LoadConst { dest, .. }
        | I::LoadFunctionRef { dest, .. }
        | I::LoadTypeRef { dest, .. }
        | I::DefineFunction { dest, .. }
        | I::LoadVar { dest, .. }
        | I::LoadVarOrDefault { dest, .. }
        | I::LoadGlobal { dest, .. }
        | I::LoadScoped { dest, .. }
        | I::CaptureVar { dest, .. } => none.writes(*dest),

        I::Move { dest, src }
        | I::Pipe { dest, src }
        | I::ExtractInnerVal { dest, src }
        | I::ConstructTyped { dest, src, .. }
        | I::WrapOk { dest, src }
        | I::CheckConstraint { dest, src, .. } => Operands::reads(*src).writes(*dest),

        I::GetTypePath { dest, value } | I::EnsureResult { dest, value } => {
            Operands::reads(*value).writes(*dest)
        }
        I::IsType {
            dest,
            value,
            type_path,
        } => Operands::reads2(*value, *type_path)
            .keeps(flag::SECOND)
            .writes(*dest),

        I::DotAccess { dest, object, .. } | I::DotAccessOrDefault { dest, object, .. } => {
            Operands::reads(*object).writes(*dest)
        }
        I::DynamicDotAccess {
            dest,
            object,
            property,
        } => Operands::reads2(*object, *property).writes(*dest),

        I::Add { dest, left, right }
        | I::Sub { dest, left, right }
        | I::Mul { dest, left, right }
        | I::Eq { dest, left, right }
        | I::Gt { dest, left, right }
        | I::Lt { dest, left, right } => Operands::reads2(*left, *right)
            .keeps(flag::FIRST | flag::SECOND)
            .writes(*dest),
        I::StrEndsWith {
            dest,
            string,
            suffix,
        } => Operands::reads2(*string, *suffix)
            .keeps(flag::FIRST | flag::SECOND)
            .writes(*dest),
        I::StrStartsWith {
            dest,
            string,
            prefix,
        } => Operands::reads2(*string, *prefix)
            .keeps(flag::FIRST | flag::SECOND)
            .writes(*dest),

        // `function` is the register that holds the callee.
        I::Call {
            dest,
            function,
            args_start,
            args_count,
        }
        | I::CallWithSpread {
            dest,
            function,
            args_start,
            args_count,
            ..
        } => Operands::range(*args_start, u32::from(*args_count))
            .also_keeps(*function)
            .writes(*dest),
        I::CallLambda {
            dest,
            lambda,
            args_start,
            args_count,
        } => Operands::range(*args_start, u32::from(*args_count))
            .also_keeps(*lambda)
            .writes(*dest),
        I::CallNative {
            dest,
            args_start,
            args_count,
            ..
        }
        | I::CallUserFunction {
            dest,
            args_start,
            args_count,
            ..
        } => Operands::range(*args_start, u32::from(*args_count)).writes(*dest),
        I::TailCall {
            args_start,
            args_count,
            ..
        } => Operands::range(*args_start, u32::from(*args_count)),
        I::TemplateInterpolate {
            dest,
            parts_start,
            parts_count,
        } => Operands::range(*parts_start, u32::from(*parts_count)).writes(*dest),
        I::CallLibBuiltin {
            dest,
            function,
            args,
        } => Operands::reads2(*function, *args)
            .keeps(flag::FIRST)
            .writes(*dest),

        I::MakeVec {
            dest,
            elements_start,
            count,
        }
        | I::MakeVecWithSpread {
            dest,
            elements_start,
            count,
            ..
        } => Operands::range(*elements_start, u32::from(*count)).writes(*dest),

        // In-place writes: the target keeps its value.
        I::VecPush { vec, value } | I::VecAppend { vec, value } => {
            Operands::reads(*value).updates(*vec)
        }
        I::SetElement {
            collection,
            index,
            value,
        } => Operands::reads2(*index, *value).updates(*collection),
        I::DotSet { object, value, .. } => Operands::reads(*value).updates(*object),
        I::DynamicDotSet {
            object,
            property,
            value,
        } => Operands::reads2(*property, *value).updates(*object),
        I::MergeMaps { dest, source } => Operands::reads(*source).updates(*dest),
        // A cond or match whose default arm ran keeps the value already in
        // `dest`, so ending the flow reads it.
        I::EndFlow { dest } => none.updates(*dest),

        I::StoreVar { value, .. } | I::StoreGlobal { value, .. } | I::StoreScoped { value, .. } => {
            Operands::reads(*value)
        }
        I::DeferredVarExpr { thunk, .. } => Operands::reads(*thunk),
        I::CondBranchStart { condition, .. } => Operands::reads(*condition).keeps(flag::FIRST),
        I::CondBranchEnd { result, .. } => Operands::reads(*result),
        I::Return { value } => Operands::reads(*value),
        I::ReturnIfErr { src } => Operands::reads(*src),

        I::BeginFlow { .. }
        | I::SetNamespace { .. }
        | I::EnterScope { .. }
        | I::ExitScope
        | I::BeginErrorCapture
        | I::EndErrorCapture
        | I::RegisterLocalImplementation { .. }
        | I::RegisterLocalType { .. }
        | I::ProvenArgs { .. }
        | I::CheckParam { .. } => none,

        I::Jump { .. } | I::JumpIf { .. } | I::JumpIfNot { .. } | I::CondBranch { .. } => {
            return None;
        }
    })
}

/// Whether control can leave the straight line at `instruction`: a skipped
/// branch resumes after its `CondBranchEnd`, and a flow that has run a
/// branch resumes at its `EndFlow`.
fn is_flow_boundary(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::BeginFlow { .. }
            | Instruction::EndFlow { .. }
            | Instruction::CondBranchStart { .. }
            | Instruction::CondBranchEnd { .. }
    )
}

/// The string constant `id` names, if it is one.
fn constant_name(constants: &[Constant], id: ConstantId) -> Option<&str> {
    match constants.get(id as usize)? {
        Constant::StringRef(name) | Constant::FunctionRef(name) | Constant::TypeRef(name) => {
            Some(name)
        }
        Constant::VariantTypeRef(type_ref) => Some(type_ref.name.as_str()),
        Constant::Val(Val::Str(name)) => Some(name),
        Constant::Val(_) => None,
    }
}

/// What the lambdas, thunks and function values among a body's constants
/// can name, at any time while the body's frame is live.
#[derive(Default)]
struct Named<'a> {
    /// Every name a nested body binds, captures or refers to.
    names: AHashSet<&'a str>,
    /// A constant holds a value whose names can't be listed: syntax kept
    /// for evaluation at run time, which looks its variables up by name.
    opaque: bool,
}

impl<'a> Named<'a> {
    /// Add the names the code inside `value` can read. A string held as
    /// data names nothing: only an instruction's own operand is looked up.
    fn add_value(&mut self, value: &'a Val, constants: &'a [Constant], depth: usize) {
        // Constants nest a few levels at most; anything deeper is treated
        // as unlistable rather than walked.
        if depth > 64 {
            self.opaque = true;
            return;
        }
        match value {
            Val::Vec(items) => {
                for item in items.iter() {
                    self.add_value(item, constants, depth + 1);
                }
            }
            Val::Map(entries) => {
                for (key, item) in entries.iter() {
                    self.add_value(key, constants, depth + 1);
                    self.add_value(item, constants, depth + 1);
                }
            }
            Val::Box(boxed) => {
                let any = boxed.as_any();
                if let Some(lambda) = any.downcast_ref::<LambdaInfo>() {
                    self.add_body(lambda, constants, depth + 1);
                } else if let Some(function) =
                    any.downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                {
                    self.names.insert(function.name.as_str());
                } else if any.downcast_ref::<crate::lang::refs::TypeRef>().is_none() {
                    self.opaque = true;
                }
            }
            Val::Str(_)
            | Val::Int(_)
            | Val::Dec(_)
            | Val::Bool(_)
            | Val::Byte(_)
            | Val::Bytes(_)
            | Val::Null => {}
        }
    }

    /// Add every name a nested body can read: its parameters, captures and
    /// each string its instructions refer to, to any depth.
    fn add_body(&mut self, lambda: &'a LambdaInfo, constants: &'a [Constant], depth: usize) {
        self.names
            .extend(lambda.parameters.iter().map(String::as_str));
        self.names
            .extend(lambda.capture_vars.iter().map(String::as_str));
        for (name, value) in &lambda.closure_env {
            self.names.insert(name);
            self.add_value(value, constants, depth + 1);
        }
        for instruction in &lambda.instructions {
            for id in constant_operands(instruction).into_iter().flatten() {
                self.add_constant(id, constants, depth + 1);
            }
        }
    }

    fn add_constant(&mut self, id: ConstantId, constants: &'a [Constant], depth: usize) {
        match constants.get(id as usize) {
            // An instruction's own string operand is a name it can look up
            // (the variable a `LoadVar` reads is one), unlike a string held
            // inside a collection.
            Some(Constant::Val(value)) if !matches!(value, Val::Str(_)) => {
                self.add_value(value, constants, depth)
            }
            Some(_) => {
                if let Some(name) = constant_name(constants, id) {
                    self.names.insert(name);
                }
            }
            // A constant the pool doesn't hold can't be listed.
            None => self.opaque = true,
        }
    }
}

/// The constant-pool operands of `instruction`.
///
/// Exhaustive for the same reason as [`operands`].
fn constant_operands(instruction: &Instruction) -> [Option<ConstantId>; 3] {
    use Instruction as I;
    match instruction {
        I::LoadConst { constant, .. } => [Some(*constant), None, None],
        I::LoadFunctionRef { function_name, .. } => [Some(*function_name), None, None],
        I::LoadTypeRef { type_ref, .. } => [Some(*type_ref), None, None],
        I::BeginFlow { origin_name, .. } => [*origin_name, None, None],
        I::CondBranchStart { branch_name, .. } | I::CondBranchEnd { branch_name, .. } => {
            [Some(*branch_name), None, None]
        }
        I::DotAccess { property, .. } | I::DotSet { property, .. } => [Some(*property), None, None],
        I::DotAccessOrDefault {
            property,
            default_value,
            ..
        } => [Some(*property), Some(*default_value), None],
        I::ConstructTyped { type_info, .. } => [Some(*type_info), None, None],
        I::LoadVar { var_name, .. }
        | I::StoreVar { var_name, .. }
        | I::LoadScoped { var_name, .. }
        | I::StoreScoped { var_name, .. }
        | I::DeferredVarExpr { var_name, .. }
        | I::CaptureVar { var_name, .. } => [Some(*var_name), None, None],
        I::LoadVarOrDefault {
            var_name,
            default_value,
            ..
        } => [Some(*var_name), Some(*default_value), None],
        I::SetNamespace { namespace } => [Some(*namespace), None, None],
        I::LoadGlobal {
            namespace,
            var_name,
            ..
        }
        | I::StoreGlobal {
            namespace,
            var_name,
            ..
        } => [Some(*namespace), Some(*var_name), None],
        I::RegisterLocalImplementation {
            source_type,
            target_type,
            implementation_function_name,
        } => [
            Some(*source_type),
            Some(*target_type),
            Some(*implementation_function_name),
        ],
        I::RegisterLocalType {
            type_name,
            constructor_function_name,
        } => [Some(*type_name), Some(*constructor_function_name), None],
        I::CheckConstraint { spec, .. } => [Some(*spec), None, None],
        I::CheckParam { var_name, spec, .. } => [Some(*var_name), Some(*spec), None],

        I::Move { .. }
        | I::EndFlow { .. }
        | I::CondBranch { .. }
        | I::Pipe { .. }
        | I::DefineFunction { .. }
        | I::Add { .. }
        | I::Sub { .. }
        | I::Mul { .. }
        | I::Eq { .. }
        | I::StrEndsWith { .. }
        | I::StrStartsWith { .. }
        | I::Gt { .. }
        | I::Lt { .. }
        | I::Call { .. }
        | I::CallNative { .. }
        | I::CallLambda { .. }
        | I::CallWithSpread { .. }
        | I::CallUserFunction { .. }
        | I::TailCall { .. }
        | I::TemplateInterpolate { .. }
        | I::DynamicDotAccess { .. }
        | I::DynamicDotSet { .. }
        | I::VecAppend { .. }
        | I::GetTypePath { .. }
        | I::CallLibBuiltin { .. }
        | I::Jump { .. }
        | I::JumpIf { .. }
        | I::JumpIfNot { .. }
        | I::Return { .. }
        | I::ExtractInnerVal { .. }
        | I::IsType { .. }
        | I::EnsureResult { .. }
        | I::MakeVec { .. }
        | I::VecPush { .. }
        | I::MakeVecWithSpread { .. }
        | I::SetElement { .. }
        | I::MergeMaps { .. }
        | I::EnterScope { .. }
        | I::ExitScope
        | I::ReturnIfErr { .. }
        | I::BeginErrorCapture
        | I::EndErrorCapture
        | I::WrapOk { .. }
        | I::ProvenArgs { .. } => [None, None, None],
    }
}

/// The plan for one body: one entry of [`flag`] bits per instruction.
///
/// `parameters` are the names the body's frame binds on entry, `constants`
/// the pool its instructions index, and `is_local_function` says whether a
/// function id is that of a lambda or of a function defined in another
/// function's body, whose frame reads the names it doesn't bind from its
/// caller's.
pub fn plan(
    instructions: &[Instruction],
    parameters: &[String],
    constants: &[Constant],
    is_local_function: &dyn Fn(FunctionId) -> bool,
    writes_parameter: &dyn Fn(FunctionId, usize) -> bool,
) -> Vec<u8> {
    let mut flags = vec![0u8; instructions.len()];
    let Some(operands) = instructions
        .iter()
        .map(operands)
        .collect::<Option<Vec<Operands>>>()
    else {
        return flags;
    };
    let regions = Regions::of(instructions);
    plan_registers(instructions, &operands, regions.as_ref(), &mut flags);
    report_calls_keeping_arguments(instructions, &flags);
    if let Some(regions) = &regions {
        plan_variables(
            instructions,
            parameters,
            constants,
            is_local_function,
            regions,
            &mut flags,
        );
        plan_writers(
            instructions,
            &operands,
            constants,
            writes_parameter,
            &mut flags,
        );
    }
    flags
}

/// With `HOT_MOVES_REPORT` set, print a line for each call a plan leaves
/// copying its arguments (see [`calls_keeping_arguments`]), and a count of
/// the calls planned. Run over the test suites, this is the scan of all the
/// bytecode they compile.
fn report_calls_keeping_arguments(instructions: &[Instruction], plan: &[u8]) {
    static REPORT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*REPORT.get_or_init(|| std::env::var_os("HOT_MOVES_REPORT").is_some()) {
        return;
    }
    let calls = instructions
        .iter()
        .filter(|instruction| {
            matches!(
                instruction,
                Instruction::Call { .. }
                    | Instruction::CallNative { .. }
                    | Instruction::CallLambda { .. }
                    | Instruction::CallWithSpread { .. }
                    | Instruction::CallUserFunction { .. }
                    | Instruction::TailCall { .. }
            )
        })
        .count();
    eprintln!("moves: planned {calls} calls");
    for index in calls_keeping_arguments(instructions, plan) {
        eprintln!("moves: call keeps its arguments: {:?}", instructions[index]);
    }
}

/// The calls of `instructions` whose arguments `plan` does not move: the
/// plan could not show that every register of the argument range is dead
/// after the call.
///
/// The compiler writes an argument register for one call. It reuses the
/// register afterwards (an inner call's argument slot becomes a slot of the
/// outer call), so a later instruction can read the register, but never the
/// value the call was given. A call listed here is one where the plan can't
/// see that, and it copies its arguments as before. In the bytecode of both
/// test suites there is none, and the compiler asserts as much of every body
/// it finishes in a debug build.
pub fn calls_keeping_arguments(instructions: &[Instruction], plan: &[u8]) -> Vec<usize> {
    use Instruction as I;
    instructions
        .iter()
        .enumerate()
        .filter(|(index, instruction)| {
            let (I::Call { args_count, .. }
            | I::CallNative { args_count, .. }
            | I::CallLambda { args_count, .. }
            | I::CallWithSpread { args_count, .. }
            | I::CallUserFunction { args_count, .. }
            | I::TailCall { args_count, .. }) = instruction
            else {
                return false;
            };
            *args_count > 0
                && plan
                    .get(*index)
                    .is_none_or(|entry| entry & flag::RANGE == 0)
        })
        .map(|(index, _)| index)
        .collect()
}

/// How the instructions of a body nest: the blocks control runs through in
/// order (the body, a branch, a scope) and the flows that hold branches.
struct Regions {
    /// The innermost region each instruction runs in.
    of: Vec<usize>,
    /// Each region's enclosing region; region 0 is the body.
    parent: Vec<usize>,
    /// Whether the instructions directly in a region run in order, each
    /// reached whenever the one before it finished. Not so between the
    /// branches of a flow, which skips to its end once a branch has run.
    in_order: Vec<bool>,
    /// What opened each region.
    kind: Vec<Region>,
}

/// What a region of a body is.
#[derive(Clone, Copy, PartialEq)]
enum Region {
    /// The body or a scope.
    Block,
    /// A flow that runs one of its branches and then skips to its end
    /// (`cond`, `match`, an inlined `if`).
    OneBranchFlow,
    /// Any other flow.
    Flow,
    /// A branch of a flow.
    Branch,
}

impl Regions {
    /// The regions of `instructions`, or `None` when its scopes, flows and
    /// branches don't nest properly.
    fn of(instructions: &[Instruction]) -> Option<Self> {
        use Instruction as I;
        let mut regions = Regions {
            of: Vec::with_capacity(instructions.len()),
            parent: vec![0],
            in_order: vec![true],
            kind: vec![Region::Block],
        };
        // The open regions, each with the instruction that opened it.
        let mut open: Vec<(usize, &Instruction)> = Vec::new();
        let mut current = 0;
        for instruction in instructions {
            match instruction {
                // An opener runs in the region it is found in.
                I::EnterScope { .. } | I::BeginFlow { .. } | I::CondBranchStart { .. } => {
                    regions.of.push(current);
                    regions.parent.push(current);
                    regions
                        .in_order
                        .push(!matches!(instruction, I::BeginFlow { .. }));
                    regions.kind.push(match instruction {
                        I::BeginFlow {
                            flow_type: FlowType::Cond | FlowType::Match,
                            ..
                        } => Region::OneBranchFlow,
                        I::BeginFlow { .. } => Region::Flow,
                        I::CondBranchStart { .. } => Region::Branch,
                        _ => Region::Block,
                    });
                    open.push((current, instruction));
                    current = regions.parent.len() - 1;
                }
                I::ExitScope | I::EndFlow { .. } | I::CondBranchEnd { .. } => {
                    let (outer, opener) = open.pop()?;
                    let pairs = matches!(
                        (opener, instruction),
                        (I::EnterScope { .. }, I::ExitScope)
                            | (I::BeginFlow { .. }, I::EndFlow { .. })
                            | (I::CondBranchStart { .. }, I::CondBranchEnd { .. })
                    );
                    if !pairs {
                        return None;
                    }
                    // A branch's end runs only when its branch did. The end
                    // of a scope or a flow runs in the region around it.
                    regions
                        .of
                        .push(if matches!(instruction, I::CondBranchEnd { .. }) {
                            current
                        } else {
                            outer
                        });
                    current = outer;
                }
                _ => regions.of.push(current),
            }
        }
        open.is_empty().then_some(regions)
    }

    /// Whether region `outer` is `inner` or encloses it.
    fn encloses(&self, outer: usize, mut inner: usize) -> bool {
        loop {
            if inner == outer {
                return true;
            }
            if inner == 0 {
                return false;
            }
            inner = self.parent[inner];
        }
    }

    /// Whether the instruction at `later` can still run once the one at
    /// `earlier`, which comes before it in the body, has. It can't when
    /// `earlier` is in a branch of a flow that runs one branch and `later`
    /// is elsewhere in that flow: in another branch, or in the test of
    /// one. Having run a branch, such a flow skips to its end.
    fn may_follow(&self, earlier: usize, later: usize) -> bool {
        let target = self.of[later];
        let mut inner = self.of[earlier];
        if self.encloses(inner, target) {
            return true;
        }
        // Climb to the innermost region that holds both. `inner` is then
        // the region under it that holds `earlier` alone.
        while inner != 0 {
            let outer = self.parent[inner];
            if self.encloses(outer, target) {
                return !(self.kind[outer] == Region::OneBranchFlow
                    && self.kind[inner] == Region::Branch);
            }
            inner = outer;
        }
        true
    }
}

/// Mark the register operands that are dead after their instruction.
fn plan_registers(
    instructions: &[Instruction],
    operands: &[Operands],
    regions: Option<&Regions>,
    flags: &mut [u8],
) {
    /// One instruction's use of a register. An instruction that reads and
    /// writes the same register reads it first.
    #[derive(Clone, Copy, PartialEq)]
    enum Touch {
        Reads,
        Writes,
    }
    // Every touch of each register, in instruction order.
    let mut touches: AHashMap<RegisterId, Vec<(usize, Touch)>> = AHashMap::new();
    // The run each instruction belongs to: the instructions between two
    // flow instructions, which is a run of its own.
    let mut run_of = Vec::with_capacity(instructions.len());
    let mut run = 0usize;
    for (index, ops) in operands.iter().enumerate() {
        let boundary = is_flow_boundary(&instructions[index]);
        if boundary {
            run += 1;
        }
        run_of.push(run);
        if boundary {
            run += 1;
        }
        let mut read = false;
        for register in ops.all_reads() {
            let seen = touches.entry(register).or_default();
            if seen.last() != Some(&(index, Touch::Reads)) {
                seen.push((index, Touch::Reads));
            }
            read |= ops.write == Some(register);
        }
        if let Some(register) = ops.write
            && !read
        {
            touches
                .entry(register)
                .or_default()
                .push((index, Touch::Writes));
        }
    }

    for (index, ops) in operands.iter().enumerate() {
        let dead = |register: RegisterId| {
            // A register the instruction writes holds its result afterwards,
            // and one it reads twice must outlive the first read.
            if ops.write == Some(register)
                || ops.update == Some(register)
                || ops.all_reads().filter(|read| *read == register).count() != 1
            {
                return false;
            }
            let Some(seen) = touches.get(&register) else {
                return false;
            };
            let later = seen.partition_point(|(at, _)| *at <= index);
            match seen.get(later) {
                // Nothing touches it again.
                None => true,
                Some((_, Touch::Reads)) => false,
                // The next touch replaces the value. That ends this one
                // only if the write is certain to run before a later read.
                Some((write, Touch::Writes)) => {
                    run_of[*write] == run_of[index]
                        || regions.is_some_and(|regions| {
                            let block = regions.of[*write];
                            regions.in_order[block] && regions.encloses(block, regions.of[index])
                        })
                }
            }
        };
        let mut entry = 0u8;
        for (position, register) in ops.reads.iter().enumerate() {
            if ops.kept & (1 << position) == 0 && register.is_some_and(dead) {
                entry |= 1 << position;
            }
        }
        if let Some((start, count)) = ops.range
            && count > 0
            && (0..count).all(|offset| dead(start + offset))
        {
            entry |= flag::RANGE;
        }
        flags[index] = entry;
    }
}

/// Mark the `LoadVar`s that are the last read of a binding of the body's
/// own frame.
fn plan_variables<'a>(
    instructions: &'a [Instruction],
    parameters: &[String],
    constants: &'a [Constant],
    is_local_function: &dyn Fn(FunctionId) -> bool,
    regions: &Regions,
    flags: &mut [u8],
) {
    use Instruction as I;

    /// Where the body first binds a name, and whether every read of the
    /// name since has been inside the region of that binding.
    struct Binding {
        region: usize,
        covers_reads: bool,
    }
    let mut bindings: AHashMap<&str, Binding> = AHashMap::new();
    // Names read before the body binds them: they are its captures, or its
    // caller's or namespace's variables.
    let mut read_unbound: AHashSet<&str> = AHashSet::new();
    // The instructions that name each string outside a binding, in order.
    let mut named_at: AHashMap<&str, Vec<usize>> = AHashMap::new();
    // Every string a nested body or a collection constant holds.
    let mut nested = Named::default();
    for (index, instruction) in instructions.iter().enumerate() {
        match instruction {
            // A parallel flow binds and reads its names when it ends, in
            // other VMs; lookups by scope depth and function definitions
            // read names this pass doesn't follow.
            I::DeferredVarExpr { .. }
            | I::LoadScoped { .. }
            | I::StoreScoped { .. }
            | I::CaptureVar { .. }
            | I::DefineFunction { .. } => return,
            // A function defined in another's body reads the names it
            // doesn't bind from the frame of whoever called it.
            I::CallUserFunction { function_id, .. } | I::TailCall { function_id, .. }
                if is_local_function(*function_id) =>
            {
                return;
            }
            _ => {}
        }

        let binds = match instruction {
            I::StoreVar { var_name, .. } => Some(*var_name),
            _ => None,
        };
        if let Some(id) = binds {
            let Some(name) = constant_name(constants, id) else {
                return;
            };
            if !read_unbound.contains(name) {
                // A name bound in a scope or a branch is gone, or was never
                // bound, outside it.
                bindings.entry(name).or_insert_with(|| Binding {
                    region: regions.of[index],
                    covers_reads: true,
                });
            }
            continue;
        }

        for id in constant_operands(instruction).into_iter().flatten() {
            match constants.get(id as usize) {
                // A collection or a nested body: whatever it names is read
                // at a time this pass can't place.
                Some(Constant::Val(value)) if !matches!(value, Val::Str(_)) => {
                    nested.add_value(value, constants, 0);
                }
                Some(_) => {
                    let Some(name) = constant_name(constants, id) else {
                        continue;
                    };
                    named_at.entry(name).or_default().push(index);
                    match bindings.get_mut(name) {
                        Some(binding) => {
                            binding.covers_reads &=
                                regions.encloses(binding.region, regions.of[index]);
                        }
                        None => {
                            read_unbound.insert(name);
                        }
                    }
                }
                None => return,
            }
        }
        if nested.opaque {
            return;
        }
    }
    for (index, instruction) in instructions.iter().enumerate() {
        let I::LoadVar { var_name, .. } = instruction else {
            continue;
        };
        let Some(name) = constant_name(constants, *var_name) else {
            continue;
        };
        let own = parameters.iter().any(|parameter| parameter == name)
            || bindings
                .get(name)
                .is_some_and(|binding| binding.covers_reads);
        if own
            && !nested.names.contains(name)
            && named_at
                .get(name)
                .is_some_and(|named| nothing_follows(regions, named, index))
        {
            flags[index] |= flag::VAR;
        }
    }
}

/// How many later mentions of a name [`nothing_follows`] will rule out one
/// by one. A read with more after it than this is not a last read.
const LATER_MENTIONS: usize = 32;

/// Whether none of the instructions `named` (those that name a string, in
/// order) can run after the one at `index`: the ones after it in the body
/// are all in branches that don't run once this one has.
fn nothing_follows(regions: &Regions, named: &[usize], index: usize) -> bool {
    let later = &named[named.partition_point(|at| *at <= index)..];
    later.len() <= LATER_MENTIONS && later.iter().all(|at| !regions.may_follow(index, *at))
}

/// The core functions that write their first argument in place when
/// nothing else holds it (hot-std's own, or the native under each).
fn writes_first_argument(callee: &str) -> bool {
    let name = callee.strip_prefix("::hot::coll/").unwrap_or(callee);
    matches!(
        name,
        "assoc" | "assoc-some" | "merge" | "delete" | "concat" | "update"
    )
}

/// The name in `register` when instruction `before` runs, if a constant put
/// it there: the callee of a call.
fn loaded_name<'a>(
    instructions: &[Instruction],
    constants: &'a [Constant],
    before: usize,
    register: RegisterId,
) -> Option<&'a str> {
    let mut register = register;
    for instruction in instructions[..before].iter().rev() {
        match instruction {
            Instruction::LoadConst { dest, constant } if *dest == register => {
                return constant_name(constants, *constant);
            }
            Instruction::LoadFunctionRef {
                dest,
                function_name,
            } if *dest == register => return constant_name(constants, *function_name),
            Instruction::Move { dest, src } if *dest == register => register = *src,
            other if operands(other).is_none_or(|ops| ops.write == Some(register)) => {
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Mark the loads whose value goes on to be written in place (see
/// [`flag::WRITER`]).
fn plan_writers(
    instructions: &[Instruction],
    operands: &[Operands],
    constants: &[Constant],
    writes_parameter: &dyn Fn(FunctionId, usize) -> bool,
    flags: &mut [u8],
) {
    use Instruction as I;
    // Where each variable gives its value up: one load on each path
    // through the body, in order.
    let mut taken_at: AHashMap<ConstantId, Vec<usize>> = AHashMap::new();
    for (index, instruction) in instructions.iter().enumerate() {
        if let I::LoadVar { var_name, .. } = instruction
            && flags[index] & flag::VAR != 0
        {
            taken_at.entry(*var_name).or_default().push(index);
        }
    }
    for start in 0..instructions.len() {
        let I::LoadVar { dest, var_name } = &instructions[start] else {
            continue;
        };
        let Some(taken) = taken_at.get(var_name) else {
            continue;
        };
        // The register that holds the value, or the argument Vec it is the
        // first item of.
        let mut held = *dest;
        let mut in_arguments = false;
        let mut written = false;
        let mut writer = start;
        for index in start + 1..instructions.len() {
            writer = index;
            let instruction = &instructions[index];
            if is_flow_boundary(instruction) {
                break;
            }
            let entry = flags[index];
            match instruction {
                I::Move { dest, src } if *src == held && entry & flag::FIRST != 0 => {
                    held = *dest;
                    continue;
                }
                I::MakeVec {
                    dest,
                    elements_start,
                    ..
                } if !in_arguments && *elements_start == held && entry & flag::RANGE != 0 => {
                    held = *dest;
                    in_arguments = true;
                    continue;
                }
                I::MakeVecWithSpread {
                    elements_start,
                    spread_mask,
                    ..
                } if !in_arguments && *elements_start == held => {
                    written = spread_mask & 1 == 1 && entry & flag::RANGE != 0;
                }
                I::MergeMaps { source, .. } if !in_arguments && *source == held => {
                    written = entry & flag::FIRST != 0;
                }
                I::DotSet { object: target, .. }
                | I::DynamicDotSet { object: target, .. }
                | I::SetElement {
                    collection: target, ..
                }
                | I::VecPush { vec: target, .. }
                | I::VecAppend { vec: target, .. }
                    if !in_arguments && *target == held =>
                {
                    written = true;
                }
                I::Call {
                    function,
                    args_start,
                    ..
                } if !in_arguments && *args_start == held => {
                    written = entry & flag::RANGE != 0
                        && loaded_name(instructions, constants, index, *function)
                            .is_some_and(writes_first_argument);
                }
                I::CallUserFunction {
                    function_id,
                    args_start,
                    args_count,
                    ..
                } if !in_arguments
                    && held >= *args_start
                    && held - *args_start < u32::from(*args_count) =>
                {
                    written = entry & flag::RANGE != 0
                        && writes_parameter(*function_id, (held - *args_start) as usize);
                }
                I::CallLibBuiltin { function, args, .. } if in_arguments && *args == held => {
                    written = entry & flag::SECOND != 0
                        && loaded_name(instructions, constants, index, *function).is_some_and(
                            |native| crate::lang::hot::coll::owning_writer(native).is_some(),
                        );
                }
                _ if operands[index].mentions(held) => {}
                _ => continue,
            }
            break;
        }
        // The variable must have given the value up before the write: at
        // this load or at one between it and the writer. No flow starts or
        // ends between the two, so such a load is on the writer's path.
        if written && taken.iter().any(|at| (start..writer).contains(at)) {
            flags[start] |= flag::WRITER;
        }
    }
}

/// The parameters of a body, as a bit per position (the first 64), that a
/// call of it goes on to write in place: see [`flag::WRITER`].
pub fn writer_parameters(
    instructions: &[Instruction],
    plan: &[u8],
    parameters: &[String],
    constants: &[Constant],
) -> u64 {
    if plan.len() != instructions.len() {
        return 0;
    }
    let mut written = 0u64;
    for (instruction, entry) in instructions.iter().zip(plan) {
        if entry & flag::WRITER == 0 {
            continue;
        }
        if let Instruction::LoadVar { var_name, .. } = instruction
            && let Some(name) = constant_name(constants, *var_name)
            && let Some(position) = parameters.iter().position(|parameter| parameter == name)
            && position < 64
        {
            written |= 1 << position;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::ScopeType;

    /// The plan of a body whose names don't matter.
    fn registers_only(instructions: &[Instruction]) -> Vec<u8> {
        plan(instructions, &[], &[], &|_| false, &|_, _| false)
    }

    fn strings(names: &[&str]) -> Vec<Constant> {
        names
            .iter()
            .map(|name| Constant::StringRef((*name).into()))
            .collect()
    }

    fn plan_of(
        instructions: &[Instruction],
        parameters: &[&str],
        constants: &[Constant],
    ) -> Vec<u8> {
        let parameters: Vec<String> = parameters.iter().map(|name| name.to_string()).collect();
        plan(instructions, &parameters, constants, &|_| false, &|_, _| {
            false
        })
    }

    fn load(dest: RegisterId, var_name: ConstantId) -> Instruction {
        Instruction::LoadVar { dest, var_name }
    }
    fn mov(dest: RegisterId, src: RegisterId) -> Instruction {
        Instruction::Move { dest, src }
    }
    fn call(dest: RegisterId, function: RegisterId, args_start: RegisterId, n: u8) -> Instruction {
        Instruction::Call {
            dest,
            function,
            args_start,
            args_count: n,
        }
    }
    fn constant(dest: RegisterId, constant: ConstantId) -> Instruction {
        Instruction::LoadConst { dest, constant }
    }

    #[test]
    fn a_register_rewritten_before_its_next_read_is_dead() {
        // assoc(assoc(m, ..), ..): the inner call's argument slot is reused
        // for an argument of the outer call.
        let body = [
            load(5, 0),       // m
            mov(4, 5),        // inner arg 0
            constant(9, 1),   // "assoc"
            call(3, 9, 4, 1), // inner call
            mov(2, 3),        // outer arg 0
            constant(4, 1),   // slot 4 again, as a temporary
            mov(3, 4),        // outer arg 1
            constant(9, 1),
            call(1, 9, 2, 2),
            Instruction::Return { value: 1 },
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[1], flag::FIRST, "the temporary is moved to the slot");
        assert_eq!(plan[3] & flag::RANGE, flag::RANGE, "the inner call takes");
        assert_eq!(plan[4], flag::FIRST);
        assert_eq!(plan[8] & flag::RANGE, flag::RANGE);
        assert_eq!(plan[9], flag::FIRST);
    }

    #[test]
    fn a_register_read_later_is_not_dead() {
        let body = [
            constant(1, 0),
            mov(2, 1),
            constant(9, 0),
            call(3, 9, 2, 1),
            // Register 1 is read again, so the move above must copy.
            mov(4, 1),
            Instruction::Return { value: 4 },
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[1], 0);
        assert_eq!(plan[3] & flag::RANGE, flag::RANGE);
        assert_eq!(plan[4], flag::FIRST);
    }

    #[test]
    fn a_write_in_another_run_does_not_end_a_register() {
        // The write to register 1 sits in a branch that may be skipped, so
        // the read after the flow can still see the first value.
        let body = [
            constant(1, 0),
            mov(2, 1),
            Instruction::BeginFlow {
                flow_type: crate::lang::bytecode::FlowType::Cond,
                result_modifier: crate::lang::bytecode::FlowResultModifier::One,
                source: None,
                origin_name: None,
            },
            Instruction::CondBranchStart {
                branch_name: 0,
                condition: 2,
                skip_target: 0,
            },
            constant(1, 0),
            Instruction::CondBranchEnd {
                branch_name: 0,
                result: 1,
            },
            Instruction::EndFlow { dest: 3 },
            mov(4, 1),
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[1], 0, "register 1 is read after the flow");
        assert_eq!(plan[5], 0, "and the branch's own read is not its last");
        assert_eq!(plan[7], flag::FIRST);
    }

    fn cond_flow() -> Instruction {
        Instruction::BeginFlow {
            flow_type: crate::lang::bytecode::FlowType::Cond,
            result_modifier: crate::lang::bytecode::FlowResultModifier::One,
            source: None,
            origin_name: None,
        }
    }
    fn branch(condition: RegisterId) -> Instruction {
        Instruction::CondBranchStart {
            branch_name: 0,
            condition,
            skip_target: 0,
        }
    }
    fn branch_end(result: RegisterId) -> Instruction {
        Instruction::CondBranchEnd {
            branch_name: 0,
            result,
        }
    }

    #[test]
    fn a_read_in_each_branch_of_a_conditional_is_a_last_read() {
        // if(c, [...xs, 0], xs). Constant 0 names the branches.
        let constants = strings(&["branch", "xs"]);
        let body = [
            cond_flow(),
            constant(1, 0),
            branch(1),
            load(2, 1),
            branch_end(2),
            constant(3, 0),
            branch(3),
            load(4, 1),
            branch_end(4),
            Instruction::EndFlow { dest: 5 },
        ];
        let plan = plan_of(&body, &["xs"], &constants);
        assert_eq!(plan[3] & flag::VAR, flag::VAR, "the first branch's read");
        assert_eq!(plan[7] & flag::VAR, flag::VAR, "the second branch's read");

        // A read after the flow follows whichever branch ran.
        let mut read_after = body.to_vec();
        read_after.push(load(6, 1));
        let plan = plan_of(&read_after, &["xs"], &constants);
        assert_eq!(plan[3] & flag::VAR, 0);
        assert_eq!(plan[7] & flag::VAR, 0);
        assert_eq!(plan[10] & flag::VAR, flag::VAR);

        // A flow that runs every branch whose test holds runs both reads.
        let mut every_branch = body.to_vec();
        every_branch[0] = Instruction::BeginFlow {
            flow_type: FlowType::CondAll,
            result_modifier: crate::lang::bytecode::FlowResultModifier::One,
            source: None,
            origin_name: None,
        };
        let plan = plan_of(&every_branch, &["xs"], &constants);
        assert_eq!(plan[3] & flag::VAR, 0);
        assert_eq!(plan[7] & flag::VAR, flag::VAR);
    }

    #[test]
    fn a_read_in_a_test_is_followed_by_the_branches() {
        // cond { f(xs) => { xs } g(xs) => { 0 } }
        let constants = strings(&["branch", "xs"]);
        let body = [
            cond_flow(),
            load(1, 1),
            branch(1),
            load(2, 1),
            branch_end(2),
            load(3, 1),
            branch(3),
            constant(4, 0),
            branch_end(4),
            Instruction::EndFlow { dest: 5 },
        ];
        let plan = plan_of(&body, &["xs"], &constants);
        assert_eq!(plan[1] & flag::VAR, 0, "the first test's read");
        // Once the first branch has run, the flow skips the second test.
        assert_eq!(plan[3] & flag::VAR, flag::VAR, "the first branch's read");
        assert_eq!(plan[5] & flag::VAR, flag::VAR, "the second test's read");

        // A conditional inside the test of a branch: the branch follows
        // whichever of its branches ran.
        let body = [
            cond_flow(),
            cond_flow(),
            constant(1, 0),
            branch(1),
            load(2, 1),
            branch_end(2),
            Instruction::EndFlow { dest: 3 },
            branch(3),
            load(4, 1),
            branch_end(4),
            Instruction::EndFlow { dest: 5 },
        ];
        let plan = plan_of(&body, &["xs"], &constants);
        assert_eq!(plan[4] & flag::VAR, 0);
        assert_eq!(plan[8] & flag::VAR, flag::VAR);
    }

    #[test]
    fn a_read_in_a_branch_inside_a_branch_is_a_last_read() {
        // if(a, if(b, xs, xs), xs)
        let constants = strings(&["branch", "xs"]);
        let body = [
            cond_flow(),
            constant(1, 0),
            branch(1),
            cond_flow(),
            constant(2, 0),
            branch(2),
            load(3, 1),
            branch_end(3),
            constant(4, 0),
            branch(4),
            load(5, 1),
            branch_end(5),
            Instruction::EndFlow { dest: 6 },
            branch_end(6),
            constant(7, 0),
            branch(7),
            load(8, 1),
            branch_end(8),
            Instruction::EndFlow { dest: 9 },
        ];
        let plan = plan_of(&body, &["xs"], &constants);
        for read in [6, 10, 16] {
            assert_eq!(plan[read] & flag::VAR, flag::VAR, "the read at {read}");
        }
    }

    #[test]
    fn a_write_after_a_nested_flow_ends_a_register() {
        // f(g(x), if(c, a, b)): the slot g's argument was in is written
        // again for f, after the flow of the `if`. That write runs whatever
        // the flow did, so g's read of the slot was its value's last.
        let body = [
            constant(1, 0),
            call(2, 9, 1, 1),
            cond_flow(),
            branch(2),
            constant(5, 0),
            branch_end(5),
            Instruction::EndFlow { dest: 3 },
            constant(1, 0),
            call(4, 9, 1, 1),
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[1] & flag::RANGE, flag::RANGE);
        assert!(calls_keeping_arguments(&body, &plan).is_empty());
    }

    #[test]
    fn a_write_between_branches_does_not_end_a_register() {
        // The write to register 1 is the second branch's condition. Once
        // the first branch has run the flow skips it, and the read after
        // the flow sees what the first branch read.
        let body = [
            constant(1, 0),
            cond_flow(),
            constant(8, 0),
            branch(8),
            call(2, 9, 1, 1),
            branch_end(2),
            constant(1, 0),
            branch(1),
            constant(6, 0),
            branch_end(6),
            Instruction::EndFlow { dest: 3 },
            mov(4, 1),
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[4] & flag::RANGE, 0);
        assert_eq!(calls_keeping_arguments(&body, &plan), vec![4]);
        assert_eq!(plan[11], flag::FIRST);
    }

    #[test]
    fn flows_that_do_not_nest_leave_only_the_runs() {
        // An end without its start: a write is trusted only in its own run.
        let body = [
            constant(1, 0),
            mov(2, 1),
            Instruction::EndFlow { dest: 3 },
            constant(1, 0),
            mov(4, 1),
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[1], 0);
        assert_eq!(plan[4], flag::FIRST);
    }

    #[test]
    fn a_callees_name_and_a_condition_stay_in_their_registers() {
        let body = [
            constant(1, 0),
            constant(9, 0),
            call(2, 9, 1, 1),
            branch(2),
            Instruction::CallLibBuiltin {
                dest: 3,
                function: 9,
                args: 2,
            },
        ];
        let plan = registers_only(&body);
        assert_eq!(plan[2], flag::RANGE, "the arguments move, the name stays");
        assert_eq!(plan[3], 0);
        assert_eq!(plan[4], flag::SECOND, "the argument Vec moves");
    }

    #[test]
    fn an_operand_the_instruction_writes_or_reads_twice_is_kept() {
        let body = [
            mov(1, 1),
            Instruction::Eq {
                dest: 2,
                left: 1,
                right: 1,
            },
            Instruction::SetElement {
                collection: 3,
                index: 2,
                value: 2,
            },
        ];
        assert_eq!(registers_only(&body), vec![0, 0, 0]);
    }

    #[test]
    fn a_jump_empties_the_plan() {
        let body = [mov(1, 2), Instruction::Jump { offset: 0 }];
        assert_eq!(registers_only(&body), vec![0, 0]);
    }

    #[test]
    fn the_last_read_of_a_parameter_takes_it() {
        let constants = strings(&["acc", "x"]);
        let body = [load(1, 0), load(2, 1), load(3, 0), load(4, 1)];
        let plan = plan_of(&body, &["acc", "x"], &constants);
        assert_eq!(plan[0] & flag::VAR, 0);
        assert_eq!(plan[1] & flag::VAR, 0);
        assert_eq!(plan[2] & flag::VAR, flag::VAR);
        assert_eq!(plan[3] & flag::VAR, flag::VAR);
    }

    #[test]
    fn a_name_the_frame_does_not_bind_is_never_taken() {
        let constants = strings(&["outer"]);
        let body = [load(1, 0)];
        assert_eq!(plan_of(&body, &[], &constants)[0] & flag::VAR, 0);
    }

    #[test]
    fn a_local_is_taken_only_when_its_binding_covers_every_read() {
        let constants = strings(&["t", "u"]);
        let store = |var_name, value| Instruction::StoreVar {
            var_name,
            value,
            metadata: None,
        };
        // `t` is read before the body binds it, so the body may be reading
        // a capture; `u` is bound first.
        let body = [load(1, 0), store(0, 1), store(1, 1), load(2, 0), load(3, 1)];
        let plan = plan_of(&body, &[], &constants);
        assert_eq!(plan[3] & flag::VAR, 0);
        assert_eq!(plan[4] & flag::VAR, flag::VAR);

        // Bound in a scope, read outside it: the outer read is of another
        // binding.
        let body = [
            Instruction::EnterScope {
                scope_type: ScopeType::Flow,
            },
            store(1, 1),
            Instruction::ExitScope,
            load(3, 1),
        ];
        assert_eq!(plan_of(&body, &[], &constants)[3] & flag::VAR, 0);
    }

    #[test]
    fn a_name_called_or_captured_later_is_not_taken() {
        // A call through a bare name finds a local of that name first.
        let constants = strings(&["count"]);
        let body = [load(1, 0), constant(2, 0), call(3, 2, 1, 1)];
        assert_eq!(plan_of(&body, &["count"], &constants)[0] & flag::VAR, 0);

        // A lambda created in the body reads the frame by name.
        let lambda = LambdaInfo {
            parameters: vec![],
            instructions: vec![load(1, 0)],
            register_count: 2,
            capture_vars: vec!["acc".to_string()],
            closure_env: Default::default(),
            defining_namespace: String::new(),
            is_lazy_param: false,
            used_registers: vec![1],
            moves: Vec::new(),
            structural_hash_cache: Default::default(),
            defining_frame: None,
            result_params: Vec::new(),
        };
        let constants = vec![
            Constant::StringRef("acc".into()),
            Constant::Val(Val::Box(Box::new(lambda))),
        ];
        // Before or after the load, the lambda keeps the name live.
        let body = [constant(2, 1), load(1, 0)];
        assert_eq!(plan_of(&body, &["acc"], &constants)[1] & flag::VAR, 0);
        let body = [load(1, 0), constant(2, 1)];
        assert_eq!(plan_of(&body, &["acc"], &constants)[0] & flag::VAR, 0);
    }

    #[test]
    fn a_name_a_nested_body_reads_without_capturing_is_not_taken() {
        // As the compiler emits it: the name is a Str constant, and the
        // lambda lists no captures, so it reads the frame when it runs.
        let lambda = LambdaInfo {
            parameters: vec![],
            instructions: vec![load(1, 0)],
            register_count: 2,
            capture_vars: Vec::new(),
            closure_env: Default::default(),
            defining_namespace: String::new(),
            is_lazy_param: false,
            used_registers: vec![1],
            moves: Vec::new(),
            structural_hash_cache: Default::default(),
            defining_frame: None,
            result_params: Vec::new(),
        };
        let constants = vec![
            Constant::Val(Val::from("xs")),
            Constant::Val(Val::Box(Box::new(lambda))),
            Constant::Val(Val::Int(1)),
        ];
        let store = Instruction::StoreVar {
            var_name: 0,
            value: 1,
            metadata: None,
        };
        // The local is read, and only then is the lambda created.
        let body = [constant(1, 2), store, load(2, 0), constant(3, 1)];
        assert_eq!(plan_of(&body, &[], &constants)[2] & flag::VAR, 0);
        // Without the lambda the same read takes the local.
        assert_eq!(
            plan_of(&body[..3], &[], &constants)[2] & flag::VAR,
            flag::VAR
        );

        // A string inside a collection names nothing.
        let constants = vec![
            Constant::Val(Val::from("xs")),
            Constant::Val(Val::vec(vec![Val::from("xs")])),
            Constant::Val(Val::Int(1)),
        ];
        assert_eq!(plan_of(&body, &[], &constants)[2] & flag::VAR, flag::VAR);
    }

    #[test]
    fn a_call_of_a_local_function_by_id_blocks_variable_takes() {
        let constants = strings(&["acc"]);
        let body = [
            load(1, 0),
            Instruction::CallUserFunction {
                dest: 2,
                function_id: 7,
                args_start: 1,
                args_count: 1,
            },
        ];
        let parameters = vec!["acc".to_string()];
        let local = plan(&body, &parameters, &constants, &|id| id == 7, &|_, _| false);
        assert_eq!(local[0] & flag::VAR, 0);
        assert_eq!(local[1] & flag::RANGE, flag::RANGE, "registers still move");
        let top_level = plan(&body, &parameters, &constants, &|_| false, &|_, _| false);
        assert_eq!(top_level[0] & flag::VAR, flag::VAR);
    }

    #[test]
    fn a_parameter_moved_into_a_writer_is_marked() {
        // (acc, x) { assoc(acc, x, x) }
        let constants = strings(&["acc", "x", "assoc", "length"]);
        let body = [
            load(2, 0),
            mov(1, 2),
            load(3, 1),
            mov(2, 3),
            constant(9, 2),
            call(0, 9, 1, 2),
            Instruction::Return { value: 0 },
        ];
        let marks = plan_of(&body, &["acc", "x"], &constants);
        assert_eq!(
            marks[0] & (flag::VAR | flag::WRITER),
            flag::VAR | flag::WRITER
        );
        assert_eq!(marks[2] & flag::WRITER, 0, "the key is not written");
        assert_eq!(
            writer_parameters(&body, &marks, &["acc".into(), "x".into()], &constants),
            1
        );

        // length(acc) reads its argument: nothing is written.
        let reader = [
            load(2, 0),
            mov(1, 2),
            constant(9, 3),
            call(0, 9, 1, 1),
            Instruction::Return { value: 0 },
        ];
        let marks = plan_of(&reader, &["acc"], &constants);
        assert_eq!(marks[0] & flag::VAR, flag::VAR);
        assert_eq!(marks[0] & flag::WRITER, 0);

        // [...acc, x]
        let spread = [
            load(2, 0),
            mov(1, 2),
            load(3, 1),
            mov(2, 3),
            Instruction::MakeVecWithSpread {
                dest: 0,
                elements_start: 1,
                count: 2,
                spread_mask: 1,
            },
            Instruction::Return { value: 0 },
        ];
        let marks = plan_of(&spread, &["acc", "x"], &constants);
        assert_eq!(marks[0] & flag::WRITER, flag::WRITER);
        assert_eq!(marks[2] & flag::WRITER, 0);

        // A user function that writes the parameter it is handed.
        let passed = [
            load(2, 0),
            mov(1, 2),
            Instruction::CallUserFunction {
                dest: 0,
                function_id: 4,
                args_start: 1,
                args_count: 1,
            },
            Instruction::Return { value: 0 },
        ];
        let parameters = vec!["acc".to_string()];
        let to_a_writer = plan(&passed, &parameters, &constants, &|_| false, &|id, at| {
            id == 4 && at == 0
        });
        assert_eq!(to_a_writer[0] & flag::WRITER, flag::WRITER);
        let to_a_reader = plan(&passed, &parameters, &constants, &|_| false, &|_, _| false);
        assert_eq!(to_a_reader[0] & flag::WRITER, 0);
    }
}
