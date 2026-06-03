//! DXGI Desktop Duplication ベースのディスプレイキャプチャ (ゼロコピー版)。
//!
//! acquire → CopyResource(shader-readable) → GPU 色変換 → NV12 テクスチャ、で完結する。
//! CPU での LUT トーンマップと staging リードバックは廃し、色変換は `convert::Converter`
//! (D3D11 シェーダ) が GPU 上で行う。デバイスは `DeviceBundle` を capture・encode で共有。

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIOutput,
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use super::convert::Converter;
use super::{DisplayInfo, Frame, ScreenCapturer, SendTexture, SharedDevice};

pub struct DxgiCapturer {
    /// 共有デバイスの保持 (寿命確保)。実 COM 参照は duplication / converter が持つ。
    _device: SharedDevice,
    duplication: IDXGIOutputDuplication,
    converter: Converter,
}

/// 列挙専用の使い捨て D3D11 デバイスを作る (起動時 1 回)。共有デバイスとは別。
fn create_enum_device() -> Result<ID3D11Device> {
    // SAFETY: D3D11CreateDevice は OUT パラメータを初期化する。out-only。
    unsafe {
        let mut device: Option<ID3D11Device> = None;
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
            None,
        )
        .context("D3D11CreateDevice (enum)")?;
        device.ok_or_else(|| anyhow!("D3D11 device null"))
    }
}

/// 接続済みの全ディスプレイ (DXGI 出力) を列挙する。`id` は EnumOutputs のインデックス。
pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    let device = create_enum_device()?;
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
    pub fn new_primary(device: SharedDevice) -> Result<Self> {
        Self::new_for_output(device, 0)
    }

    /// 指定インデックスの DXGI 出力をキャプチャ対象にする。共有デバイスを受け取る。
    pub fn new_for_output(device: SharedDevice, output_index: u32) -> Result<Self> {
        let d3d = &device.device;

        // SAFETY: device は live。cast は同 COM オブジェクトの別 IID。
        let dxgi_device: IDXGIDevice = d3d.cast().context("cast IDXGIDevice")?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.context("GetAdapter")?;
        let output: IDXGIOutput = unsafe { adapter.EnumOutputs(output_index) }
            .with_context(|| format!("EnumOutputs({output_index}) -- no such display?"))?;
        // SAFETY: output live。GetDesc は値返し。GDI デバイス名 (\\.\DISPLAYn) を得る。
        let out_desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
        let gdi_name = String::from_utf16_lossy(&out_desc.DeviceName)
            .trim_end_matches('\0')
            .to_string();

        // このディスプレイの HDR 有効状態と SDR white level を取得する。
        let (hdr_active, scale) = hdr_info(&gdi_name);
        tracing::info!(display = %gdi_name, hdr_active, sdr_white_scale = scale, "capture color");

        // HDR 有効時のみ FP16(scRGB) 取得する。HDR デスクトップは MPO により通常の
        // DuplicateOutput だと FP16/BGRA8 が混在するため、DuplicateOutput1 で scRGB に固定する。
        // SDR は従来の BGRA8 素通し (DuplicateOutput)。色変換は GPU シェーダで行う。
        let duplication: IDXGIOutputDuplication = if hdr_active {
            // DuplicateOutput1 は DPI-aware プロセスでないと E_ACCESSDENIED になる。
            // SAFETY: プロセス全体の設定。既に設定済みなら失敗するだけで無害。
            unsafe {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            match output.cast::<IDXGIOutput5>() {
                Ok(output5) => match unsafe {
                    output5.DuplicateOutput1(d3d, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT])
                } {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "DuplicateOutput1(FP16) failed, falling back");
                        let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
                        unsafe { output1.DuplicateOutput(d3d) }.context("DuplicateOutput")?
                    }
                },
                Err(_) => {
                    let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
                    unsafe { output1.DuplicateOutput(d3d) }.context("DuplicateOutput")?
                }
            }
        } else {
            let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
            unsafe { output1.DuplicateOutput(d3d) }.context("DuplicateOutput")?
        };

        let converter = Converter::new(&device, hdr_active, scale)?;

        Ok(Self {
            _device: device,
            duplication,
            converter,
        })
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

        // GPU で NV12 へ変換 (HDR トーンマップ込み)。CPU リードバックなし。
        let convert_result = self.converter.convert(&src_tex);

        // SAFETY: AcquireNextFrame と対になる。変換成否に関わらず解放する。
        let _ = unsafe { self.duplication.ReleaseFrame() };

        let nv12 = convert_result?;

        // NV12 テクスチャの寸法を読む (= キャプチャ解像度)。
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: nv12 live。GetDesc は OUT を埋める。
        unsafe { nv12.GetDesc(&mut desc) };

        Ok(Some(Frame {
            width: desc.Width,
            height: desc.Height,
            texture: SendTexture(nv12),
        }))
    }
}

