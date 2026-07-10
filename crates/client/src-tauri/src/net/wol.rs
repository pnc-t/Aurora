//! WoL (Wake-on-LAN) 中継への wake 要求。
//!
//! aurora-wol デーモンと直接 Noise XX-PSK ハンドシェイクし、`WolMsg::Wake` を送って
//! `WolMsg::Ack` を待つだけの軽量接続。映像セッション ([`ControlMsg`] / `SessionState`) とは
//! 無関係の独立した接続で、成功/失敗を返したら即切断する。
use super::*;

use aurora_common::framing::{decode_wol_msg, send_wol_msg};
use aurora_common::proto::WolMsg;

/// WoL 中継 (aurora-wol) 用に保存するパスワードの secret キー。host の接続パスワード
/// (secret::get/put のキーは host そのもの) と名前空間を分けて衝突を避ける。
fn wol_secret_key(relay_host: &str, relay_port: u16) -> String {
    format!("wol:{relay_host}:{relay_port}")
}

/// MAC アドレス文字列 (`:` / `-` 区切りどちらも許容) をバイト列へ変換する。
fn parse_mac(mac: &str) -> Result<[u8; 6], String> {
    let parts: Vec<&str> = mac.split(['-', ':']).collect();
    if parts.len() != 6 {
        return Err(format!("invalid MAC address: {mac}"));
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).map_err(|_| format!("invalid MAC address: {mac}"))?;
    }
    Ok(out)
}

/// ハンドシェイク I/O を `CONNECT_TIMEOUT` で包む。無応答な aurora-wol への無限ハングを防ぐ
/// (connect.rs の `hs_io` と同じ理由だが、モジュール private のためここに複製する)。
async fn wol_io(fut: impl std::future::Future<Output = std::io::Result<()>>) -> Result<(), String> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(err)
}

/// WoL 中継デーモン (aurora-wol) へ Wake 要求を送る。
///
/// 映像セッションは開始しない: TCP connect → Noise XX-PSK ハンドシェイク →
/// `WolMsg::Wake` 送信 → `WolMsg::Ack` 待ち → (関数終了で) 切断。
/// `password` が `Some` ならそれを使い、`save_password=true` なら DPAPI へ保存する。
/// `None` なら保存済みパスワードを使う (無ければユーザー可読なエラーを返す)。
#[tauri::command]
pub async fn wake_host(
    app: tauri::AppHandle,
    relay_host: String,
    relay_port: u16,
    password: Option<String>,
    save_password: bool,
    mac: String,
) -> Result<(), String> {
    let mac_bytes = parse_mac(&mac)?;
    let secret_key = wol_secret_key(&relay_host, relay_port);
    let password = match password {
        Some(p) => p,
        None => crate::secret::get(&app, &secret_key)
            .ok_or_else(|| "保存済みパスワードがありません".to_string())?,
    };

    let target = format!("{relay_host}:{relay_port}");
    let tcp_addr: SocketAddr = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| format!("resolve {relay_host}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {relay_host}"))?;

    tracing::info!(%tcp_addr, "wol: connecting");
    let mut tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(tcp_addr))
        .await
        .map_err(|_| "TCP connect timed out".to_string())?
        .map_err(|e| format!("TCP connect: {e}"))?;
    tcp.set_nodelay(true).ok();

    let psk = psk_from_password(&password);

    // --- Noise XX-PSK initiator (host 接続と同じハンドシェイク形式、静的鍵は使い捨て) ---
    let mut hs = HandshakeState::new_initiator(&psk, None).map_err(err)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();
    let mut out_msg = Vec::new();
    // -> e
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    wol_io(write_tcp_frame(&mut tcp, &out_msg)).await?;
    // <- e, ee, s, es
    wol_io(read_tcp_frame(&mut tcp, &mut buf)).await?;
    hs.read_msg(&buf, &mut tmp).map_err(err)?;
    // -> s, se, psk
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    wol_io(write_tcp_frame(&mut tcp, &out_msg)).await?;
    let session = hs.into_transport().map_err(err)?;
    tracing::info!("wol: noise handshake complete");

    // ハンドシェイク成功 = パスワード一致。保存はユーザーが明示的に選んだときだけ。
    if save_password && let Err(e) = crate::secret::put(&app, &secret_key, &password) {
        tracing::warn!(error = %e, "failed to save wol relay password");
    }

    let mut tx_nonce: u64 = 0;
    send_wol_msg(
        &mut tcp,
        &session,
        &WolMsg::Wake { mac: mac_bytes },
        &mut tx_nonce,
    )
    .await
    .map_err(err)?;

    wol_io(read_tcp_frame(&mut tcp, &mut buf)).await?;
    let mut plain = Vec::new();
    let mut rx_nonce: u64 = 0;
    let msg = decode_wol_msg(&session, &buf, &mut rx_nonce, &mut plain).map_err(err)?;
    match msg {
        WolMsg::Ack { ok: true, .. } => {
            tracing::info!("wol: wake acked");
            Ok(())
        }
        WolMsg::Ack { ok: false, message } => Err(message),
        other => Err(format!("unexpected message: {other:?}")),
    }
}
