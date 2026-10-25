; Gantry installer for Windows (Inno Setup 6). Built by the release workflow:
;   iscc /DAppVer=v5.5.0 packaging\windows\gantry.iss      (run from the repository folder, after cargo build --release)
; It installs for the current user only (no administrator rights), adds a Start menu entry and an uninstaller,
; and optionally a desktop shortcut and the nxm:// link handler for the Nexus Mods download button.
#ifndef AppVer
  #define AppVer "dev"
#endif

[Setup]
AppId={{EA420FD8-F17F-458B-9097-C903D77B87B0}
AppName=Gantry
AppVersion={#AppVer}
AppPublisher=pigeoonn
AppPublisherURL=https://github.com/pigeoonn/gantry
AppSupportURL=https://github.com/pigeoonn/gantry/issues
AppUpdatesURL=https://github.com/pigeoonn/gantry/releases
DefaultDirName={autopf}\Gantry
DefaultGroupName=Gantry
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..
OutputBaseFilename=gantry-{#AppVer}-windows-x86_64-setup
SetupIconFile=gantry.ico
UninstallDisplayIcon={app}\rig.exe
LicenseFile=..\..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
Name: "nxm"; Description: "Open Nexus Mods download links (nxm://) with Gantry"; Flags: unchecked

[Files]
Source: "..\..\target\release\rig.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\docs\INSTALL.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Gantry"; Filename: "{app}\rig.exe"
Name: "{autodesktop}\Gantry"; Filename: "{app}\rig.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\rig.exe"; Parameters: "--register-nxm"; Tasks: nxm; Flags: runhidden
Filename: "{app}\rig.exe"; Description: "Start Gantry"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\rig.exe"; Parameters: "--unregister-nxm"; Flags: runhidden; RunOnceId: "GantryUnregisterNxm"
