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
use indexmap::IndexMap;

use super::core_registry::CoreVariableRegistry;
use super::refinements::error_location;
use crate::lang::ast::{
    AstNode, DeepPath, Flow, FlowType, FnCall, FnDef, LiteralType, NamespaceAliases, Program, Ref,
    Refinement, TemplatePart, TypeDef, TypeExpr, Value, Var, VarRef, WHERE_TYPE_PREFIX,
    is_where_predicate,
};
use crate::lang::errors::{CompilerError, ErrorLocation};
use crate::lang::natives::is_pure_native;
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
        Value::Val(val, _) => scalar_literal(val),
        _ => None,
    }
}

/// A scalar literal, when a Map literal's entry is one.
fn scalar_literal(val: &Val) -> Option<&Val> {
    match val {
        Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => Some(val),
        Val::Box(boxed) => match boxed.as_any().downcast_ref::<AstNode>() {
            Some(AstNode(value)) => literal(value),
            None => None,
        },
        _ => None,
    }
}

/// The reference and value of a local binding, `name value`: as in the
/// compiler, two values whose first is a reference.
fn local_binding(values: &[Value]) -> Option<(&VarRef, &Value)> {
    match values {
        [Value::Ref(Ref::Var(var_ref)), value] => Some((var_ref, value)),
        _ => None,
    }
}

/// The entries of a Map literal, when `val` is one.
fn map_literal(val: &Val) -> Option<&IndexMap<Val, Val>> {
    match val {
        Val::Map(entries) => Some(entries),
        Val::Box(boxed) => match boxed.as_any().downcast_ref::<AstNode>() {
            Some(AstNode(Value::Val(val, _))) => map_literal(val),
            _ => None,
        },
        _ => None,
    }
}

/// The constructor call whose Map literal is being checked, and the path
/// to the field being checked in it.
struct RecordSite {
    shown: String,
    location: Option<ErrorLocation>,
    path: Vec<String>,
}

impl RecordSite {
    /// Where a field's problem is, for its message: ` at `inner.n` of `Outer``.
    fn at(&self) -> String {
        if self.path.is_empty() {
            String::new()
        } else {
            format!(" at `{}` of `{}`", self.path.join("."), self.shown)
        }
    }
}

/// A diagnostic about a literal, before it has a location.
struct LiteralProblem {
    code: &'static str,
    message: String,
    help: Option<&'static str>,
    warning: bool,
}

impl LiteralProblem {
    fn report(
        self,
        suffix: String,
        location: Option<ErrorLocation>,
        out: &mut ConstraintDiagnostics,
    ) {
        let found = diagnostic(
            self.code,
            format!("{}{suffix}", self.message),
            self.help,
            self.warning,
            location,
        );
        if self.warning {
            out.warnings.push(found);
        } else {
            out.errors.push(found);
        }
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
    /// Members of each literal union by qualified name, accumulated across
    /// every declaration (an open union can be extended).
    literal_members: AHashMap<String, Vec<Val>>,
}

impl<'a> Index<'a> {
    fn new(program: &'a Program, core: &'a CoreVariableRegistry) -> Self {
        let mut index = Index {
            vars: AHashMap::new(),
            aliases: AHashMap::new(),
            core,
            refinements: Vec::new(),
            types: AHashMap::new(),
            literal_members: AHashMap::new(),
        };
        for (ns_path, namespace) in &program.namespaces {
            let ns = ns_path.to_string();
            index.aliases.insert(ns.clone(), &namespace.aliases);
            for (var, value) in &namespace.scope.vars {
                let name = var.sym.name().to_string();
                match value {
                    Value::TypeDef(type_def) => {
                        let qualified = format!("{ns}/{name}");
                        if let Some(members) = union_members(type_def) {
                            let known = index.literal_members.entry(qualified.clone()).or_default();
                            for member in members {
                                if !known.contains(&member) {
                                    known.push(member);
                                }
                            }
                        }
                        index.types.insert(qualified, type_def);
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
        self.type_key(ns, name)
            .and_then(|key| self.types.get(&key).copied())
    }

    /// The qualified name of the type `name` names in `ns`, through the
    /// namespace's aliases (`::alias/Type`) when it uses one.
    fn type_key(&self, ns: &str, name: &str) -> Option<String> {
        if self.types.contains_key(name) {
            return Some(name.to_string());
        }
        if let Some((alias, type_name)) = name
            .strip_prefix("::")
            .and_then(|rest| rest.rsplit_once('/'))
            && let Some(source) = self
                .aliases
                .get(ns)
                .and_then(|aliases| aliases.get(&crate::lang::ast::NsPath::from(alias)))
        {
            let key = format!("{source}/{type_name}");
            if self.types.contains_key(&key) {
                return Some(key);
            }
        }
        let local = format!("{ns}/{name}");
        if self.types.contains_key(&local) {
            return Some(local);
        }
        let info = self.core.get(name)?;
        let core = format!("{}/{}", info.namespace_path, name);
        self.types.contains_key(&core).then_some(core)
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
                    Value::MultipleValues(values) => self.check_local_binding(&ns, values, out),
                    Value::FnCall(call) => {
                        self.check_call(&ns, call, out);
                        self.check_representation_call(&ns, call, &mut representation_warned, out);
                    }
                    Value::Ref(Ref::Var(var_ref)) => {
                        self.check_binding_shadows(&ns, &var_ref.var, out)
                    }
                    Value::Match(m) => {
                        for arm in &m.arms {
                            self.check_representation_arm(
                                &ns,
                                arm,
                                &mut representation_warned,
                                out,
                            );
                        }
                    }
                    Value::MatchArm(arm) => {
                        self.check_representation_arm(&ns, arm, &mut representation_warned, out)
                    }
                    _ => {}
                });
            }
        }
    }

