//! # Named arguments
//!
//! `User(name: "Ada", active: true)` passes a record type's fields by name.
//! Placeholder lowering already walks every call with the program's
//! declarations in hand, so it handles named calls too: for each call with a
//! named argument it resolves the callee and checks the names against the
//! record's fields ([`RecordTypes::named_fields`]), lowers each argument with
//! its field as the parameter slot, so a `%` in a field typed `Fn` becomes a
//! lambda there, and then rewrites the call to its Map form
//! ([`into_map_call`]): `User({name: "Ada", active: true})`. Later phases
//! only see that form.
//!
//! The callee must be a record type, or an enum variant whose payload is a
//! record. Every name must be a field, every field that doesn't allow `null`
//! must be given, and every argument must be named. Like the Map form, a
//! named call on a type with custom constructors (`type fn`) goes to the
//! default constructor, so the custom ones don't run. A type declared twice,
//! once with its fields and once with its constructors, is one type. A type
//! with a one-parameter constructor that takes a Map is rejected: that
//! constructor decides which keys it reads. A parameter or local named like
//! the type shadows it, as it does for any call, so a named call by that
//! name is rejected too. A local bound to a qualified name, such as
//! `Shape ::geo/Shape` in a function body, is a local import: a named call by
//! that name goes to the type it names.

use crate::lang::ast::{
    AstNode, DeepPath, FnCall, FnCallArg, NsPath, Program, Ref, Source, TypeDef, TypeField, Value,
    split_top_level,
};
use crate::lang::compiler::Compiler;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::val::Val;
use ahash::AHashMap;
use indexmap::IndexMap;
use std::cell::RefCell;

const ONLY_RECORDS: &str = "Only record types and enum variants take named arguments.";

/// A named call that doesn't fit its callee. Placeholder lowering reports it
/// with the enclosing binding's location.
pub(super) struct NamedArgSite {
    pub callee: String,
    pub message: String,
    pub src: Option<Source>,
}

/// The program's type declarations, for resolving the callee of a named call.
pub(super) struct RecordTypes {
    /// Declarations by namespace and name.
    declared: AHashMap<(String, String), TypeDef>,
    /// The namespace of each `meta {core: true}` type, which any namespace
    /// names without an import.
    core: AHashMap<String, String>,
    /// Imports such as `User ::app::users/User`, by namespace and local name.
    imports: AHashMap<(String, String), (String, String)>,
    /// Namespace aliases (`::http ::hot::http`) by namespace.
    aliases: AHashMap<String, AHashMap<String, String>>,
    /// Variants enrolled with `Source -> Enum.Variant`, by the enum's
    /// namespace and name and the variant, to the source type.
    enrolled: AHashMap<(String, String, String), (String, String)>,
    /// Types declared by the blocks being lowered, innermost last.
    local: RefCell<Vec<Vec<(String, TypeDef)>>>,
    /// Namespace aliases declared by the blocks being lowered, innermost last.
    local_aliases: RefCell<Vec<Vec<(String, String)>>>,
    /// The parameters and locals in scope where calls are being lowered,
    /// innermost last. A call by one of these names goes to the local value,
    /// not to a type of the same name (see the compiler's `local_names`).
    /// A local import carries the qualified name it is bound to.
    shadowing: RefCell<Vec<(String, Option<String>)>>,
    /// Named calls that don't fit, found while lowering the current binding.
    pub sites: RefCell<Vec<NamedArgSite>>,
}

