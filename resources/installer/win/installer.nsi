; Hot Dev installer for Windows.
;
;   makensis /DVERSION=<version> installer.nsi
;       Installs for every user of the machine, in Program Files, with
;       administrator rights.
;   makensis /DVERSION=<version> /DPER_USER installer.nsi
;       Installs for the current user, in %LOCALAPPDATA%\Programs, without
;       administrator rights. install.ps1 uses this one by default.
;
; The two keep separate PATH entries and uninstall registrations (HKLM or
; HKCU), so both can be installed at once.

!include "MUI2.nsh"
!include "WinMessages.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
    !error "Pass the version: makensis /DVERSION=<version> installer.nsi"
!endif

!ifdef PER_USER
    !define DISPLAY_NAME "Hot Dev (User)"
    !define REG_ROOT HKCU
    !define ENV_KEY "Environment"
    !define PATH_SCOPE "user"
    OutFile "hot_${VERSION}_x86_64_user_installer.exe"
    InstallDir "$LOCALAPPDATA\Programs\Hot Dev"
    RequestExecutionLevel user
!else
    !define DISPLAY_NAME "Hot Dev"
    !define REG_ROOT HKLM
    !define ENV_KEY "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
    !define PATH_SCOPE "system"
    OutFile "hot_${VERSION}_x86_64_installer.exe"
    InstallDir "$PROGRAMFILES64\Hot Dev"
    RequestExecutionLevel admin
!endif

Name "${DISPLAY_NAME}"

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

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Check for running processes on init
Function .onInit
    ; Check if hot.exe is running and offer to close it
    Call CheckAndCloseRunningProcesses

!ifdef PER_USER
    ; The machine PATH comes before the user's, so a machine install wins.
    ${If} ${FileExists} "$PROGRAMFILES64\Hot Dev\hot.exe"
        MessageBox MB_OK|MB_ICONINFORMATION \
            "Hot Dev is also installed for all users, in $PROGRAMFILES64\Hot Dev.$\r$\n$\r$\nWindows puts the machine PATH first, so that copy runs instead of this one until you uninstall it." \
            /SD IDOK
    ${EndIf}
!endif
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

; Helper function: Check if string contains substring
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

; Pushes "yes" if the PATH value exists in ${REG_ROOT}\${ENV_KEY}, else "no".
Function PathValueExists
    Push $R0
    Push $R1
    StrCpy $R0 0

PathValueLoop:
    ClearErrors
    EnumRegValue $R1 ${REG_ROOT} "${ENV_KEY}" $R0
    IfErrors PathValueMissing
    StrCmp $R1 "" PathValueMissing
    ; StrCmp ignores case, as Windows does for value names
    StrCmp $R1 "Path" PathValueFound
    IntOp $R0 $R0 + 1
    Goto PathValueLoop

PathValueFound:
    StrCpy $R1 "yes"
    Goto PathValueDone

PathValueMissing:
    StrCpy $R1 "no"

PathValueDone:
    Exch $R1
    Exch
    Pop $R0
FunctionEnd

; Add install directory to the ${PATH_SCOPE} PATH without external plugins.
; NSIS strings hold 1024 characters, and ReadRegStr returns an empty string
; for a longer PATH, so a PATH it can't read is left alone, not overwritten.
Function AddToPath
    ClearErrors
    ReadRegStr $0 ${REG_ROOT} "${ENV_KEY}" "Path"
    IfErrors 0 PathRead

    Call PathValueExists
    Pop $1
    StrCmp $1 "yes" 0 PathMissing
    DetailPrint "Your ${PATH_SCOPE} PATH is too long for the installer to change"
    MessageBox MB_OK|MB_ICONEXCLAMATION \
        "Your ${PATH_SCOPE} PATH is too long for the installer to change.$\r$\n$\r$\nAdd this folder to it yourself:$\r$\n$INSTDIR" \
        /SD IDOK
    Goto PathDone

PathMissing:
    ; A user without a PATH of their own yet
    StrCpy $1 "$INSTDIR"
    Goto PathWrite

PathRead:
    Push $0
    Push "$INSTDIR"
    Call StrContains
    Pop $1
    StrCmp $1 "" 0 PathExists

    StrCpy $1 "$0;$INSTDIR"

