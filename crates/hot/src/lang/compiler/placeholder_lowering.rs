//! # Placeholder lambda lowering
//!
//! After parsing, `%` placeholder arguments survive in the AST as bare
//! [`Value::Placeholder`] nodes. This compiler pass walks every function body
//! in the program, looks up the callee signatures in the global namespace
//! registry, and wraps each placeholder-bearing argument into a lambda at the
//! **nearest enclosing parameter slot whose declared type is `Fn`**.
//!
//! The rule is a single sentence:
//!
//! > A `%` placeholder is bound by the nearest enclosing parameter slot
//! > whose declared type contains `Fn`. If there is no such slot, the
//! > program does not compile — write `%(expr)` to construct an explicit
//! > lambda value.
//!
//! This replaces the old hardcoded "list of known higher-order functions"
//! heuristic. It works for any function (built-in or user-defined) whose
//! argument is declared `Fn` (or any union/lazy variant containing `Fn`).
//!
//! Explicit `%(expr)` boundaries are already handled by the parser via
//! `force_wrap_placeholder`, so by the time this pass runs the only bare
//! [`Value::Placeholder`] nodes remaining come from implicit usages.

use crate::lang::ast::*;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::errors::{CompilerError, CompilerErrors, CompilerResult, ErrorLocation};
use crate::lang::parser::{contains_placeholder, wrap_placeholder_arg};
use crate::val::Val;
use indexmap::IndexMap;
use std::path::PathBuf;

