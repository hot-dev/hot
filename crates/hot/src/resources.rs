use std::path::{Path, PathBuf};

/// Gets the resources directory path based on the current binary location and installation type.
///
/// This function handles different deployment scenarios:
/// - HOT_HOME environment variable: Uses `$HOT_HOME/resources` (highest priority)
/// - Development: Uses workspace-relative resources directory
/// - macOS package install: Uses `/usr/local/share/hot/` when binary is at `/usr/local/bin/hot`
/// - Linux package install: Uses `/usr/share/hot/` when binary is at `/usr/bin/hot`
/// - Windows install: Uses resources directory next to executable
/// - Bundled CLI: Uses embedded resources directory next to executable
pub fn get_resources_path() -> Result<PathBuf, String> {
    // Try to get the current executable path
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get current executable path: {}", e))?;

    // Get the parent directory of the executable
    let exe_dir = exe_path
        .parent()
        .ok_or("Failed to get executable directory")?;

    // Check for different installation patterns

    // Case 0: HOT_HOME environment variable (highest priority)
    if let Ok(hot_home) = std::env::var("HOT_HOME") {
        let resources_path = PathBuf::from(hot_home).join("resources");
        if resources_path.exists() {
            return Ok(resources_path);
        }
    }

    // Case 1: macOS package installation (/usr/local/bin/hot -> /usr/local/share/hot/)
    if exe_dir == Path::new("/usr/local/bin") {
        let resources_path = PathBuf::from("/usr/local/share/hot");
        if resources_path.exists() {
            return Ok(resources_path);
        }
    }

    // Case 2: Linux system installation (/usr/bin/hot -> /usr/share/hot/)
    if exe_dir == Path::new("/usr/bin") {
        let resources_path = PathBuf::from("/usr/share/hot");
        if resources_path.exists() {
            return Ok(resources_path);
        }
    }

    // Case 3: Local installation or bundled CLI (resources next to executable)
    let local_resources = exe_dir.join("resources");
    if local_resources.exists() {
        return Ok(local_resources);
    }

    // Case 4: Development environment (look for workspace-relative resources)
    // Try to find the workspace root by looking for Cargo.toml
    let mut current_dir = exe_dir;
    loop {
        let cargo_toml = current_dir.join("Cargo.toml");
        if cargo_toml.exists() {
            // Check if this is the workspace root by looking for the resources directory
            let workspace_resources = current_dir.join("resources");
            if workspace_resources.exists() {
                return Ok(workspace_resources);
            }
        }

        // Move up one directory
        if let Some(parent) = current_dir.parent() {
            current_dir = parent;
        } else {
            break;
        }
    }

    // Case 5: Current working directory relative (fallback for development)
    let cwd_resources = std::env::current_dir()
        .map_err(|e| format!("Failed to get current directory: {}", e))?
        .join("resources");
    if cwd_resources.exists() {
        return Ok(cwd_resources);
    }

    // If nothing is found, return an error
    Err(format!(
        "Could not locate resources directory. Executable path: {}",
        exe_path.display()
    ))
}

/// Gets the path to a bundled hotbox binary for the given Linux architecture.
///
/// Hotbox binaries are shipped inside the resources directory so that `hot dev`
/// can bind-mount the correct Linux binary into Docker containers.
///
/// # Arguments
/// * `arch` - Target architecture: `"arm64"` or `"x86_64"`
pub fn get_hotbox_path(arch: &str) -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    let hotbox_path = resources_path
        .join("bin")
        .join(format!("hotbox-linux-{}", arch));
    if hotbox_path.exists() {
        Ok(hotbox_path)
    } else {
        Err(format!(
            "hotbox binary not found for arch {}: {}",
            arch,
            hotbox_path.display()
        ))
    }
}

/// Gets the migration directory path for the specified database type.
///
/// # Arguments
/// * `db_type` - The database type ("sqlite" or "postgres")
///
/// # Returns
/// The full path to the migration directory for the specified database type.
pub fn get_migration_path(db_type: &str) -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("db").join(db_type).join("migrations"))
}

/// Gets the app assets directory path.
///
/// # Returns
/// The full path to the app assets directory.
pub fn get_app_assets_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("app").join("assets"))
}

/// Gets the web assets directory path.
///
/// # Returns
/// The full path to the web assets directory.
pub fn get_web_assets_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("web").join("assets"))
}

