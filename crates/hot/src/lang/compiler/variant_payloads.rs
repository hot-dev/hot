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
use crate::lang::ast::{NamespaceAliases, NsPath, Program, TypeDef, Value};

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

        let mut plain = AHashSet::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns = prefixed(&ns_path.to_string());
            let aliases = &namespace.aliases;
            // A payload type known not to be a record: every member of
            // `A | B` or `A?` is a built-in or a declared alias or enum. An
            // unresolved name keeps the variant, as before.
            let is_plain = |payload: &str| {
                payload.trim_end_matches('?').split('|').all(|member| {
                    let head = member.split('<').next().unwrap_or(member).trim();
                    let head = head.trim_end_matches('?');
                    BUILTIN_TYPES.contains(&head)
                        || self
                            .qualify_type(&ns, aliases, &records, head)
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
                            self.qualify_type(&ns, aliases, &records, enum_path)
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
    /// writes it: `::alias/Type`, a local type, or a core type.
    fn qualify_type(
        &self,
        ns: &str,
        aliases: &NamespaceAliases,
        records: &AHashMap<String, bool>,
        name: &str,
    ) -> Option<String> {
        if name.starts_with("::") {
            let (path, type_name) = name.rsplit_once('/')?;
            let path = NsPath::from_string(path);
            let path = aliases.get(&path).unwrap_or(&path);
            return Some(format!("{}/{type_name}", prefixed(&path.to_string())));
        }
        let local = format!("{ns}/{name}");
        if records.contains_key(&local) {
            return Some(local);
        }
        let core = self.core_variables.get(name)?;
        Some(format!("{}/{name}", prefixed(&core.namespace_path)))
    }
}
