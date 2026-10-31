//! `hot deps` — list, show, add, remove, and update project dependencies.

use hot::val::Val;
use tracing::info;

use crate::cli::DepsAction;

pub(crate) async fn run_deps(
    action: &DepsAction,
    conf: &Val,
    project_name: &str,
) -> Result<(), String> {
    match action {
        DepsAction::List => {
            println!("Dependencies for project '{}':", project_name);

            if hot::project::project_uses_deps_format(conf, project_name) {
                match hot::project::get_resolved_project_dependencies(conf, project_name) {
                    Ok(resolved_deps) => {
                        for dep in &resolved_deps {
                            if dep.name == "hot.dev/hot-std" {
                                println!("  - {} (built-in)", dep.name);
                            } else {
                                println!(
                                    "  - {} (path: {})",
                                    dep.name,
                                    dep.resolved_path.display()
                                );
                            }
                        }

                        if resolved_deps.is_empty() {
                            println!("  No dependencies configured");
                        }
                    }
                    Err(e) => {
                        println!("  Error resolving dependencies: {}", e);
                    }
                }
            } else {
                println!(
                    "  No dependencies configured. Add them to hot.project.{}.deps in hot.hot.",
                    project_name
                );
            }
        }

        DepsAction::Show => {
            println!("Dependency details for project '{}':", project_name);

            if hot::project::project_uses_deps_format(conf, project_name) {
                let resolved_deps =
                    hot::project::get_resolved_project_dependencies(conf, project_name)
                        .map_err(|e| format!("Failed to resolve dependencies: {}", e))?;

                for resolved_dep in &resolved_deps {
                    println!("\n{}:", resolved_dep.name);
                    println!("  Path: {}", resolved_dep.resolved_path.display());

                    let pkg_file = resolved_dep.resolved_path.join("pkg.hot");
                    if pkg_file.exists() {
                        println!("  Package file: {}", pkg_file.display());
                    } else {
                        println!("  Package file: Not found");
                    }
                }
            } else {
                println!("Project uses legacy pkg.paths format. Use 'hot deps list' to see paths.");
            }
        }

        DepsAction::Add {
            package,
            local,
            version,
            git,
            ..
        } => {
            let spec = if let Some(l) = local {
                format!("{{\"local\": \"{}\"}}", l)
            } else if let Some(g) = git {
                format!("{{\"git\": \"{}\"}}", g)
            } else if let Some(v) = version {
                format!("\"{}\"", v)
            } else {
                "{\"local\": \"./path/to/package\"}".to_string()
            };
            return Err(format!(
                "hot deps add is not implemented yet. Add the dependency to hot.hot:\n  \
                 hot.project.{}.deps {{\"{}\": {}}}",
                project_name, package, spec
            ));
        }

        DepsAction::Remove { package } => {
            return Err(format!(
                "hot deps remove is not implemented yet. Remove \"{}\" from \
                 hot.project.{}.deps in hot.hot.",
                package, project_name
            ));
        }

        DepsAction::Update => {
            info!("Updating dependencies for project '{}'", project_name);

            if hot::project::project_uses_deps_format(conf, project_name) {
                let resolved_deps =
                    hot::project::get_resolved_project_dependencies(conf, project_name)
                        .map_err(|e| format!("Failed to resolve dependencies: {}", e))?;

                println!("Resolved {} dependencies:", resolved_deps.len());
                for resolved_dep in &resolved_deps {
                    println!(
                        "  - {} -> {}",
                        resolved_dep.name,
                        resolved_dep.resolved_path.display()
                    );
                }
            } else {
                println!("Project uses legacy pkg.paths format. Use 'hot deps migrate' first.");
            }
        }

        DepsAction::Migrate => {
            if hot::project::project_uses_deps_format(conf, project_name) {
                println!(
                    "Project '{}' already uses the new deps format",
                    project_name
                );
                return Ok(());
            }

            println!("Migration from pkg.paths to deps format is no longer needed.");
            println!("The legacy pkg.paths system has been removed.");
            println!("All projects now use the deps format by default.");
            println!(
                "\nTo add dependencies, list them in hot.project.{}.deps in hot.hot.",
                project_name
            );
        }
    }

    Ok(())
}
