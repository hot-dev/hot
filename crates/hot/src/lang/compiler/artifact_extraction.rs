//! Compiler artifact extraction methods.
//!
//! These methods scan a compiled `Program` AST and populate the
//! corresponding fields on `Compiler` (`event_handlers`,
//! `scheduled_functions`, `mcp_tools`, `webhooks`, `agents`,
//! `send_targets`) as well as expose the immutable read-side
//! getters (`get_event_handlers`, etc.).
//!
//! The artifact *value types* themselves live in [`super::artifacts`].
//! The orchestrator (`Compiler::compile_program`) calls these from the
//! engine/build layer after bytecode generation.

use super::Compiler;
use super::artifacts::{
    AgentDef, AgentDefs, EventHandler, EventHandlers, McpTool, McpTools, ScheduledFunction,
    ScheduledFunctions, SendTarget, SendTargetSource, SendTargets, Webhook, Webhooks, WorkflowDef,
    WorkflowDefs, auto_generate_mcp_tool_name, auto_generate_webhook_name, harvest_tool_meta,
    is_url_safe_service_name,
};
use crate::lang::ast::{FnCall, FnCallArg, Meta, Namespace, NsPath, Program, Ref, Value, Var};
use crate::lang::errors::{CompilerErrors, CompilerResult};
use crate::val::Val;

/// Maximum alias-chain depth we'll follow when resolving the target of a
/// reference-typed value (e.g. `aliased ::lib/handler` where `::lib/handler`
/// might itself be aliased again). Practically nobody chains aliases deeper
/// than a couple of hops; the cap is a defense-in-depth cycle guard.
const MAX_ALIAS_DEPTH: usize = 16;

/// Resolve a `Value::Ref` chain to the concrete callable (`Value::Fn` /
/// `Value::Lambda`) it ultimately points at, plus the `Var` that owns
/// that callable. Returns `None` if the value isn't a reference, the
/// chain dead-ends in something that isn't a function, or we hit the
/// depth cap.
///
/// Cross-namespace refs are matched after walking the containing
/// namespace's aliases (so `::tg-adapter/record-voice` ultimately
/// finds `::team-agent::telegram-adapter/record-voice`).
fn resolve_alias_target<'a>(
    program: &'a Program,
    current_ns: &str,
    value: &'a Value,
) -> Option<(&'a Var, &'a Value)> {
    let mut current = value;
    let mut current_ns_str = current_ns.to_string();

    for _ in 0..MAX_ALIAS_DEPTH {
        match current {
            Value::Ref(Ref::Ns(ns_ref)) => {
                let fn_name = ns_ref.function_name.as_deref()?;
                let resolved_ns_str =
                    resolve_ns_alias(program, &current_ns_str, &ns_ref.ns).to_string();
                let (_, namespace) = program
                    .namespaces
                    .iter()
                    .find(|(p, _)| p.to_string() == resolved_ns_str)?;
                let (var, val) = namespace
                    .scope
                    .vars
                    .iter()
                    .find(|(v, _)| v.sym.name() == fn_name)?;
                if matches!(val, Value::Fn(_) | Value::Lambda(_)) {
                    return Some((var, val));
                }
                current = val;
                current_ns_str = resolved_ns_str;
            }
            Value::Ref(Ref::Var(var_ref)) => {
                // Same-namespace alias, e.g. `aliased base-handler` when
                // both live in the same `ns`. Look up the symbol in the
                // current namespace's scope.
                let (_, namespace) = program
                    .namespaces
                    .iter()
                    .find(|(p, _)| p.to_string() == current_ns_str)?;
                let (var, val) = namespace
                    .scope
                    .vars
                    .iter()
                    .find(|(v, _)| v.sym.name() == var_ref.var.sym.name())?;
                if matches!(val, Value::Fn(_) | Value::Lambda(_)) {
                    return Some((var, val));
                }
                current = val;
            }
            _ => return None,
        }
    }
    None
}

/// Walk a `Value::Ref` chain and return the fully-qualified
/// `::namespace/function` name of the underlying callable (a `Value::Fn`
/// or `Value::Lambda`). Returns `None` if the value isn't a reference,
/// the chain dead-ends in something other than a function, or we hit
/// the depth cap.
///
/// Mirrors `resolve_alias_target` but yields the qualified-name string
/// rather than the AST node — handy when we need to look up the target
/// in registries keyed by qualified name (e.g. the compiler's
/// `function_mapping`).
pub(super) fn resolve_alias_target_qualified_name(
    program: &Program,
    current_ns: &str,
    value: &Value,
) -> Option<String> {
    let mut current = value;
    let mut current_ns_str = current_ns.to_string();

    for _ in 0..MAX_ALIAS_DEPTH {
        match current {
            Value::Ref(Ref::Ns(ns_ref)) => {
                let fn_name = ns_ref.function_name.as_deref()?;
                let resolved_ns_str =
                    resolve_ns_alias(program, &current_ns_str, &ns_ref.ns).to_string();
                let (_, namespace) = program
                    .namespaces
                    .iter()
                    .find(|(p, _)| p.to_string() == resolved_ns_str)?;
                let (_, val) = namespace
                    .scope
                    .vars
                    .iter()
                    .find(|(v, _)| v.sym.name() == fn_name)?;
                if matches!(val, Value::Fn(_) | Value::Lambda(_)) {
                    return Some(format!("{}/{}", resolved_ns_str, fn_name));
                }
                current = val;
                current_ns_str = resolved_ns_str;
            }
            Value::Ref(Ref::Var(var_ref)) => {
                let target_name = var_ref.var.sym.name();
                let (_, namespace) = program
                    .namespaces
                    .iter()
                    .find(|(p, _)| p.to_string() == current_ns_str)?;
                let (_, val) = namespace
                    .scope
                    .vars
                    .iter()
                    .find(|(v, _)| v.sym.name() == target_name)?;
                if matches!(val, Value::Fn(_) | Value::Lambda(_)) {
                    return Some(format!("{}/{}", current_ns_str, target_name));
                }
                current = val;
            }
            _ => return None,
        }
    }
    None
}

/// Resolve a namespace path through the containing namespace's alias
/// table: if `ns_path` is the LHS of an alias declaration in the
/// containing namespace, return the RHS (the real path); otherwise
/// return `ns_path` unchanged.
fn resolve_ns_alias(program: &Program, containing_ns: &str, ns_path: &NsPath) -> NsPath {
    if let Some((_, namespace)) = program
        .namespaces
        .iter()
        .find(|(p, _)| p.to_string() == containing_ns)
        && let Some(real_path) = namespace.aliases.get(ns_path)
    {
        return real_path.clone();
    }
    ns_path.clone()
}

/// Build the `Var` we should hand to `EventHandler::new` /
/// `ScheduledFunction::new` when the var is an alias to another
/// function. Returns a clone of the original var with `meta` replaced
/// by the target⊕alias merge (alias keys winning) so registered
/// artifacts carry library-supplied keys like `doc:` even when the
/// alias only declared agentic keys.
///
/// When the var isn't an alias (or the target has no meta to merge in)
/// we return a clone of the original var unchanged — same as the
/// pre-alias-meta behavior.
fn build_effective_var_for_alias(var: &Var, target: Option<(&Var, &Value)>) -> Var {
    let mut effective = var.clone();
    if let (Some((target_var, _)), Some(alias_meta)) = (target, &var.meta) {
        let target_meta_val = target_var.meta.as_ref().map(|m| &m.val);
        let merged = merge_alias_meta(target_meta_val, &alias_meta.val);
        effective.meta = Some(Meta { val: merged });
    }
    effective
}

/// Merge alias meta on top of target meta; alias keys win on collision.
/// Used so `tg-handler meta {on-event:"x"} ::lib/handler` ends up with
/// the library's `doc:` etc. preserved while the alias's agentic keys
/// drive registration. Mirrors `merge_trailing_meta`'s semantics from
/// the parser.
fn merge_alias_meta(target_meta: Option<&Val>, alias_meta: &Val) -> Val {
    match (target_meta, alias_meta) {
        (Some(Val::Map(target_map)), Val::Map(alias_map)) => {
            let mut merged = (**target_map).clone();
            for (k, v) in alias_map.iter() {
                merged.insert(k.clone(), v.clone());
            }
            Val::Map(Box::new(merged))
        }
        // Target has no map meta (e.g. None or non-Map): just use the
        // alias meta as-is.
        _ => alias_meta.clone(),
    }
}

