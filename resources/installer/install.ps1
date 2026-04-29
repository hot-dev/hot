#
# Hot Dev Installer Script for Windows
# https://hot.dev
#
# Usage:
#   irm https://get.hot.dev/install.ps1 | iex
#   $env:HOT_VERSION = "1.4.0"; irm https://get.hot.dev/install.ps1 | iex
#
# This script downloads and installs Hot silently on Windows.
#

$ErrorActionPreference = "Stop"

$HotVersion = $env:HOT_VERSION
if ([string]::IsNullOrWhiteSpace($HotVersion) -or $HotVersion -eq "latest") {
    $BaseUrl = "https://get.hot.dev/releases/latest"
}
else {
    $BaseUrl = "https://get.hot.dev/releases/$($HotVersion.TrimStart('v'))"
}

function Get-Architecture {
    # Use PROCESSOR_ARCHITECTURE which works on all PowerShell versions
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        "AMD64" { return "x86_64" }
        "ARM64" { return "arm64" }
        "x86" {
            # Check if running 32-bit PowerShell on 64-bit Windows
            if ($env:PROCESSOR_ARCHITEW6432 -eq "AMD64") {
                return "x86_64"
            }
            Write-Host "Error: 32-bit Windows is not supported"
            exit 1
        }
        default {
            Write-Host "Error: Unsupported architecture: $arch"
            exit 1
        }
    }
}

function Get-DownloadUrl {
    param([string]$Arch)

    switch ($Arch) {
        "x86_64" { return "$BaseUrl/hot_windows_x86_64.exe" }
        "arm64" {
            Write-Host "ARM64 Windows installer not yet available. Trying x86_64 (runs via emulation)..."
            return "$BaseUrl/hot_windows_x86_64.exe"
        }
        default {
            Write-Host "Error: No installer available for Windows $Arch"
            exit 1
        }
    }
}

function Install-Hot {
    Write-Host ""
    Write-Host "Hot Dev Installer"
    Write-Host ""
    Write-Host "License: Apache-2.0"
    Write-Host ""
    Write-Host "By continuing, you accept the Apache License, Version 2.0."
    Write-Host ""

    # Detect architecture
    $arch = Get-Architecture
    Write-Host "Detected platform: Windows $arch"

    # Get download URL
    $url = Get-DownloadUrl -Arch $arch
    $filename = [System.IO.Path]::GetFileName($url)
    $tempFile = Join-Path $env:TEMP $filename

    # Download
    Write-Host "Downloading $filename..."
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $url -OutFile $tempFile -UseBasicParsing
    }
    catch {
        Write-Host "Error: Download failed: $_"
        exit 1
    }

    # Install silently
    Write-Host "Installing Hot..."
    try {
        $process = Start-Process -FilePath $tempFile -ArgumentList "/S" -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            Write-Host "Error: Installation failed with exit code: $($process.ExitCode)"
            exit 1
        }
    }
    catch {
        Write-Host "Error: Installation failed: $_"
        exit 1
    }

    # Cleanup
    Remove-Item -Path $tempFile -Force -ErrorAction SilentlyContinue

    # Refresh PATH for current session
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")

    # Verify installation
    Write-Host ""
    $hotPath = Get-Command hot -ErrorAction SilentlyContinue
    if ($hotPath) {
        try {
            $version = & hot --version 2>$null
            Write-Host "Hot installed successfully!"
            Write-Host "Version: $version"
        }
        catch {
            Write-Host "Hot installed successfully!"
        }
    }
    else {
        Write-Host "Installation completed, but 'hot' command not found in PATH."
        Write-Host "Please restart your terminal or PowerShell session."
    }

    Write-Host ""
    Write-Host "Documentation: https://hot.dev/docs"
    Write-Host ""
}

# Run the installer
Install-Hot
