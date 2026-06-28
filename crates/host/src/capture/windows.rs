//! DXGI Desktop Duplication ベースのディスプレイキャプチャ (ゼロコピー版)。
//!
//! acquire → CopyResource(shader-readable) → GPU 色変換 → NV12 テクスチャ、で完結する。
//! CPU での LUT トーンマップと staging リードバックは廃し、色変換は `convert::Converter`
//! (D3D11 シェーダ) が GPU 上で行う。デバイスは `DeviceBundle` を capture・encode で共有。

use std::ffi::c_void;
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
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_NV12, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutput6,
    IDXGIOutputDuplication, IDXGIResource,
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
    /// GetFramePointerShape 用の再利用バッファ (形状更新フレームのみ書き込む)。
    pointer_buf: Vec<u8>,
    /// 取得テクスチャの実フォーマットを初回 1 回だけログするための既出フラグ (診断用)。
    logged_src_format: bool,
}

/// 現在の入力デスクトップが既定の "Default" (通常のユーザーデスクトップ) かを返す。
///
/// ロック画面や UAC のセキュアデスクトップ中は `OpenInputDesktop` 自体が失敗するか、
/// 名前が "Winlogon" になるため false を返す。判定不能 (名前取得失敗) は true
/// (= 従来挙動: 固着時間に数える) へ安全側に倒す。
///
/// capture_loop の re-exec 判定に使う: ロック/UAC 中の ACCESS_LOST はデスクトップが
/// 戻れば復帰する正常動作であり、「プロセス再起動が必要な固着」として数えてはならない。
pub fn input_desktop_is_default() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    };

    // SAFETY: 入力デスクトップを読み取り最小権限で開くだけ。失敗 = セキュアデスクトップ等で
    // 開けない状態なので false (Default ではない) と判定できる。
    let desk =
        match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
            Ok(d) => d,
            Err(_) => return false,
        };
    let mut name = [0u16; 64];
    let mut needed = 0u32;
    // SAFETY: desk は直前に取得した有効ハンドル。バッファ長 (バイト) を渡し UTF-16 名を受ける。
    let got = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    // SAFETY: 上で開いたデスクトップハンドルを閉じる (リーク防止)。
    let _ = unsafe { CloseDesktop(desk) };
    match got {
        Ok(()) => {
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("default")
        }
        Err(_) => true,
    }
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

