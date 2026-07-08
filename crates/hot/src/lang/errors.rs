// Compiler errors for Hot.
//
// This module provides comprehensive error handling for the compiler,
// including pretty error messages using ariadne and error collection.

use ahash::AHashMap;
use ariadne::{ColorGenerator, Config, Label, Report, ReportKind, Source};
use std::fmt;
use std::path::PathBuf;

/// Location information for errors
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorLocation {
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
    pub file: Option<PathBuf>,
}

/// LSP-style diagnostic severity. Today every compiler error is `Error`, but
/// the field exists so consumers can switch behavior on severity if/when we
/// add warnings or hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    /// LSP numeric value (1=Error, 2=Warning, 3=Information, 4=Hint).
    pub fn as_lsp_number(self) -> u8 {
        match self {
            DiagnosticSeverity::Error => 1,
            DiagnosticSeverity::Warning => 2,
            DiagnosticSeverity::Information => 3,
            DiagnosticSeverity::Hint => 4,
        }
    }
}

/// LSP-shaped diagnostic position: 0-based line and 0-based UTF-16 character
/// offset (we approximate with byte/char offset; sufficient for ASCII source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticPosition {
    pub line: u32,
    pub character: u32,
}

/// LSP-shaped range with start and end positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticRange {
    pub start: DiagnosticPosition,
    pub end: DiagnosticPosition,
}

/// LSP-style diagnostic emitted by `hot check --format json`.
///
/// Mirrors the LSP `Diagnostic` shape so editors and CI tooling can consume
/// the CLI output directly. The `file` field is added on top of the LSP
/// schema because the CLI emits diagnostics for the whole project at once.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Absolute path to the source file the diagnostic refers to. `None`
    /// means the diagnostic could not be tied to a file (rare).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub range: DiagnosticRange,
    /// Numeric LSP severity (1=Error, 2=Warning, 3=Information, 4=Hint).
    pub severity: u8,
    /// Hot error identifier such as `"unresolved-variable"` (kebab-case,
    /// no prefix). Stable across compiler versions and safe for tooling
    /// to key on (suppression lists, docs links, CI counters, etc.).
    pub code: String,
    /// Tool name producing the diagnostic, always `"hot"`.
    pub source: String,
    /// Human-readable message (already includes ariadne rendering when the
    /// originating `CompilerErrors` had source content available).
    pub message: String,
}

