//! Update checking functionality for the Hot CLI
//!
//! Checks for newer versions from the configured release channel
//! and provides download URLs for the appropriate platform/architecture.

use crate::build_info;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default base URL for official release downloads.
const DEFAULT_RELEASE_BASE_URL: &str = "https://get.hot.dev/releases/latest";

/// Default Windows installer URL for official package installs.
const DEFAULT_WINDOWS_INSTALLER_URL: &str = "https://get.hot.dev/install.ps1";

/// The macOS and Linux installer script.
const INSTALL_SCRIPT_URL: &str = "https://get.hot.dev/install.sh";

/// The file `install.sh` (from the release archive) and the per-user macOS
/// package leave in `<prefix>/share/hot`, so `hot update` can update that
/// install in place.
const ARCHIVE_RECEIPT: &str = "install-receipt.json";

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
        // A release's own folder is named by its bare version, as the release
        // workflow uploads it: releases/2.6.11/hot_2.6.11_macos_arm64.pkg.
        Err(_) => Some(match target_version {
            Some(version) => format!(
                "https://get.hot.dev/releases/{}",
                version.trim().trim_start_matches('v')
            ),
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
        Self::detect(release_base_url, None)
    }

    /// Like `detect_with_base_url`, for a specific release: its folder names
    /// packages with the version, as `hot_2.6.11_macos_arm64.pkg`.
    pub fn detect_for_version(release_base_url: &str, version: &str) -> Option<Self> {
        Self::detect(release_base_url, Some(version.trim_start_matches('v')))
    }

    fn detect(release_base_url: &str, version: Option<&str>) -> Option<Self> {
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

        let extension = match os {
            // An install made from the release archive updates from it
            _ if matches!(detect_install_method(), InstallMethod::Archive { .. }) => "tar.gz",
            "macos" => "pkg",   // hot_macos_{arch}.pkg
            "linux" => "deb",   // hot_linux_{arch}.deb
            "windows" => "exe", // hot_windows_{arch}.exe
            _ => return None,   // Unsupported OS
        };
        let package_name = match version {
            Some(version) => format!("hot_{version}_{package_os}_{package_arch}.{extension}"),
            None => format!("hot_{package_os}_{package_arch}.{extension}"),
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

/// Whether moving from `current` to `latest` crosses a major version.
pub fn is_major_upgrade(current: &str, latest: &str) -> bool {
    matches!(
        (parse_version(current), parse_version(latest)),
        (Some(current), Some(latest)) if latest.0 > current.0
    )
}

/// Ask before `hot update` moves to a new major version. Without a terminal
/// to ask on, it doesn't upgrade and prints the explicit command instead.
pub fn confirm_major_upgrade(latest: &str) -> bool {
    use is_terminal::IsTerminal;
    use std::io::Write;
    println!("Hot {latest} is a new major version. Read the upgrade notes first:");
    println!("  https://hot.dev/docs/migrations");
    println!();
    if !std::io::stdin().is_terminal() {
        println!("Run `hot update --version {latest}` to upgrade.");
        return false;
    }
    print!("Upgrade to {latest}? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
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
        // A custom base URL keeps the latest-folder package names.
        let platform = if std::env::var("HOT_UPDATE_BASE_URL").is_ok() {
            PlatformInfo::detect_with_base_url(&release_base_url)
        } else {
            PlatformInfo::detect_for_version(&release_base_url, version)
        };
        let platform = match platform {
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
    let client = hot::outbound::client_builder()
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
    /// Installed by `install.sh` from the release archive, as
    /// `<prefix>/bin/hot` and `<prefix>/share/hot` (a user install by default)
    Archive { prefix: PathBuf },
    /// Installed via package installer or other method
    Package,
}

/// The prefix of an install that `install.sh` made from the release archive:
/// the binary is `<prefix>/bin/hot` and its receipt is in `<prefix>/share/hot`.
fn archive_install_prefix(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().ok()?;
    let bin_dir = exe.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    let prefix = bin_dir.parent()?;
    prefix
        .join("share")
        .join("hot")
        .join(ARCHIVE_RECEIPT)
        .is_file()
        .then(|| prefix.to_path_buf())
}

/// The installer records its scope beside the executable. Older per-user
/// installers can be recognized by their HKCU uninstall registration, even
/// when the user chose a directory outside LOCALAPPDATA.
fn is_windows_user_install(
    exe: &Path,
    local_app_data: Option<&Path>,
    user_uninstaller: Option<&str>,
) -> bool {
    if let Some(dir) = exe.parent()
        && let Ok(scope) = std::fs::read_to_string(dir.join("install-scope"))
    {
        match scope.trim() {
            "user" => return true,
            "system" => return false,
            _ => {}
        }
    }
    let normalize = |path: &str| {
        path.trim_matches('"')
            .replace('/', "\\")
            .trim_start_matches(r"\\?\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let exe = normalize(&exe.to_string_lossy());
    if let Some(uninstaller) = user_uninstaller {
        let uninstaller = normalize(uninstaller);
        if let Some(dir) = uninstaller.strip_suffix(r"\uninstall.exe")
            && exe == format!(r"{dir}\hot.exe")
        {
            return true;
        }
    }

    // Compatibility with an older default install whose registration is gone.
    let Some(local_app_data) = local_app_data else {
        return false;
    };
    let base = normalize(&local_app_data.to_string_lossy());
    !base.is_empty()
        && exe
            .strip_prefix(&base)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn windows_user_uninstaller() -> Option<String> {
    // NSIS writes its uninstall registration in the 32-bit view. Use JSON and
    // UTF-8 output so paths containing non-ASCII characters survive capture.
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding; \
             $key = [Microsoft.Win32.RegistryKey]::OpenBaseKey(\
                 [Microsoft.Win32.RegistryHive]::CurrentUser, \
                 [Microsoft.Win32.RegistryView]::Registry32).OpenSubKey(\
                 'Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\HotDev'); \
             if ($key) { $key.GetValue('UninstallString') | ConvertTo-Json -Compress }",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn windows_update_command(
    user_install: bool,
    install_dir: &Path,
    target_version: Option<&str>,
    installer_url: &str,
) -> String {
    let scope = if user_install { "user" } else { "system" };
    let mut script = format!(
        "$env:HOT_INSTALL_SCOPE={}; $env:HOT_INSTALL_DIR={}; ",
        powershell_literal(scope),
        powershell_literal(&install_dir.to_string_lossy())
    );
    if let Some(version) = target_version {
        script.push_str(&format!(
            "$env:HOT_VERSION={}; ",
            powershell_literal(version)
        ));
    }
    script.push_str(&format!("irm {} | iex", powershell_literal(installer_url)));
    format!(
        "Start-Process powershell -ArgumentList @('-ExecutionPolicy','Bypass','-Command',{}){}",
        powershell_literal(&script),
        if user_install { "" } else { " -Verb RunAs" }
    )
}

/// Detect how Hot was installed by checking the binary path
pub fn detect_install_method() -> InstallMethod {
    // Get the path to the current executable
    let exe_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return InstallMethod::Package,
    };

    if let Some(prefix) = archive_install_prefix(&exe_path) {
        return InstallMethod::Archive { prefix };
    }

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

        // Spawn PowerShell to run the install script, for the install that's
        // running: a user install updates without administrator rights.
        let exe = std::env::current_exe()?;
        let install_dir = exe.parent().ok_or("Could not find the install directory")?;
        let user_install = is_windows_user_install(
            &exe,
            std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
            windows_user_uninstaller().as_deref(),
        );
        let ps_command = windows_update_command(
            user_install,
            install_dir,
            target_version,
            &windows_installer_url(),
        );
        Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-Command", &ps_command])
            .spawn()?;

        // Exit immediately so the installer can replace hot.exe
        std::process::exit(0);
    }

    let client = download_client()?;

    if let InstallMethod::Archive { prefix } = &install_method {
        return install_archive(&client, platform, prefix).await;
    }

    // Download the package into a directory only this user can write, since
    // root runs whatever is there
    let temp_dir = tempfile::Builder::new().prefix("hot-update-").tempdir()?;
    let temp_file = temp_dir.path().join(&platform.package_name);

    let expected = fetch_sha256(&client, &platform.download_url).await?;
    download(&client, &platform.download_url, &temp_file).await?;
    match expected {
        Some(expected) => check_sha256(&platform.download_url, &temp_file, &expected)?,
        // Releases before checksums were published still install
        None => eprintln!(
            "Warning: this release publishes no checksum for {}; not verified.",
            platform.package_name
        ),
    }

    // Run the appropriate installer
    println!("Installing (requires sudo)...");
    println!();

    match platform.os {
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
    }
}

fn download_client() -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    let user_agent = format!("hot/{}", crate::build_info::VERSION);
    Ok(hot::outbound::client_builder()
        .timeout(std::time::Duration::from_secs(300)) // 5 min timeout for large downloads
        .user_agent(&user_agent)
        .build()?)
}

/// Download `url` to `dest`, showing progress
async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = url.rsplit('/').next().unwrap_or(url);
    println!("Downloading {}...", name);

    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Err(format!("Download failed: server returned {}", response.status()).into());
    }

    let total_size = response.content_length();
    let mut downloaded: u64 = 0;
    let mut file = std::fs::File::create(dest)?;

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
    Ok(())
}

/// The SHA-256 the release publishes for `url`, as `<url>.sha256`, or `None`
/// when it publishes none (releases before user installs).
async fn fetch_sha256(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let response = client.get(format!("{url}.sha256")).send().await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let sums = response.text().await?;
    let expected = sums
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("The checksum file at {url}.sha256 is empty"))?;
    Ok(Some(expected.to_ascii_lowercase()))
}

/// Check `file`, downloaded from `url`, against its published SHA-256
fn check_sha256(
    url: &str,
    file: &Path,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = url.rsplit('/').next().unwrap_or(url);
    let mut hasher = Sha256::new();
    let mut reader = std::fs::File::open(file)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(
            format!("Checksum mismatch for {name}: expected {expected}, got {actual}").into(),
        );
    }
    Ok(())
}

/// Update an install made from the release archive in place, as its owner and
/// without sudo: `<prefix>/bin/hot` and `<prefix>/share/hot`. Everything is
/// staged inside the prefix, so each replacement is a rename, and a failure
/// leaves the old install working.
async fn install_archive(
    client: &reqwest::Client,
    platform: &PlatformInfo,
    prefix: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bin_dir = prefix.join("bin");
    let share_hot = prefix.join("share").join("hot");

    let stage = match tempfile::Builder::new()
        .prefix(".hot-update-")
        .tempdir_in(prefix.join("share"))
    {
        Ok(stage) => stage,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(format!(
                "You can't write to {}. To update it, run the installer as its owner, or with sudo:\n  \
                 curl -fsSL {INSTALL_SCRIPT_URL} | sh -s -- --system --prefix {}",
                prefix.display(),
                prefix.display()
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    };

    // Only releases whose hot runs from any folder publish the archive's
    // checksum, so an older one can't replace this install.
    let Some(expected) = fetch_sha256(client, &platform.download_url).await? else {
        return Err(format!(
            "This Hot release predates user installs, so it can't run from {}. \
             Install it for the whole machine instead:\n  \
             curl -fsSL {INSTALL_SCRIPT_URL} | sh -s -- --system",
            prefix.display()
        )
        .into());
    };
    let archive = stage.path().join(&platform.package_name);
    download(client, &platform.download_url, &archive).await?;
    check_sha256(&platform.download_url, &archive, &expected)?;

    println!("Installing into {}...", prefix.display());
    let unpacked = stage.path().join("hot");
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(&archive)?);
    tar::Archive::new(gz)
        .unpack(&unpacked)
        .map_err(|e| format!("Could not unpack {}: {}", platform.package_name, e))?;
    if !unpacked.join("hot").is_file() || !unpacked.join("pkg").join("hot-std").is_dir() {
        return Err(format!(
            "{} is missing the hot binary or its standard library",
            platform.package_name
        )
        .into());
    }
    std::fs::write(
        unpacked.join(ARCHIVE_RECEIPT),
        "{\"method\": \"archive\"}\n",
    )?;

    // The new binary waits beside the old one
    let new_exe = bin_dir.join(format!(".hot.new.{}", std::process::id()));
    std::fs::copy(unpacked.join("hot"), &new_exe)?;
    std::fs::remove_file(unpacked.join("hot"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_exe, std::fs::Permissions::from_mode(0o755))?;
    }

    // Swap the resources, then the binary. The old resources go into the
    // stage, which is removed when it drops.
    let old_resources = stage.path().join("old");
    let swapped = std::fs::rename(&share_hot, &old_resources)
        .and_then(|()| match std::fs::rename(&unpacked, &share_hot) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::rename(&old_resources, &share_hot);
                Err(e)
            }
        })
        .and_then(|()| match std::fs::rename(&new_exe, bin_dir.join("hot")) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::rename(&share_hot, &unpacked);
                let _ = std::fs::rename(&old_resources, &share_hot);
                Err(e)
            }
        });
    if let Err(e) = swapped {
        let _ = std::fs::remove_file(&new_exe);
        return Err(format!(
            "Could not replace the install in {}: {}",
            prefix.display(),
            e
        )
        .into());
    }
    Ok(())
}

