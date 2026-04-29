// Call Graph Builder
//
// Builds a function call graph from a Hot Program AST and resolves transitive
// context (ctx) requirements. This enables the compiler to determine exactly
// which ctx variables are needed based on what user code actually calls,
// rather than requiring all ctx vars from every included package namespace.
//
// The call graph is built after `resolve_all_variable_references` (Phase 1.5),
// so imported variables are already resolved from Ref::Var to Ref::Ns.
// Namespace aliases are resolved using each namespace's `aliases` map.

use crate::lang::ast::{Meta, NsPath, Program, Ref, TemplatePart, Value};
use crate::lang::compiler::box_checker::{
    BoxRequirement, FnBoxRequirement, ProgramBoxRequirements,
};
use crate::lang::compiler::ctx_checker::{
    CtxKeyConfig, NamespaceCtxRequirements, ProgramCtxRequirements,
};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};

/// A call graph mapping functions to their callees and ctx/box requirements.
pub struct CallGraph {
    /// Maps fully-qualified function name -> set of callee FQNs
    edges: AHashMap<String, AHashSet<String>>,
    /// Maps fully-qualified function name -> direct ctx requirements from meta
    fn_ctx_requirements: AHashMap<String, Vec<CtxKeyConfig>>,
    /// Maps fully-qualified function name -> box resource requirements from meta
    fn_box_requirements: AHashMap<String, BoxRequirement>,
    /// Maps fully-qualified function name -> source file path
    fn_source_files: AHashMap<String, String>,
}

impl CallGraph {
    /// Build a call graph from a Program AST.
    ///
    /// This must be called AFTER `resolve_all_variable_references` so that
    /// imported vars are already resolved to Ref::Ns.
    pub fn build(program: &Program) -> Self {
        let mut graph = CallGraph {
            edges: AHashMap::new(),
            fn_ctx_requirements: AHashMap::new(),
            fn_box_requirements: AHashMap::new(),
            fn_source_files: AHashMap::new(),
        };

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            let aliases = &namespace.aliases;
            let source_file = namespace
                .source_file
                .as_ref()
                .map(|p| p.display().to_string());

            for (var, value) in &namespace.scope.vars {
                let var_name = var.sym.to_string();
                let fqn = format!("{}/{}", ns_str, var_name);

                // Record source file for this function
                if let Some(ref sf) = source_file {
                    graph.fn_source_files.insert(fqn.clone(), sf.clone());
                }

                // Extract function-level ctx requirements from var.meta
                if let Some(ctx_keys) = extract_fn_ctx_from_meta(&var.meta) {
                    graph.fn_ctx_requirements.insert(fqn.clone(), ctx_keys);
                }

                // Extract function-level box requirements from var.meta
                if let Some(box_req) = extract_fn_box_from_meta(&var.meta) {
                    graph.fn_box_requirements.insert(fqn.clone(), box_req);
                }

                // Walk the value to find function calls
                if let Value::Fn(fn_defs) = value {
                    let mut callees = AHashSet::new();
                    for fn_def in fn_defs {
                        collect_callees_from_value(&fn_def.body, &ns_str, aliases, &mut callees);
                    }
                    graph.edges.insert(fqn, callees);
                } else if let Value::Ref(r) = value {
                    // Var imports like `api-request ::slack::api/request` are Value::Ref,
                    // not Value::Fn. Create an edge so the BFS can follow through the
                    // indirection and reach the target function's ctx requirements.
                    if let Some(target_fqn) = resolve_ref_fqn(r, &ns_str, aliases) {
                        let mut callees = AHashSet::new();
                        callees.insert(target_fqn);
                        graph.edges.insert(fqn, callees);
                    }
                }
            }
        }

