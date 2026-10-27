//! Convert Hot type annotations to JSON Schema
//!
//! This module provides functions to convert Hot type annotations (strings)
//! to JSON Schema format for MCP tool definitions.

use crate::lang::ast::{
    FnArg, LiteralType, Ref, TypeDef, TypeExpr, TypeField, Value, split_top_level,
};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use fastnum::D256;
use indexmap::IndexMap;

/// Registry of type definitions for resolving custom types
#[derive(Debug, Default, Clone)]
pub struct TypeRegistry {
    /// Maps type names (simple or fully qualified) to their definitions
    types: AHashMap<String, TypeDefInfo>,
}

/// One variant of an enum, normalized for schema emission
#[derive(Debug, Clone)]
pub struct VariantInfo {
    /// Variant name (e.g. "Circle", "Point")
    pub name: String,
    /// `Some(type_ref)` for variants that carry a payload (`Circle(Circle)`),
    /// `None` for unit variants (`Point`).
    pub type_ref: Option<String>,
}

/// Information about a type definition
#[derive(Debug, Clone)]
pub struct TypeDefInfo {
    /// The type definition fields (None for aliases/variants)
    pub fields: Option<Vec<TypeField>>,
    /// For enum/variant types, the variants with their optional payload type refs
    pub variants: Option<Vec<VariantInfo>>,
    /// For type aliases, the aliased type
    pub type_alias: Option<String>,
    /// What the type's `where` clause contributes, when it has one.
    pub refinement: Option<RefinementSchema>,
    /// For literal-union aliases (`"a" | "b"`, `1 | 2`, `"a" | null`), the
    /// members in declaration order. Open unions accumulate members across
    /// repeated declarations.
    pub literals: Option<Vec<LiteralType>>,
    /// True for `open` declarations
    pub is_open: bool,
}

/// What a `where` clause contributes to a type's schema.
#[derive(Debug, Clone, Default)]
pub struct RefinementSchema {
    /// Keywords for the canonical predicate shapes, such as `minimum`. Length
    /// bounds are `$minLength`/`$maxLength` until the base type says whether
    /// they mean characters or items.
    pub keywords: Vec<(String, Val)>,
    /// The predicate's source text.
    pub source: String,
    /// Part of the predicate has no JSON Schema form. It is still enforced
    /// at runtime; the schema describes it.
    pub describe: bool,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self {
            types: AHashMap::new(),
        }
    }

    /// Returns the number of registered types
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Returns true if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Register a type definition
    pub fn register(&mut self, name: &str, type_def: &TypeDef) {
        let info = self.info_for(name, type_def);

        // Register with simple name
        self.types.insert(name.to_string(), info.clone());

        // Also register with type_def.name if different
        let def_name = type_def.name.to_string();
        if def_name != name {
            self.types.insert(def_name, info);
        }
    }

    /// Register a type with its fully qualified name
    pub fn register_qualified(&mut self, qualified_name: &str, type_def: &TypeDef) {
        let info = self.info_for(qualified_name, type_def);

        self.types.insert(qualified_name.to_string(), info.clone());

        // Also register with simple name (last part after /)
        if let Some(simple_name) = qualified_name.rsplit('/').next() {
            self.types.insert(simple_name.to_string(), info);
        }
    }

    /// Record what a registered type's `where` clause contributes to its
    /// schema, under its qualified and simple names.
    pub fn set_refinement(&mut self, qualified_name: &str, refinement: RefinementSchema) {
        let simple_name = qualified_name.rsplit('/').next().unwrap_or(qualified_name);
        for name in [qualified_name, simple_name] {
            if let Some(info) = self.types.get_mut(name) {
                info.refinement = Some(refinement.clone());
            }
        }
    }

    /// Build the info for `type_def` registered under `key`. A repeated
    /// open literal-union declaration (`HttpMethod type open | "TRACE"`)
    /// extends the prior entry's members; any other redeclaration
    /// replaces it.
    fn info_for(&self, key: &str, type_def: &TypeDef) -> TypeDefInfo {
        let mut info = TypeDefInfo {
            fields: type_def.fields.clone(),
            variants: variants_from_type_def(type_def),
            type_alias: type_def.type_alias.as_ref().map(|ta| ta.to_string()),
            refinement: None,
            literals: type_def.type_alias.as_ref().and_then(literal_members),
            is_open: type_def.is_open,
        };

        if let Some(prior) = self.types.get(key)
            && (prior.is_open || info.is_open)
            && let (Some(prior_members), Some(members)) = (&prior.literals, &info.literals)
        {
            let mut merged = prior_members.clone();
            for member in members {
                if !merged.contains(member) {
                    merged.push(member.clone());
                }
            }
            let alias = TypeExpr::Union(merged.iter().cloned().map(TypeExpr::Literal).collect());
            info.type_alias = Some(alias.to_string());
            info.literals = Some(merged);
            info.is_open = true;
        }

        info
    }

    /// Look up a type by name, returning the (possibly fully-qualified) match key alongside it.
    pub fn get_with_key(&self, name: &str) -> Option<(String, &TypeDefInfo)> {
        if let Some(info) = self.types.get(name) {
            return Some((name.to_string(), info));
        }
        if let Some(simple_name) = name.rsplit('/').next()
            && let Some(info) = self.types.get(simple_name)
        {
            return Some((simple_name.to_string(), info));
        }
        None
    }

    /// Look up a type by name
    pub fn get(&self, name: &str) -> Option<&TypeDefInfo> {
        // Try exact match first
        if let Some(info) = self.types.get(name) {
            return Some(info);
        }

        // Try without namespace prefix (::ns/Type -> Type)
        if let Some(simple_name) = name.rsplit('/').next() {
            return self.types.get(simple_name);
        }

        None
    }
}

/// Build the `VariantInfo` list for an enum-shaped TypeDef.
fn variants_from_type_def(type_def: &TypeDef) -> Option<Vec<VariantInfo>> {
    type_def.variants.as_ref().map(|vs| {
        vs.iter()
            .map(|v| VariantInfo {
                name: v.name.to_string(),
                type_ref: v.type_ref.clone(),
            })
            .collect()
    })
}

