//! The order in which namespaces run their top-level bindings.
//!
//! Each namespace's bindings run as one block, namespace by namespace. A
//! binding that reads another namespace's value (`s4 ::mp::text/s4`), or
//! calls a function that does, needs that namespace to have run first.
//! Otherwise the value is missing when it is read, and the VM's fallback
//! evaluates the initializer a second time. Source order alone does not
//! guarantee this: files load alphabetically, so an importer can come first.
//!
//! [`initialization_order`] keeps source order wherever it can and moves a
//! namespace only after the namespaces whose values it reads. Namespaces
//! that read each other's values keep their source order among themselves.

use super::call_graph::collect_callees_from_value;
use crate::lang::ast::{Namespace, NsPath, Program, Ref, Value};
use ahash::{AHashMap, AHashSet};

/// What a top-level binding is, for ordering purposes.
enum Binding<'a> {
    /// A function: calling it runs its body.
    Function(&'a Namespace, Vec<&'a Value>),
    /// A value, computed when its namespace runs.
    Value,
    /// A qualified import (`x ::other/y`): the binding stores what it names.
    Import(String),
    /// A type, or the `ns` declaration: nothing to wait for.
    Other,
}

/// The namespaces of `program` in the order their bindings should run.
pub(super) fn initialization_order(program: &Program) -> Vec<(&NsPath, &Namespace)> {
    let mut bindings: AHashMap<String, Binding> = AHashMap::new();
    for (ns_path, namespace) in &program.namespaces {
        let ns = ns_path.to_string();
        for (var, value) in &namespace.scope.vars {
            let binding = match value {
                Value::Fn(defs) => {
                    Binding::Function(namespace, defs.iter().map(|def| &def.body).collect())
                }
                Value::TypeDef(_) | Value::TypeImplementation(_) => Binding::Other,
                _ if var.sym.name() == "ns" => Binding::Other,
                Value::Ref(Ref::Ns(ns_ref)) => match &ns_ref.function_name {
                    Some(name) => {
                        let home = namespace
                            .aliases
                            .get(&ns_ref.ns)
                            .unwrap_or(&ns_ref.ns)
                            .to_string();
                        Binding::Import(format!("{home}/{name}"))
                    }
                    None => Binding::Other,
                },
                _ => Binding::Value,
            };
            // A later binding of the same name is the one others see.
            bindings.insert(format!("{ns}/{}", var.sym.name()), binding);
        }
    }

    let entries: Vec<_> = program.namespaces.iter().collect();
    let index: AHashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, (ns_path, _))| (ns_path.to_string(), i))
        .collect();
    let needs: Vec<AHashSet<usize>> = entries
        .iter()
        .enumerate()
        .map(|(own, (ns_path, namespace))| {
            values_read(&ns_path.to_string(), namespace, &bindings)
                .iter()
                .filter_map(|fqn| index.get(home_namespace(fqn)).copied())
                .filter(|&i| i != own)
                .collect()
        })
        .collect();

    // Take the earliest namespace whose needs have all run; when every
    // namespace left waits on another, take the earliest of them.
    let mut placed = vec![false; entries.len()];
    let mut order = Vec::with_capacity(entries.len());
    while order.len() < entries.len() {
        let next = (0..entries.len())
            .find(|&i| !placed[i] && needs[i].iter().all(|&dep| placed[dep]))
            .or_else(|| (0..entries.len()).find(|&i| !placed[i]))
            .expect("an unplaced namespace remains");
        placed[next] = true;
        order.push(entries[next]);
    }
    order
}

/// The other namespaces' values that `namespace`'s top-level bindings read
/// while they run: directly, through imports, or through functions they call.
fn values_read(
    ns: &str,
    namespace: &Namespace,
    bindings: &AHashMap<String, Binding>,
) -> Vec<String> {
    let mut pending: Vec<String> = Vec::new();
    for (var, value) in &namespace.scope.vars {
        let fqn = format!("{ns}/{}", var.sym.name());
        match bindings.get(&fqn) {
            Some(Binding::Value) => {
                let mut refs = AHashSet::new();
                collect_callees_from_value(value, ns, &namespace.aliases, &mut refs);
                pending.extend(refs);
            }
            Some(Binding::Import(target)) => pending.push(target.clone()),
            _ => {}
        }
    }

    let mut seen: AHashSet<String> = AHashSet::new();
    let mut read = Vec::new();
    while let Some(fqn) = pending.pop() {
        if !seen.insert(fqn.clone()) {
            continue;
        }
        match bindings.get(&fqn) {
            Some(Binding::Function(home, bodies)) => {
                let home_ns = home_namespace(&fqn);
                let mut refs = AHashSet::new();
                for body in bodies {
                    collect_callees_from_value(body, home_ns, &home.aliases, &mut refs);
                }
                pending.extend(refs);
            }
            // An imported value is stored when the importing namespace runs.
            Some(Binding::Import(target)) => {
                if names_value(target, bindings) {
                    read.push(fqn.clone());
                }
                pending.push(target.clone());
            }
            Some(Binding::Value) => read.push(fqn),
            Some(Binding::Other) | None => {}
        }
    }
    read
}

/// Whether `fqn`, followed through imports, is a value rather than a
/// function or type.
fn names_value(fqn: &str, bindings: &AHashMap<String, Binding>) -> bool {
    let mut seen = AHashSet::new();
    let mut fqn = fqn;
    while seen.insert(fqn) {
        match bindings.get(fqn) {
            Some(Binding::Value) => return true,
            Some(Binding::Import(target)) => fqn = target,
            _ => return false,
        }
    }
    false
}

fn home_namespace(fqn: &str) -> &str {
    fqn.rsplit_once('/').map_or(fqn, |(ns, _)| ns)
}