/// GDI 名 (`\\.\DISPLAYn`) に一致する DXGI 出力を、**毎回新しいファクトリ**で全アダプタ×出力を
/// 走査して返す。出力名はシステム全体で一意なので名前一致で特定でき、トポロジ変更直後の
/// 古い index に依存しない。見つからない (= デタッチ中) なら Err。
/// 対象ディスプレイ (`\\.\DISPLAYn`) の Desktop Duplication が**実際に成功する**アダプタを選ぶ。
///
/// ハイブリッド GPU (NVIDIA Optimus / AMD 切替) のノートでは、内蔵パネルの描画・スキャンアウトを
/// iGPU が担い、dGPU は同じ出力を**列挙はする**が複製しようとすると `DXGI_ERROR_UNSUPPORTED`
/// (0x887A0004) を返す。逆に外部出力が dGPU 直結のこともある。どちらが正解かは環境依存なので
/// 「出力を所有していそうなアダプタ」を名前一致で当てる方式 (旧 `adapter_for_output`) は外す。
/// ここでは候補アダプタ上に試験デバイスを作って実際に `DuplicateOutput` を叩き、`UNSUPPORTED`
/// でないものを選ぶ。これでハイブリッド GPU でも「画面真っ暗」(複製恒常失敗) を回避する。
///
/// 戻り値が `None` のときは候補が見つからない/全滅で、呼び出し側が既定アダプタにフォールバックする。
pub fn select_capture_adapter(target: &str) -> Option<IDXGIAdapter> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
    // SAFETY: CreateDXGIFactory1 は OUT パラメータを初期化する。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    // 複製が UNSUPPORTED ではなかった (= 正解の見込みがある) 最初のアダプタ。Ok が無いとき採用。
    let mut fallback: Option<IDXGIAdapter> = None;
    let mut ai = 0u32;
    loop {
        // SAFETY: EnumAdapters1 は範囲外で NOT_FOUND を返す。
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let idx = ai;
        ai += 1;
        let adapter_name = unsafe { adapter.GetDesc1() }
            .map(|d| {
                String::from_utf16_lossy(&d.Description)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_default();
        let mut oi = 0u32;
        loop {
            // SAFETY: EnumOutputs は範囲外で NOT_FOUND を返す。
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(_) => break,
            };
            oi += 1;
            // SAFETY: output live。GetDesc は値返し。
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            let name = String::from_utf16_lossy(&desc.DeviceName)
                .trim_end_matches('\0')
                .to_string();
            if name != target {
                continue;
            }
            let Ok(adp) = adapter.cast::<IDXGIAdapter>() else {
                continue;
            };
            match probe_duplication(&adp, &output) {
                DuplProbe::Ok => {
                    tracing::info!(
                        adapter_index = idx,
                        gpu = %adapter_name,
                        "capture adapter selected (DuplicateOutput probe ok)"
                    );
                    return Some(adp);
                }
                DuplProbe::Unsupported => {
                    tracing::warn!(
                        adapter_index = idx,
                        gpu = %adapter_name,
                        "adapter lists display but DuplicateOutput unsupported; trying next"
                    );
                }
                DuplProbe::OtherErr(e) => {
                    // UNSUPPORTED 以外 (一過性の NOT_CURRENTLY_AVAILABLE 等) は正解アダプタの
                    // 見込みが高い。即決はせず候補として保持し、Ok が他に無ければ採用する。
                    tracing::info!(
                        adapter_index = idx,
                        gpu = %adapter_name,
                        error = %e,
                        "DuplicateOutput probe non-fatal; keeping adapter as fallback"
                    );
                    if fallback.is_none() {
                        fallback = Some(adp);
                    }
                }
            }
        }
    }
    fallback
}

enum DuplProbe {
    Ok,
    Unsupported,
    OtherErr(windows::core::Error),
}

/// `adapter` 上に試験 D3D11 デバイスを作り、`output` の Desktop Duplication を一度試す。
/// 結果の duplication は即破棄する (本番の capturer が後で作り直す)。
fn probe_duplication(adapter: &IDXGIAdapter, output: &IDXGIOutput) -> DuplProbe {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED;

    let feature_levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1];
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: adapter 指定時は DriverType=UNKNOWN 必須。OUT パラメータ device を初期化する。
    let hr = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    };
    if let Err(e) = hr {
        return DuplProbe::OtherErr(e);
    }
    let Some(device) = device else {
        return DuplProbe::OtherErr(windows::core::Error::from(
            windows::Win32::Foundation::E_FAIL,
        ));
    };
    let output1: IDXGIOutput1 = match output.cast() {
        Ok(o) => o,
        Err(e) => return DuplProbe::OtherErr(e),
    };
    // SAFETY: device・output1 は live。成功時の duplication は即 Drop で解放される。
    match unsafe { output1.DuplicateOutput(&device) } {
        Ok(_dup) => DuplProbe::Ok,
        Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => DuplProbe::Unsupported,
        Err(e) => DuplProbe::OtherErr(e),
    }
}

