; Aurora Host インストーラ (NSIS)
;
; 操作される側 (aurora-host.exe) を配布する per-machine インストーラ。
; Apps & features からクリーンにアンインストールできる体験は client (Tauri NSIS) と揃える。
;
; ビルド: installer\host\build.ps1 (release ビルド → makensis)。
;
; 設計方針:
;   - 本体は per-machine (%ProgramFiles%\Aurora Host) に置き、インストーラ全体を昇格で動かす。
;     host は SYSTEM サービス "AuroraHost" としてこの exe を実行するため、ユーザー権限で
;     書き換えられる場所 (旧: %LOCALAPPDATA%\Programs) に置くと、同一ユーザーの非昇格コードが
;     exe を差し替えるだけで SYSTEM 権限を得られてしまう。昇格スクリプト (VDD 導入/削除)
;     も同じ理由で管理者専用の $INSTDIR から実行し、$TEMP は経由しない。
;   - SYSTEM サービス化 (UAC / ロック画面 / サインイン画面の遠隔操作のため)。サービスが本体を
;     対話セッションへ SYSTEM で、トレイをログオンユーザーの昇格 (管理者) 権限で起動する。exe を上書きする前に
;     必ずサービスを止める (止めないとサービスが本体を即座に起動し直し、exe がロックされ続ける)。
;   - 旧版 (ONLOGON タスク + %LOCALAPPDATA%\Aurora) からの移行: --migrate-user-state を
;     **インストールした利用者として** 実行して設定/パスワードを %ProgramData%\Aurora\host へ移し
;     (旧パスワードはその利用者でしか復号できない)、--install-service がタスクを削除する。
;   - クリーンアンインストール最優先。痕跡 = exe / サービス "AuroraHost" / 旧スケジュールタスク
;     "Aurora Host" / %ProgramData%\Aurora\host (設定・パスワード・ログ) / %LOCALAPPDATA%\Aurora
;     配下の host のファイル (旧版の設定・トレイのログ。フォルダ自体は client と共用) /
;     ショートカット / レジストリ。全部消す。
;   - サービス/タスクの登録・解除は host 自身の --install-service 等に委譲 (二重実装しない)。
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
; サービス運用では「今すぐ起動」で何も起動しない (サービスが既に本体を起動している。ここで
; 昇格した本体を起動すると、サービスの本体より先に単一インスタンスを取って居座りうる)。
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchHostIfNoService
!define MUI_FINISHPAGE_RUN_TEXT "Aurora Host を今すぐ起動する"
!define MUI_FINISHPAGE_TEXT "接続パスワードが未設定の場合は、トレイアイコン右クリック →「パスワードを変更」で設定してください。$\n(サービスとして登録した場合、Aurora Host は既に起動しています)"

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
; 旧版が入っていた (= 更新インストール) か。.onInit で、このインストールが登録を書き換える前に読む。
Var IsUpgrade

Function .onInit
  ReadRegStr $IsUpgrade HKLM "${REGROOT}" "InstallDir"
FunctionEnd

; インストール先の検証。SYSTEM サービスがこのフォルダの exe を実行し、exe は同じフォルダの DLL も
; 読むため、一般ユーザーが前もって中身を置けたフォルダ (既存の別アプリのフォルダや自分で作った
; フォルダ) は使わせない。空 or 存在しない新しいフォルダ、または前回のインストール先だけを許す。
Function .onVerifyInstDir
  StrCmp $INSTDIR $IsUpgrade verify_ok
  IfFileExists "$INSTDIR\*.*" 0 verify_ok
  FindFirst $0 $1 "$INSTDIR\*.*"
  verify_loop:
    StrCmp $1 "" verify_empty
    StrCmp $1 "." verify_next
    StrCmp $1 ".." verify_next
    FindClose $0
    Abort
  verify_next:
    FindNext $0 $1
    Goto verify_loop
  verify_empty:
    FindClose $0
  verify_ok:
FunctionEnd

; ---------------------------------------------------------------------------
; exe を上書きする前に呼ぶ。サービスを先に止める (止めないとサービスが本体を起動し直して
; exe をロックし続ける。net stop はサービスが本体とトレイを止め終えるまで待つ)。その後で
; 残ったプロセス (旧版の ONLOGON 運用・手動起動) を taskkill し、ファイルハンドル解放の
; ラグを「自分自身へのリネーム」でロック解除を確認しながら最大 2 秒 (200ms x 10) だけ待つ。
Function StopRunningHost
  nsExec::Exec '"$SYSDIR\net.exe" stop AuroraHost'
  Pop $0
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM ${EXENAME} /F'
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

