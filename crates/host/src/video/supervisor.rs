//! 本体側: 取り込みワーカーを起動・監視し、net との口 (チャネル) を中継する。
//!
//! net から見た口は、ワーカー化する前に capture/encode スレッドと直接つないでいたものと同じ
//! ([`VideoLink`])。ワーカーが落ちたら (固着からの回復を含む) 作り直し、表示中のディスプレイ・
//! fps・解像度・カーソル合成・コーデック・ビットレート・テイクオーバー状態を送り直してから
//! キーフレームを要求する。TCP/UDP は本体が持ったままなので、クライアントは切断されない。

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aurora_common::proto::VideoCodec;
use tokio::sync::mpsc;

use super::wire::{FromWorker, ToWorker, read_msg, write_msg};
use crate::encode::EncodedNal;
use crate::net::{CodecStatus, EncoderCommand};
use crate::tray::SharedTrayBus;

/// 最初のワーカーの準備完了を待つ上限 (本体はこの後で接続待受を始める)。
const FIRST_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// net からの指示をワーカーへ送る周期。キーフレーム要求等の遅れはこの程度で問題ない。
const FORWARD_POLL: Duration = Duration::from_millis(5);
/// これより長く動いたワーカーの終了は、固着からの回復 (想定内の作り直し) とみなし即座に起動する。
/// 短命な終了 (起動失敗) が続く場合だけ待ちを伸ばす。
const HEALTHY_RUNTIME: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// 最初のワーカーの準備完了で分かる情報 (最初のフレームの寸法・エンコード可能なコーデック)。
type ReadyInfo = (Option<(u32, u32)>, Vec<VideoCodec>);
type ReadySender = std::sync::mpsc::SyncSender<ReadyInfo>;

/// ワーカー起動時の設定。
pub struct VideoConfig {
    pub display: String,
    pub fps_cap: u32,
    pub max_bitrate_bps: u32,
    pub encode_gpu: Option<String>,
    pub perf: bool,
}

/// net とつなぐ口と、最初のワーカーが報告した情報。
pub struct VideoLink {
    pub nal_rx: mpsc::Receiver<EncodedNal>,
    pub codec_status_rx: mpsc::Receiver<CodecStatus>,
    pub enc_cmd_tx: mpsc::Sender<EncoderCommand>,
    pub select_tx: std::sync::mpsc::Sender<String>,
    pub fps_tx: std::sync::mpsc::Sender<u32>,
    pub scale_tx: std::sync::mpsc::Sender<u32>,
    pub cursor_tx: std::sync::mpsc::Sender<bool>,
    pub force_frame: Arc<AtomicBool>,
    /// 最初のフレームの実寸法 (取れなければ `None`)。
    pub resolution: Option<(u32, u32)>,
    /// 実際にエンコードできるコーデック。
    pub host_codecs: Vec<VideoCodec>,
}

/// 作り直したワーカーへ送り直す、net が最後に指示した状態。
#[derive(Default)]
struct Replay {
    display: String,
    fps: Option<u32>,
    scale: Option<u32>,
    cursor: Option<bool>,
    codec: Option<VideoCodec>,
    bitrate: Option<u32>,
    takeover: bool,
}

/// 今動いているワーカーの標準入力と、送り直す状態。指示の送信と作り直し時の再送が混ざらないよう
/// 1 つのロックで守る。
struct Shared {
    stdin: Option<ChildStdin>,
    replay: Replay,
}

impl Shared {
    fn send(&mut self, msg: &ToWorker) {
        if let Some(stdin) = self.stdin.as_mut()
            && write_msg(stdin, msg)
                .and_then(|()| Ok(stdin.flush()?))
                .is_err()
        {
            // ワーカーが落ちた。監視側が作り直して状態を送り直す。
            self.stdin = None;
        }
    }
}