        graph
    }

    /// Resolve all ctx requirements reachable from a set of root function FQNs.
    ///
    /// Performs a BFS from the roots through the call graph edges, collecting
    /// the union of all ctx requirements from reachable functions.
    pub fn resolve_ctx_requirements(&self, roots: &[String]) -> ProgramCtxRequirements {
        let mut visited = AHashSet::new();
        let mut queue: Vec<String> = roots.to_vec();
        let mut all_keys: AHashMap<String, CtxKeyConfig> = AHashMap::new();
        // Track which namespace each key came from (for error reporting)
        let mut key_sources: AHashMap<String, (String, Option<String>)> = AHashMap::new();

        while let Some(fqn) = queue.pop() {
            if !visited.insert(fqn.clone()) {
                continue;
            }

            // Collect ctx requirements from this function
            if let Some(keys) = self.fn_ctx_requirements.get(&fqn) {
                let source_file = self.fn_source_files.get(&fqn).cloned();
                for key in keys {
                    if !all_keys.contains_key(&key.key) {
                        all_keys.insert(key.key.clone(), key.clone());
                        key_sources.insert(key.key.clone(), (fqn.clone(), source_file.clone()));
                    }
                }
            }

            // Enqueue callees
            if let Some(callees) = self.edges.get(&fqn) {
                for callee in callees {
                    if !visited.contains(callee) {
                        queue.push(callee.clone());
                    }
                }
            }
        }

        // Build ProgramCtxRequirements grouped by source namespace/function
        let mut ns_map: AHashMap<String, NamespaceCtxRequirements> = AHashMap::new();
        for (key_name, key_config) in all_keys {
            let (fqn, source_file) = key_sources.get(&key_name).cloned().unwrap_or_default();
            let entry = ns_map
                .entry(fqn.clone())
                .or_insert_with(|| NamespaceCtxRequirements {
                    namespace: fqn,
                    keys: Vec::new(),
                    source_file,
                });
            entry.keys.push(key_config);
        }

        ProgramCtxRequirements {
            namespaces: ns_map.into_values().collect(),
        }
    }

    /// Identify root functions: all functions defined in user source files (not packages).
    ///
    /// User source files are identified by their path NOT containing common package
    /// directory patterns (e.g., `/pkg/` or `hot-std`). This is a heuristic;
    /// the source_file paths from the namespace tell us where the code lives.
    pub fn identify_user_roots(&self, program: &Program) -> Vec<String> {
        let mut roots = Vec::new();

        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            let source_file = namespace
                .source_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();

            // Skip package namespaces -- they contain /pkg/ in their source path
            // User source files are in /src/ directories of the project root
            if is_package_path(&source_file) {
                continue;
            }

            for (var, value) in &namespace.scope.vars {
                // Only include functions (not types, constants, etc.)
                if matches!(value, Value::Fn(_)) {
                    let fqn = format!("{}/{}", ns_str, var.sym);
                    roots.push(fqn);
                }
            }

            // Also add top-level non-function values as roots since they
            // execute during initialization and may call functions
            for (var, value) in &namespace.scope.vars {
                if !matches!(value, Value::Fn(_)) {
                    let fqn = format!("{}/{}", ns_str, var.sym);
                    // Only if we recorded edges for it (it made function calls)
                    if self.edges.contains_key(&fqn) {
                        roots.push(fqn);
                    }
                }
            }
        }

        roots
    }

    /// Resolve all box requirements reachable from a set of root function FQNs.
    ///
    /// Performs a BFS from the roots through the call graph edges, collecting
    /// box requirements from all reachable functions.
    pub fn resolve_box_requirements(&self, roots: &[String]) -> ProgramBoxRequirements {
        let mut visited = AHashSet::new();
        let mut queue: Vec<String> = roots.to_vec();
        let mut requirements = Vec::new();
        let mut seen_fqns = AHashSet::new();

        while let Some(fqn) = queue.pop() {
            if !visited.insert(fqn.clone()) {
                continue;
            }

            if let Some(box_req) = self.fn_box_requirements.get(&fqn)
                && !box_req.is_empty()
                && seen_fqns.insert(fqn.clone())
            {
                requirements.push(FnBoxRequirement {
                    fqn: fqn.clone(),
                    source_file: self.fn_source_files.get(&fqn).cloned(),
                    requirement: box_req.clone(),
                });
            }

            if let Some(callees) = self.edges.get(&fqn) {
                for callee in callees {
                    if !visited.contains(callee) {
                        queue.push(callee.clone());
                    }
                }
            }
        }

        ProgramBoxRequirements { requirements }
    }

    /// Convenience: extract box requirements for all user-reachable code.
    pub fn resolve_user_box_requirements(&self, program: &Program) -> ProgramBoxRequirements {
        let roots = self.identify_user_roots(program);
        self.resolve_box_requirements(&roots)
    }

    /// Convenience: extract ctx requirements for all user-reachable code.
    pub fn resolve_user_ctx_requirements(&self, program: &Program) -> ProgramCtxRequirements {
        let roots = self.identify_user_roots(program);
        tracing::debug!(
            "Call graph: {} root functions from user source, {} total functions tracked",
            roots.len(),
            self.edges.len()
        );
        self.resolve_ctx_requirements(&roots)
    }

    /// Get the number of functions in the graph
    pub fn function_count(&self) -> usize {
        self.edges.len()
    }

    /// Get the number of functions with ctx requirements
    pub fn ctx_function_count(&self) -> usize {
        self.fn_ctx_requirements.len()
    }

    /// Get the number of functions with box requirements
    pub fn box_function_count(&self) -> usize {
        self.fn_box_requirements.len()
    }
}

