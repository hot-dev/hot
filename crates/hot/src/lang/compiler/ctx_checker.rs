// Context Requirements Checker
//
// This module checks that required context variables are present in the
// execution context. Requirements are declared via function-level metadata
// and resolved transitively through the call graph.
//
// Metadata format (on individual functions):
//   my-fn meta {ctx: {
//     "api.key": {},                                    // required, secret (defaults)
//     "rate.limit": {"required": false, "default": 1000, "secret": false}
//   }}
//   fn () { ... }
//
// Per-key properties:
//   - required: bool (default: true) - must be provided at runtime
//   - default: any (no default) - value to use if not provided (implies required: false)
//   - secret: bool (default: true) - if true, value will be masked in call db
//
// The call graph (see call_graph.rs) resolves transitive ctx requirements so
// that only requirements reachable from user code are enforced.

use crate::lang::ast::Program;
use crate::val::Val;
use ahash::{AHashMap, AHashSet};

/// Configuration for a single context key
#[derive(Debug, Clone)]
pub struct CtxKeyConfig {
    /// The context key name
    pub key: String,
    /// Whether this key is required (default: true, but false if default is provided)
    pub required: bool,
    /// Default value if not provided
    pub default: Option<Val>,
    /// Whether this value is a secret (default: true) - if true, value will be masked in call db
    pub secret: bool,
}

impl Default for CtxKeyConfig {
    fn default() -> Self {
        Self {
            key: String::new(),
            required: true,
            default: None,
            secret: true,
        }
    }
}

/// Context requirements for a namespace
#[derive(Debug, Clone, Default)]
pub struct NamespaceCtxRequirements {
    /// Namespace path (e.g., "::anthropic::api")
    pub namespace: String,
    /// All context key configurations
    pub keys: Vec<CtxKeyConfig>,
    /// Source file for error reporting
    pub source_file: Option<String>,
}

impl NamespaceCtxRequirements {
    /// Get required keys (no default provided and required: true)
    pub fn required_keys(&self) -> Vec<&CtxKeyConfig> {
        self.keys
            .iter()
            .filter(|k| k.required && k.default.is_none())
            .collect()
    }

    /// Get optional keys (required: false or has default)
    pub fn optional_keys(&self) -> Vec<&CtxKeyConfig> {
        self.keys
            .iter()
            .filter(|k| !k.required || k.default.is_some())
            .collect()
    }

    /// Get secret keys (secret: true, the default)
    pub fn secret_keys(&self) -> Vec<&CtxKeyConfig> {
        self.keys.iter().filter(|k| k.secret).collect()
    }

    /// Get non-secret keys (secret: false)
    pub fn non_secret_keys(&self) -> Vec<&CtxKeyConfig> {
        self.keys.iter().filter(|k| !k.secret).collect()
    }
}

/// Collected context requirements from a program
#[derive(Debug, Clone, Default)]
pub struct ProgramCtxRequirements {
    /// All namespace requirements
    pub namespaces: Vec<NamespaceCtxRequirements>,
}

impl ProgramCtxRequirements {
    /// Get all required context keys (deduplicated)
    pub fn all_required_keys(&self) -> AHashSet<String> {
        self.namespaces
            .iter()
            .flat_map(|ns| ns.required_keys().into_iter().map(|k| k.key.clone()))
            .collect()
    }

    /// Get all optional context keys (deduplicated)
    pub fn all_optional_keys(&self) -> AHashSet<String> {
        self.namespaces
            .iter()
            .flat_map(|ns| ns.optional_keys().into_iter().map(|k| k.key.clone()))
            .collect()
    }

    /// Get all secret keys (secret: true, deduplicated)
    pub fn all_secret_keys(&self) -> AHashSet<String> {
        let keys: AHashSet<String> = self
            .namespaces
            .iter()
            .flat_map(|ns| ns.secret_keys().into_iter().map(|k| k.key.clone()))
            .collect();
        tracing::debug!(
            "all_secret_keys: found {} secret keys from {} namespaces: {:?}",
            keys.len(),
            self.namespaces.len(),
            keys
        );
        keys
    }

    /// Get all non-secret keys (secret: false, deduplicated)
    pub fn all_non_secret_keys(&self) -> AHashSet<String> {
        self.namespaces
            .iter()
            .flat_map(|ns| ns.non_secret_keys().into_iter().map(|k| k.key.clone()))
            .collect()
    }

    /// Get all default values as a map
    pub fn all_defaults(&self) -> AHashMap<String, Val> {
        self.namespaces
            .iter()
            .flat_map(|ns| {
                ns.keys
                    .iter()
                    .filter_map(|k| k.default.as_ref().map(|v| (k.key.clone(), v.clone())))
            })
            .collect()
    }

