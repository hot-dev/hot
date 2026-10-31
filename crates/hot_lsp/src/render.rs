//! Signatures, hover text and type lookups for definitions.

use crate::index::{Def, Index, Scope};
use crate::syntax::{Local, LocalKind};
use hot::lang::ast::{FlowType, FnDef, TypeDef, Value};
use hot::val::Val;
use tower_lsp::lsp_types::{
    CompletionItemKind, Documentation, ParameterInformation, ParameterLabel, SignatureInformation,
};

fn flow_modifier(flow_type: &FlowType) -> &'static str {
    match flow_type {
        FlowType::Serial => "",
        FlowType::Parallel | FlowType::ParallelShort => " parallel",
        FlowType::Cond | FlowType::CondShort => " cond",
        FlowType::CondAll | FlowType::CondAllShort => " cond-all",
        FlowType::SerialShort => " serial",
        FlowType::Pipe => " pipe",
        FlowType::Match | FlowType::MatchShort => " match",
        FlowType::MatchAll | FlowType::MatchAllShort => " match-all",
    }
}

/// The parameters of one arity, as written (`lazy x: Int`).
pub fn params(index: &Index, fn_def: &FnDef) -> Vec<String> {
    let mut params: Vec<String> = fn_def
        .args
        .args
        .iter()
        .map(|arg| {
            let name = arg.var.sym.name();
            let lazy = if arg.lazy { "lazy " } else { "" };
            match &arg.type_annotation {
                Some(t) => format!("{lazy}{name}: {}", index.display_type(t)),
                None => format!("{lazy}{name}"),
            }
        })
        .collect();
    if fn_def.args.variadic
        && let Some(last) = params.last_mut()
    {
        last.insert_str(0, "...");
    }
    params
}

/// One arity's signature: `fn match(c: Color): Str`.
pub fn fn_signature(index: &Index, fn_def: &FnDef) -> String {
    let modifier = match &fn_def.body {
        Value::Flow(flow) => flow_modifier(&flow.flow_type),
        _ => "",
    };
    let ret = fn_def
        .return_type
        .as_ref()
        .map(|t| format!(": {}", index.display_type(t)))
        .unwrap_or_default();
    format!("fn{modifier}({}){ret}", params(index, fn_def).join(", "))
}

