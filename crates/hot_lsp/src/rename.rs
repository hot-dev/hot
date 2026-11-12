//! Rename.
//!
//! A rename edits every name find-references finds, so it is only offered
//! for what those references are exact for: a local, a parameter, and a
//! function, type or binding a project file defines. It refuses where an
//! edit could change what the code means:
//!
//! * a field, a named argument's label or an enum variant, whose uses on
//!   values of unknown type can't all be found;
//! * a binding whose name is a key of its block's result (`parallel`, `All`);
//! * an import written under another name, and anything a library defines;
//! * a new name that is not one identifier, or that already means something
//!   where the old name is declared or used.
//!
//! The server also checks the project with the edits applied before it
//! answers (`Backend::rename`), so an edit that would add a problem is
//! not sent.

use crate::features::{self, RefSite, Target};
use crate::index::{DocContext, Index, Resolved, Resolver, Scope};
use crate::syntax::LocalKind;
use crate::text::LineIndex;
use ahash::AHashSet;
use hot::lang::ast::Value;
use hot::lang::lexer::{Lexer, TokenType};
use tower_lsp_server::ls_types::Uri;

/// What a rename renames.
pub struct Renaming {
    pub target: Target,
    /// The name as it is now.
    pub old: String,
    /// Char span of the name under the cursor.
    pub span: (usize, usize),
    /// The namespace that defines it, for a definition.
    ns: Option<String>,
    is_type: bool,
}

/// One replacement in a document: a char span and its new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// What the cursor at `offset` would rename. `Ok(None)` when it is on
/// nothing with a name; `Err` says why a name there can't be renamed.
pub fn prepare(
    index: &Index,
    uri: &Uri,
    ctx: &DocContext,
    offset: usize,
    project_files: &AHashSet<String>,
) -> Result<Option<Renaming>, String> {
    if ctx.syntax.in_string(offset) {
        return Ok(None);
    }
    let local = |name: &str, start: usize, span: (usize, usize)| {
        let Some(local) = ctx.locals().iter().find(|l| l.start == start) else {
            return Err(format!(
                "`{name}` can't be renamed: its function has no body yet."
            ));
        };
        if matches!(local.kind, LocalKind::Binding(..)) && ctx.syntax.keys_a_result(local.start) {
            return Err(format!(
                "`{name}` can't be renamed: it is a key of its block's result."
            ));
        }
        Ok(Some(Renaming {
            target: Target::Local {
                uri: uri.clone(),
                start,
            },
            old: name.to_string(),
            span,
            ns: None,
            is_type: false,
        }))
    };
    // `%.amount` in a precondition: the parameter.
    if let Some(arg) = ctx.arg_ref_at(offset) {
        return local(&arg.name, arg.param_start, (arg.start, arg.end));
    }
    if let Some(key) = ctx.syntax.key_at(offset) {
        return Err(format!(
            "`{}` can't be renamed: a field's uses can't all be found.",
            key.name
        ));
    }
    if let Some(name) = ctx.syntax.label_at(offset) {
        return Err(format!(
            "`{name}` can't be renamed: a variant's or a branch's uses can't all be found."
        ));
    }
    if ctx.syntax.placeholder_at(offset).is_some() {
        return Ok(None);
    }
    let Some(path) = ctx.syntax.path_at(offset) else {
        return Ok(None);
    };
    if let Some(member) = path.cursor_member {
        return Err(format!(
            "`{}` can't be renamed: a field's or a variant's uses can't all be found.",
            path.members[member]
        ));
    }
    let Some(name) = path.name.as_deref() else {
        return Err("A namespace can't be renamed.".to_string());
    };
    if offset < path.name_start || offset > path.name_end {
        return Err("A namespace can't be renamed.".to_string());
    }
    let span = (path.name_start, path.name_end);
    match Scope::new(index, ctx, offset).resolve(&features::head(&path)) {
        None => Err(format!(
            "`{name}` can't be renamed: nothing in the checked project defines it."
        )),
        Some(Resolved::Local(found)) => local(name, found.start, span),
        Some(Resolved::Def(def)) => {
            if def.name != name {
                return Err(format!(
                    "`{name}` is an import of `{}`. Rename that where it is defined.",
                    def.qualified()
                ));
            }
            let in_project = def
                .var
                .src
                .as_ref()
                .and_then(|src| src.file.as_ref())
                .is_some_and(|file| project_files.contains(file));
            if !in_project {
                return Err(format!(
                    "`{}` can't be renamed: it is defined outside this project.",
                    def.qualified()
                ));
            }
            Ok(Some(Renaming {
                target: Target::Def {
                    qualified: def.qualified(),
                    name: def.name.to_string(),
                },
                old: name.to_string(),
                span,
                ns: Some(def.ns.to_string()),
                is_type: matches!(def.value, Value::TypeDef(_)),
            }))
        }
    }
}