    /// Check if there are any requirements
    pub fn has_requirements(&self) -> bool {
        self.namespaces
            .iter()
            .any(|ns| !ns.required_keys().is_empty())
    }
}

/// Error for a missing context requirement
#[derive(Debug, Clone)]
pub struct MissingCtxError {
    /// The missing context key
    pub key: String,
    /// Namespace that requires this key
    pub namespace: String,
    /// Source file where the requirement is declared
    pub source_file: Option<String>,
}

impl std::fmt::Display for MissingCtxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(file) = &self.source_file {
            write!(
                f,
                "Missing required context variable '{}' (required by {} in {})",
                self.key, self.namespace, file
            )
        } else {
            write!(
                f,
                "Missing required context variable '{}' (required by {})",
                self.key, self.namespace
            )
        }
    }
}

/// Result of checking context requirements
#[derive(Debug, Clone)]
pub struct CtxCheckResult {
    /// Missing required context variables
    pub missing: Vec<MissingCtxError>,
    /// All required keys that were checked
    pub required_keys: AHashSet<String>,
    /// Keys that were found in context
    pub found_keys: AHashSet<String>,
}

impl CtxCheckResult {
    /// Returns true if all requirements are satisfied
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty()
    }

    /// Format errors for display
    pub fn format_errors(&self) -> String {
        if self.missing.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        output.push_str("Context requirements not satisfied:\n");
        for error in &self.missing {
            output.push_str(&format!("  - {}\n", error));
        }
        output.push_str("\nPlease set these as context variables.");
        output
    }
}

/// Extract context requirements using call-graph resolution.
///
/// Builds a call graph from the program, identifies user root functions, and
/// transitively resolves only the ctx requirements that are actually reachable
/// from user code. This avoids requiring ctx variables from package functions
/// that are never called.
///
/// Context requirements must be declared on individual functions via
/// `meta { ctx: {"key": {}} }`. Namespace-level ctx meta is not supported.
pub fn extract_ctx_requirements_via_call_graph(program: &Program) -> ProgramCtxRequirements {
    let call_graph = crate::lang::compiler::call_graph::CallGraph::build(program);

    tracing::debug!(
        "Call graph built: {} functions tracked, {} with ctx requirements",
        call_graph.function_count(),
        call_graph.ctx_function_count()
    );

    call_graph.resolve_user_ctx_requirements(program)
}

/// Parse a single key's configuration from its value
pub fn parse_key_config(key: &str, config: &Val) -> CtxKeyConfig {
    let mut key_config = CtxKeyConfig {
        key: key.to_string(),
        required: true,
        default: None,
        secret: true,
    };

    if let Val::Map(config_map) = config {
        // Extract 'required' property (default: true, but false if 'default' is provided)
        if let Some(Val::Bool(required)) = config_map.get(&Val::from("required")) {
            key_config.required = *required;
        }

        // Extract 'default' property
        if let Some(default_val) = config_map.get(&Val::from("default")) {
            key_config.default = Some(default_val.clone());
            // Having a default implies required: false (unless explicitly set to true)
            if !config_map.contains_key(&Val::from("required")) {
                key_config.required = false;
            }
        }

        // Extract 'secret' property (default: true, meaning value will be masked)
        if let Some(Val::Bool(secret)) = config_map.get(&Val::from("secret")) {
            key_config.secret = *secret;
        }
    }
    // If config is not a Map (e.g., empty or null), use defaults

    key_config
}

