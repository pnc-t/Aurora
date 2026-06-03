//! クライアント側ネットワーク。
//! - TCP 制御チャネル: Noise XX-PSK initiator、ControlMsg を bincode で送受信
//! - UDP 映像チャネル: 暗号化 VideoPacket を受信、フレーム再構成して Channel<Response> でフロントへ
//!
//! 単一セッションのみ保持し、`SessionState` (managed state) に置く。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bincode::Options;
use ddd_common::clipboard::{self, ClipboardHandle};
use ddd_common::framing::{
    AudioPacket, HEADER_LEN, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
};
use ddd_common::noise::{HandshakeState, NONCE_DOMAIN_CTRL, TransportSession, psk_from_password};
use ddd_common::proto::{ControlMsg, Display, FsEntry, InputEvent};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::audio::{self, AudioPlayer};
use crate::stats::{StatsCollector, StatsWindow};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
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

/// ホストの ProjFS から見えるクライアント共有ルート。`set_share_root` で設定する。
/// 未設定時はホームディレクトリを既定とする。spawn したリーダタスクと共有するため Arc。
#[derive(Clone)]
pub struct ShareRoot(pub Arc<Mutex<PathBuf>>);

impl Default for ShareRoot {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        ShareRoot(Arc::new(Mutex::new(home)))
    }
}

/// ホストへ投影する共有フォルダを設定する。
#[tauri::command]
pub async fn set_share_root(
    path: String,
    state: tauri::State<'_, ShareRoot>,
) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    *state.0.lock().await = p;
    tracing::info!(path = %path, "share root set");
    Ok(())
}