/// Whether `new` can replace the name: it is one identifier, and the
/// namespace doesn't define it already.
pub fn check_name(index: &Index, renaming: &Renaming, new: &str) -> Result<(), String> {
    let mut lexer = Lexer::new(new);
    let tokens = lexer.tokenize();
    let one_identifier = matches!(
        tokens.first().map(|t| &t.token_type),
        Some(TokenType::Identifier(name)) if name == new
    ) && tokens
        .iter()
        .skip(1)
        .all(|t| matches!(t.token_type, TokenType::Eof));
    if !one_identifier || matches!(new, "where" | "meta" | "ns" | "open" | "_") {
        return Err(format!("`{new}` is not a name."));
    }
    let capital = |name: &str| name.chars().next().is_some_and(char::is_uppercase);
    if renaming.is_type && capital(&renaming.old) && !capital(new) {
        return Err(format!(
            "`{new}` is not a type name: a type's name starts with a capital letter."
        ));
    }
    if let Some(ns) = &renaming.ns
        && index.get(ns, new).is_some()
    {
        return Err(format!("`{ns}` already defines `{new}`."));
    }
    Ok(())
}

/// The edits that rename the target in one document. `Err` when `new`
/// already means something where the old name is declared or used.
pub fn edits(
    index: &Index,
    ctx: &DocContext,
    renaming: &Renaming,
    new: &str,
) -> Result<Vec<Edit>, String> {
    let sites = features::reference_sites(index, ctx, &renaming.target);
    if sites.is_empty() {
        return Ok(Vec::new());
    }
    let old = &renaming.old;
    match &renaming.target {
        Target::Local { start, .. } => {
            let Some(local) = ctx.locals().iter().find(|l| l.start == *start) else {
                return Ok(Vec::new());
            };
            let scope = (local.scope.0.min(local.start), local.scope.1);
            // Another local of the new name, where this one is visible.
            let shadows = ctx.locals().iter().any(|other| {
                other.name == new
                    && other.start != local.start
                    && other.scope.0.min(other.start) <= scope.1
                    && scope.0 <= other.scope.1
            });
            // A use of the new name that this local would capture.
            let captures = ctx.syntax.all_scopes().any(|syntax| {
                syntax.references().iter().any(|path| {
                    path.ns.is_none()
                        && path.name.as_deref() == Some(new)
                        && scope.0 <= path.name_start
                        && path.name_start <= scope.1
                })
            });
            if shadows || captures {
                return Err(format!(
                    "`{old}` can't be renamed to `{new}`: `{new}` is already used where `{old}` is visible."
                ));
            }
        }
        Target::Def { .. } => {
            let mut resolver = Resolver::new(index, ctx);
            for site in sites.iter().filter(|site| site.bare) {
                if ctx.local_at(new, site.start).is_some()
                    || resolver.scope(site.start).global(new).is_some()
                {
                    return Err(format!(
                        "`{old}` can't be renamed to `{new}`: `{new}` already names something where `{old}` is used."
                    ));
                }
            }
        }
    }
    Ok(sites
        .iter()
        .map(|site: &RefSite| Edit {
            start: site.start,
            end: site.end,
            // `{name, email}`: the key stays, the value is renamed.
            text: if site.bare && ctx.syntax.punned_at(site.start) {
                format!("{old}: {new}")
            } else {
                new.to_string()
            },
        })
        .collect())
}

