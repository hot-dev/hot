//! Update checking functionality for the Hot CLI
//!
//! Checks for newer versions from the configured release channel
//! and provides download URLs for the appropriate platform/architecture.

use crate::build_info;
use futures::StreamExt;
use std::io::Write;
use std::process::Command;

/// Default base URL for official release downloads.
const DEFAULT_RELEASE_BASE_URL: &str = "https://get.hot.dev/releases/latest";

/// Default Windows installer URL for official package installs.
const DEFAULT_WINDOWS_INSTALLER_URL: &str = "https://get.hot.dev/install.ps1";

fn release_base_url(target_version: Option<&str>) -> Option<String> {
    if std::env::var("HOT_UPDATE_DISABLED")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
    {
        return None;
    }

    match std::env::var("HOT_UPDATE_BASE_URL") {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(value.trim_end_matches('/').to_string()),
        Err(_) => Some(match target_version {
            Some(version) => format!("https://get.hot.dev/releases/v{}", version.trim()),
            None => DEFAULT_RELEASE_BASE_URL.to_string(),
        }),
    }
}

fn windows_installer_url() -> String {
    std::env::var("HOT_UPDATE_INSTALLER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WINDOWS_INSTALLER_URL.to_string())
}

/// Represents the detected platform and architecture
#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub os: &'static str,
    pub arch: &'static str,
    pub package_name: String,
    pub download_url: String,
}

impl PlatformInfo {
    /// Detect the current platform and architecture
    /// Note: Package names in the 'latest' folder use user-friendly names
    /// without version numbers for stable download URLs.
    /// Naming convention matches .github/workflows/build-and-package.yml
    pub fn detect_with_base_url(release_base_url: &str) -> Option<Self> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        // Map Rust's arch names to user-friendly package arch names
        let package_arch = match arch {
            "x86_64" => "x86_64",
            "aarch64" => "arm64",
            _ => return None, // Unsupported architecture
        };

        // Map OS to user-friendly names
        let package_os = match os {
            "macos" => "macos",
            "linux" => "linux",
            "windows" => "windows",
            _ => return None, // Unsupported OS
        };

        let package_name = match os {
            "macos" => {
                // macOS uses: hot_macos_{arch}.pkg
                format!("hot_{}_{}.pkg", package_os, package_arch)
            }
            "linux" => {
                // Linux uses: hot_linux_{arch}.deb
                format!("hot_{}_{}.deb", package_os, package_arch)
            }
            "windows" => {
                // Windows uses: hot_windows_{arch}.exe
                format!("hot_{}_{}.exe", package_os, package_arch)
            }
            _ => return None, // Unsupported OS
        };

        let download_url = format!(
            "{}/{}",
            release_base_url.trim_end_matches('/'),
            package_name
        );

        Some(PlatformInfo {
            os,
            arch: package_arch,
            package_name,
            download_url,
        })
    }

    /// Get a human-readable platform description
    pub fn description(&self) -> String {
        let os_name = match self.os {
            "macos" => "macOS",
            "linux" => "Linux",
            "windows" => "Windows",
            other => other,
        };

        let arch_name = match self.arch {
            "x86_64" => "x86_64",
            "arm64" => "ARM64",
            other => other,
        };

        format!("{} ({})", os_name, arch_name)
    }
}

/// Result of a version check
#[derive(Debug)]
pub enum UpdateCheckResult {
    /// Update checks are disabled for this build or environment
    Disabled,
    /// A specific requested version is available to install
    TargetVersion {
        current_version: String,
        target_version: String,
        platform: PlatformInfo,
    },
    /// A newer version is available
    UpdateAvailable {
        current_version: String,
        latest_version: String,
        platform: PlatformInfo,
    },
    /// Already running the latest version
    UpToDate {
        current_version: String,
        platform: PlatformInfo,
    },
    /// Could not determine platform
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },
    /// Network or parsing error
    CheckFailed { error: String },
}

/// Parse a version string into comparable components
/// Handles versions like "0.4.0" or "0.4.0-beta.1"
fn parse_version(version: &str) -> Option<(u32, u32, u32, Option<String>)> {
    let version = version.trim();

    // Split off any pre-release suffix (e.g., "-beta.1")
    let (version_part, prerelease) = if let Some(idx) = version.find('-') {
        (&version[..idx], Some(version[idx + 1..].to_string()))
    } else {
        (version, None)
    };

    let parts: Vec<&str> = version_part.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let major = parts[0].parse().ok()?;
    let minor = parts[1].parse().ok()?;
    let patch = parts[2].parse().ok()?;

    Some((major, minor, patch, prerelease))
}

