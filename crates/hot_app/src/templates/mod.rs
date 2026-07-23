use askama::Template;
use chrono::Datelike;
use hot::db::{Run, User, run::RunStatus};
use hot::val::{Val, ValFormat};
use once_cell::sync::Lazy;
use serde_json::Value as JsonValue;
use std::fs;
use std::sync::RwLock;
use uuid::Uuid;

/// Serialize data for embedding in an HTML `<script>` element.
///
/// JSON escaping alone does not prevent a string containing `</script>` from
/// ending the element, so escape HTML-significant characters and JS line
/// separators without changing the parsed JSON value.
pub(crate) fn script_safe_json<T: serde::Serialize>(value: &T, fallback: &str) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| fallback.to_string())
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Human-readable size for blob summaries (e.g. "5.2 MB").
fn format_blob_size(size: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = size.max(0) as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", size as i64, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// Replace `::hot::blob/BlobRef` typed maps with a compact one-line summary
/// string so spilled payloads render as previews instead of raw ref maps.
/// The full content is available via the blob download API by ref id.
pub(crate) fn summarize_blob_refs_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let is_blob_ref = matches!(
                map.get("$type"),
                Some(JsonValue::String(t)) if t == hot::blob::BLOB_REF_TYPE
            );
            if is_blob_ref {
                let inner = map.get("$val").and_then(|v| v.as_object());
                let get_str = |key: &str| {
                    inner
                        .and_then(|m| m.get(key))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                let size = inner
                    .and_then(|m| m.get("size"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let preview = get_str("preview");
                let mut summary = format!("#blob[{}", format_blob_size(size));
                let content_type = get_str("content-type");
                if !content_type.is_empty() {
                    summary.push_str(&format!(" {}", content_type));
                }
                let id = get_str("id");
                if !id.is_empty() {
                    summary.push_str(&format!(" ref={}", id));
                }
                if !preview.is_empty() {
                    summary.push_str(&format!(" preview={:?}", preview));
                }
                summary.push(']');
                return JsonValue::String(summary);
            }
            JsonValue::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), summarize_blob_refs_json(v)))
                    .collect(),
            )
        }
        JsonValue::Array(items) => {
            JsonValue::Array(items.iter().map(summarize_blob_refs_json).collect())
        }
        other => other.clone(),
    }
}

/// Format a JSON value as a Hot literal string
/// This is used for displaying results in the UI with Hot syntax (default)
fn format_json_as_hot_literal(value: &JsonValue, indent: usize) -> String {
    // Render spilled blob refs as compact previews rather than raw ref maps.
    let summarized;
    let value = if hot::blob::json_contains_blob_ref(value) {
        summarized = summarize_blob_refs_json(value);
        &summarized
    } else {
        value
    };
    // Convert JSON to Val and use the Hot format
    if let Ok(val) = serde_json::from_value::<Val>(value.clone()) {
        if indent == 0 {
            val.format(ValFormat::Hot)
        } else {
            val.format_hot(indent)
        }
    } else {
        // Fallback to JSON string if conversion fails
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
    }
}

// Global context to store assets prefix
static ASSETS_PREFIX: Lazy<RwLock<String>> =
    Lazy::new(|| RwLock::new(crate::server::ASSETS_URL_PREFIX.to_string()));

// Initialize the assets prefix
pub fn init_assets_prefix(prefix: String) {
    let mut assets = ASSETS_PREFIX.write().unwrap();
    *assets = prefix;
}

// Get the assets prefix for templates
pub fn get_assets_prefix() -> String {
    let assets = ASSETS_PREFIX.read().unwrap();
    assets.clone()
}

// Generate cache busting parameter for CSS
pub fn get_css_cache_buster() -> String {
    let assets_prefix = get_assets_prefix();
    let css_path = format!("{}css/styles.css", assets_prefix.trim_start_matches('/'));

    // Try to get the file's last modified time
    if let Ok(metadata) = fs::metadata(&css_path)
        && let Ok(modified) = metadata.modified()
        && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        return format!("?v={}", duration.as_secs());
    }

    // Fallback to current timestamp if file doesn't exist or can't read metadata
    let now = std::time::SystemTime::now();
    if let Ok(duration) = now.duration_since(std::time::UNIX_EPOCH) {
        format!("?v={}", duration.as_secs())
    } else {
        "?v=1".to_string()
    }
}

// Get the cache-busted CSS URL
pub fn get_css_url() -> String {
    let assets_prefix = get_assets_prefix();
    let cache_buster = get_css_cache_buster();
    format!("{}css/styles.css{}", assets_prefix, cache_buster)
}

// Get cache buster string for assets
// - Debug builds: use timestamp for immediate cache busting during development
// - Release builds: use git SHA (consistent across servers, changes on deploy)
fn get_cache_buster() -> String {
    if cfg!(debug_assertions) {
        // Dev mode: use current timestamp so changes are visible immediately
        let now = std::time::SystemTime::now();
        if let Ok(duration) = now.duration_since(std::time::UNIX_EPOCH) {
            return format!("?v={}", duration.as_secs());
        }
    }

    // Release mode: use first 8 chars of git SHA
    let sha = crate::build_info::GIT_SHA;
    let short_sha = &sha[..8.min(sha.len())];
    format!("?v={}", short_sha)
}

// Get a cache-busted asset URL for any asset path
pub fn get_asset_url(path: &str) -> String {
    let assets_prefix = get_assets_prefix();
    format!("{}{}{}", assets_prefix, path, get_cache_buster())
}

// Get the current year for copyright text
pub fn get_current_year() -> i32 {
    let now = chrono::Local::now();
    now.year()
}

// Breadcrumb structure
#[derive(Debug, Clone)]
pub struct BreadcrumbItem {
    pub text: String,
    pub link: Option<String>, // None for the last item (non-clickable)
}

pub type Breadcrumbs = Vec<BreadcrumbItem>;

impl BreadcrumbItem {
    pub fn new(text: String, link: Option<String>) -> Self {
        Self { text, link }
    }

    pub fn clickable(text: String, link: String) -> Self {
        Self {
            text,
            link: Some(link),
        }
    }

    /// Alias for clickable - creates a breadcrumb with a link
    pub fn link(text: String, link: String) -> Self {
        Self::clickable(text, link)
    }

    pub fn current(text: String) -> Self {
        Self { text, link: None }
    }
}

// Helper function to build base breadcrumbs for pages that include environment
pub fn build_base_breadcrumbs_with_env(_session: &crate::auth::Session) -> Breadcrumbs {
    // Org/env context is now shown via dropdowns in the breadcrumb bar,
    // so the base breadcrumb no longer includes org/env text.
    Vec::new()
}

// Helper function to build base breadcrumbs for pages that don't include environment
pub fn build_base_breadcrumbs_without_env(_session: &crate::auth::Session) -> Breadcrumbs {
    // Org context is now shown via a dropdown in the breadcrumb bar,
    // so the base breadcrumb no longer includes org text.
    Vec::new()
}

// Helper function to truncate text for display
pub fn truncate_text(text: &Option<serde_json::Value>, max_len: usize) -> String {
    match text {
        Some(value) => {
            let text_str = value.to_string();
            truncate_string(&text_str, max_len)
        }
        None => "null".to_string(),
    }
}

/// Truncate a string to a maximum length, adding ellipsis if truncated
pub fn truncate_string(text: &str, max_len: usize) -> String {
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(max_len).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        text.to_string()
    }
}

// Helper function to format values for display
pub fn format_value_display(value: &Option<serde_json::Value>, raw_mode: &bool) -> String {
    crate::handlers::format_value_for_display(value, *raw_mode)
}

// Helper function to build URLs with query parameters
pub fn build_url_with_params(
    base_url: &str,
    page: Option<&i64>,
    raw_mode: &bool,
    inspect_mode: &bool,
) -> String {
    let mut params = Vec::new();

    if let Some(p) = page
        && *p != 1
    {
        params.push(format!("p={}", p));
    }

    if *raw_mode {
        params.push("raw=1".to_string());
    }

    if *inspect_mode {
        params.push("inspect=1".to_string());
    }

    if params.is_empty() {
        base_url.to_string()
    } else {
        format!("{}?{}", base_url, params.join("&"))
    }
}

// Askama custom filters
pub mod filters {
    use std::fmt;

    /// Round a float to the nearest integer
    #[askama::filter_fn]
    pub fn round<T: fmt::Display>(value: T, _: &dyn askama::Values) -> askama::Result<String> {
        if let Ok(num) = value.to_string().parse::<f64>() {
            Ok(num.round().to_string())
        } else {
            Ok(value.to_string())
        }
    }

    /// Format a UTC datetime in the user's display timezone, appending the timezone abbreviation.
    ///
    /// Usage in templates:
    ///   {{ dt|tz(page_context.display_timezone, page_context.timezone_abbreviation, "%Y-%m-%d %H:%M:%S") }}
    #[askama::filter_fn]
    pub fn tz(
        dt: &chrono::DateTime<chrono::Utc>,
        _: &dyn askama::Values,
        timezone: &str,
        tz_abbr: &str,
        fmt: &str,
    ) -> askama::Result<String> {
        let formatted = crate::timezone::format_in_timezone(dt, timezone, fmt);
        Ok(format!("{} {}", formatted, tz_abbr))
    }

    /// Format a UTC datetime in the user's display timezone without appending an abbreviation.
    ///
    /// Usage in templates:
    ///   {{ dt|tzf(page_context.display_timezone, "%Y-%m-%d") }}
    #[askama::filter_fn]
    pub fn tzf(
        dt: &chrono::DateTime<chrono::Utc>,
        _: &dyn askama::Values,
        timezone: &str,
        fmt: &str,
    ) -> askama::Result<String> {
        Ok(crate::timezone::format_in_timezone(dt, timezone, fmt))
    }

    /// Shorten a UUID to its last 12 hex characters (hyphens removed).
    ///
    /// Usage in templates:
    ///   {{ some_id|shortuuid }}
    #[askama::filter_fn]
    pub fn shortuuid<T: std::fmt::Display>(
        value: T,
        _: &dyn askama::Values,
    ) -> askama::Result<String> {
        let s = value.to_string();
        let no_hyphens: String = s.chars().filter(|c| *c != '-').collect();
        if no_hyphens.len() > 12 {
            Ok(no_hyphens[no_hyphens.len() - 12..].to_string())
        } else {
            Ok(no_hyphens)
        }
    }

    /// Format an integer with comma thousands separators.
    ///
    /// Usage in templates:
    ///   {{ count|commafy }}
    #[askama::filter_fn]
    pub fn commafy<T: std::fmt::Display>(
        value: T,
        _: &dyn askama::Values,
    ) -> askama::Result<String> {
        let s = value.to_string();
        let bytes = s.as_bytes();
        let is_negative = bytes.first() == Some(&b'-');
        let digits = if is_negative { &s[1..] } else { &s };
        let mut result = String::with_capacity(s.len() + digits.len() / 3);
        if is_negative {
            result.push('-');
        }
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                result.push(',');
            }
            result.push(c);
        }
        Ok(result)
    }
}

/// Helper function to get shortened UUID string (last 12 characters without hyphens)
pub fn get_uuid_short(uuid: &uuid::Uuid) -> String {
    let uuid_str = uuid.to_string();
    let uuid_no_hyphens = uuid_str.replace('-', "");
    if uuid_no_hyphens.len() >= 12 {
        uuid_no_hyphens[uuid_no_hyphens.len() - 12..].to_string()
    } else {
        uuid_no_hyphens
    }
}

/// Represents a node in the run graph for ECharts visualization
#[derive(serde::Serialize, Debug, Clone)]
pub struct GraphNodeData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(serde::Serialize, Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub node_type: String,      // "run", "event", "task"
    pub status: Option<String>, // For runs: "running", "succeeded", etc. None for events
    pub text_color: String,     // Color for the text based on status/theme
    pub x: f64,
    pub y: f64,
    pub symbol_size: Vec<f64>,  // [width, height] for rectangular nodes
    pub result: Option<String>, // Stringified result for runs (for search)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_current: Option<bool>, // Whether this is the current/focus node
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_us: Option<i64>, // Queue wait time in microseconds (run nodes only)
}

#[derive(serde::Serialize, Debug, Clone)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
}

