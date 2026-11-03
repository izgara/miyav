; Miyav installer. Build with: makensis -DVERSION=x.y.z miyav.nsi
;
; Installs per user into %LOCALAPPDATA%\Programs, so there is no admin prompt and
; the app can update itself. Everything it needs is inside: nothing is downloaded
; at first start.

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"

!define APPNAME "Miyav"
!define EXENAME "Miyav.exe"
!define REGKEY  "Software\Microsoft\Windows\CurrentVersion\Uninstall\Miyav"
!define RUNKEY  "Software\Microsoft\Windows\CurrentVersion\Run"

Name              "${APPNAME}"
OutFile           "dist/Miyav-${VERSION}-setup.exe"
InstallDir        "$LOCALAPPDATA\Programs\Miyav"
InstallDirRegKey  HKCU "Software\Miyav" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
SetCompressorDictSize 64
ShowInstDetails   show
ShowUnInstDetails hide

VIProductVersion "${VERSION}.0"
VIAddVersionKey  "ProductName"     "${APPNAME}"
VIAddVersionKey  "FileDescription" "${APPNAME} setup"
VIAddVersionKey  "FileVersion"     "${VERSION}"
VIAddVersionKey  "LegalCopyright"  "GPL-3.0-or-later"

!define MUI_ICON   "ui/app.ico"
!define MUI_UNICON "ui/app.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Open ${APPNAME}"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; The app holds its own exe open while running, so close it before writing.
!macro CloseRunning
  nsExec::Exec 'taskkill /IM "${EXENAME}" /F'
  Pop $0
  ; The recorder dies with the app, but make sure: it holds the OBS files.
  nsExec::Exec 'taskkill /IM miyav-obs.exe /F'
  Pop $0
  Sleep 400
!macroend

Section "Install"
  !insertmacro CloseRunning
  SetOutPath "$INSTDIR"
  File "Miyav.exe"
  File "WebView2Loader.dll"
  File "ffmpeg.exe"
  File "LICENSE"
  File "NOTICE.md"
  ; The OBS files the recorder runs on (obs/fetch.sh). The old copy goes first, so
  ; nothing from an earlier OBS version lingers beside the new one.
  RMDir /r "$INSTDIR\obs"
  File /r "obs/stage/obs"

  WriteRegStr HKCU "Software\Miyav" "InstallDir" "$INSTDIR"
  ; "Start with Windows" left on by a portable copy now starts this one.
  ReadRegStr $0 HKCU "${RUNKEY}" "Miyav"
  StrCmp $0 "" +2
  WriteRegStr HKCU "${RUNKEY}" "Miyav" "$\"$INSTDIR\${EXENAME}$\" --tray"

  CreateShortcut "$SMPROGRAMS\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"
  CreateShortcut "$DESKTOP\${APPNAME}.lnk"    "$INSTDIR\${EXENAME}"

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Add/Remove Programs entry
  WriteRegStr   HKCU "${REGKEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr   HKCU "${REGKEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${REGKEY}" "DisplayIcon"     "$INSTDIR\${EXENAME}"
  WriteRegStr   HKCU "${REGKEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegStr   HKCU "${REGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${REGKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${REGKEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${REGKEY}" "EstimatedSize" "$0"

  ; A silent install is the app updating itself (rust/src/update.rs): it closed for
  ; the update, so open the new version again, into the tray.
  IfSilent 0 +2
    Exec '"$INSTDIR\${EXENAME}" --tray'
SectionEnd

Section "Uninstall"
  !insertmacro CloseRunning

  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\WebView2Loader.dll"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\NOTICE.md"
  RMDir /r "$INSTDIR\obs"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir  "$INSTDIR"

  Delete "$SMPROGRAMS\${APPNAME}.lnk"
  Delete "$DESKTOP\${APPNAME}.lnk"

  DeleteRegKey HKCU "${REGKEY}"
  DeleteRegKey HKCU "Software\Miyav"
  DeleteRegValue HKCU "${RUNKEY}" "Miyav"

  ; Settings in %APPDATA%\Miyav and your videos are left in place.
SectionEnd
