use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // Embed Windows application icon and metadata when targeting Windows
    // This works for both native Windows builds and cross-compilation
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        embed_windows_resources();
    }

    // Capture git SHA at build time
    let git_sha = get_git_sha();
    println!("cargo:rustc-env=GIT_SHA={}", git_sha);

    // Capture version from resources/version.txt
    let version = get_version();
    println!("cargo:rustc-env=HOT_VERSION={}", version);

    println!("cargo:rerun-if-changed=../../resources/version.txt");
    println!("cargo:rerun-if-changed=../../resources/git-revision.txt");

    // Rerun if git HEAD changes (this catches new commits)
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
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

/// Embed Windows resources (icon, version info, etc.) into the executable
/// Called when targeting Windows (including cross-compilation)
fn embed_windows_resources() {
    // Look for ICO file in the resources directory
    let icon_path = Path::new("../../resources/application/icons/hot_icon.ico");

    if icon_path.exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon_path.to_str().unwrap());
        res.set("ProductName", "Hot Dev");
        res.set("FileDescription", "Hot Dev Command Line Interface");
        res.set("LegalCopyright", "Copyright © 2025-2026 Hot Dev, LLC");
        res.set("CompanyName", "Hot Dev, LLC");

        if let Err(e) = res.compile() {
            eprintln!("Warning: Failed to compile Windows resources: {}", e);
        } else {
            println!("cargo:warning=Embedded Windows icon from {:?}", icon_path);
        }
    } else {
        // Icon not found - this is expected when building without an ICO file
        // The Windows installer will generate the ICO from PNG during packaging
        eprintln!(
            "Note: Windows icon not found at {:?}, executable will use default icon",
            icon_path
        );
        eprintln!("To embed an icon, create hot_icon.ico from hot_icon.png");
    }

    println!("cargo:rerun-if-changed=../../resources/application/icons/hot_icon.ico");
}
