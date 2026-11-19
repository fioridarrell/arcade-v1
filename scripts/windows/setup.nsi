; Arcade V1.0 one-click installer for Windows 10/11 x64: arcade-v1-windows-x86_64-setup.exe
;
; Double-click, and it installs for the current user (no administrator password), starts the
; arcade service now and at every login, adds Arcade V1.0 to the Start menu and the desktop, and
; opens it. Running a newer Setup.exe later updates in place; the wallet and chains are kept.
; It lists itself in Settings > Apps, where Uninstall removes the programs and keeps the data.
;
; All the installing is scripts\install.ps1 (--prebuilt): this only carries the release folder to
; it, so the two never drift apart. Built by the release workflow from the packaged folder:
;
;   makensis /NOCD /DVERSION=v0.1.77 /DPKG=arcade-v1-windows-x86_64 /DOUT=arcade-v1-windows-x86_64-setup.exe scripts\windows\setup.nsi
;
; (/NOCD: PKG and OUT are relative to where makensis runs. On Linux: apt install nsis, and -NOCD -D...)

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "dev"
!endif
!ifndef PKG
  !define PKG "arcade-v1-windows-x86_64"
!endif
!ifndef OUT
  !define OUT "arcade-v1-windows-x86_64-setup.exe"
!endif

!define NAME "Arcade V1.0"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ArcadeV1"

Name "${NAME}"
OutFile "${OUT}"
Icon "${PKG}\icon.ico"
UninstallIcon "${PKG}\icon.ico"
InstallDir "$LOCALAPPDATA\Programs\Arcade"
BrandingText "${NAME} ${VERSION}"
ShowInstDetails show
ShowUninstDetails show

!include "MUI2.nsh"
!define MUI_ICON "${PKG}\icon.ico"
!define MUI_UNICON "${PKG}\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open Arcade V1.0"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenArcade
!define MUI_FINISHPAGE_TEXT "Arcade V1.0 is installed, and its service is running: it starts by itself every time you log in.$\r$\n$\r$\nOpen it from the Start menu or the desktop. The first time, make a wallet (write its 12 words down) or restore one."
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

VIProductVersion "0.0.0.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Open source"

Function OpenArcade
  ; The same launcher as the Start menu shortcut: it sets the data folder for the app.
  IfFileExists "$INSTDIR\arcade-desktop.vbs" 0 +2
    Exec '"$WINDIR\System32\wscript.exe" //B "$INSTDIR\arcade-desktop.vbs"'
FunctionEnd

Section "Arcade V1.0" SecMain
  SectionIn RO
  DetailPrint "Unpacking ${NAME} ${VERSION}..."
  SetOutPath "$PLUGINSDIR\pkg"
  File /r "${PKG}\*"

  DetailPrint "Installing (this stops a running arcade first so it can finish writing)..."
  ; ARCADE_NO_OPEN: the finish page offers to open it instead, also on updates.
  System::Call 'kernel32::SetEnvironmentVariable(t "ARCADE_NO_OPEN", t "1")'
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\pkg\scripts\install.ps1" --prebuilt "$PLUGINSDIR\pkg"'
  Pop $0
  StrCmp $0 "0" installed
    MessageBox MB_ICONSTOP "The install did not finish (code $0). Click Show details for what happened, or run the installer again."
    SetOutPath "$TEMP"
    Abort "Install failed."
  installed:

  SetOutPath "$TEMP"
  IfFileExists "$INSTDIR\arcade-desktop.vbs" 0 +2
    CreateShortcut "$DESKTOP\Arcade V1.0.lnk" "$WINDIR\System32\wscript.exe" '//B "$INSTDIR\arcade-desktop.vbs"' "$INSTDIR\arcade-v1.ico" 0 SW_SHOWMINIMIZED "" "Arcade V1.0"

  WriteUninstaller "$INSTDIR\Uninstall Arcade V1.0.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "ARCADE"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "https://github.com/martinseeger2002/arcade-v1"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\arcade-v1.ico"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall Arcade V1.0.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall Arcade V1.0.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  ; Programs, login task, shortcuts and PATH entry go; the data folder (wallet, 12 words,
  ; chains) stays, so installing again picks it up.
  SetOutPath "$PLUGINSDIR"
  File "/oname=$PLUGINSDIR\uninstall.ps1" "${PKG}\scripts\uninstall.ps1"
  SetOutPath "$TEMP"
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\uninstall.ps1" --keep-data'
  Pop $0
  Delete "$DESKTOP\Arcade V1.0.lnk"
  Delete "$INSTDIR\Uninstall Arcade V1.0.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINST_KEY}"
  IfSilent +2
    MessageBox MB_ICONINFORMATION "Arcade V1.0 is removed. Your wallet, its 12 words and your chains are kept in $LOCALAPPDATA\arcade (or the folder you chose): installing again picks them up. Delete that folder yourself only once your 12 words are written down."
SectionEnd
