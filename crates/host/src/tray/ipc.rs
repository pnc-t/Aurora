//! SYSTEM サービス運用で、本体 (SYSTEM) と単独トレイ (`--tray`、管理者の昇格権限) をつなぐ名前付きパイプ。
//!
//! 形式: 1 要求 = `u32 LE 長さ + JSON` ([`TrayRequest`])、応答も同じ形式 ([`TrayReply`])。
//!
//! Windows 固有の落とし穴 (パイプは同じ PC の任意プロセスから名前で開けるため):
//! - **接続できる相手を絞る**: DACL は SYSTEM / Administrators のみ。トレイはサービスが昇格
//!   (管理者) トークンで起動するので開ける。UAC の制限付きトークンでは Administrators が拒否専用
//!   になるため、同じユーザーでも通常権限のプロセスは開けない (開けると、パスワードの総当たりや
//!   未設定時の先回り設定をされうる)。さらに本体と同じセッションのクライアントだけ受け付ける
//!   (別セッションの RDP ユーザー・切替中ユーザーを弾く)。ネットワーク越し (SMB) は拒否。
//! - **相手が本物の本体か確かめる**: 本体より先に一般ユーザーが同名パイプを作っておけば、トレイが
//!   入力したパスワードを盗める。トレイはパイプの所有者が SYSTEM / Administrators であることを
//!   確認してから送る (一般ユーザーは所有者を SYSTEM にしたオブジェクトを作れない)。
//! - PSK は流さない (照合は本体側。`control` の doc 参照)。

use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::control::{TrayContext, TrayReply, TrayRequest, handle};

/// パイプ名。
const PIPE_NAME: &str = r"\\.\pipe\AuroraHostTray";
/// 1 メッセージの上限 (状態スナップショットでも数 KB。巨大長さでのメモリ確保を防ぐ)。
const MAX_MESSAGE: usize = 256 * 1024;
/// トレイがパイプを開く権限: FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES |
/// READ_CONTROL | SYNCHRONIZE (読み書きと所有者確認に要る最小限)。
const CLIENT_ACCESS: u32 = 0x0012_0083;
/// パイプの DACL (継承なし)。昇格した管理者と SYSTEM のみ。
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
/// 同時に受け付けるトレイ接続の上限 (サービスが起こすトレイ + 手動で出したもの程度)。
const MAX_CLIENTS: usize = 4;
/// トレイが本体の応答を待つ上限。本体が固まってもトレイ UI まで固めない。
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// ---- 本体 (サーバ) 側 ----

/// SDDL から作ったセキュリティ記述子 (Drop で LocalFree)。パイプインスタンスを作るたびに使う。
struct PipeSecurity(windows::Win32::Security::PSECURITY_DESCRIPTOR);

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW が LocalAlloc した記述子を 1 度だけ解放する。
        let _ = unsafe { LocalFree(HLOCAL(self.0.0)) };
    }
}

// SAFETY: 記述子は作成後に読み取り専用で参照するだけ (どのスレッドから使っても同じ内容)。
unsafe impl Send for PipeSecurity {}
// SAFETY: 同上。
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    fn new() -> anyhow::Result<Self> {
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::PSECURITY_DESCRIPTOR;
        use windows::core::HSTRING;

        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL は定数文字列。成功時 sd は LocalAlloc 領域で、Drop で解放する。
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(PIPE_SDDL),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| anyhow::anyhow!("parse pipe SDDL: {e}"))?;
        Ok(Self(sd))
    }

    fn create(
        &self,
        first: bool,
    ) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        use windows::Win32::Security::SECURITY_ATTRIBUTES;
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: attrs は呼び出し中有効な SECURITY_ATTRIBUTES で、記述子は self が保持している。
        unsafe {
            tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    PIPE_NAME,
                    (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }
}

/// 本体側: トレイからの要求を受け続ける (戻らない。致命的な失敗はログに残して終わる)。
pub async fn serve(ctx: TrayContext) {
    if let Err(e) = serve_inner(ctx).await {
        tracing::error!(error = %e, "tray pipe server stopped; the tray cannot control the host");
    }
}

