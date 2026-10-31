//! `hot check` — type/syntax check the project, optionally watching.
//!
//! Exit codes: 0 when the check passes (warnings included, unless
//! `--check.deny-warnings`), 1 when it reports errors (or warnings with
//! `--check.deny-warnings`), and 2 when the check could not run at all
//! (an unknown format, a missing path, a project with no .hot files).

use std::path::{Path, PathBuf};

use hot::lang::errors::{
    CompilerError, CompilerErrors, Diagnostic, DiagnosticPosition, DiagnosticRange,
};
use hot::secret_scan::SecretMatch;
use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::scan_cli_inputs;
use crate::conf::{get_merged_src_paths, get_merged_test_paths};

/// Output format of `hot check` and `hot watch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckFormat {
    /// The ariadne report.
    Pretty,
    /// One line per diagnostic.
    Simple,
    /// A pretty-printed JSON array of LSP-shaped diagnostics.
    Json,
    /// The same array on one line.
    JsonMin,
}

impl CheckFormat {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("pretty") => Ok(Self::Pretty),
            Some("simple") => Ok(Self::Simple),
            Some("json") => Ok(Self::Json),
            Some("json-min") => Ok(Self::JsonMin),
            Some(other) => Err(format!(
                "Unknown check format '{}': expected pretty, simple, json or json-min",
                other
            )),
        }
    }

    fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::JsonMin)
    }
}

/// Settings shared by one `hot check` run and every `hot watch` run.
pub(crate) struct CheckOptions<'a> {
    pub format: CheckFormat,
    pub raw: bool,
    pub deny_warnings: bool,
    pub allow_secret_shape: bool,
    pub conf: &'a Val,
    pub global_options: &'a GlobalOptions,
}

/// Whether check output gets ANSI colors: only in local development, on a
/// terminal, and when `NO_COLOR` is unset.
fn use_color() -> bool {
    use std::io::IsTerminal;
    hot::env::is_local_dev()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::io::stdout().is_terminal()
}

/// Whether a path is, or holds, at least one `.hot` file.
fn has_hot_files(path: &Path) -> bool {
    fn walk(path: &Path, depth: usize) -> bool {
        if path.is_file() {
            return path.extension().and_then(|e| e.to_str()) == Some("hot");
        }
        if depth == 0 {
            return false;
        }
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|e| walk(&e.path(), depth - 1))
            })
            .unwrap_or(false)
    }
    walk(path, 32)
}

struct CheckInputs {
    src_paths: Vec<String>,
    test_paths: Vec<String>,
    project_name: String,
}

fn check_inputs(
    conf: &Val,
    global_options: &GlobalOptions,
    extra_path: Option<&str>,
) -> Result<CheckInputs, String> {
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));
    let mut src_paths = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );
    let include_tests = global_options
        .with_tests
        .unwrap_or_else(|| conf.get_bool("check.with-tests"));
    let test_paths = if include_tests {
        get_merged_test_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.test_paths,
        )
    } else {
        Vec::new()
    };

    // An explicit path is checked even when it is outside the project's src
    // paths. A typo must not pass as "No issues found".
    if let Some(ep) = extra_path {
        let path = Path::new(ep);
        if !path.exists() {
            return Err(format!("Path not found: {}", ep));
        }
        if !has_hot_files(path) {
            return Err(format!("No .hot files found in {}", ep));
        }
        let path_str = if path.is_absolute() {
            ep.to_string()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path).display().to_string())
                .unwrap_or_else(|_| ep.to_string())
        };
        if !src_paths.iter().any(|p| path_str.starts_with(p)) {
            src_paths.push(path_str);
        }
    } else if hot::project::check_project_defined(conf, &project_name).is_ok()
        && !src_paths.iter().any(|p| has_hot_files(Path::new(p)))
    {
        let mut message = format!(
            "No .hot files found in the project's src paths ({})",
            src_paths.join(", ")
        );
        if !Path::new("hot.hot").exists() && Path::new("pkg.hot").exists() {
            message.push_str(
                "\nThis is a package directory (pkg.hot, no hot.hot): pass the package's \
                 source directory, e.g. `hot check src`.",
            );
        }
        return Err(message);
    }

    Ok(CheckInputs {
        src_paths,
        test_paths,
        project_name,
    })
}

fn severity_word(error: &CompilerError) -> &'static str {
    match error.severity().as_lsp_number() {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "hint",
    }
}

/// One `simple` output line: `file:line:col: severity [code] message`.
fn simple_line(error: &CompilerError) -> String {
    let text = error.to_string();
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match error.location() {
        Some(loc) => {
            let file = loc
                .file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<source>".to_string());
            let prefix = format!("{}:{}:{}: ", file, loc.line, loc.column);
            let message = first.strip_prefix(&prefix).unwrap_or(first);
            format!(
                "{}{} [{}] {}",
                prefix,
                severity_word(error),
                error.code(),
                message
            )
        }
        None => format!("{} [{}] {}", severity_word(error), error.code(), first),
    }
}

