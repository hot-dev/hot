use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be available"),
    );
    let workspace_root = find_workspace_root(&manifest_dir);

    // Capture version from resources/version.txt
    let version = get_version(workspace_root.as_deref());
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    // Capture git SHA at build time
    let git_sha = get_git_sha(&manifest_dir, workspace_root.as_deref());
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    let build_fingerprint = get_build_fingerprint(&manifest_dir, workspace_root.as_deref())
        .expect("failed to fingerprint Hot compiler sources");
    println!(
        "cargo:rustc-env=HOT_BUILD_FINGERPRINT={}",
        build_fingerprint
    );

    emit_rerun_directives(&manifest_dir, workspace_root.as_deref());
}

fn find_workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    let package_manifest = fs::read_to_string(manifest_dir.join("Cargo.toml")).ok()?;
    if !package_manifest
        .lines()
        .any(|line| line.contains("workspace = true") || line.contains(".workspace = true"))
    {
        // Published/packaged manifests have workspace inheritance resolved.
        // Do not accidentally fingerprint an unrelated parent workspace when
        // such a package is vendored below one.
        return None;
    }

    manifest_dir.ancestors().find_map(|candidate| {
        let manifest = fs::read_to_string(candidate.join("Cargo.toml")).ok()?;
        manifest
            .lines()
            .any(|line| line.trim() == "[workspace]")
            .then(|| candidate.to_path_buf())
    })
}

fn get_version(workspace_root: Option<&Path>) -> String {
    if let Some(workspace_root) = workspace_root
        && let Ok(version) = fs::read_to_string(workspace_root.join("resources/version.txt"))
    {
        let version = version.trim();
        if !version.is_empty() {
            return version.to_string();
        }
    }

    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
}

fn get_git_sha(manifest_dir: &Path, workspace_root: Option<&Path>) -> String {
    let source_root = workspace_root.unwrap_or(manifest_dir);

    // First, try git command (prefer live data for local dev)
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(source_root)
        .output()
        && output.status.success()
        && let Ok(sha) = String::from_utf8(output.stdout)
    {
        let sha = sha.trim();
        if !sha.is_empty() {
            return sha.to_string();
        }
    }

    // Fallback: try reading from .git/HEAD
    let git_dir = source_root.join(".git");
    if let Ok(head_ref) = fs::read_to_string(git_dir.join("HEAD")) {
        let head_ref = head_ref.trim();
        if let Some(ref_path) = head_ref.strip_prefix("ref: ") {
            // It's a symbolic ref, read the actual SHA
            if let Ok(sha) = fs::read_to_string(git_dir.join(ref_path)) {
                return sha.trim().to_string();
            }
        } else {
            // Detached HEAD, the content is the SHA itself
            return head_ref.to_string();
        }
    }

    "unknown".to_string()
}

fn get_build_fingerprint(
    manifest_dir: &Path,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let mut files = Vec::new();
    collect_rust_sources(&manifest_dir.join("src"), &mut files)?;
    files.push(manifest_dir.join("build.rs"));
    files.push(manifest_dir.join("Cargo.toml"));
    for optional in [
        manifest_dir.join("Cargo.lock"),
        workspace_root
            .map(|root| root.join("Cargo.toml"))
            .unwrap_or_default(),
        workspace_root
            .map(|root| root.join("Cargo.lock"))
            .unwrap_or_default(),
    ] {
        if optional.is_file() {
            files.push(optional);
        }
    }
    files.sort();
    files.dedup();

    let mut hasher = blake3::Hasher::new();
    for path in files {
        let relative = if let Ok(relative) = path.strip_prefix(manifest_dir) {
            relative.to_path_buf()
        } else if let Some(workspace_root) = workspace_root {
            Path::new("workspace").join(
                path.strip_prefix(workspace_root)
                    .map_err(|e| format!("failed to relativize {}: {}", path.display(), e))?,
            )
        } else {
            return Err(format!(
                "fingerprint input {} is outside the package",
                path.display()
            ));
        };
        let bytes =
            fs::read(&path).map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&bytes);
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn emit_rerun_directives(manifest_dir: &Path, workspace_root: Option<&Path>) {
    let mut paths = vec![
        manifest_dir.join("src"),
        manifest_dir.join("build.rs"),
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("Cargo.lock"),
    ];
    if let Some(workspace_root) = workspace_root {
        paths.extend([
            workspace_root.join("resources/version.txt"),
            workspace_root.join(".git/HEAD"),
            workspace_root.join("Cargo.toml"),
            workspace_root.join("Cargo.lock"),
        ]);
    }
    paths.sort();
    paths.dedup();

    for path in paths {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn collect_rust_sources(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("failed to read entry in {}: {}", dir.display(), e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("failed to inspect {}: {}", path.display(), e))?;
        if file_type.is_dir() {
            collect_rust_sources(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}