async fn serve_inner(ctx: TrayContext) -> anyhow::Result<()> {
    let security = PipeSecurity::new()?;
    let own_session = own_session_id()?;
    // 最初のインスタンスは「既に同名パイプがある (= 先回りで作られた)」なら失敗させる。
    let mut server = security.create(true).map_err(|e| {
        anyhow::anyhow!("create tray pipe (already created by another process?): {e}")
    })?;
    tracing::info!("tray pipe listening");
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CLIENTS));
    loop {
        // 1 回の失敗でループごと終えると、以後トレイから一切操作できなくなる。作り直して続ける。
        if let Err(e) = server.connect().await {
            tracing::warn!(error = %e, "tray pipe connect failed; recreating instance");
            server = next_instance(&security).await;
            continue;
        }
        let client = server;
        server = next_instance(&security).await;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("too many tray pipe clients; dropping new connection");
            continue;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = handle_client(client, ctx, own_session).await {
                tracing::debug!(error = %e, "tray pipe client ended");
            }
        });
    }
}

/// 次の待受インスタンスを作る。作れるまで間を空けて繰り返す (一過性の失敗で待受を失わない)。
async fn next_instance(
    security: &PipeSecurity,
) -> tokio::net::windows::named_pipe::NamedPipeServer {
    loop {
        match security.create(false) {
            Ok(s) => return s,
            Err(e) => {
                tracing::warn!(error = %e, "create tray pipe instance failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_client(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    ctx: TrayContext,
    own_session: u32,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeClientSessionId;

    let mut session = u32::MAX;
    // SAFETY: pipe は接続済みの有効なパイプハンドル。session は出力先のスタック変数。
    unsafe { GetNamedPipeClientSessionId(HANDLE(pipe.as_raw_handle()), &mut session) }?;
    if session != own_session {
        tracing::warn!(
            client_session = session,
            own_session,
            "rejected tray pipe client from another session"
        );
        return Ok(());
    }
    loop {
        let len = match pipe.read_u32_le().await {
            Ok(n) => n as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(len <= MAX_MESSAGE, "tray request too large ({len} bytes)");
        let mut buf = vec![0u8; len];
        pipe.read_exact(&mut buf).await?;
        let req: TrayRequest = serde_json::from_slice(&buf)?;
        // パスワード保存・許可リスト書き込み等の同期 I/O を含むので tokio の外で処理する。
        let ctx = ctx.clone();
        let reply = tokio::task::spawn_blocking(move || handle(&ctx, req)).await?;
        let out = serde_json::to_vec(&reply)?;
        pipe.write_u32_le(out.len() as u32).await?;
        pipe.write_all(&out).await?;
    }
}

fn own_session_id() -> anyhow::Result<u32> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut session = 0u32;
    // SAFETY: 自プロセス ID のセッションを出力先のスタック変数へ受けるだけ。
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }?;
    Ok(session)
}

// ---- トレイ (クライアント) 側 ----

/// ワーカーへの 1 要求。`deadline` を過ぎた要求は UI 側が既に「応答なし」と表示済みなので、
/// 本体へ送らずに捨てる (失敗と見せた操作が後から適用されるのを防ぐ)。
struct Job {
    req: TrayRequest,
    reply: mpsc::Sender<TrayReply>,
    deadline: Instant,
}

/// パイプ越しに本体を操作する [`super::TrayControl`]。I/O は専用スレッドで行い、UI スレッドは
/// 応答を [`CALL_TIMEOUT`] までしか待たない。
pub struct PipeControl {
    tx: mpsc::Sender<Job>,
    /// ワーカーが本体と通信中か。本体が固まって前の要求が返らない間は、新しい要求を積まずに
    /// 即「応答なし」を返す (1 秒周期のポーリングごとに UI を 5 秒止めない)。
    busy: Arc<AtomicBool>,
}

impl PipeControl {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = busy.clone();
        let _ = std::thread::Builder::new()
            .name("tray-pipe".into())
            .spawn(move || worker(rx, worker_busy));
        Self { tx, busy }
    }
}

const NO_RESPONSE: &str = "本体が応答しません。\n直前の操作が後から反映される場合があります。";

impl super::TrayControl for PipeControl {
    fn call(&self, req: TrayRequest) -> TrayReply {
        if self.busy.load(Ordering::Acquire) {
            return TrayReply::Failed(NO_RESPONSE.into());
        }
        let (reply, reply_rx) = mpsc::channel();
        let job = Job {
            req,
            reply,
            deadline: Instant::now() + CALL_TIMEOUT,
        };
        if self.tx.send(job).is_err() {
            return TrayReply::Failed("本体に接続できません。".into());
        }
        reply_rx
            .recv_timeout(CALL_TIMEOUT)
            .unwrap_or_else(|_| TrayReply::Failed(NO_RESPONSE.into()))
    }
}

fn worker(rx: mpsc::Receiver<Job>, busy: Arc<AtomicBool>) {
    let mut conn: Option<std::fs::File> = None;
    for job in rx {
        if Instant::now() >= job.deadline {
            continue;
        }
        busy.store(true, Ordering::Release);
        let reply = exchange(&mut conn, &job.req);
        busy.store(false, Ordering::Release);
        let _ = job.reply.send(reply);
    }
}

/// 必要なら繋ぎ直して 1 往復する。失敗したら接続を捨て、次の要求で繋ぎ直す。
fn exchange(conn: &mut Option<std::fs::File>, req: &TrayRequest) -> TrayReply {
    if conn.is_none() {
        match connect() {
            Ok(f) => *conn = Some(f),
            Err(e) => {
                tracing::debug!(error = %e, "tray pipe connect failed");
                return TrayReply::Failed("本体に接続できません。".into());
            }
        }
    }
    let Some(f) = conn.as_mut() else {
        return TrayReply::Failed("本体に接続できません。".into());
    };
    match roundtrip(f, req) {
        Ok(reply) => reply,
        Err(e) => {
            // 本体の再起動などで切れた。次の要求で繋ぎ直す。
            tracing::debug!(error = %e, "tray pipe request failed; reconnecting next time");
            *conn = None;
            TrayReply::Failed("本体に接続できません。".into())
        }
    }
}

fn connect() -> anyhow::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .access_mode(CLIENT_ACCESS)
        .open(PIPE_NAME)?;
    anyhow::ensure!(
        server_is_trusted(&f)?,
        "tray pipe is not owned by SYSTEM/Administrators; refusing to talk to it"
    );
    Ok(f)
}

/// パイプ (サーバが作ったオブジェクト) の所有者が SYSTEM か Administrators か。
fn server_is_trusted(f: &std::fs::File) -> anyhow::Result<bool> {
    use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows::Win32::Security::{
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };

    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: f は開いたパイプのハンドル (READ_CONTROL 付き)。owner は sd の内部を指し、sd は
    // 成功時 LocalAlloc 領域で下で解放する。
    let err = unsafe {
        GetSecurityInfo(
            HANDLE(f.as_raw_handle()),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    anyhow::ensure!(
        err == ERROR_SUCCESS,
        "GetSecurityInfo(pipe) failed: {err:?}"
    );
    // SAFETY: owner は sd が生きている間有効な SID。
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    // SAFETY: GetSecurityInfo が LocalAlloc した記述子を 1 度だけ解放する。
    let _ = unsafe { LocalFree(HLOCAL(sd.0)) };
    Ok(trusted)
}

fn roundtrip(f: &mut std::fs::File, req: &TrayRequest) -> anyhow::Result<TrayReply> {
    let out = serde_json::to_vec(req)?;
    f.write_all(&(out.len() as u32).to_le_bytes())?;
    f.write_all(&out)?;
    let mut len = [0u8; 4];
    f.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_MESSAGE, "tray reply too large ({len} bytes)");
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

/// サービス配下の本体がトレイ用パイプを開いているか (接続はしない)。手動起動 (スタートメニュー等)
/// が本体の二重起動に当たったとき、閉じたトレイを出し直す判断に使う。
pub fn host_pipe_exists() -> bool {
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::core::HSTRING;
    // SAFETY: NUL 終端のパイプ名を渡し、空きインスタンスの有無を最大 1ms 待つだけ。
    unsafe { WaitNamedPipeW(&HSTRING::from(PIPE_NAME), 1) }.as_bool()
}