impl GraphNodeData {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

/// Display info for access attribution — who/what triggered an action
#[derive(Debug, Clone)]
pub struct AccessInfo {
    pub access_id: Uuid,
    /// "api_key", "service_key", "session", or "scheduler"
    pub credential_type: String,
    /// Display label: key name, session id prefix, or "API Key"
    pub credential_label: String,
    /// The credential ID (api_key_id, service_key_id, or session_id)
    pub credential_id: Option<Uuid>,
    pub source: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub created_at: String,
}

impl AccessInfo {
    /// Build an AccessInfo from an Access record, optionally looking up credential names
    pub fn from_access(
        access: &hot::db::access::Access,
        api_key_name: Option<&str>,
        service_key_name: Option<&str>,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let (credential_type, credential_label, credential_id) =
            if let Some(session_id) = &access.session_id {
                (
                    "session".to_string(),
                    format!("Session {}", &session_id.to_string()[..8]),
                    Some(*session_id),
                )
            } else if let Some(service_key_id) = &access.service_key_id {
                (
                    "service_key".to_string(),
                    service_key_name.map(|n| n.to_string()).unwrap_or_else(|| {
                        format!("Service Key {}", &service_key_id.to_string()[..8])
                    }),
                    Some(*service_key_id),
                )
            } else if let Some(api_key_id) = &access.api_key_id {
                (
                    "api_key".to_string(),
                    api_key_name
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| format!("API Key {}", &api_key_id.to_string()[..8])),
                    Some(*api_key_id),
                )
            } else {
                (access.source.clone(), access.source.clone(), None)
            };

        let created_at = format!(
            "{} {}",
            crate::timezone::format_in_timezone(&access.created_at, timezone, "%Y-%m-%d %H:%M:%S"),
            tz_abbr
        );

        Self {
            access_id: access.access_id,
            credential_type,
            credential_label,
            credential_id,
            source: access.source.clone(),
            ip_address: access.ip_address.clone(),
            user_agent: access.user_agent.clone(),
            method: access.method.clone(),
            path: access.path.clone(),
            created_at,
        }
    }
}

// Simple helper struct for run display
#[derive(Debug, Clone)]
pub struct RunDisplay {
    pub run_id: Uuid,
    pub stream_id: Uuid,
    pub env_id: Uuid,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub stop_time: Option<chrono::DateTime<chrono::Utc>>,
    pub start_time_formatted: String,
    pub stop_time_formatted: String,
    pub duration_formatted: String, // Total time (queue wait + execution) for display
    pub duration_us: i64,           // Total time in microseconds (queue wait + execution)
    pub exec_time_formatted: String, // Execution time only (stop - start)
    pub exec_time_us: i64,          // Execution time in microseconds
    pub status: String,
    pub run_type: String,
    pub is_completed: bool,
    pub by_user_id: Option<Uuid>,
    pub origin_run_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub project_name: String,        // Empty string or "N/A" if no project
    pub result: Option<String>,      // Hot-formatted result string
    pub result_json: Option<String>, // JSON result string for format toggle
    pub event_fn: Option<String>,    // Function that triggered this run (from event data)
    // Retry fields
    pub retry_attempt: i16, // Current retry attempt (0 = first try)
    pub next_retry_at: Option<chrono::DateTime<chrono::Utc>>, // Next retry time
    pub is_retry: bool,     // True if this is a retry run (retry_attempt > 0)
    // Queue timing - time between event enqueue and run start
    pub queue_wait_us: Option<i64>, // Microseconds spent waiting in queue (event-triggered runs only)
    pub queue_wait_formatted: Option<String>, // Formatted queue wait time
    pub queued_at: Option<chrono::DateTime<chrono::Utc>>, // When event was enqueued
    pub queued_at_formatted: Option<String>, // Formatted event time
}

#[derive(Debug, Clone)]
pub struct TaskDisplay {
    pub task_id: Uuid,
    pub stream_id: Uuid,
    pub env_id: Uuid,
    pub origin_run_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub function_name: String,
    pub task_type: String,
    pub status: String,
    pub created_at_formatted: String,
    pub start_time_formatted: String,
    pub stop_time_formatted: String,
    pub duration_formatted: String,
    pub duration_ms: Option<i64>,
    pub result: Option<String>,
    pub result_json: Option<String>,
    // Container-specific fields parsed from result JSON
    pub exit_code: Option<i64>,
    pub size: Option<String>,
    pub compute_units: Option<i64>,
    pub cus_multiplier: Option<f64>,
    pub slot_wait_ms: Option<i64>,
    pub image_pull_ms: Option<i64>,
    pub execution_ms: Option<i64>,
    pub logs_collect_ms: Option<i64>,
    pub container_id: Option<String>,
    pub backend: Option<String>,
    pub infra_failure: bool,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub origin_run_fn: Option<String>,
    pub args_hot: Option<String>,
    pub args_json: Option<String>,
    pub container_image: Option<String>,
    pub container_cmd_snippet: Option<String>,
    pub container_cmd_full: Option<String>,
}

impl TaskDisplay {
    pub fn from_with_timezone(task: &hot::db::Task, timezone: &str, tz_abbr: &str) -> Self {
        let created_at_formatted = format!(
            "{} {}",
            crate::timezone::format_in_timezone(&task.created_at, timezone, "%Y-%m-%d %H:%M:%S"),
            tz_abbr
        );

        let start_time_formatted = task
            .start_time
            .map(|t| {
                format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(&t, timezone, "%Y-%m-%d %H:%M:%S"),
                    tz_abbr
                )
            })
            .unwrap_or_else(|| "-".to_string());

        let stop_time_formatted = task
            .stop_time
            .map(|t| {
                format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(&t, timezone, "%Y-%m-%d %H:%M:%S"),
                    tz_abbr
                )
            })
            .unwrap_or_else(|| "-".to_string());

        let duration_formatted = task
            .duration_ms
            .map(|ms| {
                if ms < 1000 {
                    format!("{}ms", ms)
                } else if ms < 60_000 {
                    format!("{:.1}s", ms as f64 / 1000.0)
                } else {
                    format!("{:.1}m", ms as f64 / 60_000.0)
                }
            })
            .unwrap_or_else(|| "-".to_string());

        let result = task
            .result
            .as_ref()
            .map(|r| format_json_as_hot_literal(r, 0));

        let result_json = task
            .result
            .as_ref()
            .map(|r| serde_json::to_string_pretty(r).unwrap_or_else(|_| r.to_string()));

        // Parse container-specific fields from result JSON
        // Success: result is direct object. Failure: result is { $type, $val: { msg, err } }
        let result_obj = task.result.as_ref().and_then(|v| {
            if let Some(obj) = v.as_object() {
                if obj.contains_key("exit-code") || obj.contains_key("compute-units") {
                    return Some(obj);
                }
                if let Some(val) = obj.get("$val").and_then(|v| v.as_object())
                    && let Some(err) = val.get("err").and_then(|e| e.as_object())
                {
                    return Some(err);
                }
            }
            None
        });

        let exit_code = result_obj.and_then(|r| r.get("exit-code").and_then(|v| v.as_i64()));
        let size =
            result_obj.and_then(|r| r.get("size").and_then(|v| v.as_str().map(String::from)));
        let compute_units =
            result_obj.and_then(|r| r.get("compute-units").and_then(|v| v.as_i64()));
        let cus_multiplier =
            result_obj.and_then(|r| r.get("cus-multiplier").and_then(|v| v.as_f64()));
        let slot_wait_ms = result_obj.and_then(|r| r.get("slot-wait-ms").and_then(|v| v.as_i64()));
        let image_pull_ms =
            result_obj.and_then(|r| r.get("image-pull-ms").and_then(|v| v.as_i64()));
        let execution_ms = result_obj.and_then(|r| r.get("execution-ms").and_then(|v| v.as_i64()));
        let logs_collect_ms =
            result_obj.and_then(|r| r.get("logs-collect-ms").and_then(|v| v.as_i64()));
        let container_id = result_obj.and_then(|r| {
            r.get("container-id")
                .and_then(|v| v.as_str().map(String::from))
        });
        let backend =
            result_obj.and_then(|r| r.get("backend").and_then(|v| v.as_str().map(String::from)));
        let infra_failure = result_obj
            .and_then(|r| r.get("infra-failure").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        let stdout =
            result_obj.and_then(|r| r.get("stdout").and_then(|v| v.as_str().map(String::from)));
        let stderr =
            result_obj.and_then(|r| r.get("stderr").and_then(|v| v.as_str().map(String::from)));

        let origin_run_fn = task.origin_run_fn.clone();

        let args_hot = task.args.as_ref().map(|a| format_json_as_hot_literal(a, 0));

        let args_json = task
            .args
            .as_ref()
            .map(|a| serde_json::to_string_pretty(a).unwrap_or_else(|_| a.to_string()));

        let args_obj = task.args.as_ref().and_then(|v| v.as_object());
        let container_image =
            args_obj.and_then(|a| a.get("image").and_then(|v| v.as_str().map(String::from)));
        let container_cmd_full = args_obj.and_then(|a| {
            a.get("cmd")
                .and_then(|v| v.as_str().map(String::from))
                .or_else(|| a.get("script").and_then(|v| v.as_str().map(String::from)))
        });
        let container_cmd_snippet = container_cmd_full.as_ref().map(|cmd| {
            let first_line = cmd.lines().next().unwrap_or(cmd);
            if first_line.len() > 60 {
                format!("{}…", &first_line[..57])
            } else if cmd.lines().count() > 1 {
                format!("{}…", first_line)
            } else {
                first_line.to_string()
            }
        });

        Self {
            task_id: task.task_id,
            stream_id: task.stream_id,
            env_id: task.env_id,
            origin_run_id: task.origin_run_id,
            run_id: task.run_id,
            function_name: task.function_name.clone(),
            task_type: task.task_type.clone(),
            status: task.status.clone(),
            created_at_formatted,
            start_time_formatted,
            stop_time_formatted,
            duration_formatted,
            duration_ms: task.duration_ms,
            result,
            result_json,
            exit_code,
            size,
            compute_units,
            cus_multiplier,
            slot_wait_ms,
            image_pull_ms,
            execution_ms,
            logs_collect_ms,
            container_id,
            backend,
            infra_failure,
            stdout,
            stderr,
            origin_run_fn,
            args_hot,
            args_json,
            container_image,
            container_cmd_snippet,
            container_cmd_full,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventDisplay {
    pub event_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_fn: Option<String>, // Function from event data (fn field)
    pub event_time: String,
    pub project_id: Option<Uuid>,
    pub project_name: String,    // Empty string if no project
    pub event_data: String,      // Formatted as Hot literal (for display)
    pub event_data_json: String, // Raw JSON for JavaScript format switching
    pub created_at: String,      // Pre-formatted with timezone
    pub handled: bool,
}

// Helper function to format microseconds into human-readable duration
fn format_duration_us(total_micros: i64) -> String {
    if total_micros < 1000 {
        format!("{}μs", total_micros)
    } else if total_micros < 1_000_000 {
        let ms = total_micros / 1000;
        let us = total_micros % 1000;
        format!("{}.{}ms", ms, us / 100)
    } else if total_micros >= 3_600_000_000 {
        // 1 hour or more
        let hours = total_micros / 3_600_000_000;
        let mins = (total_micros % 3_600_000_000) / 60_000_000;
        let secs = (total_micros % 60_000_000) / 1_000_000;
        format!("{}h {}m {}s", hours, mins, secs)
    } else if total_micros >= 60_000_000 {
        // 1 minute or more
        let mins = total_micros / 60_000_000;
        let secs = (total_micros % 60_000_000) / 1_000_000;
        format!("{}m {}s", mins, secs)
    } else {
        let ms = total_micros / 1000;
        let us = total_micros % 1000;
        if ms < 1000 {
            format!("{}.{}ms", ms, us / 100)
        } else {
            format!("{:.1}s", total_micros as f64 / 1_000_000.0)
        }
    }
}

impl RunDisplay {
    /// Create a RunDisplay with timezone-aware formatting
    pub fn from_with_timezone(run: &Run, timezone: &str, tz_abbr: &str) -> Self {
        let start_time_formatted = format!(
            "{} {}",
            crate::timezone::format_in_timezone(&run.start_time, timezone, "%Y-%m-%d %H:%M:%S"),
            tz_abbr
        );

        // Calculate execution time in microseconds (stop - start)
        let exec_time_us = if let Some(stop_time) = run.stop_time {
            stop_time
                .signed_duration_since(run.start_time)
                .num_microseconds()
                .unwrap_or(0)
        } else {
            chrono::Utc::now()
                .signed_duration_since(run.start_time)
                .num_microseconds()
                .unwrap_or(0)
        };

        // Calculate queue wait time (start - queued_at) for event-triggered runs
        let queue_wait_us = run.queued_at.map(|queued_at| {
            run.start_time
                .signed_duration_since(queued_at)
                .num_microseconds()
                .unwrap_or(0)
                .max(0) // Clamp to 0 to handle any timing precision issues
        });

        // Total duration = queue wait + execution time
        let duration_us = exec_time_us + queue_wait_us.unwrap_or(0);

        let (exec_time_formatted, stop_time_formatted, is_completed) =
            if let Some(stop_time) = run.stop_time {
                let formatted = format_duration_us(exec_time_us);
                let stop_formatted = format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(&stop_time, timezone, "%Y-%m-%d %H:%M:%S"),
                    tz_abbr
                );
                (formatted, stop_formatted, true)
            } else {
                let formatted = format_duration_us(exec_time_us);
                (formatted, "Running".to_string(), false)
            };

        // Format total duration
        let duration_formatted = format_duration_us(duration_us);

        // Format queue wait time
        let queue_wait_formatted = queue_wait_us.map(format_duration_us);

        // Format queued_at (event time)
        let queued_at_formatted = run.queued_at.map(|queued_at| {
            format!(
                "{} {}",
                crate::timezone::format_in_timezone(&queued_at, timezone, "%Y-%m-%d %H:%M:%S"),
                tz_abbr
            )
        });

        // Use actual status from database
        let status = RunStatus::from_id(run.status_id)
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        RunDisplay {
            run_id: run.run_id,
            stream_id: run.stream_id,
            env_id: run.env_id,
            start_time: run.start_time,
            stop_time: run.stop_time,
            start_time_formatted,
            stop_time_formatted,
            duration_formatted,
            duration_us,
            exec_time_formatted,
            exec_time_us,
            status,
            run_type: run.run_type.clone(),
            is_completed,
            by_user_id: run.by_user_id,
            origin_run_id: run.origin_run_id,
            event_id: run.event_id,
            project_id: run.project_id,
            project_name: run
                .project_name
                .clone()
                .unwrap_or_else(|| "N/A".to_string()),
            result: run
                .result
                .as_ref()
                .map(|v| format_json_as_hot_literal(v, 0)),
            result_json: run
                .result
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())),
            event_fn: run.event_fn.clone(),
            retry_attempt: run.retry_attempt,
            next_retry_at: run.next_retry_at,
            is_retry: run.retry_attempt > 0,
            queue_wait_us,
            queue_wait_formatted,
            queued_at: run.queued_at,
            queued_at_formatted,
        }
    }
}