/// The on-disk file a secret-scan match points at, when it is known.
fn shape_match_file(hit: &SecretMatch, files: &[(String, PathBuf)]) -> Option<String> {
    files
        .iter()
        .find(|(rel, _)| rel == &hit.path)
        .map(|(_, path)| {
            std::path::absolute(path)
                .unwrap_or_else(|_| path.clone())
                .display()
                .to_string()
        })
}

/// A secret-shape match without the matched text: check output goes to
/// terminals, CI logs and editors, so it names the kind and place only.
fn shape_match_message(hit: &SecretMatch) -> String {
    format!(
        "potential secret ({}) in {} at line {}. Allowlist intentional values with \
         hot.build.allow-secret-shape in hot.hot, or rotate the credential and \
         read it from ::hot::ctx.",
        hit.kind, hit.path, hit.line
    )
}

/// The pretty report of secret-shape matches, without their text.
fn shape_scan_report(hits: &[SecretMatch]) -> String {
    let mut out = format!(
        "found {} potential secret(s) in bundled content:\n",
        hits.len()
    );
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}. {} (line {}) — {}\n",
            i + 1,
            hit.path,
            hit.line,
            hit.kind
        ));
    }
    out.push_str(
        "\nIf these are intentional (test fixtures, example configs, public keys, …), \
         allowlist them in hot.hot:\n  hot.build.allow-secret-shape [\"path/to/file\"]\n\
         …or pass `--allow-secret-shape` to skip the scan for this check.\n\
         Otherwise: rotate the credential and remove it from the bundle. Production \
         secrets belong in `::hot::ctx`, not in source.",
    );
    out
}

fn shape_match_diagnostic(hit: &SecretMatch, files: &[(String, PathBuf)]) -> Diagnostic {
    let line = hit.line.saturating_sub(1) as u32;
    Diagnostic {
        file: shape_match_file(hit, files),
        range: DiagnosticRange {
            start: DiagnosticPosition { line, character: 0 },
            end: DiagnosticPosition { line, character: 0 },
        },
        severity: 1,
        code: "secret-shape".to_string(),
        source: "hot".to_string(),
        message: shape_match_message(hit),
    }
}

/// Run one check and print its report. Returns the exit code (0 or 1); an
/// `Err` means the check could not run (exit 2).
pub(crate) fn run_check_once(
    options: &CheckOptions<'_>,
    context_storage: Option<&ahash::AHashMap<String, Val>>,
    extra_path: Option<&str>,
) -> Result<i32, String> {
    tracing::debug!("Checking project sources");
    let conf = options.conf;
    let inputs = check_inputs(conf, options.global_options, extra_path)?;

    // The same scan, allowlist and override as `hot build`. It runs after the
    // type check, so its findings are reported next to the real diagnostics.
    let run_scan = || {
        scan_cli_inputs(
            conf,
            options.global_options,
            &inputs.src_paths,
            options.allow_secret_shape,
        )
    };

    if options.format == CheckFormat::Pretty {
        let color = use_color();
        let mut warnings = CompilerErrors::new();
        let result = hot::lang::engine::Engine::check_sources_pipeline_with_context_warnings(
            &inputs.src_paths,
            &inputs.test_paths,
            Some(conf),
            Some(&inputs.project_name),
            context_storage,
            color,
            Some(&mut warnings),
        );
        let (hits, _) = run_scan()?;
        let had_warnings = warnings.has_warnings();
        let failed = match result {
            Ok(()) => {
                if had_warnings {
                    println!("{}", warnings.format_warnings(color));
                }
                false
            }
            Err(e) => {
                println!("{}", e);
                true
            }
        };
        if !hits.is_empty() {
            println!("{}", shape_scan_report(&hits));
        }
        let failed = failed || !hits.is_empty();
        if !failed && !had_warnings && !options.raw {
            println!("No issues found");
        }
        return Ok(if failed || (had_warnings && options.deny_warnings) {
            1
        } else {
            0
        });
    }

    // Structured paths (simple and JSON) share the diagnostics pipeline.
    // Missing context variables are reported as `missing-context` warnings
    // (fatal with `--check.deny-warnings`).
    let available_ctx_keys: ahash::AHashSet<String> = context_storage
        .map(|cs| cs.keys().cloned().collect())
        .unwrap_or_default();
    let errors = hot::lang::engine::Engine::check_sources_pipeline_diagnostics_with_ctx(
        &inputs.src_paths,
        &inputs.test_paths,
        Some(conf),
        Some(&inputs.project_name),
        Some(&available_ctx_keys),
    );
    let (hits, files) = run_scan()?;
    let fail =
        !errors.is_empty() || !hits.is_empty() || (options.deny_warnings && errors.has_warnings());

    if options.format == CheckFormat::Simple {
        for error in errors.errors.iter().chain(errors.warnings.iter()) {
            println!("{}", simple_line(error));
        }
        for hit in &hits {
            let file = shape_match_file(hit, &files).unwrap_or_else(|| hit.path.clone());
            println!(
                "{}:{}:1: error [secret-shape] {}",
                file,
                hit.line,
                shape_match_message(hit)
            );
        }
        if !fail && !errors.has_warnings() && !options.raw {
            println!("No issues found");
        }
        return Ok(if fail { 1 } else { 0 });
    }

    let mut diagnostics = errors.to_diagnostics();
    diagnostics.extend(hits.iter().map(|hit| shape_match_diagnostic(hit, &files)));
    let serialized = if options.format == CheckFormat::JsonMin {
        serde_json::to_string(&diagnostics)
    } else {
        serde_json::to_string_pretty(&diagnostics)
    }
    .unwrap_or_else(|_| "[]".to_string());
    println!("{}", serialized);
    Ok(if fail { 1 } else { 0 })
}