fn find_output_by_name(target: &str) -> Result<IDXGIOutput> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
    // SAFETY: CreateDXGIFactory1 は OUT パラメータ (factory) を初期化する。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    // 見つからなかったときに「いま実際に存在する出力名」を診断できるよう収集する。
    // テイクオーバーのリサイズ後に GDI 名が振り直されて cur_name が stale 化する仮説の判定に使う。
    let mut available: Vec<String> = Vec::new();
    let mut ai = 0u32;
    loop {
        // SAFETY: EnumAdapters1 は範囲外で NOT_FOUND を返す。
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => break,
        };
        ai += 1;
        let mut oi = 0u32;
        loop {
            // SAFETY: EnumOutputs は範囲外で NOT_FOUND を返す。
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(_) => break,
            };
            oi += 1;
            // SAFETY: output live。GetDesc は値返し。
            let desc = unsafe { output.GetDesc() }.context("output GetDesc")?;
            let name = String::from_utf16_lossy(&desc.DeviceName)
                .trim_end_matches('\0')
                .to_string();
            if name == target {
                return Ok(output);
            }
            available.push(name);
        }
    }
    Err(anyhow!(
        "no DXGI output named {target} (detached?); available: [{}]",
        available.join(", ")
    ))
}

/// 全 DXGI アダプタ × 全出力を列挙して人間可読な行にする (`--list-displays` 診断用)。
///
/// 仮想ディスプレイ (IDD / VDD) は **別アダプタ** (例: "IddSampleDriver Device" や
/// Indirect Display 用の仮想アダプタ) に出ることが多い。本アプリの [`enumerate_displays`] は
/// 単一アダプタ (enum デバイスの GetAdapter) の出力しか見ないため、仮想ディスプレイを
/// 捕捉対象にできるか (= どのアダプタ/output index に出るか・desktop に attach されるか) を
/// 実機で確認するための診断。Desktop Duplication は「そのアダプタで作った D3D11 デバイス」が
/// 要るため、仮想ディスプレイが別アダプタなら capture 側もそのアダプタで device を作る必要がある。
pub fn debug_list_outputs() -> Result<Vec<String>> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};

    // GDI デバイス名 (`\\.\DISPLAYn`) が提供する解像度モード (distinct な w×h) を列挙する。
    // ChangeDisplaySettingsEx で設定できる候補 = ここに出る解像度。仮想ディスプレイが
    // クライアント実寸に近い任意サイズを提供するか (= display.rs で解像度を合わせられるか) を見る。
    let mode_summary = |gdi_name: &str| -> String {
        use windows::Win32::Graphics::Gdi::{
            DEVMODEW, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
        };
        use windows::core::PCWSTR;
        let wide: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut set: std::collections::BTreeSet<(u32, u32)> = std::collections::BTreeSet::new();
        let mut i = 0u32;
        loop {
            let mut dm = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: wide は NUL 終端の有効な GDI 名。dm は dmSize 設定済みで OUT を埋める。
            let ok = unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(wide.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(i),
                    &mut dm,
                )
            };
            if !ok.as_bool() {
                break;
            }
            set.insert((dm.dmPelsWidth, dm.dmPelsHeight));
            i += 1;
        }
        let list: Vec<String> = set.iter().map(|(w, h)| format!("{w}x{h}")).collect();
        format!("    modes({}): {}", list.len(), list.join(", "))
    };

    // SAFETY: CreateDXGIFactory1 は OUT パラメータ (factory) を初期化する。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut lines = Vec::new();
    let mut ai = 0u32;
    loop {
        // SAFETY: EnumAdapters1 は範囲外で DXGI_ERROR_NOT_FOUND を返す。
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => break,
        };
        // SAFETY: adapter live。GetDesc1 は値返し。
        let adesc = unsafe { adapter.GetDesc1() }.context("adapter GetDesc1")?;
        let aname = String::from_utf16_lossy(&adesc.Description)
            .trim_end_matches('\0')
            .to_string();

        let mut oi = 0u32;
        let mut any = false;
        loop {
            // SAFETY: EnumOutputs は範囲外で NOT_FOUND を返す。
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(_) => break,
            };
            any = true;
            // SAFETY: output live。GetDesc は値返し。
            let odesc = unsafe { output.GetDesc() }.context("output GetDesc")?;
            let r = odesc.DesktopCoordinates;
            let oname = String::from_utf16_lossy(&odesc.DeviceName)
                .trim_end_matches('\0')
                .to_string();
            lines.push(format!(
                "adapter[{ai}] \"{aname}\"  output[{oi}] {oname}  {}x{} @ ({},{})  attached_to_desktop={}",
                r.right - r.left,
                r.bottom - r.top,
                r.left,
                r.top,
                odesc.AttachedToDesktop.as_bool(),
            ));
            lines.push(mode_summary(&oname));
            oi += 1;
        }
        if !any {
            lines.push(format!("adapter[{ai}] \"{aname}\"  (no outputs)"));
        }
        ai += 1;
    }
    if lines.is_empty() {
        lines.push("(no DXGI adapters/outputs found)".to_string());
    }
    Ok(lines)
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
        Self::from_output(device, output)
    }

    /// GDI 名 (`\\.\DISPLAYn`) で DXGI 出力を選びキャプチャする。
    ///
    /// インデックスではなく**新しい DXGI ファクトリで出力を引き直す**ため、テイクオーバー等で
    /// ディスプレイのアタッチ/デタッチが起きて出力レイアウト (index) が変わった直後でも、
    /// 正しい出力を捕捉できる。共有デバイスの GetAdapter はキャッシュされ古いレイアウトを返す
    /// ことがあり (= EnumOutputs(n) が "no such display" になる)、その回避が目的。複製
    /// (DuplicateOutput) は共有デバイスのまま行う (同一物理アダプタなので有効)。
    pub fn new_for_name(device: SharedDevice, target_name: &str) -> Result<Self> {
        let output = find_output_by_name(target_name)?;
        Self::from_output(device, output)
    }

    fn from_output(device: SharedDevice, output: IDXGIOutput) -> Result<Self> {
        let d3d = &device.device;
        // SAFETY: output live。GetDesc は値返し。GDI デバイス名 (\\.\DISPLAYn) を得る。
        let out_desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
        let gdi_name = String::from_utf16_lossy(&out_desc.DeviceName)
            .trim_end_matches('\0')
            .to_string();

        // このディスプレイの advancedColorEnabled と SDR white level を取得する。
        let (adv_color_enabled, scale) = hdr_info(&gdi_name);
        // 本物の HDR 判定はデスクトップ合成の色空間を優先する。advancedColorEnabled は
        // Windows 11 の Automatic Color Management 対応モニタで HDR をオフにしても true の
        // まま残り、ACM-SDR を HDR と誤検知して常時 FP16/トーンマップ経路に入れてしまう。
        // 色空間が取れる場合はそれ (= G2084 のときだけ HDR) を採用し、取れない古い環境のみ
        // advancedColorEnabled にフォールバックする。
        let color_space_hdr = output_color_space_is_hdr(&output);
        let hdr_active = color_space_hdr.unwrap_or(adv_color_enabled);
        // ACM-SDR: ディスプレイ HDR はオフだが Automatic Color Management (Advanced Color) が
        // オンの状態。デスクトップは linear scRGB で合成され、legacy DuplicateOutput が渡す
        // B8G8R8A8_UNORM 面は linear 値を保持する (sRGB 符号化ではない)。SDR 素通し経路は
        // 「sRGB 済み」前提で sRGB デコードせずサンプルする (map_formats) ため、linear を gamma
        // と誤解して中間調が暗く沈む。OETF を適用する経路 (is_hdr 経路) に scale=1.0 で乗せて
        // 補正する。取得形式は FP16 化せず BGRA8 のまま (DuplicateOutput) でよい。
        let acm_sdr = !hdr_active && adv_color_enabled;
        let needs_oetf = hdr_active || acm_sdr;
        let oetf_scale = if hdr_active { scale } else { 1.0 };
        tracing::info!(
            display = %gdi_name,
            hdr_active,
            acm_sdr,
            advanced_color_enabled = adv_color_enabled,
            color_space_hdr = ?color_space_hdr,
            sdr_white_scale = scale,
            "capture color"
        );

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

        let converter = Converter::new(&device, needs_oetf, oetf_scale)?;

        Ok(Self {
            _device: device,
            duplication,
            converter,
            pointer_buf: Vec::new(),
            logged_src_format: false,
        })
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn set_target_height(&mut self, max_height: u32) {
        self.converter.set_target_height(max_height);
    }

    fn set_cursor_overlay(&mut self, enabled: bool) {
        self.converter.set_cursor_overlay(enabled);
    }

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

        // カーソルを取得して converter へ渡す (DXGI はカーソルを映像に含めないため別取得)。
        // 形状が更新されたフレームだけ取り直し、位置はマウスが動いたフレームのみ更新する。
        if info.PointerShapeBufferSize > 0 {
            let needed = info.PointerShapeBufferSize as usize;
            if self.pointer_buf.len() < needed {
                self.pointer_buf.resize(needed, 0);
            }
            let mut required = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: pointer_buf は needed バイト確保済み。OUT パラメータを埋める。
            // AcquireNextFrame〜ReleaseFrame の間でのみ有効 (convert より前のここで呼ぶ)。
            let got = unsafe {
                self.duplication.GetFramePointerShape(
                    info.PointerShapeBufferSize,
                    self.pointer_buf.as_mut_ptr() as *mut c_void,
                    &mut required,
                    &mut shape,
                )
            };
            if got.is_ok() {
                let _ = self.converter.update_cursor_shape(
                    shape.Type,
                    shape.Width,
                    shape.Height,
                    shape.Pitch,
                    &self.pointer_buf[..required as usize],
                );
            }
        }
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            self.converter
                .set_cursor_pos(p.Position.x, p.Position.y, p.Visible.as_bool());
        }

        let resource =
            resource.ok_or_else(|| anyhow!("AcquireNextFrame returned null resource"))?;
        let src_tex: ID3D11Texture2D = resource.cast().context("cast ID3D11Texture2D")?;

        // 診断: 取得テクスチャの実フォーマットを初回 1 回だけ出す。HDR オフ時に ACM が
        // FP16(scRGB linear) を渡すか BGRA8 を渡すかを確かめ、SDR 経路で OETF が要るか判定する。
        if !self.logged_src_format {
            let mut sdesc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
            // SAFETY: src_tex live。GetDesc は OUT を埋める。
            unsafe { src_tex.GetDesc(&mut sdesc) };
            tracing::info!(src_format = ?sdesc.Format, "capture src texture format");
            self.logged_src_format = true;
        }

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

