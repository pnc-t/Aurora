//! 仮想ディスプレイ (VirtualDrivers / MttVDD, IddCx) の検出と制御。
//!
//! RDP 風に「物理画面を触らず、クライアントの表示領域ぴったりの仮想ディスプレイ」をキャプチャ
//! するための土台。実機調査 (2026-06-17) で判明した事実:
//! - 仮想ディスプレイは物理と同じ GPU アダプタ (AMD) の DXGI output として出る
//!   (別アダプタではない) → 既存の共有 D3D11 デバイスでそのまま Desktop Duplication 可能。
//! - 仮想モニタのハードウェア ID は `MTT1337` (FriendlyName "Generic Monitor (VDD by MTT)")。
//!   これで仮想ディスプレイの GDI 名 (`\\.\DISPLAYn`) を確実に識別できる。
//! - 提供解像度は `C:\VirtualDisplayDriver\vdd_settings.xml` の `<resolutions>` で定義し、
//!   ドライバ再起動 (instance `ROOT\DISPLAY\0000`) で反映する。
//!
//! Windows のみ実体があり、他 OS は常に「未検出」。

/// 仮想モニタのハードウェア ID 署名 (DeviceID に含まれる)。VirtualDrivers/MttVDD 固有。
#[cfg(target_os = "windows")]
const VDD_MONITOR_HWID: &str = "MTT1337";

/// 検出した仮想ディスプレイの情報。
#[derive(Debug, Clone)]
pub struct VddInfo {
    /// GDI デバイス名 (`\\.\DISPLAYn`)。DXGI 出力名と一致するためキャプチャ対象の特定に使う。
    pub gdi_name: String,
    /// 現在 (またはレジストリ既定) の解像度。デスクトップにアタッチされていないと DXGI
    /// 出力に出ないため、HostInfo へ合成エントリを出すときの初期サイズに使う。
    pub width: u32,
    pub height: u32,
}

/// 接続中の仮想ディスプレイ (MttVDD) を検出し、その GDI 名を返す。未検出なら `None`。
#[cfg(target_os = "windows")]
pub fn detect() -> Option<VddInfo> {
    use windows::Win32::Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW};
    use windows::core::PCWSTR;

    fn wstr(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    let mut i = 0u32;
    loop {
        let mut adapter = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: 第 1 引数 NULL でアダプタ (\\.\DISPLAYn) を順に列挙する。範囲外で FALSE。
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut adapter, 0) };
        if !ok.as_bool() {
            break;
        }
        i += 1;

        let adapter_name = wstr(&adapter.DeviceName); // \\.\DISPLAYn
        let aw: Vec<u16> = adapter_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // このアダプタにぶら下がるモニタ (子) を見て、MttVDD かどうか判定する。
        let mut mon = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: aw は NUL 終端のアダプタ名。モニタ 0 番の情報を取る。
        let okm = unsafe { EnumDisplayDevicesW(PCWSTR(aw.as_ptr()), 0, &mut mon, 0) };
        if !okm.as_bool() {
            continue;
        }
        let dev_id = wstr(&mon.DeviceID);
        let dev_str = wstr(&mon.DeviceString);
        if dev_id.contains(VDD_MONITOR_HWID) || dev_str.contains("VDD by MTT") {
            let (width, height) = match current_resolution(&aw) {
                Some(wh) => wh,
                None => {
                    // 解像度が読めない VDD は 1080p 仮置き。これが実クライアントと食い違うと
                    // 「解像度が合わない」症状の元になるため warn で残す (実機切り分け用)。
                    tracing::warn!(
                        name = %adapter_name,
                        "VDD resolution unreadable (CURRENT/REGISTRY both failed); using 1080p fallback"
                    );
                    (1920, 1080)
                }
            };
            tracing::debug!(name = %adapter_name, width, height, dev = %dev_str, "VDD detected");
            return Some(VddInfo {
                gdi_name: adapter_name,
                width,
                height,
            });
        }
    }
    None
}