/// ワーカーを起動し、最初の準備完了 (または待ち時間切れ) まで待って net との口を返す。
pub fn start(cfg: VideoConfig, tray_bus: SharedTrayBus) -> Result<VideoLink> {
    let (nal_tx, nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (codec_tx, codec_status_rx) = mpsc::channel::<CodecStatus>(4);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);
    let (select_tx, select_rx) = std::sync::mpsc::channel::<String>();
    let (fps_tx, fps_rx) = std::sync::mpsc::channel::<u32>();
    let (scale_tx, scale_rx) = std::sync::mpsc::channel::<u32>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<bool>();
    let force_frame = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<ReadyInfo>(1);

    let shared = Arc::new(Mutex::new(Shared {
        stdin: None,
        replay: Replay {
            display: cfg.display.clone(),
            ..Default::default()
        },
    }));

    // net → ワーカー の中継。
    {
        let shared = shared.clone();
        let force_frame = force_frame.clone();
        let mut enc_cmd_rx = enc_cmd_rx;
        std::thread::Builder::new()
            .name("video-forward".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(FORWARD_POLL);
                    let Ok(mut s) = shared.lock() else { return };
                    while let Ok(cmd) = enc_cmd_rx.try_recv() {
                        match cmd {
                            EncoderCommand::SetCodec(c) => s.replay.codec = Some(c),
                            EncoderCommand::SetBitrate(b) => s.replay.bitrate = Some(b),
                            EncoderCommand::RequestKeyframe => {}
                        }
                        s.send(&ToWorker::Encoder(cmd));
                    }
                    while let Ok(name) = select_rx.try_recv() {
                        s.replay.display = name.clone();
                        s.send(&ToWorker::SelectDisplay(name));
                    }
                    while let Ok(f) = fps_rx.try_recv() {
                        s.replay.fps = Some(f);
                        s.send(&ToWorker::Fps(f));
                    }
                    while let Ok(h) = scale_rx.try_recv() {
                        s.replay.scale = Some(h);
                        s.send(&ToWorker::Scale(h));
                    }
                    while let Ok(on) = cursor_rx.try_recv() {
                        s.replay.cursor = Some(on);
                        s.send(&ToWorker::Cursor(on));
                    }
                    if force_frame.swap(false, Ordering::AcqRel) {
                        s.send(&ToWorker::ForceFrame);
                    }
                    let takeover = crate::display::is_takeover_active();
                    if takeover != s.replay.takeover {
                        s.replay.takeover = takeover;
                        s.send(&ToWorker::Takeover(takeover));
                    }
                }
            })
            .context("spawn video-forward thread")?;
    }

    // ワーカーの起動・監視と ワーカー → net の中継。
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("video-supervisor".into())
            .spawn(move || supervise(cfg, shared, nal_tx, codec_tx, tray_bus, ready_tx))
            .context("spawn video-supervisor thread")?;
    }

    let (resolution, host_codecs) = match ready_rx.recv_timeout(FIRST_READY_TIMEOUT) {
        Ok(r) => r,
        Err(_) => {
            // 取り込みが始められなくても待受は始める (ワーカーは裏で作り直し続ける)。
            tracing::warn!("capture worker not ready in time; assuming H.264 only");
            (None, vec![VideoCodec::H264])
        }
    };
    Ok(VideoLink {
        nal_rx,
        codec_status_rx,
        enc_cmd_tx,
        select_tx,
        fps_tx,
        scale_tx,
        cursor_tx,
        force_frame,
        resolution,
        host_codecs,
    })
}

