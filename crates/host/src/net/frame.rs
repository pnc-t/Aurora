//! TCP 制御フレームと UDP メディアデータグラムのフレーミング/暗号化ヘルパ。
use super::*;

/// UDP メディアデータグラムを構築する。
///
/// plaintext は `[stream tag(1B)] + framed` を `nonce` で暗号化し、受信側が復号前に
/// nonce を知れるよう **暗号文の前に nonce を 8B (BE) 平文で付与**する。
/// 出力: `[nonce(8B BE)] + [ciphertext]`。
pub(crate) fn encrypt_udp(
    session: &TransportSession,
    nonce: u64,
    stream: u8,
    framed: &[u8],
) -> Result<Vec<u8>> {
    let mut plain = Vec::with_capacity(1 + framed.len());
    plain.push(stream);
    plain.extend_from_slice(framed);
    let mut dgram = Vec::with_capacity(8 + plain.len() + 16);
    dgram.extend_from_slice(&nonce.to_be_bytes());
    session.encrypt(nonce, &plain, &mut dgram)?;
    Ok(dgram)
}

/// クライアントの UDP "AURHELLO" を待ち、対向アドレスを確定する。
///
/// hello は単発 UDP のためロスしうる。クライアントは映像受信が始まるまで再送するので、
/// ここでは各受信に上限を設けるだけにし、長時間 hello が来なければ Err を返して
/// セッションを諦める (無限ブロックで accept ループまで止めない安全弁)。
///
/// `expected_ip` はハンドシェイクを完了した TCP ピアの IP。これと一致する送信元からの
/// hello のみ受理し、別 IP からの hello はストリーム乗っ取り/増幅の試みとして捨てる。
/// Tailnet / LAN / localhost いずれも client の TCP と UDP は同一インターフェースから
/// 出るため送信元 IP は一致する (ポートは異なってよい)。
pub(crate) async fn wait_udp_hello(
    udp: &UdpSocket,
    expected_ip: std::net::IpAddr,
) -> Result<SocketAddr> {
    use std::time::Duration;
    let mut buf = [0u8; 64];
    loop {
        let (n, src) = tokio::time::timeout(Duration::from_secs(30), udp.recv_from(&mut buf))
            .await
            .context("timed out waiting for UDP hello")??;
        if &buf[..n] == b"AURHELLO" {
            if src.ip() == expected_ip {
                return Ok(src);
            }
            // TCP ピア以外からの hello は乗っ取り/増幅の疑い。受理せず待機を続ける。
            tracing::warn!(?src, expected = %expected_ip, "rejected UDP hello from unexpected source");
            continue;
        }
        tracing::debug!(?src, n, "unexpected UDP probe");
    }
}

pub(crate) async fn send_control_msg<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    session: &TransportSession,
    msg: &ControlMsg,
    tcp_tx_nonce: &mut u64,
) -> Result<()> {
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let plain = bincode_opts.serialize(msg).context("encode ControlMsg")?;
    let nonce = NONCE_DOMAIN_CTRL | *tcp_tx_nonce;
    *tcp_tx_nonce += 1;
    let mut cipher = Vec::new();
    session.encrypt(nonce, &plain, &mut cipher)?;
    write_tcp_frame(stream, &cipher).await?;
    Ok(())
}

pub(crate) async fn read_tcp_frame<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await.context("read len")?;
    // 長さは u16 なので最大 65535。受信サイズはこれで自然に有界で、メッセージ本体の
    // 上限は復号後の bincode (with_limit 64KiB) が守る。
    let len = u16::from_be_bytes(len_buf) as usize;
    out.resize(len, 0);
    stream.read_exact(out).await.context("read frame body")?;
    Ok(())
}

pub(crate) async fn write_tcp_frame<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    data: &[u8],
) -> Result<()> {
    if data.len() > u16::MAX as usize {
        return Err(anyhow!("frame too large: {}", data.len()));
    }
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await.context("write len")?;
    stream.write_all(data).await.context("write body")?;
    stream.flush().await.ok();
    Ok(())
}
