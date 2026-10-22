//! Static checks for `where` constraints. They run on every compile, so
//! `hot check`, the LSP and `hot run` report the same diagnostics.
//!
//! - `impure-constraint`: a predicate reaches a native that isn't pure.
//! - `constraint-not-bool`: a predicate's result is definitely not a `Bool`.
//! - `unsupported-constraint`: a refined `lazy` parameter, or a precondition
//!   on a signature with `lazy` parameters.
//! - `constraint-violation`: a literal that bounded constant evaluation
//!   proves fails its constraint.
//! - `unknown-literal` (warning): a literal outside an open union's known
//!   values.
//! - `where-changes-representation` (warning): a refined primitive type used
//!   with `is-type` or as a `match` arm. Its values are untagged, so those
//!   checks run the predicate instead of comparing a tag.

use ahash::{AHashMap, AHashSet};

use super::core_registry::CoreVariableRegistry;
use super::refinements::error_location;
use crate::lang::ast::{
    AstNode, FnCall, FnDef, LiteralType, NamespaceAliases, Program, Ref, Refinement, TemplatePart,
    TypeDef, TypeExpr, Value, Var, WHERE_TYPE_PREFIX, is_where_predicate,
};
use crate::lang::errors::{CompilerError, ErrorLocation};
use crate::lang::hot::libmap::is_pure_native;
use crate::lang::parser::parse_type_annotation;
use crate::val::Val;

/// Diagnostics from the constraint checks.
#[derive(Default)]
pub(crate) struct ConstraintDiagnostics {
    pub errors: Vec<CompilerError>,
    pub warnings: Vec<CompilerError>,
}

/// Run every static constraint check on a lowered program.
pub(crate) fn check_constraints(
    program: &Program,
    core: &CoreVariableRegistry,
) -> ConstraintDiagnostics {
    let index = Index::new(program, core);
    let mut out = ConstraintDiagnostics::default();
    index.check_predicates(&mut out);
    index.check_signatures(&mut out);
    index.check_literals(program, &mut out);
    out
}

const PURITY_HELP: &str = "a predicate must be deterministic and free of side effects: no clock, \
     randomness, environment, context, files, stores, HTTP, events, tasks or sockets";

fn diagnostic(
    code: &'static str,
    message: String,
    help: Option<&str>,
    warning: bool,
    location: Option<ErrorLocation>,
) -> CompilerError {
    CompilerError::Constraint {
        code,
        message,
        help: help.map(str::to_string),
        warning,
        location,
    }
}

/// Visit `value` and everything inside it.
fn walk<'v>(value: &'v Value, visit: &mut dyn FnMut(&'v Value)) {
    visit(value);
    match value {
        Value::FnCall(call) => {
            walk(&call.function, visit);
            for arg in &call.args {
                walk(&arg.value, visit);
            }
        }
        Value::Flow(flow) | Value::CondDefault(flow) => {
            for expr in &flow.expressions {
                walk(expr, visit);
            }
        }
        Value::Cond(_, condition, flow) => {
            walk(condition, visit);
            for expr in &flow.expressions {
                walk(expr, visit);
            }
        }
        Value::Lambda(lambda) => walk(&lambda.body, visit),
        Value::Fn(defs) => {
            for def in defs {
                walk(&def.body, visit);
            }
        }
        Value::MultipleValues(values) => {
            for value in values {
                walk(value, visit);
            }
        }
        Value::TemplateLiteral(template) => {
            for part in &template.parts {
                if let TemplatePart::Expression(expr) = part {
                    walk(expr, visit);
                }
            }
        }
        Value::Raw(inner) | Value::Do(inner) => walk(inner, visit),
        Value::Match(m) => {
            walk(&m.value, visit);
            for arm in &m.arms {
                walk(&arm.body, visit);
            }
        }
        Value::MatchArm(arm) => walk(&arm.body, visit),
        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } => {
            for value in base_entries.values() {
                walk_val(value, visit);
            }
            for (_, value) in spread_entries {
                walk(value, visit);
            }
        }
        Value::Val(val, _) => walk_val(val, visit),
        _ => {}
    }
}

