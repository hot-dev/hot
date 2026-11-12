//! Request handlers that answer from the index and one document.

use crate::index::{Def, DocContext, Index, Resolved, Resolver, Scope};
use crate::render;
use crate::syntax::{
    ArgRef, CompletionContext, Key, KeyOwner, Local, LocalKind, OutlineItem, OutlineKind, RefPath,
    ValueShape, WhereSubject,
};
use crate::text::{Document, LineIndex};
use ahash::{AHashMap, AHashSet};
use hot::lang::ast::{Source, Value};
use hot::val::Val;
use std::path::{Path, PathBuf};
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, DocumentSymbol, Hover, HoverContents, InsertTextFormat,
    Location, MarkupContent, MarkupKind, Position, Range, SignatureHelp, SymbolInformation,
    SymbolKind, Uri,
};

/// Text of files that locations point into: open documents first, then disk.
pub struct Files<'a> {
    pub root: &'a Path,
    pub open: &'a AHashMap<PathBuf, (Uri, std::sync::Arc<Document>)>,
    cache: AHashMap<PathBuf, Option<(String, LineIndex)>>,
}

impl<'a> Files<'a> {
    pub fn new(
        root: &'a Path,
        open: &'a AHashMap<PathBuf, (Uri, std::sync::Arc<Document>)>,
    ) -> Self {
        Self {
            root,
            open,
            cache: AHashMap::new(),
        }
    }

    /// The text of a file, and its URI.
    pub fn text(&mut self, path: &Path) -> Option<(Uri, String)> {
        if let Some((uri, doc)) = self.open.get(path) {
            return Some((uri.clone(), doc.text.clone()));
        }
        let text = std::fs::read_to_string(path).ok()?;
        Some((crate::file_uri(path)?, text))
    }

    /// The location of a definition's source.
    pub fn location(&mut self, src: &Source) -> Option<Location> {
        let file = crate::analysis::normalize(self.root, Path::new(src.file.as_ref()?));
        let uri = match self.open.get(&file) {
            Some((uri, _)) => uri.clone(),
            None => crate::file_uri(&file)?,
        };
        let range = if let Some((_, doc)) = self.open.get(&file) {
            doc.lines
                .hot_range(&doc.text, src.line, src.column, src.length)
        } else {
            if !self.cache.contains_key(&file) {
                let entry = std::fs::read_to_string(&file).ok().map(|text| {
                    let index = LineIndex::new(&text);
                    (text, index)
                });
                self.cache.insert(file.clone(), entry);
            }
            match self.cache.get(&file).and_then(|e| e.as_ref()) {
                Some((text, index)) => index.hot_range(text, src.line, src.column, src.length),
                None => return None,
            }
        };
        Some(Location::new(uri, range))
    }
}

fn markdown(value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }
}

/// The reference at `offset` without its member accesses.
fn head(path: &RefPath) -> RefPath {
    RefPath {
        members: Vec::new(),
        cursor_member: None,
        ..path.clone()
    }
}

/// The record type a constructor call builds: `Point(`, or the payload of
/// `Shape.Circle(`.
fn record_of_callee<'a>(scope: &Scope<'a>, callee: &RefPath) -> Option<Def<'a>> {
    let def = scope.resolve_def(&head(callee))?;
    let Value::TypeDef(type_def) = def.value else {
        return None;
    };
    match callee.members.first() {
        None => render::resolve_type(scope, &callee_name(callee)?).or(Some(def)),
        Some(variant) => {
            let variant = type_def
                .variants
                .as_ref()?
                .iter()
                .find(|v| v.name.name() == variant)?;
            render::resolve_type(
                &Scope::in_namespace(scope.index, def.ns),
                variant.type_ref.as_deref()?,
            )
        }
    }
}

/// A callee as a type annotation would name it: `Point`, `::model/Point`.
fn callee_name(callee: &RefPath) -> Option<String> {
    let name = callee.name.as_deref()?;
    Some(match &callee.ns {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_string(),
    })
}

/// The record type a key names a field of, and the field's annotation.
fn key_field<'a>(scope: &Scope<'a>, key: &Key) -> Option<(Def<'a>, String)> {
    let def = match &key.owner {
        KeyOwner::Call(callee) => record_of_callee(scope, callee)?,
        KeyOwner::TypeDecl(name) => scope
            .global(name)
            .filter(|def| matches!(def.value, Value::TypeDef(_)))?,
        KeyOwner::Record => return None,
    };
    let annotation = render::type_fields(&def)
        .into_iter()
        .find(|(name, _)| *name == key.name)?
        .1;
    Some((def, annotation))
}

/// The fields `%.` reaches in the `where` clause at `offset`: the parameters
/// of a precondition, or the fields of the record being checked.
fn placeholder_fields(
    scope: &Scope<'_>,
    ctx: &DocContext,
    offset: usize,
) -> Option<Vec<(String, Option<String>)>> {
    let def = match ctx.syntax.where_subject(&ctx.text, offset)? {
        WhereSubject::Args(parameters) => return Some(parameters),
        WhereSubject::Type(annotation) => render::resolve_type(scope, &annotation)?,
        WhereSubject::Decl(name) => render::resolve_type(scope, &name)?,
    };
    Some(
        render::type_fields(&def)
            .into_iter()
            .map(|(name, annotation)| (name, Some(annotation)))
            .collect(),
    )
}

