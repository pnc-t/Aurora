//! クライアント側ネットワーク。
//! - TCP 制御チャネル: Noise XX-PSK initiator、ControlMsg を bincode で送受信
//! - UDP 映像チャネル: 暗号化 VideoPacket を受信、フレーム再構成して Channel<Response> でフロントへ
//!
//! 単一セッションのみ保持し、`SessionState` (managed state) に置く。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bincode::Options;
use ddd_common::clipboard::{self, ClipboardHandle};
use ddd_common::framing::{
    AudioPacket, HEADER_LEN, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
};
use ddd_common::noise::{HandshakeState, TransportSession, psk_from_password};
use ddd_common::proto::{ControlMsg, Display, InputEvent};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::audio::{self, AudioPlayer};
use crate::stats::{StatsCollector, StatsWindow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket, tcp::OwnedReadHalf, tcp::OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const MAX_TCP_FRAME: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// フロント (TS) に流す制御イベント。タグ付き enum。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientEvent {
    Connected,
    HostInfo {
        displays: Vec<DisplayDto>,
        audio: bool,
    },
    Pong {
        rtt_us: i64,
    },
    Stats {
        bitrate_kbps: u32,
        fps: u8,
        rtt_us: u32,
        loss_permille: u16,
    },
    FileResult {
        id: u32,
        ok: bool,
        message: String,
    },
    Disconnected {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayDto {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
}

impl From<Display> for DisplayDto {
    fn from(d: Display) -> Self {
        Self {
            id: d.id,
            width: d.width,
            height: d.height,
            name: d.name,
        }
    }
}

/// セッションごとに保持するハンドル。
pub struct SessionHandle {
    /// 制御メッセージを書き込み側タスクに投げる
    pub ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
    /// ファイル転送チャンク専用の bounded チャネル。満杯時に送信が待つことで
    /// JS 側の読み込みにバックプレッシャがかかり、入力など他の制御を詰まらせない。
    pub file_tx: mpsc::Sender<ControlMsg>,
    /// 読み書き両タスクをまとめて止める
    pub tasks: Vec<JoinHandle<()>>,
    /// クリップボード同期ハンドル
    pub clipboard: ClipboardHandle,
    /// 音声再生 (ホストが音声非対応なら None)。保持目的は RAII: SessionHandle の drop で
    /// 再生スレッドへ EOF が伝わり停止する。直接読み出すことはない。
    #[allow(dead_code)]
    pub audio: Option<AudioPlayer>,
}

impl SessionHandle {
    /// セッションを停止する: クリップボードスレッドへ shutdown 通知 + tokio タスク abort。
    /// `audio` は self の drop で再生スレッドへ EOF が伝わり停止する。
    pub fn shutdown(mut self) {
        self.clipboard.shutdown();
        for t in std::mem::take(&mut self.tasks) {
            t.abort();
        }
    }
}

#[derive(Default)]
pub struct SessionState {
    pub inner: Mutex<Option<SessionHandle>>,
}

#[tauri::command]
pub async fn connect_to_host(
    host: String,
    password: String,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    // 既存セッションがあれば閉じる
    {
        let mut guard = state.inner.lock().await;
        if let Some(s) = guard.take() {
            s.shutdown();
        }
    }

    let tcp_addr: SocketAddr = format!("{host}:7000")
        .parse()
        .map_err(|e| format!("invalid host: {e}"))?;
    let udp_addr = tcp_addr;

    tracing::info!(%tcp_addr, "connecting");
    let mut tcp = TcpStream::connect(tcp_addr)
        .await
        .map_err(|e| format!("TCP connect: {e}"))?;
    tcp.set_nodelay(true).ok();

    let psk = psk_from_password(&password);

    // --- Noise XX-PSK initiator ---
    let mut hs = HandshakeState::new_initiator(&psk).map_err(err)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();
    // -> e
    let mut out_msg = Vec::new();
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    write_tcp_frame(&mut tcp, &out_msg).await.map_err(err)?;
    // <- e, ee, s, es
    read_tcp_frame(&mut tcp, &mut buf).await.map_err(err)?;
    hs.read_msg(&buf, &mut tmp).map_err(err)?;
    // -> s, se, psk
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    write_tcp_frame(&mut tcp, &out_msg).await.map_err(err)?;

    let session = Arc::new(Mutex::new(hs.into_transport().map_err(err)?));
    tracing::info!("noise handshake complete");

    // 最初の制御メッセージ (HostInfo) を受け取る
    read_tcp_frame(&mut tcp, &mut buf).await.map_err(err)?;
    let mut plain = Vec::new();
    session
        .lock()
        .await
        .decrypt(&buf, &mut plain)
        .map_err(err)?;
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    let host_info: ControlMsg = opts.deserialize(&plain).map_err(err)?;

    event_channel.send(ClientEvent::Connected).ok();
    if let ControlMsg::HostInfo { displays, audio } = host_info {
        event_channel
            .send(ClientEvent::HostInfo {
                displays: displays.into_iter().map(Into::into).collect(),
                audio,
            })
            .ok();
    } else {
        return Err(format!("expected HostInfo, got {host_info:?}"));
    }

    // UDP ソケットをローカル任意ポートで開き、サーバへ向けて "DDDHELLO"
    let udp = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("UDP bind: {e}"))?,
    );
    udp.connect(udp_addr)
        .await
        .map_err(|e| format!("UDP connect: {e}"))?;
    udp.send(b"DDDHELLO")
        .await
        .map_err(|e| format!("UDP hello: {e}"))?;

    let (read_half, write_half) = tcp.into_split();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let (file_tx, file_rx) = mpsc::channel::<ControlMsg>(32);

    // クリップボード同期 (このセッション専用に起動、disconnect で shutdown)
    let (clip_handle, mut clip_change_rx) = clipboard::spawn();
    let clip_apply_tx = clip_handle.apply_tx.clone();

    // 受信品質メトリクス。Ping の基準時刻と共有して RTT を算出する。
    let stats = StatsCollector::new();
    let session_start = Instant::now();

    let evt_clone = event_channel.clone();
    let sess_w = session.clone();
    let writer_task = tokio::spawn(async move {
        let _ = tcp_writer_loop(write_half, sess_w, ctrl_rx, file_rx).await;
        let _ = evt_clone.send(ClientEvent::Disconnected {
            reason: "writer closed".into(),
        });
    });

    let evt_clone = event_channel.clone();
    let sess_r = session.clone();
    let clip_apply_for_reader = clip_apply_tx.clone();
    let stats_for_reader = stats.clone();
    let reader_task = tokio::spawn(async move {
        let res = tcp_reader_loop(
            read_half,
            sess_r,
            evt_clone.clone(),
            clip_apply_for_reader,
            stats_for_reader,
            session_start,
        )
        .await;
        let reason = match res {
            Ok(()) => "server closed".to_string(),
            Err(e) => format!("read error: {e}"),
        };
        let _ = evt_clone.send(ClientEvent::Disconnected { reason });
    });

    // 音声再生 (ベストエフォート)。失敗しても映像のみで継続する。
    let audio_player = match audio::spawn() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "audio playback disabled");
            None
        }
    };
    let audio_tx = audio_player.as_ref().map(|p| p.sender());

    let sess_u = session.clone();
    let stats_for_udp = stats.clone();
    let udp_task = tokio::spawn(async move {
        if let Err(e) = udp_reader_loop(udp, sess_u, video_channel, audio_tx, stats_for_udp).await {
            tracing::warn!(error = %e, "UDP reader ended");
        }
    });

    // クリップボード変化 → ControlMsg::Clipboard 送信
    let ctrl_for_clip = ctrl_tx.clone();
    let clip_forward_task = tokio::spawn(async move {
        while let Some(text) = clip_change_rx.recv().await {
            if ctrl_for_clip.send(ControlMsg::Clipboard { text }).is_err() {
                break;
            }
        }
    });

    // ping ループ (1 秒)。RTT を細かく取り、適応制御の応答性を上げる。
    let ctrl_for_ping = ctrl_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let ts_us = session_start.elapsed().as_micros() as u64;
            if ctrl_for_ping.send(ControlMsg::Ping { ts_us }).is_err() {
                break;
            }
        }
    });

    // 統計タスク (1 秒窓)。計測結果をホスト (適応制御用) とフロント (表示用) に送る。
    let ctrl_for_stats = ctrl_tx.clone();
    let evt_for_stats = event_channel.clone();
    let stats_for_task = stats.clone();
    let stats_task = tokio::spawn(async move {
        let mut window = StatsWindow::new(&stats_for_task);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let s = window.sample(&stats_for_task);
            let _ = ctrl_for_stats.send(ControlMsg::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
            let _ = evt_for_stats.send(ClientEvent::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
        }
    });

    let handle = SessionHandle {
        ctrl_tx,
        file_tx,
        tasks: vec![
            writer_task,
            reader_task,
            udp_task,
            ping_task,
            stats_task,
            clip_forward_task,
        ],
        clipboard: clip_handle,
        audio: audio_player,
    };
    *state.inner.lock().await = Some(handle);
    Ok(())
}