fn walk_val<'v>(val: &'v Val, visit: &mut dyn FnMut(&'v Value)) {
    match val {
        Val::Box(boxed) => {
            if let Some(node) = boxed.as_any().downcast_ref::<AstNode>() {
                walk(&node.0, visit);
            }
        }
        Val::Vec(items) => {
            for item in items {
                walk_val(item, visit);
            }
        }
        Val::Map(entries) => {
            for value in entries.values() {
                walk_val(value, visit);
            }
        }
        _ => {}
    }
}

/// A literal value, when `value` is one.
fn literal(value: &Value) -> Option<&Val> {
    match value {
        Value::Val(
            val @ (Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null),
            _,
        ) => Some(val),
        _ => None,
    }
}

fn literal_type_val(literal: &LiteralType) -> Val {
    match literal {
        LiteralType::Str(s) => Val::from(s.clone()),
        LiteralType::Int(n) => Val::Int(*n),
        LiteralType::Dec(d) => d
            .parse::<fastnum::D256>()
            .map(Val::Dec)
            .unwrap_or(Val::Null),
        LiteralType::Bool(b) => Val::Bool(*b),
        LiteralType::Null => Val::Null,
    }
}

/// The known values of a literal-union alias.
fn union_members(type_def: &TypeDef) -> Option<Vec<Val>> {
    match type_def.type_alias.as_ref()? {
        TypeExpr::Literal(literal) => Some(vec![literal_type_val(literal)]),
        TypeExpr::Union(members) => members
            .iter()
            .map(|m| match m {
                TypeExpr::Literal(literal) => Some(literal_type_val(literal)),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn val_text(val: &Val) -> String {
    match val {
        Val::Str(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

/// The program's declarations, indexed for the checks.
struct Index<'a> {
    vars: AHashMap<(String, String), (&'a Var, &'a Value)>,
    aliases: AHashMap<String, &'a NamespaceAliases>,
    core: &'a CoreVariableRegistry,
    /// Every `where` clause, with a description of what it constrains.
    refinements: Vec<&'a Refinement>,
    /// Type declarations by qualified name.
    types: AHashMap<String, &'a TypeDef>,
}

impl<'a> Index<'a> {
    fn new(program: &'a Program, core: &'a CoreVariableRegistry) -> Self {
        let mut index = Index {
            vars: AHashMap::new(),
            aliases: AHashMap::new(),
            core,
            refinements: Vec::new(),
            types: AHashMap::new(),
        };
        for (ns_path, namespace) in &program.namespaces {
            let ns = ns_path.to_string();
            index.aliases.insert(ns.clone(), &namespace.aliases);
            for (var, value) in &namespace.scope.vars {
                let name = var.sym.name().to_string();
                match value {
                    Value::TypeDef(type_def) => {
                        index.types.insert(format!("{ns}/{name}"), type_def);
                        if let Some(refinement) = &type_def.refinement {
                            index.refinements.push(refinement);
                        }
                    }
                    Value::Fn(defs) => {
                        for def in defs {
                            if let Some(refinement) = &def.precondition {
                                index.refinements.push(refinement);
                            }
                        }
                    }
                    _ => {}
                }
                index.vars.insert((ns.clone(), name), (var, value));
            }
        }
        index
    }

    /// The namespace and name a call's function refers to.
    fn resolve(&self, ns: &str, function: &Value) -> Option<(String, String)> {
        match function {
            Value::Ref(Ref::Var(var_ref)) => {
                let name = var_ref.var.sym.name();
                if self.vars.contains_key(&(ns.to_string(), name.to_string())) {
                    return Some((ns.to_string(), name.to_string()));
                }
                let info = self.core.get(name)?;
                Some((info.namespace_path.clone(), name.to_string()))
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                let name = ns_ref.function_name.clone()?;
                let target = self
                    .aliases
                    .get(ns)
                    .and_then(|aliases| aliases.get(&ns_ref.ns))
                    .cloned()
                    .unwrap_or_else(|| ns_ref.ns.clone());
                Some((target.to_string(), name))
            }
            _ => None,
        }
    }

    /// The core function a call names, when it isn't shadowed locally.
    fn core_call<'c>(&self, ns: &str, call: &'c FnCall) -> Option<&'c str> {
        let Value::Ref(Ref::Var(var_ref)) = call.function.as_ref() else {
            return None;
        };
        let name = var_ref.var.sym.name();
        if self.vars.contains_key(&(ns.to_string(), name.to_string())) {
            return None;
        }
        self.core.get(name).map(|_| name)
    }

    /// The native a `call-lib(::ns/fn, [...])` call invokes.
    fn call_lib_native(&self, ns: &str, call: &FnCall) -> Option<String> {
        // `call-lib` is a compiler builtin, not a declared function.
        let Value::Ref(Ref::Var(callee)) = call.function.as_ref() else {
            return None;
        };
        if callee.var.sym.name() != "call-lib" {
            return None;
        }
        let Value::Ref(Ref::Ns(ns_ref)) = &call.args.first()?.value else {
            return None;
        };
        let function = ns_ref.function_name.as_deref()?;
        let target = self
            .aliases
            .get(ns)
            .and_then(|aliases| aliases.get(&ns_ref.ns))
            .cloned()
            .unwrap_or_else(|| ns_ref.ns.clone());
        Some(format!("{target}/{function}"))
    }

    fn function_defs(&self, ns: &str, name: &str) -> Option<&'a [FnDef]> {
        match self.vars.get(&(ns.to_string(), name.to_string())) {
            Some((_, Value::Fn(defs))) => Some(defs.as_slice()),
            _ => None,
        }
    }

    fn predicate_defs<'r>(&self, refinement: &'r Refinement) -> Option<(&'r str, &'a [FnDef])> {
        let (ns, name) = refinement.predicate.rsplit_once('/')?;
        Some((ns, self.function_defs(ns, name)?))
    }

    /// The first impure native `value` reaches, and the functions on the way.
    fn find_impure(
        &self,
        ns: &str,
        value: &Value,
        visited: &mut AHashSet<(String, String)>,
        chain: &mut Vec<String>,
    ) -> Option<String> {
        let mut calls = Vec::new();
        walk(value, &mut |v| {
            if let Value::FnCall(call) = v {
                calls.push(call);
            }
        });
        for call in calls {
            if let Some(native) = self.call_lib_native(ns, call) {
                if !is_pure_native(&native) {
                    return Some(native);
                }
                continue;
            }
            let Some((callee_ns, callee)) = self.resolve(ns, &call.function) else {
                continue;
            };
            if !visited.insert((callee_ns.clone(), callee.clone())) {
                continue;
            }
            if let Some(defs) = self.function_defs(&callee_ns, &callee) {
                chain.push(format!("{callee_ns}/{callee}"));
                for def in defs {
                    if let Some(native) = self.find_impure(&callee_ns, &def.body, visited, chain) {
                        return Some(native);
                    }
                }
                chain.pop();
            }
        }
        None
    }

    /// `impure-constraint` and `constraint-not-bool`.
    fn check_predicates(&self, out: &mut ConstraintDiagnostics) {
        for refinement in &self.refinements {
            let Some((ns, defs)) = self.predicate_defs(refinement) else {
                continue;
            };
            let location = refinement.src.as_ref().map(error_location);
            let mut visited = AHashSet::new();
            let mut chain = Vec::new();
            for def in defs {
                if let Some(native) = self.find_impure(ns, &def.body, &mut visited, &mut chain) {
                    let through = if chain.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " (through {})",
                            chain
                                .iter()
                                .map(|f| format!("`{f}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    out.errors.push(diagnostic(
                        "impure-constraint",
                        format!(
                            "The `where` predicate `{}` calls `{native}`{through}, which isn't allowed in a predicate",
                            refinement.source
                        ),
                        Some(PURITY_HELP),
                        false,
                        location.clone(),
                    ));
                    break;
                }
            }
            if let Some(def) = defs.first()
                && let Some(types) = self.result_types(ns, &def.body, 0)
                && !types.iter().any(|t| t == "Bool" || t == "Any")
            {
                out.errors.push(diagnostic(
                    "constraint-not-bool",
                    format!(
                        "The `where` predicate `{}` returns {}, not a Bool",
                        refinement.source,
                        types.join(" | ")
                    ),
                    Some("a predicate must return true or false; truthy values don't pass"),
                    false,
                    location,
                ));
            }
        }
    }

    /// The types an expression can return, or `None` when unknown.
    fn result_types(&self, ns: &str, expr: &Value, depth: usize) -> Option<Vec<String>> {
        if depth > 8 {
            return None;
        }
        let union = |lists: Vec<Option<Vec<String>>>| -> Option<Vec<String>> {
            let mut out: Vec<String> = Vec::new();
            for list in lists {
                for t in list? {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            Some(out)
        };
        match expr {
            Value::Val(val, _) => Some(vec![
                match val {
                    Val::Bool(_) => "Bool",
                    Val::Int(_) => "Int",
                    Val::Dec(_) => "Dec",
                    Val::Str(_) => "Str",
                    Val::Vec(_) => "Vec",
                    Val::Map(_) => "Map",
                    Val::Null => "Null",
                    _ => return None,
                }
                .to_string(),
            ]),
            Value::Flow(flow) => self.result_types(ns, flow.expressions.last()?, depth + 1),
            Value::FnCall(call) => match self.core_call(ns, call) {
                Some("not") => Some(vec!["Bool".to_string()]),
                Some("and" | "or") => union(
                    call.args
                        .iter()
                        .map(|a| self.result_types(ns, &a.value, depth + 1))
                        .collect(),
                ),
                Some("if") => {
                    let mut branches: Vec<Option<Vec<String>>> = call
                        .args
                        .iter()
                        .skip(1)
                        .map(|a| self.result_types(ns, &a.value, depth + 1))
                        .collect();
                    if call.args.len() == 2 {
                        branches.push(Some(vec!["Null".to_string()]));
                    }
                    union(branches)
                }
                _ => {
                    let (callee_ns, callee) = self.resolve(ns, &call.function)?;
                    let defs = self.function_defs(&callee_ns, &callee)?;
                    let mut types: Vec<String> = Vec::new();
                    for def in defs {
                        let declared = def.return_type.as_deref()?;
                        if !matches!(
                            declared,
                            "Bool" | "Int" | "Dec" | "Str" | "Vec" | "Map" | "Null"
                        ) {
                            return None;
                        }
                        if !types.iter().any(|t| t == declared) {
                            types.push(declared.to_string());
                        }
                    }
                    (!types.is_empty()).then_some(types)
                }
            },
            _ => None,
        }
    }

    /// The type declaration a type name refers to.
    fn type_def(&self, ns: &str, name: &str) -> Option<&'a TypeDef> {
        if let Some(type_def) = self.types.get(name) {
            return Some(type_def);
        }
        if let Some(type_def) = self.types.get(&format!("{ns}/{name}")) {
            return Some(type_def);
        }
        let info = self.core.get(name)?;
        self.types
            .get(&format!("{}/{}", info.namespace_path, name))
            .copied()
    }

    /// Whether an annotation names a type with a `where` clause.
    fn annotation_is_refined(&self, ns: &str, annotation: &str) -> bool {
        if annotation.contains(WHERE_TYPE_PREFIX) {
            return true;
        }
        let Some(type_expr) = parse_type_annotation(annotation) else {
            return false;
        };
        let mut names = Vec::new();
        collect_names(&type_expr, &mut names);
        names.iter().any(|name| {
            self.type_def(ns, name)
                .is_some_and(|t| t.refinement.is_some())
        })
    }

    /// `unsupported-constraint`.
    fn check_signatures(&self, out: &mut ConstraintDiagnostics) {
        for ((ns, name), (var, value)) in &self.vars {
            let Value::Fn(defs) = value else {
                continue;
            };
            if is_where_predicate(name) {
                continue;
            }
            for def in defs {
                self.check_captured_parameters(ns, name, def, out);
                for arg in def.args.args.iter().filter(|a| a.lazy) {
                    if arg
                        .type_annotation
                        .as_deref()
                        .is_some_and(|a| self.annotation_is_refined(ns, a))
                    {
                        out.errors.push(diagnostic(
                            "unsupported-constraint",
                            format!(
                                "Parameter `{}` of `{}` is lazy, so its `where` constraint can't be checked when `{}` is called",
                                arg.var.sym.name(),
                                name,
                                name
                            ),
                            Some("drop `lazy`, or check the value after forcing it"),
                            false,
                            arg.var.src.as_ref().or(var.src.as_ref()).map(error_location),
                        ));
                    }
                }
                if let Some(precondition) = &def.precondition
                    && def.args.args.iter().any(|a| a.lazy)
                {
                    out.errors.push(diagnostic(
                        "unsupported-constraint",
                        format!(
                            "`{name}` has lazy parameters, so its argument-list precondition can't be checked"
                        ),
                        Some("drop `lazy`, or check the arguments in the body"),
                        false,
                        precondition.src.as_ref().map(error_location),
                    ));
                }
            }
        }
    }

    /// A parameter's predicate sees only its own value, so naming another
    /// parameter is an error that points to argument-list preconditions.
    fn check_captured_parameters(
        &self,
        ns: &str,
        function: &str,
        def: &FnDef,
        out: &mut ConstraintDiagnostics,
    ) {
        let params: AHashSet<&str> = def.args.args.iter().map(|a| a.var.sym.name()).collect();
        for arg in &def.args.args {
            let Some(annotation) = arg.type_annotation.as_deref() else {
                continue;
            };
            if !annotation.starts_with(WHERE_TYPE_PREFIX) {
                continue;
            }
            let Some(refinement) = self
                .type_def(ns, annotation)
                .and_then(|t| t.refinement.as_ref())
            else {
                continue;
            };
            let Some((_, defs)) = self.predicate_defs(refinement) else {
                continue;
            };
            let Some(predicate) = defs.first() else {
                continue;
            };
            let own = predicate.args.args.first().map(|a| a.var.sym.name());
            let mut captured = None;
            walk(&predicate.body, &mut |v| {
                if let Value::Ref(Ref::Var(var_ref)) = v {
                    let name = var_ref.var.sym.name();
                    if captured.is_none()
                        && Some(name) != own
                        && name != arg.var.sym.name()
                        && params.contains(name)
                    {
                        captured = Some(name.to_string());
                    }
                }
            });
            if let Some(other) = captured {
                out.errors.push(diagnostic(
                    "invalid-constraint",
                    format!(
                        "The `where` predicate on parameter `{}` of `{function}` uses `{other}`, another parameter. A parameter's predicate sees only its own value",
                        arg.var.sym.name()
                    ),
                    Some(&format!(
                        "relate parameters in an argument-list precondition: `fn (...) where gte(%.{}, %.{other})`",
                        arg.var.sym.name()
                    )),
                    false,
                    refinement.src.as_ref().map(error_location),
                ));
            }
        }
    }

    /// `constraint-violation`, `unknown-literal` and
    /// `where-changes-representation`.
    fn check_literals(&self, program: &Program, out: &mut ConstraintDiagnostics) {
        let mut representation_warned: AHashSet<String> = AHashSet::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                if is_where_predicate(var.sym.name()) {
                    continue;
                }
                // A typed binding with a literal initializer.
                if let (Some(annotation), Some(literal)) =
                    (var.type_annotation.as_deref(), literal(value))
                {
                    let location = var.src.as_ref().map(error_location);
                    self.check_literal_against(&ns, annotation, literal, location, out);
                }
                walk(value, &mut |v| match v {
                    Value::FnCall(call) => {
                        self.check_call(&ns, call, out);
                        self.check_representation_call(&ns, call, &mut representation_warned, out);
                    }
                    Value::Match(m) => {
                        for arm in &m.arms {
                            self.check_representation_arm(
                                &ns,
                                arm.type_name.as_deref(),
                                arm.variant.is_none(),
                                &mut representation_warned,
                                out,
                            );
                        }
                    }
                    Value::MatchArm(arm) => self.check_representation_arm(
                        &ns,
                        arm.type_name.as_deref(),
                        arm.variant.is_none(),
                        &mut representation_warned,
                        out,
                    ),
                    _ => {}
                });
            }
        }
    }

    /// Literal arguments of a call: to a constructor or to refined or
    /// open-union parameters.
    fn check_call(&self, ns: &str, call: &FnCall, out: &mut ConstraintDiagnostics) {
        let Some((callee_ns, callee)) = self.resolve(ns, &call.function) else {
            return;
        };
        let location = call.src.as_ref().map(error_location);
        match self.vars.get(&(callee_ns.clone(), callee.clone())) {
            Some((_, Value::TypeDef(_))) => {
                if let [arg] = call.args.as_slice()
                    && let Some(literal) = literal(&arg.value)
                {
                    let qualified = format!("{callee_ns}/{callee}");
                    let location = arg.src.as_ref().map(error_location).or(location);
                    self.check_literal_against(&callee_ns, &qualified, literal, location, out);
                }
            }
            Some((_, Value::Fn(defs))) => {
                let Some(def) = defs
                    .iter()
                    .find(|d| d.args.args.len() == call.args.len() && !d.args.variadic)
                else {
                    return;
                };
                for (arg, param) in call.args.iter().zip(&def.args.args) {
                    let (Some(literal), Some(annotation)) =
                        (literal(&arg.value), param.type_annotation.as_deref())
                    else {
                        continue;
                    };
                    let location = arg.src.as_ref().map(error_location).or(location.clone());
                    self.check_literal_against(&callee_ns, annotation, literal, location, out);
                }
            }
            _ => {}
        }
    }

    /// Check a literal against the type an annotation names, in `ns`.
    fn check_literal_against(
        &self,
        ns: &str,
        annotation: &str,
        literal: &Val,
        location: Option<ErrorLocation>,
        out: &mut ConstraintDiagnostics,
    ) {
        let Some(TypeExpr::Named(name) | TypeExpr::Simple(name)) =
            parse_type_annotation(annotation)
        else {
            return;
        };
        let Some(type_def) = self.type_def(ns, &name) else {
            return;
        };
        // An open union accepts unknown values, with a warning for a literal.
        if type_def.is_open
            && let Some(members) = union_members(type_def)
        {
            if !members.contains(literal) {
                out.warnings.push(diagnostic(
                    "unknown-literal",
                    format!(
                        "{} isn't one of the known values of `{}`",
                        val_text(literal),
                        name.rsplit('/').next().unwrap_or(&name)
                    ),
                    Some("open unions accept unknown values; check the spelling"),
                    true,
                    location,
                ));
            }
            return;
        }
        let type_ns = if name.starts_with("::") {
            name.rsplit_once('/').map_or(ns, |(n, _)| n).to_string()
        } else {
            ns.to_string()
        };
        if let Some(failed) = self.literal_fails(&type_ns, type_def, literal, 0) {
            let shown = if name.contains(WHERE_TYPE_PREFIX) {
                format!("`{failed}`")
            } else {
                format!("`{}` ({failed})", name.rsplit('/').next().unwrap_or(&name))
            };
            out.errors.push(diagnostic(
                "constraint-violation",
                format!("{} fails {shown}", val_text(literal)),
                None,
                false,
                location,
            ));
        }
    }

    /// The source of the refinement a literal fails, when bounded constant
    /// evaluation can tell.
    fn literal_fails(
        &self,
        ns: &str,
        type_def: &TypeDef,
        literal: &Val,
        depth: usize,
    ) -> Option<String> {
        if depth > 4 {
            return None;
        }
        // An alias of another refinement must satisfy that one too.
        if let Some(TypeExpr::Named(base) | TypeExpr::Simple(base)) = &type_def.type_alias
            && let Some(base_def) = self.type_def(ns, base)
            && let Some(failed) = self.literal_fails(ns, base_def, literal, depth + 1)
        {
            return Some(failed);
        }
        let refinement = type_def.refinement.as_ref()?;
        let (predicate_ns, defs) = self.predicate_defs(refinement)?;
        let def = defs.first()?;
        let parameter = def.args.args.first()?.var.sym.name();
        let mut evaluator = Evaluator {
            index: self,
            ns: predicate_ns,
            parameter,
            candidate: literal,
            fuel: 256,
        };
        match evaluator.eval(&def.body) {
            Some(Val::Bool(false)) => Some(refinement.source.clone()),
            _ => None,
        }
    }

    fn warn_representation(
        &self,
        ns: &str,
        name: &str,
        warned: &mut AHashSet<String>,
        out: &mut ConstraintDiagnostics,
        location: Option<ErrorLocation>,
    ) {
        let Some(type_def) = self.type_def(ns, name) else {
            return;
        };
        let primitive = matches!(type_def.type_alias, Some(TypeExpr::Builtin(_)))
            || matches!(&type_def.type_alias, Some(TypeExpr::Named(n) | TypeExpr::Simple(n)) if matches!(n.as_str(), "Int" | "Dec" | "Str" | "Bool"));
        if !primitive || type_def.refinement.is_none() || !warned.insert(name.to_string()) {
            return;
        }
        out.warnings.push(diagnostic(
            "where-changes-representation",
            format!(
                "`{name}` has a `where` clause, so its values are plain base values without a type tag. `is-type` and `match` on `{name}` run its predicate"
            ),
            None,
            true,
            location,
        ));
    }

    fn check_representation_call(
        &self,
        ns: &str,
        call: &FnCall,
        warned: &mut AHashSet<String>,
        out: &mut ConstraintDiagnostics,
    ) {
        if self.core_call(ns, call) != Some("is-type") {
            return;
        }
        let Some(Value::Ref(Ref::Var(type_ref))) = call.args.get(1).map(|a| &a.value) else {
            return;
        };
        self.warn_representation(
            ns,
            type_ref.var.sym.name(),
            warned,
            out,
            call.src.as_ref().map(error_location),
        );
    }

    fn check_representation_arm(
        &self,
        ns: &str,
        type_name: Option<&str>,
        whole_type: bool,
        warned: &mut AHashSet<String>,
        out: &mut ConstraintDiagnostics,
    ) {
        if let Some(type_name) = type_name
            && whole_type
        {
            self.warn_representation(ns, type_name, warned, out, None);
        }
    }
}

fn collect_names(type_expr: &TypeExpr, names: &mut Vec<String>) {
    match type_expr {
        TypeExpr::Simple(name) | TypeExpr::Named(name) => names.push(name.clone()),
        TypeExpr::Generic(_, params) => {
            for param in params {
                collect_names(param, names);
            }
        }
        TypeExpr::Optional(inner) => collect_names(inner, names),
        TypeExpr::Union(members) => {
            for member in members {
                collect_names(member, names);
            }
        }
        _ => {}
    }
}

/// Bounded constant evaluation of a predicate on a literal candidate. It
/// knows the canonical predicate functions and gives up (`None`) on
/// anything else.
struct Evaluator<'i, 'a> {
    index: &'i Index<'a>,
    ns: &'i str,
    parameter: &'i str,
    candidate: &'i Val,
    fuel: usize,
}

fn as_number(val: &Val) -> Option<fastnum::D256> {
    match val {
        Val::Int(n) => Some(fastnum::D256::from(*n)),
        Val::Dec(d) => Some(*d),
        _ => None,
    }
}

impl Evaluator<'_, '_> {
    fn eval(&mut self, expr: &Value) -> Option<Val> {
        self.fuel = self.fuel.checked_sub(1)?;
        match expr {
            Value::Val(val, _) => match val {
                Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => {
                    Some(val.clone())
                }
                Val::Vec(items) => Some(Val::Vec(
                    items
                        .iter()
                        .map(|item| match item {
                            Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => {
                                Some(item.clone())
                            }
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()?,
                )),
                _ => None,
            },
            Value::Ref(Ref::Var(var_ref)) if var_ref.var.sym.name() == self.parameter => {
                Some(self.candidate.clone())
            }
            Value::Flow(flow) if flow.expressions.len() == 1 => self.eval(&flow.expressions[0]),
            Value::FnCall(call) => self.eval_call(call),
            _ => None,
        }
    }

    fn eval_call(&mut self, call: &FnCall) -> Option<Val> {
        let name = self.index.core_call(self.ns, call)?;
        let arg =
            |this: &mut Self, i: usize| -> Option<Val> { this.eval(&call.args.get(i)?.value) };
        let compare = |this: &mut Self| -> Option<std::cmp::Ordering> {
            let (a, b) = (arg(this, 0)?, arg(this, 1)?);
            as_number(&a)?.partial_cmp(&as_number(&b)?)
        };
        use std::cmp::Ordering::*;
        Some(match (name, call.args.len()) {
            ("gt", 2) => Val::Bool(compare(self)? == Greater),
            ("gte", 2) => Val::Bool(compare(self)? != Less),
            ("lt", 2) => Val::Bool(compare(self)? == Less),
            ("lte", 2) => Val::Bool(compare(self)? != Greater),
            ("eq", 2) => Val::Bool(arg(self, 0)? == arg(self, 1)?),
            ("ne", 2) => Val::Bool(arg(self, 0)? != arg(self, 1)?),
            ("between", 3) => {
                let x = as_number(&arg(self, 0)?)?;
                let low = as_number(&arg(self, 1)?)?;
                let high = as_number(&arg(self, 2)?)?;
                Val::Bool(low <= x && x <= high)
            }
            ("in", 2) => match arg(self, 1)? {
                Val::Vec(items) => Val::Bool(items.contains(&arg(self, 0)?)),
                _ => return None,
            },
            ("not", 1) => match arg(self, 0)? {
                Val::Bool(b) => Val::Bool(!b),
                _ => return None,
            },
            ("and", _) => {
                for i in 0..call.args.len() {
                    match arg(self, i)? {
                        Val::Bool(false) => return Some(Val::Bool(false)),
                        Val::Bool(true) => {}
                        _ => return None,
                    }
                }
                Val::Bool(true)
            }
            ("or", _) => {
                for i in 0..call.args.len() {
                    match arg(self, i)? {
                        Val::Bool(true) => return Some(Val::Bool(true)),
                        Val::Bool(false) => {}
                        _ => return None,
                    }
                }
                Val::Bool(false)
            }
            ("if", 2 | 3) => match arg(self, 0)? {
                Val::Bool(true) => arg(self, 1)?,
                Val::Bool(false) if call.args.len() == 3 => arg(self, 2)?,
                Val::Bool(false) => Val::Null,
                _ => return None,
            },
            ("is-null", 1) => Val::Bool(matches!(arg(self, 0)?, Val::Null)),
            ("length", 1) => match arg(self, 0)? {
                // Hot's `length` of a Str counts bytes.
                Val::Str(s) => Val::Int(s.len() as i64),
                Val::Vec(items) => Val::Int(items.len() as i64),
                _ => return None,
            },
            ("starts-with", 2) | ("ends-with", 2) | ("contains", 2) => {
                let (Val::Str(s), Val::Str(part)) = (arg(self, 0)?, arg(self, 1)?) else {
                    return None;
                };
                Val::Bool(match name {
                    "starts-with" => s.starts_with(part.as_ref()),
                    "ends-with" => s.ends_with(part.as_ref()),
                    _ => s.contains(part.as_ref()),
                })
            }
            _ => return None,
        })
    }
}