PathWrite:
    WriteRegExpandStr ${REG_ROOT} "${ENV_KEY}" "Path" "$1"
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000

    DetailPrint "Added $INSTDIR to ${PATH_SCOPE} PATH"
    Goto PathDone

PathExists:
    DetailPrint "$INSTDIR is already in PATH"

PathDone:
FunctionEnd

; Remove install directory from the ${PATH_SCOPE} PATH without external
; plugins. A PATH it can't read (missing, or too long) is left alone.
Function un.RemoveFromPath
    ClearErrors
    ReadRegStr $0 ${REG_ROOT} "${ENV_KEY}" "Path"
    IfErrors 0 RemovePathRead
    DetailPrint "Could not read the ${PATH_SCOPE} PATH; remove $INSTDIR from it yourself"
    Return

RemovePathRead:
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

    WriteRegExpandStr ${REG_ROOT} "${ENV_KEY}" "Path" "$3"
    SendMessage ${HWND_BROADCAST} ${WM_WININICHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd

; Helper function for uninstaller: Simple string replace.
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

Section "Install"
    ; Clean up old installation files to prevent stale files from being found
    ; Remove old resources directory (will be recreated with fresh files)
    RMDir /r "$INSTDIR\resources"
    ; Remove legacy paths from older installers
    RMDir /r "$INSTDIR\pkg"
    RMDir /r "$INSTDIR\db"
    RMDir /r "$INSTDIR\app"
    Delete "$INSTDIR\hot.exe"
    Delete "$INSTDIR\hot_icon.ico"

    ; Install fresh files
    SetOutPath "$INSTDIR"
    File "hot.exe"
    File "hot_icon.ico"

    SetOutPath "$INSTDIR\resources"
    File /r "resources\*.*"

    ; Create uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Preserve the scope even when the user chose a custom directory. The
    ; receipt lives beside hot.exe so hot update can find it in any folder.
    FileOpen $0 "$INSTDIR\install-scope" w
!ifdef PER_USER
    FileWrite $0 "user"
!else
    FileWrite $0 "system"
!endif
    FileClose $0

    ; Add to PATH
    Call AddToPath

    ; Start menu shortcuts, in a folder named for this install so a user and
    ; a machine install don't share one
    CreateDirectory "$SMPROGRAMS\${DISPLAY_NAME}"
    CreateShortcut "$SMPROGRAMS\${DISPLAY_NAME}\Hot Dev.lnk" "$INSTDIR\hot.exe"
    CreateShortcut "$SMPROGRAMS\${DISPLAY_NAME}\Uninstall.lnk" "$INSTDIR\uninstall.exe"

    ; Registry for Add/Remove Programs
    WriteRegStr ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev" "DisplayName" "${DISPLAY_NAME}"
    WriteRegStr ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev" "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev" "DisplayIcon" "$INSTDIR\hot_icon.ico"
    WriteRegStr ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev" "DisplayVersion" "${VERSION}"
    WriteRegStr ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev" "Publisher" "Hot Dev, LLC"
SectionEnd

Section "Uninstall"
    ; Remove all installed files
    Delete "$INSTDIR\hot.exe"
    Delete "$INSTDIR\hot_icon.ico"
    Delete "$INSTDIR\uninstall.exe"
    Delete "$INSTDIR\install-scope"
    RMDir /r "$INSTDIR\resources"
    ; Remove legacy paths from older installers
    RMDir /r "$INSTDIR\pkg"
    RMDir /r "$INSTDIR\db"
    RMDir /r "$INSTDIR\app"
    ; Remove install directory (only if empty)
    RMDir "$INSTDIR"

    ; Remove from PATH
    Call un.RemoveFromPath

    ; Remove shortcuts
    Delete "$SMPROGRAMS\${DISPLAY_NAME}\Hot Dev.lnk"
    Delete "$SMPROGRAMS\${DISPLAY_NAME}\Uninstall.lnk"
    RMDir "$SMPROGRAMS\${DISPLAY_NAME}"

    ; Remove registry
    DeleteRegKey ${REG_ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\HotDev"
SectionEnd
