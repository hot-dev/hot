//! The project the server analyzes: its root, configuration and paths.
//!
//! The root comes from the client (`workspaceFolders`, then `rootUri`), not
//! from the server's working directory, and a configuration that fails to
//! load is an error to report on `hot.hot`, not a reason to stop.

use ahash::AHashSet;
use hot::val::Val;
use std::path::{Path, PathBuf};

/// The configuration file that marks a project root.
pub const CONF_FILE: &str = "hot.hot";
/// The context file `hot check` reads context keys from.
pub const CTX_FILE: &str = "hot/ctx.hot";

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    /// Whether `root` has a `hot.hot`.
    pub has_conf: bool,
    pub conf: Val,
    /// Why `hot.hot` failed to load, if it did.
    pub conf_error: Option<String>,
    /// The project to check, when there is a `hot.hot`.
    pub project_name: Option<String>,
    /// Absolute source and test paths of the project.
    pub src_paths: Vec<String>,
    pub test_paths: Vec<String>,
    /// Context keys `hot/ctx.hot` sets, for `missing-context` warnings.
    pub ctx_keys: Option<AHashSet<String>>,
}

impl Workspace {
    /// No project yet (before the client names a root).
    pub fn empty(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            has_conf: false,
            conf: resolve_conf(default_conf(), false),
            conf_error: None,
            project_name: None,
            src_paths: Vec::new(),
            test_paths: Vec::new(),
            ctx_keys: Some(AHashSet::new()),
        }
    }

    /// Load the project at `root`.
    pub fn load(root: &Path) -> Self {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let conf_path = root.join(CONF_FILE);
        let has_conf = conf_path.is_file();

        let mut conf = default_conf();
        let mut conf_error = None;
        if has_conf {
            match load_conf_file(&conf_path) {
                Ok(file_conf) => conf = conf.merge(&file_conf),
                Err(e) => conf_error = Some(e),
            }
        }
        let conf = resolve_conf(conf, has_conf);

        let (project_name, src_paths, test_paths) = if has_conf {
            let project = hot::project::get_default_project_name(&conf);
            let absolute = |paths: Vec<String>| -> Vec<String> {
                paths
                    .into_iter()
                    .map(|p| absolute_under(&root, &p).display().to_string())
                    .collect()
            };
            let src = absolute(hot::project::get_project_src_paths(&conf, &project));
            // `hot check` type-checks tests unless `check.with-tests` is off.
            let test = if conf.get_bool_or_default("check.with-tests", true) {
                absolute(hot::project::get_project_test_paths(&conf, &project))
            } else {
                Vec::new()
            };
            (Some(project), src, test)
        } else {
            (None, Vec::new(), Vec::new())
        };

        let ctx_path = root.join(CTX_FILE);
        let ctx_keys = if ctx_path.is_file() {
            load_ctx_keys(&ctx_path)
        } else {
            Some(AHashSet::new())
        };

        Self {
            root,
            has_conf,
            conf,
            conf_error,
            project_name,
            src_paths,
            test_paths,
            ctx_keys,
        }
    }

    pub fn conf_path(&self) -> PathBuf {
        self.root.join(CONF_FILE)
    }

    /// Whether a file change should reload the configuration.
    pub fn is_config_file(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        match name {
            "hot.hot" | "pkg.hot" => true,
            "ctx.hot" => path.parent().is_some_and(|p| p.ends_with("hot")),
            _ => false,
        }
    }
}

/// The nearest directory at or above `start` that has a `hot.hot`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        Some(start)
    } else {
        start.parent()
    };
    while let Some(d) = dir {
        if d.join(CONF_FILE).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn absolute_under(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    // Normalize `.` and `..` lexically: the path may not exist yet.
    let mut normalized = PathBuf::new();
    for component in joined.components() {
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

fn default_conf() -> Val {
    let mut conf = Val::map_empty();
    let cache_conf = hot::cache::get_resolved_conf(Val::map_empty());
    conf = conf.set("cache", cache_conf);
    conf
}

/// Apply the defaults the CLI applies, so the server checks with the same
/// settings `hot check` uses.
fn resolve_conf(mut conf: Val, in_project: bool) -> Val {
    let project_conf = hot::project::get_resolved_conf(conf.clone());
    conf = conf.merge(&project_conf);
    conf = hot::check::get_resolved_conf(conf);
    let emitter = conf.get("emitter").unwrap_or_else(Val::map_empty);
    conf = conf.set(
        "emitter",
        hot::lang::emitter::get_resolved_conf(emitter, in_project),
    );
    let queue = conf.get("queue").unwrap_or_else(Val::map_empty);
    conf = conf.set("queue", hot::queue::get_resolved_conf(queue, in_project));
    conf
}

/// Run Hot code for configuration with stdout captured: stdout is the LSP
/// transport, so a `println` in `hot.hot` must not reach it.
fn eval_quietly(code: &str) -> Result<hot::lang::engine::ExecutedEngine, String> {
    hot::lang::hot::io::start_capture_stdout();
    let result = std::panic::catch_unwind(|| {
        hot::lang::engine::Engine::execute_eval_and_retain_state(code, &[], &[], None, None, false)
    });
    let _ = hot::lang::hot::io::release_capture_stdout();
    match result {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err("evaluation panicked".to_string()),
    }
}

/// Evaluate `hot.hot` and return its `hot` value.
fn load_conf_file(path: &Path) -> Result<Val, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    if content.trim().is_empty() {
        return Ok(Val::map_empty());
    }
    // A syntax error is reported with its location by the diagnostics for
    // the file itself.
    if let Err(e) = hot::lang::parser::parse_hot_file(&content, &path.to_string_lossy()) {
        return Err(e.message.clone());
    }
    let code = if content.contains("::hot::conf ns") {
        content
    } else {
        format!("::hot::conf ns\n\n{}", content)
    };
    let engine = eval_quietly(&code)?;
    Ok(engine
        .extract_var(Some("::hot::conf"), "hot")
        .unwrap_or_else(Val::map_empty))
}

/// The context keys `hot/ctx.hot` sets. `None` when it fails to run: then
/// no `missing-context` warnings are reported, rather than one per key.
fn load_ctx_keys(path: &Path) -> Option<AHashSet<String>> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return Some(AHashSet::new());
    }
    let code = if content.contains("::hot::run::ctx ns") {
        content
    } else {
        format!("::hot::run::ctx ns\n\n{}", content)
    };
    let engine = eval_quietly(&code).ok()?;
    Some(engine.extract_context_storage().keys().cloned().collect())
}