/// Compare two versions, returns true if `latest` is newer than `current`
fn is_newer_version(current: &str, latest: &str) -> bool {
    let current_parsed = match parse_version(current) {
        Some(v) => v,
        None => return false,
    };

    let latest_parsed = match parse_version(latest) {
        Some(v) => v,
        None => return false,
    };

    // Compare major.minor.patch
    match latest_parsed
        .0
        .cmp(&current_parsed.0)
        .then(latest_parsed.1.cmp(&current_parsed.1))
        .then(latest_parsed.2.cmp(&current_parsed.2))
    {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            // Same version numbers, compare pre-release
            // A release without pre-release is newer than one with pre-release
            // e.g., 0.4.0 is newer than 0.4.0-beta.1
            match (&current_parsed.3, &latest_parsed.3) {
                (Some(_), None) => true, // Current is pre-release, latest is release
                _ => false,              // Either both are releases, or latest is pre-release
            }
        }
    }
}

fn is_valid_requested_version(version: &str) -> bool {
    parse_version(version).is_some()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// Check for updates by fetching the latest version from the server, or by
/// resolving the requested version's release artifact.
pub async fn check_for_updates(target_version: Option<&str>) -> UpdateCheckResult {
    let current_version = build_info::VERSION.to_string();
    let target_version = target_version
        .map(str::trim)
        .filter(|version| !version.is_empty());

    if let Some(version) = target_version
        && !is_valid_requested_version(version)
    {
        return UpdateCheckResult::CheckFailed {
            error: format!("Invalid version format: {}", version),
        };
    }

    let release_base_url = match release_base_url(target_version) {
        Some(url) => url,
        None => return UpdateCheckResult::Disabled,
    };

    if let Some(version) = target_version {
        let platform = match PlatformInfo::detect_with_base_url(&release_base_url) {
            Some(p) => p,
            None => {
                return UpdateCheckResult::UnsupportedPlatform {
                    os: std::env::consts::OS,
                    arch: std::env::consts::ARCH,
                };
            }
        };

        if version == current_version {
            return UpdateCheckResult::UpToDate {
                current_version,
                platform,
            };
        }

        return UpdateCheckResult::TargetVersion {
            current_version,
            target_version: version.to_string(),
            platform,
        };
    }

    // Fetch the latest version
    let latest_version = match fetch_latest_version(&release_base_url).await {
        Ok(v) => v,
        Err(e) => {
            return UpdateCheckResult::CheckFailed {
                error: e.to_string(),
            };
        }
    };

    // Check if we can determine the platform
    let platform = match PlatformInfo::detect_with_base_url(&release_base_url) {
        Some(p) => p,
        None => {
            return UpdateCheckResult::UnsupportedPlatform {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
            };
        }
    };

    // Compare versions
    if is_newer_version(&current_version, &latest_version) {
        UpdateCheckResult::UpdateAvailable {
            current_version,
            latest_version,
            platform,
        }
    } else {
        UpdateCheckResult::UpToDate {
            current_version,
            platform,
        }
    }
}

/// Fetch the latest version string from the server
async fn fetch_latest_version(
    release_base_url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let user_agent = format!("hot/{}", crate::build_info::VERSION);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(&user_agent)
        .build()?;

    let version_check_url = format!("{}/version.txt", release_base_url.trim_end_matches('/'));
    let response = client.get(version_check_url).send().await?;

    if !response.status().is_success() {
        return Err(format!("Server returned status: {}", response.status()).into());
    }

    let version = response.text().await?.trim().to_string();

    if version.is_empty() {
        return Err("Empty version response from server".into());
    }

    // Validate it looks like a version
    if parse_version(&version).is_none() {
        return Err(format!("Invalid version format: {}", version).into());
    }

    Ok(version)
}

/// How Hot was installed
#[derive(Debug, Clone, PartialEq)]
pub enum InstallMethod {
    /// Installed via Homebrew
    Homebrew,
    /// Installed via package installer or other method
    Package,
}

/// Detect how Hot was installed by checking the binary path
pub fn detect_install_method() -> InstallMethod {
    // Get the path to the current executable
    let exe_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return InstallMethod::Package,
    };

    let path_str = exe_path.to_string_lossy();

    // Homebrew paths:
    // - Apple Silicon: /opt/homebrew/bin/hot or /opt/homebrew/Cellar/hot/...
    // - Intel Mac: /usr/local/bin/hot or /usr/local/Cellar/hot/...
    // - Linux Homebrew: /home/linuxbrew/.linuxbrew/bin/hot
    if path_str.contains("/opt/homebrew/")
        || path_str.contains("/usr/local/Cellar/")
        || path_str.contains("/linuxbrew/")
        || (path_str.starts_with("/usr/local/bin/") && cfg!(target_os = "macos"))
    {
        // Double-check by seeing if brew knows about hot
        if let Ok(output) = Command::new("brew").args(["list", "hot"]).output()
            && output.status.success()
        {
            return InstallMethod::Homebrew;
        }
    }

    InstallMethod::Package
}

