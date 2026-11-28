//! Precomputed static initialization plan for cached bytecode runs.
//!
//! Warm worker invocations (`call_function_with_cached_bytecode*`) previously
//! re-interpreted every top-level instruction in `BytecodeProgram::entry_point`
//! on every event or task — thousands of pure `SetNamespace`, `LoadConst`,
//! `LoadFunctionRef`, `LoadTypeRef`, `StoreVar`, and `StoreGlobal` instructions
//! for `hot-std`, packages, and project definitions.
//!
//! [`StaticInitPlan::build`] analyzes `program.entry_point` once per cached
//! build and splits it into an ordered sequence of [`StaticInitStep`]s:
//! - [`StaticInitStep::ApplyStatic`] replays pre-evaluated pure namespace
//!   bindings and `current_namespace` transitions without touching the
//!   interpreter loop.
//! - [`StaticInitStep::ExecuteDynamic`] executes contiguous ranges of the
//!   original `program.entry_point` (preserving exact instruction pointers for
//!   jumps and `source_map` diagnostics) for every dynamic module initializer
//!   such as `::hot::ctx/get`, `::hot::resource/read*`, `::tool/from-fn`,
//!   flows, closures, constraints, and deep-set mutations.
//!
//! Ordering across dynamic steps is strictly preserved: namespaces that appear
//! after a dynamic step are never pre-populated before that dynamic step runs,
//! so runtime checks like "namespace hasn't run yet" observe identical state
//! to full `entry_point` execution.

use crate::lang::bytecode::{BytecodeProgram, Constant, Instruction, LambdaInfo, RegisterId};
use crate::lang::runtime::vm::NamespaceVars;
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use std::ops::Range;
use std::sync::Arc;

/// One step in a [`StaticInitPlan`].
#[derive(Debug, Clone)]
pub enum StaticInitStep {
    /// Apply precomputed pure static namespace bindings and update
    /// `current_namespace`.
    ///
    /// When `replace_all` is `true` (used when step 0 is static), the map
    /// already contains `::hot::main/ns` and replaces `vm.namespace_variables`
    /// in O(1) via `Arc::clone` (copy-on-write on subsequent mutations).
    ApplyStatic {
        namespace_variables: Arc<NamespaceVars>,
        current_namespace: String,
        replace_all: bool,
    },
    /// Execute `program.entry_point[range]` on the live VM.
    ExecuteDynamic(Range<usize>),
}

/// Precomputed initialization plan derived from a [`BytecodeProgram`].
#[derive(Debug, Clone)]
pub struct StaticInitPlan {
    pub steps: Vec<StaticInitStep>,
    pub static_instruction_count: usize,
    pub dynamic_instruction_count: usize,
}

impl StaticInitPlan {
    /// Fallback plan that executes the entire `entry_point` dynamically.
    fn full_dynamic(len: usize) -> Self {
        let steps = if len == 0 {
            Vec::new()
        } else {
            vec![StaticInitStep::ExecuteDynamic(0..len)]
        };
        Self {
            steps,
            static_instruction_count: 0,
            dynamic_instruction_count: len,
        }
    }