/// The parameter a precondition's `%.name` names, as a local.
fn arg_local(arg: &ArgRef) -> Local {
    Local {
        name: arg.name.clone(),
        start: arg.param_start,
        end: arg.param_end,
        scope: (arg.param_start, arg.param_end),
        visible_from: arg.param_start,
        kind: LocalKind::Param(arg.annotation.clone()),
    }
}

pub fn hover(index: &Index, doc: &Document, ctx: &DocContext, offset: usize) -> Option<Hover> {
    // `%.amount` in a precondition: the parameter.
    if let Some(arg) = ctx.arg_ref_at(offset) {
        let mut hover = markdown(render::local_hover(&arg_local(arg)));
        hover.range = Some(doc.char_range(arg.start, arg.end - arg.start));
        return Some(hover);
    }
    // `%`: what it stands for where it is written.
    if let Some((start, end)) = ctx.syntax.placeholder_at(offset) {
        let value = match ctx.syntax.where_subject(&ctx.text, offset) {
            Some(WhereSubject::Type(t)) => {
                format!("```hot\n%: {t}\n```\n\nthe value this constraint checks")
            }
            Some(WhereSubject::Decl(name)) => {
                format!("```hot\n%: {name}\n```\n\nthe value this constraint checks")
            }
            Some(WhereSubject::Args(parameters)) => format!(
                "```hot\n%: {{ {} }}\n```\n\nthe arguments this precondition checks, by parameter name",
                parameters
                    .iter()
                    .map(|(name, annotation)| match annotation {
                        Some(t) => format!("{name}: {t}"),
                        None => name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "```hot\n%\n```\n\nthe parameter of an implicit lambda".to_string(),
        };
        let mut hover = markdown(value);
        hover.range = Some(doc.char_range(start, end - start));
        return Some(hover);
    }
    // A key: the field it names, when a type is named for it.
    if let Some(key) = ctx.syntax.key_at(offset) {
        let scope = Scope::new(index, ctx, offset);
        let (def, annotation) = key_field(&scope, &key)?;
        let mut hover = markdown(format!(
            "```hot\n{}: {}\n```\n\nfield of `{}`",
            key.name,
            index.display_type(&annotation),
            def.qualified()
        ));
        hover.range = Some(doc.char_range(key.start, key.end - key.start));
        return Some(hover);
    }
    let path = ctx.syntax.path_at(offset)?;
    let scope = Scope::new(index, ctx, offset);
    let range = Some(doc.char_range(path.name_start, path.name_end - path.name_start));
    if path.name.is_none() {
        let ns = scope.expand(path.ns.as_deref()?);
        index.namespace(&ns)?;
        return Some(markdown(format!("### {ns}\n\nnamespace\n")));
    }
    let resolved = scope.resolve(&head(&path))?;
    let mut hover = match (resolved, path.cursor_member) {
        (Resolved::Def(def), Some(0)) => markdown(render::variant_hover(&def, &path.members[0])?),
        (_, Some(_)) => return None,
        (Resolved::Local(local), None) => markdown(render::local_hover(&local)),
        (Resolved::Def(def), None) => markdown(render::def_hover(index, &def)),
    };
    if path.cursor_member.is_none() {
        hover.range = range;
    }
    Some(hover)
}

pub fn definition(
    index: &Index,
    uri: &Uri,
    doc: &Document,
    ctx: &DocContext,
    offset: usize,
    files: &mut Files<'_>,
) -> Option<Location> {
    if let Some(arg) = ctx.arg_ref_at(offset) {
        return Some(Location::new(
            uri.clone(),
            doc.char_range(arg.param_start, arg.param_end - arg.param_start),
        ));
    }
    // A key: the type that declares the field.
    if let Some(key) = ctx.syntax.key_at(offset) {
        let scope = Scope::new(index, ctx, offset);
        let (def, _) = key_field(&scope, &key)?;
        return files.location(def.var.src.as_ref()?);
    }
    let path = ctx.syntax.path_at(offset)?;
    let scope = Scope::new(index, ctx, offset);
    if path.name.is_none() {
        // A namespace: the file that declares it.
        let ns = scope.expand(path.ns.as_deref()?);
        let file = index.namespace(&ns)?.source_file.clone()?;
        let file = crate::analysis::normalize(files.root, &file);
        let target = match files.open.get(&file) {
            Some((uri, _)) => uri.clone(),
            None => crate::file_uri(&file)?,
        };
        return Some(Location::new(
            target,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
        ));
    }
    match scope.resolve(&head(&path))? {
        Resolved::Local(local) => Some(Location::new(
            uri.clone(),
            doc.char_range(local.start, local.end - local.start),
        )),
        Resolved::Def(def) => files.location(def.var.src.as_ref()?),
    }
}

pub fn signature_help(index: &Index, ctx: &DocContext, offset: usize) -> Option<SignatureHelp> {
    let call = ctx.syntax.call_at(offset)?;
    let scope = Scope::new(index, ctx, offset);
    let def = scope.resolve_def(&head(&call.callee))?;
    let signatures = if let Some(variant) = call.callee.members.first() {
        // `Shape.Circle(`: the variant's payload.
        let Value::TypeDef(type_def) = def.value else {
            return None;
        };
        let v = type_def
            .variants
            .as_ref()?
            .iter()
            .find(|v| v.name.name() == variant)?;
        let payload = v.type_ref.clone()?;
        // A record payload is built by naming its fields.
        let fields: Vec<String> =
            render::resolve_type(&Scope::in_namespace(index, def.ns), &payload)
                .map(|record| render::type_fields(&record))
                .unwrap_or_default()
                .into_iter()
                .map(|(name, t)| format!("{}: {}", name, index.display_type(&t)))
                .collect();
        let parameters = if fields.is_empty() {
            vec![payload]
        } else {
            fields
        };
        vec![tower_lsp_server::ls_types::SignatureInformation {
            label: format!("{}.{}({})", def.name, variant, parameters.join(", ")),
            documentation: None,
            parameters: Some(
                parameters
                    .into_iter()
                    .map(|p| tower_lsp_server::ls_types::ParameterInformation {
                        label: tower_lsp_server::ls_types::ParameterLabel::Simple(p),
                        documentation: None,
                    })
                    .collect(),
            ),
            active_parameter: None,
        }]
    } else {
        render::signatures(index, &def)
    };
    if signatures.is_empty() {
        return None;
    }
    let mut active = call.active_parameter;
    let active_signature = signatures
        .iter()
        .position(|s| {
            let count = s.parameters.as_ref().map_or(0, Vec::len) as u32;
            count > active || s.label.contains("...")
        })
        .unwrap_or(0);
    // A named argument is the parameter of its name, wherever it is
    // written. One with no label yet is the first not yet named.
    let names: Vec<String> = signatures[active_signature]
        .parameters
        .iter()
        .flatten()
        .map(|p| match &p.label {
            tower_lsp_server::ls_types::ParameterLabel::Simple(label) => label
                .split(':')
                .next()
                .unwrap_or(label)
                .trim()
                .trim_start_matches("lazy ")
                .to_string(),
            _ => String::new(),
        })
        .collect();
    let named = match &call.active_label {
        Some(label) => names.iter().position(|name| name == label),
        None if !call.labels.is_empty() => {
            names.iter().position(|name| !call.labels.contains(name))
        }
        None => None,
    };
    if let Some(named) = named {
        active = named as u32;
    }
    Some(SignatureHelp {
        signatures,
        active_signature: Some(active_signature as u32),
        active_parameter: Some(active),
    })
}

fn def_item(index: &Index, def: &Def<'_>) -> CompletionItem {
    CompletionItem {
        label: def.name.to_string(),
        kind: Some(render::completion_kind(def.value)),
        detail: render::detail(index, def.value),
        documentation: render::documentation(def),
        ..Default::default()
    }
}

/// Record fields of the type a value has, as far as the source says: each
/// name, with its type annotation when the value's type is known.
fn fields_of_value(
    scope: &Scope<'_>,
    ctx: &DocContext,
    base: &RefPath,
) -> Option<Vec<(String, Option<String>)>> {
    let index = scope.index;
    // The type the base names, or the shape of the value it is bound to.
    let mut current: Option<Def<'_>> = None;
    let mut literal_keys: Option<Vec<String>> = None;
    let shape_type = |shape: ValueShape| -> (Option<Def<'_>>, Option<Vec<String>>) {
        match shape {
            ValueShape::Call(callee) => (
                scope
                    .resolve_def(&head(&callee))
                    .and_then(|d| render::type_of_call(index, &d)),
                None,
            ),
            ValueShape::Record(keys) => (None, Some(keys)),
            ValueShape::Other => (None, None),
        }
    };
    match scope.resolve(&head(base))? {
        Resolved::Local(local) => match &local.kind {
            LocalKind::Param(Some(annotation)) | LocalKind::Binding(_, Some(annotation)) => {
                current = render::resolve_type(scope, annotation)
            }
            LocalKind::Binding(value, None) => {
                (current, literal_keys) = shape_type(ctx.syntax.value_shape(*value));
            }
            _ => {}
        },
        Resolved::Def(def) => {
            // A binding in this document is fresher than the index.
            let shape = (base.ns.is_none() && def.ns == scope.ns)
                .then(|| ctx.syntax.top_level_value(base.name.as_deref()?))
                .flatten()
                .map(|value| ctx.syntax.value_shape(value));
            if let Some(shape) = shape {
                (current, literal_keys) = shape_type(shape);
            } else {
                match def.value {
                    Value::FnCall(call) => {
                        let callee = match call.function.as_ref() {
                            Value::Ref(hot::lang::ast::Ref::Var(v)) => {
                                Scope::in_namespace(index, def.ns).global(v.var.sym.name())
                            }
                            Value::Ref(hot::lang::ast::Ref::Ns(r)) => r
                                .function_name
                                .as_deref()
                                .and_then(|name| index.get(&r.ns.to_string(), name))
                                .map(|d| index.follow(d)),
                            _ => None,
                        };
                        current = callee.and_then(|c| render::type_of_call(index, &c));
                    }
                    Value::Val(Val::Map(map), _) => {
                        literal_keys = Some(
                            map.keys()
                                .filter_map(|k| match k {
                                    Val::Str(s) => Some(s.to_string()),
                                    _ => None,
                                })
                                .collect(),
                        );
                    }
                    _ => {
                        current = def.var.type_annotation.as_deref().and_then(|t| {
                            render::resolve_type(&Scope::in_namespace(index, def.ns), t)
                        });
                    }
                }
            }
        }
    }
    if let Some(keys) = literal_keys
        && base.members.is_empty()
    {
        return Some(keys.into_iter().map(|k| (k, None)).collect());
    }
    // Walk `.member` accesses through field types.
    for member in &base.members {
        let def = current?;
        let annotation = render::type_fields(&def)
            .into_iter()
            .find(|(name, _)| name == member)
            .map(|(_, t)| t)?;
        current = render::resolve_type(&Scope::in_namespace(index, def.ns), &annotation);
    }
    let def = current?;
    Some(
        render::type_fields(&def)
            .into_iter()
            .map(|(name, t)| (name, Some(t)))
            .collect(),
    )
}

pub fn completion(index: &Index, ctx: &DocContext, offset: usize) -> Option<Vec<CompletionItem>> {
    let context = ctx.syntax.completion_at(offset)?;
    let scope = Scope::new(index, ctx, offset);
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen = AHashSet::new();
    let mut push = |item: CompletionItem, items: &mut Vec<CompletionItem>| {
        if seen.insert(item.label.clone()) {
            items.push(item);
        }
    };
    match context {
        CompletionContext::Namespace { parent, partial } => {
            let expanded = if parent.is_empty() {
                String::new()
            } else {
                scope.expand(&parent)
            };
            let prefix = format!("{expanded}::");
            for ns in index.namespaces() {
                let Some(rest) = ns.strip_prefix(&prefix) else {
                    continue;
                };
                let segment = rest.split("::").next().unwrap_or("");
                if segment.is_empty() || !segment.starts_with(&partial) {
                    continue;
                }
                push(
                    CompletionItem {
                        label: segment.to_string(),
                        kind: Some(CompletionItemKind::MODULE),
                        detail: Some(format!("namespace {prefix}{segment}")),
                        ..Default::default()
                    },
                    &mut items,
                );
            }
            if parent.is_empty() {
                for (alias, target) in scope.aliases() {
                    let name = alias.trim_start_matches("::");
                    if !name.contains("::") && name.starts_with(&partial) {
                        push(
                            CompletionItem {
                                label: name.to_string(),
                                kind: Some(CompletionItemKind::MODULE),
                                detail: Some(format!("alias of {target}")),
                                ..Default::default()
                            },
                            &mut items,
                        );
                    }
                }
            }
        }
        CompletionContext::Member { ns, partial } => {
            let expanded = scope.expand(&ns);
            let mut defs = index.defs_in(&expanded);
            if defs.is_empty() {
                defs = index.defs_in(&ns);
            }
            for def in defs {
                if def.name.starts_with(&partial) {
                    push(def_item(index, &def), &mut items);
                }
            }
        }
        CompletionContext::Dot { base, partial } => {
            // A type: its variants.
            if base.members.is_empty()
                && let Some(def) = scope.resolve_def(&base)
                && let Value::TypeDef(type_def) = def.value
                && let Some(variants) = &type_def.variants
            {
                for v in variants {
                    let name = v.name.name();
                    if name.starts_with(&partial) {
                        push(
                            CompletionItem {
                                label: name.to_string(),
                                kind: Some(CompletionItemKind::ENUM_MEMBER),
                                detail: Some(match &v.type_ref {
                                    Some(t) => format!("{}.{}({})", def.name, name, t),
                                    None => format!("{}.{}", def.name, name),
                                }),
                                ..Default::default()
                            },
                            &mut items,
                        );
                    }
                }
            } else {
                for (name, annotation) in fields_of_value(&scope, ctx, &base)? {
                    if !name.starts_with(&partial) {
                        continue;
                    }
                    let detail =
                        annotation.map(|t| format!("{}: {}", name, index.display_type(&t)));
                    push(
                        CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail,
                            ..Default::default()
                        },
                        &mut items,
                    );
                }
            }
        }
        CompletionContext::TypeFields {
            type_path,
            present,
            partial,
        } => {
            let def = scope.resolve_def(&type_path)?;
            for (name, annotation) in render::type_fields(&def) {
                if present.contains(&name) || !name.starts_with(&partial) {
                    continue;
                }
                push(field_item(index, &name, &annotation), &mut items);
            }
        }
        CompletionContext::CallArgs {
            callee,
            present,
            partial,
        } => {
            // A record type's fields, to name: `Point(x: 1, |`.
            if let Some(def) = record_of_callee(&scope, &callee) {
                for (name, annotation) in render::type_fields(&def) {
                    if present.contains(&name) || !name.starts_with(&partial) {
                        continue;
                    }
                    push(field_item(index, &name, &annotation), &mut items);
                }
            }
            // And any name, once one is being typed.
            if !partial.is_empty() {
                for item in bare_items(index, ctx, &scope, offset, &partial) {
                    push(item, &mut items);
                }
            }
            if items.is_empty() {
                return None;
            }
        }
        CompletionContext::PlaceholderDot { partial } => {
            for (name, annotation) in placeholder_fields(&scope, ctx, offset)? {
                if !name.starts_with(&partial) {
                    continue;
                }
                push(
                    CompletionItem {
                        label: name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: annotation.map(|t| format!("{}: {}", name, index.display_type(&t))),
                        ..Default::default()
                    },
                    &mut items,
                );
            }
        }
        CompletionContext::Bare { partial } => {
            for item in bare_items(index, ctx, &scope, offset, &partial) {
                push(item, &mut items);
            }
        }
    }
    Some(items)
}

/// A record field to name in a constructor call: `name: `.
fn field_item(index: &Index, name: &str, annotation: &str) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("{}: {}", name, index.display_type(annotation))),
        insert_text: Some(format!("{}: ", name)),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        ..Default::default()
    }
}

