//! `hot check` without the runtime.
//!
//! A check discovers the project's compilation units (hot-std, the resolved
//! dependencies, the source and test paths), parses them, resolves names,
//! compiles, and validates handlers, schedules, `meta`, context and box
//! requirements. Nothing executes.
//!
//! * [`check_project`] returns the diagnostics `hot check --check.format json`
//!   prints, as [`CompilerErrors`] (errors plus warnings, each with its code
//!   and location).
//! * [`check_project_text`] is the pretty `hot check`, `hot watch` and the
//!   `hot dev` pre-check: errors come back as one formatted message.
//!
//! `hot` runs both through its `Engine::check_sources_pipeline_*` functions,
//! adding its on-disk unit cache and its hot-std lookup.

use crate::lang::ast::{NsPath, Program};
use crate::lang::cache::unit::{CompilationUnit, UnitStore};
use crate::lang::compiler::Compiler;
use crate::lang::compiler::ctx_checker::ProgramCtxRequirements;
use crate::lang::discover::{
    DiscoveredUnit, discover_compilation_units_with, parse_units_with_cache,
};
use crate::lang::errors::{CompilerError, CompilerErrors, ErrorLocation};
use crate::lang::hot_std::HotStd;
use crate::lang::project::DependencyResolver;
use crate::val::Val;
use ahash::AHashSet;

/// What a check compiles against and reports on.
#[derive(Clone, Default)]
pub struct CheckOptions<'a> {
    /// The resolved conf (what `hot.hot` binds to `hot`). The check reads
    /// `project.<name>.deps` and `schedule.min-interval-seconds` from it.
    pub conf: Option<&'a Val>,
    /// The project whose dependencies to resolve.
    pub project_name: Option<&'a str>,
    /// The context keys that have values. [`check_project`] reports each
    /// required key missing from them as a `missing-context` warning; without
    /// them it skips that check. [`check_project_text`] treats `None` as no
    /// keys.
    pub available_ctx_keys: Option<&'a AHashSet<String>>,
    /// The hot-std to compile against.
    pub hot_std: HotStd,
    /// Where parsed units are cached between runs. Without one every unit is
    /// parsed each time.
    pub unit_store: Option<&'a dyn UnitStore>,
}

impl CheckOptions<'_> {
    fn resolver(&self) -> Result<DependencyResolver, String> {
        Ok(DependencyResolver::new(self.hot_std.dir()?))
    }

    fn discover(
        &self,
        src_paths: &[String],
        test_paths: &[String],
    ) -> Result<Vec<DiscoveredUnit>, String> {
        discover_compilation_units_with(
            self.conf,
            self.project_name,
            src_paths,
            test_paths,
            &self.resolver()?,
        )
    }
}

/// Explicit policy for required runtime context validation.
///
/// This is intentionally separate from `Option<context_storage>`: an absent
/// storage map can mean either "collect requirements only" or "runtime has
/// zero keys", depending on the caller.
#[derive(Debug, Clone)]
pub enum ContextValidationPolicy {
    /// Resolve the requirements but don't fail on unset values.
    CollectOnly,
    /// Fail unless every required key is among these.
    RequireSatisfied(AHashSet<String>),
}

