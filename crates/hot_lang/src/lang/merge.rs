//! Merging parsed files into one program.
//!
//! Every file parses to its own namespace map; these functions combine them
//! the way `hot` does for project sources (strict: a name is declared once
//! per namespace) and for eval/REPL input (a later declaration shadows).

use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;

/// Policy for resolving a same-name binding when merging a namespace that
/// already exists in the target map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsMergePolicy {
    /// Static source files: a name may be declared only once per namespace
    /// across files. A second declaration from a different file is an error —
    /// this keeps the merged program independent of file, unit, and
    /// cache-hit/miss ordering (a conflict errors no matter which side was
    /// merged first).
    Strict,
    /// Eval / REPL input: a later declaration shadows earlier ones. The
    /// binding is appended, so name resolution and execution order pick the
    /// newest definition — the same rule that lets accumulated REPL source
    /// redefine a function within one file.
    Shadow,
}

/// Source file a binding came from, for conflict diagnostics and same-file
/// detection. Falls back to the namespace's file when the var has no span.
fn binding_file(
    var: &crate::lang::ast::Var,
    ns_source_file: &Option<std::path::PathBuf>,
) -> Option<String> {
    var.src
        .as_ref()
        .and_then(|s| s.file.clone())
        .or_else(|| ns_source_file.as_ref().map(|p| p.display().to_string()))
}

fn file_label(file: &Option<String>) -> &str {
    file.as_deref().unwrap_or("<unknown file>")
}

/// Whether two source paths name the same file. Discovery yields canonical
/// paths, but a `hot run` target keeps the path as typed (relative, or through
/// a symlink such as macOS `/tmp`), so compare canonical forms when the strings
/// differ.
fn same_source_file(a: &str, b: &str) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

fn literal_type_expr(expr: &crate::lang::ast::TypeExpr) -> bool {
    match expr {
        crate::lang::ast::TypeExpr::Literal(_) => true,
        crate::lang::ast::TypeExpr::Union(parts) => parts.iter().all(literal_type_expr),
        _ => false,
    }
}

/// Repeated open literal-union declarations are extensions, not ordinary
/// redefinitions. Keep both declarations so the type checker can compose
/// their members (and report the dedicated open/closed mismatch when needed).
fn is_open_literal_union_pair(
    prior: &crate::lang::ast::Value,
    incoming: &crate::lang::ast::Value,
) -> bool {
    let (crate::lang::ast::Value::TypeDef(prior), crate::lang::ast::Value::TypeDef(incoming)) =
        (prior, incoming)
    else {
        return false;
    };
    let (Some(prior_alias), Some(incoming_alias)) = (&prior.type_alias, &incoming.type_alias)
    else {
        return false;
    };

    (prior.is_open || incoming.is_open)
        && literal_type_expr(prior_alias)
        && literal_type_expr(incoming_alias)
}

fn static_deep_path_parts(path: &crate::lang::ast::DeepPath) -> Option<Vec<String>> {
    use crate::lang::ast::DeepPath;

    match path {
        DeepPath::Key(key) => Some(vec![format!("key:{key}")]),
        DeepPath::Index(index) => Some(vec![format!("index:{index}")]),
        DeepPath::Chain(left, right) => {
            let mut parts = static_deep_path_parts(left)?;
            parts.extend(static_deep_path_parts(right)?);
            Some(parts)
        }
        DeepPath::DynamicIndex(_) | DeepPath::Append => None,
    }
}

/// Two static deep-set assignments commute only when their paths diverge.
/// Prefix-related, dynamic, and append paths may affect the same value and
/// therefore remain conflicts across static source files.
fn disjoint_deep_sets(prior: &crate::lang::ast::Var, incoming: &crate::lang::ast::Var) -> bool {
    let (Some(prior), Some(incoming)) = (&prior.deep_set, &incoming.deep_set) else {
        return false;
    };
    let (Some(prior), Some(incoming)) = (
        static_deep_path_parts(prior),
        static_deep_path_parts(incoming),
    ) else {
        return false;
    };

    prior
        .iter()
        .zip(&incoming)
        .any(|(prior, incoming)| prior != incoming)
}

