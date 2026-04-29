// Hot Language Variable Resolution Pass
//
// This module implements compile-time variable resolution to catch unresolved
// references before runtime. It walks the AST and validates that all variable
// references can be resolved within their scope.

use crate::lang::ast::{Flow, FnCall, Namespace, NamespaceAliases, Program, Ref, Value};
use crate::lang::errors::{CompilerError, CompilerErrors, CompilerResult, ErrorLocation};
use crate::lang::runtime::resolution::UnifiedResolver;
use ahash::{AHashMap, AHashSet};
use std::path::PathBuf;

/// Variable resolution context - now uses UnifiedResolver
pub struct ResolutionContext {
    /// Unified resolver for consistent resolution logic
    unified_resolver: UnifiedResolver,
    /// Current function parameters (for local scope)
    current_function_params: Vec<String>,
    /// Current local variables (for local scope within functions)
    current_local_variables: AHashSet<String>,
    /// Source file information for error reporting
    current_file: Option<PathBuf>,
    /// Source content for error reporting
    source_content: Option<String>,
    /// All source files for comprehensive error reporting
    source_files: AHashMap<String, String>,
}

impl Default for ResolutionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolutionContext {
    pub fn new() -> Self {
        Self {
            unified_resolver: UnifiedResolver::new(),
            current_function_params: Vec::new(),
            current_local_variables: AHashSet::new(),
            current_file: None,
            source_content: None,
            source_files: AHashMap::new(),
        }
    }

    /// Create a new ResolutionContext with an existing NamespaceRegistry
    /// This allows the resolver to access core variables extracted by the compiler
    pub fn with_namespace_registry(
        namespace_registry: crate::lang::bytecode::NamespaceRegistry,
    ) -> Self {
        Self {
            unified_resolver: UnifiedResolver::with_namespace_registry(namespace_registry),
            current_function_params: Vec::new(),
            current_local_variables: AHashSet::new(),
            current_file: None,
            source_content: None,
            source_files: AHashMap::new(),
        }
    }

    /// Create a new ResolutionContext with both NamespaceRegistry and CoreVariableRegistry
    /// This allows the resolver to access core variables extracted by the compiler
    pub fn with_registries(
        namespace_registry: crate::lang::bytecode::NamespaceRegistry,
        core_variables: crate::lang::compiler::core_registry::CoreVariableRegistry,
    ) -> Self {
        Self {
            unified_resolver: UnifiedResolver::with_registries(namespace_registry, core_variables),
            current_function_params: Vec::new(),
            current_local_variables: AHashSet::new(),
            current_file: None,
            source_content: None,
            source_files: AHashMap::new(),
        }
    }

    pub fn set_source_info(&mut self, file: Option<PathBuf>, content: Option<String>) {
        self.current_file = file;
        self.source_content = content;
    }

    /// Add a source file for error reporting
    pub fn add_source_file(&mut self, file_path: PathBuf, content: String) {
        // Store the source content for this file
        if let Some(file_str) = file_path.to_str() {
            self.source_files.insert(file_str.to_string(), content);
        }
    }

    /// Add a variable to the unified resolver
    pub fn add_variable(&mut self, namespace: &str, var_name: &str) {
        use crate::lang::bytecode::{VariableInfo, VariableType};

        let var_info = VariableInfo {
            name: var_name.to_string(),
            var_type: VariableType::Value,
            metadata: None,
            function_id: None,
        };
        self.unified_resolver.add_variable(namespace, var_info);
    }

    /// Add a function to the unified resolver
    pub fn add_function(&mut self, namespace: &str, func_name: &str) {
        use crate::lang::bytecode::{VariableInfo, VariableType};

        let var_info = VariableInfo {
            name: func_name.to_string(),
            var_type: VariableType::Function,
            metadata: None,
            function_id: None,
        };
        self.unified_resolver.add_variable(namespace, var_info);
    }

    /// Check if a variable can be resolved using unified resolution logic
    pub fn can_resolve_variable(&self, var_name: &str) -> bool {
        // Check function parameters first (local scope)
        if self.current_function_params.contains(&var_name.to_string()) {
            return true;
        }

        // Check local variables (within function scope)
        if self.current_local_variables.contains(var_name) {
            return true;
        }

        // Check Hot language built-in variables (type constructors, etc.)
        if self.can_resolve_hot_variable(var_name) {
            return true;
        }

        // Use unified resolver for consistent resolution logic
        self.unified_resolver.can_resolve_variable(var_name)
    }