/// `text` with `edits` applied.
pub fn apply(text: &str, edits: &[Edit]) -> String {
    let lines = LineIndex::new(text);
    let mut edits: Vec<&Edit> = edits.iter().collect();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut out = text.to_string();
    for edit in edits {
        let start = lines.char_to_byte(text, edit.start);
        let end = lines.char_to_byte(text, edit.end);
        out.replace_range(start..end, &edit.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::lang::ast::Program;

    struct Project {
        index: Index,
        files: Vec<(&'static str, &'static str)>,
        project: AHashSet<String>,
    }

    /// A project of `files`, all but those under `/lib/` its own.
    fn project(files: &[(&'static str, &'static str)]) -> Project {
        let mut namespaces = Default::default();
        for (path, text) in files {
            let parsed = hot::lang::parser::parse_hot_file(text, path).unwrap();
            hot::lang::engine::discover::merge_program_namespaces_strict(
                &mut namespaces,
                parsed.namespaces,
            )
            .unwrap();
        }
        Project {
            index: Index::new(Program {
                namespaces,
                current_namespace: hot::lang::ast::NsPath::hot_main(),
            }),
            files: files.to_vec(),
            project: files
                .iter()
                .filter(|(path, _)| !path.starts_with("/lib/"))
                .map(|(path, _)| path.to_string())
                .collect(),
        }
    }

    fn offset_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).expect(needle)].chars().count()
    }

    impl Project {
        /// Rename what is `delta` chars into the first `needle` of `file`
        /// to `new`: each file's text afterwards, for the files that change.
        fn rename(
            &self,
            file: &str,
            needle: &str,
            delta: usize,
            new: &str,
        ) -> Result<Vec<(&'static str, String)>, String> {
            let text = self.files.iter().find(|(path, _)| *path == file).unwrap().1;
            let uri: Uri = format!("file://{file}").parse().unwrap();
            let ctx = DocContext::new(text);
            let offset = offset_of(text, needle) + delta;
            let renaming = prepare(&self.index, &uri, &ctx, offset, &self.project)?
                .ok_or_else(|| "nothing to rename".to_string())?;
            check_name(&self.index, &renaming, new)?;
            let mut changed = Vec::new();
            for (path, text) in &self.files {
                if matches!(renaming.target, Target::Local { .. }) && *path != file {
                    continue;
                }
                let ctx = DocContext::new(text);
                let edits = edits(&self.index, &ctx, &renaming, new)?;
                if !edits.is_empty() {
                    let renamed = apply(text, &edits);
                    // What a rename writes must still parse.
                    hot::lang::parser::parse_hot_file(&renamed, path)
                        .unwrap_or_else(|e| panic!("{renamed}\n{e:?}"));
                    changed.push((*path, renamed));
                }
            }
            Ok(changed)
        }

        /// The one file a rename changes, afterwards.
        fn renamed(&self, file: &str, needle: &str, delta: usize, new: &str) -> String {
            let mut changed = self.rename(file, needle, delta, new).unwrap();
            assert_eq!(changed.len(), 1, "{changed:?}");
            changed.remove(0).1
        }

        fn refused(&self, file: &str, needle: &str, delta: usize, new: &str) -> String {
            self.rename(file, needle, delta, new).unwrap_err()
        }
    }

    const APP: &str = r#"::app ns

limit 10

withdraw fn (amount: Int, balance: Int)
    where lte(%.amount, %.balance)
    : Int where gte(%, 0)
{
    left sub(balance, amount)
    note `took ${amount} of ${balance}, ${`left ${left}`}`
    left
}

clamp fn (x: Int): Int {
    x min(x, limit)
    x
}

each fn (xs: Vec<Int>): Vec<Int> {
    map(xs, (x) { add(x, limit) })
}