/// Normalize the merged validation view used by a seeded Shadow extension.
///
/// `merge_namespace(Shadow)` appends declarations so the bytecode emitter can
/// execute the new source in order. Validation, however, must not keep an old
/// ordinary binding visible after a later REPL/eval declaration replaces it.
/// Doing so turns old and new function signatures into apparent overloads even
/// though extension emission removes the old callable surface.
///
/// Open literal-union declarations and deep-set updates are additive rather
/// than ordinary shadows. Keep compatible prior members for those forms:
///
/// * literal unions retain prior declarations when either side is open, so
///   member accumulation and open/closed mismatch diagnostics still work;
/// * deep sets retain the root declaration and prior disjoint static updates,
///   while a conflicting prior update is superseded by the new one.
pub fn normalize_shadow_bindings(
    validation_program: &mut crate::lang::ast::Program,
    extension_program: &crate::lang::ast::Program,
) {
    use crate::lang::ast::{Value, Var};

    for (ns_path, extension_namespace) in &extension_program.namespaces {
        let Some(validation_namespace) = validation_program.namespaces.get_mut(ns_path) else {
            continue;
        };

        let mut extension_by_name: AHashMap<String, Vec<(Var, Value)>> = AHashMap::new();
        for (var, value) in &extension_namespace.scope.vars {
            if var.sym.name() == "ns" {
                continue;
            }
            extension_by_name
                .entry(var.sym.name().to_string())
                .or_default()
                .push((var.clone(), value.clone()));
        }

        for (name, extension_entries) in extension_by_name {
            let extension_vars: AHashSet<Var> = extension_entries
                .iter()
                .map(|(var, _)| var.clone())
                .collect();
            let all_deep_sets = extension_entries
                .iter()
                .all(|(var, _)| var.deep_set.is_some());
            let all_literal_types = extension_entries.iter().all(|(_, value)| {
                let Value::TypeDef(type_def) = value else {
                    return false;
                };
                type_def.type_alias.as_ref().is_some_and(literal_type_expr)
            });

            validation_namespace.scope.vars.retain(|var, value| {
                if var.sym.name() != name || extension_vars.contains(var) {
                    return true;
                }

                if all_deep_sets {
                    // A root declaration establishes the value that later
                    // deep updates mutate. Prior deep updates survive only
                    // when every new path is statically disjoint.
                    return var.deep_set.is_none()
                        || extension_entries
                            .iter()
                            .all(|(incoming, _)| disjoint_deep_sets(var, incoming));
                }

                if all_literal_types {
                    return extension_entries
                        .iter()
                        .any(|(_, incoming)| is_open_literal_union_pair(value, incoming));
                }

                // Ordinary binding: the extension's declarations are the
                // complete newest surface for this name.
                false
            });
        }
    }
}