/// Gets the path to a legal document.
///
/// # Arguments
/// * `filename` - The filename of the legal document (e.g., "LICENSE.md", "PRIVACY_POLICY.md")
///
/// # Returns
/// The full path to the legal document.
pub fn get_legal_document_path(filename: &str) -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    let legal_path = resources_path.join("legal").join(filename);
    if legal_path.exists() {
        Ok(legal_path)
    } else {
        Err(format!(
            "Legal document not found: {}",
            legal_path.display()
        ))
    }
}

/// Gets the documentation directory path.
///
/// # Returns
/// The full path to the docs directory.
pub fn get_docs_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("docs"))
}

/// Gets the package documentation directory path.
///
/// Pre-generated package documentation JSON files are stored here.
///
/// # Returns
/// The full path to the pkg-docs directory.
pub fn get_pkg_docs_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("pkg-docs"))
}

/// Gets the blog directory path.
///
/// Blog posts are stored as markdown files in resources/blog/.
///
/// # Returns
/// The full path to the blog directory.
pub fn get_blog_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("blog"))
}

/// Gets the AI resources directory path.
///
/// AI coding hints and skills are stored in resources/ai/.
///
/// # Returns
/// The full path to the ai directory.
pub fn get_ai_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("ai"))
}

/// Gets the path to a specific AI skill.
///
/// # Arguments
/// * `skill_name` - The name of the skill (e.g., "hot-language")
///
/// # Returns
/// The full path to the skill directory.
pub fn get_skill_path(skill_name: &str) -> Result<PathBuf, String> {
    let ai_path = get_ai_path()?;
    let skill_path = ai_path.join("skills").join(skill_name);
    if skill_path.exists() {
        Ok(skill_path)
    } else {
        Err(format!(
            "Skill not found: {} (looked in {})",
            skill_name,
            skill_path.display()
        ))
    }
}

/// Reads an AI resource file from the ai directory.
///
/// # Arguments
/// * `filename` - Filename within resources/ai/ (e.g., "AGENTS.md")
///
/// # Returns
/// The contents of the file as a string.
pub fn read_ai_file(filename: &str) -> Result<String, String> {
    let ai_path = get_ai_path()?;
    let file_path = ai_path.join(filename);
    if file_path.exists() {
        std::fs::read_to_string(&file_path)
            .map_err(|e| format!("Failed to read AI file {}: {}", file_path.display(), e))
    } else {
        Err(format!(
            "AI file not found: {} (looked in {})",
            filename,
            file_path.display()
        ))
    }
}

/// Reads the AGENTS.md template.
///
/// # Returns
/// The contents of the AGENTS.md template.
pub fn read_agents_md() -> Result<String, String> {
    read_ai_file("AGENTS.md")
}

/// Gets the init templates directory path.
///
/// Project initialization templates are stored in resources/init/.
///
/// # Returns
/// The full path to the init directory.
pub fn get_init_path() -> Result<PathBuf, String> {
    let resources_path = get_resources_path()?;
    Ok(resources_path.join("init"))
}

/// Reads an init template file.
///
/// # Arguments
/// * `template_file` - Filename within resources/init/ (e.g., "hot.hot.template")
///
/// # Returns
/// The contents of the template file as a string.
pub fn read_init_template(template_file: &str) -> Result<String, String> {
    let init_path = get_init_path()?;
    let template_path = init_path.join(template_file);
    if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .map_err(|e| format!("Failed to read template {}: {}", template_path.display(), e))
    } else {
        Err(format!(
            "Template not found: {} (looked in {})",
            template_file,
            template_path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_resources_path() {
        // This test will depend on the environment it's run in
        // In development, it should find the workspace resources
        match get_resources_path() {
            Ok(path) => {
                println!("Resources path: {}", path.display());
                assert!(path.exists(), "Resources path should exist");
            }
            Err(e) => {
                println!("Could not determine resources path: {}", e);
                // Don't fail the test as this depends on the environment
            }
        }
    }

    #[test]
    fn test_get_migration_path() {
        // Test that migration paths are constructed correctly
        match get_resources_path() {
            Ok(_) => {
                if let Ok(sqlite_path) = get_migration_path("sqlite") {
                    assert!(
                        sqlite_path
                            .to_string_lossy()
                            .contains("db/sqlite/migrations")
                    );
                }
                if let Ok(postgres_path) = get_migration_path("postgres") {
                    assert!(
                        postgres_path
                            .to_string_lossy()
                            .contains("db/postgres/migrations")
                    );
                }
            }
            Err(_) => {
                // Skip test if resources can't be found
            }
        }
    }
}