    /// Analyze `program.entry_point` and pre-evaluate pure static bindings.
    pub fn build(program: &BytecodeProgram) -> Self {
        let instrs = &program.entry_point;
        let n = instrs.len();
        if n == 0 {
            return Self {
                steps: Vec::new(),
                static_instruction_count: 0,
                dynamic_instruction_count: 0,
            };
        }

        // Pass 1: mark every instruction inside a flow, error-capture region,
        // lexical scope, or jump/branch span as non-splittable dynamic code.
        let mut must_be_dynamic = vec![false; n];
        let mut flow_depth: usize = 0;
        let mut error_capture_depth: usize = 0;
        let mut scope_depth: usize = 0;
        for (i, instr) in instrs.iter().enumerate() {
            match instr {
                Instruction::BeginFlow { .. } => {
                    flow_depth += 1;
                    must_be_dynamic[i] = true;
                }
                Instruction::EndFlow { .. } => {
                    must_be_dynamic[i] = true;
                    flow_depth = flow_depth.saturating_sub(1);
                }
                Instruction::BeginErrorCapture => {
                    error_capture_depth += 1;
                    must_be_dynamic[i] = true;
                }
                Instruction::EndErrorCapture => {
                    must_be_dynamic[i] = true;
                    error_capture_depth = error_capture_depth.saturating_sub(1);
                }
                Instruction::EnterScope { .. } => {
                    scope_depth += 1;
                    must_be_dynamic[i] = true;
                }
                Instruction::ExitScope => {
                    must_be_dynamic[i] = true;
                    scope_depth = scope_depth.saturating_sub(1);
                }
                Instruction::Jump { offset }
                | Instruction::JumpIf { offset, .. }
                | Instruction::JumpIfNot { offset, .. }
                | Instruction::CondBranch { target: offset, .. }
                | Instruction::CondBranchStart {
                    skip_target: offset,
                    ..
                } => {
                    let target = (i as isize + *offset as isize).clamp(0, n as isize) as usize;
                    let lo = i.min(target);
                    let hi = i.max(target).min(n - 1);
                    for flag in &mut must_be_dynamic[lo..=hi] {
                        *flag = true;
                    }
                }
                _ => {
                    if flow_depth > 0 || error_capture_depth > 0 || scope_depth > 0 {
                        must_be_dynamic[i] = true;
                    }
                }
            }
        }

        // Pass 2: walk instructions in order, committing pure-static statements
        // at safe statement boundaries (`SetNamespace`, `StoreVar`, `StoreGlobal`,
        // or a pure self-`Move` after a pure statement) and keeping everything
        // else as contiguous `ExecuteDynamic` ranges.
        let mut steps: Vec<StaticInitStep> = Vec::new();
        let mut static_instruction_count: usize = 0;
        let mut dynamic_instruction_count: usize = 0;

        let mut current_namespace = "::hot::main".to_string();
        // Track namespaces that already have their implicit `ns` binding so
        // `SetNamespace` mirrors `VM::ensure_namespace_has_ns_variable`.
        let mut has_ns_var: AHashSet<String> = AHashSet::new();
        has_ns_var.insert("::hot::main".to_string());

        let mut pending_static_ns: IndexMap<String, IndexMap<String, Val>> = IndexMap::new();
        // Cumulative mirror of pure static bindings committed so far, used to
        // fold `LoadGlobal` and `LoadVar` across statements/namespaces as long
        // as no untrusted dynamic mutation has tainted the target namespace.
        let mut known_static_ns: IndexMap<String, IndexMap<String, Val>> = IndexMap::new();
        let mut tainted_namespaces: AHashSet<String> = AHashSet::new();
        let mut tainted_all_ns = false;

        let mut pending_static_is_first = true;
        let mut pending_static_has_work = false;
        let mut pending_static_count: usize = 0;
        // Seed step 0 with ::hot::main/ns to match VirtualMachine::new().
        let main_ns_ref = crate::lang::refs::namespace_ref("::hot::main".to_string());
        pending_static_ns
            .entry("::hot::main".to_string())
            .or_default()
            .insert("ns".to_string(), main_ns_ref.clone());
        known_static_ns
            .entry("::hot::main".to_string())
            .or_default()
            .insert("ns".to_string(), main_ns_ref);

        // Scratch register state carried across committed static instructions
        // since the last dynamic instruction. Cleared whenever dynamic code runs.
        let mut committed_regs: AHashMap<RegisterId, Val> = AHashMap::new();

        // Tentative buffer for the current uncommitted statement.
        let mut tentative_start: Option<usize> = None;
        let mut tentative_regs: AHashMap<RegisterId, Val> = AHashMap::new();

        let flush_static = |steps: &mut Vec<StaticInitStep>,
                            pending_static_ns: &mut IndexMap<String, IndexMap<String, Val>>,
                            pending_static_is_first: &mut bool,
                            pending_static_has_work: &mut bool,
                            pending_static_count: &mut usize,
                            static_instruction_count: &mut usize,
                            current_namespace: &str| {
            if *pending_static_has_work {
                let ns_vars: NamespaceVars = std::mem::take(pending_static_ns)
                    .into_iter()
                    .map(|(ns, vars)| (ns, Arc::new(vars)))
                    .collect();
                let replace_all = *pending_static_is_first;
                *pending_static_is_first = false;
                *pending_static_has_work = false;
                *static_instruction_count += *pending_static_count;
                *pending_static_count = 0;
                steps.push(StaticInitStep::ApplyStatic {
                    namespace_variables: Arc::new(ns_vars),
                    current_namespace: current_namespace.to_string(),
                    replace_all,
                });
            } else if *pending_static_is_first {
                // First step will be dynamic; drop the pre-seeded ::hot::main
                // entry because VirtualMachine::new already seeded it.
                pending_static_ns.clear();
                *pending_static_is_first = false;
            }
        };

        let push_dynamic = |steps: &mut Vec<StaticInitStep>,
                            dynamic_instruction_count: &mut usize,
                            range: Range<usize>| {
            if range.is_empty() {
                return;
            }
            *dynamic_instruction_count += range.end - range.start;
            if let Some(StaticInitStep::ExecuteDynamic(prev)) = steps.last_mut()
                && prev.end == range.start
            {
                prev.end = range.end;
                return;
            }
            steps.push(StaticInitStep::ExecuteDynamic(range));
        };

        for (i, instr) in instrs.iter().enumerate() {
            if must_be_dynamic[i] {
                let dyn_start = tentative_start.take().unwrap_or(i);
                tentative_regs.clear();
                committed_regs.clear();
                tainted_all_ns = true;
                // Keep `current_namespace` accurate if a SetNamespace appeared
                // inside a dynamic region (rare, defensive).
                if let Instruction::SetNamespace { namespace } = instr {
                    let Some(ns_name) = constant_string(program, *namespace) else {
                        return Self::full_dynamic(n);
                    };
                    current_namespace = ns_name.clone();
                    has_ns_var.insert(ns_name);
                }
                flush_static(
                    &mut steps,
                    &mut pending_static_ns,
                    &mut pending_static_is_first,
                    &mut pending_static_has_work,
                    &mut pending_static_count,
                    &mut static_instruction_count,
                    &current_namespace,
                );
                push_dynamic(
                    &mut steps,
                    &mut dynamic_instruction_count,
                    dyn_start..(i + 1),
                );
                continue;
            }

            fn reg_lookup<'a>(
                reg: RegisterId,
                tentative: &'a AHashMap<RegisterId, Val>,
                committed: &'a AHashMap<RegisterId, Val>,
            ) -> Option<&'a Val> {
                tentative.get(&reg).or_else(|| committed.get(&reg))
            }