/// Check if a source file path looks like a package path (not user source)
fn is_package_path(path: &str) -> bool {
    // Package files typically live under a /pkg/ directory
    // or in hot-std paths, or in dependency resolution paths.
    // Handle both absolute paths (/foo/pkg/...) and relative (pkg/...).
    path.contains("/pkg/")
        || path.starts_with("pkg/")
        || path.contains("/hot-std/")
        || path.starts_with("hot-std/")
        || path.contains(".hot/cache/")
}

/// Extract ctx requirements from a function's variable metadata.
///
/// Looks for `meta { ctx: { "key": {required: true}, ... } }` on the Var.
fn extract_fn_ctx_from_meta(meta: &Option<Meta>) -> Option<Vec<CtxKeyConfig>> {
    let meta = meta.as_ref()?;

    let ctx_val = match &meta.val {
        Val::Map(map) => map.get(&Val::from("ctx"))?,
        _ => return None,
    };

    let mut keys = Vec::new();
    match ctx_val {
        Val::Map(ctx_map) => {
            for (key, config) in ctx_map.iter() {
                let key_str = match key {
                    Val::Str(s) => s.clone(),
                    _ => continue,
                };
                let key_config =
                    crate::lang::compiler::ctx_checker::parse_key_config(&key_str, config);
                keys.push(key_config);
            }
        }
        _ => return None,
    }

    if keys.is_empty() { None } else { Some(keys) }
}

/// Extract box requirements from a function's variable metadata.
///
/// Looks for `meta { box: { min-size: "medium", network: true } }` on the Var.
fn extract_fn_box_from_meta(meta: &Option<Meta>) -> Option<BoxRequirement> {
    let meta = meta.as_ref()?;

    let box_val = match &meta.val {
        Val::Map(map) => map.get(&Val::from("box"))?,
        _ => return None,
    };

    let box_map = match box_val {
        Val::Map(m) => m,
        _ => return None,
    };

    let min_size = box_map.get(&Val::from("min-size")).and_then(|v| match v {
        Val::Str(s) => Some(s.to_string()),
        _ => None,
    });

    let network = box_map.get(&Val::from("network")).and_then(|v| match v {
        Val::Bool(b) => Some(*b),
        _ => None,
    });

    let req = BoxRequirement { min_size, network };
    if req.is_empty() { None } else { Some(req) }
}

