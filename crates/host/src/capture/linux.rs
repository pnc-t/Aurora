//! Linux 画面キャプチャ: xdg-desktop-portal (ScreenCast) + PipeWire。
//!
//! フロー:
//! 1. `ashpd` で xdg-desktop-portal の ScreenCast を開き、モニタを 1 つ選択する。
//!    ポータルから PipeWire ノード ID と remote fd を受け取る。
//! 2. その fd で PipeWire に接続し、ノードへ Stream を張る。
//! 3. PipeWire メインループ (同期) を専用スレッドで回し、`process` コールバックで
//!    届いた BGRx バッファを `Frame` (BGRA 相当) として mpsc に流す。
//!
//! 注意:
//! - Wayland/X11 いずれもポータル経由 (PipeWire) で統一。compositor が
//!   xdg-desktop-portal-* バックエンドを提供している必要がある。
//! - ポータルのソース選択はユーザー操作 (モニタ選択ダイアログ) を伴う場合がある。
//! - 本番検証は実機 (Linux + PipeWire) で行うこと。pipewire-rs / ashpd の API は
//!   バージョン差があり、ここでは pipewire 0.8 / ashpd 0.9 を前提にしている。

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use pipewire as pw;
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::VideoInfoRaw;
use pw::spa::pod::Pod;
use pw::spa::utils::Direction;

use super::{DisplayInfo, Frame, ScreenCapturer};

pub struct PipewireCapturer {
    rx: Receiver<Frame>,
}

/// Linux ではポータル/PipeWire 交渉前に解像度・配置を確定できないため、単一の primary を
/// 仮値で返す。実解像度は capture_loop が最初のフレームで上書きする。マルチディスプレイの
/// 動的切替はポータルのソース選択 UI に委ねる前提で未対応 (id は 0 固定)。
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    Ok(vec![DisplayInfo {
        id: 0,
        name: "primary".to_string(),
        width: 1920,
        height: 1080,
        x: 0,
        y: 0,
    }])
}

/// PipeWire コールバック間で共有する状態。
struct UserData {
    format: VideoInfoRaw,
    tx: SyncSender<Frame>,
}

impl PipewireCapturer {
    /// Linux はポータルがソースを選ぶため出力インデックスは無視し primary を開く。
    /// `_device` は Windows 専用の共有 D3D11 デバイス (Linux では `()`)。
    pub fn new_for_output(_device: super::SharedDevice, _output_index: u32) -> Result<Self> {
        Self::new_primary(_device)
    }

    pub fn new_primary(_device: super::SharedDevice) -> Result<Self> {
        // フレーム送出用。詰まったら捨てる (try_send) ためのバウンデッドチャネル。
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(2);
        // セットアップ (ポータル交渉 + Stream 接続) の成否を同期的に受け取る。
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                if let Err(e) = run_capture(frame_tx, &ready_tx) {
                    // セットアップ段階での失敗のみここで通知 (run 中の失敗は ready 済み)。
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { rx: frame_rx }),
            Ok(Err(e)) => Err(anyhow!("pipewire capture init failed: {e}")),
            Err(_) => Err(anyhow!("pipewire capture thread exited before init")),
        }
    }
}

impl ScreenCapturer for PipewireCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire capture thread ended")),
        }
    }
}

/// ポータルから PipeWire ノード ID と remote fd を取得する (非同期)。
async fn open_portal() -> Result<(u32, std::os::fd::OwnedFd)> {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, ScreenCast, SourceType};

    let proxy = ScreenCast::new().await.context("ScreenCast::new")?;
    let session = proxy.create_session().await.context("create_session")?;
    proxy
        .select_sources(
            &session,
            CursorMode::Embedded,
            SourceType::Monitor.into(),
            false, // multiple = false (1 モニタ)
            None,  // restore_token
            PersistMode::DoNot,
        )
        .await
        .context("select_sources")?;

    let response = proxy
        .start(&session, None)
        .await
        .context("portal start")?
        .response()
        .context("portal start response")?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no streams"))?;
    let node_id = stream.pipe_wire_node_id();

    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .context("open_pipe_wire_remote")?;

    // session / proxy は drop されると Close が飛びキャプチャが止まるため、
    // ストリーミング中は生かし続ける。ここでは forget せず呼び出し側へは fd/node のみ返し、
    // session/proxy は runtime と共に run_capture スコープで保持する。
    std::mem::forget(session);
    std::mem::forget(proxy);
    Ok((node_id, fd))
}

fn run_capture(
    frame_tx: SyncSender<Frame>,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    // ポータル交渉だけ tokio current-thread ランタイムで実行する。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime for portal")?;
    let (node_id, fd) = rt.block_on(open_portal())?;
    tracing::info!(node_id, "pipewire screencast node acquired");

    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None).context("pw MainLoop")?;
    let context = pw::context::Context::new(&mainloop).context("pw Context")?;
    let core = context.connect_fd(fd, None).context("pw connect_fd")?;

    let stream = pw::stream::Stream::new(
        &core,
        "aurora-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("pw Stream::new")?;

    let user_data = UserData {
        format: VideoInfoRaw::new(),
        tx: frame_tx,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_, _, old, new| {
            tracing::debug!(?old, ?new, "pipewire stream state");
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            if let Err(e) = user_data.format.parse(param) {
                tracing::warn!(error = ?e, "failed to parse video format");
            } else {
                tracing::info!(
                    width = user_data.format.size().width,
                    height = user_data.format.size().height,
                    "pipewire negotiated format"
                );
            }
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let stride = chunk.stride().max(0) as u32;
            let size = chunk.size() as usize;
            let width = user_data.format.size().width;
            let height = user_data.format.size().height;
            if width == 0 || height == 0 || stride == 0 {
                return;
            }
            if let Some(slice) = data.data() {
                let copy_len = size.min(slice.len());
                // BGRx をそのまま BGRA バッファとして扱う (X チャネルは下流の
                // swscale で無視される)。
                let bgra = slice[..copy_len].to_vec();
                let frame = Frame {
                    width,
                    height,
                    stride,
                    bgra,
                };
                // バックプレッシャ時は捨てる (次フレームで回復)。
                let _ = user_data.tx.try_send(frame);
            }
        })
        .register()
        .context("register stream listener")?;

    // EnumFormat パラメータ (BGRx/RGBx, 任意サイズ, 任意フレームレート) を構築。
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 7680,
                height: 4320
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("Pod::from_bytes"))?];

    stream
        .connect(
            Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw stream connect")?;

    // ここまで来ればセットアップ成功。以降の失敗はログのみ。
    let _ = ready_tx.send(Ok(()));

    // ブロッキング: メインループを回し続ける。
    mainloop.run();
    Ok(())
}