/// Run the placeholder lowering pass over the entire program.
///
/// On success, every implicit `%` placeholder has been wrapped into a lambda
/// at the appropriate `Fn`-typed parameter slot. On failure, the returned
/// [`CompilerErrors`] describes each `%` that did not land in a `Fn` slot
/// (with source location and a `%(expr)` migration hint).
pub fn lower_program(program: &mut Program) -> CompilerResult<()> {
    let registry = SignatureRegistry::build(program);

    // Walk every function body in every namespace and lower placeholders.
    // We mutate values in place via `std::mem::replace` so the IndexMap's
    // key order is preserved — downstream compilation phases rely on
    // deterministic var iteration order.
    let ns_paths: Vec<NsPath> = program.namespaces.keys().cloned().collect();
    for ns_path in ns_paths {
        let aliases = program
            .namespaces
            .get(&ns_path)
            .map(|n| n.aliases.clone())
            .unwrap_or_default();
        let var_keys: Vec<Var> = program
            .namespaces
            .get(&ns_path)
            .map(|n| n.scope.vars.keys().cloned().collect())
            .unwrap_or_default();
        for var in var_keys {
            let Some(namespace) = program.namespaces.get_mut(&ns_path) else {
                continue;
            };
            let Some(slot) = namespace.scope.vars.get_mut(&var) else {
                continue;
            };
            let taken = std::mem::replace(slot, Value::Placeholder(0));
            let lowered = lower_value(taken, &ns_path, &aliases, &registry);
            *slot = lowered;
        }
    }

    // After lowering, scan for any leftover bare placeholders. Each one is a
    // user error: a `%` that did not land in a `Fn`-typed parameter slot.
    let mut errors: Vec<CompilerError> = Vec::new();
    for (ns_path, namespace) in &program.namespaces {
        let source_file = namespace.source_file.clone();
        for (var, value) in &namespace.scope.vars {
            collect_placeholder_errors(
                value,
                var.sym.name(),
                ns_path,
                source_file.as_ref(),
                &mut errors,
            );
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let mut bundle = CompilerErrors::new();
        for err in errors {
            bundle.add(err);
        }
        Err(bundle)
    }
}

/// Map of declared function signatures across the whole program.
///
/// We index two ways:
/// * `by_qualified` — full `(namespace, name)` for explicit `::ns/name` calls
///   and same-namespace calls.
/// * `by_short_name` — bare `name` to handle core/auto-imported functions
///   (e.g. `some`, `filter`, `map`) called by short name from any namespace.
///   Within a short-name bucket, signatures from core-tagged definitions are
///   listed first so they win the lookup.
///
/// Built once per pass over an immutable view of the program so we can mutate
/// function bodies while looking up callees.
struct SignatureRegistry {
    by_qualified: IndexMap<(String, String), Vec<FnArgs>>,
    by_short_name: IndexMap<String, Vec<FnArgs>>,
}

impl SignatureRegistry {
    fn build(program: &Program) -> Self {
        let mut by_qualified: IndexMap<(String, String), Vec<FnArgs>> = IndexMap::new();
        let mut by_short_name: IndexMap<String, Vec<FnArgs>> = IndexMap::new();
        let mut by_short_name_non_core: IndexMap<String, Vec<FnArgs>> = IndexMap::new();

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                let Value::Fn(fn_defs) = value else { continue };
                let name = var.sym.name().to_string();
                let qualified_entry = by_qualified
                    .entry((ns_str.clone(), name.clone()))
                    .or_default();
                let is_core = CoreVariableRegistry::has_core_metadata(var);
                let short_bucket = if is_core {
                    by_short_name.entry(name.clone()).or_default()
                } else {
                    by_short_name_non_core.entry(name.clone()).or_default()
                };
                for fn_def in fn_defs {
                    qualified_entry.push(fn_def.args.clone());
                    short_bucket.push(fn_def.args.clone());
                }
            }
        }

        // Append non-core short-name entries after core ones so core wins.
        for (name, sigs) in by_short_name_non_core {
            by_short_name.entry(name).or_default().extend(sigs);
        }

        Self {
            by_qualified,
            by_short_name,
        }
    }

    /// Look up the declared signature of the function being called.
    /// Returns `None` if the callee is dynamic (lexical Fn variable, computed
    /// expression, etc.) — those cases simply don't introduce a wrap boundary.
    fn lookup(
        &self,
        function: &Value,
        arity: usize,
        current_ns: &NsPath,
        aliases: &NamespaceAliases,
    ) -> Option<&FnArgs> {
        match function {
            Value::Ref(Ref::Var(vr)) => {
                let name = vr.var.sym.name();
                let current_ns_str = ToString::to_string(current_ns);
                // 1. Same namespace
                if let Some(sigs) = self.by_qualified.get(&(current_ns_str, name.to_string()))
                    && let Some(found) = best_match(sigs, arity)
                {
                    return Some(found);
                }
                // 2. Auto-imported / core (or unique short name fallback)
                self.by_short_name
                    .get(name)
                    .and_then(|sigs| best_match(sigs, arity))
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                let resolved_ns = aliases
                    .get(&ns_ref.ns)
                    .cloned()
                    .unwrap_or_else(|| ns_ref.ns.clone());
                let name = ns_ref.function_name.as_deref()?;
                let sigs = self
                    .by_qualified
                    .get(&(ToString::to_string(&resolved_ns), name.to_string()))?;
                best_match(sigs, arity)
            }
            _ => None,
        }
    }
}

fn best_match(candidates: &[FnArgs], arity: usize) -> Option<&FnArgs> {
    // Prefer exact arity match (or variadic arity match), then any candidate
    // with at least one Fn-typed parameter (so an overload that matters for
    // placeholder lowering wins over a same-name primitive overload), then
    // first overall.
    candidates
        .iter()
        .find(|args| {
            // Non-variadic: exact arity. Variadic: at least the fixed params,
            // since the trailing `...rest` accepts zero or more arguments.
            if args.variadic {
                arity + 1 >= args.args.len()
            } else {
                args.args.len() == arity
            }
        })
        .or_else(|| {
            candidates.iter().find(|args| {
                args.args.iter().any(|a| {
                    a.type_annotation
                        .as_deref()
                        .map(is_fn_typed)
                        .unwrap_or(false)
                })
            })
        })
        .or_else(|| candidates.first())
}

/// True when a parameter's declared type annotation contains the `Fn` token.
/// Catches `Fn`, `Lazy Fn`, `Fn | Null`, `Fn?`, `Vec<Fn>`, etc.
fn is_fn_typed(annotation: &str) -> bool {
    annotation
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == "Fn")
}

