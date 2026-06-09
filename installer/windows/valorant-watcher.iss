; Inno Setup script for the valorant-watcher background process
; downloads the latest watcher zip from the github release, extracts it to
; %LOCALAPPDATA%\valorant-watcher, registers per-user autostart, launches once
;
; requires Inno Setup 6.1+ (uses the built-in download page)
; build: ISCC.exe /DMyAppVersion=1.2.3 installer\windows\valorant-watcher.iss

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#define MyAppName       "VALORANT Watcher"
#define MyAppPublisher  "VALORANT Streamsniper"
#define MyAppExeName    "valorant-watcher.exe"
#define WatcherDirName  "valorant-watcher"
#define WatcherZipName  "valorant-watcher-windows.zip"
#define WatcherURL      "https://github.com/ryleqTheReal/valorant-watcher-rust/releases/latest/download/valorant-watcher-windows.zip"
#define ConfigURL       "https://github.com/ryleqTheReal/valorant-watcher-rust/releases/latest/download/config.json"
#define ConfigName      "config.json"
#define RunKey          "Software\Microsoft\Windows\CurrentVersion\Run"
#define RunValue        "ValorantWatcher"

[Setup]
AppId={{8F3C2A14-7B9E-4D6A-9C21-5E4F8A0B1D33}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
PrivilegesRequired=lowest
DefaultDirName={localappdata}\{#WatcherDirName}
DisableDirPage=yes
DisableProgramGroupPage=yes
OutputDir={#SourcePath}Output
OutputBaseFilename=valorant-watcher-setup
SetupIconFile={#SourcePath}..\..\assets\icon.ico
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Registry]
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#RunValue}"; \
    ValueData: """{app}\{#MyAppExeName}"""; Flags: uninsdeletevalue

[Code]
var
  DownloadPage: TDownloadWizardPage;

function OnDownloadProgress(const Url, FileName: String; const Progress, ProgressMax: Int64): Boolean;
begin
  Result := True;
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing),
    SetupMessage(msgPreparingDesc), @OnDownloadProgress);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  if CurPageID = wpReady then
  begin
    DownloadPage.Clear;
    DownloadPage.Add('{#WatcherURL}', '{#WatcherZipName}', '');
    DownloadPage.Add('{#ConfigURL}', '{#ConfigName}', '');
    DownloadPage.Show;
    try
      try
        DownloadPage.Download;
        Result := True;
      except
        if not DownloadPage.AbortedByUser then
          MsgBox('Could not download the watcher: ' + AddPeriod(GetExceptionMessage),
            mbCriticalError, MB_OK);
        Result := False;
      end;
    finally
      DownloadPage.Hide;
    end;
  end
  else
    Result := True;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
  ZipPath, DestPath, ConfigSrc, ConfigDst: String;
begin
  if CurStep = ssInstall then
  begin
    // stop a running instance so the exe can be overwritten cleanly
    Exec('taskkill.exe', '/F /IM "{#MyAppExeName}"', '', SW_HIDE,
      ewWaitUntilTerminated, ResultCode);
  end
  else if CurStep = ssPostInstall then
  begin
    ZipPath := ExpandConstant('{tmp}\{#WatcherZipName}');
    DestPath := ExpandConstant('{app}');
    Exec('powershell.exe',
      '-NoProfile -ExecutionPolicy Bypass -Command "Expand-Archive -LiteralPath '''
      + ZipPath + ''' -DestinationPath ''' + DestPath + ''' -Force"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if ResultCode <> 0 then
    begin
      MsgBox('Extraction failed (exit ' + IntToStr(ResultCode) + ')',
        mbCriticalError, MB_OK);
      Exit;
    end;
    // copy downloaded config only on first install; preserve user edits on updates
    ConfigSrc := ExpandConstant('{tmp}\{#ConfigName}');
    ConfigDst := ExpandConstant('{app}\{#ConfigName}');
    if not FileExists(ConfigDst) then
      FileCopy(ConfigSrc, ConfigDst, False);
    // launch once so the user can complete first-time login
    Exec(ExpandConstant('{app}\{#MyAppExeName}'), '', DestPath, SW_SHOWNORMAL,
      ewNoWait, ResultCode);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
    Exec('taskkill.exe', '/F /IM "{#MyAppExeName}"', '', SW_HIDE,
      ewWaitUntilTerminated, ResultCode)
  else if CurUninstallStep = usPostUninstall then
    DelTree(ExpandConstant('{app}'), True, True, True);
end;
