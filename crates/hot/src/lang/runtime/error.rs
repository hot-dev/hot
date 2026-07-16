// Runtime error reporting with source location information and pretty printing

use ariadne::{ColorGenerator, Config, Label, Report, ReportKind};
use std::path::PathBuf;

/// Source location information for runtime errors
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLocation {
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
    pub file: Option<PathBuf>,
}

/// Runtime error with rich context for pretty printing
#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub location: Option<SourceLocation>,
    pub function_name: Option<String>,
    pub instruction_pointer: Option<usize>,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref location) = self.location {
            let file_prefix = if let Some(ref file) = location.file {
                format!("{}:", file.display())
            } else {
                String::new()
            };

            write!(
                f,
                "{}Runtime error at line {}, column {}: {}",
                file_prefix, location.line, location.column, self.message
            )
        } else {
            // Enhanced format for runtime errors without location. The
            // instruction pointer is deliberately omitted: it is VM-internal
            // context that means nothing to users (it stays visible in the
            // Debug format).
            let function_context = if let Some(ref func_name) = self.function_name {
                format!(" in function '{}'", func_name)
            } else {
                String::new()
            };

            write!(f, "Runtime error{}: {}", function_context, self.message)
        }
    }
}

impl RuntimeError {
    /// Create a new runtime error with just a message
    pub fn new(message: String) -> Self {
        Self {
            message,
            location: None,
            function_name: None,
            instruction_pointer: None,
        }
    }

    /// Create a new runtime error with location information
    pub fn with_location(message: String, location: SourceLocation) -> Self {
        Self {
            message,
            location: Some(location),
            function_name: None,
            instruction_pointer: None,
        }
    }

    /// Add function context to the error
    pub fn with_function(mut self, function_name: String) -> Self {
        self.function_name = Some(function_name);
        self
    }

    /// Add instruction pointer context to the error
    pub fn with_instruction_pointer(mut self, ip: usize) -> Self {
        self.instruction_pointer = Some(ip);
        self
    }

    /// Format this error using ariadne source context.
    /// When `color` is true, output includes ANSI color codes for terminal display.
    pub fn format_error(&self, source: &str, color: bool) -> Option<String> {
        let location = self.location.as_ref()?;

        // Validate position is within source bounds
        let source_len = source.len();
        if location.position > source_len {
            // Position is beyond end of file, create a simple error message
            return Some(format!(
                "Runtime error at end of file (line {}, column {}): {}",
                location.line, location.column, self.message
            ));
        }

        let mut colors = ColorGenerator::new();
        let label_color = colors.next();

        let span_start = location.position;
        let span_end = (location.position + location.length.max(1)).min(source_len);

        let source_name = if let Some(ref file) = location.file {
            file.display().to_string()
        } else {
            "<source>".to_string()
        };

        let mut report_builder = Report::build(
            ReportKind::Error,
            (source_name.as_str(), span_start..span_end),
        )
        .with_config(Config::default().with_color(color))
        .with_code("runtime-error")
        .with_message("Runtime Error");

        // Add function context if available
        if let Some(ref func_name) = self.function_name {
            report_builder = report_builder.with_note(format!("In function: {}", func_name));
        }

        let report = report_builder.with_label(
            Label::new((source_name.as_str(), span_start..span_end))
                .with_message(&self.message)
                .with_color(label_color),
        );

        let mut output = Vec::new();
        match report.finish().write(
            (source_name.as_str(), ariadne::Source::from(source)),
            &mut output,
        ) {
            Ok(_) => String::from_utf8(output).ok(),
            Err(_) => {
                // Ariadne failed, create a simple error message
                Some(format!(
                    "Runtime error at line {}, column {}: {}",
                    location.line, location.column, self.message
                ))
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Convert a simple string error into a RuntimeError
impl From<String> for RuntimeError {
    fn from(message: String) -> Self {
        RuntimeError::new(message)
    }
}

/// Convert a &str error into a RuntimeError
impl From<&str> for RuntimeError {
    fn from(message: &str) -> Self {
        RuntimeError::new(message.to_string())
    }
}