fn lower_value(
    value: Value,
    current_ns: &NsPath,
    aliases: &NamespaceAliases,
    registry: &SignatureRegistry,
) -> Value {
    lower_value_inner(value, current_ns, aliases, registry, 0)
}

/// `arg_index_offset` shifts how source-level argument indices map onto
/// declared parameter indices for the **outermost** call in `value`. This
/// is used by the pipe-flow case to model that every non-leading stage of
/// a pipe chain has an implicit first argument (the previous result).
fn lower_value_inner(
    value: Value,
    current_ns: &NsPath,
    aliases: &NamespaceAliases,
    registry: &SignatureRegistry,
    arg_index_offset: usize,
) -> Value {
    match value {
        Value::FnCall(mut call) => {
            // Effective arity for signature lookup includes the piped LHS, if any.
            let effective_arity = call.args.len() + arg_index_offset;
            let signature_args: Vec<Option<String>> = registry
                .lookup(&call.function, effective_arity, current_ns, aliases)
                .map(|args| {
                    args.args
                        .iter()
                        .map(|a| a.type_annotation.clone())
                        .collect()
                })
                .unwrap_or_default();

            call.function = Box::new(lower_value(*call.function, current_ns, aliases, registry));

            let new_args: Vec<FnCallArg> = call
                .args
                .into_iter()
                .enumerate()
                .map(|(i, mut arg)| {
                    // Inner expressions of an arg are not "outermost" — they
                    // carry no piped LHS, so reset the offset to 0.
                    let lowered = lower_value(arg.value, current_ns, aliases, registry);
                    let param_index = i + arg_index_offset;
                    let is_fn_slot = signature_args
                        .get(param_index)
                        .and_then(|opt| opt.as_deref())
                        .map(is_fn_typed)
                        .unwrap_or(false);
                    arg.value = if is_fn_slot && contains_placeholder(&lowered) {
                        wrap_placeholder_arg(lowered)
                    } else {
                        lowered
                    };
                    arg
                })
                .collect();
            call.args = new_args;
            Value::FnCall(call)
        }

        Value::Flow(mut flow) => {
            let is_pipe = matches!(flow.flow_type, FlowType::Pipe);
            flow.expressions = flow
                .expressions
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    // In a pipe flow, every stage after the first has an
                    // implicit leading arg (the previous result). Account
                    // for that when looking up the callee signature.
                    let offset = if is_pipe && i > 0 { 1 } else { 0 };
                    lower_value_inner(e, current_ns, aliases, registry, offset)
                })
                .collect();
            Value::Flow(flow)
        }

        Value::Fn(fn_defs) => {
            // Bodies of nested function definitions are independent units —
            // recurse into each. Aliases come from the enclosing namespace
            // (Hot doesn't currently scope aliases per nested-fn).
            let new_defs = fn_defs
                .into_iter()
                .map(|mut fn_def| {
                    fn_def.body = lower_value(fn_def.body, current_ns, aliases, registry);
                    fn_def
                })
                .collect();
            Value::Fn(new_defs)
        }

        Value::Lambda(mut lambda) => {
            lambda.body = Box::new(lower_value(*lambda.body, current_ns, aliases, registry));
            Value::Lambda(lambda)
        }

        Value::Cond(label, cond, mut flow) => {
            let cond = lower_value(*cond, current_ns, aliases, registry);
            flow.expressions = flow
                .expressions
                .into_iter()
                .map(|e| lower_value(e, current_ns, aliases, registry))
                .collect();
            Value::Cond(label, Box::new(cond), flow)
        }

        Value::CondDefault(mut flow) => {
            flow.expressions = flow
                .expressions
                .into_iter()
                .map(|e| lower_value(e, current_ns, aliases, registry))
                .collect();
            Value::CondDefault(flow)
        }

        Value::TemplateLiteral(mut tl) => {
            tl.parts = tl
                .parts
                .into_iter()
                .map(|p| match p {
                    TemplatePart::Expression(e) => TemplatePart::Expression(Box::new(lower_value(
                        *e, current_ns, aliases, registry,
                    ))),
                    other => other,
                })
                .collect();
            Value::TemplateLiteral(tl)
        }

        Value::Raw(inner) => {
            Value::Raw(Box::new(lower_value(*inner, current_ns, aliases, registry)))
        }

        Value::Do(inner) => Value::Do(Box::new(lower_value(*inner, current_ns, aliases, registry))),

        Value::MultipleValues(vals) => Value::MultipleValues(
            vals.into_iter()
                .map(|v| lower_value(v, current_ns, aliases, registry))
                .collect(),
        ),

        Value::Match(mut m) => {
            m.value = Box::new(lower_value(*m.value, current_ns, aliases, registry));
            m.arms = m
                .arms
                .into_iter()
                .map(|mut arm| {
                    arm.body = lower_value(arm.body, current_ns, aliases, registry);
                    arm
                })
                .collect();
            Value::Match(m)
        }

        Value::MatchArm(mut arm) => {
            arm.body = lower_value(arm.body, current_ns, aliases, registry);
            Value::MatchArm(arm)
        }

        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } => Value::MapWithSpread {
            base_entries: base_entries
                .into_iter()
                .map(|(k, v)| (k, lower_val(v, current_ns, aliases, registry)))
                .collect(),
            spread_entries: spread_entries
                .into_iter()
                .map(|(idx, v)| (idx, lower_value(v, current_ns, aliases, registry)))
                .collect(),
        },

        Value::Val(v, ty) => Value::Val(lower_val(v, current_ns, aliases, registry), ty),

        // Leaf nodes — no children to recurse into.
        Value::Placeholder(_)
        | Value::Ref(_)
        | Value::TypeDef(_)
        | Value::TypeImplementation(_)
        | Value::Unbound(_)
        | Value::VariadicExpansion(_) => value,
    }
}

