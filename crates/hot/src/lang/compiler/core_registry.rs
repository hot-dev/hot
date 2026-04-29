// Core variable registry for Hot.
//
// This module provides a dedicated registry for core variables (functions, types, constants)
// that are marked with core metadata and should be auto-imported everywhere.
//

use crate::lang::ast::{Value, Var};
use crate::val::Val;
use ahash::AHashMap;
use indexmap::IndexMap;

/// Information about a core variable
#[derive(Debug, Clone)]
pub struct CoreVariableInfo {
    /// The actual value of the core variable (function, type constructor, constant, etc.)
    pub value: Value,
    /// The namespace where this core variable was defined
    pub namespace_path: String,
    /// The variable type (for categorization)
    pub variable_type: CoreVariableType,
}

/// Types of core variables
#[derive(Debug, Clone, PartialEq)]
pub enum CoreVariableType {
    /// A core function (e.g., `add`, `map`, `if`)
    Function,
    /// A type constructor (e.g., `Vec`, `Map`, `Str`)
    TypeConstructor,
    /// A constant value (e.g., `PI`, mathematical constants)
    Constant,
}

/// Registry for core variables that should be auto-imported everywhere
#[derive(Debug, Clone)]
pub struct CoreVariableRegistry {
    /// Map from variable name (symbol) to core variable info
    variables: IndexMap<String, CoreVariableInfo>,
    /// Fast lookup by namespace for incremental updates
    by_namespace: AHashMap<String, Vec<String>>,
}

impl CoreVariableRegistry {
    /// Create a new empty core variable registry
    pub fn new() -> Self {
        Self {
            variables: IndexMap::new(),
            by_namespace: AHashMap::new(),
        }
    }

    /// Add a core variable to the registry
    pub fn add_core_variable(
        &mut self,
        var_name: String,
        value: Value,
        namespace_path: String,
        variable_type: CoreVariableType,
    ) {
        let info = CoreVariableInfo {
            value,
            namespace_path: namespace_path.clone(),
            variable_type,
        };

        // Add to main registry
        self.variables.insert(var_name.clone(), info);

        // Add to namespace index for incremental updates
        self.by_namespace
            .entry(namespace_path)
            .or_default()
            .push(var_name);
    }

    /// Get a core variable by name
    pub fn get(&self, var_name: &str) -> Option<&CoreVariableInfo> {
        self.variables.get(var_name)
    }

    /// Check if a core variable exists
    pub fn contains(&self, var_name: &str) -> bool {
        self.variables.contains_key(var_name)
    }

    /// Get all core variables
    pub fn iter(&self) -> impl Iterator<Item = (&String, &CoreVariableInfo)> {
        self.variables.iter()
    }

    /// Get core variables by type
    pub fn get_by_type(&self, var_type: CoreVariableType) -> Vec<(&String, &CoreVariableInfo)> {
        self.variables
            .iter()
            .filter(|(_, info)| info.variable_type == var_type)
            .collect()
    }

