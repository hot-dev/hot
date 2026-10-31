//! Project diagnostics.
//!
//! The server runs the same check `hot check --check.format json` runs
//! (`Engine::check_sources_pipeline_diagnostics_with_overlays`), with open
//! buffers in place of their files, so editor diagnostics include every
//! validation the check runs, test files, and errors other files cause. The
//! result is grouped by file. An error without a location goes to the file
//! it names, else to `hot.hot`, else to a window message; never to whichever
//! buffer happens to be open.

use crate::text::LineIndex;
use crate::workspace::Workspace;
use ahash::{AHashMap, AHashSet};
use hot::lang::ast::Program;
use hot::lang::engine::pipeline::SourceOverlays;
use hot::lang::errors::{CompilerError, CompilerErrors};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

/// What one analysis runs on.
pub struct Input {
    pub workspace: Arc<Workspace>,
    /// Open `.hot` documents: canonical path and text.
    pub open: Vec<(PathBuf, String)>,
}

/// What one analysis found.
pub struct Output {
    /// Diagnostics by canonical file path. Files without problems are absent.
    pub by_file: AHashMap<PathBuf, Vec<Diagnostic>>,
    /// Errors that belong to no file, for a window message.
    pub messages: Vec<String>,
    /// The definitions of the program the check parsed, when it got that
    /// far.
    pub index: Option<crate::index::Index>,
    /// Whether the check stopped at parse errors, so it says nothing about
    /// files other than the ones in `by_file`.
    pub parse_failed: bool,
    /// The project's source files (canonical).
    pub project_files: Vec<PathBuf>,
    /// The project's source files as the check names them (the paths
    /// definitions in `index` carry).
    pub project_files_raw: Vec<String>,
}

/// An absolute, canonical form of a path the check reported, for comparing
/// with document paths.
pub fn normalize(root: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return canonical;
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn is_config_like(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("hot.hot" | "pkg.hot" | "ctx.hot")
    )
}

/// Text of files diagnostics point into: open buffers first, then disk.
struct Texts<'a> {
    open: &'a AHashMap<PathBuf, String>,
    disk: AHashMap<PathBuf, Option<(String, LineIndex)>>,
}

impl Texts<'_> {
    fn range(&mut self, path: &Path, line: usize, column: usize, length: usize) -> Range {
        if !self.disk.contains_key(path) {
            let text = self
                .open
                .get(path)
                .cloned()
                .or_else(|| std::fs::read_to_string(path).ok());
            let entry = text.map(|text| {
                let index = LineIndex::new(&text);
                (text, index)
            });
            self.disk.insert(path.to_path_buf(), entry);
        }
        match self.disk.get(path).and_then(|e| e.as_ref()) {
            Some((text, index)) => index.hot_range(text, line, column, length),
            None => {
                let start = Position::new(
                    line.saturating_sub(1) as u32,
                    column.saturating_sub(1) as u32,
                );
                Range::new(
                    start,
                    Position::new(start.line, start.character + length.max(1) as u32),
                )
            }
        }
    }
}

fn severity(error: &CompilerError) -> DiagnosticSeverity {
    match error.severity() {
        hot::lang::errors::DiagnosticSeverity::Warning => DiagnosticSeverity::WARNING,
        hot::lang::errors::DiagnosticSeverity::Information => DiagnosticSeverity::INFORMATION,
        hot::lang::errors::DiagnosticSeverity::Hint => DiagnosticSeverity::HINT,
        hot::lang::errors::DiagnosticSeverity::Error => DiagnosticSeverity::ERROR,
    }
}

fn diagnostic(
    range: Range,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("hot".to_string()),
        message,
        ..Default::default()
    }
}

/// The start of a file, for a diagnostic about the whole file.
fn file_start() -> Range {
    Range::new(Position::new(0, 0), Position::new(0, 0))
}