/// The names a bare word can complete to, nearest first: locals, this
/// document's top-level names, the namespace's, then core names.
fn bare_items(
    index: &Index,
    ctx: &DocContext,
    scope: &Scope<'_>,
    offset: usize,
    partial: &str,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut locals: Vec<_> = scope.locals().to_vec();
    locals.sort_by_key(|l| std::cmp::Reverse(l.start));
    for local in locals {
        // Not the word being typed.
        let typing = local.start <= offset && offset <= local.end;
        if local.name.starts_with(partial) && !typing {
            items.push(CompletionItem {
                label: local.name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: match &local.kind {
                    LocalKind::Param(Some(t)) | LocalKind::Binding(_, Some(t)) => {
                        Some(format!("{}: {}", local.name, t))
                    }
                    LocalKind::Param(None) => Some("parameter".to_string()),
                    _ => Some("local".to_string()),
                },
                ..Default::default()
            });
        }
    }
    // This document's own top-level names, fresher than the index.
    let ns = scope.ns.clone();
    for (name, start, end) in ctx.syntax.top_level_names() {
        let typing = start <= offset && offset <= end;
        if ctx.namespace_at(start) == ns && name.starts_with(partial) && !typing {
            items.push(match index.get(&ns, &name) {
                Some(def) => def_item(index, &def),
                None => CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    ..Default::default()
                },
            });
        }
    }
    for def in index.defs_in(&ns) {
        if def.name.starts_with(partial) {
            items.push(def_item(index, &def));
        }
    }
    for def in index.core_defs() {
        if def.name.starts_with(partial) {
            items.push(def_item(index, &def));
        }
    }
    items
}

