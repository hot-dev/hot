// Engine Event Emitter System
//
// This module provides event emission capabilities for the bytecode engine,

use crate::lang::event::ExecutionContext;
use crate::val::Val;
use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

use crate::val;

/// Engine event for execution
#[derive(Debug, Clone, PartialEq)]
pub struct EngineEvent {
    pub execution_context: ExecutionContext,
    pub event_id: Uuid,
    pub event_type: String,
    pub event_data: Val,
    pub event_time: DateTime<Utc>,
}

impl EngineEvent {
    /// Creates a new event with the execution context, event type, and data.
    ///
    /// The event ID and timestamp are automatically generated.
    pub fn new(execution_context: ExecutionContext, event_type: String, event_data: Val) -> Self {
        Self {
            execution_context,
            event_id: Uuid::now_v7(),
            event_type,
            event_data,
            event_time: Utc::now(),
        }
    }

    /// Creates a run:start event
    pub fn run_start(execution_context: &ExecutionContext) -> Self {
        Self::new(
            execution_context.clone(),
            "run:start".to_string(),
            val!({
                "start_time": Utc::now().to_rfc3339(),
            }),
        )
    }

    /// Creates a run:stop event with result
    pub fn run_stop(execution_context: &ExecutionContext, result: Val) -> Self {
        Self::new(
            execution_context.clone(),
            "run:stop".to_string(),
            val!({
                "stop_time": Utc::now().to_rfc3339(),
                "result": result,
            }),
        )
    }

    /// Creates a run:fail event with canonical status fields plus the legacy
    /// `failure` payload consumed by existing database/UI code.
    pub fn run_fail(execution_context: &ExecutionContext, failure: Val) -> Self {
        let (kind, explicit) = match typed_payload_name(&failure).as_deref() {
            Some("::hot::run/Failure" | "::hot::task/Failure") => ("failure", true),
            _ => ("unhandled-error", false),
        };
        let msg = payload_msg(&failure).unwrap_or_else(|| failure.to_string());
        let err = payload_err(&failure).unwrap_or_else(|| failure.clone());
        let origin = payload_origin(&failure).unwrap_or(Val::Null);

        Self::new(
            execution_context.clone(),
            "run:fail".to_string(),
            val!({
                "stop_time": Utc::now().to_rfc3339(),
                "failure": failure,
                "kind": kind,
                "msg": msg,
                "err": err,
                "origin": origin,
                "explicit": explicit,
            }),
        )
    }

    /// Creates a run:cancel event with canonical status fields plus the legacy
    /// `cancellation` payload consumed by existing database/UI code.
    pub fn run_cancel(execution_context: &ExecutionContext, cancellation: Val) -> Self {
        let msg = payload_msg(&cancellation).unwrap_or_else(|| cancellation.to_string());
        let data = payload_data(&cancellation).unwrap_or_else(|| cancellation.clone());
        let origin = payload_origin(&cancellation).unwrap_or(Val::Null);

        Self::new(
            execution_context.clone(),
            "run:cancel".to_string(),
            val!({
                "stop_time": Utc::now().to_rfc3339(),
                "cancellation": cancellation,
                "kind": "cancellation",
                "msg": msg,
                "data": data,
                "origin": origin,
                "explicit": true,
            }),
        )
    }

    /// Creates a call:start event for function invocation tracking
    #[allow(clippy::too_many_arguments)]
    pub fn call_start(
        execution_context: &ExecutionContext,
        call_id: Uuid,
        parent_call_id: Option<Uuid>,
        function_name: String,
        static_scope: String,
        runtime_path: String,
        call_depth: usize,
        args: Vec<Val>,
        source: Option<&crate::lang::bytecode::SourceLocation>,
        start_time: DateTime<Utc>,
        flow: Option<Val>,
    ) -> Self {
        let mut event_data = val!({
            "call_id": call_id.to_string(),
            "parent_call_id": parent_call_id.map(|id| Val::from(id.to_string())).unwrap_or(Val::Null),
            "function_name": function_name,
            "static_scope": static_scope,
            "runtime_path": runtime_path,
            "call_depth": call_depth as i64,
            "args": Val::Vec(args),
            "start_time": start_time.to_rfc3339(),
            "flow": flow.unwrap_or(Val::Null),
        });

        // Add source location if available
        if let Some(source) = source {
            event_data = event_data.merge(&val!({
                "file": source.file.clone().unwrap_or("<unknown>".to_string()),
                "line": source.line as i64,
                "column": source.column as i64,
                "position": source.position as i64,
            }));
        } else {
            event_data = event_data.merge(&val!({
                "file": Val::Null,
                "line": 0i64,
                "column": 0i64,
                "position": 0i64,
            }));
        }

        Self::new(
            execution_context.clone(),
            "call:start".to_string(),
            event_data,
        )
    }

