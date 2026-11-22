use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=HOT_GIT_SHA");
    let explicit_git_sha = env::var("HOT_GIT_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let git_sha = explicit_git_sha.clone().unwrap_or_else(get_git_sha);
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

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
}

fn get_git_sha() -> String {
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && output.status.success()
        && let Ok(sha) = String::from_utf8(output.stdout)
    {
        return sha.trim().to_string();
    }

    if let Ok(sha) = fs::read_to_string("../../resources/git-revision.txt") {
        let sha = sha.trim();
        if !sha.is_empty() && sha != "unknown" {
            return sha.to_string();
        }
    }

    "unknown".to_string()
}

fn get_version() -> String {
    if let Ok(version) = fs::read_to_string("../../resources/version.txt") {
        let version = version.trim();
        if !version.is_empty() {
            return version.to_string();
        }
    }

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
