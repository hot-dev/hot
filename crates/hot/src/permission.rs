//! Unified permission model for Hot sessions and (future) team-based access control.
//!
//! Permissions are a map of resource URNs to action arrays:
//! ```json
//! {
//!   "mcp:weather/get-forecast": ["execute"],
//!   "stream:abc-123": ["read"],
//!   "event:order:*": ["create"]
//! }
//! ```
//!
//! Resource URN format: `type:path`
//! - `type` is the resource category (mcp, stream, event, run, webhook, etc.)
//! - `path` is the resource identifier, may contain `/` for hierarchy and `*` for suffix wildcard
//!
//! Actions: create, read, update, delete, execute, * (wildcard)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ============================================================================
// Actions
// ============================================================================

/// Well-known action constants
pub mod actions {
    pub const CREATE: &str = "create";
    pub const READ: &str = "read";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const EXECUTE: &str = "execute";
    pub const WILDCARD: &str = "*";

    /// All concrete actions (excluding wildcard)
    pub const ALL: &[&str] = &[CREATE, READ, UPDATE, DELETE, EXECUTE];

    /// Check if a string is a valid action
    pub fn is_valid(action: &str) -> bool {
        matches!(action, CREATE | READ | UPDATE | DELETE | EXECUTE | WILDCARD)
    }
}

// ============================================================================
// Resource Types
// ============================================================================

/// Well-known resource type constants
pub mod resource_types {
    pub const MCP: &str = "mcp";
    pub const WEBHOOK: &str = "webhook";
    pub const STREAM: &str = "stream";
    pub const EVENT: &str = "event";
    pub const RUN: &str = "run";
    pub const PROJECT: &str = "project";
    pub const BUILD: &str = "build";
    pub const CONTEXT: &str = "context";
    pub const KEY: &str = "key";
    pub const SESSION: &str = "session";
    pub const ENV: &str = "env";

    /// All known resource types
    pub const ALL: &[&str] = &[
        MCP, WEBHOOK, STREAM, EVENT, RUN, PROJECT, BUILD, CONTEXT, KEY, SESSION, ENV,
    ];

    /// Resource types that service keys are allowed to use.
    /// Service keys are customer-facing credentials and must not access
    /// administrative resources like context vars, builds, keys, sessions, etc.
    pub const SERVICE_KEY_ALLOWED: &[&str] = &[MCP, WEBHOOK, STREAM, EVENT, RUN];

    /// Check if a string is a known resource type
    pub fn is_known(resource_type: &str) -> bool {
        ALL.contains(&resource_type)
    }

    /// Get valid actions for a resource type.
    /// Returns None for unknown types (allow all).
    pub fn valid_actions(resource_type: &str) -> Option<&'static [&'static str]> {
        use super::actions::*;
        match resource_type {
            MCP => Some(&[EXECUTE]),
            WEBHOOK => Some(&[EXECUTE]),
            STREAM => Some(&[READ]),
            EVENT => Some(&[CREATE, READ]),
            RUN => Some(&[READ]),
            PROJECT => Some(&[CREATE, READ, UPDATE, DELETE]),
            BUILD => Some(&[CREATE, READ, EXECUTE]),
            CONTEXT => Some(&[CREATE, READ, UPDATE, DELETE]),
            KEY => Some(&[CREATE, READ, UPDATE, DELETE]),
            SESSION => Some(&[CREATE, READ, DELETE]),
            ENV => Some(&[READ]),
            _ => None,
        }
    }
}

// ============================================================================
// Permission Set
// ============================================================================

/// A set of permissions mapping resource URNs to allowed actions.
///
/// Resource URN format: `type:path` where path can contain `*` suffix wildcard.
/// Actions: create, read, update, delete, execute, * (all).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Permissions(HashMap<String, Vec<String>>);

#[derive(Debug)]
pub enum PermissionError {
    /// An action string is not recognized
    InvalidAction(String),
    /// A resource URN is malformed (missing type prefix)
    InvalidResource(String),
    /// A resource has an empty action list (grants nothing, likely a mistake)
    EmptyActionList(String),
    /// An action is not valid for the given resource type
    ActionNotValidForResource {
        action: String,
        resource: String,
        valid_actions: Vec<String>,
    },
    /// A resource type is not allowed for this credential type
    DisallowedResourceType {
        resource_type: String,
        allowed: Vec<String>,
    },
    /// The requested permissions exceed what the parent allows
    Escalation { resource: String, action: String },
    /// JSON deserialization error
    JsonError(String),
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermissionError::InvalidAction(action) => {
                write!(
                    f,
                    "Invalid action: '{}'. Valid actions: create, read, update, delete, execute, *",
                    action
                )
            }
            PermissionError::InvalidResource(resource) => {
                write!(
                    f,
                    "Invalid resource URN: '{}'. Expected format: type:path (e.g. mcp:*, stream:my-id)",
                    resource
                )
            }
            PermissionError::EmptyActionList(resource) => {
                write!(
                    f,
                    "Resource '{}' has an empty action list. Each resource must have at least one action.",
                    resource
                )
            }
            PermissionError::ActionNotValidForResource {
                action,
                resource,
                valid_actions,
            } => {
                write!(
                    f,
                    "Action '{}' is not valid for resource '{}'. Valid actions: {}",
                    action,
                    resource,
                    valid_actions.join(", ")
                )
            }
            PermissionError::DisallowedResourceType {
                resource_type,
                allowed,
            } => {
                write!(
                    f,
                    "Resource type '{}' is not allowed for this credential. Allowed types: {}",
                    resource_type,
                    allowed.join(", ")
                )
            }
            PermissionError::Escalation { resource, action } => {
                write!(
                    f,
                    "Permission escalation: action '{}' on resource '{}' exceeds parent permissions",
                    action, resource
                )
            }
            PermissionError::JsonError(msg) => write!(f, "Permission JSON error: {}", msg),
        }
    }
}

