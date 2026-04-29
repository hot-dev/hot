// Hot Language Unified Resolution System
//
// This module provides a single, canonical resolution system that both the compiler
// and VM use to ensure consistent variable/function lookup behavior. This allows
// resolution errors to be caught at compile time rather than runtime.

use crate::lang::bytecode::{NamespaceRegistry, VariableInfo, VariableType};
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::hot::libmap::get_hotlib_functions;
use crate::val::Val;
use ahash::{AHashMap, AHashSet};

/// Unified resolution context that can be used by both compiler and VM
#[derive(Debug, Clone)]
pub struct UnifiedResolver {
    /// Registry of all namespaces and their variables/functions
    pub namespace_registry: NamespaceRegistry,
    /// Registry of core variables with metadata for auto-import
    pub core_variables: Option<CoreVariableRegistry>,
    /// Hotlib functions (always available)
    pub hotlib_functions: AHashSet<String>,
    /// Current namespace being processed
    pub current_namespace: String,
    /// Current lexical scope stack (for local variables)
    pub lexical_scopes: Vec<AHashMap<String, Val>>,
}

impl Default for UnifiedResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl UnifiedResolver {
    pub fn new() -> Self {
        let hotlib_functions = get_hotlib_functions().into_iter().collect();

        Self {
            namespace_registry: NamespaceRegistry::new(),
            core_variables: None,
            hotlib_functions,
            current_namespace: "::".to_string(),
            lexical_scopes: Vec::new(),
        }
    }

    /// Create a UnifiedResolver with an existing NamespaceRegistry
    /// This allows access to core variables extracted by the compiler
    pub fn with_namespace_registry(namespace_registry: NamespaceRegistry) -> Self {
        let hotlib_functions = get_hotlib_functions().into_iter().collect();

        Self {
            namespace_registry,
            core_variables: None,
            hotlib_functions,
            current_namespace: "::".to_string(),
            lexical_scopes: Vec::new(),
        }
    }

    /// Create a UnifiedResolver with both NamespaceRegistry and CoreVariableRegistry
    /// This allows access to core variables extracted by the compiler
    pub fn with_registries(
        namespace_registry: NamespaceRegistry,
        core_variables: CoreVariableRegistry,
    ) -> Self {
        let hotlib_functions = get_hotlib_functions().into_iter().collect();

        Self {
            namespace_registry,
            core_variables: Some(core_variables),
            hotlib_functions,
            current_namespace: "::".to_string(),
            lexical_scopes: Vec::new(),
        }
    }

    /// Set the current namespace context
    pub fn set_current_namespace(&mut self, namespace: String) {
        self.current_namespace = namespace;
    }

    /// Push a new lexical scope (for function parameters, local variables)
    pub fn push_lexical_scope(&mut self, scope: AHashMap<String, Val>) {
        self.lexical_scopes.push(scope);
    }

    /// Pop the current lexical scope
    pub fn pop_lexical_scope(&mut self) {
        self.lexical_scopes.pop();
    }

    /// Add a variable to the namespace registry
    pub fn add_variable(&mut self, namespace: &str, var_info: VariableInfo) {
        self.namespace_registry.add_variable(namespace, var_info);
    }

    /// UNIFIED VARIABLE RESOLUTION - Same logic as VM
    ///
    /// Resolution order:
    /// 1. Fully qualified names (::namespace/var) -> Direct lookup
    /// 2. Unqualified names -> Check in order:
    ///    a) Lexical scope (local variables)
    ///    b) Current namespace variables
    ///    c) Core variables (auto-imported variables with core: true)
    ///    d) Other namespaces for core variables
    pub fn can_resolve_variable(&self, var_name: &str) -> bool {
        // call-lib is the only built-in function and is always available as a variable reference
        if var_name == "call-lib" {
            return true;
        }

        // 1. FULLY QUALIFIED NAMES - Direct namespace lookup
        if var_name.starts_with("::") {
            return self.can_resolve_qualified_variable(var_name);
        }

        // 2. UNQUALIFIED NAMES - Check in specified order

        // 2a. LEXICAL SCOPE - Check local variables first (innermost to outermost)
        for scope in self.lexical_scopes.iter().rev() {
            if scope.contains_key(var_name) {
                return true;
            }
        }

        // 2b. CURRENT NAMESPACE - Check namespace variables
        if let Some(variables) = self
            .namespace_registry
            .get_variables(&self.current_namespace)
            && variables.iter().any(|v| v.name == var_name)
        {
            return true;
        }

        // 2c. CORE VARIABLES - Check for core variables across all namespaces
        if self.is_core_variable_available(var_name) {
            return true;
        }

        false
    }

