//! WoL 中継デーモン (aurora-wol)。
//!
//! Tailnet 経由で client からの wake 要求を受け、自宅 LAN へマジックパケットを
//! ブロードキャストして対象 PC を起こす。raspi 等の常時稼働小型機での常駐を想定する。
//! 認証は host と同じ Noise XX-PSK ハンドシェイク。映像セッション (`ControlMsg`) とは
//! 独立したプロトコル ([`WolMsg`]) で、1 接続 = 1 Wake 要求 = 即切断の軽量ラウンドトリップ。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use aurora_common::framing::{decode_wol_msg, read_tcp_frame, send_wol_msg, write_tcp_frame};
use aurora_common::noise::{HandshakeState, psk_from_password};
use aurora_common::proto::WolMsg;
use tokio::net::{TcpListener, TcpStream, UdpSocket};

// バイナリ内蔵インストーラ。raspi 常駐化を 1 バイナリで完結させるため、OS 分岐は
// このディスパッチ 1 箇所に閉じ、実装は install.rs へモジュール単位で切り出す。
#[cfg(target_os = "linux")]
mod install;

/// host (7000) と衝突しない既定ポート。
const DEFAULT_PORT: u16 = 7100;
/// ハンドシェイクの各 I/O タイムアウト。host の `HANDSHAKE_TIMEOUT` と同じ理由
/// (認証前の無応答接続で accept ループが詰まる DoS を防ぐ)。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// ハンドシェイク〜Wake 受信〜Ack 送信を含む 1 接続あたりの全体タイムアウト。
const SESSION_TIMEOUT: Duration = Duration::from_secs(15);
/// マジックパケットの宛先ごとの送出回数 (パケットロス対策)。
const SEND_REPEAT: usize = 3;
/// 同時ハンドシェイク接続数の上限。accept ごとの無制限 spawn によるコネクション
/// フラッド DoS (D-3) を抑える。
const MAX_WOL_CLIENTS: usize = 8;

struct Args {
    port: u16,
    password: Option<String>,
    bind: Option<IpAddr>,
}

fn parse_args() -> Result<Args> {
    let mut port = DEFAULT_PORT;
    let mut password: Option<String> = None;
    let mut password_file: Option<PathBuf> = None;
    let mut bind: Option<IpAddr> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => {
                let v = it.next().context("--port requires a value")?;
                port = v.parse().context("--port expects an integer 1..=65535")?;
            }
            "--password" => {
                password = Some(it.next().context("--password requires a value")?);
            }
            "--password-file" => {
                let v = it.next().context("--password-file requires a value")?;
                password_file = Some(PathBuf::from(v));
            }
            "--bind" => {
                let v = it.next().context("--bind requires a value")?;
                bind = Some(v.parse().context("--bind expects an IP address")?);
            }
            other => bail!("unknown arg: {other}"),
        }
    }
    // 優先順: 引数 > ファイル (どちらも無ければ main() で終了する)。
    if password.is_none()
        && let Some(path) = password_file
    {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read password file: {}", path.display()))?;
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            password = Some(trimmed);
        }
    }
    Ok(Args {
        port,
        password,
        bind,
    })
}

fn main() -> Result<()> {
    // install/uninstall はユーザー対話の CLI ツールであり、常駐サーバの起動経路
    // (ログ初期化・パスワード必須化) とは別物のため、最初に分岐して素通しする。
    if let Some(subcmd) = std::env::args().nth(1)
        && (subcmd == "install" || subcmd == "uninstall")
    {
        #[cfg(target_os = "linux")]
        {
            return install::run(&subcmd, std::env::args().skip(2));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = subcmd;
            bail!("install/uninstall は Linux (systemd) 専用です");
        }
    }

    let args = parse_args()?;

    // raspi 常駐前提でコンソール接続を仮定せず、host/client と同じ集約ログ初期化を使う
    // (%LOCALAPPDATA%\Aurora\logs 相当、非 Windows は ~/.local/share/Aurora/logs)。
    let _log_guard = aurora_common::logging::init(aurora_common::logging::LogOptions {
        file_stem: "wol",
        filter: tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "aurora=info".into()),
        dir: aurora_common::logging::default_log_dir(),
        console: true,
    });

    let Some(password) = args.password else {
        eprintln!(
            "error: password required (--password <PASS> or --password-file <PATH>, file must be chmod 600)"
        );
        std::process::exit(2);
    };
    let psk = psk_from_password(&password);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        port = args.port,
        "aurora-wol starting"
    );

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_server(args.bind, args.port, psk))
}

