; SPDX-License-Identifier: AGPL-3.0-or-later
; Inno Setup script for the supaS3 installer. Built by package.ps1, which
; passes the version, the architecture and the folder holding the files.
#ifndef AppVersion
  #error AppVersion is not defined
#endif
#ifndef Arch
  #error Arch is not defined
#endif
#ifndef SourceDir
  #error SourceDir is not defined
#endif
#ifndef OutputDir
  #error OutputDir is not defined
#endif

[Setup]
AppId={{6F0B3C0E-7E57-4C7B-9C55-5B1F1D3A52E1}
AppName=supaS3
AppVersion={#AppVersion}
AppPublisher=supaS3 contributors
AppPublisherURL=https://github.com/web-casa/supas3
AppSupportURL=https://github.com/web-casa/supas3/issues
DefaultDirName={autopf}\supaS3
DefaultGroupName=supaS3
DisableProgramGroupPage=yes
LicenseFile={#SourceDir}\LICENSE.txt
OutputDir={#OutputDir}
OutputBaseFilename=supas3-{#AppVersion}-windows-{#Arch}-setup
SetupIconFile={#SourceDir}\supas3.ico
UninstallDisplayIcon={app}\supas3.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Installs for the current user unless they choose otherwise, so no
; administrator rights are needed.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\supas3.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD_PARTY_LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSES\*"; DestDir: "{app}\LICENSES"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\supaS3"; Filename: "{app}\supas3.exe"
Name: "{autodesktop}\supaS3"; Filename: "{app}\supas3.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\supas3.exe"; Description: "{cm:LaunchProgram,supaS3}"; Flags: nowait postinstall skipifsilent
