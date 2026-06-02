//! 画面キャプチャ抽象 (Port)。OS別の Adapter は cfg で差し替える。

use std::time::Duration;

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA8 行間隔 (バイト)。`width * 4` と一致するとは限らない。
    pub stride: u32,
    pub bgra: Vec<u8>,
}

pub trait ScreenCapturer: Send {
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;
}

/// 接続クライアントへ提示する 1 ディスプレイの情報。
/// `x` / `y` はデスクトップ仮想座標系での原点 (入力注入のマッピングに使う)。
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

impl From<&DisplayInfo> for ddd_common::proto::Display {
    fn from(d: &DisplayInfo) -> Self {
        Self {
            id: d.id,
            width: d.width,
            height: d.height,
            name: d.name.clone(),
        }
    }
}

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{DxgiCapturer as PlatformCapturer, enumerate_displays};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{PipewireCapturer as PlatformCapturer, enumerate_displays};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{ScreenCaptureKitCapturer as PlatformCapturer, enumerate_displays};