/// What to suggest after `hot update` fails to install. `brew_command` is
/// the Homebrew command for this update, when Homebrew can do it.
pub fn print_install_failure_help(
    platform: &PlatformInfo,
    target_version: Option<&str>,
    brew_command: Option<&str>,
) {
    let version_arg = match target_version {
        Some(version) => format!(" --version {version}"),
        None => String::new(),
    };
    match (detect_install_method(), brew_command) {
        (InstallMethod::Homebrew, Some(command)) => {
            eprintln!("You can update manually with:");
            eprintln!("  {command}");
        }
        (InstallMethod::Archive { prefix }, _) => {
            eprintln!("You can reinstall with:");
            eprintln!(
                "  curl -fsSL {INSTALL_SCRIPT_URL} | sh -s -- --prefix {}{version_arg}",
                prefix.display()
            );
        }
        _ => {
            eprintln!("You can download manually from:");
            eprintln!("  {}", platform.download_url);
            if platform.os != "windows" {
                eprintln!();
                eprintln!("Or install Hot just for you, without sudo:");
                eprintln!("  curl -fsSL {INSTALL_SCRIPT_URL} | sh -s -- --user{version_arg}");
            }
        }
    }
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
    fn only_an_install_with_a_receipt_is_an_archive_install() {
        let root = tempfile::tempdir().unwrap();
        let prefix = root.path().join(".local");
        let exe = prefix.join("bin").join("hot");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(prefix.join("share").join("hot")).unwrap();
        std::fs::write(&exe, "").unwrap();

        // A package or hand-made install has no receipt.
        assert_eq!(archive_install_prefix(&exe), None);

        std::fs::write(
            prefix.join("share").join("hot").join(ARCHIVE_RECEIPT),
            "{\"method\": \"archive\"}\n",
        )
        .unwrap();
        assert_eq!(
            archive_install_prefix(&exe),
            Some(prefix.canonicalize().unwrap())
        );
    }

    #[test]
    fn a_windows_user_install_lives_under_local_app_data() {
        let local = Some(Path::new(r"C:\Users\Ada\AppData\Local"));
        assert!(is_windows_user_install(
            Path::new(r"C:\Users\Ada\AppData\Local\Programs\Hot Dev\hot.exe"),
            local,
            None
        ));
        // Paths ignore case on Windows
        assert!(is_windows_user_install(
            Path::new(r"c:\users\ada\appdata\local\Programs\Hot Dev\hot.exe"),
            Some(Path::new(r"C:\Users\Ada\AppData\Local\")),
            None
        ));
        assert!(!is_windows_user_install(
            Path::new(r"C:\Program Files\Hot Dev\hot.exe"),
            local,
            None
        ));
        // A sibling folder that only shares the prefix
        assert!(!is_windows_user_install(
            Path::new(r"C:\Users\Ada\AppData\LocalLow\Hot Dev\hot.exe"),
            local,
            None
        ));
        assert!(!is_windows_user_install(
            Path::new(r"C:\Users\Ada\AppData\Local\Programs\Hot Dev\hot.exe"),
            None,
            None
        ));
    }

    #[test]
    fn a_custom_windows_install_keeps_its_recorded_scope() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("hot.exe");
        let receipt = root.path().join("install-scope");
        std::fs::write(&receipt, "user").unwrap();
        assert!(is_windows_user_install(&exe, None, None));

        std::fs::write(&receipt, "system").unwrap();
        assert!(!is_windows_user_install(&exe, Some(root.path()), None));
    }

    #[test]
    fn an_older_custom_user_install_matches_its_uninstall_registration() {
        let exe = Path::new(r"\\?\C:\Users\Ada\Tools\Hot Dev\hot.exe");
        let uninstaller = Some(r#""c:\users\ada\tools\Hot Dev\uninstall.exe""#);
        assert!(is_windows_user_install(exe, None, uninstaller));
        assert!(!is_windows_user_install(
            Path::new(r"C:\Program Files\Hot Dev\hot.exe"),
            None,
            uninstaller
        ));
    }

    #[test]
    fn windows_updates_pass_the_directory_and_quote_powershell_literals() {
        let command = windows_update_command(
            true,
            Path::new(r"C:\Users\O'Brien\Tools\Hot Dev"),
            Some("3.0.2"),
            DEFAULT_WINDOWS_INSTALLER_URL,
        );
        assert!(command.contains("HOT_INSTALL_SCOPE=''user''"));
        assert!(command.contains(r"HOT_INSTALL_DIR=''C:\Users\O''''Brien\Tools\Hot Dev''"));
        assert!(command.contains("HOT_VERSION=''3.0.2''"));
        assert!(!command.contains("-Verb RunAs"));

        let command = windows_update_command(
            false,
            Path::new(r"D:\Tools\Hot Dev"),
            None,
            DEFAULT_WINDOWS_INSTALLER_URL,
        );
        assert!(command.contains("HOT_INSTALL_SCOPE=''system''"));
        assert!(command.contains(r"HOT_INSTALL_DIR=''D:\Tools\Hot Dev''"));
        assert!(command.ends_with(" -Verb RunAs"));
    }

    #[test]
    fn test_major_upgrade_detection() {
        assert!(is_major_upgrade("2.6.11", "3.0.0"));
        assert!(!is_major_upgrade("2.6.11", "2.7.0"));
        assert!(!is_major_upgrade("3.0.0", "3.0.1"));
        assert!(!is_major_upgrade("3.0.0", "garbage"));
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

    #[test]
    fn test_platform_info_for_a_release_uses_its_folder_names() {
        let base = "https://get.hot.dev/releases/2.6.11";
        let info = PlatformInfo::detect_for_version(base, "v2.6.11").unwrap();
        assert!(
            info.package_name.starts_with("hot_2.6.11_"),
            "{}",
            info.package_name
        );
        assert!(
            info.download_url
                .starts_with("https://get.hot.dev/releases/2.6.11/hot_2.6.11_"),
            "{}",
            info.download_url
        );
    }
}
