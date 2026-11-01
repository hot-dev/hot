// Hot Language Variable Resolution Pass
//
// This module implements compile-time variable resolution to catch unresolved
// references before runtime. It walks the AST and validates that all variable
// references can be resolved within their scope.

use crate::lang::ast::{
    AstNode, Flow, FnCall, MatchArm, Namespace, NamespaceAliases, Program, Ref, Source,
    TemplatePart, Value,
};
use crate::lang::errors::{CompilerError, CompilerErrors, CompilerResult, ErrorLocation};
use crate::lang::runtime::resolution::UnifiedResolver;
use crate::val::Val;
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
    /// Source of the top-level definition being validated, for a reference
    /// with no source of its own (`...name` in a Vec literal).
    current_def_src: Option<Source>,
    /// How many literals, templates, flow arms, spreads or `do(...)` the
    /// walk is inside. 3.0.0 didn't check the names in these, so an
    /// unresolved one there is a warning (see `report_unresolved`).
    unchecked_before: u32,
    /// Non-fatal diagnostics.
    warnings: Vec<CompilerError>,
}

impl Resolver {
    pub fn new() -> Self {
        Self {
            context: ResolutionContext::new(),
            errors: CompilerErrors::new(),
            current_aliases: NamespaceAliases::new(),
            current_def_src: None,
            unchecked_before: 0,
            warnings: Vec::new(),
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
            current_def_src: None,
            unchecked_before: 0,
            warnings: Vec::new(),
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
            current_def_src: None,
            unchecked_before: 0,
            warnings: Vec::new(),
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

    /// Resolve all variables in a program, and rewrite the names in each
    /// top-level and namespace `meta` to the definitions they name.
    pub fn resolve_program(&mut self, program: &mut Program) -> CompilerResult<()> {
        // First pass: collect all variable and function declarations
        self.collect_declarations(program);

        self.resolve_meta_refs(program);

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
        for (var, value) in &namespace.scope.vars {
            self.current_def_src = var.src.clone();
            self.validate_value_references(value)?;
        }
        self.current_def_src = None;
        Ok(())
    }

    /// Where `text` first appears in the definition being validated, from
    /// the definition's start; the definition itself when it doesn't.
    fn definition_location_of(&self, text: &str) -> Option<ErrorLocation> {
        let src = self.current_def_src.as_ref()?;
        let content = src
            .file
            .as_ref()
            .and_then(|file| self.context.source_files.get(file));
        let (line, column, position, length) = match content
            .and_then(|content| Some((content, content.get(src.position..)?.find(text)?)))
        {
            Some((content, offset)) => {
                let position = src.position + offset;
                let (line, column) = self.context.calculate_line_column(content, position);
                (line, column, position, text.len())
            }
            None => (src.line, src.column, src.position, src.length),
        };
        Some(ErrorLocation {
            line,
            column,
            position,
            length,
            file: src.file.as_ref().map(PathBuf::from),
        })
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
            Value::Cond(..)
            | Value::CondDefault(_)
            | Value::Match(_)
            | Value::MatchArm(_)
            | Value::TemplateLiteral(_)
            | Value::Raw(_)
            | Value::Do(_)
            | Value::MapWithSpread { .. } => {
                self.unchecked_before += 1;
                let result = self.validate_nested_references(value);
                self.unchecked_before -= 1;
                result?;
            }
            Value::VariadicExpansion(name) => {
                // `...name` reads the variable `name`. Unchecked before,
                // so a warning (see `report_unresolved`).
                let var_name = name.split('.').next().unwrap_or(name);
                if !self.context.can_resolve_variable(var_name) {
                    self.unchecked_before += 1;
                    self.report_unresolved(CompilerError::UnresolvedVariable {
                        var_name: var_name.to_string(),
                        namespace: self.context.unified_resolver.current_namespace.clone(),
                        message: format!("Variable '{}' cannot be resolved", var_name),
                        location: self
                            .definition_location_of(&format!("...{name}"))
                            .or_else(|| self.context.create_error_location(var_name)),
                    });
                    self.unchecked_before -= 1;
                }
            }
            Value::TypeImplementation(_) | Value::Unbound(_) | Value::Placeholder(_) => {
                // No references to validate
            }
        }
        Ok(())
    }

    /// Validate the references inside a flow arm, template, spread, `do`
    /// or `raw`.
    fn validate_nested_references(&mut self, value: &Value) -> CompilerResult<()> {
        match value {
            Value::Cond(_, predicate, body) => {
                self.validate_value_references(predicate)?;
                self.validate_flow_references(body)?;
            }
            Value::CondDefault(body) => {
                self.validate_flow_references(body)?;
            }
            Value::Match(match_expr) => {
                self.validate_value_references(&match_expr.value)?;
                for arm in &match_expr.arms {
                    self.validate_match_arm_references(arm)?;
                }
            }
            Value::MatchArm(arm) => {
                self.validate_match_arm_references(arm)?;
            }
            Value::TemplateLiteral(template) => {
                for part in &template.parts {
                    if let TemplatePart::Expression(expr) = part {
                        self.validate_value_references(expr)?;
                    }
                }
            }
            Value::Raw(inner) | Value::Do(inner) => {
                self.validate_value_references(inner)?;
            }
            Value::MapWithSpread {
                base_entries,
                spread_entries,
            } => {
                for value in base_entries.values() {
                    self.validate_val_references(value)?;
                }
                for (_, value) in spread_entries {
                    self.validate_value_references(value)?;
                }
            }
            _ => self.validate_value_references(value)?,
        }
        Ok(())
    }

    /// Report an unresolved reference. Inside a literal, template, flow
    /// arm, spread or `do(...)` it is a warning: 3.0.0 didn't check these,
    /// and a program whose unresolved name there is never evaluated ran.
    fn report_unresolved(&mut self, error: CompilerError) {
        if self.unchecked_before == 0 {
            self.errors.add(error);
            return;
        }
        let (code, headline, detail, location) = match error {
            CompilerError::UnresolvedVariable {
                var_name,
                message,
                location,
                ..
            } => (
                "unresolved-variable",
                format!("Unresolved variable '{var_name}'"),
                message,
                location,
            ),
            CompilerError::UnresolvedFunction {
                func_name,
                message,
                location,
                ..
            } => (
                "unresolved-function",
                format!("Unresolved function '{func_name}'"),
                message,
                location,
            ),
            other => {
                self.errors.add(other);
                return;
            }
        };
        self.warnings.push(CompilerError::Constraint {
            code,
            message: format!("{headline}: {detail}"),
            help: Some("evaluating it fails at run time".to_string()),
            warning: true,
            location,
        });
    }

    /// Move out the non-fatal diagnostics collected by `resolve_program`.
    pub fn take_warnings(&mut self) -> Vec<CompilerError> {
        std::mem::take(&mut self.warnings)
    }

    /// Validate a match arm's body, with the arm's binding (`Variant(v) =>`)
    /// in scope.
    fn validate_match_arm_references(&mut self, arm: &MatchArm) -> CompilerResult<()> {
        if let Some(binding) = &arm.binding {
            self.context.current_local_variables.insert(binding.clone());
        }
        self.validate_value_references(&arm.body)
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
                    self.report_unresolved(CompilerError::UnresolvedVariable {
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
                    // `::ns/Type.Variant` resolves when `::ns/Type` does, as
                    // `Type.Variant` does when `Type` does.
                    let member = match func_name.split_once('.') {
                        Some((type_name, _)) => format!("{}/{}", resolved_ns_str, type_name),
                        None => qualified_name.clone(),
                    };
                    if !self.context.unified_resolver.can_resolve_variable(&member)
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
                            self.report_unresolved(CompilerError::UnresolvedFunction {
                                func_name: qualified_name.clone(),
                                namespace: self.context.unified_resolver.current_namespace.clone(),
                                message: format!(
                                    "'{}' is not defined in namespace '{}'{}",
                                    func_name, resolved_ns_str, suggestion
                                ),
                                location,
                            });
                        } else {
                            self.report_unresolved(CompilerError::UnresolvedVariable {
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
            // Bindings inside flow arms and literals are collected too, so
            // a body's later references to them resolve.
            Value::Cond(_, predicate, body) => {
                self.collect_local_variables(predicate)?;
                for expr in &body.expressions {
                    self.collect_local_variables(expr)?;
                }
            }
            Value::CondDefault(body) => {
                for expr in &body.expressions {
                    self.collect_local_variables(expr)?;
                }
            }
            Value::Match(match_expr) => {
                self.collect_local_variables(&match_expr.value)?;
                for arm in &match_expr.arms {
                    self.collect_local_variables(&arm.body)?;
                }
            }
            Value::MatchArm(arm) => {
                self.collect_local_variables(&arm.body)?;
            }
            Value::Raw(inner) | Value::Do(inner) => {
                self.collect_local_variables(inner)?;
            }
            Value::Val(val, _) => {
                self.collect_val_local_variables(val)?;
            }
            _ => {
                // For other value types, no local variables to collect
            }
        }
        Ok(())
    }

    /// Collect local variable definitions from the expressions inside a
    /// Vec or Map literal.
    fn collect_val_local_variables(&mut self, val: &Val) -> CompilerResult<()> {
        match val {
            Val::Vec(elements) => {
                for element in elements {
                    self.collect_val_local_variables(element)?;
                }
            }
            Val::Map(map) => {
                for value in map.values() {
                    self.collect_val_local_variables(value)?;
                }
            }
            Val::Box(boxed) => {
                if let Some(node) = boxed.as_any().downcast_ref::<AstNode>() {
                    self.collect_local_variables(&node.0)?;
                } else if let Some(fn_call) = boxed.as_any().downcast_ref::<FnCall>() {
                    for arg in &fn_call.args {
                        self.collect_local_variables(&arg.value)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Validate references in a Val (for nested structures)
    fn validate_val_references(&mut self, val: &Val) -> CompilerResult<()> {
        match val {
            Val::Vec(elements) => {
                for element in elements {
                    self.validate_val_references(element)?;
                }
            }
            Val::Map(map) => {
                for (key, value) in map.iter() {
                    self.validate_val_references(key)?;
                    self.validate_val_references(value)?;
                }
            }
            Val::Box(boxed) => {
                // The parser boxes each element of a literal that is not
                // itself a literal (`[f(x)]`, `{a: x}`, `[...xs]`).
                self.unchecked_before += 1;
                let result = if let Some(node) = boxed.as_any().downcast_ref::<AstNode>() {
                    self.validate_value_references(&node.0)
                } else if let Some(fn_call) = boxed.as_any().downcast_ref::<FnCall>() {
                    self.validate_function_call_references(fn_call)
                } else {
                    Ok(())
                };
                self.unchecked_before -= 1;
                result?;
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
// Meta references
// ============================================================================
//
// Meta is a literal, so a bare name in it (`clock-from: found-at`,
// `cites: [::text/s4]`, `agent: SupportAgent`) is kept as an unevaluated
// ref. Left as written, it would be looked up only when something reads the
// meta, from the reader's namespace. This pass resolves each name from the
// namespace that declares the meta and rewrites it to the qualified name of
// the definition it names: aliases are expanded and var-imports followed, so
// every spelling of one definition becomes the same `::ns/name` ref. A name
// that resolves to nothing is a compile error.

/// Top-level meta keys whose bare names must name types.
const META_TYPE_KEYS: [&str; 3] = ["agent", "workflow", "workflows"];

/// Var-import chains longer than this are treated as cycles.
const MAX_IMPORT_HOPS: usize = 32;

/// What a top-level definition is, as far as a meta reference cares.
enum MetaDecl {
    Type,
    /// A var-import (`s4 ::mp::text/s4`), with its target's qualified name.
    Import(String),
    Other,
}

/// Top-level definitions of the program by qualified name, plus the core
/// registry for auto-imported names.
struct MetaScope {
    decls: AHashMap<String, MetaDecl>,
    /// The variants of each closed enum by qualified name. Open enums can
    /// gain variants elsewhere, so they are not listed.
    closed_enum_variants: AHashMap<String, Vec<String>>,
    core: crate::lang::compiler::core_registry::CoreVariableRegistry,
}

impl MetaScope {
    fn new(
        program: &Program,
        core: crate::lang::compiler::core_registry::CoreVariableRegistry,
    ) -> Self {
        let mut decls = AHashMap::new();
        let mut closed_enum_variants: AHashMap<String, Vec<String>> = AHashMap::new();
        for (ns_path, namespace) in &program.namespaces {
            for (var, value) in &namespace.scope.vars {
                if var.deep_set.is_some() || var.deep_path.is_some() {
                    continue;
                }
                if let Value::TypeDef(type_def) = value
                    && !type_def.is_open
                    && let Some(variants) = &type_def.variants
                {
                    closed_enum_variants
                        .entry(format!("{}/{}", ns_path, var.sym.name()))
                        .or_default()
                        .extend(variants.iter().map(|v| v.name.name().to_string()));
                }
                let decl = if let Some(target) = Self::import_target(value, &namespace.aliases) {
                    MetaDecl::Import(target)
                } else if matches!(value, Value::TypeDef(_)) {
                    MetaDecl::Type
                } else {
                    MetaDecl::Other
                };
                // A name declared more than once (arities, open extensions)
                // is a type if any of its declarations is.
                let key = format!("{}/{}", ns_path, var.sym.name());
                match decls.get(&key) {
                    Some(MetaDecl::Type) => {}
                    Some(_) if !matches!(decl, MetaDecl::Type) => {}
                    _ => {
                        decls.insert(key, decl);
                    }
                }
            }
        }
        Self {
            decls,
            closed_enum_variants,
            core,
        }
    }

    /// The qualified target of a var-import: `local ::ns/name` parses either
    /// to the bare ref or to `[VarRef(local), Ref::Ns(target)]`.
    fn import_target(value: &Value, aliases: &NamespaceAliases) -> Option<String> {
        match value {
            Value::Ref(Ref::Ns(ns_ref)) => Self::qualify(ns_ref, aliases),
            Value::MultipleValues(values) => {
                let mut target = None;
                for value in values {
                    match value {
                        Value::Ref(Ref::Ns(ns_ref)) if target.is_none() => {
                            target = Some(Self::qualify(ns_ref, aliases)?);
                        }
                        Value::Ref(Ref::Var(_)) => {}
                        _ => return None,
                    }
                }
                target
            }
            _ => None,
        }
    }

    fn qualify(ns_ref: &crate::lang::ast::NsRef, aliases: &NamespaceAliases) -> Option<String> {
        let name = ns_ref.function_name.as_ref()?;
        let ns = aliases.get(&ns_ref.ns).unwrap_or(&ns_ref.ns);
        Some(format!("{}/{}", ns, name))
    }

    /// Follow var-imports to the definition they name.
    fn follow(&self, qualified: String) -> String {
        let mut current = qualified;
        for _ in 0..MAX_IMPORT_HOPS {
            match self.decls.get(&current) {
                Some(MetaDecl::Import(target)) if *target != current => current = target.clone(),
                _ => break,
            }
        }
        current
    }

    /// Resolve a bare name from `ns`: a top-level definition there, then a
    /// core name.
    fn resolve_bare(&self, ns: &str, name: &str) -> Option<String> {
        let local = format!("{}/{}", ns, name);
        if self.decls.contains_key(&local) {
            return Some(self.follow(local));
        }
        let core = self.core.get(name)?;
        let core_ns = if core.namespace_path.starts_with("::") {
            core.namespace_path.clone()
        } else {
            format!("::{}", core.namespace_path)
        };
        Some(self.follow(format!("{}/{}", core_ns, name)))
    }

    /// Whether `qualified` names something other than a type. Only known for
    /// definitions in this program and core names; anything else is assumed
    /// to be fine.
    fn is_known_non_type(&self, qualified: &str) -> bool {
        match self.decls.get(qualified) {
            Some(MetaDecl::Type) => false,
            Some(_) => true,
            None => qualified.rsplit_once('/').is_some_and(|(ns, name)| {
                self.core.get(name).is_some_and(|core| {
                    core.namespace_path.trim_start_matches("::") == ns.trim_start_matches("::")
                        && core.variable_type
                            != crate::lang::compiler::core_registry::CoreVariableType::TypeConstructor
                })
            }),
        }
    }
}

impl Resolver {
    /// Rewrite every name in top-level and namespace meta to the qualified
    /// name of the definition it names, reporting the ones that resolve to
    /// nothing. Meta on bindings inside function bodies is left alone: it is
    /// loaded in the body's own frame, where locals are in scope.
    fn resolve_meta_refs(&mut self, program: &mut Program) {
        let core = self.context.unified_resolver.core_variables.clone();
        let scope = MetaScope::new(program, core.unwrap_or_default());

        for (ns_path, namespace) in program.namespaces.iter_mut() {
            let ns = ns_path.to_string();
            self.context
                .unified_resolver
                .set_current_namespace(ns.clone());
            self.current_aliases = namespace.aliases.clone();

            if let Some(meta) = namespace.meta.take() {
                namespace.meta = Some(crate::lang::ast::Meta {
                    val: self.resolve_meta_val(&scope, &ns, &meta.val, true, false),
                });
            }

            if !namespace.scope.vars.keys().any(|var| var.meta.is_some()) {
                continue;
            }
            let vars = std::mem::take(&mut namespace.scope.vars);
            namespace.scope.vars = vars
                .into_iter()
                .map(|(mut var, value)| {
                    if let Some(meta) = var.meta.take() {
                        var.meta = Some(crate::lang::ast::Meta {
                            val: self.resolve_meta_val(&scope, &ns, &meta.val, true, false),
                        });
                    }
                    (var, value)
                })
                .collect();
        }
    }

    fn resolve_meta_val(
        &mut self,
        scope: &MetaScope,
        ns: &str,
        val: &crate::val::Val,
        top_level: bool,
        type_ref: bool,
    ) -> crate::val::Val {
        use crate::val::Val;
        match val {
            Val::Map(map) => Val::Map(Box::new(
                map.iter()
                    .map(|(key, value)| {
                        let type_ref = top_level
                            && matches!(key, Val::Str(k) if META_TYPE_KEYS.contains(&k.as_ref()));
                        (
                            key.clone(),
                            self.resolve_meta_val(scope, ns, value, false, type_ref),
                        )
                    })
                    .collect(),
            )),
            Val::Vec(items) => Val::Vec(
                items
                    .iter()
                    .map(|item| self.resolve_meta_val(scope, ns, item, false, type_ref))
                    .collect(),
            ),
            Val::Box(boxed) => match boxed.as_any().downcast_ref::<crate::lang::ast::AstNode>() {
                Some(crate::lang::ast::AstNode(Value::Ref(ref_val))) => self
                    .resolve_meta_ref(scope, ns, ref_val, type_ref)
                    .map(|resolved| Val::Box(Box::new(crate::lang::ast::AstNode(resolved))))
                    .unwrap_or_else(|| val.clone()),
                _ => val.clone(),
            },
            _ => val.clone(),
        }
    }

    /// The canonical ref for one meta name, or `None` (after reporting the
    /// error) when it does not resolve.
    fn resolve_meta_ref(
        &mut self,
        scope: &MetaScope,
        ns: &str,
        ref_val: &Ref,
        type_ref: bool,
    ) -> Option<Value> {
        let (qualified, src, written) = match ref_val {
            Ref::Var(var_ref) => {
                let name = var_ref.var.sym.name();
                let src = var_ref.src.as_ref().or(var_ref.var.src.as_ref());
                let Some(qualified) = scope.resolve_bare(ns, name) else {
                    self.report_unresolved_meta_name(name, src, type_ref);
                    return None;
                };
                // `Enum.Variant` names the variant's constructor, as compiled
                // code names it (`::ns/Enum.Variant`); a unit variant reads
                // as its value.
                if let Some(crate::lang::ast::DeepPath::Key(variant)) = &var_ref.var.deep_path
                    && matches!(scope.decls.get(&qualified), Some(MetaDecl::Type))
                    && let Some((target_ns, type_name)) = qualified.rsplit_once('/')
                {
                    if let Some(variants) = scope.closed_enum_variants.get(&qualified)
                        && !variants.contains(variant)
                    {
                        self.errors.add(CompilerError::UnresolvedVariable {
                            var_name: format!("{}.{}", name, variant),
                            namespace: ns.to_string(),
                            message: format!("'{}' has no variant '{}'", name, variant),
                            location: src.map(Self::src_location),
                        });
                        return None;
                    }
                    return Some(Value::Ref(Ref::Ns(crate::lang::ast::NsRef {
                        ns: crate::lang::ast::NsPath::from_string(target_ns),
                        src: src.cloned(),
                        function_name: Some(format!("{}.{}", type_name, variant)),
                    })));
                }
                if var_ref.var.deep_path.is_some() {
                    // `config.port`: keep the path, qualify the root.
                    let mut var_ref = var_ref.clone();
                    var_ref.var.sym = crate::lang::ast::Sym::String(qualified);
                    return Some(Value::Ref(Ref::Var(var_ref)));
                }
                (qualified, src, name.to_string())
            }
            Ref::Ns(ns_ref) => {
                let Some(qualified) = MetaScope::qualify(ns_ref, &self.current_aliases) else {
                    // A bare namespace (`::mp::text`): expand its alias.
                    let mut ns_ref = ns_ref.clone();
                    if let Some(target) = self.current_aliases.get(&ns_ref.ns) {
                        ns_ref.ns = target.clone();
                    }
                    return Some(Value::Ref(Ref::Ns(ns_ref)));
                };
                let errors_before = self.errors.errors.len();
                let _ = self.validate_ref_references(ref_val);
                if self.errors.errors.len() > errors_before {
                    return None;
                }
                let written = format!("{}/{}", ns_ref.ns, ns_ref.function_name.as_deref()?);
                (scope.follow(qualified), ns_ref.src.as_ref(), written)
            }
        };

        if type_ref && scope.is_known_non_type(&qualified) {
            self.errors.add(CompilerError::UnresolvedType {
                type_name: written.clone(),
                namespace: ns.to_string(),
                message: format!("'{}' is not a type", written),
                location: src.map(Self::src_location),
            });
            return None;
        }

        let (target_ns, name) = qualified.rsplit_once('/')?;
        Some(Value::Ref(Ref::Ns(crate::lang::ast::NsRef {
            ns: crate::lang::ast::NsPath::from_string(target_ns),
            src: src.cloned(),
            function_name: Some(name.to_string()),
        })))
    }

    fn report_unresolved_meta_name(
        &mut self,
        name: &str,
        src: Option<&crate::lang::ast::Source>,
        type_ref: bool,
    ) {
        let namespace = self.context.unified_resolver.current_namespace.clone();
        let location = src.map(Self::src_location);
        self.errors.add(if type_ref {
            CompilerError::UnresolvedType {
                type_name: name.to_string(),
                namespace,
                message: format!("Type '{}' is not defined", name),
                location,
            }
        } else {
            CompilerError::UnresolvedVariable {
                var_name: name.to_string(),
                namespace,
                message: format!("Variable '{}' cannot be resolved", name),
                location,
            }
        });
    }

    fn src_location(src: &crate::lang::ast::Source) -> ErrorLocation {
        ErrorLocation {
            line: src.line,
            column: src.column,
            position: src.position,
            length: src.length,
            file: src.file.as_ref().map(PathBuf::from),
        }
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
                tracing::debug!(
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
///
/// A reference with a deep path (`Shape.Circle`, `config.port`) is left for
/// the compiler, which resolves the import and keeps the path: a `Ref::Ns`
/// has no path, so rewriting it would drop the `.Circle`.
fn resolve_variable_to_namespace_function(
    var: &crate::lang::ast::Var,
    containing_ns: &crate::lang::ast::NsPath,
    namespace_data: &AHashMap<crate::lang::ast::NsPath, AHashMap<String, crate::lang::ast::Value>>,
) -> Option<crate::lang::ast::Value> {
    if var.deep_path.is_some() {
        return None;
    }
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