/// Download the package and run the installer
pub async fn run_installer(
    platform: &PlatformInfo,
    target_version: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Check if installed via Homebrew
    let install_method = detect_install_method();

    if install_method == InstallMethod::Homebrew && target_version.is_none() {
        println!("Detected Homebrew installation.");
        println!("Running: brew upgrade hot");
        println!();

        let status = Command::new("brew").args(["upgrade", "hot"]).status()?;

        if status.success() {
            return Ok(());
        } else {
            return Err("brew upgrade failed".into());
        }
    }

    // Windows: Use PowerShell install script to avoid file locking issues
    // Windows cannot replace a running executable, so we spawn the installer
    // script in a separate process and exit immediately.
    if platform.os == "windows" {
        println!("Launching installer...");
        println!();
        println!("The installer will open in a new window.");
        println!("This process will now exit to allow the update to proceed.");
        println!();

        // Spawn PowerShell to run the install script
        let ps_command = if let Some(version) = target_version {
            format!(
                "Start-Process powershell -ArgumentList @('-ExecutionPolicy','Bypass','-Command','$env:HOT_VERSION=''{}''; irm {} | iex') -Verb RunAs",
                version,
                windows_installer_url()
            )
        } else {
            format!(
                "Start-Process powershell -ArgumentList @('-ExecutionPolicy','Bypass','-Command','irm {} | iex') -Verb RunAs",
                windows_installer_url()
            )
        };
        Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-Command", &ps_command])
            .spawn()?;

        // Exit immediately so the installer can replace hot.exe
        std::process::exit(0);
    }

    // Download the package using reqwest
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join(&platform.package_name);

    println!("Downloading {}...", platform.package_name);

    let user_agent = format!("hot/{}", crate::build_info::VERSION);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300)) // 5 min timeout for large downloads
        .user_agent(&user_agent)
        .build()?;

    let response = client.get(&platform.download_url).send().await?;

    if !response.status().is_success() {
        return Err(format!("Download failed: server returned {}", response.status()).into());
    }

    let total_size = response.content_length();
    let mut downloaded: u64 = 0;
    let mut file = std::fs::File::create(&temp_file)?;

    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;

        // Show progress
        if let Some(total) = total_size {
            let percent = (downloaded as f64 / total as f64 * 100.0) as u32;
            print!("\rDownloading... {}%  ", percent);
            std::io::stdout().flush().ok();
        }
    }
    println!("\rDownload complete.    ");

    // Run the appropriate installer
    println!("Installing (requires sudo)...");
    println!();

    let install_result = match platform.os {
        "macos" => {
            let status = Command::new("sudo")
                .args(["installer", "-pkg"])
                .arg(&temp_file)
                .args(["-target", "/"])
                .status()?;

            if status.success() {
                Ok(())
            } else {
                Err("macOS installer failed".into())
            }
        }
        "linux" => {
            // Try dpkg first
            let status = Command::new("sudo")
                .args(["dpkg", "-i"])
                .arg(&temp_file)
                .status()?;

            if status.success() {
                Ok(())
            } else {
                Err(
                    "dpkg install failed. You may need to run: sudo apt --fix-broken install"
                        .into(),
                )
            }
        }
        // Windows is handled earlier via PowerShell script (exits before reaching here)
        _ => Err(format!("Unsupported OS for automatic install: {}", platform.os).into()),
    };

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_file);

    install_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0, None)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3, None)));
        assert_eq!(
            parse_version("0.4.0-beta.1"),
            Some((0, 4, 0, Some("beta.1".to_string())))
        );
        assert_eq!(parse_version("invalid"), None);
        assert_eq!(parse_version("0.4"), None);
    }

    #[test]
    fn test_is_newer_version() {
        // Newer versions
        assert!(is_newer_version("0.4.0", "0.5.0"));
        assert!(is_newer_version("0.4.0", "0.4.1"));
        assert!(is_newer_version("0.4.0", "1.0.0"));
        assert!(is_newer_version("0.4.0-beta.1", "0.4.0"));

        // Same or older versions
        assert!(!is_newer_version("0.5.0", "0.4.0"));
        assert!(!is_newer_version("0.4.0", "0.4.0"));
        assert!(!is_newer_version("0.4.0", "0.4.0-beta.1"));
    }

    #[test]
    fn test_requested_version_validation() {
        assert!(is_valid_requested_version("1.4.0"));
        assert!(is_valid_requested_version("2.0.0-beta.1"));
        assert!(!is_valid_requested_version("latest"));
        assert!(!is_valid_requested_version("1.9"));
        assert!(!is_valid_requested_version("1.4.0/extra"));
        assert!(!is_valid_requested_version("1.4.0\""));
    }

    #[test]
    fn test_platform_info_detect() {
        // This test depends on the current platform
        let info = PlatformInfo::detect_with_base_url(DEFAULT_RELEASE_BASE_URL);
        assert!(info.is_some());

        let info = info.unwrap();
        assert!(!info.package_name.is_empty());
        assert!(!info.download_url.is_empty());
        assert!(info.download_url.starts_with(DEFAULT_RELEASE_BASE_URL));
        // Verify no version number in package name (latest folder uses versionless names)
        assert!(!info.package_name.contains("0."));
    }
}