pick fn match (r: Result): Any {
    Result.Ok (value) => { value }
    _ => { null }
}
"#;

    #[test]
    fn a_parameter_is_renamed_in_its_body_its_precondition_and_templates() {
        let project = project(&[("/app.hot", APP)]);
        let renamed = project.renamed("/app.hot", "sub(balance, amount)", 13, "sum");
        assert!(
            renamed.contains("withdraw fn (sum: Int, balance: Int)"),
            "{renamed}"
        );
        assert!(renamed.contains("where lte(%.sum, %.balance)"), "{renamed}");
        assert!(renamed.contains("left sub(balance, sum)"), "{renamed}");
        assert!(renamed.contains("`took ${sum} of ${balance}"), "{renamed}");
        assert!(!renamed.contains("amount"), "{renamed}");
        // From the precondition's `%.amount`, the same rename.
        assert_eq!(project.renamed("/app.hot", "%.amount", 2, "sum"), renamed);
        // And from the declaration.
        assert_eq!(
            project.renamed("/app.hot", "amount: Int", 0, "sum"),
            renamed
        );
    }

    #[test]
    fn a_local_is_renamed_where_it_is_visible_and_no_further() {
        let project = project(&[("/app.hot", APP)]);
        // A binding, into a template nested in a template.
        let renamed = project.renamed("/app.hot", "left sub(", 0, "rest");
        assert!(renamed.contains("rest sub(balance, amount)"), "{renamed}");
        assert!(
            renamed.contains("${`left ${rest}`}`\n    rest\n}"),
            "{renamed}"
        );
        // The parameter `x` of `clamp`: its use in the value that shadows
        // it, and nothing after.
        let renamed = project.renamed("/app.hot", "x: Int): Int {\n    x min", 0, "n");
        assert!(
            renamed.contains("clamp fn (n: Int): Int {\n    x min(n, limit)\n    x\n}"),
            "{renamed}"
        );
        // The binding that shadows it: its uses after, not the lambda's `x`.
        let renamed = project.renamed("/app.hot", "x min(x, limit)", 0, "low");
        assert!(
            renamed.contains("clamp fn (x: Int): Int {\n    low min(x, limit)\n    low\n}"),
            "{renamed}"
        );
        assert!(renamed.contains("(x) { add(x, limit) }"), "{renamed}");
        // A lambda's parameter and a match arm's binding.
        let renamed = project.renamed("/app.hot", "(x) { add(x", 1, "item");
        assert!(
            renamed.contains("map(xs, (item) { add(item, limit) })"),
            "{renamed}"
        );
        let renamed = project.renamed("/app.hot", "{ value }", 2, "v");
        assert!(renamed.contains("Result.Ok (v) => { v }"), "{renamed}");
    }

    #[test]
    fn a_new_name_that_is_taken_is_refused() {
        let project = project(&[
            ("/app.hot", APP),
            (
                "/lib/core.hot",
                "::hot::core ns\n\nadd meta {core: true} fn (...xs: Any): Any { xs }\n",
            ),
        ]);
        // Another parameter, a later binding, a name used in the body.
        for (needle, new) in [
            ("amount: Int", "balance"),
            ("amount: Int", "left"),
            ("amount: Int", "sub"),
            ("x: Int): Int {\n    x min", "limit"),
            ("xs: Vec<Int>", "x"),
        ] {
            let error = project.refused("/app.hot", needle, 0, new);
            assert!(error.contains("already used"), "{needle} -> {new}: {error}");
        }
        // A definition: a name the namespace has, a core name, and a local
        // where it is used.
        let error = project.refused("/app.hot", "limit 10", 0, "clamp");
        assert!(error.contains("already defines"), "{error}");
        let error = project.refused("/app.hot", "limit 10", 0, "add");
        assert!(error.contains("already names something"), "{error}");
        let error = project.refused("/app.hot", "limit 10", 0, "xs");
        assert!(error.contains("already names something"), "{error}");
    }

    #[test]
    fn only_a_name_is_a_new_name() {
        let project = project(&[(
            "/app.hot",
            "::app ns\n\nPoint type { x: Int }\n\nf fn (a: Int): Int { a }\n",
        )]);
        for new in [
            "", "a b", "1a", "fn", "match", "where", "a.b", "::a", "a(", "true", "null", "%",
            "\"a\"", "a/b",
        ] {
            let error = project.refused("/app.hot", "a: Int", 0, new);
            assert!(error.contains("is not a name"), "{new:?}: {error}");
        }
        let error = project.refused("/app.hot", "Point type", 0, "point");
        assert!(error.contains("capital letter"), "{error}");
        let renamed = project.renamed("/app.hot", "a: Int", 0, "new-name");
        assert!(
            renamed.contains("f fn (new-name: Int): Int { new-name }"),
            "{renamed}"
        );
    }

    const MODEL: &str = r#"::app::model ns

Circle type { radius: Dec }
Shape enum { Circle(Circle), Dot }

area fn (c: Circle): Dec { mul(c.radius, c.radius) }

area-of fn (radius: Dec): Dec {
    area(Circle(radius: radius))
}

origin Circle({radius: 0.0})
"#;

    const USES: &str = r#"::app::uses ns

::model ::app::model
area ::app::model/area
ar ::app::model/area

Circle type { r: Int }

big fn (c: ::model/Circle): Dec {
    shape ::model/Shape.Circle(radius: 1.0)
    add(area(c), ar(c), ::model/area(c), ::app::model/area(::model/origin))
}

