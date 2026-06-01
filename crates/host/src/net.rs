//! TCP 制御 + UDP 映像のサーバ。単一クライアントモデル。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bincode::Options;
use ddd_common::framing::{
    AudioPacket, MAX_PAYLOAD, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
};
use ddd_common::noise::{HandshakeState, TransportSession};
use ddd_common::proto::{ControlMsg, Display, InputEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc};

use crate::audio::EncodedAudio;
use crate::encode::EncodedNal;

/// 制御ループ → encode スレッドへの通知。
pub enum EncoderCommand {
    RequestKeyframe,
}

pub struct ServerConfig {
    pub tcp_addr: SocketAddr,
    pub udp_addr: SocketAddr,
    pub psk: [u8; 32],
    pub display: Display,
    pub audio: bool,
}

pub async fn run_server(
    cfg: ServerConfig,
    mut nal_rx: mpsc::Receiver<EncodedNal>,
    mut audio_rx: mpsc::Receiver<EncodedAudio>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputEvent>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    mut clip_change_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    let tcp = TcpListener::bind(cfg.tcp_addr)
        .await
        .with_context(|| format!("bind TCP {}", cfg.tcp_addr))?;
    let udp = Arc::new(
        UdpSocket::bind(cfg.udp_addr)
            .await
            .with_context(|| format!("bind UDP {}", cfg.udp_addr))?,
    );
    tracing::info!(tcp = %cfg.tcp_addr, udp = %cfg.udp_addr, "host listening");

    loop {
        let (stream, peer) = tcp.accept().await?;
        tracing::info!(peer = %peer, "TCP client connected");
        let psk = cfg.psk;
        let display = cfg.display.clone();
        let audio = cfg.audio;
        let udp = udp.clone();
        let enc_tx2 = enc_tx.clone();
        let input_tx2 = input_tx.clone();
        let clip_apply_tx2 = clip_apply_tx.clone();
        // nal_rx / audio_rx / clip_change_rx は単一所有のため、新規セッション開始時は既存
        // セッションを置き換える必要がある。MVP は単一クライアントのみ受けて完了まで占有する。
        let nal_rx_ref = &mut nal_rx;
        let audio_rx_ref = &mut audio_rx;
        let clip_change_rx_ref = &mut clip_change_rx;
        if let Err(e) = handle_client(
            stream,
            peer,
            psk,
            display,
            audio,
            udp,
            enc_tx2,
            input_tx2,
            clip_apply_tx2,
            nal_rx_ref,
            audio_rx_ref,
            clip_change_rx_ref,
        )
        .await
        {
            tracing::warn!(error = %e, "client session ended");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    psk: [u8; 32],
    display: Display,
    audio: bool,
    udp: Arc<UdpSocket>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputEvent>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    nal_rx: &mut mpsc::Receiver<EncodedNal>,
    audio_rx: &mut mpsc::Receiver<EncodedAudio>,
    clip_change_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // Noise XX-PSK レスポンダ
    let mut hs = HandshakeState::new_responder(&psk)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();

    // <- e
    read_tcp_frame(&mut stream, &mut buf).await?;
    hs.read_msg(&buf, &mut tmp)?;
    // -> e, ee, s, es
    let mut out_msg = Vec::new();
    hs.write_msg(&[], &mut out_msg)?;
    write_tcp_frame(&mut stream, &out_msg).await?;
    // <- s, se, psk
    read_tcp_frame(&mut stream, &mut buf).await?;
    hs.read_msg(&buf, &mut tmp)?;

    let session = Arc::new(Mutex::new(hs.into_transport()?));
    tracing::info!(peer = %peer, "noise handshake complete");

    // HostInfo を即時送信
    let host_info = ControlMsg::HostInfo {
        displays: vec![display.clone()],
        audio,
    };
    send_control_msg(&mut stream, &session, &host_info).await?;

    // UDP 対向アドレス確定待ち。クライアントは最初に 8B の "DDDHELLO" を送る想定。
    let peer_udp = wait_udp_hello(&udp).await?;
    tracing::info!(peer_udp = %peer_udp, "UDP peer registered");

    let mut seq: u32 = 0;
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);

    loop {
        tokio::select! {
            // 制御メッセージ受信
            res = read_tcp_frame_into(&mut stream, &mut buf) => {
                res?;
                let mut plain = Vec::new();
                session.lock().await.decrypt(&buf, &mut plain)?;
                let msg: ControlMsg = bincode_opts.deserialize(&plain)
                    .context("decode ControlMsg")?;
                match msg {
                    ControlMsg::Ping { ts_us } => {
                        send_control_msg(&mut stream, &session, &ControlMsg::Pong { ts_us }).await?;
                    }
                    ControlMsg::RequestKeyframe => {
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::Input(ev) => {
                        if input_tx.send(ev).await.is_err() {
                            tracing::warn!("input channel closed");
                        }
                    }
                    ControlMsg::Clipboard { text } => {
                        if clip_apply_tx.send(text).is_err() {
                            tracing::warn!("clipboard apply channel closed");
                        }
                    }
                    other => {
                        tracing::debug!(?other, "received control msg (ignored in MVP)");
                    }
                }
            }
            // ローカルクリップボード変化 → 制御チャネルで送信
            Some(text) = clip_change_rx.recv() => {
                send_control_msg(&mut stream, &session, &ControlMsg::Clipboard { text }).await?;
            }
            // NAL 受信 → UDP 送信 (STREAM_VIDEO)
            Some(nal) = nal_rx.recv() => {
                let frame_id = nal.frame_id;
                let keyframe = nal.keyframe;
                let mut chunks = nal.data.chunks(MAX_PAYLOAD).peekable();
                let mut first = true;
                while let Some(chunk) = chunks.next() {
                    let is_last = chunks.peek().is_none();
                    let mut flags = VideoPacketFlags::empty();
                    if first { flags |= VideoPacketFlags::FRAME_START; }
                    if is_last { flags |= VideoPacketFlags::FRAME_END; }
                    if keyframe { flags |= VideoPacketFlags::KEYFRAME; }
                    let pkt = VideoPacket { seq, flags, frame_id, payload: chunk };
                    let mut framed = Vec::with_capacity(chunk.len() + 7);
                    pkt.encode(&mut framed).context("encode VideoPacket")?;
                    let mut ct = Vec::new();
                    session.lock().await.encrypt(&tagged(STREAM_VIDEO, &framed), &mut ct)?;
                    udp.send_to(&ct, peer_udp).await.context("UDP send")?;
                    seq = seq.wrapping_add(1);
                    first = false;
                }
            }
            // 音声フレーム受信 → UDP 送信 (STREAM_AUDIO)
            Some(au) = audio_rx.recv() => {
                let pkt = AudioPacket { seq: au.seq, payload: &au.data };
                let mut framed = Vec::with_capacity(au.data.len() + 4);
                pkt.encode(&mut framed).context("encode AudioPacket")?;
                let mut ct = Vec::new();
                session.lock().await.encrypt(&tagged(STREAM_AUDIO, &framed), &mut ct)?;
                udp.send_to(&ct, peer_udp).await.context("UDP send audio")?;
            }
        }
    }
}

/// ストリーム種別タグを先頭に付けた plaintext を作る。
fn tagged(stream: u8, framed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + framed.len());
    out.push(stream);
    out.extend_from_slice(framed);
    out
}

async fn wait_udp_hello(udp: &UdpSocket) -> Result<SocketAddr> {
    let mut buf = [0u8; 64];
    loop {
        let (n, src) = udp.recv_from(&mut buf).await?;
        if &buf[..n] == b"DDDHELLO" {
            return Ok(src);
        }
        tracing::debug!(?src, n, "unexpected UDP probe");
    }
}

async fn send_control_msg(
    stream: &mut TcpStream,
    session: &Arc<Mutex<TransportSession>>,
    msg: &ControlMsg,
) -> Result<()> {
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let plain = bincode_opts.serialize(msg).context("encode ControlMsg")?;
    let mut cipher = Vec::new();
    session.lock().await.encrypt(&plain, &mut cipher)?;
    write_tcp_frame(stream, &cipher).await?;
    Ok(())
}

async fn read_tcp_frame_into(stream: &mut TcpStream, out: &mut Vec<u8>) -> Result<()> {
    read_tcp_frame(stream, out).await
}

async fn read_tcp_frame(stream: &mut TcpStream, out: &mut Vec<u8>) -> Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await.context("read len")?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > 64 * 1024 {
        return Err(anyhow!("frame too large: {len}"));
    }
    out.resize(len, 0);
    stream.read_exact(out).await.context("read frame body")?;
    Ok(())
}

async fn write_tcp_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    if data.len() > u16::MAX as usize {
        return Err(anyhow!("frame too large: {}", data.len()));
    }
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await.context("write len")?;
    stream.write_all(data).await.context("write body")?;
    stream.flush().await.ok();
    Ok(())
}