#[tauri::command]
pub async fn connect_to_host(
    host: String,
    password: String,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
    share_root: tauri::State<'_, ShareRoot>,
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

    // 最初の制御メッセージ (HostInfo) を受け取る。CTRL ドメインの nonce 0 で復号し、
    // 以降の TCP 受信は reader ループが nonce 1 から続ける。
    read_tcp_frame(&mut tcp, &mut buf).await.map_err(err)?;
    let mut plain = Vec::new();
    session
        .lock()
        .await
        .decrypt(NONCE_DOMAIN_CTRL, &buf, &mut plain)
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

    // UDP ソケットをローカル任意ポートで開き、サーバへ向けて "DDDHELLO"。
    // 高 fps 時のバースト受信での取りこぼし (= 映像ノイズ) を抑えるため受信バッファを拡大する。
    let udp = Arc::new(make_udp_socket().map_err(|e| format!("UDP socket: {e}"))?);
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
    // FS RPC 応答は制御チャネル経由で writer ループへ流す (host の同期コールバックを起こす)。
    let fs_reply_tx = ctrl_tx.clone();
    let share_root_for_reader = share_root.0.clone();
    let reader_task = tokio::spawn(async move {
        let res = tcp_reader_loop(
            read_half,
            sess_r,
            evt_clone.clone(),
            clip_apply_for_reader,
            stats_for_reader,
            session_start,
            fs_reply_tx,
            share_root_for_reader,
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
    // ロス検出時にキーフレームを要求して映像を即時回復させるための送信口。
    let keyframe_tx = ctrl_tx.clone();
    let udp_task = tokio::spawn(async move {
        if let Err(e) = udp_reader_loop(
            udp,
            sess_u,
            video_channel,
            audio_tx,
            stats_for_udp,
            keyframe_tx,
        )
        .await
        {
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
        // task abort だけでなくクリップボードスレッドにも shutdown を伝える
        // (connect_to_host のセッション置換経路と同じ後始末に統一)。
        s.shutdown();
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

/// ホスト上のファイルのダウンロードを要求する。受信は reader ループが
/// `ddd-downloads/` に保存し、完了を FileResult イベントで通知する。
#[tauri::command]
pub async fn request_file(
    id: u32,
    path: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.as_ref().ok_or("not connected")?;
    s.ctrl_tx
        .send(ControlMsg::FileRequest { id, path })
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
    // クライアント → ホストの TCP 制御は CTRL ドメインの連番 nonce。ホスト側 rx と
    // TCP の順序保証により歩調が合う。
    let mut tcp_tx_nonce: u64 = 0;
    loop {
        // 制御チャネルとファイルチャネルの両方を待つ。どちらかが閉じたら終了。
        let msg = tokio::select! {
            m = ctrl_rx.recv() => m,
            m = file_rx.recv() => m,
        };
        let Some(msg) = msg else { break };
        let plain = opts.serialize(&msg)?;
        let nonce = NONCE_DOMAIN_CTRL | tcp_tx_nonce;
        tcp_tx_nonce += 1;
        let mut cipher = Vec::new();
        session.lock().await.encrypt(nonce, &plain, &mut cipher)?;
        write_tcp_frame(&mut write_half, &cipher).await?;
    }
    Ok(())
}

/// ダウンロード受信状態 (ホスト → クライアント)。1 セッション 1 転送のみ。
struct DownloadState {
    id: u32,
    file: tokio::fs::File,
    written: u64,
    path: String,
}

/// `ddd-downloads/` 配下に保存先ファイルを作る。ファイル名はパス区切りを除去して安全化する。
async fn open_download(name: &str) -> anyhow::Result<(tokio::fs::File, String)> {
    use std::path::Path;
    let dir = Path::new("ddd-downloads");
    tokio::fs::create_dir_all(dir).await?;
    let safe = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    let path = dir.join(safe);
    let file = tokio::fs::File::create(&path).await?;
    Ok((file, path.display().to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn tcp_reader_loop(
    mut read_half: OwnedReadHalf,
    session: Arc<Mutex<TransportSession>>,
    event_channel: Channel<ClientEvent>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    stats: Arc<StatsCollector>,
    session_start: Instant,
    fs_reply_tx: mpsc::UnboundedSender<ControlMsg>,
    share_root: Arc<Mutex<PathBuf>>,
) -> anyhow::Result<()> {
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    let mut buf = Vec::new();
    let mut plain = Vec::new();
    let mut download: Option<DownloadState> = None;
    // HostInfo (nonce 0) は connect_to_host で消費済みなので 1 から続ける。
    let mut tcp_rx_nonce: u64 = 1;
    loop {
        read_tcp_frame(&mut read_half, &mut buf).await?;
        plain.clear();
        let nonce = NONCE_DOMAIN_CTRL | tcp_rx_nonce;
        tcp_rx_nonce += 1;
        session.lock().await.decrypt(nonce, &buf, &mut plain)?;
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
            // --- ダウンロード受信 (ホスト → クライアント) ---
            ControlMsg::FileOffer { id, name, size } => match open_download(&name).await {
                Ok((file, path)) => {
                    tracing::info!(id, name = %name, size, path = %path, "download start");
                    download = Some(DownloadState {
                        id,
                        file,
                        written: 0,
                        path,
                    });
                }
                Err(e) => {
                    let _ = event_channel.send(ClientEvent::FileResult {
                        id,
                        ok: false,
                        message: format!("open failed: {e}"),
                    });
                }
            },
            ControlMsg::FileChunk {
                id,
                offset,
                data,
                last,
            } => match download.as_mut() {
                Some(d) if d.id == id => {
                    if offset != d.written {
                        tracing::warn!(
                            id,
                            expected = d.written,
                            got = offset,
                            "download offset mismatch"
                        );
                    }
                    if let Err(e) = d.file.write_all(&data).await {
                        let message = format!("write failed: {e}");
                        let _ = event_channel.send(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message,
                        });
                        download = None;
                    } else {
                        d.written += data.len() as u64;
                        if last {
                            let (written, path) = (d.written, d.path.clone());
                            let res = d.file.flush().await;
                            download = None;
                            let ev = match res {
                                Ok(()) => {
                                    tracing::info!(id, written, path = %path, "download complete");
                                    ClientEvent::FileResult {
                                        id,
                                        ok: true,
                                        message: path,
                                    }
                                }
                                Err(e) => ClientEvent::FileResult {
                                    id,
                                    ok: false,
                                    message: format!("flush failed: {e}"),
                                },
                            };
                            let _ = event_channel.send(ev);
                        }
                    }
                }
                _ => tracing::warn!(id, "download chunk without matching offer, ignored"),
            },
            // --- ProjFS ドライブリダイレクト: 自分のファイルをホストへ配信 ---
            ControlMsg::FsList {
                req_id,
                path,
                offset,
            } => {
                let root = share_root.lock().await.clone();
                let reply = fs_list(&root, &path, offset).await;
                let _ = fs_reply_tx.send(reply.into_msg(req_id));
            }
            ControlMsg::FsRead {
                req_id,
                path,
                offset,
                length,
            } => {
                let root = share_root.lock().await.clone();
                let (data, error) = fs_read(&root, &path, offset, length).await;
                let _ = fs_reply_tx.send(ControlMsg::FsReadReply {
                    req_id,
                    data,
                    error,
                });
            }
            // --- 書き戻し (ホスト Explorer → クライアント、一方向) ---
            ControlMsg::FsCreate { path, is_dir } => {
                let root = share_root.lock().await.clone();
                if let Err(e) = fs_create(&root, &path, is_dir).await {
                    tracing::warn!(path = %path, error = %e, "FsCreate failed");
                }
            }
            ControlMsg::FsWrite { path, offset, data } => {
                let root = share_root.lock().await.clone();
                if let Err(e) = fs_write(&root, &path, offset, &data).await {
                    tracing::warn!(path = %path, error = %e, "FsWrite failed");
                }
            }
            ControlMsg::FsDelete { path } => {
                let root = share_root.lock().await.clone();
                if let Err(e) = fs_delete(&root, &path).await {
                    tracing::warn!(path = %path, error = %e, "FsDelete failed");
                }
            }
            ControlMsg::FsRename { from, to } => {
                let root = share_root.lock().await.clone();
                if let Err(e) = fs_rename(&root, &from, &to).await {
                    tracing::warn!(from = %from, to = %to, error = %e, "FsRename failed");
                }
            }
            other => tracing::debug!(?other, "ignored control msg"),
        }
    }
}

/// 1 ページの一覧結果。
struct ListPage {
    entries: Vec<FsEntry>,
    done: bool,
    error: Option<String>,
}

impl ListPage {
    fn err(msg: String) -> Self {
        Self {
            entries: Vec::new(),
            done: true,
            error: Some(msg),
        }
    }
    fn into_msg(self, req_id: u64) -> ControlMsg {
        ControlMsg::FsListReply {
            req_id,
            entries: self.entries,
            done: self.done,
            error: self.error,
        }
    }
}

/// 制御フレーム 64KB 上限に収めるための 1 ページあたりエントリ数上限。
const LIST_PAGE: usize = 128;
/// FsRead の上限 (host も 32KB ずつ要求するが念のためクランプ)。
const MAX_READ: u32 = 64 * 1024;

/// 共有ルート相対パス (`/` 区切り) を実パスへ解決する。`..` によるルート外脱出を禁止。
async fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err("path traversal rejected".into());
        }
        p.push(comp);
    }
    let canon_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| format!("canonicalize root: {e}"))?;
    let canon = tokio::fs::canonicalize(&p)
        .await
        .map_err(|e| format!("canonicalize: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err("path escapes share root".into());
    }
    Ok(canon)
}

/// 共有ルート相対 `rel` のディレクトリを `offset` から最大 `LIST_PAGE` 件列挙する。
async fn fs_list(root: &Path, rel: &str, offset: u32) -> ListPage {
    let dir = match resolve_in_root(root, rel).await {
        Ok(p) => p,
        Err(e) => return ListPage::err(e),
    };
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return ListPage::err(format!("read_dir: {e}")),
    };
    let mut all: Vec<FsEntry> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(ent)) => {
                let name = ent.file_name().to_string_lossy().to_string();
                let (is_dir, size) = match ent.metadata().await {
                    Ok(m) => (m.is_dir(), m.len()),
                    Err(_) => continue, // メタ取得不能なエントリはスキップ
                };
                all.push(FsEntry { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => return ListPage::err(format!("read entry: {e}")),
        }
    }
    // ページ境界の安定のため名前で整列 (host 側でも ProjFS 規約で再整列する)。
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let total = all.len();
    let start = offset as usize;
    let entries: Vec<FsEntry> = all.into_iter().skip(start).take(LIST_PAGE).collect();
    let done = start + entries.len() >= total;
    ListPage {
        entries,
        done,
        error: None,
    }
}

/// 共有ルート相対 `rel` のファイルを `offset` から最大 `length` バイト読む。
/// 戻り (data, error)。空 data かつ error=None は EOF。
async fn fs_read(root: &Path, rel: &str, offset: u64, length: u32) -> (Vec<u8>, Option<String>) {
    let path = match resolve_in_root(root, rel).await {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return (Vec::new(), Some(format!("open: {e}"))),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        return (Vec::new(), Some(format!("seek: {e}")));
    }
    let want = length.min(MAX_READ) as usize;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    // 要求長を満たすか EOF まで読む (短い read を埋める)。
    while filled < want {
        match file.read(&mut buf[filled..]).await {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) => return (Vec::new(), Some(format!("read: {e}"))),
        }
    }
    buf.truncate(filled);
    (buf, None)
}

/// 書き込み系操作のパス解決。対象が未存在でも使えるよう canonicalize はせず、
/// `..` / 絶対成分を弾いて root 配下に閉じ込める (レキシカルなトラバーサル防止)。
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." || comp.contains('\\') || comp.contains(':') {
            return Err(format!("unsafe path component: {comp}"));
        }
        p.push(comp);
    }
    Ok(p)
}

