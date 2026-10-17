//! host の SYSTEM サービス化 (UAC / ロック画面 / サインイン画面を遠隔操作するための土台)。
//!
//! 構成: SCM が `aurora-host --service` を LocalSystem / Session 0 で起動する。Session 0 は
//! 対話デスクトップを持たずキャプチャできないため、サービスは**自分の SYSTEM トークンを複製して
//! セッション ID だけをアクティブなコンソールセッションへ差し替え**、そこへ本体
//! (`aurora-host --service-child`) を起動する。本体は SYSTEM なので Winlogon デスクトップ
//! (UAC 同意画面・ロック/サインイン画面) へ `SetThreadDesktop` でき、複製・入力注入できる。
//! RustDesk / Sunshine / Chrome Remote Desktop と同じ定石。
//!
//! Windows 固有の落とし穴:
//! - `SetTokenInformation(TokenSessionId)` は SeTcbPrivilege が要る (LocalSystem は保持)。
//!   管理者の昇格プロセスからでは失敗するので、本体の起動はサービスからしかできない。
//! - コンソールセッションはログオフや高速ユーザー切替で**別 ID に変わる** (ログオフ後の
//!   サインイン画面は新しいセッション)。WTS 通知を受けたら付け替える。同一セッション内の
//!   ログオン/ロックでは張り替えない (本体がデスクトップ切替に追従する)。
//! - 本体の自己 re-exec (DXGI ACCESS_LOST 回復) は、サービス配下では子を産まずに終了して
//!   サービスに再起動させる ([`is_service_child`])。孫プロセスはサービスから監視できない。
//! - トレイ (`--tray`) は SYSTEM ではなく**ログオンユーザーの昇格 (管理者) トークン** で起動する
//!   (`WTSQueryUserToken` が返すのは UAC の制限付きトークンなので、`TokenLinkedToken` で昇格側を
//!   得る。旧版の ONLOGON `/RL HIGHEST` と同じく同意画面は出ない)。SYSTEM の UI を対話デスクトップ
//!   に出すとローカル利用者への権限昇格穴になり、逆に通常権限で動かすと同じユーザーの通常権限
//!   プロセスが UIPI に阻まれずトレイを自動操作できてしまう (パイプも管理者限定にしてある)。
//!   管理者でないユーザーのセッションと、サインイン前はトレイを起動しない。

/// SCM に登録するサービス名 (`sc query AuroraHost` で引ける)。
#[cfg(target_os = "windows")]
const SERVICE_NAME: &str = "AuroraHost";
/// サービス配下で起動された本体である印 (CLI 引数)。サービスだけが付ける。
pub const SERVICE_CHILD_ARG: &str = "--service-child";
/// サービスが本体へ「後始末して終了せよ」を伝える名前付きイベント。Global 名前空間に作れるのは
/// SeCreateGlobalPrivilege を持つサービス/管理者だけなので、一般ユーザーは先回りで作れない。
#[cfg(target_os = "windows")]
const STOP_EVENT_NAME: &str = "Global\\AuroraHostStop";

/// 現プロセスがサービスから起動された本体かを返す。引数を見るだけなので誰でも付けられるが、
/// 変わるのは「自己 re-exec の代わりに終了する」「二重起動ダイアログを出さない」挙動だけで、
/// 権限には影響しない。
pub fn is_service_child() -> bool {
    std::env::args().skip(1).any(|a| a == SERVICE_CHILD_ARG)
}

