//! Compiler artifact types: structured data the compiler extracts from a
//! Hot program and exposes to downstream consumers (engine, build, bundle,
//! event-handler dispatch, etc.).
//!
//! These types are *outputs* of the various `Compiler::extract_*` methods;
//! they live here (rather than in `mod.rs`) so the orchestrator file stays
//! focused on the compile pipeline and bytecode emission.
//!
//! The companion methods that populate these types live in
//! [`super::artifact_extraction`].

use crate::lang::ast::Var;
use crate::val::Val;
use indexmap::IndexMap;

/// Represents an event handler found during compilation
#[derive(Debug, Clone, PartialEq)]
pub struct EventHandler {
    pub event_type: String,
    pub event_handler: Val,
}

/// Normalize a file path for manifest portability.
/// System hot-std paths are normalized to `hot-std:<relative>` format.
/// This ensures manifests are portable across different installations.
fn normalize_manifest_file_path(file: Option<String>) -> Option<String> {
    let file = file?;

    // Known hot-std installation path patterns
    let hot_std_markers = [
        "/usr/local/share/hot/pkg/hot-std/",
        "/usr/share/hot/pkg/hot-std/",
        "/hot/pkg/hot-std/", // Matches HOT_HOME and dev paths
    ];

    // Check if this is a hot-std path and normalize it
    for marker in &hot_std_markers {
        if let Some(idx) = file.find(marker) {
            // Extract the relative path after hot-std/
            let relative = &file[idx + marker.len()..];
            return Some(format!("hot-std:{}", relative));
        }
    }

    // Also handle HOT_HOME-based paths
    if let Ok(hot_home) = std::env::var("HOT_HOME") {
        let hot_std_prefix = format!("{}/pkg/hot-std/", hot_home);
        if let Some(relative) = file.strip_prefix(&hot_std_prefix) {
            return Some(format!("hot-std:{}", relative));
        }
    }

    Some(file)
}

impl EventHandler {
    /// Create a new EventHandler from AST components
    pub fn new(event_type: String, namespace: &str, var_name: &str, var: &Var) -> Self {
        // Extract source location from the Var's src field
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        // Use fully-qualified function name: "::namespace/var"
        let fn_name = format!("{}/{}", namespace, var_name);

        let meta = var
            .meta
            .as_ref()
            .map(|m| m.val.resolve_boxes())
            .unwrap_or(Val::Null);

        let event_handler = crate::val!({
            "fn": fn_name,
            "meta": meta,
            "file": file.map(|f: String| Val::from(f)).unwrap_or(Val::Null),
            "line": line.map(|l: i32| Val::Int(l as i64)).unwrap_or(Val::Null),
            "column": column.map(|c: i32| Val::Int(c as i64)).unwrap_or(Val::Null),
            "position": position.map(|p: i32| Val::Int(p as i64)).unwrap_or(Val::Null),
        });

        EventHandler {
            event_type,
            event_handler,
        }
    }
}

/// Collection of event handlers organized by event type
pub type EventHandlers = IndexMap<String, Vec<EventHandler>>;

/// Represents an MCP tool found during compilation
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub service: String,
    pub name: String,
    pub auth_mode: String,
    pub mcp_tool: Val,
}