impl From<&Run> for RunDisplay {
    fn from(run: &Run) -> Self {
        let start_time_formatted = run.start_time.format("%Y-%m-%d %H:%M:%S %Z").to_string();

        // Calculate execution time in microseconds (stop - start)
        let exec_time_us = if let Some(stop_time) = run.stop_time {
            stop_time
                .signed_duration_since(run.start_time)
                .num_microseconds()
                .unwrap_or(0)
        } else {
            chrono::Utc::now()
                .signed_duration_since(run.start_time)
                .num_microseconds()
                .unwrap_or(0)
        };

        // Calculate queue wait time (start - queued_at) for event-triggered runs
        let queue_wait_us = run.queued_at.map(|queued_at| {
            run.start_time
                .signed_duration_since(queued_at)
                .num_microseconds()
                .unwrap_or(0)
                .max(0) // Clamp to 0 to handle any timing precision issues
        });

        // Total duration = queue wait + execution time
        let duration_us = exec_time_us + queue_wait_us.unwrap_or(0);

        let (exec_time_formatted, stop_time_formatted, is_completed) =
            if let Some(stop_time) = run.stop_time {
                let formatted = format_duration_us(exec_time_us);
                let stop_formatted = stop_time.format("%Y-%m-%d %H:%M:%S %Z").to_string();
                (formatted, stop_formatted, true)
            } else {
                let formatted = format_duration_us(exec_time_us);
                (formatted, "Running".to_string(), false)
            };

        // Format total duration
        let duration_formatted = format_duration_us(duration_us);

        // Format queue wait time
        let queue_wait_formatted = queue_wait_us.map(format_duration_us);

        // Format queued_at (event time)
        let queued_at_formatted = run
            .queued_at
            .map(|queued_at| queued_at.format("%Y-%m-%d %H:%M:%S %Z").to_string());

        // Use actual status from database
        let status = RunStatus::from_id(run.status_id)
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        RunDisplay {
            run_id: run.run_id,
            stream_id: run.stream_id,
            env_id: run.env_id,
            start_time: run.start_time,
            stop_time: run.stop_time,
            start_time_formatted,
            stop_time_formatted,
            duration_formatted,
            duration_us,
            exec_time_formatted,
            exec_time_us,
            status,
            run_type: run.run_type.clone(),
            is_completed,
            by_user_id: run.by_user_id,
            origin_run_id: run.origin_run_id,
            event_id: run.event_id,
            project_id: run.project_id,
            project_name: run
                .project_name
                .clone()
                .unwrap_or_else(|| "N/A".to_string()),
            result: run
                .result
                .as_ref()
                .map(|v| format_json_as_hot_literal(v, 0)),
            result_json: run
                .result
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())),
            event_fn: run.event_fn.clone(),
            retry_attempt: run.retry_attempt,
            next_retry_at: run.next_retry_at,
            is_retry: run.retry_attempt > 0,
            queue_wait_us,
            queue_wait_formatted,
            queued_at: run.queued_at,
            queued_at_formatted,
        }
    }
}

// File display structure
#[derive(Debug, Clone)]
pub struct FileDisplay {
    pub file_id: Uuid,
    pub path: String,
    pub size: i64,
    pub size_formatted: String,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub storage_backend: String,
    pub created_by_run_id: Option<Uuid>,
    pub updated_by_run_id: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&hot::db::file::FileRecord> for FileDisplay {
    fn from(file: &hot::db::file::FileRecord) -> Self {
        // Format file size
        let size_formatted = if file.size < 1024 {
            format!("{} B", file.size)
        } else if file.size < 1024 * 1024 {
            format!("{:.1} KB", file.size as f64 / 1024.0)
        } else if file.size < 1024 * 1024 * 1024 {
            format!("{:.1} MB", file.size as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.1} GB", file.size as f64 / (1024.0 * 1024.0 * 1024.0))
        };

        FileDisplay {
            file_id: file.file_id,
            path: file.path.clone(),
            size: file.size,
            size_formatted,
            etag: file.etag.clone(),
            content_type: file.content_type.clone(),
            storage_backend: file.storage_backend.clone(),
            created_by_run_id: file.created_by_run_id,
            updated_by_run_id: file.updated_by_run_id,
            created_at: file.created_at.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
            updated_at: file.updated_at.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        }
    }
}

impl FileDisplay {
    pub fn from_with_timezone(
        file: &hot::db::file::FileRecord,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let size_formatted = if file.size < 1024 {
            format!("{} B", file.size)
        } else if file.size < 1024 * 1024 {
            format!("{:.1} KB", file.size as f64 / 1024.0)
        } else if file.size < 1024 * 1024 * 1024 {
            format!("{:.1} MB", file.size as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.1} GB", file.size as f64 / (1024.0 * 1024.0 * 1024.0))
        };

        FileDisplay {
            file_id: file.file_id,
            path: file.path.clone(),
            size: file.size,
            size_formatted,
            etag: file.etag.clone(),
            content_type: file.content_type.clone(),
            storage_backend: file.storage_backend.clone(),
            created_by_run_id: file.created_by_run_id,
            updated_by_run_id: file.updated_by_run_id,
            created_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &file.created_at,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            updated_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &file.updated_at,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
        }
    }
}

impl EventDisplay {
    /// Create an EventDisplay with timezone-aware formatting
    pub fn from_with_timezone(
        event: &hot::db::event::Event,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let event_time = format!(
            "{} {}",
            crate::timezone::format_in_timezone(&event.event_time, timezone, "%Y-%m-%d %H:%M:%S"),
            tz_abbr
        );

        let created_at = format!(
            "{} {}",
            crate::timezone::format_in_timezone(&event.created_at, timezone, "%Y-%m-%d %H:%M:%S"),
            tz_abbr
        );

        // Extract function name from event data if present
        let event_fn = event
            .event_data
            .get("fn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Keep raw JSON for JavaScript format switching
        let event_data_json =
            serde_json::to_string(&event.event_data).unwrap_or_else(|_| "{}".to_string());

        EventDisplay {
            event_id: event.event_id,
            env_id: event.env_id,
            stream_id: event.stream_id,
            event_type: event.event_type.clone(),
            event_fn,
            event_time,
            project_id: None,
            project_name: String::new(),
            event_data: format_json_as_hot_literal(&event.event_data, 0),
            event_data_json,
            created_at,
            handled: event.handled,
        }
    }
}

impl From<&hot::db::event::Event> for EventDisplay {
    fn from(event: &hot::db::event::Event) -> Self {
        let event_time = event.event_time.format("%Y-%m-%d %H:%M:%S %Z").to_string();
        let created_at = event.created_at.format("%Y-%m-%d %H:%M:%S %Z").to_string();

        // Extract function name from event data if present
        let event_fn = event
            .event_data
            .get("fn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Keep raw JSON for JavaScript format switching
        let event_data_json =
            serde_json::to_string(&event.event_data).unwrap_or_else(|_| "{}".to_string());

        EventDisplay {
            event_id: event.event_id,
            env_id: event.env_id,
            stream_id: event.stream_id,
            event_type: event.event_type.clone(),
            event_fn,
            event_time,
            project_id: None,
            project_name: String::new(),
            event_data: format_json_as_hot_literal(&event.event_data, 0),
            event_data_json,
            created_at,
            handled: event.handled,
        }
    }
}

// Context for public pages (signin, signup, invite acceptance)
#[derive(Debug, Clone)]
pub struct PublicPageContext {
    pub current_page: String,
    pub assets_prefix: String,
    pub current_year: i32,
    pub css_url: String,
    pub is_local_dev: bool,
    pub is_hot_cloud: bool,
    pub is_self_host: bool,
    pub show_cloud_upsells: bool,
    pub billing_enabled: bool,
    pub pricing_url: String,
    pub support_email: String,
    pub web_url: String,
    pub is_production: bool,
}

impl PublicPageContext {
    pub fn new(current_page: &str) -> Self {
        let conf = hot::val!({
            "product": {
                "experience": hot::product::ProductExperienceMode::LocalDev.as_str(),
            },
            "billing": {
                "enabled": false,
            },
        });
        Self::new_with_conf(current_page, &conf)
    }

    pub fn new_with_conf(current_page: &str, conf: &hot::val::Val) -> Self {
        Self {
            current_page: current_page.to_string(),
            assets_prefix: get_assets_prefix(),
            current_year: get_current_year(),
            css_url: get_css_url(),
            is_local_dev: hot::product::is_local_dev_experience(conf),
            is_hot_cloud: hot::product::is_hot_cloud(conf),
            is_self_host: hot::product::is_self_host(conf),
            show_cloud_upsells: hot::product::should_show_cloud_upsells(conf),
            billing_enabled: hot::product::billing_enabled(conf),
            pricing_url: hot::product::pricing_url(conf),
            support_email: hot::product::support_email(conf),
            web_url: hot::product::web_url(conf),
            is_production: hot::env::get_env() == "production",
        }
    }

    /// Get a cache-busted asset URL. Usage: {{ page_context.asset("js/htmx.min.js") }}
    pub fn asset(&self, path: &str) -> String {
        get_asset_url(path)
    }
}

/// Type of global notice/alert
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeType {
    Info,
    Success,
    Warning,
    Error,
}

impl NoticeType {
    pub fn bg_class(&self) -> &'static str {
        match self {
            NoticeType::Info => {
                "bg-blue-50 dark:bg-blue-900/30 border-blue-300 dark:border-blue-700"
            }
            NoticeType::Success => {
                "bg-green-50 dark:bg-green-900/30 border-green-300 dark:border-green-700"
            }
            NoticeType::Warning => {
                "bg-yellow-50 dark:bg-yellow-900/30 border-yellow-300 dark:border-yellow-700"
            }
            NoticeType::Error => "bg-red-50 dark:bg-red-900/30 border-red-300 dark:border-red-700",
        }
    }

    pub fn icon_class(&self) -> &'static str {
        match self {
            NoticeType::Info => "text-blue-500 dark:text-blue-400",
            NoticeType::Success => "text-green-500 dark:text-green-400",
            NoticeType::Warning => "text-yellow-500 dark:text-yellow-400",
            NoticeType::Error => "text-red-500 dark:text-red-400",
        }
    }

    pub fn text_class(&self) -> &'static str {
        match self {
            NoticeType::Info => "text-blue-800 dark:text-blue-200",
            NoticeType::Success => "text-green-800 dark:text-green-200",
            NoticeType::Warning => "text-yellow-800 dark:text-yellow-200",
            NoticeType::Error => "text-red-800 dark:text-red-200",
        }
    }

    pub fn link_class(&self) -> &'static str {
        match self {
            NoticeType::Info => {
                "text-blue-700 dark:text-blue-300 hover:text-blue-900 dark:hover:text-blue-100"
            }
            NoticeType::Success => {
                "text-green-700 dark:text-green-300 hover:text-green-900 dark:hover:text-green-100"
            }
            NoticeType::Warning => {
                "text-yellow-700 dark:text-yellow-300 hover:text-yellow-900 dark:hover:text-yellow-100"
            }
            NoticeType::Error => {
                "text-red-700 dark:text-red-300 hover:text-red-900 dark:hover:text-red-100"
            }
        }
    }
}

/// A global notice/alert to display at the top of pages
#[derive(Debug, Clone)]
pub struct GlobalNotice {
    pub notice_type: NoticeType,
    pub message: String,
    pub action_text: Option<String>,
    pub action_url: Option<String>,
}