impl Compiler {
    /// Extract event handlers from the compiled program
    /// This should be called after compilation is complete
    /// Returns an error if any event handlers have invalid signatures
    pub fn extract_event_handlers(&mut self, program: &Program) -> CompilerResult<()> {
        self.event_handlers.clear();
        self.validation_errors.clear(); // Clear at the start of extraction

        tracing::trace!(
            "Extracting event handlers from {} namespaces",
            program.namespaces.len()
        );

        // Iterate through all namespaces in the program
        for (ns_path, namespace) in &program.namespaces {
            tracing::trace!("Scanning namespace: {}", ns_path.to_string());
            self.scan_namespace_for_event_handlers(&ns_path.to_string(), namespace, program);
        }

        tracing::trace!("Found {} event handler types", self.event_handlers.len());

        // Return validation errors if any were collected
        if !self.validation_errors.is_empty() {
            let mut errors = CompilerErrors::new();

            // Add source files for pretty error reporting
            for (path, content) in &self.file_contents {
                errors.add_source(path.display().to_string(), content.clone());
            }

            for error in self.validation_errors.drain(..) {
                errors.add(error);
            }

            return Err(errors);
        }

        Ok(())
    }

    /// Extract scheduled functions from the compiled program
    /// This should be called after compilation is complete
    /// Returns an error if any scheduled functions have invalid signatures
    pub fn extract_scheduled_functions(&mut self, program: &Program) -> CompilerResult<()> {
        self.scheduled_functions.clear();
        // Note: validation_errors is NOT cleared here to accumulate errors from event handlers

        tracing::trace!(
            "Extracting scheduled functions from {} namespaces",
            program.namespaces.len()
        );

        // Iterate through all namespaces in the program
        for (ns_path, namespace) in &program.namespaces {
            tracing::trace!(
                "Scanning namespace for scheduled functions: {}",
                ns_path.to_string()
            );
            self.scan_namespace_for_scheduled_functions(&ns_path.to_string(), namespace, program);
        }

        tracing::trace!(
            "Found {} scheduled function types",
            self.scheduled_functions.len()
        );

        // Return validation errors if any were collected
        if !self.validation_errors.is_empty() {
            let mut errors = CompilerErrors::new();

            // Add source files for pretty error reporting
            for (path, content) in &self.file_contents {
                errors.add_source(path.display().to_string(), content.clone());
            }

            for error in self.validation_errors.drain(..) {
                errors.add(error);
            }

            return Err(errors);
        }

        Ok(())
    }

    /// Get the extracted event handlers
    pub fn get_event_handlers(&self) -> &EventHandlers {
        &self.event_handlers
    }

    /// Get the extracted scheduled functions
    pub fn get_scheduled_functions(&self) -> &ScheduledFunctions {
        &self.scheduled_functions
    }

    /// Get the extracted MCP tools
    pub fn get_mcp_tools(&self) -> &McpTools {
        &self.mcp_tools
    }

    /// Populate the global tool-spec registry used by
    /// `::hot::internal::mcp/schema-from-fn` so that any Hot function
    /// with typed parameters (or an annotated return type) can be
    /// resolved to its `{input-schema, output-schema, description?,
    /// display-name?}` map at runtime.
    ///
    /// This walks every namespace and emits one entry per function
    /// definition, keyed by its fully qualified name (`::ns/name`).
    /// Description chain (first non-empty wins):
    ///   `meta {tool: {description: ...}}`
    ///   -> `meta {mcp:  {description: ...}}`
    ///   -> `meta {doc:  ...}`
    /// Display-name chain:
    ///   `meta {tool: {name: ...}}`
    ///   -> `meta {mcp:  {name: ...}}`
    pub fn extract_tool_specs(&self, program: &Program) -> CompilerResult<()> {
        let registry = self.build_tool_specs(program);
        tracing::trace!(
            "Installing tool-spec registry with {} entries",
            registry.entries.len()
        );
        crate::lang::hot::internal_mcp::set_registry(registry);
        Ok(())
    }