impl McpTool {
    /// Create a new McpTool from AST components
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: String,
        name: String,
        auth_mode: String,
        namespace: &str,
        var_name: &str,
        var: &Var,
        description: Option<String>,
        title: Option<String>,
        input_schema: Option<Val>,
        output_schema: Option<Val>,
        icons: Option<Val>,
        annotations: Option<Val>,
    ) -> Self {
        // Extract source location from the Var's src field
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        // Build the MCP tool Val with all fields
        let mut tool_map = indexmap::IndexMap::new();
        tool_map.insert(Val::from("service"), Val::from(service.clone()));
        tool_map.insert(Val::from("name"), Val::from(name.clone()));
        tool_map.insert(Val::from("auth_mode"), Val::from(auth_mode.clone()));
        tool_map.insert(
            Val::from("fn"),
            Val::from(format!("{}/{}", namespace, var_name)),
        );
        if let Some(desc) = description {
            tool_map.insert(Val::from("description"), Val::from(desc));
        }
        if let Some(t) = title {
            tool_map.insert(Val::from("title"), Val::from(t));
        }
        if let Some(schema) = input_schema {
            tool_map.insert(Val::from("input_schema"), schema);
        }
        if let Some(schema) = output_schema {
            tool_map.insert(Val::from("output_schema"), schema);
        }
        if let Some(ic) = icons {
            tool_map.insert(Val::from("icons"), ic);
        }
        if let Some(ann) = annotations {
            tool_map.insert(Val::from("annotations"), ann);
        }
        if let Some(m) = var.meta.as_ref() {
            tool_map.insert(Val::from("meta"), m.val.resolve_boxes());
        }
        if let Some(f) = file {
            tool_map.insert(Val::from("file"), Val::from(f));
        } else {
            tool_map.insert(Val::from("file"), Val::Null);
        }
        if let Some(l) = line {
            tool_map.insert(Val::from("line"), Val::from(l as i64));
        } else {
            tool_map.insert(Val::from("line"), Val::Null);
        }
        if let Some(c) = column {
            tool_map.insert(Val::from("column"), Val::from(c as i64));
        } else {
            tool_map.insert(Val::from("column"), Val::Null);
        }
        if let Some(p) = position {
            tool_map.insert(Val::from("position"), Val::from(p as i64));
        } else {
            tool_map.insert(Val::from("position"), Val::Null);
        }

        let mcp_tool = Val::Map(Box::new(tool_map));

        McpTool {
            service,
            name,
            auth_mode,
            mcp_tool,
        }
    }
}

/// Check if a service name is URL-safe.
/// Allowed characters: alphanumeric, hyphens, underscores, dots.
/// Must not be empty, must not start/end with a dot or hyphen.
pub(super) fn is_url_safe_service_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Must not start or end with dot/hyphen
    if name.starts_with('.') || name.starts_with('-') || name.ends_with('.') || name.ends_with('-')
    {
        return false;
    }
    // All characters must be alphanumeric, hyphen, underscore, or dot
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub(super) fn auto_generate_mcp_tool_name(namespace: &str, var_name: &str) -> String {
    let ns = namespace
        .trim_start_matches("::")
        .replace("::", "_")
        .replace('-', "_");
    let var = var_name.replace('-', "_");

    if ns.is_empty() {
        var
    } else {
        format!("{}_{}", ns, var)
    }
}

/// Harvest the `(display_name, description)` pair for a Hot var
/// from `meta {tool: {...}}`, `meta {mcp: {...}}`, and `meta {doc: ...}`.
///
/// Description chain (first non-empty wins):
///   `meta {tool: {description}}` -> `meta {mcp: {description}}` ->
///   `meta {doc: ...}`.
/// Display-name chain:
///   `meta {tool: {name}}` -> `meta {mcp: {name}}`.
pub(crate) fn harvest_tool_meta(var: &Var) -> (Option<String>, Option<String>) {
    let Some(meta) = var.meta.as_ref() else {
        return (None, None);
    };
    let Val::Map(meta_map) = &meta.val else {
        return (None, None);
    };

    let nested = |key: &str, sub: &str| -> Option<String> {
        match meta_map.get(&Val::from(key)) {
            Some(Val::Map(m)) => match m.get(&Val::from(sub)) {
                Some(Val::Str(s)) => Some(s.trim().to_string()),
                _ => None,
            },
            _ => None,
        }
    };

    let display_name = nested("tool", "name").or_else(|| nested("mcp", "name"));

    let description = nested("tool", "description")
        .or_else(|| nested("mcp", "description"))
        .or_else(|| match meta_map.get(&Val::from("doc")) {
            Some(Val::Str(d)) => Some(d.trim().to_string()),
            _ => None,
        });

    (display_name, description)
}

/// Collection of MCP tools organized by service
pub type McpTools = IndexMap<String, Vec<McpTool>>;