/// Members of a literal-union alias (`"a" | "b"`, `1 | 2`, `"a" | null`),
/// or `None` if any member is not a literal.
fn literal_members(alias: &TypeExpr) -> Option<Vec<LiteralType>> {
    match alias {
        TypeExpr::Literal(lit) => Some(vec![lit.clone()]),
        TypeExpr::Union(members) => members
            .iter()
            .map(|member| match member {
                TypeExpr::Literal(lit) => Some(lit.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Schema-generation context — accumulates `$defs` for cyclic types
/// and tracks the visited set for cycle detection.
#[derive(Debug, Default)]
pub struct SchemaContext {
    /// Definitions hoisted into `$defs` keyed by sanitized name.
    pub defs: IndexMap<Val, Val>,
    /// Names of types currently being expanded (cycle detection).
    pub visited: AHashSet<String>,
    /// Names of types we've decided to hoist into `$defs` because they
    /// participate in a cycle. Subsequent occurrences emit a `$ref`.
    pub hoisted: AHashSet<String>,
    /// Captured warnings (e.g. untyped fn params used as tools).
    pub warnings: Vec<String>,
}

impl SchemaContext {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Sanitize a type name for use as a JSON Pointer fragment in `$defs`.
fn sanitize_def_name(name: &str) -> String {
    name.trim_start_matches("::")
        .replace('/', "_")
        .replace("::", "_")
}

/// Convert a Hot type annotation string to JSON Schema with type registry
pub fn type_to_json_schema_with_registry(type_str: &str, registry: &TypeRegistry) -> Val {
    let mut ctx = SchemaContext::new();
    let schema = type_to_schema_ctx(type_str, registry, &mut ctx);
    wrap_with_defs(schema, &ctx)
}

/// Like `type_to_json_schema_with_registry` but exposes the underlying
/// `SchemaContext` so callers can collect warnings or merge `$defs`.
pub fn type_to_json_schema_with_ctx(
    type_str: &str,
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    type_to_schema_ctx(type_str, registry, ctx)
}

/// Wrap a top-level schema with a `$defs` section if any cyclic types
/// were hoisted during traversal.
///
/// If the top-level schema is itself just a `$ref` into our own `$defs`
/// (which happens when the caller asked for a recursive type directly,
/// e.g. `type_to_json_schema_with_registry("Tree", ...)`), the
/// definition is inlined at the top level. The `$defs` entry is kept
/// so that inner self-references still resolve. This makes the schema
/// usable by validators and MCP clients that don't (or won't) follow a
/// top-level `$ref`, without sacrificing strict JSON Schema validity.
fn wrap_with_defs(schema: Val, ctx: &SchemaContext) -> Val {
    if ctx.defs.is_empty() {
        return schema;
    }
    let inlined = inline_top_level_ref(schema, &ctx.defs);
    if let Val::Map(mut existing) = inlined {
        existing.insert(Val::from("$defs"), Val::Map(Box::new(ctx.defs.clone())));
        return Val::Map(existing);
    }
    let mut wrapped = IndexMap::new();
    wrapped.insert(Val::from("$defs"), Val::Map(Box::new(ctx.defs.clone())));
    wrapped.insert(Val::from("schema"), inlined);
    Val::Map(Box::new(wrapped))
}

/// If `schema` is exactly `{"$ref": "#/$defs/<name>"}` and `<name>` is
/// present in `defs`, return the def's contents instead. Otherwise
/// returns `schema` unchanged.
fn inline_top_level_ref(schema: Val, defs: &IndexMap<Val, Val>) -> Val {
    let map = match &schema {
        Val::Map(m) => m,
        _ => return schema,
    };
    if map.len() != 1 {
        return schema;
    }
    let ref_str = match map.get(&Val::from("$ref")) {
        Some(Val::Str(s)) => s.clone(),
        _ => return schema,
    };
    let name = match (*ref_str).strip_prefix("#/$defs/") {
        Some(n) => n,
        None => return schema,
    };
    if let Some(def_schema) = defs.get(&Val::from(name)) {
        return def_schema.clone();
    }
    schema
}

/// Convert a Hot type annotation string to JSON Schema (without registry)
pub fn type_to_json_schema(type_str: &str) -> Val {
    let registry = TypeRegistry::new();
    type_to_json_schema_with_registry(type_str, &registry)
}

/// Internal entry point used by `with_ctx` callers.
fn type_to_schema_ctx(type_str: &str, registry: &TypeRegistry, ctx: &mut SchemaContext) -> Val {
    let type_str = type_str.trim();

    if type_str.is_empty() || type_str == "Any" {
        return Val::map_empty();
    }

    if let Some(union_parts) = parse_union(type_str) {
        return union_to_schema_ctx(&union_parts, registry, ctx);
    }

    if let Some(inner_type) = type_str.strip_suffix('?') {
        return optional_to_schema_ctx(inner_type, registry, ctx);
    }

    if let Some(literal) = LiteralType::parse(type_str) {
        return literals_to_schema(&[literal]);
    }

    if let Some((base, params)) = parse_generic(type_str) {
        return generic_to_schema_ctx(&base, &params, registry, ctx);
    }

    if is_builtin_type(type_str) {
        return builtin_to_schema(type_str);
    }

    custom_type_to_schema_ctx(type_str, registry, ctx)
}

/// Check if a type string is a built-in type
fn is_builtin_type(type_str: &str) -> bool {
    matches!(
        type_str,
        "Str"
            | "String"
            | "Int"
            | "Integer"
            | "Dec"
            | "Number"
            | "Bool"
            | "Boolean"
            | "Null"
            | "Vec"
            | "Map"
            | "Any"
            | "Bytes"
            | "Date"
            | "DateTime"
            | "Time"
            | "Duration"
            | "Uuid"
            | "Uri"
            | "Url"
    )
}

/// Parse union type "T | U | V" into parts. A `|` inside a string literal
/// (`"a|b"`) does not split.
fn parse_union(type_str: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = split_top_level(type_str, '|')
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();

    if parts.len() > 1 { Some(parts) } else { None }
}

/// Parse generic type "Vec<T>" into base and params
fn parse_generic(type_str: &str) -> Option<(String, Vec<String>)> {
    let start = type_str.find('<')?;
    let end = type_str.rfind('>')?;

    if end <= start {
        return None;
    }

    let base = type_str[..start].trim().to_string();
    let params = split_top_level(&type_str[start + 1..end], ',')
        .into_iter()
        .filter(|param| !param.is_empty())
        .map(str::to_string)
        .collect();

    Some((base, params))
}

/// Convert union types to JSON Schema "anyOf"
fn union_to_schema_ctx(parts: &[String], registry: &TypeRegistry, ctx: &mut SchemaContext) -> Val {
    if let Some(literals) = parts
        .iter()
        .map(|p| LiteralType::parse(p))
        .collect::<Option<Vec<_>>>()
    {
        return literals_to_schema(&literals);
    }

    let schemas: Vec<Val> = parts
        .iter()
        .map(|p| type_to_schema_ctx(p, registry, ctx))
        .collect();
    let mut map = IndexMap::new();
    map.insert(Val::from("anyOf"), Val::Vec(schemas));
    Val::Map(Box::new(map))
}

/// Convert literal members to typed `enum` schemas, one per JSON type
/// (`1 | 2.5` shares a `number` enum). A lone group is returned as is;
/// several groups, or a `null` member, become an `anyOf` like `T?` does.
fn literals_to_schema(literals: &[LiteralType]) -> Val {
    let has_dec = literals.iter().any(|l| matches!(l, LiteralType::Dec(_)));
    let mut groups: IndexMap<&str, Vec<Val>> = IndexMap::new();
    let mut has_null = false;

    for literal in literals {
        let (json_type, val) = match literal {
            LiteralType::Str(s) => ("string", Val::from(s.as_str())),
            LiteralType::Int(n) if has_dec => ("number", Val::Int(*n)),
            LiteralType::Int(n) => ("integer", Val::Int(*n)),
            LiteralType::Dec(d) => match d.parse::<D256>() {
                Ok(dec) => ("number", Val::Dec(dec)),
                Err(_) => continue,
            },
            LiteralType::Bool(b) => ("boolean", Val::Bool(*b)),
            LiteralType::Null => {
                has_null = true;
                continue;
            }
        };
        let members = groups.entry(json_type).or_default();
        if !members.contains(&val) {
            members.push(val);
        }
    }

    let mut schemas: Vec<Val> = groups
        .into_iter()
        .map(|(json_type, members)| {
            let mut map = IndexMap::new();
            map.insert(Val::from("type"), Val::from(json_type));
            map.insert(Val::from("enum"), Val::Vec(members));
            Val::Map(Box::new(map))
        })
        .collect();
    if has_null {
        schemas.push(builtin_to_schema("Null"));
    }

    if schemas.len() == 1 {
        return schemas.remove(0);
    }
    let mut map = IndexMap::new();
    map.insert(Val::from("anyOf"), Val::Vec(schemas));
    Val::Map(Box::new(map))
}

/// Convert optional type to JSON Schema with null union
fn optional_to_schema_ctx(
    inner_type: &str,
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    let inner_schema = type_to_schema_ctx(inner_type, registry, ctx);
    let null_schema = builtin_to_schema("Null");
    let mut map = IndexMap::new();
    map.insert(
        Val::from("anyOf"),
        Val::Vec(vec![inner_schema, null_schema]),
    );
    Val::Map(Box::new(map))
}

/// Convert generic types to JSON Schema
fn generic_to_schema_ctx(
    base: &str,
    params: &[String],
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    match base {
        "Vec" => {
            let items_schema = if params.is_empty() {
                Val::map_empty()
            } else {
                type_to_schema_ctx(&params[0], registry, ctx)
            };
            let mut map = IndexMap::new();
            map.insert(Val::from("type"), Val::from("array"));
            map.insert(Val::from("items"), items_schema);
            Val::Map(Box::new(map))
        }
        "Map" => {
            let value_schema = if params.len() >= 2 {
                type_to_schema_ctx(&params[1], registry, ctx)
            } else {
                Val::map_empty()
            };
            let mut map = IndexMap::new();
            map.insert(Val::from("type"), Val::from("object"));
            map.insert(Val::from("additionalProperties"), value_schema);
            Val::Map(Box::new(map))
        }
        "Result" => {
            if params.is_empty() {
                Val::map_empty()
            } else {
                type_to_schema_ctx(&params[0], registry, ctx)
            }
        }
        _ => custom_type_to_schema_ctx(base, registry, ctx),
    }
}

/// Convert built-in types to JSON Schema
fn builtin_to_schema(type_str: &str) -> Val {
    let mut map = IndexMap::new();

    match type_str {
        "Str" | "String" => {
            map.insert(Val::from("type"), Val::from("string"));
        }
        "Int" | "Integer" => {
            map.insert(Val::from("type"), Val::from("integer"));
        }
        "Dec" | "Number" => {
            map.insert(Val::from("type"), Val::from("number"));
        }
        "Bool" | "Boolean" => {
            map.insert(Val::from("type"), Val::from("boolean"));
        }
        "Null" => {
            map.insert(Val::from("type"), Val::from("null"));
        }
        "Vec" => {
            map.insert(Val::from("type"), Val::from("array"));
        }
        "Map" => {
            map.insert(Val::from("type"), Val::from("object"));
        }
        "Any" => {
            return Val::map_empty();
        }
        // Format-tagged scalars — JSON Schema "format" provides
        // additional semantic info while keeping the type string-shaped
        // on the wire (matching Hot's serialization conventions).
        "Bytes" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("byte"));
        }
        "Date" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("date"));
        }
        "DateTime" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("date-time"));
        }
        "Time" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("time"));
        }
        "Duration" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("duration"));
        }
        "Uuid" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("uuid"));
        }
        "Uri" | "Url" => {
            map.insert(Val::from("type"), Val::from("string"));
            map.insert(Val::from("format"), Val::from("uri"));
        }
        _ => {
            // This shouldn't be called for non-builtin types
            map.insert(Val::from("type"), Val::from("object"));
        }
    }

    Val::Map(Box::new(map))
}