    /// Get core variables from a specific namespace
    pub fn get_from_namespace(&self, namespace_path: &str) -> Vec<(&String, &CoreVariableInfo)> {
        if let Some(var_names) = self.by_namespace.get(namespace_path) {
            var_names
                .iter()
                .filter_map(|name| self.variables.get(name).map(|info| (name, info)))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Remove all core variables from a namespace (for incremental updates)
    pub fn remove_namespace(&mut self, namespace_path: &str) {
        if let Some(var_names) = self.by_namespace.remove(namespace_path) {
            for var_name in var_names {
                self.variables.shift_remove(&var_name);
            }
        }
    }

    /// Check if a variable has core metadata in any supported format
    /// Supports: meta ["core"], meta "core", meta {core: true}
    pub fn has_core_metadata(var: &Var) -> bool {
        if let Some(meta) = &var.meta {
            match &meta.val {
                // Check for meta ["core"] - vector containing "core" string
                Val::Vec(tags) => tags.iter().any(|tag| {
                    if let Val::Str(tag_str) = tag {
                        &**tag_str == "core"
                    } else {
                        false
                    }
                }),
                // Check for meta "core" - direct "core" string
                Val::Str(tag_str) => &**tag_str == "core",
                // Check for meta {core: true} - map with core key set to true
                Val::Map(map) => {
                    for (key, value) in map.iter() {
                        if let (Val::Str(key_str), Val::Bool(true)) = (key, value)
                            && &**key_str == "core"
                        {
                            return true;
                        }
                    }
                    false
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Determine the type of a core variable based on its value and metadata
    pub fn determine_variable_type(value: &Value) -> CoreVariableType {
        match value {
            Value::Fn(_) => {
                // Check if this is a type constructor by looking for "type" in metadata
                // For now, assume all functions are regular functions until richer
                // metadata is available.
                CoreVariableType::Function
            }
            Value::Val(_, _) => CoreVariableType::Constant,
            _ => CoreVariableType::Function, // Default to function
        }
    }

    /// Get the count of core variables
    pub fn len(&self) -> usize {
        self.variables.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }
}

impl Default for CoreVariableRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract core variables from an AST Program
/// This is used when loading from bytecode cache where the AST is available
/// but compilation wasn't performed (so extract_core_variables wasn't called)
pub fn extract_core_variables_from_ast(
    program: &crate::lang::ast::Program,
) -> CoreVariableRegistry {
    let mut registry = CoreVariableRegistry::new();

    tracing::trace!(
        "Extracting core variables from {} namespaces (from cached AST)",
        program.namespaces.len()
    );

    let mut core_var_count = 0;

    // Iterate through all namespaces and collect core variables
    for (ns_path, namespace) in &program.namespaces {
        let ns_path_str = ns_path.to_string();

        // Check all variables in the namespace
        for (var, value) in &namespace.scope.vars {
            let is_core = has_core_metadata_on_var(var) || is_core_type_def(var, value);
            if is_core {
                core_var_count += 1;
                tracing::trace!(
                    "Found core variable: {} in namespace {}",
                    var.sym.name(),
                    ns_path_str
                );

                // Determine the variable type
                let core_var_type = match value {
                    crate::lang::ast::Value::Fn(_) => CoreVariableType::Function,
                    crate::lang::ast::Value::TypeDef(_) => CoreVariableType::TypeConstructor,
                    _ => CoreVariableType::Constant,
                };

                registry.add_core_variable(
                    var.sym.name().to_string(),
                    value.clone(),
                    ns_path_str.clone(),
                    core_var_type,
                );
            }
        }
    }

    tracing::debug!(
        "Extracted {} core variables from cached AST",
        core_var_count
    );
    registry
}

/// Check if a Var has core metadata (helper for extract_core_variables_from_ast)
fn has_core_metadata_on_var(var: &crate::lang::ast::Var) -> bool {
    CoreVariableRegistry::has_core_metadata(var)
}

/// Check if a Value is a core TypeDef by checking the var's meta.
/// Meta is always on `Var`, never on `TypeDef`.
fn is_core_type_def(var: &crate::lang::ast::Var, value: &crate::lang::ast::Value) -> bool {
    if !matches!(value, crate::lang::ast::Value::TypeDef(_)) {
        return false;
    }
    if let Some(meta) = &var.meta {
        match &meta.val {
            crate::val::Val::Vec(tags) => tags.iter().any(|tag| {
                if let crate::val::Val::Str(tag_str) = tag {
                    tag_str.as_ref() == "core"
                } else {
                    false
                }
            }),
            crate::val::Val::Str(tag_str) => tag_str.as_ref() == "core",
            crate::val::Val::Map(map) => {
                for (key, value) in map.iter() {
                    if let (crate::val::Val::Str(key_str), crate::val::Val::Bool(true)) =
                        (key, value)
                        && key_str.as_ref() == "core"
                    {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    } else {
        false
    }
}
