#!/bin/bash
set -e

# Function to strip markdown formatting for plain text display
# Converts LICENSE.md to readable plain text for installer license pages
# Also converts Unicode characters to ASCII for NSIS compatibility
strip_markdown() {
    local input="$1"
    local output="$2"

    # Use perl for consistent regex handling
    perl -pe '
        # Strip markdown formatting
        s/^#+ //;                           # Remove header markers
        s/\*\*([^*]+)\*\*/$1/g;            # Remove bold markers
        s/\*([^*]+)\*/$1/g;                # Remove italic markers
        s/`([^`]+)`/$1/g;                  # Remove code markers
        s/\[([^\]]+)\]\(\1\)/$1/g;         # Remove redundant links where text = url
        s/\[([^\]]+)\]\(([^)]+)\)/$1 ($2)/g;  # Convert other links to "text (url)"
        s/^---$/________________________________________/;  # Horizontal rules
        s/^- /  * /;                        # Bullet points (ASCII)

        # Convert Unicode to ASCII for NSIS compatibility
        s/©/(c)/g;                          # Copyright
        s/®/(R)/g;                          # Registered trademark
        s/™/(TM)/g;                         # Trademark
        s/—/--/g;                           # Em dash
        s/–/-/g;                            # En dash
        s/[""]/"/g;                         # Smart quotes to straight quotes
        s/['\'\']/'"'"'/g;                  # Smart apostrophes to straight
        s/…/.../g;                          # Ellipsis
        s/•/*/g;                            # Bullet
    ' "$input" > "$output"
}

echo "Creating Windows installer packages..."

# Extract version from resources/version.txt (single source of truth)
VERSION=$(head -1 /workspace/resources/version.txt | tr -d '[:space:]')
echo "Package version: $VERSION"

# Create output directory
mkdir -p /workspace/target/packages

# Function to create installer for a specific architecture
create_installer_for_arch() {
    local arch=$1
    local binary_name=$2

    echo "Creating installer for $arch..."

    # Create staging directory
    local staging_dir="/workspace/staging/$arch"
    rm -rf "$staging_dir"
    mkdir -p "$staging_dir"

    # Copy binary
    cp "/workspace/target/docker-builds/windows/$binary_name" "$staging_dir/hot.exe"

    # Copy resources needed at runtime:
    # - db/ for migrations
    # - pkg/ for hot-std (standard library)
    # - ai/ for AI AGENTS.md and skills
    # - init/ for project initialization templates
    # - LICENSE.md for license display

    # Copy database migrations
    if [ -d "/workspace/resources/db" ]; then
        mkdir -p "$staging_dir/resources/db"
        cp -r /workspace/resources/db/* "$staging_dir/resources/db/"
    fi

    # Copy hot-std (standard library)
    if [ -d "/workspace/hot/pkg/hot-std" ]; then
        cp -r /workspace/hot/pkg/hot-std "$staging_dir/"
    fi

    # Copy app assets (CSS, JS, images for dev server)
    if [ -d "/workspace/resources/app" ]; then
        mkdir -p "$staging_dir/resources/app"
        cp -r /workspace/resources/app/* "$staging_dir/resources/app/"
    fi

    # Copy AI resources (AGENTS.md and skills)
    if [ -d "/workspace/resources/ai" ]; then
        mkdir -p "$staging_dir/resources/ai"
        cp -r /workspace/resources/ai/* "$staging_dir/resources/ai/"
    fi

    # Copy init templates
    if [ -d "/workspace/resources/init" ]; then
        mkdir -p "$staging_dir/resources/init"
        cp -r /workspace/resources/init/* "$staging_dir/resources/init/"
    fi

    # Copy license and notice files to resources
    if [ -f "/workspace/LICENSE" ]; then
        cp /workspace/LICENSE "$staging_dir/resources/LICENSE"
    fi
    if [ -f "/workspace/NOTICE" ]; then
        cp /workspace/NOTICE "$staging_dir/resources/NOTICE"
    fi

    # Copy license file for installer display
    if [ -f "/workspace/LICENSE" ]; then
        cp /workspace/LICENSE "$staging_dir/LICENSE.txt"
    else
        echo "Apache License 2.0 - See https://www.apache.org/licenses/LICENSE-2.0" > "$staging_dir/LICENSE.txt"
    fi

    # Convert PNG icon to ICO format for Windows installer
    if [ -f "/workspace/resources/application/icons/hot_icon.png" ]; then
        echo "Converting hot_icon.png to ICO format..."
        # Create multi-resolution ICO (16x16, 32x32, 48x48, 64x64, 128x128, 256x256)
        convert "/workspace/resources/application/icons/hot_icon.png" \
            -define icon:auto-resize=256,128,64,48,32,16 \
            "$staging_dir/hot_icon.ico"
    else
        echo "Warning: hot_icon.png not found, installer will use default icon"
        # Create empty placeholder so NSIS doesn't fail
        touch "$staging_dir/hot_icon.ico"
    fi

    # Copy installer branding images (header and welcome banner)
    if [ -f "/workspace/resources/installer/win/hot_header.bmp" ]; then
        cp "/workspace/resources/installer/win/hot_header.bmp" "$staging_dir/"
    fi
    if [ -f "/workspace/resources/installer/win/hot_welcome.bmp" ]; then
        cp "/workspace/resources/installer/win/hot_welcome.bmp" "$staging_dir/"
    fi

    # Create the NSIS script directly (avoids plugin compatibility issues)
    cat > "$staging_dir/installer.nsi" << 'NSIS_SCRIPT'
; Hot Dev Windows Installer Script
; Simple version without external plugins for maximum compatibility

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "WinMessages.nsh"
!include "LogicLib.nsh"

; Product info
!define PRODUCT_NAME "Hot Dev"
!define PRODUCT_PUBLISHER "Hot Dev, LLC"
!define PRODUCT_WEB_SITE "https://hot.dev"
!define PRODUCT_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
!define PRODUCT_UNINST_ROOT_KEY "HKLM"

; Installer attributes
Name "${PRODUCT_NAME} ${VERSION}"
OutFile "hot_${VERSION}_${ARCH}_installer.exe"
InstallDir "$PROGRAMFILES64\Hot Dev"
InstallDirRegKey HKLM "${PRODUCT_UNINST_KEY}" "InstallLocation"
ShowInstDetails show
ShowUnInstDetails show
RequestExecutionLevel admin

; Modern UI settings
!define MUI_ABORTWARNING
!define MUI_ICON "hot_icon.ico"
!define MUI_UNICON "hot_icon.ico"

; Header image (top right on most pages)
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_BITMAP "hot_header.bmp"
!define MUI_HEADERIMAGE_RIGHT

; Welcome/Finish page image (left panel)
!define MUI_WELCOMEFINISHPAGE_BITMAP "hot_welcome.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "hot_welcome.bmp"

!define MUI_WELCOMEPAGE_TITLE "Welcome to ${PRODUCT_NAME} Setup"
!define MUI_WELCOMEPAGE_TEXT "This wizard will guide you through the installation of ${PRODUCT_NAME} ${VERSION}.$\r$\n$\r$\nHot is a programming language for automating workflows and integrating APIs.$\r$\n$\r$\nClick Next to continue."

!define MUI_LICENSEPAGE_CHECKBOX

!define MUI_FINISHPAGE_TITLE "Installation Complete"
!define MUI_FINISHPAGE_TEXT "${PRODUCT_NAME} has been installed on your computer.$\r$\n$\r$\nTo start using Hot, open a NEW terminal window and type: hot --help$\r$\n$\r$\nClick Finish to close this wizard."
!define MUI_FINISHPAGE_LINK "Visit ${PRODUCT_WEB_SITE}"
!define MUI_FINISHPAGE_LINK_LOCATION "${PRODUCT_WEB_SITE}"

; Pages
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Install section
Section "Hot Dev" SEC_MAIN
    SectionIn RO

    ; Clean up old installation files to prevent stale files from being found
    ; Remove old resources directory (will be recreated with fresh files)
    RMDir /r "$INSTDIR\resources"
    ; Remove legacy paths from older installers
    RMDir /r "$INSTDIR\pkg"
    RMDir /r "$INSTDIR\db"
    RMDir /r "$INSTDIR\app"
    RMDir /r "$INSTDIR\hot-std"
    Delete "$INSTDIR\hot.exe"

    ; Install main executable
    SetOutPath "$INSTDIR"
    SetOverwrite on
    File "hot.exe"

    ; Install runtime resources (db/, ai/, init/)
    SetOutPath "$INSTDIR\resources\db"
    File /nonfatal /r "resources\db\*.*"

    ; Install hot-std (standard library) - copy entire directory including pkg.hot and tests
    SetOutPath "$INSTDIR\resources\pkg"
    File /nonfatal /r "hot-std"

    ; Install app assets (CSS, JS, images for dev server)
    SetOutPath "$INSTDIR\resources\app"
    File /nonfatal /r "resources\app\*.*"

    ; Install AI resources (AGENTS.md and skills)
    SetOutPath "$INSTDIR\resources\ai"
    File /nonfatal /r "resources\ai\*.*"

    ; Install init templates
    SetOutPath "$INSTDIR\resources\init"
    File /nonfatal /r "resources\init\*.*"

    ; Create uninstaller
    SetOutPath "$INSTDIR"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Write registry keys for Add/Remove Programs
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "DisplayName" "${PRODUCT_NAME}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "Publisher" "${PRODUCT_PUBLISHER}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "URLInfoAbout" "${PRODUCT_WEB_SITE}"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegDWORD ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "NoModify" 1
    WriteRegDWORD ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "NoRepair" 1

    ; Get installed size
    ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
    IntFmt $0 "0x%08X" $0
    WriteRegDWORD ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}" "EstimatedSize" "$0"

    ; Add to system PATH
    Call AddToPath

SectionEnd

; Uninstall section
Section "Uninstall"
    ; Remove from PATH
    Call un.RemoveFromPath

    ; Remove all installed files
    Delete "$INSTDIR\hot.exe"
    Delete "$INSTDIR\uninstall.exe"
    RMDir /r "$INSTDIR\resources"
    ; Remove legacy paths from older installers
    RMDir /r "$INSTDIR\pkg"
    RMDir /r "$INSTDIR\db"
    RMDir /r "$INSTDIR\app"
    RMDir /r "$INSTDIR\hot-std"
    ; Remove install directory (only if empty)
    RMDir "$INSTDIR"

    ; Remove registry keys
    DeleteRegKey ${PRODUCT_UNINST_ROOT_KEY} "${PRODUCT_UNINST_KEY}"
SectionEnd

; Function to add install directory to PATH
Function AddToPath
    ; Read current PATH
    ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path"

    ; Check if already in PATH
    StrCpy $1 $0
    StrCpy $2 "$INSTDIR"

    ; Simple check - if INSTDIR is found in PATH, skip
    Push $1
    Push $2
    Call StrContains
    Pop $3
    StrCmp $3 "" 0 PathExists

    ; Add to PATH
    StrCpy $1 "$0;$INSTDIR"
    WriteRegExpandStr HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path" "$1"

    ; Broadcast environment change to all windows
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000

    DetailPrint "Added $INSTDIR to system PATH"
    Goto PathDone

PathExists:
    DetailPrint "$INSTDIR is already in PATH"

PathDone:
FunctionEnd

; Function to remove install directory from PATH
Function un.RemoveFromPath
    ; Read current PATH
    ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path"

    ; Remove our directory (simple approach - may leave trailing semicolon)
    Push $0
    Push ";$INSTDIR"
    Call un.StrReplace
    Pop $1

    Push $1
    Push "$INSTDIR;"
    Call un.StrReplace
    Pop $2

    Push $2
    Push "$INSTDIR"
    Call un.StrReplace
    Pop $3

    ; Write back
    WriteRegExpandStr HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path" "$3"

    ; Broadcast environment change
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd

; Helper function: Check if string contains substring
; Usage: Push "haystack" / Push "needle" / Call StrContains / Pop $result
Function StrContains
    Exch $1 ; needle
    Exch
    Exch $0 ; haystack
    Push $2
    Push $3

    StrLen $2 $1
    StrLen $3 $0

    ${If} $2 > $3
        StrCpy $0 ""
        Goto StrContainsDone
    ${EndIf}

    StrCpy $3 0

StrContainsLoop:
    StrCpy $4 $0 $2 $3
    StrCmp $4 $1 StrContainsFound
    IntOp $3 $3 + 1
    StrLen $4 $0
    IntCmp $3 $4 0 0 StrContainsNotFound
    Goto StrContainsLoop

StrContainsFound:
    StrCpy $0 "found"
    Goto StrContainsDone

StrContainsNotFound:
    StrCpy $0 ""

StrContainsDone:
    Pop $3
    Pop $2
    Exch $0
    Exch
    Pop $1
FunctionEnd

; Helper function for uninstaller: Simple string replace
Function un.StrReplace
    Exch $1 ; find
    Exch
    Exch $0 ; string
    Push $2
    Push $3
    Push $4
    Push $5

    StrLen $2 $1
    StrCpy $3 0

StrReplaceLoop:
    StrCpy $4 $0 $2 $3
    StrCmp $4 $1 StrReplaceFound
    IntOp $3 $3 + 1
    StrLen $5 $0
    IntCmp $3 $5 0 0 StrReplaceDone
    Goto StrReplaceLoop

StrReplaceFound:
    StrCpy $4 $0 $3
    IntOp $5 $3 + $2
    StrCpy $5 $0 "" $5
    StrCpy $0 "$4$5"

StrReplaceDone:
    Pop $5
    Pop $4
    Pop $3
    Pop $2
    Exch $0
    Exch
    Pop $1
FunctionEnd

; Check for admin rights and running processes on init
Function .onInit
    UserInfo::GetAccountType
    Pop $0
    StrCmp $0 "Admin" +3
        MessageBox MB_OK|MB_ICONSTOP "Administrator rights required to install ${PRODUCT_NAME}."
        Abort

    ; Check if hot.exe is running and offer to close it
    Call CheckAndCloseRunningProcesses
FunctionEnd

; Function to check for and close running hot.exe processes
Function CheckAndCloseRunningProcesses
    ; Use tasklist to check if hot.exe is running
    nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq hot.exe" /NH'
    Pop $0  ; Exit code
    Pop $1  ; Output

    ; Check if "hot.exe" appears in the output (means it's running)
    Push $1
    Push "hot.exe"
    Call StrContains
    Pop $2

    StrCmp $2 "" ProcessNotRunning

    ; Process is running - notify user and offer to close
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION \
        "Hot Dev is currently running.$\r$\n$\r$\nThe installer needs to close all running instances of Hot to continue.$\r$\n$\r$\nClick OK to close Hot and continue installation, or Cancel to abort." \
        IDOK CloseProcess IDCANCEL AbortInstall

CloseProcess:
    ; Force kill all hot.exe processes
    nsExec::ExecToLog 'taskkill /F /IM hot.exe'

    ; Brief delay to allow process to fully terminate
    Sleep 1000

    ; Verify process was closed
    nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq hot.exe" /NH'
    Pop $0
    Pop $1

    Push $1
    Push "hot.exe"
    Call StrContains
    Pop $2

    StrCmp $2 "" ProcessClosed

    ; Still running - warn user
    MessageBox MB_OK|MB_ICONEXCLAMATION \
        "Unable to close all Hot processes. Please close them manually and try again."
    Abort

ProcessClosed:
    DetailPrint "Closed running Hot processes"
    Goto ProcessNotRunning

AbortInstall:
    Abort

ProcessNotRunning:
FunctionEnd
NSIS_SCRIPT

    # Replace VERSION and ARCH placeholders in the NSIS script
    sed -i "s/\${VERSION}/$VERSION/g" "$staging_dir/installer.nsi"
    sed -i "s/\${ARCH}/$arch/g" "$staging_dir/installer.nsi"

    # Build the installer
    cd "$staging_dir"

    echo "Running makensis for $arch..."
    makensis -V3 installer.nsi

    # Move installer to output directory
    mv "hot_${VERSION}_${arch}_installer.exe" "/workspace/target/packages/"

    # Sign binaries and installer if certificate is available
    if [ -f "/workspace/certs/codesign.pfx" ] && [ -n "$CODESIGN_PASSWORD" ]; then
        echo "Signing hot.exe binary..."
        osslsigncode sign \
            -pkcs12 /workspace/certs/codesign.pfx \
            -pass "$CODESIGN_PASSWORD" \
            -n "Hot Dev" \
            -i "https://hot.dev" \
            -t "http://timestamp.digicert.com" \
            -in "$staging_dir/hot.exe" \
            -out "$staging_dir/hot_signed.exe"
        mv "$staging_dir/hot_signed.exe" "$staging_dir/hot.exe"
        echo "Binary signed successfully."

        echo "Signing installer..."
        osslsigncode sign \
            -pkcs12 /workspace/certs/codesign.pfx \
            -pass "$CODESIGN_PASSWORD" \
            -n "Hot Dev" \
            -i "https://hot.dev" \
            -t "http://timestamp.digicert.com" \
            -in "/workspace/target/packages/hot_${VERSION}_${arch}_installer.exe" \
            -out "/workspace/target/packages/hot_${VERSION}_${arch}_installer_signed.exe"
        mv "/workspace/target/packages/hot_${VERSION}_${arch}_installer_signed.exe" \
           "/workspace/target/packages/hot_${VERSION}_${arch}_installer.exe"
        echo "Installer signed successfully."
    fi

    echo "Created: hot_${VERSION}_${arch}_installer.exe"
}

# Create installer for x86_64
if [ -f "/workspace/target/docker-builds/windows/hot-windows-x86_64.exe" ]; then
    create_installer_for_arch "x86_64" "hot-windows-x86_64.exe"
else
    echo "Warning: x86_64 binary not found, skipping..."
fi

# Create installer for aarch64 (if available)
if [ -f "/workspace/target/docker-builds/windows/hot-windows-aarch64.exe" ]; then
    create_installer_for_arch "aarch64" "hot-windows-aarch64.exe"
else
    echo "Note: aarch64 binary not found, skipping ARM64 installer."
fi

echo ""
echo "Windows packaging completed!"
echo "Installers created in target/packages/"
ls -la /workspace/target/packages/ 2>/dev/null || echo "(no files created)"