/// What find-references looks for.
pub enum Target {
    Local { uri: Uri, start: usize },
    Def { qualified: String, name: String },
}

/// The target of find-references at `offset`, and its declaration.
pub fn reference_target(
    index: &Index,
    uri: &Uri,
    doc: &Document,
    ctx: &DocContext,
    offset: usize,
    files: &mut Files<'_>,
) -> Option<(Target, Option<Location>)> {
    if let Some(arg) = ctx.arg_ref_at(offset) {
        return Some((
            Target::Local {
                uri: uri.clone(),
                start: arg.param_start,
            },
            Some(Location::new(
                uri.clone(),
                doc.char_range(arg.param_start, arg.param_end - arg.param_start),
            )),
        ));
    }
    let path = ctx.syntax.path_at(offset)?;
    if path.cursor_member.is_some() {
        return None;
    }
    let scope = Scope::new(index, ctx, offset);
    match scope.resolve(&head(&path))? {
        Resolved::Local(local) => Some((
            Target::Local {
                uri: uri.clone(),
                start: local.start,
            },
            Some(Location::new(
                uri.clone(),
                doc.char_range(local.start, local.end - local.start),
            )),
        )),
        Resolved::Def(def) => Some((
            Target::Def {
                qualified: def.qualified(),
                name: def.name.to_string(),
            },
            def.var.src.as_ref().and_then(|src| files.location(src)),
        )),
    }
}

