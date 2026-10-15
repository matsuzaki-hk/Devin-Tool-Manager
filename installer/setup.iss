; Devin Tool Manager インストーラ (Inno Setup 6)
; ビルド: build.ps1 経由で ISCC 呼び出し。/D で版・payload を注入可能。

#ifndef AppVersion
  #define AppVersion "0.4.12"
#endif
#ifndef VsixFile
  #define VsixFile "devin-tool-manager-" + AppVersion + ".vsix"
#endif
#ifndef PayloadDir
  #define PayloadDir "payload"
#endif

#define ExtId "local.devin-tool-manager"

[Setup]
AppId={{8D3B7A2E-5C1F-4A9E-B6D2-0E7F3A1C9B4D}}
AppName=Devin Tool Manager
AppVersion={#AppVersion}
AppVerName=Devin Tool Manager {#AppVersion}
AppPublisher=DTM Project
; 拡張が dtm.exe をこの固定パスで探索するため変更不可にする
DefaultDirName={localappdata}\Programs\Devin Next\DevinToolManager
DisableDirPage=yes
UsePreviousAppDir=no
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=dist
OutputBaseFilename=dtm-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
UninstallDisplayName=Devin Tool Manager
UninstallDisplayIcon={app}\dtm.exe
VersionInfoVersion=1.0.0.0
VersionInfoDescription=Devin Tool Manager セットアップ

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
; dtm.exe が実行中ロックされていても BeforeInstall で .old-* へ退避してから上書き
Source: "{#PayloadDir}\dtm.exe"; DestDir: "{app}"; Flags: ignoreversion; BeforeInstall: RetireOldDtm
Source: "{#PayloadDir}\dtm.root"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\{#VsixFile}"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Code]
var
  DevinCli: String;

{ Devin CLI の解決: 既定パス → Program Files → PATH(where) の順 }
function FindDevinCli(): String;
var
  tmpFile, line: String;
  res: Integer;
  lines: TArrayOfString;
begin
  Result := ExpandConstant('{localappdata}\Programs\Devin Next\bin\devin-desktop-next.cmd');
  if FileExists(Result) then Exit;
  Result := ExpandConstant('{autopf}\Devin Next\bin\devin-desktop-next.cmd');
  if FileExists(Result) then Exit;
  Result := '';
  tmpFile := ExpandConstant('{tmp}\devin-where.txt');
  if Exec(ExpandConstant('{cmd}'),
          '/c where devin-desktop-next > "' + tmpFile + '" 2>nul',
          '', SW_HIDE, ewWaitUntilTerminated, res) and FileExists(tmpFile) then begin
    if LoadStringsFromFile(tmpFile, lines) and (GetArrayLength(lines) > 0) then begin
      line := Trim(lines[0]);
      if (line <> '') and FileExists(line) then Result := line;
    end;
  end;
end;

procedure RetireOldDtm();
var
  oldName, newName: String;
begin
  oldName := ExpandConstant('{app}\dtm.exe');
  if FileExists(oldName) then begin
    newName := oldName + '.old-' + GetDateTimeString('yyyymmddhhnnss', '-', ':');
    { 実行中の exe はリネーム可能。失敗時は後続のコピーが Inno 標準の再試行
      ダイアログを出すため、ここでは成否を問題にしない }
    RenameFile(oldName, newName);
  end;
end;

{ Devin CLI を非表示で同期実行し、成否を返す }
function RunDevinCli(args: String): Boolean;
var
  res: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'),
                 '/c ""' + DevinCli + '" ' + args + '"',
                 '', SW_HIDE, ewWaitUntilTerminated, res) and (res = 0);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  vsix, msg: String;
begin
  if CurStep = ssPostInstall then begin
    DevinCli := FindDevinCli();
    if DevinCli = '' then begin
      msg := 'dtm.exe の配備は完了しましたが、Devin CLI ' +
             '(devin-desktop-next) が見つからなかったため拡張機能は未導入です。' + #13#10 +
             'Devin Next 導入後に以下を実行してください:' + #13#10 + #13#10 +
             '  devin-desktop-next --install-extension <path>\{#VsixFile}';
      MsgBox(msg, mbInformation, MB_OK);
      Exit;
    end;
    vsix := ExpandConstant('{tmp}\{#VsixFile}');
    if not RunDevinCli('--install-extension "' + vsix + '"') then begin
      MsgBox('拡張機能のインストールに失敗しました。' + #13#10 +
             'dtm.exe は導入済みです。拡張は Devin の拡張機能画面から ' +
             'VSIX を指定して導入できます。', mbError, MB_OK);
    end;
  end;
end;

procedure CurUninstallStepChanged(CurStep: TUninstallStep);
begin
  if CurStep = usUninstall then begin
    DevinCli := FindDevinCli();
    if DevinCli <> '' then
      RunDevinCli('--uninstall-extension {#ExtId}');
  end;
end;
