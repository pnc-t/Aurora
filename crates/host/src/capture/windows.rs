//! DXGI Desktop Duplication ベースのプライマリディスプレイキャプチャ。

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIOutput,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::{DisplayInfo, Frame, ScreenCapturer};

pub struct DxgiCapturer {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

/// D3D11 デバイス + コンテキストを 1 つ作る。各キャプチャ・列挙で共通。
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: D3D11CreateDevice は OUT パラメータの ID3D11Device 等を初期化する。out-only。
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1];
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice failed")?;
        Ok((
            device.ok_or_else(|| anyhow!("D3D11 device null"))?,
            context.ok_or_else(|| anyhow!("D3D11 context null"))?,
        ))
    }
}

/// 接続済みの全ディスプレイ (DXGI 出力) を列挙する。`id` は EnumOutputs のインデックス
/// で、これが `new_for_output` / `SelectDisplay` の指定値になる。
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    let (device, _context) = create_device()?;
    let dxgi_device: IDXGIDevice = device.cast().context("cast IDXGIDevice")?;
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.context("GetAdapter")?;

    let mut displays = Vec::new();
    let mut i = 0u32;
    loop {
        // SAFETY: EnumOutputs は範囲外で DXGI_ERROR_NOT_FOUND を返す。
        let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(i) } {
            Ok(o) => o,
            Err(_) => break,
        };
        // SAFETY: output live。GetDesc は DXGI_OUTPUT_DESC を値で返す。
        let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
        let r = desc.DesktopCoordinates;
        let name = String::from_utf16_lossy(&desc.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        displays.push(DisplayInfo {
            id: i,
            name,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            x: r.left,
            y: r.top,
        });
        i += 1;
    }

    if displays.is_empty() {
        return Err(anyhow!("no DXGI outputs found"));
    }
    Ok(displays)
}

impl DxgiCapturer {
    pub fn new_primary() -> Result<Self> {
        Self::new_for_output(0)
    }

    /// 指定インデックスの DXGI 出力をキャプチャ対象にする。
    pub fn new_for_output(output_index: u32) -> Result<Self> {
        let (device, context) = create_device()?;

        // SAFETY: 取得した device は live。cast は同 COM オブジェクトの別 IID。
        let dxgi_device: IDXGIDevice = device.cast().context("cast IDXGIDevice")?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.context("GetAdapter")?;
        let output: IDXGIOutput = unsafe { adapter.EnumOutputs(output_index) }
            .with_context(|| format!("EnumOutputs({output_index}) -- no such display?"))?;
        let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
        let duplication: IDXGIOutputDuplication =
            unsafe { output1.DuplicateOutput(&device) }.context("DuplicateOutput")?;

        // SAFETY: duplication live。GetDesc は値返し。
        let desc = unsafe { duplication.GetDesc() };
        let width = desc.ModeDesc.Width;
        let height = desc.ModeDesc.Height;

        Ok(Self {
            _device: device,
            context,
            duplication,
            staging: None,
            width,
            height,
        })
    }

    fn ensure_staging(&mut self, width: u32, height: u32) -> Result<&ID3D11Texture2D> {
        let need_new = match &self.staging {
            Some(_) => self.width != width || self.height != height,
            None => true,
        };
        if need_new {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: Default::default(),
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: Default::default(),
            };
            // SAFETY: device は live、desc は全フィールド初期化済み、out param は null 可。
            let tex: ID3D11Texture2D = unsafe {
                let mut tex: Option<ID3D11Texture2D> = None;
                self._device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(staging)")?;
                tex.ok_or_else(|| anyhow!("staging texture null"))?
            };
            self.staging = Some(tex);
            self.width = width;
            self.height = height;
        }
        Ok(self.staging.as_ref().unwrap())
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: out param は OK の場合のみ初期化される。
        let acquire_result = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };

        match acquire_result {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) => return Err(anyhow!("AcquireNextFrame: {e}")),
        }

        let resource =
            resource.ok_or_else(|| anyhow!("AcquireNextFrame returned null resource"))?;
        let src_tex: ID3D11Texture2D = resource.cast().context("cast ID3D11Texture2D")?;

        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src_tex.GetDesc(&mut src_desc) };

        let staging = self
            .ensure_staging(src_desc.Width, src_desc.Height)?
            .clone();

        // SAFETY: 同 device の 2 つの texture 間で CopyResource。
        unsafe { self.context.CopyResource(&staging, &src_tex) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging は D3D11_USAGE_STAGING + CPU_ACCESS_READ。Map は安全に呼べる。
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(staging)")?;
        }

        let width = src_desc.Width;
        let height = src_desc.Height;
        let stride = mapped.RowPitch;
        let size = (stride as usize) * (height as usize);

        let mut bgra = vec![0u8; size];
        // SAFETY: mapped.pData は size バイト読める。
        unsafe {
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, bgra.as_mut_ptr(), size);
            self.context.Unmap(&staging, 0);
        }

        // SAFETY: AcquireNextFrame と対になる。
        let _ = unsafe { self.duplication.ReleaseFrame() };

        Ok(Some(Frame {
            width,
            height,
            stride,
            bgra,
            captured_at: Instant::now(),
        }))
    }
}