impl std::error::Error for PermissionError {}

impl Permissions {
    /// Create an empty permission set
    pub fn new() -> Self {
        Permissions(HashMap::new())
    }

    /// Create from a HashMap
    pub fn from_map(map: HashMap<String, Vec<String>>) -> Self {
        Permissions(map)
    }

    /// Create from a JSON value (the stored format).
    /// This parses the structure but does NOT validate the contents.
    /// Use `from_json_validated` for untrusted input.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, PermissionError> {
        let map: HashMap<String, Vec<String>> = serde_json::from_value(value.clone())
            .map_err(|e| PermissionError::JsonError(e.to_string()))?;
        Ok(Permissions(map))
    }

    /// Create from a JSON value and validate the contents.
    /// Use this for all untrusted input (API requests, UI form submissions).
    pub fn from_json_validated(value: &serde_json::Value) -> Result<Self, PermissionError> {
        let perms = Self::from_json(value)?;
        perms.validate()?;
        Ok(perms)
    }

    /// Convert to a JSON value for storage
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(serde_json::Value::Null)
    }

    /// Get the inner map
    pub fn inner(&self) -> &HashMap<String, Vec<String>> {
        &self.0
    }

    /// Check if empty (no permissions)
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Validate all permissions: actions are valid, resource URNs are well-formed,
    /// and actions are appropriate for each resource type.
    pub fn validate(&self) -> Result<(), PermissionError> {
        for (resource, action_list) in &self.0 {
            // Reject empty resource keys
            if resource.is_empty() {
                return Err(PermissionError::InvalidResource(resource.clone()));
            }

            // Validate resource URN format (must be `type:path` or `*:*`)
            let resource_type = parse_resource_type(resource)
                .ok_or_else(|| PermissionError::InvalidResource(resource.clone()))?;

            // Reject empty action lists — they are meaningless (grant nothing)
            if action_list.is_empty() {
                return Err(PermissionError::EmptyActionList(resource.clone()));
            }

            // Validate each action
            for action in action_list {
                if action.is_empty() {
                    return Err(PermissionError::InvalidAction(String::new()));
                }

                if !actions::is_valid(action) {
                    return Err(PermissionError::InvalidAction(action.clone()));
                }

                // If action is wildcard, skip per-type validation
                if action == actions::WILDCARD {
                    continue;
                }

                // If resource type is wildcard (*:*), any action is valid
                if resource_type == "*" {
                    continue;
                }

                // Validate action is valid for this resource type
                if let Some(valid) = resource_types::valid_actions(resource_type)
                    && !valid.contains(&action.as_str())
                {
                    return Err(PermissionError::ActionNotValidForResource {
                        action: action.clone(),
                        resource: resource.clone(),
                        valid_actions: valid.iter().map(|s| s.to_string()).collect(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate that all resource types in this permission set are within the allowed set.
    /// Rejects `*:*` (universal wildcard) unless `"*"` is in the allowed list.
    /// Used to restrict service keys to customer-appropriate resource types.
    pub fn validate_resource_types(&self, allowed: &[&str]) -> Result<(), PermissionError> {
        let allowed_strs: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
        for resource in self.0.keys() {
            let resource_type = parse_resource_type(resource)
                .ok_or_else(|| PermissionError::InvalidResource(resource.clone()))?;
            if resource_type == "*" || !allowed.contains(&resource_type) {
                return Err(PermissionError::DisallowedResourceType {
                    resource_type: resource_type.to_string(),
                    allowed: allowed_strs,
                });
            }
        }
        Ok(())
    }

    /// Check if this permission set grants a specific action on a specific resource.
    ///
    /// Handles wildcard matching:
    /// - `*:*` matches any resource
    /// - `mcp:*` matches `mcp:weather/get-forecast`
    /// - `mcp:weather/*` matches `mcp:weather/get-forecast` but not `mcp:time/now`
    /// - `["*"]` action list matches any action
    pub fn has_permission(&self, resource: &str, action: &str) -> bool {
        for (pattern, action_list) in &self.0 {
            if resource_matches(pattern, resource) && action_matches(action_list, action) {
                return true;
            }
        }
        false
    }

    /// Check if this permission set grants read access to a specific stream.
    /// Used for stream transitive access checks.
    pub fn has_stream_read(&self, stream_id: &str) -> bool {
        let resource = format!("stream:{}", stream_id);
        self.has_permission(&resource, actions::READ)
    }

    /// Get all stream IDs that this permission set grants read access to.
    /// Returns None if unrestricted (has `stream:*` or `*:*` with read).
    /// Returns Some(vec) of specific stream IDs if restricted.
    pub fn stream_restrictions(&self) -> Option<Vec<String>> {
        // Check for broad access
        if self.has_permission("stream:__any__check__", actions::READ) {
            // If a made-up stream ID passes, we have wildcard access
            return None;
        }

        // Collect specific stream IDs
        let mut stream_ids = Vec::new();
        for (pattern, action_list) in &self.0 {
            if action_matches(action_list, actions::READ)
                && let Some(resource_type) = parse_resource_type(pattern)
                && resource_type == "stream"
            {
                let path = &pattern[7..]; // skip "stream:"
                if !path.contains('*') {
                    stream_ids.push(path.to_string());
                }
            }
        }

        Some(stream_ids)
    }

    /// Validate that this permission set is a subset of a parent permission set.
    /// Returns Ok(()) if all permissions in `self` are covered by `parent`.
    /// Returns Err with the first escalation found.
    pub fn validate_subset_of(&self, parent: &Permissions) -> Result<(), PermissionError> {
        for (resource, action_list) in &self.0 {
            for action in action_list {
                let effective_action = if action == actions::WILDCARD {
                    // Wildcard means all actions - check each valid action for this resource type
                    let resource_type = parse_resource_type(resource).unwrap_or("*");
                    let valid =
                        resource_types::valid_actions(resource_type).unwrap_or(actions::ALL);
                    for va in valid {
                        if !parent.has_permission(resource, va) {
                            return Err(PermissionError::Escalation {
                                resource: resource.clone(),
                                action: va.to_string(),
                            });
                        }
                    }
                    continue;
                } else {
                    action.as_str()
                };

                if !parent.has_permission(resource, effective_action) {
                    return Err(PermissionError::Escalation {
                        resource: resource.clone(),
                        action: effective_action.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Internal Helpers
// ============================================================================

/// Parse the resource type from a URN. Returns the type portion before the first `:`.
///
/// Valid formats:
/// - `*:*` — universal wildcard (returns `"*"`)
/// - `type:path` — e.g. `mcp:weather/*` (returns `"mcp"`)
/// - `type:*` — e.g. `mcp:*` (returns `"mcp"`)
///
/// Invalid (returns None):
/// - Empty string, bare `*`, no colon, empty type before colon (`:path`),
///   empty path after colon (`type:`), or `*:anything` that isn't `*:*`.
fn parse_resource_type(urn: &str) -> Option<&str> {
    if urn == "*:*" {
        return Some("*");
    }
    let colon_pos = urn.find(':')?;
    let resource_type = &urn[..colon_pos];
    let path = &urn[colon_pos + 1..];

    // Type must be non-empty and not a bare wildcard (only `*:*` is valid for wildcard type)
    if resource_type.is_empty() || resource_type == "*" {
        return None;
    }

    // Path must be non-empty
    if path.is_empty() {
        return None;
    }

    // Type must be alphanumeric/hyphens (no special chars except hyphens)
    if !resource_type
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }

    Some(resource_type)
}

/// Check if a resource URN pattern matches a concrete resource.
///
/// Rules:
/// - `*:*` matches everything
/// - Exact match
/// - `mcp:*` matches any resource starting with `mcp:`
/// - `mcp:weather/*` matches `mcp:weather/get-forecast`
/// - `event:order:*` matches `event:order:placed`
fn resource_matches(pattern: &str, resource: &str) -> bool {
    // Universal wildcard
    if pattern == "*:*" {
        return true;
    }

    // Exact match
    if pattern == resource {
        return true;
    }

    // Suffix wildcard matching
    if let Some(prefix) = pattern.strip_suffix('*') {
        return resource.starts_with(prefix);
    }

    false
}

/// Check if an action list grants a specific action.
/// `["*"]` grants any action.
fn action_matches(action_list: &[String], required: &str) -> bool {
    action_list
        .iter()
        .any(|a| a == actions::WILDCARD || a == required)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to construct a Permissions from a slice of (resource, actions) tuples.
    fn perms(entries: &[(&str, &[&str])]) -> Permissions {
        let mut map = HashMap::new();
        for (resource, action_list) in entries {
            map.insert(
                resource.to_string(),
                action_list.iter().map(|a| a.to_string()).collect(),
            );
        }
        Permissions(map)
    }

    // ========================================================================
    // has_permission — exact matching
    // ========================================================================

    #[test]
    fn exact_resource_exact_action() {
        let p = perms(&[("mcp:weather/get-forecast", &["execute"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(!p.has_permission("mcp:weather/get-forecast", "read"));
        assert!(!p.has_permission("mcp:weather/other-tool", "execute"));
    }

    #[test]
    fn multiple_actions_on_one_resource() {
        let p = perms(&[("event:order:placed", &["create", "read"])]);
        assert!(p.has_permission("event:order:placed", "create"));
        assert!(p.has_permission("event:order:placed", "read"));
        assert!(!p.has_permission("event:order:placed", "delete"));
    }

    #[test]
    fn multiple_resources() {
        let p = perms(&[
            ("mcp:weather/get-forecast", &["execute"]),
            ("stream:abc-123", &["read"]),
            ("event:order:placed", &["create"]),
        ]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("stream:abc-123", "read"));
        assert!(p.has_permission("event:order:placed", "create"));
        assert!(!p.has_permission("mcp:time/now", "execute"));
        assert!(!p.has_permission("stream:other", "read"));
    }

    // ========================================================================
    // has_permission — wildcard resources
    // ========================================================================

    #[test]
    fn type_wildcard_matches_all_paths() {
        let p = perms(&[("mcp:*", &["execute"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("mcp:time/now", "execute"));
        assert!(!p.has_permission("mcp:weather/get-forecast", "read"));
        assert!(!p.has_permission("stream:abc", "execute"));
    }

    #[test]
    fn path_wildcard_within_type() {
        let p = perms(&[("mcp:weather/*", &["execute"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("mcp:weather/lookup", "execute"));
        assert!(!p.has_permission("mcp:time/now", "execute"));
    }

    #[test]
    fn event_wildcard_with_colons() {
        let p = perms(&[("event:order:*", &["create"])]);
        assert!(p.has_permission("event:order:placed", "create"));
        assert!(p.has_permission("event:order:shipped", "create"));
        assert!(!p.has_permission("event:user:created", "create"));
    }

    #[test]
    fn universal_wildcard_full_access() {
        let p = perms(&[("*:*", &["*"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("stream:abc-123", "read"));
        assert!(p.has_permission("event:anything", "create"));
        assert!(p.has_permission("context:any-var", "delete"));
        assert!(p.has_permission("build:123", "create"));
    }

    #[test]
    fn universal_wildcard_read_only() {
        let p = perms(&[("*:*", &["read"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "read"));
        assert!(p.has_permission("stream:abc-123", "read"));
        assert!(!p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(!p.has_permission("event:order:placed", "create"));
    }

    #[test]
    fn action_wildcard_on_specific_type() {
        let p = perms(&[("event:*", &["*"])]);
        assert!(p.has_permission("event:order:placed", "create"));
        assert!(p.has_permission("event:order:placed", "read"));
        assert!(!p.has_permission("stream:abc", "read"));
    }

    // ========================================================================
    // has_permission — no match
    // ========================================================================

    #[test]
    fn empty_permissions_deny_everything() {
        let p = Permissions::new();
        assert!(!p.has_permission("mcp:anything", "execute"));
        assert!(!p.has_permission("*:*", "*"));
    }

    #[test]
    fn unrelated_resource_denied() {
        let p = perms(&[("mcp:weather/*", &["execute"])]);
        assert!(!p.has_permission("stream:abc-123", "read"));
        assert!(!p.has_permission("webhook:my-hook", "execute"));
    }

    // ========================================================================
    // has_permission — overlapping rules
    // ========================================================================

    #[test]
    fn broader_rule_covers_narrower_request() {
        let p = perms(&[
            ("mcp:*", &["execute"]),
            ("mcp:weather/get-forecast", &["execute"]),
        ]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("mcp:time/now", "execute"));
    }

    #[test]
    fn multiple_rules_combine_additively() {
        let p = perms(&[("mcp:*", &["execute"]), ("stream:*", &["read"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("stream:abc", "read"));
        assert!(!p.has_permission("mcp:weather/get-forecast", "read"));
        assert!(!p.has_permission("stream:abc", "execute"));
    }

    // ========================================================================
    // stream_restrictions
    // ========================================================================

    #[test]
    fn stream_restrictions_specific_streams() {
        let p = perms(&[("stream:abc-123", &["read"]), ("stream:def-456", &["read"])]);
        let restrictions = p.stream_restrictions();
        assert!(restrictions.is_some());
        let mut ids = restrictions.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["abc-123", "def-456"]);
    }

    #[test]
    fn stream_restrictions_wildcard_returns_none() {
        let p = perms(&[("stream:*", &["read"])]);
        assert!(p.stream_restrictions().is_none());
    }

    #[test]
    fn stream_restrictions_universal_wildcard_returns_none() {
        let p = perms(&[("*:*", &["read"])]);
        assert!(p.stream_restrictions().is_none());
    }

    #[test]
    fn stream_restrictions_no_stream_access() {
        let p = perms(&[("mcp:*", &["execute"])]);
        let restrictions = p.stream_restrictions();
        assert!(restrictions.is_some());
        assert!(restrictions.unwrap().is_empty());
    }

    #[test]
    fn has_stream_read_specific() {
        let p = perms(&[("stream:abc-123", &["read"])]);
        assert!(p.has_stream_read("abc-123"));
        assert!(!p.has_stream_read("other-stream"));
    }

    // ========================================================================
    // validate — valid permissions
    // ========================================================================

    #[test]
    fn validate_valid_mixed_permissions() {
        let p = perms(&[
            ("mcp:weather/get-forecast", &["execute"]),
            ("stream:abc-123", &["read"]),
            ("event:order:*", &["create", "read"]),
            ("context:*", &["create", "read", "update", "delete"]),
        ]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_full_access_valid() {
        let p = perms(&[("*:*", &["*"])]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_universal_read_only_valid() {
        let p = perms(&[("*:*", &["read"])]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_universal_multiple_actions_valid() {
        let p = perms(&[("*:*", &["read", "create"])]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_type_wildcard_with_action_wildcard_valid() {
        let p = perms(&[("mcp:*", &["*"])]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_empty_permissions_valid() {
        let p = Permissions::new();
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_unknown_resource_type_passes() {
        // Unknown types are allowed (forward-compatible — new resource types can be added)
        let p = perms(&[("custom-thing:my-resource", &["read"])]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_all_known_resource_types() {
        for rt in resource_types::ALL {
            let resource = format!("{}:test-path", rt);
            if let Some(valid_actions) = resource_types::valid_actions(rt) {
                let action = valid_actions[0].to_string();
                let p = perms(&[(&resource, &[&action])]);
                assert!(p.validate().is_ok(), "should validate for type '{}'", rt);
            }
        }
    }

    // ========================================================================
    // validate — invalid actions
    // ========================================================================

    #[test]
    fn validate_rejects_unknown_action() {
        let p = perms(&[("mcp:weather/*", &["destroy"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidAction(ref a) if a == "destroy"));
    }

    #[test]
    fn validate_rejects_empty_action_string() {
        let p = perms(&[("mcp:weather/*", &[""])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidAction(ref a) if a.is_empty()));
    }

    #[test]
    fn validate_rejects_typo_actions() {
        for bad in &["Read", "CREATE", "Execute", "write", "remove", "list"] {
            let p = perms(&[("mcp:test", &[bad])]);
            assert!(p.validate().is_err(), "should reject action '{}'", bad);
        }
    }

    // ========================================================================
    // validate — invalid resource URNs
    // ========================================================================

    #[test]
    fn validate_rejects_no_colon() {
        let p = perms(&[("no-colon-here", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_empty_resource_key() {
        let p = perms(&[("", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_bare_star() {
        let p = perms(&[("*", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_star_colon_not_star() {
        // `*:foo` is not valid — only `*:*` is allowed for the wildcard type
        let p = perms(&[("*:foo", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_colon_only() {
        let p = perms(&[(":", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_empty_type_before_colon() {
        let p = perms(&[(":path", &["read"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_empty_path_after_colon() {
        let p = perms(&[("mcp:", &["execute"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::InvalidResource(_)));
    }

    #[test]
    fn validate_rejects_type_with_special_chars() {
        for bad in &["mcp!:test", "stream@:test", "event :test", "run\t:test"] {
            let p = perms(&[(bad, &["read"])]);
            assert!(
                p.validate().is_err(),
                "should reject resource type '{}'",
                bad
            );
        }
    }

    // ========================================================================
    // validate — empty action list
    // ========================================================================

    #[test]
    fn validate_rejects_empty_action_list() {
        let p = perms(&[("mcp:weather/*", &[])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PermissionError::EmptyActionList(_)));
    }

    // ========================================================================
    // validate — wrong action for resource type
    // ========================================================================

    #[test]
    fn validate_rejects_create_on_mcp() {
        let p = perms(&[("mcp:weather/*", &["create"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            PermissionError::ActionNotValidForResource { .. }
        ));
    }

    #[test]
    fn validate_rejects_execute_on_stream() {
        let p = perms(&[("stream:abc", &["execute"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            PermissionError::ActionNotValidForResource { .. }
        ));
    }

    #[test]
    fn validate_rejects_delete_on_event() {
        let p = perms(&[("event:order:placed", &["delete"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            PermissionError::ActionNotValidForResource { .. }
        ));
    }

    #[test]
    fn validate_rejects_create_on_run() {
        let p = perms(&[("run:abc-123", &["create"])]);
        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            PermissionError::ActionNotValidForResource { .. }
        ));
    }

    #[test]
    fn validate_allows_wildcard_action_on_known_type() {
        // `*` action should pass even on a known type (it means "all valid actions for this type")
        let p = perms(&[("mcp:weather/*", &["*"])]);
        assert!(p.validate().is_ok());
    }

    // ========================================================================
    // validate_subset_of
    // ========================================================================

    #[test]
    fn subset_of_same_permissions() {
        let p = perms(&[("mcp:*", &["execute"]), ("event:*", &["create", "read"])]);
        assert!(p.validate_subset_of(&p).is_ok());
    }

    #[test]
    fn subset_narrower_resource() {
        let parent = perms(&[("mcp:*", &["execute"])]);
        let child = perms(&[("mcp:weather/get-forecast", &["execute"])]);
        assert!(child.validate_subset_of(&parent).is_ok());
    }

    #[test]
    fn subset_different_type_escalation() {
        let parent = perms(&[("mcp:*", &["execute"])]);
        let child = perms(&[("stream:abc-123", &["read"])]);
        let err = child.validate_subset_of(&parent).unwrap_err();
        assert!(matches!(err, PermissionError::Escalation { .. }));
    }

    #[test]
    fn subset_action_escalation() {
        let parent = perms(&[("event:*", &["read"])]);
        let child = perms(&[("event:order:placed", &["create"])]);
        let err = child.validate_subset_of(&parent).unwrap_err();
        assert!(matches!(err, PermissionError::Escalation { .. }));
    }

    #[test]
    fn subset_broader_resource_escalation() {
        let parent = perms(&[("mcp:weather/*", &["execute"])]);
        let child = perms(&[("mcp:time/now", &["execute"])]);
        assert!(child.validate_subset_of(&parent).is_err());
    }

    #[test]
    fn subset_of_full_access_parent() {
        let parent = perms(&[("*:*", &["*"])]);

        // Any child should be a valid subset of full access
        let child = perms(&[
            ("mcp:weather/get-forecast", &["execute"]),
            ("stream:abc-123", &["read"]),
            ("event:order:*", &["create", "read"]),
            ("context:*", &["create", "read", "update", "delete"]),
        ]);
        assert!(child.validate_subset_of(&parent).is_ok());

        // Even another full-access set
        let child_full = perms(&[("*:*", &["*"])]);
        assert!(child_full.validate_subset_of(&parent).is_ok());
    }

    #[test]
    fn subset_child_action_wildcard_against_parent_explicit_actions() {
        // Parent has explicit actions; child requests wildcard on the same resource
        let parent = perms(&[("mcp:*", &["execute"])]);
        // Child asks for * action on mcp:weather/* — validate_subset checks each valid action
        // for mcp (only "execute"), and parent grants it, so this should be OK
        let child = perms(&[("mcp:weather/*", &["*"])]);
        assert!(child.validate_subset_of(&parent).is_ok());
    }

    #[test]
    fn subset_child_action_wildcard_partial_coverage() {
        // Parent grants only "read" on events, child wants "*" on events
        // Since event supports create+read, parent only has read → escalation
        let parent = perms(&[("event:*", &["read"])]);
        let child = perms(&[("event:order:*", &["*"])]);
        assert!(child.validate_subset_of(&parent).is_err());
    }

    #[test]
    fn subset_empty_child_always_ok() {
        let parent = perms(&[("mcp:*", &["execute"])]);
        let child = Permissions::new();
        assert!(child.validate_subset_of(&parent).is_ok());
    }

    #[test]
    fn subset_empty_parent_rejects_anything() {
        let parent = Permissions::new();
        let child = perms(&[("mcp:weather/get-forecast", &["execute"])]);
        assert!(child.validate_subset_of(&parent).is_err());
    }

    // ========================================================================
    // from_json / to_json / from_json_validated
    // ========================================================================

    #[test]
    fn from_json_valid() {
        let json = serde_json::json!({
            "mcp:weather/get-forecast": ["execute"],
            "stream:abc-123": ["read"]
        });
        let p = Permissions::from_json(&json).unwrap();
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("stream:abc-123", "read"));
    }

    #[test]
    fn from_json_full_access() {
        let json = serde_json::json!({"*:*": ["*"]});
        let p = Permissions::from_json(&json).unwrap();
        assert!(p.has_permission("anything:anywhere", "execute"));
    }

    #[test]
    fn from_json_empty_object() {
        let json = serde_json::json!({});
        let p = Permissions::from_json(&json).unwrap();
        assert!(p.is_empty());
        assert!(!p.has_permission("mcp:test", "execute"));
    }

    #[test]
    fn from_json_rejects_null() {
        let json = serde_json::Value::Null;
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_array() {
        let json = serde_json::json!(["mcp:*"]);
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_string() {
        let json = serde_json::json!("mcp:*");
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_number() {
        let json = serde_json::json!(42);
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_non_array_actions() {
        let json = serde_json::json!({"mcp:*": "execute"});
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_non_string_actions() {
        let json = serde_json::json!({"mcp:*": [1, 2, 3]});
        assert!(Permissions::from_json(&json).is_err());
    }

    #[test]
    fn from_json_validated_accepts_valid() {
        let json = serde_json::json!({"mcp:weather/*": ["execute"], "stream:abc": ["read"]});
        assert!(Permissions::from_json_validated(&json).is_ok());
    }

    #[test]
    fn from_json_validated_rejects_empty_action_list() {
        let json = serde_json::json!({"mcp:weather/*": []});
        assert!(Permissions::from_json_validated(&json).is_err());
    }

    #[test]
    fn from_json_validated_rejects_bad_urn() {
        let json = serde_json::json!({"no-colon": ["read"]});
        assert!(Permissions::from_json_validated(&json).is_err());
    }

    #[test]
    fn from_json_validated_rejects_bad_action() {
        let json = serde_json::json!({"mcp:test": ["fly"]});
        assert!(Permissions::from_json_validated(&json).is_err());
    }

    #[test]
    fn from_json_validated_rejects_wrong_action_for_type() {
        let json = serde_json::json!({"stream:abc": ["execute"]});
        assert!(Permissions::from_json_validated(&json).is_err());
    }

    #[test]
    fn to_json_roundtrip() {
        let original = perms(&[("mcp:weather/*", &["execute"]), ("stream:abc", &["read"])]);
        let json = original.to_json();
        let restored = Permissions::from_json(&json).unwrap();
        assert_eq!(original, restored);
    }

    // ========================================================================
    // is_empty / Default
    // ========================================================================

    #[test]
    fn new_is_empty() {
        let p = Permissions::new();
        assert!(p.is_empty());
    }

    #[test]
    fn default_is_empty() {
        let p = Permissions::default();
        assert!(p.is_empty());
    }

    #[test]
    fn non_empty_after_construction() {
        let p = perms(&[("mcp:*", &["execute"])]);
        assert!(!p.is_empty());
    }

    // ========================================================================
    // PermissionError display
    // ========================================================================

    #[test]
    fn error_display_invalid_action() {
        let err = PermissionError::InvalidAction("fly".to_string());
        let msg = err.to_string();
        assert!(msg.contains("fly"));
        assert!(msg.contains("create, read, update, delete, execute"));
    }

    #[test]
    fn error_display_invalid_resource() {
        let err = PermissionError::InvalidResource("bad".to_string());
        let msg = err.to_string();
        assert!(msg.contains("bad"));
        assert!(msg.contains("type:path"));
    }

    #[test]
    fn error_display_empty_action_list() {
        let err = PermissionError::EmptyActionList("mcp:test".to_string());
        let msg = err.to_string();
        assert!(msg.contains("mcp:test"));
        assert!(msg.contains("empty action list"));
    }

    #[test]
    fn error_display_wrong_action_for_resource() {
        let err = PermissionError::ActionNotValidForResource {
            action: "create".to_string(),
            resource: "mcp:test".to_string(),
            valid_actions: vec!["execute".to_string()],
        };
        let msg = err.to_string();
        assert!(msg.contains("create"));
        assert!(msg.contains("mcp:test"));
        assert!(msg.contains("execute"));
    }

    #[test]
    fn error_display_escalation() {
        let err = PermissionError::Escalation {
            resource: "mcp:test".to_string(),
            action: "execute".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("escalation"));
        assert!(msg.contains("mcp:test"));
    }

    // ========================================================================
    // actions module
    // ========================================================================

    #[test]
    fn actions_is_valid_known() {
        assert!(actions::is_valid("create"));
        assert!(actions::is_valid("read"));
        assert!(actions::is_valid("update"));
        assert!(actions::is_valid("delete"));
        assert!(actions::is_valid("execute"));
        assert!(actions::is_valid("*"));
    }

    #[test]
    fn actions_is_valid_unknown() {
        assert!(!actions::is_valid("fly"));
        assert!(!actions::is_valid("Read"));
        assert!(!actions::is_valid(""));
        assert!(!actions::is_valid("CREATE"));
    }

    #[test]
    fn actions_all_does_not_include_wildcard() {
        assert!(!actions::ALL.contains(&"*"));
        assert_eq!(actions::ALL.len(), 5);
    }

    // ========================================================================
    // resource_types module
    // ========================================================================

    #[test]
    fn resource_types_all_known() {
        assert!(resource_types::is_known("mcp"));
        assert!(resource_types::is_known("webhook"));
        assert!(resource_types::is_known("stream"));
        assert!(resource_types::is_known("event"));
        assert!(resource_types::is_known("run"));
        assert!(resource_types::is_known("project"));
        assert!(resource_types::is_known("build"));
        assert!(resource_types::is_known("context"));
        assert!(resource_types::is_known("key"));
        assert!(resource_types::is_known("session"));
        assert!(resource_types::is_known("env"));
    }

    #[test]
    fn resource_types_unknown() {
        assert!(!resource_types::is_known("custom"));
        assert!(!resource_types::is_known(""));
        assert!(!resource_types::is_known("*"));
    }

    #[test]
    fn resource_types_valid_actions_mcp_only_execute() {
        let actions = resource_types::valid_actions("mcp").unwrap();
        assert_eq!(actions, &["execute"]);
    }

    #[test]
    fn resource_types_valid_actions_stream_only_read() {
        let actions = resource_types::valid_actions("stream").unwrap();
        assert_eq!(actions, &["read"]);
    }

    #[test]
    fn resource_types_valid_actions_context_crud() {
        let actions = resource_types::valid_actions("context").unwrap();
        assert!(actions.contains(&"create"));
        assert!(actions.contains(&"read"));
        assert!(actions.contains(&"update"));
        assert!(actions.contains(&"delete"));
    }

    #[test]
    fn resource_types_valid_actions_unknown_returns_none() {
        assert!(resource_types::valid_actions("custom").is_none());
    }

    // ========================================================================
    // parse_resource_type (internal, tested via validate)
    // ========================================================================

    #[test]
    fn parse_resource_type_valid_cases() {
        assert_eq!(parse_resource_type("*:*"), Some("*"));
        assert_eq!(parse_resource_type("mcp:weather"), Some("mcp"));
        assert_eq!(parse_resource_type("mcp:weather/get-forecast"), Some("mcp"));
        assert_eq!(parse_resource_type("mcp:*"), Some("mcp"));
        assert_eq!(parse_resource_type("event:order:placed"), Some("event"));
        assert_eq!(parse_resource_type("my-custom:path"), Some("my-custom"));
    }

    #[test]
    fn parse_resource_type_invalid_cases() {
        assert_eq!(parse_resource_type(""), None);
        assert_eq!(parse_resource_type("*"), None);
        assert_eq!(parse_resource_type("no-colon"), None);
        assert_eq!(parse_resource_type(":path"), None);
        assert_eq!(parse_resource_type("mcp:"), None);
        assert_eq!(parse_resource_type(":"), None);
        assert_eq!(parse_resource_type("*:foo"), None); // only *:* is valid
        assert_eq!(parse_resource_type("*:"), None);
    }

    // ========================================================================
    // resource_matches (internal, tested via has_permission)
    // ========================================================================

    #[test]
    fn resource_matches_exact() {
        assert!(resource_matches(
            "mcp:weather/get-forecast",
            "mcp:weather/get-forecast"
        ));
        assert!(!resource_matches(
            "mcp:weather/get-forecast",
            "mcp:weather/lookup"
        ));
    }

    #[test]
    fn resource_matches_suffix_wildcard() {
        assert!(resource_matches("mcp:*", "mcp:weather/get-forecast"));
        assert!(resource_matches(
            "mcp:weather/*",
            "mcp:weather/get-forecast"
        ));
        assert!(!resource_matches("mcp:weather/*", "mcp:time/now"));
    }

    #[test]
    fn resource_matches_universal() {
        assert!(resource_matches("*:*", "mcp:weather/get-forecast"));
        assert!(resource_matches("*:*", "anything:at-all"));
    }

    #[test]
    fn resource_matches_no_false_prefix_match() {
        // "mcp:weather" should not match "mcp:weather/get-forecast" without wildcard
        assert!(!resource_matches("mcp:weather", "mcp:weather/get-forecast"));
    }

    // ========================================================================
    // action_matches (internal)
    // ========================================================================

    #[test]
    fn action_matches_exact() {
        let actions = vec!["execute".to_string()];
        assert!(action_matches(&actions, "execute"));
        assert!(!action_matches(&actions, "read"));
    }

    #[test]
    fn action_matches_wildcard() {
        let actions = vec!["*".to_string()];
        assert!(action_matches(&actions, "execute"));
        assert!(action_matches(&actions, "read"));
        assert!(action_matches(&actions, "create"));
    }

    #[test]
    fn action_matches_multiple() {
        let actions = vec!["create".to_string(), "read".to_string()];
        assert!(action_matches(&actions, "create"));
        assert!(action_matches(&actions, "read"));
        assert!(!action_matches(&actions, "delete"));
    }

    #[test]
    fn action_matches_empty_denies() {
        let actions: Vec<String> = vec![];
        assert!(!action_matches(&actions, "read"));
    }

    // ========================================================================
    // Real-world scenarios
    // ========================================================================

    #[test]
    fn scenario_mcp_only_client() {
        let p = perms(&[("mcp:weather/*", &["execute"]), ("stream:*", &["read"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.has_permission("stream:my-stream", "read"));
        assert!(!p.has_permission("mcp:time/now", "execute"));
        assert!(!p.has_permission("event:something", "create"));
        assert!(p.validate().is_ok());
    }

    #[test]
    fn scenario_event_producer() {
        let p = perms(&[("event:*", &["create"]), ("run:*", &["read"])]);
        assert!(p.has_permission("event:order:placed", "create"));
        assert!(p.has_permission("run:abc-123", "read"));
        assert!(!p.has_permission("event:order:placed", "read"));
        assert!(!p.has_permission("mcp:anything", "execute"));
        assert!(p.validate().is_ok());
    }

    #[test]
    fn scenario_admin_key() {
        let p = perms(&[("*:*", &["*"])]);
        // Admin can do anything
        assert!(p.has_permission("mcp:anything", "execute"));
        assert!(p.has_permission("webhook:my-hook", "execute"));
        assert!(p.has_permission("context:secret-var", "delete"));
        assert!(p.has_permission("key:abc", "create"));
        assert!(p.validate().is_ok());
    }

    #[test]
    fn scenario_read_only_observer() {
        let p = perms(&[("*:*", &["read"])]);
        assert!(p.has_permission("mcp:weather/get-forecast", "read"));
        assert!(p.has_permission("event:order:placed", "read"));
        assert!(!p.has_permission("event:order:placed", "create"));
        assert!(!p.has_permission("mcp:weather/get-forecast", "execute"));
        assert!(p.validate().is_ok());
    }

    #[test]
    fn scenario_session_from_restricted_api_key() {
        let api_key_perms = perms(&[("mcp:*", &["execute"]), ("stream:*", &["read"])]);

        // Session requests a subset — should pass
        let session_perms = perms(&[("mcp:weather/*", &["execute"]), ("stream:abc", &["read"])]);
        assert!(session_perms.validate_subset_of(&api_key_perms).is_ok());

        // Session requests something the API key doesn't have — should fail
        let session_perms = perms(&[("event:order:*", &["create"])]);
        assert!(session_perms.validate_subset_of(&api_key_perms).is_err());
    }

    // ========================================================================
    // validate_resource_types (service key hardening)
    // ========================================================================

    #[test]
    fn validate_resource_types_all_allowed_types_pass() {
        let p = perms(&[
            ("mcp:*", &["execute"]),
            ("webhook:*", &["execute"]),
            ("stream:*", &["read"]),
            ("event:*", &["create", "read"]),
            ("run:*", &["read"]),
        ]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_ok()
        );
    }

    #[test]
    fn validate_resource_types_rejects_universal_wildcard() {
        let p = perms(&[("*:*", &["*"])]);
        let err = p
            .validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
            .unwrap_err();
        assert!(matches!(
            err,
            PermissionError::DisallowedResourceType { .. }
        ));
    }

    #[test]
    fn validate_resource_types_rejects_context() {
        let p = perms(&[("context:*", &["read"])]);
        let err = p
            .validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
            .unwrap_err();
        assert!(matches!(
            err,
            PermissionError::DisallowedResourceType {
                ref resource_type, ..
            } if resource_type == "context"
        ));
    }

    #[test]
    fn validate_resource_types_rejects_build() {
        let p = perms(&[("build:*", &["create"])]);
        let err = p
            .validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
            .unwrap_err();
        assert!(matches!(
            err,
            PermissionError::DisallowedResourceType {
                ref resource_type, ..
            } if resource_type == "build"
        ));
    }

    #[test]
    fn validate_resource_types_rejects_key() {
        let p = perms(&[("key:*", &["read"])]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_err()
        );
    }

    #[test]
    fn validate_resource_types_rejects_session() {
        let p = perms(&[("session:*", &["read"])]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_err()
        );
    }

    #[test]
    fn validate_resource_types_rejects_env() {
        let p = perms(&[("env:*", &["read"])]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_err()
        );
    }

    #[test]
    fn validate_resource_types_rejects_project() {
        let p = perms(&[("project:*", &["read"])]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_err()
        );
    }

    #[test]
    fn validate_resource_types_mixed_allowed_and_disallowed() {
        let p = perms(&[("mcp:*", &["execute"]), ("context:*", &["read"])]);
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_err()
        );
    }

    #[test]
    fn validate_resource_types_empty_permissions_pass() {
        let p = Permissions::new();
        assert!(
            p.validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
                .is_ok()
        );
    }

    #[test]
    fn validate_resource_types_error_shows_allowed_list() {
        let p = perms(&[("build:*", &["create"])]);
        let err = p
            .validate_resource_types(resource_types::SERVICE_KEY_ALLOWED)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("build"));
        assert!(msg.contains("mcp"));
        assert!(msg.contains("webhook"));
        assert!(msg.contains("stream"));
        assert!(msg.contains("event"));
        assert!(msg.contains("run"));
    }
}