/// References to `target` in one document.
pub fn references_in(
    index: &Index,
    uri: &Uri,
    text: &str,
    target: &Target,
    out: &mut Vec<Location>,
) {
    let name = match target {
        Target::Local {
            uri: target_uri, ..
        } if target_uri != uri => return,
        Target::Local { .. } => None,
        Target::Def { name, .. } => Some(name.as_str()),
    };
    if let Some(name) = name
        && !text.contains(name)
    {
        return;
    }
    let ctx = DocContext::new(text);
    let lines = LineIndex::new(text);
    for site in reference_sites(index, &ctx, target) {
        out.push(Location::new(
            uri.clone(),
            lines.char_range(text, site.start, site.end - site.start),
        ));
    }
}

/// A name in a document that refers to the target of a search.
pub struct RefSite {
    /// Char span of the name.
    pub start: usize,
    pub end: usize,
}

/// Each name in a document that refers to `target`, its declaration
/// included, in source order.
pub fn reference_sites(index: &Index, ctx: &DocContext, target: &Target) -> Vec<RefSite> {
    let mut sites = Vec::new();
    let mut resolver = Resolver::new(index, ctx);
    for syntax in ctx.syntax.all_scopes() {
        for path in syntax.references() {
            let Some(path_name) = path.name.as_deref() else {
                continue;
            };
            let matches = match target {
                Target::Local { start, .. } => {
                    path.ns.is_none()
                        && matches!(
                            resolver.resolve(&head(&path), path.name_start),
                            Some(Resolved::Local(local)) if local.start == *start
                        )
                }
                Target::Def { qualified, name } => {
                    path_name == name
                        && matches!(
                            resolver.resolve(&head(&path), path.name_start),
                            Some(Resolved::Def(def)) if def.qualified() == *qualified
                        )
                }
            };
            if matches {
                sites.push(RefSite {
                    start: path.name_start,
                    end: path.name_end,
                });
            }
        }
    }
    // `%.amount` in a precondition names the parameter `amount`.
    if let Target::Local { start, .. } = target {
        sites.extend(
            ctx.arg_refs
                .iter()
                .filter(|arg| arg.param_start == *start)
                .map(|arg| RefSite {
                    start: arg.start,
                    end: arg.end,
                }),
        );
    }
    sites.sort_unstable_by_key(|site| site.start);
    sites.dedup_by_key(|site| site.start);
    sites
}