            match instr {
                Instruction::SetNamespace { namespace } => {
                    let Some(ns_name) = constant_string(program, *namespace) else {
                        return Self::full_dynamic(n);
                    };
                    // Commit any preceding pure scratch instructions + SetNamespace.
                    let start = tentative_start.take().unwrap_or(i);
                    for (reg, val) in tentative_regs.drain() {
                        committed_regs.insert(reg, val);
                    }
                    current_namespace = ns_name.clone();
                    if has_ns_var.insert(ns_name.clone()) {
                        let ns_ref = crate::lang::refs::namespace_ref(ns_name.clone());
                        pending_static_ns
                            .entry(ns_name.clone())
                            .or_default()
                            .insert("ns".to_string(), ns_ref.clone());
                        known_static_ns
                            .entry(ns_name)
                            .or_default()
                            .insert("ns".to_string(), ns_ref);
                    }
                    pending_static_has_work = true;
                    pending_static_count += (i + 1) - start;
                }
                Instruction::LoadGlobal {
                    dest,
                    namespace,
                    var_name,
                } => {
                    let folded = if !tainted_all_ns {
                        constant_string(program, *namespace)
                            .zip(constant_string(program, *var_name))
                            .and_then(|(ns_name, name)| {
                                if tainted_namespaces.contains(&ns_name) {
                                    None
                                } else {
                                    known_static_ns
                                        .get(&ns_name)
                                        .and_then(|vars| vars.get(&name))
                                        .cloned()
                                }
                            })
                    } else {
                        None
                    };
                    if let Some(val) = folded {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, val);
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::LoadVar { dest, var_name } => {
                    let folded = if !tainted_all_ns {
                        constant_string(program, *var_name).and_then(|name| {
                            if name.starts_with("::") && name.contains('/') {
                                let stripped = name.trim_start_matches("::");
                                let (ns_path, vname) = stripped.rsplit_once('/')?;
                                let full_ns = format!("::{ns_path}");
                                if tainted_namespaces.contains(&full_ns) {
                                    None
                                } else {
                                    known_static_ns
                                        .get(&full_ns)
                                        .and_then(|vars| vars.get(vname))
                                        .cloned()
                                }
                            } else if tainted_namespaces.contains(&current_namespace) {
                                None
                            } else {
                                known_static_ns
                                    .get(&current_namespace)
                                    .and_then(|vars| vars.get(&name))
                                    .cloned()
                            }
                        })
                    } else {
                        None
                    };
                    if let Some(val) = folded {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, val);
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::LoadConst { dest, constant } => {
                    if let Some(val) = load_pure_static_constant(program, *constant) {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, val);
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::LoadFunctionRef {
                    dest,
                    function_name,
                } => {
                    let Some(name) = constant_string_ref(program, *function_name) else {
                        return Self::full_dynamic(n);
                    };
                    if tentative_start.is_none() {
                        tentative_start = Some(i);
                    }
                    tentative_regs.insert(
                        *dest,
                        crate::lang::runtime::function_ref::function_ref(name),
                    );
                }
                Instruction::LoadTypeRef { dest, type_ref } => {
                    if let Some(val) = load_pure_static_constant(program, *type_ref) {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, val);
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::Move { dest, src } => {
                    if let Some(val) = reg_lookup(*src, &tentative_regs, &committed_regs).cloned() {
                        if dest == src && tentative_start.is_none() {
                            // Self-move emitted by `track_last_value` after a hidden
                            // `where` predicate: pure no-op on namespace state.
                            pending_static_has_work = true;
                            pending_static_count += 1;
                        } else {
                            if tentative_start.is_none() {
                                tentative_start = Some(i);
                            }
                            tentative_regs.insert(*dest, val);
                        }
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::MakeVec {
                    dest,
                    elements_start,
                    count,
                } => {
                    let mut elems = Vec::with_capacity(*count as usize);
                    let mut ok = true;
                    for offset in 0..(*count as RegisterId) {
                        if let Some(val) =
                            reg_lookup(*elements_start + offset, &tentative_regs, &committed_regs)
                        {
                            elems.push(val.clone());
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, Val::vec(elems));
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::MakeMap {
                    dest,
                    entries_start,
                    count,
                } => {
                    let count_usize = *count as usize;
                    let mut map = crate::val::ValIndexMap::with_capacity_and_hasher(
                        count_usize,
                        Default::default(),
                    );
                    let mut ok = true;
                    for idx in 0..count_usize {
                        let key_reg = *entries_start + (2 * idx) as RegisterId;
                        let val_reg = key_reg + 1;
                        let (Some(key), Some(val)) = (
                            reg_lookup(key_reg, &tentative_regs, &committed_regs),
                            reg_lookup(val_reg, &tentative_regs, &committed_regs),
                        ) else {
                            ok = false;
                            break;
                        };
                        if key.check_shape("map key").is_err() {
                            ok = false;
                            break;
                        }
                        map.insert(key.clone(), val.clone());
                    }
                    if ok {
                        if tentative_start.is_none() {
                            tentative_start = Some(i);
                        }
                        tentative_regs.insert(*dest, Val::map(map));
                    } else {
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::StoreVar { var_name, value } => {
                    let Some(name) = constant_string(program, *var_name) else {
                        return Self::full_dynamic(n);
                    };
                    if let Some(val) = reg_lookup(*value, &tentative_regs, &committed_regs).cloned()
                    {
                        let start = tentative_start.take().unwrap_or(i);
                        for (reg, v) in tentative_regs.drain() {
                            committed_regs.insert(reg, v);
                        }
                        if name == "ns" {
                            has_ns_var.insert(current_namespace.clone());
                        }
                        pending_static_ns
                            .entry(current_namespace.clone())
                            .or_default()
                            .insert(name.clone(), val.clone());
                        known_static_ns
                            .entry(current_namespace.clone())
                            .or_default()
                            .insert(name, val);
                        pending_static_has_work = true;
                        pending_static_count += (i + 1) - start;
                    } else {
                        tainted_namespaces.insert(current_namespace.clone());
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                Instruction::StoreGlobal {
                    namespace,
                    var_name,
                    value,
                } => {
                    let (Some(ns_name), Some(name)) = (
                        constant_string(program, *namespace),
                        constant_string(program, *var_name),
                    ) else {
                        return Self::full_dynamic(n);
                    };
                    if let Some(val) = reg_lookup(*value, &tentative_regs, &committed_regs).cloned()
                    {
                        let start = tentative_start.take().unwrap_or(i);
                        for (reg, v) in tentative_regs.drain() {
                            committed_regs.insert(reg, v);
                        }
                        if name == "ns" {
                            has_ns_var.insert(ns_name.clone());
                        }
                        pending_static_ns
                            .entry(ns_name.clone())
                            .or_default()
                            .insert(name.clone(), val.clone());
                        known_static_ns
                            .entry(ns_name)
                            .or_default()
                            .insert(name, val);
                        pending_static_has_work = true;
                        pending_static_count += (i + 1) - start;
                    } else {
                        tainted_namespaces.insert(ns_name);
                        let dyn_start = tentative_start.take().unwrap_or(i);
                        tentative_regs.clear();
                        committed_regs.clear();
                        flush_static(
                            &mut steps,
                            &mut pending_static_ns,
                            &mut pending_static_is_first,
                            &mut pending_static_has_work,
                            &mut pending_static_count,
                            &mut static_instruction_count,
                            &current_namespace,
                        );
                        push_dynamic(
                            &mut steps,
                            &mut dynamic_instruction_count,
                            dyn_start..(i + 1),
                        );
                    }
                }
                _ => {
                    // Arbitrary dynamic instructions (e.g., calls, deep-set,
                    // constraints) may write to namespace state at runtime.
                    tainted_all_ns = true;
                    let dyn_start = tentative_start.take().unwrap_or(i);
                    tentative_regs.clear();
                    committed_regs.clear();
                    flush_static(
                        &mut steps,
                        &mut pending_static_ns,
                        &mut pending_static_is_first,
                        &mut pending_static_has_work,
                        &mut pending_static_count,
                        &mut static_instruction_count,
                        &current_namespace,
                    );
                    push_dynamic(
                        &mut steps,
                        &mut dynamic_instruction_count,
                        dyn_start..(i + 1),
                    );
                }
            }
        }

        // Flush any remaining committed static batch, and emit any uncommitted
        // trailing instructions (e.g., a terminal TypeImplementation LoadConst)
        // dynamically so nothing is dropped.
        flush_static(
            &mut steps,
            &mut pending_static_ns,
            &mut pending_static_is_first,
            &mut pending_static_has_work,
            &mut pending_static_count,
            &mut static_instruction_count,
            &current_namespace,
        );
        if let Some(start) = tentative_start {
            push_dynamic(&mut steps, &mut dynamic_instruction_count, start..n);
        }

        // Pass 3 (soundness verification): prove every ExecuteDynamic range is
        // register-self-contained (never reads a register written only by a
        // preceding ApplyStatic step). If any range ever violates this, fall
        // back cleanly to executing the entire entry_point dynamically.
        if !verify_dynamic_ranges_self_contained(instrs, &steps) {
            tracing::debug!(
                "StaticInitPlan fell back to full dynamic init (cross-boundary register read)"
            );
            return Self::full_dynamic(n);
        }

        Self {
            steps,
            static_instruction_count,
            dynamic_instruction_count,
        }
    }
}

/// Verify that no `ExecuteDynamic` step reads a register before defining it
/// within the same dynamic range (ignoring self-moves `Move { dest, src }`
/// where `dest == src` emitted solely for `last_result_register`).
fn verify_dynamic_ranges_self_contained(instrs: &[Instruction], steps: &[StaticInitStep]) -> bool {
    // Registers left alive by an earlier dynamic range remain in `vm.registers`
    // because `ApplyStatic` never overwrites registers. Only reads of registers
    // written exclusively by a skipped `ApplyStatic` would observe stale state.
    let mut defined_by_dynamic: AHashSet<RegisterId> = AHashSet::new();
    for step in steps {
        let StaticInitStep::ExecuteDynamic(range) = step else {
            continue;
        };
        for instr in &instrs[range.clone()] {
            if let Instruction::Move { dest, src } = instr
                && dest == src
            {
                continue;
            }
            if let Some(ops) = crate::lang::moves::operands(instr) {
                if ops
                    .all_reads()
                    .any(|reg| !defined_by_dynamic.contains(&reg))
                {
                    return false;
                }
                if let Some(dest) = ops.write {
                    defined_by_dynamic.insert(dest);
                }
                if let Some(upd) = ops.update {
                    defined_by_dynamic.insert(upd);
                }
            } else {
                // Jump/branch: check any condition register operand.
                let cond = match instr {
                    Instruction::JumpIf { condition, .. }
                    | Instruction::JumpIfNot { condition, .. }
                    | Instruction::CondBranch { condition, .. }
                    | Instruction::CondBranchStart { condition, .. } => Some(*condition),
                    _ => None,
                };
                if let Some(reg) = cond
                    && !defined_by_dynamic.contains(&reg)
                {
                    return false;
                }
            }
        }
    }
    true
}

fn constant_string(program: &BytecodeProgram, id: u32) -> Option<String> {
    match program.constants.get(id as usize)? {
        Constant::Val(Val::Str(s)) | Constant::StringRef(s) => Some(s.to_string()),
        _ => None,
    }
}

fn constant_string_ref(program: &BytecodeProgram, id: u32) -> Option<String> {
    match program.constants.get(id as usize)? {
        Constant::StringRef(s) | Constant::Val(Val::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Resolve a constant purely if and only if `VirtualMachine::load_constant`
/// and `LoadConst` return it without reading VM state, resolving AST nodes,
/// or capturing closure variables.
fn load_pure_static_constant(program: &BytecodeProgram, id: u32) -> Option<Val> {
    match program.constants.get(id as usize)? {
        Constant::Val(v) if is_pure_static_val(v) => {
            if let Val::Box(boxed) = v
                && let Some(lambda) = boxed.as_any().downcast_ref::<LambdaInfo>()
            {
                let mut clean = lambda.clone();
                clean.closure_env.clear();
                clean.defining_frame = None;
                return Some(Val::Box(Box::new(clean)));
            }
            Some(v.clone())
        }
        Constant::Val(_) => None,
        Constant::FunctionRef(name) => Some(crate::lang::runtime::function_ref::function_ref(
            name.to_string(),
        )),
        Constant::TypeRef(name) | Constant::StringRef(name) => {
            Some(Val::Str(std::sync::Arc::clone(name)))
        }
        Constant::VariantTypeRef(type_ref) => Some(Val::Box(Box::new(type_ref.clone()))),
    }
}

fn is_pure_static_val(val: &Val) -> bool {
    match val {
        Val::Null
        | Val::Bool(_)
        | Val::Int(_)
        | Val::Dec(_)
        | Val::Str(_)
        | Val::Bytes(_)
        | Val::Byte(_) => true,
        Val::Vec(items) => items.iter().all(is_pure_static_val),
        Val::Map(map) => map
            .iter()
            .all(|(k, v)| is_pure_static_val(k) && is_pure_static_val(v)),
        Val::Box(boxed) => {
            if let Some(lambda) = boxed.as_any().downcast_ref::<LambdaInfo>() {
                // At top-level `entry_point` (outside any `EnterScope` or
                // `BeginFlow`), `scope_stack` contains only the empty global
                // `Namespace` scope, so a zero-capture lambda (e.g., `where`
                // type predicates and pure top-level lambdas) captures nothing
                // and has identical lookup semantics with `defining_frame = None`.
                return lambda.capture_vars.is_empty() && lambda.closure_env.is_empty();
            }
            boxed.as_any().is::<crate::lang::refs::NamespaceRef>()
                || boxed.as_any().is::<crate::lang::refs::TypeRef>()
                || boxed
                    .as_any()
                    .is::<crate::lang::runtime::function_ref::FunctionRef>()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::cache::bytecode_cache::{
        CacheMetadata, CachedBytecode, CachedBytecodeArtifacts,
    };
    use crate::lang::engine::Engine;
    use crate::lang::runtime::vm::VirtualMachine;
    use std::sync::Arc;

    fn compile_with_std(files: &[(&str, &str)]) -> (tempfile::TempDir, CachedBytecode) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("create src");
        for (name, content) in files {
            let path = src.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, content).expect("write source");
        }
        let (_, artifacts) = Engine::compile_project_for_cache(
            &[src.to_string_lossy().to_string()],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("compile_project_for_cache should succeed");
        let compiler = crate::lang::Compiler::new();
        let tool_specs = compiler.build_tool_specs(&artifacts.hot_ast.program);
        let skill_specs = compiler.build_skill_specs(&artifacts.hot_ast.program);
        let cached = CachedBytecode::new(
            CacheMetadata {
                project_name: "test".to_string(),
                hot_version: crate::build_info::VERSION.to_string(),
                git_sha: crate::build_info::GIT_SHA.to_string(),
                cache_format_version: crate::hasher::CacheType::Bytecode.format_version(),
                created_at: 0,
                file_hashes: Vec::new(),
                cache_key: "test".to_string(),
            },
            CachedBytecodeArtifacts {
                program: artifacts.program,
                function_mapping: artifacts.function_mapping,
                core_functions: artifacts.core_functions,
                type_implementations: artifacts.type_implementations,
                hot_ast: artifacts.hot_ast,
                tool_specs,
                skill_specs,
            },
        );
        (dir, cached)
    }

    #[test]
    fn entry_register_count_and_function_registers_stay_bounded() {
        let (_dir, cached) = compile_with_std(&[(
            "app.hot",
            r#"
::app ns

Positive type Int where gt(%, 0)

A 1
B 2
C 3
D "hello"
E [1, 2, 3]
F {a: 1, b: 2}

fib fn (n: Int): Int {
    cond {
        lte(n, 1) => n
        => add(fib(sub(n, 1)), fib(sub(n, 2)))
    }
}

use-lambda fn (xs: Vec<Int>): Vec<Int> {
    map(xs, (x) { add(x, 1) })
}

run fn (): Map {
    {fib: fib(10), mapped: use-lambda([1, 2, 3])}
}

RESULT 42
NonZero type Int where ne(%, 0)
"#,
        )]);

        assert!(
            cached.program.entry_register_count < 64,
            "entry_register_count should reuse scratch registers across hot-std and app declarations, got {}",
            cached.program.entry_register_count
        );
        let max_fn_regs = cached
            .program
            .functions
            .iter()
            .map(|f| f.register_count)
            .max()
            .unwrap_or(0);
        assert!(
            max_fn_regs < 1024,
            "per-function 0-based register_count should stay bounded across hot-std, got max {max_fn_regs}"
        );
        let run_fn_id = *cached
            .function_mapping
            .get("::app/run/0")
            .expect("::app/run/0 in function_mapping");
        let run_fn_regs = cached.program.functions[run_fn_id as usize].register_count;
        assert!(
            run_fn_regs < 32,
            "user function ::app/run should use 0-based frame registers, got {run_fn_regs}"
        );

        // Verify a trailing hidden `where` predicate preserves the preceding
        // declaration's register as the namespace's last value.
        let mut vm = VirtualMachine::new(
            Arc::clone(&cached.program),
            Some(Arc::clone(&cached.hot_ast)),
            Arc::clone(&cached.function_mapping),
            Arc::clone(&cached.core_functions),
            Arc::clone(&cached.type_implementations),
            Arc::clone(&cached.runtime_core_variables),
            None,
        );
        let entry_result = vm.execute().expect("entry_point execute should succeed");
        assert_eq!(entry_result, Val::Int(42));

        let result = Engine::call_function_with_cached_bytecode(
            "::app/run",
            &[],
            Arc::new(cached),
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
            None,
        )
        .expect("run should succeed");
        assert_eq!(result, crate::val!({"fib": 55, "mapped": [2, 3, 4]}));
    }

    #[test]
    fn static_init_plan_matches_full_execute_with_mixed_static_and_dynamic_modules() {
        let (_dir, cached) = compile_with_std(&[
            (
                "consts.hot",
                r#"
::app::consts ns

GREETING "hi"
NUMS [10, 20, 30]
CFG {a: 1, b: "two"}
Status enum { Ready, Done }
Item type { name: Str, count: Int }

fmt fn (who: Str): Str { `${GREETING} ${who}` }
"#,
            ),
            (
                "dynmod.hot",
                r#"
::app::dynmod ns

cfg-prefix meta {ctx: {"app.prefix": {default: "default-prefix"}}} ::hot::ctx/get("app.prefix")
computed ::app::consts/fmt(cfg-prefix)
doubler (x) { mul(x, 2) }
add-prefix (s) { `${cfg-prefix}:${s}` }
nested.a 1
nested.b 2

read-state fn (): Map {
    {
        prefix: cfg-prefix,
        computed: computed,
        doubled: doubler(7),
        prefixed: add-prefix("ok"),
        nested: nested,
        status: ::app::consts/Status.Ready,
        item: ::app::consts/Item({name: "x", count: 3}),
    }
}
"#,
            ),
            (
                "after.hot",
                r#"
::app::after ns

TAIL "tail-value"
get-tail fn (): Str { TAIL }
"#,
            ),
        ]);

        let plan = cached.static_init_plan();
        assert!(
            plan.static_instruction_count > 1000,
            "expected hot-std and static namespaces to pre-evaluate statically, got static={} dynamic={}",
            plan.static_instruction_count,
            plan.dynamic_instruction_count
        );
        assert!(
            plan.dynamic_instruction_count > 0
                && plan.dynamic_instruction_count < cached.program.entry_point.len() / 10,
            "expected only a tiny dynamic slice, got static={} dynamic={} total={}",
            plan.static_instruction_count,
            plan.dynamic_instruction_count,
            cached.program.entry_point.len()
        );

        let make_vm = |prefix: &str| {
            let mut vm = VirtualMachine::new(
                Arc::clone(&cached.program),
                Some(Arc::clone(&cached.hot_ast)),
                Arc::clone(&cached.function_mapping),
                Arc::clone(&cached.core_functions),
                Arc::clone(&cached.type_implementations),
                Arc::clone(&cached.runtime_core_variables),
                None,
            );
            vm.context_storage
                .insert("app.prefix".to_string(), Val::from(prefix));
            vm.apply_ctx_defaults(
                cached
                    .runtime_ctx_defaults
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            vm
        };

        let mut vm_full = make_vm("alpha");
        vm_full.execute().expect("full execute should succeed");

        let mut vm_fast = make_vm("alpha");
        vm_fast
            .execute_static_init_plan(plan)
            .expect("static init plan should succeed");

        assert_eq!(vm_fast.current_namespace, vm_full.current_namespace);
        assert_eq!(
            vm_fast.namespace_variables.len(),
            vm_full.namespace_variables.len()
        );
        for (ns, full_vars) in vm_full.namespace_variables.iter() {
            let fast_vars = vm_fast
                .namespace_variables
                .get(ns)
                .unwrap_or_else(|| panic!("missing namespace {ns}"));
            assert_eq!(
                fast_vars.len(),
                full_vars.len(),
                "variable count mismatch in {ns}"
            );
            for (k, v) in full_vars.iter() {
                assert_eq!(fast_vars.get(k), Some(v), "binding mismatch for {ns}/{k}");
            }
        }

        // Dynamic initializers must re-run per invocation with live context_storage.
        let cached_arc = Arc::new(cached);
        let mut ctx_a = AHashMap::new();
        ctx_a.insert("app.prefix".to_string(), Val::from("first-run"));
        let res_a = Engine::call_function_with_cached_bytecode(
            "::app::dynmod/read-state",
            &[],
            Arc::clone(&cached_arc),
            None,
            None,
            None,
            None,
            Some(ctx_a),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("first cached call");

        let mut ctx_b = AHashMap::new();
        ctx_b.insert("app.prefix".to_string(), Val::from("second-run"));
        let res_b = Engine::call_function_with_cached_bytecode(
            "::app::dynmod/read-state",
            &[],
            Arc::clone(&cached_arc),
            None,
            None,
            None,
            None,
            Some(ctx_b),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("second cached call");

        assert_eq!(res_a.get("prefix"), Some(Val::from("first-run")));
        assert_eq!(res_a.get("computed"), Some(Val::from("hi first-run")));
        assert_eq!(res_a.get("doubled"), Some(Val::Int(14)));
        assert_eq!(res_a.get("prefixed"), Some(Val::from("first-run:ok")));
        assert_eq!(res_b.get("prefix"), Some(Val::from("second-run")));
        assert_eq!(res_b.get("computed"), Some(Val::from("hi second-run")));
        assert_eq!(res_b.get("prefixed"), Some(Val::from("second-run:ok")));
    }

    #[test]
    #[ignore = "manual microbenchmark for PR 3e before/after measurement"]
    fn bench_static_init_vs_full_execute() {
        let (_dir, cached) = compile_with_std(&[(
            "app.hot",
            r#"
::app ns

GREETING "hello"
add-one fn (x: Int): Int { add(x, 1) }
"#,
        )]);
        let plan = cached.static_init_plan();
        let entry_regs = cached.program.entry_register_count;
        let max_fn_regs = cached
            .program
            .functions
            .iter()
            .map(|f| f.register_count)
            .max()
            .unwrap_or(0);
        let add_one_id = *cached.function_mapping.get("::app/add-one/1").unwrap();
        let add_one_regs = cached.program.functions[add_one_id as usize].register_count;

        let make_vm = || {
            VirtualMachine::new(
                Arc::clone(&cached.program),
                Some(Arc::clone(&cached.hot_ast)),
                Arc::clone(&cached.function_mapping),
                Arc::clone(&cached.core_functions),
                Arc::clone(&cached.type_implementations),
                Arc::clone(&cached.runtime_core_variables),
                None,
            )
        };

        // Warmup
        for _ in 0..5 {
            let mut vm = make_vm();
            vm.execute().unwrap();
            let mut vm = make_vm();
            vm.attach_cached_dispatch_tables(
                cached.sorted_function_key_indices(),
                cached.native_core_shadows(),
            );
            vm.execute_static_init_plan(plan).unwrap();
        }

        let iters = 200;
        let full_start = std::time::Instant::now();
        for _ in 0..iters {
            let mut vm = make_vm();
            vm.execute().unwrap();
            let v = vm
                .call_function_bypassing_unified_lookup("::app/add-one", &[Val::Int(41)])
                .unwrap();
            assert_eq!(v, Val::Int(42));
        }
        let full_us = full_start.elapsed().as_micros() as f64 / iters as f64;

        let mut vm_new_ns = 0u128;
        let mut init_ns = 0u128;
        let mut call_ns = 0u128;
        let fast_start = std::time::Instant::now();
        for _ in 0..iters {
            let t0 = std::time::Instant::now();
            let mut vm = make_vm();
            vm.attach_cached_dispatch_tables(
                cached.sorted_function_key_indices(),
                cached.native_core_shadows(),
            );
            let t1 = std::time::Instant::now();
            vm.execute_static_init_plan(plan).unwrap();
            let t2 = std::time::Instant::now();
            let v = vm
                .call_function_bypassing_unified_lookup("::app/add-one", &[Val::Int(41)])
                .unwrap();
            let t3 = std::time::Instant::now();
            assert_eq!(v, Val::Int(42));
            vm_new_ns += (t1 - t0).as_nanos();
            init_ns += (t2 - t1).as_nanos();
            call_ns += (t3 - t2).as_nanos();
        }
        let fast_us = fast_start.elapsed().as_micros() as f64 / iters as f64;

        eprintln!(
            "BENCH: entry_instrs={} static={} dynamic={} steps={} entry_regs={} max_fn_regs={} add_one_regs={} functions={} | full_execute={:.1}us/call static_init={:.1}us/call (vm_new={:.1}us init={:.1}us call={:.1}us) speedup={:.2}x",
            cached.program.entry_point.len(),
            plan.static_instruction_count,
            plan.dynamic_instruction_count,
            plan.steps.len(),
            entry_regs,
            max_fn_regs,
            add_one_regs,
            cached.program.functions.len(),
            full_us,
            fast_us,
            vm_new_ns as f64 / 1000.0 / iters as f64,
            init_ns as f64 / 1000.0 / iters as f64,
            call_ns as f64 / 1000.0 / iters as f64,
            full_us / fast_us,
        );
    }
}
