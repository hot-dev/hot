use crate::val;
use crate::val::Val;
use ahash::AHashSet;

pub fn get_resolved_conf(conf: Val) -> Val {
    let resolved_conf = conf.clone();

    // Get the default project name from configuration, fallback to "my-hot-project"
    let default_project_name = resolved_conf
        .get("set")
        .and_then(|default| default.get("project"))
        .map(|project| match project {
            Val::Str(s) => (*s).to_string(),
            _ => project.to_string().trim_matches('"').to_string(),
        })
        .unwrap_or_else(|| "my-hot-project".to_string());

    // Check if there are existing projects
    let existing_projects = resolved_conf.get("project");

    let projects_conf = if let Some(existing_projects) = existing_projects {
        // If there are existing projects, only create the default "my-hot-project" if:
        // 1. The default project name is not "my-hot-project" (user has explicitly set a different default)
        // 2. OR the default project name doesn't exist in existing projects
        if default_project_name != "my-hot-project" {
            // User has explicitly set a different default project - create it if it doesn't exist
            if existing_projects.get(&default_project_name).is_none() {
                let default_project_conf = create_default_project_conf(&default_project_name);
                default_project_conf.merge(&existing_projects)
            } else {
                existing_projects.clone()
            }
        } else {
            // Default project name is "my-hot-project" and there are existing projects
            // Don't create the default fallback project - use existing projects as-is
            existing_projects.clone()
        }
    } else {
        // No existing projects - create the default project as fallback
        create_default_project_conf(&default_project_name)
    };

    // Create the full configuration structure
    let mut full_conf = val!({
        "project": projects_conf,
        "set": {
            "project": default_project_name
        }
    });

    // Merge with existing configuration
    full_conf = full_conf.merge(&resolved_conf);

    full_conf
}

fn create_default_project_conf(project_name: &str) -> Val {
    // Always use relative paths for better portability and readability
    let default_src_path = "./hot/src".to_string();
    let default_test_path = "./hot/test".to_string();
    let default_resource_path = "./hot/resources".to_string();

    // Create the default project configuration with deps format
    let project_conf = val!({
        "src": {
            "paths": [default_src_path]
        },
        "test": {
            "paths": [default_test_path],
            "capture": true
        },
        "resources": {
            "paths": [default_resource_path],
            "excludes": []
        },
        "ignore": {
            "respect-gitignore": true,
            "excludes": []
        },
        "deps": {}
    });

    // Return with the project name as the key
    val!({
        project_name: project_conf
    })
}

/// Get the project configuration for a specific project name
pub fn get_project_conf(conf: &Val, project_name: &str) -> Option<Val> {
    conf.get("project")
        .and_then(|projects| projects.get(project_name))
}

/// Get the default project name from configuration
pub fn get_default_project_name(conf: &Val) -> String {
    conf.get("set")
        .and_then(|set| set.get("project"))
        .map(|project| match project {
            Val::Str(s) => (*s).to_string(),
            _ => project.to_string().trim_matches('"').to_string(),
        })
        .unwrap_or_else(|| "my-hot-project".to_string())
}

/// Get paths for a specific project and path type (src, test, pkg)
pub fn get_project_paths(conf: &Val, project_name: &str, path_type: &str) -> Vec<String> {
    get_project_conf(conf, project_name)
        .and_then(|project_conf| project_conf.get(path_type))
        .and_then(|path_conf| path_conf.get("paths"))
        .map(|paths| match paths {
            Val::Vec(vec) => vec
                .iter()
                .map(|v| match v {
                    Val::Str(s) => (*s).to_string(),
                    _ => v.to_string().trim_matches('"').to_string(),
                })
                .collect(),
            Val::Str(s) => vec![(*s).to_string()],
            _ => vec![paths.to_string().trim_matches('"').to_string()],
        })
        .unwrap_or_default()
}

/// Get src paths for a specific project
pub fn get_project_src_paths(conf: &Val, project_name: &str) -> Vec<String> {
    get_project_paths(conf, project_name, "src")
}

/// Get test paths for a specific project
pub fn get_project_test_paths(conf: &Val, project_name: &str) -> Vec<String> {
    get_project_paths(conf, project_name, "test")
}

/// Get resource paths for a specific project
pub fn get_project_resource_paths(conf: &Val, project_name: &str) -> Vec<String> {
    get_project_paths(conf, project_name, "resources")
}

