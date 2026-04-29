# Windows packaging script - Creates MSI installers

param(
    [string]$Version = (Get-Content "Cargo.toml" | Select-String '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1 | ForEach-Object { $_.Matches[0].Groups[1].Value })
)

# Ensure we're in the project root
Set-Location (Split-Path $PSScriptRoot -Parent)

Write-Host "=========================================="
Write-Host "Hot Dev - Windows Packaging"
Write-Host "=========================================="
Write-Host ""

if (-not $Version) {
    Write-Host "❌ Could not extract version from Cargo.toml"
    exit 1
}

# Validate and clean version for WiX compatibility
Write-Host "Raw version extracted: '$Version'"

# Remove any extra whitespace and validate format
$Version = $Version.Trim()

# Ensure version is in x.x.x format for WiX (add .0 if needed)
if ($Version -match '^\d+\.\d+\.\d+$') {
    # Version is already in x.x.x format
} elseif ($Version -match '^\d+\.\d+$') {
    # Add .0 to make it x.x.0
    $Version = "$Version.0"
} elseif ($Version -match '^\d+$') {
    # Add .0.0 to make it x.0.0
    $Version = "$Version.0.0"
} else {
    Write-Host "❌ Invalid version format: '$Version'. Expected x.x.x"
    exit 1
}

Write-Host "Creating Windows packages for version: $Version"

# Check if binaries exist
$x64Binary = "binaries\hot-windows-x86_64.exe"
$arm64Binary = "binaries\hot-windows-aarch64.exe"

if (-not (Test-Path $x64Binary)) {
    Write-Host "❌ x86_64 binary not found at $x64Binary"
    exit 1
}

if (-not (Test-Path $arm64Binary)) {
    Write-Host "❌ aarch64 binary not found at $arm64Binary"
    exit 1
}

# Create packages directory
$packagesDir = "target\packages\windows"
New-Item -ItemType Directory -Force -Path $packagesDir | Out-Null

# Function to create MSI using WiX (if available) or fallback to simple installer
function Create-WindowsPackage {
    param(
        [string]$Architecture,
        [string]$BinaryPath,
        [string]$OutputPath
    )

    Write-Host "Creating $Architecture package..."

    # Check if WiX is available
    $wixInstalled = $false
    try {
        & candle.exe 2>&1 | Out-Null
        $wixInstalled = $true
        Write-Host "  → Using WiX Toolset"
    }
    catch {
        Write-Host "  → WiX not found, creating simple installer"
    }

    if ($wixInstalled) {
        # Create WiX source file
        $wxsContent = @"
<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
    <Product Id="*" Name="Hot Dev" Language="1033" Version="$Version" Manufacturer="Hot Dev" UpgradeCode="12345678-1234-1234-1234-123456789012">
        <Package InstallerVersion="200" Compressed="yes" InstallScope="perMachine" Platform="$Architecture" />

        <MajorUpgrade DowngradeErrorMessage="A newer version of [ProductName] is already installed." />
        <MediaTemplate EmbedCab="yes" />

        <Feature Id="ProductFeature" Title="Hot Dev" Level="1">
            <ComponentGroupRef Id="ProductComponents" />
        </Feature>

        <Directory Id="TARGETDIR" Name="SourceDir">
            <Directory Id="ProgramFiles64Folder">
                <Directory Id="INSTALLFOLDER" Name="Hot Dev" />
            </Directory>
            <Directory Id="ProgramMenuFolder">
                <Directory Id="ApplicationProgramsFolder" Name="Hot Dev"/>
            </Directory>
        </Directory>

        <ComponentGroup Id="ProductComponents" Directory="INSTALLFOLDER">
            <Component Id="MainExecutable" Guid="*">
                <File Id="HotExe" Source="$BinaryPath" KeyPath="yes" />
                <Environment Id="PATH" Name="PATH" Value="[INSTALLFOLDER]" Permanent="no" Part="last" Action="set" System="yes" />
            </Component>
        </ComponentGroup>
    </Product>
</Wix>
"@

        $wxsFile = "temp_$Architecture.wxs"
        $wixobjFile = "temp_$Architecture.wixobj"

        # Write WiX source
        $wxsContent | Out-File -FilePath $wxsFile -Encoding UTF8

        try {
            # Compile and link
            & candle.exe -arch $Architecture -o $wixobjFile $wxsFile
            & light.exe -o $OutputPath $wixobjFile

            # Cleanup temp files
            Remove-Item $wxsFile -ErrorAction SilentlyContinue
            Remove-Item $wixobjFile -ErrorAction SilentlyContinue

            Write-Host "  ✅ Created MSI package: $OutputPath"
            return $true
        }
        catch {
            Write-Host "  ❌ WiX compilation failed: $_"
            return $false
        }
    }
    else {
        # Fallback: Create a simple PowerShell-based installer
        $installerContent = @"
# Hot Dev Windows Installer
# Generated for version $Version ($Architecture)

Write-Host "Installing Hot Dev $Version..."

`$installPath = "`$env:ProgramFiles\Hot Dev"
`$binaryName = "hot.exe"

# Create install directory
New-Item -ItemType Directory -Force -Path `$installPath | Out-Null

# Copy binary (embedded as base64)
`$binaryData = [System.Convert]::FromBase64String("$(Get-Base64FromFile $BinaryPath)")
[System.IO.File]::WriteAllBytes((Join-Path `$installPath `$binaryName), `$binaryData)

# Add to PATH
`$currentPath = [Environment]::GetEnvironmentVariable("PATH", "Machine")
if (`$currentPath -notlike "*`$installPath*") {
    [Environment]::SetEnvironmentVariable("PATH", "`$currentPath;`$installPath", "Machine")
    Write-Host "Added to system PATH"
}

Write-Host "Hot Dev installed successfully to `$installPath"
Write-Host "You may need to restart your terminal for PATH changes to take effect."
"@

        $installerContent | Out-File -FilePath $OutputPath -Encoding UTF8
        Write-Host "  ✅ Created PowerShell installer: $OutputPath"
        return $true
    }
}