/// Check a project and return its diagnostics: the errors and warnings
/// `hot check --check.format json` prints. A failure that has no source
/// location (a dependency that doesn't resolve, say) is a single
/// `InvalidFunctionCall` with `func_name = "<pipeline>"` and no location.
pub fn check_project(
    src_paths: &[String],
    test_paths: &[String],
    options: &CheckOptions,
) -> CompilerErrors {
    fn synthesize(message: String) -> CompilerErrors {
        let mut errs = CompilerErrors::new();
        errs.add(CompilerError::InvalidFunctionCall {
            func_name: "<pipeline>".to_string(),
            message,
            location: None,
        });
        errs
    }

    // Phase 1: Discover compilation units
    let units = match options.discover(src_paths, test_paths) {
        Ok(u) => u,
        Err(msg) => return synthesize(msg),
    };

    // Phase 2: Parse units (errors come back pre-formatted from the parser)
    let (namespaces, parsed_files) = match parse_units_with_cache(&units, false, options.unit_store)
    {
        Ok(pair) => pair,
        Err(_msg) => {
            // Re-parse each file directly to recover structured per-file
            // errors with line/column information. Slower than the cached
            // pipeline but only runs on the failure path.
            let mut errs = CompilerErrors::new();
            for unit in &units {
                for file_path in &unit.files {
                    let content = match std::fs::read_to_string(file_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if let Err(parse_err) = crate::lang::parser::parse_hot_file(&content, file_path)
                    {
                        errs.add_source(file_path.clone(), content.clone());
                        // Use the raw message — the outer ariadne renderer
                        // in `to_diagnostics()` will wrap it with the
                        // snippet using the location we attach below, so
                        // we don't want to embed a second pre-rendered
                        // ariadne report inside it.
                        let location = ErrorLocation {
                            line: parse_err.location.line,
                            column: parse_err.location.column,
                            position: parse_err.location.position,
                            length: parse_err.location.length.max(1),
                            file: parse_err
                                .location
                                .file
                                .clone()
                                .or_else(|| Some(std::path::PathBuf::from(file_path))),
                        };
                        let code = parse_err.code();
                        if code == "parse-error" {
                            errs.add(CompilerError::InvalidFunctionCall {
                                func_name: "<parse>".to_string(),
                                message: parse_err.message.clone(),
                                location: Some(location),
                            });
                        } else {
                            // A `where` misuse keeps its own code, as the
                            // constraint diagnostics do.
                            errs.add(CompilerError::Constraint {
                                code,
                                message: parse_err.message.clone(),
                                help: None,
                                warning: false,
                                location: Some(location),
                            });
                        }
                    }
                }
            }
            if errs.is_empty() {
                // Couldn't pin down a specific file — fall back to the
                // pre-formatted aggregate string.
                return synthesize(_msg);
            }
            return errs;
        }
    };

    // Phase 1.5: Resolve variable references
    let mut combined_program = Program {
        namespaces,
        current_namespace: NsPath::hot_main(),
    };
    crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

    // Phase 2: Compile + validate
    let mut compiler = Compiler::new();
    for parsed in &parsed_files {
        compiler.add_source_file(
            std::path::PathBuf::from(&parsed.path),
            parsed.content.clone(),
        );
    }

    let compile_result = compiler.compile_program(&mut combined_program);
    // Capture non-fatal warnings (deprecated-API usage), scoped to
    // project-owned files so dependency/stdlib code never adds noise.
    let scoped_warnings = {
        let project_files = project_source_files(&units);
        scope_warnings_to_project(compiler.take_diagnostics(), &project_files)
    };
    // The same handler and schedule validations the text check runs in
    // `validate_compiled_program`, collected as diagnostics.
    let compile_result = compile_result.and_then(|()| {
        compiler
            .extract_event_handlers(&combined_program)
            .and_then(|()| compiler.extract_scheduled_functions(&combined_program))
    });
    let mut errors = match compile_result {
        Ok(()) => CompilerErrors::new(),
        Err(mut errors) => {
            // Make sure source content is attached so diagnostics can
            // render rich messages via ariadne in `to_diagnostics()`.
            // `parsed_files` only contains freshly-parsed (cache-miss)
            // files, so on subsequent runs cached source files won't be
            // in the cache. Lazy-load any file referenced by an error
            // location that isn't already cached.
            for parsed in &parsed_files {
                errors.add_source(parsed.path.clone(), parsed.content.clone());
            }
            let referenced_paths: AHashSet<String> = errors
                .errors
                .iter()
                .filter_map(|e| {
                    e.location()
                        .and_then(|l| l.file.as_ref())
                        .map(|f| f.display().to_string())
                })
                .collect();
            for path in referenced_paths {
                if errors.source_cache.contains_key(&path) {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    errors.add_source(path, content);
                }
            }
            errors
        }
    };

    // Optional ctx-requirements check. Each missing key becomes a
    // `missing-context` warning with a file-level location (meta
    // declarations don't carry per-key spans yet).
    if let Some(available) = options.available_ctx_keys {
        let call_graph = crate::lang::compiler::call_graph::CallGraph::build(&combined_program);
        let ctx_requirements = call_graph.resolve_user_ctx_requirements(&combined_program);
        if ctx_requirements.has_requirements() {
            let result = crate::lang::compiler::ctx_checker::check_ctx_requirements(
                &ctx_requirements,
                available,
            );
            for missing in &result.missing {
                errors.add_warning(missing.to_compiler_error());
            }
        }
    }

    // Merge in the deprecation warnings. They ride alongside errors so
    // `to_diagnostics()` emits them (severity 2), but never affect
    // `is_empty()` / the success exit code.
    errors.warnings.extend(scoped_warnings.warnings);
    for (path, content) in scoped_warnings.source_cache {
        errors.add_source(path, content);
    }

    errors
}

/// Check a project the way the pretty `hot check` does: errors come back as
/// one formatted message (`color` adds ANSI colors).
///
/// With `warnings_out`, project-scoped warnings (deprecated API usage) are
/// collected into it, and required context values missing from
/// `options.available_ctx_keys` are `missing-context` warnings rather than a
/// failed check. Without it (the `hot dev` pre-check), a missing context
/// value fails the check.
pub fn check_project_text(
    src_paths: &[String],
    test_paths: &[String],
    options: &CheckOptions,
    color: bool,
    mut warnings_out: Option<&mut CompilerErrors>,
) -> Result<(), String> {
    let units = options.discover(src_paths, test_paths)?;
    let (namespaces, parsed_files) = parse_units_with_cache(&units, color, options.unit_store)?;

    let mut combined_program = Program {
        namespaces,
        current_namespace: NsPath::hot_main(),
    };
    crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

    let mut compiler = Compiler::new();
    for parsed in &parsed_files {
        compiler.add_source_file(
            std::path::PathBuf::from(&parsed.path),
            parsed.content.clone(),
        );
    }
    compile_program_checked(
        &mut compiler,
        &mut combined_program,
        &units,
        color,
        warnings_out.as_deref_mut(),
    )?;

    validate_checked_program(
        &mut compiler,
        &combined_program,
        options.conf,
        options.available_ctx_keys,
        color,
        warnings_out,
    )
}

/// Compile a merged program, failing on compile errors and on calls to
/// missing native functions. On success, `warnings_out` receives the
/// compiler's warnings for project-owned files.
pub fn compile_program_checked(
    compiler: &mut Compiler,
    combined_program: &mut Program,
    units: &[DiscoveredUnit],
    color: bool,
    warnings_out: Option<&mut CompilerErrors>,
) -> Result<(), String> {
    // Try comprehensive compilation with validation first
    match compiler.compile_program(combined_program) {
        Ok(()) => {
            // Compilation with validation succeeded. Capture non-fatal
            // deprecation warnings for the caller (scoped to project files)
            // without failing the check.
            if let Some(out) = warnings_out {
                let project_files = project_source_files(units);
                *out = scope_warnings_to_project(compiler.take_diagnostics(), &project_files);
            }
        }
        Err(errors) => {
            let formatted = errors.format_error(color);
            let error_message = String::from("Compile-time errors found:\n") + &formatted;
            return Err(error_message);
        }
    }

    // Also check for call-lib validation errors (legacy support)
    let call_lib_errors = compiler.get_call_lib_errors();
    if !call_lib_errors.is_empty() {
        // Show all missing functions at once!
        let error_message = if call_lib_errors.len() == 1 {
            String::from("Compile-time error: ") + &call_lib_errors[0]
        } else {
            let formatted_errors = call_lib_errors
                .iter()
                .enumerate()
                .map(|(i, err)| format!("  {}. {}", i + 1, err))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "Compile-time errors found ({} missing functions):\n{}",
                call_lib_errors.len(),
                formatted_errors
            )
        };
        return Err(error_message);
    }

    tracing::debug!(" Compilation successful!");
    Ok(())
}

