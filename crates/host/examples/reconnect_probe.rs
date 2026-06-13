//! 再接続の安定性を GUI 抜きで再現する結合テスト用クライアント。
//!
//! 実際の Tauri クライアントと同じ接続フロー (Noise ハンドシェイク → HostInfo 受信 →
//! UDP "AURHELLO" 送信 → しばらく保持 → 切断) を繰り返し、各サイクルの成否を表示する。
//! 「切断→再接続」を素早く回したときに host が確立直後にセッションを閉じる
//! (= クライアントから見て `read error: early eof`) 現象を切り分けるために使う。
//!
//! 使い方:
//! ```text
//! # term1
//! cargo run -p aurora-host                # host (debug, ログを見る)
//! # term2
//! cargo run -p aurora-host --example reconnect_probe -- <password> [cycles] [hold_ms] [gap_ms]
//! ```
//! 既定: cycles=8, hold_ms=2000, gap_ms=100。永続 ID 鍵 (固定) を使うのでデバイスピン留めの
//! 許可リスト照合経路も実クライアントと同じになる。

use anyhow::{Context, Result, anyhow};
use aurora_common::noise::{
    HandshakeState, NONCE_DOMAIN_CTRL, StaticKeypair, generate_static_keypair, psk_from_password,
};
use aurora_common::proto::ControlMsg;
use bincode::Options;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const ADDR: &str = "127.0.0.1:7000";

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).context("read len")?;
    let n = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).context("read body")?;
    Ok(buf)
}

fn write_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(data)?;
    Ok(())
}

/// 1 セッション: 接続 → ハンドシェイク → HostInfo → UDP hello → hold_ms 保持 → 切断。
fn one_session(psk: &[u8; 32], id: &StaticKeypair, hold: Duration) -> Result<()> {
    let mut stream = TcpStream::connect(ADDR).context("tcp connect")?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;

    let mut hs = HandshakeState::new_initiator(psk, Some(&id.private))?;
    let mut out = Vec::new();
    let mut tmp = Vec::new();
    // -> e
    hs.write_msg(&[], &mut out)?;
    write_frame(&mut stream, &out)?;
    // <- e, ee, s, es
    let msg = read_frame(&mut stream)?;
    hs.read_msg(&msg, &mut tmp)?;
    // -> s, se, psk
    hs.write_msg(&[], &mut out)?;
    write_frame(&mut stream, &out)?;
    let session = hs.into_transport()?;

    // HostInfo (CTRL nonce 0) を受信する。host が確立直後に切ると、ここで early eof になる。
    let ct = read_frame(&mut stream).context("read HostInfo (early eof here = host closed)")?;
    let mut plain = Vec::new();
    session.decrypt(NONCE_DOMAIN_CTRL, &ct, &mut plain)?;
    let info: ControlMsg = bincode::DefaultOptions::new()
        .with_limit(64 * 1024)
        .deserialize(&plain)
        .context("decode HostInfo")?;
    if !matches!(info, ControlMsg::HostInfo { .. }) {
        return Err(anyhow!("expected HostInfo, got {info:?}"));
    }

    // UDP "AURHELLO" を送る (実クライアントと同じ。host が UDP peer を登録し映像を流し始める)。
    let udp = UdpSocket::bind("0.0.0.0:0").context("udp bind")?;
    udp.connect(ADDR).context("udp connect")?;
    udp.send(b"AURHELLO").context("udp hello")?;

    // hold の間 TCP を保持して映像セッションが続く状況を模す。受信は読み捨て。
    let deadline = Instant::now() + hold;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    while Instant::now() < deadline {
        match read_frame(&mut stream) {
            Ok(_) => {}                                             // 制御フレーム読み捨て
            Err(e) if is_timeout(&e) => {}                          // hold 継続
            Err(e) => return Err(e).context("hold phase TCP read"), // 切断/エラー
        }
    }
    // drop(stream)/drop(udp) で切断。
    Ok(())
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .map(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            })
            .unwrap_or(false)
    })
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let password = args
        .next()
        .ok_or_else(|| anyhow!("usage: reconnect_probe <password> [cycles] [hold_ms] [gap_ms]"))?;
    let cycles: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let hold_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let gap_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);

    let psk = psk_from_password(&password);
    // 全サイクルで同じ永続 ID 鍵を使う (実クライアントの identity.bin 相当)。
    let id = generate_static_keypair()?;
    println!(
        "probe: cycles={cycles} hold={hold_ms}ms gap={gap_ms}ms pub_key_prefix={:02x}{:02x}{:02x}{:02x}",
        id.public[0], id.public[1], id.public[2], id.public[3]
    );

    let mut ok = 0usize;
    let mut fail = 0usize;
    for i in 1..=cycles {
        let t = Instant::now();
        match one_session(&psk, &id, Duration::from_millis(hold_ms)) {
            Ok(()) => {
                ok += 1;
                println!("cycle {i:>3}: OK    ({} ms)", t.elapsed().as_millis());
            }
            Err(e) => {
                fail += 1;
                println!(
                    "cycle {i:>3}: FAIL  ({} ms)  {e:#}",
                    t.elapsed().as_millis()
                );
            }
        }
        std::thread::sleep(Duration::from_millis(gap_ms));
    }
    println!("--- done: {ok} ok, {fail} fail ---");
    Ok(())
}