impl GlobalNotice {
    pub fn new(notice_type: NoticeType, message: impl Into<String>) -> Self {
        Self {
            notice_type,
            message: message.into(),
            action_text: None,
            action_url: None,
        }
    }

    pub fn with_action(mut self, text: impl Into<String>, url: impl Into<String>) -> Self {
        self.action_text = Some(text.into());
        self.action_url = Some(url.into());
        self
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(NoticeType::Info, message)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(NoticeType::Success, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(NoticeType::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(NoticeType::Error, message)
    }
}

/// Build global notices based on session state
fn build_global_notices(session: &crate::auth::Session) -> Vec<GlobalNotice> {
    let mut notices = Vec::new();

    // Check hosted billing status only when billing is enabled for this product experience.
    if session.billing_enabled
        && let Some(ref org) = session.current_org
        && let Some(status) = session.current_org_subscription_status
    {
        let billing_url = format!("/@{}/billing", org.slug);
        match status {
            hot::db::OrgPlanStatus::Active => {
                // No notice needed
            }
            hot::db::OrgPlanStatus::Pending => {
                notices.push(
                    GlobalNotice::warning(
                        "Your plan change is pending. Please complete checkout to activate all features.",
                    )
                    .with_action("Complete Checkout →", billing_url),
                );
            }
            hot::db::OrgPlanStatus::Inactive => {
                notices.push(
                    GlobalNotice::warning("Your plan is inactive.")
                        .with_action("Manage Billing →", billing_url),
                );
            }
        }
    }

    notices
}

/// All (page_name, docs_path) mappings for app pages → Hot website docs.
/// Stored as a constant so tests can validate that every anchor actually exists
/// in the corresponding markdown file.
const DOCS_PATH_MAPPINGS: &[(&str, &str)] = &[
    ("dashboard", "/docs/app#dashboard"),
    ("runs", "/docs/app#runs"),
    ("events", "/docs/app#events"),
    ("streams", "/docs/app#streams"),
    ("files", "/docs/app#files"),
    ("schedules", "/docs/app#scheduled-runs"),
    ("event_handlers", "/docs/app#event-handlers"),
    ("agents", "/docs/agents"),
    ("mcp_services", "/docs/mcp"),
    ("webhook_services", "/docs/webhooks"),
    ("projects", "/docs/app#projects"),
    ("contexts", "/docs/app#context-variables"),
    ("docs", "/docs/app#docs"),
    ("keys", "/docs/authentication#api-keys"),
    ("service_keys", "/docs/authentication#service-keys"),
    ("domains", "/docs/domains"),
    ("envs", "/docs/app#navigation-scope"),
    ("alerts", "/docs/alerts"),
];

/// Map app page names to their corresponding docs path on the Hot website.
fn default_docs_path(current_page: &str) -> Option<&'static str> {
    DOCS_PATH_MAPPINGS
        .iter()
        .find(|(page, _)| *page == current_page)
        .map(|(_, path)| *path)
}

// Context for private pages (authenticated users)
#[derive(Debug, Clone)]
pub struct PrivatePageContext {
    pub current_page: String,
    pub assets_prefix: String,
    pub current_year: i32,
    pub css_url: String,
    // User-specific fields (always present for authenticated users)
    pub user_initials: String,
    pub user_name: String,
    pub user_orgs: Vec<hot::db::org::Org>,
    pub current_org: Option<hot::db::org::Org>,
    pub current_env: Option<hot::db::env::Env>,
    pub current_org_envs: Vec<hot::db::env::Env>,
    pub breadcrumbs: Breadcrumbs,
    // Whether to show the environment selector in the breadcrumb bar.
    // True for env-scoped pages, false for org-level pages (teams, users, envs, alerts).
    pub show_env_selector: bool,
    // Environment mode flag
    pub is_local_dev: bool,
    pub is_hot_cloud: bool,
    pub is_self_host: bool,
    pub show_cloud_upsells: bool,
    pub billing_enabled: bool,
    pub pricing_url: String,
    pub support_email: String,
    pub api_url: String,
    // True only when HOT_ENV == "production"
    pub is_production: bool,
    // True if user has no orgs (needs to claim a handle)
    pub needs_org_for_billing: bool,
    // True if current org is an individual org
    pub current_org_is_individual: bool,
    // Display timezone (IANA format, e.g., "America/New_York")
    pub display_timezone: String,
    // Timezone abbreviation for display (e.g., "EST", "PST")
    pub timezone_abbreviation: String,
    // Subscription status for current org (None for local dev or no org)
    pub subscription_status: Option<hot::db::OrgPlanStatus>,
    // Resolved features for current org. Used for feature gating in templates.
    pub features: hot::db::Features,
    // Plan name for current org (e.g., "Hot Cloud Pro"). Used for display purposes.
    pub plan_name: Option<String>,
    // Global notices to display at top of page
    pub global_notices: Vec<GlobalNotice>,
    // User's preferred value display format ("hot" or "json")
    pub value_format: String,
    // Base URL for the Hot website, resolved from the current environment
    pub web_url: String,
    // Optional docs path for a "Docs" link in the breadcrumb bar (e.g. "/docs/app#runs")
    pub docs_path: Option<String>,
}

impl PrivatePageContext {
    pub fn new(current_page: &str, session: &crate::auth::Session) -> Self {
        let current_org_is_individual = session
            .current_org
            .as_ref()
            .map(|org| org.is_individual())
            .unwrap_or(false);

        let timezone_abbreviation =
            crate::timezone::get_timezone_abbreviation(&session.display_timezone);

        Self {
            current_page: current_page.to_string(),
            assets_prefix: get_assets_prefix(),
            current_year: get_current_year(),
            css_url: get_css_url(),
            user_initials: session.user_initials.clone(),
            user_name: session.user_name.clone(),
            user_orgs: session.display_orgs(),
            current_org: session.current_org.clone(),
            current_env: session.current_env.clone(),
            current_org_envs: session.current_org_envs.clone(),
            breadcrumbs: build_base_breadcrumbs_with_env(session),
            show_env_selector: true,
            is_local_dev: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            is_hot_cloud: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::HotCloud
            ),
            is_self_host: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::SelfHost
            ),
            show_cloud_upsells: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            billing_enabled: session.billing_enabled,
            pricing_url: session.product_pricing_url.clone(),
            support_email: session.product_support_email.clone(),
            api_url: hot::env::get_api_url().trim_end_matches('/').to_string(),
            is_production: hot::env::get_env() == "production",
            needs_org_for_billing: session.has_no_orgs(),
            current_org_is_individual,
            display_timezone: session.display_timezone.clone(),
            timezone_abbreviation,
            subscription_status: session.current_org_subscription_status,
            features: session.current_org_features.clone(),
            plan_name: session.current_org_plan_name.clone(),
            global_notices: build_global_notices(session),
            value_format: session.value_format.clone(),
            web_url: session.product_web_url.clone(),
            docs_path: default_docs_path(current_page).map(|s| s.to_string()),
        }
    }

    // Create context with custom breadcrumbs for pages that include environment
    pub fn with_breadcrumbs(
        current_page: &str,
        session: &crate::auth::Session,
        breadcrumbs: Breadcrumbs,
    ) -> Self {
        let current_org_is_individual = session
            .current_org
            .as_ref()
            .map(|org| org.is_individual())
            .unwrap_or(false);

        let timezone_abbreviation =
            crate::timezone::get_timezone_abbreviation(&session.display_timezone);

        Self {
            current_page: current_page.to_string(),
            assets_prefix: get_assets_prefix(),
            current_year: get_current_year(),
            css_url: get_css_url(),
            user_initials: session.user_initials.clone(),
            user_name: session.user_name.clone(),
            user_orgs: session.display_orgs(),
            current_org: session.current_org.clone(),
            current_env: session.current_env.clone(),
            current_org_envs: session.current_org_envs.clone(),
            breadcrumbs,
            show_env_selector: true,
            is_local_dev: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            is_hot_cloud: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::HotCloud
            ),
            is_self_host: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::SelfHost
            ),
            show_cloud_upsells: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            billing_enabled: session.billing_enabled,
            pricing_url: session.product_pricing_url.clone(),
            support_email: session.product_support_email.clone(),
            api_url: hot::env::get_api_url().trim_end_matches('/').to_string(),
            is_production: hot::env::get_env() == "production",
            needs_org_for_billing: session.has_no_orgs(),
            current_org_is_individual,
            display_timezone: session.display_timezone.clone(),
            timezone_abbreviation,
            subscription_status: session.current_org_subscription_status,
            features: session.current_org_features.clone(),
            plan_name: session.current_org_plan_name.clone(),
            global_notices: build_global_notices(session),
            value_format: session.value_format.clone(),
            web_url: session.product_web_url.clone(),
            docs_path: default_docs_path(current_page).map(|s| s.to_string()),
        }
    }

    // Create context for org-level pages (no environment shown)
    pub fn for_org_page(
        current_page: &str,
        session: &crate::auth::Session,
        breadcrumbs: Breadcrumbs,
    ) -> Self {
        let current_org_is_individual = session
            .current_org
            .as_ref()
            .map(|org| org.is_individual())
            .unwrap_or(false);

        let timezone_abbreviation =
            crate::timezone::get_timezone_abbreviation(&session.display_timezone);

        Self {
            current_page: current_page.to_string(),
            assets_prefix: get_assets_prefix(),
            current_year: get_current_year(),
            css_url: get_css_url(),
            user_initials: session.user_initials.clone(),
            user_name: session.user_name.clone(),
            user_orgs: session.display_orgs(),
            current_org: session.current_org.clone(),
            current_env: session.current_env.clone(),
            current_org_envs: session.current_org_envs.clone(),
            breadcrumbs,
            show_env_selector: false,
            is_local_dev: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            is_hot_cloud: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::HotCloud
            ),
            is_self_host: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::SelfHost
            ),
            show_cloud_upsells: matches!(
                session.product_experience,
                hot::product::ProductExperienceMode::LocalDev
            ),
            billing_enabled: session.billing_enabled,
            pricing_url: session.product_pricing_url.clone(),
            support_email: session.product_support_email.clone(),
            api_url: hot::env::get_api_url().trim_end_matches('/').to_string(),
            is_production: hot::env::get_env() == "production",
            needs_org_for_billing: session.has_no_orgs(),
            current_org_is_individual,
            display_timezone: session.display_timezone.clone(),
            timezone_abbreviation,
            subscription_status: session.current_org_subscription_status,
            features: session.current_org_features.clone(),
            plan_name: session.current_org_plan_name.clone(),
            global_notices: build_global_notices(session),
            value_format: session.value_format.clone(),
            web_url: session.product_web_url.clone(),
            docs_path: default_docs_path(current_page).map(|s| s.to_string()),
        }
    }

    /// Check if the current org has custom domains enabled.
    /// Returns true if the feature is enabled (Pro+ plans, self-hosted, local dev).
    /// Usage in templates: `{% if page_context.has_custom_domains() %}`
    pub fn has_custom_domains(&self) -> bool {
        self.features.has_custom_domains()
    }

    /// Check if the current org has service keys enabled.
    /// Returns true if the feature is enabled (Pro+ plans, self-hosted, local dev).
    /// Usage in templates: `{% if page_context.has_service_keys() %}`
    pub fn has_service_keys(&self) -> bool {
        self.features.has_service_keys()
    }

    /// Check if the current org has alerts enabled.
    /// Returns true if the feature is enabled (Starter+ plans, self-hosted, local dev).
    /// Usage in templates: `{% if page_context.has_alerts() %}`
    pub fn has_alerts(&self) -> bool {
        self.features.has_alerts()
    }

    /// Get a cache-busted asset URL. Usage: {{ page_context.asset("js/htmx.min.js") }}
    pub fn asset(&self, path: &str) -> String {
        get_asset_url(path)
    }

    /// Get the full docs URL (web_url + docs_path), or None if no docs_path is set.
    /// Usage in templates: `{% if let Some(url) = page_context.docs_url() %}`
    pub fn docs_url(&self) -> Option<String> {
        self.docs_path
            .as_ref()
            .map(|path| format!("{}{}", self.web_url, path))
    }

    /// Override the default docs path for this page.
    pub fn with_docs_path(mut self, path: &str) -> Self {
        self.docs_path = Some(path.to_string());
        self
    }
}

#[derive(Template)]
#[template(path = "signin.html")]
pub struct SignIn<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub error_message: &'a str,
    pub invite_code: &'a str,
    pub next: &'a str,
    pub plan: &'a str,
    pub billing: &'a str,
    pub form_token: &'a str,
}

#[derive(Template)]
#[template(path = "signup.html")]
pub struct SignUp<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub error_message: &'a str,
    pub email: &'a str,
    pub name: &'a str,
    pub invite_code: &'a str,
    pub plan: &'a str,
    pub plan_display_name: &'a str,
    pub billing: &'a str,
    pub form_token: &'a str,
    /// When true, the error box includes a "sign in instead" link
    /// (used for the duplicate-email error).
    pub show_signin_link: bool,
}

