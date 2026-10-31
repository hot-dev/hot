//! Enum variants whose payload is not a record, for binding a `match` arm's
//! payload.
//!
//! A variant value is `{$type: "::ns/Enum.Variant", $val: payload}`. Field
//! access reads through it to a record payload, so an arm on a record variant
//! keeps the variant: it reads as the payload and can still be passed on as
//! the enum. Any other payload (`Open(Vec)`, `NotOwed(Str)`) is unreachable
//! through the variant, so its arm binds the payload instead. This pass
//! records those variants before any bytecode is emitted, since an arm can
//! come before the enum it matches.

use ahash::{AHashMap, AHashSet};

use super::Compiler;
use crate::lang::ast::{NamespaceAliases, NsPath, Program, Ref, TypeDef, Value};

/// Built-in type names a payload type expression can start with.
const BUILTIN_TYPES: &[&str] = &[
    "Any", "Bool", "Byte", "Bytes", "Dec", "Fn", "Int", "Map", "Null", "Str", "Vec",
];

/// `ns` with the leading `::` that type tags carry.
fn prefixed(ns: &str) -> String {
    if ns.starts_with("::") {
        ns.to_string()
    } else {
        format!("::{ns}")
    }
}

impl Compiler {
    /// Record the program's enum variants that carry a payload other than a
    /// record, declared (`enum { Open(Vec) }`) or enrolled by an arrow
    /// (`Label -> Animal.Named`).
    pub(super) fn collect_plain_payload_variants(&mut self, program: &Program) {
        // Whether each declared type is a record, by qualified name.
        let mut records: AHashMap<String, bool> = self
            .constraint_type_defs
            .iter()
            .map(|(name, def)| (prefixed(name), def.fields.is_some()))
            .collect();
        for (ns_path, namespace) in &program.namespaces {
            let ns = prefixed(&ns_path.to_string());
            for (var, value) in &namespace.scope.vars {
                if let Value::TypeDef(def) = value {
                    records.insert(format!("{ns}/{}", var.sym.name()), def.fields.is_some());
                }
            }
        }

        // Var imports of types (`Status ::shop::types/Status`), by the
        // qualified name they bind, to the qualified name they import.
        let mut imports: AHashMap<String, String> = AHashMap::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns = prefixed(&ns_path.to_string());
            for (var, value) in &namespace.scope.vars {
                if let Value::Ref(Ref::Ns(ns_ref)) = value
                    && let Some(name) = &ns_ref.function_name
                {
                    let path = namespace.aliases.get(&ns_ref.ns).unwrap_or(&ns_ref.ns);
                    imports.insert(
                        format!("{ns}/{}", var.sym.name()),
                        format!("{}/{name}", prefixed(&path.to_string())),
                    );
                }
            }
        }

        let mut plain = AHashSet::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns = prefixed(&ns_path.to_string());
            let aliases = &namespace.aliases;
            // A payload a value of which can be something other than a
            // record: a top-level member of `A | B` (or `A?`, which can be
            // null) that is a built-in, such as `Vec<Int | Dec>`, or a
            // declared alias or enum. A payload that is always a record,
            // or names only unresolved types, keeps the variant.
            let is_plain = |payload: &str| {
                let payload = payload.trim();
                payload.ends_with('?')
                    || top_level_members(payload).iter().any(|member| {
                        let head = member.split('<').next().unwrap_or(member).trim();
                        let head = head.trim_end_matches('?');
                        member.trim().ends_with('?')
                            || BUILTIN_TYPES.contains(&head)
                            || self
                                .qualify_type(&ns, aliases, &records, &imports, head)
                                .and_then(|qualified| records.get(&qualified))
                                .is_some_and(|is_record| !is_record)
                    })
            };
            for (var, value) in &namespace.scope.vars {
                match value {
                    Value::TypeDef(TypeDef {
                        variants: Some(variants),
                        ..
                    }) => {
                        let enum_name = format!("{ns}/{}", var.sym.name());
                        for variant in variants {
                            if let Some(payload) = &variant.type_ref
                                && is_plain(payload)
                            {
                                plain.insert(format!("{enum_name}.{}", variant.name.name()));
                            }
                        }
                    }
                    Value::TypeImplementation(arrow) => {
                        let Some((enum_path, variant)) = arrow.target_type.rsplit_once('.') else {
                            continue;
                        };
                        if let Some(enum_name) =
                            self.qualify_type(&ns, aliases, &records, &imports, enum_path)
                            && is_plain(&arrow.source_type)
                        {
                            plain.insert(format!("{enum_name}.{variant}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.plain_payload_variants.extend(plain);
    }

    /// The qualified name of the declared type `name` as namespace `ns`
    /// writes it: `::alias/Type`, a local type, a var import of one, or a
    /// core type.
    fn qualify_type(
        &self,
        ns: &str,
        aliases: &NamespaceAliases,
        records: &AHashMap<String, bool>,
        imports: &AHashMap<String, String>,
        name: &str,
    ) -> Option<String> {
        let mut qualified = if name.starts_with("::") {
            let (path, type_name) = name.rsplit_once('/')?;
            let path = NsPath::from_string(path);
            let path = aliases.get(&path).unwrap_or(&path);
            format!("{}/{type_name}", prefixed(&path.to_string()))
        } else {
            format!("{ns}/{name}")
        };
        // Follow var imports; the bound guards against an import cycle.
        for _ in 0..16 {
            match imports.get(&qualified) {
                Some(target) if !records.contains_key(&qualified) => qualified = target.clone(),
                _ => break,
            }
        }
        if name.starts_with("::") || records.contains_key(&qualified) {
            return Some(qualified);
        }
        let core = self.core_variables.get(name)?;
        Some(format!("{}/{name}", prefixed(&core.namespace_path)))
    }
}

/// The members of a top-level union type annotation (`A | Vec<B | C>` is
/// `A` and `Vec<B | C>`), ignoring `|` inside generic arguments.
fn top_level_members(annotation: &str) -> Vec<&str> {
    let mut members = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in annotation.char_indices() {
        match c {
            '<' | '(' | '{' => depth += 1,
            '>' | ')' | '}' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                members.push(&annotation[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    members.push(&annotation[start..]);
    members
}

#[cfg(test)]
mod tests {
    use super::top_level_members;

    #[test]
    fn union_members_ignore_bars_inside_generics() {
        assert_eq!(top_level_members("Vec<Int | Dec>"), vec!["Vec<Int | Dec>"]);
        assert_eq!(
            top_level_members("Ask | Map<Str, A | B>"),
            vec!["Ask ", " Map<Str, A | B>"]
        );
        assert_eq!(top_level_members("Str"), vec!["Str"]);
    }
}
