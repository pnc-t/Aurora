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
use crate::capture::DisplayInfo;
use crate::encode::EncodedNal;
use crate::input::DisplayGeom;
use crate::projfs::{self, FsBridge, FsReply};

use crate::encode::{DEFAULT_BITRATE_BPS, MAX_BITRATE_BPS, MIN_BITRATE_BPS};

/// 制御ループ → encode スレッドへの通知。
pub enum EncoderCommand {
    RequestKeyframe,
    /// 適応制御による平均ビットレート変更 (bps)。
    SetBitrate(u32),
}

/// 制御ループ → input スレッドへの通知。入力イベントとディスプレイ切替を 1 本にまとめる。
pub enum InputCmd {
    Event(InputEvent),
    /// キャプチャ対象ディスプレイ切替に伴う座標系更新。
    SetDisplay(DisplayGeom),
}

/// クライアント Stats を元にしたビットレート適応制御 (AIMD)。
///
/// - 損失率が高い → 乗算的に下げる (Multiplicative Decrease)
/// - 損失が少なく RTT も良好 → 加算的に上げる (Additive Increase)
///
/// 制御は `MIN_BITRATE_BPS`..=`MAX_BITRATE_BPS` にクランプする。
struct BitrateController {
    current_bps: u32,
}

impl BitrateController {
    fn new() -> Self {
        Self {
            current_bps: DEFAULT_BITRATE_BPS,
        }
    }

    /// 新しい Stats を反映し、ビットレートを変更すべきなら新しい値 (bps) を返す。
    fn on_stats(&mut self, loss_permille: u16, rtt_us: u32) -> Option<u32> {
        const LOSS_HIGH: u16 = 50; // 5% 超で輻輳とみなす
        const LOSS_LOW: u16 = 10; // 1% 未満なら余裕あり
        const RTT_OK_US: u32 = 80_000; // 80ms 未満なら増速を許可
        const STEP_UP_BPS: u32 = 1_000_000;

        let next = if loss_permille > LOSS_HIGH {
            (self.current_bps as f32 * 0.7) as u32
        } else if loss_permille < LOSS_LOW && rtt_us < RTT_OK_US {
            self.current_bps.saturating_add(STEP_UP_BPS)
        } else {
            self.current_bps
        }
        .clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS);

        if next != self.current_bps {
            self.current_bps = next;
            Some(next)
        } else {
            None
        }
    }
}

/// 受信中のファイルアップロード状態 (クライアント → ホスト)。1 セッション 1 転送のみ。
struct UploadState {
    id: u32,
    file: tokio::fs::File,
    written: u64,
    total: u64,
    path: String,
}

/// `ddd-received/` 配下に保存先ファイルを作る。ファイル名はパス区切りを除去して安全化する。
async fn open_upload(name: &str) -> Result<(tokio::fs::File, String)> {
    use std::path::Path;
    let dir = Path::new("ddd-received");
    tokio::fs::create_dir_all(dir)
        .await
        .context("create ddd-received dir")?;
    // パス区切りを含む名前を弾き、ファイル名成分のみを採用する
    let safe = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload.bin");
    let path = dir.join(safe);
    let file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    Ok((file, path.display().to_string()))
}

/// 要求されたファイルを開き、FileOffer + FileChunk 列を `out_tx` 経由でクライアントへ送る。
/// 読み取りエラーは FileResult(ok=false) で通知する。成功完了は last=true のチャンクで示す
/// (受信側がローカル保存の成否を判断するため、成功時の FileResult はホストからは送らない)。
async fn stream_file_to_client(id: u32, path: &str, out_tx: mpsc::Sender<ControlMsg>) {
    const CHUNK: usize = 32 * 1024;

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            let _ = out_tx
                .send(ControlMsg::FileResult {
                    id,
                    ok: false,
                    message: format!("open failed: {e}"),
                })
                .await;
            return;
        }
    };
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin")
        .to_string();

    if out_tx
        .send(ControlMsg::FileOffer { id, name, size })
        .await
        .is_err()
    {
        return;
    }

    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                let _ = out_tx
                    .send(ControlMsg::FileResult {
                        id,
                        ok: false,
                        message: format!("read failed: {e}"),
                    })
                    .await;
                return;
            }
        };
        let last = n == 0 || offset + n as u64 >= size;
        if out_tx
            .send(ControlMsg::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
                last,
            })
            .await
            .is_err()
        {
            return;
        }
        offset += n as u64;
        if last {
            break;
        }
    }
    tracing::info!(id, size, "file download streamed");
}

pub struct ServerConfig {
    pub tcp_addr: SocketAddr,
    pub udp_addr: SocketAddr,
    pub psk: [u8; 32],
    pub displays: Vec<DisplayInfo>,
    pub audio: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    cfg: ServerConfig,
    mut nal_rx: mpsc::Receiver<EncodedNal>,
    mut audio_rx: mpsc::Receiver<EncodedAudio>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<u32>,
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
        let displays = cfg.displays.clone();
        let audio = cfg.audio;
        let udp = udp.clone();
        let enc_tx2 = enc_tx.clone();
        let input_tx2 = input_tx.clone();
        let cap_select_tx2 = cap_select_tx.clone();
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
            displays,
            audio,
            udp,
            enc_tx2,
            input_tx2,
            cap_select_tx2,
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
    displays: Vec<DisplayInfo>,
    audio: bool,
    udp: Arc<UdpSocket>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<u32>,
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

    // HostInfo を即時送信 (全ディスプレイを提示)
    let host_info = ControlMsg::HostInfo {
        displays: displays.iter().map(Display::from).collect(),
        audio,
    };
    send_control_msg(&mut stream, &session, &host_info).await?;

