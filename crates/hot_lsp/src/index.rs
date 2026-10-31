//! Top-level definitions of the checked program, and name resolution.
//!
//! The index is built from the program the last successful check parsed
//! (hot-std, dependencies and the project, with open buffers), so it sees
//! what the compiler sees. Names resolve the way the compiler resolves them:
//! a qualified name through the namespace's aliases; a bare name as a local,
//! then a definition in the current namespace, then a core name from the
//! core registry (which knows the namespace that defines it). Var-imports
//! are followed to the definition they name.

use crate::syntax::{Local, RefPath, Syntax};
use ahash::AHashMap;
use hot::lang::ast::{Namespace, Program, Ref, Value, Var};

/// Var-import chains longer than this are treated as cycles.
const MAX_IMPORT_HOPS: usize = 32;

/// A top-level definition.
#[derive(Clone, Copy)]
pub struct Def<'a> {
    pub ns: &'a str,
    pub name: &'a str,
    pub var: &'a Var,
    pub value: &'a Value,
}

impl Def<'_> {
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.ns, self.name)
    }
}

/// What a reference resolves to.
pub enum Resolved<'a> {
    /// A local binding or parameter in the document.
    Local(Local),
    /// A top-level definition (var-imports followed).
    Def(Def<'a>),
}

pub struct Index {
    pub program: Program,
    ns_names: Vec<String>,
    ns_lookup: AHashMap<String, usize>,
    /// Per namespace: name → index of its first (non deep-set) definition.
    vars: Vec<AHashMap<String, usize>>,
    /// Core name → the namespace that defines it, as the core registry has it.
    core: AHashMap<String, String>,
    /// How each hidden inline-refinement type (`$Where-…`) reads in source.
    refinements: AHashMap<String, String>,
}

/// Names the parser adds that are not definitions.
fn is_hidden(var: &Var, value: &Value) -> bool {
    let name = var.sym.name();
    hot::lang::ast::is_where_predicate(name)
        || hot::lang::ast::is_inline_refinement_type(name)
        || var.deep_set.is_some()
        || var.deep_path.is_some()
        || (name == "ns"
            && var.src.is_none()
            && matches!(value, Value::Ref(Ref::Ns(r)) if r.function_name.is_none()))
}

impl Index {
    pub fn new(program: Program) -> Self {
        let core = hot::lang::compiler::core_registry::extract_core_variables_from_ast(&program)
            .iter()
            .map(|(name, info)| {
                let ns = if info.namespace_path.starts_with("::") {
                    info.namespace_path.clone()
                } else {
                    format!("::{}", info.namespace_path)
                };
                (name.clone(), ns)
            })
            .collect();
        let mut ns_names = Vec::new();
        let mut ns_lookup = AHashMap::new();
        let mut vars = Vec::new();
        for (i, (path, namespace)) in program.namespaces.iter().enumerate() {
            let name = path.to_string();
            ns_lookup.insert(name.clone(), i);
            ns_names.push(name);
            let mut names = AHashMap::new();
            for (j, (var, value)) in namespace.scope.vars.iter().enumerate() {
                if !is_hidden(var, value) {
                    names.entry(var.sym.name().to_string()).or_insert(j);
                }
            }
            vars.push(names);
        }
        let refinements = hot::lang::ast::inline_refinement_displays(program.namespaces.values());
        Self {
            program,
            ns_names,
            ns_lookup,
            vars,
            core,
            refinements,
        }
    }

    /// A type annotation as it reads in source: debug forms such as
    /// `Union([Builtin(Int), Builtin(Dec)])` become `Int | Dec`, and hidden
    /// inline refinements show their `where` clause.
    pub fn display_type(&self, annotation: &str) -> String {
        fn single(t: &str) -> String {
            if let Some(inner) = t.strip_prefix("Builtin(").and_then(|t| t.strip_suffix(')')) {
                inner.to_string()
            } else if let Some(inner) = t
                .strip_prefix("Named(\"")
                .and_then(|t| t.strip_suffix("\")"))
            {
                inner.to_string()
            } else {
                t.to_string()
            }
        }
        let formatted = match annotation
            .strip_prefix("Union([")
            .and_then(|t| t.strip_suffix("])"))
        {
            Some(inner) => inner
                .split(", ")
                .map(single)
                .collect::<Vec<_>>()
                .join(" | "),
            None => single(annotation),
        };
        hot::lang::ast::display_inline_refinements(&formatted, &self.refinements)
    }