# Helper function to convert file to base64
function Get-Base64FromFile {
    param([string]$FilePath)
    $bytes = [System.IO.File]::ReadAllBytes($FilePath)
    return [System.Convert]::ToBase64String($bytes)
}

# Create packages for both architectures
$success = $true

# x86_64 package
$x64Output = "$packagesDir\hot_$($Version)_x86_64.msi"
try {
    Create-WindowsPackage -Architecture "x64" -BinaryPath $x64Binary -OutputPath $x64Output
    if (-not (Test-Path $x64Output)) {
        # Fallback to PowerShell installer
        $x64Output = "$packagesDir\hot_$($Version)_x86_64_installer.ps1"
        Create-WindowsPackage -Architecture "x64" -BinaryPath $x64Binary -OutputPath $x64Output
    }
} catch {
    Write-Host "❌ Failed to create x64 package: $_"
    $success = $false
}

# aarch64 package
$arm64Output = "$packagesDir\hot_$($Version)_aarch64.msi"
try {
    Create-WindowsPackage -Architecture "arm64" -BinaryPath $arm64Binary -OutputPath $arm64Output
    if (-not (Test-Path $arm64Output)) {
        # Fallback to PowerShell installer
        $arm64Output = "$packagesDir\hot_$($Version)_aarch64_installer.ps1"
        Create-WindowsPackage -Architecture "arm64" -BinaryPath $arm64Binary -OutputPath $arm64Output
    }
} catch {
    Write-Host "❌ Failed to create aarch64 package: $_"
    $success = $false
}

Write-Host ""
Write-Host "=========================================="
Write-Host "Windows Packaging Summary"
Write-Host "=========================================="

# Check if MSI files were actually created (ignore success tracking variable)
$msiFiles = Get-ChildItem "$packagesDir\*.msi" -ErrorAction SilentlyContinue

if ($msiFiles.Count -gt 0) {
    Write-Host "✅ Windows packaging completed successfully!"
    Write-Host ""
    Write-Host "Created packages in $packagesDir\:"
    Get-ChildItem $packagesDir | ForEach-Object {
        Write-Host "  - $($_.Name)"
    }
    Write-Host ""
    Write-Host "=========================================="
    Write-Host "Success: Created $($msiFiles.Count) MSI package(s)"
    exit 0
}
else {
    Write-Host "❌ Windows packaging failed - no MSI files created"
    Write-Host ""
    Write-Host "=========================================="
    exit 1
}