    /// UNIFIED FUNCTION RESOLUTION - Same logic as VM
    ///
    /// Resolution order:
    /// 1. Fully qualified names (::namespace/func/arity) -> Direct lookup
    /// 2. Unqualified names -> Check in order:
    ///    a) Lexical scope (local function variables)
    ///    b) Current namespace qualified (namespace/func/arity)
    ///    c) Core functions (auto-imported functions with core: true)
    ///    d) Hotlib functions (unqualified)
    pub fn can_resolve_function(&self, func_name: &str, arity: Option<usize>) -> bool {
        // call-lib is the only built-in function and is always available
        if func_name == "call-lib" {
            return true;
        }

        // 1. FULLY QUALIFIED NAMES - Direct lookup
        if func_name.starts_with("::") {
            // Try exact match first
            if self.hotlib_functions.contains(func_name) {
                return true;
            }

            // Try with arity if provided
            if let Some(arity) = arity {
                let arity_key = format!("{}/{}", func_name, arity);
                if self.hotlib_functions.contains(&arity_key) {
                    return true;
                }
            }

            return self.can_resolve_qualified_function(func_name, arity);
        }

        // 2. UNQUALIFIED NAMES - Check in specified order

        // 2a. LEXICAL SCOPE - Check if it's a local function variable
        for scope in self.lexical_scopes.iter().rev() {
            if scope.contains_key(func_name) {
                return true;
            }
        }

        // 2b. CURRENT NAMESPACE QUALIFIED - Try namespace/func/arity
        if let Some(arity) = arity {
            let qualified_arity_key = format!("{}/{}/{}", self.current_namespace, func_name, arity);
            if self.hotlib_functions.contains(&qualified_arity_key) {
                return true;
            }
        }

        let qualified_function_name = format!("{}/{}", self.current_namespace, func_name);
        if self.hotlib_functions.contains(&qualified_function_name) {
            return true;
        }

        // 2c. CORE FUNCTIONS - Check for core functions across all namespaces
        if self.is_core_function_available(func_name) {
            return true;
        }

        // 2d. HOTLIB FUNCTIONS - Unqualified hotlib lookup
        if self.hotlib_functions.contains(func_name) {
            return true;
        }

        false
    }

    /// Check if a qualified variable can be resolved
    fn can_resolve_qualified_variable(&self, qualified_name: &str) -> bool {
        if let Some(last_slash) = qualified_name.rfind('/') {
            let namespace = &qualified_name[..last_slash];
            let var_name = &qualified_name[last_slash + 1..];

            if let Some(variables) = self.namespace_registry.get_variables(namespace) {
                return variables.iter().any(|v| v.name == var_name);
            }
        }
        false
    }

    /// Check if a qualified function can be resolved
    fn can_resolve_qualified_function(&self, qualified_name: &str, _arity: Option<usize>) -> bool {
        if let Some(last_slash) = qualified_name.rfind('/') {
            let namespace = &qualified_name[..last_slash];
            let func_name = &qualified_name[last_slash + 1..];

            if let Some(variables) = self.namespace_registry.get_variables(namespace) {
                // Check if there's a function variable with this name
                for var in variables {
                    if var.name == func_name {
                        match var.var_type {
                            VariableType::Function | VariableType::CoreFunction => return true,
                            VariableType::TypeConstructor => return true, // Type constructors are callable
                            VariableType::Value => {
                                // Any value can potentially be called as a function in Hot
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Check if a core variable is available across all namespaces
    fn is_core_variable_available(&self, var_name: &str) -> bool {
        // Use the dedicated CoreVariableRegistry as the single source of truth
        if let Some(ref core_vars) = self.core_variables {
            return core_vars.contains(var_name);
        }

        // If no CoreVariableRegistry is available, we can't determine core variables.
        // This should only happen in narrowly scoped call sites or tests.
        false
    }

    /// Check if a core function is available across all namespaces
    fn is_core_function_available(&self, func_name: &str) -> bool {
        // Use the dedicated CoreVariableRegistry as the single source of truth
        if let Some(ref core_vars) = self.core_variables {
            return core_vars.contains(func_name);
        }

        // If no CoreVariableRegistry is available, we can't determine core functions.
        // This should only happen in narrowly scoped call sites or tests.
        false
    }

    /// Get detailed resolution information for debugging
    pub fn get_resolution_info(&self, name: &str) -> ResolutionInfo {
        let mut info = ResolutionInfo {
            name: name.to_string(),
            found_in_lexical_scope: false,
            found_in_current_namespace: false,
            found_as_core: false,
            found_in_hotlib: false,
            qualified_matches: Vec::new(),
        };

        // Check lexical scopes
        for scope in self.lexical_scopes.iter().rev() {
            if scope.contains_key(name) {
                info.found_in_lexical_scope = true;
                break;
            }
        }

        // Check current namespace
        if let Some(variables) = self
            .namespace_registry
            .get_variables(&self.current_namespace)
            && variables.iter().any(|v| v.name == name)
        {
            info.found_in_current_namespace = true;
        }

        // Check core variables
        info.found_as_core =
            self.is_core_variable_available(name) || self.is_core_function_available(name);

        // Check hotlib
        info.found_in_hotlib = self.hotlib_functions.contains(name);

        // Check qualified matches
        if name.starts_with("::")
            && (self.can_resolve_qualified_variable(name)
                || self.can_resolve_qualified_function(name, None))
        {
            info.qualified_matches.push(name.to_string());
        }

        info
    }
}

/// Detailed information about where a name was resolved
#[derive(Debug, Clone)]
pub struct ResolutionInfo {
    pub name: String,
    pub found_in_lexical_scope: bool,
    pub found_in_current_namespace: bool,
    pub found_as_core: bool,
    pub found_in_hotlib: bool,
    pub qualified_matches: Vec<String>,
}

impl ResolutionInfo {
    pub fn is_resolved(&self) -> bool {
        self.found_in_lexical_scope
            || self.found_in_current_namespace
            || self.found_as_core
            || self.found_in_hotlib
            || !self.qualified_matches.is_empty()
    }

    pub fn resolution_summary(&self) -> String {
        let mut parts = Vec::new();

        if self.found_in_lexical_scope {
            parts.push("lexical scope");
        }
        if self.found_in_current_namespace {
            parts.push("current namespace");
        }
        if self.found_as_core {
            parts.push("core auto-import");
        }
        if self.found_in_hotlib {
            parts.push("hotlib");
        }
        if !self.qualified_matches.is_empty() {
            parts.push("qualified name");
        }

        if parts.is_empty() {
            "not resolved".to_string()
        } else {
            format!("resolved via: {}", parts.join(", "))
        }
    }
}