own fn (): Circle { Circle(r: 1) }

report fn (c: ::model/Circle): Map {
    area ::model/area(c)
    {area, c}
}
"#;

    #[test]
    fn a_definition_is_renamed_in_every_file() {
        let project = project(&[("/model.hot", MODEL), ("/uses.hot", USES)]);
        let changed = project
            .rename("/model.hot", "area fn", 0, "surface")
            .unwrap();
        let model = &changed
            .iter()
            .find(|(path, _)| *path == "/model.hot")
            .unwrap()
            .1;
        let uses = &changed
            .iter()
            .find(|(path, _)| *path == "/uses.hot")
            .unwrap()
            .1;
        assert!(model.contains("surface fn (c: Circle)"), "{model}");
        assert!(
            model.contains("    surface(Circle(radius: radius))"),
            "{model}"
        );
        // `area-of` is another name.
        assert!(model.contains("area-of fn"), "{model}");
        // An import under the same name is renamed with its uses. One
        // under another name keeps its name.
        assert!(uses.contains("surface ::app::model/surface\n"), "{uses}");
        assert!(uses.contains("ar ::app::model/surface\n"), "{uses}");
        assert!(
            uses.contains(
                "add(surface(c), ar(c), ::model/surface(c), ::app::model/surface(::model/origin))"
            ),
            "{uses}"
        );
        // The local `area` in `report` is not the function.
        assert!(
            uses.contains("    area ::model/surface(c)\n    {area, c}"),
            "{uses}"
        );
        // From a use in the other file, through the alias: the same edits.
        assert_eq!(
            project
                .rename("/uses.hot", "::model/area(c), ::app", 9, "surface")
                .unwrap(),
            changed
        );
    }

    #[test]
    fn a_type_is_renamed_and_its_namesakes_are_not() {
        let project = project(&[("/model.hot", MODEL), ("/uses.hot", USES)]);
        let changed = project
            .rename("/model.hot", "Circle type", 0, "Disc")
            .unwrap();
        let model = &changed
            .iter()
            .find(|(path, _)| *path == "/model.hot")
            .unwrap()
            .1;
        let uses = &changed
            .iter()
            .find(|(path, _)| *path == "/uses.hot")
            .unwrap()
            .1;
        assert!(model.contains("Disc type { radius: Dec }"), "{model}");
        // The variant keeps its name. Its payload is the type.
        assert!(
            model.contains("Shape enum { Circle(Disc), Dot }"),
            "{model}"
        );
        assert!(model.contains("area fn (c: Disc)"), "{model}");
        assert!(model.contains("area(Disc(radius: radius))"), "{model}");
        assert!(model.contains("origin Disc({radius: 0.0})"), "{model}");
        assert!(uses.contains("big fn (c: ::model/Disc): Dec"), "{uses}");
        // `Shape.Circle` is the variant, and `::app::uses/Circle` another type.
        assert!(uses.contains("::model/Shape.Circle(radius: 1.0)"), "{uses}");
        assert!(uses.contains("Circle type { r: Int }"), "{uses}");
        assert!(
            uses.contains("own fn (): Circle { Circle(r: 1) }"),
            "{uses}"
        );
    }

    #[test]
    fn a_punned_map_entry_keeps_its_key() {
        let project = project(&[("/model.hot", MODEL), ("/uses.hot", USES)]);
        let renamed = project.renamed("/uses.hot", "area ::model/area(c)\n", 0, "size");
        assert!(
            renamed.contains("    size ::model/area(c)\n    {area: size, c}"),
            "{renamed}"
        );
        let renamed = project.renamed("/uses.hot", "c: ::model/Circle): Map", 0, "circle");
        assert!(
            renamed.contains("    area ::model/area(circle)\n    {area, c: circle}"),
            "{renamed}"
        );
        // A top-level name, punned in another file's map.
        let project = self::project(&[
            ("/a.hot", "::a ns\n\nname \"x\"\n"),
            (
                "/b.hot",
                "::b ns\n\nname ::a/name\n\nf fn (): Map { {name,} }\n",
            ),
        ]);
        let changed = project.rename("/a.hot", "name \"x\"", 0, "title").unwrap();
        let b = &changed
            .iter()
            .find(|(path, _)| *path == "/b.hot")
            .unwrap()
            .1;
        assert!(b.contains("title ::a/title\n"), "{b}");
        assert!(b.contains("{ {name: title,} }"), "{b}");
    }

    #[test]
    fn what_cannot_be_renamed_exactly_is_refused() {
        let project = project(&[
            ("/model.hot", MODEL),
            ("/uses.hot", USES),
            (
                "/lib/hot.hot",
                "::hot::math ns\n\nmul meta {core: true} fn (a: Dec, b: Dec): Dec { a }\nadd meta {core: true} fn (...xs: Any): Any { xs }\n",
            ),
        ]);
        // A field where it is declared, a named argument's label, a field
        // access, a variant where it is used and where it is declared.
        for (file, needle, delta) in [
            ("/model.hot", "Circle(Circle)", 0),
            ("/model.hot", "radius: Dec }", 0),
            ("/model.hot", "radius: radius))", 0),
            ("/model.hot", "c.radius", 2),
            ("/uses.hot", "Shape.Circle", 6),
        ] {
            let error = project.refused(file, needle, delta, "r");
            assert!(error.contains("can't all be found"), "{needle}: {error}");
        }
        // A namespace, on its own or before a name.
        let error = project.refused("/uses.hot", "::model/Circle", 3, "m");
        assert!(error.contains("namespace"), "{error}");
        let error = project.refused("/uses.hot", "::model ::app::model", 3, "m");
        assert!(error.contains("namespace"), "{error}");
        // An import under another name.
        let error = project.refused("/uses.hot", "ar(c)", 0, "a2");
        assert!(
            error.contains("is an import of `::app::model/area`"),
            "{error}"
        );
        // A library's definition.
        let error = project.refused("/model.hot", "mul(c.radius", 0, "times");
        assert!(error.contains("outside this project"), "{error}");
        // A keyword, a string, a comment and `%` are not names.
        let text = "::t ns\n\n// note\nf fn (xs: Vec<Int>): Vec<Int> { map(xs, add(%, 1)) } // \"s\"\ns \"text\"\n";
        let project = self::project(&[("/t.hot", text)]);
        for (needle, delta) in [("fn (xs", 0), ("\"text\"", 2), ("%, 1", 0), ("note", 1)] {
            let error = project.refused("/t.hot", needle, delta, "z");
            assert!(error.contains("nothing to rename"), "{needle}: {error}");
        }
    }

    #[test]
    fn a_name_that_keys_a_result_is_refused() {
        let text = r#"::app ns

load fn (id: Str): Map {
    data parallel {
        customer fetch(id)
        orders fetch(id)
    }
    data.customer
}

measure fn (xs: Vec<Int>): All<Map> {
    total length(xs)
    total
}

plain fn (id: Str): Str {
    kept fetch(id)
    effects cond-all {
        eq(id, "a") => kept { kept }
    }
    kept
}

fetch fn (id: Str): Str { id }
"#;
        let project = project(&[("/app.hot", text)]);
        for needle in ["customer fetch", "total length"] {
            let error = project.refused("/app.hot", needle, 0, "renamed");
            assert!(
                error.contains("key of its block's result"),
                "{needle}: {error}"
            );
        }
        // The flow's own name is an ordinary binding.
        let renamed = project.renamed("/app.hot", "data parallel", 0, "loaded");
        assert!(renamed.contains("loaded parallel {"), "{renamed}");
        assert!(renamed.contains("loaded.customer"), "{renamed}");
        // A branch name is not the binding of that name.
        let error = project.refused("/app.hot", "kept { kept }", 0, "saved");
        assert!(error.contains("can't all be found"), "{error}");
        let renamed = project.renamed("/app.hot", "kept fetch", 0, "saved");
        assert!(renamed.contains("saved fetch(id)"), "{renamed}");
        assert!(
            renamed.contains("=> kept { saved }\n    }\n    saved\n"),
            "{renamed}"
        );
    }

    #[test]
    fn edits_apply_by_char_offset() {
        let text = "::a ns\n\nf fn (n: Int): Str { `é 🎉 ${n} — ${n}` }\n";
        let project = project(&[("/a.hot", text)]);
        let renamed = project.renamed("/a.hot", "n: Int", 0, "count");
        assert!(
            renamed.contains("f fn (count: Int): Str { `é 🎉 ${count} — ${count}` }"),
            "{renamed}"
        );
    }
}
