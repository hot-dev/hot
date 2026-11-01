#
# Hot Dev Installer Script for Windows
# https://hot.dev
#
# Usage:
#   irm https://get.hot.dev/install.ps1 | iex
#   $env:HOT_VERSION = "1.4.0"; irm https://get.hot.dev/install.ps1 | iex
#   $env:HOT_INSTALL_SCOPE = "system"; irm https://get.hot.dev/install.ps1 | iex
#
# By default Hot installs for the current user, without administrator rights,
# in %LOCALAPPDATA%\Programs\Hot Dev. HOT_INSTALL_SCOPE=system installs it for
# every user of the machine, in Program Files, which asks for administrator
# rights. In CI, with a machine install already present, or for a release from
# before user installs, the default is system.
#

$ErrorActionPreference = "Stop"

$UserDir = Join-Path $env:LOCALAPPDATA "Programs\Hot Dev"
$SystemDir = Join-Path $env:ProgramFiles "Hot Dev"

$HotVersion = $env:HOT_VERSION
if ([string]::IsNullOrWhiteSpace($HotVersion)) {
    $HotVersion = "latest"
}
$HotVersion = $HotVersion.Trim().TrimStart('v')
if ($HotVersion -notmatch '^[A-Za-z0-9.-]+$') {
    Write-Host "Error: Invalid version: $HotVersion"
    exit 1
}

if (-not [string]::IsNullOrWhiteSpace($env:HOT_INSTALL_BASE_URL)) {
    # A mirror, like `hot update`'s HOT_UPDATE_BASE_URL, keeps the latest
    # folder's names
    $BaseUrl = $env:HOT_INSTALL_BASE_URL.TrimEnd('/')
    $FilePrefix = "hot"
}
elseif ($HotVersion -eq "latest") {
    $BaseUrl = "https://get.hot.dev/releases/latest"
    # The latest folder names files without the version: hot_windows_x86_64.exe
    $FilePrefix = "hot"
}
else {
    $BaseUrl = "https://get.hot.dev/releases/$HotVersion"
    $FilePrefix = "hot_$HotVersion"
}