impl RecordTypes {
    pub(super) fn build(program: &Program) -> Self {
        let mut types = RecordTypes {
            declared: AHashMap::new(),
            core: AHashMap::new(),
            imports: AHashMap::new(),
            aliases: AHashMap::new(),
            enrolled: AHashMap::new(),
            local: RefCell::new(Vec::new()),
            local_aliases: RefCell::new(Vec::new()),
            shadowing: RefCell::new(Vec::new()),
            sites: RefCell::new(Vec::new()),
        };
        let mut arrows = Vec::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns = ns_path.to_string();
            let aliases: AHashMap<String, String> = namespace
                .aliases
                .iter()
                .map(|(alias, target)| (alias.to_string(), target.to_string()))
                .collect();
            for (var, value) in &namespace.scope.vars {
                let name = var.sym.name();
                match value {
                    Value::TypeDef(def) => {
                        // The last core declaration of a name wins, as in the
                        // runtime core registry: a project's core `Next`
                        // overrides hot-std's.
                        if CoreVariableRegistry::has_core_metadata(var) {
                            types.core.insert(name.to_string(), ns.clone());
                        }
                        match types.declared.get_mut(&(ns.clone(), name.to_string())) {
                            Some(declared) => merge_declaration(declared, def),
                            None => {
                                types
                                    .declared
                                    .insert((ns.clone(), name.to_string()), def.clone());
                            }
                        }
                    }
                    Value::Ref(Ref::Ns(ns_ref)) => {
                        if let Some(target) = &ns_ref.function_name {
                            let target_ns = ns_ref.ns.to_string();
                            let target_ns = aliases.get(&target_ns).cloned().unwrap_or(target_ns);
                            types.imports.insert(
                                (ns.clone(), name.to_string()),
                                (target_ns, target.clone()),
                            );
                        }
                    }
                    Value::TypeImplementation(ti) => {
                        arrows.push((ns.clone(), ti.source_type.clone(), ti.target_type.clone()));
                    }
                    _ => {}
                }
            }
            if !aliases.is_empty() {
                types.aliases.insert(ns, aliases);
            }
        }
        for (ns, source, target) in arrows {
            let Some((enum_name, variant)) = target.rsplit_once('.') else {
                continue;
            };
            if let (Some((enum_ns, enum_name)), Some(source)) =
                (types.resolve(&ns, enum_name), types.resolve(&ns, &source))
            {
                types
                    .enrolled
                    .insert((enum_ns, enum_name, variant.to_string()), source);
            }
        }
        types
    }

    /// Make the types and namespace aliases a block declares visible while
    /// its expressions are lowered. A function body binds a type as
    /// `MultipleValues([name, TypeDef])`, and other blocks as a reference to
    /// the name followed by the `TypeDef`. Returns whether a frame was
    /// pushed, for [`Self::leave_block`].
    pub(super) fn enter_block(
        &self,
        expressions: &[Value],
        aliases: &[(usize, NsPath, NsPath)],
    ) -> bool {
        fn local_type(name: &Value, value: &Value) -> Option<(String, TypeDef)> {
            match (name, value) {
                (Value::Ref(Ref::Var(vr)), Value::TypeDef(def)) if vr.var.deep_path.is_none() => {
                    Some((vr.var.sym.name().to_string(), def.clone()))
                }
                _ => None,
            }
        }
        let bound = expressions
            .iter()
            .filter_map(|expression| match expression {
                Value::MultipleValues(values) if values.len() == 2 => {
                    local_type(&values[0], &values[1])
                }
                _ => None,
            });
        let paired = expressions
            .windows(2)
            .filter_map(|pair| local_type(&pair[0], &pair[1]));
        let frame: Vec<(String, TypeDef)> = bound.chain(paired).collect();
        if frame.is_empty() && aliases.is_empty() {
            return false;
        }
        self.local.borrow_mut().push(frame);
        self.local_aliases.borrow_mut().push(
            aliases
                .iter()
                .map(|(_, alias, target)| (alias.to_string(), target.to_string()))
                .collect(),
        );
        true
    }

    pub(super) fn leave_block(&self, pushed: bool) {
        if pushed {
            self.local.borrow_mut().pop();
            self.local_aliases.borrow_mut().pop();
        }
    }

    /// Bring parameters or a block's local into scope, shadowing types of the
    /// same name. Returns the mark to pass to [`Self::unshadow`] when the
    /// scope ends.
    pub(super) fn shadow(&self, names: impl IntoIterator<Item = String>) -> usize {
        let mut shadowing = self.shadowing.borrow_mut();
        let mark = shadowing.len();
        shadowing.extend(names.into_iter().map(|name| (name, None)));
        mark
    }

    /// Bring a block's local import into scope: `name` stands for the
    /// qualified `target` (`::geo/Shape`) from here on, in place of any
    /// type of the same name. [`Self::unshadow`] ends it with the block.
    pub(super) fn import_locally(&self, name: String, target: String) {
        self.shadowing.borrow_mut().push((name, Some(target)));
    }

    pub(super) fn unshadow(&self, mark: usize) {
        self.shadowing.borrow_mut().truncate(mark);
    }

    /// The field each argument of a named call fills, in argument order, as
    /// (type annotation, field name), or why the call doesn't fit. `piped`
    /// is true for a pipe stage, whose first argument is the piped value.
    /// `ns` is the namespace the call is in.
    pub(super) fn named_fields(
        &self,
        call: &FnCall,
        piped: bool,
        ns: &str,
    ) -> Result<Vec<(Option<String>, String)>, NamedArgSite> {
        let callee = callee_display(&call.function);
        let site = |message: String, src: &Option<Source>| NamedArgSite {
            callee: callee.clone(),
            message,
            src: src.clone().or_else(|| call.src.clone()),
        };
        if piped {
            return Err(site(
                "A piped value can't fill a named field. Pass every field by name.".to_string(),
                &call.src,
            ));
        }
        let fields = self
            .callee_fields(&call.function, ns, &callee)
            .map_err(|message| site(message, &call.src))?;

        let mut slots = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            let Some(name) = &arg.name else {
                return Err(site(
                    format!("Pass every field by name, or pass one Map: `{callee}({{...}})`."),
                    &arg.src,
                ));
            };
            let Some(field) = fields.iter().find(|field| field.name.name() == name) else {
                return Err(site(unknown_field(&callee, name, &fields), &arg.src));
            };
            slots.push((Some(field.type_annotation.clone()), name.clone()));
        }

        let missing: Vec<&str> = fields
            .iter()
            .filter(|field| !Compiler::type_expr_allows_null(&field.type_expr))
            .map(|field| field.name.name())
            .filter(|field| {
                !call
                    .args
                    .iter()
                    .any(|arg| arg.name.as_deref() == Some(*field))
            })
            .collect();
        match missing.as_slice() {
            [] => Ok(slots),
            [field] => Err(site(
                format!("Missing required field `{field}`."),
                &call.src,
            )),
            many => Err(site(
                format!("Missing required fields {}.", code_list(many)),
                &call.src,
            )),
        }
    }

    /// The declared fields of a named call's callee: a record type's, or the
    /// payload record's for an enum variant.
    fn callee_fields(
        &self,
        function: &Value,
        ns: &str,
        callee: &str,
    ) -> Result<Vec<TypeField>, String> {
        let (type_name, variant) = match function {
            Value::Ref(Ref::Var(vr)) => {
                let name = vr.var.sym.name();
                match &vr.var.deep_path {
                    None => match name.split_once('.') {
                        Some((type_name, variant)) => {
                            (type_name.to_string(), Some(variant.to_string()))
                        }
                        None => (name.to_string(), None),
                    },
                    Some(DeepPath::Key(variant)) => (name.to_string(), Some(variant.clone())),
                    Some(_) => {
                        return Err(format!("`{callee}` isn't a record type. {ONLY_RECORDS}"));
                    }
                }
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                let Some(name) = ns_ref.function_name.as_deref() else {
                    return Err(format!("`{callee}` isn't a record type. {ONLY_RECORDS}"));
                };
                match name.split_once('.') {
                    Some((type_name, variant)) => (
                        format!("{}/{}", ns_ref.ns, type_name),
                        Some(variant.to_string()),
                    ),
                    None => (format!("{}/{}", ns_ref.ns, name), None),
                }
            }
            _ => return Err(ONLY_RECORDS.to_string()),
        };

        let local = if type_name.contains('/') {
            None
        } else {
            self.shadowing
                .borrow()
                .iter()
                .rev()
                .find(|(name, _)| *name == type_name)
                .map(|(_, target)| target.clone())
        };
        // A local import of a variant (`Circle ::geo/Shape.Circle`) names
        // the enum and the variant whose payload the call builds.
        let (local, variant) = match (local, variant) {
            (Some(Some(target)), None) => match target.rsplit_once('/') {
                Some((target_ns, name)) if name.contains('.') => {
                    let (enum_name, variant) = name.split_once('.').unwrap_or((name, ""));
                    (
                        Some(Some(format!("{target_ns}/{enum_name}"))),
                        Some(variant.to_string()),
                    )
                }
                _ => (Some(Some(target)), None),
            },
            // So does a namespace's var import of one.
            (None, None) if !type_name.contains('/') => {
                match self.imports.get(&(ns.to_string(), type_name.clone())) {
                    Some((target_ns, name)) if name.contains('.') => {
                        let (enum_name, variant) = name.split_once('.').unwrap_or((name, ""));
                        (
                            Some(Some(format!("{target_ns}/{enum_name}"))),
                            Some(variant.to_string()),
                        )
                    }
                    _ => (None, None),
                }
            }
            other => other,
        };
        let shadowed = matches!(local, Some(None));
        let found = match &local {
            Some(Some(target)) => self.find(ns, target),
            _ => self.find(ns, &type_name),
        };
        let Some((def, def_ns, def_name)) = found.filter(|_| !shadowed) else {
            if shadowed {
                return Err(format!(
                    "`{type_name}` is a local value here, not a record type. {ONLY_RECORDS}"
                ));
            }
            return Err(format!("`{type_name}` isn't a record type. {ONLY_RECORDS}"));
        };
        let Some(variant) = variant else {
            if def.variants.is_some() {
                return Err(format!(
                    "`{callee}` is an enum. Name a variant, as in `{callee}.Variant(...)`."
                ));
            }
            if let Some(constructor) = map_constructor(&def) {
                return Err(format!(
                    "`{callee}` has a constructor that takes one Map, `{constructor}`, which \
                     decides the keys it reads. Pass the Map: `{callee}({{...}})`."
                ));
            }
            return def
                .fields
                .ok_or_else(|| format!("`{callee}` has no fields to name."));
        };

        if def.variants.is_none() {
            return Err(format!(
                "`{type_name}` isn't an enum, so it has no variant `{variant}`."
            ));
        }
        let declared = def
            .variants
            .iter()
            .flatten()
            .find(|v| v.name.name() == variant);
        let payload = match declared {
            Some(declared) => {
                let Some(type_ref) = &declared.type_ref else {
                    return Err(format!("`{callee}` carries no data."));
                };
                self.find(&def_ns, type_ref).ok_or_else(|| {
                    format!("`{callee}` carries `{type_ref}`, which has no fields.")
                })?
            }
            None => self
                .enrolled
                .get(&(def_ns, def_name, variant.clone()))
                .and_then(|(ns, name)| {
                    let def = self.declared.get(&(ns.clone(), name.clone()))?;
                    Some((def.clone(), ns.clone(), name.clone()))
                })
                .ok_or_else(|| format!("`{type_name}` has no variant `{variant}`."))?,
        };
        // A variant constructor builds its payload from the Map the way the
        // payload type's default constructor does, so the payload's own
        // custom constructors don't apply.
        let (payload, _, payload_name) = payload;
        payload
            .fields
            .ok_or_else(|| format!("`{callee}` carries `{payload_name}`, which has no fields."))
    }

    /// The declaration `name` refers to in namespace `ns`, with the
    /// namespace and name it is declared under. Types declared by enclosing
    /// blocks come first.
    fn find(&self, ns: &str, name: &str) -> Option<(TypeDef, String, String)> {
        if !name.contains('/') {
            let local = self.local.borrow();
            for frame in local.iter().rev() {
                let mut defs = frame
                    .iter()
                    .filter(|(local_name, _)| local_name == name)
                    .map(|(_, def)| def);
                if let Some(first) = defs.next() {
                    let mut def = first.clone();
                    for later in defs {
                        merge_declaration(&mut def, later);
                    }
                    return Some((def, ns.to_string(), name.to_string()));
                }
            }
        }
        let (def_ns, def_name) = self.resolve(ns, name)?;
        let def = self.declared.get(&(def_ns.clone(), def_name.clone()))?;
        Some((def.clone(), def_ns, def_name))
    }

    /// The namespace and name of the declared type that `name` (`User`,
    /// `::app::users/User`, `::alias/User`) refers to in namespace `ns`.
    fn resolve(&self, ns: &str, name: &str) -> Option<(String, String)> {
        if let Some((prefix, type_name)) = name.rsplit_once('/') {
            let block_alias = self
                .local_aliases
                .borrow()
                .iter()
                .rev()
                .flatten()
                .find(|(alias, _)| alias == prefix)
                .map(|(_, target)| target.clone());
            let target_ns = block_alias
                .or_else(|| {
                    self.aliases
                        .get(ns)
                        .and_then(|aliases| aliases.get(prefix))
                        .cloned()
                })
                .unwrap_or_else(|| prefix.to_string());
            let key = (target_ns, type_name.to_string());
            return self.declared.contains_key(&key).then_some(key);
        }
        let key = (ns.to_string(), name.to_string());
        if self.declared.contains_key(&key) {
            return Some(key);
        }
        if let Some(target) = self.imports.get(&key)
            && self.declared.contains_key(target)
        {
            return Some(target.clone());
        }
        let core_ns = self.core.get(name)?;
        Some((core_ns.clone(), name.to_string()))
    }
}

