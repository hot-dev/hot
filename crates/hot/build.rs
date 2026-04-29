use std::fs;
use std::process::Command;

fn main() {
    // Capture version from resources/version.txt
    let version = get_version();
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    // Capture git SHA at build time
    let git_sha = get_git_sha();
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    // Rerun if version changes
    println!("cargo:rerun-if-changed=../../resources/version.txt");
    // Rerun if git HEAD changes (for git SHA)
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn get_version() -> String {
    // Read version from resources/version.txt
    if let Ok(version) = fs::read_to_string("../../resources/version.txt") {
        let version = version.trim();
        if !version.is_empty() {
            return version.to_string();
        }
    }
    "unknown".to_string()
}

fn get_git_sha() -> String {
    // First, try git command (prefer live data for local dev)
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && output.status.success()
        && let Ok(sha) = String::from_utf8(output.stdout)
    {
        let sha = sha.trim();
        if !sha.is_empty() {
            return sha.to_string();
        }
    }

    // Fallback: try reading from .git/HEAD
    if let Ok(head_ref) = fs::read_to_string("../../.git/HEAD") {
        let head_ref = head_ref.trim();
        if let Some(ref_path) = head_ref.strip_prefix("ref: ") {
            // It's a symbolic ref, read the actual SHA
            if let Ok(sha) = fs::read_to_string(format!("../../.git/{}", ref_path)) {
                return sha.trim().to_string();
            }
        } else {
            // Detached HEAD, the content is the SHA itself
            return head_ref.to_string();
        }
    }

    "unknown".to_string()
}