/// Recursively collect callee FQNs from a Value AST node.
fn collect_callees_from_value(
    value: &Value,
    current_ns: &str,
    aliases: &indexmap::IndexMap<NsPath, NsPath>,
    callees: &mut AHashSet<String>,
) {
    match value {
        Value::FnCall(fn_call) => {
            // Extract the callee's FQN from the function reference
            if let Some(fqn) = resolve_callee_fqn(&fn_call.function, current_ns, aliases) {
                callees.insert(fqn);
            }
            // Also walk the function expression itself (could be a complex expression)
            collect_callees_from_value(&fn_call.function, current_ns, aliases, callees);
            // Walk arguments (may contain function references or nested calls)
            for arg in &fn_call.args {
                collect_callees_from_value(&arg.value, current_ns, aliases, callees);
            }
        }

        Value::Flow(flow) => {
            for expr in &flow.expressions {
                collect_callees_from_value(expr, current_ns, aliases, callees);
            }
        }

        Value::Fn(fn_defs) => {
            // Nested function definitions (rare but possible)
            for fn_def in fn_defs {
                collect_callees_from_value(&fn_def.body, current_ns, aliases, callees);
            }
        }

        Value::Lambda(lambda) => {
            collect_callees_from_value(&lambda.body, current_ns, aliases, callees);
        }

        Value::Cond(_, cond_val, flow) => {
            collect_callees_from_value(cond_val, current_ns, aliases, callees);
            for expr in &flow.expressions {
                collect_callees_from_value(expr, current_ns, aliases, callees);
            }
        }

        Value::CondDefault(flow) => {
            for expr in &flow.expressions {
                collect_callees_from_value(expr, current_ns, aliases, callees);
            }
        }

        Value::Match(match_expr) => {
            collect_callees_from_value(&match_expr.value, current_ns, aliases, callees);
            for arm in &match_expr.arms {
                collect_callees_from_value(&arm.body, current_ns, aliases, callees);
            }
        }

        Value::MatchArm(arm) => {
            collect_callees_from_value(&arm.body, current_ns, aliases, callees);
        }

        Value::TemplateLiteral(tl) => {
            for part in &tl.parts {
                if let TemplatePart::Expression(expr) = part {
                    collect_callees_from_value(expr, current_ns, aliases, callees);
                }
            }
        }

        Value::Raw(inner) | Value::Do(inner) => {
            collect_callees_from_value(inner, current_ns, aliases, callees);
        }

        Value::MultipleValues(values) => {
            for v in values {
                collect_callees_from_value(v, current_ns, aliases, callees);
            }
        }

        // Function references in non-call positions (e.g., passed as arguments)
        // These represent potential callees too
        Value::Ref(r) => {
            if let Some(fqn) = resolve_ref_fqn(r, current_ns, aliases) {
                callees.insert(fqn);
            }
        }

        Value::MapWithSpread { spread_entries, .. } => {
            for (_, spread_val) in spread_entries {
                collect_callees_from_value(spread_val, current_ns, aliases, callees);
            }
        }

        // Leaf nodes: no function calls to extract
        Value::Val(_, _)
        | Value::TypeDef(_)
        | Value::TypeImplementation(_)
        | Value::Unbound(_)
        | Value::VariadicExpansion(_)
        | Value::Placeholder(_) => {}
    }
}

/// Resolve a function call's target to a fully-qualified name.
fn resolve_callee_fqn(
    function: &Value,
    current_ns: &str,
    aliases: &indexmap::IndexMap<NsPath, NsPath>,
) -> Option<String> {
    match function {
        Value::Ref(r) => resolve_ref_fqn(r, current_ns, aliases),
        _ => None, // Dynamic/computed function references can't be resolved statically
    }
}