/// Check that all required context variables are present
pub fn check_ctx_requirements(
    requirements: &ProgramCtxRequirements,
    available_keys: &AHashSet<String>,
) -> CtxCheckResult {
    let mut missing = Vec::new();
    let mut found_keys = AHashSet::new();
    let required_keys = requirements.all_required_keys();

    for ns_req in &requirements.namespaces {
        for key_config in ns_req.required_keys() {
            if available_keys.contains(&key_config.key) {
                found_keys.insert(key_config.key.clone());
            } else {
                missing.push(MissingCtxError {
                    key: key_config.key.clone(),
                    namespace: ns_req.namespace.clone(),
                    source_file: ns_req.source_file.clone(),
                });
            }
        }
    }

    CtxCheckResult {
        missing,
        required_keys,
        found_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_config_empty_config() {
        // {} - required, secret by default
        let config = parse_key_config("api.key", &Val::map_empty());

        assert_eq!(config.key, "api.key");
        assert!(config.required);
        assert!(config.secret); // secret by default
        assert!(config.default.is_none());
    }

    #[test]
    fn test_parse_key_config_with_default() {
        // {"default": 1000, "secret": false}
        let config_map: indexmap::IndexMap<Val, Val> = [
            (Val::from("default"), Val::Int(1000)),
            (Val::from("secret"), Val::Bool(false)),
        ]
        .into_iter()
        .collect();

        let config = parse_key_config("rate.limit", &Val::Map(Box::new(config_map)));

        assert_eq!(config.key, "rate.limit");
        assert!(!config.required); // default implies not required
        assert!(!config.secret); // explicitly not secret
        assert_eq!(config.default, Some(Val::Int(1000)));
    }

    #[test]
    fn test_mixed_key_configs() {
        // Test NamespaceCtxRequirements helpers with a mix of key types
        let req = NamespaceCtxRequirements {
            namespace: "::test/my-fn".to_string(),
            keys: vec![
                parse_key_config("api.key", &Val::map_empty()),
                parse_key_config(
                    "rate.limit",
                    &Val::Map(Box::new(
                        [
                            (Val::from("default"), Val::Int(1000)),
                            (Val::from("secret"), Val::Bool(false)),
                        ]
                        .into_iter()
                        .collect(),
                    )),
                ),
            ],
            source_file: None,
        };

        assert_eq!(req.keys.len(), 2);
        assert_eq!(req.required_keys().len(), 1);
        assert_eq!(req.optional_keys().len(), 1);
        assert_eq!(req.secret_keys().len(), 1);
        assert_eq!(req.non_secret_keys().len(), 1);
    }

    #[test]
    fn test_check_requirements_satisfied() {
        let mut requirements = ProgramCtxRequirements::default();
        requirements.namespaces.push(NamespaceCtxRequirements {
            namespace: "::test".to_string(),
            keys: vec![CtxKeyConfig {
                key: "api.key".to_string(),
                required: true,
                default: None,
                secret: true,
            }],
            source_file: None,
        });

        let available: AHashSet<String> = ["api.key".to_string()].into_iter().collect();
        let result = check_ctx_requirements(&requirements, &available);

        assert!(result.is_ok());
        assert!(result.missing.is_empty());
    }

    #[test]
    fn test_check_requirements_missing() {
        let mut requirements = ProgramCtxRequirements::default();
        requirements.namespaces.push(NamespaceCtxRequirements {
            namespace: "::test".to_string(),
            keys: vec![
                CtxKeyConfig {
                    key: "api.key".to_string(),
                    required: true,
                    default: None,
                    secret: true,
                },
                CtxKeyConfig {
                    key: "other.key".to_string(),
                    required: true,
                    default: None,
                    secret: true,
                },
            ],
            source_file: Some("test.hot".to_string()),
        });

        let available: AHashSet<String> = ["api.key".to_string()].into_iter().collect();
        let result = check_ctx_requirements(&requirements, &available);

        assert!(!result.is_ok());
        assert_eq!(result.missing.len(), 1);
        assert_eq!(result.missing[0].key, "other.key");
    }

    #[test]
    fn test_all_defaults() {
        let mut requirements = ProgramCtxRequirements::default();
        requirements.namespaces.push(NamespaceCtxRequirements {
            namespace: "::test".to_string(),
            keys: vec![
                CtxKeyConfig {
                    key: "api.key".to_string(),
                    required: true,
                    default: None,
                    secret: true,
                },
                CtxKeyConfig {
                    key: "rate.limit".to_string(),
                    required: false,
                    default: Some(Val::Int(1000)),
                    secret: false,
                },
            ],
            source_file: None,
        });

        let defaults = requirements.all_defaults();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults.get("rate.limit"), Some(&Val::Int(1000)));
    }

    #[test]
    fn test_all_secret_keys() {
        let mut requirements = ProgramCtxRequirements::default();
        requirements.namespaces.push(NamespaceCtxRequirements {
            namespace: "::test".to_string(),
            keys: vec![
                CtxKeyConfig {
                    key: "api.key".to_string(),
                    required: true,
                    default: None,
                    secret: true, // secret
                },
                CtxKeyConfig {
                    key: "rate.limit".to_string(),
                    required: false,
                    default: Some(Val::Int(1000)),
                    secret: false, // not secret
                },
            ],
            source_file: None,
        });

        let secret_keys = requirements.all_secret_keys();
        assert_eq!(secret_keys.len(), 1);
        assert!(secret_keys.contains("api.key"));
        assert!(!secret_keys.contains("rate.limit"));
    }
}