/// `tailscale ip -4` のコマンドを構築する (サーバ側 async とインストーラ側 sync で共有)。
pub(crate) fn tailscale_ip_cmd() -> std::process::Command {
    let mut cmd = std::process::Command::new("tailscale");
    cmd.args(["ip", "-4"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `tailscale ip -4` の標準出力から最初の IPv4 行を取り出す。
pub(crate) fn parse_tailscale_ip(text: &str) -> Option<IpAddr> {
    text.lines()
        .map(str::trim)
        .find_map(|l| l.parse::<Ipv4Addr>().ok())
        .map(IpAddr::V4)
}

/// `tailscale ip -4` を実行して tailnet IPv4 を取得する。CLI 不在・非ゼロ終了・パース失敗は None。
async fn tailscale_ip() -> Option<IpAddr> {
    let out = tokio::process::Command::from(tailscale_ip_cmd())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    parse_tailscale_ip(&text)
}

/// bind 時に実効待受 IP を解決する。`--bind` 明示ならそれを、未指定なら Tailscale IP を待つ。
/// raspi が tailscaled より先に起動しうるためポーリングで待ち、TIMEOUT 超過で loopback に
/// fail-close する (LAN/Tailnet 到達不能のまま。--bind で明示公開できる旨を warn)。
async fn resolve_bind_ip(explicit: Option<IpAddr>) -> IpAddr {
    const POLL: Duration = Duration::from_secs(2);
    const TIMEOUT: Duration = Duration::from_secs(120);
    if let Some(ip) = explicit {
        return ip;
    }
    let start = Instant::now();
    let mut waited = false;
    loop {
        if let Some(ip) = tailscale_ip().await {
            tracing::info!(%ip, "binding to Tailscale interface (auto; override with --bind)");
            return ip;
        }
        if start.elapsed() >= TIMEOUT {
            tracing::warn!(
                "Tailscale IP not found; binding to 127.0.0.1 (loopback only). Pass --bind <ip> to expose (e.g. a Tailscale/LAN IP, or 0.0.0.0 for all interfaces)."
            );
            return IpAddr::V4(Ipv4Addr::LOCALHOST);
        }
        if !waited {
            tracing::info!("Tailscale IP not ready yet; waiting before bind");
            waited = true;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// 送信元 IP ごとの認証失敗を追跡し、連続失敗で指数的に遅延させるスロットル。
/// host (net/auth.rs) と同じポリシー。ロックアウト無しの単一要素 PSK へのオンライン総当たりの
/// レートを IP 単位で落とす。TCP ハンドシェイクを完了した相手の IP のみ記録するため送信元詐称は効かない。
struct WolThrottle {
    inner: Mutex<HashMap<IpAddr, FailRecord>>,
}

struct FailRecord {
    fails: u32,
    last: Instant,
}

impl WolThrottle {
    const RESET_AFTER: Duration = Duration::from_secs(600);
    const BASE_DELAY: Duration = Duration::from_secs(1);
    const MAX_DELAY: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn record_failure(&self, ip: IpAddr) -> Duration {
        let now = Instant::now();
        let Ok(mut map) = self.inner.lock() else {
            return Self::BASE_DELAY;
        };
        // 十分古いエントリは掃除する (無制限 IP でメモリが増え続けないように)。
        map.retain(|_, r| now.duration_since(r.last) < Self::RESET_AFTER);
        let rec = map.entry(ip).or_insert(FailRecord {
            fails: 0,
            last: now,
        });
        rec.fails = rec.fails.saturating_add(1);
        rec.last = now;
        let shift = (rec.fails - 1).min(5);
        Self::BASE_DELAY
            .checked_mul(1u32 << shift)
            .unwrap_or(Self::MAX_DELAY)
            .min(Self::MAX_DELAY)
    }

    fn record_success(&self, ip: IpAddr) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&ip);
        }
    }
}

async fn run_server(bind: Option<IpAddr>, port: u16, psk: [u8; 32]) -> Result<()> {
    let bind_ip = resolve_bind_ip(bind).await;
    let addr = SocketAddr::new(bind_ip, port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "listening");

    // コネクションフラッド DoS (D-3) を抑える同時接続数上限。
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_WOL_CLIENTS));
    // 送信元 IP 単位の認証スロットル (D-1)。全接続で共有する。
    let throttle = Arc::new(WolThrottle::new());

    loop {
        let (stream, peer) = listener.accept().await?;
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(%peer, max = MAX_WOL_CLIENTS, "connection limit reached; rejecting");
                drop(stream);
                continue;
            }
        };
        let throttle = throttle.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(SESSION_TIMEOUT, handle_conn(stream, psk, peer, throttle))
                .await
            {
                Ok(Ok(())) => tracing::info!(%peer, "session complete"),
                Ok(Err(e)) => tracing::warn!(%peer, error = %e, "session failed"),
                Err(_) => tracing::warn!(%peer, "session timed out"),
            }
        });
    }
}