/// Represents a webhook found during compilation
#[derive(Debug, Clone, PartialEq)]
pub struct Webhook {
    pub service: String,
    pub path: String,
    pub method: String,
    pub name: String,
    pub webhook: Val,
}

impl Webhook {
    /// Create a new Webhook from AST components
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: String,
        path: String,
        method: String,
        name: String,
        namespace: &str,
        var_name: &str,
        var: &Var,
        description: Option<String>,
        auth_mode: Option<String>,
    ) -> Self {
        // Extract source location from the Var's src field
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        // Build the webhook endpoint Val with all fields
        let mut endpoint_map = indexmap::IndexMap::new();
        endpoint_map.insert(Val::from("service"), Val::from(service.clone()));
        endpoint_map.insert(Val::from("path"), Val::from(path.clone()));
        endpoint_map.insert(Val::from("method"), Val::from(method.clone()));
        endpoint_map.insert(Val::from("name"), Val::from(name.clone()));
        endpoint_map.insert(
            Val::from("fn"),
            Val::from(format!("{}/{}", namespace, var_name)),
        );
        if let Some(desc) = description {
            endpoint_map.insert(Val::from("description"), Val::from(desc));
        }
        let auth = auth_mode.unwrap_or_else(|| "none".to_string());
        endpoint_map.insert(Val::from("auth_mode"), Val::from(auth));
        if let Some(m) = var.meta.as_ref() {
            endpoint_map.insert(Val::from("meta"), m.val.resolve_boxes());
        }
        if let Some(f) = file {
            endpoint_map.insert(Val::from("file"), Val::from(f));
        } else {
            endpoint_map.insert(Val::from("file"), Val::Null);
        }
        if let Some(l) = line {
            endpoint_map.insert(Val::from("line"), Val::from(l as i64));
        } else {
            endpoint_map.insert(Val::from("line"), Val::Null);
        }
        if let Some(c) = column {
            endpoint_map.insert(Val::from("column"), Val::from(c as i64));
        } else {
            endpoint_map.insert(Val::from("column"), Val::Null);
        }
        if let Some(p) = position {
            endpoint_map.insert(Val::from("position"), Val::from(p as i64));
        } else {
            endpoint_map.insert(Val::from("position"), Val::Null);
        }

        let webhook = Val::Map(Box::new(endpoint_map));

        Webhook {
            service,
            path,
            method,
            name,
            webhook,
        }
    }
}

pub(super) fn auto_generate_webhook_name(namespace: &str, var_name: &str) -> String {
    let ns = namespace.trim_start_matches("::").replace("::", "_");
    let var = var_name.replace('-', "_");

    if ns.is_empty() {
        var
    } else {
        format!("{}_{}", ns, var)
    }
}

/// Collection of webhooks organized by service
pub type Webhooks = IndexMap<String, Vec<Webhook>>;

/// Represents a scheduled function found during compilation
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledFunction {
    pub cron_expression: String,
    pub scheduled_function: Val,
}

impl ScheduledFunction {
    /// Create a new ScheduledFunction from AST components
    pub fn new(cron_expression: String, namespace: &str, var_name: &str, var: &Var) -> Self {
        // Extract source location from the Var's src field
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        // Use fully-qualified function name: "::namespace/var"
        let fn_name = format!("{}/{}", namespace, var_name);

        let meta = var
            .meta
            .as_ref()
            .map(|m| m.val.resolve_boxes())
            .unwrap_or(Val::Null);

        let scheduled_function = crate::val!({
            "fn": fn_name,
            "meta": meta,
            "file": file.map(|f: String| Val::from(f)).unwrap_or(Val::Null),
            "line": line.map(|l: i32| Val::Int(l as i64)).unwrap_or(Val::Null),
            "column": column.map(|c: i32| Val::Int(c as i64)).unwrap_or(Val::Null),
            "position": position.map(|p: i32| Val::Int(p as i64)).unwrap_or(Val::Null),
        });

        ScheduledFunction {
            cron_expression,
            scheduled_function,
        }
    }
}

