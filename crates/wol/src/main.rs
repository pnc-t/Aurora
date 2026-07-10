//! WoL 中継デーモン (aurora-wol)。
//!
//! Tailnet 経由で client からの wake 要求を受け、自宅 LAN へマジックパケットを
//! ブロードキャストして対象 PC を起こす。raspi 等の常時稼働小型機での常駐を想定する。
//! 認証は host と同じ Noise XX-PSK ハンドシェイク。映像セッション (`ControlMsg`) とは
//! 独立したプロトコル ([`WolMsg`]) で、1 接続 = 1 Wake 要求 = 即切断の軽量ラウンドトリップ。

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aurora_common::framing::{decode_wol_msg, read_tcp_frame, send_wol_msg, write_tcp_frame};
use aurora_common::noise::{HandshakeState, psk_from_password};
use aurora_common::proto::WolMsg;
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// host (7000) と衝突しない既定ポート。
const DEFAULT_PORT: u16 = 7100;
/// ハンドシェイクの各 I/O タイムアウト。host の `HANDSHAKE_TIMEOUT` と同じ理由
/// (認証前の無応答接続で accept ループが詰まる DoS を防ぐ)。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// ハンドシェイク〜Wake 受信〜Ack 送信を含む 1 接続あたりの全体タイムアウト。
const SESSION_TIMEOUT: Duration = Duration::from_secs(15);
/// マジックパケットの宛先ごとの送出回数 (パケットロス対策)。
const SEND_REPEAT: usize = 3;

struct Args {
    port: u16,
    password: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut port = DEFAULT_PORT;
    let mut password: Option<String> = None;
    let mut password_file: Option<PathBuf> = None;
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
            other => bail!("unknown arg: {other}"),
        }
    }
    // 優先順: 引数 > ファイル (どちらも無ければ main() で終了する)。
    if password.is_none() {
        if let Some(path) = password_file {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read password file: {}", path.display()))?;
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                password = Some(trimmed);
            }
        }
    }
    Ok(Args { port, password })
}

fn main() -> Result<()> {
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
    rt.block_on(run_server(args.port, psk))
}

async fn run_server(port: u16, psk: [u8; 32]) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            match tokio::time::timeout(SESSION_TIMEOUT, handle_conn(stream, psk)).await {
                Ok(Ok(())) => tracing::info!(%peer, "session complete"),
                Ok(Err(e)) => tracing::warn!(%peer, error = %e, "session failed"),
                Err(_) => tracing::warn!(%peer, "session timed out"),
            }
        });
    }
}

/// 1 接続を処理する: Noise ハンドシェイク → `WolMsg::Wake` 受信 → マジックパケット送出 →
/// `WolMsg::Ack` 返信 → (呼び出し元で) 切断。Wake 以外/フレーム異常は即エラーで切断する。
async fn handle_conn(mut stream: TcpStream, psk: [u8; 32]) -> Result<()> {
    let mut hs = HandshakeState::new_responder(&psk, None)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();

    // host (net/client.rs) と同じ 3 メッセージ Noise XX-PSK ハンドシェイク。
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
            let (stream, _peer) = listener.accept().await.unwrap();
            handle_conn(stream, psk).await.unwrap();
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