    /// Check if a variable can be resolved using Hot language rules
    fn can_resolve_hot_variable(&self, var_name: &str) -> bool {
        // The hardcoded approach has been removed.
        // Core variables should be resolved through the proper Hot mechanisms:
        // 1. Core variable extraction (compiler extracts variables with core metadata)
        // 2. Unified resolution (UnifiedResolver finds core variables in NamespaceRegistry)
        // 3. Auto-import logic (core variables are automatically available)
        //
        // This method now only handles edge cases and local variable patterns.

        // 3. Check if it's a type constructor from user-defined types
        if self.is_type_constructor(var_name) {
            return true;
        }

        false
    }

    // Removed permissive auto-import list for core functions.

    /// Check if a name refers to a type constructor
    fn is_type_constructor(&self, name: &str) -> bool {
        // Check if it's a user-defined type that might be defined in the program
        // For now, we'll be permissive with capitalized names that look like types
        if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            // Common patterns for user-defined types
            return true;
        }
        false
    }

    /// Check if a function can be resolved using unified resolution logic
    pub fn can_resolve_function(&self, func_name: &str) -> bool {
        // Use unified resolver for consistent resolution logic
        // We don't have arity information at this point, so pass None
        self.unified_resolver.can_resolve_function(func_name, None)
    }

    /// Create an error location with available information
    fn create_error_location(&self, var_name: &str) -> Option<ErrorLocation> {
        // Try current file first (most likely location)
        if let Some(file) = &self.current_file
            && let Some(content) = &self.source_content
            && let Some(position) = content.find(var_name)
        {
            let (line, column) = self.calculate_line_column(content, position);
            return Some(ErrorLocation {
                line,
                column,
                position,
                length: var_name.len(),
                file: Some(file.clone()),
            });
        }

        // Final fallback to default position if we have a current file. Do not
        // scan unrelated files by raw text: duplicate names can point the error
        // at the wrong namespace.
        self.current_file.as_ref().map(|file| ErrorLocation {
            line: 1,
            column: 1,
            position: 0,
            length: var_name.len(),
            file: Some(file.clone()),
        })
    }

    /// Calculate line and column from position in source
    fn calculate_line_column(&self, source: &str, position: usize) -> (usize, usize) {
        let mut line = 1;
        let mut column = 1;

        for (i, ch) in source.chars().enumerate() {
            if i >= position {
                break;
            }
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }

        (line, column)
    }
}

/// Variable resolver for Hot programs
pub struct Resolver {
    context: ResolutionContext,
    errors: CompilerErrors,
    /// Namespace aliases for the currently-being-validated namespace.
    /// Updated when entering each namespace so that aliased references
    /// (e.g. `::http/get` where `::http` is an alias for `::hot::http`)
    /// can be resolved during validation.
    current_aliases: NamespaceAliases,
}

impl Resolver {
    pub fn new() -> Self {
        Self {
            context: ResolutionContext::new(),
            errors: CompilerErrors::new(),
            current_aliases: NamespaceAliases::new(),
        }
    }

    /// Create a new Resolver with an existing NamespaceRegistry
    /// This allows the resolver to access core variables extracted by the compiler
    pub fn with_namespace_registry(
        namespace_registry: crate::lang::bytecode::NamespaceRegistry,
    ) -> Self {
        Self {
            context: ResolutionContext::with_namespace_registry(namespace_registry),
            errors: CompilerErrors::new(),
            current_aliases: NamespaceAliases::new(),
        }
    }

    /// Create a new Resolver with both NamespaceRegistry and CoreVariableRegistry
    /// This allows the resolver to access core variables extracted by the compiler
    pub fn with_registries(
        namespace_registry: crate::lang::bytecode::NamespaceRegistry,
        core_variables: crate::lang::compiler::core_registry::CoreVariableRegistry,
    ) -> Self {
        Self {
            context: ResolutionContext::with_registries(namespace_registry, core_variables),
            errors: CompilerErrors::new(),
            current_aliases: NamespaceAliases::new(),
        }
    }