/// Collection of scheduled functions organized by cron expression
pub type ScheduledFunctions = IndexMap<String, Vec<ScheduledFunction>>;

/// Represents an agent type definition found during compilation
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub type_name: String,
    pub namespace: String,
    pub agent_val: Val,
}

impl AgentDef {
    pub fn new(
        type_name: String,
        namespace: &str,
        var: &Var,
        agent_meta: &indexmap::IndexMap<Val, Val>,
        config_fields: Option<&[crate::lang::ast::TypeField]>,
    ) -> Self {
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        let name = agent_meta.get(&Val::from("name")).and_then(|v| match v {
            Val::Str(s) => Some(Val::from((**s).to_owned())),
            _ => None,
        });

        // Description: agent.description > top-level doc
        let description = agent_meta
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some(Val::from(s.trim().to_string())),
                _ => None,
            })
            .or_else(|| {
                var.meta.as_ref().and_then(|m| {
                    if let Val::Map(meta_map) = &m.val
                        && let Some(Val::Str(doc)) = meta_map.get(&Val::from("doc"))
                    {
                        return Some(Val::from(doc.trim().to_string()));
                    }
                    None
                })
            });

        let tags = agent_meta.get(&Val::from("tags")).cloned();

        // Harvest external MCP server specs declared on the agent type.
        // Runtime composition (`::mcp::ai/options-with-mcp`) resolves these
        // lazily — they may be Map literals, Var/Fn references, or single-key
        // wrappers. We can't *call* fn refs at compile time, so the harvest is
        // a structural normalization: each entry becomes a small map of the
        // form `{kind, name?, url?, target?}` that the inspector / dashboard
        // can render and that downstream tools can use to enumerate the MCP
        // surface area of an agent without re-walking the type's meta blob.
        let mcp_servers = agent_meta
            .get(&Val::from("mcp-servers"))
            .and_then(Self::summarize_mcp_servers);

        // Build config_fields from struct fields
        let fields_val = config_fields.map(|fields| {
            let mut fields_map = indexmap::IndexMap::new();
            for field in fields {
                fields_map.insert(
                    Val::from(field.name.to_string()),
                    Val::from(field.type_annotation.clone()),
                );
            }
            Val::Map(Box::new(fields_map))
        });

        // Full meta from the variable
        let meta = var
            .meta
            .as_ref()
            .and_then(|m| serde_json::to_value(&m.val).ok())
            .map(|j| serde_json::from_value::<Val>(j).unwrap_or(Val::Null));

        let mut agent_map = indexmap::IndexMap::new();
        agent_map.insert(Val::from("type_name"), Val::from(type_name.clone()));
        agent_map.insert(Val::from("namespace"), Val::from(namespace.to_string()));
        if let Some(n) = name {
            agent_map.insert(Val::from("name"), n);
        }
        if let Some(d) = description {
            agent_map.insert(Val::from("description"), d);
        }
        if let Some(t) = tags {
            agent_map.insert(Val::from("tags"), t);
        }
        if let Some(servers) = mcp_servers {
            agent_map.insert(Val::from("mcp_servers"), servers);
        }
        if let Some(f) = fields_val {
            agent_map.insert(Val::from("config_fields"), f);
        }
        if let Some(m) = meta {
            agent_map.insert(Val::from("meta"), m);
        }
        if let Some(f) = file {
            agent_map.insert(Val::from("file"), Val::from(f));
        }
        if let Some(l) = line {
            agent_map.insert(Val::from("line"), Val::from(l as i64));
        }
        if let Some(c) = column {
            agent_map.insert(Val::from("column"), Val::from(c as i64));
        }
        if let Some(p) = position {
            agent_map.insert(Val::from("position"), Val::from(p as i64));
        }

        AgentDef {
            type_name,
            namespace: namespace.to_string(),
            agent_val: Val::Map(Box::new(agent_map)),
        }
    }

    /// Normalize the `mcp-servers` Vec from agent meta into a Vec of summary
    /// maps for inspector/UI surfacing. Returns None if `mcp-servers` is
    /// missing or not a Vec; returns an empty Vec for `mcp-servers: []`.
    /// Per-entry shapes that we can statically classify:
    ///   - Map with `url`             → `{kind: "map", name?, url}`
    ///   - Single-key map `{n: ...}`  → `{kind: "named", name: n, inner: ...}`
    ///   - Var/Fn ref                  → `{kind: "ref", target: <name>}`
    ///   - Anything else               → `{kind: "unknown"}` (logged as warn)
    pub(super) fn summarize_mcp_servers(servers_val: &Val) -> Option<Val> {
        let Val::Vec(items) = servers_val else {
            if !matches!(servers_val, Val::Null) {
                tracing::warn!(
                    "agent meta `mcp-servers` must be a Vec, got: {:?}",
                    servers_val
                );
            }
            return None;
        };
        let mut out: Vec<Val> = Vec::with_capacity(items.len());
        for item in items {
            out.push(Self::summarize_mcp_entry(item));
        }
        Some(Val::Vec(out))
    }

    fn summarize_mcp_entry(entry: &Val) -> Val {
        match entry {
            Val::Map(map) => {
                // Single-key wrapper: {<name>: <inner config or fn>} — but only
                // when the inner value is itself a Map or a function/var ref.
                // (`{url: "..."}` is technically a single-key map but the value
                // is a Str, so we treat it as a config map.)
                if map.len() == 1
                    && let Some((only_key, inner_val)) = map.iter().next()
                {
                    let inner_is_callable_or_map =
                        matches!(inner_val, Val::Map(_)) || Self::val_is_fn_or_var_ref(inner_val);
                    if inner_is_callable_or_map {
                        let name_str = match only_key {
                            Val::Str(s) => (**s).to_owned(),
                            _ => format!("{:?}", only_key),
                        };
                        let mut summary = indexmap::IndexMap::new();
                        summary.insert(Val::from("kind"), Val::from("named"));
                        summary.insert(Val::from("name"), Val::from(name_str));
                        summary.insert(Val::from("inner"), Self::summarize_mcp_entry(inner_val));
                        return Val::Map(Box::new(summary));
                    }
                }
                // Bare config map: surface url if present.
                let url = map.get(&Val::from("url")).and_then(|v| match v {
                    Val::Str(s) => Some(Val::from((**s).to_owned())),
                    _ => None,
                });
                let mut summary = indexmap::IndexMap::new();
                summary.insert(Val::from("kind"), Val::from("map"));
                if let Some(u) = url {
                    summary.insert(Val::from("url"), u);
                }
                Val::Map(Box::new(summary))
            }
            other if Self::val_is_fn_or_var_ref(other) => {
                let mut summary = indexmap::IndexMap::new();
                summary.insert(Val::from("kind"), Val::from("ref"));
                if let Some(target) = Self::extract_ref_target(other) {
                    summary.insert(Val::from("target"), Val::from(target));
                }
                Val::Map(Box::new(summary))
            }
            other => {
                tracing::warn!(
                    "agent meta `mcp-servers` entry is not a Map or callable ref: {:?}",
                    other
                );
                let mut summary = indexmap::IndexMap::new();
                summary.insert(Val::from("kind"), Val::from("unknown"));
                Val::Map(Box::new(summary))
            }
        }
    }

    /// Best-effort detection of "this value names a function or variable
    /// reference" without forcing the parser/AST module surface here. The
    /// agent meta map is built by parsing meta literals; bare identifiers are
    /// normally Strings (the symbol name) until they're resolved at compile
    /// time. We accept both Box<AstNode>-wrapped Refs and plain Strs that
    /// look like qualified function names.
    fn val_is_fn_or_var_ref(val: &Val) -> bool {
        match val {
            Val::Box(b) => {
                if let Some(crate::lang::ast::AstNode(value)) =
                    b.as_any().downcast_ref::<crate::lang::ast::AstNode>()
                {
                    matches!(value, crate::lang::ast::Value::Ref(_))
                } else {
                    false
                }
            }
            // A literal Str whose contents look like a qualified function name
            // (`::ns/name`) — also treated as a ref so users can write
            // `mcp-servers: [::env/weather-config]` and have it surface in the
            // inspector. Bare unqualified names are ambiguous (could be plain
            // strings) so we only match the qualified form.
            Val::Str(s) => s.starts_with("::") && s.contains('/'),
            _ => false,
        }
    }

    fn extract_ref_target(val: &Val) -> Option<String> {
        match val {
            Val::Box(b) => {
                if let Some(crate::lang::ast::AstNode(crate::lang::ast::Value::Ref(refv))) =
                    b.as_any().downcast_ref::<crate::lang::ast::AstNode>()
                {
                    Some(format!("{:?}", refv))
                } else {
                    None
                }
            }
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        }
    }
}