Function LaunchHostIfNoService
  ReadRegDWORD $0 HKLM "${REGROOT}" "Service"
  ${If} $0 != 1
    Exec '"$INSTDIR\${EXENAME}"'
  ${EndIf}
FunctionEnd

; サイレント (自己更新) 時は MUI_FINISHPAGE_RUN のページ自体が出ないため、
; インストール成功後にここで明示的に再起動する。サービス運用ならサービスの開始 (SecService の
; --install-service) が本体を起動するので何もしない。対話インストールではフィニッシュページの
; 「今すぐ起動する」(LaunchHostIfNoService) に任せる。
Function .onInstSuccess
  IfSilent 0 onsuccess_done
  ReadRegDWORD $0 HKLM "${REGROOT}" "Service"
  ${If} $0 != 1
    Exec '"$INSTDIR\${EXENAME}"'
  ${EndIf}
  onsuccess_done:
FunctionEnd

; ---------------------------------------------------------------------------
Section "Aurora Host 本体" SecCore
  SectionIn RO

  ; 旧プロセス (サービス・本体・トレイ) を止めてから配置し、確実に上書きできるようにする。
  ; サービス運用では対話インストールでも止めないと上書きできない。
  Call StopRunningHost

  SetOutPath "$INSTDIR"
  ; 所有者と ACL を固める (SYSTEM / Administrators は変更可、Users は読み取り・実行のみ、親からの
  ; 継承を切る)。既定の %ProgramFiles% では元々この形だが、別の場所を選ばれたときに一般ユーザーが
  ; exe を差し替えて SYSTEM 権限を得るのを防ぐ。
  ; 継承付きの ACE はフォルダにだけ付け (/T を付けない)、中身は「親から継承」へ戻す。/T で
  ; 子ファイルにも付けようとすると、(OI)(CI) はファイルに付けられず、/inheritance:r で継承分だけ
  ; 剥がれて DACL が空になり、管理者でも exe を上書きできなくなる (2026-10-17 実機で発生)。
  nsExec::Exec '"$SYSDIR\icacls.exe" "$INSTDIR" /setowner *S-1-5-32-544 /T /C /Q'
  Pop $0
  nsExec::Exec '"$SYSDIR\icacls.exe" "$INSTDIR" /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F *S-1-5-32-545:(OI)(CI)RX /C /Q'
  Pop $0
  nsExec::Exec '"$SYSDIR\icacls.exe" "$INSTDIR\*" /reset /T /C /Q'
  Pop $0
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