    /// A typed binding with a literal value inside a body, `x: Port 0`. Only
    /// a binding's reference carries a type annotation.
    fn check_local_binding(&self, ns: &str, values: &[Value], out: &mut ConstraintDiagnostics) {
        if let Some((var_ref, value)) = local_binding(values)
            && var_ref.var.deep_set.is_none()
            && var_ref.var.deep_path.is_none()
            && let Some(annotation) = var_ref.var.type_annotation.as_deref()
            && let Some(literal) = literal(value)
        {
            let location = var_ref
                .src
                .as_ref()
                .or(var_ref.var.src.as_ref())
                .map(error_location);
            self.check_literal_against(ns, annotation, literal, location, out);
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
            Some((_, Value::TypeDef(type_def))) => {
                let [arg] = call.args.as_slice() else {
                    return;
                };
                let location = arg.src.as_ref().map(error_location).or(location);
                if let Some(literal) = literal(&arg.value) {
                    let qualified = format!("{callee_ns}/{callee}");
                    self.check_literal_against(&callee_ns, &qualified, literal, location, out);
                } else if let Value::Val(val, _) = &arg.value
                    && let Some(entries) = map_literal(val)
                    && let Some((record_ns, record, shown)) =
                        self.constructed_record(&callee_ns, &callee, &call.function, type_def)
                {
                    let mut site = RecordSite {
                        shown,
                        location,
                        path: Vec::new(),
                    };
                    self.check_record_fields(&record_ns, record, entries, &mut site, out);
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
        if let Some(problem) = self.literal_problem(ns, annotation, literal) {
            problem.report(String::new(), location, out);
        }
    }

    /// What is wrong with a literal for the type an annotation names, in
    /// `ns`, when bounded constant evaluation can tell.
    fn literal_problem(&self, ns: &str, annotation: &str, literal: &Val) -> Option<LiteralProblem> {
        let name = match parse_type_annotation(annotation)? {
            TypeExpr::Named(name) | TypeExpr::Simple(name) => name,
            TypeExpr::Optional(inner) if *literal != Val::Null => match *inner {
                TypeExpr::Named(name) | TypeExpr::Simple(name) => name,
                _ => return None,
            },
            _ => return None,
        };
        let key = self.type_key(ns, &name)?;
        let type_def = self.types.get(&key).copied()?;
        let short_name = name.rsplit('/').next().unwrap_or(&name);
        if let Some(members) = self.literal_members.get(&key) {
            if members.contains(literal) {
                return None;
            }
            // An open union accepts unknown values, with a warning for a
            // literal; a closed one rejects them.
            return Some(if type_def.is_open {
                LiteralProblem {
                    code: "unknown-literal",
                    message: format!(
                        "{} isn't one of the known values of `{short_name}`",
                        val_text(literal)
                    ),
                    help: Some("open unions accept unknown values; check the spelling"),
                    warning: true,
                }
            } else {
                let known = members.iter().map(val_text).collect::<Vec<_>>().join(" | ");
                LiteralProblem {
                    code: "constraint-violation",
                    message: format!(
                        "{} isn't one of the values of `{short_name}` ({known})",
                        val_text(literal)
                    ),
                    help: Some("declare the union `open` to accept other values"),
                    warning: false,
                }
            });
        }
        let type_ns = key.rsplit_once('/').map_or(ns, |(n, _)| n);
        let failed = self.literal_fails(type_ns, type_def, literal, 0)?;
        let shown = if name.contains(WHERE_TYPE_PREFIX) {
            format!("`{failed}`")
        } else {
            format!("`{short_name}` ({failed})")
        };
        Some(LiteralProblem {
            code: "constraint-violation",
            message: format!("{} fails {shown}", val_text(literal)),
            help: None,
            warning: false,
        })
    }

    /// The record a constructor call builds: the type itself, or the payload
    /// of the enum variant it names (`Shape.Circle`). With its namespace and
    /// its name for messages.
    fn constructed_record<'t>(
        &'t self,
        callee_ns: &str,
        callee: &str,
        function: &Value,
        type_def: &'t TypeDef,
    ) -> Option<(String, &'t TypeDef, String)> {
        let variant = match function {
            Value::Ref(Ref::Var(var_ref)) => match &var_ref.var.deep_path {
                None => None,
                Some(DeepPath::Key(variant)) => Some(variant.as_str()),
                Some(_) => return None,
            },
            _ => None,
        };
        let Some(variant) = variant else {
            return Some((callee_ns.to_string(), type_def, callee.to_string()));
        };
        let payload = type_def
            .variants
            .as_ref()?
            .iter()
            .find(|v| v.name.name() == variant)?
            .type_ref
            .as_deref()?;
        let key = self.type_key(callee_ns, payload)?;
        let record = self.types.get(&key).copied()?;
        let record_ns = key.rsplit_once('/')?.0.to_string();
        let shown = payload.rsplit('/').next().unwrap_or(payload).to_string();
        Some((record_ns, record, shown))
    }

    /// Literal fields of a record built from a Map literal, against their
    /// field types, then the record's invariant when the fields it reads are
    /// literals. Nested Map literals are records' fields too.
    fn check_record_fields(
        &self,
        ns: &str,
        record: &TypeDef,
        entries: &IndexMap<Val, Val>,
        site: &mut RecordSite,
        out: &mut ConstraintDiagnostics,
    ) {
        let Some(fields) = record.fields.as_ref() else {
            return;
        };
        // A one-argument custom constructor receives the Map and decides
        // what it means.
        if record
            .constructor_functions
            .as_ref()
            .is_some_and(|ctors| ctors.iter().any(|c| c.args.args.len() == 1))
        {
            return;
        }
        let mut failed = false;
        for field in fields {
            let Some(value) = entries.get(&Val::from(field.name.name())) else {
                continue;
            };
            site.path.push(field.name.name().to_string());
            if let Some(literal) = scalar_literal(value) {
                if let Some(problem) = self.literal_problem(ns, &field.type_annotation, literal) {
                    failed |= !problem.warning;
                    problem.report(site.at(), site.location.clone(), out);
                }
            } else if let Some(inner) = map_literal(value)
                && let Some(TypeExpr::Named(name) | TypeExpr::Simple(name)) =
                    parse_type_annotation(&field.type_annotation)
                && let Some(key) = self.type_key(ns, &name)
                && let Some(inner_record) = self.types.get(&key).copied()
            {
                let inner_ns = key.rsplit_once('/').map_or(ns, |(n, _)| n);
                self.check_record_fields(inner_ns, inner_record, inner, site, out);
            }
            site.path.pop();
        }
        // A field that fails stops the constructor before its invariant.
        if failed || record.refinement.is_none() {
            return;
        }
        let known: IndexMap<Val, Val> = fields
            .iter()
            .filter_map(|field| {
                let key = Val::from(field.name.name());
                let literal = scalar_literal(entries.get(&key)?)?;
                Some((key, literal.clone()))
            })
            .collect();
        let candidate = Val::Map(Box::new(known.clone()));
        if let Some(failed) = self.literal_fails(ns, record, &candidate, 0) {
            let fields = known
                .iter()
                .map(|(k, v)| match k {
                    Val::Str(k) => format!("{k}: {}", val_text(v)),
                    k => format!("{k}: {}", val_text(v)),
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.errors.push(diagnostic(
                "constraint-violation",
                format!(
                    "{{{fields}}} fails `{}` ({failed}){}",
                    record.name.name(),
                    site.at()
                ),
                None,
                false,
                site.location.clone(),
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
            ns: predicate_ns.to_string(),
            locals: AHashMap::from_iter([(parameter.to_string(), literal.clone())]),
            fuel: 256,
            depth: 0,
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
        // One warning per type; one without a location can't be shown, so it
        // doesn't count.
        if !primitive
            || type_def.refinement.is_none()
            || location.is_none()
            || !warned.insert(name.to_string())
        {
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

    /// `binding-shadows-function`: a binding written `name (…)` whose name is
    /// a function. A call never has a space before `(`, so `if (x) {…}` or
    /// `assert (…)` binds a local that hides the function instead of calling it.
    fn check_binding_shadows(&self, ns: &str, var: &Var, out: &mut ConstraintDiagnostics) {
        if !var
            .meta
            .as_ref()
            .is_some_and(crate::lang::ast::is_spaced_paren_binding_meta)
        {
            return;
        }
        let name = var.sym.name();
        let is_function = self.core.get(name).is_some_and(|info| {
            info.variable_type == crate::lang::compiler::core_registry::CoreVariableType::Function
        }) || matches!(
            self.vars.get(&(ns.to_string(), name.to_string())),
            Some((_, Value::Fn(_)))
        );
        if !is_function {
            return;
        }
        let help =
            format!("a call has no space before `(`: write `{name}(…)`, or rename the binding");
        out.warnings.push(diagnostic(
            "binding-shadows-function",
            format!("`{name} (…)` binds a local named `{name}`, which hides the `{name}` function"),
            Some(&help),
            true,
            var.src.as_ref().map(error_location),
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
        arm: &crate::lang::ast::MatchArm,
        warned: &mut AHashSet<String>,
        out: &mut ConstraintDiagnostics,
    ) {
        if let Some(type_name) = arm.type_name.as_deref()
            && arm.variant.is_none()
        {
            let location = arm.src.as_ref().map(error_location);
            self.warn_representation(ns, type_name, warned, out, location);
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
/// knows the canonical predicate functions and follows calls into the
/// program's own functions. It interprets rather than runs, and gives up
/// (`None`) on anything it doesn't know, so it can't report a literal that
/// would pass.
struct Evaluator<'i, 'a> {
    index: &'i Index<'a>,
    /// The namespace of the body being evaluated.
    ns: String,
    /// The parameters and local bindings in scope.
    locals: AHashMap<String, Val>,
    /// Evaluation steps left, shared by every call.
    fuel: usize,
    /// Calls into the program's functions in progress.
    depth: usize,
}

/// How deep the evaluator follows calls into the program's functions.
const MAX_CALL_DEPTH: usize = 8;

fn as_number(val: &Val) -> Option<fastnum::D256> {
    match val {
        Val::Int(n) => Some(fastnum::D256::from(*n)),
        Val::Dec(d) => Some(*d),
        _ => None,
    }
}

/// Whether a value certainly has the plain type an annotation names. Any
/// other annotation is unknown, so evaluation gives up.
fn admits(annotation: Option<&str>, value: &Val) -> bool {
    match annotation {
        None | Some("Any") => true,
        Some("Str") => matches!(value, Val::Str(_)),
        Some("Int") => matches!(value, Val::Int(_)),
        Some("Dec") => matches!(value, Val::Dec(_)),
        Some("Bool") => matches!(value, Val::Bool(_)),
        Some("Null") => matches!(value, Val::Null),
        Some(_) => false,
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
            Value::Ref(Ref::Var(var_ref)) => self.lookup(&var_ref.var),
            Value::Flow(flow) => self.eval_flow(flow),
            Value::FnCall(call) => self.eval_call(call),
            _ => None,
        }
    }

    /// A local, or a top-level literal constant of the namespace. A record
    /// candidate is read only by field (`%.low`), since it holds just the
    /// fields given as literals.
    fn lookup(&self, var: &Var) -> Option<Val> {
        let name = var.sym.name();
        let value = match self.locals.get(name) {
            Some(value) => value.clone(),
            None => match self.index.vars.get(&(self.ns.clone(), name.to_string())) {
                Some((_, value)) => literal(value)?.clone(),
                None => return None,
            },
        };
        match (&var.deep_path, value) {
            (None, Val::Map(_)) => None,
            (None, value) => Some(value),
            (Some(DeepPath::Key(key)), Val::Map(entries)) => {
                scalar_literal(entries.get(&Val::from(key.as_str()))?).cloned()
            }
            _ => None,
        }
    }

    fn eval_flow(&mut self, flow: &Flow) -> Option<Val> {
        if !flow.aliases.is_empty() || flow.result_modifier.is_some() {
            return None;
        }
        match flow.flow_type {
            FlowType::Serial | FlowType::SerialShort => self.eval_serial(&flow.expressions),
            FlowType::Cond | FlowType::CondShort => self.eval_cond(&flow.expressions),
            _ => None,
        }
    }

    /// A body's bindings, in order, then its last expression.
    fn eval_serial(&mut self, expressions: &[Value]) -> Option<Val> {
        let saved = self.locals.clone();
        let result = self.eval_serial_scoped(expressions);
        self.locals = saved;
        result
    }

    fn eval_serial_scoped(&mut self, expressions: &[Value]) -> Option<Val> {
        let (last, bindings) = expressions.split_last()?;
        for binding in bindings {
            let Value::MultipleValues(values) = binding else {
                return None;
            };
            let (var_ref, value) = local_binding(values)?;
            let var = &var_ref.var;
            if var.type_annotation.is_some() || var.deep_set.is_some() || var.deep_path.is_some() {
                return None;
            }
            let value = self.eval(value)?;
            self.locals.insert(var.sym.name().to_string(), value);
        }
        // A body that ends in a binding has that binding's value.
        if matches!(last, Value::MultipleValues(values) if local_binding(values).is_some()) {
            return None;
        }
        self.eval(last)
    }

    /// The first arm whose condition is `true`. A non-Bool condition, or no
    /// arm matching, gives up.
    fn eval_cond(&mut self, arms: &[Value]) -> Option<Val> {
        for arm in arms {
            match arm {
                Value::Cond(_, condition, body) => match self.eval(condition)? {
                    Val::Bool(true) => return self.eval_flow_body(body),
                    Val::Bool(false) => {}
                    _ => return None,
                },
                Value::CondDefault(body) => return self.eval_flow_body(body),
                _ => return None,
            }
        }
        None
    }

    fn eval_flow_body(&mut self, body: &Flow) -> Option<Val> {
        if !body.aliases.is_empty() || body.result_modifier.is_some() {
            return None;
        }
        self.eval_serial(&body.expressions)
    }

    /// A call to one of the program's functions: its one definition for
    /// this many arguments, with plain parameter types and no constraints of
    /// its own, evaluated in its namespace.
    fn eval_user_call(&mut self, call: &FnCall) -> Option<Val> {
        if self.depth >= MAX_CALL_DEPTH
            || call
                .args
                .iter()
                .any(|a| a.lazy || a.spread || a.name.is_some())
        {
            return None;
        }
        let (callee_ns, callee) = self.index.resolve(&self.ns, &call.function)?;
        let defs = self.index.function_defs(&callee_ns, &callee)?;
        let mut matching = defs
            .iter()
            .filter(|d| !d.args.variadic && d.args.args.len() == call.args.len());
        let def = matching.next()?;
        if matching.next().is_some()
            || def.precondition.is_some()
            || def.args.args.iter().any(|a| a.lazy)
        {
            return None;
        }
        let mut locals = AHashMap::new();
        for (arg, param) in call.args.iter().zip(&def.args.args) {
            let value = self.eval(&arg.value)?;
            if !admits(param.type_annotation.as_deref(), &value) {
                return None;
            }
            locals.insert(param.var.sym.name().to_string(), value);
        }
        let saved_locals = std::mem::replace(&mut self.locals, locals);
        let saved_ns = std::mem::replace(&mut self.ns, callee_ns);
        self.depth += 1;
        let result = self.eval(&def.body);
        self.depth -= 1;
        self.locals = saved_locals;
        self.ns = saved_ns;
        let result = result?;
        admits(def.return_type.as_deref(), &result).then_some(result)
    }

    fn eval_call(&mut self, call: &FnCall) -> Option<Val> {
        if call.result_path.is_some() {
            return None;
        }
        // A parameter or local named like a function hides it.
        if let Value::Ref(Ref::Var(callee)) = call.function.as_ref()
            && (callee.var.deep_path.is_some() || self.locals.contains_key(callee.var.sym.name()))
        {
            return None;
        }
        let Some(name) = self.index.core_call(&self.ns, call) else {
            return self.eval_user_call(call);
        };
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
