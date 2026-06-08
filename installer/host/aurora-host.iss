; Aurora Host インストーラ (Inno Setup)
;
; 操作される側 (aurora-host.exe) を配布するためのインストーラ。
; client は Tauri の NSIS が別途生成するため、これは host 専用。
;
; ビルド: installer\host\build.ps1 を実行 (release ビルド → ISCC でコンパイル)。
; 要 Inno Setup 6 (winget install JRSoftware.InnoSetup)。
;
; 設計方針:
;   - クリーンアンインストール最優先。痕跡 = (1) exe (2) ONLOGON タスク "Aurora Host"
;     (3) %LOCALAPPDATA%\Aurora (port / password.bin) の 3 点のみ。すべて削除する。
;   - 自動起動タスク登録は host 自身の `--install` / `--uninstall` に委譲 (二重実装しない)。
;   - 画面キャプチャ (DXGI) は対話セッション必須 = サービス化しない。ONLOGON タスクで常駐。
;   - ProjFS (ファイル転送) は任意。希望時のみ Windows オプション機能を有効化する。
;     共有システム機能なのでアンインストールでは無効化しない (他用途を壊さない)。

#define AppName "Aurora Host"
#define AppVersion "0.1.0"
#define AppPublisher "Aurora"
#define ExeName "aurora-host.exe"

[Setup]
AppId={{CE80B243-BA0D-458A-B015-4F133E6F3AC5}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\Aurora Host
DefaultGroupName=Aurora Host
UninstallDisplayIcon={app}\{#ExeName}
; ONLOGON /RL HIGHEST タスク登録と (任意の) ProjFS 有効化に管理者権限が要る。
PrivilegesRequired=admin
; 想定配置は単一ユーザーの自宅 PC。UAC 昇格でもプロファイルは切り替わらないため
; {localappdata} は host を実行する対話ユーザーのものと一致する。よって管理者モードでの
; per-user 領域 (LOCALAPPDATA\Aurora) 削除は意図どおり機能する → 警告を抑制する。
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=dist
OutputBaseFilename=aurora-host-setup-{#AppVersion}
SetupIconFile=..\..\crates\client\src-tauri\icons\icon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; 既に常駐している exe をアップグレード時に置き換えられるよう、起動中プロセスの終了を促す。
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "default"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "ログオン時に自動起動する (推奨)"; GroupDescription: "セットアップ:"
Name: "projfs"; Description: "ファイル転送機能を有効化する (Windows の Client-ProjFS を有効化。再起動が必要な場合あり)"; GroupDescription: "オプション機能:"; Flags: unchecked

[Files]
Source: "..\..\target\release\{#ExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Aurora Host"; Filename: "{app}\{#ExeName}"
Name: "{group}\Aurora Host のアンインストール"; Filename: "{uninstallexe}"

[Run]
; 自動起動タスク "Aurora Host" を登録 (host 自身の実装に委譲)。
Filename: "{app}\{#ExeName}"; Parameters: "--install"; Flags: runhidden waituntilterminated; Tasks: autostart; StatusMsg: "ログオン時自動起動を登録しています..."
; ProjFS (ファイル転送) を有効化。共有機能なのでアンインストールでは戻さない。
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart"""; Flags: runhidden waituntilterminated; Tasks: projfs; StatusMsg: "ファイル転送機能 (Client-ProjFS) を有効化しています..."
; 完了後にトレイ常駐を起動 (任意)。
Filename: "{app}\{#ExeName}"; Description: "Aurora Host を今すぐ起動する"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 1) 常駐プロセスを止めてから 2) 自動起動タスクを解除。どちらもファイル削除前に実行される。
Filename: "taskkill.exe"; Parameters: "/IM {#ExeName} /F"; Flags: runhidden; RunOnceId: "KillHost"
Filename: "{app}\{#ExeName}"; Parameters: "--uninstall"; Flags: runhidden; RunOnceId: "RemoveTask"

[UninstallDelete]
; 設定・暗号化パスワードを残さない (完全削除)。
Type: filesandordirs; Name: "{localappdata}\Aurora"