/// 1 接続を処理する: Noise ハンドシェイク → `WolMsg::Wake` 受信 → マジックパケット送出 →
/// `WolMsg::Ack` 返信 → (呼び出し元で) 切断。Wake 以外/フレーム異常は即エラーで切断する。
async fn handle_conn(
    mut stream: TcpStream,
    psk: [u8; 32],
    peer: SocketAddr,
    throttle: Arc<WolThrottle>,
) -> Result<()> {
    let mut hs = HandshakeState::new_responder(&psk, None)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();

    // host (net/client.rs) と同じ 3 メッセージ Noise XX-PSK ハンドシェイク。失敗時 (主に PSK
    // 不一致による復号失敗) は送信元 IP 単位の指数バックオフを挟んでからエラーを返す (D-1)。
    let session = match async {
        // <- e
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (e)")??;
        hs.read_msg(&buf, &mut tmp)?;
        // -> e, ee, s, es
        let mut out_msg = Vec::new();
        hs.write_msg(&[], &mut out_msg)?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, write_tcp_frame(&mut stream, &out_msg))
            .await
            .context("handshake timed out (e,ee,s,es)")??;
        // <- s, se, psk
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (s,se,psk)")??;
        hs.read_msg(&buf, &mut tmp)?;
        let session = hs.into_transport()?;
        Ok::<_, anyhow::Error>(session)
    }
    .await
    {
        Ok(s) => {
            throttle.record_success(peer.ip());
            s
        }
        Err(e) => {
            // PSK 不一致は最終メッセージの復号失敗として出る。送信元 IP 単位の指数バックオフで
            // オンライン総当たりレートを落とす (host と対称)。
            let delay = throttle.record_failure(peer.ip());
            tracing::warn!(%peer, error = %e, delay_ms = delay.as_millis() as u64, "handshake failed; throttling source IP");
            tokio::time::sleep(delay).await;
            return Err(e);
        }
    };
    tracing::info!("noise handshake complete");

    let mut tx_nonce: u64 = 0;
    let mut rx_nonce: u64 = 0;
    let mut plain = Vec::new();

    read_tcp_frame(&mut stream, &mut buf).await?;
    let msg = decode_wol_msg(&session, &buf, &mut rx_nonce, &mut plain)?;
    let WolMsg::Wake { mac } = msg else {
        bail!("expected Wake, got {msg:?}");
    };

    let (ok, message) = match send_magic_packet(mac).await {
        Ok(()) => (true, "magic packet sent".to_string()),
        Err(e) => (false, format!("send failed: {e}")),
    };
    if ok {
        tracing::info!(mac = %format_mac(mac), "magic packet sent");
    } else {
        tracing::warn!(mac = %format_mac(mac), message = %message, "magic packet send failed");
    }

    let ack = WolMsg::Ack { ok, message };
    send_wol_msg(&mut stream, &session, &ack, &mut tx_nonce).await?;
    Ok(())
}

fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// マジックパケット (0xFF×6 + MAC×16 の 102B) を UDP ブロードキャストで送出する。
/// グローバルブロードキャスト (`255.255.255.255:9`) に加え、検出できたローカル IPv4
/// インターフェースそれぞれのサブネットブロードキャストアドレスへも送る (ルータ/NIC 設定
/// によってはグローバルブロードキャストが素通りしないことがあるため二重化する)。宛先ごとに
/// [`SEND_REPEAT`] 回送出しパケットロスに備える。1 件でも送出できれば成功とする。
async fn send_magic_packet(mac: [u8; 6]) -> Result<()> {
    let mut packet = Vec::with_capacity(102);
    packet.extend_from_slice(&[0xFFu8; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .context("bind UDP socket")?;
    socket.set_broadcast(true).context("set SO_BROADCAST")?;

    let mut targets: Vec<SocketAddr> =
        vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 9))];
    targets.extend(
        local_broadcast_addrs()
            .into_iter()
            .map(|ip| SocketAddr::V4(SocketAddrV4::new(ip, 9))),
    );

    let mut last_err = None;
    let mut any_ok = false;
    for target in &targets {
        for _ in 0..SEND_REPEAT {
            match socket.send_to(&packet, target).await {
                Ok(_) => any_ok = true,
                Err(e) => last_err = Some(e),
            }
        }
    }
    if any_ok {
        Ok(())
    } else {
        Err(last_err
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("no broadcast target reachable")))
    }
}

/// 検出できたローカル IPv4 インターフェース (ループバック除く) のサブネットブロードキャスト
/// アドレス一覧を返す。取得失敗時は空 (グローバルブロードキャストのみで継続する)。
fn local_broadcast_addrs() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = get_if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            get_if_addrs::IfAddr::V4(v4) => Some(v4.broadcast.unwrap_or_else(|| {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                Ipv4Addr::from(ip | !mask)
            })),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_common::noise::HandshakeState;
    use tokio::net::TcpListener;

    /// ハンドシェイク → Wake → マジックパケット送出 (ループバック) → Ack の一連が通ることを
    /// 確認する。実 LAN のマジックパケット受信検証はできないため、送出処理自体が成功する
    /// ことだけを見る (実対象への到達確認は実機任せ)。
    #[tokio::test]
    async fn wake_roundtrip_over_loopback() {
        let psk = psk_from_password("hunter2-test-password");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let throttle = Arc::new(WolThrottle::new());
            handle_conn(stream, psk, peer, throttle).await.unwrap();
        });

        // クライアント役 (initiator) を手動でハンドシェイクさせ、Wake を送って Ack を待つ。
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut hs = HandshakeState::new_initiator(&psk, None).unwrap();
        let mut buf = Vec::new();
        let mut tmp = Vec::new();
        hs.write_msg(&[], &mut buf).unwrap();
        write_tcp_frame(&mut stream, &buf).await.unwrap();
        read_tcp_frame(&mut stream, &mut buf).await.unwrap();
        hs.read_msg(&buf, &mut tmp).unwrap();
        hs.write_msg(&[], &mut buf).unwrap();
        write_tcp_frame(&mut stream, &buf).await.unwrap();
        let session = hs.into_transport().unwrap();

        let mut tx_nonce = 0u64;
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        send_wol_msg(&mut stream, &session, &WolMsg::Wake { mac }, &mut tx_nonce)
            .await
            .unwrap();

        let mut rx_nonce = 0u64;
        let mut plain = Vec::new();
        read_tcp_frame(&mut stream, &mut buf).await.unwrap();
        let ack = decode_wol_msg(&session, &buf, &mut rx_nonce, &mut plain).unwrap();
        match ack {
            WolMsg::Ack { ok, .. } => assert!(ok, "magic packet send should succeed on loopback"),
            other => panic!("unexpected msg: {other:?}"),
        }

        server.await.unwrap();
    }
}
