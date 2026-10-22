//! Constraint specs for the `CheckConstraint` instruction.
//!
//! A spec is a constant map describing what a value must satisfy at a
//! boundary. Its `type` entry is a tree of nodes keyed by `kind`:
//!
//! - `refine`: `base` must pass, then the predicate function `pred` must
//!   return `true`. Carries `constraint`, `source` and `location` for errors.
//! - `base`: the value's runtime type must be `type` (strict; only exact
//!   numeric conversions apply).
//! - `literal`: the value must equal one of `values`.
//! - `vec` / `map`: every element, or every map value, must pass `elem`.
//! - `optional`: `null`, or `inner`.
//! - `union`: at least one of `members`.
//! - `struct`: each constrained field passes its spec, then `invariant`.
//! - `enum`: the whole value passes `invariant`.
//! - `named`: the spec registered for the named type in the program's
//!   `constraint_types` table, which also handles recursive types.
//! - `any`: anything.
//!
//! Only types that contain a refinement or a closed literal type are
//! constrained; every other annotation produces no check at all.

use ahash::AHashSet;

use super::Compiler;
use crate::lang::ast::{
    BuiltinType, LiteralType, Refinement, TypeDef, TypeExpr, WHERE_TYPE_PREFIX,
    is_inline_refinement_type,
};
use crate::lang::bytecode::{Constant, ConstantId, Instruction, RegisterId};
use crate::lang::parser::parse_type_annotation;
use crate::val::Val;

/// Where a check runs, for diagnostics: `ConstraintViolation.boundary` and
/// the identity fields that go with it.
pub(super) struct Boundary<'a> {
    pub kind: &'static str,
    pub function: Option<&'a str>,
    pub parameter: Option<&'a str>,
    pub binding: Option<&'a str>,
    pub declared_type: Option<&'a str>,
}

impl<'a> Boundary<'a> {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            function: None,
            parameter: None,
            binding: None,
            declared_type: None,
        }
    }
}

fn node(kind: &str, entries: Vec<(&str, Val)>) -> Val {
    Val::map_from_iter(
        std::iter::once((Val::from("kind"), Val::from(kind)))
            .chain(entries.into_iter().map(|(k, v)| (Val::from(k), v))),
    )
}

fn literal_val(literal: &LiteralType) -> Val {
    match literal {
        LiteralType::Str(s) => Val::from(s.clone()),
        LiteralType::Int(n) => Val::Int(*n),
        LiteralType::Dec(d) => d
            .parse::<fastnum::D256>()
            .map(Val::Dec)
            .unwrap_or_else(|_| Val::from(d.clone())),
        LiteralType::Bool(b) => Val::Bool(*b),
        LiteralType::Null => Val::Null,
    }
}

fn builtin_name(builtin: &BuiltinType) -> &'static str {
    match builtin {
        BuiltinType::Str => "Str",
        BuiltinType::Int => "Int",
        BuiltinType::Dec => "Dec",
        BuiltinType::Bool => "Bool",
        BuiltinType::Vec => "Vec",
        BuiltinType::Map => "Map",
        BuiltinType::Null => "Null",
    }
}

/// Functions a fast predicate form stands for; a namespace that defines one
/// of these itself gets no fast forms.
const FAST_FUNCTIONS: &[&str] = &[
    "and", "between", "eq", "gt", "gte", "in", "length", "lt", "lte",
];

/// Runtime type names a strict base check understands.
fn is_base_type_name(name: &str) -> bool {
    matches!(
        name,
        "Str" | "Int" | "Dec" | "Bool" | "Vec" | "Map" | "Null" | "Fn" | "Bytes" | "Byte"
    )
}

