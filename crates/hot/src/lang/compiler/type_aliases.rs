//! Top-level aliases of types, bound before any namespace runs.
//!
//! An alias is a binding like any other (`Thing ::o::a/Thing`), so it would
//! be stored when its namespace's block runs. A function that names it can
//! be called earlier than that, by a value in a namespace that runs first.
//! [`initialization_order`](super::namespace_order::initialization_order)
//! can't always prevent this: it only waits for values, and namespaces that
//! read each other's values can't all run first.
//!
//! A type needs nothing to run: pass 1 binds every type before any block
//! does. So a name for a type, or for one of its variants, needs nothing
//! either, and is bound right after the types. Load order then has no say in
//! whether it is there.

use super::Compiler;
use crate::lang::ast::{DeepPath, Namespace, NsPath, NsRef, Program, Ref, Value};
use crate::lang::bytecode::RegisterId;

impl Compiler {
    /// Bind each top-level alias of a type or variant in `program`.
    pub(super) fn bind_type_aliases(&mut self, program: &Program) -> Result<(), String> {
        for (ns_path, namespace) in &program.namespaces {
            let ns = ns_path.to_string();
            self.current_namespace = ns.clone();
            self.current_aliases = namespace.aliases.clone();
            if let Some(file_path) = self.namespace_to_file.get(&ns) {
                self.current_file = Some(file_path.clone());
            }

            let aliases: Vec<_> = namespace
                .scope
                .vars
                .iter()
                .filter(|(var, _)| var.deep_set.is_none())
                .filter_map(|(var, value)| {
                    let target = self.aliased_type(ns_path, namespace, value)?;
                    Some((var, target))
                })
                .collect();

            // A target is a full name, which this namespace's aliases must
            // not rewrite.
            self.current_aliases = Default::default();
            for (var, target) in aliases {
                self.set_source_location_from_var(var);
                let register = self.load_type_alias_target(&target)?;
                self.emit_store_global(&ns, var.sym.name(), register);
            }
        }
        Ok(())
    }

    /// The type or variant that `value` names, with imports followed, when
    /// `value` is an alias of one.
    fn aliased_type(
        &self,
        ns_path: &NsPath,
        namespace: &Namespace,
        value: &Value,
    ) -> Option<NsRef> {
        // The cap only bounds local aliases that form a cycle.
        const MAX_LOCAL_ALIASES: usize = 16;
        let mut value = value;
        for _ in 0..MAX_LOCAL_ALIASES {
            let var_ref = match value {
                Value::Ref(Ref::Ns(ns_ref)) => {
                    ns_ref.function_name.as_ref()?;
                    return self.type_named_by(self.resolve_ns_ref_alias(ns_ref));
                }
                Value::Ref(Ref::Var(var_ref)) => var_ref,
                _ => return None,
            };
            let name = var_ref.var.sym.name();
            let local = namespace
                .scope
                .vars
                .iter()
                .rev()
                .find(|(var, _)| var.sym.name() == name)
                .map(|(_, value)| value);
            let written = match &var_ref.var.deep_path {
                None => match local {
                    Some(next @ Value::Ref(_)) => {
                        value = next;
                        continue;
                    }
                    _ => name.to_string(),
                },
                Some(DeepPath::Key(variant)) => format!("{name}.{variant}"),
                Some(_) => return None,
            };
            return self.type_named_by(self.resolve_ns_ref_alias(&NsRef {
                ns: ns_path.clone(),
                src: None,
                function_name: Some(written),
            }));
        }
        None
    }

    /// `target`, when it names a declared type or one of its variants.
    fn type_named_by(&self, target: NsRef) -> Option<NsRef> {
        let name = target.function_name.as_deref()?;
        let base = name.split('.').next().unwrap_or(name);
        self.constraint_type_defs
            .contains_key(&format!("{}/{base}", target.ns))
            .then_some(target)
    }

    /// Load the value an alias of `target` holds, as the alias's own binding
    /// stores it.
    fn load_type_alias_target(&mut self, target: &NsRef) -> Result<RegisterId, String> {
        let name = target.function_name.as_deref().unwrap_or_default();
        let qualified = format!("{}/{name}", target.ns);
        if self.is_known_function(&qualified) && !self.unit_variants.contains(&qualified) {
            Ok(self.load_function_alias(&qualified))
        } else {
            self.compile_reference(&Ref::Ns(target.clone()))
        }
    }
}
