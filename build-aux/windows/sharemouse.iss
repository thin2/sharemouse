; Per-user Windows 11 x64 test installer. No elevation, firewall changes or data removal.
#ifndef SourceDir
  #error SourceDir must point at a staged runtime
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
[Setup]
AppId={{3C417918-7CB5-47A7-B5A1-842D60A2CF8C}
AppName=Sharemouse
AppVersion=0.11.0
AppPublisher=Sharemouse contributors
DefaultDirName={localappdata}\Programs\Sharemouse
DefaultGroupName=Sharemouse
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
OutputDir={#OutputDir}
OutputBaseFilename=Sharemouse-windows-x64-setup
Compression=lzma2
SolidCompression=yes
LicenseFile={#SourceDir}\LICENSE
SetupIconFile={#SourceDir}\app-icon.ico
UninstallDisplayIcon={app}\lan-mouse.exe
CloseApplications=no
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "contextmenu"; Description: "Install Explorer Send with Sharemouse entry"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Sharemouse"; Filename: "{app}\lan-mouse.exe"
Name: "{group}\Uninstall Sharemouse"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Sharemouse"; Filename: "{app}\lan-mouse.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lan-mouse.exe"; Parameters: "cli integration install"; Tasks: contextmenu; Flags: runhidden waituntilterminated
Filename: "{app}\lan-mouse.exe"; Description: "Start Sharemouse"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\lan-mouse.exe"; Parameters: "cli integration uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveContextMenu"

[Registry]
; The app creates/updates this value on launch according to the saved preference.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "Sharemouse"; Flags: dontcreatekey uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\*\shell\SharemouseSend"; Flags: dontcreatekey uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\SharemouseSend"; Flags: dontcreatekey uninsdeletekey

[Code]
function StopRunningApp(): Boolean;
var
  ExitCode, Attempt: Integer;
  Exe: String;
begin
  Result := True;
  Exe := ExpandConstant('{app}\lan-mouse.exe');
  if not FileExists(Exe) then exit;
  { CLI timeout bounds shutdown; never force-kill a transfer or native input owner. }
  if not Exec(Exe, 'cli quit', '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then begin
    Result := False;
    exit;
  end;
  if CheckForMutexes('Local\SharemouseFrontend') then begin
    for Attempt := 1 to 50 do begin
      if not CheckForMutexes('Local\SharemouseFrontend') then exit;
      Sleep(100);
    end;
    Result := False;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not StopRunningApp() then
    Result := 'Quit Sharemouse from its tray menu, then retry installation.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := StopRunningApp();
  if not Result then MsgBox('Quit Sharemouse from its tray menu, then retry uninstall.', mbError, MB_OK);
end;