/// Convert a custom type to JSON Schema by looking up its definition
fn custom_type_to_schema_ctx(
    type_name: &str,
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    let resolved_key = registry
        .get_with_key(type_name)
        .map(|(k, _)| k)
        .unwrap_or_else(|| type_name.to_string());

    // Cycle detected — hoist this type into $defs and emit a $ref.
    if ctx.visited.contains(&resolved_key) {
        ctx.hoisted.insert(resolved_key.clone());
        return ref_to(&resolved_key);
    }

    // Already-hoisted (from a prior cycle): emit $ref directly.
    if ctx.hoisted.contains(&resolved_key) {
        return ref_to(&resolved_key);
    }

    ctx.visited.insert(resolved_key.clone());

    let schema = if let Some(type_info) = registry.get(&resolved_key) {
        let schema = if let Some(ref literals) = type_info.literals {
            literals_to_schema(literals)
        } else if let Some(ref alias) = type_info.type_alias {
            type_to_schema_ctx(alias, registry, ctx)
        } else if let Some(ref variants) = type_info.variants {
            enum_to_schema_ctx(&resolved_key, variants, registry, ctx)
        } else if let Some(ref fields) = type_info.fields {
            fields_to_schema_ctx(fields, registry, ctx)
        } else {
            let mut map = IndexMap::new();
            map.insert(Val::from("type"), Val::from("object"));
            Val::Map(Box::new(map))
        };
        match &type_info.refinement {
            Some(refinement) => apply_refinement(schema, refinement),
            None => schema,
        }
    } else {
        let mut map = IndexMap::new();
        map.insert(Val::from("type"), Val::from("object"));
        map.insert(
            Val::from("description"),
            Val::from(format!("Custom type: {}", type_name)),
        );
        Val::Map(Box::new(map))
    };

    ctx.visited.remove(&resolved_key);

    // If this type ended up hoisted (it's part of a cycle), record the
    // expanded schema in $defs and emit a $ref at this position too.
    if ctx.hoisted.contains(&resolved_key) {
        let def_key = Val::from(sanitize_def_name(&resolved_key));
        ctx.defs.entry(def_key).or_insert_with(|| schema.clone());
        return ref_to(&resolved_key);
    }

    schema
}