/// Collection of agent definitions
pub type AgentDefs = Vec<AgentDef>;

/// Represents a named workflow type definition found during compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDef {
    pub type_name: String,
    pub namespace: String,
    pub workflow_val: Val,
}

impl WorkflowDef {
    pub fn new(
        type_name: String,
        namespace: &str,
        var: &Var,
        workflow_meta: &indexmap::IndexMap<Val, Val>,
    ) -> Self {
        let (file, line, column, position) = if let Some(ref src) = var.src {
            (
                normalize_manifest_file_path(src.file.clone()),
                Some(src.line as i32),
                Some(src.column as i32),
                Some(src.position as i32),
            )
        } else {
            (None, None, None, None)
        };

        let name = workflow_meta.get(&Val::from("name")).and_then(|v| match v {
            Val::Str(s) => Some(Val::from((**s).to_owned())),
            _ => None,
        });

        // Description: workflow.description > top-level doc.
        let description = workflow_meta
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some(Val::from(s.trim().to_string())),
                _ => None,
            })
            .or_else(|| {
                var.meta.as_ref().and_then(|m| {
                    if let Val::Map(meta_map) = &m.val
                        && let Some(Val::Str(doc)) = meta_map.get(&Val::from("doc"))
                    {
                        return Some(Val::from(doc.trim().to_string()));
                    }
                    None
                })
            });

        let tags = workflow_meta.get(&Val::from("tags")).cloned();

        // Full meta from the variable.
        let meta = var
            .meta
            .as_ref()
            .and_then(|m| serde_json::to_value(&m.val).ok())
            .map(|j| serde_json::from_value::<Val>(j).unwrap_or(Val::Null));

        let mut workflow_map = indexmap::IndexMap::new();
        workflow_map.insert(Val::from("type_name"), Val::from(type_name.clone()));
        workflow_map.insert(Val::from("namespace"), Val::from(namespace.to_string()));
        if let Some(n) = name {
            workflow_map.insert(Val::from("name"), n);
        }
        if let Some(d) = description {
            workflow_map.insert(Val::from("description"), d);
        }
        if let Some(t) = tags {
            workflow_map.insert(Val::from("tags"), t);
        }
        if let Some(m) = meta {
            workflow_map.insert(Val::from("meta"), m);
        }
        if let Some(f) = file {
            workflow_map.insert(Val::from("file"), Val::from(f));
        }
        if let Some(l) = line {
            workflow_map.insert(Val::from("line"), Val::from(l as i64));
        }
        if let Some(c) = column {
            workflow_map.insert(Val::from("column"), Val::from(c as i64));
        }
        if let Some(p) = position {
            workflow_map.insert(Val::from("position"), Val::from(p as i64));
        }

        WorkflowDef {
            type_name,
            namespace: namespace.to_string(),
            workflow_val: Val::Map(Box::new(workflow_map)),
        }
    }
}

/// Collection of named workflow definitions.
pub type WorkflowDefs = Vec<WorkflowDef>;

/// Represents a statically-detected `send()` call in a handler function body.
#[derive(Debug, Clone, PartialEq)]
pub struct SendTarget {
    pub event_name: String,
    pub namespace: String,
    pub var_name: String,
    pub source: SendTargetSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SendTargetSource {
    Static,
}

/// Collected send targets keyed by `namespace/var_name`.
pub type SendTargets = IndexMap<String, Vec<SendTarget>>;
