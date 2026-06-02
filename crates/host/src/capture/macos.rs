//! macOS 画面キャプチャ: ScreenCaptureKit (objc2 経由の薄い FFI)。
//!
//! `SCStream` に `SCStreamOutput` デリゲートを登録し、届いた `CMSampleBuffer` 内の
//! `CVPixelBuffer` (BGRA) を `Frame` に詰めて mpsc へ流す。
//!
//! 注意:
//! - 初回利用時に画面収録の許可ダイアログが出る (TCC)。許可がないと黒画面になる。
//! - macOS 12.3+ 必須。
//! - objc2 0.6 / objc2-screen-capture-kit 0.3 系を前提にしている。CoreVideo / dispatch2
//!   の細部はバージョン差があるため、本番検証は実機 (macOS) で行うこと。

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use anyhow::{Result, anyhow};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};

use super::{DisplayInfo, Frame, ScreenCapturer};

/// 接続済みディスプレイを列挙する。MVP では SCShareableContent のプライマリ 1 枚のみ返す
/// (id は 0 固定、原点は 0,0)。マルチディスプレイの動的切替は未対応。
pub fn enumerate_displays() -> anyhow::Result<Vec<DisplayInfo>> {
    let display = primary_display()?;
    // SAFETY: display は有効。width/height はピクセル寸法を返す。
    let (w, h) = unsafe { (display.width(), display.height()) };
    Ok(vec![DisplayInfo {
        id: 0,
        name: "primary".to_string(),
        width: w as u32,
        height: h as u32,
        x: 0,
        y: 0,
    }])
}

/// kCVPixelFormatType_32BGRA ('BGRA')。
const PIXEL_FORMAT_32BGRA: u32 = 0x4247_5241;

pub struct ScreenCaptureKitCapturer {
    rx: Receiver<Frame>,
    // Stream / デリゲートは drop されるとキャプチャが止まるため保持し続ける。
    _stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
}

// --- SCStreamOutput デリゲート ---

struct OutputIvars {
    tx: SyncSender<Frame>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DddStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type != SCStreamOutputType::Screen {
                return;
            }
            if let Some(frame) = sample_to_frame(sample_buffer) {
                let _ = self.ivars().tx.try_send(frame);
            }
        }
    }
);

impl StreamOutput {
    fn new(tx: SyncSender<Frame>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { tx });
        // SAFETY: NSObject の指定イニシャライザ。
        unsafe { msg_send![super(this), init] }
    }
}

/// CMSampleBuffer → Frame (BGRA)。失敗時は None。
fn sample_to_frame(sample: &CMSampleBuffer) -> Option<Frame> {
    use objc2_core_media::CMSampleBufferGetImageBuffer;
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };

    // SAFETY: sample は有効。GetImageBuffer は CVImageBuffer (=CVPixelBuffer) を返す。
    let image_buffer = unsafe { CMSampleBufferGetImageBuffer(sample) }?;
    let read_only = CVPixelBufferLockFlags::ReadOnly;

    // SAFETY: lock/unlock を対で呼ぶ。
    unsafe {
        if CVPixelBufferLockBaseAddress(&image_buffer, read_only) != 0 {
            return None;
        }
    }
    let result = (|| {
        // SAFETY: ロック中の base address / 行幅 / 寸法取得。
        let base = unsafe { CVPixelBufferGetBaseAddress(&image_buffer) };
        if base.is_null() {
            return None;
        }
        let stride = unsafe { CVPixelBufferGetBytesPerRow(&image_buffer) };
        let width = unsafe { CVPixelBufferGetWidth(&image_buffer) };
        let height = unsafe { CVPixelBufferGetHeight(&image_buffer) };
        let size = stride * height;
        let mut bgra = vec![0u8; size];
        // SAFETY: base は size バイト読める (ロック中)。
        unsafe {
            std::ptr::copy_nonoverlapping(base as *const u8, bgra.as_mut_ptr(), size);
        }
        Some(Frame {
            width: width as u32,
            height: height as u32,
            stride: stride as u32,
            bgra,
        })
    })();
    // SAFETY: lock と対。
    unsafe {
        CVPixelBufferUnlockBaseAddress(&image_buffer, read_only);
    }
    result
}

