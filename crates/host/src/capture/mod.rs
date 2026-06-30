//! 画面キャプチャ抽象 (Port)。OS別の Adapter は cfg で差し替える。

use std::time::Duration;

/// 1 フレーム。Windows は GPU 上の NV12 テクスチャ (ゼロコピー)、他 OS は CPU の BGRA8。
#[cfg(not(target_os = "windows"))]
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA8 行間隔 (バイト)。`width * 4` と一致するとは限らない。
    pub stride: u32,
    pub bgra: Vec<u8>,
}

/// Windows: GPU 色変換済みの NV12 テクスチャを保持する。CPU リードバックなし。
/// `Clone` はテクスチャの COM 参照を増やすだけ (画素コピー無し)。capture_loop が
/// 直近フレームを保持し、接続/キーフレーム要求時に再投入するのに使う。
#[cfg(target_os = "windows")]
#[derive(Clone)]
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
#[derive(Clone)]
pub struct SendTexture(pub ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D);

#[cfg(target_os = "windows")]
// SAFETY: 上記の根拠 (multithread-protected device) によりスレッド間 move は安全。
unsafe impl Send for SendTexture {}

pub trait ScreenCapturer: Send {
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<Frame>>;

    /// 送出する映像の最大高さを設定する (0 = キャプチャ解像度のまま)。アスペクト比は維持。
    /// 既定は no-op (縮小未対応の OS 実装はそのまま)。Windows は GPU 縮小で実装する。
    fn set_target_height(&mut self, _max_height: u32) {}

    /// 映像へのカーソル合成 ON/OFF (クライアントのマウスキャプチャ中だけ ON)。
    /// 既定は no-op。Windows は GPU で焼き込む。
    fn set_cursor_overlay(&mut self, _enabled: bool) {}
}

/// capture・convert・encode で共有するデバイス。Windows は `Arc<DeviceBundle>`、
/// 他 OS は不要なので `()`。コンストラクタへ一様に渡せるようにする。
#[cfg(target_os = "windows")]
pub type SharedDevice = std::sync::Arc<device::DeviceBundle>;
#[cfg(not(target_os = "windows"))]
pub type SharedDevice = ();

/// 共有デバイスを 1 つ生成する。プロセス起動時に 1 回だけ呼ぶ。
///
/// `target_name` は初期キャプチャ対象の GDI 名 (`\\.\DISPLAYn`)。その出力を所有する
/// アダプタ上にデバイスを作る (ハイブリッド GPU で `DuplicateOutput` が失敗するのを防ぐ)。
#[cfg(target_os = "windows")]
pub fn create_shared_device(target_name: &str) -> anyhow::Result<SharedDevice> {
    let adapter = match windows::select_capture_adapter(target_name) {
        windows::CaptureAdapter::Adapter(a) => Some(a),
        windows::CaptureAdapter::AllUnsupported => {
            tracing::warn!(
                target = %target_name,
                "no DXGI adapter could DuplicateOutput (hybrid muxless?); using default adapter"
            );
            None
        }
        windows::CaptureAdapter::NotFound => {
            tracing::warn!(
                target = %target_name,
                "target output not found on any adapter; using default adapter"
            );
            None
        }
    };
    Ok(std::sync::Arc::new(device::DeviceBundle::new(
        adapter.as_ref(),
    )?))
}
#[cfg(not(target_os = "windows"))]
pub fn create_shared_device(_target_name: &str) -> anyhow::Result<SharedDevice> {
    Ok(())
}

/// dGPU プロセスでは内蔵パネルの Desktop Duplication が全アダプタで UNSUPPORTED になる
/// ハイブリッド GPU (muxless Optimus) を検出する。main がこのとき GPU プリファレンスを
/// iGPU(省電力) にして re-exec する判断に使う。Windows 以外は常に false。
#[cfg(target_os = "windows")]
pub fn capture_all_adapters_unsupported(target_name: &str) -> bool {
    matches!(
        windows::select_capture_adapter(target_name),
        windows::CaptureAdapter::AllUnsupported
    )
}
#[cfg(not(target_os = "windows"))]
pub fn capture_all_adapters_unsupported(_target_name: &str) -> bool {
    false
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
            // 仮想ディスプレイ判定は net 側で virtualdisplay::detect() の GDI 名と
            // 突き合わせて設定する (ここでは判定材料を持たないため既定 false)。
            is_virtual: false,
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
pub use windows::{
    DxgiCapturer as PlatformCapturer, debug_frame_to_bgra, debug_list_outputs, enumerate_displays,
    input_desktop_is_default,
};

/// Windows 以外: ロック画面/セキュアデスクトップ相当の概念を扱わないため常に true
/// (= capture_loop の固着時間は常に数える)。
#[cfg(not(target_os = "windows"))]
pub fn input_desktop_is_default() -> bool {
    true
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{PipewireCapturer as PlatformCapturer, enumerate_displays};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{ScreenCaptureKitCapturer as PlatformCapturer, enumerate_displays};