/// Merge `incoming` into `target[ns_path]`, combining members instead of
/// replacing the whole namespace. All namespace-merge sites (unit files,
/// cross-unit, target file, eval/REPL code) go through here so duplicate
/// handling is uniform and deterministic.
///
/// Bindings whose name already exists in the target namespace:
///   * same source file — skipped (idempotent re-merge; e.g. `hot run x.hot`
///     parses `x.hot` both as part of its source unit and as the target file)
///   * different file + `Strict` — error naming both files
///   * different file + `Shadow` — appended, newest wins
pub fn merge_namespace(
    target: &mut IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
    ns_path: crate::lang::ast::NsPath,
    incoming: crate::lang::ast::Namespace,
    policy: NsMergePolicy,
) -> Result<(), String> {
    use indexmap::map::Entry;

    let slot = match target.entry(ns_path) {
        Entry::Vacant(slot) => {
            slot.insert(incoming);
            return Ok(());
        }
        Entry::Occupied(slot) => slot,
    };
    let ns_path = slot.key().clone();
    let existing = slot.into_mut();
    let incoming_source_file = incoming.source_file.clone();

    // Namespace metadata: adopt when the existing declaration has none;
    // conflicting redeclaration is an error for static code, newest-wins
    // for eval input.
    if let Some(incoming_meta) = incoming.meta {
        match &existing.meta {
            None => existing.meta = Some(incoming_meta),
            Some(existing_meta) if *existing_meta != incoming_meta => {
                if policy == NsMergePolicy::Strict {
                    return Err(format!(
                        "Namespace '{}' declares conflicting metadata in {} and {}",
                        ns_path,
                        existing
                            .source_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .as_deref()
                            .unwrap_or("<unknown file>"),
                        incoming_source_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .as_deref()
                            .unwrap_or("<unknown file>"),
                    ));
                }
                existing.meta = Some(incoming_meta);
            }
            Some(_) => {}
        }
    }

    // Aliases are namespace-wide rather than lexical. Retargeting one while
    // shadowing would retroactively change previously compiled declarations,
    // so conflicting targets are errors under both policies.
    for (alias, alias_target) in incoming.aliases {
        match existing.aliases.get(&alias) {
            Some(current) if *current != alias_target => {
                return Err(format!(
                    "Namespace '{}' declares alias '{}' with conflicting targets '{}' and '{}'",
                    ns_path, alias, current, alias_target
                ));
            }
            Some(_) => {}
            None => {
                existing.aliases.insert(alias, alias_target);
            }
        }
    }

    // Bindings: compare by symbol name (Var identity includes source spans,
    // so map-key equality can't detect same-name redeclarations).
    for (var, value) in incoming.scope.vars {
        let name = var.sym.name();
        let existing_same_name: Vec<_> = existing
            .scope
            .vars
            .iter()
            .rev()
            .filter(|(v, _)| v.sym.name() == name)
            .map(|(var, value)| (var.clone(), value.clone()))
            .collect();

        // Every file's `::path ns` declaration parses to a `ns` binding, so a
        // namespace assembled from several files legitimately sees one per
        // file — keep the first and skip the rest.
        if name == "ns" && !existing_same_name.is_empty() {
            continue;
        }

        if existing_same_name.is_empty() {
            existing.scope.vars.insert(var, value);
        } else {
            let same_file = existing_same_name.iter().any(|(prior, _)| {
                let prior_file = binding_file(prior, &existing.source_file);
                let incoming_file = binding_file(&var, &incoming_source_file);
                match (prior_file, incoming_file) {
                    (Some(prior), Some(incoming)) => same_source_file(&prior, &incoming),
                    _ => false,
                }
            });
            if same_file {
                // Same file merged twice — keep the existing binding.
                continue;
            }

            match policy {
                NsMergePolicy::Strict => {
                    let legal_open_extension = existing_same_name
                        .iter()
                        .all(|(_, prior_value)| is_open_literal_union_pair(prior_value, &value));
                    let legal_deep_set = existing_same_name
                        .iter()
                        .all(|(prior, _)| disjoint_deep_sets(prior, &var));
                    if legal_open_extension || legal_deep_set {
                        existing.scope.vars.insert(var, value);
                        continue;
                    }

                    let (prior, _) = &existing_same_name[0];
                    let prior_file = binding_file(prior, &existing.source_file);
                    let incoming_file = binding_file(&var, &incoming_source_file);
                    return Err(format!(
                        "Duplicate definition of '{}' in namespace '{}': defined in {} and {}",
                        name,
                        ns_path,
                        file_label(&prior_file),
                        file_label(&incoming_file),
                    ));
                }
                NsMergePolicy::Shadow => {
                    existing.scope.vars.insert(var, value);
                }
            }
        }
    }

    Ok(())
}

