; Arcade V1.0 one-click installer for Windows 10/11 x64: arcade-v1-windows-x86_64-setup.exe
;
; Double-click, and it installs for the current user (no administrator password), starts the
; arcade service now and at every login, adds Arcade V1.0 to the Start menu and the desktop, and
; opens it. Running a newer Setup.exe later updates in place; the wallet and chains are kept.
; It lists itself in Settings > Apps, where Uninstall removes the programs and keeps the data.
;
; The release zip is not compiled into this stub. The release workflow hashes that zip, passes
; the hash as /DPAYLOAD_SHA256, then appends the zip with scripts/release/payload_trailer.py.
; At install time Verify-Payload.ps1 hashes those appended bytes and compares them to the hash
; in this file (also stored in the version resource, which Authenticode covers). Nothing is
; unpacked, and install.ps1 does not run, unless they match.
;
;   makensis /NOCD /DVERSION=v0.1.77 /DPKG=arcade-v1-windows-x86_64 /DOUT=stub.exe /DPAYLOAD_SHA256=<64 hex> scripts\windows\setup.nsi
;   python scripts\release\payload_trailer.py append --stub stub.exe --payload arcade-v1-windows-x86_64-payload.zip --out arcade-v1-windows-x86_64-setup.exe
;
; (/NOCD: PKG and OUT are relative to where makensis runs. On Linux: apt install nsis, and -NOCD -D...)
; Sign setup.exe after the append. Signing before the append would put the payload outside the
; signed range, and the trailer reader ignores bytes past the certificate table.

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
!ifndef PAYLOAD_SHA256
  !error "PAYLOAD_SHA256 is required: the lowercase SHA-256 of the payload zip appended after makensis"
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
!define MUI_FINISHPAGE_TEXT "Arcade V1.0 is installed, and its service is running: it starts by itself every time you log in.$\r$\n$\r$\nOpen it from the Start menu or the desktop. The first time, make a wallet (write its 12 words down) or restore one.$\r$\n$\r$\nArcade V1.0 is in beta: testing is happening now, and things can change or break (test coins have no value).$\r$\nThank you for trying it out! Questions, problems or ideas: team@arcade-nodes.com"
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
; Plain text in the version resource, so the release check can read the hash back
; out of the signed stub. Verify-Payload.ps1 is given the same value.
VIAddVersionKey "Comments" "ARCADE_PAYLOAD_SHA256=${PAYLOAD_SHA256}"

Function .onInit
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  ; The verifier is part of this stub, not part of the release payload.
  File "/oname=$PLUGINSDIR\Verify-Payload.ps1" "scripts\windows\Verify-Payload.ps1"
FunctionEnd

Function OpenArcade
  ; The same launcher as the Start menu shortcut: it sets the data folder for the app.
  IfFileExists "$INSTDIR\arcade-desktop.vbs" 0 +2
    Exec '"$WINDIR\System32\wscript.exe" //B "$INSTDIR\arcade-desktop.vbs"'
FunctionEnd

Section "Arcade V1.0" SecMain
  SectionIn RO
  DetailPrint "Checking the installer payload against the SHA-256 embedded in this installer..."
  DetailPrint "ARCADE_PAYLOAD_SHA256=${PAYLOAD_SHA256}"
  ; Verify-Payload.ps1 hashes the appended zip in memory. Expand-Archive runs only
  ; after the hash matches, and only inside that script. A mismatch does not write
  ; the payload and does not get to install.ps1.
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\Verify-Payload.ps1" -Installer "$EXEPATH" -Expected "${PAYLOAD_SHA256}" -DestDir "$PLUGINSDIR\pkg"'
  Pop $0
  StrCmp $0 "0" payload_ok
    Delete "$PLUGINSDIR\payload.zip"
    RMDir /r "$PLUGINSDIR\pkg"
    MessageBox MB_ICONSTOP "Arcade V1.0 was not installed.$\r$\n$\r$\nThe installer checked its payload against the SHA-256 embedded in this installer, and they do not match (or the payload is missing).$\r$\n$\r$\nNothing was extracted and nothing was installed. Download arcade-v1-windows-x86_64-setup.exe again from the release page.$\r$\n$\r$\nShow details has the expected hash and the hash that was found."
    Abort "The installer payload does not match the embedded SHA-256. Nothing was extracted and nothing was installed."
  payload_ok:

  IfFileExists "$PLUGINSDIR\pkg\${PKG}\scripts\install.ps1" have_script
    RMDir /r "$PLUGINSDIR\pkg"
    MessageBox MB_ICONSTOP "Arcade V1.0 was not installed.$\r$\n$\r$\nThe payload matched its SHA-256 but did not contain the installer script. Nothing else was installed."
    Abort "The payload did not contain the installer script. Nothing was installed."
  have_script:

  DetailPrint "Installing (this stops a running arcade first so it can finish writing)..."
  ; ARCADE_NO_OPEN: the finish page offers to open it instead, also on updates.
  System::Call 'kernel32::SetEnvironmentVariable(t "ARCADE_NO_OPEN", t "1")'
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\pkg\${PKG}\scripts\install.ps1" --prebuilt "$PLUGINSDIR\pkg\${PKG}"'
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
