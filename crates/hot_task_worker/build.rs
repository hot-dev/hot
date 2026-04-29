use std::fs;
use std::process::Command;

fn main() {
    let git_sha = get_git_sha();
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    let version = get_version();
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-changed=../../resources/git-revision.txt");
    println!("cargo:rerun-if-changed=../../resources/version.txt");
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