/// `T(a: x, b: y)` as `T({a: x, b: y})`, once its arguments are lowered.
/// Literal values go into the Map as they are and other expressions as AST
/// nodes, as in a Map literal.
pub(super) fn into_map_call(mut call: FnCall) -> FnCall {
    let src = call.args.first().and_then(|arg| arg.src.clone());
    let mut entries = IndexMap::new();
    for arg in std::mem::take(&mut call.args) {
        let value = match arg.value {
            Value::Val(val, _) => val,
            other => Val::Box(Box::new(AstNode(other))),
        };
        entries.insert(Val::from(arg.name.unwrap_or_default()), value);
    }
    call.args = vec![FnCallArg {
        value: Value::Val(Val::Map(Box::new(entries)), Some("Map".to_string())),
        lazy: false,
        spread: false,
        name: None,
        src,
    }];
    call
}

/// The callee of a named call as written: `User`, `Shape.Circle`,
/// `::geo/Shape.Circle`.
fn callee_display(function: &Value) -> String {
    match function {
        Value::Ref(Ref::Var(vr)) => match &vr.var.deep_path {
            Some(DeepPath::Key(variant)) => format!("{}.{}", vr.var.sym.name(), variant),
            _ => vr.var.sym.name().to_string(),
        },
        Value::Ref(Ref::Ns(ns_ref)) => match &ns_ref.function_name {
            Some(name) => format!("{}/{}", ns_ref.ns, name),
            None => ns_ref.ns.to_string(),
        },
        _ => "this call".to_string(),
    }
}