/// Check mode's validation of a compiled program (see
/// [`check_project_text`] for how `warnings_out` changes the context policy).
pub fn validate_checked_program(
    compiler: &mut Compiler,
    combined_program: &Program,
    conf: Option<&Val>,
    available_ctx_keys: Option<&AHashSet<String>>,
    color: bool,
    warnings_out: Option<&mut CompilerErrors>,
) -> Result<(), String> {
    // A caller that collects warnings (`hot check`, `hot watch`) gets unset
    // required context values as warnings instead of a failed check: values
    // are supplied per environment, and `run`/`eval` still refuse to start
    // without them. Callers without a warnings sink (the `hot dev`
    // pre-check) keep the strict policy.
    let context_policy = if warnings_out.is_some() {
        ContextValidationPolicy::CollectOnly
    } else {
        ContextValidationPolicy::RequireSatisfied(available_ctx_keys.cloned().unwrap_or_default())
    };
    let ctx_requirements =
        validate_compiled_program(compiler, combined_program, conf, context_policy, color)?;

    if let Some(out) = warnings_out
        && ctx_requirements.has_requirements()
    {
        let available = available_ctx_keys.cloned().unwrap_or_default();
        let ctx_result = crate::lang::compiler::ctx_checker::check_ctx_requirements(
            &ctx_requirements,
            &available,
        );
        for missing in &ctx_result.missing {
            out.add_warning(missing.to_compiler_error());
        }
    }

    tracing::debug!(" Check completed successfully");
    Ok(())
}

