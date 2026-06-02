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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIOutput,
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use super::{DisplayInfo, Frame, ScreenCapturer};

pub struct DxgiCapturer {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    staging_format: DXGI_FORMAT,
    /// HDR(scRGB FP16) → SDR sRGB8 変換テーブル。このディスプレイの SDR white level で正規化済み。
    scrgb_lut: Box<[u8; 65536]>,
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
        // SAFETY: output live。GetDesc は値返し。GDI デバイス名 (\\.\DISPLAYn) を得る。
        let out_desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
        let gdi_name = String::from_utf16_lossy(&out_desc.DeviceName)
            .trim_end_matches('\0')
            .to_string();

        // このディスプレイの HDR 有効状態と SDR white level を取得する。
        let (hdr_active, scale) = hdr_info(&gdi_name);
        tracing::info!(display = %gdi_name, hdr_active, sdr_white_scale = scale, "capture color");

        // HDR 有効時のみ FP16(scRGB) 取得 + トーンマップする。HDR デスクトップは MPO により
        // 通常の DuplicateOutput だと FP16/BGRA8 が混在するため、DuplicateOutput1 で取得
        // フォーマットを scRGB に固定して混在を消す。SDR ディスプレイは従来の BGRA8 素通し
        // (DuplicateOutput) のままにする (変換不要で実績がある経路)。
        let duplication: IDXGIOutputDuplication = if hdr_active {
            // DuplicateOutput1 は DPI-aware プロセスでないと E_ACCESSDENIED になる。
            // SAFETY: プロセス全体の設定。既に設定済みなら失敗するだけで無害。
            unsafe {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            match output.cast::<IDXGIOutput5>() {
                Ok(output5) => match unsafe {
                    output5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT])
                } {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "DuplicateOutput1(FP16) failed, falling back");
                        let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
                        unsafe { output1.DuplicateOutput(&device) }.context("DuplicateOutput")?
                    }
                },
                Err(_) => {
                    let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
                    unsafe { output1.DuplicateOutput(&device) }.context("DuplicateOutput")?
                }
            }
        } else {
            let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
            unsafe { output1.DuplicateOutput(&device) }.context("DuplicateOutput")?
        };

        // SAFETY: duplication live。GetDesc は値返し。
        let desc = unsafe { duplication.GetDesc() };
        let width = desc.ModeDesc.Width;
        let height = desc.ModeDesc.Height;

        Ok(Self {
            _device: device,
            context,
            duplication,
            staging: None,
            staging_format: DXGI_FORMAT_B8G8R8A8_UNORM,
            scrgb_lut: build_scrgb_lut(scale),
            width,
            height,
        })
    }

    /// ソーステクスチャと同じフォーマット・サイズの staging を用意する。
    /// `CopyResource` はフォーマット一致が前提なので、HDR (FP16) ソースには FP16 staging を作る。
    fn ensure_staging(
        &mut self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<&ID3D11Texture2D> {
        let need_new = match &self.staging {
            Some(_) => {
                self.width != width || self.height != height || self.staging_format != format
            }
            None => true,
        };
        if need_new {
            tracing::info!(
                format = format.0,
                width,
                height,
                "capture source format (10=R16G16B16A16F scRGB/HDR, 87=B8G8R8A8 SDR)"
            );
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
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
            self.staging_format = format;
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

        let width = src_desc.Width;
        let height = src_desc.Height;
        let src_format = src_desc.Format;

        let staging = self.ensure_staging(width, height, src_format)?.clone();

        // SAFETY: 同 device・同フォーマット・同サイズの 2 texture 間で CopyResource。
        unsafe { self.context.CopyResource(&staging, &src_tex) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging は D3D11_USAGE_STAGING + CPU_ACCESS_READ。Map は安全に呼べる。
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(staging)")?;
        }

        let src_pitch = mapped.RowPitch as usize;
        // 出力は常に行間隔ぴったりの BGRA8。HDR(FP16) ソースは scRGB→sRGB トーンマップする。
        let dst_stride = width as usize * 4;
        let mut bgra = vec![0u8; dst_stride * height as usize];

        let convert_result: Result<()> = match src_format {
            DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => {
                // SAFETY: mapped.pData は各行 src_pitch バイト読める。BGRA8 をそのままコピー。
                unsafe {
                    let base = mapped.pData as *const u8;
                    for y in 0..height as usize {
                        std::ptr::copy_nonoverlapping(
                            base.add(y * src_pitch),
                            bgra.as_mut_ptr().add(y * dst_stride),
                            dst_stride,
                        );
                    }
                }
                Ok(())
            }
            DXGI_FORMAT_R16G16B16A16_FLOAT => {
                let lut = &self.scrgb_lut;
                // SAFETY: mapped.pData は各行 src_pitch バイト読める。RGBA 各 16bit half。
                unsafe {
                    let base = mapped.pData as *const u8;
                    for y in 0..height as usize {
                        let row = base.add(y * src_pitch) as *const u16;
                        let dst_row = y * dst_stride;
                        for x in 0..width as usize {
                            let r = lut[*row.add(x * 4) as usize];
                            let g = lut[*row.add(x * 4 + 1) as usize];
                            let b = lut[*row.add(x * 4 + 2) as usize];
                            let o = dst_row + x * 4;
                            bgra[o] = b;
                            bgra[o + 1] = g;
                            bgra[o + 2] = r;
                            bgra[o + 3] = 255;
                        }
                    }
                }
                Ok(())
            }
            other => Err(anyhow!(
                "unsupported capture format {:?} (0x{:x})",
                other,
                other.0
            )),
        };

        // SAFETY: Map と対になる Unmap。変換成否に関わらず必ず解放する。
        unsafe { self.context.Unmap(&staging, 0) };
        // SAFETY: AcquireNextFrame と対になる。
        let _ = unsafe { self.duplication.ReleaseFrame() };

        convert_result?;

        Ok(Some(Frame {
            width,
            height,
            stride: dst_stride as u32,
            bgra,
            captured_at: Instant::now(),
        }))
    }
}

/// IEEE 754 half (16bit) を f32 へ変換する (ビット演算)。
fn half_to_f32(h: u16) -> f32 {
    let h = h as u32;
    let sign = (h & 0x8000) << 16;
    let exp = (h & 0x7c00) >> 10;
    let mant = h & 0x03ff;
    let bits = if exp == 0 {
        if mant == 0 {
            sign // ±0
        } else {
            // 非正規化数を正規化する
            let mut e: i32 = -1;
            let mut m = mant;
            loop {
                e += 1;
                m <<= 1;
                if m & 0x0400 != 0 {
                    break;
                }
            }
            let mant = (m & 0x03ff) << 13;
            let exp = ((127 - 15 - e) as u32) << 23;
            sign | exp | mant
        }
    } else if exp == 0x1f {
        // Inf / NaN
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        // 正規化数 (half バイアス 15 → f32 バイアス 127)
        sign | ((exp + 112) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// リニア値 (sRGB プライマリ) を sRGB OETF で符号化する。
fn srgb_encode(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// scRGB half 値 → SDR sRGB 8bit の変換テーブルを `scale` (SDR white level / 80nit) 込みで作る。
/// scRGB はリニア光で (1,1,1)=80nit。HDR デスクトップは SDR 白を `scale*80`nit で合成するため、
/// `scrgb/scale` で正規化 → [0,1] ハードクランプ (HDR ハイライトはクリップ) → sRGB 符号化する。
fn build_scrgb_lut(scale: f32) -> Box<[u8; 65536]> {
    let inv = 1.0 / scale.max(f32::MIN_POSITIVE);
    let mut t = Box::new([0u8; 65536]);
    for (i, slot) in t.iter_mut().enumerate() {
        let lin = (half_to_f32(i as u16) * inv).clamp(0.0, 1.0);
        *slot = (srgb_encode(lin) * 255.0 + 0.5) as u8;
    }
    t
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
