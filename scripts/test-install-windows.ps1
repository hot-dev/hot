# Exercise scope/directory forwarding without downloading or running an installer.
$ErrorActionPreference = 'Stop'
$root = Join-Path ([System.IO.Path]::GetTempPath()) ("hot-installer-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$saved = @{}
foreach ($name in @('LOCALAPPDATA', 'ProgramFiles', 'TEMP', 'HOT_INSTALL_SCOPE', 'HOT_INSTALL_DIR', 'HOT_VERSION')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name)
}
try {
    $env:LOCALAPPDATA = Join-Path $root 'Local'
    $env:ProgramFiles = Join-Path $root 'Program Files'
    $env:TEMP = $root
    $env:HOT_VERSION = '3.0.2'
    $source = Get-Content (Join-Path $PSScriptRoot '../resources/installer/install.ps1') -Raw
    # Stop after launching and cleaning up: verification would execute hot.exe.
    $source = $source.Substring(0, $source.IndexOf('    # Refresh PATH for current session')) + "`n}`n"
    . ([scriptblock]::Create($source))

    function Get-Architecture { return 'x86_64' }
    function Test-Url { return $true }
    function Get-VerifiedFile {
        param([string]$Url, [string]$OutFile)
        $script:downloadUrl = $Url
        Set-Content -Path $OutFile -Value 'stand-in installer'
    }
    function Start-Process {
        param($FilePath, $ArgumentList, [switch]$Wait, [switch]$PassThru)
        $script:installerArguments = $ArgumentList
        return @{ ExitCode = 0 }
    }

    foreach ($scope in @('user', 'system')) {
        $env:HOT_INSTALL_SCOPE = $scope
        $env:HOT_INSTALL_DIR = Join-Path $root "O'Brien/Tools/Hot Dev"
        Install-Hot
        if ($script:installerArguments -cne "/S /D=$env:HOT_INSTALL_DIR") {
            throw "Lost the custom install directory: $script:installerArguments"
        }
        if (($scope -eq 'user') -ne $script:downloadUrl.EndsWith('_user.exe')) {
            throw "Downloaded the wrong installer for $scope : $script:downloadUrl"
        }
    }
    $env:HOT_INSTALL_SCOPE = 'user'
    Remove-Item Env:HOT_INSTALL_DIR
    Install-Hot
    if ($script:installerArguments -cne "/S /D=$UserDir") {
        throw "Changed the default install directory: $script:installerArguments"
    }
    Write-Host 'Windows installer scope and directory tests passed.'
}
finally {
    foreach ($name in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name])
    }
    Remove-Item -Path $root -Recurse -Force
}