/// GDI 名 (NUL 終端の wide) の現在解像度を返す。デスクトップに非アタッチでも
/// レジストリ既定モードで埋まるよう、CURRENT → REGISTRY の順にフォールバックする。
#[cfg(target_os = "windows")]
fn current_resolution(adapter_wide: &[u16]) -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, ENUM_REGISTRY_SETTINGS, EnumDisplaySettingsW,
    };
    use windows::core::PCWSTR;

    for mode in [ENUM_CURRENT_SETTINGS, ENUM_REGISTRY_SETTINGS] {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: adapter_wide は NUL 終端の有効な GDI 名。dm は dmSize 設定済みで OUT を埋める。
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR(adapter_wide.as_ptr()), mode, &mut dm) };
        if ok.as_bool() && dm.dmPelsWidth >= 2 && dm.dmPelsHeight >= 2 {
            return Some((dm.dmPelsWidth, dm.dmPelsHeight));
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub fn detect() -> Option<VddInfo> {
    None
}

// --- 解像度モードの補完 + ドライバ reload (RDP 風の解像度合わせの土台) ---
//
// MttVDD は提供解像度を `C:\VirtualDisplayDriver\vdd_settings.xml` の `<resolutions>` からのみ
// 読む (named pipe に解像度設定コマンドは無い)。そこで起動時に一度だけ、よくあるクライアント
// 解像度を XML に追記して `RELOAD_DRIVER` で反映する。以後はその豊富なモード一覧から
// `ChangeDisplaySettingsEx` (display.rs) が最近傍を選ぶため、標準解像度では実質ピクセル一致になり、
// 接続中の reload (チラつき・output index 変動) を避けられる。書込みは冪等 (既にあれば何もしない)。

/// VDD 設定 XML の既定パス (Control アプリのインストール先)。
#[cfg(target_os = "windows")]
const VDD_XML_PATH: &str = r"C:\VirtualDisplayDriver\vdd_settings.xml";

/// 追記するよくあるクライアント解像度 (物理 px・偶数)。16:9 / 16:10 / 3:2 / 21:9 / 4:3 / 5:4 と
/// ノート PC の実解像度を広めに網羅し、最近傍が実質一致になるようにする。
///
/// **同梱インストーラのリッチ XML (`installer/host/vdd_settings.rich.xml`) と同期させること。**
/// インストーラ経由なら新規デバイス登録時にこの一覧が読まれる。この実行時補完
/// (`ensure_common_resolutions`) は手動 (`--vdd-enrich`) 用のフォールバックで、反映には別途
/// 管理者デバイス再起動が要る (XML 追記だけでは EDID は再生成されない)。
#[cfg(target_os = "windows")]
const COMMON_RESOLUTIONS: &[(u32, u32)] = &[
    // 小型 / 互換
    (800, 600),
    (1024, 600),
    // 4:3
    (1024, 768),
    (1600, 1200),
    (2048, 1536),
    // 5:4
    (1280, 1024),
    // 16:9
    (1280, 720),
    (1366, 768),
    (1536, 864),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3200, 1800),
    (3840, 2160),
    // 16:10
    (1280, 800),
    (1440, 900),
    (1680, 1050),
    (1920, 1200),
    (2304, 1440),
    (2560, 1600),
    (2880, 1800),
    (3840, 2400),
    // 3:2
    (1920, 1280),
    (2160, 1440),
    (2256, 1504),
    (2496, 1664),
    (3000, 2000),
    (3240, 2160),
    // 21:9 ウルトラワイド
    (2560, 1080),
    (3440, 1440),
    (3840, 1600),
    // ノート PC 実解像度
    (2048, 1280),
    (2240, 1400),
    (3456, 2160),
];

/// `vdd_settings.xml` の `<resolutions>` に未登録の [`COMMON_RESOLUTIONS`] を追記する。
/// 1 つでも追記したら `Ok(true)` (= reload が必要)。既に揃っていれば `Ok(false)`。
/// 書込み不可 (権限など) はエラーで返し、呼び出し側は best-effort で諦める。
#[cfg(target_os = "windows")]
pub fn ensure_common_resolutions() -> anyhow::Result<bool> {
    use anyhow::{Context, anyhow};

    let xml =
        std::fs::read_to_string(VDD_XML_PATH).with_context(|| format!("read {VDD_XML_PATH}"))?;
    let missing: Vec<(u32, u32)> = COMMON_RESOLUTIONS
        .iter()
        .copied()
        .filter(|&(w, h)| !xml_has_resolution(&xml, w, h))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    let mut blocks = String::new();
    for (w, h) in missing {
        blocks.push_str(&format!(
            "        <resolution>\n            <width>{w}</width>\n            <height>{h}</height>\n            <refresh_rate>60</refresh_rate>\n        </resolution>\n",
        ));
    }
    // `</resolutions>` の直前へ挿入する (簡易: 厳密パースは避ける)。
    let idx = xml
        .find("</resolutions>")
        .ok_or_else(|| anyhow!("no </resolutions> element in {VDD_XML_PATH}"))?;
    let mut out = String::with_capacity(xml.len() + blocks.len());
    out.push_str(&xml[..idx]);
    out.push_str(&blocks);
    out.push_str(&xml[idx..]);
    std::fs::write(VDD_XML_PATH, out).with_context(|| format!("write {VDD_XML_PATH}"))?;
    Ok(true)
}

/// `<resolution>` ブロックを走査して (w,h) が既にあるか判定する。
#[cfg(target_os = "windows")]
fn xml_has_resolution(xml: &str, w: u32, h: u32) -> bool {
    fn tag(s: &str, name: &str) -> Option<u32> {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let a = s.find(&open)? + open.len();
        let b = s[a..].find(&close)? + a;
        s[a..b].trim().parse().ok()
    }
    xml.split("<resolution>")
        .skip(1)
        .any(|block| tag(block, "width") == Some(w) && tag(block, "height") == Some(h))
}

// 注: ドライバの named pipe (`\\.\pipe\MTTVirtualDisplayPipe`) には `RELOAD_DRIVER` /
// `SETDISPLAYCOUNT` 等のコマンドがあるが、**解像度モード一覧 (EDID) を再生成しない**ことを
// 実機確認済み (RELOAD_DRIVER は仮想ディスプレイを非アクティブに落とすだけ)。新しい解像度の
// 反映にはデバイス再起動 (= 管理者権限、`pnputil /restart-device ROOT\DISPLAY\0000` や Control
// アプリの "Restart Driver") が要る。よって解像度の充実はインストール時 (管理者) に行い、
// 実行時のホスト (ユーザー権限) からは pipe reload を呼ばない。

#[cfg(not(target_os = "windows"))]
pub fn ensure_common_resolutions() -> anyhow::Result<bool> {
    Ok(false)
}