function Get-Architecture {
    # Use PROCESSOR_ARCHITECTURE which works on all PowerShell versions
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        "AMD64" { return "x86_64" }
        "ARM64" {
            Write-Host "ARM64 Windows installer not yet available. Using x86_64 (runs via emulation)..."
            return "x86_64"
        }
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

function Test-Url {
    param([string]$Url)
    try {
        Invoke-WebRequest -Uri $Url -Method Head -UseBasicParsing | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

# Download $Url to $OutFile and check it against the release's $Url.sha256
# when the release publishes one
function Get-VerifiedFile {
    param([string]$Url, [string]$OutFile)

    Write-Host "Downloading $([System.IO.Path]::GetFileName($Url))..."
    try {
        Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
    }
    catch {
        Write-Host "Error: Download failed: $_"
        exit 1
    }

    try {
        $sums = (Invoke-WebRequest -Uri "$Url.sha256" -UseBasicParsing).Content
    }
    catch {
        Write-Host "Warning: this release publishes no checksum for $([System.IO.Path]::GetFileName($Url)); not verified."
        return
    }
    if ($sums -is [byte[]]) {
        $sums = [System.Text.Encoding]::ASCII.GetString($sums)
    }
    $expected = ($sums.Trim() -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Path $OutFile -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Write-Host "Error: Checksum mismatch for $([System.IO.Path]::GetFileName($Url)): expected $expected, got $actual"
        exit 1
    }
}

# Whether $Dir is in the user's or the machine's PATH, as stored
function Test-OnPath {
    param([string]$Dir, [string]$Scope)
    $path = [System.Environment]::GetEnvironmentVariable("Path", $Scope)
    if ([string]::IsNullOrEmpty($path)) {
        return $false
    }
    foreach ($entry in $path.Split(';')) {
        if ($entry.TrimEnd('\') -ieq $Dir.TrimEnd('\')) {
            return $true
        }
    }
    return $false
}

function Install-Hot {
    Write-Host ""
    Write-Host "Hot Dev Installer"
    Write-Host ""
    Write-Host "License: Apache-2.0"
    Write-Host ""
    Write-Host "By continuing, you accept the Apache License, Version 2.0."
    Write-Host ""

    $arch = Get-Architecture
    Write-Host "Detected platform: Windows $arch"

    $scope = "$env:HOT_INSTALL_SCOPE".Trim().ToLowerInvariant()
    $chosen = $scope -ne ""
    if ($chosen -and $scope -ne "user" -and $scope -ne "system") {
        Write-Host "Error: HOT_INSTALL_SCOPE must be user or system, not $env:HOT_INSTALL_SCOPE"
        exit 1
    }
    if (-not $chosen) {
        if (-not [string]::IsNullOrWhiteSpace($env:CI)) {
            # CI runners install for the machine, as they always have
            $scope = "system"
        }
        elseif (Test-Path (Join-Path $SystemDir "hot.exe")) {
            $scope = "system"
            Write-Host "Updating the existing install for all users. Set HOT_INSTALL_SCOPE=user to install for just you."
        }
        else {
            $scope = "user"
        }
    }

    $userUrl = "$BaseUrl/${FilePrefix}_windows_${arch}_user.exe"
    $systemUrl = "$BaseUrl/${FilePrefix}_windows_${arch}.exe"
    if ($scope -eq "user" -and -not (Test-Url $userUrl)) {
        if ($chosen) {
            Write-Host "Error: This Hot release ($HotVersion) has no installer for one user. Set HOT_INSTALL_SCOPE=system."
            exit 1
        }
        Write-Host "This Hot release ($HotVersion) has no installer for one user, so it installs for all users, as an administrator."
        $scope = "system"
    }

    if ($scope -eq "user") {
        $url = $userUrl
        $installDir = $UserDir
        $pathScope = "User"
    }
    else {
        $url = $systemUrl
        $installDir = $SystemDir
        $pathScope = "Machine"
    }

    # A folder of our own, so nothing else can swap the installer before it runs
    $tempDir = Join-Path $env:TEMP ("hot-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tempDir | Out-Null
    try {
        $installer = Join-Path $tempDir ([System.IO.Path]::GetFileName($url))
        Get-VerifiedFile -Url $url -OutFile $installer

        # The machine installer asks for administrator rights itself
        if ($scope -eq "user") {
            Write-Host "Installing Hot into $installDir..."
        }
        else {
            Write-Host "Installing Hot for all users (needs administrator rights)..."
        }
        try {
            $process = Start-Process -FilePath $installer -ArgumentList "/S" -Wait -PassThru
            if ($process.ExitCode -ne 0) {
                Write-Host "Error: Installation failed with exit code: $($process.ExitCode)"
                exit 1
            }
        }
        catch {
            Write-Host "Error: Installation failed: $_"
            exit 1
        }
    }
    finally {
        Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    # Refresh PATH for current session
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")

    # Verify installation
    Write-Host ""
    $hotExe = Join-Path $installDir "hot.exe"
    try {
        $version = & $hotExe version 2>$null
        if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
        Write-Host "Hot installed successfully!"
        Write-Host "Version: $version"
    }
    catch {
        Write-Host "Error: Installation completed, but $hotExe does not run."
        exit 1
    }

    if (-not (Test-OnPath -Dir $installDir -Scope $pathScope)) {
        Write-Host ""
        Write-Host "$installDir is not on your PATH (it may be too long for the installer to change)."
        Write-Host "Add it in Settings > System > About > Advanced system settings > Environment Variables."
    }
    else {
        Write-Host "Open a new terminal to use hot."
    }

    if ($scope -eq "user" -and (Test-Path (Join-Path $SystemDir "hot.exe"))) {
        Write-Host ""
        Write-Host "Hot is also installed for all users, in $SystemDir."
        Write-Host "Windows puts the machine PATH first, so that copy runs instead. To remove it:"
        Write-Host "  & `"$SystemDir\uninstall.exe`""
    }

    Write-Host ""
    Write-Host "Documentation: https://hot.dev/docs"
    Write-Host ""
}

# Run the installer
Install-Hot