/// Add a refinement's keywords to its base type's schema. Length bounds
/// become `minItems`/`maxItems` on arrays. On strings they are described
/// instead: Hot's `length` counts UTF-8 bytes, and `minLength`/`maxLength`
/// count characters. A predicate with no schema form is described.
fn apply_refinement(schema: Val, refinement: &RefinementSchema) -> Val {
    let Val::Map(mut map) = schema else {
        return schema;
    };
    let base_type = map
        .get(&Val::from("type"))
        .and_then(|t| match t {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mut describe = refinement.describe;
    let mut byte_length = false;
    for (keyword, value) in &refinement.keywords {
        let keyword = match (keyword.as_str(), base_type.as_str()) {
            ("$minLength" | "$maxLength", "string") => {
                describe = true;
                byte_length = true;
                continue;
            }
            ("$minLength", "array") => "minItems",
            ("$maxLength", "array") => "maxItems",
            ("$minLength" | "$maxLength", _) => {
                describe = true;
                continue;
            }
            (other, _) => other,
        };
        map.insert(Val::from(keyword), value.clone());
    }
    if describe {
        let mut text = format!("Must satisfy: {}", refinement.source);
        if byte_length {
            text.push_str(" (length counts UTF-8 bytes)");
        }
        let description = match map.get(&Val::from("description")) {
            Some(Val::Str(existing)) => format!("{existing}. {text}"),
            _ => text,
        };
        map.insert(Val::from("description"), Val::from(description));
    }
    Val::Map(map)
}

/// The JSON Schema form of a `where` predicate. `param` is the predicate's
/// parameter (or `%`). Comparisons with literal bounds, `between`, length
/// bounds, `::hot::regex/is-match` with a literal pattern, `in` with a
/// literal list, `eq` with a literal, and `and` of those become keywords;
/// anything else makes the schema describe the predicate instead.
pub fn refinement_schema(body: &Value, param: &str, source: &str) -> RefinementSchema {
    let mut keywords = Vec::new();
    let canonical = canonical_predicate(body, param, &mut keywords);
    RefinementSchema {
        keywords,
        source: source.to_string(),
        describe: !canonical,
    }
}

/// The candidate itself, or `length` of it.
enum Subject {
    Value,
    Length,
}

fn is_param(value: &Value, param: &str) -> bool {
    match value {
        Value::Placeholder(1) => true,
        Value::Ref(Ref::Var(var_ref)) => {
            var_ref.var.deep_path.is_none()
                && (var_ref.var.sym.name() == param
                    || var_ref.var.sym.name() == "__placeholder_1__")
        }
        _ => false,
    }
}

/// The namespaces whose function a predicate's schema keyword stands for,
/// by function name.
fn canonical_predicate_namespaces(name: &str) -> &'static [&'static str] {
    match name {
        "between" | "eq" | "gt" | "gte" | "in" | "lt" | "lte" => &["::hot::cmp"],
        "and" => &["::hot::bool"],
        "length" => &["::hot::coll", "::hot::str"],
        "is-match" => &["::hot::regex"],
        _ => &[],
    }
}

/// Whether every call in a `where` predicate body that a schema keyword or
/// the fast check would read (`gt`, `between`, `length`, `is-match`, ...)
/// names that standard function, not another one with the same name: one
/// defined or imported in `namespace`, or reached through an alias of
/// another namespace. Reading a lookalike as the standard function would
/// enforce or advertise the wrong rule.
pub fn predicate_calls_are_canonical(
    body: &Value,
    namespace: &crate::lang::ast::Namespace,
) -> bool {
    let canonical_ns = |name: &str, ns: &crate::lang::ast::NsPath| {
        let ns = namespace.aliases.get(ns).unwrap_or(ns).to_string();
        canonical_predicate_namespaces(name).contains(&ns.as_str())
    };
    match body {
        Value::FnCall(call) => {
            let callee = match call.function.as_ref() {
                Value::Ref(Ref::Ns(ns_ref)) => match ns_ref.function_name.as_deref() {
                    Some(name) if !canonical_predicate_namespaces(name).is_empty() => {
                        canonical_ns(name, &ns_ref.ns)
                    }
                    _ => true,
                },
                Value::Ref(Ref::Var(var_ref)) => {
                    let name = var_ref.var.sym.name();
                    if canonical_predicate_namespaces(name).is_empty() {
                        true
                    } else {
                        let mut local = namespace
                            .scope
                            .vars
                            .iter()
                            .filter(|(var, _)| var.sym.name() == name)
                            .peekable();
                        if local.peek().is_none() {
                            // `is-match` isn't a core function; the others are.
                            name != "is-match"
                        } else {
                            local.all(|(_, value)| {
                                matches!(value, Value::Ref(Ref::Ns(ns_ref))
                                    if ns_ref.function_name.as_deref() == Some(name)
                                        && canonical_ns(name, &ns_ref.ns))
                            })
                        }
                    }
                }
                _ => true,
            };
            callee
                && call
                    .args
                    .iter()
                    .all(|arg| predicate_calls_are_canonical(&arg.value, namespace))
        }
        Value::Flow(flow) => flow
            .expressions
            .iter()
            .all(|expr| predicate_calls_are_canonical(expr, namespace)),
        _ => true,
    }
}

fn callee_name(value: &Value) -> Option<&str> {
    match value {
        Value::Ref(Ref::Var(var_ref)) => Some(var_ref.var.sym.name()),
        Value::Ref(Ref::Ns(ns_ref)) => ns_ref.function_name.as_deref(),
        _ => None,
    }
}

fn subject(value: &Value, param: &str) -> Option<Subject> {
    if is_param(value, param) {
        return Some(Subject::Value);
    }
    match value {
        Value::FnCall(call)
            if callee_name(&call.function) == Some("length")
                && call.args.len() == 1
                && is_param(&call.args[0].value, param) =>
        {
            Some(Subject::Length)
        }
        _ => None,
    }
}

fn number(value: &Value) -> Option<Val> {
    match value {
        Value::Val(v @ (Val::Int(_) | Val::Dec(_)), _) => Some(v.clone()),
        _ => None,
    }
}

fn literal_value(value: &Value) -> Option<Val> {
    match value {
        Value::Val(v @ (Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null), _) => {
            Some(v.clone())
        }
        _ => None,
    }
}

fn canonical_predicate(expr: &Value, param: &str, out: &mut Vec<(String, Val)>) -> bool {
    let Value::FnCall(call) = expr else {
        return match expr {
            Value::Flow(flow) if flow.expressions.len() == 1 => {
                canonical_predicate(&flow.expressions[0], param, out)
            }
            _ => false,
        };
    };
    let args: Vec<&Value> = call.args.iter().map(|a| &a.value).collect();
    let mut push = |keyword: &str, value: Val| out.push((keyword.to_string(), value));
    match (
        callee_name(&call.function).unwrap_or_default(),
        args.as_slice(),
    ) {
        ("and", parts) => {
            // Every part contributes what it can; one without a schema form
            // makes the whole predicate described too.
            let mut all = true;
            for part in parts {
                all &= canonical_predicate(part, param, out);
            }
            all
        }
        (op @ ("gt" | "gte" | "lt" | "lte"), [a, b]) => {
            // `op(subject, bound)`, or `op(bound, subject)` reversed.
            let (subj, bound, op) = match (subject(a, param), number(b)) {
                (Some(subj), Some(bound)) => (subj, bound, op),
                _ => match (number(a), subject(b, param)) {
                    (Some(bound), Some(subj)) => (
                        subj,
                        bound,
                        match op {
                            "gt" => "lt",
                            "gte" => "lte",
                            "lt" => "gt",
                            _ => "gte",
                        },
                    ),
                    _ => return false,
                },
            };
            match subj {
                Subject::Value => push(
                    match op {
                        "gt" => "exclusiveMinimum",
                        "gte" => "minimum",
                        "lt" => "exclusiveMaximum",
                        _ => "maximum",
                    },
                    bound,
                ),
                Subject::Length => {
                    let Val::Int(n) = bound else {
                        return false;
                    };
                    match op {
                        "gt" => push("$minLength", Val::Int(n + 1)),
                        "gte" => push("$minLength", Val::Int(n)),
                        "lt" => push("$maxLength", Val::Int(n - 1)),
                        _ => push("$maxLength", Val::Int(n)),
                    }
                }
            }
            true
        }
        ("between", [x, low, high]) => {
            let (Some(subj), Some(low), Some(high)) =
                (subject(x, param), number(low), number(high))
            else {
                return false;
            };
            match subj {
                Subject::Value => {
                    push("minimum", low);
                    push("maximum", high);
                }
                Subject::Length => {
                    push("$minLength", low);
                    push("$maxLength", high);
                }
            }
            true
        }
        ("in", [x, Value::Val(Val::Vec(items), _)]) if is_param(x, param) => {
            let literals: Option<Vec<Val>> = items
                .iter()
                .map(|item| match item {
                    Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => {
                        Some(item.clone())
                    }
                    _ => None,
                })
                .collect();
            match literals {
                Some(values) => {
                    push("enum", Val::Vec(values));
                    true
                }
                None => false,
            }
        }
        ("eq", [a, b]) => match (
            is_param(a, param),
            literal_value(b),
            literal_value(a),
            is_param(b, param),
        ) {
            (true, Some(value), _, _) | (_, _, Some(value), true) => {
                push("const", value);
                true
            }
            _ => false,
        },
        ("is-match", [x, Value::Val(Val::Str(pattern), _)]) if is_param(x, param) => {
            push("pattern", Val::Str(pattern.clone()));
            true
        }
        _ => false,
    }
}

/// Build a `{"$ref": "#/$defs/<sanitized>"}` Val.
fn ref_to(type_name: &str) -> Val {
    let mut map = IndexMap::new();
    map.insert(
        Val::from("$ref"),
        Val::from(format!("#/$defs/{}", sanitize_def_name(type_name))),
    );
    Val::Map(Box::new(map))
}

/// Render an enum's `VariantInfo` list as JSON Schema.
///
/// - All-unit variants collapse into `{enum: [v1, v2, ...]}` (unchanged).
/// - Mixed/payload-bearing variants become a discriminated `oneOf`
///   matching Hot's runtime `{$type: "Enum.Variant", $val: <payload>}`
///   shape, where unit variants only require `$type`.
fn enum_to_schema_ctx(
    enum_name: &str,
    variants: &[VariantInfo],
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    let any_payload = variants.iter().any(|v| v.type_ref.is_some());
    if !any_payload {
        let mut map = IndexMap::new();
        let names: Vec<Val> = variants
            .iter()
            .map(|v| Val::from(v.name.as_str()))
            .collect();
        map.insert(Val::from("enum"), Val::Vec(names));
        return Val::Map(Box::new(map));
    }

    let one_of: Vec<Val> = variants
        .iter()
        .map(|v| {
            let qualified_name = format!("{}.{}", enum_name, v.name);

            let mut props = IndexMap::new();
            let mut tag_map = IndexMap::new();
            tag_map.insert(Val::from("const"), Val::from(qualified_name.as_str()));
            props.insert(Val::from("$type"), Val::Map(Box::new(tag_map)));

            let mut required = vec![Val::from("$type")];

            if let Some(ref payload_type) = v.type_ref {
                let payload_schema = type_to_schema_ctx(payload_type, registry, ctx);
                props.insert(Val::from("$val"), payload_schema);
                required.push(Val::from("$val"));
            }

            let mut variant_schema = IndexMap::new();
            variant_schema.insert(Val::from("type"), Val::from("object"));
            variant_schema.insert(Val::from("properties"), Val::Map(Box::new(props)));
            variant_schema.insert(Val::from("required"), Val::Vec(required));
            Val::Map(Box::new(variant_schema))
        })
        .collect();

    let mut schema = IndexMap::new();
    schema.insert(Val::from("oneOf"), Val::Vec(one_of));
    let mut discriminator = IndexMap::new();
    discriminator.insert(Val::from("propertyName"), Val::from("$type"));
    schema.insert(
        Val::from("discriminator"),
        Val::Map(Box::new(discriminator)),
    );
    Val::Map(Box::new(schema))
}

/// Convert type fields to JSON Schema object
fn fields_to_schema_ctx(
    fields: &[TypeField],
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    let mut properties = IndexMap::new();
    let mut required = Vec::new();

    for field in fields {
        let field_name = field.name.to_string();
        let type_ann = &field.type_annotation;

        let is_optional = type_ann.ends_with('?');
        let field_schema = type_to_schema_ctx(type_ann, registry, ctx);

        properties.insert(Val::from(field_name.clone()), field_schema);

        if !is_optional {
            required.push(Val::from(field_name));
        }
    }

    let mut schema = IndexMap::new();
    schema.insert(Val::from("type"), Val::from("object"));
    schema.insert(Val::from("properties"), Val::Map(Box::new(properties)));

    if !required.is_empty() {
        schema.insert(Val::from("required"), Val::Vec(required));
    }

    Val::Map(Box::new(schema))
}

/// Generate input schema from function arguments with type registry
pub fn args_to_input_schema_with_registry(args: &[FnArg], registry: &TypeRegistry) -> Val {
    let mut ctx = SchemaContext::new();
    let schema = args_to_input_schema_with_ctx(args, registry, &mut ctx);
    wrap_with_defs(schema, &ctx)
}

/// Generate input schema from function arguments using a shared
/// `SchemaContext` (for callers that need to collect warnings or merge `$defs`).
pub fn args_to_input_schema_with_ctx(
    args: &[FnArg],
    registry: &TypeRegistry,
    ctx: &mut SchemaContext,
) -> Val {
    if args.is_empty() {
        let mut map = IndexMap::new();
        map.insert(Val::from("type"), Val::from("object"));
        map.insert(Val::from("properties"), Val::map_empty());
        return Val::Map(Box::new(map));
    }

    let mut properties = IndexMap::new();
    let mut required = Vec::new();

    for arg in args {
        let arg_name = arg.var.sym.name().to_string();

        let type_schema = if let Some(ref type_ann) = arg.type_annotation {
            let is_optional = type_ann.ends_with('?');
            if !is_optional {
                required.push(Val::from(arg_name.clone()));
            }
            type_to_schema_ctx(type_ann, registry, ctx)
        } else {
            // Untyped fn param used as a tool — emit a warning so callers
            // can surface this to the developer (tools should be fully typed).
            ctx.warnings.push(format!(
                "Tool parameter `{}` has no type annotation; \
                 schema falls back to `Any`. Add an explicit type \
                 (e.g. `{}: Str`) for a better tool schema.",
                arg_name, arg_name
            ));
            Val::map_empty()
        };

        properties.insert(Val::from(arg_name), type_schema);
    }

    let mut schema = IndexMap::new();
    schema.insert(Val::from("type"), Val::from("object"));
    schema.insert(Val::from("properties"), Val::Map(Box::new(properties)));

    if !required.is_empty() {
        schema.insert(Val::from("required"), Val::Vec(required));
    }

    Val::Map(Box::new(schema))
}

/// Generate input schema from function arguments (without registry)
pub fn args_to_input_schema(args: &[FnArg]) -> Val {
    let registry = TypeRegistry::new();
    args_to_input_schema_with_registry(args, &registry)
}

/// Generate output schema from return type annotation with type registry
pub fn return_type_to_output_schema_with_registry(
    return_type: Option<&str>,
    registry: &TypeRegistry,
) -> Option<Val> {
    match return_type {
        Some(rt) if !rt.is_empty() && rt != "Any" => {
            Some(type_to_json_schema_with_registry(rt, registry))
        }
        _ => None,
    }
}

/// Generate output schema from return type annotation (without registry)
pub fn return_type_to_output_schema(return_type: Option<&str>) -> Option<Val> {
    let registry = TypeRegistry::new();
    return_type_to_output_schema_with_registry(return_type, &registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::{Sym, TypeExpr};
    use serde_json::json;

    #[test]
    fn test_builtin_types() {
        let schema = type_to_json_schema("Str");
        assert_eq!(schema.get_str("type"), "string");

        let schema = type_to_json_schema("Int");
        assert_eq!(schema.get_str("type"), "integer");

        let schema = type_to_json_schema("Bool");
        assert_eq!(schema.get_str("type"), "boolean");
    }

    #[test]
    fn test_vec_type() {
        let schema = type_to_json_schema("Vec<Str>");
        assert_eq!(schema.get_str("type"), "array");

        if let Some(items) = schema.get("items") {
            assert_eq!(items.get_str("type"), "string");
        } else {
            panic!("Expected items in array schema");
        }
    }

    #[test]
    fn test_optional_type() {
        let schema = type_to_json_schema("Str?");

        if let Some(Val::Vec(any_of)) = schema.get("anyOf") {
            assert_eq!(any_of.len(), 2);
        } else {
            panic!("Expected anyOf for optional type");
        }
    }

    #[test]
    fn test_union_type() {
        let schema = type_to_json_schema("Str | Int");

        if let Some(Val::Vec(any_of)) = schema.get("anyOf") {
            assert_eq!(any_of.len(), 2);
        } else {
            panic!("Expected anyOf for union type");
        }
    }

    #[test]
    fn test_custom_type_with_registry() {
        let mut registry = TypeRegistry::new();

        // Register a User type
        let user_type = TypeDef {
            name: Sym::String("User".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("id".to_string()),
                    type_annotation: "Int".to_string(),
                    type_expr: TypeExpr::Simple("Int".to_string()),
                },
                TypeField {
                    name: Sym::String("name".to_string()),
                    type_annotation: "Str".to_string(),
                    type_expr: TypeExpr::Simple("Str".to_string()),
                },
                TypeField {
                    name: Sym::String("email".to_string()),
                    type_annotation: "Str?".to_string(),
                    type_expr: TypeExpr::Optional(Box::new(TypeExpr::Simple("Str".to_string()))),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("User", &user_type);

        let schema = type_to_json_schema_with_registry("User", &registry);

        assert_eq!(schema.get_str("type"), "object");

        if let Some(props) = schema.get("properties") {
            assert!(props.get("id").is_some());
            assert!(props.get("name").is_some());
            assert!(props.get("email").is_some());
        } else {
            panic!("Expected properties in schema");
        }

        if let Some(Val::Vec(required)) = schema.get("required") {
            // email is optional, so only id and name should be required
            assert_eq!(required.len(), 2);
        } else {
            panic!("Expected required array");
        }
    }

    #[test]
    fn test_nested_custom_types() {
        let mut registry = TypeRegistry::new();

        // Register Address type
        let address_type = TypeDef {
            name: Sym::String("Address".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("street".to_string()),
                    type_annotation: "Str".to_string(),
                    type_expr: TypeExpr::Simple("Str".to_string()),
                },
                TypeField {
                    name: Sym::String("city".to_string()),
                    type_annotation: "Str".to_string(),
                    type_expr: TypeExpr::Simple("Str".to_string()),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Address", &address_type);

        // Register User type with Address field
        let user_type = TypeDef {
            name: Sym::String("User".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("name".to_string()),
                    type_annotation: "Str".to_string(),
                    type_expr: TypeExpr::Simple("Str".to_string()),
                },
                TypeField {
                    name: Sym::String("address".to_string()),
                    type_annotation: "Address".to_string(),
                    type_expr: TypeExpr::Simple("Address".to_string()),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("User", &user_type);

        let schema = type_to_json_schema_with_registry("User", &registry);

        // Check that address is expanded
        if let Some(props) = schema.get("properties") {
            if let Some(addr_schema) = props.get("address") {
                assert_eq!(addr_schema.get_str("type"), "object");
                // Address should have its own properties
                if let Some(addr_props) = addr_schema.get("properties") {
                    assert!(addr_props.get("street").is_some());
                    assert!(addr_props.get("city").is_some());
                } else {
                    panic!("Expected address properties");
                }
            } else {
                panic!("Expected address field");
            }
        } else {
            panic!("Expected properties");
        }
    }

    #[test]
    fn test_format_hints_for_special_builtins() {
        let cases = [
            ("Bytes", "string", "byte"),
            ("Date", "string", "date"),
            ("DateTime", "string", "date-time"),
            ("Time", "string", "time"),
            ("Duration", "string", "duration"),
            ("Uuid", "string", "uuid"),
            ("Uri", "string", "uri"),
            ("Url", "string", "uri"),
        ];
        for (name, expected_type, expected_format) in cases {
            let schema = type_to_json_schema(name);
            assert_eq!(
                schema.get_str("type"),
                expected_type,
                "type mismatch for {name}"
            );
            assert_eq!(
                schema.get_str("format"),
                expected_format,
                "format mismatch for {name}"
            );
        }
    }

    #[test]
    fn test_unit_only_enum_collapses_to_enum() {
        let mut registry = TypeRegistry::new();
        let direction = TypeDef {
            name: Sym::String("Direction".to_string()),
            fields: None,
            type_alias: None,
            variants: Some(vec![
                crate::lang::ast::VariantDef {
                    name: Sym::String("Up".to_string()),
                    type_ref: None,
                },
                crate::lang::ast::VariantDef {
                    name: Sym::String("Down".to_string()),
                    type_ref: None,
                },
            ]),
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Direction", &direction);

        let schema = type_to_json_schema_with_registry("Direction", &registry);
        let enum_vals = schema.get("enum").expect("expected enum array");
        if let Val::Vec(vs) = enum_vals {
            assert_eq!(vs.len(), 2);
        } else {
            panic!("expected enum to be a vec");
        }
    }

    #[test]
    fn test_payload_enum_renders_discriminated_oneof() {
        let mut registry = TypeRegistry::new();
        let circle = TypeDef {
            name: Sym::String("Circle".to_string()),
            fields: Some(vec![TypeField {
                name: Sym::String("radius".to_string()),
                type_annotation: "Dec".to_string(),
                type_expr: TypeExpr::Simple("Dec".to_string()),
            }]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Circle", &circle);

        let shape = TypeDef {
            name: Sym::String("Shape".to_string()),
            fields: None,
            type_alias: None,
            variants: Some(vec![
                crate::lang::ast::VariantDef {
                    name: Sym::String("Circle".to_string()),
                    type_ref: Some("Circle".to_string()),
                },
                crate::lang::ast::VariantDef {
                    name: Sym::String("Point".to_string()),
                    type_ref: None,
                },
            ]),
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Shape", &shape);

        let schema = type_to_json_schema_with_registry("Shape", &registry);

        let one_of = schema.get("oneOf").expect("expected oneOf");
        let one_of_vec = match one_of {
            Val::Vec(v) => v,
            _ => panic!("oneOf must be a vec"),
        };
        assert_eq!(one_of_vec.len(), 2);

        let discriminator = schema.get("discriminator").expect("expected discriminator");
        assert_eq!(discriminator.get_str("propertyName"), "$type");

        // Circle variant has $val payload
        let circle_variant = &one_of_vec[0];
        let circle_props = circle_variant
            .get("properties")
            .expect("circle variant missing properties");
        assert!(circle_props.get("$type").is_some());
        assert!(circle_props.get("$val").is_some());

        // Point variant has only $type
        let point_variant = &one_of_vec[1];
        let point_props = point_variant
            .get("properties")
            .expect("point variant missing properties");
        assert!(point_props.get("$type").is_some());
        assert!(point_props.get("$val").is_none());
    }

    #[test]
    fn test_recursive_type_hoists_to_defs() {
        let mut registry = TypeRegistry::new();

        // type Tree { value: Int, children: Vec<Tree> }
        let tree = TypeDef {
            name: Sym::String("Tree".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("value".to_string()),
                    type_annotation: "Int".to_string(),
                    type_expr: TypeExpr::Simple("Int".to_string()),
                },
                TypeField {
                    name: Sym::String("children".to_string()),
                    type_annotation: "Vec<Tree>".to_string(),
                    type_expr: TypeExpr::Generic(
                        "Vec".to_string(),
                        vec![TypeExpr::Simple("Tree".to_string())],
                    ),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Tree", &tree);

        let schema = type_to_json_schema_with_registry("Tree", &registry);

        // Top-level should be the *inlined* Tree definition, not a bare
        // `$ref`. The `$defs` entry is kept so the inner self-reference
        // (`children.items`) still resolves.
        assert_eq!(
            schema.get_str("type"),
            "object",
            "top-level should be inlined object schema, got: {:?}",
            schema
        );
        assert!(
            schema.get("$ref").is_none(),
            "top-level should not be a bare $ref"
        );

        let defs = schema.get("$defs").expect("expected $defs section");
        assert!(defs.get("Tree").is_some(), "Tree must remain in $defs");

        let props = schema.get("properties").expect("expected properties");
        let children = props.get("children").expect("expected children field");
        let items = children.get("items").expect("expected vec items");
        assert_eq!(items.get_str("$ref"), "#/$defs/Tree");
    }

    #[test]
    fn test_recursive_type_referenced_from_outer_struct() {
        let mut registry = TypeRegistry::new();

        // Tree is recursive on its own.
        let tree = TypeDef {
            name: Sym::String("Tree".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("value".to_string()),
                    type_annotation: "Int".to_string(),
                    type_expr: TypeExpr::Simple("Int".to_string()),
                },
                TypeField {
                    name: Sym::String("children".to_string()),
                    type_annotation: "Vec<Tree>".to_string(),
                    type_expr: TypeExpr::Generic(
                        "Vec".to_string(),
                        vec![TypeExpr::Simple("Tree".to_string())],
                    ),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Tree", &tree);

        // Forest *contains* a Tree but isn't itself recursive — the
        // outer schema should be inlined as a normal object, with Tree
        // hoisted into $defs and referenced from the `root` field.
        let forest = TypeDef {
            name: Sym::String("Forest".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("name".to_string()),
                    type_annotation: "Str".to_string(),
                    type_expr: TypeExpr::Simple("Str".to_string()),
                },
                TypeField {
                    name: Sym::String("root".to_string()),
                    type_annotation: "Tree".to_string(),
                    type_expr: TypeExpr::Simple("Tree".to_string()),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Forest", &forest);

        let schema = type_to_json_schema_with_registry("Forest", &registry);

        assert_eq!(schema.get_str("type"), "object");
        assert!(schema.get("$ref").is_none());

        // Tree must be hoisted because it's recursive.
        let defs = schema.get("$defs").expect("expected $defs section");
        assert!(defs.get("Tree").is_some(), "Tree must be in $defs");

        // Forest.root must reference Tree by $ref.
        let props = schema.get("properties").expect("expected properties");
        let root = props.get("root").expect("expected root field");
        assert_eq!(root.get_str("$ref"), "#/$defs/Tree");
    }

    #[test]
    fn test_untyped_fn_param_emits_warning() {
        use crate::lang::ast::{FnArg, Var};

        let registry = TypeRegistry::new();
        let mut ctx = SchemaContext::new();
        let args = vec![FnArg {
            var: Var {
                sym: Sym::String("x".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            lazy: false,
            type_annotation: None,
        }];
        let _ = args_to_input_schema_with_ctx(&args, &registry, &mut ctx);
        assert_eq!(ctx.warnings.len(), 1);
        assert!(ctx.warnings[0].contains("`x`"));
    }

    #[test]
    fn test_vec_of_custom_type() {
        let mut registry = TypeRegistry::new();

        let item_type = TypeDef {
            name: Sym::String("Item".to_string()),
            fields: Some(vec![TypeField {
                name: Sym::String("id".to_string()),
                type_annotation: "Int".to_string(),
                type_expr: TypeExpr::Simple("Int".to_string()),
            }]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
            refinement: None,
        };
        registry.register("Item", &item_type);

        let schema = type_to_json_schema_with_registry("Vec<Item>", &registry);

        assert_eq!(schema.get_str("type"), "array");

        if let Some(items) = schema.get("items") {
            assert_eq!(items.get_str("type"), "object");
            assert!(items.get("properties").is_some());
        } else {
            panic!("Expected items schema");
        }
    }

    fn parse(source: &str) -> crate::lang::ast::Program {
        crate::lang::parser::parse_hot(source).expect("source should parse")
    }

    /// Register the program's types the way `build_type_registry` does.
    fn register_program(registry: &mut TypeRegistry, program: &crate::lang::ast::Program) {
        for (ns_path, namespace) in &program.namespaces {
            for (var, value) in &namespace.scope.vars {
                if let crate::lang::ast::Value::TypeDef(type_def) = value {
                    let qualified_name = format!("{}/{}", ns_path, var.sym.name());
                    registry.register_qualified(&qualified_name, type_def);
                }
            }
        }
    }

    fn registry_from_source(source: &str) -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        register_program(&mut registry, &parse(source));
        registry
    }

    fn schema_json(type_str: &str, registry: &TypeRegistry) -> serde_json::Value {
        serde_json::to_value(type_to_json_schema_with_registry(type_str, registry)).unwrap()
    }

    #[test]
    fn test_literal_union_param_emits_enum() {
        let program = parse(
            r#"
::probe::thirteen ns
Fruit type "apple" | "banana"
search fn (query: Str, fruit: Fruit): Vec<Str> { [] }
"#,
        );
        let mut registry = TypeRegistry::new();
        register_program(&mut registry, &program);
        let fn_def = program
            .namespaces
            .values()
            .flat_map(|ns| ns.scope.vars.iter())
            .find_map(|(var, value)| match value {
                crate::lang::ast::Value::Fn(fn_defs) if var.sym.name() == "search" => {
                    fn_defs.first()
                }
                _ => None,
            })
            .expect("search fn");

        let schema = args_to_input_schema_with_registry(&fn_def.args.args, &registry);
        assert_eq!(
            serde_json::to_value(schema).unwrap(),
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "fruit": {"type": "string", "enum": ["apple", "banana"]}
                },
                "required": ["query", "fruit"]
            })
        );
    }

    #[test]
    fn test_literal_union_aliases_emit_typed_enums() {
        let registry = registry_from_source(
            r#"
::t ns
Roll type 1 | 2 | 3
Ratio type 0.5 | 1.5
Step type 1 | 2.5
Flag type true | false
Sep type "a|b" | "c"
Only type "A"
"#,
        );

        let cases = [
            ("Roll", json!({"type": "integer", "enum": [1, 2, 3]})),
            ("Ratio", json!({"type": "number", "enum": [0.5, 1.5]})),
            ("Step", json!({"type": "number", "enum": [1, 2.5]})),
            ("Flag", json!({"type": "boolean", "enum": [true, false]})),
            ("Sep", json!({"type": "string", "enum": ["a|b", "c"]})),
            ("Only", json!({"type": "string", "enum": ["A"]})),
            ("::t/Roll", json!({"type": "integer", "enum": [1, 2, 3]})),
        ];
        for (type_str, expected) in cases {
            assert_eq!(schema_json(type_str, &registry), expected, "{type_str}");
        }
    }

    #[test]
    fn test_nullable_literal_union() {
        let registry = registry_from_source(
            r#"
::t ns
Size type "s" | "m" | null
Fruit type "apple" | "banana"
Mixed type "auto" | 0 | null
"#,
        );

        let nullable_size = json!({
            "anyOf": [{"type": "string", "enum": ["s", "m"]}, {"type": "null"}]
        });
        assert_eq!(schema_json("Size", &registry), nullable_size);
        assert_eq!(
            schema_json("Fruit?", &registry),
            json!({
                "anyOf": [{"type": "string", "enum": ["apple", "banana"]}, {"type": "null"}]
            })
        );
        assert_eq!(
            schema_json("Mixed", &registry),
            json!({
                "anyOf": [
                    {"type": "string", "enum": ["auto"]},
                    {"type": "integer", "enum": [0]},
                    {"type": "null"}
                ]
            })
        );
    }

    #[test]
    fn test_open_literal_union_accumulates_declarations() {
        let mut registry = registry_from_source(
            r#"
::t ns
HttpMethod type open "GET" | "POST"
HttpMethod type open | "TRACE"
HttpMethod type open "POST" | "OPTIONS"
"#,
        );
        let expected = json!({
            "type": "string",
            "enum": ["GET", "POST", "TRACE", "OPTIONS"]
        });
        assert_eq!(schema_json("HttpMethod", &registry), expected);
        assert_eq!(schema_json("::t/HttpMethod", &registry), expected);

        // A same-named open union in another namespace stays separate.
        register_program(
            &mut registry,
            &parse(
                r#"
::other ns
HttpMethod type open "BREW"
"#,
            ),
        );
        assert_eq!(schema_json("::t/HttpMethod", &registry), expected);
        assert_eq!(
            schema_json("::other/HttpMethod", &registry),
            json!({"type": "string", "enum": ["BREW"]})
        );

        // Redeclaring a closed union replaces it.
        let registry = registry_from_source(
            r#"
::t ns
Fruit type "apple"
Fruit type "banana"
"#,
        );
        assert_eq!(
            schema_json("Fruit", &registry),
            json!({"type": "string", "enum": ["banana"]})
        );
    }

    #[test]
    fn test_inline_literal_annotations() {
        let cases = [
            (
                r#""fast" | "slow""#,
                json!({"type": "string", "enum": ["fast", "slow"]}),
            ),
            (
                r#"Vec<"a|b" | "c,d">"#,
                json!({
                    "type": "array",
                    "items": {"type": "string", "enum": ["a|b", "c,d"]}
                }),
            ),
            (
                "Str | null",
                json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
            ),
            (
                r#""auto" | Int"#,
                json!({
                    "anyOf": [{"type": "string", "enum": ["auto"]}, {"type": "integer"}]
                }),
            ),
        ];
        for (type_str, expected) in cases {
            let schema = serde_json::to_value(type_to_json_schema(type_str)).unwrap();
            assert_eq!(schema, expected, "{type_str}");
        }
    }

    #[test]
    fn test_enum_variants_output_unchanged() {
        let registry = registry_from_source(
            r#"
::t ns
Direction enum { Up, Down }
"#,
        );
        assert_eq!(
            schema_json("Direction", &registry),
            json!({"enum": ["Up", "Down"]})
        );
    }
}
