//! Convert Hot type annotations to JSON Schema
//!
//! This module provides functions to convert Hot type annotations (strings)
//! to JSON Schema format for MCP tool definitions.

use crate::lang::ast::{FnArg, TypeDef, TypeField};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
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
        let info = TypeDefInfo {
            fields: type_def.fields.clone(),
            variants: variants_from_type_def(type_def),
            type_alias: type_def.type_alias.as_ref().map(|ta| ta.to_string()),
        };

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
        let info = TypeDefInfo {
            fields: type_def.fields.clone(),
            variants: variants_from_type_def(type_def),
            type_alias: type_def.type_alias.as_ref().map(|ta| ta.to_string()),
        };

        self.types.insert(qualified_name.to_string(), info.clone());

        // Also register with simple name (last part after /)
        if let Some(simple_name) = qualified_name.rsplit('/').next() {
            self.types.insert(simple_name.to_string(), info);
        }
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

/// Parse union type "T | U | V" into parts
fn parse_union(type_str: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0;

    for ch in type_str.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth -= 1;
                current.push(ch);
            }
            '|' if depth == 0 => {
                let part = current.trim().to_string();
                if !part.is_empty() {
                    parts.push(part);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let part = current.trim().to_string();
    if !part.is_empty() {
        parts.push(part);
    }

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
    let params_str = &type_str[start + 1..end];

    let mut params = Vec::new();
    let mut current = String::new();
    let mut depth = 0;

    for ch in params_str.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let param = current.trim().to_string();
                if !param.is_empty() {
                    params.push(param);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let param = current.trim().to_string();
    if !param.is_empty() {
        params.push(param);
    }

    Some((base, params))
}

/// Convert union types to JSON Schema "anyOf"
fn union_to_schema_ctx(parts: &[String], registry: &TypeRegistry, ctx: &mut SchemaContext) -> Val {
    let schemas: Vec<Val> = parts
        .iter()
        .map(|p| type_to_schema_ctx(p, registry, ctx))
        .collect();
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
        if let Some(ref alias) = type_info.type_alias {
            type_to_schema_ctx(alias, registry, ctx)
        } else if let Some(ref variants) = type_info.variants {
            enum_to_schema_ctx(&resolved_key, variants, registry, ctx)
        } else if let Some(ref fields) = type_info.fields {
            fields_to_schema_ctx(fields, registry, ctx)
        } else {
            let mut map = IndexMap::new();
            map.insert(Val::from("type"), Val::from("object"));
            Val::Map(Box::new(map))
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
    use crate::lang::ast::Sym;

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
                },
                TypeField {
                    name: Sym::String("name".to_string()),
                    type_annotation: "Str".to_string(),
                },
                TypeField {
                    name: Sym::String("email".to_string()),
                    type_annotation: "Str?".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
                },
                TypeField {
                    name: Sym::String("city".to_string()),
                    type_annotation: "Str".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
        };
        registry.register("Address", &address_type);

        // Register User type with Address field
        let user_type = TypeDef {
            name: Sym::String("User".to_string()),
            fields: Some(vec![
                TypeField {
                    name: Sym::String("name".to_string()),
                    type_annotation: "Str".to_string(),
                },
                TypeField {
                    name: Sym::String("address".to_string()),
                    type_annotation: "Address".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
            }]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
                },
                TypeField {
                    name: Sym::String("children".to_string()),
                    type_annotation: "Vec<Tree>".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
                },
                TypeField {
                    name: Sym::String("children".to_string()),
                    type_annotation: "Vec<Tree>".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
                },
                TypeField {
                    name: Sym::String("root".to_string()),
                    type_annotation: "Tree".to_string(),
                },
            ]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
            }]),
            type_alias: None,
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: false,
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
}