/// Compiler error types
#[derive(Debug, Clone)]
pub enum CompilerError {
    /// Variable reference cannot be resolved
    UnresolvedVariable {
        var_name: String,
        namespace: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Function reference cannot be resolved
    UnresolvedFunction {
        func_name: String,
        namespace: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Type reference cannot be resolved
    UnresolvedType {
        type_name: String,
        namespace: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Type mismatch error
    TypeMismatch {
        expected: String,
        actual: String,
        var_name: Option<String>,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Function arity mismatch
    ArityMismatch {
        func_name: String,
        expected: usize,
        actual: usize,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid function call
    InvalidFunctionCall {
        func_name: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// call-lib validation error
    CallLibError {
        func_name: String,
        message: String,
        location: Option<ErrorLocation>,
    },

    // Enhanced type checking errors
    /// Invalid type annotation
    InvalidTypeAnnotation {
        annotation: String,
        reason: String,
        location: Option<ErrorLocation>,
    },
    /// Circular type reference
    CircularTypeReference {
        type_name: String,
        cycle_path: Vec<String>,
        location: Option<ErrorLocation>,
    },
    /// Invalid generic arity
    InvalidGenericArity {
        type_name: String,
        expected: usize,
        actual: usize,
        location: Option<ErrorLocation>,
    },
    /// Invalid union type
    InvalidUnionType {
        types: Vec<String>,
        reason: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid implementation
    InvalidImplementation {
        source_type: String,
        target_type: String,
        reason: String,
        location: Option<ErrorLocation>,
    },
    /// Missing type implementation
    MissingTypeImplementation {
        source_type: String,
        target_type: String,
        location: Option<ErrorLocation>,
    },
    /// Ambiguous type implementation
    AmbiguousTypeImplementation {
        source_type: String,
        target_type: String,
        candidates: Vec<String>,
        location: Option<ErrorLocation>,
    },
    /// Invalid lazy usage
    InvalidLazyUsage {
        context: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid variadic usage
    InvalidVariadicUsage {
        context: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid flow type
    InvalidFlowType {
        flow_type: String,
        expected_types: Vec<String>,
        actual_type: String,
        location: Option<ErrorLocation>,
    },
    /// Incompatible flow branches
    IncompatibleFlowBranches {
        branch_types: Vec<String>,
        location: Option<ErrorLocation>,
    },
    /// Invalid scheduled function (e.g., missing event parameter)
    InvalidScheduledFunction {
        func_name: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid event handler (e.g., missing event parameter)
    InvalidEventHandler {
        func_name: String,
        event_type: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// Invalid function syntax - using fn instead of lambda syntax
    InvalidFunctionSyntax {
        context: String,
        func_name: String,
        arg_position: usize,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// `match` on an `enum open` is missing the required `_` default arm.
    /// Open enums admit new variants from any package, so the type checker
    /// can't prove exhaustiveness at the use site - a default arm is the
    /// only way to keep the match safe under future enrollment.
    OpenEnumMatchMissingDefault {
        enum_name: String,
        location: Option<ErrorLocation>,
    },
    /// `match` on a closed `enum` doesn't cover every declared variant and
    /// has no `_` default arm. The set of missing variants is included so
    /// the user can mechanically add the needed arms.
    NonExhaustiveMatch {
        enum_name: String,
        missing_variants: Vec<String>,
        location: Option<ErrorLocation>,
    },
    /// A literal-union type was re-declared with mismatched openness, or
    /// the `open` modifier was applied to a non-literal type alias.
    /// Examples that trigger this:
    ///   - `Fruit type "a"` followed by `Fruit type open | "b"`
    ///     (closed initial, open extension)
    ///   - `Fruit type open "a"` followed by `Fruit type "b"`
    ///     (open initial, closed re-declaration)
    ///   - `Foo type open Int` (`open` applied to a non-literal alias)
    OpenLiteralUnionMismatch {
        type_name: String,
        message: String,
        location: Option<ErrorLocation>,
    },
    /// A type-implementation arrow (`Source -> Target`) was declared inside a
    /// nested scope (e.g., a function body). Arrows mutate the global
    /// implementation registry, so a nested arrow is a hidden global side
    /// effect that other code in the same compilation unit may inadvertently
    /// pick up. Lift the arrow to the top level of a namespace.
    NestedTypeImplementation {
        source_type: String,
        target_type: String,
        location: Option<ErrorLocation>,
    },
    /// A referenced definition is marked `meta { deprecated: true }` (or
    /// `meta { deprecated: "use X instead" }`). This is a *warning*, not an
    /// error: it never fails compilation, but `hot check` and the LSP surface
    /// it so callers migrate off the deprecated API. `note`, when present,
    /// carries the custom migration message from the definition's metadata.
    DeprecatedUsage {
        name: String,
        note: Option<String>,
        location: Option<ErrorLocation>,
    },
    /// A local binding inside a function or lambda body is never referenced.
    /// This is a *warning*: silently dropping a value is how swallowed
    /// `Result.Err`s hide (Hot signatures declare success types, so the
    /// checker cannot see which calls can fail — every unused binding is a
    /// potential dropped error). Prefix the name with `_` to declare the
    /// discard intentional (e.g. `_quit if-err(write(...), false)`).
    UnusedBinding {
        name: String,
        location: Option<ErrorLocation>,
    },
    /// A statically known record/struct type does not contain the requested
    /// field. This rolls out as a warning first because runtime map access is
    /// still permissive and returns null for missing fields.
    UnknownField {
        type_name: String,
        field_name: String,
        location: Option<ErrorLocation>,
    },
}

impl fmt::Display for CompilerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompilerError::UnresolvedVariable {
                var_name,
                namespace,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Unresolved variable '{}' in namespace '{}': {}",
                        var_name, namespace, message
                    )
                }
            }
            CompilerError::UnresolvedFunction {
                func_name,
                namespace,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Unresolved function '{}' in namespace '{}': {}",
                        func_name, namespace, message
                    )
                }
            }
            CompilerError::UnresolvedType {
                type_name,
                namespace,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Unresolved type '{}' in namespace '{}': {}",
                        type_name, namespace, message
                    )
                }
            }
            CompilerError::TypeMismatch {
                expected,
                actual,
                var_name,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else if let Some(var) = var_name {
                    write!(
                        f,
                        "Type mismatch for variable '{}': expected '{}', got '{}'. {}",
                        var, expected, actual, message
                    )
                } else {
                    write!(
                        f,
                        "Type mismatch: expected '{}', got '{}'. {}",
                        expected, actual, message
                    )
                }
            }
            CompilerError::ArityMismatch {
                func_name,
                expected,
                actual,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Arity mismatch for function '{}': expected {} arguments, got {}. {}",
                        func_name, expected, actual, message
                    )
                }
            }
            CompilerError::InvalidFunctionCall {
                func_name,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(f, "Invalid function call '{}': {}", func_name, message)
                }
            }
            CompilerError::CallLibError {
                func_name,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "call-lib error for function '{}': {}",
                        func_name, message
                    )
                }
            }
            CompilerError::InvalidTypeAnnotation {
                annotation,
                reason,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid type annotation '{}': {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        annotation,
                        reason
                    )
                } else {
                    write!(f, "Invalid type annotation '{}': {}", annotation, reason)
                }
            }
            CompilerError::CircularTypeReference {
                type_name,
                cycle_path,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Circular type reference in '{}': {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        type_name,
                        cycle_path.join(" -> ")
                    )
                } else {
                    write!(
                        f,
                        "Circular type reference in '{}': {}",
                        type_name,
                        cycle_path.join(" -> ")
                    )
                }
            }
            CompilerError::InvalidGenericArity {
                type_name,
                expected,
                actual,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid generic arity for '{}': expected {} parameters, got {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        type_name,
                        expected,
                        actual
                    )
                } else {
                    write!(
                        f,
                        "Invalid generic arity for '{}': expected {} parameters, got {}",
                        type_name, expected, actual
                    )
                }
            }
            CompilerError::InvalidUnionType {
                types,
                reason,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid union type '{}': {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        types.join(" | "),
                        reason
                    )
                } else {
                    write!(f, "Invalid union type '{}': {}", types.join(" | "), reason)
                }
            }
            CompilerError::InvalidImplementation {
                source_type,
                target_type,
                reason,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid implementation '{}' -> '{}': {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        source_type,
                        target_type,
                        reason
                    )
                } else {
                    write!(
                        f,
                        "Invalid implementation '{}' -> '{}': {}",
                        source_type, target_type, reason
                    )
                }
            }
            CompilerError::MissingTypeImplementation {
                source_type,
                target_type,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Missing implementation: '{}' -> '{}' not defined",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        source_type,
                        target_type
                    )
                } else {
                    write!(
                        f,
                        "Missing implementation: '{}' -> '{}' not defined",
                        source_type, target_type
                    )
                }
            }
            CompilerError::AmbiguousTypeImplementation {
                source_type,
                target_type,
                candidates,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Ambiguous implementation: '{}' -> '{}' has multiple candidates: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        source_type,
                        target_type,
                        candidates.join(", ")
                    )
                } else {
                    write!(
                        f,
                        "Ambiguous implementation: '{}' -> '{}' has multiple candidates: {}",
                        source_type,
                        target_type,
                        candidates.join(", ")
                    )
                }
            }
            CompilerError::InvalidLazyUsage { context, location } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid lazy usage in context: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        context
                    )
                } else {
                    write!(f, "Invalid lazy usage in context: {}", context)
                }
            }
            CompilerError::InvalidVariadicUsage { context, location } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid variadic usage in context: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        context
                    )
                } else {
                    write!(f, "Invalid variadic usage in context: {}", context)
                }
            }
            CompilerError::InvalidFlowType {
                flow_type,
                expected_types,
                actual_type,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Invalid flow type '{}': expected one of [{}], got '{}'",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        flow_type,
                        expected_types.join(", "),
                        actual_type
                    )
                } else {
                    write!(
                        f,
                        "Invalid flow type '{}': expected one of [{}], got '{}'",
                        flow_type,
                        expected_types.join(", "),
                        actual_type
                    )
                }
            }
            CompilerError::IncompatibleFlowBranches {
                branch_types,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: Incompatible flow branch types: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        branch_types.join(", ")
                    )
                } else {
                    write!(
                        f,
                        "Incompatible flow branch types: {}",
                        branch_types.join(", ")
                    )
                }
            }
            CompilerError::InvalidScheduledFunction {
                func_name,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(f, "Invalid scheduled function '{}': {}", func_name, message)
                }
            }
            CompilerError::InvalidEventHandler {
                func_name,
                event_type,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Invalid event handler '{}' for event '{}': {}",
                        func_name, event_type, message
                    )
                }
            }
            CompilerError::InvalidFunctionSyntax {
                context,
                func_name,
                arg_position,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        message
                    )
                } else {
                    write!(
                        f,
                        "Invalid function syntax in {} (argument {} of '{}'): {}",
                        context, arg_position, func_name, message
                    )
                }
            }
            CompilerError::OpenEnumMatchMissingDefault {
                enum_name,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: `match` on open enum `{}` requires a `_` default arm",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        enum_name
                    )
                } else {
                    write!(
                        f,
                        "`match` on open enum `{}` requires a `_` default arm",
                        enum_name
                    )
                }
            }
            CompilerError::NonExhaustiveMatch {
                enum_name,
                missing_variants,
                location,
            } => {
                let missing = missing_variants
                    .iter()
                    .map(|v| format!("`{}.{}`", enum_name, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: non-exhaustive `match` on `{}`: missing {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        enum_name,
                        missing
                    )
                } else {
                    write!(
                        f,
                        "non-exhaustive `match` on `{}`: missing {}",
                        enum_name, missing
                    )
                }
            }
            CompilerError::OpenLiteralUnionMismatch {
                type_name,
                message,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: open literal union `{}`: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        type_name,
                        message
                    )
                } else {
                    write!(f, "open literal union `{}`: {}", type_name, message)
                }
            }
            CompilerError::NestedTypeImplementation {
                source_type,
                target_type,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: type implementation `{} -> {}` must be declared at top level (declared inside a function body)",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        source_type,
                        target_type,
                    )
                } else {
                    write!(
                        f,
                        "type implementation `{} -> {}` must be declared at top level (declared inside a function body)",
                        source_type, target_type,
                    )
                }
            }
            CompilerError::UnusedBinding { name, location } => {
                let body = format!(
                    "unused binding `{}` (prefix with `_` if the discard is intentional)",
                    name
                );
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<unknown>".to_string()),
                        loc.line,
                        loc.column,
                        body
                    )
                } else {
                    write!(f, "{}", body)
                }
            }
            CompilerError::DeprecatedUsage {
                name,
                note,
                location,
            } => {
                let body = match note {
                    Some(n) => format!("`{}` is deprecated: {}", name, n),
                    None => format!("`{}` is deprecated", name),
                };
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        body
                    )
                } else {
                    write!(f, "{}", body)
                }
            }
            CompilerError::UnknownField {
                type_name,
                field_name,
                location,
            } => {
                let body = format!(
                    "`{}` does not have a known field `{}`",
                    type_name, field_name
                );
                if let Some(loc) = location {
                    write!(
                        f,
                        "{}:{}:{}: {}",
                        loc.file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<source>".to_string()),
                        loc.line,
                        loc.column,
                        body
                    )
                } else {
                    write!(f, "{}", body)
                }
            }
        }
    }
}