impl ScreenCaptureKitCapturer {
    /// macOS は MVP では primary 固定。出力インデックスは無視する。
    pub fn new_for_output(_output_index: u32) -> Result<Self> {
        Self::new_primary()
    }

    pub fn new_primary() -> Result<Self> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(2);

        let display = primary_display()?;

        // フィルタ: プライマリディスプレイ全体、除外ウィンドウなし。
        let empty: Retained<NSArray<objc2::runtime::AnyObject>> = NSArray::new();
        // SAFETY: display / empty は有効。
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &empty,
            )
        };

        let config = build_config(&display);
        let output = StreamOutput::new(frame_tx);

        // SAFETY: filter / config は有効、stream デリゲートは nil。
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };

        // サンプルハンドラ用シリアルキュー。
        let queue = dispatch2::DispatchQueue::new("com.ddd.capture", None);
        let output_proto = ProtocolObject::from_ref(&*output);

        // SAFETY: stream / output 有効。Screen 出力を要求。
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output_proto,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("addStreamOutput failed: {e:?}"))?;
        }

        // startCapture (完了を待つ)。
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let handler = RcBlock::new(move |error: *mut NSError| {
            if error.is_null() {
                let _ = done_tx.send(Ok(()));
            } else {
                // SAFETY: error は有効な NSError。
                let desc = unsafe { (*error).localizedDescription() };
                let _ = done_tx.send(Err(desc.to_string()));
            }
        });
        // SAFETY: stream 有効、handler は completion block。
        unsafe {
            let _: () = msg_send![&stream, startCaptureWithCompletionHandler: &*handler];
        }

        match done_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow!("startCapture failed: {e}")),
            Err(_) => return Err(anyhow!("startCapture timed out")),
        }

        Ok(Self {
            rx: frame_rx,
            _stream: stream,
            _output: output,
        })
    }
}

/// `SCShareableContent` を同期取得してプライマリディスプレイを返す。
fn primary_display() -> Result<Retained<SCDisplay>> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<Retained<SCShareableContent>, String>>();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() {
                // SAFETY: error 有効。
                let desc = unsafe { (*error).localizedDescription() };
                let _ = tx.send(Err(desc.to_string()));
            } else if content.is_null() {
                let _ = tx.send(Err("nil shareable content".into()));
            } else {
                // SAFETY: content は +0 で渡るため retain して所有する。
                match unsafe { Retained::retain(content) } {
                    Some(c) => {
                        let _ = tx.send(Ok(c));
                    }
                    None => {
                        let _ = tx.send(Err("retain failed".into()));
                    }
                }
            }
        },
    );
    // SAFETY: クラスメソッド。handler は completion block。
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }

    let content = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(anyhow!("getShareableContent failed: {e}")),
        Err(_) => return Err(anyhow!("getShareableContent timed out")),
    };

    // SAFETY: displays は NSArray<SCDisplay>。
    let displays = unsafe { content.displays() };
    displays
        .first()
        .ok_or_else(|| anyhow!("no display available"))
}

fn build_config(display: &SCDisplay) -> Retained<SCStreamConfiguration> {
    // SAFETY: 既定イニシャライザ + setter。
    unsafe {
        let config = SCStreamConfiguration::new();
        let width = display.width();
        let height = display.height();
        config.setWidth(width);
        config.setHeight(height);
        config.setPixelFormat(PIXEL_FORMAT_32BGRA);
        config.setShowsCursor(true);
        // 60fps: minimumFrameInterval = CMTime(1, 60)。
        let interval = objc2_core_media::CMTime {
            value: 1,
            timescale: 60,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        config.setMinimumFrameInterval(interval);
        config
    }
}

impl ScreenCapturer for ScreenCaptureKitCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("SCStream output ended")),
        }
    }
}

// SAFETY: rx は単一スレッド (capture loop) からのみ触られる。Retained は内部で
// スレッドセーフな参照カウントを持つ。capturer はキャプチャスレッドへ move される。
unsafe impl Send for ScreenCaptureKitCapturer {}