#[tauri::command]
pub async fn disconnect(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    let mut guard = state.inner.lock().await;
    if let Some(s) = guard.take() {
        for t in s.tasks {
            t.abort();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn send_input(
    event: InputEvent,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.as_ref().ok_or("not connected")?;
    s.ctrl_tx
        .send(ControlMsg::Input(event))
        .map_err(|_| "ctrl channel closed".to_string())
}

#[tauri::command]
pub async fn request_keyframe(state: tauri::State<'_, SessionState>) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.as_ref().ok_or("not connected")?;
    s.ctrl_tx
        .send(ControlMsg::RequestKeyframe)
        .map_err(|_| "ctrl channel closed".to_string())
}

/// キャプチャ対象ディスプレイの切替を要求する。
#[tauri::command]
pub async fn select_display(id: u32, state: tauri::State<'_, SessionState>) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.as_ref().ok_or("not connected")?;
    s.ctrl_tx
        .send(ControlMsg::SelectDisplay { id })
        .map_err(|_| "ctrl channel closed".to_string())
}

/// ファイル転送開始の告知。チャンク列の前に 1 回だけ呼ぶ。
#[tauri::command]
pub async fn send_file_offer(
    id: u32,
    name: String,
    size: u64,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let msg = ControlMsg::FileOffer { id, name, size };
    send_file_msg(&state, msg).await
}

/// ファイルチャンクを 1 つ送る。bounded チャネル経由なので満杯時はここで待つ
/// (= JS 側 invoke が待ち、読み込みにバックプレッシャがかかる)。
#[tauri::command]
pub async fn send_file_chunk(
    id: u32,
    offset: u64,
    data: Vec<u8>,
    last: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let msg = ControlMsg::FileChunk {
        id,
        offset,
        data,
        last,
    };
    send_file_msg(&state, msg).await
}

/// bounded file チャネルへ送る共通処理。ロックは send の await をまたがないよう
/// Sender を clone してから解放する。
async fn send_file_msg(
    state: &tauri::State<'_, SessionState>,
    msg: ControlMsg,
) -> Result<(), String> {
    let file_tx = {
        let guard = state.inner.lock().await;
        let s = guard.as_ref().ok_or("not connected")?;
        s.file_tx.clone()
    };
    file_tx
        .send(msg)
        .await
        .map_err(|_| "file channel closed".to_string())
}

// -------- internal loops --------

async fn tcp_writer_loop(
    mut write_half: OwnedWriteHalf,
    session: Arc<Mutex<TransportSession>>,
    mut ctrl_rx: mpsc::UnboundedReceiver<ControlMsg>,
    mut file_rx: mpsc::Receiver<ControlMsg>,
) -> anyhow::Result<()> {
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    loop {
        // 制御チャネルとファイルチャネルの両方を待つ。どちらかが閉じたら終了。
        let msg = tokio::select! {
            m = ctrl_rx.recv() => m,
            m = file_rx.recv() => m,
        };
        let Some(msg) = msg else { break };
        let plain = opts.serialize(&msg)?;
        let mut cipher = Vec::new();
        session.lock().await.encrypt(&plain, &mut cipher)?;
        write_tcp_frame(&mut write_half, &cipher).await?;
    }
    Ok(())
}

async fn tcp_reader_loop(
    mut read_half: OwnedReadHalf,
    session: Arc<Mutex<TransportSession>>,
    event_channel: Channel<ClientEvent>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    stats: Arc<StatsCollector>,
    session_start: Instant,
) -> anyhow::Result<()> {
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    let mut buf = Vec::new();
    let mut plain = Vec::new();
    loop {
        read_tcp_frame(&mut read_half, &mut buf).await?;
        plain.clear();
        session.lock().await.decrypt(&buf, &mut plain)?;
        let msg: ControlMsg = opts.deserialize(&plain)?;
        match msg {
            ControlMsg::Pong { ts_us } => {
                // ts_us は Ping 送信時の session_start からの経過。往復差で RTT を得る。
                let now_us = session_start.elapsed().as_micros() as u64;
                let rtt_us = now_us.saturating_sub(ts_us);
                stats.set_rtt_us(rtt_us.min(u32::MAX as u64) as u32);
                let _ = event_channel.send(ClientEvent::Pong {
                    rtt_us: rtt_us as i64,
                });
            }
            ControlMsg::HostInfo { displays, audio } => {
                let _ = event_channel.send(ClientEvent::HostInfo {
                    displays: displays.into_iter().map(Into::into).collect(),
                    audio,
                });
            }
            ControlMsg::Clipboard { text } => {
                if clip_apply_tx.send(text).is_err() {
                    tracing::debug!("clipboard apply channel closed");
                }
            }
            ControlMsg::FileResult { id, ok, message } => {
                let _ = event_channel.send(ClientEvent::FileResult { id, ok, message });
            }
            other => tracing::debug!(?other, "ignored control msg"),
        }
    }
}

/// 受信したパケットからフレームを再構成し、Channel<InvokeResponseBody> でフロントへ。
/// フロント宛バイナリの先頭に小さなヘッダを付ける:
///   [0]   flags (bit0=keyframe)
///   [1..3] frame_id (be u16)
///   [3..]  Annex-B H.264 NALU
async fn udp_reader_loop(
    udp: Arc<UdpSocket>,
    session: Arc<Mutex<TransportSession>>,
    video_channel: Channel<InvokeResponseBody>,
    audio_tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    stats: Arc<StatsCollector>,
) -> anyhow::Result<()> {
    let mut recv_buf = vec![0u8; 2048];
    let mut plain = Vec::new();

    // 映像フレーム組立状態 (STREAM_VIDEO のみで使う)
    let mut cur_frame_id: Option<u16> = None;
    let mut cur_keyframe = false;
    let mut cur_payload: Vec<u8> = Vec::new();
    let mut last_seq: Option<u32> = None;
    let mut dropping = false;

    loop {
        let n = udp.recv(&mut recv_buf).await?;
        // 受信スループット (映像 + 音声、暗号文サイズ) を計測。
        stats.add_bytes(n as u64);
        plain.clear();
        if session
            .lock()
            .await
            .decrypt(&recv_buf[..n], &mut plain)
            .is_err()
        {
            // 復号失敗パケットは捨てる
            continue;
        }

        // 先頭 1B のストリームタグで映像 / 音声を振り分ける
        let (&tag, body) = match plain.split_first() {
            Some(x) => x,
            None => continue,
        };

        if tag == STREAM_AUDIO {
            if let Some(tx) = &audio_tx {
                if let Ok(pkt) = AudioPacket::decode(body) {
                    // 再生スレッドへ Opus フレームを渡す (seq は MVP では未使用)
                    let _ = tx.send(pkt.payload.to_vec());
                }
            }
            continue;
        }
        if tag != STREAM_VIDEO {
            continue;
        }

        let pkt = match VideoPacket::decode(body) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // 損失率算出のため受信した映像パケットを計上。
        stats.add_video_pkt();

        // seq 連続性チェック
        if let Some(prev) = last_seq {
            if pkt.seq != prev.wrapping_add(1) {
                // ロス検出: 欠落数を推定して計上し、現フレームを破棄して次の開始まで dropping
                let missing = pkt.seq.wrapping_sub(prev).wrapping_sub(1);
                stats.add_lost(missing as u64);
                cur_payload.clear();
                cur_frame_id = None;
                dropping = true;
            }
        }
        last_seq = Some(pkt.seq);

        if pkt.flags.contains(VideoPacketFlags::FRAME_START) {
            cur_payload.clear();
            cur_frame_id = Some(pkt.frame_id);
            cur_keyframe = pkt.flags.contains(VideoPacketFlags::KEYFRAME);
            dropping = false;
        }

        if dropping || cur_frame_id != Some(pkt.frame_id) {
            // フレーム境界が合わないので破棄
            continue;
        }

        if cur_payload.len() + pkt.payload.len() > MAX_FRAME_BYTES {
            tracing::warn!("frame too large, dropping");
            cur_payload.clear();
            cur_frame_id = None;
            dropping = true;
            continue;
        }
        cur_payload.extend_from_slice(pkt.payload);
        let _ = HEADER_LEN; // (一応参照)

        if pkt.flags.contains(VideoPacketFlags::FRAME_END) {
            let mut out = Vec::with_capacity(3 + cur_payload.len());
            out.push(if cur_keyframe { 1u8 } else { 0u8 });
            out.extend_from_slice(&pkt.frame_id.to_be_bytes());
            out.extend_from_slice(&cur_payload);
            // バイナリで送信
            let _ = video_channel.send(InvokeResponseBody::Raw(out));
            stats.add_frame();
            cur_payload.clear();
            cur_frame_id = None;
        }
    }
}

// -------- TCP framing util (host net.rs と同形) --------

async fn read_tcp_frame<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > MAX_TCP_FRAME {
        anyhow::bail!("frame too large: {len}");
    }
    out.resize(len, 0);
    stream.read_exact(out).await?;
    Ok(())
}

async fn write_tcp_frame<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    data: &[u8],
) -> anyhow::Result<()> {
    if data.len() > u16::MAX as usize {
        anyhow::bail!("frame too large: {}", data.len());
    }
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    stream.flush().await.ok();
    Ok(())
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}
