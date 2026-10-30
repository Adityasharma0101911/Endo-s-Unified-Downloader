; The Windows installer: the GUI, the CLI and the browser extension, installed for the current
; user only (no admin) into %LOCALAPPDATA%\Programs\Endo's Unified Downloader, a folder the app can
; update itself in. Built by .github/workflows/packages.yml after `cargo build --release --locked`:
;   iscc /DAppVersion=X.Y.Z packaging\windows\installer.iss
; which writes dist\Endos-Unified-Downloader-vX.Y.Z-windows-x64-setup.exe.
; Uninstalling keeps the downloads and the data folder (%LOCALAPPDATA%\EndosUnifiedDownloader).

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=X.Y.Z installer.iss
#endif

#define AppName "Endo's Unified Downloader"
#define GuiExe "Endos-Unified-Downloader.exe"

[Setup]
AppId={{45CE9D6F-9031-44A4-A9B4-727B24AD5F92}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Endo's Unified Downloader contributors
AppPublisherURL=https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader
AppUpdatesURL=https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader/releases
PrivilegesRequired=lowest
DefaultDirName={userpf}\{#AppName}
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#GuiExe}
OutputDir=..\..\dist
OutputBaseFilename=Endos-Unified-Downloader-v{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\..\assets\logo.ico

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; The extension an older install or the updater left goes first, so no stale file stays in it.
Type: filesandordirs; Name: "{app}\extension"

[Files]
Source: "..\..\target\release\{#GuiExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\Endos-Unified-Downloader-CLI.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\NOTICE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\extension\*"; DestDir: "{app}\extension"; Excludes: "\test\*,\package.json"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#GuiExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#GuiExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#GuiExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What the updater leaves next to the programs: the extension it replaced, the old programs it
; keeps until the next start, and the files of a swap that was cut short.
Type: filesandordirs; Name: "{app}\extension"
Type: filesandordirs; Name: "{app}\.extension-*"
Type: files; Name: "{app}\*.old.exe"
Type: files; Name: "{app}\*.old-*.exe"
Type: files; Name: "{app}\*.exe.new"

[Code]
// The app writes these itself (launch at startup, and the endo-dl: link that starts it); they
// would point at the programs just removed.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'EndosUnifiedDownloader');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\endo-dl');
  end;
end;