/// Fold a later declaration of the same type into `def`. A type declared as
/// `Spot type { ... }` and again as `Spot type fn ...` is one type, with the
/// fields of one and the constructors of the other, in either order.
fn merge_declaration(def: &mut TypeDef, later: &TypeDef) {
    if later.fields.is_some() {
        def.fields = later.fields.clone();
    }
    if let Some(constructors) = &later.constructor_functions {
        def.constructor_functions
            .get_or_insert_with(Vec::new)
            .extend(constructors.iter().cloned());
    }
}

/// A custom constructor of `def` with one parameter that takes a Map, as
/// written: `(m: Map)`.
fn map_constructor(def: &TypeDef) -> Option<String> {
    def.constructor_functions
        .iter()
        .flatten()
        .find_map(|constructor| match constructor.args.args.as_slice() {
            [param] => {
                let annotation = param.type_annotation.as_deref()?;
                takes_map(annotation).then(|| {
                    let lazy = if param.lazy { "lazy " } else { "" };
                    format!("({lazy}{}: {annotation})", param.var.sym.name())
                })
            }
            _ => None,
        })
}

/// Whether a parameter type takes a Map: `Map`, with or without type
/// arguments or `?`, or a union with such a member. `Any` doesn't count, as
/// a Map argument still goes to the default constructor then.
fn takes_map(annotation: &str) -> bool {
    split_top_level(annotation, '|').into_iter().any(|member| {
        let member = member.trim_end_matches('?');
        let name = member.split('<').next().unwrap_or(member).trim();
        matches!(name, "Map" | "::hot::type/Map")
    })
}

