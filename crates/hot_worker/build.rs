use std::fs;
use std::process::Command;

fn main() {
    // Capture git SHA at build time
    let git_sha = get_git_sha();
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    // Capture version from resources/version.txt
    let version = get_version();
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    // Rerun if git HEAD changes (this catches new commits)
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    // Rerun if the captured git revision file changes
    println!("cargo:rerun-if-changed=../../resources/git-revision.txt");
    println!("cargo:rerun-if-changed=../../resources/version.txt");
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