/// Merge a complete static-source program using the canonical member-level
/// policy. This is public so sibling crates such as `hot_lsp` cannot drift
/// back to whole-namespace replacement.
pub fn merge_program_namespaces_strict(
    target: &mut IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
    incoming: IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
) -> Result<(), String> {
    for (ns_path, namespace) in incoming {
        merge_namespace(target, ns_path, namespace, NsMergePolicy::Strict)?;
    }
    Ok(())
}

#[cfg(test)]
mod ns_merge_tests {
    use super::*;
    use crate::lang::ast::{Namespace, NsPath};

    /// Parse `source` as if it came from `file` and return its namespaces.
    fn parse(source: &str, file: &str) -> IndexMap<NsPath, Namespace> {
        crate::lang::parser::parse_hot_file(source, file)
            .expect("test source should parse")
            .namespaces
    }

    fn parse_program(source: &str, file: &str) -> crate::lang::ast::Program {
        crate::lang::parser::parse_hot_file(source, file).expect("test source should parse")
    }

    fn merge_all(
        target: &mut IndexMap<NsPath, Namespace>,
        namespaces: IndexMap<NsPath, Namespace>,
        policy: NsMergePolicy,
    ) -> Result<(), String> {
        for (ns_path, namespace) in namespaces {
            merge_namespace(target, ns_path, namespace, policy)?;
        }
        Ok(())
    }

