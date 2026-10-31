//! Request handlers that answer from the index and one document.

use crate::index::{Def, DocContext, Index, Resolved, Scope};
use crate::render;
use crate::syntax::{CompletionContext, LocalKind, RefPath, ValueShape};
use crate::text::{Document, LineIndex};
use ahash::{AHashMap, AHashSet};
use hot::lang::ast::{Source, Value};
use hot::val::Val;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Hover, HoverContents, InsertTextFormat, Location,
    MarkupContent, MarkupKind, Position, Range, SignatureHelp, SymbolInformation, SymbolKind, Url,
};

/// Text of files that locations point into: open documents first, then disk.
pub struct Files<'a> {
    pub root: &'a Path,
    pub open: &'a AHashMap<PathBuf, (Url, std::sync::Arc<Document>)>,
    cache: AHashMap<PathBuf, Option<(String, LineIndex)>>,
}

impl<'a> Files<'a> {
    pub fn new(
        root: &'a Path,
        open: &'a AHashMap<PathBuf, (Url, std::sync::Arc<Document>)>,
    ) -> Self {
        Self {
            root,
            open,
            cache: AHashMap::new(),
        }
    }

    /// The text of a file, and its URI.
    pub fn text(&mut self, path: &Path) -> Option<(Url, String)> {
        if let Some((uri, doc)) = self.open.get(path) {
            return Some((uri.clone(), doc.text.clone()));
        }
        let text = std::fs::read_to_string(path).ok()?;
        Some((Url::from_file_path(path).ok()?, text))
    }

    /// The location of a definition's source.
    pub fn location(&mut self, src: &Source) -> Option<Location> {
        let file = crate::analysis::normalize(self.root, Path::new(src.file.as_ref()?));
        let uri = match self.open.get(&file) {
            Some((uri, _)) => uri.clone(),
            None => Url::from_file_path(&file).ok()?,
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

pub fn hover(index: &Index, doc: &Document, ctx: &DocContext, offset: usize) -> Option<Hover> {
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
    uri: &Url,
    doc: &Document,
    ctx: &DocContext,
    offset: usize,
    files: &mut Files<'_>,
) -> Option<Location> {
    let path = ctx.syntax.path_at(offset)?;
    let scope = Scope::new(index, ctx, offset);
    if path.name.is_none() {
        // A namespace: the file that declares it.
        let ns = scope.expand(path.ns.as_deref()?);
        let file = index.namespace(&ns)?.source_file.clone()?;
        let file = crate::analysis::normalize(files.root, &file);
        let target = match files.open.get(&file) {
            Some((uri, _)) => uri.clone(),
            None => Url::from_file_path(&file).ok()?,
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
        vec![tower_lsp::lsp_types::SignatureInformation {
            label: format!("{}.{}({})", def.name, variant, payload),
            documentation: None,
            parameters: Some(vec![tower_lsp::lsp_types::ParameterInformation {
                label: tower_lsp::lsp_types::ParameterLabel::Simple(payload),
                documentation: None,
            }]),
            active_parameter: None,
        }]
    } else {
        render::signatures(index, &def)
    };
    if signatures.is_empty() {
        return None;
    }
    let active = call.active_parameter;
    let active_signature = signatures
        .iter()
        .position(|s| {
            let count = s.parameters.as_ref().map_or(0, Vec::len) as u32;
            count > active || s.label.contains("...")
        })
        .unwrap_or(0) as u32;
    Some(SignatureHelp {
        signatures,
        active_signature: Some(active_signature),
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
            LocalKind::Param(Some(annotation)) => current = render::resolve_type(scope, annotation),
            LocalKind::Binding(value) => {
                (current, literal_keys) = shape_type(ctx.syntax.value_shape(*value));
            }
            _ => {}
        },
        Resolved::Def(def) => {
            // A binding in this document is fresher than the index.
            let shape = (base.ns.is_none())
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
                push(
                    CompletionItem {
                        label: name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(format!("{}: {}", name, index.display_type(&annotation))),
                        insert_text: Some(format!("{}: ", name)),
                        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                        ..Default::default()
                    },
                    &mut items,
                );
            }
        }
        CompletionContext::Bare { partial } => {
            let mut locals: Vec<_> = scope.locals().to_vec();
            locals.sort_by_key(|l| std::cmp::Reverse(l.start));
            for local in locals {
                // Not the word being typed.
                let typing = local.start <= offset && offset <= local.end;
                if local.name.starts_with(&partial) && !typing {
                    push(
                        CompletionItem {
                            label: local.name.clone(),
                            kind: Some(CompletionItemKind::VARIABLE),
                            detail: match &local.kind {
                                LocalKind::Param(Some(t)) => Some(format!("{}: {}", local.name, t)),
                                LocalKind::Param(None) => Some("parameter".to_string()),
                                _ => Some("local".to_string()),
                            },
                            ..Default::default()
                        },
                        &mut items,
                    );
                }
            }
            // This document's own top-level names, fresher than the index.
            let ns = scope.ns.clone();
            for (name, start, end) in ctx.syntax.top_level_names() {
                let typing = start <= offset && offset <= end;
                if ctx.namespace_at(start) == ns && name.starts_with(&partial) && !typing {
                    let item = match index.get(&ns, &name) {
                        Some(def) => def_item(index, &def),
                        None => CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::VARIABLE),
                            ..Default::default()
                        },
                    };
                    push(item, &mut items);
                }
            }
            for def in index.defs_in(&ns) {
                if def.name.starts_with(&partial) {
                    push(def_item(index, &def), &mut items);
                }
            }
            for def in index.core_defs() {
                if def.name.starts_with(&partial) {
                    push(def_item(index, &def), &mut items);
                }
            }
        }
    }
    Some(items)
}

/// What find-references looks for.
pub enum Target {
    Local { uri: Url, start: usize },
    Def { qualified: String, name: String },
}

/// The target of find-references at `offset`, and its declaration.
pub fn reference_target(
    index: &Index,
    uri: &Url,
    doc: &Document,
    ctx: &DocContext,
    offset: usize,
    files: &mut Files<'_>,
) -> Option<(Target, Option<Location>)> {
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
    uri: &Url,
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
    for syntax in ctx.syntax.all_scopes() {
        for path in syntax.references() {
            let Some(path_name) = path.name.as_deref() else {
                continue;
            };
            let matches = match target {
                Target::Local { start, .. } => {
                    path.ns.is_none()
                        && matches!(
                            Scope::new(index, &ctx, path.name_start).resolve(&head(&path)),
                            Some(Resolved::Local(local)) if local.start == *start
                        )
                }
                Target::Def { qualified, name } => {
                    path_name == name
                        && Scope::new(index, &ctx, path.name_start)
                            .resolve_def(&head(&path))
                            .is_some_and(|def| def.qualified() == *qualified)
                }
            };
            if matches {
                out.push(Location::new(
                    uri.clone(),
                    lines.char_range(text, path.name_start, path.name_end - path.name_start),
                ));
            }
        }
    }
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
