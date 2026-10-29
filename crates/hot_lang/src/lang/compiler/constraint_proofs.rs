//! Skipping `where` checks on values already proven.
//!
//! Values are immutable, so a parameter checked on entry still satisfies its
//! constraint anywhere in a body that never rebinds it. A check of the same
//! spec on it is skipped: a typed binding of the bare parameter, the return
//! check when the body returns it, and the prologue check of a call that
//! passes it on (through `ProvenArgs`). Two specs are the same definition
//! only when they are equal, and each inline `where` gets its own predicate,
//! so separately written predicates never match.

use ahash::{AHashMap, AHashSet};

use super::Compiler;
use super::constraint_specs::SignatureChecks;
use crate::lang::ast::{FlowType, FnCallArg, NamespaceAliases, Program, Ref, Value};
use crate::lang::bytecode::FunctionId;
use crate::val::Val;

/// A function's parameter annotations, for proving its arguments at call
/// sites: its namespace and aliases, and each definition's annotations.
pub(super) struct ParamAnnotations {
    namespace: String,
    aliases: NamespaceAliases,
    defs: Vec<Vec<Option<String>>>,
}

impl Compiler {
    /// Record every function's parameter annotations.
    pub(super) fn collect_param_annotations(&mut self, program: &Program) {
        for (ns_path, namespace) in &program.namespaces {
            for (var, value) in &namespace.scope.vars {
                let Value::Fn(defs) = value else {
                    continue;
                };
                self.param_annotations.insert(
                    format!("{ns_path}/{}", var.sym.name()),
                    ParamAnnotations {
                        namespace: ns_path.to_string(),
                        aliases: namespace.aliases.clone(),
                        defs: defs
                            .iter()
                            .map(|def| {
                                def.args
                                    .args
                                    .iter()
                                    .map(|arg| {
                                        // Variadic and lazy parameters take no proofs.
                                        if def.args.variadic || arg.lazy {
                                            None
                                        } else {
                                            arg.type_annotation.clone()
                                        }
                                    })
                                    .collect()
                            })
                            .collect(),
                    },
                );
            }
        }
        self.callee_param_specs.clear();
    }

    /// The parameters of a function body that its signature checks on entry
    /// and the body never rebinds, with their specs.
    pub(super) fn proven_params(
        checks: Option<&SignatureChecks>,
        body: &Value,
    ) -> AHashMap<String, Val> {
        let Some(checks) = checks else {
            return AHashMap::new();
        };
        let mut bound = AHashSet::new();
        collect_bound_names(body, &mut bound);
        checks
            .params
            .iter()
            .filter(|(name, _, _)| !bound.contains(name))
            .map(|(name, spec, _)| (name.clone(), spec.clone()))
            .collect()
    }

    /// Whether `value` is a proven parameter with this spec.
    pub(super) fn is_proven(&self, value: &Value, spec: &Val) -> bool {
        match value {
            Value::Ref(Ref::Var(var_ref)) if var_ref.var.deep_path.is_none() => self
                .proven_params
                .get(var_ref.var.sym.name())
                .is_some_and(|proven| proven == spec),
            _ => false,
        }
    }

    /// Whether a body returns a proven parameter with this spec.
    pub(super) fn returns_proven(&self, body: &Value, spec: &Val) -> bool {
        self.is_proven(returned_value(body), spec)
    }

    /// The `ProvenArgs` mask for a call to `function_id`: bit `i` when
    /// argument `i` is a proven parameter with parameter `i`'s spec.
    pub(super) fn proven_args_mask(&mut self, function_id: FunctionId, args: &[FnCallArg]) -> u64 {
        if self.proven_params.is_empty() {
            return 0;
        }
        let Some(specs) = self.callee_param_specs(function_id) else {
            return 0;
        };
        let mut mask = 0;
        for (index, arg) in args.iter().enumerate().take(64) {
            if arg.lazy || arg.spread {
                continue;
            }
            if let Some(Some(spec)) = specs.get(index)
                && self.is_proven(&arg.value, spec)
            {
                mask |= 1 << index;
            }
        }
        mask
    }