#[derive(Template)]
#[template(path = "claim_handle.html")]
pub struct ClaimHandle<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub error_message: &'a str,
    pub org_name: &'a str,
    pub org_slug: &'a str,
    pub account_type: &'a str,
    pub plan: &'a str,
    pub billing: &'a str,
}

#[derive(Template)]
#[template(path = "signup_plans.html")]
pub struct SignupPlans<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub plans: Vec<hot::db::Plan>,
    pub web_url: &'a str,
}

#[derive(Template)]
#[template(path = "check_email.html")]
pub struct CheckEmail<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub email: &'a str,
    pub form_token: &'a str,
    pub already_pending: bool,
    pub is_free_plan: bool,
    /// When true, hide the resend form and explain the resend cap.
    pub resend_capped: bool,
}

#[derive(Template)]
#[template(path = "oauth_select_email.html")]
pub struct OAuthSelectEmail<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub emails: &'a [String],
}

#[derive(Template)]
#[template(path = "verification_error.html")]
pub struct VerificationError<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub error_title: &'a str,
    pub error_message: &'a str,
}

#[derive(Template)]
#[template(path = "verify_email.html")]
pub struct VerifyEmail<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub token: &'a str,
    pub form_token: &'a str,
}

#[derive(Template)]
#[template(path = "signout.html")]
pub struct SignOut {
    pub title: &'static str,
    pub page_context: PublicPageContext,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub web_url: String,
    pub success_message: &'a str,
    pub chart_data_json: &'a str,
    pub status_chart_data_json: &'a str,
    pub projects: Vec<hot::db::Project>,
    pub projects_json: String,
}

#[derive(Template)]
#[template(path = "components/dashboard_recent_runs_rows.html")]
pub struct DashboardRecentRunsTable {
    pub recent_runs: Vec<RunDisplay>,
}

#[derive(Template)]
#[template(path = "components/dashboard_recent_events_rows.html")]
pub struct DashboardRecentEventsTable {
    pub recent_events: Vec<EventDisplay>,
}

#[derive(Template)]
#[template(path = "components/dashboard_recent_streams_rows.html")]
pub struct DashboardRecentStreamsTable {
    pub recent_streams: Vec<StreamListItem>,
}

#[derive(Template)]
#[template(path = "components/dashboard_recent_tasks_rows.html")]
pub struct DashboardRecentTasksTable {
    pub recent_tasks: Vec<TaskDisplay>,
}

#[derive(Template)]
#[template(path = "checkout_success.html")]
pub struct CheckoutSuccess<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
}

#[derive(Template)]
#[template(path = "billing.html")]
pub struct Billing<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub user_orgs: Vec<OrgWithBilling>,
}

// Helper struct for org listing with billing info
pub struct OrgWithBilling {
    pub org: hot::db::org::Org,
    pub subscription: Option<hot::db::OrgPlan>,
    pub plan: Option<hot::db::Plan>,
}

#[derive(Template)]
#[template(path = "org_billing.html")]
pub struct OrgBilling<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: &'a hot::db::org::Org,
    pub subscription: Option<hot::db::OrgPlan>,
    pub plan: Option<hot::db::Plan>,
    pub success_message: &'a str,
    pub billing_provider_configured: bool,
}

#[derive(Template)]
#[template(path = "org_usage.html")]
pub struct OrgUsage<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: &'a hot::db::org::Org,
    pub plan: Option<hot::db::Plan>,
    pub features: hot::db::Features,
    pub month_start: chrono::DateTime<chrono::Utc>,
    pub can_upgrade: bool, // false for Scale plan
}

/// Partial template for usage stats (loaded via HTMX)
#[derive(Template)]
#[template(path = "partials/org_usage_stats.html")]
pub struct OrgUsageStats<'a> {
    pub org: &'a hot::db::org::Org,
    pub plan: Option<hot::db::Plan>,
    pub features: hot::db::Features,
    pub usage: hot::db::OrgUsageStats,
    pub month_start: chrono::DateTime<chrono::Utc>,
    pub can_upgrade: bool,
    pub is_local_dev: bool,
}

#[derive(Template)]
#[template(path = "org_plan_select.html")]
pub struct OrgPlanSelect<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: &'a hot::db::org::Org,
    pub plans: Vec<hot::db::Plan>,
    pub current_plan_name: Option<String>,
    pub current_plan_sort_order: Option<i32>,
}

#[derive(Template)]
#[template(path = "account.html")]
pub struct Account<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub user: &'a User,
    pub billing_enabled: bool,
    pub user_timezone: String,
    pub value_format: String,
    pub saved: bool,
}

#[derive(Template)]
#[template(path = "account_notifications.html")]
pub struct AccountNotifications<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub notification_prefs: &'a crate::handlers::account::NotificationPreferences,
    pub saved: bool,
}

#[derive(Template)]
#[template(path = "checkout_form.html")]
pub struct CheckoutForm<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub plan: Option<hot::db::Plan>,
    pub billing_period: &'a str,
}

#[derive(Template)]
#[template(path = "org_checkout_form.html")]
pub struct OrgCheckoutForm<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub org: hot::db::org::Org,
    pub plan: hot::db::Plan,
    pub billing_period: &'a str,
    pub all_plans: Vec<hot::db::Plan>,
}

#[derive(Template)]
#[template(path = "runs_list.html")]
pub struct RunsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub runs: Vec<RunDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_runs: i64,
    // Filter state fields
    pub selected_statuses: Vec<String>,
    pub selected_run_types: Vec<String>,
    pub selected_time_range: String,
    pub selected_project: String, // Empty string means "All Projects"
    pub search_query: String,     // Search term
    pub projects: Vec<hot::db::Project>,
}

/// Partial template for runs table content (for HTMX updates)
#[derive(Template)]
#[template(path = "components/runs_table_content.html")]
pub struct RunsTableContent {
    pub runs: Vec<RunDisplay>,
    pub current_page_num: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_runs: i64,
}

#[derive(Template)]
#[template(path = "run_detail.html")]
pub struct RunDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub run: Option<RunDisplay>,
    pub run_id: Uuid,
    pub stream_id: Uuid,
    pub raw_mode: bool,
    pub inspect_mode: bool,
    pub graph_data_json: String,
    pub access_info: Option<AccessInfo>,
    pub associated_task_id: Option<Uuid>,
}

#[derive(Template)]
#[template(path = "components/run_detail_tasks_tab.html")]
pub struct RunDetailTasksTab {
    pub tasks: Vec<TaskDisplay>,
}

#[derive(Template)]
#[template(path = "tasks_list.html")]
pub struct TasksList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub tasks: Vec<TaskDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_tasks: i64,
    pub selected_statuses: Vec<String>,
    pub selected_task_types: Vec<String>,
    pub selected_time_range: String,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "components/tasks_table_content.html")]
pub struct TasksTableContent {
    pub tasks: Vec<TaskDisplay>,
    pub current_page_num: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_tasks: i64,
}

#[derive(Template)]
#[template(path = "task_detail.html")]
pub struct TaskDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub task: TaskDisplay,
    pub graph_data_json: String,
}

#[derive(Template)]
#[template(path = "orgs_list.html")]
pub struct OrgsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub orgs: Vec<hot::db::org::Org>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_orgs: i64,
}

#[derive(Template)]
#[template(path = "orgs_new.html")]
pub struct OrgsNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub error_message: &'a str,
    pub org_name: &'a str,
    pub org_slug: &'a str,
    pub account_type: &'a str,
    pub plans: Vec<hot::db::Plan>,
    pub selected_plan: &'a str,
    pub selected_billing: &'a str,
    pub is_local_dev: bool,
}

#[derive(Template)]
#[template(path = "orgs_detail.html")]
pub struct OrgsDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: hot::db::org::Org,
    pub active_page: &'a str,
}

#[derive(Template)]
#[template(path = "orgs_edit.html")]
pub struct OrgsEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: hot::db::org::Org,
    pub error_message: &'a str,
    pub org_timezone: String,
}

#[derive(Template)]
#[template(path = "orgs_not_found.html")]
pub struct OrgsNotFound<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org_slug: String,
}

#[derive(Template)]
#[template(path = "teams_list.html")]
pub struct TeamsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub teams: Vec<hot::db::team::Team>,
    pub is_admin: bool,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_teams: i64,
}

#[derive(Template)]
#[template(path = "teams_new.html")]
pub struct TeamsNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub error_message: &'a str,
    pub name: &'a str,
}

#[derive(Template)]
#[template(path = "teams_detail.html")]
pub struct TeamsDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team: hot::db::team::Team,
}

#[derive(Template)]
#[template(path = "teams_edit.html")]
pub struct TeamsEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team: hot::db::team::Team,
    pub error_message: &'a str,
}

#[derive(Template)]
#[template(path = "teams_not_found.html")]
pub struct TeamsNotFound<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team_id: String,
    pub is_admin: bool,
}

// Org User Management Templates
#[derive(Debug, Clone)]
pub struct OrgUserDisplay {
    pub user_id: String,
    pub email: String,
    pub name: Option<String>,
    pub role_name: String,
    pub org_user_role_id: i16,
    pub active: bool,
    pub created_at_formatted: String,
}

#[derive(Debug, Clone)]
pub struct InviteDisplay {
    pub invite_id: String,
    pub invite_code: String,
    pub email: String,
    pub role_name: String,
    pub status: String,
    pub created_at_formatted: String,
    pub expires_at_formatted: String,
    pub is_expired: bool,
}

#[derive(Template)]
#[template(path = "org_users_list.html")]
pub struct OrgUsersList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: hot::db::org::Org,
    pub org_users: Vec<OrgUserDisplay>,
    pub pending_invites: Vec<InviteDisplay>,
    pub is_admin: bool,
    pub active_page: &'a str,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_users: i64,
    /// Currently-used team-member seats (active members + pending invites).
    pub team_members_used: i64,
    /// Plan limit for team members. `None` when the plan is unlimited
    /// (encoded as `-1` in the underlying features struct).
    pub team_members_limit: Option<i64>,
    /// `true` when `team_members_limit` is `Some(n)` and `team_members_used >= n`.
    pub team_members_at_limit: bool,
}

#[derive(Template)]
#[template(path = "org_users_invite.html")]
pub struct OrgUsersInvite<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: hot::db::org::Org,
    pub error_message: &'a str,
    pub success_message: &'a str,
    pub email: &'a str,
    pub role_id: i16,
    pub active_page: &'a str,
}

#[derive(Template)]
#[template(path = "org_users_edit.html")]
pub struct OrgUsersEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub org: hot::db::org::Org,
    pub org_user: OrgUserDisplay,
    pub error_message: &'a str,
    pub active_page: &'a str,
}

#[derive(Template)]
#[template(path = "invite_accept.html")]
pub struct InviteAccept<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub invite_code: &'a str,
    pub email: &'a str,
    pub org_name: &'a str,
    pub role_name: &'a str,
    pub invited_by_name: &'a str,
    pub error_message: &'a str,
    pub is_authenticated: bool,
}

// Team user display structures
#[derive(Debug, Clone)]
pub struct TeamUserDisplay {
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub role_name: String,
    pub team_user_role_id: i16,
    pub active: bool,
    pub created_at_formatted: String,
}

#[derive(Template)]
#[template(path = "team_users_list.html")]
pub struct TeamUsersList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team: hot::db::team::Team,
    pub team_users: Vec<hot::db::team::TeamUserWithRole>,
    pub can_manage: bool,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_team_users: i64,
}

#[derive(Template)]
#[template(path = "team_users_add.html")]
pub struct TeamUsersAdd<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team: hot::db::team::Team,
    pub available_users: Vec<hot::db::org::OrgUserWithRole>,
    pub error_message: &'a str,
    pub selected_user_id: Option<Uuid>,
    pub selected_role_id: i16,
}

#[derive(Template)]
#[template(path = "team_users_edit.html")]
pub struct TeamUsersEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub team: hot::db::team::Team,
    pub team_user: TeamUserDisplay,
    pub error_message: &'a str,
}

#[derive(Template)]
#[template(path = "keys_list.html")]
pub struct ApiKeysList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub api_keys: Vec<hot::db::api_key::ApiKey>,
    pub is_admin: bool,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_keys: i64,
}

#[derive(Template)]
#[template(path = "keys_new.html")]
pub struct ApiKeysNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub generated_key: String,
    pub api_key_id: Uuid,
    pub error_message: &'a str,
    pub description: &'a str,
    pub access_level: &'a str,
    pub mcp_tools_json: String,
    pub webhooks_json: String,
}

#[derive(Template)]
#[template(path = "keys_edit.html")]
pub struct ApiKeysEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub api_key: hot::db::api_key::ApiKey,
    pub error_message: &'a str,
    pub access_level: &'a str,
    pub permissions_json: String,
    pub mcp_tools_json: String,
    pub webhooks_json: String,
}

