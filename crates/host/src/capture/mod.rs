//! 画面キャプチャ抽象 (Port)。OS別の Adapter は cfg で差し替える。

use std::time::Duration;

/// 1 フレーム。Windows は GPU 上の NV12 テクスチャ (ゼロコピー)、他 OS は CPU の BGRA8。
#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA8 行間隔 (バイト)。`width * 4` と一致するとは限らない。
    pub stride: u32,
    pub bgra: Vec<u8>,
}

/// Windows: GPU 色変換済みの NV12 テクスチャを保持する。CPU リードバックなし。
#[cfg(target_os = "windows")]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// NV12 GPU テクスチャ。共有デバイスは multithread-protected。
    pub texture: SendTexture,
}

/// `ID3D11Texture2D` をスレッド間で move 可能にする newtype。
///
/// 共有デバイスが multithread-protected なので、別スレッド (encode) から
/// このテクスチャを参照しても D3D11 が内部で直列化する。`MfH264Encoder` の
/// `unsafe impl Send` と同じ根拠。
#[cfg(target_os = "windows")]
pub struct SendTexture(pub ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D);

// SAFETY: 上記の根拠 (multithread-protected device) によりスレッド間 move は安全。
#[cfg(target_os = "windows")]
unsafe impl Send for SendTexture {}

pub trait ScreenCapturer: Send {
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;
}

/// capture・convert・encode で共有するデバイス。Windows は `Arc<DeviceBundle>`、
/// 他 OS は不要なので `()`。コンストラクタへ一様に渡せるようにする。
#[cfg(target_os = "windows")]
pub type SharedDevice = std::sync::Arc<device::DeviceBundle>;
#[cfg(not(target_os = "windows"))]
pub type SharedDevice = ();

/// 共有デバイスを 1 つ生成する。プロセス起動時に 1 回だけ呼ぶ。
#[cfg(target_os = "windows")]
pub fn create_shared_device() -> anyhow::Result<SharedDevice> {
    Ok(std::sync::Arc::new(device::DeviceBundle::new()?))
}
#[cfg(not(target_os = "windows"))]
pub fn create_shared_device() -> anyhow::Result<SharedDevice> {
    Ok(())
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

impl From<&DisplayInfo> for aurora_common::proto::Display {
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
pub mod convert;
#[cfg(target_os = "windows")]
pub mod device;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{DxgiCapturer as PlatformCapturer, debug_frame_to_bgra, enumerate_displays};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{PipewireCapturer as PlatformCapturer, enumerate_displays};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{ScreenCaptureKitCapturer as PlatformCapturer, enumerate_displays};