fn supervise(
    cfg: VideoConfig,
    shared: Arc<Mutex<Shared>>,
    nal_tx: mpsc::Sender<EncodedNal>,
    codec_tx: mpsc::Sender<CodecStatus>,
    tray_bus: SharedTrayBus,
    ready_tx: ReadySender,
) {
    let mut ready_tx = Some(ready_tx);
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        match spawn_worker() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                if let Ok(mut s) = shared.lock() {
                    s.stdin = child.stdin.take();
                    let init = ToWorker::Init {
                        display: s.replay.display.clone(),
                        fps_cap: cfg.fps_cap,
                        max_bitrate_bps: cfg.max_bitrate_bps,
                        encode_gpu: cfg.encode_gpu.clone(),
                        perf: cfg.perf,
                    };
                    s.send(&init);
                    for msg in replay_messages(&s.replay) {
                        s.send(&msg);
                    }
                }
                tracing::info!(pid = child.id(), "capture worker started");
                if let Some(stdout) = stdout
                    && !relay(
                        BufReader::new(stdout),
                        &nal_tx,
                        &codec_tx,
                        &tray_bus,
                        &mut ready_tx,
                    )
                {
                    // net 側が終わった (本体の終了処理中)。作り直さない。
                    let _ = child.kill();
                    return;
                }
                if let Ok(mut s) = shared.lock() {
                    s.stdin = None;
                }
                let status = child.wait().ok();
                let ran = started.elapsed();
                tracing::warn!(
                    exit = ?status.and_then(|s| s.code()),
                    ran_ms = ran.as_millis() as u64,
                    "capture worker exited; restarting"
                );
                if ran >= HEALTHY_RUNTIME {
                    backoff = BACKOFF_MIN;
                    continue;
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to start capture worker"),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// 作り直したワーカーへ送る、net が最後に指示した状態とキーフレーム要求。
fn replay_messages(r: &Replay) -> Vec<ToWorker> {
    let mut out = vec![ToWorker::Takeover(r.takeover)];
    if let Some(f) = r.fps {
        out.push(ToWorker::Fps(f));
    }
    if let Some(h) = r.scale {
        out.push(ToWorker::Scale(h));
    }
    if let Some(on) = r.cursor {
        out.push(ToWorker::Cursor(on));
    }
    if let Some(c) = r.codec {
        out.push(ToWorker::Encoder(EncoderCommand::SetCodec(c)));
    }
    if let Some(b) = r.bitrate {
        out.push(ToWorker::Encoder(EncoderCommand::SetBitrate(b)));
    }
    // 新しいエンコーダは参照フレームを持たない。接続中のクライアントが続きを復号できるよう
    // キーフレームから始め、画面が静止していても 1 枚送らせる。
    out.push(ToWorker::Encoder(EncoderCommand::RequestKeyframe));
    out.push(ToWorker::ForceFrame);
    out
}

/// ワーカーの出力を net へ流す。ワーカーが終わったら `true`、net 側が終わったら `false`。
fn relay(
    mut out: impl std::io::Read,
    nal_tx: &mpsc::Sender<EncodedNal>,
    codec_tx: &mpsc::Sender<CodecStatus>,
    tray_bus: &SharedTrayBus,
    ready_tx: &mut Option<ReadySender>,
) -> bool {
    loop {
        let msg = match read_msg::<FromWorker>(&mut out) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(e) => {
                tracing::warn!(error = %e, "capture worker pipe broken");
                return true;
            }
        };
        match msg {
            FromWorker::Ready {
                resolution,
                host_codecs,
            } => {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send((resolution, host_codecs));
                }
            }
            FromWorker::Nal(nal) => {
                if nal_tx.blocking_send(nal).is_err() {
                    return false;
                }
            }
            FromWorker::Codec(status) => {
                if codec_tx.blocking_send(status).is_err() {
                    return false;
                }
            }
            FromWorker::EncodeStalled => tray_bus.on_encode_stalled(),
            FromWorker::RestoreTakeover => {
                tracing::error!(
                    "capture worker reports takeover stuck; restoring physical displays"
                );
                if let Err(e) = crate::display::end_takeover() {
                    tracing::warn!(error = %e, "end_takeover requested by capture worker failed");
                }
            }
        }
    }
}

/// 自分の exe を `--capture-worker` で起動する。トークン・セッション・デスクトップは本体から
/// 引き継ぐ (サービス配下なら SYSTEM・対話セッション・winsta0\default)。
fn spawn_worker() -> Result<Child> {
    let exe = std::env::current_exe().context("get current exe path")?;
    let mut cmd = Command::new(exe);
    cmd.arg(super::WORKER_ARG);
    // 本体がサービス配下なら、ワーカーの判定 (ロック中の固着の数え方等) も揃える。
    if crate::system::service::is_service_child() {
        cmd.arg(crate::system::service::SERVICE_CHILD_ARG);
    }
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--log-level"
            && let Some(level) = args.next()
        {
            cmd.arg("--log-level").arg(level);
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CLI の開発実行 (デバッグビルドはコンソール サブシステム) で、ワーカーが新しいコンソール
        // 窓を開かないようにする。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().context("spawn capture worker")
}
