; ModForge installer for Windows (Inno Setup 6). Built by the release workflow:
;   iscc /DAppVer=v6.0.0 packaging\windows\modforge.iss      (run from the repository folder, after cargo build --release)
; It installs for the current user only (no administrator rights), adds a Start menu entry and an uninstaller,
; and optionally a desktop shortcut and the nxm:// link handler for the Nexus Mods download button.
#ifndef AppVer
  #define AppVer "dev"
#endif

[Setup]
AppId={{EA420FD8-F17F-458B-9097-C903D77B87B0}
AppName=ModForge
AppVersion={#AppVer}
AppPublisher=David
AppPublisherURL=https://github.com/pigeoonn/modforge
AppSupportURL=https://github.com/pigeoonn/modforge/issues
AppUpdatesURL=https://github.com/pigeoonn/modforge/releases
DefaultDirName={autopf}\ModForge
DefaultGroupName=ModForge
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..
OutputBaseFilename=modforge-{#AppVer}-windows-x86_64-setup
SetupIconFile=modforge.ico
UninstallDisplayIcon={app}\modforge.exe
LicenseFile=..\..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
Name: "nxm"; Description: "Open Nexus Mods download links (nxm://) with ModForge"; Flags: unchecked

[Files]
Source: "..\..\target\release\modforge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\docs\INSTALL.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\ModForge"; Filename: "{app}\modforge.exe"
Name: "{autodesktop}\ModForge"; Filename: "{app}\modforge.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\modforge.exe"; Parameters: "--register-nxm"; Tasks: nxm; Flags: runhidden
Filename: "{app}\modforge.exe"; Description: "Start ModForge"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\modforge.exe"; Parameters: "--unregister-nxm"; Flags: runhidden; RunOnceId: "ModForgeUnregisterNxm"
