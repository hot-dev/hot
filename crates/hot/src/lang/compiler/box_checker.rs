// Box Requirements Checker
//
// This module checks that container resource requirements declared via
// `meta { box: { min-size: "medium", network: true } }` are satisfiable
// by the target environment's plan limits.
//
// Requirements are resolved transitively through the call graph — if user
// code calls a package function that needs "medium" containers, the user's
// deployment must support at least "medium".
//
// Metadata format (on individual functions):
//   my-fn meta {box: {min-size: "medium"}}
//   fn () { ... }
//
//   my-fn meta {box: {min-size: "small", network: true}}
//   fn () { ... }
//
// The call graph (see call_graph.rs) resolves transitive box requirements so
// that only requirements reachable from user code are enforced.

/// Box resource requirement for a single function.
#[derive(Debug, Clone, Default)]
pub struct BoxRequirement {
    /// Minimum container size preset (e.g. "nano", "small", "medium").
    /// None means the function doesn't declare a specific minimum.
    pub min_size: Option<String>,
    /// Whether the function requires internet access.
    /// None means the function doesn't declare a network requirement.
    pub network: Option<bool>,
}

impl BoxRequirement {
    pub fn is_empty(&self) -> bool {
        self.min_size.is_none() && self.network.is_none()
    }
}

/// A single function's box requirement with source tracking.
#[derive(Debug, Clone)]
pub struct FnBoxRequirement {
    /// Fully-qualified function name (e.g. "::ffmpeg/probe")
    pub fqn: String,
    /// Source file where the requirement is declared
    pub source_file: Option<String>,
    /// The box requirement
    pub requirement: BoxRequirement,
}

/// Collected box requirements from a program, resolved via call graph.
#[derive(Debug, Clone, Default)]
pub struct ProgramBoxRequirements {
    /// Per-function box requirements (only those reachable from user code)
    pub requirements: Vec<FnBoxRequirement>,
}

/// Ordered container size presets, smallest to largest.
const SIZE_ORDER: &[&str] = &[
    "nano", "micro", "small", "medium", "large", "xlarge", "2xlarge", "4xlarge",
];

/// Get the ordinal index of a size name (0 = nano, 7 = 4xlarge).
/// Returns None for unrecognized sizes.
fn size_ordinal(size: &str) -> Option<usize> {
    SIZE_ORDER.iter().position(|&s| s == size)
}

/// Compare two size names. Returns true if `a` >= `b` in the size hierarchy.
pub fn size_gte(a: &str, b: &str) -> bool {
    match (size_ordinal(a), size_ordinal(b)) {
        (Some(a_ord), Some(b_ord)) => a_ord >= b_ord,
        _ => false,
    }
}

/// Get the memory_mb for a given size preset (used for plan limit comparison).
pub fn size_memory_mb(size: &str) -> Option<u64> {
    match size {
        "nano" => Some(64),
        "micro" => Some(128),
        "small" => Some(256),
        "medium" => Some(512),
        "large" => Some(1024),
        "xlarge" => Some(2048),
        "2xlarge" => Some(4096),
        "4xlarge" => Some(8192),
        _ => None,
    }
}

impl ProgramBoxRequirements {
    /// Get the effective minimum size across all reachable functions.
    /// Returns the largest min-size declared by any reachable function.
    pub fn effective_min_size(&self) -> Option<String> {
        let mut max_ordinal: Option<(usize, String)> = None;
        for req in &self.requirements {
            if let Some(ref size) = req.requirement.min_size
                && let Some(ord) = size_ordinal(size)
            {
                match &max_ordinal {
                    Some((current_max, _)) if ord <= *current_max => {}
                    _ => max_ordinal = Some((ord, size.clone())),
                }
            }
        }
        max_ordinal.map(|(_, size)| size)
    }

    /// Whether any reachable function requires network access.
    pub fn requires_network(&self) -> bool {
        self.requirements
            .iter()
            .any(|r| r.requirement.network == Some(true))
    }

    /// Check if there are any box requirements at all.
    pub fn has_requirements(&self) -> bool {
        !self.requirements.is_empty()
    }

    /// Get all functions that declare a min-size requirement.
    pub fn functions_requiring_min_size(&self) -> Vec<&FnBoxRequirement> {
        self.requirements
            .iter()
            .filter(|r| r.requirement.min_size.is_some())
            .collect()
    }

    /// Get all functions that declare a network requirement.
    pub fn functions_requiring_network(&self) -> Vec<&FnBoxRequirement> {
        self.requirements
            .iter()
            .filter(|r| r.requirement.network == Some(true))
            .collect()
    }
}

