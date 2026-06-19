; Aurora Host インストーラ (NSIS)
;
; 操作される側 (aurora-host.exe) を配布する per-user インストーラ。
; client (Tauri NSIS) と同じ「管理者プロンプトなしで入れて、Apps & features から
; クリーンにアンインストールできる」体験に揃える。
;
; ビルド: installer\host\build.ps1 (release ビルド → makensis)。
;
; 設計方針:
;   - 本体は per-user (%LOCALAPPDATA%\Programs\Aurora Host) に置く。RequestExecutionLevel
;     user なのでインストール自体は UAC 不要。
;   - ただし「ログオン時自動起動 (/RL HIGHEST タスク)」と「ProjFS 有効化」はシステム操作で
;     昇格が要る。これらを選んだときだけ ExecShellWait "runas" で 1 回 UAC を出して実行する。
;   - クリーンアンインストール最優先。痕跡 = exe / スケジュールタスク "Aurora Host" /
;     %LOCALAPPDATA%\Aurora (port / password.bin) / ショートカット / レジストリ。全部消す。
;   - 自動起動タスクの登録/解除は host 自身の --install / --uninstall に委譲 (二重実装しない)。
;   - ProjFS は共有システム機能なのでアンインストールでは無効化しない。

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"

!ifndef APPVERSION
  !define APPVERSION "0.1.0"
!endif
!define APPNAME    "Aurora Host"
!define PUBLISHER  "Aurora"
!define EXENAME    "aurora-host.exe"
!define REGROOT    "Software\Aurora Host"
!define UNINSTKEY  "Software\Microsoft\Windows\CurrentVersion\Uninstall\AuroraHost"

Name "${APPNAME} ${APPVERSION}"
OutFile "..\dist\aurora-host-setup-${APPVERSION}.exe"
; 本体は per-user。インストール自体に管理者権限は要らない。
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Aurora Host"
InstallDirRegKey HKCU "${REGROOT}" "InstallDir"
SetCompressor /SOLID lzma

VIProductVersion "${APPVERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileVersion" "${APPVERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APPNAME} Installer"

!define MUI_ICON   "..\..\crates\client\src-tauri\icons\icon.ico"
!define MUI_UNICON "..\..\crates\client\src-tauri\icons\icon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Aurora Host を今すぐ起動する"
!define MUI_FINISHPAGE_TEXT "接続パスワードは未設定です。トレイアイコン右クリック →「パスワード変更」、$\nまたは端末で `aurora-host --set-password` を一度実行してください。"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "Japanese"

; ---------------------------------------------------------------------------
Section "Aurora Host 本体" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "..\..\target\release\${EXENAME}"
  ; VDD 削除スクリプトを同梱しておく (アンインストール時に使用)。
  File "remove_vdd.ps1"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Apps & features 登録 (per-user = HKCU)
  WriteRegStr   HKCU "${REGROOT}"   "InstallDir"        "$INSTDIR"
  WriteRegStr   HKCU "${UNINSTKEY}" "DisplayName"       "${APPNAME}"
  WriteRegStr   HKCU "${UNINSTKEY}" "DisplayVersion"    "${APPVERSION}"
  WriteRegStr   HKCU "${UNINSTKEY}" "Publisher"         "${PUBLISHER}"
  WriteRegStr   HKCU "${UNINSTKEY}" "DisplayIcon"       "$INSTDIR\${EXENAME}"
  WriteRegStr   HKCU "${UNINSTKEY}" "UninstallString"   '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr   HKCU "${UNINSTKEY}" "InstallLocation"   "$INSTDIR"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify"          1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair"          1

  CreateShortcut "$SMPROGRAMS\Aurora Host.lnk" "$INSTDIR\${EXENAME}"
SectionEnd

Section "ログオン時に自動起動 (推奨)" SecAutostart
  ; ONLOGON /RL HIGHEST タスクの登録は昇格が要る。host 自身の --install に委譲し、
  ; ここだけ runas で 1 回だけ昇格する。タスク名は "Aurora Host" (autostart.rs)。
  ExecShellWait "runas" "$INSTDIR\${EXENAME}" "--install" SW_HIDE
  ; アンインストール時に --uninstall を呼ぶべきか判定するためのフラグ。
  WriteRegDWORD HKCU "${REGROOT}" "Autostart" 1
SectionEnd

