use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // Capture git SHA at build time (or use CI override to preserve compiler caches)
    println!("cargo:rerun-if-env-changed=HOT_GIT_SHA");
    let explicit_git_sha = env::var("HOT_GIT_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let git_sha = explicit_git_sha.clone().unwrap_or_else(get_git_sha);
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    // Capture version from resources/version.txt
    let version = get_version();
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    // Only emit rerun-if-changed for paths that exist: a missing path forces
    // Cargo to re-run this build script and recompile the crate on every invocation.
    if explicit_git_sha.is_none() {
        emit_git_rerun_directives();
    }
    if Path::new("../../resources/version.txt").exists() {
        println!("cargo:rerun-if-changed=../../resources/version.txt");
    }

    // Use absolute paths relative to the workspace root
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir).parent().unwrap().parent().unwrap();

    let input_css = workspace_root.join("resources/app/assets/css/input.css");
    let output_css = workspace_root.join("resources/app/assets/css/styles.css");

    println!("cargo:rerun-if-changed={}", input_css.display());

    // Watch all template files for changes (including subdirectories like components/)
    let template_dir = Path::new(&manifest_dir).join("templates");
    if template_dir.exists() {
        for entry in walkdir::WalkDir::new(&template_dir).into_iter().flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(ext) = path.extension()
                && ext == "html"
            {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    // Run tailwindcss with absolute paths
    let output = Command::new("tailwindcss")
        .args([
            "-i",
            input_css.to_str().unwrap(),
            "-o",
            output_css.to_str().unwrap(),
        ])
        .current_dir(workspace_root)
        .output();

    match output {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("tailwindcss failed: {}", stderr);
                std::process::exit(1);
            } else {
                // Print success message to stdout so cargo watch can see it
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.is_empty() {
                    println!("tailwindcss: {}", stdout.trim());
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to run tailwindcss: {}", e);
            eprintln!("Make sure tailwindcss is installed and available in PATH");
            std::process::exit(1);
        }
    }
}

fn get_git_sha() -> String {
    // First, try git command (prefer live data for local dev)
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && output.status.success()
        && let Ok(sha) = String::from_utf8(output.stdout)
    {
        return sha.trim().to_string();
    }

    // Fall back to captured file (for Docker/deployed environments without .git)
    if let Ok(sha) = fs::read_to_string("../../resources/git-revision.txt") {
        let sha = sha.trim();
        if !sha.is_empty() && sha != "unknown" {
            return sha.to_string();
        }
    }

    // Final fallback if both methods fail
    "unknown".to_string()
}

fn get_version() -> String {
    // Read version from resources/version.txt
    if let Ok(version) = fs::read_to_string("../../resources/version.txt") {
        let version = version.trim();
        if !version.is_empty() {
            return version.to_string();
        }
    }

    // Fallback if version file not found
    "0.0.0".to_string()
}

fn emit_git_rerun_directives() {
    let head_path = Path::new("../../.git/HEAD");
    if head_path.exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
        if let Ok(head) = fs::read_to_string(head_path)
            && let Some(git_ref) = head.trim().strip_prefix("ref: ")
        {
            let ref_path = format!("../../.git/{git_ref}");
            if Path::new(&ref_path).exists() {
                println!("cargo:rerun-if-changed={ref_path}");
            } else if Path::new("../../.git/packed-refs").exists() {
                println!("cargo:rerun-if-changed=../../.git/packed-refs");
            }
        }
    }
    if Path::new("../../resources/git-revision.txt").exists() {
        println!("cargo:rerun-if-changed=../../resources/git-revision.txt");
    }
}