/// Resolve a Ref to a fully-qualified function name.
fn resolve_ref_fqn(
    r: &Ref,
    current_ns: &str,
    aliases: &indexmap::IndexMap<NsPath, NsPath>,
) -> Option<String> {
    match r {
        Ref::Ns(ns_ref) => {
            // Resolve namespace through aliases
            let resolved_ns = if let Some(source_path) = aliases.get(&ns_ref.ns) {
                source_path.to_string()
            } else {
                ns_ref.ns.to_string()
            };

            ns_ref
                .function_name
                .as_ref()
                .map(|fn_name| format!("{}/{}", resolved_ns, fn_name))
        }
        Ref::Var(var_ref) => {
            // Local variable reference -- could be a function in the same namespace
            // After resolve_all_variable_references, most cross-namespace refs
            // are already converted to Ref::Ns. Remaining Var refs are local.
            let var_name = var_ref.var.sym.to_string();
            Some(format!("{}/{}", current_ns, var_name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::*;
    use indexmap::IndexMap;

    fn make_ns_path(name: &str) -> NsPath {
        NsPath(vec![NsPathPart::Sym(Sym::String(name.to_string()))])
    }

    fn make_var(name: &str) -> Var {
        Var {
            sym: Sym::String(name.to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            src: None,
        }
    }

    fn make_var_with_ctx(name: &str, ctx_keys: Vec<(&str, bool)>) -> Var {
        let ctx_map: IndexMap<Val, Val> = ctx_keys
            .into_iter()
            .map(|(k, required)| {
                let config: IndexMap<Val, Val> = [(Val::from("required"), Val::Bool(required))]
                    .into_iter()
                    .collect();
                (Val::from(k), Val::Map(Box::new(config)))
            })
            .collect();

        let meta_map: IndexMap<Val, Val> = [(Val::from("ctx"), Val::Map(Box::new(ctx_map)))]
            .into_iter()
            .collect();

        Var {
            sym: Sym::String(name.to_string()),
            deep_set: None,
            deep_path: None,
            meta: Some(Meta {
                val: Val::Map(Box::new(meta_map)),
            }),
            src: None,
        }
    }

    fn make_fn_call_ns(ns: &str, fn_name: &str) -> Value {
        Value::FnCall(FnCall {
            function: Box::new(Value::Ref(Ref::Ns(NsRef {
                ns: make_ns_path(ns),
                src: None,
                function_name: Some(fn_name.to_string()),
            }))),
            args: vec![],
            result_path: None,
            src: None,
        })
    }

    fn make_fn_call_local(fn_name: &str) -> Value {
        Value::FnCall(FnCall {
            function: Box::new(Value::Ref(Ref::Var(VarRef {
                var: make_var(fn_name),
                data: None,
                src: None,
            }))),
            args: vec![],
            result_path: None,
            src: None,
        })
    }

    fn make_simple_fn(body: Value) -> Value {
        Value::Fn(vec![FnDef {
            args: FnArgs {
                args: vec![],
                variadic: false,
            },
            body,
            return_type: None,
        }])
    }

    #[test]
    fn test_direct_call_detected() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app namespace with a function that calls ::lib/helper
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_ns("lib", "helper")),
        );

        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let callees = graph.edges.get("::app/my-fn").unwrap();
        assert!(callees.contains("::lib/helper"));
    }

    #[test]
    fn test_local_call_detected() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        let mut scope = Scope {
            vars: IndexMap::new(),
        };
        // my-fn calls local-helper (same namespace)
        scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_local("local-helper")),
        );
        scope.vars.insert(
            make_var("local-helper"),
            make_simple_fn(Value::Val(Val::Null, None)),
        );

        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let callees = graph.edges.get("::app/my-fn").unwrap();
        assert!(callees.contains("::app/local-helper"));
    }

    #[test]
    fn test_fn_ctx_requirements_extracted() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("lib"),
        };

        let mut scope = Scope {
            vars: IndexMap::new(),
        };
        scope.vars.insert(
            make_var_with_ctx("get-creds", vec![("api.key", true), ("api.secret", true)]),
            make_simple_fn(Value::Val(Val::Null, None)),
        );

        program.namespaces.insert(
            make_ns_path("lib"),
            Namespace {
                path: make_ns_path("lib"),
                scope,
                meta: None,
                source_file: Some("pkg/lib/src/lib.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let ctx = graph.fn_ctx_requirements.get("::lib/get-creds").unwrap();
        assert_eq!(ctx.len(), 2);
        assert!(ctx.iter().any(|k| k.key == "api.key" && k.required));
        assert!(ctx.iter().any(|k| k.key == "api.secret" && k.required));
    }

    #[test]
    fn test_transitive_ctx_resolution() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app/my-fn calls ::lib/do-thing
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_ns("lib", "do-thing")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::lib/do-thing calls ::lib/get-creds which has ctx requirements
        let mut lib_scope = Scope {
            vars: IndexMap::new(),
        };
        lib_scope.vars.insert(
            make_var("do-thing"),
            make_simple_fn(make_fn_call_local("get-creds")),
        );
        lib_scope.vars.insert(
            make_var_with_ctx("get-creds", vec![("api.key", true)]),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("lib"),
            Namespace {
                path: make_ns_path("lib"),
                scope: lib_scope,
                meta: None,
                source_file: Some("pkg/lib/src/lib.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);

        // Resolve from ::app/my-fn -- should transitively find api.key
        let reqs = graph.resolve_ctx_requirements(&["::app/my-fn".to_string()]);
        let required = reqs.all_required_keys();
        assert!(required.contains("api.key"));
    }

    #[test]
    fn test_unreachable_ctx_not_included() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app/my-fn calls ::lib/safe-fn (no ctx)
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_ns("lib", "safe-fn")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::lib has safe-fn (no ctx) and dangerous-fn (has ctx)
        let mut lib_scope = Scope {
            vars: IndexMap::new(),
        };
        lib_scope.vars.insert(
            make_var("safe-fn"),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        lib_scope.vars.insert(
            make_var_with_ctx("dangerous-fn", vec![("secret.key", true)]),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("lib"),
            Namespace {
                path: make_ns_path("lib"),
                scope: lib_scope,
                meta: None,
                source_file: Some("pkg/lib/src/lib.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);

        // Resolve from ::app/my-fn -- should NOT include secret.key
        let reqs = graph.resolve_ctx_requirements(&["::app/my-fn".to_string()]);
        let required = reqs.all_required_keys();
        assert!(!required.contains("secret.key"));
    }

    #[test]
    fn test_alias_resolution() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app has alias ::s3 -> ::aws::s3
        // ::app/my-fn calls ::s3/put-object (which should resolve to ::aws::s3/put-object)
        let mut aliases = IndexMap::new();
        let s3_alias = NsPath(vec![NsPathPart::Sym(Sym::String("s3".to_string()))]);
        let s3_full = NsPath(vec![
            NsPathPart::Sym(Sym::String("aws".to_string())),
            NsPathPart::Sym(Sym::String("s3".to_string())),
        ]);
        aliases.insert(s3_alias.clone(), s3_full);

        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_ns("s3", "put-object")),
        );

        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases,
            },
        );

        let graph = CallGraph::build(&program);
        let callees = graph.edges.get("::app/my-fn").unwrap();
        assert!(callees.contains("::aws::s3/put-object"));
    }

    #[test]
    fn test_cycle_handling() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app/fn-a calls ::app/fn-b, ::app/fn-b calls ::app/fn-a (cycle)
        let mut scope = Scope {
            vars: IndexMap::new(),
        };
        scope
            .vars
            .insert(make_var("fn-a"), make_simple_fn(make_fn_call_local("fn-b")));
        scope.vars.insert(
            make_var_with_ctx("fn-b", vec![("cycle.key", true)]),
            make_simple_fn(make_fn_call_local("fn-a")),
        );

        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);

        // Should not infinite loop, and should find cycle.key
        let reqs = graph.resolve_ctx_requirements(&["::app/fn-a".to_string()]);
        let required = reqs.all_required_keys();
        assert!(required.contains("cycle.key"));
    }

    #[test]
    fn test_user_roots_identification() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // User source
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("user-fn"),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // Package source
        let mut lib_scope = Scope {
            vars: IndexMap::new(),
        };
        lib_scope.vars.insert(
            make_var("pkg-fn"),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("lib"),
            Namespace {
                path: make_ns_path("lib"),
                scope: lib_scope,
                meta: None,
                source_file: Some("pkg/lib/src/lib.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let roots = graph.identify_user_roots(&program);

        assert!(roots.contains(&"::app/user-fn".to_string()));
        assert!(!roots.contains(&"::lib/pkg-fn".to_string()));
    }

    #[test]
    fn test_cdn_cache_relative_path_excluded_from_roots() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // User source
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("user-fn"),
            make_simple_fn(make_fn_call_ns("xai::responses", "chat")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("hot/src/myapp/bot.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // CDN package with relative .hot/cache/ path (not starting with /)
        let mut collections_scope = Scope {
            vars: IndexMap::new(),
        };
        collections_scope.vars.insert(
            make_var_with_ctx("management-request", vec![("xai.management.api.key", true)]),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("xai::collections"),
            Namespace {
                path: make_ns_path("xai::collections"),
                scope: collections_scope,
                meta: None,
                source_file: Some(
                    ".hot/cache/cdn/hot.dev/xai/1.0.3/src/xai/collections.hot".into(),
                ),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let roots = graph.identify_user_roots(&program);

        // User function should be a root
        assert!(roots.contains(&"::app/user-fn".to_string()));
        // CDN package function with relative .hot/cache/ path should NOT be a root
        assert!(!roots.contains(&"::xai::collections/management-request".to_string()));

        // Resolve ctx requirements from user roots — should NOT include unreachable package ctx
        let reqs = graph.resolve_user_ctx_requirements(&program);
        let required = reqs.all_required_keys();
        assert!(
            !required.contains("xai.management.api.key"),
            "CDN package ctx should not be required when function is unreachable from user code"
        );
    }

    #[test]
    fn test_var_import_ctx_resolved_transitively() {
        // Simulates the pattern: ::slack::misc imports api-request from ::slack::api,
        // ::slack::api/request has ctx requirements. User code calls ::slack::misc/auth-test
        // which calls api-request(...). The ctx requirement should be found.
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // User namespace: calls ::slack::misc/auth-test
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("main"),
            make_simple_fn(make_fn_call_ns("slack::misc", "auth-test")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("hot/src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::slack::misc namespace: has var import `api-request ::slack::api/request`
        // and auth-test calls api-request(...)
        let mut misc_scope = Scope {
            vars: IndexMap::new(),
        };
        // auth-test calls the local var "api-request"
        misc_scope.vars.insert(
            make_var("auth-test"),
            make_simple_fn(make_fn_call_local("api-request")),
        );
        // Var import: api-request = ::slack::api/request (Value::Ref, not Value::Fn)
        misc_scope.vars.insert(
            make_var("api-request"),
            Value::Ref(Ref::Ns(NsRef {
                ns: make_ns_path("slack::api"),
                src: None,
                function_name: Some("request".to_string()),
            })),
        );
        program.namespaces.insert(
            make_ns_path("slack::misc"),
            Namespace {
                path: make_ns_path("slack::misc"),
                scope: misc_scope,
                meta: None,
                source_file: Some("pkg/slack/src/slack/misc.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::slack::api namespace: request has ctx requirement
        let mut api_scope = Scope {
            vars: IndexMap::new(),
        };
        api_scope.vars.insert(
            make_var_with_ctx("request", vec![("slack.api.key", true)]),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("slack::api"),
            Namespace {
                path: make_ns_path("slack::api"),
                scope: api_scope,
                meta: None,
                source_file: Some("pkg/slack/src/slack/api.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);

        // Verify the var import created an edge
        let import_edges = graph.edges.get("::slack::misc/api-request");
        assert!(
            import_edges.is_some(),
            "Var import should create an edge to the target function"
        );
        assert!(import_edges.unwrap().contains("::slack::api/request"));

        // Resolve from user root — should transitively find slack.api.key
        let reqs = graph.resolve_ctx_requirements(&["::app/main".to_string()]);
        let required = reqs.all_required_keys();
        assert!(
            required.contains("slack.api.key"),
            "ctx requirement should be reachable through var import chain: \
             app/main -> slack::misc/auth-test -> slack::misc/api-request -> slack::api/request"
        );
    }

    fn make_var_with_box(name: &str, min_size: Option<&str>, network: Option<bool>) -> Var {
        let mut box_map: IndexMap<Val, Val> = IndexMap::new();
        if let Some(size) = min_size {
            box_map.insert(Val::from("min-size"), Val::from(size));
        }
        if let Some(net) = network {
            box_map.insert(Val::from("network"), Val::Bool(net));
        }

        let meta_map: IndexMap<Val, Val> = [(Val::from("box"), Val::Map(Box::new(box_map)))]
            .into_iter()
            .collect();

        Var {
            sym: Sym::String(name.to_string()),
            deep_set: None,
            deep_path: None,
            meta: Some(Meta {
                val: Val::Map(Box::new(meta_map)),
            }),
            src: None,
        }
    }

    #[test]
    fn test_box_requirements_extracted() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("ffmpeg"),
        };

        let mut scope = Scope {
            vars: IndexMap::new(),
        };
        scope.vars.insert(
            make_var_with_box("probe", Some("nano"), None),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        scope.vars.insert(
            make_var_with_box("transcode", Some("medium"), None),
            make_simple_fn(Value::Val(Val::Null, None)),
        );

        program.namespaces.insert(
            make_ns_path("ffmpeg"),
            Namespace {
                path: make_ns_path("ffmpeg"),
                scope,
                meta: None,
                source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        assert_eq!(graph.box_function_count(), 2);

        let probe_req = graph.fn_box_requirements.get("::ffmpeg/probe").unwrap();
        assert_eq!(probe_req.min_size.as_deref(), Some("nano"));
        assert_eq!(probe_req.network, None);

        let transcode_req = graph.fn_box_requirements.get("::ffmpeg/transcode").unwrap();
        assert_eq!(transcode_req.min_size.as_deref(), Some("medium"));
    }

    #[test]
    fn test_box_requirements_with_network() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("playwright"),
        };

        let mut scope = Scope {
            vars: IndexMap::new(),
        };
        scope.vars.insert(
            make_var_with_box("screenshot", Some("small"), Some(true)),
            make_simple_fn(Value::Val(Val::Null, None)),
        );

        program.namespaces.insert(
            make_ns_path("playwright"),
            Namespace {
                path: make_ns_path("playwright"),
                scope,
                meta: None,
                source_file: Some("pkg/playwright/src/playwright.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let req = graph
            .fn_box_requirements
            .get("::playwright/screenshot")
            .unwrap();
        assert_eq!(req.min_size.as_deref(), Some("small"));
        assert_eq!(req.network, Some(true));
    }

    #[test]
    fn test_transitive_box_resolution() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app/process-video calls ::ffmpeg/transcode
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("process-video"),
            make_simple_fn(make_fn_call_ns("ffmpeg", "transcode")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::ffmpeg/transcode has box: {min-size: "medium"}
        let mut ffmpeg_scope = Scope {
            vars: IndexMap::new(),
        };
        ffmpeg_scope.vars.insert(
            make_var_with_box("transcode", Some("medium"), None),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("ffmpeg"),
            Namespace {
                path: make_ns_path("ffmpeg"),
                scope: ffmpeg_scope,
                meta: None,
                source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);
        let reqs = graph.resolve_box_requirements(&["::app/process-video".to_string()]);

        assert!(reqs.has_requirements());
        assert_eq!(reqs.effective_min_size(), Some("medium".to_string()));
    }

    #[test]
    fn test_unreachable_box_not_included() {
        let mut program = Program {
            namespaces: IndexMap::new(),
            current_namespace: make_ns_path("app"),
        };

        // ::app/my-fn calls ::ffmpeg/probe (nano)
        let mut app_scope = Scope {
            vars: IndexMap::new(),
        };
        app_scope.vars.insert(
            make_var("my-fn"),
            make_simple_fn(make_fn_call_ns("ffmpeg", "probe")),
        );
        program.namespaces.insert(
            make_ns_path("app"),
            Namespace {
                path: make_ns_path("app"),
                scope: app_scope,
                meta: None,
                source_file: Some("src/app.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        // ::ffmpeg has probe (nano) and transcode (medium)
        let mut ffmpeg_scope = Scope {
            vars: IndexMap::new(),
        };
        ffmpeg_scope.vars.insert(
            make_var_with_box("probe", Some("nano"), None),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        ffmpeg_scope.vars.insert(
            make_var_with_box("transcode", Some("medium"), None),
            make_simple_fn(Value::Val(Val::Null, None)),
        );
        program.namespaces.insert(
            make_ns_path("ffmpeg"),
            Namespace {
                path: make_ns_path("ffmpeg"),
                scope: ffmpeg_scope,
                meta: None,
                source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".into()),
                aliases: IndexMap::new(),
            },
        );

        let graph = CallGraph::build(&program);

        // Only probe is reachable — medium (from transcode) should NOT be included
        let reqs = graph.resolve_box_requirements(&["::app/my-fn".to_string()]);
        assert_eq!(reqs.effective_min_size(), Some("nano".to_string()));
        assert_eq!(reqs.requirements.len(), 1);
    }

    #[test]
    fn test_is_package_path_covers_relative_cache() {
        // Absolute paths
        assert!(is_package_path(
            "/Users/me/project/.hot/cache/cdn/hot.dev/xai/1.0.3/src/xai/collections.hot"
        ));
        // Relative paths (the bug case)
        assert!(is_package_path(
            ".hot/cache/cdn/hot.dev/xai/1.0.3/src/xai/collections.hot"
        ));
        // Other package patterns
        assert!(is_package_path("pkg/lib/src/lib.hot"));
        assert!(is_package_path("/foo/pkg/lib/src/lib.hot"));
        assert!(is_package_path("hot-std/src/core.hot"));
        // User source files
        assert!(!is_package_path("src/app.hot"));
        assert!(!is_package_path("hot/src/myapp/bot.hot"));
    }
}