/// Error when box requirements can't be satisfied.
#[derive(Debug, Clone)]
pub struct BoxRequirementError {
    pub kind: BoxRequirementErrorKind,
    /// Function that declared the unsatisfiable requirement
    pub fqn: String,
    /// Source file where the requirement is declared
    pub source_file: Option<String>,
}

#[derive(Debug, Clone)]
pub enum BoxRequirementErrorKind {
    /// Plan's max container size is smaller than the required min-size
    SizeTooSmall { required: String, plan_max: String },
    /// Plan doesn't allow network access but function requires it
    NetworkNotAllowed,
}

impl std::fmt::Display for BoxRequirementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let location = if let Some(ref file) = self.source_file {
            format!(" (declared by {} in {})", self.fqn, file)
        } else {
            format!(" (declared by {})", self.fqn)
        };

        match &self.kind {
            BoxRequirementErrorKind::SizeTooSmall { required, plan_max } => {
                write!(
                    f,
                    "Container size '{}' required but plan only supports up to '{}'{}",
                    required, plan_max, location
                )
            }
            BoxRequirementErrorKind::NetworkNotAllowed => {
                write!(
                    f,
                    "Container network access required but not allowed by plan{}",
                    location
                )
            }
        }
    }
}

/// Result of checking box requirements against plan limits.
#[derive(Debug, Clone)]
pub struct BoxCheckResult {
    pub errors: Vec<BoxRequirementError>,
}

impl BoxCheckResult {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn format_errors(&self) -> String {
        if self.errors.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        output.push_str("Box resource requirements not satisfied:\n");
        for error in &self.errors {
            output.push_str(&format!("  - {}\n", error));
        }
        output.push_str(
            "\nUpgrade your plan or remove the dependency that requires these resources.",
        );
        output
    }
}

