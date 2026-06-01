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
}

#[cfg(target_os = "windows")]
mod mf;
#[cfg(target_os = "windows")]
pub use mf::MfH264Encoder as PlatformEncoder;