    pub fn namespaces(&self) -> &[String] {
        &self.ns_names
    }

    pub fn namespace(&self, ns: &str) -> Option<&Namespace> {
        let &i = self.ns_lookup.get(ns)?;
        self.program.namespaces.get_index(i).map(|(_, n)| n)
    }

    fn def_at(&self, ns_index: usize, var_index: usize) -> Option<Def<'_>> {
        let (_, namespace) = self.program.namespaces.get_index(ns_index)?;
        let (var, value) = namespace.scope.vars.get_index(var_index)?;
        Some(Def {
            ns: &self.ns_names[ns_index],
            name: var.sym.name(),
            var,
            value,
        })
    }

    /// The definition of `name` in namespace `ns`, as written (imports not
    /// followed).
    pub fn get(&self, ns: &str, name: &str) -> Option<Def<'_>> {
        let &i = self.ns_lookup.get(ns)?;
        let &j = self.vars[i].get(name)?;
        self.def_at(i, j)
    }

    /// Definitions in a namespace, in source order.
    pub fn defs_in(&self, ns: &str) -> Vec<Def<'_>> {
        let Some(&i) = self.ns_lookup.get(ns) else {
            return Vec::new();
        };
        let mut indices: Vec<usize> = self.vars[i].values().copied().collect();
        indices.sort_unstable();
        indices
            .into_iter()
            .filter_map(|j| self.def_at(i, j))
            .collect()
    }

    /// Every definition, namespace by namespace.
    pub fn all_defs(&self) -> impl Iterator<Item = Def<'_>> {
        (0..self.ns_names.len()).flat_map(move |i| {
            let mut indices: Vec<usize> = self.vars[i].values().copied().collect();
            indices.sort_unstable();
            indices.into_iter().filter_map(move |j| self.def_at(i, j))
        })
    }

    /// The namespace that defines core name `name`.
    pub fn core_ns(&self, name: &str) -> Option<&str> {
        self.core.get(name).map(String::as_str)
    }

    /// Core definitions (auto-imported everywhere).
    pub fn core_defs(&self) -> Vec<Def<'_>> {
        let mut defs: Vec<Def<'_>> = self
            .core
            .iter()
            .filter_map(|(name, ns)| self.get(ns, name))
            .collect();
        defs.sort_by(|a, b| a.name.cmp(b.name));
        defs
    }

    /// Aliases a namespace declares, in every file.
    pub fn aliases(&self, ns: &str) -> Vec<(String, String)> {
        self.namespace(ns)
            .map(|namespace| {
                namespace
                    .aliases
                    .iter()
                    .map(|(alias, target)| (alias.to_string(), target.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The qualified target of a var-import definition (`s ::ns/name`).
    fn import_target(&self, def: &Def<'_>) -> Option<(String, String)> {
        let ns_ref = match def.value {
            Value::Ref(Ref::Ns(ns_ref)) => ns_ref,
            Value::MultipleValues(values) => values.iter().find_map(|v| match v {
                Value::Ref(Ref::Ns(ns_ref)) => Some(ns_ref),
                _ => None,
            })?,
            _ => return None,
        };
        let name = ns_ref.function_name.clone()?;
        let namespace = self.namespace(def.ns)?;
        let ns = namespace
            .aliases
            .get(&ns_ref.ns)
            .unwrap_or(&ns_ref.ns)
            .to_string();
        Some((ns, name))
    }

    /// Follow var-imports to the definition they name.
    pub fn follow<'a>(&'a self, mut def: Def<'a>) -> Def<'a> {
        for _ in 0..MAX_IMPORT_HOPS {
            match self
                .import_target(&def)
                .and_then(|(ns, name)| self.get(&ns, &name))
            {
                Some(target) if target.qualified() != def.qualified() => def = target,
                _ => break,
            }
        }
        def
    }

    /// Whether a definition is a var-import.
    pub fn is_import(&self, def: &Def<'_>) -> bool {
        self.import_target(def).is_some()
    }
}

/// The name-resolution facts of one document text, computed once.
pub struct DocContext {
    pub syntax: Syntax,
    decls: Vec<(usize, String)>,
    aliases: Vec<(String, String, String)>,
    imports: Vec<(String, String, RefPath)>,
    locals: Vec<Local>,
}

impl DocContext {
    pub fn new(text: &str) -> Self {
        let syntax = Syntax::new(text);
        let decls = syntax.ns_decls();
        let aliases = syntax.aliases();
        let imports = syntax.imports();
        let locals = syntax.locals(text);
        Self {
            syntax,
            decls,
            aliases,
            imports,
            locals,
        }
    }

    /// The namespace in effect at `offset`.
    pub fn namespace_at(&self, offset: usize) -> String {
        self.decls
            .iter()
            .take_while(|(start, _)| *start <= offset)
            .last()
            .map(|(_, ns)| ns.clone())
            .unwrap_or_else(|| "::".to_string())
    }
}

/// Name-resolution context of one document position.
pub struct Scope<'a> {
    pub index: &'a Index,
    /// The namespace in effect.
    pub ns: String,
    /// Aliases in effect: the document's own (fresh while typing) first,
    /// then those other files declare for the namespace.
    aliases: Vec<(String, String)>,
    /// The document's var-imports in this namespace, by local name.
    imports: AHashMap<String, RefPath>,
    /// Locals visible at the position.
    locals: Vec<Local>,
}

impl<'a> Scope<'a> {
    pub fn new(index: &'a Index, doc: &DocContext, offset: usize) -> Self {
        let ns = doc.namespace_at(offset);
        let mut aliases: Vec<(String, String)> = doc
            .aliases
            .iter()
            .filter(|(alias_ns, _, _)| *alias_ns == ns)
            .map(|(_, alias, target)| (alias.clone(), target.clone()))
            .collect();
        aliases.extend(index.aliases(&ns));
        let imports = doc
            .imports
            .iter()
            .filter(|(import_ns, _, _)| *import_ns == ns)
            .map(|(_, name, target)| (name.clone(), target.clone()))
            .collect();
        let locals = doc
            .locals
            .iter()
            .filter(|l| {
                // On its own declaration, or where it is in scope.
                (l.start <= offset && offset <= l.end)
                    || (l.scope.0 <= offset && offset <= l.scope.1 && l.visible_from <= offset)
            })
            .cloned()
            .collect();
        Self {
            index,
            ns,
            aliases,
            imports,
            locals,
        }
    }

    /// A scope in namespace `ns` with no document around it, for names
    /// written inside a definition (its annotations, its return type).
    pub fn in_namespace(index: &'a Index, ns: &str) -> Self {
        Self {
            index,
            ns: ns.to_string(),
            aliases: index.aliases(ns),
            imports: AHashMap::new(),
            locals: Vec::new(),
        }
    }

    /// Expand an alias at the start of a namespace path.
    pub fn expand(&self, ns: &str) -> String {
        for (alias, target) in &self.aliases {
            if ns == alias {
                return target.clone();
            }
            if let Some(rest) = ns.strip_prefix(alias.as_str())
                && rest.starts_with("::")
            {
                return format!("{target}{rest}");
            }
        }
        ns.to_string()
    }

    pub fn aliases(&self) -> &[(String, String)] {
        &self.aliases
    }

    pub fn locals(&self) -> &[Local] {
        &self.locals
    }

    /// The innermost local named `name`.
    pub fn local(&self, name: &str) -> Option<&Local> {
        self.locals
            .iter()
            .filter(|l| l.name == name)
            .max_by_key(|l| l.start)
    }

    /// A top-level definition by qualified namespace and name.
    fn qualified(&self, ns: &str, name: &str) -> Option<Def<'a>> {
        let expanded = self.expand(ns);
        self.index
            .get(&expanded, name)
            .or_else(|| self.index.get(ns, name))
            .map(|def| self.index.follow(def))
    }

    /// Resolve a bare name that is not a local.
    pub fn global(&self, name: &str) -> Option<Def<'a>> {
        if let Some(target) = self.imports.get(name)
            && let (Some(ns), Some(target_name)) = (&target.ns, &target.name)
            && let Some(def) = self.qualified(ns, target_name)
        {
            return Some(def);
        }
        if let Some(def) = self.index.get(&self.ns, name) {
            return Some(self.index.follow(def));
        }
        let core_ns = self.index.core_ns(name)?;
        self.index
            .get(core_ns, name)
            .map(|def| self.index.follow(def))
    }

    /// Resolve the name part of a reference.
    pub fn resolve(&self, path: &RefPath) -> Option<Resolved<'a>> {
        let name = path.name.as_deref()?;
        match &path.ns {
            Some(ns) => self.qualified(ns, name).map(Resolved::Def),
            None => {
                if let Some(local) = self.local(name) {
                    return Some(Resolved::Local(local.clone()));
                }
                self.global(name).map(Resolved::Def)
            }
        }
    }

    /// Resolve a reference to a top-level definition, ignoring locals.
    pub fn resolve_def(&self, path: &RefPath) -> Option<Def<'a>> {
        match self.resolve(path)? {
            Resolved::Def(def) => Some(def),
            Resolved::Local(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(sources: &[(&str, &str)]) -> Program {
        let mut namespaces = Default::default();
        for (path, text) in sources {
            let parsed = hot::lang::parser::parse_hot_file(text, path).unwrap();
            hot::lang::engine::discover::merge_program_namespaces_strict(
                &mut namespaces,
                parsed.namespaces,
            )
            .unwrap();
        }
        Program {
            namespaces,
            current_namespace: hot::lang::ast::NsPath::hot_main(),
        }
    }

    fn offset_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).expect(needle)].chars().count()
    }

    #[test]
    fn core_names_resolve_to_the_namespace_that_defines_them() {
        // Two namespaces define `get`; only one marks it core.
        let index = Index::new(program(&[
            (
                "/coll.hot",
                "::hot::coll ns\n\nget meta {core: true} fn (m: Map, k: Str): Any { m }\n",
            ),
            (
                "/store.hot",
                "::hot::store ns\n\nget fn (k: Str): Any { k }\n",
            ),
        ]));
        let text = "::app ns\n\nf fn (m: Map): Any { get(m, \"k\") }\n";
        let doc = DocContext::new(text);
        let offset = offset_of(text, "get(");
        let scope = Scope::new(&index, &doc, offset);
        let path = doc.syntax.path_at(offset).unwrap();
        let def = scope.resolve_def(&path).unwrap();
        assert_eq!(def.qualified(), "::hot::coll/get");
    }

    #[test]
    fn aliases_imports_and_locals() {
        let lib = "::mylib::util ns\n\nshout fn (s: Str): Str { s }\n";
        let app = "::app::main ns  // the app\n\n::util ::mylib::util // helpers\nsh ::mylib::util/shout\n\nf fn (total: Int): Str {\n  ::util/shout(\"a\")\n  sh(\"b\")\n  total\n}\n\ntotal fn (): Int { 1 }\n";
        let index = Index::new(program(&[("/lib.hot", lib), ("/app.hot", app)]));
        let doc = DocContext::new(app);

        let at = |needle: &str, delta: usize| {
            let offset = offset_of(app, needle) + delta;
            let scope = Scope::new(&index, &doc, offset);
            let path = doc.syntax.path_at(offset).unwrap();
            match scope.resolve(&path) {
                Some(Resolved::Def(def)) => def.qualified(),
                Some(Resolved::Local(local)) => format!("local {}", local.name),
                None => "none".to_string(),
            }
        };
        assert_eq!(at("::util/shout(", 8), "::mylib::util/shout");
        assert_eq!(at("sh(\"b\")", 0), "::mylib::util/shout");
        // The parameter shadows the namespace's `total`.
        assert_eq!(at("  total\n}", 2), "local total");
        assert_eq!(at("total fn (): Int", 0), "::app::main/total");
    }
}