/// ファイル / ディレクトリを作成する (ファイルは 0 バイトに truncate)。
async fn fs_create(root: &Path, rel: &str, is_dir: bool) -> Result<(), String> {
    let p = safe_join(root, rel)?;
    if is_dir {
        tokio::fs::create_dir_all(&p)
            .await
            .map_err(|e| format!("create_dir: {e}"))
    } else {
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::File::create(&p)
            .await
            .map(|_| ())
            .map_err(|e| format!("create_file: {e}"))
    }
}

/// ファイルの `offset` 以降にバイトを書き込む (なければ作成、truncate はしない)。
async fn fs_write(root: &Path, rel: &str, offset: u64, data: &[u8]) -> Result<(), String> {
    let p = safe_join(root, rel)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .await
        .map_err(|e| format!("open: {e}"))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("seek: {e}"))?;
    file.write_all(data)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// ファイル / ディレクトリを削除する。
async fn fs_delete(root: &Path, rel: &str) -> Result<(), String> {
    let p = safe_join(root, rel)?;
    match tokio::fs::metadata(&p).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(&p)
            .await
            .map_err(|e| format!("remove_dir: {e}")),
        Ok(_) => tokio::fs::remove_file(&p)
            .await
            .map_err(|e| format!("remove_file: {e}")),
        Err(_) => Ok(()), // 既に無いなら成功扱い
    }
}

