//! capture・convert・encode で共有する D3D11 デバイス一式 (Windows)。
//!
//! ゼロコピー映像パイプラインでは「キャプチャテクスチャ → GPU 色変換 → NV12 →
//! HW エンコーダ」を**同一 D3D11 デバイス上**で完結させる必要がある。デバイスを
//! 1 つだけ生成して `DxgiCapturer` / `Converter` / HW MFT で共有する。
//!
//! capture スレッドと MFT 内部スレッドが同じデバイス・即時コンテキストを触るため
//! `ID3D11Multithread::SetMultithreadProtected(TRUE)` で内部直列化を有効化する。

use anyhow::{Context, Result, anyhow};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MFSTARTUP_LITE, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::Interface;

/// MF_VERSION = (MF_SDK_VERSION << 16) | MF_API_VERSION = (2 << 16) | 0x0070
const MF_VERSION_CONST: u32 = (2u32 << 16) | 0x0070;

/// 共有 D3D11 デバイス + 即時コンテキスト + MF 用 DXGI デバイスマネージャ。
pub struct DeviceBundle {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
}

// SAFETY: device は SetMultithreadProtected(TRUE) 済みで、即時コンテキストへの
// 並行アクセスは D3D11 が内部で直列化する。COM は MTA で初期化される。よって
// capture / encode スレッド間で本構造体を共有しても安全。既存の
// `unsafe impl Send for MfH264Encoder` と同じ流儀。
unsafe impl Send for DeviceBundle {}
unsafe impl Sync for DeviceBundle {}

/// このスレッドで COM(MTA) + Media Foundation を初期化する (冪等)。各ワーカースレッド
/// (capture / encode) の先頭で呼ぶ。MFStartup はプロセス内参照カウントなので多重呼び OK。
pub fn init_thread_com() -> Result<()> {
    // SAFETY: 多重初期化は許可される。既に MTA 初期化済みなら S_FALSE が返るだけ。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION_CONST, MFSTARTUP_LITE).context("MFStartup")?;
    }
    Ok(())
}

impl DeviceBundle {
    /// 共有デバイスを 1 つ生成する。プロセス起動時に 1 回だけ呼ぶ。
    pub fn new() -> Result<Self> {
        init_thread_com()?;

        // SAFETY: D3D11CreateDevice は OUT パラメータを初期化する。BGRA + VIDEO_SUPPORT。
        let (device, context) = unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ];
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice (shared bundle)")?;
            (
                device.ok_or_else(|| anyhow!("D3D11 device null"))?,
                context.ok_or_else(|| anyhow!("D3D11 context null"))?,
            )
        };

        // capture スレッドと MFT 内部スレッドが同一デバイスを触るため必須。
        let mt: ID3D11Multithread = device.cast().context("cast ID3D11Multithread")?;
        // SAFETY: mt は device と同じ COM オブジェクト。戻り値の旧設定値は無視する。
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }

        // MF へデバイスを共有する DXGI デバイスマネージャ。
        // SAFETY: MFCreateDXGIDeviceManager は token と manager を OUT で返す。
        let manager = unsafe {
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .context("MFCreateDXGIDeviceManager")?;
            let manager = manager.ok_or_else(|| anyhow!("IMFDXGIDeviceManager null"))?;
            manager
                .ResetDevice(&device, token)
                .context("IMFDXGIDeviceManager::ResetDevice")?;
            manager
        };

        tracing::info!("shared D3D11 device created (multithread-protected, video support)");
        Ok(Self {
            device,
            context,
            manager,
        })
    }
}