/// 指定 GDI デバイス名 (`\\.\DISPLAYn`) の (HDR 有効か, scRGB 正規化係数) を返す。
/// HDR 有効 = advancedColorEnabled。係数 = SDRWhiteLevel/1000 (= white_nits/80)。
/// 取得失敗時は `(false, 1.0)` (= SDR とみなす)。
fn hdr_info(gdi_device_name: &str) -> (bool, f32) {
    use std::mem::size_of;
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let default = (false, 1.0);

    // SAFETY: アクティブパスの本数を問い合わせ、その分だけ確保してから QueryDisplayConfig する。
    unsafe {
        let mut num_paths = 0u32;
        let mut num_modes = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
            != ERROR_SUCCESS
        {
            return default;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return default;
        }
        paths.truncate(num_paths as usize);

        for path in &paths {
            // パスの source が対象の GDI デバイス名か確認する。
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = path.sourceInfo.adapterId;
            src.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&src.viewGdiDeviceName);
            if name.trim_end_matches('\0') != gdi_device_name {
                continue;
            }

            // advancedColorEnabled (bit1) を見て HDR 有効か判定する。
            let mut aci = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            aci.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            aci.header.size = size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            aci.header.adapterId = path.targetInfo.adapterId;
            aci.header.id = path.targetInfo.id;
            let hdr_active = if DisplayConfigGetDeviceInfo(&mut aci.header) == 0 {
                (aci.Anonymous.value >> 1) & 1 == 1
            } else {
                false
            };

            // SDR white level を取得し正規化係数にする。
            let mut wl = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            wl.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            wl.header.size = size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
            wl.header.adapterId = path.targetInfo.adapterId;
            wl.header.id = path.targetInfo.id;
            let scale = if DisplayConfigGetDeviceInfo(&mut wl.header) == 0 {
                (wl.SDRWhiteLevel as f32 / 1000.0).clamp(0.25, 16.0)
            } else {
                1.0
            };

            return (hdr_active, scale);
        }
        default
    }
}

/// デバッグ用 (`--dump-bmp`): NV12 `Frame` を BGRA8 (行詰め) に読み戻す。
/// staging へ `CopyResource` → `Map` し、BT.601 limited 逆変換で BGRA を組む。
pub fn debug_frame_to_bgra(device: &SharedDevice, frame: &Frame) -> Result<Vec<u8>> {
    let w = frame.width as usize;
    let h = frame.height as usize;

    let desc = D3D11_TEXTURE2D_DESC {
        Width: frame.width,
        Height: frame.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    // SAFETY: device live、desc 全フィールド初期化済み。
    let staging = unsafe {
        let mut tex = None;
        device
            .device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .context("CreateTexture2D(debug NV12 staging)")?;
        tex.ok_or_else(|| anyhow!("debug staging null"))?
    };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: 同サイズ・同フォーマット間コピー後、STAGING を Map する。
    unsafe {
        device.context.CopyResource(&staging, &frame.texture.0);
        device
            .context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .context("Map(debug NV12)")?;
    }

    let pitch = mapped.RowPitch as usize;
    let mut bgra = vec![0u8; w * h * 4];
    // SAFETY: pData は luma(h 行) + chroma(h/2 行) を各 pitch バイトで保持する。
    unsafe {
        let base = mapped.pData as *const u8;
        let chroma = base.add(pitch * h);
        for j in 0..h {
            for i in 0..w {
                let y = *base.add(j * pitch + i) as i32;
                let cbase = (j / 2) * pitch + (i / 2) * 2;
                let u = *chroma.add(cbase) as i32;
                let v = *chroma.add(cbase + 1) as i32;
                // BT.601 limited 逆変換 (8bit 固定小数)。
                let c = y - 16;
                let d = u - 128;
                let e = v - 128;
                let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255);
                let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255);
                let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255);
                let o = (j * w + i) * 4;
                bgra[o] = b as u8;
                bgra[o + 1] = g as u8;
                bgra[o + 2] = r as u8;
                bgra[o + 3] = 255;
            }
        }
        device.context.Unmap(&staging, 0);
    }
    Ok(bgra)
}
