// Hidden Rust-backed bindings for `::hot::internal::skill/*`.
//
// Mirrors `internal_mcp.rs` but for `meta {skill: {...}}` annotations.
// The compiler walks every Hot fn def, harvests the skill meta as a
// raw `Val::Map`, and parks it in an execution-scoped registry keyed by
// fully qualified function name. `::ai::skill/from-fn` consults this
// registry to reconstruct a `Skill` record at runtime without
// re-parsing source.
//
// Like `internal_mcp`, this namespace is intentionally undocumented
// (no-doc) and is meant to be called only from the `::ai::skill`
// Hot module.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::function_ref::extract_function_ref;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;

pub use hot_lang::lang::registry::skill::*;

fn extract_fn_name(val: &Val) -> Option<String> {
    if let Some(fr) = extract_function_ref(val) {
        return Some(fr.name.clone());
    }
    if let Val::Str(s) = val {
        return Some((**s).to_owned());
    }
    if let Val::Map(m) = val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        return extract_fn_name(inner);
    }
    None
}

/// `::hot::internal::skill/meta-from-fn(f: Fn): Map`
///
/// Returns the raw `meta {skill: {...}}` map associated with `f`,
/// merged with `{name: <fq-fn-name>}` for convenience. Returns an
/// `Err` when `f` has no `meta {skill: ...}` annotation.
pub fn meta_from_fn(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::internal::skill/meta-from-fn", args, 1);
    let name = match extract_fn_name(&args[0]) {
        Some(n) => n,
        None => {
            return HotResult::Err(Val::from(
                "::hot::internal::skill/meta-from-fn: argument must be a function reference \
                 or a fully qualified function name string"
                    .to_string(),
            ));
        }
    };
    with_registry(|g| match g.entries.get(&name) {
        Some(entry) => {
            // Always include the canonical Hot fn name so callers can
            // round-trip a Skill back to its source fn even when no
            // `name:` was provided in the annotation.
            let mut merged: IndexMap<Val, Val> = match &entry.skill_meta {
                Val::Map(m) => (**m).clone(),
                _ => IndexMap::new(),
            };
            merged
                .entry(Val::from("fn-name"))
                .or_insert_with(|| Val::from(entry.name.as_str()));
            HotResult::Ok(Val::Map(Box::new(merged)))
        }
        None => HotResult::Err(Val::from(format!(
            "::hot::internal::skill/meta-from-fn: no skill registered for '{}' (function may \
             have no `meta {{skill: ...}}` annotation)",
            name
        ))),
    })
}

/// `::hot::internal::skill/all-skill-metas(): Vec<Map>`
///
/// Returns every registered skill meta as a list of maps. Used by
/// `::ai::skill/all` and any admin tooling that needs to enumerate
/// known skills.
pub fn all_skill_metas(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(
            "::hot::internal::skill/all-skill-metas takes no arguments".to_string(),
        ));
    }
    with_registry(|g| {
        let out: Vec<Val> = g
            .entries
            .values()
            .map(|e| {
                let mut merged: IndexMap<Val, Val> = match &e.skill_meta {
                    Val::Map(m) => (**m).clone(),
                    _ => IndexMap::new(),
                };
                merged
                    .entry(Val::from("fn-name"))
                    .or_insert_with(|| Val::from(e.name.as_str()));
                Val::Map(Box::new(merged))
            })
            .collect();
        HotResult::Ok(Val::Vec(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::runtime::function_ref::function_ref;
    use parking_lot::Mutex;
    use std::sync::Arc;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_entry(name: &str, desc: &str) -> SkillSpecEntry {
        let mut m = IndexMap::new();
        m.insert(Val::from("description"), Val::from(desc));
        m.insert(Val::from("when"), Val::Vec(vec![Val::from("trigger")]));
        SkillSpecEntry {
            name: name.to_string(),
            skill_meta: Val::Map(Box::new(m)),
        }
    }

    #[test]
    fn test_meta_from_fn_lookup() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/tone", "How we talk"));
        let v = meta_from_fn(&[function_ref("::demo/tone".to_string())]);
        match v {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(
                    m.get(&Val::from("description")),
                    Some(&Val::from("How we talk"))
                );
                assert_eq!(
                    m.get(&Val::from("fn-name")),
                    Some(&Val::from("::demo/tone"))
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn test_meta_from_fn_unknown() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        match meta_from_fn(&[function_ref("::nope/nope".to_string())]) {
            HotResult::Err(_) => {}
            other => panic!("expected err, got: {:?}", other),
        }
    }

    #[test]
    fn test_all_skill_metas() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/a", "A"));
        put_entry(fixture_entry("::demo/b", "B"));
        match all_skill_metas(&[]) {
            HotResult::Ok(Val::Vec(v)) => assert_eq!(v.len(), 2),
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn scoped_mutations_restore_outer_registry() {
        let mut outer = SkillSpecRegistry::default();
        outer.entries.insert(
            "::outer/skill".into(),
            fixture_entry("::outer/skill", "outer"),
        );
        let outer = Arc::new(outer);
        let _outer = ThreadRegistryGuard::install(outer.clone());
        {
            let _inner = ThreadRegistryGuard::install(Arc::new(SkillSpecRegistry::default()));
            assert!(matches!(
                meta_from_fn(&[Val::from("::outer/skill")]),
                HotResult::Err(_)
            ));
            put_entry(fixture_entry("::inner/one", "one"));
            assert!(get_registry().entries.contains_key("::inner/one"));
            let mut additional = SkillSpecRegistry::default();
            additional
                .entries
                .insert("::inner/two".into(), fixture_entry("::inner/two", "two"));
            set_registry(additional);
            assert_eq!(get_registry().entries.len(), 2);
            replace_registry((*outer).clone());
            assert!(!get_registry().entries.contains_key("::inner/one"));
            clear_registry();
            assert!(get_registry().entries.is_empty());
        }
        assert_eq!(get_registry().entries.len(), 1);
        assert!(get_registry().entries.contains_key("::outer/skill"));
    }
}