/// Project definitions whose name contains `query` (case-insensitively).
pub fn workspace_symbols(
    index: &Index,
    query: &str,
    project_files: &AHashSet<String>,
    files: &mut Files<'_>,
) -> Vec<SymbolInformation> {
    let query = query.to_lowercase();
    let mut out = Vec::new();
    for def in index.all_defs() {
        if out.len() >= 200 {
            break;
        }
        let Some(src) = def.var.src.as_ref() else {
            continue;
        };
        if !src.file.as_ref().is_some_and(|f| project_files.contains(f))
            || !def.name.to_lowercase().contains(&query)
            || index.is_import(&def)
        {
            continue;
        }
        let Some(location) = files.location(src) else {
            continue;
        };
        let kind = match def.value {
            Value::Fn(_) => SymbolKind::FUNCTION,
            Value::TypeDef(t) if t.variants.is_some() => SymbolKind::ENUM,
            Value::TypeDef(_) => SymbolKind::STRUCT,
            _ => SymbolKind::VARIABLE,
        };
        #[allow(deprecated)]
        out.push(SymbolInformation {
            name: def.name.to_string(),
            kind,
            tags: None,
            deprecated: None,
            location,
            container_name: Some(def.ns.to_string()),
        });
    }
    out
}

fn symbol_kind(kind: OutlineKind) -> SymbolKind {
    match kind {
        OutlineKind::Namespace => SymbolKind::NAMESPACE,
        OutlineKind::Function | OutlineKind::Test => SymbolKind::FUNCTION,
        OutlineKind::Constructor => SymbolKind::CONSTRUCTOR,
        OutlineKind::Struct => SymbolKind::STRUCT,
        OutlineKind::Enum => SymbolKind::ENUM,
        OutlineKind::Type => SymbolKind::CLASS,
        OutlineKind::Field => SymbolKind::FIELD,
        OutlineKind::Variant => SymbolKind::ENUM_MEMBER,
        OutlineKind::Binding => SymbolKind::VARIABLE,
        OutlineKind::Property => SymbolKind::PROPERTY,
        OutlineKind::Arrow => SymbolKind::OPERATOR,
    }
}

fn document_symbol(doc: &Document, item: &OutlineItem) -> DocumentSymbol {
    let detail = match (item.kind, &item.detail) {
        (OutlineKind::Test, Some(detail)) => Some(format!("test {detail}")),
        (OutlineKind::Test, None) => Some("test".to_string()),
        (_, detail) => detail.clone(),
    };
    #[allow(deprecated)]
    DocumentSymbol {
        name: item.name.clone(),
        detail,
        kind: symbol_kind(item.kind),
        tags: None,
        deprecated: None,
        range: doc.char_range(item.start, item.end - item.start),
        selection_range: doc.char_range(item.name_start, item.name_end - item.name_start),
        children: (!item.children.is_empty()).then(|| {
            item.children
                .iter()
                .map(|child| document_symbol(doc, child))
                .collect()
        }),
    }
}

/// The declarations of a document, as a tree: a namespace holds what is
/// declared in it, a type its fields, variants and constructors, and a
/// function with several arities each of them.
pub fn document_symbols(doc: &Document, ctx: &DocContext) -> Vec<DocumentSymbol> {
    ctx.syntax
        .outline(&doc.text)
        .iter()
        .map(|item| document_symbol(doc, item))
        .collect()
}