#[derive(Template)]
#[template(path = "envs_list.html")]
pub struct EnvsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub envs: Vec<hot::db::env::Env>,
    pub is_admin: bool,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_envs: i64,
}

#[derive(Template)]
#[template(path = "envs_new.html")]
pub struct EnvsNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub error_message: &'a str,
    pub name: &'a str,
}

#[derive(Template)]
#[template(path = "envs_edit.html")]
pub struct EnvsEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub env: hot::db::env::Env,
    pub error_message: &'a str,
}

#[derive(Template)]
#[template(path = "events_list.html")]
pub struct EventsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub events: Vec<EventListItem>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_events: i64,
    pub inspect_mode: bool,
    pub selected_handled: String,    // "all", "handled", or "unhandled"
    pub selected_time_range: String, // ISO 8601 duration or "all"
    pub search_query: String,        // Search term
}

/// Partial template for events table content (for HTMX updates)
#[derive(Template)]
#[template(path = "components/events_table_content.html")]
pub struct EventsTableContent {
    pub events: Vec<EventListItem>,
    pub current_page_num: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_events: i64,
}

/// Event list item with pre-formatted dates for timezone display
#[derive(Debug, Clone)]
pub struct EventListItem {
    pub event_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_fn: Option<String>,
    pub event_time: String, // Pre-formatted with timezone
    pub created_at: String, // Pre-formatted with timezone
    pub handled: bool,
    pub event_data: serde_json::Value,
    pub event_data_formatted: String, // Formatted as Hot literal for display
    pub event_data_json: String,      // Raw JSON for JavaScript format switching
}

impl EventListItem {
    pub fn from_with_timezone(
        event: &hot::db::event::Event,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let event_fn = event
            .event_data
            .get("fn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Pre-format the event data as Hot literal
        let event_data_formatted = format_json_as_hot_literal(&event.event_data, 0);

        // Keep raw JSON for JavaScript format switching
        let event_data_json =
            serde_json::to_string(&event.event_data).unwrap_or_else(|_| "{}".to_string());

        Self {
            event_id: event.event_id,
            env_id: event.env_id,
            stream_id: event.stream_id,
            event_type: event.event_type.clone(),
            event_fn,
            event_time: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &event.event_time,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            created_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &event.created_at,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            handled: event.handled,
            event_data: event.event_data.clone(),
            event_data_formatted,
            event_data_json,
        }
    }
}

#[derive(Template)]
#[template(path = "events_detail.html")]
pub struct EventDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub event: EventDisplay,
    pub event_runs: Vec<RunDisplay>,
    pub stream_graphs: Vec<StreamGraphData>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub access_info: Option<AccessInfo>,
}

#[derive(serde::Serialize)]
pub struct StreamGraphData {
    pub stream_id: Uuid,
    pub stream_id_short: String,
    pub graph_data: GraphNodeData,
    pub graph_data_json: String,
}

#[derive(Template)]
#[template(path = "events_detail_table.html")]
pub struct EventDetailTable {
    pub event_id: Uuid,
    pub event_runs: Vec<RunDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
}

/// Stream list item with pre-formatted dates for timezone display
#[derive(Debug, Clone)]
pub struct StreamListItem {
    pub stream_id: Uuid,
    pub env_id: Uuid,
    pub project_ids: Option<serde_json::Value>,
    pub project_names: Option<serde_json::Value>,
    pub total_runs: i64,
    pub total_events: i64,
    pub start_time: String,       // Pre-formatted with timezone
    pub last_activity_at: String, // Pre-formatted with timezone
    pub start_time_raw: chrono::DateTime<chrono::Utc>, // Raw for calculations
    pub last_activity_at_raw: chrono::DateTime<chrono::Utc>, // Raw for calculations
    pub duration_ms: i64,         // Pre-calculated duration in ms
    pub latest_event_type: Option<String>,
    pub latest_run_fn: Option<String>,
}

impl StreamListItem {
    pub fn from_with_timezone(
        stream: &hot::db::stream::StreamSummary,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let duration_ms =
            stream.last_activity_at.timestamp_millis() - stream.start_time.timestamp_millis();
        Self {
            stream_id: stream.stream_id,
            env_id: stream.env_id,
            project_ids: stream.project_ids.clone(),
            project_names: stream.project_names.clone(),
            total_runs: stream.total_runs,
            total_events: stream.total_events,
            start_time: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &stream.start_time,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            last_activity_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &stream.last_activity_at,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            start_time_raw: stream.start_time,
            last_activity_at_raw: stream.last_activity_at,
            duration_ms,
            latest_event_type: stream.latest_event_type.clone(),
            latest_run_fn: stream.latest_run_fn.clone(),
        }
    }

    /// Get formatted project names as a comma-separated string
    pub fn project_names_display(&self) -> String {
        match &self.project_names {
            Some(json_val) => {
                if let Some(arr) = json_val.as_array() {
                    let names: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                    if names.is_empty() {
                        "N/A".to_string()
                    } else {
                        names.join(", ")
                    }
                } else {
                    "N/A".to_string()
                }
            }
            None => "N/A".to_string(),
        }
    }

    pub fn has_context(&self) -> bool {
        self.latest_event_type.is_some() || self.latest_run_fn.is_some()
    }
}

#[derive(Template)]
#[template(path = "streams_list.html")]
pub struct StreamsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub streams: Vec<StreamListItem>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_streams: i64,
    pub selected_project: String,    // Empty string means "All Projects"
    pub selected_time_range: String, // ISO 8601 duration or "all"
    pub search_query: String,        // Search term
    pub projects: Vec<hot::db::Project>,
}

/// Partial template for streams table content (for HTMX updates)
#[derive(Template)]
#[template(path = "components/streams_table_content.html")]
pub struct StreamsTableContent {
    pub streams: Vec<StreamListItem>,
    pub current_page_num: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_streams: i64,
}

#[derive(Template)]
#[template(path = "stream_detail.html")]
pub struct StreamDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub stream: StreamListItem,
    pub runs: Vec<RunDisplay>,
    pub events: Vec<EventListItem>,
    pub tasks: Vec<TaskDisplay>,
    pub graph_data: GraphNodeData,
    pub graph_data_json: String,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
}

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub project_id: uuid::Uuid,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub active: bool,
    pub builds_count: i64,
    pub active_build_id: Option<uuid::Uuid>, // ID of deployed build
    pub context_vars_count: i64,
}

#[derive(Template)]
#[template(path = "projects_list.html")]
pub struct ProjectsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub projects: Vec<ProjectSummary>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_projects: i64,
    pub search_query: String,
    pub selected_time_range: String,
}

#[derive(Template)]
#[template(path = "projects_detail.html")]
pub struct ProjectsDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub project: hot::db::project::Project,
    pub has_deployed_build: bool,
    pub deployed_build_id: String,
    pub deployed_build_type: String,
    pub deployed_build_hash: String,
    pub deployed_build_size: i32,
    pub deployed_build_updated_at: String,
}

#[derive(Template)]
#[template(path = "projects_not_found.html")]
pub struct ProjectsNotFound<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub project_name: String,
}

#[derive(Template)]
#[template(path = "env_switch_prompt.html")]
pub struct EnvSwitchPrompt<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub message: String,
    pub switch_url: String,
    pub back_url: String,
    pub back_label: String,
}

#[derive(Template)]
#[template(path = "projects_builds.html")]
pub struct ProjectsBuilds<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub project: hot::db::project::Project,
    pub builds: Vec<hot::db::build::Build>,
    pub deploy_warning: Option<String>,
}

#[derive(Template)]
#[template(path = "files_list.html")]
pub struct FilesList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub files: Vec<FileDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_files: i64,
    pub selected_time_range: String,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "files_detail.html")]
pub struct FileDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub file: FileDisplay,
}

// ---------------------------------------------------------------------------
// Stores (`::hot::store`) browser
// ---------------------------------------------------------------------------

/// Format a byte count as a human-readable string ("1.2 KB", "3.4 MB", ...).
pub fn format_bytes(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Encode an arbitrary JSON value (used as a store entry key) into a URL-safe
/// path segment. Round-trippable via [`decode_entry_key`].
pub fn encode_entry_key(key: &serde_json::Value) -> String {
    use base64::Engine as _;
    let bytes = serde_json::to_vec(key).unwrap_or_else(|_| b"null".to_vec());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a key that was encoded with [`encode_entry_key`].
pub fn decode_entry_key(encoded: &str) -> Result<serde_json::Value, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| format!("Invalid encoded key: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid encoded key JSON: {e}"))
}