impl Compiler {
    /// Record the program's type declarations, so constraint specs can be
    /// built for any annotation during emission.
    pub(super) fn collect_constraint_types(&mut self, program: &crate::lang::ast::Program) {
        for (ns_path, namespace) in &program.namespaces {
            for (var, value) in &namespace.scope.vars {
                let name = var.sym.name();
                match value {
                    crate::lang::ast::Value::TypeDef(type_def) => {
                        self.constraint_type_defs
                            .insert(format!("{ns_path}/{name}"), type_def.clone());
                    }
                    crate::lang::ast::Value::Fn(defs) => {
                        if crate::lang::ast::is_where_predicate(name)
                            && let Some(def) = defs.first()
                            && let Some(param) = def.args.args.first()
                        {
                            self.constraint_predicates.insert(
                                format!("{ns_path}/{name}"),
                                (param.var.sym.name().to_string(), def.body.clone()),
                            );
                            if !is_match_calls_are_hot_regex(&def.body, namespace) {
                                self.foreign_regex_predicates
                                    .insert(format!("{ns_path}/{name}"));
                            }
                        } else if FAST_FUNCTIONS.contains(&name)
                            && !matches!(
                                ns_path.to_string().as_str(),
                                "::hot::cmp" | "::hot::bool" | "::hot::coll" | "::hot::str"
                            )
                        {
                            self.shadowing_namespaces.insert(ns_path.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        self.constrained_types.clear();
        self.constraint_spec_ids = self.program.constraint_types.iter().cloned().collect();
    }

    /// The constraint spec for the type `annotation` names in the current
    /// namespace, or `None` when that type has no constraints.
    pub(super) fn constraint_spec_for_annotation(&mut self, annotation: &str) -> Option<Val> {
        let type_expr = parse_type_annotation(annotation)?;
        self.constraint_spec(&type_expr)
    }

    /// Spec for a constrained type expression; `None` when unconstrained.
    fn constraint_spec(&mut self, type_expr: &TypeExpr) -> Option<Val> {
        match type_expr {
            TypeExpr::Any | TypeExpr::Builtin(_) => None,
            TypeExpr::Simple(name) | TypeExpr::Named(name) => {
                if is_base_type_name(name) || name == "Any" {
                    return None;
                }
                let qualified = self.resolve_constraint_type(name)?;
                if !self.type_is_constrained(&qualified, &mut AHashSet::new()) {
                    return None;
                }
                self.named_spec_id(&qualified)?;
                Some(node("named", vec![("type", Val::from(qualified))]))
            }
            TypeExpr::Generic(name, params) => match (name.as_str(), params.as_slice()) {
                ("Vec", [elem]) => {
                    let elem = self.constraint_spec(elem)?;
                    Some(node("vec", vec![("elem", elem)]))
                }
                ("Map", [_, value]) => {
                    let value = self.constraint_spec(value)?;
                    Some(node("map", vec![("elem", value)]))
                }
                _ => None,
            },
            TypeExpr::Optional(inner) => {
                let inner = self.constraint_spec(inner)?;
                Some(node("optional", vec![("inner", inner)]))
            }
            TypeExpr::Union(members) => {
                if members.iter().all(|m| matches!(m, TypeExpr::Literal(_))) {
                    return Some(self.literal_spec(members, None));
                }
                let constrained: Vec<Option<Val>> =
                    members.iter().map(|m| self.constraint_spec(m)).collect();
                if constrained.iter().all(Option::is_none) {
                    return None;
                }
                let members = constrained
                    .into_iter()
                    .zip(members)
                    .map(|(spec, member)| spec.unwrap_or_else(|| self.base_spec(member)))
                    .collect();
                Some(node("union", vec![("members", Val::Vec(members))]))
            }
            TypeExpr::Literal(_) => Some(self.literal_spec(std::slice::from_ref(type_expr), None)),
        }
    }

    /// Spec for any type: the constrained spec when there is one, otherwise
    /// a strict base check. Used inside refinements, where the base type is
    /// enforced too.
    fn full_spec(&mut self, type_expr: &TypeExpr) -> Val {
        match self.constraint_spec(type_expr) {
            Some(spec) => spec,
            None => self.base_spec(type_expr),
        }
    }

    /// A strict base-type check for an unconstrained type. Custom types
    /// without constraints are not checked (gradual typing).
    fn base_spec(&mut self, type_expr: &TypeExpr) -> Val {
        match type_expr {
            TypeExpr::Any => node("any", vec![]),
            TypeExpr::Builtin(builtin) => {
                node("base", vec![("type", Val::from(builtin_name(builtin)))])
            }
            TypeExpr::Simple(name) | TypeExpr::Named(name) => {
                if is_base_type_name(name) {
                    node("base", vec![("type", Val::from(name.as_str()))])
                } else {
                    node("any", vec![])
                }
            }
            TypeExpr::Generic(name, params) => match (name.as_str(), params.as_slice()) {
                ("Vec", [elem]) => {
                    let elem = self.full_spec(elem);
                    node("vec", vec![("elem", elem)])
                }
                ("Map", [_, value]) => {
                    let value = self.full_spec(value);
                    node("map", vec![("elem", value)])
                }
                ("Vec", _) | ("Map", _) => node("base", vec![("type", Val::from(name.as_str()))]),
                _ => node("any", vec![]),
            },
            TypeExpr::Optional(inner) => {
                let inner = self.full_spec(inner);
                node("optional", vec![("inner", inner)])
            }
            TypeExpr::Union(members) => {
                let members = members.iter().map(|m| self.full_spec(m)).collect();
                node("union", vec![("members", Val::Vec(members))])
            }
            TypeExpr::Literal(_) => self.literal_spec(std::slice::from_ref(type_expr), None),
        }
    }

    fn literal_spec(&self, members: &[TypeExpr], type_name: Option<&str>) -> Val {
        let values = members
            .iter()
            .filter_map(|m| match m {
                TypeExpr::Literal(literal) => Some(literal_val(literal)),
                _ => None,
            })
            .collect();
        let mut entries = vec![("values", Val::Vec(values))];
        if let Some(type_name) = type_name {
            entries.push(("type", Val::from(type_name)));
        }
        node("literal", entries)
    }

    /// Qualified name of a declared type, or `None` when it isn't one.
    fn resolve_constraint_type(&self, name: &str) -> Option<String> {
        if self.constraint_type_defs.contains_key(name) {
            return Some(name.to_string());
        }
        let (resolved, _) = self.resolve_type_path(name);
        if self.constraint_type_defs.contains_key(&resolved) {
            return Some(resolved);
        }
        let local = format!("{}/{}", self.current_namespace, name);
        self.constraint_type_defs
            .contains_key(&local)
            .then_some(local)
    }

    /// Whether a declared type carries a constraint: a refinement, a closed
    /// literal union, or a constrained field or alias target.
    pub(super) fn type_is_constrained(
        &mut self,
        qualified: &str,
        visiting: &mut AHashSet<String>,
    ) -> bool {
        if let Some(known) = self.constrained_types.get(qualified) {
            return *known;
        }
        if !visiting.insert(qualified.to_string()) {
            return false;
        }
        let Some(type_def) = self.constraint_type_defs.get(qualified).cloned() else {
            return false;
        };
        let constrained = type_def.refinement.is_some()
            || match (&type_def.type_alias, &type_def.fields) {
                (Some(alias), _) => self.type_expr_is_constrained(alias, visiting, &type_def),
                (None, Some(fields)) => fields
                    .iter()
                    .any(|f| self.type_expr_is_constrained(&f.type_expr, visiting, &type_def)),
                _ => false,
            };
        visiting.remove(qualified);
        self.constrained_types
            .insert(qualified.to_string(), constrained);
        constrained
    }

    fn type_expr_is_constrained(
        &mut self,
        type_expr: &TypeExpr,
        visiting: &mut AHashSet<String>,
        owner: &TypeDef,
    ) -> bool {
        match type_expr {
            TypeExpr::Any | TypeExpr::Builtin(_) => false,
            // A literal alias is a closed union unless declared `open`.
            TypeExpr::Literal(_) => !owner.is_open,
            TypeExpr::Simple(name) | TypeExpr::Named(name) => {
                if is_base_type_name(name) {
                    return false;
                }
                match self.resolve_constraint_type(name) {
                    Some(qualified) => self.type_is_constrained(&qualified, visiting),
                    None => false,
                }
            }
            TypeExpr::Generic(_, params) => params
                .iter()
                .any(|p| self.type_expr_is_constrained(p, visiting, owner)),
            TypeExpr::Optional(inner) => self.type_expr_is_constrained(inner, visiting, owner),
            TypeExpr::Union(members) => {
                if members.iter().all(|m| matches!(m, TypeExpr::Literal(_))) {
                    return !owner.is_open;
                }
                members
                    .iter()
                    .any(|m| self.type_expr_is_constrained(m, visiting, owner))
            }
        }
    }

    /// The constant id of a constrained named type's spec, building and
    /// registering it on first use.
    fn named_spec_id(&mut self, qualified: &str) -> Option<ConstantId> {
        if let Some(id) = self.constraint_spec_ids.get(qualified) {
            return Some(*id);
        }
        let type_def = self.constraint_type_defs.get(qualified)?.clone();
        // Reserve the entry first so recursive references resolve to it.
        let placeholder = self.program.add_constant(Constant::Val(Val::Null));
        self.constraint_spec_ids
            .insert(qualified.to_string(), placeholder);
        let saved_namespace = std::mem::replace(
            &mut self.current_namespace,
            qualified
                .rsplit_once('/')
                .map_or_else(String::new, |(ns, _)| ns.to_string()),
        );
        let spec = self.build_named_spec(qualified, &type_def);
        self.current_namespace = saved_namespace;
        // Constants are interned, so store the spec under a fresh id and
        // point the table at it.
        let id = self.program.add_constant(Constant::Val(spec));
        self.constraint_spec_ids.insert(qualified.to_string(), id);
        self.program
            .constraint_types
            .push((qualified.to_string(), id));
        Some(id)
    }

    fn build_named_spec(&mut self, qualified: &str, type_def: &TypeDef) -> Val {
        let display_name = if is_inline_refinement_type(qualified) {
            "<inline>".to_string()
        } else {
            qualified.to_string()
        };
        if let Some(alias) = &type_def.type_alias {
            if let Some(spec) = self.open_union_spec(type_def) {
                return spec;
            }
            let base = match alias {
                TypeExpr::Literal(_) if !type_def.is_open => {
                    self.literal_spec(std::slice::from_ref(alias), Some(qualified))
                }
                TypeExpr::Union(members)
                    if !type_def.is_open
                        && members.iter().all(|m| matches!(m, TypeExpr::Literal(_))) =>
                {
                    self.literal_spec(members, Some(qualified))
                }
                _ => self.full_spec(alias),
            };
            return match &type_def.refinement {
                Some(refinement) => self.refine_node(base, refinement, &display_name),
                None => base,
            };
        }
        let invariant = type_def
            .refinement
            .as_ref()
            .map(|r| self.refine_node(node("any", vec![]), r, &display_name))
            .unwrap_or(Val::Null);
        if let Some(fields) = &type_def.fields {
            let mut field_specs = Vec::new();
            for field in fields {
                if let Some(spec) = self.constraint_spec(&field.type_expr) {
                    field_specs.push((Val::from(field.name.name().to_string()), spec));
                }
            }
            return node(
                "struct",
                vec![
                    ("type", Val::from(qualified)),
                    ("fields", Val::map_from_iter(field_specs)),
                    ("invariant", invariant),
                ],
            );
        }
        node(
            "enum",
            vec![("type", Val::from(qualified)), ("invariant", invariant)],
        )
    }

    /// The spec of an argument-list precondition: a refinement of the record
    /// of bound arguments.
    pub(super) fn precondition_spec(&self, refinement: &Refinement) -> Val {
        self.refine_node(
            node("base", vec![("type", Val::from("Map"))]),
            refinement,
            "<inline>",
        )
    }

    /// The fast form of a predicate made only of canonical shapes: bounds,
    /// lengths, `in` and `eq` with literals, and `::hot::regex/is-match` with
    /// a literal pattern, joined by `and`. The VM checks it without calling
    /// the predicate. `None` when any part needs the call.
    fn fast_form(&self, refinement: &Refinement) -> Option<Val> {
        let (namespace, _) = refinement.predicate.rsplit_once('/')?;
        if self.shadowing_namespaces.contains(namespace) {
            return None;
        }
        let (param, body) = self.constraint_predicates.get(&refinement.predicate)?;
        let schema = crate::lang::json_schema::refinement_schema(body, param, &refinement.source);
        if schema.describe
            || schema.keywords.is_empty()
            || (self
                .foreign_regex_predicates
                .contains(&refinement.predicate)
                && schema.keywords.iter().any(|(k, _)| k == "pattern"))
        {
            return None;
        }
        Some(Val::map_from_iter(
            schema.keywords.into_iter().map(|(k, v)| (Val::from(k), v)),
        ))
    }

    fn refine_node(&self, base: Val, refinement: &Refinement, constraint: &str) -> Val {
        let mut node = refine_node(base, refinement, constraint);
        if let (Some(fast), Val::Map(entries)) = (self.fast_form(refinement), &mut node) {
            entries.insert(Val::from("fast"), fast);
        }
        node
    }

    /// Emit a check of `src` against `type_spec`, returning the register that
    /// holds the checked value (or the `ConstraintError`), or a `Bool` when
    /// `test` is set.
    pub(super) fn emit_constraint_check(
        &mut self,
        src: RegisterId,
        type_spec: Val,
        boundary: &Boundary,
        test: bool,
    ) -> RegisterId {
        let spec = self.constraint_boundary_constant(type_spec, boundary);
        let dest = self.allocate_register();
        self.emit_instruction(Instruction::CheckConstraint {
            dest,
            src,
            spec,
            test,
        });
        dest
    }
}

/// Whether every `is-match` call in a predicate body is
/// `::hot::regex/is-match`: written in full, through a namespace alias, or
/// through a var alias in `namespace`. The fast form tests a `pattern` with
/// that function's own matcher, so any other `is-match` needs the call.
fn is_match_calls_are_hot_regex(
    body: &crate::lang::ast::Value,
    namespace: &crate::lang::ast::Namespace,
) -> bool {
    use crate::lang::ast::{NsRef, Ref, Value};
    let is_hot_regex = |ns_ref: &NsRef| {
        let ns = namespace.aliases.get(&ns_ref.ns).unwrap_or(&ns_ref.ns);
        ns_ref.function_name.as_deref() == Some("is-match") && ns.to_string() == "::hot::regex"
    };
    match body {
        Value::FnCall(call) => {
            let callee = match call.function.as_ref() {
                Value::Ref(Ref::Ns(ns_ref))
                    if ns_ref.function_name.as_deref() == Some("is-match") =>
                {
                    is_hot_regex(ns_ref)
                }
                Value::Ref(Ref::Var(var_ref)) if var_ref.var.sym.name() == "is-match" => {
                    namespace.scope.vars.iter().any(|(var, value)| {
                        var.sym.name() == "is-match"
                            && matches!(value, Value::Ref(Ref::Ns(ns_ref)) if is_hot_regex(ns_ref))
                    })
                }
                _ => true,
            };
            callee
                && call
                    .args
                    .iter()
                    .all(|arg| is_match_calls_are_hot_regex(&arg.value, namespace))
        }
        Value::Flow(flow) => flow
            .expressions
            .iter()
            .all(|expr| is_match_calls_are_hot_regex(expr, namespace)),
        _ => true,
    }
}

fn refine_node(base: Val, refinement: &Refinement, constraint: &str) -> Val {
    let location = refinement
        .src
        .as_ref()
        .map(|src| {
            Val::map_from_iter([
                (
                    Val::from("file"),
                    src.file.clone().map(Val::from).unwrap_or(Val::Null),
                ),
                (Val::from("line"), Val::Int(src.line as i64)),
                (Val::from("column"), Val::Int(src.column as i64)),
            ])
        })
        .unwrap_or(Val::Null);
    node(
        "refine",
        vec![
            ("base", base),
            ("pred", Val::from(refinement.predicate.clone())),
            ("constraint", Val::from(constraint)),
            ("source", Val::from(refinement.source.clone())),
            ("location", location),
        ],
    )
}

/// The `where` checks a function signature declares.
pub(super) struct SignatureChecks {
    function: String,
    /// Refined parameters: name, spec, declared type.
    params: Vec<(String, Val, String)>,
    /// All parameter names, for the precondition's record of arguments.
    param_names: Vec<String>,
    precondition: Option<Val>,
    /// Return spec and declared type.
    ret: Option<(Val, String)>,
}

impl Compiler {
    /// The checks a signature needs, or `None` when it has no constraints.
    pub(super) fn signature_checks(
        &mut self,
        function_name: &str,
        args: &crate::lang::ast::FnArgs,
        return_type: Option<&str>,
        precondition: Option<&Refinement>,
    ) -> Option<SignatureChecks> {
        let mut params = Vec::new();
        for (index, arg) in args.args.iter().enumerate() {
            // Lazy parameters are rejected statically (`unsupported-constraint`).
            if arg.lazy {
                continue;
            }
            let Some(annotation) = arg.type_annotation.as_deref() else {
                continue;
            };
            let Some(spec) = self.constraint_spec_for_annotation(annotation) else {
                continue;
            };
            // A variadic parameter's annotation types each of its values.
            let spec = if args.variadic && index + 1 == args.args.len() {
                node("vec", vec![("elem", spec)])
            } else {
                spec
            };
            params.push((
                arg.var.sym.name().to_string(),
                spec,
                self.display_annotation(annotation),
            ));
        }
        let precondition = precondition.map(|r| self.precondition_spec(r));
        let ret = return_type.and_then(|annotation| {
            self.constraint_spec_for_annotation(annotation)
                .map(|spec| (spec, self.display_annotation(annotation)))
        });
        if params.is_empty() && precondition.is_none() && ret.is_none() {
            return None;
        }
        let function = if function_name.starts_with("::") {
            function_name.to_string()
        } else {
            format!("{}/{}", self.current_namespace, function_name)
        };
        Some(SignatureChecks {
            function,
            params,
            param_names: args
                .args
                .iter()
                .map(|a| a.var.sym.name().to_string())
                .collect(),
            precondition,
            ret,
        })
    }

    /// Check each refined parameter, rebinding it to the checked value, then
    /// the argument-list precondition. A failure returns its `Err` before the
    /// body runs.
    pub(super) fn emit_signature_prologue(
        &mut self,
        checks: &SignatureChecks,
    ) -> Result<(), String> {
        for (name, spec, annotation) in &checks.params {
            let var_name = self
                .program
                .add_constant(Constant::Val(Val::from(name.as_str())));
            let reg = self.allocate_register();
            self.emit_instruction(Instruction::LoadVar {
                dest: reg,
                var_name,
            });
            let boundary = Boundary {
                function: Some(&checks.function),
                parameter: Some(name),
                declared_type: Some(annotation),
                ..Boundary::new("parameter")
            };
            let checked = self.emit_constraint_check(reg, spec.clone(), &boundary, false);
            self.emit_instruction(Instruction::ReturnIfErr { src: checked });
            self.emit_instruction(Instruction::StoreVar {
                var_name,
                value: checked,
                metadata: None,
            });
        }
        if let Some(spec) = &checks.precondition {
            let record = crate::lang::ast::Value::Val(
                Val::map_from_iter(checks.param_names.iter().map(|name| {
                    (
                        Val::from(name.as_str()),
                        Val::Box(Box::new(crate::lang::ast::AstNode(
                            crate::lang::ast::Value::Ref(crate::lang::ast::Ref::Var(
                                crate::lang::ast::VarRef {
                                    var: crate::lang::ast::Var {
                                        sym: crate::lang::ast::Sym::String(name.clone()),
                                        deep_set: None,
                                        deep_path: None,
                                        meta: None,
                                        type_annotation: None,
                                        src: None,
                                    },
                                    src: None,
                                },
                            )),
                        ))),
                    )
                })),
                None,
            );
            let reg = self.compile_value(&record)?;
            let boundary = Boundary {
                function: Some(&checks.function),
                ..Boundary::new("arguments")
            };
            let checked = self.emit_constraint_check(reg, spec.clone(), &boundary, false);
            self.emit_instruction(Instruction::ReturnIfErr { src: checked });
        }
        Ok(())
    }

    /// Check a successful return value; an `Err` passes through unchanged.
    pub(super) fn emit_return_check(
        &mut self,
        checks: &SignatureChecks,
        result: RegisterId,
    ) -> RegisterId {
        let Some((spec, annotation)) = &checks.ret else {
            return result;
        };
        let boundary = Boundary {
            function: Some(&checks.function),
            declared_type: Some(annotation),
            ..Boundary::new("return")
        };
        self.emit_constraint_check(result, spec.clone(), &boundary, false)
    }

    /// Check a typed binding's value against its declared type's
    /// constraints, returning the register to store.
    pub(super) fn constrain_binding(
        &mut self,
        var: &crate::lang::ast::Var,
        reg: RegisterId,
    ) -> RegisterId {
        let Some(annotation) = var.type_annotation.clone() else {
            return reg;
        };
        let Some(spec) = self.constraint_spec_for_annotation(&annotation) else {
            return reg;
        };
        let declared_type = self.display_annotation(&annotation);
        let boundary = Boundary {
            binding: Some(var.sym.name()),
            declared_type: Some(&declared_type),
            ..Boundary::new("binding")
        };
        self.emit_constraint_check(reg, spec, &boundary, false)
    }
}

impl Compiler {
    /// A type annotation for messages: an inline refinement shows as
    /// `Base where predicate` rather than its hidden alias name.
    pub(super) fn display_annotation(&self, annotation: &str) -> String {
        if !annotation.contains(WHERE_TYPE_PREFIX) {
            return annotation.to_string();
        }
        match parse_type_annotation(annotation) {
            Some(type_expr) => self.display_type(&type_expr),
            None => annotation.to_string(),
        }
    }

    fn display_type(&self, type_expr: &TypeExpr) -> String {
        let join = |types: &[TypeExpr], separator: &str| {
            types
                .iter()
                .map(|t| self.display_type(t))
                .collect::<Vec<_>>()
                .join(separator)
        };
        match type_expr {
            TypeExpr::Named(name) | TypeExpr::Simple(name) if is_inline_refinement_type(name) => {
                match self
                    .resolve_constraint_type(name)
                    .and_then(|qualified| self.constraint_type_defs.get(&qualified))
                {
                    Some(TypeDef {
                        type_alias: Some(base),
                        refinement: Some(refinement),
                        ..
                    }) => format!("{} where {}", self.display_type(base), refinement.source),
                    _ => name.clone(),
                }
            }
            TypeExpr::Generic(name, params) => format!("{}<{}>", name, join(params, ", ")),
            TypeExpr::Optional(inner) => format!("{}?", self.display_type(inner)),
            TypeExpr::Union(members) => join(members, " | "),
            other => other.to_string(),
        }
    }

    /// The constant for a `CheckConstraint` at a boundary.
    pub(super) fn constraint_boundary_constant(
        &mut self,
        type_spec: Val,
        boundary: &Boundary,
    ) -> ConstantId {
        let mut entries = vec![
            (Val::from("type"), type_spec),
            (Val::from("boundary"), Val::from(boundary.kind)),
        ];
        for (key, value) in [
            ("function", boundary.function),
            ("parameter", boundary.parameter),
            ("binding", boundary.binding),
            ("declared-type", boundary.declared_type),
        ] {
            if let Some(value) = value {
                entries.push((Val::from(key), Val::from(value)));
            }
        }
        self.program
            .add_constant(Constant::Val(Val::map_from_iter(entries)))
    }

    /// An open literal union's spec: its values are plain values of the
    /// members' base types, and any value of those types is accepted.
    fn open_union_spec(&mut self, type_def: &TypeDef) -> Option<Val> {
        if !type_def.is_open {
            return None;
        }
        let members = match type_def.type_alias.as_ref()? {
            TypeExpr::Union(members) => members.clone(),
            literal @ TypeExpr::Literal(_) => vec![literal.clone()],
            _ => return None,
        };
        let mut bases: Vec<&str> = Vec::new();
        for member in &members {
            let TypeExpr::Literal(literal) = member else {
                return None;
            };
            let base = match literal {
                LiteralType::Str(_) => "Str",
                LiteralType::Int(_) => "Int",
                LiteralType::Dec(_) => "Dec",
                LiteralType::Bool(_) => "Bool",
                LiteralType::Null => "Null",
            };
            if !bases.contains(&base) {
                bases.push(base);
            }
        }
        let specs: Vec<Val> = bases
            .into_iter()
            .map(|base| node("base", vec![("type", Val::from(base))]))
            .collect();
        Some(match specs.len() {
            1 => specs.into_iter().next().unwrap_or(Val::Null),
            _ => node("union", vec![("members", Val::Vec(specs))]),
        })
    }

    /// The boundary constant for a constrained type's constructor, or `None`
    /// when the type has no constraints. An open literal union counts: its
    /// constructor checks the base type and keeps the value plain.
    pub(super) fn constructor_check_constant(&mut self, qualified: &str) -> Option<ConstantId> {
        let open_union = self
            .constraint_type_defs
            .get(qualified)
            .is_some_and(|t| t.is_open && Self::is_literal_union(t));
        if !open_union && !self.type_is_constrained(qualified, &mut AHashSet::new()) {
            return None;
        }
        self.named_spec_id(qualified)?;
        let spec = node("named", vec![("type", Val::from(qualified))]);
        let boundary = Boundary {
            declared_type: Some(qualified),
            ..Boundary::new("constructor")
        };
        Some(self.constraint_boundary_constant(spec, &boundary))
    }

    /// A refinement alias (`Port type Int where …`) or a literal union
    /// (`Fruit type "apple" | "banana"`, closed or open): its values stay
    /// plain base values.
    pub(super) fn is_constrained_alias(type_def: &TypeDef) -> bool {
        (type_def.type_alias.is_some() && type_def.refinement.is_some())
            || Self::is_literal_union(type_def)
    }

    fn is_literal_union(type_def: &TypeDef) -> bool {
        match &type_def.type_alias {
            Some(TypeExpr::Literal(_)) => true,
            Some(TypeExpr::Union(members)) => {
                members.iter().all(|m| matches!(m, TypeExpr::Literal(_)))
            }
            _ => false,
        }
    }

    /// The spec of a refinement alias or literal union, for a `match` arm
    /// that guards on it.
    pub(super) fn untagged_alias_spec(&mut self, qualified: &str) -> Option<Val> {
        let type_def = self.constraint_type_defs.get(qualified)?;
        if !Self::is_constrained_alias(type_def) {
            return None;
        }
        self.named_spec_id(qualified)?;
        Some(node("named", vec![("type", Val::from(qualified))]))
    }

    /// Whether a type name resolves to a constrained alias.
    pub(super) fn names_constrained_alias(&self, name: &str) -> bool {
        self.resolve_constraint_type(name)
            .and_then(|qualified| self.constraint_type_defs.get(&qualified))
            .is_some_and(Self::is_constrained_alias)
    }
}