/// サービスへ転送する起動引数 (`--install-service` 自体を除いた現プロセスの引数)。
/// `--bind` / `--log-level` 等をサービス経由の本体にも効かせるため。
pub fn current_service_forward_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| a != "--install-service" && a != "--uninstall-service")
        .collect()
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Security::{
        DuplicateTokenEx, GetTokenInformation, SecurityIdentification, SetTokenInformation,
        TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION,
        TOKEN_LINKED_TOKEN, TOKEN_QUERY, TokenElevation, TokenLinkedToken, TokenPrimary,
        TokenSessionId,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW,
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
        PROCESS_INFORMATION, ResetEvent, ResumeThread, STARTUPINFOW, SetEvent, TerminateProcess,
        WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        SessionChangeReason,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{SERVICE_CHILD_ARG, SERVICE_NAME, STOP_EVENT_NAME};

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    /// `WTSGetActiveConsoleSessionId` の「コンソールに接続中のセッションが無い」(切替の過渡)。
    const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;
    /// 本体の死活・セッション変化を見る周期。WTS 通知でも即時に起きるので短くしなくてよい。
    const POLL: Duration = Duration::from_millis(500);
    /// これより長く生きた本体の終了は「正常稼働後の終了 (re-exec 回復等)」とみなし即再起動する。
    /// 短命な終了 (起動失敗・二重起動) が続く場合だけ待ちを伸ばし、CPU を焼くループを避ける。
    const HEALTHY_RUNTIME: Duration = Duration::from_secs(30);
    const BACKOFF_MIN: Duration = Duration::from_secs(1);
    const BACKOFF_MAX: Duration = Duration::from_secs(60);
    /// トレイが異常終了したときの再起動待ち (explorer 起動前のログオン直後などを空振りしないよう緩め)。
    const TRAY_RETRY: Duration = Duration::from_secs(5);
    /// 同じセッションに既にトレイが居たときに確かめ直す間隔。
    const TRAY_ALREADY_RUNNING_RETRY: Duration = Duration::from_secs(60);
    /// 停止要求から本体の自主終了を待つ上限。過ぎたら強制終了する。
    const GRACEFUL_STOP: Duration = Duration::from_secs(5);
    /// 停止処理にかかりうる上限として SCM へ伝える目安 (本体の自主終了待ち + 強制終了の待ち)。
    /// 伝えないと、シャットダウン猶予の短い環境で SCM が停止中のサービスを強制終了しうる。
    const STOP_WAIT_HINT: Duration = Duration::from_secs(15);
    /// 受信を許可するファイアウォール規則の名前。登録・削除ともこの名前で引く。
    const FIREWALL_RULE_NAME: &str = "Aurora Host";

    /// Drop で CloseHandle するカーネルハンドル (トークン / プロセス)。
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: 取得に成功した有効なハンドルだけを保持し、1 度だけ閉じる。
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: 保持するのはカーネルハンドル値だけで、どのスレッドから閉じても安全。
    unsafe impl Send for OwnedHandle {}

    /// 本体への停止要求イベント ([`STOP_EVENT_NAME`]、手動リセット)。強制終了 (`TerminateProcess`)
    /// では本体の後始末 (仮想ディスプレイのテイクオーバー復元・ProjFS ドライブ解除) が走らず、
    /// アンインストールや更新でそのまま次の起動が無いと物理画面が暗転したまま残る。
    struct StopEvent(OwnedHandle);

    impl StopEvent {
        fn create() -> Result<Self> {
            // SAFETY: 名前付きの手動リセットイベントを既定のセキュリティ (SYSTEM の既定 DACL) で
            // 作るだけ。ハンドルは OwnedHandle が閉じる。
            let h = unsafe {
                CreateEventW(
                    None,
                    true,
                    false,
                    &windows::core::HSTRING::from(STOP_EVENT_NAME),
                )
            }?;
            Ok(Self(OwnedHandle(h)))
        }

        fn set(&self) {
            // SAFETY: 有効なイベントハンドルをシグナル状態にするだけ。
            let _ = unsafe { SetEvent(self.0.0) };
        }

        fn reset(&self) {
            // SAFETY: 有効なイベントハンドルを非シグナル状態に戻すだけ。
            let _ = unsafe { ResetEvent(self.0.0) };
        }
    }

    /// 本体とトレイを入れるジョブ。サービスプロセスが異常終了してハンドルが閉じると、OS が中の
    /// プロセスをまとめて終了する (KILL_ON_JOB_CLOSE)。入れないと孤児の本体がポートと単一
    /// インスタンスを握り続け、再起動したサービスの本体が即終了するバックオフに陥る。
    /// 本体が起こす孫 (自己更新の setup 等) はジョブに入れない (SILENT_BREAKAWAY_OK)。
    /// 入れると、更新中にサービスを止めた時点で setup ごと殺されてしまう。
    struct Job(OwnedHandle);

    impl Job {
        fn new() -> Result<Self> {
            // SAFETY: 無名ジョブを既定のセキュリティで作るだけ。ハンドルは OwnedHandle が閉じる。
            let job = OwnedHandle(unsafe { CreateJobObjectW(None, PCWSTR::null()) }?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
            // SAFETY: job は有効なジョブハンドル。info は呼び出し中有効で、長さはその実サイズ。
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }?;
            Ok(Self(job))
        }
    }

    /// サービスが起動して見張っている本体。
    struct Child {
        process: OwnedHandle,
        pid: u32,
        session: u32,
        started: Instant,
    }

    enum Event {
        Stop,
        /// WTS セッション変化。対象はループ側でアクティブセッションを引き直して決める。
        /// `logon_changed` はサインイン/サインアウトか (閉じたトレイの記憶を捨てる契機)。
        SessionChanged {
            logon_changed: bool,
        },
    }

    /// `--service`: SCM のディスパッチャへ制御を渡す (サービス停止まで戻らない)。
    /// SCM 以外 (コンソール) から直接実行すると即エラーで戻る。
    pub fn run_dispatcher() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("start service dispatcher (run via the service control manager, not directly)")
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!(error = %e, "service stopped with error");
        }
    }

    fn run_service() -> Result<()> {
        let (tx, rx) = mpsc::channel::<Event>();
        let handler = move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = tx.send(Event::Stop);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::SessionChange(param) => {
                    let logon_changed = matches!(
                        param.reason,
                        SessionChangeReason::SessionLogon | SessionChangeReason::SessionLogoff
                    );
                    let _ = tx.send(Event::SessionChanged { logon_changed });
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };
        let status = service_control_handler::register(SERVICE_NAME, handler)
            .context("register service control handler")?;
        let set_state = |state: ServiceState, accepted: ServiceControlAccept| {
            status.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accepted,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
        };
        set_state(
            ServiceState::Running,
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::SESSION_CHANGE,
        )
        .context("report service running")?;
        tracing::info!("service running");

        supervise(&rx, || {
            let _ = status.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 1,
                wait_hint: STOP_WAIT_HINT,
                process_id: None,
            });
        });

        set_state(ServiceState::Stopped, ServiceControlAccept::empty())
            .context("report service stopped")?;
        tracing::info!("service stopped");
        Ok(())
    }

    /// 本体とトレイをアクティブなコンソールセッションに 1 つずつ生かし続ける。Stop で両方止めて戻る
    /// (止め始める前に `on_stopping` で SCM へ停止処理中を伝える)。
    fn supervise(rx: &mpsc::Receiver<Event>, on_stopping: impl Fn()) {
        let forward = super::current_service_forward_args()
            .into_iter()
            .filter(|a| a != "--service")
            .collect::<Vec<_>>();
        // ジョブを作れなくても起動は続ける (サービスが落ちたときの後始末が効かなくなるだけ)。
        let job = Job::new()
            .inspect_err(|e| tracing::warn!(error = %e, "failed to create job object; children may outlive the service"))
            .ok();
        let stop_event = StopEvent::create()
            .inspect_err(|e| tracing::warn!(error = %e, "failed to create host stop event; stopping will skip cleanup"))
            .ok();
        let mut child: Option<Child> = None;
        let mut backoff = BACKOFF_MIN;
        let mut next_spawn = Instant::now();
        let mut tray: Option<Child> = None;
        let mut next_tray = Instant::now();
        // 利用者が「トレイを閉じる」を選んだセッション。そのセッションの間は出し直さない。
        let mut tray_closed_in: Option<u32> = None;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    on_stopping();
                    if let Some(c) = child.take() {
                        stop_host(&c, stop_event.as_ref(), "service stopping");
                    }
                    if let Some(t) = tray.take() {
                        terminate(&t, "service stopping");
                    }
                    return;
                }
                // サインイン/サインアウトをまたいだら、閉じたトレイの記憶は捨てる (同じセッション
                // ID が別のログオンで再利用されうるため、ID だけで覚えていると出し直されない)。
                Ok(Event::SessionChanged { logon_changed }) => {
                    if logon_changed {
                        tray_closed_in = None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            // SAFETY: 引数なしで現在のコンソールセッション ID を返すだけ。
            let target = unsafe { WTSGetActiveConsoleSessionId() };

            if let Some(c) = &child {
                if let Some(code) = exit_code(c) {
                    let ran = c.started.elapsed();
                    if ran >= HEALTHY_RUNTIME {
                        backoff = BACKOFF_MIN;
                        next_spawn = Instant::now();
                    } else {
                        next_spawn = Instant::now() + backoff;
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                    tracing::warn!(
                        pid = c.pid,
                        exit_code = code,
                        ran_ms = ran.as_millis() as u64,
                        "host process exited; respawning"
                    );
                    child = None;
                } else if target != NO_CONSOLE_SESSION && c.session != target {
                    // ログオフ・ユーザー切替: 旧セッションの本体は新しいコンソールを映せない。
                    // 切替の過渡で一瞬「コンソール無し」が返るだけのときは付け替えない
                    // (接続中のクライアントを無用に切ってしまう)。
                    stop_host(c, stop_event.as_ref(), "console session changed");
                    child = None;
                    next_spawn = Instant::now();
                }
            }

            if child.is_none() && target != NO_CONSOLE_SESSION && Instant::now() >= next_spawn {
                // 前回の停止要求が残っていると新しい本体が即終了してしまう。
                if let Some(ev) = &stop_event {
                    ev.reset();
                }
                match spawn_in_session(target, &forward, job.as_ref()) {
                    Ok(c) => {
                        tracing::info!(pid = c.pid, session = c.session, "host process started");
                        child = Some(c);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, session = target, "failed to start host process");
                        next_spawn = Instant::now() + backoff;
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                }
            }

            if let Some(t) = &tray {
                if let Some(code) = exit_code(t) {
                    if code == 0 {
                        // 利用者が「トレイを閉じる」を選んだ。
                        tracing::info!(
                            session = t.session,
                            "tray closed; not restarting in this session"
                        );
                        tray_closed_in = Some(t.session);
                    } else if code == crate::tray::TRAY_ALREADY_RUNNING_EXIT as u32 {
                        // 手動で出したトレイが既に居る。閉じたと見なさず、そちらが閉じられたら
                        // 出し直せるよう間を空けて確かめ直す。
                        next_tray = Instant::now() + TRAY_ALREADY_RUNNING_RETRY;
                    } else {
                        tracing::warn!(exit_code = code, "tray exited abnormally; restarting");
                        next_tray = Instant::now() + TRAY_RETRY;
                    }
                    tray = None;
                } else if target != NO_CONSOLE_SESSION && t.session != target {
                    terminate(t, "console session changed");
                    tray = None;
                }
            }

            if tray.is_none()
                && target != NO_CONSOLE_SESSION
                && tray_closed_in != Some(target)
                && Instant::now() >= next_tray
            {
                match spawn_tray(target, job.as_ref()) {
                    Ok(TraySpawn::Started(t)) => {
                        tracing::info!(pid = t.pid, session = t.session, "tray started");
                        tray = Some(t);
                    }
                    // サインイン画面 (ユーザー未ログオン)。ログオンしたら次の周期で起動する。
                    Ok(TraySpawn::NoUser) => {}
                    // 管理者でないユーザー: このセッションでは出さない (毎周期試さない)。
                    Ok(TraySpawn::NotAdmin) => {
                        tracing::info!(
                            session = target,
                            "signed-in user is not an administrator; tray is not started"
                        );
                        tray_closed_in = Some(target);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, session = target, "failed to start tray");
                        next_tray = Instant::now() + TRAY_RETRY;
                    }
                }
            }
        }
    }

    /// 終了していれば終了コード、生きていれば `None`。
    fn exit_code(c: &Child) -> Option<u32> {
        // SAFETY: c.process は CreateProcessAsUserW が返した有効なプロセスハンドル。待たずに状態だけ見る。
        if unsafe { WaitForSingleObject(c.process.0, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: 終了済みプロセスの終了コードを出力先のスタック変数へ受けるだけ。
        let _ = unsafe { GetExitCodeProcess(c.process.0, &mut code) };
        Some(code)
    }

    /// 本体に後始末をさせてから終了させる。停止要求イベントで自主終了を促し、[`GRACEFUL_STOP`]
    /// 以内に終わらなければ強制終了する。
    fn stop_host(c: &Child, event: Option<&StopEvent>, reason: &str) {
        if let Some(ev) = event {
            tracing::info!(
                pid = c.pid,
                session = c.session,
                reason,
                "asking host to stop"
            );
            ev.set();
            // SAFETY: c.process は有効なプロセスハンドル。終了を上限付きで待つだけ。
            let waited =
                unsafe { WaitForSingleObject(c.process.0, GRACEFUL_STOP.as_millis() as u32) };
            if waited == WAIT_OBJECT_0 {
                return;
            }
            tracing::warn!(pid = c.pid, "host did not stop in time; terminating");
        }
        terminate(c, reason);
    }

    /// 本体/トレイを強制終了する。本体の後始末 (テイクオーバー復元等) は走らないので、本体には
    /// 先に [`stop_host`] を使う。
    fn terminate(c: &Child, reason: &str) {
        tracing::info!(
            pid = c.pid,
            session = c.session,
            reason,
            "terminating process"
        );
        // SAFETY: c.process は有効なプロセスハンドル (PROCESS_TERMINATE を含む全権限で作成済み)。
        let _ = unsafe { TerminateProcess(c.process.0, 1) };
        // SAFETY: 同上。終了完了を最大 5 秒だけ待つ (ポート解放を次の起動より先にするため)。
        let _ = unsafe { WaitForSingleObject(c.process.0, 5000) };
    }

    /// 自分 (LocalSystem) のトークンを複製し、セッションだけを `session` に差し替えて本体を起動する。
    fn spawn_in_session(session: u32, forward: &[String], job: Option<&Job>) -> Result<Child> {
        let mut own = HANDLE::default();
        // SAFETY: 自プロセスのトークンを複製用の権限で開く。own は出力先。
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut own,
            )
        }
        .context("OpenProcessToken(self)")?;
        let own = OwnedHandle(own);

        let mut primary = HANDLE::default();
        // SAFETY: own は有効なトークン。プロセス起動に使う一次トークンとして複製する。
        unsafe {
            DuplicateTokenEx(
                own.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityIdentification,
                TokenPrimary,
                &mut primary,
            )
        }
        .context("DuplicateTokenEx")?;
        let primary = OwnedHandle(primary);

        // SAFETY: primary は直前に複製した一次トークン。u32 のセッション ID を長さ付きで渡す。
        unsafe {
            SetTokenInformation(
                primary.0,
                TokenSessionId,
                (&session as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .context("SetTokenInformation(TokenSessionId) (needs LocalSystem)")?;

        let mut args = vec![SERVICE_CHILD_ARG.to_string()];
        args.extend(forward.iter().cloned());
        // 環境は親 (SYSTEM) を継承する。
        create_process(&primary, &args, None, session, job)
    }

    enum TraySpawn {
        Started(Child),
        NoUser,
        NotAdmin,
    }

    /// ログオンユーザーの昇格 (管理者) トークンでトレイ (`--tray`) を起動する。
    fn spawn_tray(session: u32, job: Option<&Job>) -> Result<TraySpawn> {
        let mut user = HANDLE::default();
        // SAFETY: session のログオンユーザーのトークンを出力先へ受ける (SeTcb 権限 = LocalSystem で可)。
        // 失敗はサインイン画面などユーザー未ログオンの状態。
        if unsafe { WTSQueryUserToken(session, &mut user) }.is_err() {
            return Ok(TraySpawn::NoUser);
        }
        let user = OwnedHandle(user);
        let Some(admin) = elevated_token(user)? else {
            return Ok(TraySpawn::NotAdmin);
        };
        let env = EnvBlock::for_user(&admin)?;
        create_process(&admin, &["--tray".to_string()], Some(&env), session, job)
            .map(TraySpawn::Started)
    }

    /// ユーザートークンの昇格版を返す。UAC の制限付きトークンなら対の昇格トークン (SeTcb 権限
    /// = LocalSystem からは一次トークンとして得られる)、既に昇格済みならそのまま。昇格できない
    /// (管理者でない) ユーザーは `None`。
    fn elevated_token(user: OwnedHandle) -> Result<Option<OwnedHandle>> {
        if is_elevated(&user)? {
            return Ok(Some(user));
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let mut len = 0u32;
        // SAFETY: user は有効なトークン。linked は出力先で、長さはその実サイズ。分割トークンで
        // ないユーザーでは失敗する (= 昇格側が無い)。
        let got = unsafe {
            GetTokenInformation(
                user.0,
                TokenLinkedToken,
                Some((&mut linked as *mut TOKEN_LINKED_TOKEN).cast()),
                std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut len,
            )
        };
        if got.is_err() {
            return Ok(None);
        }
        let linked = OwnedHandle(linked.LinkedToken);
        Ok(is_elevated(&linked)?.then_some(linked))
    }

    fn is_elevated(token: &OwnedHandle) -> Result<bool> {
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        // SAFETY: token は有効なトークン。elevation は出力先で、長さはその実サイズ。
        unsafe {
            GetTokenInformation(
                token.0,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        }
        .context("GetTokenInformation(TokenElevation)")?;
        Ok(elevation.TokenIsElevated != 0)
    }

    /// `CreateEnvironmentBlock` が作るユーザーの環境変数ブロック (Drop で解放)。トレイに利用者の
    /// `%LOCALAPPDATA%` 等を持たせる (SYSTEM の環境を継承させない)。
    struct EnvBlock(*mut std::ffi::c_void);

    impl EnvBlock {
        fn for_user(token: &OwnedHandle) -> Result<Self> {
            let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: token は有効なユーザートークン。env は成功時に確保されたブロックを指し、Drop で解放する。
            unsafe { CreateEnvironmentBlock(&mut env, token.0, false) }
                .context("CreateEnvironmentBlock")?;
            Ok(Self(env))
        }
    }

    impl Drop for EnvBlock {
        fn drop(&mut self) {
            // SAFETY: CreateEnvironmentBlock が確保したブロックを 1 度だけ解放する。
            let _ = unsafe { DestroyEnvironmentBlock(self.0) };
        }
    }

    /// 自分の exe を `token` で `session` の対話デスクトップに起動する。`job` があれば、走り出す前
    /// (停止状態で作って割り当ててから再開) にジョブへ入れる。
    fn create_process(
        token: &OwnedHandle,
        args: &[String],
        env: Option<&EnvBlock>,
        session: u32,
        job: Option<&Job>,
    ) -> Result<Child> {
        let exe = std::env::current_exe().context("get current exe path")?;
        let mut cmdline = quote_arg(&exe.to_string_lossy());
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(&quote_arg(a));
        }
        let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let exe_w: Vec<u16> = exe
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 対話ウィンドウステーションの既定デスクトップ。Winlogon への切替は本体側が追従する。
        let mut desktop: Vec<u16> = "winsta0\\default"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: 全ポインタは本関数のスタック/Vec/env を指し呼び出し中は生存する。cmdline_w は
        // CreateProcessW 系の要件どおり書き込み可能な NUL 終端バッファ。env は UNICODE 形式のブロック。
        unsafe {
            CreateProcessAsUserW(
                token.0,
                PCWSTR(exe_w.as_ptr()),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
                env.map(|e| e.0 as *const std::ffi::c_void),
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        }
        .context("CreateProcessAsUserW")?;
        let thread = OwnedHandle(pi.hThread);
        let process = OwnedHandle(pi.hProcess);
        if let Some(job) = job {
            // SAFETY: どちらも直前に作った有効なハンドル。プロセスはまだ停止状態。
            if let Err(e) = unsafe { AssignProcessToJobObject(job.0.0, process.0) } {
                tracing::warn!(error = %e, "failed to assign process to job object");
            }
        }
        // SAFETY: CREATE_SUSPENDED で作った主スレッドを再開する。
        unsafe { ResumeThread(thread.0) };
        drop(thread);
        Ok(Child {
            process,
            pid: pi.dwProcessId,
            session,
            started: Instant::now(),
        })
    }

    /// 空白・タブを含む (または空の) 引数だけ二重引用符で囲む。値はローカル CLI / インストーラ由来。
    fn quote_arg(a: &str) -> String {
        if a.is_empty() || a.contains(' ') || a.contains('\t') {
            format!("\"{a}\"")
        } else {
            a.to_string()
        }
    }

    /// サービス配下の本体側: サービスからの停止要求を待つスレッドを起こし、届いたら `on_stop` を
    /// 呼ぶ (`on_stop` は後始末をしてプロセスを終了する想定)。イベントが開けなければ何もしない
    /// (サービスはその場合 [`GRACEFUL_STOP`] 後に強制終了する)。
    pub fn watch_stop_request(on_stop: impl FnOnce() + Send + 'static) {
        use windows::Win32::System::Threading::{
            INFINITE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE,
        };
        // SAFETY: 名前付きイベントを待機権限だけで開く。
        let event = match unsafe {
            OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &windows::core::HSTRING::from(STOP_EVENT_NAME),
            )
        } {
            Ok(h) => OwnedHandle(h),
            Err(e) => {
                tracing::warn!(error = %e, "cannot open service stop event; stop will be forced");
                return;
            }
        };
        let _ = std::thread::Builder::new()
            .name("stop-watch".into())
            .spawn(move || {
                // フィールド (HANDLE) 単位でなく Send なラッパーごとスレッドへ移す。
                let event = event;
                // SAFETY: event は有効なイベントハンドル。シグナルされるまで待つだけ。
                if unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
                    tracing::info!("stop requested by service");
                    on_stop();
                }
            });
    }

    /// `--install-service`: サービスを登録 (既存なら設定更新) して開始する。管理者権限が要る。
    /// 旧方式のログオン時タスクは、同じセッションで二重起動になる (先に起きた方だけが残り、
    /// どちらが SYSTEM か不定になる) ため削除する。
    pub fn install(forward_args: &[String]) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("open service manager (run as administrator)")?;
        let exe = std::env::current_exe().context("get current exe path")?;
        let mut launch_arguments = vec![OsString::from("--service")];
        launch_arguments.extend(forward_args.iter().map(OsString::from));
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("Aurora Host"),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe.clone(),
            launch_arguments,
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let access =
            ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
        let service = match manager.open_service(SERVICE_NAME, access) {
            Ok(s) => {
                s.change_config(&info).context("update service config")?;
                s
            }
            Err(_) => manager
                .create_service(&info, access)
                .context("create service")?,
        };
        service
            .set_description("Aurora リモートデスクトップのホスト (UAC / ロック画面にも対応)")
            .context("set service description")?;
        // サービス自体が落ちたら遠隔から二度と入れなくなる (無人運用)。SCM に再起動させる。
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    };
                    3
                ]),
            })
            .context("set service failure actions")?;

        if crate::system::is_installed() {
            crate::system::uninstall().context("remove legacy logon task")?;
        }
        allow_through_firewall(&exe);

        let running = service
            .query_status()
            .map(|s| s.current_state != ServiceState::Stopped)
            .unwrap_or(false);
        if !running {
            service.start(&[] as &[&str]).context("start service")?;
        }
        println!("installed and started service \"{SERVICE_NAME}\"");
        if running {
            println!(
                "note: the service was already running; restart it to apply the new arguments"
            );
        }
        Ok(())
    }

    /// `--uninstall-service`: サービスを停止して削除する。未登録なら何もしない。
    pub fn uninstall() -> Result<()> {
        remove_firewall_rule();
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("open service manager (run as administrator)")?;
        let service = match manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        ) {
            Ok(s) => s,
            Err(_) => {
                println!("service \"{SERVICE_NAME}\" is not installed");
                return Ok(());
            }
        };
        let state = service
            .query_status()
            .context("query service status")?
            .current_state;
        if state != ServiceState::Stopped {
            service.stop().context("stop service")?;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let s = service.query_status().context("query service status")?;
                if s.current_state == ServiceState::Stopped {
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("service did not stop within 15s");
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        service.delete().context("delete service")?;
        println!("removed service \"{SERVICE_NAME}\"");
        Ok(())
    }

    /// 本体の受信をファイアウォールで許可する (既存の同名規則は作り直す)。SYSTEM サービスの本体が
    /// 待ち受けても Windows は「アクセスを許可しますか」を出さず黙って遮断するので、登録時に
    /// 明示的に許可しないと外 (Tailnet 含む) から一切つながらない。公開範囲は待受アドレス
    /// (既定は Tailscale IP) が絞るので、規則はプログラム単位・全プロファイルにする。
    /// 失敗してもサービス登録は失敗扱いにしない (ファイアウォール無効の環境等) が、知らせる。
    fn allow_through_firewall(exe: &std::path::Path) {
        remove_firewall_rule();
        let name = format!("name={FIREWALL_RULE_NAME}");
        let program = format!("program={}", exe.display());
        let added = netsh(&[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &name,
            "dir=in",
            "action=allow",
            &program,
            "enable=yes",
            "profile=any",
        ]);
        if added {
            println!("added firewall rule \"{FIREWALL_RULE_NAME}\"");
        } else {
            tracing::warn!("failed to add firewall rule; inbound connections may be blocked");
            eprintln!(
                "warning: could not add firewall rule \"{FIREWALL_RULE_NAME}\";                  allow {} manually if clients cannot connect",
                exe.display()
            );
        }
    }

    /// 同名の規則を全て消す。無ければ何もしない (netsh は失敗を返すが無視してよい)。
    fn remove_firewall_rule() {
        let name = format!("name={FIREWALL_RULE_NAME}");
        netsh(&["advfirewall", "firewall", "delete", "rule", &name]);
    }

    fn netsh(args: &[&str]) -> bool {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        Command::new("netsh")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(target_os = "windows")]
pub use imp::{
    install as install_service, run_dispatcher, uninstall as uninstall_service, watch_stop_request,
};

/// 非 Windows はサービス運用が無い (停止要求も来ない)。
#[cfg(not(target_os = "windows"))]
pub fn watch_stop_request(_on_stop: impl FnOnce() + Send + 'static) {}

#[cfg(not(target_os = "windows"))]
pub fn run_dispatcher() -> anyhow::Result<()> {
    anyhow::bail!("--service is only supported on Windows")
}

#[cfg(not(target_os = "windows"))]
pub fn install_service(_forward_args: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("--install-service is only supported on Windows")
}

#[cfg(not(target_os = "windows"))]
pub fn uninstall_service() -> anyhow::Result<()> {
    anyhow::bail!("--uninstall-service is only supported on Windows")
}
