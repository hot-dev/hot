use std::path::{Path, PathBuf};

/// Where the macOS package installs Hot's resources.
const MACOS_PACKAGE_RESOURCES: &str = "/usr/local/share/hot";

/// Where the Linux package installs Hot's resources.
const LINUX_PACKAGE_RESOURCES: &str = "/usr/share/hot";

/// The resources directories of the running binary's own installation.
///
/// - `<prefix>/share/hot` for a binary at `<prefix>/bin/hot`: the macOS and
///   Linux packages (`/usr/local`, `/usr`), Homebrew (`/opt/homebrew`) and
///   user installs (`~/.local`)
/// - `<exe_dir>/resources`: Windows and bundled installs
///
/// Both the path the binary was started from and its symlink-resolved path
/// are tried, so a symlink to `hot` finds the installation it points to.
pub fn installation_resource_dirs() -> Vec<PathBuf> {
    match std::env::current_exe() {
        Ok(exe) => installation_resource_dirs_for(&exe),
        Err(_) => Vec::new(),
    }
}

fn installation_resource_dirs_for(exe: &Path) -> Vec<PathBuf> {
    let mut exes = vec![exe.to_path_buf()];
    if let Ok(resolved) = exe.canonicalize()
        && resolved != exe
    {
        exes.push(resolved);
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    for exe in &exes {
        let Some(exe_dir) = exe.parent() else {
            continue;
        };
        let share = exe_dir
            .parent()
            .filter(|_| exe_dir.file_name().is_some_and(|name| name == "bin"))
            .map(|prefix| prefix.join("share").join("hot"));
        for dir in share.into_iter().chain([exe_dir.join("resources")]) {
            if dir.is_dir() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Directories that hold hot.dev's bundled packages (hot-std and the others),
/// in the order they're searched:
/// 1. `$HOT_HOME/pkg`
/// 2. `./hot/pkg`, for a checkout of this repository
/// 3. The running binary's installation (see [`installation_resource_dirs`])
/// 4. The macOS and Linux package locations
///
/// Callers join a package name onto each and take the first that exists.
pub fn bundled_pkg_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOT_HOME") {
        dirs.push(PathBuf::from(home).join("pkg"));
    }
    dirs.push(PathBuf::from("./hot/pkg"));
    for dir in installation_resource_dirs()
        .into_iter()
        .chain([MACOS_PACKAGE_RESOURCES, LINUX_PACKAGE_RESOURCES].map(PathBuf::from))
    {
        let pkg = dir.join("pkg");
        if !dirs.contains(&pkg) {
            dirs.push(pkg);
        }
    }
    dirs
}

/// Gets the resources directory path based on the current binary location and installation type.
///
/// This function handles different deployment scenarios:
/// - HOT_HOME environment variable: Uses `$HOT_HOME/resources` (highest priority)
/// - Installed binary: Uses its installation's resources (see [`installation_resource_dirs`])
/// - Development: Uses workspace-relative resources directory
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

    // Cases 1-3: the binary's own installation
    if let Some(resources_path) = installation_resource_dirs_for(&exe_path).into_iter().next() {
        return Ok(resources_path);
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

    /// Lays out `<prefix>/bin/hot` and `<prefix>/share/hot` under `root`.
    fn install_prefix(root: &Path, prefix: &str) -> (PathBuf, PathBuf) {
        let exe = root.join(prefix).join("bin").join("hot");
        let share = root.join(prefix).join("share").join("hot");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(&exe, "").unwrap();
        (exe, share)
    }

    #[test]
    fn a_binary_in_a_bin_dir_finds_the_prefix_share_dir() {
        let root = tempfile::tempdir().unwrap();
        let (exe, share) = install_prefix(root.path(), ".local");

        let dirs = installation_resource_dirs_for(&exe);
        assert_eq!(dirs.first(), Some(&share), "{dirs:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_binary_finds_the_installation_it_points_to() {
        let root = tempfile::tempdir().unwrap();
        let (exe, share) = install_prefix(root.path(), "install");
        let link_dir = root.path().join("elsewhere").join("bin");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("hot");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        let dirs = installation_resource_dirs_for(&link);
        let share = share.canonicalize().unwrap();
        assert!(dirs.contains(&share), "{dirs:?} should contain {share:?}");
    }

    #[test]
    fn only_a_bin_dir_looks_in_its_prefix_share_dir() {
        let root = tempfile::tempdir().unwrap();
        // A build in target/debug must not pick up an unrelated target/share/hot.
        let exe = root.path().join("target").join("debug").join("hot");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.path().join("target").join("share").join("hot")).unwrap();
        std::fs::write(&exe, "").unwrap();
        assert!(installation_resource_dirs_for(&exe).is_empty());

        let bundled = root.path().join("target").join("debug").join("resources");
        std::fs::create_dir_all(&bundled).unwrap();
        assert_eq!(installation_resource_dirs_for(&exe).first(), Some(&bundled));
    }

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
                // ends_with compares whole path components, so it matches
                // regardless of the platform separator (`\` on Windows).
                if let Ok(sqlite_path) = get_migration_path("sqlite") {
                    assert!(sqlite_path.ends_with("db/sqlite/migrations"));
                }
                if let Ok(postgres_path) = get_migration_path("postgres") {
                    assert!(postgres_path.ends_with("db/postgres/migrations"));
                }
            }
            Err(_) => {
                // Skip test if resources can't be found
            }
        }
    }
}
