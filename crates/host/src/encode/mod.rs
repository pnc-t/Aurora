//! 映像エンコーダ抽象 (Port)。

use crate::capture::Frame;
#[cfg(target_os = "windows")]
use crate::capture::SharedDevice;

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
mod mf_hw;

/// Windows のプラットフォームエンコーダ。HW (NVENC/AMF/QSV) を優先し、初期化に失敗
/// したら SW (Microsoft H.264 MFT) へフォールバックする。どちらも GPU NV12 `Frame` を消費。
#[cfg(target_os = "windows")]
pub enum PlatformEncoder {
    Hw(mf_hw::MfHwH264Encoder),
    Sw(mf::MfH264Encoder),
}

#[cfg(target_os = "windows")]
impl PlatformEncoder {
    pub fn new(device: SharedDevice, max_bitrate_bps: u32) -> anyhow::Result<Self> {
        if std::env::var_os("AURORA_FORCE_SW_ENCODER").is_some() {
            tracing::warn!("AURORA_FORCE_SW_ENCODER set: forcing software H.264 MFT");
            return Ok(Self::Sw(mf::MfH264Encoder::new(device, max_bitrate_bps)?));
        }
        match mf_hw::MfHwH264Encoder::new(device.clone(), max_bitrate_bps) {
            Ok(e) => {
                tracing::info!("video encoder: hardware H.264 MFT (zero-copy NV12)");
                Ok(Self::Hw(e))
            }
            Err(e) => {
                tracing::warn!(error = %e, "HW H.264 encoder unavailable; falling back to software MFT (NV12 read-back)");
                Ok(Self::Sw(mf::MfH264Encoder::new(device, max_bitrate_bps)?))
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl VideoEncoder for PlatformEncoder {
    fn push_frame(&mut self, frame: &Frame) -> anyhow::Result<()> {
        match self {
            Self::Hw(e) => e.push_frame(frame),
            Self::Sw(e) => e.push_frame(frame),
        }
    }
    fn poll_nal(&mut self) -> anyhow::Result<Option<EncodedNal>> {
        match self {
            Self::Hw(e) => e.poll_nal(),
            Self::Sw(e) => e.poll_nal(),
        }
    }
    fn request_keyframe(&mut self) {
        match self {
            Self::Hw(e) => e.request_keyframe(),
            Self::Sw(e) => e.request_keyframe(),
        }
    }
    fn set_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()> {
        match self {
            Self::Hw(e) => e.set_bitrate(bitrate_bps),
            Self::Sw(e) => e.set_bitrate(bitrate_bps),
        }
    }
}

// Linux / macOS は ffmpeg-next 経由の H.264 エンコーダを共有する
// (Linux: nvenc/libx264, macOS: videotoolbox)。
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod ffmpeg;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use ffmpeg::FfmpegH264Encoder as PlatformEncoder;