Section "サービスとして常駐 (必須・UAC / ロック画面も操作可)" SecService
  ; 必須: host の設定・パスワードは %ProgramData%\Aurora\host (SYSTEM / 管理者専用) に置くため、
  ; 非昇格で手動起動した host は状態を読み書きできない。常にサービスで動かす。サイレント更新
  ; (setup は昇格済みの host から起動され UAC を出さない) でも毎回登録し直し、旧版の ONLOGON
  ; 運用や手動起動のみの運用もここでサービスへ移行する。
  SectionIn RO

  ; 旧版の設定/パスワードを移す。旧パスワードはインストールした利用者でしか復号できないので、
  ; この (利用者として昇格した) インストーラから呼ぶ。移行先に既にあれば何もしない。失敗しても
  ; サービス登録は続ける (パスワードはトレイから設定し直せる) が、黙って失わないよう知らせる。
  ; 新規インストールでは呼ばない: 移行元 (%LOCALAPPDATA%\Aurora) は利用者が自由に書ける場所で、
  ; 同じユーザーで動くマルウェアが事前に置いたパスワード・許可端末を取り込ませないため。
  ${If} $IsUpgrade != ""
    ExecWait '"$INSTDIR\${EXENAME}" --migrate-user-state' $0
    ${If} $0 != 0
      DetailPrint "旧版の設定・パスワードの移行に失敗しました (終了コード $0)"
      ${IfNot} ${Silent}
        MessageBox MB_ICONEXCLAMATION|MB_OK "旧版の設定・パスワードを移行できませんでした。$\nインストール後、トレイアイコンからパスワードを設定し直してください。"
      ${EndIf}
    ${EndIf}
  ${EndIf}

  ; サービス登録 (既存なら設定更新) + 旧 ONLOGON タスク削除 + 開始は host に委譲する。
  ; Service=1 は成功したときだけ書く (失敗時はフィニッシュ/サイレント後に従来どおり本体を直接起動する)。
  ExecWait '"$INSTDIR\${EXENAME}" --install-service' $0
  ${If} $0 == 0
    WriteRegDWORD HKLM "${REGROOT}" "Service" 1
    DeleteRegValue HKLM "${REGROOT}" "Autostart"
  ${Else}
    DetailPrint "サービスの登録に失敗しました (終了コード $0)"
    DeleteRegValue HKLM "${REGROOT}" "Service"
    ${IfNot} ${Silent}
      MessageBox MB_ICONSTOP|MB_OK "Aurora Host をサービスとして登録できませんでした (終了コード $0)。$\nUAC の確認画面・ロック画面は遠隔操作できません。%ProgramData%\Aurora\host\logs を確認してください。"
    ${EndIf}
  ${EndIf}
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
  ExecShellWait "" "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
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
  ExecShellWait "" "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
    '-NoProfile -ExecutionPolicy Bypass -Command "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart"' SW_HIDE
  secprojfs_skip:
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "aurora-host 本体を %ProgramFiles% に配置します (必須)。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecService}   "SYSTEM サービスとして登録し、PC 起動時から常駐します (必須)。UAC の確認画面・ロック画面・サインイン画面も遠隔で操作できます。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVDD}       "仮想ディスプレイドライバ (MttVDD) をインストールします。RDP 風の仮想画面で接続するのに必要です。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecProjFS}    "RDP 風ファイル転送に使う Windows 機能 Client-ProjFS を有効化します。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
Section "Uninstall"
  ; 1) サービスを停止・削除する (サービスが本体とトレイを止める)。登録が無ければ何もしない。
  ExecShellWait "" "$INSTDIR\${EXENAME}" "--uninstall-service" SW_HIDE

  ; 2) 残った常駐プロセスを停止 (起動中だと exe を消せない)。
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM ${EXENAME} /F'
  Pop $0

  ; 3) 旧版の自動起動タスクを登録していたら解除 (アンインストーラは昇格済み)。
  ReadRegDWORD $0 HKLM "${REGROOT}" "Autostart"
  ${If} $0 == 1
    ExecShellWait "" "$INSTDIR\${EXENAME}" "--uninstall" SW_HIDE
  ${EndIf}

  ; 4) VDD ドライバを削除 (インストールした場合のみ)。
  ;    デバイス削除・ドライバストア削除・C:\ProgramData\Aurora\vdd 削除を一括で行う。
  ReadRegDWORD $0 HKLM "${REGROOT}" "VDDInstalled"
  ${If} $0 == 1
    ExecShellWait "" "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
      '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\remove_vdd.ps1"' SW_HIDE
  ${EndIf}

  ; 5) ファイル / ショートカットを削除。
  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\remove_vdd.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"
  SetShellVarContext all
  Delete "$SMPROGRAMS\Aurora Host.lnk"
  SetShellVarContext current

  ; 6) 設定・暗号化パスワード・ログを完全削除。host の状態は PC 単位 (%ProgramData%\Aurora\host)。
  ;    親の Aurora は VDD (vdd\) と共用なので空のときだけ消す。
  SetShellVarContext all
  RMDir /r "$APPDATA\Aurora\host"
  RMDir "$APPDATA\Aurora"
  SetShellVarContext current
  ;    per-user 側 (%LOCALAPPDATA%\Aurora) は client と共用で、利用者が自由に書ける (中身を
  ;    ジャンクションにされうる) 場所なので、フォルダごと再帰削除せず host のファイルだけを消す
  ;    (旧版の状態・トレイのログ・ProjFS 投影ルート)。
  Delete "$LOCALAPPDATA\Aurora\password.bin"
  Delete "$LOCALAPPDATA\Aurora\port"
  Delete "$LOCALAPPDATA\Aurora\encode-gpu"
  Delete "$LOCALAPPDATA\Aurora\pinning"
  Delete "$LOCALAPPDATA\Aurora\trusted_devices.txt"
  Delete "$LOCALAPPDATA\Aurora\takeover_topology.txt"
  Delete "$LOCALAPPDATA\Aurora\takeover_ccd.bin"
  Delete "$LOCALAPPDATA\Aurora\logs\host.*"
  Delete "$LOCALAPPDATA\Aurora\logs\tray.*"
  RMDir "$LOCALAPPDATA\Aurora\remote"
  RMDir "$LOCALAPPDATA\Aurora\logs"
  RMDir "$LOCALAPPDATA\Aurora"

  ; 7) レジストリを掃除。
  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKLM "${REGROOT}"
SectionEnd
