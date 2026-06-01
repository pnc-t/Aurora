//! 画面キャプチャ抽象 (Port)。OS別の Adapter は cfg で差し替える。

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA8 行間隔 (バイト)。`width * 4` と一致するとは限らない。
    pub stride: u32,
    pub bgra: Vec<u8>,
    #[allow(dead_code)] // 取得時刻。遅延計測用に保持 (現状未使用)
    pub captured_at: Instant,
}

pub trait ScreenCapturer: Send {
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;
}

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::DxgiCapturer as PlatformCapturer;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::PipewireCapturer as PlatformCapturer;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::ScreenCaptureKitCapturer as PlatformCapturer;