/// `::ns/name` references in a message, in order.
fn qualified_names(message: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find("::") {
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, ':' | '/' | '-' | '_' | '$')))
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        let path = &tail[..end];
        if let Some((ns, name)) = path.rsplit_once('/')
            && !ns.is_empty()
            && !name.is_empty()
        {
            out.push((ns.to_string(), name.to_string()));
        }
        rest = &tail[end.max(2)..];
    }
    out
}

/// Where in the program a qualified name is defined.
fn definition_location(
    program: &Program,
    ns: &str,
    name: &str,
) -> Option<(PathBuf, usize, usize, usize)> {
    let (_, namespace) = program
        .namespaces
        .iter()
        .find(|(path, _)| path.to_string() == ns)?;
    let (var, _) = namespace
        .scope
        .vars
        .iter()
        .find(|(var, _)| var.sym.name() == name && var.src.is_some())?;
    let src = var.src.as_ref()?;
    let file = src
        .file
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| namespace.source_file.clone())?;
    Some((file, src.line, src.column, name.chars().count()))
}

pub fn run(input: Input) -> Output {
    let workspace = &input.workspace;
    let root = workspace.root.as_path();
    // Nothing to check: no project, and no file open.
    if !workspace.has_conf && input.open.is_empty() {
        return Output {
            by_file: AHashMap::new(),
            messages: Vec::new(),
            index: None,
            parse_failed: false,
            project_files: Vec::new(),
            project_files_raw: Vec::new(),
        };
    }
    let open: AHashMap<PathBuf, String> = input.open.iter().cloned().collect();

    // Buffers that differ from disk replace their files; every open file
    // the project doesn't include joins the check.
    let mut overlays = SourceOverlays::default();
    for (path, text) in &input.open {
        if is_config_like(path) {
            continue;
        }
        let on_disk = std::fs::read_to_string(path).ok();
        if on_disk.as_deref() != Some(text.as_str()) {
            overlays.files.insert(path.clone(), text.clone());
        }
        overlays.extra_files.push(path.clone());
    }

    // A configuration that failed to load names no project to check; check
    // the open files on their own instead of reporting a missing project.
    let use_project = workspace.has_conf && workspace.conf_error.is_none();
    let (src_paths, test_paths, project) = if use_project {
        (
            workspace.src_paths.clone(),
            workspace.test_paths.clone(),
            workspace.project_name.clone(),
        )
    } else {
        (Vec::new(), Vec::new(), None)
    };
    let check = hot::lang::engine::Engine::check_sources_pipeline_diagnostics_with_overlays(
        &src_paths,
        &test_paths,
        Some(&workspace.conf),
        project.as_deref(),
        workspace.ctx_keys.as_ref(),
        &overlays,
    );

    let mut texts = Texts {
        open: &open,
        disk: AHashMap::new(),
    };
    let mut by_file: AHashMap<PathBuf, Vec<Diagnostic>> = AHashMap::new();
    let mut messages = Vec::new();
    let project_files: Vec<PathBuf> = check
        .project_files
        .iter()
        .map(|f| normalize(root, Path::new(f)))
        .collect();
    let known_files: AHashSet<&PathBuf> = project_files.iter().chain(open.keys()).collect();

    route_errors(
        &check.diagnostics,
        check.program.as_ref(),
        workspace,
        &known_files,
        &mut texts,
        &mut by_file,
        &mut messages,
    );

    // The configuration file: its syntax, and why it failed to load.
    if workspace.has_conf {
        let conf_path = normalize(root, &workspace.conf_path());
        let text = open
            .get(&conf_path)
            .cloned()
            .or_else(|| std::fs::read_to_string(&conf_path).ok())
            .unwrap_or_default();
        let parse_error = syntax_diagnostic(&conf_path, &text);
        let has_parse_error = parse_error.is_some();
        if let Some(d) = parse_error {
            by_file.entry(conf_path.clone()).or_default().push(d);
        }
        if let Some(error) = &workspace.conf_error
            && !has_parse_error
        {
            by_file.entry(conf_path).or_default().push(diagnostic(
                file_start(),
                DiagnosticSeverity::ERROR,
                "configuration-error",
                format!("Configuration error: {error}"),
            ));
        }
    }
    // Other open configuration files: their syntax.
    for (path, text) in &input.open {
        if is_config_like(path)
            && *path != normalize(root, &workspace.conf_path())
            && let Some(d) = syntax_diagnostic(path, text)
        {
            by_file.entry(path.clone()).or_default().push(d);
        }
    }

    Output {
        by_file,
        messages,
        index: check.program.map(crate::index::Index::new),
        parse_failed: check.parse_failed,
        project_files,
        project_files_raw: check.project_files,
    }
}