/// Get resource excludes (extra glob patterns) for a project
pub fn get_project_resource_excludes(conf: &Val, project_name: &str) -> Vec<String> {
    get_project_conf(conf, project_name)
        .and_then(|project_conf| project_conf.get("resources"))
        .and_then(|res_conf| res_conf.get("excludes"))
        .map(|excludes| match excludes {
            Val::Vec(vec) => vec
                .iter()
                .map(|v| match v {
                    Val::Str(s) => (*s).to_string(),
                    _ => v.to_string().trim_matches('"').to_string(),
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

/// Whether the project should respect .gitignore during file discovery (default true).
pub fn get_project_respect_gitignore(conf: &Val, project_name: &str) -> bool {
    get_project_conf(conf, project_name)
        .and_then(|project_conf| project_conf.get("ignore"))
        .and_then(|ignore_conf| ignore_conf.get("respect-gitignore"))
        .map(|v| match v {
            Val::Bool(b) => b,
            _ => v.to_string().parse().unwrap_or(true),
        })
        .unwrap_or(true)
}

/// Build a `ResourceRegistry` from project config and install it
/// process-globally so that `::hot::resource/*` bindings can read from it.
///
/// `extra_paths` are appended after the project-config paths (typically
/// from `--resource.path` CLI flags). When `force_no_gitignore` is true
/// it overrides the project's `respect-gitignore` setting.
pub fn install_resource_registry(
    conf: &Val,
    project_name: &str,
    extra_paths: &[String],
    force_no_gitignore: bool,
) -> crate::lang::hot::resource::ResourceRegistry {
    let mut paths: Vec<std::path::PathBuf> = get_project_resource_paths(conf, project_name)
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    for p in extra_paths {
        paths.push(std::path::PathBuf::from(p));
    }
    let respect = if force_no_gitignore {
        false
    } else {
        get_project_respect_gitignore(conf, project_name)
    };
    let mut excludes = get_project_ignore_excludes(conf, project_name);
    excludes.extend(get_project_resource_excludes(conf, project_name));
    let registry = crate::lang::hot::resource::build_registry(&paths, respect, &excludes);
    crate::lang::hot::resource::set_registry(registry.clone());
    registry
}

/// Project-level ignore excludes applied to all file discovery (sources, tests, resources).
pub fn get_project_ignore_excludes(conf: &Val, project_name: &str) -> Vec<String> {
    get_project_conf(conf, project_name)
        .and_then(|project_conf| project_conf.get("ignore"))
        .and_then(|ignore_conf| ignore_conf.get("excludes"))
        .map(|excludes| match excludes {
            Val::Vec(vec) => vec
                .iter()
                .map(|v| match v {
                    Val::Str(s) => (*s).to_string(),
                    _ => v.to_string().trim_matches('"').to_string(),
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

/// Get test capture setting for a specific project
pub fn get_project_test_capture(conf: &Val, project_name: &str) -> bool {
    get_project_conf(conf, project_name)
        .and_then(|project_conf| project_conf.get("test"))
        .and_then(|test_conf| test_conf.get("capture"))
        .map(|capture| match capture {
            Val::Bool(b) => b,
            _ => capture.to_string().parse().unwrap_or(true),
        })
        .unwrap_or(true)
}

/// Get dependencies for a specific project
pub fn get_project_dependencies(conf: &Val, project_name: &str) -> Option<Val> {
    get_project_conf(conf, project_name).and_then(|project_conf| project_conf.get("deps"))
}

/// Parse project dependencies using the dependency resolver
pub fn parse_project_dependencies(
    conf: &Val,
    project_name: &str,
) -> Result<Vec<crate::lang::project::ProjectDependency>, String> {
    if let Some(deps_val) = get_project_dependencies(conf, project_name) {
        crate::lang::project::DependencyResolver::parse_project_dependencies(&deps_val)
    } else {
        Ok(Vec::new())
    }
}

/// Check if a project uses the new deps format or the legacy pkg.paths format
pub fn project_uses_deps_format(conf: &Val, project_name: &str) -> bool {
    get_project_dependencies(conf, project_name).is_some()
}

/// Get resolved dependencies for a project (including hot-std and transitive dependencies)
///
/// This function resolves all direct dependencies and their transitive dependencies
/// by parsing pkg.hot files. Project-level dependency specs act as overrides for
/// transitive dependencies.
pub fn get_resolved_project_dependencies(
    conf: &Val,
    project_name: &str,
) -> Result<Vec<crate::lang::project::ResolvedDependency>, String> {
    let project_deps = parse_project_dependencies(conf, project_name)?;
    let resolver = crate::lang::project::DependencyResolver::default();

    // Build project overrides map for transitive resolution
    let project_overrides =
        crate::lang::project::DependencyResolver::build_project_overrides(&project_deps);

    // Use recursive resolution to include transitive dependencies
    resolver.resolve_all_dependencies_recursive(&project_deps, &project_overrides)
}

/// Result type for compile_project_for_cache to satisfy clippy
/// Returns (BytecodeProgram, function_mapping, core_functions, type_implementations, AST Program, HotAst)
pub type CompileForCacheResult = (
    crate::lang::bytecode::BytecodeProgram,
    indexmap::IndexMap<String, u32>,
    indexmap::IndexMap<String, u32>,
    indexmap::IndexMap<(String, String), String>,
    crate::lang::ast::Program,
    crate::lang::ast::HotAst,
);

/// Compile a project and return the BytecodeProgram with registries, AST, and HotAst for caching
/// This is used by the worker to generate cache files on first run
pub fn compile_project_for_cache(
    project_name: &str,
    src_paths: &[String],
    conf: &Val,
) -> Result<CompileForCacheResult, String> {
    tracing::debug!("Compiling project '{}' for bytecode caching", project_name);

    // Collect all .hot files in the correct loading order
    let mut all_hot_files = Vec::new();

    // Load project dependencies (including hot-std)
    match get_resolved_project_dependencies(conf, project_name) {
        Ok(resolved_deps) => {
            for dep in &resolved_deps {
                let dep_path = dep.resolved_path.to_string_lossy().to_string();
                let files = crate::lang::engine::Engine::discover_hot_files(&dep_path)?;
                all_hot_files.extend(files);
            }
        }
        Err(e) => {
            tracing::warn!("Warning: Failed to load project dependencies: {}", e);
        }
    }

    // Add source files
    for src_path in src_paths {
        let files = crate::lang::engine::Engine::discover_hot_files(src_path)?;
        all_hot_files.extend(files);
    }

    // Remove duplicates while preserving order
    let mut seen = AHashSet::new();
    all_hot_files.retain(|file| seen.insert(file.clone()));

    // Parse all files into a combined program
    let mut combined_program = crate::lang::ast::Program {
        namespaces: indexmap::IndexMap::new(),
        current_namespace: crate::lang::ast::NsPath::hot_main(),
    };

    // Parse all files
    for file_path in &all_hot_files {
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| format!("Failed to read file {}: {}", file_path, e))?;

        let file_program =
            crate::lang::parser::parse_hot_file(&content, file_path).map_err(|e| {
                if let Some(formatted) = e.format_error(&content, false) {
                    format!("Parse errors:\n{}", formatted)
                } else {
                    format!("Failed to parse {}: {}", file_path, e)
                }
            })?;

        // Merge namespaces
        for (ns_path, namespace) in file_program.namespaces {
            combined_program.namespaces.insert(ns_path, namespace);
        }
    }

    // Resolve variable references
    crate::lang::compiler::resolver::resolve_all_variable_references(&mut combined_program);

    // Compile the program
    let mut compiler = crate::lang::compiler::Compiler::new();

    // Add source files for error reporting
    for file_path in &all_hot_files {
        if let Ok(content) = std::fs::read_to_string(file_path) {
            compiler.add_source_file(std::path::PathBuf::from(file_path), content);
        }
    }

    // Compile with validation
    compiler
        .compile_program(&mut combined_program)
        .map_err(|errors| format!("Compilation errors:\n{}", errors.format_error(false)))?;

    // Extract the compiled BytecodeProgram, registries, and AST
    let program = compiler.get_program();
    let function_mapping = compiler.get_function_mapping().clone();
    let core_functions = compiler.get_core_functions().clone();
    let type_implementations = compiler.get_type_implementations().clone();

    // Build HotAst with variable index for caching
    let hot_ast = crate::lang::ast::HotAst::from_program(combined_program.clone());

    tracing::debug!(
        "Successfully compiled project '{}' for caching (with {} namespaces)",
        project_name,
        combined_program.namespaces.len()
    );
    Ok((
        program.clone(),
        function_mapping,
        core_functions,
        type_implementations,
        combined_program,
        hot_ast,
    ))
}
