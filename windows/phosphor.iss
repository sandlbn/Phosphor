; Inno Setup script for Phosphor (Windows installer).
;
; Do not run this file directly with a hardcoded version — the version is
; injected by windows\build_installer.ps1 so Cargo.toml stays the single source
; of truth:
;
;   iscc.exe /DMyAppVersion=0.4.4 windows\phosphor.iss
;
; The build script also signs both target\release\phosphor.exe (before this
; runs) and the installer this produces (afterwards), so no [Setup] SignTool is
; configured here.

#ifndef MyAppVersion
  #error "Define MyAppVersion, e.g. iscc /DMyAppVersion=0.4.4 windows\phosphor.iss"
#endif

#define MyAppName "Phosphor"
#define MyAppPublisher "Marcin Spoczynski"
#define MyAppURL "https://github.com/sandlbn/Phosphor"
#define MyAppExeName "phosphor.exe"

[Setup]
AppId={{9F4B6E2A-3C7D-4A1E-9B2F-PHOSPHORSID01}}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\Phosphor
DefaultGroupName=Phosphor
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
SetupIconFile=..\assets\phosphor.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
OutputDir=..\dist
OutputBaseFilename=Phosphor-{#MyAppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; 64-bit only build.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\phosphor.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion

[Icons]
Name: "{group}\Phosphor"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,Phosphor}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Phosphor"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,Phosphor}"; Flags: nowait postinstall skipifsilent

; Note: user data lives in %APPDATA%\phosphor and is intentionally left in place
; on uninstall (config, playlists, HVSC cache). Delete it manually for a full wipe.
