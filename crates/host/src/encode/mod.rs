//! 映像エンコーダ抽象 (Port)。

use crate::capture::Frame;

#[derive(Debug, Clone)]
pub struct EncodedNal {
    /// Annex-B (`00 00 00 01` start code 付き) の H.264 バイト列。
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub frame_id: u16,
}

pub trait VideoEncoder: Send {
    fn push_frame(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn poll_nal(&mut self) -> anyhow::Result<Option<EncodedNal>>;
    fn request_keyframe(&mut self);
    /// 平均ビットレート (bps) を実行時に変更する。適応制御から呼ばれる。
    fn set_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()>;
}

/// エンコーダ初期ビットレート (bps)。適応制御の開始点。
pub const DEFAULT_BITRATE_BPS: u32 = 8_000_000;
/// 適応制御で許容する下限 / 上限ビットレート (bps)。PLAN.md の帯域目標に基づく。
pub const MIN_BITRATE_BPS: u32 = 1_000_000;
pub const MAX_BITRATE_BPS: u32 = 15_000_000;

#[cfg(target_os = "windows")]
mod mf;
#[cfg(target_os = "windows")]
pub use mf::MfH264Encoder as PlatformEncoder;

// Linux / macOS は ffmpeg-next 経由の H.264 エンコーダを共有する
// (Linux: nvenc/libx264, macOS: videotoolbox)。
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod ffmpeg;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use ffmpeg::FfmpegH264Encoder as PlatformEncoder;