/// Collection of compiler errors with pretty formatting support
#[derive(Debug, Clone, Default)]
pub struct CompilerErrors {
    pub errors: Vec<CompilerError>,
    /// Non-fatal warnings (e.g. deprecated-API usage). Warnings never cause
    /// `is_empty()` to report failure, so they do not fail compilation, but
    /// they are surfaced by `hot check` and the LSP.
    pub warnings: Vec<CompilerError>,
    pub source_cache: AHashMap<String, String>,
}

impl CompilerErrors {
    pub fn new() -> Self {
        Self {
            errors: Vec::new(),
            warnings: Vec::new(),
            source_cache: AHashMap::new(),
        }
    }

    pub fn with_error(error: CompilerError) -> Self {
        let mut errors = Self::new();
        errors.add(error);
        errors
    }

    pub fn add(&mut self, error: CompilerError) {
        self.errors.push(error);
    }

    /// Record a non-fatal warning. Warnings are reported but never fail
    /// compilation (`is_empty()` ignores them).
    pub fn add_warning(&mut self, warning: CompilerError) {
        self.warnings.push(warning);
    }

    pub fn add_source(&mut self, name: String, content: String) {
        self.source_cache.insert(name, content);
    }

    /// True when there are no hard errors. Warnings do not count, so a
    /// warnings-only result is still considered a successful compile.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// True when at least one non-fatal warning was collected.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn warnings_len(&self) -> usize {
        self.warnings.len()
    }

    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Render only the collected warnings (ariadne when source is available).
    /// Returns an empty string when there are no warnings so callers can
    /// unconditionally print the result.
    pub fn format_warnings(&self, color: bool) -> String {
        if self.warnings.is_empty() {
            return String::new();
        }
        let mut output = Vec::new();
        for warning in &self.warnings {
            if let Some(formatted) = self.create_ariadne_report(warning, color) {
                output.push(formatted);
            } else {
                output.push(warning.to_string());
            }
        }
        output.join("\n\n")
    }

    /// Format errors using ariadne reports when source is available, plain text otherwise.
    /// When `color` is true, output includes ANSI color codes for terminal display.
    pub fn format_error(&self, color: bool) -> String {
        if self.errors.is_empty() {
            return "No errors".to_string();
        }

        let mut output = Vec::new();

        for error in &self.errors {
            if let Some(formatted) = self.create_ariadne_report(error, color) {
                output.push(formatted);
            } else {
                output.push(error.to_string());
            }
        }

        output.join("\n\n")
    }

    fn create_ariadne_report(&self, error: &CompilerError, color: bool) -> Option<String> {
        let location = match error {
            CompilerError::UnresolvedVariable { location, .. } => location.as_ref(),
            CompilerError::UnresolvedFunction { location, .. } => location.as_ref(),
            CompilerError::UnresolvedType { location, .. } => location.as_ref(),
            CompilerError::TypeMismatch { location, .. } => location.as_ref(),
            CompilerError::ArityMismatch { location, .. } => location.as_ref(),
            CompilerError::InvalidFunctionCall { location, .. } => location.as_ref(),
            CompilerError::CallLibError { location, .. } => location.as_ref(),
            CompilerError::InvalidTypeAnnotation { location, .. } => location.as_ref(),
            CompilerError::CircularTypeReference { location, .. } => location.as_ref(),
            CompilerError::InvalidGenericArity { location, .. } => location.as_ref(),
            CompilerError::InvalidUnionType { location, .. } => location.as_ref(),
            CompilerError::InvalidImplementation { location, .. } => location.as_ref(),
            CompilerError::MissingTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::AmbiguousTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::InvalidLazyUsage { location, .. } => location.as_ref(),
            CompilerError::InvalidVariadicUsage { location, .. } => location.as_ref(),
            CompilerError::InvalidFlowType { location, .. } => location.as_ref(),
            CompilerError::IncompatibleFlowBranches { location, .. } => location.as_ref(),
            CompilerError::InvalidScheduledFunction { location, .. } => location.as_ref(),
            CompilerError::InvalidEventHandler { location, .. } => location.as_ref(),
            CompilerError::InvalidFunctionSyntax { location, .. } => location.as_ref(),
            CompilerError::OpenEnumMatchMissingDefault { location, .. } => location.as_ref(),
            CompilerError::NonExhaustiveMatch { location, .. } => location.as_ref(),
            CompilerError::OpenLiteralUnionMismatch { location, .. } => location.as_ref(),
            CompilerError::NestedTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::DeprecatedUsage { location, .. } => location.as_ref(),
            CompilerError::UnusedBinding { location, .. } => location.as_ref(),
            CompilerError::UnknownField { location, .. } => location.as_ref(),
        }?;

        let source_name = if let Some(file) = &location.file {
            file.display().to_string()
        } else {
            "<source>".to_string()
        };

        // Try to find source content
        let source = self
            .source_cache
            .get(&source_name)
            .or_else(|| self.source_cache.get("<source>"))
            .or_else(|| self.source_cache.get("<input>"))?;

        let mut colors = ColorGenerator::new();
        let label_color = colors.next();

        let span_start = location.position;
        let span_end = location.position + location.length.max(1);

        let report_kind = match error.severity() {
            DiagnosticSeverity::Warning => ReportKind::Warning,
            _ => ReportKind::Error,
        };
        let mut report = Report::build(report_kind, (source_name.as_str(), span_start..span_end))
            .with_config(Config::default().with_color(color));

        match error {
            CompilerError::UnresolvedVariable {
                var_name, message, ..
            } => {
                report = report
                    .with_code("unresolved-variable")
                    .with_message(format!("Unresolved variable '{}'", var_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    );
            }
            CompilerError::UnresolvedFunction {
                func_name, message, ..
            } => {
                report = report
                    .with_code("unresolved-function")
                    .with_message(format!("Unresolved function '{}'", func_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    );
            }
            CompilerError::UnresolvedType {
                type_name, message, ..
            } => {
                report = report
                    .with_code("unresolved-type")
                    .with_message(format!("Unresolved type '{}'", type_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    );
            }
            CompilerError::TypeMismatch {
                expected,
                actual,
                message,
                ..
            } => {
                report = report
                    .with_code("type-mismatch")
                    .with_message("Type mismatch")
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!(
                                "Expected '{}', got '{}'\n{}",
                                expected, actual, message
                            ))
                            .with_color(label_color),
                    );
            }
            CompilerError::ArityMismatch {
                func_name,
                expected,
                actual,
                message,
                ..
            } => {
                report = report
                    .with_code("arity-mismatch")
                    .with_message(format!("Arity mismatch for function '{}'", func_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!(
                                "Expected {} arguments, got {}\n{}",
                                expected, actual, message
                            ))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidFunctionCall {
                func_name, message, ..
            } => {
                report = report
                    .with_code("invalid-function-call")
                    .with_message(format!("Invalid function call '{}'", func_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    );
            }
            CompilerError::CallLibError {
                func_name, message, ..
            } => {
                report = report
                    .with_code("call-lib-error")
                    .with_message(format!("call-lib error for function '{}'", func_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidTypeAnnotation {
                annotation, reason, ..
            } => {
                report = report
                    .with_code("invalid-type-annotation")
                    .with_message(format!("Invalid type annotation '{}'", annotation))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(reason)
                            .with_color(label_color),
                    );
            }
            CompilerError::CircularTypeReference {
                type_name,
                cycle_path,
                ..
            } => {
                report = report
                    .with_code("circular-type-reference")
                    .with_message(format!("Circular type reference in '{}'", type_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("Cycle: {}", cycle_path.join(" -> ")))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidGenericArity {
                type_name,
                expected,
                actual,
                ..
            } => {
                report = report
                    .with_code("invalid-generic-arity")
                    .with_message(format!("Invalid generic arity for '{}'", type_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!(
                                "Expected {} parameters, got {}",
                                expected, actual
                            ))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidUnionType { types, reason, .. } => {
                report = report
                    .with_code("invalid-union-type")
                    .with_message(format!("Invalid union type '{}'", types.join(" | ")))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(reason)
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidImplementation {
                source_type,
                target_type,
                reason,
                ..
            } => {
                report = report
                    .with_code("invalid-implementation")
                    .with_message(format!(
                        "Invalid implementation: '{}' -> '{}'",
                        source_type, target_type
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(reason)
                            .with_color(label_color),
                    );
            }
            CompilerError::MissingTypeImplementation {
                source_type,
                target_type,
                ..
            } => {
                report = report
                    .with_code("missing-type-implementation")
                    .with_message(format!(
                        "Missing implementation: '{}' -> '{}' not defined",
                        source_type, target_type
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message("Implementation required here")
                            .with_color(label_color),
                    );
            }
            CompilerError::AmbiguousTypeImplementation {
                source_type,
                target_type,
                candidates,
                ..
            } => {
                report = report
                    .with_code("ambiguous-type-implementation")
                    .with_message(format!(
                        "Ambiguous implementation: '{}' -> '{}'",
                        source_type, target_type
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("Multiple candidates: {}", candidates.join(", ")))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidLazyUsage { context, .. } => {
                report = report
                    .with_code("invalid-lazy-usage")
                    .with_message("Invalid lazy usage")
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("Cannot use lazy in context: {}", context))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidVariadicUsage { context, .. } => {
                report = report
                    .with_code("invalid-variadic-usage")
                    .with_message("Invalid variadic usage")
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("Cannot use variadic in context: {}", context))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidFlowType {
                flow_type,
                expected_types,
                actual_type,
                ..
            } => {
                report = report
                    .with_code("invalid-flow-type")
                    .with_message(format!("Invalid flow type '{}'", flow_type))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!(
                                "Expected one of [{}], got '{}'",
                                expected_types.join(", "),
                                actual_type
                            ))
                            .with_color(label_color),
                    );
            }
            CompilerError::IncompatibleFlowBranches { branch_types, .. } => {
                report = report
                    .with_code("incompatible-flow-branches")
                    .with_message("Incompatible flow branch types")
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("Branch types: {}", branch_types.join(", ")))
                            .with_color(label_color),
                    );
            }
            CompilerError::InvalidScheduledFunction {
                func_name, message, ..
            } => {
                report = report
                    .with_code("invalid-scheduled-function")
                    .with_message(format!("Invalid scheduled function '{}'", func_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    )
                    .with_help(
                        "Scheduled functions must accept an event parameter: fn (event) { ... }",
                    );
            }
            CompilerError::InvalidEventHandler {
                func_name,
                event_type,
                message,
                ..
            } => {
                report = report
                    .with_code("invalid-event-handler")
                    .with_message(format!(
                        "Invalid event handler '{}' for event '{}'",
                        func_name, event_type
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    )
                    .with_help("Event handlers must accept an event parameter: fn (event) { ... }");
            }
            CompilerError::InvalidFunctionSyntax {
                context,
                func_name,
                arg_position,
                message,
                ..
            } => {
                report = report
                    .with_code("invalid-function-syntax")
                    .with_message(format!(
                        "Invalid function syntax in argument {} of '{}'",
                        arg_position, func_name
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message)
                            .with_color(label_color),
                    )
                    .with_help(format!(
                        "Use lambda syntax `(args) {{ body }}` instead of `fn (args) {{ body }}`\n\
                         when passing functions as arguments in {}",
                        context
                    ));
            }
            CompilerError::OpenEnumMatchMissingDefault { enum_name, .. } => {
                report = report
                    .with_code("open-enum-match-missing-default")
                    .with_message(format!(
                        "`match` on open enum `{}` requires a `_` default arm",
                        enum_name
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message("add `_ => { ... }` to handle future variants")
                            .with_color(label_color),
                    )
                    .with_help(
                        "Open enums (`enum open { ... }`) admit new variants enrolled via \
                         `Source -> Enum.Variant` arrows from any package, so the type checker \
                         can't prove the match is exhaustive. Adding a `_` default arm keeps \
                         the match safe under future enrollment.",
                    );
            }
            CompilerError::NonExhaustiveMatch {
                enum_name,
                missing_variants,
                ..
            } => {
                let missing = missing_variants
                    .iter()
                    .map(|v| format!("`{}.{}`", enum_name, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                report = report
                    .with_code("non-exhaustive-match")
                    .with_message(format!("non-exhaustive `match` on `{}`", enum_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!("missing arms for {}", missing))
                            .with_color(label_color),
                    )
                    .with_help(
                        "Closed enums (`enum { ... }`) require every variant to be matched. \
                         Either add the missing arms or add a `_` default arm.",
                    );
            }
            CompilerError::OpenLiteralUnionMismatch {
                type_name, message, ..
            } => {
                report = report
                    .with_code("open-literal-union-mismatch")
                    .with_message(format!(
                        "open literal union `{}` declaration mismatch",
                        type_name
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(message.clone())
                            .with_color(label_color),
                    )
                    .with_help(
                        "Closed literal unions can't be re-declared as open (and vice versa); \
                         the `open` modifier only applies to literal unions like `\"a\" | \"b\"`. \
                         Pick the openness up front: use `Foo type open \"a\" | \"b\"` to allow \
                         later top-level extensions, or `Foo type \"a\" | \"b\"` to forbid them.",
                    );
            }
            CompilerError::NestedTypeImplementation {
                source_type,
                target_type,
                ..
            } => {
                report = report
                    .with_code("nested-type-implementation")
                    .with_message(format!(
                        "type implementation `{} -> {}` must be declared at top level",
                        source_type, target_type,
                    ))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message("nested arrow declarations are not allowed")
                            .with_color(label_color),
                    )
                    .with_help(
                        "Type implementations (`->` arrows) mutate the global \
                         implementation registry, so declaring one inside a function \
                         body is a hidden global side effect. Lift the arrow \
                         (and any types it depends on) to the top level of a namespace.",
                    );
            }
            CompilerError::UnusedBinding { name, .. } => {
                report = report
                    .with_code("unused-binding")
                    .with_message(format!("binding `{}` is never used", name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message("bound here but never referenced")
                            .with_color(label_color),
                    )
                    .with_help(
                        "An unused binding silently drops its value — including any                          Result.Err a fallible call returned. Handle the value, or                          prefix the name with `_` to declare the discard intentional.",
                    );
            }
            CompilerError::DeprecatedUsage { name, note, .. } => {
                let label_msg = match note {
                    Some(n) => format!("deprecated: {}", n),
                    None => "this is deprecated".to_string(),
                };
                report = report
                    .with_code("deprecated-usage")
                    .with_message(format!("`{}` is deprecated", name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(label_msg)
                            .with_color(label_color),
                    );
            }
            CompilerError::UnknownField {
                type_name,
                field_name,
                ..
            } => {
                report = report
                    .with_code("unknown-field")
                    .with_message(format!("Unknown field `{}`", field_name))
                    .with_label(
                        Label::new((source_name.as_str(), span_start..span_end))
                            .with_message(format!(
                                "`{}` does not have a known field `{}`",
                                type_name, field_name
                            ))
                            .with_color(label_color),
                    );
            }
        }

        let mut buffer = Vec::new();
        if report
            .finish()
            .write((source_name.as_str(), Source::from(source)), &mut buffer)
            .is_ok()
        {
            String::from_utf8(buffer).ok()
        } else {
            None
        }
    }
}

impl fmt::Display for CompilerErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let output = self.format_error(false);
        f.write_str(&output)
    }
}

impl std::error::Error for CompilerErrors {}

impl From<CompilerError> for CompilerErrors {
    fn from(error: CompilerError) -> Self {
        Self::with_error(error)
    }
}

/// Result type for compiler operations
pub type CompilerResult<T> = std::result::Result<T, CompilerErrors>;

impl CompilerError {
    /// Borrow the optional location attached to any error variant.
    pub fn location(&self) -> Option<&ErrorLocation> {
        match self {
            CompilerError::UnresolvedVariable { location, .. } => location.as_ref(),
            CompilerError::UnresolvedFunction { location, .. } => location.as_ref(),
            CompilerError::UnresolvedType { location, .. } => location.as_ref(),
            CompilerError::TypeMismatch { location, .. } => location.as_ref(),
            CompilerError::ArityMismatch { location, .. } => location.as_ref(),
            CompilerError::InvalidFunctionCall { location, .. } => location.as_ref(),
            CompilerError::CallLibError { location, .. } => location.as_ref(),
            CompilerError::InvalidTypeAnnotation { location, .. } => location.as_ref(),
            CompilerError::CircularTypeReference { location, .. } => location.as_ref(),
            CompilerError::InvalidGenericArity { location, .. } => location.as_ref(),
            CompilerError::InvalidUnionType { location, .. } => location.as_ref(),
            CompilerError::InvalidImplementation { location, .. } => location.as_ref(),
            CompilerError::MissingTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::AmbiguousTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::InvalidLazyUsage { location, .. } => location.as_ref(),
            CompilerError::InvalidVariadicUsage { location, .. } => location.as_ref(),
            CompilerError::InvalidFlowType { location, .. } => location.as_ref(),
            CompilerError::IncompatibleFlowBranches { location, .. } => location.as_ref(),
            CompilerError::InvalidScheduledFunction { location, .. } => location.as_ref(),
            CompilerError::InvalidEventHandler { location, .. } => location.as_ref(),
            CompilerError::InvalidFunctionSyntax { location, .. } => location.as_ref(),
            CompilerError::OpenEnumMatchMissingDefault { location, .. } => location.as_ref(),
            CompilerError::NonExhaustiveMatch { location, .. } => location.as_ref(),
            CompilerError::OpenLiteralUnionMismatch { location, .. } => location.as_ref(),
            CompilerError::NestedTypeImplementation { location, .. } => location.as_ref(),
            CompilerError::DeprecatedUsage { location, .. } => location.as_ref(),
            CompilerError::UnusedBinding { location, .. } => location.as_ref(),
            CompilerError::UnknownField { location, .. } => location.as_ref(),
        }
    }

    /// Diagnostic severity for this variant. Today only `DeprecatedUsage` is a
    /// warning; everything else is a hard error that fails compilation.
    pub fn severity(&self) -> DiagnosticSeverity {
        match self {
            CompilerError::DeprecatedUsage { .. } | CompilerError::UnknownField { .. } => {
                DiagnosticSeverity::Warning
            }
            _ => DiagnosticSeverity::Error,
        }
    }

    /// Stable kebab-case identifier for this variant (e.g. `"type-mismatch"`
    /// for `TypeMismatch`).
    ///
    /// These identifiers appear in compiler diagnostics and LSP output. They
    /// are deliberately self-descriptive (rather than opaque numeric codes
    /// like `E004`) so users, LLMs, and editor integrations can understand an
    /// error at a glance without a lookup table. Treat them as a public
    /// API surface: once an identifier ships, renames are breaking changes
    /// for any tooling that keys on the name (e.g. future `allow`/`deny`
    /// suppression lists, CI counters, docs links).
    pub fn code(&self) -> &'static str {
        match self {
            CompilerError::UnresolvedVariable { .. } => "unresolved-variable",
            CompilerError::UnresolvedFunction { .. } => "unresolved-function",
            CompilerError::UnresolvedType { .. } => "unresolved-type",
            CompilerError::TypeMismatch { .. } => "type-mismatch",
            CompilerError::ArityMismatch { .. } => "arity-mismatch",
            CompilerError::InvalidFunctionCall { .. } => "invalid-function-call",
            CompilerError::CallLibError { .. } => "call-lib-error",
            CompilerError::InvalidTypeAnnotation { .. } => "invalid-type-annotation",
            CompilerError::CircularTypeReference { .. } => "circular-type-reference",
            CompilerError::InvalidGenericArity { .. } => "invalid-generic-arity",
            CompilerError::InvalidUnionType { .. } => "invalid-union-type",
            CompilerError::InvalidImplementation { .. } => "invalid-implementation",
            CompilerError::MissingTypeImplementation { .. } => "missing-type-implementation",
            CompilerError::AmbiguousTypeImplementation { .. } => "ambiguous-type-implementation",
            CompilerError::InvalidLazyUsage { .. } => "invalid-lazy-usage",
            CompilerError::InvalidVariadicUsage { .. } => "invalid-variadic-usage",
            CompilerError::InvalidFlowType { .. } => "invalid-flow-type",
            CompilerError::IncompatibleFlowBranches { .. } => "incompatible-flow-branches",
            CompilerError::InvalidScheduledFunction { .. } => "invalid-scheduled-function",
            CompilerError::InvalidEventHandler { .. } => "invalid-event-handler",
            CompilerError::InvalidFunctionSyntax { .. } => "invalid-function-syntax",
            CompilerError::OpenEnumMatchMissingDefault { .. } => "open-enum-match-missing-default",
            CompilerError::NonExhaustiveMatch { .. } => "non-exhaustive-match",
            CompilerError::OpenLiteralUnionMismatch { .. } => "open-literal-union-mismatch",
            CompilerError::NestedTypeImplementation { .. } => "nested-type-implementation",
            CompilerError::DeprecatedUsage { .. } => "deprecated-usage",
            CompilerError::UnusedBinding { .. } => "unused-binding",
            CompilerError::UnknownField { .. } => "unknown-field",
        }
    }
}

impl CompilerErrors {
    /// Convert all collected errors into LSP-style `Diagnostic`s.
    ///
    /// The diagnostic message is rendered via the same ariadne pipeline used
    /// for terminal output, so consumers get the rich body when source is
    /// available in the cache. When no source is available we fall back to
    /// the plain `Display` rendering.
    pub fn to_diagnostics(&self) -> Vec<Diagnostic> {
        self.errors
            .iter()
            .chain(self.warnings.iter())
            .map(|e| self.error_to_diagnostic(e))
            .collect()
    }

    fn error_to_diagnostic(&self, error: &CompilerError) -> Diagnostic {
        let (file, range) = match error.location() {
            Some(loc) => {
                let start_line = loc.line.saturating_sub(1) as u32;
                let start_char = loc.column.saturating_sub(1) as u32;
                let end_char = start_char.saturating_add(loc.length.max(1) as u32);
                let range = DiagnosticRange {
                    start: DiagnosticPosition {
                        line: start_line,
                        character: start_char,
                    },
                    end: DiagnosticPosition {
                        line: start_line,
                        character: end_char,
                    },
                };
                let file = loc.file.as_ref().map(|p| p.display().to_string());
                (file, range)
            }
            None => (
                None,
                DiagnosticRange {
                    start: DiagnosticPosition {
                        line: 0,
                        character: 0,
                    },
                    end: DiagnosticPosition {
                        line: 0,
                        character: 0,
                    },
                },
            ),
        };

        // Reuse the ariadne pipeline (no color) so the message is consistent
        // with what the LSP and terminal show. Falls back to Display when no
        // source is cached.
        let mut single = CompilerErrors::new();
        single.errors.push(error.clone());
        single.source_cache = self.source_cache.clone();
        let message = single
            .create_ariadne_report(error, false)
            .unwrap_or_else(|| error.to_string());

        Diagnostic {
            file,
            range,
            severity: error.severity().as_lsp_number(),
            code: error.code().to_string(),
            source: "hot".to_string(),
            message,
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::path::PathBuf;

    fn loc(line: usize, column: usize, length: usize, file: &str) -> ErrorLocation {
        ErrorLocation {
            line,
            column,
            position: 0,
            length,
            file: Some(PathBuf::from(file)),
        }
    }

    #[test]
    fn empty_collection_yields_no_diagnostics() {
        let errors = CompilerErrors::new();
        assert!(errors.to_diagnostics().is_empty());
    }

    #[test]
    fn warnings_render_with_warning_severity_and_do_not_fail() {
        let mut errors = CompilerErrors::new();
        errors.add_warning(CompilerError::DeprecatedUsage {
            name: "try-call".to_string(),
            note: Some("use `try` instead".to_string()),
            location: Some(loc(2, 1, 8, "/tmp/x.hot")),
        });

        // Warnings never count as compilation failure.
        assert!(errors.is_empty(), "warnings must not make is_empty() false");
        assert!(errors.has_warnings());

        // to_diagnostics surfaces the warning with severity 2 (Warning).
        let diags = errors.to_diagnostics();
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code, "deprecated-usage");
        assert_eq!(d.severity, DiagnosticSeverity::Warning.as_lsp_number());
        assert_eq!(d.severity, 2);
    }

    #[test]
    fn unknown_field_warning_uses_stable_code_and_warning_severity() {
        let mut errors = CompilerErrors::new();
        errors.add_warning(CompilerError::UnknownField {
            type_name: "A".to_string(),
            field_name: "b".to_string(),
            location: Some(loc(3, 5, 3, "/tmp/x.hot")),
        });

        assert!(errors.is_empty(), "unknown-field is rollout warning only");
        assert!(errors.has_warnings());

        let diags = errors.to_diagnostics();
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code, "unknown-field");
        assert_eq!(d.severity, DiagnosticSeverity::Warning.as_lsp_number());
        assert!(d.message.contains("known field `b`"));
    }

    #[test]
    fn errors_and_warnings_both_appear_in_diagnostics() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::UnresolvedVariable {
            var_name: "foo".to_string(),
            namespace: "test".to_string(),
            message: "unresolved foo".to_string(),
            location: Some(loc(3, 5, 3, "/tmp/x.hot")),
        });
        errors.add_warning(CompilerError::DeprecatedUsage {
            name: "old".to_string(),
            note: None,
            location: Some(loc(9, 1, 3, "/tmp/x.hot")),
        });

        let diags = errors.to_diagnostics();
        assert_eq!(diags.len(), 2);
        // Errors come first (severity 1), then warnings (severity 2).
        assert_eq!(diags[0].severity, 1);
        assert_eq!(diags[1].severity, 2);
        assert!(!errors.is_empty(), "an error must make is_empty() false");
    }

    #[test]
    fn diagnostic_basic_fields() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::UnresolvedVariable {
            var_name: "foo".to_string(),
            namespace: "test".to_string(),
            message: "unresolved foo".to_string(),
            location: Some(loc(3, 5, 3, "/tmp/x.hot")),
        });

        let diags = errors.to_diagnostics();
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code, "unresolved-variable");
        assert_eq!(d.source, "hot");
        assert_eq!(d.severity, DiagnosticSeverity::Error.as_lsp_number());
        assert_eq!(d.file.as_deref(), Some("/tmp/x.hot"));
        // 1-based line/col → 0-based LSP positions
        assert_eq!(d.range.start.line, 2);
        assert_eq!(d.range.start.character, 4);
        // length 3 → end character should be start + 3
        assert_eq!(d.range.end.character, 7);
        assert!(d.message.contains("foo"));
    }

    #[test]
    fn diagnostic_without_location_uses_zero_range() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::InvalidFunctionCall {
            func_name: "<pipeline>".to_string(),
            message: "boom".to_string(),
            location: None,
        });

        let diags = errors.to_diagnostics();
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert!(d.file.is_none());
        assert_eq!(d.range.start.line, 0);
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.line, 0);
        assert_eq!(d.range.end.character, 0);
        assert!(d.message.contains("boom"));
    }

    #[test]
    fn diagnostic_zero_length_fills_to_one_char() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::UnresolvedFunction {
            func_name: "bar".to_string(),
            namespace: "test".to_string(),
            message: "unresolved bar".to_string(),
            location: Some(loc(1, 1, 0, "/tmp/y.hot")),
        });

        let diags = errors.to_diagnostics();
        let d = &diags[0];
        // length 0 should be clamped to at least 1 so the range is visible
        assert_eq!(d.range.end.character, d.range.start.character + 1);
        assert_eq!(d.code, "unresolved-function");
    }

    #[test]
    fn diagnostic_renders_ariadne_message_with_source() {
        let source = "name 1\nfoo 2\nbar baz\n".to_string();
        let mut errors = CompilerErrors::new();
        errors.add_source("/tmp/z.hot".to_string(), source);
        errors.add(CompilerError::UnresolvedVariable {
            var_name: "baz".to_string(),
            namespace: "test".to_string(),
            message: "unresolved baz".to_string(),
            location: Some(loc(3, 5, 3, "/tmp/z.hot")),
        });

        let diags = errors.to_diagnostics();
        let d = &diags[0];
        // ariadne report includes the file name and a snippet header
        assert!(d.message.contains("/tmp/z.hot"));
        assert!(d.message.contains("baz"));
    }

    #[test]
    fn diagnostic_serializes_to_lsp_shaped_json() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::UnresolvedVariable {
            var_name: "qux".to_string(),
            namespace: "test".to_string(),
            message: "unresolved qux".to_string(),
            location: Some(loc(2, 1, 3, "/tmp/a.hot")),
        });
        let diags = errors.to_diagnostics();
        let json = serde_json::to_value(&diags).expect("serialize");
        let arr = json.as_array().expect("array");
        let obj = arr[0].as_object().expect("object");
        // LSP-required fields
        assert!(obj.contains_key("range"));
        assert!(obj.contains_key("severity"));
        assert!(obj.contains_key("code"));
        assert!(obj.contains_key("source"));
        assert!(obj.contains_key("message"));
        // Severity 1 = Error per LSP spec
        assert_eq!(obj["severity"].as_u64(), Some(1));
        assert_eq!(obj["code"].as_str(), Some("unresolved-variable"));
    }

    #[test]
    fn diagnostic_skips_file_when_absent() {
        let mut errors = CompilerErrors::new();
        errors.add(CompilerError::InvalidFunctionCall {
            func_name: "<pipeline>".to_string(),
            message: "no source".to_string(),
            location: None,
        });
        let json = serde_json::to_value(errors.to_diagnostics()).unwrap();
        let obj = json[0].as_object().unwrap();
        // `file` field is optional and should be omitted when None
        assert!(!obj.contains_key("file"));
    }

    #[test]
    fn error_codes_are_stable_per_variant() {
        // Quick sanity check that representative variants still emit the
        // expected identifiers — guards against accidental renames.
        //
        // These identifiers appear in user-facing diagnostics and LSP
        // output, so renames are breaking changes for any tooling that
        // keys on them (future `allow`/`deny` suppression lists, docs
        // links, CI counters, etc.).
        assert_eq!(
            CompilerError::UnresolvedVariable {
                var_name: "x".into(),
                namespace: "ns".into(),
                message: "msg".into(),
                location: None,
            }
            .code(),
            "unresolved-variable"
        );
        assert_eq!(
            CompilerError::UnresolvedFunction {
                func_name: "x".into(),
                namespace: "ns".into(),
                message: "msg".into(),
                location: None,
            }
            .code(),
            "unresolved-function"
        );
        assert_eq!(
            CompilerError::ArityMismatch {
                func_name: "x".into(),
                expected: 1,
                actual: 0,
                message: "msg".into(),
                location: None,
            }
            .code(),
            "arity-mismatch"
        );
    }
}
