; Aurora Host インストーラ (NSIS)
;
; 操作される側 (aurora-host.exe) を配布する per-machine インストーラ。
; Apps & features からクリーンにアンインストールできる体験は client (Tauri NSIS) と揃える。
;
; ビルド: installer\host\build.ps1 (release ビルド → makensis)。
;
; 設計方針:
;   - 本体は per-machine (%ProgramFiles%\Aurora Host) に置き、インストーラ全体を昇格で動かす。
;     自動起動タスクは /RL HIGHEST (昇格) でこの exe を毎ログオン実行するため、ユーザー権限で
;     書き換えられる場所 (旧: %LOCALAPPDATA%\Programs) に置くと、同一ユーザーの非昇格コードが
;     exe を差し替えるだけで UAC 無しに管理者権限を得られてしまう。昇格スクリプト (VDD 導入/削除)
;     も同じ理由で管理者専用の $INSTDIR から実行し、$TEMP は経由しない。
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
; 本体は per-machine (設計方針参照)。インストール/アンインストールとも昇格して動く。
RequestExecutionLevel admin
InstallDir "$PROGRAMFILES64\Aurora Host"
InstallDirRegKey HKLM "${REGROOT}" "InstallDir"
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

; サイレント自己更新 (host の自己更新機構が "aurora-host-setup-<ver>.exe /S" を実行する)
; の待機リトライ回数カウンタ。対話インストールでは使わない。
Var RetryCount

; ---------------------------------------------------------------------------
; サイレント更新時のみ呼ぶ。update.rs は setup 起動後に自プロセスを exit するが、
; ファイルハンドル解放に一瞬ラグがあるため、taskkill 後に「自分自身へのリネーム」で
; ロック解除を確認しながら最大 2 秒 (200ms x 10) だけ待つ (対話インストールでは
; InstFiles ページの表示自体が数百ms のバッファになるため、この待機は不要)。
Function StopRunningHost
  nsExec::Exec 'taskkill /IM ${EXENAME} /F'
  Pop $0

  IfFileExists "$INSTDIR\${EXENAME}" 0 stoprunninghost_done
    StrCpy $RetryCount 0
    stoprunninghost_wait:
      ClearErrors
      Rename "$INSTDIR\${EXENAME}" "$INSTDIR\${EXENAME}"
      IfErrors 0 stoprunninghost_done
      IntOp $RetryCount $RetryCount + 1
      IntCmp $RetryCount 10 stoprunninghost_done
      Sleep 200
      Goto stoprunninghost_wait
  stoprunninghost_done:
FunctionEnd

; サイレント (自己更新) 時は MUI_FINISHPAGE_RUN のページ自体が出ないため、
; インストール成功後にここで明示的に再起動する。対話インストールでは
; フィニッシュページの「今すぐ起動する」に任せるので何もしない。
Function .onInstSuccess
  IfSilent 0 +2
    Exec '"$INSTDIR\${EXENAME}"'
FunctionEnd

; ---------------------------------------------------------------------------
Section "Aurora Host 本体" SecCore
  SectionIn RO

  ; サイレント更新時のみ: 旧プロセスを止めてから配置し、確実に上書きできるようにする。
  ; 対話インストールでは何もしない (従来通り InstFiles ページでそのまま上書き)。
  IfSilent seccore_stop_running seccore_after_stop
  seccore_stop_running:
    Call StopRunningHost
  seccore_after_stop:

  SetOutPath "$INSTDIR"
  File "..\..\target\release\${EXENAME}"
  ; VDD 削除スクリプトを同梱しておく (アンインストール時に使用)。
  File "remove_vdd.ps1"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Apps & features 登録 (per-machine = HKLM)
  WriteRegStr   HKLM "${REGROOT}"   "InstallDir"        "$INSTDIR"
  WriteRegStr   HKLM "${UNINSTKEY}" "DisplayName"       "${APPNAME}"
  WriteRegStr   HKLM "${UNINSTKEY}" "DisplayVersion"    "${APPVERSION}"
  WriteRegStr   HKLM "${UNINSTKEY}" "Publisher"         "${PUBLISHER}"
  WriteRegStr   HKLM "${UNINSTKEY}" "DisplayIcon"       "$INSTDIR\${EXENAME}"
  WriteRegStr   HKLM "${UNINSTKEY}" "UninstallString"   '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKLM "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr   HKLM "${UNINSTKEY}" "InstallLocation"   "$INSTDIR"
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify"          1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair"          1

  ; per-machine なのでスタートメニューは全ユーザー側に置く。$LOCALAPPDATA 等はインストールした
  ; ユーザー側を指したままにする必要があるため、使い終わったら current へ戻す。
  SetShellVarContext all
  CreateShortcut "$SMPROGRAMS\Aurora Host.lnk" "$INSTDIR\${EXENAME}"
  SetShellVarContext current
SectionEnd

Section "ログオン時に自動起動 (推奨)" SecAutostart
  ; サイレント更新契約: 自己更新は無人実行前提のため UAC を一切出さない。既存インストールの
  ; スケジュールタスクは exe パス固定で登録済みなので再登録は不要。Autostart レジストリの
  ; 既存フラグもここで丸ごとスキップすることで書き換えず、ユーザー設定を保持する。
  IfSilent secautostart_skip secautostart_run
  secautostart_run:
    ; ONLOGON /RL HIGHEST タスクの登録は host 自身の --install に委譲する (インストーラは
    ; 昇格済みなので追加の UAC は出ない)。タスク名は "Aurora Host" (autostart.rs)。
    ExecShellWait "" "$INSTDIR\${EXENAME}" "--install" SW_HIDE
    ; アンインストール時に --uninstall を呼ぶべきか判定するためのフラグ。
    WriteRegDWORD HKLM "${REGROOT}" "Autostart" 1
  secautostart_skip:
SectionEnd

Section /o "仮想ディスプレイドライバ (VDD) をインストール" SecVDD
  ; サイレント更新は UAC を出さない契約のため丸ごとスキップする (既定 /o = 通常は選択されない
  ; オプション機能だが、/COMPONENTS で明示指定された場合の防御として明示ガードする)。
  IfSilent secvdd_skip secvdd_run
  secvdd_run:
  ; MttVDD (Virtual Display Driver) を C:\ProgramData\Aurora\vdd に展開し、デバイスノード作成 +
  ; ドライバ束縛まで行う (インストーラは昇格済み)。
  ; 展開先は $TEMP ではなく管理者専用の $INSTDIR 配下にする。$TEMP はユーザー権限で書き込めるため、
  ; 展開から昇格実行までの間に同一ユーザーの非昇格コードが ps1 / nefconw を差し替えられる。
  ;
  ; 重要: MttVDD は Root\MttVDD の root 列挙ソフトウェアデバイス。`pnputil /add-driver /install`
  ; はドライバをストアに入れるだけでデバイスノードを作らない (= 仮想ディスプレイが 0 個のまま)。
  ; そこで nefconw でデバイスノード (Root\MttVDD, Display クラス) を先に作ってから pnputil で
  ; ドライバを束縛する。これで初めて仮想モニタ (MTT1337) が出現し host が検出できる。
  ;
  ; 旧版は cmd の "&" チェーンで copy/nefconw/pnputil を繋いでいたが、クォートが脆弱でエラー検知も
  ; 検証も無く、実機で pnputil の add-driver が黙って失敗してもそのまま完了扱いになっていた。
  ; そこで導入ロジックを専用 PowerShell スクリプト install_vdd.ps1 (ASCII・ログ出力・MTT1337 検証
  ; +再試行付き) に集約し、ここからは昇格して呼ぶだけにする。失敗時は C:\ProgramData\Aurora\vdd\
  ; install_log.txt に原因が残る。
  SetOutPath "$INSTDIR\vdd-setup"
  File "vdd\MttVDD.dll"
  File "vdd\MttVDD.inf"
  File "vdd\mttvdd.cat"
  File "vdd\vdd_settings.xml"
  File "vdd\nefconw.exe"
  File "install_vdd.ps1"

  ; 配置 + ノード作成 + ドライバ束縛 + 検証を install_vdd.ps1 が行う。
  ; -HostExe を渡し、VDD 登録直後に host --align-vdd で描画 GPU をキャプチャ GPU に合わせる
  ; (クロス GPU 固着の恒久回避。SecCore は先に実行済みなので exe は配置済み)。
  ExecShellWait "" "powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\vdd-setup\install_vdd.ps1" -HostExe "$INSTDIR\${EXENAME}"' SW_HIDE

  ; 展開物を削除 (SetOutPath でカレントになっているため抜けてから消す)。
  SetOutPath "$INSTDIR"
  RMDir /r "$INSTDIR\vdd-setup"

  WriteRegDWORD HKLM "${REGROOT}" "VDDInstalled" 1
  secvdd_skip:
SectionEnd

Section /o "ファイル転送機能を有効化 (Client-ProjFS)" SecProjFS
  ; サイレント更新は UAC を出さない契約のため丸ごとスキップする (既定 /o の防御ガード)。
  IfSilent secprojfs_skip secprojfs_run
  secprojfs_run:
  ; Windows オプション機能の有効化 (インストーラは昇格済み)。再起動が必要な場合あり。
  ; 共有機能なのでアンインストールでは戻さない。
  ExecShellWait "" "powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -Command "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart"' SW_HIDE
  secprojfs_skip:
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "aurora-host 本体を %ProgramFiles% に配置します (必須)。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} "ログオン時に自動起動する ONLOGON タスクを登録します。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVDD}       "仮想ディスプレイドライバ (MttVDD) をインストールします。RDP 風の仮想画面で接続するのに必要です。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecProjFS}    "RDP 風ファイル転送に使う Windows 機能 Client-ProjFS を有効化します。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
Section "Uninstall"
  ; 1) 常駐プロセスを停止 (起動中だと exe を消せない)。
  nsExec::Exec 'taskkill /IM ${EXENAME} /F'
  Pop $0

  ; 2) 自動起動タスクを登録していたら解除 (アンインストーラは昇格済み)。
  ReadRegDWORD $0 HKLM "${REGROOT}" "Autostart"
  ${If} $0 == 1
    ExecShellWait "" "$INSTDIR\${EXENAME}" "--uninstall" SW_HIDE
  ${EndIf}

  ; 3) VDD ドライバを削除 (インストールした場合のみ)。
  ;    デバイス削除・ドライバストア削除・C:\ProgramData\Aurora\vdd 削除を一括で行う。
  ReadRegDWORD $0 HKLM "${REGROOT}" "VDDInstalled"
  ${If} $0 == 1
    ExecShellWait "" "powershell.exe" \
      '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\remove_vdd.ps1"' SW_HIDE
  ${EndIf}

  ; 4) ファイル / ショートカットを削除。
  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\remove_vdd.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"
  SetShellVarContext all
  Delete "$SMPROGRAMS\Aurora Host.lnk"
  SetShellVarContext current

  ; 5) 設定・暗号化パスワード・ログを完全削除 (インストール先と違いこれらは per-user)。
  RMDir /r "$LOCALAPPDATA\Aurora"

  ; 6) レジストリを掃除。
  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKLM "${REGROOT}"
SectionEnd