    /// Creates a call:stop event for function return/completion
    pub fn call_stop(
        execution_context: &ExecutionContext,
        call_id: Uuid,
        return_value: Option<Val>,
        error: Option<String>,
        end_time: DateTime<Utc>,
        duration_us: i64,
    ) -> Self {
        // Unwrap Result.Ok values for cleaner display in the UI
        // Result.Err values are kept as-is since they indicate errors
        let unwrapped_return_value = return_value
            .map(|val| {
                if val.is_ok() {
                    val.unwrap_ok().cloned().unwrap_or(val)
                } else {
                    val
                }
            })
            .unwrap_or(Val::Null);

        Self::new(
            execution_context.clone(),
            "call:stop".to_string(),
            val!({
                "call_id": call_id.to_string(),
                "return_value": unwrapped_return_value,
                "error": error.map(Val::from).unwrap_or(Val::Null),
                "end_time": end_time.to_rfc3339(),
                "duration_us": duration_us,
            }),
        )
    }
}

fn map_get<'a>(val: &'a Val, key: &str) -> Option<&'a Val> {
    match val {
        Val::Map(map) => map.get(&Val::from(key)),
        _ => None,
    }
}

fn typed_payload_name(val: &Val) -> Option<String> {
    match map_get(val, "$type") {
        Some(Val::Str(type_name)) => Some((**type_name).to_owned()),
        _ => None,
    }
}

fn payload_body(val: &Val) -> &Val {
    map_get(val, "$val").unwrap_or(val)
}