/// A symbol tree as a list, for a client that takes no tree.
pub fn flat_symbols(uri: &Uri, symbols: &[DocumentSymbol]) -> Vec<SymbolInformation> {
    fn flatten(
        uri: &Uri,
        symbols: &[DocumentSymbol],
        container: Option<&str>,
        out: &mut Vec<SymbolInformation>,
    ) {
        for symbol in symbols {
            #[allow(deprecated)]
            out.push(SymbolInformation {
                name: symbol.name.clone(),
                kind: symbol.kind,
                tags: None,
                deprecated: None,
                location: Location::new(uri.clone(), symbol.range),
                container_name: container.map(str::to_string),
            });
            if let Some(children) = &symbol.children {
                flatten(uri, children, Some(&symbol.name), out);
            }
        }
    }
    let mut out = Vec::new();
    flatten(uri, symbols, None, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::lang::ast::Program;

    const APP: &str = r#"::app ns

Port type Int where between(%, 1, 65535)
Range type { low: Int, high: Int } where lte(%.low, %.high)
Point type { x: Int, y: Int }
Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }
LineItem type { sku: Str, qty: Int }
Bulk type LineItem where gt(%.qty, 100)
plain 4
max-retries: Int 3

withdraw fn (amount: Int, balance: Int)
    where lte(%.amount, %.balance)
    : Int where gte(%, 0)
{
    sub(balance, amount)
}

make fn (qty: Int): LineItem where gt(%.qty, 0) { LineItem(sku: "a", qty: qty) }

total fn (item: LineItem where gt(%.qty, 0), bulk: Bulk): Int {
    plain 9
    limit: Int 3 where gt(%, plain)
    made make(1)
    add(item.qty, made.qty, bulk.qty, limit)
}

build fn (x: Int): Point {
    p Point(x: x, y: 2)
    c Shape.Circle(radius: 1.0)
    p
}

pick fn match (r: Result): Any {
    Result.Ok (value) => { value }
    _ => { null }
}
"#;

    fn index() -> Index {
        let mut namespaces = Default::default();
        let parsed = hot::lang::parser::parse_hot_file(APP, "/app.hot").unwrap();
        hot::lang::engine::discover::merge_program_namespaces_strict(
            &mut namespaces,
            parsed.namespaces,
        )
        .unwrap();
        Index::new(Program {
            namespaces,
            current_namespace: hot::lang::ast::NsPath::hot_main(),
        })
    }

    fn offset_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).expect(needle)].chars().count()
    }

    /// Hover text at `delta` chars into the first `needle` of `text`.
    fn hover_at(index: &Index, text: &str, needle: &str, delta: usize) -> Option<String> {
        let doc = Document::new(text.to_string(), 0);
        let ctx = DocContext::new(text);
        let hover = hover(index, &doc, &ctx, offset_of(text, needle) + delta)?;
        match hover.contents {
            HoverContents::Markup(content) => Some(content.value),
            _ => None,
        }
    }

    /// Completion labels with the cursor at the end of `text`.
    fn complete(index: &Index, text: &str) -> Vec<String> {
        let ctx = DocContext::new(text);
        let mut labels: Vec<String> = completion(index, &ctx, text.chars().count())
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.label)
            .collect();
        labels.sort();
        labels
    }

    /// The active signature's label and active parameter, with the cursor
    /// at the end of `text`.
    fn signature(index: &Index, text: &str) -> (String, String) {
        let ctx = DocContext::new(text);
        let help = signature_help(index, &ctx, text.chars().count()).expect("signature help");
        let active = &help.signatures[help.active_signature.unwrap_or(0) as usize];
        let parameter = active
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.get(help.active_parameter? as usize))
            .map(|parameter| match &parameter.label {
                tower_lsp_server::ls_types::ParameterLabel::Simple(label) => label.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        (active.label.clone(), parameter)
    }

    #[test]
    fn hover_shows_constraints() {
        let index = index();
        let port = hover_at(&index, APP, "Port type", 0).unwrap();
        assert!(
            port.contains("type Int where between(%, 1, 65535)"),
            "{port}"
        );
        assert!(port.contains("#### Constraint"), "{port}");
        let range = hover_at(&index, APP, "Range type", 0).unwrap();
        assert!(
            range.contains("type { ... } where lte(%.low, %.high)"),
            "{range}"
        );
        let withdraw = hover_at(&index, APP, "withdraw fn", 0).unwrap();
        assert!(
            withdraw.contains(
                "fn(amount: Int, balance: Int) where lte(%.amount, %.balance): Int where gte(%, 0)"
            ),
            "{withdraw}"
        );
        let retries = hover_at(&index, APP, "max-retries: Int", 0).unwrap();
        assert!(retries.contains("max-retries: Int"), "{retries}");
    }

    #[test]
    fn hover_on_locals_of_constrained_code() {
        let index = index();
        // A parameter of a signature written over several lines.
        let balance = hover_at(&index, APP, "sub(balance", 4).unwrap();
        assert!(
            balance.contains("balance: Int") && balance.contains("parameter"),
            "{balance}"
        );
        // A typed binding, with its constraint.
        let limit = hover_at(&index, APP, "limit)\n", 0).unwrap();
        assert!(limit.contains("limit: Int where gt(%, plain)"), "{limit}");
        assert!(limit.contains("local binding"), "{limit}");
        // No variable is named `where`.
        assert_eq!(hover_at(&index, APP, "where lte(%.amount", 1), None);
        // A match arm's binding, written with a space.
        let value = hover_at(&index, APP, "{ value }", 2).unwrap();
        assert!(value.contains("match arm binding"), "{value}");
    }

    #[test]
    fn a_predicate_resolves_names_at_namespace_scope() {
        let index = index();
        // `plain` in the predicate is the namespace's, not the local `plain 9`.
        let in_predicate = hover_at(&index, APP, "gt(%, plain)", 6).unwrap();
        assert!(in_predicate.contains("### ::app/plain"), "{in_predicate}");
        let percent = hover_at(&index, APP, "%, plain)", 0).unwrap();
        assert!(percent.contains("%: Int"), "{percent}");
        let record = hover_at(&index, APP, "%.low", 0).unwrap();
        assert!(record.contains("%: Range"), "{record}");
        let arguments = hover_at(&index, APP, "%.amount", 0).unwrap();
        assert!(
            arguments.contains("amount: Int, balance: Int"),
            "{arguments}"
        );
    }

    /// The text of every reference to what is `delta` chars into the
    /// first `needle`, with the line each is on.
    fn references_at(index: &Index, text: &str, needle: &str, delta: usize) -> Vec<String> {
        let uri: Uri = "file:///app.hot".parse().unwrap();
        let doc = Document::new(text.to_string(), 0);
        let ctx = DocContext::new(text);
        let open = AHashMap::new();
        let mut files = Files::new(Path::new("/"), &open);
        let offset = offset_of(text, needle) + delta;
        let Some((target, _)) = reference_target(index, &uri, &doc, &ctx, offset, &mut files)
        else {
            return Vec::new();
        };
        let mut locations = Vec::new();
        references_in(index, &uri, text, &target, &mut locations);
        locations
            .iter()
            .map(|location| {
                let line = doc.lines.line(text, location.range.start.line as usize);
                let (start, end) = (
                    location.range.start.character as usize,
                    location.range.end.character as usize,
                );
                format!("{}:{}", location.range.start.line + 1, &line[start..end])
            })
            .collect()
    }

    #[test]
    fn a_precondition_refers_to_its_parameters() {
        let index = index();
        // From the parameter, and from the `%.amount` that names it.
        let expected = ["13:amount", "14:amount", "17:amount"];
        assert_eq!(references_at(&index, APP, "amount: Int", 0), expected);
        assert_eq!(references_at(&index, APP, "%.amount", 2), expected);
        assert_eq!(
            references_at(&index, APP, "sub(balance, amount)", 13),
            expected
        );
        let hover = hover_at(&index, APP, "%.amount", 2).unwrap();
        assert!(
            hover.contains("amount: Int") && hover.contains("parameter"),
            "{hover}"
        );
        // A record's `%.low` is a field, not a parameter.
        assert!(references_at(&index, APP, "%.low", 2).is_empty());
        // `%.qty` on a parameter's own constraint is a field of its type.
        assert!(references_at(&index, APP, "%.qty, 0), bulk", 2).is_empty());

        let doc = Document::new(APP.to_string(), 0);
        let ctx = DocContext::new(APP);
        let uri: Uri = "file:///app.hot".parse().unwrap();
        let open = AHashMap::new();
        let mut files = Files::new(Path::new("/"), &open);
        let offset = offset_of(APP, "%.balance") + 2;
        let location = definition(&index, &uri, &doc, &ctx, offset, &mut files).unwrap();
        assert_eq!(location.range.start, Position::new(12, 26));
    }

    #[test]
    fn a_variant_is_not_a_reference_to_the_type_of_its_name() {
        let index = index();
        // `Circle(Circle)` in `Shape`: the payload is the type, the variant
        // isn't. `Shape.Circle(` names the variant.
        assert_eq!(
            references_at(&index, APP, "Circle type", 0),
            ["6:Circle", "7:Circle"]
        );
        assert_eq!(hover_at(&index, APP, "Circle(Circle)", 0), None);
    }

    #[test]
    fn a_named_argument_is_a_field_not_a_variable() {
        let index = index();
        let label = hover_at(&index, APP, "x: x, y: 2", 0).unwrap();
        assert!(
            label.contains("x: Int") && label.contains("field of `::app/Point`"),
            "{label}"
        );
        // The value after it is the parameter.
        let value = hover_at(&index, APP, "x: x, y: 2", 3).unwrap();
        assert!(value.contains("parameter"), "{value}");
        let radius = hover_at(&index, APP, "radius: 1.0", 0).unwrap();
        assert!(
            radius.contains("radius: Dec") && radius.contains("::app/Circle"),
            "{radius}"
        );
        // A field where it is declared.
        let declared = hover_at(&index, APP, "low: Int", 0).unwrap();
        assert!(declared.contains("field of `::app/Range`"), "{declared}");
    }

    #[test]
    fn constructor_calls_complete_and_explain_their_fields() {
        let index = index();
        assert_eq!(complete(&index, "::app ns\n\nq Point("), ["x", "y"]);
        assert_eq!(complete(&index, "::app ns\n\nq Point(x: 1, "), ["y"]);
        assert_eq!(complete(&index, "::app ns\n\nq Shape.Circle("), ["radius"]);
        // A function call's arguments are any names, once one is typed.
        assert!(complete(&index, "::app ns\n\nq make(").is_empty());
        assert!(complete(&index, "::app ns\n\nq make(pl").contains(&"plain".to_string()));

        let (label, parameter) = signature(&index, "::app ns\n\nq Point(");
        assert_eq!(label, "Point(x: Int, y: Int)");
        assert_eq!(parameter, "x: Int");
        // By name, not by position.
        let (_, parameter) = signature(&index, "::app ns\n\nq Point(y: ");
        assert_eq!(parameter, "y: Int");
        let (_, parameter) = signature(&index, "::app ns\n\nq Point(y: 2, ");
        assert_eq!(parameter, "x: Int");
        let (label, _) = signature(&index, "::app ns\n\nq Shape.Circle(");
        assert_eq!(label, "Shape.Circle(radius: Dec)");
        let (label, _) = signature(&index, "::app ns\n\nq Port(");
        assert_eq!(label, "Port(value: Int where between(%, 1, 65535))");
        let (label, _) = signature(&index, "::app ns\n\nq Range(");
        assert_eq!(label, "Range(low: Int, high: Int) where lte(%.low, %.high)");
        let (label, parameter) = signature(&index, "::app ns\n\nq withdraw(1, ");
        assert_eq!(
            label,
            "withdraw(amount: Int, balance: Int) where lte(%.amount, %.balance): Int where gte(%, 0)"
        );
        assert_eq!(parameter, "balance: Int");
    }

    #[test]
    fn fields_complete_through_refined_types_and_placeholders() {
        let index = index();
        // With the cursor `extra` chars into `needle`, in the whole text.
        let ctx = DocContext::new(APP);
        let at = |needle: &str, extra: usize| -> Vec<String> {
            let mut labels: Vec<String> = completion(&index, &ctx, offset_of(APP, needle) + extra)
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.label)
                .collect();
            labels.sort();
            labels
        };
        let fields = ["qty", "sku"];
        // A parameter refined inline, a function that returns a refined
        // type, and a named refinement of a record.
        assert_eq!(at("item.qty, made", 5), fields);
        assert_eq!(at("made.qty, bulk", 5), fields);
        assert_eq!(at("bulk.qty, limit", 5), fields);
        // `%.` in a record's invariant, and in a precondition.
        assert_eq!(at("%.low, %.high", 2), ["high", "low"]);
        assert_eq!(at("%.amount, %.balance", 2), ["amount", "balance"]);
        assert_eq!(at("%.qty, 0), bulk", 2), fields);
    }
}