    // UDP 対向アドレス確定待ち。クライアントは最初に 8B の "DDDHELLO" を送る想定。
    let peer_udp = wait_udp_hello(&udp).await?;
    tracing::info!(peer_udp = %peer_udp, "UDP peer registered");

    let mut seq: u32 = 0;
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let mut upload: Option<UploadState> = None;
    let mut bitrate = BitrateController::new();
    // ダウンロード (ホスト → クライアント) のチャンクを select ループへ流す出口。
    // 送信タスクがここに積み、ループが TCP へ書く (stream への書き込みは単一箇所に集約)。
    let (out_tx, mut out_rx) = mpsc::channel::<ControlMsg>(64);

    // ProjFS ドライブリダイレクト。コールバック (同期) → ここへ FsList/FsRead 要求が流れる。
    // 開始失敗 (Client-ProjFS 無効など) はファイル投影を諦めてリモデス本体は継続する。
    let (fs_req_tx, mut fs_req_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let fs_bridge = Arc::new(FsBridge::new(fs_req_tx));
    let _virt = match projfs::start(fs_bridge.clone(), std::path::Path::new("ddd-remote")) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "ProjFS drive redirection disabled");
            None
        }
    };

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
                    ControlMsg::Stats { bitrate_kbps, fps, rtt_us, loss_permille } => {
                        tracing::debug!(bitrate_kbps, fps, rtt_us, loss_permille, "client stats");
                        if let Some(new_bps) = bitrate.on_stats(loss_permille, rtt_us) {
                            tracing::info!(new_bps, loss_permille, rtt_us, "adapt bitrate");
                            let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                        }
                    }
                    ControlMsg::Input(ev) => {
                        if input_tx.send(InputCmd::Event(ev)).await.is_err() {
                            tracing::warn!("input channel closed");
                        }
                    }
                    ControlMsg::SelectDisplay { id } => {
                        match displays.iter().find(|d| d.id == id) {
                            Some(d) => {
                                tracing::info!(id, name = %d.name, "select display");
                                // capture スレッドへ切替指示。
                                let _ = cap_select_tx.send(id);
                                // input スレッドの座標系を新ディスプレイへ更新。
                                let geom = DisplayGeom {
                                    x: d.x,
                                    y: d.y,
                                    width: d.width,
                                    height: d.height,
                                };
                                let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
                                // 新キャプチャの最初の出力を keyframe にしてクライアント復帰を促す。
                                let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                            }
                            None => tracing::warn!(id, "SelectDisplay for unknown display id"),
                        }
                    }
                    ControlMsg::Clipboard { text } => {
                        if clip_apply_tx.send(text).is_err() {
                            tracing::warn!("clipboard apply channel closed");
                        }
                    }
                    ControlMsg::FileOffer { id, name, size } => {
                        match open_upload(&name).await {
                            Ok((file, path)) => {
                                tracing::info!(id, name = %name, size, path = %path, "file upload start");
                                upload = Some(UploadState { id, file, written: 0, total: size, path });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "open upload failed");
                                let msg = ControlMsg::FileResult { id, ok: false, message: e.to_string() };
                                send_control_msg(&mut stream, &session, &msg).await?;
                            }
                        }
                    }
                    ControlMsg::FileChunk { id, offset, data, last } => {
                        match upload.as_mut() {
                            Some(u) if u.id == id => {
                                if offset != u.written {
                                    tracing::warn!(id, expected = u.written, got = offset, "chunk offset mismatch");
                                }
                                if let Err(e) = u.file.write_all(&data).await {
                                    let message = format!("write failed: {e}");
                                    tracing::warn!(id, error = %e, "file write failed");
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_control_msg(&mut stream, &session, &msg).await?;
                                } else {
                                    u.written += data.len() as u64;
                                    if last {
                                        let (written, total, path) = (u.written, u.total, u.path.clone());
                                        let res = u.file.flush().await;
                                        upload = None;
                                        let msg = match res {
                                            Ok(()) => {
                                                tracing::info!(id, written, total, path = %path, "file upload complete");
                                                ControlMsg::FileResult { id, ok: true, message: path }
                                            }
                                            Err(e) => ControlMsg::FileResult { id, ok: false, message: format!("flush failed: {e}") },
                                        };
                                        send_control_msg(&mut stream, &session, &msg).await?;
                                    }
                                }
                            }
                            _ => {
                                tracing::warn!(id, "file chunk without matching offer, ignored");
                            }
                        }
                    }
                    ControlMsg::FileRequest { id, path } => {
                        tracing::info!(id, path = %path, "file download requested");
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            stream_file_to_client(id, &path, out_tx).await;
                        });
                    }
                    // ProjFS コールバックへの応答。相関 ID で待機中のコールバックを起こす。
                    ControlMsg::FsListReply { req_id, entries, done, error } => {
                        fs_bridge.complete(req_id, FsReply::List { entries, done, error });
                    }
                    ControlMsg::FsReadReply { req_id, data, error } => {
                        fs_bridge.complete(req_id, FsReply::Read { data, error });
                    }
                    other => {
                        tracing::debug!(?other, "received control msg (ignored in MVP)");
                    }
                }
            }
            // ダウンロード送信タスクからのメッセージ (FileOffer/FileChunk/FileResult)
            Some(msg) = out_rx.recv() => {
                send_control_msg(&mut stream, &session, &msg).await?;
            }
            // ProjFS コールバックからの FS RPC 要求 (FsList/FsRead) を TCP へ送る
            Some(msg) = fs_req_rx.recv() => {
                send_control_msg(&mut stream, &session, &msg).await?;
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