/// Recurse into Map/Vec literals and `Val::Box(AstNode(Value))` so that
/// FnCalls embedded inside literals (e.g. `map(items, [filter(xs, eq(%, k)), 1])`)
/// also get their placeholders wrapped at the inner HOF boundary.
fn lower_val(
    val: Val,
    current_ns: &NsPath,
    aliases: &NamespaceAliases,
    registry: &SignatureRegistry,
) -> Val {
    match val {
        Val::Box(b) => {
            if b.as_any().downcast_ref::<AstNode>().is_some() {
                let ast = b.into_any().downcast::<AstNode>().unwrap();
                Val::Box(Box::new(AstNode(lower_value(
                    ast.0, current_ns, aliases, registry,
                ))))
            } else {
                Val::Box(b)
            }
        }
        Val::Map(m) => Val::Map(Box::new(
            m.into_iter()
                .map(|(k, v)| (k, lower_val(v, current_ns, aliases, registry)))
                .collect(),
        )),
        Val::Vec(v) => Val::Vec(
            v.into_iter()
                .map(|x| lower_val(x, current_ns, aliases, registry))
                .collect(),
        ),
        other => other,
    }
}

/// Walk a (lowered) AST and emit a [`CompilerError`] for every leftover
/// `Value::Placeholder` node. The presence of one means the user wrote `%`
/// somewhere with no enclosing `Fn`-typed parameter slot to bind to.
fn collect_placeholder_errors(
    value: &Value,
    enclosing_var: &str,
    ns_path: &NsPath,
    source_file: Option<&PathBuf>,
    errors: &mut Vec<CompilerError>,
) {
    match value {
        Value::Placeholder(n) => {
            errors.push(make_placeholder_error(
                *n,
                enclosing_var,
                ns_path,
                source_file,
                None,
            ));
        }
        Value::Ref(Ref::Var(vr)) => {
            // Bare `%`/`%N` parsed inside a deep_path may leave a placeholder
            // marker on the var name; treat it the same way.
            let name = vr.var.sym.name();
            if name.starts_with("__placeholder_") {
                let n = name
                    .strip_prefix("__placeholder_")
                    .and_then(|s| s.strip_suffix("__"))
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                errors.push(make_placeholder_error(
                    n,
                    enclosing_var,
                    ns_path,
                    source_file,
                    vr.src.as_ref(),
                ));
            }
        }
        Value::FnCall(call) => {
            collect_placeholder_errors(&call.function, enclosing_var, ns_path, source_file, errors);
            for arg in &call.args {
                collect_placeholder_errors(&arg.value, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::Flow(flow) => {
            for e in &flow.expressions {
                collect_placeholder_errors(e, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::Fn(fn_defs) => {
            for fn_def in fn_defs {
                collect_placeholder_errors(
                    &fn_def.body,
                    enclosing_var,
                    ns_path,
                    source_file,
                    errors,
                );
            }
        }
        Value::Lambda(lambda) => {
            collect_placeholder_errors(&lambda.body, enclosing_var, ns_path, source_file, errors);
        }
        Value::Cond(_, cond, flow) => {
            collect_placeholder_errors(cond, enclosing_var, ns_path, source_file, errors);
            for e in &flow.expressions {
                collect_placeholder_errors(e, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::CondDefault(flow) => {
            for e in &flow.expressions {
                collect_placeholder_errors(e, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::TemplateLiteral(tl) => {
            for p in &tl.parts {
                if let TemplatePart::Expression(e) = p {
                    collect_placeholder_errors(e, enclosing_var, ns_path, source_file, errors);
                }
            }
        }
        Value::Raw(inner) | Value::Do(inner) => {
            collect_placeholder_errors(inner, enclosing_var, ns_path, source_file, errors);
        }
        Value::MultipleValues(vals) => {
            for v in vals {
                collect_placeholder_errors(v, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::Match(m) => {
            collect_placeholder_errors(&m.value, enclosing_var, ns_path, source_file, errors);
            for arm in &m.arms {
                collect_placeholder_errors(&arm.body, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::MatchArm(arm) => {
            collect_placeholder_errors(&arm.body, enclosing_var, ns_path, source_file, errors);
        }
        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } => {
            for v in base_entries.values() {
                collect_val_placeholder_errors(v, enclosing_var, ns_path, source_file, errors);
            }
            for (_, v) in spread_entries {
                collect_placeholder_errors(v, enclosing_var, ns_path, source_file, errors);
            }
        }
        Value::Val(v, _) => {
            collect_val_placeholder_errors(v, enclosing_var, ns_path, source_file, errors);
        }
        Value::Ref(Ref::Ns(_))
        | Value::TypeDef(_)
        | Value::TypeImplementation(_)
        | Value::Unbound(_)
        | Value::VariadicExpansion(_) => {}
    }
}

fn collect_val_placeholder_errors(
    val: &Val,
    enclosing_var: &str,
    ns_path: &NsPath,
    source_file: Option<&PathBuf>,
    errors: &mut Vec<CompilerError>,
) {
    match val {
        Val::Box(b) => {
            if let Some(ast) = b.as_any().downcast_ref::<AstNode>() {
                collect_placeholder_errors(&ast.0, enclosing_var, ns_path, source_file, errors);
            }
        }
        Val::Map(m) => {
            for v in m.values() {
                collect_val_placeholder_errors(v, enclosing_var, ns_path, source_file, errors);
            }
        }
        Val::Vec(v) => {
            for x in v {
                collect_val_placeholder_errors(x, enclosing_var, ns_path, source_file, errors);
            }
        }
        _ => {}
    }
}

fn make_placeholder_error(
    n: usize,
    enclosing_var: &str,
    ns_path: &NsPath,
    source_file: Option<&PathBuf>,
    src: Option<&Source>,
) -> CompilerError {
    let token = if n == 1 {
        "%".to_string()
    } else {
        format!("%{}", n)
    };
    let location = src.map(|s| ErrorLocation {
        line: s.line,
        column: s.column,
        position: s.position,
        length: s.length,
        file: source_file.cloned(),
    });
    let message = format!(
        "Placeholder `{}` in `{}` (in `{}`) has no enclosing parameter \
         slot of type `Fn` to bind to. Wrap the expression with `%(expr)` \
         to construct an explicit lambda value, or move the `%` inside \
         a higher-order function call.",
        token, enclosing_var, ns_path
    );
    CompilerError::CallLibError {
        func_name: token,
        message,
        location,
    }
}