/// DXGI 出力の現在のデスクトップ合成色空間が本物の HDR (PQ/scRGB) かを返す。
///
/// `IDXGIOutput6::GetDesc1` の `ColorSpace` が `RGB_FULL_G2084_NONE_P2020` のときだけ
/// 本物の HDR (FP16 取得 + トーンマップが要る) と判定する。`advancedColorEnabled`
/// (DisplayConfig) は Windows 11 の Automatic Color Management 対応モニタで HDR をオフに
/// しても true のまま残り ACM-SDR を誤検知するため、色空間という確実な信号で上書きする。
/// `IDXGIOutput6` が無い古い環境では `None` を返し、呼び出し側が従来判定にフォールバックする。
fn output_color_space_is_hdr(output: &IDXGIOutput) -> Option<bool> {
    let output6: IDXGIOutput6 = output.cast().ok()?;
    // SAFETY: output6 は live。GetDesc1 は DXGI_OUTPUT_DESC1 を値で返す。
    let desc = unsafe { output6.GetDesc1() }.ok()?;
    Some(desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

/// 指定 GDI デバイス名 (`\\.\DISPLAYn`) の (advancedColorEnabled, scRGB 正規化係数) を返す。
/// 係数 = SDRWhiteLevel/1000 (= white_nits/80)。取得失敗時は `(false, 1.0)`。
/// `advancedColorEnabled` だけでは ACM-SDR を誤検知するため、HDR の最終判定は呼び出し側で
/// [`output_color_space_is_hdr`] (色空間) を優先する。本関数は white level (scale) の取得と、
/// 色空間が取れない古い環境向けのフォールバック信号の供給を担う。
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