    pub fn set_source_info(&mut self, file: Option<PathBuf>, content: Option<String>) {
        self.context.set_source_info(file.clone(), content.clone());
        if let (Some(f), Some(c)) = (file, content) {
            self.errors.add_source(f.display().to_string(), c);
        }
    }

    pub fn add_source_file(&mut self, file_path: PathBuf, content: String) {
        self.context
            .add_source_file(file_path.clone(), content.clone());
        self.errors
            .add_source(file_path.display().to_string(), content);
    }

    /// Resolve all variables in a program
    pub fn resolve_program(&mut self, program: &Program) -> CompilerResult<()> {
        // First pass: collect all variable and function declarations
        self.collect_declarations(program);

        // Second pass: validate all references
        self.validate_references(program)?;

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors.clone())
        }
    }

    /// Collect all variable and function declarations
    fn collect_declarations(&mut self, program: &Program) {
        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();

            for (var, value) in &namespace.scope.vars {
                let var_name = var.sym.name();

                // Add the variable itself
                self.context.add_variable(&ns_str, var_name);

                // If it's a function definition, also add it as a function
                if matches!(value, Value::Fn(_)) {
                    self.context.add_function(&ns_str, var_name);
                }
            }
        }
    }

    /// Validate all variable references in the program
    fn validate_references(&mut self, program: &Program) -> CompilerResult<()> {
        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            self.context
                .unified_resolver
                .set_current_namespace(ns_str.clone());
            // Store the current namespace's aliases so that Ref::Ns validation
            // can resolve aliased references (e.g. ::http -> ::hot::http)
            self.current_aliases = namespace.aliases.clone();
            self.validate_namespace_references(namespace)?;
        }
        Ok(())
    }

    /// Validate references in a namespace
    fn validate_namespace_references(&mut self, namespace: &Namespace) -> CompilerResult<()> {
        for (_var, value) in &namespace.scope.vars {
            self.validate_value_references(value)?;
        }
        Ok(())
    }

    /// Validate references in a value
    fn validate_value_references(&mut self, value: &Value) -> CompilerResult<()> {
        match value {
            Value::Ref(ref_val) => {
                self.validate_ref_references(ref_val)?;
            }
            Value::FnCall(fn_call) => {
                self.validate_function_call_references(fn_call)?;
            }
            Value::Flow(flow) => {
                self.validate_flow_references(flow)?;
            }
            Value::MultipleValues(values) => {
                // Mirror compiler behavior: treat [Ref(Var name), expr] as assignment
                if values.len() >= 2
                    && let Value::Ref(Ref::Var(var_ref)) = &values[0]
                {
                    // Validate RHS first with current scope
                    // Special-case lambda RHS to ensure parameters are scoped correctly
                    if let Value::Lambda(lambda) = &values[1] {
                        let saved_params = self.context.current_function_params.clone();
                        // Push lambda parameters
                        for param in &lambda.args.args {
                            self.context
                                .current_function_params
                                .push(param.var.sym.name().to_string());
                        }
                        // Validate body explicitly
                        self.validate_value_references(&lambda.body)?;
                        // Restore params
                        self.context.current_function_params = saved_params;
                    } else if let Value::FnCall(fn_call) = &values[1]
                        && fn_call.args.len() == 1
                        && matches!(fn_call.args[0].value, Value::Lambda(_))
                    {
                        if let Value::Lambda(lambda) = &fn_call.args[0].value {
                            let saved_params = self.context.current_function_params.clone();
                            for param in &lambda.args.args {
                                self.context
                                    .current_function_params
                                    .push(param.var.sym.name().to_string());
                            }
                            self.validate_value_references(&lambda.body)?;
                            self.context.current_function_params = saved_params;
                        }
                    } else {
                        self.validate_value_references(&values[1])?;
                    }
                    // Then bind the new local
                    let var_name = var_ref.var.sym.name().to_string();
                    self.context.current_local_variables.insert(var_name);
                    // Validate any remaining values (if present)
                    for rest in &values[2..] {
                        self.validate_value_references(rest)?;
                    }
                    return Ok(());
                }
                // Otherwise, validate all items normally
                for v in values {
                    self.validate_value_references(v)?;
                }
            }
            Value::Fn(fn_defs) => {
                // Nested function definitions can capture variables from outer scope (closures)
                // Save current state to restore after validation
                let saved_params = self.context.current_function_params.clone();
                let saved_locals = self.context.current_local_variables.clone();

                for fn_def in fn_defs {
                    // Add function parameters to params scope (don't clear - preserve outer scope)
                    for param in &fn_def.args.args {
                        self.context
                            .current_function_params
                            .push(param.var.sym.name().to_string());
                    }
                    // Collect and validate function body
                    self.collect_local_variables(&fn_def.body)?;
                    self.validate_value_references(&fn_def.body)?;
                }

                // Restore previous scope
                self.context.current_function_params = saved_params;
                self.context.current_local_variables = saved_locals;
            }
            Value::Val(val, _) => {
                self.validate_val_references(val)?;
            }
            Value::Lambda(lambda) => {
                // Lambdas can capture variables from outer scope (closures)
                // Save current state to restore after validation
                let saved_params = self.context.current_function_params.clone();
                let saved_locals = self.context.current_local_variables.clone();

                // Add lambda parameters to function params scope
                // NOTE: Don't clear locals - lambdas should see outer scope variables (closures)
                for param in &lambda.args.args {
                    self.context
                        .current_function_params
                        .push(param.var.sym.name().to_string());
                }

                // Validate lambda body with both outer scope variables AND lambda parameters in scope
                self.validate_value_references(&lambda.body)?;

                // Restore previous scope
                self.context.current_function_params = saved_params;
                self.context.current_local_variables = saved_locals;
            }
            Value::TypeDef(_) => {
                // Type definitions don't contain variable references that need validation
                // Type field names are not variables
            }
            _ => {
                // Other value types don't contain references
            }
        }
        Ok(())
    }

    /// Validate references in a Ref
    fn validate_ref_references(&mut self, ref_val: &Ref) -> CompilerResult<()> {
        match ref_val {
            Ref::Var(var_ref) => {
                let var_name = var_ref.var.sym.name();
                if !self.context.can_resolve_variable(var_name) {
                    // Try to build a precise error location from AST metadata if available
                    let location = if let Some(src) = &var_ref.src {
                        Some(ErrorLocation {
                            line: src.line,
                            column: src.column,
                            position: src.position,
                            length: src.length,
                            file: src.file.as_ref().map(PathBuf::from),
                        })
                    } else {
                        self.context.create_error_location(var_name)
                    };
                    self.errors.add(CompilerError::UnresolvedVariable {
                        var_name: var_name.to_string(),
                        namespace: self.context.unified_resolver.current_namespace.clone(),
                        message: format!("Variable '{}' cannot be resolved", var_name),
                        location,
                    });
                }
            }
            Ref::Ns(ns_ref) => {
                // Validate namespace references: check that the namespace exists
                // and, if a function/variable name is specified, that it exists
                // within the namespace.
                let ns_path_str = format!("{}", ns_ref.ns);

                // Resolve through namespace aliases (e.g. ::http -> ::hot::http)
                let resolved_ns_str =
                    if let Some(source_path) = self.current_aliases.get(&ns_ref.ns) {
                        format!("{}", source_path)
                    } else {
                        ns_path_str.clone()
                    };

                if let Some(func_name) = &ns_ref.function_name {
                    // Qualified reference like ::hot::alert/alert — validate both
                    // namespace and function/variable name
                    let qualified_name = format!("{}/{}", resolved_ns_str, func_name);
                    if !self
                        .context
                        .unified_resolver
                        .can_resolve_variable(&qualified_name)
                        && !self
                            .context
                            .unified_resolver
                            .can_resolve_function(&qualified_name, None)
                    {
                        let location = ns_ref.src.as_ref().map(|src| ErrorLocation {
                            line: src.line,
                            column: src.column,
                            position: src.position,
                            length: src.length,
                            file: src.file.as_ref().map(PathBuf::from),
                        });

                        // Provide a more helpful message depending on whether the
                        // namespace itself exists or just the member is wrong.
                        let ns_exists = self
                            .context
                            .unified_resolver
                            .namespace_registry
                            .get_variables(&resolved_ns_str)
                            .is_some();

                        if ns_exists {
                            // Namespace exists but member doesn't — suggest available names
                            let available: Vec<String> = self
                                .context
                                .unified_resolver
                                .namespace_registry
                                .get_variables(&resolved_ns_str)
                                .unwrap_or_default()
                                .iter()
                                .map(|v| v.name.clone())
                                .collect();
                            let suggestion = if available.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    ". Available in '{}': {}",
                                    resolved_ns_str,
                                    available.join(", ")
                                )
                            };
                            self.errors.add(CompilerError::UnresolvedFunction {
                                func_name: qualified_name.clone(),
                                namespace: self.context.unified_resolver.current_namespace.clone(),
                                message: format!(
                                    "'{}' is not defined in namespace '{}'{}",
                                    func_name, resolved_ns_str, suggestion
                                ),
                                location,
                            });
                        } else {
                            self.errors.add(CompilerError::UnresolvedVariable {
                                var_name: qualified_name.clone(),
                                namespace: self.context.unified_resolver.current_namespace.clone(),
                                message: format!(
                                    "Namespace '{}' cannot be resolved",
                                    resolved_ns_str
                                ),
                                location,
                            });
                        }
                    }
                } else {
                    // Bare namespace reference (no function name) — validate the
                    // namespace path itself exists, but only for paths that look
                    // like they should resolve to a registered namespace (skip
                    // namespace alias declarations which define new aliases).
                    let ns_exists = self
                        .context
                        .unified_resolver
                        .namespace_registry
                        .get_variables(&resolved_ns_str)
                        .is_some();
                    let is_alias_target = self
                        .current_aliases
                        .values()
                        .any(|v| format!("{}", v) == ns_path_str);

                    if !ns_exists && !is_alias_target {
                        // Check if this is the RHS of an alias declaration; those
                        // are stored as scope vars and should not be flagged.
                        // We only flag when the namespace is used in an expression
                        // context where it should resolve.
                        // For now, skip bare namespace refs — the primary risk is
                        // qualified refs (with function_name) which are validated above.
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate references in a function call
    fn validate_function_call_references(&mut self, fn_call: &FnCall) -> CompilerResult<()> {
        // Validate the function reference
        self.validate_value_references(&fn_call.function)?;

        // Validate argument references
        for arg in &fn_call.args {
            self.validate_value_references(&arg.value)?;
        }

        Ok(())
    }

    /// Validate references in a flow
    fn validate_flow_references(&mut self, flow: &Flow) -> CompilerResult<()> {
        // Activate flow-scoped aliases, saving previous state for restore
        let saved_aliases = if !flow.aliases.is_empty() {
            Some(self.current_aliases.clone())
        } else {
            None
        };
        let mut alias_cursor = 0;

        // Sequential lexical scoping: variables bound earlier in the flow are
        // available to later expressions. Recognize assignment patterns of the
        // form: Ref(Var(name)) followed by an expression.
        let mut i = 0;
        while i < flow.expressions.len() {
            // Activate any aliases whose position matches the current expression index
            while alias_cursor < flow.aliases.len() && flow.aliases[alias_cursor].0 <= i {
                let (_, ref alias, ref source) = flow.aliases[alias_cursor];
                self.current_aliases.insert(alias.clone(), source.clone());
                alias_cursor += 1;
            }
            let expr = &flow.expressions[i];

            // Detect "name value" assignment pattern
            if i + 1 < flow.expressions.len()
                && let Value::Ref(Ref::Var(var_ref)) = expr
            {
                let var_name = var_ref.var.sym.name().to_string();
                // Validate RHS first using current scope
                let rhs = &flow.expressions[i + 1];
                // Special-case lambda/fn RHS: validate with parameters in scope
                if let Value::Lambda(lambda) = rhs {
                    let saved_params = self.context.current_function_params.clone();
                    for param in &lambda.args.args {
                        self.context
                            .current_function_params
                            .push(param.var.sym.name().to_string());
                    }
                    self.validate_value_references(&lambda.body)?;
                    self.context.current_function_params = saved_params;
                } else if let Value::Fn(fn_defs) = rhs {
                    // Handle `name fn (params) { body }` pattern
                    // This is a local function definition that can capture outer scope variables
                    for fn_def in fn_defs {
                        let saved_params = self.context.current_function_params.clone();
                        // Add function parameters
                        for param in &fn_def.args.args {
                            self.context
                                .current_function_params
                                .push(param.var.sym.name().to_string());
                        }
                        // Collect and validate function body
                        self.collect_local_variables(&fn_def.body)?;
                        self.validate_value_references(&fn_def.body)?;
                        self.context.current_function_params = saved_params;
                    }
                } else {
                    self.validate_value_references(rhs)?;
                }
                // Bind variable into local scope for subsequent expressions
                self.context.current_local_variables.insert(var_name);
                i += 2;
                continue;
            }

            // Detect two-expression lambda sugar:
            //   name (param1, param2, ...) { body }
            // Parsed as: [ FnCall(name, [Ref(param1), Ref(param2), ...]), Flow(body) ]
            if let Value::FnCall(fn_call) = expr
                && let Value::Ref(Ref::Var(var_ref)) = fn_call.function.as_ref()
            {
                let local_name = var_ref.var.sym.name().to_string();
                if i + 1 < flow.expressions.len() {
                    // Ensure all args are simple Ref(Var) parameters
                    let mut param_names: Vec<String> = Vec::new();
                    let mut args_ok = true;
                    for arg in &fn_call.args {
                        if let Value::Ref(Ref::Var(pv)) = &arg.value {
                            param_names.push(pv.var.sym.name().to_string());
                        } else {
                            args_ok = false;
                            break;
                        }
                    }

                    if args_ok {
                        let body_expr = &flow.expressions[i + 1];
                        // Validate body with lambda params in scope
                        let saved_params = self.context.current_function_params.clone();
                        for p in param_names {
                            self.context.current_function_params.push(p);
                        }
                        match body_expr {
                            Value::Flow(body_flow) => {
                                self.validate_flow_references(body_flow)?;
                            }
                            other => {
                                self.validate_value_references(other)?;
                            }
                        }
                        self.context.current_function_params = saved_params;

                        // Bind local name for subsequent expressions
                        self.context.current_local_variables.insert(local_name);
                        i += 2;
                        continue;
                    }
                }
            }

            // Detect inline local function definition: name (args) { body }
            // This is represented as a Value::FnCall where function is a local name
            if let Value::FnCall(fn_call) = expr
                && let Value::Ref(Ref::Var(var_ref)) = fn_call.function.as_ref()
            {
                let local_name = var_ref.var.sym.name().to_string();
                // If the call has a single lambda arg, treat as local def: name (args) { body }
                if fn_call.args.len() == 1 {
                    let arg_val = &fn_call.args[0].value;
                    if let Value::Lambda(lambda) = arg_val {
                        // Validate lambda body with params in scope
                        let saved_params = self.context.current_function_params.clone();
                        for param in &lambda.args.args {
                            self.context
                                .current_function_params
                                .push(param.var.sym.name().to_string());
                        }
                        self.validate_value_references(&lambda.body)?;
                        self.context.current_function_params = saved_params;
                        // Bind local name and continue
                        self.context.current_local_variables.insert(local_name);
                        i += 1;
                        continue;
                    }
                }
            }

            // Normal expression validation
            self.validate_value_references(expr)?;
            i += 1;
        }

        // Restore aliases to pre-flow state
        if let Some(saved) = saved_aliases {
            self.current_aliases = saved;
        }

        Ok(())
    }

    /// Collect local variable definitions from a value (first pass)
    fn collect_local_variables(&mut self, value: &Value) -> CompilerResult<()> {
        match value {
            Value::MultipleValues(values) => {
                // Check if this is a variable assignment pattern: [Var, Value]
                if values.len() == 2
                    && let Value::Ref(Ref::Var(var_ref)) = &values[0]
                {
                    let var_name = var_ref.var.sym.name().to_string();
                    self.context.current_local_variables.insert(var_name);
                }
                // Recursively collect from all values
                for val in values {
                    self.collect_local_variables(val)?;
                }
            }
            Value::Flow(flow) => {
                // Collect local variables from flow expressions
                // Must detect "name value" assignment pattern (same as validate_flow_references)
                let mut i = 0;
                while i < flow.expressions.len() {
                    let expr = &flow.expressions[i];

                    // Detect "name value" assignment pattern
                    if i + 1 < flow.expressions.len()
                        && let Value::Ref(Ref::Var(var_ref)) = expr
                    {
                        let var_name = var_ref.var.sym.name().to_string();
                        // Insert the variable into local scope
                        self.context.current_local_variables.insert(var_name);
                        // Recursively collect from RHS
                        self.collect_local_variables(&flow.expressions[i + 1])?;
                        i += 2;
                        continue;
                    }

                    // Not an assignment pattern, collect from expression normally
                    self.collect_local_variables(expr)?;
                    i += 1;
                }
            }
            Value::FnCall(fn_call) => {
                // Recursively collect from function call arguments
                for arg in &fn_call.args {
                    self.collect_local_variables(&arg.value)?;
                }
            }
            Value::Fn(_fn_defs) => {
                // Don't collect from nested function definitions - they have their own scope
                // This prevents variables from inner functions leaking to outer scope
            }
            Value::Lambda(lambda) => {
                // Lambdas have their own scope for local variable collection
                // But they inherit outer scope for closure capture
                // Save current scope
                let saved_params = self.context.current_function_params.clone();
                let saved_locals = self.context.current_local_variables.clone();

                // Add lambda parameters to local scope
                for param in &lambda.args.args {
                    self.context
                        .current_function_params
                        .push(param.var.sym.name().to_string());
                }

                // Only collect local variables from lambda body during collection pass
                // Do NOT validate here - validation happens in the second pass
                // after all local variables have been collected
                self.collect_local_variables(&lambda.body)?;

                // Restore previous scope
                self.context.current_function_params = saved_params;
                self.context.current_local_variables = saved_locals;
            }
            _ => {
                // For other value types, no local variables to collect
            }
        }
        Ok(())
    }

    /// Validate references in a Val (for nested structures)
    #[allow(clippy::only_used_in_recursion)]
    fn validate_val_references(&mut self, val: &crate::val::Val) -> CompilerResult<()> {
        match val {
            crate::val::Val::Vec(elements) => {
                for element in elements {
                    self.validate_val_references(element)?;
                }
            }
            crate::val::Val::Map(map) => {
                for (key, value) in map.iter() {
                    self.validate_val_references(key)?;
                    self.validate_val_references(value)?;
                }
            }
            crate::val::Val::Box(_boxed) => {
                // Box contains a trait object, we can't validate its contents directly
                // This would require a more sophisticated approach to handle boxed values
                // For now, we skip validation of boxed contents
            }
            _ => {
                // Other Val types don't contain references
            }
        }
        Ok(())
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Phase-1.5: namespace-import resolution
// ============================================================================
//
// After all source programs are merged into a single `Program`, but before
// type-checking and bytecode emission, free variable references that name
// an imported namespace function must be rewritten to point at the
// namespace-qualified function. This pass is what makes
// `(import [::hot::coll :refer [map]])` work — once it runs, every bare
// `map` reference inside that namespace's scope holds an explicit
// `Ref::Ns` value pointing at `::hot::coll/map`.
//
// The pass intentionally skips into `Value::Flow` because flow-local
// variables must be resolved at runtime by the VM, not at AST-rewrite
// time (otherwise e.g. `y g` where `g` is a flow-local variable would be
// rewritten to point at g's *value* instead of capturing g's runtime
// value into y).

/// Resolve all variable references to namespace functions (Phase 1.5).
/// Called after all programs are merged to handle imported functions.
pub fn resolve_all_variable_references(program: &mut crate::lang::ast::Program) {
    use crate::lang::ast::{NsPath, Value};

    tracing::debug!("Starting variable reference resolution phase...");

    // First, collect all namespace data to avoid borrowing issues
    let namespace_data: AHashMap<NsPath, AHashMap<String, crate::lang::ast::Value>> = program
        .namespaces
        .iter()
        .map(|(ns_path, namespace)| {
            let mut imported_functions = AHashMap::new();
            for (var, value) in namespace.scope.vars.iter() {
                if let Value::Ref(crate::lang::ast::Ref::Ns(ns_ref)) = value {
                    tracing::debug!(
                        "Found imported function '{}' -> {:?}",
                        var.sym.name(),
                        ns_ref
                    );
                    imported_functions.insert(var.sym.name().to_string(), value.clone());
                }
            }
            tracing::debug!(
                "Namespace {:?} has {} imported functions",
                ns_path,
                imported_functions.len()
            );
            (ns_path.clone(), imported_functions)
        })
        .collect();

    // Now iterate through all namespaces and resolve variable references
    for (ns_path, namespace) in program.namespaces.iter_mut() {
        let mut vars_to_update = Vec::new();

        for (var, value) in namespace.scope.vars.iter() {
            let mut updated_value = value.clone();
            let mut changed = false;
            resolve_var_refs_in_value(&mut updated_value, ns_path, &namespace_data, &mut changed);

            if changed {
                tracing::debug!(
                    "Variable '{}' in namespace {:?} was updated",
                    var.sym.name(),
                    ns_path
                );
                vars_to_update.push((var.clone(), updated_value));
            }
        }

        for (var, updated_value) in vars_to_update {
            namespace.scope.vars.insert(var, updated_value);
        }
    }

    tracing::debug!("Completed variable reference resolution phase.");
}

/// Recursively resolve variable references in a Value
fn resolve_var_refs_in_value(
    value: &mut crate::lang::ast::Value,
    containing_ns: &crate::lang::ast::NsPath,
    namespace_data: &AHashMap<crate::lang::ast::NsPath, AHashMap<String, crate::lang::ast::Value>>,
    changed: &mut bool,
) {
    use crate::lang::ast::{Ref, Value};
    use crate::val::Val;

    match value {
        Value::FnCall(fn_call) => {
            // Resolve the function reference itself
            if let Value::Ref(Ref::Var(var_ref)) = fn_call.function.as_ref()
                && let Some(resolved_value) = resolve_variable_to_namespace_function(
                    &var_ref.var,
                    containing_ns,
                    namespace_data,
                )
            {
                *fn_call.function = resolved_value;
                *changed = true;
            }

            // And recurse into args
            for arg in fn_call.args.iter_mut() {
                resolve_var_refs_in_value(&mut arg.value, containing_ns, namespace_data, changed);
            }
        }
        Value::Ref(Ref::Var(var_ref)) => {
            tracing::debug!(
                "Checking if var '{}' resolves to namespace function",
                var_ref.var.sym.name()
            );
            if let Some(resolved_value) =
                resolve_variable_to_namespace_function(&var_ref.var, containing_ns, namespace_data)
            {
                tracing::warn!(
                    "RESOLVING var '{}' to {:?}",
                    var_ref.var.sym.name(),
                    resolved_value
                );
                *value = resolved_value;
                *changed = true;
            }
        }
        Value::Flow(flow) => {
            // Flow-local variables must be resolved at runtime by the VM,
            // not at AST-rewrite time. See module header for why.
            _ = (flow, containing_ns, namespace_data, changed);
        }
        Value::Val(Val::Vec(_), _) | Value::Val(Val::Map(_), _) => {
            // Vec/Map items are Val (runtime value), not Value (AST node);
            // they don't carry namespace-import bindings to rewrite.
        }
        _ => {}
    }
}

/// Look up `var` in `containing_ns`'s import table; if it names an
/// imported namespace function, return the resolved `Ref::Ns` value.
fn resolve_variable_to_namespace_function(
    var: &crate::lang::ast::Var,
    containing_ns: &crate::lang::ast::NsPath,
    namespace_data: &AHashMap<crate::lang::ast::NsPath, AHashMap<String, crate::lang::ast::Value>>,
) -> Option<crate::lang::ast::Value> {
    namespace_data
        .get(containing_ns)
        .and_then(|imported| imported.get(var.sym.name()))
        .cloned()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_variable_resolution() {
        let mut resolver = Resolver::new();

        // Test that variables in the same namespace can be resolved
        resolver.context.add_variable("::test", "my_var");
        resolver
            .context
            .unified_resolver
            .set_current_namespace("::test".to_string());

        assert!(resolver.context.can_resolve_variable("my_var"));
        assert!(!resolver.context.can_resolve_variable("unknown_var"));
    }

    #[test]
    fn test_function_resolution() {
        let resolver = Resolver::new();

        // Test that hotlib functions can be resolved
        assert!(resolver.context.can_resolve_function("::hot::math/add"));

        // Test that unknown functions cannot be resolved
        assert!(!resolver.context.can_resolve_function("unknown_function"));
    }
}
