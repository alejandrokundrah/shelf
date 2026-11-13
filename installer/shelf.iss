; Builds Shelf-Setup.exe with Inno Setup 6 (free). Run from the installer folder.
#define AppName "Shelf"
#define AppVersion "0.1.0"

[Setup]
AppId={{B7E3C1A2-5D4F-4E8A-9C61-2F0A7D5B3E19}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Shelf
DefaultDirName={autopf}\Shelf
DefaultGroupName=Shelf
DisableProgramGroupPage=yes
OutputDir=Output
OutputBaseFilename=Shelf-Setup
SetupIconFile=icon.ico
UninstallDisplayIcon={app}\icon.ico
Compression=lzma2
SolidCompression=yes
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "startup"; Description: "Start Shelf when Windows starts"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "..\target\release\shelf.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "icon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Shelf"; Filename: "{app}\shelf.exe"; IconFilename: "{app}\icon.ico"
Name: "{autodesktop}\Shelf"; Filename: "{app}\shelf.exe"; IconFilename: "{app}\icon.ico"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Shelf"; ValueData: """{app}\shelf.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
; Lets other PCs on private networks reach Shelf without a firewall pop-up.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Shelf"" dir=in action=allow program=""{app}\shelf.exe"" profile=private enable=yes"; Flags: runhidden
Filename: "{app}\shelf.exe"; Description: "Launch Shelf now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/im shelf.exe /f"; Flags: runhidden; RunOnceId: "KillShelf"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Shelf"""; Flags: runhidden; RunOnceId: "DelFirewall"