Section /o "仮想ディスプレイドライバ (VDD) をインストール" SecVDD
  ; MttVDD (Virtual Display Driver) を C:\VirtualDisplayDriver に展開し、デバイスノード作成 +
  ; ドライバ束縛まで行う。ドライバのインストールは昇格が要るため runas で一回だけ UAC を出す。
  ; アンインストールではドライバは削除しない (システム共有コンポーネント、手動削除を推奨)。
  ; $TEMP はユーザー権限で書き込み可能。コピーと nefconw/pnputil は runas で昇格して実行。
  ;
  ; 重要: MttVDD は Root\MttVDD の root 列挙ソフトウェアデバイス。`pnputil /add-driver /install`
  ; はドライバをストアに入れるだけでデバイスノードを作らない (= 仮想ディスプレイが 0 個のまま)。
  ; そこで nefconw でデバイスノード (Root\MttVDD, Display クラス) を先に作ってから pnputil で
  ; ドライバを束縛する。これで初めて仮想モニタ (MTT1337) が出現し host が検出できる。
  ;
  ; 旧版は cmd の "&" チェーンで copy/nefconw/pnputil を繋いでいたが、クォートが脆弱でエラー検知も
  ; 検証も無く、実機で pnputil の add-driver が黙って失敗してもそのまま完了扱いになっていた。
  ; そこで導入ロジックを専用 PowerShell スクリプト install_vdd.ps1 (ASCII・ログ出力・MTT1337 検証
  ; +再試行付き) に集約し、ここからは昇格して呼ぶだけにする。失敗時は C:\VirtualDisplayDriver\
  ; install_log.txt に原因が残る。
  SetOutPath "$TEMP\AuroraVDDSetup"
  File "vdd\MttVDD.dll"
  File "vdd\MttVDD.inf"
  File "vdd\mttvdd.cat"
  File "vdd\vdd_settings.xml"
  File "vdd\nefconw.exe"
  File "install_vdd.ps1"

  ; 配置 + ノード作成 + ドライバ束縛 + 検証を install_vdd.ps1 が行う (昇格が 1 回必要)。
  ExecShellWait "runas" "powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -File "$TEMP\AuroraVDDSetup\install_vdd.ps1"' SW_HIDE

  ; 一時ファイルを削除。
  RMDir /r "$TEMP\AuroraVDDSetup"

  WriteRegDWORD HKCU "${REGROOT}" "VDDInstalled" 1
SectionEnd

Section /o "ファイル転送機能を有効化 (Client-ProjFS)" SecProjFS
  ; Windows オプション機能の有効化は昇格が要る。再起動が必要な場合あり。
  ; 共有機能なのでアンインストールでは戻さない。
  ExecShellWait "runas" "powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -Command "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart"' SW_HIDE
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "aurora-host 本体を %LOCALAPPDATA%\Programs に配置します (必須)。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} "ログオン時に自動起動する ONLOGON タスクを登録します (昇格が 1 回必要)。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVDD}       "仮想ディスプレイドライバ (MttVDD) をインストールします。RDP 風の仮想画面で接続するのに必要です (昇格が 1 回必要)。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecProjFS}    "RDP 風ファイル転送に使う Windows 機能 Client-ProjFS を有効化します (昇格が 1 回必要)。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
Section "Uninstall"
  ; 1) 常駐プロセスを停止 (起動中だと exe を消せない)。
  nsExec::Exec 'taskkill /IM ${EXENAME} /F'
  Pop $0

  ; 2) 自動起動タスクを登録していたら解除 (昇格が要る)。未登録なら無駄な UAC を出さない。
  ReadRegDWORD $0 HKCU "${REGROOT}" "Autostart"
  ${If} $0 == 1
    ExecShellWait "runas" "$INSTDIR\${EXENAME}" "--uninstall" SW_HIDE
  ${EndIf}

  ; 3) VDD ドライバを削除 (インストールした場合のみ)。
  ;    デバイス削除・ドライバストア削除・C:\VirtualDisplayDriver 削除を一括で行う。
  ReadRegDWORD $0 HKCU "${REGROOT}" "VDDInstalled"
  ${If} $0 == 1
    ExecShellWait "runas" "powershell.exe" \
      '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\remove_vdd.ps1"' SW_HIDE
  ${EndIf}

  ; 4) ファイル / ショートカットを削除。
  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\remove_vdd.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"
  Delete "$SMPROGRAMS\Aurora Host.lnk"

  ; 5) 設定・暗号化パスワード・ログを完全削除 (per-user なので昇格不要)。
  RMDir /r "$LOCALAPPDATA\Aurora"

  ; 6) レジストリを掃除。
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "${REGROOT}"
SectionEnd