/// Post-compile structural validations shared by build, check, and execute
/// modes, with an explicit policy for runtime context satisfaction.
///
/// These are cheap passes over the already-compiled program. Running them
/// in Execute mode as well gives `run`/`eval` the same pre-flight failures
/// `check` reports — before any user code executes — without paying for
/// the separate check pipeline's second full compile.
pub fn validate_compiled_program(
    compiler: &mut Compiler,
    combined_program: &Program,
    conf: Option<&Val>,
    context_policy: ContextValidationPolicy,
    color: bool,
) -> Result<ProgramCtxRequirements, String> {
    // Validate event handlers (event parameter requirement) and every
    // function's `timeout` meta
    if let Err(e) = compiler.extract_event_handlers(combined_program) {
        let formatted = e.format_error(color);
        return Err(String::from("Function validation errors:\n") + &formatted);
    }

    // Validate scheduled functions (event parameter requirement)
    if let Err(e) = compiler.extract_scheduled_functions(combined_program) {
        let formatted = e.format_error(color);
        return Err(String::from("Scheduled function validation errors:\n") + &formatted);
    }
    if let Some(conf) = conf {
        let min_interval_secs = crate::lang::schedule::min_recurring_interval_secs(conf);
        for cron_expression in compiler.get_scheduled_functions().keys() {
            crate::lang::schedule::validate_recurring_schedule_interval(
                cron_expression,
                min_interval_secs,
            )
            .map_err(|e| format!("Schedule policy error: {}", e.message()))?;
        }
    }

    // Build call graph once for both ctx and box requirement resolution
    let call_graph = crate::lang::compiler::call_graph::CallGraph::build(combined_program);

    // Check context requirements using call graph resolution
    // This only includes ctx requirements reachable from user code.
    let ctx_requirements = call_graph.resolve_user_ctx_requirements(combined_program);

    if ctx_requirements.has_requirements()
        && let ContextValidationPolicy::RequireSatisfied(available_keys) = context_policy
    {
        let ctx_result = crate::lang::compiler::ctx_checker::check_ctx_requirements(
            &ctx_requirements,
            &available_keys,
        );

        if !ctx_result.is_ok() {
            return Err(ctx_result.format_errors());
        }

        tracing::debug!(
            " Context requirements satisfied: {} required keys found",
            ctx_result.found_keys.len()
        );
    }

    // Check box requirements: validate size names and report
    let box_requirements = call_graph.resolve_user_box_requirements(combined_program);

    if box_requirements.has_requirements() {
        // Validate all declared size names are recognized
        for req in &box_requirements.requirements {
            if let Some(ref size) = req.requirement.min_size
                && crate::lang::compiler::box_checker::size_memory_mb(size).is_none()
            {
                return Err(format!(
                    "Invalid box size '{}' in meta {{box: {{min-size: \"{}\"}}}} on {}\n\
                     Valid sizes: nano, micro, small, medium, large, xlarge, 2xlarge, 4xlarge",
                    size, size, req.fqn
                ));
            }
        }

        if let Some(min_size) = box_requirements.effective_min_size() {
            tracing::debug!(
                " Box requirements: min-size={}, network={}, from {} function(s)",
                min_size,
                box_requirements.requires_network(),
                box_requirements.requirements.len()
            );
        }
    }

    // Hand the resolved ctx requirements back so Execute mode can apply
    // defaults and secret keys without rebuilding the call graph.
    Ok(ctx_requirements)
}