/// A parse error in a single file, as a diagnostic.
fn syntax_diagnostic(path: &Path, text: &str) -> Option<Diagnostic> {
    let error = hot::lang::parser::parse_hot_file(text, &path.to_string_lossy()).err()?;
    let index = LineIndex::new(text);
    let loc = &error.location;
    let range = index.hot_range(text, loc.line, loc.column, loc.length);
    let message = error
        .format_error(text, false)
        .unwrap_or_else(|| error.to_string());
    Some(diagnostic(
        range,
        DiagnosticSeverity::ERROR,
        error.code(),
        message,
    ))
}

/// Convert the check's errors and warnings to LSP diagnostics by file.
fn route_errors(
    errors: &CompilerErrors,
    program: Option<&Program>,
    workspace: &Workspace,
    known_files: &AHashSet<&PathBuf>,
    texts: &mut Texts<'_>,
    by_file: &mut AHashMap<PathBuf, Vec<Diagnostic>>,
    messages: &mut Vec<String>,
) {
    let root = workspace.root.as_path();
    let rendered = errors.to_diagnostics();
    let all = errors.errors.iter().chain(errors.warnings.iter());
    let mut seen = AHashSet::new();
    for (error, rendered) in all.zip(rendered) {
        let code = error.code();
        let severity = severity(error);
        let message = rendered.message;
        if !seen.insert((
            error.location().map(|l| (l.file.clone(), l.line, l.column)),
            message.clone(),
        )) {
            continue;
        }

        if let Some(loc) = error.location()
            && let Some(file) = &loc.file
        {
            let path = normalize(root, file);
            let range = texts.range(&path, loc.line, loc.column, loc.length);
            by_file
                .entry(path)
                .or_default()
                .push(diagnostic(range, severity, code, message));
            continue;
        }

        // No file: the definition the error names, a file it names, the
        // configuration, or a window message.
        let plain = error.to_string();
        let named = program.and_then(|program| {
            qualified_names(&plain)
                .into_iter()
                .find_map(|(ns, name)| definition_location(program, &ns, &name))
        });
        if let Some((file, line, column, length)) = named {
            let path = normalize(root, &file);
            let range = texts.range(&path, line, column, length);
            by_file
                .entry(path)
                .or_default()
                .push(diagnostic(range, severity, code, message));
            continue;
        }
        let mentioned: Vec<PathBuf> = known_files
            .iter()
            .filter(|path| {
                let absolute = path.display().to_string();
                let relative = path
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                plain.contains(&absolute) || (!relative.is_empty() && plain.contains(&relative))
            })
            .map(|path| (*path).clone())
            .collect();
        if !mentioned.is_empty() {
            for path in mentioned {
                by_file.entry(path).or_default().push(diagnostic(
                    file_start(),
                    severity,
                    code,
                    message.clone(),
                ));
            }
            continue;
        }
        if workspace.has_conf {
            by_file
                .entry(normalize(root, &workspace.conf_path()))
                .or_default()
                .push(diagnostic(file_start(), severity, code, message));
        } else {
            messages.push(plain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_qualified_names_in_messages() {
        assert_eq!(
            qualified_names(
                "Non-exhaustive match in '::app::main/color-name': missing ::app::model/Color.Blue"
            ),
            [
                ("::app::main".to_string(), "color-name".to_string()),
                ("::app::model".to_string(), "Color".to_string())
            ]
        );
        assert!(qualified_names("Project 'x' is not defined").is_empty());
    }
}
