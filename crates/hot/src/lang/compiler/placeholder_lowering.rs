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
use crate::lang::compiler::named_args::{RecordTypes, into_map_call};
use crate::lang::errors::{CompilerError, CompilerErrors, CompilerResult, ErrorLocation};
use crate::lang::parser::{contains_placeholder, count_placeholder_lambdas, wrap_placeholder_arg};
use crate::val::Val;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::path::PathBuf;

/// Run the placeholder lowering pass over the entire program.
///
/// On success, every implicit `%` placeholder has been wrapped into a lambda
/// at the appropriate `Fn`-typed parameter slot. On failure, the returned
/// [`CompilerErrors`] describes each `%` that did not land in a `Fn` slot
/// (with source location and a `%(expr)` migration hint), and each implicit
/// lambda that would contain another implicit lambda.
pub fn lower_program(program: &mut Program) -> CompilerResult<()> {
    let registry = SignatureRegistry::build(program);
    let mut errors: Vec<CompilerError> = Vec::new();

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
            for site in registry.nested.borrow_mut().drain(..) {
                errors.push(site.into_error(&var, namespace.source_file.as_ref()));
            }
            for site in registry.records.sites.borrow_mut().drain(..) {
                errors.push(CompilerError::NamedArgument {
                    callee: site.callee,
                    message: site.message,
                    location: site
                        .src
                        .map(|s| error_location(&s, &var, namespace.source_file.as_ref())),
                });
            }
            registry.implicit.borrow_mut().clear();
        }
    }

    // `where` predicates are lambdas now; make each an ordinary function.
    crate::lang::compiler::refinements::lower_predicates(program, &mut errors);

    // After lowering, scan for any leftover bare placeholders. Each one is a
    // user error: a `%` that did not land in a `Fn`-typed parameter slot.
    for (ns_path, namespace) in &program.namespaces {
        for (var, value) in &namespace.scope.vars {
            let binding = ScannedBinding {
                var,
                ns_path,
                namespace_file: namespace.source_file.as_ref(),
            };
            collect_placeholder_errors(value, &binding, var.src.as_ref(), &mut errors);
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
    /// Nested implicit lambdas found while lowering the current variable.
    /// Drained by [`lower_program`], which knows the enclosing binding.
    nested: RefCell<Vec<NestedPlaceholderSite>>,
    /// Implicit lambdas made while lowering the current variable, in the
    /// order they were made, so a nested one can name the inner lambda.
    implicit: RefCell<Vec<ImplicitLambdaSite>>,
    /// Record types, for calls with named arguments.
    records: RecordTypes,
}

/// An argument that became an implicit `%` lambda.
#[derive(Clone)]
struct ImplicitLambdaSite {
    /// The call the argument belongs to.
    function: String,
    src: Option<Source>,
}

/// A call whose `Fn` argument would become an implicit `%` lambda that
/// contains another implicit `%` lambda.
struct NestedPlaceholderSite {
    function: String,
    parameter: Option<String>,
    src: Option<Source>,
    /// The implicit lambda inside the argument.
    inner: Option<ImplicitLambdaSite>,
}

impl NestedPlaceholderSite {
    /// `var` is the top-level binding the call was found in.
    fn into_error(self, var: &Var, namespace_file: Option<&PathBuf>) -> CompilerError {
        let (inner_function, inner_src) = match self.inner {
            Some(inner) => (Some(inner.function), inner.src),
            None => (None, None),
        };
        CompilerError::NestedPlaceholder {
            function: self.function,
            parameter: self.parameter,
            location: self.src.map(|s| error_location(&s, var, namespace_file)),
            inner_function,
            inner_location: inner_src.map(|s| error_location(&s, var, namespace_file)),
        }
    }
}

/// Location of a diagnostic at `src` inside the top-level binding `var`.
///
/// The file comes from the spans, not the namespace: `Namespace::source_file`
/// names only the first file merged into that namespace. That is wrong for a
/// namespace split across files, and for the root namespace `::`, which holds
/// every file without an `ns` declaration but names the first file loaded (a
/// hot-std module).
fn error_location(src: &Source, var: &Var, namespace_file: Option<&PathBuf>) -> ErrorLocation {
    ErrorLocation {
        line: src.line,
        column: src.column,
        position: src.position,
        length: src.length,
        file: src
            .file
            .as_ref()
            .or_else(|| var.src.as_ref().and_then(|s| s.file.as_ref()))
            .map(PathBuf::from)
            .or_else(|| namespace_file.cloned()),
    }
}

/// Display name of a callee for diagnostics: `map`, `::hot::coll/map`.
fn callee_name(function: &Value) -> String {
    match function {
        Value::Ref(Ref::Var(vr)) => vr.var.sym.name().to_string(),
        Value::Ref(Ref::Ns(ns_ref)) => match &ns_ref.function_name {
            Some(name) => format!("{}/{}", ns_ref.ns, name),
            None => ns_ref.ns.to_string(),
        },
        _ => "this call".to_string(),
    }
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
            nested: RefCell::new(Vec::new()),
            implicit: RefCell::new(Vec::new()),
            records: RecordTypes::build(program),
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

/// Lower a cond arm's body as the block it is.
fn lower_arm_flow(
    flow: crate::lang::ast::Flow,
    current_ns: &NsPath,
    aliases: &NamespaceAliases,
    registry: &SignatureRegistry,
) -> crate::lang::ast::Flow {
    match lower_value(Value::Flow(flow), current_ns, aliases, registry) {
        Value::Flow(flow) => flow,
        _ => unreachable!("lowering a flow returns a flow"),
    }
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
            // A named call (`User(name: "Ada")`) checks its names against the
            // callee's fields. Each argument's field is its parameter slot,
            // and the call becomes its Map form once its arguments are lowered.
            let named_fields = if call.args.iter().any(|arg| arg.name.is_some()) {
                let piped = arg_index_offset > 0;
                match registry
                    .records
                    .named_fields(&call, piped, &current_ns.to_string())
                {
                    Ok(fields) => Some(fields),
                    Err(site) => {
                        registry.records.sites.borrow_mut().push(site);
                        None
                    }
                }
            } else {
                None
            };
            // Effective arity for signature lookup includes the piped LHS, if any.
            let effective_arity = call.args.len() + arg_index_offset;
            // (type annotation, parameter name) per declared parameter.
            let signature_args: Vec<(Option<String>, String)> = match &named_fields {
                Some(fields) => fields.clone(),
                None => registry
                    .lookup(&call.function, effective_arity, current_ns, aliases)
                    .map(|args| {
                        args.args
                            .iter()
                            .map(|a| (a.type_annotation.clone(), a.var.sym.name().to_string()))
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        // A `where` predicate's wrapper takes a function even when
                        // hot-std isn't loaded to declare it.
                        if crate::lang::compiler::refinements::is_predicate_call(&call.function) {
                            vec![(Some("Fn".to_string()), "f".to_string())]
                        } else {
                            Vec::new()
                        }
                    }),
            };

            call.function = Box::new(lower_value(*call.function, current_ns, aliases, registry));
            let function_name = callee_name(&call.function);
            let call_src = call.src.clone();

            let new_args: Vec<FnCallArg> = call
                .args
                .into_iter()
                .enumerate()
                .map(|(i, mut arg)| {
                    let param_index = i + arg_index_offset;
                    let implicit_before = registry.implicit.borrow().len();
                    let is_fn_slot = signature_args
                        .get(param_index)
                        .and_then(|(annotation, _)| annotation.as_deref())
                        .map(is_fn_typed)
                        .unwrap_or(false);
                    // Placeholder lambdas already present are explicit `%(...)`
                    // boundaries. Any extra one after lowering was created
                    // implicitly inside this argument.
                    let lambdas_before = if is_fn_slot {
                        count_placeholder_lambdas(&arg.value)
                    } else {
                        0
                    };
                    // Inner expressions of an arg are not "outermost" — they
                    // carry no piped LHS, so reset the offset to 0.
                    let lowered = lower_value(arg.value, current_ns, aliases, registry);
                    arg.value = if is_fn_slot && contains_placeholder(&lowered) {
                        // Wrapping here would put an implicit lambda around
                        // another one: two `%` meaning different parameters.
                        if !matches!(lowered, Value::Placeholder(_))
                            && count_placeholder_lambdas(&lowered) > lambdas_before
                        {
                            let inner = registry.implicit.borrow().get(implicit_before).cloned();
                            registry.nested.borrow_mut().push(NestedPlaceholderSite {
                                function: function_name.clone(),
                                parameter: signature_args
                                    .get(param_index)
                                    .map(|(_, name)| name.clone()),
                                src: call_src.clone(),
                                inner,
                            });
                        }
                        if !matches!(lowered, Value::Placeholder(_)) {
                            registry.implicit.borrow_mut().push(ImplicitLambdaSite {
                                function: function_name.clone(),
                                src: arg.src.clone(),
                            });
                        }
                        wrap_placeholder_arg(lowered)
                    } else {
                        lowered
                    };
                    arg
                })
                .collect();
            call.args = new_args;
            if named_fields.is_some() {
                return Value::FnCall(into_map_call(call));
            }
            Value::FnCall(call)
        }

        Value::Flow(mut flow) => {
            let is_pipe = matches!(flow.flow_type, FlowType::Pipe);
            // Types and aliases the block declares can be named in its calls.
            let pushed = registry
                .records
                .enter_block(&flow.expressions, &flow.aliases);
            let mark = registry.records.shadow([]);
            // A binding is one `MultipleValues([name, value])` expression, or
            // (in a braced block) a bare `name` followed by its value, as the
            // compiler pairs them. `pair_names[i]` names the binding whose
            // value is expression `i` in the second form.
            let mut pair_names: Vec<Option<String>> = vec![None; flow.expressions.len()];
            if !is_pipe {
                let mut i = 0;
                while i < flow.expressions.len() {
                    match &flow.expressions[i] {
                        Value::Ref(Ref::Var(name)) if i + 1 < flow.expressions.len() => {
                            pair_names[i + 1] = Some(name.var.sym.name().to_string());
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
            }
            flow.expressions = flow
                .expressions
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    // In a pipe flow, every stage after the first has an
                    // implicit leading arg (the previous result). Account
                    // for that when looking up the callee signature.
                    let offset = if is_pipe && i > 0 { 1 } else { 0 };
                    let lowered = lower_value_inner(e, current_ns, aliases, registry, offset);
                    // A local shadows a type of its name from the next
                    // expression on. Functions and types a body defines don't,
                    // and a local import names the type it is bound to.
                    let binding = match (&lowered, &pair_names[i]) {
                        (Value::MultipleValues(binding), _) => match binding.as_slice() {
                            [Value::Ref(Ref::Var(name)), value] => {
                                Some((name.var.sym.name().to_string(), value))
                            }
                            _ => None,
                        },
                        (value, Some(name)) => Some((name.clone(), value)),
                        _ => None,
                    };
                    if let Some((name, value)) = binding {
                        match value {
                            Value::Fn(_) | Value::TypeDef(_) | Value::TypeImplementation(_) => {}
                            Value::Ref(Ref::Ns(NsRef {
                                ns,
                                function_name: Some(target),
                                ..
                            })) => registry
                                .records
                                .import_locally(name, format!("{ns}/{target}")),
                            _ => {
                                registry.records.shadow([name]);
                            }
                        }
                    }
                    lowered
                })
                .collect();
            registry.records.unshadow(mark);
            registry.records.leave_block(pushed);
            Value::Flow(flow)
        }

        Value::Fn(fn_defs) => {
            // Bodies of nested function definitions are independent units —
            // recurse into each. Aliases come from the enclosing namespace
            // (Hot doesn't currently scope aliases per nested-fn).
            let new_defs = fn_defs
                .into_iter()
                .map(|mut fn_def| {
                    // Lazy parameters hold thunks, which don't shadow.
                    let mark = registry.records.shadow(
                        fn_def
                            .args
                            .args
                            .iter()
                            .filter(|arg| !arg.lazy)
                            .map(|arg| arg.var.sym.name().to_string()),
                    );
                    fn_def.body = lower_value(fn_def.body, current_ns, aliases, registry);
                    registry.records.unshadow(mark);
                    fn_def
                })
                .collect();
            Value::Fn(new_defs)
        }

        Value::Lambda(mut lambda) => {
            let mark = registry.records.shadow(
                lambda
                    .args
                    .args
                    .iter()
                    .map(|arg| arg.var.sym.name().to_string()),
            );
            lambda.body = Box::new(lower_value(*lambda.body, current_ns, aliases, registry));
            registry.records.unshadow(mark);
            Value::Lambda(lambda)
        }

        // An arm's body is a block: lower it as one, so its local imports
        // and shadows apply to its later expressions.
        Value::Cond(label, cond, flow) => {
            let cond = lower_value(*cond, current_ns, aliases, registry);
            Value::Cond(
                label,
                Box::new(cond),
                lower_arm_flow(flow, current_ns, aliases, registry),
            )
        }

        Value::CondDefault(flow) => {
            Value::CondDefault(lower_arm_flow(flow, current_ns, aliases, registry))
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

/// The top-level binding [`collect_placeholder_errors`] is scanning.
struct ScannedBinding<'a> {
    var: &'a Var,
    ns_path: &'a NsPath,
    namespace_file: Option<&'a PathBuf>,
}

/// Walk a (lowered) AST and emit a [`CompilerError`] for every leftover
/// `Value::Placeholder` node. The presence of one means the user wrote `%`
/// somewhere with no enclosing `Fn`-typed parameter slot to bind to.
///
/// A bare placeholder has no span of its own, so `near` carries the innermost
/// enclosing one: the call argument it sits in, else the binding's name.
fn collect_placeholder_errors(
    value: &Value,
    binding: &ScannedBinding,
    near: Option<&Source>,
    errors: &mut Vec<CompilerError>,
) {
    match value {
        Value::Placeholder(n) => {
            errors.push(make_placeholder_error(*n, binding, near));
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
                errors.push(make_placeholder_error(n, binding, vr.src.as_ref().or(near)));
            }
        }
        Value::FnCall(call) => {
            let near = call.src.as_ref().or(near);
            collect_placeholder_errors(&call.function, binding, near, errors);
            for arg in &call.args {
                collect_placeholder_errors(&arg.value, binding, arg.src.as_ref().or(near), errors);
            }
        }
        Value::Flow(flow) => {
            for e in &flow.expressions {
                collect_placeholder_errors(e, binding, near, errors);
            }
        }
        Value::Fn(fn_defs) => {
            for fn_def in fn_defs {
                collect_placeholder_errors(&fn_def.body, binding, near, errors);
            }
        }
        Value::Lambda(lambda) => {
            collect_placeholder_errors(&lambda.body, binding, near, errors);
        }
        Value::Cond(_, cond, flow) => {
            collect_placeholder_errors(cond, binding, near, errors);
            for e in &flow.expressions {
                collect_placeholder_errors(e, binding, near, errors);
            }
        }
        Value::CondDefault(flow) => {
            for e in &flow.expressions {
                collect_placeholder_errors(e, binding, near, errors);
            }
        }
        Value::TemplateLiteral(tl) => {
            for p in &tl.parts {
                if let TemplatePart::Expression(e) = p {
                    collect_placeholder_errors(e, binding, near, errors);
                }
            }
        }
        Value::Raw(inner) | Value::Do(inner) => {
            collect_placeholder_errors(inner, binding, near, errors);
        }
        Value::MultipleValues(vals) => {
            for v in vals {
                collect_placeholder_errors(v, binding, near, errors);
            }
        }
        Value::Match(m) => {
            collect_placeholder_errors(&m.value, binding, near, errors);
            for arm in &m.arms {
                collect_placeholder_errors(&arm.body, binding, near, errors);
            }
        }
        Value::MatchArm(arm) => {
            collect_placeholder_errors(&arm.body, binding, near, errors);
        }
        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } => {
            for v in base_entries.values() {
                collect_val_placeholder_errors(v, binding, near, errors);
            }
            for (_, v) in spread_entries {
                collect_placeholder_errors(v, binding, near, errors);
            }
        }
        Value::Val(v, _) => {
            collect_val_placeholder_errors(v, binding, near, errors);
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
    binding: &ScannedBinding,
    near: Option<&Source>,
    errors: &mut Vec<CompilerError>,
) {
    match val {
        Val::Box(b) => {
            if let Some(ast) = b.as_any().downcast_ref::<AstNode>() {
                collect_placeholder_errors(&ast.0, binding, near, errors);
            }
        }
        Val::Map(m) => {
            for v in m.values() {
                collect_val_placeholder_errors(v, binding, near, errors);
            }
        }
        Val::Vec(v) => {
            for x in v {
                collect_val_placeholder_errors(x, binding, near, errors);
            }
        }
        _ => {}
    }
}

fn make_placeholder_error(
    n: usize,
    binding: &ScannedBinding,
    src: Option<&Source>,
) -> CompilerError {
    let token = if n == 1 {
        "%".to_string()
    } else {
        format!("%{}", n)
    };
    let location = src.map(|s| error_location(s, binding.var, binding.namespace_file));
    let message = format!(
        "Placeholder `{}` in `{}` (in `{}`) has no enclosing parameter \
         slot of type `Fn` to bind to. Wrap the expression with `%(expr)` \
         to construct an explicit lambda value, or move the `%` inside \
         a higher-order function call.",
        token,
        binding.var.sym.name(),
        binding.ns_path
    );
    CompilerError::CallLibError {
        func_name: token,
        message,
        location,
    }
}
