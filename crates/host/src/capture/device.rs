//! capture・convert・encode で共有する D3D11 デバイス一式 (Windows)。
//!
//! ゼロコピー映像パイプラインでは「キャプチャテクスチャ → GPU 色変換 → NV12 →
//! HW エンコーダ」を**同一 D3D11 デバイス上**で完結させる必要がある。デバイスを
//! 1 つだけ生成して `DxgiCapturer` / `Converter` / HW MFT で共有する。
//!
//! capture スレッドと MFT 内部スレッドが同じデバイス・即時コンテキストを触るため
//! `ID3D11Multithread::SetMultithreadProtected(TRUE)` で内部直列化を有効化する。

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
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
    /// このデバイスが載っているアダプタの LUID。エンコーダ選択 (キャプチャと同一 GPU の
    /// MFT を優先する LUID フィルタ) に使う。取得失敗時はゼロ LUID (後段のフィルタが
    /// 総当たり列挙へ劣化するだけで致命ではない)。
    pub adapter_luid: LUID,
    /// このデバイスが載っているアダプタの DXGI 名 (Description)。VDD の描画 GPU を
    /// キャプチャ GPU に一致させる (friendlyname 照合) のに使う。取得失敗時は空文字列。
    pub adapter_name: String,
}

// SAFETY: device は SetMultithreadProtected(TRUE) 済みで、即時コンテキストへの
// 並行アクセスは D3D11 が内部で直列化する。COM は MTA で初期化される。よって
// capture / encode スレッド間で本構造体を共有しても安全。既存の
// `unsafe impl Send for MfH264Encoder` と同じ流儀。
unsafe impl Send for DeviceBundle {}
// SAFETY: Send と同根拠。device は multithread-protected で並行アクセスは D3D11 が直列化する。
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
    ///
    /// `adapter` を渡すとそのアダプタ上にデバイスを作る。ハイブリッド GPU のノートでは
    /// キャプチャ対象ディスプレイを所有するアダプタが既定アダプタと異なることがあり、その場合
    /// 既定アダプタのデバイスでは `DuplicateOutput` が恒常的に失敗する (画面真っ暗)。対象出力を
    /// 所有するアダプタを明示することでこれを避ける。`None` なら既定アダプタ (単一 GPU 環境)。
    pub fn new(adapter: Option<&IDXGIAdapter>) -> Result<Self> {
        init_thread_com()?;

        // D3D11: pAdapter を渡す場合は DriverType を UNKNOWN にしなければ E_INVALIDARG。
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };

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
                adapter,
                driver_type,
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

        // 選択されたアダプタ名 (+ LUID) をログに出す (ハイブリッド GPU の取り違え診断用)。
        let desc = device
            .cast::<IDXGIDevice>()
            .ok()
            // SAFETY: dxgi_device は live。GetAdapter は所有権付き COM を返す。
            .and_then(|dxgi_device| unsafe { dxgi_device.GetAdapter() }.ok())
            // SAFETY: adp は live。GetDesc は値 (OUT 構造体) を返す。
            .and_then(|adp| unsafe { adp.GetDesc() }.ok());
        let (adapter_luid, adapter_name) = match &desc {
            Some(desc) => {
                let name = String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_string();
                tracing::info!(
                    gpu = %name,
                    explicit_adapter = adapter.is_some(),
                    "shared D3D11 device adapter"
                );
                (desc.AdapterLuid, name)
            }
            None => {
                tracing::warn!(
                    "adapter LUID unavailable; encoder LUID filter degrades to enumeration order"
                );
                (LUID::default(), String::new())
            }
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
            adapter_luid,
            adapter_name,
        })
    }
}