    /// Build the tool-spec registry from `program` without installing
    /// it into the global registry. Used by cache write paths so the
    /// registry can be persisted alongside the bytecode and rehydrated
    /// when the cache is loaded (including in fresh worker processes
    /// and zip-build deployments).
    pub fn build_tool_specs(
        &self,
        program: &Program,
    ) -> crate::lang::hot::internal_mcp::ToolSpecRegistry {
        let type_registry = self.build_type_registry(program);
        let mut registry = crate::lang::hot::internal_mcp::ToolSpecRegistry::default();

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                // Resolve the function we should derive the schema from:
                // either the var's own `Value::Fn` or, if it's an alias
                // (`Value::Ref` chain), the ultimate target's `Value::Fn`.
                // We register an entry under the *alias's* fully-qualified
                // name so callers like `::hot::internal::mcp/schema-from-fn`
                // can look it up by the same name they call.
                let (effective_var, fn_defs) = match value {
                    Value::Fn(fn_defs) => (var.clone(), fn_defs),
                    Value::Ref(_) => {
                        let Some((target_var, target_value)) =
                            resolve_alias_target(program, &ns_str, value)
                        else {
                            continue;
                        };
                        let Value::Fn(fn_defs) = target_value else {
                            continue;
                        };
                        let effective =
                            build_effective_var_for_alias(var, Some((target_var, target_value)));
                        (effective, fn_defs)
                    }
                    _ => continue,
                };
                let Some(fn_def) = fn_defs.first() else {
                    continue;
                };
                let var_name = var.sym.to_string();
                let fq_name = format!("{}/{}", ns_str, var_name);

                let input_schema = crate::lang::json_schema::args_to_input_schema_with_registry(
                    &fn_def.args.args,
                    &type_registry,
                );
                let output_schema =
                    crate::lang::json_schema::return_type_to_output_schema_with_registry(
                        fn_def.return_type.as_deref(),
                        &type_registry,
                    );

                let (display_name, description) = harvest_tool_meta(&effective_var);

                registry.entries.insert(
                    fq_name.clone(),
                    crate::lang::hot::internal_mcp::ToolSpecEntry {
                        name: fq_name,
                        input_schema,
                        output_schema,
                        description,
                        display_name,
                    },
                );
            }
        }

        registry
    }

    /// Back-compat alias for `extract_tool_specs`. New code should call
    /// the generalized name; this thin wrapper keeps existing call
    /// sites compiling during the rename window.
    pub fn extract_tool_schemas(&self, program: &Program) -> CompilerResult<()> {
        self.extract_tool_specs(program)
    }

    /// Populate the global skill-spec registry consumed by
    /// `::hot::internal::skill/meta-from-fn` and `::ai::skill/from-fn`.
    ///
    /// Walks every namespace and emits one entry per Hot function that
    /// carries a `meta {skill: {...}}` annotation. The raw skill map
    /// is stashed as-is so consumers can decide how to interpret
    /// fields like `description`, `when`, `body`, `body-fn`, `tools`,
    /// and `requires` without coupling the compiler to the runtime
    /// `Skill` shape.
    pub fn extract_skill_specs(&self, program: &Program) -> CompilerResult<()> {
        let registry = self.build_skill_specs(program);
        tracing::trace!(
            "Installing skill-spec registry with {} entries",
            registry.entries.len()
        );
        crate::lang::hot::internal_skill::set_registry(registry);
        Ok(())
    }

    /// Build the skill-spec registry from `program` without installing
    /// it into the global registry. Persisted alongside cached
    /// bytecode and rehydrated on cache load (see
    /// `build_tool_specs` for the rationale).
    pub fn build_skill_specs(
        &self,
        program: &Program,
    ) -> crate::lang::hot::internal_skill::SkillSpecRegistry {
        let mut registry = crate::lang::hot::internal_skill::SkillSpecRegistry::default();

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                // Accept both a direct function definition and an alias
                // chain that ultimately resolves to one. For aliases we
                // pull `meta.skill` from the *merged* meta (alias keys
                // win), which lets a wrapper either inherit the
                // library's skill metadata or override individual fields
                // (e.g. `description`, `when`).
                let effective_var = match value {
                    Value::Fn(_) => var.clone(),
                    Value::Ref(_) => {
                        let Some((target_var, target_value)) =
                            resolve_alias_target(program, &ns_str, value)
                        else {
                            continue;
                        };
                        if !matches!(target_value, Value::Fn(_)) {
                            continue;
                        }
                        build_effective_var_for_alias(var, Some((target_var, target_value)))
                    }
                    _ => continue,
                };
                let Some(meta) = effective_var.meta.as_ref() else {
                    continue;
                };
                let Val::Map(meta_map) = &meta.val else {
                    continue;
                };
                let Some(skill_val) = meta_map.get(&Val::from("skill")) else {
                    continue;
                };
                if !matches!(skill_val, Val::Map(_)) {
                    continue;
                }

                let var_name = var.sym.to_string();
                let fq_name = format!("{}/{}", ns_str, var_name);
                registry.entries.insert(
                    fq_name.clone(),
                    crate::lang::hot::internal_skill::SkillSpecEntry {
                        name: fq_name,
                        skill_meta: skill_val.clone(),
                    },
                );
            }
        }

        registry
    }

    /// Extract MCP tools from the compiled program
    /// This should be called after compilation is complete
    pub fn extract_mcp_tools(&mut self, program: &Program) -> CompilerResult<()> {
        self.mcp_tools.clear();

        tracing::trace!(
            "Extracting MCP tools from {} namespaces",
            program.namespaces.len()
        );

        // Build a type registry from all type definitions in the program
        let type_registry = self.build_type_registry(program);
        tracing::trace!("Built type registry with {} types", type_registry.len());

        // Iterate through all namespaces in the program
        for (ns_path, namespace) in &program.namespaces {
            tracing::trace!("Scanning namespace for MCP tools: {}", ns_path.to_string());
            self.scan_namespace_for_mcp_tools(
                &ns_path.to_string(),
                namespace,
                &type_registry,
                program,
            );
        }

        tracing::trace!("Found {} MCP services", self.mcp_tools.len());

        Ok(())
    }

    /// Build a type registry from all type definitions in the program
    fn build_type_registry(&self, program: &Program) -> crate::lang::json_schema::TypeRegistry {
        let mut registry = crate::lang::json_schema::TypeRegistry::new();

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();

            for (var, value) in &namespace.scope.vars {
                if let Value::TypeDef(type_def) = value {
                    let type_name = var.sym.name();
                    let qualified_name = format!("{}/{}", ns_str, type_name);

                    // Register with both simple and qualified names
                    registry.register_qualified(&qualified_name, type_def);

                    tracing::trace!(
                        "Registered type {} ({}) with {} fields",
                        type_name,
                        qualified_name,
                        type_def.fields.as_ref().map(|f| f.len()).unwrap_or(0)
                    );
                }
            }
        }

        registry
    }

    /// Scan a single namespace for MCP tools
    fn scan_namespace_for_mcp_tools(
        &mut self,
        ns_name: &str,
        namespace: &Namespace,
        type_registry: &crate::lang::json_schema::TypeRegistry,
        program: &Program,
    ) {
        tracing::trace!(
            "Scanning namespace {} for MCP tools with {} variables",
            ns_name,
            namespace.scope.vars.len()
        );

        // Scan all variables in the namespace scope
        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.to_string();
            tracing::trace!(
                "Checking variable for mcp: {} (has meta: {})",
                var_name,
                var.meta.is_some()
            );

            // For aliases (`Value::Ref`), resolve the chain so we can
            // (a) auto-derive the schema from the underlying function's
            // signature and (b) merge the target's `doc:` etc. into the
            // alias's meta before extracting the `mcp:` config. This
            // mirrors the pattern used for event handlers / schedules /
            // webhooks below.
            let alias_target = if matches!(value, Value::Ref(_)) {
                resolve_alias_target(program, ns_name, value)
            } else {
                None
            };
            let effective_var = if alias_target.is_some() {
                build_effective_var_for_alias(var, alias_target)
            } else {
                var.clone()
            };
            // The value used for schema generation: for aliases, this is
            // the resolved target's `Value::Fn`. Otherwise, the var's
            // own value.
            let schema_value: &Value = match alias_target {
                Some((_, target_value)) => target_value,
                None => value,
            };

            if let Some(mcp_config) = self.extract_mcp_from_var(&effective_var) {
                // Extract service (required)
                let service = match mcp_config.get(&Val::from("service")) {
                    Some(Val::Str(s)) => (**s).to_string(),
                    _ => {
                        tracing::warn!(
                            "MCP tool {} missing required 'service' field, skipping",
                            var_name
                        );
                        continue;
                    }
                };

                // Validate service name is URL-safe
                if !is_url_safe_service_name(&service) {
                    tracing::warn!(
                        "MCP tool {} has invalid service name '{}': must be URL-safe (alphanumeric, hyphens, underscores, dots). Skipping.",
                        var_name,
                        service
                    );
                    continue;
                }

                // Extract or auto-generate tool name
                let name = match mcp_config.get(&Val::from("name")) {
                    Some(Val::Str(s)) => (**s).to_string(),
                    _ => auto_generate_mcp_tool_name(ns_name, &var_name),
                };

                tracing::trace!(
                    "Found MCP tool: {} (service: {}, name: {})",
                    var_name,
                    service,
                    name
                );

                // Extract optional fields
                // description: explicit mcp description, falls back to top-level doc meta
                // (read from the *effective* var so aliases inherit
                // the library function's doc string).
                let description = match mcp_config.get(&Val::from("description")) {
                    Some(Val::Str(s)) => Some(s.trim().to_string()),
                    _ => effective_var.meta.as_ref().and_then(|m| {
                        if let Val::Map(meta_map) = &m.val
                            && let Some(Val::Str(doc)) = meta_map.get(&Val::from("doc"))
                        {
                            return Some(doc.trim().to_string());
                        }
                        None
                    }),
                };

                let title = match mcp_config.get(&Val::from("title")) {
                    Some(Val::Str(s)) => Some(s.trim().to_string()),
                    _ => None,
                };

                // Check for explicitly provided schemas
                let explicit_input_schema = mcp_config
                    .get(&Val::from("input-schema"))
                    .cloned()
                    .or_else(|| mcp_config.get(&Val::from("inputSchema")).cloned());

                let explicit_output_schema = mcp_config
                    .get(&Val::from("output-schema"))
                    .cloned()
                    .or_else(|| mcp_config.get(&Val::from("outputSchema")).cloned());

                // Auto-generate schemas from function signature if not explicitly provided
                // Uses the type registry to resolve custom types.
                // For aliases, `schema_value` points at the resolved
                // target's `Value::Fn` so the alias gets a real schema
                // even though its own value is a `Value::Ref`.
                let (input_schema, output_schema) = if let Value::Fn(fn_defs) = schema_value {
                    if let Some(fn_def) = fn_defs.first() {
                        let auto_input = if explicit_input_schema.is_none() {
                            Some(
                                crate::lang::json_schema::args_to_input_schema_with_registry(
                                    &fn_def.args.args,
                                    type_registry,
                                ),
                            )
                        } else {
                            explicit_input_schema
                        };

                        let auto_output = if explicit_output_schema.is_none() {
                            crate::lang::json_schema::return_type_to_output_schema_with_registry(
                                fn_def.return_type.as_deref(),
                                type_registry,
                            )
                        } else {
                            explicit_output_schema
                        };

                        (auto_input, auto_output)
                    } else {
                        (explicit_input_schema, explicit_output_schema)
                    }
                } else {
                    (explicit_input_schema, explicit_output_schema)
                };

                let icons = mcp_config.get(&Val::from("icons")).cloned();
                let annotations = mcp_config.get(&Val::from("annotations")).cloned();

                // Extract auth mode (default: "required")
                let auth_mode = match mcp_config.get(&Val::from("auth")) {
                    Some(Val::Str(s)) => {
                        let mode = s.to_string();
                        if mode != "required" && mode != "none" {
                            tracing::warn!(
                                "MCP tool {} has invalid auth mode '{}': must be 'required' or 'none'. Defaulting to 'required'.",
                                var_name,
                                mode
                            );
                            "required".to_string()
                        } else {
                            mode
                        }
                    }
                    _ => "required".to_string(),
                };

                // Create the MCP tool. We pass the *effective* var so
                // any source-location / meta lookups downstream see the
                // alias's location (correct: that's where the wiring
                // lives) plus the merged meta (target keys filled in).
                let mcp_tool = McpTool::new(
                    service.clone(),
                    name.clone(),
                    auth_mode,
                    ns_name,
                    &var_name,
                    &effective_var,
                    description,
                    title,
                    input_schema,
                    output_schema,
                    icons,
                    annotations,
                );

                // Add to tools collection, avoiding duplicates
                let fn_name = format!("{}/{}", ns_name, var_name);
                let tools = self.mcp_tools.entry(service).or_default();
                let is_duplicate = tools.iter().any(|t| t.mcp_tool.get_str("fn") == fn_name);
                if !is_duplicate {
                    tools.push(mcp_tool);
                } else {
                    tracing::debug!("Skipping duplicate MCP tool: {}", fn_name);
                }
            }
        }
    }

    /// Extract MCP configuration from a variable's metadata if it's an MCP tool
    fn extract_mcp_from_var(&self, var: &Var) -> Option<indexmap::IndexMap<Val, Val>> {
        // Check if the variable has metadata
        let meta = var.meta.as_ref()?;

        tracing::trace!(
            "Checking mcp metadata for {}: {:?}",
            var.sym.to_string(),
            meta.val
        );

        // Handle both Map and JSON string formats
        match &meta.val {
            Val::Map(meta_map) => {
                // Check for "mcp" key
                match meta_map.get(&Val::from("mcp")) {
                    Some(Val::Bool(true)) => {
                        // mcp: true - return empty config (will use defaults)
                        // But we need service, so this is actually invalid without service
                        tracing::trace!(
                            "Found mcp: true for {} but missing service",
                            var.sym.to_string()
                        );
                        None
                    }
                    Some(Val::Map(mcp_config)) => {
                        // mcp: { service: "...", ... }
                        tracing::trace!(
                            "Found MCP tool config for {}: {:?}",
                            var.sym.to_string(),
                            mcp_config
                        );
                        Some(mcp_config.as_ref().clone())
                    }
                    _ => None,
                }
            }
            Val::Str(json_str) => {
                // JSON string format - parse it
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(mcp_value) = parsed.get("mcp")
                    && mcp_value.is_object()
                {
                    // Convert JSON object to Val, then extract the map
                    if let Ok(val) = serde_json::from_value::<Val>(mcp_value.clone())
                        && let Val::Map(mcp_map) = val
                    {
                        return Some(mcp_map.as_ref().clone());
                    }
                }
                None
            }
            _ => None,
        }
    }

    // ========================================================================
    // Webhook extraction
    // ========================================================================

    /// Get the extracted webhooks
    pub fn get_webhooks(&self) -> &Webhooks {
        &self.webhooks
    }

    /// Extract webhooks from the compiled program
    /// This should be called after compilation is complete
    pub fn extract_webhooks(&mut self, program: &Program) -> CompilerResult<()> {
        self.webhooks.clear();

        tracing::trace!(
            "Extracting webhooks from {} namespaces",
            program.namespaces.len()
        );

        for (ns_path, namespace) in &program.namespaces {
            tracing::trace!("Scanning namespace for webhooks: {}", ns_path.to_string());
            self.scan_namespace_for_webhooks(&ns_path.to_string(), namespace, program);
        }

        tracing::trace!("Found {} webhook services", self.webhooks.len());

        Ok(())
    }

    /// Scan a single namespace for webhooks
    fn scan_namespace_for_webhooks(
        &mut self,
        ns_name: &str,
        namespace: &Namespace,
        program: &Program,
    ) {
        tracing::trace!(
            "Scanning namespace {} for webhooks with {} variables",
            ns_name,
            namespace.scope.vars.len()
        );

        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.to_string();

            if let Some(webhook_config) = self.extract_webhook_from_var(var) {
                // Resolve through alias chains so a `tg-on-update
                // ::tg-adapter/on-telegram-update` form ends up
                // registering with merged meta (library `doc:` + alias
                // `webhook:` config).
                let target = resolve_alias_target(program, ns_name, value);
                let effective_var = build_effective_var_for_alias(var, target);
                let webhook_var = if target.is_some() {
                    &effective_var
                } else {
                    var
                };
                // Extract service (required)
                let service = match webhook_config.get(&Val::from("service")) {
                    Some(Val::Str(s)) => (**s).to_string(),
                    _ => {
                        tracing::warn!(
                            "Webhook {} missing required 'service' field, skipping",
                            var_name
                        );
                        continue;
                    }
                };

                // Validate service name is URL-safe
                if !is_url_safe_service_name(&service) {
                    tracing::warn!(
                        "Webhook {} has invalid service name '{}': must be URL-safe (alphanumeric, hyphens, underscores, dots). Skipping.",
                        var_name,
                        service
                    );
                    continue;
                }

                // Extract path (required)
                let path = match webhook_config.get(&Val::from("path")) {
                    Some(Val::Str(s)) => (**s).to_string(),
                    _ => {
                        tracing::warn!(
                            "Webhook {} missing required 'path' field, skipping",
                            var_name
                        );
                        continue;
                    }
                };

                // Extract method (optional, default: "POST")
                let method = match webhook_config.get(&Val::from("method")) {
                    Some(Val::Str(s)) => (**s).to_uppercase(),
                    _ => "POST".to_string(),
                };

                // Extract or auto-generate endpoint name
                let name = match webhook_config.get(&Val::from("name")) {
                    Some(Val::Str(s)) => (**s).to_string(),
                    _ => auto_generate_webhook_name(ns_name, &var_name),
                };

                tracing::trace!(
                    "Found webhook: {} (service: {}, path: {}, method: {})",
                    var_name,
                    service,
                    path,
                    method
                );

                // Extract optional fields
                // description: explicit webhook description, falls back to top-level doc meta
                let description = match webhook_config.get(&Val::from("description")) {
                    Some(Val::Str(s)) => Some(s.trim().to_string()),
                    _ => {
                        // Fall back to top-level doc meta (via the
                        // effective var so library `doc:` shows through
                        // when this is an alias).
                        webhook_var.meta.as_ref().and_then(|m| {
                            if let Val::Map(meta_map) = &m.val
                                && let Some(Val::Str(doc)) = meta_map.get(&Val::from("doc"))
                            {
                                return Some(doc.trim().to_string());
                            }
                            None
                        })
                    }
                };

                let auth_mode = match webhook_config.get(&Val::from("auth")) {
                    Some(Val::Str(s)) => Some((**s).to_string()),
                    _ => None,
                };

                // Create the webhook
                let webhook = Webhook::new(
                    service.clone(),
                    path,
                    method,
                    name,
                    ns_name,
                    &var_name,
                    webhook_var,
                    description,
                    auth_mode,
                );

                // Add to webhooks collection, avoiding duplicates
                let fn_name = format!("{}/{}", ns_name, var_name);
                let entries = self.webhooks.entry(service).or_default();
                let is_duplicate = entries.iter().any(|e| e.webhook.get_str("fn") == fn_name);
                if !is_duplicate {
                    entries.push(webhook);
                } else {
                    tracing::debug!("Skipping duplicate webhook: {}", fn_name);
                }
            }
        }
    }

    /// Extract webhook configuration from a variable's metadata
    fn extract_webhook_from_var(&self, var: &Var) -> Option<indexmap::IndexMap<Val, Val>> {
        let meta = var.meta.as_ref()?;

        // Handle both Map and JSON string formats
        match &meta.val {
            Val::Map(meta_map) => match meta_map.get(&Val::from("webhook")) {
                Some(Val::Map(webhook_config)) => Some(webhook_config.as_ref().clone()),
                _ => None,
            },
            Val::Str(json_str) => {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(webhook_value) = parsed.get("webhook")
                    && webhook_value.is_object()
                    && let Ok(val) = serde_json::from_value::<Val>(webhook_value.clone())
                    && let Val::Map(webhook_map) = val
                {
                    return Some(webhook_map.as_ref().clone());
                }
                None
            }
            _ => None,
        }
    }

    /// Get the extracted agent definitions
    pub fn get_agents(&self) -> &AgentDefs {
        &self.agents
    }

    /// Extract agent type definitions from the compiled program.
    /// Scans for types with `meta {agent: {...}}` — the `agent` key contains a map
    /// of agent-specific config (name, description, tags).
    pub fn extract_agents(&mut self, program: &Program) -> CompilerResult<()> {
        self.agents.clear();

        tracing::trace!(
            "Extracting agents from {} namespaces",
            program.namespaces.len()
        );

        for (ns_path, namespace) in &program.namespaces {
            let ns_name = ns_path.to_string();
            tracing::trace!("Scanning namespace for agents: {}", ns_name);
            self.scan_namespace_for_agents(&ns_name, namespace);
        }

        tracing::trace!("Found {} agent type definitions", self.agents.len());

        Ok(())
    }

    /// Scan a single namespace for agent type definitions.
    /// An agent is a type with `meta {agent: {name: "...", ...}}`.
    fn scan_namespace_for_agents(&mut self, ns_name: &str, namespace: &Namespace) {
        for (var, value) in &namespace.scope.vars {
            // Only consider type definitions
            let type_def = match value {
                crate::lang::ast::Value::TypeDef(td) => td,
                _ => continue,
            };

            let type_name = var.sym.to_string();

            let agent_meta_map = Self::extract_agent_meta_from_var(var);

            if let Some(agent_config) = agent_meta_map {
                tracing::trace!(
                    "Found agent type: {}/{} with config keys: {:?}",
                    ns_name,
                    type_name,
                    agent_config.keys().collect::<Vec<_>>()
                );

                let config_fields = type_def.fields.as_deref();
                let agent_def = AgentDef::new(
                    type_name.clone(),
                    ns_name,
                    var,
                    &agent_config,
                    config_fields,
                );

                // Avoid duplicates by qualified name
                let qualified = format!("{}/{}", ns_name, type_name);
                let is_duplicate = self
                    .agents
                    .iter()
                    .any(|a| format!("{}/{}", a.namespace, a.type_name) == qualified);
                if !is_duplicate {
                    self.agents.push(agent_def);
                } else {
                    tracing::debug!("Skipping duplicate agent type: {}", qualified);
                }
            }
        }
    }

    /// Extract `agent` config map from the Var's meta.
    /// Returns `Some(map)` when `meta.agent` is a Map (agent type definition).
    fn extract_agent_meta_from_var(var: &Var) -> Option<indexmap::IndexMap<Val, Val>> {
        let meta = var.meta.as_ref()?;
        Self::extract_agent_config_from_meta_val(&meta.val)
    }

    /// Given a meta Val, extract the `agent` key if it's a Map (agent config).
    fn extract_agent_config_from_meta_val(meta_val: &Val) -> Option<indexmap::IndexMap<Val, Val>> {
        match meta_val {
            Val::Map(meta_map) => match meta_map.get(&Val::from("agent")) {
                Some(Val::Map(agent_config)) => Some(agent_config.as_ref().clone()),
                _ => None,
            },
            Val::Str(json_str) => {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(agent_value) = parsed.get("agent")
                    && agent_value.is_object()
                    && let Ok(val) = serde_json::from_value::<Val>(agent_value.clone())
                    && let Val::Map(agent_map) = val
                {
                    return Some(agent_map.as_ref().clone());
                }
                None
            }
            _ => None,
        }
    }

    /// Get the extracted named workflow definitions.
    pub fn get_workflows(&self) -> &WorkflowDefs {
        &self.workflows
    }

    /// Extract named workflow type definitions from the compiled program.
    /// Scans for types with `meta {workflow: {...}}` — the `workflow` key contains
    /// a map of workflow-specific config (name, description, tags).
    pub fn extract_workflows(&mut self, program: &Program) -> CompilerResult<()> {
        self.workflows.clear();

        tracing::trace!(
            "Extracting workflows from {} namespaces",
            program.namespaces.len()
        );

        for (ns_path, namespace) in &program.namespaces {
            let ns_name = ns_path.to_string();
            tracing::trace!("Scanning namespace for workflows: {}", ns_name);
            self.scan_namespace_for_workflows(&ns_name, namespace);
        }

        tracing::trace!("Found {} workflow type definitions", self.workflows.len());

        Ok(())
    }

    /// Scan a single namespace for named workflow type definitions.
    /// A named workflow is a type with `meta {workflow: {name: "...", ...}}`.
    fn scan_namespace_for_workflows(&mut self, ns_name: &str, namespace: &Namespace) {
        for (var, value) in &namespace.scope.vars {
            // Only consider type definitions.
            match value {
                crate::lang::ast::Value::TypeDef(_) => {}
                _ => continue,
            }

            let type_name = var.sym.to_string();

            if let Some(workflow_config) = Self::extract_workflow_meta_from_var(var) {
                tracing::trace!(
                    "Found workflow type: {}/{} with config keys: {:?}",
                    ns_name,
                    type_name,
                    workflow_config.keys().collect::<Vec<_>>()
                );

                let workflow_def =
                    WorkflowDef::new(type_name.clone(), ns_name, var, &workflow_config);

                // Avoid duplicates by qualified name.
                let qualified = format!("{}/{}", ns_name, type_name);
                let is_duplicate = self
                    .workflows
                    .iter()
                    .any(|w| format!("{}/{}", w.namespace, w.type_name) == qualified);
                if !is_duplicate {
                    self.workflows.push(workflow_def);
                } else {
                    tracing::debug!("Skipping duplicate workflow type: {}", qualified);
                }
            }
        }
    }

    /// Extract `workflow` config map from the Var's meta.
    /// Returns `Some(map)` when `meta.workflow` is a Map (workflow type definition).
    fn extract_workflow_meta_from_var(var: &Var) -> Option<indexmap::IndexMap<Val, Val>> {
        let meta = var.meta.as_ref()?;
        Self::extract_workflow_config_from_meta_val(&meta.val)
    }

    /// Given a meta Val, extract the `workflow` key if it's a Map (workflow config).
    fn extract_workflow_config_from_meta_val(
        meta_val: &Val,
    ) -> Option<indexmap::IndexMap<Val, Val>> {
        match meta_val {
            Val::Map(meta_map) => match meta_map.get(&Val::from("workflow")) {
                Some(Val::Map(workflow_config)) => Some(workflow_config.as_ref().clone()),
                _ => None,
            },
            Val::Str(json_str) => {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(workflow_value) = parsed.get("workflow")
                    && workflow_value.is_object()
                    && let Ok(val) = serde_json::from_value::<Val>(workflow_value.clone())
                    && let Val::Map(workflow_map) = val
                {
                    return Some(workflow_map.as_ref().clone());
                }
                None
            }
            _ => None,
        }
    }

    // ========================================================================
    // Send target extraction (static `send()` detection)
    // ========================================================================

    /// Get the extracted send targets
    pub fn get_send_targets(&self) -> &SendTargets {
        &self.send_targets
    }

    /// Extract send targets from the compiled program.
    /// Walks all namespace vars to find `send()` calls with resolvable event names.
    pub fn extract_send_targets(&mut self, program: &Program) -> CompilerResult<()> {
        self.send_targets.clear();

        tracing::trace!(
            "Extracting send targets from {} namespaces",
            program.namespaces.len()
        );

        for (ns_path, namespace) in &program.namespaces {
            let ns_name = ns_path.to_string();
            self.scan_namespace_for_send_targets(&ns_name, namespace, program);
        }

        // Second pass: for every alias-typed var, attribute the
        // resolved target's send targets to the alias so the agent
        // graph (which keys on the alias's `ns/var`) shows the same
        // outbound edges as the underlying library function. Without
        // this aliases would render as orphan nodes with no `sends`.
        self.attribute_alias_send_targets(program);

        if self.send_targets.is_empty() {
            tracing::trace!(
                "No send targets found across {} namespaces",
                program.namespaces.len()
            );
        } else {
            for (fn_key, targets) in &self.send_targets {
                let events: Vec<&str> = targets.iter().map(|t| t.event_name.as_str()).collect();
                tracing::debug!("Send targets: {} -> {:?}", fn_key, events);
            }
        }

        Ok(())
    }

    /// For every alias var (`Value::Ref` chain ending in a `Value::Fn`),
    /// copy the target's send-target entries to the alias's key. The
    /// target's events become attributed to the alias, but each
    /// `SendTarget`'s `namespace`/`var_name` are rewritten to the alias
    /// so downstream graph code keys edges against the alias node.
    fn attribute_alias_send_targets(&mut self, program: &Program) {
        let mut additions: Vec<(String, Vec<SendTarget>)> = Vec::new();
        for (ns_path, namespace) in &program.namespaces {
            let ns_name = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                if !matches!(value, Value::Ref(_)) {
                    continue;
                }
                let Some((target_var, _)) = resolve_alias_target(program, &ns_name, value) else {
                    continue;
                };

                // Find the target's fully-qualified key by locating
                // which namespace the target var lives in (alias chains
                // can cross namespaces).
                let target_key = program.namespaces.iter().find_map(|(p, ns)| {
                    ns.scope
                        .vars
                        .iter()
                        .find(|(v, _)| std::ptr::eq(*v, target_var))
                        .map(|(v, _)| format!("{}/{}", p, v.sym.name()))
                });
                let Some(target_key) = target_key else {
                    continue;
                };

                let Some(target_targets) = self.send_targets.get(&target_key) else {
                    continue;
                };

                let alias_var_name = var.sym.to_string();
                let alias_key = format!("{}/{}", ns_name, alias_var_name);
                let rewritten: Vec<SendTarget> = target_targets
                    .iter()
                    .map(|t| SendTarget {
                        event_name: t.event_name.clone(),
                        namespace: ns_name.clone(),
                        var_name: alias_var_name.clone(),
                        source: t.source.clone(),
                    })
                    .collect();
                additions.push((alias_key, rewritten));
            }
        }
        for (key, targets) in additions {
            self.send_targets.entry(key).or_insert(targets);
        }
    }

    /// Scan a namespace for send() calls in function bodies.
    fn scan_namespace_for_send_targets(
        &mut self,
        ns_name: &str,
        namespace: &Namespace,
        program: &Program,
    ) {
        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.to_string();

            let mut event_names: Vec<String> = Vec::new();
            Self::find_send_calls_recursive(
                value,
                namespace,
                program,
                &self.core_variables,
                &mut event_names,
            );

            if !event_names.is_empty() {
                event_names.sort();
                event_names.dedup();
                let fn_key = format!("{}/{}", ns_name, var_name);
                let targets: Vec<SendTarget> = event_names
                    .into_iter()
                    .map(|event_name| SendTarget {
                        event_name,
                        namespace: ns_name.to_string(),
                        var_name: var_name.clone(),
                        source: SendTargetSource::Static,
                    })
                    .collect();
                self.send_targets.insert(fn_key, targets);
            }
        }
    }

    /// Determine if a FnCall's callee resolves to the core `send` function (`::hot::event/send`).
    fn is_core_send_call(
        callee: &Value,
        namespace: &Namespace,
        core_variables: &crate::lang::compiler::core_registry::CoreVariableRegistry,
    ) -> bool {
        match callee {
            // Fully qualified or aliased: ::hot::event/send or ::ev/send
            Value::Ref(Ref::Ns(ns_ref)) => {
                if ns_ref.function_name.as_deref() == Some("send") {
                    let ns_str = ns_ref.ns.to_string();
                    if ns_str == "::hot::event" {
                        return true;
                    }
                    // Check aliases: if ns_str is an alias that resolves to ::hot::event
                    if let Some(resolved) = namespace.aliases.get(&ns_ref.ns) {
                        return resolved.to_string() == "::hot::event";
                    }
                }
                false
            }
            // Unqualified `send` or var-aliased send (e.g., `s ::hot::event/send`)
            Value::Ref(Ref::Var(var_ref)) => {
                let name = var_ref.var.sym.name();

                // Direct core `send`
                if name == "send" {
                    // Check if `send` is locally shadowed by a user-defined (non-core) variable
                    let is_user_shadowed = namespace.scope.vars.keys().any(|v| {
                        v.sym.name() == "send"
                            && !v
                                .meta
                                .as_ref()
                                .and_then(|m| {
                                    if let Val::Map(map) = &m.val {
                                        map.get(&Val::from("core")).and_then(|v| {
                                            if let Val::Bool(b) = v { Some(*b) } else { None }
                                        })
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(false)
                    });
                    if is_user_shadowed {
                        return false;
                    }
                    return core_variables.get("send").is_some();
                }

                // Var alias: `s ::hot::event/send` then `s(...)`
                // Look up the variable's value to see if it resolves to ::hot::event/send
                for (v, val) in &namespace.scope.vars {
                    if v.sym.name() != name {
                        continue;
                    }
                    if let Value::Ref(Ref::Ns(ns_ref)) = val
                        && ns_ref.function_name.as_deref() == Some("send")
                    {
                        let ns_str = ns_ref.ns.to_string();
                        if ns_str == "::hot::event" {
                            return true;
                        }
                        if let Some(resolved) = namespace.aliases.get(&ns_ref.ns) {
                            return resolved.to_string() == "::hot::event";
                        }
                    }
                    break;
                }
                false
            }
            _ => false,
        }
    }

    /// Try to resolve a Value to a string literal.
    fn resolve_to_string(
        value: &Value,
        namespace: &Namespace,
        program: &Program,
    ) -> Option<String> {
        match value {
            Value::Val(Val::Str(s), _) => Some((**s).to_string()),
            Value::Ref(Ref::Var(var_ref)) => {
                let name = var_ref.var.sym.name();
                for (v, val) in &namespace.scope.vars {
                    if v.sym.name() == name
                        && let Value::Val(Val::Str(s), _) = val
                    {
                        return Some((**s).to_string());
                    }
                }
                None
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                if let Some(fn_name) = &ns_ref.function_name {
                    let ns_str = ns_ref.ns.to_string();
                    // Resolve alias first
                    let resolved_ns = namespace
                        .aliases
                        .get(&ns_ref.ns)
                        .map(|p| p.to_string())
                        .unwrap_or(ns_str);
                    let target_ns_path = NsPath::from(&resolved_ns);
                    if let Some(target_ns) = program.namespaces.get(&target_ns_path) {
                        for (v, val) in &target_ns.scope.vars {
                            if v.sym.name() == fn_name
                                && let Value::Val(Val::Str(s), _) = val
                            {
                                return Some((**s).to_string());
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Try to extract event name from an `Event({type: "...", ...})` constructor call.
    fn extract_event_constructor_type(args: &[FnCallArg]) -> Option<String> {
        if args.len() != 1 {
            return None;
        }
        // The arg should be a map literal with a "type" key
        if let Value::Val(Val::Map(map), _) = &args[0].value
            && let Some(Val::Str(s)) = map.get(&Val::from("type"))
        {
            return Some((**s).to_string());
        }
        None
    }

    /// Recursively walk a Value AST to find `send()` calls and extract event names.
    fn find_send_calls_recursive(
        value: &Value,
        namespace: &Namespace,
        program: &Program,
        core_variables: &crate::lang::compiler::core_registry::CoreVariableRegistry,
        results: &mut Vec<String>,
    ) {
        match value {
            Value::FnCall(fn_call) => {
                if Self::is_core_send_call(&fn_call.function, namespace, core_variables) {
                    // Try to extract event name from send() arguments
                    match fn_call.args.len() {
                        1 => {
                            // send("event-name") or send(Event({type: "...", data: ...}))
                            if let Some(name) =
                                Self::resolve_to_string(&fn_call.args[0].value, namespace, program)
                            {
                                results.push(name);
                            } else if let Value::FnCall(inner_call) = &fn_call.args[0].value {
                                // Check for Event({type: "..."}) constructor
                                if let Some(name) =
                                    Self::extract_event_constructor_type(&inner_call.args)
                                {
                                    results.push(name);
                                }
                            }
                        }
                        2 => {
                            // send("event-name", data)
                            if let Some(name) =
                                Self::resolve_to_string(&fn_call.args[0].value, namespace, program)
                            {
                                results.push(name);
                            }
                        }
                        _ => {}
                    }
                }

                // Recurse into the callee and all arguments
                Self::find_send_calls_recursive(
                    &fn_call.function,
                    namespace,
                    program,
                    core_variables,
                    results,
                );
                for arg in &fn_call.args {
                    Self::find_send_calls_recursive(
                        &arg.value,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::Fn(fn_defs) => {
                for fn_def in fn_defs {
                    Self::find_send_calls_recursive(
                        &fn_def.body,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::Flow(flow) => {
                for expr in &flow.expressions {
                    Self::find_send_calls_recursive(
                        expr,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::Cond(_, condition, result_flow) => {
                Self::find_send_calls_recursive(
                    condition,
                    namespace,
                    program,
                    core_variables,
                    results,
                );
                for expr in &result_flow.expressions {
                    Self::find_send_calls_recursive(
                        expr,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::CondDefault(flow) => {
                for expr in &flow.expressions {
                    Self::find_send_calls_recursive(
                        expr,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::Match(match_expr) => {
                Self::find_send_calls_recursive(
                    &match_expr.value,
                    namespace,
                    program,
                    core_variables,
                    results,
                );
                for arm in &match_expr.arms {
                    Self::find_send_calls_recursive(
                        &arm.body,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::Lambda(lambda) => {
                Self::find_send_calls_recursive(
                    &lambda.body,
                    namespace,
                    program,
                    core_variables,
                    results,
                );
            }
            Value::TemplateLiteral(template) => {
                for part in &template.parts {
                    if let crate::lang::ast::TemplatePart::Expression(expr) = part {
                        Self::find_send_calls_recursive(
                            expr.as_ref(),
                            namespace,
                            program,
                            core_variables,
                            results,
                        );
                    }
                }
            }
            Value::Raw(inner) | Value::Do(inner) => {
                Self::find_send_calls_recursive(inner, namespace, program, core_variables, results);
            }
            Value::MultipleValues(values) => {
                for v in values {
                    Self::find_send_calls_recursive(v, namespace, program, core_variables, results);
                }
            }
            Value::MatchArm(arm) => {
                Self::find_send_calls_recursive(
                    &arm.body,
                    namespace,
                    program,
                    core_variables,
                    results,
                );
            }
            Value::Val(val, _) => {
                if let Val::Box(boxed) = val
                    && let Some(fn_call) = boxed.as_any().downcast_ref::<FnCall>()
                {
                    Self::find_send_calls_recursive(
                        &Value::FnCall(fn_call.clone()),
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            Value::MapWithSpread { spread_entries, .. } => {
                for (_, spread_val) in spread_entries {
                    Self::find_send_calls_recursive(
                        spread_val,
                        namespace,
                        program,
                        core_variables,
                        results,
                    );
                }
            }
            // Leaves that cannot contain send() calls
            Value::Ref(_)
            | Value::TypeDef(_)
            | Value::TypeImplementation(_)
            | Value::Unbound(_)
            | Value::VariadicExpansion(_)
            | Value::Placeholder(_) => {}
        }
    }

    /// Scan a single namespace for event handlers
    fn scan_namespace_for_event_handlers(
        &mut self,
        ns_name: &str,
        namespace: &Namespace,
        program: &Program,
    ) {
        tracing::trace!(
            "Scanning namespace {} with {} variables",
            ns_name,
            namespace.scope.vars.len()
        );

        // Scan all variables in the namespace scope
        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.to_string();
            tracing::trace!(
                "Checking variable: {} (has meta: {})",
                var_name,
                var.meta.is_some()
            );

            if let Some(event_type) = self.extract_event_type_from_var(var) {
                tracing::trace!(
                    "Found event handler: {} for event type: {}",
                    var_name,
                    event_type
                );

                // Resolve aliases like `tg-handler ::lib/handler` to the
                // underlying callable; fall back to the var's own value
                // if it isn't an alias.
                let target = resolve_alias_target(program, ns_name, value);
                let arity_value = target.map(|(_, v)| v).unwrap_or(value);
                let arity = Self::get_function_arity(arity_value);
                if arity == 0 {
                    // Extract source location from the var for error reporting
                    let location = var
                        .src
                        .as_ref()
                        .map(|src| crate::lang::errors::ErrorLocation {
                            line: src.line,
                            column: src.column,
                            position: src.position,
                            length: var_name.len(),
                            file: src.file.as_ref().map(std::path::PathBuf::from),
                        });

                    self.validation_errors.push(
                        crate::lang::errors::CompilerError::InvalidEventHandler {
                            func_name: format!("{}/{}", ns_name, var_name),
                            event_type: event_type.clone(),
                            message: "Event handler must accept an event parameter".to_string(),
                            location,
                        },
                    );
                }

                // For aliases, the handler we register should carry the
                // merged meta (target's meta as base, alias's meta wins
                // on collision) so library-supplied `doc:` etc. flows
                // through to the registered handler entry.
                let effective_var = build_effective_var_for_alias(var, target);
                let handler =
                    EventHandler::new(event_type.clone(), ns_name, &var_name, &effective_var);

                // Add to handlers collection, avoiding duplicates by fn
                let fn_name = format!("{}/{}", ns_name, var_name);
                let handlers = self.event_handlers.entry(event_type).or_default();
                let is_duplicate = handlers
                    .iter()
                    .any(|h| h.event_handler.get_str("fn") == fn_name);
                if !is_duplicate {
                    handlers.push(handler);
                } else {
                    tracing::debug!("Skipping duplicate event handler: {}", fn_name);
                }
            }
        }
    }

    /// Scan a single namespace for scheduled functions
    fn scan_namespace_for_scheduled_functions(
        &mut self,
        ns_name: &str,
        namespace: &Namespace,
        program: &Program,
    ) {
        tracing::trace!(
            "Scanning namespace {} for scheduled functions with {} variables",
            ns_name,
            namespace.scope.vars.len()
        );

        // Scan all variables in the namespace scope
        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.to_string();
            tracing::trace!(
                "Checking variable for schedule: {} (has meta: {})",
                var_name,
                var.meta.is_some()
            );

            if let Some(cron_expression) = self.extract_schedule_from_var(var) {
                tracing::trace!(
                    "Found scheduled function: {} with cron: {}",
                    var_name,
                    cron_expression
                );

                // Resolve through alias chains so a `tg-cron ::lib/cron`
                // alias is validated against the library function's
                // arity, not its own (which is 0 for a `Value::Ref`).
                let target = resolve_alias_target(program, ns_name, value);
                let arity_value = target.map(|(_, v)| v).unwrap_or(value);
                let arity = Self::get_function_arity(arity_value);
                if arity == 0 {
                    // Extract source location from the var for error reporting
                    let location = var
                        .src
                        .as_ref()
                        .map(|src| crate::lang::errors::ErrorLocation {
                            line: src.line,
                            column: src.column,
                            position: src.position,
                            length: var_name.len(),
                            file: src.file.as_ref().map(std::path::PathBuf::from),
                        });

                    self.validation_errors.push(
                        crate::lang::errors::CompilerError::InvalidScheduledFunction {
                            func_name: format!("{}/{}", ns_name, var_name),
                            message: "Scheduled function must accept an event parameter"
                                .to_string(),
                            location,
                        },
                    );
                }

                // Merge target meta in for aliases so registered entry
                // carries doc/etc. from the library function.
                let effective_var = build_effective_var_for_alias(var, target);
                let scheduled_function = ScheduledFunction::new(
                    cron_expression.clone(),
                    ns_name,
                    &var_name,
                    &effective_var,
                );

                // Add to scheduled functions collection, avoiding duplicates by fn
                let fn_name = format!("{}/{}", ns_name, var_name);
                let functions = self.scheduled_functions.entry(cron_expression).or_default();
                let is_duplicate = functions
                    .iter()
                    .any(|f| f.scheduled_function.get_str("fn") == fn_name);
                if !is_duplicate {
                    functions.push(scheduled_function);
                } else {
                    tracing::debug!("Skipping duplicate scheduled function: {}", fn_name);
                }
            }
        }
    }

    /// Get the arity (number of parameters) of a function value
    fn get_function_arity(value: &Value) -> usize {
        match value {
            Value::Fn(fn_defs) => {
                // For multi-arity functions, return the max arity
                fn_defs
                    .iter()
                    .map(|def| def.args.args.len())
                    .max()
                    .unwrap_or(0)
            }
            Value::Lambda(lambda) => lambda.args.args.len(),
            _ => 0,
        }
    }

    /// Extract event type from a variable's metadata if it's an event handler
    fn extract_event_type_from_var(&self, var: &Var) -> Option<String> {
        // Check if the variable has metadata
        let meta = var.meta.as_ref()?;

        tracing::trace!(
            "Checking metadata for {}: {:?}",
            var.sym.to_string(),
            meta.val
        );

        // Handle both Map and JSON string formats
        match &meta.val {
            Val::Map(meta_map) => {
                // Direct map format
                if let Some(Val::Str(event_type)) = meta_map.get(&Val::from("on-event")) {
                    tracing::trace!(
                        "Found event handler {} with type {}",
                        var.sym.to_string(),
                        event_type
                    );
                    return Some((**event_type).to_owned());
                }
            }
            Val::Str(json_str) => {
                // JSON string format - parse it
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(event_type) = parsed.get("on-event")
                    && let Some(event_type_str) = event_type.as_str()
                {
                    tracing::trace!(
                        "Found event handler {} with type {}",
                        var.sym.to_string(),
                        event_type_str
                    );
                    return Some(event_type_str.to_string());
                }
            }
            _ => {}
        }

        None
    }

    /// Extract cron expression from a variable's metadata if it's a scheduled function
    fn extract_schedule_from_var(&self, var: &Var) -> Option<String> {
        // Check if the variable has metadata
        let meta = var.meta.as_ref()?;

        tracing::trace!(
            "Checking schedule metadata for {}: {:?}",
            var.sym.to_string(),
            meta.val
        );

        // Handle both Map and JSON string formats
        match &meta.val {
            Val::Map(meta_map) => {
                // Direct map format
                if let Some(Val::Str(cron_expression)) = meta_map.get(&Val::from("schedule")) {
                    tracing::trace!(
                        "Found scheduled function {} with cron {}",
                        var.sym.to_string(),
                        cron_expression
                    );
                    return Some((**cron_expression).to_owned());
                }
            }
            Val::Str(json_str) => {
                // JSON string format - parse it
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                    && let Some(cron_expression) = parsed.get("schedule")
                    && let Some(cron_str) = cron_expression.as_str()
                {
                    tracing::trace!(
                        "Found scheduled function {} with cron {}",
                        var.sym.to_string(),
                        cron_str
                    );
                    return Some(cron_str.to_string());
                }
            }
            _ => {}
        }

        None
    }
}

#[cfg(test)]
mod alias_meta_tests {
    //! Tests for `name meta {…} value` and `name value meta {…}` aliasing
    //! with `meta` annotations — the wrapper-pattern shorthand. Library
    //! code stays meta-free; consumer projects alias the library function
    //! and attach agentic meta on the alias.

    use super::*;
    use crate::lang::parser::parse_hot;

    fn compile_and_extract(source: &str) -> (super::EventHandlers, super::ScheduledFunctions) {
        let mut program = parse_hot(source).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        compiler
            .extract_event_handlers(&program)
            .expect("event handler extraction");
        compiler
            .extract_scheduled_functions(&program)
            .expect("scheduled function extraction");
        (
            compiler.get_event_handlers().clone(),
            compiler.get_scheduled_functions().clone(),
        )
    }

    fn handler_meta_keys(handler: &EventHandler) -> Vec<String> {
        let Some(Val::Map(m)) = handler.event_handler.get("meta") else {
            return vec![];
        };
        let mut keys: Vec<String> = m
            .iter()
            .map(|(k, _)| match k {
                Val::Str(s) => (**s).to_string(),
                other => format!("{:?}", other),
            })
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn alias_with_meta_before_value_registers_handler() {
        let src = r#"
::test::alias-a ns

base-handler
meta { doc: "library handler" }
fn (event: Map): Map { {ok: true} }

aliased
meta { on-event: "test:a" }
base-handler
"#;
        let (handlers, _) = compile_and_extract(src);
        let entries = handlers
            .get("test:a")
            .expect("event 'test:a' should be registered");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].event_handler.get_str("fn"),
            "::test::alias-a/aliased"
        );
    }

    #[test]
    fn alias_with_meta_after_value_registers_handler() {
        // Trailing-meta form (Form B) — verifies the parser change that
        // accepts `name value meta {…}` symmetrically with the long-
        // standing `name meta {…} value` form.
        let src = r#"
::test::alias-b ns

base-handler
meta { doc: "library handler" }
fn (event: Map): Map { {ok: true} }

aliased base-handler
meta { on-event: "test:b" }
"#;
        let (handlers, _) = compile_and_extract(src);
        let entries = handlers
            .get("test:b")
            .expect("event 'test:b' should be registered (trailing meta on alias)");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].event_handler.get_str("fn"),
            "::test::alias-b/aliased"
        );
    }

    #[test]
    fn alias_meta_merges_with_target_meta_alias_wins() {
        let src = r#"
::test::alias-merge ns

base-handler
meta { doc: "library doc", retry: 1 }
fn (event: Map): Map { {ok: true} }

aliased
meta { on-event: "test:merge", retry: 5 }
base-handler
"#;
        let (handlers, _) = compile_and_extract(src);
        let entries = handlers.get("test:merge").expect("registered");
        let h = &entries[0];
        let keys = handler_meta_keys(h);
        // Both `doc` (from target) and `on-event` (from alias) present.
        assert!(
            keys.contains(&"doc".to_string()),
            "doc inherited: {:?}",
            keys
        );
        assert!(
            keys.contains(&"on-event".to_string()),
            "on-event from alias: {:?}",
            keys
        );
        assert!(
            keys.contains(&"retry".to_string()),
            "retry from alias overrides target: {:?}",
            keys
        );
        // Alias wins on collision: retry should be 5, not 1.
        let meta = h.event_handler.get("meta").unwrap();
        if let Val::Map(m) = meta {
            let retry = m.get(&Val::from("retry"));
            assert!(
                matches!(retry, Some(Val::Int(5))),
                "alias's retry=5 should win, got {:?}",
                retry
            );
        } else {
            panic!("meta should be a map");
        }
    }

    #[test]
    fn alias_with_no_function_arity_validation_passes() {
        // Regression: pre-fix, the alias's value was a `Value::Ref` whose
        // arity was 0, causing "Event handler must accept an event
        // parameter". After resolving through refs the underlying
        // function's arity drives validation.
        let src = r#"
::test::alias-arity ns

base-handler fn (event: Map): Map { {ok: true} }

aliased
meta { on-event: "test:arity" }
base-handler
"#;
        // Should compile + extract without arity errors.
        let (handlers, _) = compile_and_extract(src);
        assert!(handlers.contains_key("test:arity"));
    }

    #[test]
    fn alias_to_arity_zero_function_still_fails_validation() {
        // Sanity check: if the alias points at a true zero-arg function,
        // we still reject it.
        let src = r#"
::test::alias-zero ns

zero-arg fn (): Map { {ok: true} }

aliased
meta { on-event: "test:zero" }
zero-arg
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        let err = compiler
            .extract_event_handlers(&program)
            .expect_err("zero-arg alias should fail");
        let formatted = err.format_error(false);
        assert!(
            formatted.contains("Event handler must accept"),
            "expected arity error, got: {}",
            formatted
        );
    }

    #[test]
    fn alias_resolves_through_namespace_alias() {
        // Real-world wrapper-pattern shape: `tg-handler ::tg/handler`
        // where `::tg` is a namespace alias for `::lib::adapter`.
        let src = r#"
::lib::adapter ns
record-voice fn (event: Map): Map { {ok: true} }

::test::xns ns

::tg ::lib::adapter

tg-record-voice
meta { on-event: "telegram:record-voice" }
::tg/record-voice
"#;
        let (handlers, _) = compile_and_extract(src);
        assert!(
            handlers.contains_key("telegram:record-voice"),
            "handler should register through namespace alias; got: {:?}",
            handlers.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn scheduled_function_alias_works() {
        let src = r#"
::test::cron ns

base-cron
meta { doc: "library cron" }
fn (event: Map): Map { {ran: true} }

scheduled
meta { schedule: "every 30 seconds" }
base-cron
"#;
        let (_, scheduled) = compile_and_extract(src);
        let entries = scheduled
            .get("every 30 seconds")
            .expect("schedule registered");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].scheduled_function.get_str("fn"),
            "::test::cron/scheduled"
        );
    }

    #[test]
    fn alias_appears_in_tool_spec_registry_with_target_signature() {
        // Library function is private to the wrapper. The wrapper-pattern
        // alias is what callers see, so the tool-spec registry must
        // index the alias's fully-qualified name and back it with the
        // *target's* schema (params/return types) — otherwise
        // `::hot::internal::mcp/schema-from-fn` returns nothing.
        let src = r#"
::lib::pkg ns
do-thing fn (n: Int): Int { ::hot::math/add(n, 1) }

::user::wrapper ns

::lp ::lib::pkg

aliased-tool
meta { doc: "user-facing wrapper", tool: { name: "DoThing" } }
::lp/do-thing
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        let registry = compiler.build_tool_specs(&program);
        let entry = registry
            .entries
            .get("::user::wrapper/aliased-tool")
            .expect("alias registered in tool-spec registry");
        assert_eq!(entry.display_name.as_deref(), Some("DoThing"));
        // Schema must come from the target — input has one Int param.
        let input_obj = match &entry.input_schema {
            Val::Map(m) => m,
            other => panic!("expected map input schema, got {:?}", other),
        };
        let props = input_obj.get(&Val::from("properties")).expect("properties");
        if let Val::Map(props) = props {
            assert!(
                props.get(&Val::from("n")).is_some(),
                "schema should expose target's `n` param: {:?}",
                props.keys().collect::<Vec<_>>()
            );
        } else {
            panic!("properties should be a map");
        }
    }

    #[test]
    fn alias_appears_in_skill_spec_registry_when_skill_meta_on_alias() {
        let src = r#"
::lib::skills ns
worker fn (input: Str): Str { input }

::user::agent ns

::ls ::lib::skills

my-skill
meta { skill: { name: "Worker", description: "wraps lib worker" } }
::ls/worker
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        let registry = compiler.build_skill_specs(&program);
        let entry = registry
            .entries
            .get("::user::agent/my-skill")
            .expect("alias registered in skill-spec registry");
        if let Val::Map(skill) = &entry.skill_meta {
            let name = skill.get(&Val::from("name"));
            assert!(
                matches!(name, Some(Val::Str(s)) if s.as_ref() == "Worker"),
                "skill.name should come from alias meta, got {:?}",
                name
            );
        } else {
            panic!("skill_meta should be a Map");
        }
    }

    #[test]
    fn alias_appears_in_function_mapping_under_own_qualified_name() {
        // Regression: scheduled/event/MCP/webhook routing in the worker
        // looks up the alias's qualified name in `function_mapping` to
        // pick the build that owns it. Aliases must therefore inherit
        // the target's mapping entries (per arity) under their own name.
        let src = r#"
::lib::adapter ns
check-updates fn (event: Map): Map { event }

::user::agent ns

::adapter ::lib::adapter

tg-check-updates
meta { schedule: "*/5 * * * *" }
::adapter/check-updates
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");

        let mapping = compiler.get_function_mapping();
        let target_id = mapping
            .get("::lib::adapter/check-updates/1")
            .copied()
            .expect("target should be in function_mapping");
        let alias_id = mapping
            .get("::user::agent/tg-check-updates/1")
            .copied()
            .expect(
                "alias's arity-keyed entry should be copied into function_mapping after Phase 3.5",
            );
        assert_eq!(
            target_id, alias_id,
            "alias must point to the same FunctionId as its target"
        );
    }

    #[test]
    fn local_var_alias_appears_in_function_mapping() {
        // Same-namespace alias (Ref::Var). Worker routing should see
        // both names so it can dispatch through either.
        let src = r#"
::user::svc ns

base fn (a: Int): Int { a }

aliased base
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");

        let mapping = compiler.get_function_mapping();
        let base_id = mapping
            .get("::user::svc/base/1")
            .copied()
            .expect("base function in mapping");
        let alias_id = mapping
            .get("::user::svc/aliased/1")
            .copied()
            .expect("local-var alias should be in mapping");
        assert_eq!(base_id, alias_id);
    }

    #[test]
    fn alias_inherits_send_targets_from_resolved_function() {
        // Alias has no body of its own, so without alias attribution
        // the agent graph would render it as an orphan. Verify that
        // `extract_send_targets` copies the target's events to the
        // alias's `ns/var` key.
        let src = r#"
::lib::svc ns
emit fn (): Map {
    ::hot::event/send("svc:tick", {})
}

::user::agent ns

::ls ::lib::svc

aliased ::ls/emit
"#;
        let mut program = parse_hot(src).expect("parse");
        let mut compiler = Compiler::new();
        compiler
            .compile_program_unchecked(&mut program)
            .expect("compile");
        compiler
            .extract_send_targets(&program)
            .expect("send target extraction");
        let alias_targets = compiler
            .get_send_targets()
            .get("::user::agent/aliased")
            .expect("alias should inherit sends from target");
        let events: Vec<&str> = alias_targets
            .iter()
            .map(|t| t.event_name.as_str())
            .collect();
        assert_eq!(events, vec!["svc:tick"]);
        // The rewritten target should report the alias's coordinates.
        assert_eq!(alias_targets[0].namespace, "::user::agent");
        assert_eq!(alias_targets[0].var_name, "aliased");
    }
}