    /// Each parameter's spec for `function_id`, built in the callee's own
    /// namespace as its prologue's are. `None` unless the call can only
    /// reach one definition.
    fn callee_param_specs(&mut self, function_id: FunctionId) -> Option<Vec<Option<Val>>> {
        if let Some(specs) = self.callee_param_specs.get(&function_id) {
            return specs.clone();
        }
        let specs = (|| {
            let info = self.program.functions.get(function_id as usize)?;
            let arity = info.param_names.len();
            let entry = self.param_annotations.get(&info.name)?;
            let mut defs = entry.defs.iter().filter(|def| def.len() == arity);
            let annotations = defs.next()?.clone();
            if defs.next().is_some() {
                return None;
            }
            let namespace = entry.namespace.clone();
            let aliases = entry.aliases.clone();
            let saved_namespace = std::mem::replace(&mut self.current_namespace, namespace);
            let saved_aliases = std::mem::replace(&mut self.current_aliases, aliases);
            let specs = annotations
                .iter()
                .map(|annotation| {
                    annotation
                        .as_deref()
                        .and_then(|a| self.constraint_spec_for_annotation(a))
                })
                .collect();
            self.current_namespace = saved_namespace;
            self.current_aliases = saved_aliases;
            Some(specs)
        })();
        self.callee_param_specs.insert(function_id, specs.clone());
        specs
    }
}

/// The value a serial body returns: its last expression.
fn returned_value(body: &Value) -> &Value {
    match body {
        Value::Flow(flow)
            if flow.flow_type == FlowType::Serial && flow.result_modifier.is_none() =>
        {
            flow.expressions.last().map_or(body, returned_value)
        }
        _ => body,
    }
}

/// Every name a body binds anywhere: bindings and deep-path assignments,
/// match-arm bindings, and lambda parameters.
fn collect_bound_names(value: &Value, names: &mut AHashSet<String>) {
    match value {
        Value::MultipleValues(values) => {
            if let Some(Value::Ref(Ref::Var(var_ref))) = values.first() {
                names.insert(var_ref.var.sym.name().to_string());
            }
            for value in values {
                collect_bound_names(value, names);
            }
        }
        Value::FnCall(call) => {
            collect_bound_names(&call.function, names);
            for arg in &call.args {
                collect_bound_names(&arg.value, names);
            }
        }
        Value::Flow(flow) | Value::CondDefault(flow) => {
            for expr in &flow.expressions {
                collect_bound_names(expr, names);
            }
        }
        Value::Cond(_, condition, flow) => {
            collect_bound_names(condition, names);
            for expr in &flow.expressions {
                collect_bound_names(expr, names);
            }
        }
        Value::Lambda(lambda) => {
            for arg in &lambda.args.args {
                names.insert(arg.var.sym.name().to_string());
            }
            collect_bound_names(&lambda.body, names);
        }
        Value::Match(match_expr) => {
            collect_bound_names(&match_expr.value, names);
            for arm in &match_expr.arms {
                if let Some(binding) = &arm.binding {
                    names.insert(binding.clone());
                }
                collect_bound_names(&arm.body, names);
            }
        }
        Value::MatchArm(arm) => {
            if let Some(binding) = &arm.binding {
                names.insert(binding.clone());
            }
            collect_bound_names(&arm.body, names);
        }
        Value::Raw(inner) | Value::Do(inner) => collect_bound_names(inner, names),
        Value::TemplateLiteral(template) => {
            for part in &template.parts {
                if let crate::lang::ast::TemplatePart::Expression(expr) = part {
                    collect_bound_names(expr, names);
                }
            }
        }
        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } => {
            for value in base_entries.values() {
                collect_bound_names_in_val(value, names);
            }
            for (_, spread) in spread_entries {
                collect_bound_names(spread, names);
            }
        }
        Value::Val(val, _) => collect_bound_names_in_val(val, names),
        Value::Ref(_)
        | Value::TypeDef(_)
        | Value::TypeImplementation(_)
        | Value::Fn(_)
        | Value::Unbound(_)
        | Value::VariadicExpansion(_)
        | Value::Placeholder(_) => {}
    }
}

/// Bindings inside expressions embedded in Map and Vec literals.
fn collect_bound_names_in_val(val: &Val, names: &mut AHashSet<String>) {
    match val {
        Val::Box(boxed) => {
            if let Some(node) = boxed.as_any().downcast_ref::<crate::lang::ast::AstNode>() {
                collect_bound_names(&node.0, names);
            }
        }
        Val::Map(entries) => {
            for value in entries.values() {
                collect_bound_names_in_val(value, names);
            }
        }
        Val::Vec(items) => {
            for item in items {
                collect_bound_names_in_val(item, names);
            }
        }
        _ => {}
    }
}