#[derive(Debug, Clone)]
pub struct StoreMapDisplay {
    pub name: String,
    pub name_url: String,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub embedding_field: Option<String>,
    pub text_search: bool,
    pub entry_count: i64,
    pub storage_bytes: i64,
    pub storage_bytes_formatted: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl StoreMapDisplay {
    pub fn from(info: &hot::store::StoreMapInfo) -> Self {
        Self {
            name: info.name.clone(),
            name_url: urlencoding::encode(&info.name).into_owned(),
            embedding_model: info.embedding_model.clone(),
            embedding_dimensions: info.embedding_dimensions,
            embedding_field: info.embedding_field.clone(),
            text_search: info.text_search,
            entry_count: info.entry_count,
            storage_bytes: info.storage_bytes,
            storage_bytes_formatted: format_bytes(info.storage_bytes),
            created_at: info.created_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoreEntryDisplay {
    pub key_hot: String,
    pub key_json: String,
    pub key_preview: String,
    pub key_encoded: String,
    pub value_hot: String,
    pub value_json: String,
    pub value_preview: String,
    pub seq: i64,
    pub has_embedding: bool,
    pub embedding_dimensions: Option<usize>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl StoreEntryDisplay {
    pub fn from(entry: &hot::store::StoreEntry) -> Self {
        let key_hot = format_json_as_hot_literal(&entry.key, 0);
        let value_hot = format_json_as_hot_literal(&entry.value, 0);
        let key_json = serde_json::to_string(&entry.key).unwrap_or_else(|_| "null".to_string());
        let value_json =
            serde_json::to_string_pretty(&entry.value).unwrap_or_else(|_| "null".to_string());
        let key_preview = truncate_string(&key_hot, 80);
        let value_preview = truncate_string(&value_hot.replace('\n', " "), 120);

        Self {
            key_hot,
            key_json,
            key_preview,
            key_encoded: encode_entry_key(&entry.key),
            value_hot,
            value_json,
            value_preview,
            seq: entry.seq,
            has_embedding: entry.embedding.is_some(),
            embedding_dimensions: entry.embedding.as_ref().map(|e| e.len()),
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }

    pub fn from_info(entry: &hot::store::StoreEntryInfo) -> Self {
        let key_hot = format_json_as_hot_literal(&entry.key, 0);
        let key_json = serde_json::to_string(&entry.key).unwrap_or_else(|_| "null".to_string());
        let key_preview = truncate_string(&key_hot, 80);

        Self {
            key_hot,
            key_json,
            key_preview,
            key_encoded: encode_entry_key(&entry.key),
            value_hot: String::new(),
            value_json: String::new(),
            value_preview: String::new(),
            seq: entry.seq,
            has_embedding: entry.embedding_dimensions.is_some(),
            embedding_dimensions: entry.embedding_dimensions,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}

#[derive(Template)]
#[template(path = "stores_list.html")]
pub struct StoresList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub stores: Vec<StoreMapDisplay>,
    pub total_stores: usize,
    pub search_query: String,
    pub error_message: Option<String>,
    pub storage_type: String,
}

#[derive(Template)]
#[template(path = "store_detail.html")]
pub struct StoreDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub store: StoreMapDisplay,
    pub entries: Vec<StoreEntryDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_entries: i64,
    pub is_admin: bool,
    pub deleted_flash: bool,
    pub error_message: Option<String>,
    pub search_query: String,
    pub is_searching: bool,
    pub pagination_search_suffix: String,
}

/// Partial template returned for HTMX requests on the store detail page.
/// Renders just the entries-area markup (matching count, desktop table, mobile cards).
#[derive(Template)]
#[template(path = "components/store_entries_table.html")]
pub struct StoreEntriesTable {
    pub page_context: PrivatePageContext,
    pub store: StoreMapDisplay,
    pub entries: Vec<StoreEntryDisplay>,
    pub current_page_num: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_entries: i64,
    pub is_admin: bool,
    pub is_searching: bool,
    pub pagination_search_suffix: String,
}

#[derive(Template)]
#[template(path = "entry_detail.html")]
pub struct EntryDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub store: StoreMapDisplay,
    pub entry: StoreEntryDisplay,
    pub is_admin: bool,
}

#[derive(Template)]
#[template(path = "store_entry_value_cell.html")]
pub struct StoreEntryValueCell {
    pub entry: StoreEntryDisplay,
    pub container_id: String,
    pub view: String,
}

/// Hidden state for a value cell, rendered server-side so the column toggle
/// can restore the same markup the initial page render produced.
#[derive(Template)]
#[template(path = "store_entry_value_hidden_cell.html")]
pub struct StoreEntryValueHiddenCell {
    pub key_encoded: String,
    pub container_id: String,
    pub view: String,
}

#[derive(Template)]
#[template(path = "store_entry_value_panel.html")]
pub struct StoreEntryValuePanel {
    pub entry: StoreEntryDisplay,
}

// Schedule log display structure
#[derive(Debug, Clone)]
pub struct ScheduleLogDisplay {
    pub log_id: uuid::Uuid,
    pub schedule_id: uuid::Uuid,
    pub event_id: Option<uuid::Uuid>,
    pub stream_id: Option<uuid::Uuid>,
    pub scheduled_time: String,
    pub executed_at: String,
    pub is_backfill: bool,
    pub created_at: String,
}

impl ScheduleLogDisplay {
    pub fn from_with_timezone(
        log: &hot::db::ScheduleLog,
        stream_id: Option<uuid::Uuid>,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        Self {
            log_id: log.log_id,
            schedule_id: log.schedule_id,
            event_id: log.event_id,
            stream_id,
            scheduled_time: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &log.scheduled_time,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            executed_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &log.executed_at,
                    timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                tz_abbr
            ),
            is_backfill: log.is_backfill,
            created_at: format!(
                "{} {}",
                crate::timezone::format_in_timezone(&log.created_at, timezone, "%Y-%m-%d %H:%M:%S"),
                tz_abbr
            ),
        }
    }
}

/// Helper to extract retry display info from meta JSON
/// Handles both simple format ("retry": 3) and full format ("retry": {"attempts": 3, "delay": 5000})
pub fn extract_retry_display(meta: &Option<serde_json::Value>) -> (Option<i64>, Option<i64>) {
    if let Some(meta_obj) = meta.as_ref().and_then(|m| m.as_object())
        && let Some(retry_val) = meta_obj.get("retry")
    {
        if let Some(n) = retry_val.as_i64() {
            // Simple format: "retry": 3
            return (Some(n), None);
        } else if let Some(retry_obj) = retry_val.as_object() {
            // Full format: "retry": {"attempts": 3, "delay": 5000}
            let attempts = retry_obj.get("attempts").and_then(|v| v.as_i64());
            let delay = retry_obj.get("delay").and_then(|v| v.as_i64());
            return (attempts, delay);
        }
    }
    (None, None)
}

/// Display wrapper for ScheduleWithProject with pre-computed retry info
pub struct ScheduleDisplay {
    pub schedule_id: uuid::Uuid,
    pub build_id: uuid::Uuid,
    pub cron: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<serde_json::Value>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub active: bool,
    pub project_id: uuid::Uuid,
    pub project_name: String,
    // Pre-computed retry display
    pub retry_attempts: Option<i64>,
    pub retry_delay: Option<i64>,
    // One-time schedule info
    pub is_one_time: bool,
    pub scheduled_at: Option<String>,
    pub schedule_display: String,
}

impl From<hot::db::ScheduleWithProject> for ScheduleDisplay {
    fn from(s: hot::db::ScheduleWithProject) -> Self {
        let (retry_attempts, retry_delay) = extract_retry_display(&s.meta);

        // Check if this is a one-time @at: schedule
        let is_one_time = s.cron.starts_with(hot::db::AT_SCHEDULE_PREFIX);
        let (scheduled_at, schedule_display) = if is_one_time {
            // Parse the datetime and format it nicely
            let datetime_str = s
                .cron
                .strip_prefix(hot::db::AT_SCHEDULE_PREFIX)
                .unwrap_or("");
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(datetime_str) {
                let formatted = dt.format("%b %d, %Y %I:%M %p %Z").to_string();
                (Some(datetime_str.to_string()), formatted)
            } else {
                (Some(datetime_str.to_string()), datetime_str.to_string())
            }
        } else {
            (None, s.cron.clone())
        };

        Self {
            schedule_id: s.schedule_id,
            build_id: s.build_id,
            cron: s.cron,
            ns: s.ns,
            var: s.var,
            meta: s.meta,
            file: s.file,
            line: s.line,
            active: s.active,
            project_id: s.project_id,
            project_name: s.project_name,
            retry_attempts,
            retry_delay,
            is_one_time,
            scheduled_at,
            schedule_display,
        }
    }
}

impl ScheduleDisplay {
    pub fn from_with_timezone(
        s: hot::db::ScheduleWithProject,
        timezone: &str,
        tz_abbr: &str,
    ) -> Self {
        let (retry_attempts, retry_delay) = extract_retry_display(&s.meta);

        let is_one_time = s.cron.starts_with(hot::db::AT_SCHEDULE_PREFIX);
        let (scheduled_at, schedule_display) = if is_one_time {
            let datetime_str = s
                .cron
                .strip_prefix(hot::db::AT_SCHEDULE_PREFIX)
                .unwrap_or("");
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(datetime_str) {
                let utc_dt = dt.with_timezone(&chrono::Utc);
                let formatted = format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(&utc_dt, timezone, "%b %d, %Y %I:%M %p"),
                    tz_abbr
                );
                (Some(datetime_str.to_string()), formatted)
            } else {
                (Some(datetime_str.to_string()), datetime_str.to_string())
            }
        } else {
            (None, s.cron.clone())
        };

        Self {
            schedule_id: s.schedule_id,
            build_id: s.build_id,
            cron: s.cron,
            ns: s.ns,
            var: s.var,
            meta: s.meta,
            file: s.file,
            line: s.line,
            active: s.active,
            project_id: s.project_id,
            project_name: s.project_name,
            retry_attempts,
            retry_delay,
            is_one_time,
            scheduled_at,
            schedule_display,
        }
    }
}

/// Display wrapper for EventHandlerWithProject with pre-computed retry info
pub struct EventHandlerDisplay {
    pub event_handler_id: uuid::Uuid,
    pub build_id: uuid::Uuid,
    pub event_type: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<serde_json::Value>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub project_id: uuid::Uuid,
    pub project_name: String,
    // Pre-computed retry display
    pub retry_attempts: Option<i64>,
    pub retry_delay: Option<i64>,
}

impl From<hot::db::EventHandlerWithProject> for EventHandlerDisplay {
    fn from(h: hot::db::EventHandlerWithProject) -> Self {
        let (retry_attempts, retry_delay) = extract_retry_display(&h.meta);
        Self {
            event_handler_id: h.event_handler_id,
            build_id: h.build_id,
            event_type: h.event_type,
            ns: h.ns,
            var: h.var,
            meta: h.meta,
            file: h.file,
            line: h.line,
            project_id: h.project_id,
            project_name: h.project_name,
            retry_attempts,
            retry_delay,
        }
    }
}

#[derive(Template)]
#[template(path = "schedules_list.html")]
pub struct SchedulesList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub schedules: Vec<ScheduleDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_schedules: i64,
    pub org_active_schedules: i64,
    pub org_active_schedule_limit: i64,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "schedule_detail.html")]
pub struct ScheduleDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub schedule: hot::db::Schedule,
    pub schedule_logs: Vec<ScheduleLogDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_logs: i64,
    // Pre-computed retry display
    pub retry_attempts: Option<i64>,
    pub retry_delay: Option<i64>,
}

#[derive(Template)]
#[template(path = "event_handlers_list.html")]
pub struct EventHandlersList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub event_handlers: Vec<EventHandlerDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_handlers: i64,
    pub search_query: String,
}

/// Display wrapper for McpToolWithProject
/// Display wrapper for McpToolWithProject
pub struct McpToolDisplay {
    pub mcp_tool_id: uuid::Uuid,
    pub build_id: uuid::Uuid,
    pub service: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub project_name: String,
    pub auth_mode: String,
}

impl From<hot::db::McpToolWithProject> for McpToolDisplay {
    fn from(t: hot::db::McpToolWithProject) -> Self {
        let auth_mode = t.auth_mode().to_string();
        Self {
            mcp_tool_id: t.mcp_tool_id,
            build_id: t.build_id,
            service: t.service,
            ns: t.ns,
            var: t.var,
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
            file: t.file,
            line: t.line,
            project_name: t.project_name,
            auth_mode,
        }
    }
}

/// Service card for the MCP services list page
pub struct McpServiceCard {
    pub service: String,
    pub tool_count: i64,
    pub projects: Vec<String>,
}

#[derive(Template)]
#[template(path = "mcp_tools_list.html")]
pub struct McpServicesList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service_cards: Vec<McpServiceCard>,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "mcp_service_detail.html")]
pub struct McpServiceDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service: String,
    pub endpoint_url: String,
    pub custom_domains: Vec<String>,
    pub tools: Vec<McpToolDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_tools: i64,
    pub search_query: String,
}

// ============================================================================
// Webhooks UI
// ============================================================================

/// Display wrapper for WebhookWithProject
pub struct WebhookDisplay {
    pub webhook_id: uuid::Uuid,
    pub build_id: uuid::Uuid,
    pub service: String,
    pub path: String,
    pub method: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub auth_mode: String,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub project_name: String,
    /// Short token (last 12 hex chars of webhook_id) for URL obscurity
    pub token: String,
}

impl From<hot::db::WebhookWithProject> for WebhookDisplay {
    fn from(e: hot::db::WebhookWithProject) -> Self {
        let auth_mode = e.auth_mode().to_string();
        let token = hot::db::webhook::uuid_short(&e.webhook_id);
        Self {
            webhook_id: e.webhook_id,
            build_id: e.build_id,
            service: e.service,
            path: e.path,
            method: e.method,
            ns: e.ns,
            var: e.var,
            name: e.name,
            description: e.description,
            auth_mode,
            file: e.file,
            line: e.line,
            project_name: e.project_name,
            token,
        }
    }
}

/// Service card for the webhook services list page
pub struct WebhookServiceCard {
    pub service: String,
    pub endpoint_count: i64,
    pub projects: Vec<String>,
}

#[derive(Template)]
#[template(path = "webhook_services_list.html")]
pub struct WebhookServicesList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service_cards: Vec<WebhookServiceCard>,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "webhook_service_detail.html")]
pub struct WebhookServiceDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service: String,
    pub base_url: String,
    pub custom_domains: Vec<String>,
    pub endpoints: Vec<WebhookDisplay>,
    pub current_page_num: i64,
    pub total_pages: i64,
    pub start_page: i64,
    pub end_page: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub total_endpoints: i64,
    pub search_query: String,
}

#[derive(Template)]
#[template(path = "project_docs.html")]
pub struct ProjectDocsIndex<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub project_name: &'a str,
    pub has_docs: bool,
    pub build_info: Option<&'a str>,
    pub project_namespaces: Vec<crate::handlers::docs::NamespaceInfo>,
    pub dependencies: Vec<crate::handlers::docs::DependencyInfo>,
    pub has_schedules: bool,
    pub has_events: bool,
    pub has_webhooks: bool,
    pub has_mcp: bool,
    pub has_sends: bool,
}

// =============================================================================
// Alert Templates
// =============================================================================

#[derive(Template)]
#[template(path = "alerts/destinations_list.html")]
pub struct AlertDestinationsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub destinations: Vec<hot::db::alert::AlertDestination>,
    pub destination_details: std::collections::HashMap<uuid::Uuid, String>,
    pub is_admin: bool,
    pub flash_type: &'a str,
    pub flash_message: &'a str,
}

/// Simple name/id pair for team/user dropdowns
pub struct NamedItem {
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "alerts/destinations_new.html")]
pub struct AlertDestinationNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub teams: Vec<NamedItem>,
    pub users: Vec<NamedItem>,
    pub error_message: &'a str,
}

/// Parsed destination configuration for the edit form
#[derive(Default)]
pub struct DestinationConfigFields {
    // Email
    pub email_target: String, // "address", "org", "team", "user"
    pub email_address: String,
    pub email_team_id: String,
    pub email_user_id: String,
    // Slack
    pub slack_webhook_url: String,
    pub slack_channel: String,
    // PagerDuty
    pub pagerduty_routing_key: String,
    pub pagerduty_severity: String,
    // Webhook
    pub webhook_url: String,
    pub webhook_headers: String,
}