fn unknown_field(callee: &str, name: &str, fields: &[TypeField]) -> String {
    let names: Vec<&str> = fields.iter().map(|field| field.name.name()).collect();
    if names.is_empty() {
        return format!("`{callee}` has no fields.");
    }
    match closest(name, &names) {
        Some(close) => format!("`{callee}` has no field `{name}`. Did you mean `{close}`?"),
        None => format!(
            "`{callee}` has no field `{name}`. Its fields are {}.",
            code_list(&names)
        ),
    }
}

/// The field name within two edits of `name`, if one is.
fn closest<'a>(name: &str, names: &[&'a str]) -> Option<&'a str> {
    names
        .iter()
        .map(|candidate| (edit_distance(name, candidate), *candidate))
        .filter(|(distance, _)| *distance <= 2 && *distance < name.chars().count())
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if ca == *cb {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

fn code_list(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_field_allows_two_edits() {
        let names = ["name", "email", "active"];
        assert_eq!(closest("nme", &names), Some("name"));
        assert_eq!(closest("emial", &names), Some("email"));
        assert_eq!(closest("zzzzzz", &names), None);
        // Every edit of a short name is a different word, not a typo.
        assert_eq!(closest("id", &["ab"]), None);
    }

    #[test]
    fn map_parameter_types_take_a_map() {
        for annotation in [
            "Map",
            "Map?",
            "Map<Str, Any>",
            "Str | Map",
            "::hot::type/Map",
        ] {
            assert!(takes_map(annotation), "{annotation}");
        }
        for annotation in ["Any", "Str", "Vec<Map>", "MapEntry", "::hot::store/Map"] {
            assert!(!takes_map(annotation), "{annotation}");
        }
    }
}
