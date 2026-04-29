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
                println!("  No dependencies configured. Use 'hot deps add' to add dependencies.");
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
            branch,
            tag,
            path,
        } => {
            if !hot::project::project_uses_deps_format(conf, project_name) {
                return Err(
                    "Project uses legacy pkg.paths format. Use 'hot deps migrate' first."
                        .to_string(),
                );
            }

            let source_count = [local.is_some(), version.is_some(), git.is_some()]
                .iter()
                .filter(|&&x| x)
                .count();
            if source_count != 1 {
                return Err(
                    "Exactly one of --local, --version, or --git must be specified".to_string(),
                );
            }

            if git.is_none() {
                if branch.is_some() {
                    return Err("--branch can only be used with --git".to_string());
                }
                if tag.is_some() {
                    return Err("--tag can only be used with --git".to_string());
                }
                if path.is_some() {
                    return Err(
                        "--path can only be used with --git (for monorepo subdirectories)"
                            .to_string(),
                    );
                }
            }

            if branch.is_some() && tag.is_some() {
                return Err("Cannot specify both --branch and --tag".to_string());
            }

            println!(
                "Adding dependency '{}' to project '{}'",
                package, project_name
            );

            if let Some(git_url) = git {
                print!("  Source: Git repository {}", git_url);
                if let Some(b) = branch {
                    print!(" (branch: {})", b);
                } else if let Some(t) = tag {
                    print!(" (tag/commit: {})", t);
                }
                if let Some(p) = path {
                    print!(" at path: {}", p);
                }
                println!();
            } else if let Some(l) = local {
                println!("  Source: Local file system path {}", l);
            } else if let Some(v) = version {
                println!("  Source: Registry version {}", v);
            }

            println!("Note: Dependency addition not yet implemented - this is a placeholder");
        }

        DepsAction::Remove { package } => {
            if !hot::project::project_uses_deps_format(conf, project_name) {
                return Err(
                    "Project uses legacy pkg.paths format. Use 'hot deps migrate' first."
                        .to_string(),
                );
            }

            println!(
                "Removing dependency '{}' from project '{}'",
                package, project_name
            );
            println!("Note: Dependency removal not yet implemented - this is a placeholder");
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

                println!("Note: Dependency caching not yet implemented");
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
            println!("\nTo add dependencies to your project, use:");
            println!("  hot deps add <package-name> --path <local-path>");
            println!("  hot deps add <package-name> --git <git-url>");
            println!("  hot deps add <package-name> --version <version>");
        }
    }

    Ok(())
}