impl DestinationConfigFields {
    /// Parse config fields from a JSON value based on destination type
    pub fn from_config(dest_type_id: i16, config: &serde_json::Value) -> Self {
        let mut fields = Self::default();

        match dest_type_id {
            1 => {
                // Email - determine target type
                let target = config
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("address");
                fields.email_target = target.to_string();

                match target {
                    "address" | "" => {
                        fields.email_target = "address".to_string();
                        fields.email_address = config
                            .get("address")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    "org" => {}
                    "team" => {
                        fields.email_team_id = config
                            .get("team_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    "user" => {
                        fields.email_user_id = config
                            .get("user_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    _ => {
                        fields.email_target = "address".to_string();
                    }
                }
            }
            2 => {
                // Slack
                fields.slack_webhook_url = config
                    .get("webhook_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                fields.slack_channel = config
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            3 => {
                // PagerDuty
                fields.pagerduty_routing_key = config
                    .get("routing_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                fields.pagerduty_severity = config
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error")
                    .to_string();
            }
            4 => {
                // Webhook
                fields.webhook_url = config
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(headers) = config.get("headers")
                    && !headers.is_null()
                {
                    fields.webhook_headers =
                        serde_json::to_string_pretty(headers).unwrap_or_default();
                }
            }
            _ => {}
        }

        fields
    }
}

#[derive(Template)]
#[template(path = "alerts/destinations_edit.html")]
pub struct AlertDestinationEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub destination: hot::db::alert::AlertDestination,
    pub config_fields: DestinationConfigFields,
    pub teams: Vec<NamedItem>,
    pub users: Vec<NamedItem>,
}

/// Summary of connected channels/destinations for a subscription
pub struct SubscriptionConnections {
    pub channel_names: Vec<String>,
    pub destination_names: Vec<String>,
}

#[derive(Template)]
#[template(path = "alerts/subscriptions_list.html")]
pub struct AlertSubscriptionsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub subscriptions: Vec<hot::db::alert::AlertSubscription>,
    pub connections: Vec<SubscriptionConnections>,
    pub channels: Vec<hot::db::alert::AlertChannel>,
    pub destinations: Vec<hot::db::alert::AlertDestination>,
    pub is_admin: bool,
}

#[derive(Template)]
#[template(path = "alerts/channels_list.html")]
pub struct AlertChannelsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub channels: Vec<hot::db::alert::AlertChannel>,
    pub is_admin: bool,
}

#[derive(Template)]
#[template(path = "alerts/channels_new.html")]
pub struct AlertChannelNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
}

#[derive(Template)]
#[template(path = "alerts/channels_edit.html")]
pub struct AlertChannelEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub channel: hot::db::alert::AlertChannel,
}

#[derive(Template)]
#[template(path = "alerts/subscriptions_new.html")]
pub struct AlertSubscriptionNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub channels: Vec<hot::db::alert::AlertChannel>,
    pub destinations: Vec<hot::db::alert::AlertDestination>,
}

#[derive(Template)]
#[template(path = "alerts/subscriptions_edit.html")]
pub struct AlertSubscriptionEdit<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub subscription: hot::db::alert::AlertSubscription,
    pub channels: Vec<hot::db::alert::AlertChannel>,
    pub destinations: Vec<hot::db::alert::AlertDestination>,
    pub selected_channel_ids: Vec<uuid::Uuid>,
    pub selected_destination_ids: Vec<uuid::Uuid>,
}

/// Summary of delivery status for an alert
pub struct AlertDeliverySummary {
    pub total: usize,
    pub sent: usize,
    pub failed: usize,
    pub pending: usize,
}

#[derive(Template)]
#[template(path = "alerts/history_list.html")]
pub struct AlertHistoryList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub alerts: Vec<hot::db::alert::Alert>,
    pub delivery_summaries: Vec<AlertDeliverySummary>,
    pub current_page: i64,
    pub total_pages: i64,
    pub total_count: i64,
}

/// Delivery with destination info
pub struct AlertDeliveryDetail {
    pub delivery: hot::db::alert::AlertDelivery,
    pub destination: Option<hot::db::alert::AlertDestination>,
    /// For dynamic email destinations (org/team/user), the resolved user's email
    pub resolved_user_email: Option<String>,
}

#[derive(Template)]
#[template(path = "alerts/history_detail.html")]
pub struct AlertHistoryDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub alert: hot::db::alert::Alert,
    pub data_hot: String,
    pub data_json: String,
    pub run_id_from_data: Option<String>,
    pub delivery_details: Vec<AlertDeliveryDetail>,
}

#[derive(Template)]
#[template(path = "alerts/destination_verification.html")]
pub struct AlertDestinationVerification<'a> {
    pub title: &'a str,
    pub page_context: PublicPageContext,
    pub success: bool,
    pub result_title: &'a str,
    pub result_message: &'a str,
}

// -- Service Keys templates --

#[derive(Template)]
#[template(path = "service_keys_list.html")]
pub struct ServiceKeysList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service_keys: Vec<hot::db::service_key::ServiceKey>,
}

#[derive(Template)]
#[template(path = "service_keys_new.html")]
pub struct ServiceKeysNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub error_message: &'a str,
    pub generated_key: Option<String>,
    pub service_key_id: Option<Uuid>,
    pub mcp_tools_json: String,
    pub webhooks_json: String,
}

/// A single parsed permission rule for display purposes.
#[derive(Debug, Clone)]
pub struct PermissionDisplayRow {
    pub resource_type: String,
    pub path: String,
    pub actions: Vec<String>,
}

/// Parse a permissions JSON object into display rows.
/// Each key is `type:path` and each value is `["action1", "action2"]`.
pub fn parse_permissions_for_display(permissions: &serde_json::Value) -> Vec<PermissionDisplayRow> {
    let mut rows = Vec::new();
    if let Some(obj) = permissions.as_object() {
        for (key, actions_val) in obj {
            let (resource_type, path) = if let Some(colon_pos) = key.find(':') {
                (
                    key[..colon_pos].to_string(),
                    key[colon_pos + 1..].to_string(),
                )
            } else {
                (key.clone(), "*".to_string())
            };
            let actions: Vec<String> = actions_val
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            rows.push(PermissionDisplayRow {
                resource_type,
                path,
                actions,
            });
        }
    }
    rows.sort_by(|a, b| a.resource_type.cmp(&b.resource_type));
    rows
}

#[derive(Template)]
#[template(path = "service_keys_detail.html")]
pub struct ServiceKeyDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub service_key: hot::db::service_key::ServiceKey,
    pub permission_rows: Vec<PermissionDisplayRow>,
    pub metadata_json: Option<String>,
}

// -- Custom Domains templates --

#[derive(Template)]
#[template(path = "domains_list.html")]
pub struct DomainsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub domains: Vec<hot::db::domain::Domain>,
}

#[derive(Template)]
#[template(path = "domains_new.html")]
pub struct DomainsNew<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub error_message: &'a str,
}

#[derive(Template)]
#[template(path = "domains_detail.html")]
pub struct DomainDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub domain: hot::db::domain::Domain,
    pub flash_message: &'a str,
    pub flash_type: &'a str,
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

pub struct AgentCard {
    pub agent_id: String,
    pub build_id: String,
    pub qualified_name: String,
    pub namespace_qualified_name: String,
    pub display_name: String,
    pub namespace: String,
    pub description: String,
    pub tags: Vec<String>,
    pub handler_count: i64,
    pub project_name: String,
    pub source_file: Option<String>,
    pub source_line: Option<i32>,
}

pub struct AgentHandlerDisplay {
    pub handler_type: String,
    pub trigger: String,
    pub function: String,
    pub retry: String,
    pub source: String,
    pub source_build_id: String,
    pub source_file: Option<String>,
    pub source_line: Option<i32>,
}

pub struct WorkflowListCard {
    pub url: String,
    pub build_id: String,
    pub kind: String,
    pub kind_label: String,
    pub qualified_name: String,
    pub display_name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub handler_count: i64,
    pub project_name: String,
    pub source_file: Option<String>,
    pub source_line: Option<i32>,
    /// Agent cards only: "green" | "yellow" | "red" | "idle"; empty for
    /// workflow/unnamed cards (no health badge rendered).
    pub health_color: String,
    pub success_rate: f64,
    pub runs_24h: i64,
}

#[derive(Template)]
#[template(path = "agents_list.html")]
pub struct AgentsList<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub workflow_cards: Vec<WorkflowListCard>,
    pub search_query: String,
    pub status_filter: String,
    pub active_tab: String,
}

#[derive(Template)]
#[template(path = "agents_detail.html")]
pub struct AgentsDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub agent: AgentCard,
    /// Header stats strip (24h window).
    pub health_color: String,
    pub health_label: String,
    pub success_rate: f64,
    pub runs_24h: i64,
    /// Formatted start time of the most recent run, or "—" when never run.
    pub last_run_formatted: String,
    pub full_description: String,
    pub config_fields: Vec<(String, String)>,
    pub handlers: Vec<AgentHandlerDisplay>,
    pub runs: Vec<RunDisplay>,
    pub runs_current_page: i64,
    pub runs_total_pages: i64,
    pub runs_has_next: bool,
    pub runs_has_prev: bool,
    pub runs_total: i64,
    pub streams: Vec<StreamListItem>,
    pub active_tab: &'a str,
}

#[derive(Template)]
#[template(path = "workflow_detail.html")]
pub struct WorkflowDetail<'a> {
    pub title: &'a str,
    pub page_context: PrivatePageContext,
    pub workflow: WorkflowListCard,
    pub graph_data_url: String,
    pub handlers: Vec<AgentHandlerDisplay>,
    pub active_tab: &'a str,
}

pub struct AgentHealthCard {
    pub qualified_name: String,
    pub display_name: String,
    pub total_runs: i64,
    pub success_rate: f64,
    pub health_color: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_safe_json_cannot_close_script_element() {
        let json = script_safe_json(
            &serde_json::json!({"name": "</script><script>alert(1)</script>"}),
            "{}",
        );
        assert!(!json.contains('<'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["name"],
            "</script><script>alert(1)</script>"
        );
    }

    /// Slugify a heading the same way package docs routes do.
    /// Non-alphanumeric chars become hyphens; consecutive hyphens collapse; leading/trailing stripped.
    fn slugify(text: &str) -> String {
        text.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Parse a heading that may have an explicit anchor: "Title {#my-anchor}"
    fn parse_heading_anchor(text: &str) -> (String, Option<String>) {
        if let Some(brace_start) = text.find(" {#")
            && let Some(brace_end) = text[brace_start..].find('}')
        {
            let title = text[..brace_start].trim().to_string();
            let anchor = text[brace_start + 3..brace_start + brace_end].to_string();
            return (title, Some(anchor));
        }

        (text.trim().to_string(), None)
    }

    /// Extract all heading anchors from a markdown file.
    /// Returns a Vec of (level, anchor) pairs for every heading found.
    fn extract_anchors(markdown: &str) -> Vec<(u8, String)> {
        let mut anchors = Vec::new();
        for line in markdown.lines() {
            let trimmed = line.trim();
            let (level, rest) = if let Some(rest) = trimmed.strip_prefix("# ") {
                (1u8, rest)
            } else if let Some(rest) = trimmed.strip_prefix("## ") {
                (2, rest)
            } else if let Some(rest) = trimmed.strip_prefix("### ") {
                (3, rest)
            } else if let Some(rest) = trimmed.strip_prefix("#### ") {
                (4, rest)
            } else {
                continue;
            };
            let (title, explicit) = parse_heading_anchor(rest);
            let anchor = explicit.unwrap_or_else(|| slugify(&title));
            anchors.push((level, anchor));
        }
        anchors
    }

    #[test]
    fn truncate_string_is_unicode_safe() {
        assert_eq!(truncate_string("hello", 10), "hello");
        assert_eq!(truncate_string("hello", 3), "hel...");
        assert_eq!(truncate_string("🔥🔥🔥", 2), "🔥🔥...");
        assert_eq!(truncate_string("éclair", 1), "é...");
    }

    /// Validate that every docs_path in DOCS_PATH_MAPPINGS points to a real
    /// markdown file and (if it has a fragment) a real heading anchor.
    ///
    /// This catches stale links when docs headings are renamed or removed.
    #[test]
    fn docs_path_anchors_are_valid() {
        let docs_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("resources/docs");

        let mut errors: Vec<String> = Vec::new();

        for (page, docs_path) in DOCS_PATH_MAPPINGS {
            // Split "/docs/app#runs" into path="/docs/app" and fragment="runs"
            let (url_path, fragment) = match docs_path.split_once('#') {
                Some((p, f)) => (p, Some(f)),
                None => (*docs_path, None),
            };

            // "/docs/app" → "resources/docs/app/index.md"
            let relative = url_path.strip_prefix("/docs/").unwrap_or(url_path);
            let md_path = docs_root.join(relative).join("index.md");

            if !md_path.exists() {
                errors.push(format!(
                    "page {:?}: markdown file not found: {}",
                    page,
                    md_path.display()
                ));
                continue;
            }

            // If there's a fragment, verify the anchor exists
            if let Some(frag) = fragment {
                let content = std::fs::read_to_string(&md_path).unwrap();
                let anchors = extract_anchors(&content);
                let anchor_strings: Vec<&str> = anchors.iter().map(|(_, a)| a.as_str()).collect();

                if !anchor_strings.contains(&frag) {
                    errors.push(format!(
                        "page {:?}: anchor #{} not found in {}\n  available anchors: {:?}",
                        page,
                        frag,
                        md_path.display(),
                        anchor_strings
                    ));
                }
            }
        }

        if !errors.is_empty() {
            panic!(
                "Docs link validation failed ({} error{}):\n\n{}",
                errors.len(),
                if errors.len() == 1 { "" } else { "s" },
                errors.join("\n\n")
            );
        }
    }
}
