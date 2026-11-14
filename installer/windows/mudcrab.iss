; Inno Setup Script for Mudcrab Windows Installer
; Builds a modern Next-Next-Finish setup wizard (.exe)
;
; "Hey you, you're finally awake."

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#ifndef StagingDir
  #define StagingDir "..\..\target\x86_64-pc-windows-msvc\staging"
#endif

#ifndef OutputDir
  #define OutputDir "..\..\"
#endif

[Setup]
; Unique GUID identifying Mudcrab across versions and upgrades
AppId={{8B78124C-FE3D-4A46-95D9-38B132C19C74}}
AppName=Mudcrab
AppVersion={#AppVersion}
AppPublisher=Mudcrab Team
AppPublisherURL=https://mudcrab.dev
AppSupportURL=https://github.com/Mudcrab-Team/mudcrab/issues
AppUpdatesURL=https://mudcrab.dev

; Per-user install only: no UAC prompt, and {autopf} resolves to
; %LOCALAPPDATA%\Programs. The launcher writes its converted output next to its
; working directory, so everything it touches must belong to the running user.
DefaultDirName={autopf}\Mudcrab
DefaultGroupName=Mudcrab
AllowNoIcons=yes
PrivilegesRequired=lowest

; License agreement step
LicenseFile=..\..\LICENSE

; Output settings
OutputDir={#OutputDir}
OutputBaseFilename=mudcrab-v{#AppVersion}-windows-x64-setup
Compression=lzma2/ultra64
SolidCompression=yes

; 64-bit architecture enforcement
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible

; UI theme
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Ingest all staged binaries and assets from the unified staging seam
Source: "{#StagingDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Dirs]
; The launcher writes its converted output to ./modern_assets, so the shortcuts start
; in a folder the user can always write to ({app} may be under Program Files).
Name: "{localappdata}\Mudcrab"

[Icons]
Name: "{group}\Mudcrab Launcher"; Filename: "{app}\launcher.exe"; WorkingDir: "{localappdata}\Mudcrab"
Name: "{group}\Mudcrab Engine"; Filename: "{app}\engine.exe"; WorkingDir: "{localappdata}\Mudcrab"
Name: "{group}\Mudcrab Converter"; Filename: "{app}\converter.exe"; WorkingDir: "{localappdata}\Mudcrab"
Name: "{group}\{cm:UninstallProgram,Mudcrab}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Mudcrab"; Filename: "{app}\launcher.exe"; WorkingDir: "{localappdata}\Mudcrab"; Tasks: desktopicon

[Run]
Filename: "{app}\launcher.exe"; WorkingDir: "{localappdata}\Mudcrab"; Description: "{cm:LaunchProgram,Mudcrab}"; Flags: nowait postinstall skipifsilent
