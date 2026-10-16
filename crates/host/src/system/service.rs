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

/// SCM に登録するサービス名 (`sc query AuroraHost` で引ける)。
#[cfg(target_os = "windows")]
const SERVICE_NAME: &str = "AuroraHost";
/// サービス配下で起動された本体である印 (CLI 引数)。サービスだけが付ける。
pub const SERVICE_CHILD_ARG: &str = "--service-child";

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
        DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
    };
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
        GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
        WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{SERVICE_CHILD_ARG, SERVICE_NAME};

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

    /// サービスが起動して見張っている本体。
    struct Child {
        process: OwnedHandle,
        pid: u32,
        session: u32,
        started: Instant,
    }

    enum Event {
        Stop,
        /// WTS セッション変化。中身は見ず、ループ側でアクティブセッションを引き直す。
        SessionChanged,
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
                ServiceControl::SessionChange(_) => {
                    let _ = tx.send(Event::SessionChanged);
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

        supervise(&rx);

        set_state(ServiceState::Stopped, ServiceControlAccept::empty())
            .context("report service stopped")?;
        tracing::info!("service stopped");
        Ok(())
    }

    /// 本体をアクティブなコンソールセッションに 1 つだけ生かし続ける。Stop で本体を止めて戻る。
    fn supervise(rx: &mpsc::Receiver<Event>) {
        let forward = super::current_service_forward_args()
            .into_iter()
            .filter(|a| a != "--service")
            .collect::<Vec<_>>();
        let mut child: Option<Child> = None;
        let mut backoff = BACKOFF_MIN;
        let mut next_spawn = Instant::now();
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(c) = child.take() {
                        terminate(&c, "service stopping");
                    }
                    return;
                }
                Ok(Event::SessionChanged) | Err(mpsc::RecvTimeoutError::Timeout) => {}
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
                } else if c.session != target {
                    // ログオフ・ユーザー切替: 旧セッションの本体は新しいコンソールを映せない。
                    terminate(c, "console session changed");
                    child = None;
                    next_spawn = Instant::now();
                }
            }

            if child.is_none() && target != NO_CONSOLE_SESSION && Instant::now() >= next_spawn {
                match spawn_in_session(target, &forward) {
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

    /// 本体を強制終了する。本体は終了処理を持たないが、ProjFS の subst 残骸と VDD テイクオーバーは
    /// 次回起動時に掃除/復元されるので、ここで後始末を待たなくてよい。
    fn terminate(c: &Child, reason: &str) {
        tracing::info!(
            pid = c.pid,
            session = c.session,
            reason,
            "terminating host process"
        );
        // SAFETY: c.process は有効なプロセスハンドル (PROCESS_TERMINATE を含む全権限で作成済み)。
        let _ = unsafe { TerminateProcess(c.process.0, 1) };
        // SAFETY: 同上。終了完了を最大 5 秒だけ待つ (ポート解放を次の起動より先にするため)。
        let _ = unsafe { WaitForSingleObject(c.process.0, 5000) };
    }

    /// 自分 (LocalSystem) のトークンを複製し、セッションだけを `session` に差し替えて本体を起動する。
    fn spawn_in_session(session: u32, forward: &[String]) -> Result<Child> {
        let exe = std::env::current_exe().context("get current exe path")?;

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

        let mut cmdline = quote_arg(&exe.to_string_lossy());
        cmdline.push(' ');
        cmdline.push_str(SERVICE_CHILD_ARG);
        for a in forward {
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
        // SAFETY: 全ポインタは本関数のスタック/Vec を指し呼び出し中は生存する。cmdline_w は
        // CreateProcessW 系の要件どおり書き込み可能な NUL 終端バッファ。環境は親 (SYSTEM) を継承。
        unsafe {
            CreateProcessAsUserW(
                primary.0,
                PCWSTR(exe_w.as_ptr()),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        }
        .context("CreateProcessAsUserW")?;
        // スレッドハンドルは使わないので即閉じる。
        drop(OwnedHandle(pi.hThread));
        Ok(Child {
            process: OwnedHandle(pi.hProcess),
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

    /// `--install-service`: サービスを登録 (既存なら設定更新) して開始する。管理者権限が要る。
    /// 旧方式のログオン時タスクは、同じセッションで二重起動になる (先に起きた方だけが残り、
    /// どちらが SYSTEM か不定になる) ため削除する。
    pub fn install(forward_args: &[String]) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("open service manager (run as administrator)")?;
        let mut launch_arguments = vec![OsString::from("--service")];
        launch_arguments.extend(forward_args.iter().map(OsString::from));
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("Aurora Host"),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe().context("get current exe path")?,
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
}

#[cfg(target_os = "windows")]
pub use imp::{install as install_service, run_dispatcher, uninstall as uninstall_service};

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