/// `hot watch`: check once, then again after every change to a .hot file
/// under the watched paths. Runs until interrupted.
pub(crate) async fn run_check_watch(
    options: &CheckOptions<'_>,
    debounce_ms: u64,
) -> Result<i32, String> {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    let conf = options.conf;
    let global_options = options.global_options;
    let mut last_run = Instant::now();

    // A run that cannot start (for example a project with no .hot files yet)
    // is reported and watching continues.
    let run = || {
        if let Err(e) = run_check_once(options, None, None) {
            if options.format.is_json() {
                println!("[]");
            }
            eprintln!("Check failed: {}", e);
        }
    };
    run();

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher =
        Watcher::new(tx, notify::Config::default()).map_err(|e| e.to_string())?;
    let include_tests = global_options
        .with_tests
        .unwrap_or_else(|| conf.get_bool("watch.with-tests"));

    let mut watched = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );
    if include_tests {
        watched.extend(get_merged_test_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.test_paths,
        ));
    }
    for p in watched {
        let pb = PathBuf::from(p);
        if pb.exists() {
            watcher
                .watch(&pb, RecursiveMode::Recursive)
                .map_err(|e| e.to_string())?;
        }
    }

    let debounce = Duration::from_millis(debounce_ms.max(50));
    let mut pending = false;

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                // filter to .hot files only
                let is_hot = event
                    .paths
                    .iter()
                    .any(|p| p.extension().and_then(|e| e.to_str()) == Some("hot"));
                if is_hot {
                    match event.kind {
                        EventKind::Modify(_)
                        | EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Any => {
                            pending = true;
                        }
                        _ => {}
                    }
                }
                if pending && last_run.elapsed() >= debounce {
                    pending = false;
                    last_run = Instant::now();
                    run();
                }
            }
            Ok(Err(_e)) => {
                // Watch error - ignore and continue
            }
            Err(_e) => {
                // Channel closed or error - exit gracefully
                return Ok(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_format_parses_known_values_and_rejects_others() {
        assert_eq!(CheckFormat::parse(None).unwrap(), CheckFormat::Pretty);
        assert_eq!(
            CheckFormat::parse(Some(" Simple ")).unwrap(),
            CheckFormat::Simple
        );
        assert_eq!(
            CheckFormat::parse(Some("json-min")).unwrap(),
            CheckFormat::JsonMin
        );
        assert!(CheckFormat::parse(Some("jsonl")).is_err());
    }

    #[test]
    fn shape_scan_output_never_contains_the_matched_text() {
        let hit = SecretMatch {
            path: "hot/src/a.hot".to_string(),
            kind: "aws-access-key",
            line: 3,
            snippet: "key \"AKIA***MPLE\"".to_string(),
        };
        let message = shape_match_message(&hit);
        let report = shape_scan_report(std::slice::from_ref(&hit));
        for text in [&message, &report] {
            assert!(!text.contains("AKIA"), "{text}");
            assert!(text.contains("aws-access-key"), "{text}");
        }
        assert!(message.contains("hot/src/a.hot at line 3"), "{message}");
    }

    #[test]
    fn simple_line_keeps_location_first() {
        let error = CompilerError::UnresolvedVariable {
            var_name: "x".to_string(),
            namespace: "::app".to_string(),
            message: "Variable 'x' cannot be resolved".to_string(),
            location: Some(hot::lang::errors::ErrorLocation {
                line: 3,
                column: 5,
                position: 0,
                length: 1,
                file: Some(PathBuf::from("/p/a.hot")),
            }),
        };
        assert_eq!(
            simple_line(&error),
            "/p/a.hot:3:5: error [unresolved-variable] Variable 'x' cannot be resolved"
        );
    }
}