    fn ns<'a>(target: &'a IndexMap<NsPath, Namespace>, path: &str) -> &'a Namespace {
        target
            .iter()
            .find(|(p, _)| p.to_string() == path)
            .map(|(_, ns)| ns)
            .unwrap_or_else(|| panic!("namespace {} missing", path))
    }

    fn var_names(namespace: &Namespace) -> Vec<String> {
        namespace
            .scope
            .vars
            .keys()
            .map(|v| v.sym.name().to_string())
            .collect()
    }

    #[test]
    fn two_files_same_namespace_members_combine() {
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nalpha fn (): Str { \"A\" }\n", "a.hot"),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nbeta fn (): Str { \"B\" }\n", "b.hot"),
            NsMergePolicy::Strict,
        )
        .unwrap();

        let merged = ns(&target, "::myapp");
        let names = var_names(merged);
        assert!(
            names.contains(&"alpha".to_string()),
            "alpha kept: {names:?}"
        );
        assert!(names.contains(&"beta".to_string()), "beta added: {names:?}");
    }

    #[test]
    fn duplicate_definition_across_files_is_error_in_both_orders() {
        let a = parse("::myapp ns\n\nthing fn (): Str { \"A\" }\n", "a.hot");
        let b = parse("::myapp ns\n\nthing fn (): Str { \"B\" }\n", "b.hot");

        // Strict merging must fail regardless of merge order — this is what
        // makes the pipeline result independent of cache-hit/miss ordering.
        for (first, second) in [(a.clone(), b.clone()), (b, a)] {
            let mut target = IndexMap::new();
            merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();
            let err = merge_all(&mut target, second, NsMergePolicy::Strict)
                .expect_err("duplicate definition must error");
            assert!(err.contains("Duplicate definition of 'thing'"), "{err}");
            assert!(err.contains("a.hot") && err.contains("b.hot"), "{err}");
        }
    }

    #[test]
    fn same_file_merged_twice_is_idempotent() {
        // `hot run x.hot` parses the target file both as part of its source
        // unit and as the target file — the second merge must be a no-op.
        let mut target = IndexMap::new();
        let first = parse("::myapp ns\n\nalpha fn (): Str { \"A\" }\n", "x.hot");
        merge_all(&mut target, first.clone(), NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();

        let names = var_names(ns(&target, "::myapp"));
        let alpha_count = names.iter().filter(|n| n.as_str() == "alpha").count();
        assert_eq!(alpha_count, 1, "no duplicated binding: {names:?}");
    }

    #[test]
    fn same_file_under_different_paths_is_idempotent() {
        // Discovery yields canonical paths, while `hot run hot/src/x.hot`
        // parses the target under the path as typed. Both name one file.
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let file = src.join("x.hot");
        let source = "::myapp ns\n\nalpha fn (): Str { \"A\" }\n";
        std::fs::write(&file, source).unwrap();

        let canonical = std::fs::canonicalize(&file).unwrap();
        let as_typed = dir.path().join("src").join("..").join("src").join("x.hot");
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse(source, &canonical.to_string_lossy()),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse(source, &as_typed.to_string_lossy()),
            NsMergePolicy::Strict,
        )
        .unwrap();

        let names = var_names(ns(&target, "::myapp"));
        let alpha_count = names.iter().filter(|n| n.as_str() == "alpha").count();
        assert_eq!(alpha_count, 1, "no duplicated binding: {names:?}");
    }

    #[test]
    fn shadow_policy_appends_newest_binding_and_keeps_others() {
        // Eval/REPL: declaring into an existing namespace must preserve its
        // other members (previously the whole namespace was replaced) and a
        // re-declared name must shadow the old binding (appended last).
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse(
                "::myapp ns\n\nalpha fn (): Str { \"A\" }\nbeta fn (): Str { \"B\" }\n",
                "src.hot",
            ),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nbeta fn (): Str { \"B2\" }\n", "<eval>"),
            NsMergePolicy::Shadow,
        )
        .unwrap();

        let names = var_names(ns(&target, "::myapp"));
        assert!(
            names.contains(&"alpha".to_string()),
            "alpha kept: {names:?}"
        );
        assert_eq!(
            names.last().map(String::as_str),
            Some("beta"),
            "newest beta appended last so it wins resolution: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "beta").count(),
            2,
            "shadowed binding coexists like REPL redefinition: {names:?}"
        );
    }

    #[test]
    fn shadow_validation_keeps_only_newest_ordinary_binding() {
        let mut base = parse_program(
            "::myapp ns\n\nvalue fn (): Int { 1 }\nother 2\n",
            "base.hot",
        );
        let extension = parse_program(
            "::myapp ns\n\nvalue fn (input: Int): Int { input }\n",
            "<repl:1>",
        );
        merge_all(
            &mut base.namespaces,
            extension.namespaces.clone(),
            NsMergePolicy::Shadow,
        )
        .unwrap();

        normalize_shadow_bindings(&mut base, &extension);

        let merged = ns(&base.namespaces, "::myapp");
        assert_eq!(
            merged
                .scope
                .vars
                .keys()
                .filter(|var| var.sym.name() == "value")
                .count(),
            1,
            "the old callable signature must not remain visible to validation"
        );
        assert!(
            merged
                .scope
                .vars
                .keys()
                .any(|var| var.sym.name() == "other"),
            "unrelated bindings must survive"
        );
    }

    #[test]
    fn shadow_validation_preserves_open_unions_and_disjoint_deep_sets() {
        let mut base = parse_program(
            r#"
                ::myapp ns
                Fruit type open "apple" | "banana"
                config.db.uri "postgres://localhost"
            "#,
            "base.hot",
        );
        let extension = parse_program(
            r#"
                ::myapp ns
                Fruit type open | "kiwi"
                config.db.schema "public"
            "#,
            "<repl:1>",
        );
        merge_all(
            &mut base.namespaces,
            extension.namespaces.clone(),
            NsMergePolicy::Shadow,
        )
        .unwrap();

        normalize_shadow_bindings(&mut base, &extension);

        let merged = ns(&base.namespaces, "::myapp");
        assert_eq!(
            merged
                .scope
                .vars
                .keys()
                .filter(|var| var.sym.name() == "Fruit")
                .count(),
            2,
            "open literal-union members must accumulate"
        );
        assert_eq!(
            merged
                .scope
                .vars
                .keys()
                .filter(|var| var.sym.name() == "config")
                .count(),
            2,
            "disjoint deep-set updates must accumulate"
        );
    }

    #[test]
    fn project_declaring_hot_std_namespace_collides_deterministically() {
        // Simulates a project src file declaring a hot-std namespace path.
        // Adding a NEW name merges member-wise; redefining an EXISTING name
        // errors — in both merge orders.
        let std_ns = parse(
            "::hot::str ns\n\ntrim fn (s: Str): Str { s }\n",
            "/usr/local/share/hot/pkg/hot-std/src/hot/str.hot",
        );
        let addition = parse(
            "::hot::str ns\n\nshout fn (s: Str): Str { s }\n",
            "src/ext.hot",
        );
        let redefinition = parse(
            "::hot::str ns\n\ntrim fn (s: Str): Str { s }\n",
            "src/ext.hot",
        );

        let mut target = IndexMap::new();
        merge_all(&mut target, std_ns.clone(), NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, addition, NsMergePolicy::Strict).unwrap();
        let names = var_names(ns(&target, "::hot::str"));
        assert!(
            names.contains(&"trim".to_string()),
            "std member kept: {names:?}"
        );
        assert!(
            names.contains(&"shout".to_string()),
            "addition merged: {names:?}"
        );

        for (first, second) in [
            (std_ns.clone(), redefinition.clone()),
            (redefinition, std_ns),
        ] {
            let mut target = IndexMap::new();
            merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();
            let err = merge_all(&mut target, second, NsMergePolicy::Strict)
                .expect_err("redefining a std binding must error");
            assert!(err.contains("Duplicate definition of 'trim'"), "{err}");
        }
    }

    #[test]
    fn open_literal_union_extensions_across_files_combine() {
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse(
                "::myapp ns\n\nFruit type open \"apple\" | \"banana\"\n",
                "a.hot",
            ),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nFruit type open | \"kiwi\"\n", "b.hot"),
            NsMergePolicy::Strict,
        )
        .unwrap();

        let fruit_count = ns(&target, "::myapp")
            .scope
            .vars
            .keys()
            .filter(|var| var.sym.name() == "Fruit")
            .count();
        assert_eq!(fruit_count, 2, "both open-union declarations survive");
    }

    #[test]
    fn disjoint_deep_sets_combine_but_same_path_conflicts() {
        let uri = parse(
            "::myapp ns\n\nconfig.db.uri \"postgres://localhost\"\n",
            "a.hot",
        );
        let schema = parse("::myapp ns\n\nconfig.db.schema \"public\"\n", "b.hot");
        let duplicate_uri = parse(
            "::myapp ns\n\nconfig.db.uri \"postgres://other\"\n",
            "c.hot",
        );

        let mut target = IndexMap::new();
        merge_all(&mut target, uri, NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, schema, NsMergePolicy::Strict).unwrap();
        let config_count = ns(&target, "::myapp")
            .scope
            .vars
            .keys()
            .filter(|var| var.sym.name() == "config")
            .count();
        assert_eq!(config_count, 2, "disjoint updates are both retained");

        let err = merge_all(&mut target, duplicate_uri, NsMergePolicy::Strict)
            .expect_err("the same deep-set path must conflict");
        assert!(err.contains("Duplicate definition of 'config'"), "{err}");
    }

    #[test]
    fn alias_conflicts_error_under_both_policies() {
        let a = parse("::myapp ns\n::h ::hot::http\n", "a.hot");
        let b = parse("::myapp ns\n::h ::hot::html\n", "b.hot");

        let mut target = IndexMap::new();
        merge_all(&mut target, a.clone(), NsMergePolicy::Strict).unwrap();
        let err = merge_all(&mut target, b.clone(), NsMergePolicy::Strict)
            .expect_err("conflicting alias must error");
        assert!(err.contains("alias"), "{err}");

        let mut target = IndexMap::new();
        merge_all(&mut target, a, NsMergePolicy::Strict).unwrap();
        let err = merge_all(&mut target, b, NsMergePolicy::Shadow)
            .expect_err("shadowing cannot retarget namespace-wide aliases");
        assert!(err.contains("alias"), "{err}");
    }
}