fn payload_string_field(val: &Val, key: &str) -> Option<String> {
    match map_get(payload_body(val), key).or_else(|| map_get(val, key)) {
        Some(Val::Str(s)) => Some((**s).to_owned()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

fn payload_msg(val: &Val) -> Option<String> {
    payload_string_field(val, "$msg").or_else(|| payload_string_field(val, "msg"))
}

fn payload_err(val: &Val) -> Option<Val> {
    map_get(payload_body(val), "$err")
        .or_else(|| map_get(val, "$err"))
        .cloned()
}

fn payload_data(val: &Val) -> Option<Val> {
    map_get(payload_body(val), "$data")
        .or_else(|| map_get(val, "$data"))
        .cloned()
}

fn payload_origin(val: &Val) -> Option<Val> {
    map_get(val, "$origin")
        .or_else(|| map_get(payload_body(val), "$origin"))
        .cloned()
}

pub trait EngineEventEmitter: Send + Sync {
    /// Emits an event.
    ///
    /// Implementations should handle the event appropriately (e.g., log to console,
    /// store in database, send to monitoring system, etc.).
    fn emit(&self, event: EngineEvent);

    /// Flush all pending writes and wait for them to complete.
    ///
    /// This should be called before publishing events that reference the current run
    /// (e.g., before send-event) to ensure the run record exists in the database
    /// before child events are processed.
    ///
    /// Default implementation does nothing (for emitters that don't buffer writes).
    fn flush(&self) -> Result<(), String> {
        Ok(())
    }

    /// Gracefully shutdown the emitter and wait for all events to be processed.
    ///
    /// Default implementation does nothing. Emitters that use background processing
    /// should override this to ensure all events are flushed before shutdown.
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

pub mod console;
pub mod database;
#[cfg(test)]
pub mod database_run_events_test;
pub mod database_writer;
#[cfg(test)]
pub mod integration_test;
pub mod postgres_safety;
#[cfg(test)]
pub mod run_events_test;
#[cfg(test)]
pub mod test;

pub use console::ConsoleEngineEventEmitter;
pub use database::DatabaseEngineEventEmitter;

// Filtering system
use regex::Regex;

/// A filtering wrapper that applies event filters before delegating to the underlying emitter
pub struct FilteredEmitter<T: EngineEventEmitter> {
    inner: T,
    filter: Option<EngineEventFilter>,
}

impl<T: EngineEventEmitter> FilteredEmitter<T> {
    pub fn new(inner: T, filter_config: Option<&Val>) -> Result<Self, String> {
        let filter = if let Some(config) = filter_config {
            Some(EngineEventFilter::new(config)?)
        } else {
            None
        };

        Ok(FilteredEmitter { inner, filter })
    }
}

impl<T: EngineEventEmitter> EngineEventEmitter for FilteredEmitter<T> {
    fn emit(&self, event: EngineEvent) {
        // Apply filtering if configured
        if let Some(filter) = &self.filter
            && filter.should_filter_event(&event)
        {
            // EngineEvent is filtered out, don't emit
            return;
        }

        // EngineEvent passed filtering, emit to underlying emitter
        self.inner.emit(event);
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.shutdown()
    }
}

/// Get the resolved emitter configuration with defaults applied.
///
/// Type resolution order (highest priority wins):
/// 1. User-provided type (from hot.hot, or CLI --emitter.type)
/// 2. Context default: "db" when `in_project` is true, "none" otherwise
///
/// When `in_project` is true (hot.hot exists), defaults to "db" for run tracking.
/// When `in_project` is false, defaults to "none" (emitter disabled).
pub fn get_resolved_conf(conf: Val, in_project: bool) -> Val {
    // Check if user explicitly set a type (from hot.hot or CLI)
    // Empty string means not set, so use context-based default
    let user_type = conf.get_str_or_default("type", "");
    let user_explicitly_set_type = !user_type.is_empty();

    // Default type depends on context:
    // - In project: "db" enables run tracking in dashboard
    // - Outside project: "none" disables emitter
    // But if user explicitly set a type, use that
    let emitter_type = if user_explicitly_set_type {
        user_type
    } else if in_project {
        "db".to_string()
    } else {
        "none".to_string()
    };

    // By default, var events are disabled (empty include list filters out all vars)
    let default_filter = val!({
        "filter": {
            "var": {
                "ns": {
                    "exclude": [".*"],  // Exclude everything by default
                    "include": []       // Empty include list - no vars will be recorded
                },
                "meta": {
                    "exclude": [],
                    "include": []
                },
                "value": {
                    "exclude": [],
                    "include": []
                }
            }
        }
    });

    // Merge filter settings from provided conf (user filter settings override defaults)
    // Then set the type explicitly so it doesn't get overwritten by empty string from conf
    let merged = default_filter.merge(&conf);
    merged.set_str("type", Some(emitter_type), "")
}

/// EngineEvent filter configuration with compiled regex patterns
#[derive(Debug, Clone)]
pub struct EngineEventFilter {
    // Variable event filters
    var_filter: Option<VarFilter>,
}

/// Filter configuration for variable events
#[derive(Debug, Clone)]
pub struct VarFilter {
    ns_exclude: Vec<Regex>,
    ns_include: Vec<Regex>,
    meta_exclude: Vec<Regex>,
    meta_include: Vec<Regex>,
    value_exclude: Vec<Regex>,
    value_include: Vec<Regex>,
}

impl EngineEventFilter {
    /// Create a new EngineEventFilter from configuration
    pub fn new(config: &Val) -> Result<Self, String> {
        let filter_config = config.get("filter").unwrap_or(Val::Null);

        // Parse var filters
        let var_filter = if let Some(var_config) = filter_config.get("var") {
            Some(Self::parse_var_filter(&var_config)?)
        } else {
            None
        };

        Ok(EngineEventFilter { var_filter })
    }

    /// Parse var filter from configuration
    fn parse_var_filter(config: &Val) -> Result<VarFilter, String> {
        let ns_exclude = match config.get("ns.exclude") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        let ns_include = match config.get("ns.include") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        let meta_exclude = match config.get("meta.exclude") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        let meta_include = match config.get("meta.include") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        let value_exclude = match config.get("value.exclude") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        let value_include = match config.get("value.include") {
            Some(patterns) => Self::compile_regex_patterns(&patterns)?,
            None => vec![],
        };

        Ok(VarFilter {
            ns_exclude,
            ns_include,
            meta_exclude,
            meta_include,
            value_exclude,
            value_include,
        })
    }

    /// Compile regex patterns from a Val list
    fn compile_regex_patterns(val: &Val) -> Result<Vec<Regex>, String> {
        match val {
            Val::Vec(patterns) => {
                let mut regexes = Vec::new();
                for pattern in patterns {
                    if let Val::Str(pattern_str) = pattern {
                        match Regex::new(pattern_str) {
                            Ok(regex) => regexes.push(regex),
                            Err(e) => {
                                return Err(format!(
                                    "Invalid regex pattern '{}': {}",
                                    pattern_str, e
                                ));
                            }
                        }
                    }
                }
                Ok(regexes)
            }
            _ => Ok(vec![]),
        }
    }

    /// Check if an event should be filtered out (returns true if event should be filtered out)
    pub fn should_filter_event(&self, event: &EngineEvent) -> bool {
        // Get the var filter
        let Some(filter) = &self.var_filter else {
            // No filter configured, don't filter
            return false;
        };

        // Only filter variable events (var:start and var:stop) for now
        if !event.event_type.starts_with("var:") {
            return false;
        }

        let Val::Map(data) = &event.event_data else {
            return false;
        };

        // Extract the fields to check
        let ns = data
            .get(&Val::from("ns"))
            .and_then(|v| {
                if let Val::Str(s) = v {
                    Some(&**s)
                } else {
                    None
                }
            })
            .unwrap_or("");

        // Debug logging
        tracing::debug!(
            "Filtering var event: event_type={}, ns='{}', ns_exclude_count={}, ns_include_count={}",
            event.event_type,
            ns,
            filter.ns_exclude.len(),
            filter.ns_include.len()
        );

        let meta = data
            .get(&Val::from("meta"))
            .map(|v| self.val_to_string(v))
            .unwrap_or_else(|| "null".to_string());

        let value = if event.event_type == "var:stop" {
            data.get(&Val::from("value"))
                .map(|v| self.val_to_string(v))
                .unwrap_or_else(|| "null".to_string())
        } else {
            "null".to_string()
        };

        // Apply filtering logic: if any exclude match occurs that is not also matched by include, filter it out
        let ns_filtered = self.is_filtered_by_patterns(ns, &filter.ns_exclude, &filter.ns_include);
        let meta_filtered =
            self.is_filtered_by_patterns(&meta, &filter.meta_exclude, &filter.meta_include);
        let value_filtered =
            self.is_filtered_by_patterns(&value, &filter.value_exclude, &filter.value_include);

        let should_filter = ns_filtered || meta_filtered || value_filtered;

        // Debug logging
        if should_filter {
            tracing::debug!(
                "VAR EVENT FILTERED: ns='{}', ns_filtered={}",
                ns,
                ns_filtered
            );
        }

        // AND logic: if any of the three filters says to filter out, then filter out
        should_filter
    }

    /// Check if a value should be filtered based on exclude/include patterns
    fn is_filtered_by_patterns(
        &self,
        value: &str,
        exclude_patterns: &[Regex],
        include_patterns: &[Regex],
    ) -> bool {
        // Check if value matches any exclude pattern
        let matches_exclude = exclude_patterns.iter().any(|regex| regex.is_match(value));

        if !matches_exclude {
            // If no exclude match, don't filter
            return false;
        }

        // If matches exclude, check if it also matches include
        let matches_include = include_patterns.iter().any(|regex| regex.is_match(value));

        // Filter out if matches exclude but not include
        !matches_include
    }

    /// Convert a Val to a string representation for regex matching
    fn val_to_string(&self, val: &Val) -> String {
        match val {
            Val::Null => "null".to_string(),
            Val::Bool(b) => b.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Dec(d) => d.to_string(),
            Val::Str(s) => (**s).to_owned(),
            Val::Vec(_) => serde_json::to_string(val).unwrap_or_else(|_| "[]".to_string()),
            Val::Map(_) => serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string()),
            _ => serde_json::to_string(val).unwrap_or_else(|_| "unknown".to_string()),
        }
    }
}

/// Enrich a runtime value with AST metadata (function signatures, docs, type info)
///
/// This function looks up the variable in the AST and merges runtime value with
/// compile-time metadata like function signatures, documentation, and type definitions.
///
/// # Arguments
/// * `ast` - Optional reference to the AST for metadata lookup
/// * `val` - The runtime value to enrich
/// * `static_scope` - The static scope path for AST lookup (e.g., "::demo::schedule")
/// * `var_name` - The variable name (without $ prefix)
///
/// # Returns
/// A Val containing both runtime_value and metadata, or the original value if no AST
pub fn enrich_value_with_metadata(
    ast: Option<&crate::lang::ast::HotAst>,
    val: &Val,
    static_scope: &str,
    var_name: &str,
) -> Val {
    // If we don't have an AST, return the raw value
    let Some(ast) = ast else {
        tracing::debug!(
            "enrich_value_with_metadata: No AST provided for var '{}'",
            var_name
        );
        return val.clone();
    };

    // Try to extract rich metadata from the AST
    tracing::debug!(
        "enrich_value_with_metadata: Looking up var '{}' in scope '{}'",
        var_name,
        static_scope
    );

    if let Some(rich_meta) = ast.extract_rich_metadata(static_scope, var_name) {
        tracing::debug!(
            "enrich_value_with_metadata: Found rich metadata for '{}': {:?}",
            var_name,
            rich_meta
        );
        // Merge runtime value with AST metadata
        val!({
            "runtime_value": val.clone(),
            "metadata": rich_meta,
        })
    } else {
        tracing::debug!(
            "enrich_value_with_metadata: No metadata found for var '{}' in scope '{}'",
            var_name,
            static_scope
        );
        // No metadata found in AST, return raw value
        val.clone()
    }
}