/// The signature line(s) shown for a definition.
pub fn detail(index: &Index, value: &Value) -> Option<String> {
    match value {
        Value::Fn(fn_defs) if fn_defs.is_empty() => Some("fn()".to_string()),
        Value::Fn(fn_defs) => Some(
            fn_defs
                .iter()
                .map(|f| fn_signature(index, f))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::TypeDef(type_def) => Some(if type_def.variants.is_some() {
            "enum".to_string()
        } else {
            "type".to_string()
        }),
        _ => None,
    }
}

pub fn completion_kind(value: &Value) -> CompletionItemKind {
    match value {
        Value::Fn(_) | Value::Lambda(_) => CompletionItemKind::FUNCTION,
        Value::TypeDef(type_def) if type_def.variants.is_some() => CompletionItemKind::ENUM,
        Value::TypeDef(_) => CompletionItemKind::STRUCT,
        _ => CompletionItemKind::VARIABLE,
    }
}

/// The `doc` of a definition's meta.
pub fn doc(def: &Def<'_>) -> Option<String> {
    match def.var.meta.as_ref().map(|m| &m.val) {
        Some(Val::Map(map)) => match map.get(&Val::from("doc")) {
            Some(Val::Str(doc)) => Some(doc.trim().to_string()),
            _ => None,
        },
        _ => None,
    }
}

pub fn documentation(def: &Def<'_>) -> Option<Documentation> {
    doc(def).map(|value| {
        Documentation::MarkupContent(tower_lsp::lsp_types::MarkupContent {
            kind: tower_lsp::lsp_types::MarkupKind::Markdown,
            value,
        })
    })
}

fn display_val(val: &Val) -> String {
    match val {
        Val::Str(s) => format!("`\"{}\"`", s),
        Val::Int(i) => format!("`{}`", i),
        Val::Dec(d) => format!("`{}`", d),
        Val::Bool(b) => format!("`{}`", b),
        Val::Null => "`null`".to_string(),
        Val::Vec(v) => format!(
            "`[{}]`",
            v.iter().map(display_val).collect::<Vec<_>>().join(", ")
        ),
        Val::Map(m) => format!(
            "`{{{}}}`",
            m.iter()
                .map(|(k, v)| format!("{}: {}", display_val(k), display_val(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "`<complex value>`".to_string(),
    }
}

/// The meta of a definition other than its `doc`, for hover.
fn meta_section(def: &Def<'_>) -> String {
    let Some(meta) = def.var.meta.as_ref().map(|m| &m.val) else {
        return String::new();
    };
    let items: Vec<String> = match meta {
        Val::Map(map) => map
            .iter()
            .filter_map(|(key, value)| match key {
                Val::Str(key) if &**key != "doc" => {
                    Some(format!("- **{}**: {}", key, display_val(value)))
                }
                _ => None,
            })
            .collect(),
        Val::Vec(tags) => tags
            .iter()
            .map(|t| format!("- {}", display_val(t)))
            .collect(),
        Val::Str(s) if !s.is_empty() => vec![format!("- `\"{}\"`", s)],
        _ => Vec::new(),
    };
    if items.is_empty() {
        String::new()
    } else {
        format!("#### Meta\n\n{}\n", items.join("\n"))
    }
}

/// The type a type annotation names, resolved from `scope`. Optional and
/// refined forms resolve to their base type; generic containers to none.
pub fn resolve_type<'a>(scope: &Scope<'a>, annotation: &str) -> Option<Def<'a>> {
    let annotation = annotation.trim().trim_end_matches('?').trim();
    if annotation.contains('<') || annotation.contains('|') || annotation.is_empty() {
        return None;
    }
    let def = match annotation.rsplit_once('/') {
        Some((ns, name)) if ns.starts_with("::") => {
            let ns = scope.expand(ns);
            scope
                .index
                .get(&ns, name)
                .map(|def| scope.index.follow(def))?
        }
        _ => scope.global(annotation)?,
    };
    matches!(def.value, Value::TypeDef(_)).then_some(def)
}

/// The record fields of a type: `(name, annotation)`.
pub fn type_fields<'a>(def: &Def<'a>) -> Vec<(String, String)> {
    match def.value {
        Value::TypeDef(TypeDef {
            fields: Some(fields),
            ..
        }) => fields
            .iter()
            .map(|f| (f.name.name().to_string(), f.type_annotation.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The type a definition's value has, when it is a type constructor call or
/// a call to a function with a declared return type.
pub fn type_of_call<'a>(index: &'a Index, callee: &Def<'a>) -> Option<Def<'a>> {
    match callee.value {
        Value::TypeDef(_) => Some(*callee),
        Value::Fn(fn_defs) => {
            let ret = fn_defs.iter().find_map(|f| f.return_type.as_deref())?;
            resolve_type(&Scope::in_namespace(index, callee.ns), ret)
        }
        _ => None,
    }
}

fn type_hover(index: &Index, def: &Def<'_>, type_def: &TypeDef) -> String {
    let mut content = format!(
        "### {}\n```hot\n{}\n```\n\n",
        def.qualified(),
        detail(index, def.value).unwrap_or_default()
    );
    if let Some(doc) = doc(def) {
        content.push_str(&doc);
        content.push_str("\n\n");
    }
    if let Some(alias) = &type_def.type_alias {
        content.push_str(&format!("#### Type Alias\n\n`{}`\n\n", alias));
    }
    if let Some(variants) = &type_def.variants {
        content.push_str("#### Variants\n\n");
        for variant in variants {
            match &variant.type_ref {
                Some(t) => content.push_str(&format!("- `{}({})`\n", variant.name, t)),
                None => content.push_str(&format!("- `{}`\n", variant.name)),
            }
        }
        content.push('\n');
    }
    let fields = type_fields(def);
    if !fields.is_empty() {
        content.push_str("#### Fields\n\n");
        for (name, annotation) in &fields {
            content.push_str(&format!(
                "- `{}`: `{}`\n",
                name,
                index.display_type(annotation)
            ));
        }
        content.push('\n');
    }

    let type_name = type_def.name.name();
    let mut signatures = Vec::new();
    if !fields.is_empty() {
        signatures.push("fn(map: Map)".to_string());
    }
    for constructor in type_def.constructor_functions.iter().flatten() {
        signatures.push(fn_signature(index, constructor));
    }
    let mut outgoing = Vec::new();
    let mut incoming = Vec::new();
    for namespace in index.program.namespaces.values() {
        for value in namespace.scope.vars.values() {
            if let Value::TypeImplementation(imp) = value
                && let Value::Fn(fn_defs) = &imp.implementation
                && let Some(first) = fn_defs.first()
            {
                let args = params(index, first).join(", ");
                if imp.source_type == type_name {
                    outgoing.push(format!(
                        "implements {} fn({}): {}",
                        imp.target_type, args, imp.target_type
                    ));
                } else if imp.target_type == type_name {
                    incoming.push(format!(
                        "{} -> {} fn({}): {}",
                        imp.source_type, type_name, args, type_name
                    ));
                }
            }
        }
    }
    for group in [outgoing, incoming] {
        if !group.is_empty() {
            if !signatures.is_empty() {
                signatures.push(String::new());
            }
            signatures.extend(group);
        }
    }
    if !signatures.is_empty() {
        content.push_str(&format!("```hot\n{}\n```\n\n", signatures.join("\n")));
    }
    content.push_str(&meta_section(def));
    content
}

/// Hover text for a definition.
pub fn def_hover(index: &Index, def: &Def<'_>) -> String {
    if let Value::TypeDef(type_def) = def.value {
        return type_hover(index, def, type_def);
    }
    let mut content = format!("### {}\n", def.qualified());
    if let Some(detail) = detail(index, def.value) {
        content.push_str(&format!("```hot\n{}\n```\n\n", detail));
    }
    if let Some(doc) = doc(def) {
        content.push_str(&doc);
        content.push_str("\n\n");
    }
    // Fields of the record types the first arity takes.
    if let Value::Fn(fn_defs) = def.value
        && let Some(first) = fn_defs.first()
    {
        let scope = Scope::in_namespace(index, def.ns);
        let infos: Vec<String> = first
            .args
            .args
            .iter()
            .filter_map(|arg| {
                let annotation = arg.type_annotation.as_deref()?;
                let type_def = resolve_type(&scope, annotation)?;
                let fields = type_fields(&type_def);
                (!fields.is_empty()).then(|| {
                    format!(
                        "- `{}` ({}): {{ {} }}",
                        arg.var.sym.name(),
                        index.display_type(annotation),
                        fields
                            .iter()
                            .map(|(n, t)| format!("{}: {}", n, index.display_type(t)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
            .collect();
        if !infos.is_empty() {
            content.push_str(&format!("#### Argument Types\n\n{}\n\n", infos.join("\n")));
        }
    }
    content.push_str(&meta_section(def));
    content
}

/// Hover text for a variant of an enum: `Color.Red`.
pub fn variant_hover(def: &Def<'_>, variant: &str) -> Option<String> {
    let Value::TypeDef(type_def) = def.value else {
        return None;
    };
    let v = type_def
        .variants
        .as_ref()?
        .iter()
        .find(|v| v.name.name() == variant)?;
    let form = match &v.type_ref {
        Some(t) => format!("{}.{}({})", type_def.name.name(), variant, t),
        None => format!("{}.{}", type_def.name.name(), variant),
    };
    Some(format!(
        "### {}.{}\n```hot\n{}\n```\n\nVariant of `{}`\n",
        def.qualified(),
        variant,
        form,
        def.qualified()
    ))
}

/// Hover text for a local.
pub fn local_hover(local: &Local) -> String {
    match &local.kind {
        LocalKind::Param(Some(t)) => format!("```hot\n{}: {}\n```\n\nparameter", local.name, t),
        LocalKind::Param(None) => format!("```hot\n{}\n```\n\nparameter", local.name),
        LocalKind::Binding(_) => format!("```hot\n{}\n```\n\nlocal binding", local.name),
        LocalKind::ArmBinding => format!("```hot\n{}\n```\n\nmatch arm binding", local.name),
    }
}

fn signature(label: String, parameters: Vec<String>, doc: Option<String>) -> SignatureInformation {
    SignatureInformation {
        label,
        documentation: doc.map(Documentation::String),
        parameters: Some(
            parameters
                .into_iter()
                .map(|p| ParameterInformation {
                    label: ParameterLabel::Simple(p),
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: None,
    }
}

/// Signature help entries for a call to `def`.
pub fn signatures(index: &Index, def: &Def<'_>) -> Vec<SignatureInformation> {
    let doc = doc(def);
    match def.value {
        Value::Fn(fn_defs) => fn_defs
            .iter()
            .map(|f| {
                let params = params(index, f);
                let ret = f
                    .return_type
                    .as_ref()
                    .map(|t| format!(": {}", index.display_type(t)))
                    .unwrap_or_default();
                signature(
                    format!("{}({}){}", def.name, params.join(", "), ret),
                    params,
                    doc.clone(),
                )
            })
            .collect(),
        Value::TypeDef(type_def) => {
            let mut out = Vec::new();
            let fields = type_fields(def);
            if !fields.is_empty() {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{}: {}", n, index.display_type(t)))
                    .collect();
                out.push(signature(
                    format!("{}({{ {} }})", def.name, parts.join(", ")),
                    parts,
                    Some("Struct constructor - pass a map with the required fields".to_string()),
                ));
            }
            for constructor in type_def.constructor_functions.iter().flatten() {
                let params = params(index, constructor);
                out.push(signature(
                    format!("{}({})", def.name, params.join(", ")),
                    params,
                    Some("Custom constructor".to_string()),
                ));
            }
            if out.is_empty() {
                if type_def.type_alias.is_some() {
                    out.push(signature(
                        format!("{}(value)", def.name),
                        vec!["value".to_string()],
                        Some("Type alias constructor".to_string()),
                    ));
                } else if type_def.variants.is_some() {
                    out.push(signature(
                        format!("{}(variant)", def.name),
                        vec!["variant".to_string()],
                        Some("Variant type - use a variant constructor".to_string()),
                    ));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}