/// `from` を `to` にリネーム / 移動する。
async fn fs_rename(root: &Path, from: &str, to: &str) -> Result<(), String> {
    let src = safe_join(root, from)?;
    let dst = safe_join(root, to)?;
    if let Some(parent) = dst.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::rename(&src, &dst)
        .await
        .map_err(|e| format!("rename: {e}"))
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
    keyframe_tx: mpsc::UnboundedSender<ControlMsg>,
) -> anyhow::Result<()> {
    let mut recv_buf = vec![0u8; 2048];
    let mut plain = Vec::new();

    // ホストの UDP hello は単発でロスしうる。最初のデータグラムを受信するまで
    // 250ms ごとに hello を再送し、ホストの peer_udp 登録を確実にする (登録前は
    // ホストが映像を送れず、こちらも何も受け取れないため再送がないと無音になる)。
    let mut established = false;
    let mut hello_interval = tokio::time::interval(Duration::from_millis(250));
    hello_interval.tick().await; // 初回の即時 tick を捨てる

    // 映像フレーム組立状態 (STREAM_VIDEO のみで使う)
    let mut cur_frame_id: Option<u16> = None;
    let mut cur_keyframe = false;
    let mut cur_payload: Vec<u8> = Vec::new();
    let mut last_seq: Option<u32> = None;
    let mut dropping = false;
    // キーフレーム要求のスロットル。ロスが連続しても要求が殺到しないようにする。
    let mut last_keyframe_req = Instant::now() - Duration::from_secs(1);

    loop {
        let n = if established {
            udp.recv(&mut recv_buf).await?
        } else {
            tokio::select! {
                r = udp.recv(&mut recv_buf) => {
                    established = true;
                    r?
                }
                _ = hello_interval.tick() => {
                    let _ = udp.send(b"DDDHELLO").await;
                    continue;
                }
            }
        };
        // 受信スループット (映像 + 音声、データグラム全体) を計測。
        stats.add_bytes(n as u64);
        // 先頭 8B は平文の nonce (BE)。これで復号する。
        if n < 8 {
            continue;
        }
        let nonce = u64::from_be_bytes(recv_buf[..8].try_into().unwrap());
        plain.clear();
        if session
            .lock()
            .await
            .decrypt(nonce, &recv_buf[8..n], &mut plain)
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
                // 参照フレームが壊れたままにならないよう、キーフレームを要求して回復する。
                // 連続ロスでの殺到を避けるため最短 200ms 間隔にスロットル。
                if last_keyframe_req.elapsed() >= Duration::from_millis(200) {
                    last_keyframe_req = Instant::now();
                    let _ = keyframe_tx.send(ControlMsg::RequestKeyframe);
                }
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

/// 受信バッファを拡大した UDP ソケットを 0.0.0.0:任意ポート で開く。
///
/// 既定の受信バッファ (数百 KB) は 1440p/60fps のバースト時に溢れてパケットロス
/// (映像ノイズの主因) を招くため、4MB に広げてバーストを吸収する。
fn make_udp_socket() -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    // 失敗しても致命的ではない (既定値で続行)。
    let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);
    sock.set_nonblocking(true)?;
    let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    sock.bind(&addr.into())?;
    let std_udp: std::net::UdpSocket = sock.into();
    Ok(UdpSocket::from_std(std_udp)?)
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