/// Collect the set of project-owned source file paths (i.e. files from
/// `SourcePath` units, not dependency `Package` units). Used to scope
/// deprecation warnings to first-party code so a deprecated call buried in
/// a dependency or in `hot-std` never produces noise the user can't fix.
pub fn project_source_files(units: &[DiscoveredUnit]) -> AHashSet<String> {
    let mut set = AHashSet::new();
    for u in units {
        if matches!(u.unit, CompilationUnit::SourcePath { .. }) {
            for f in &u.files {
                // Store both the raw and canonicalized path. The discovered
                // path (often relative, e.g. `./hot/src/app.hot`) may differ
                // from the path stored in the cached AST (absolute /
                // canonicalized), so we match against either form.
                set.insert(f.clone());
                if let Ok(canon) = std::fs::canonicalize(f) {
                    set.insert(canon.display().to_string());
                }
            }
        }
    }
    set
}

/// Keep only warnings whose call site lives in a project-owned file.
/// Preserves the source cache so the retained warnings can still render
/// rich ariadne snippets.
pub fn scope_warnings_to_project(
    diagnostics: CompilerErrors,
    project_files: &AHashSet<String>,
) -> CompilerErrors {
    let mut out = CompilerErrors::new();
    out.source_cache = diagnostics.source_cache;
    for w in diagnostics.warnings {
        let file = w.location().and_then(|l| l.file.as_ref()).cloned();
        let keep = file
            .as_ref()
            .map(|f| {
                let raw = f.display().to_string();
                if project_files.contains(&raw) {
                    return true;
                }
                match std::fs::canonicalize(f) {
                    Ok(canon) => project_files.contains(&canon.display().to_string()),
                    Err(_) => false,
                }
            })
            .unwrap_or(false);
        if keep {
            // Ensure the warning's source is cached so it renders with a
            // rich ariadne snippet rather than the plain Display fallback.
            if let Some(f) = &file {
                let key = f.display().to_string();
                if !out.source_cache.contains_key(&key)
                    && let Ok(content) = std::fs::read_to_string(f)
                {
                    out.add_source(key, content);
                }
            }
            out.warnings.push(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// This checkout's hot-std, which is what the embedded copy holds.
    fn checkout_std() -> HotStd {
        HotStd::Dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg/hot-std"))
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn check(src_dir: &Path, conf: Option<&Val>) -> CompilerErrors {
        check_project(
            &[src_dir.display().to_string()],
            &[],
            &CheckOptions {
                conf,
                project_name: conf.map(|_| "app"),
                hot_std: checkout_std(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_program_checks_against_hot_std_without_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write(
            &src.join("app.hot"),
            "::app ns\n\nmain fn (): Int { add(1, 2) }\n",
        );
        let errors = check(&src, None);
        assert!(
            errors.is_empty() && !errors.has_warnings(),
            "{:?}",
            errors.to_diagnostics()
        );
    }

    #[test]
    fn errors_carry_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write(
            &src.join("app.hot"),
            "::app ns\n\nmain fn (): Int { no-such-fn(1) }\n",
        );
        let diagnostics = check(&src, None).to_diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == 1
                    && d.file.as_deref().is_some_and(|f| f.ends_with("app.hot"))),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_local_package_dependency_resolves_from_the_conf() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        write(
            &lib.join("pkg.hot"),
            "::hot::pkg ns\n\nhot.pkg.lib {\n  name: \"example.com/lib\",\n  version: \"0.1.0\",\n  deps: {},\n  src-paths: [\"src/\"],\n}\n",
        );
        write(
            &lib.join("src/lib.hot"),
            "::lib ns\n\ngreet fn (name: Str): Str { `hello ${name}` }\n",
        );
        let src = dir.path().join("src");
        write(
            &src.join("app.hot"),
            "::app ns\n\nmain fn (): Str { ::lib/greet(\"x\") }\n",
        );

        let conf = crate::lang::data::eval_data(&format!(
            "hot.project.app.deps {{\"example.com/lib\": {{local: {:?}}}}}\n",
            lib.display().to_string()
        ))
        .unwrap();
        let errors = check(&src, Some(&conf));
        assert!(errors.is_empty(), "{:?}", errors.to_diagnostics());

        // Without the dependency, the call doesn't resolve.
        assert!(!check(&src, None).is_empty());
    }
}