/// Check box requirements against plan limits.
///
/// `plan_max_memory_mb` is the plan's BOX_MEMORY_MB feature (-1 = unlimited).
/// `plan_network_allowed` is the plan's BOX_NETWORK feature.
pub fn check_box_requirements(
    requirements: &ProgramBoxRequirements,
    plan_max_memory_mb: i64,
    plan_network_allowed: bool,
) -> BoxCheckResult {
    let mut errors = Vec::new();

    // Determine the plan's max size by finding the largest preset that fits
    let plan_max_size = if plan_max_memory_mb < 0 {
        Some("4xlarge".to_string()) // unlimited
    } else {
        let mut best: Option<String> = None;
        for &size in SIZE_ORDER {
            if let Some(mem) = size_memory_mb(size)
                && mem <= plan_max_memory_mb as u64
            {
                best = Some(size.to_string());
            }
        }
        best
    };

    for req in &requirements.requirements {
        // Check min-size
        if let Some(ref required_size) = req.requirement.min_size {
            let fits = match &plan_max_size {
                Some(max) => size_gte(max, required_size),
                None => false, // plan doesn't support any size
            };
            if !fits {
                errors.push(BoxRequirementError {
                    kind: BoxRequirementErrorKind::SizeTooSmall {
                        required: required_size.clone(),
                        plan_max: plan_max_size.clone().unwrap_or_else(|| "none".to_string()),
                    },
                    fqn: req.fqn.clone(),
                    source_file: req.source_file.clone(),
                });
            }
        }

        // Check network
        if req.requirement.network == Some(true) && !plan_network_allowed {
            errors.push(BoxRequirementError {
                kind: BoxRequirementErrorKind::NetworkNotAllowed,
                fqn: req.fqn.clone(),
                source_file: req.source_file.clone(),
            });
        }
    }

    BoxCheckResult { errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_ordering() {
        assert!(size_gte("medium", "nano"));
        assert!(size_gte("medium", "small"));
        assert!(size_gte("medium", "medium"));
        assert!(!size_gte("small", "medium"));
        assert!(size_gte("4xlarge", "nano"));
        assert!(!size_gte("nano", "4xlarge"));
    }

    #[test]
    fn test_size_memory_mb() {
        assert_eq!(size_memory_mb("nano"), Some(64));
        assert_eq!(size_memory_mb("medium"), Some(512));
        assert_eq!(size_memory_mb("4xlarge"), Some(8192));
        assert_eq!(size_memory_mb("bogus"), None);
    }

    #[test]
    fn test_effective_min_size() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![
                FnBoxRequirement {
                    fqn: "::ffmpeg/probe".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("nano".to_string()),
                        network: None,
                    },
                },
                FnBoxRequirement {
                    fqn: "::ffmpeg/transcode".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("medium".to_string()),
                        network: None,
                    },
                },
                FnBoxRequirement {
                    fqn: "::ffmpeg/thumbnail".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("small".to_string()),
                        network: None,
                    },
                },
            ],
        };

        assert_eq!(reqs.effective_min_size(), Some("medium".to_string()));
    }

    #[test]
    fn test_requires_network() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![
                FnBoxRequirement {
                    fqn: "::ffmpeg/probe".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("nano".to_string()),
                        network: None,
                    },
                },
                FnBoxRequirement {
                    fqn: "::playwright/screenshot".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("small".to_string()),
                        network: Some(true),
                    },
                },
            ],
        };

        assert!(reqs.requires_network());
    }

    #[test]
    fn test_no_requirements() {
        let reqs = ProgramBoxRequirements::default();
        assert!(!reqs.has_requirements());
        assert_eq!(reqs.effective_min_size(), None);
        assert!(!reqs.requires_network());
    }

    #[test]
    fn test_check_size_satisfied() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![FnBoxRequirement {
                fqn: "::ffmpeg/transcode".to_string(),
                source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".to_string()),
                requirement: BoxRequirement {
                    min_size: Some("medium".to_string()),
                    network: None,
                },
            }],
        };

        // Plan allows up to 512MB (medium)
        let result = check_box_requirements(&reqs, 512, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_size_too_small() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![FnBoxRequirement {
                fqn: "::ffmpeg/transcode".to_string(),
                source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".to_string()),
                requirement: BoxRequirement {
                    min_size: Some("medium".to_string()),
                    network: None,
                },
            }],
        };

        // Plan only allows 128MB (micro)
        let result = check_box_requirements(&reqs, 128, false);
        assert!(!result.is_ok());
        assert_eq!(result.errors.len(), 1);
        assert!(
            matches!(&result.errors[0].kind, BoxRequirementErrorKind::SizeTooSmall { required, .. } if required == "medium")
        );
    }

    #[test]
    fn test_check_network_not_allowed() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![FnBoxRequirement {
                fqn: "::playwright/screenshot".to_string(),
                source_file: None,
                requirement: BoxRequirement {
                    min_size: None,
                    network: Some(true),
                },
            }],
        };

        let result = check_box_requirements(&reqs, -1, false);
        assert!(!result.is_ok());
        assert_eq!(result.errors.len(), 1);
        assert!(matches!(
            &result.errors[0].kind,
            BoxRequirementErrorKind::NetworkNotAllowed
        ));
    }

    #[test]
    fn test_check_network_allowed() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![FnBoxRequirement {
                fqn: "::playwright/screenshot".to_string(),
                source_file: None,
                requirement: BoxRequirement {
                    min_size: None,
                    network: Some(true),
                },
            }],
        };

        let result = check_box_requirements(&reqs, -1, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_unlimited_plan() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![FnBoxRequirement {
                fqn: "::whisper/transcribe".to_string(),
                source_file: None,
                requirement: BoxRequirement {
                    min_size: Some("4xlarge".to_string()),
                    network: Some(true),
                },
            }],
        };

        // -1 = unlimited memory, network allowed
        let result = check_box_requirements(&reqs, -1, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_multiple_errors() {
        let reqs = ProgramBoxRequirements {
            requirements: vec![
                FnBoxRequirement {
                    fqn: "::whisper/transcribe".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: Some("large".to_string()),
                        network: None,
                    },
                },
                FnBoxRequirement {
                    fqn: "::playwright/screenshot".to_string(),
                    source_file: None,
                    requirement: BoxRequirement {
                        min_size: None,
                        network: Some(true),
                    },
                },
            ],
        };

        // Plan: only 128MB (micro), no network
        let result = check_box_requirements(&reqs, 128, false);
        assert_eq!(result.errors.len(), 2);
    }

    #[test]
    fn test_error_formatting() {
        let error = BoxRequirementError {
            kind: BoxRequirementErrorKind::SizeTooSmall {
                required: "medium".to_string(),
                plan_max: "micro".to_string(),
            },
            fqn: "::ffmpeg/transcode".to_string(),
            source_file: Some("pkg/ffmpeg/src/ffmpeg.hot".to_string()),
        };

        let msg = format!("{}", error);
        assert!(msg.contains("medium"));
        assert!(msg.contains("micro"));
        assert!(msg.contains("::ffmpeg/transcode"));
        assert!(msg.contains("pkg/ffmpeg/src/ffmpeg.hot"));
    }
}
