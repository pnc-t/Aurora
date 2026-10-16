//! ディスプレイ解像度の動的変更 (RDP 風)。
//!
//! クライアントの映像表示領域サイズに合わせて、キャプチャ対象ディスプレイの物理解像度を
//! 最も近い対応モードへ変更する。黒帯を無くし 1:1 表示に近づけるのが狙い。切断時には
//! 記憶しておいた元モードへ戻す。Windows のみ実体があり、他 OS は no-op。
//!
//! 解像度変更は DXGI Desktop Duplication を ACCESS_LOST にするが、capture_loop が
//! capturer を作り直して新解像度で復帰するため、ここでは ChangeDisplaySettings だけを担う。
//! Win32 呼び出しは数十〜数百 ms ブロックしうるので `tokio::task::spawn_blocking` 上で実行する。
//!
//! 物理ディスプレイの解像度は**変更しない**方針 (はみ出しはクライアント側 object-fit で吸収)。
//! 解像度の動的変更は仮想ディスプレイのテイクオーバー中のみ (`set_virtual_resolution`)。

// 仮想ディスプレイ (MttVDD / IddCx) の検出と制御。解像度テイクオーバーの土台であり
// display の関心事なので配下に置く (旧 `crate::virtualdisplay` → `crate::display`)。
pub mod virtual_display;
pub use virtual_display::{
    detect, ensure_common_resolutions, ensure_vdd_render_gpu, process_is_elevated,
    restart_vdd_device,
};

/// 診断: 指定ディスプレイ (`\\.\DISPLAYn`) で解像度変更→復元を実測する (`--vdd-test-restore`)。
/// 「切断後に解像度/fps が戻らない」原因切り分け用。安全な仮想ディスプレイ等で使う。
#[cfg(target_os = "windows")]
pub fn debug_test_cycle(device: &str) -> Vec<String> {
    imp::debug_test_cycle(device)
}
#[cfg(not(target_os = "windows"))]
pub fn debug_test_cycle(_device: &str) -> Vec<String> {
    vec!["display mode test is Windows-only".to_string()]
}

// --- テイクオーバー公開 API (RDP 風: 仮想を主にし物理をデタッチ) ---

/// テイクオーバー実施中フラグ。capture_loop はこれが true の間、キャプチャ固着での **re-exec を
/// 抑止**する。テイクオーバー直後は CCD のトポロジ変更で仮想の Desktop Duplication が一時的に
/// ACCESS_LOST を返すが、re-exec するとトポロジ復元→再接続→再テイクオーバーの点滅ループになる。
/// 再起動せず再構築を続けてトポロジが落ち着くのを待つ方が正しい (落ち着けば複製が確立する)。
static TAKEOVER_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// テイクオーバー実施中か (capture_loop の re-exec 抑止判定用)。全 OS で参照可能。
pub fn is_takeover_active() -> bool {
    TAKEOVER_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

/// 取り込みワーカー用: テイクオーバーを行う本体から伝えられた状態を写す。ワーカーは自分では
/// テイクオーバーしないが、capture_loop の固着判定はこの状態を見る。
pub fn mirror_takeover_active(active: bool) {
    TAKEOVER_ACTIVE.store(active, std::sync::atomic::Ordering::Relaxed);
}

/// テイクオーバー開始: 現在の全トポロジを保存 (+ディスク) し、仮想 (`\\.\DISPLAYn`) を主 (0,0) +
/// 表示領域 `(w,h)` に最も近い解像度にして物理を全てデタッチする。適用後の仮想解像度を返す。
#[cfg(target_os = "windows")]
pub fn begin_takeover(virtual_name: &str, w: u32, h: u32) -> anyhow::Result<(i32, i32, u32, u32)> {
    let r = imp::begin_takeover(virtual_name, w, h);
    if r.is_ok() {
        TAKEOVER_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    r
}
/// テイクオーバー終了: ディスクに保存したトポロジへ全ディスプレイを復元し、ファイルを消す。
#[cfg(target_os = "windows")]
pub fn end_takeover() -> anyhow::Result<()> {
    TAKEOVER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    imp::end_takeover()
}
/// 起動時回復: 前回のテイクオーバーがディスクに残っていれば復元する (クラッシュ救済の安全網)。
#[cfg(target_os = "windows")]
pub fn recover_takeover_if_any() {
    TAKEOVER_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    imp::recover_takeover_if_any()
}

/// 仮想ディスプレイ (`\\.\DISPLAYn`) がデスクトップにアタッチ済みなら切り離す。
///
/// Aurora 未接続時にカーソルや Window が仮想ディスプレイへ迷い込む問題を防ぐ。
/// 起動時と切断後に呼ぶ。デタッチは `CDS_UPDATEREGISTRY` で永続化するため
/// 再起動後も仮想ディスプレイがデスクトップに残らない。
/// テイクオーバー (`begin_takeover`) が接続時に再アタッチするため、接続の妨げにはならない。
#[cfg(target_os = "windows")]
pub fn detach_vdd_if_attached(vdd_name: &str) {
    imp::detach_if_attached(vdd_name);
}

#[cfg(not(target_os = "windows"))]
pub fn detach_vdd_if_attached(_vdd_name: &str) {}

#[cfg(not(target_os = "windows"))]
pub fn begin_takeover(
    _virtual_name: &str,
    _w: u32,
    _h: u32,
) -> anyhow::Result<(i32, i32, u32, u32)> {
    Err(anyhow::anyhow!("takeover is Windows-only"))
}
#[cfg(not(target_os = "windows"))]
pub fn end_takeover() -> anyhow::Result<()> {
    Ok(())
}
#[cfg(not(target_os = "windows"))]
pub fn recover_takeover_if_any() {}

// --- Windows 実装 ---
#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::{HWND, POINTL};
    use windows::Win32::Graphics::Gdi::{
        CDS_NORESET, CDS_SET_PRIMARY, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW,
        DEVMODEW, DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW,
        DM_BITSPERPEL, DM_DISPLAYFLAGS, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
        DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW,
        EnumDisplaySettingsW,
    };
    use windows::core::PCWSTR;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn devmode_init() -> DEVMODEW {
        DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        }
    }

    /// 指定モード番号 (`ENUM_CURRENT_SETTINGS` も可) の DEVMODEW を取得する。
    fn enum_settings(device_wide: &[u16], mode: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
        let mut dm = devmode_init();
        // SAFETY: device_wide は NUL 終端の有効な GDI 名。dm は dmSize 設定済みで OUT を埋める。
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR(device_wide.as_ptr()), mode, &mut dm) };
        ok.as_bool().then_some(dm)
    }

    /// 対応モード一覧 (width, height) を列挙する (重複は許容)。
    fn list_modes(device_wide: &[u16]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut i = 0u32;
        while let Some(dm) = enum_settings(device_wide, ENUM_DISPLAY_SETTINGS_MODE(i)) {
            out.push((dm.dmPelsWidth, dm.dmPelsHeight));
            i += 1;
        }
        out
    }

    /// 表示領域 `(tw, th)` に最も近いモードを選ぶ。アスペクト比一致を強く優先し (黒帯を消す)、
    /// 次にサイズの近さで決める。
    fn pick_nearest(modes: &[(u32, u32)], tw: u32, th: u32) -> Option<(u32, u32)> {
        if tw == 0 || th == 0 {
            return None;
        }
        let ta = tw as f64 / th as f64;
        modes
            .iter()
            .filter(|(w, h)| *w >= 2 && *h >= 2)
            .copied()
            .min_by(|a, b| score(*a, ta, tw, th).total_cmp(&score(*b, ta, tw, th)))
    }

    fn score((w, h): (u32, u32), ta: f64, tw: u32, th: u32) -> f64 {
        let ma = w as f64 / h as f64;
        // アスペクト差を強めに罰する (黒帯回避優先)、加えてサイズのユークリッド距離。
        let aspect_pen = (ma - ta).abs() * 4000.0;
        let size_pen =
            (((w as f64) - (tw as f64)).powi(2) + ((h as f64) - (th as f64)).powi(2)).sqrt();
        aspect_pen + size_pen
    }

    /// 診断: device の解像度を別モードへ変更し、その後元へ復元する一連を実測する。
    /// 各 ChangeDisplaySettingsEx の DISP_CHANGE コードと前後の実解像度/周波数を返す。
    /// 「復元が効かない」原因 (apply/restore の成否・どのフラグが要るか) の切り分け用。
    pub(super) fn debug_test_cycle(device: &str) -> Vec<String> {
        let mut out = Vec::new();
        let wide = to_wide(device);
        let Some(cur) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) else {
            out.push(format!("enum CURRENT failed for {device}"));
            return out;
        };
        out.push(format!(
            "current: {}x{} {}Hz dmFields=0x{:x}",
            cur.dmPelsWidth, cur.dmPelsHeight, cur.dmDisplayFrequency, cur.dmFields.0
        ));
        let modes = list_modes(&wide);
        let Some(&(tw, th)) = modes
            .iter()
            .find(|&&(w, h)| (w, h) != (cur.dmPelsWidth, cur.dmPelsHeight) && w >= 2 && h >= 2)
        else {
            out.push("no alternative mode to test".into());
            return out;
        };

        let mut dm = devmode_init();
        dm.dmPelsWidth = tw;
        dm.dmPelsHeight = th;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        // SAFETY: wide 有効、dm 設定済み。動的変更 (flags=0)。
        let r1 = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wide.as_ptr()),
                Some(&dm),
                HWND::default(),
                CDS_TYPE(0),
                None,
            )
        };
        out.push(format!("apply {tw}x{th} (flags=0) -> DISP_CHANGE {}", r1.0));
        if let Some(a) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) {
            out.push(format!(
                "  after apply: {}x{} {}Hz",
                a.dmPelsWidth, a.dmPelsHeight, a.dmDisplayFrequency
            ));
        }

        // 復元 1: 記憶した DEVMODEW + 明示 dmFields、flags=0 (動的)。
        let mut rdm = cur;
        rdm.dmFields |= DM_PELSWIDTH
            | DM_PELSHEIGHT
            | DM_DISPLAYFREQUENCY
            | DM_POSITION
            | DM_BITSPERPEL
            | DM_DISPLAYFLAGS;
        // SAFETY: 同上。
        let r2 = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wide.as_ptr()),
                Some(&rdm),
                HWND::default(),
                CDS_TYPE(0),
                None,
            )
        };
        out.push(format!(
            "restore (flags=0, full fields) -> DISP_CHANGE {}",
            r2.0
        ));
        if let Some(a) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) {
            out.push(format!(
                "  after restore(flags=0): {}x{} {}Hz",
                a.dmPelsWidth, a.dmPelsHeight, a.dmDisplayFrequency
            ));
        }

        // 復元 2 (まだ戻っていなければ): CDS_UPDATEREGISTRY で再試行 (1=UPDATEREGISTRY)。
        if enum_settings(&wide, ENUM_CURRENT_SETTINGS)
            .map(|a| (a.dmPelsWidth, a.dmPelsHeight) != (cur.dmPelsWidth, cur.dmPelsHeight))
            .unwrap_or(false)
        {
            // SAFETY: 同上。CDS_TYPE(1) = CDS_UPDATEREGISTRY。
            let r3 = unsafe {
                ChangeDisplaySettingsExW(
                    PCWSTR(wide.as_ptr()),
                    Some(&rdm),
                    HWND::default(),
                    CDS_TYPE(1),
                    None,
                )
            };
            out.push(format!(
                "restore (CDS_UPDATEREGISTRY) -> DISP_CHANGE {}",
                r3.0
            ));
            if let Some(a) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) {
                out.push(format!(
                    "  after restore(UPDATEREGISTRY): {}x{} {}Hz",
                    a.dmPelsWidth, a.dmPelsHeight, a.dmDisplayFrequency
                ));
            }
        }
        out
    }

    // --- テイクオーバー (RDP 風: 仮想を主にし物理を全てデタッチ) ---
    //
    // セッション中だけ「仮想ディスプレイを主 (0,0,w×h)、物理を全てデタッチ」にして、実デスクトップ
    // (既存ウィンドウ) を仮想へ集約する。切断/終了/起動時に元トポロジへ復元する。復元の確実性が命
    // なので、保存トポロジをディスクにも書き、クラッシュ後の起動時に回復できるようにする。
    //
    // 重要: モニタのデタッチ・主ディスプレイ変更は **CDS_UPDATEREGISTRY が必須**。これを付けない
    // 動的 (テスト) 変更ではデタッチ/主切替が commit で失敗し、begin_takeover が即 Err → 物理を
    // 即復元する (= 暗転が一瞬で戻り、ウィンドウが仮想へ集約されない実機バグの原因だった)。
    // よって永続変更にし、クラッシュ時の暗いまま固着はディスク保存トポロジ + 起動時
    // recover_takeover_if_any で必ず回復させる (多重安全網)。

    fn wstr(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    /// 復元用に保存する 1 ディスプレイの状態 (ディスク永続のため単純なフィールドのみ)。
    struct DisplayState {
        name: String,
        w: u32,
        h: u32,
        freq: u32,
        x: i32,
        y: i32,
        bits: u32,
    }

    /// デスクトップに接続済みの全ディスプレイの現在状態を取得する。
    fn save_topology() -> Vec<DisplayState> {
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: NULL でアダプタを順に列挙。範囲外で FALSE。
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
            if !ok.as_bool() {
                break;
            }
            i += 1;
            if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) == 0 {
                continue;
            }
            let name = wstr(&dd.DeviceName);
            let wide = to_wide(&name);
            if let Some(dm) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) {
                // SAFETY: 表示用 DEVMODE の位置は Anonymous1.Anonymous2.dmPosition (POINTL)。
                let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
                out.push(DisplayState {
                    name,
                    w: dm.dmPelsWidth,
                    h: dm.dmPelsHeight,
                    freq: dm.dmDisplayFrequency,
                    x: pos.x,
                    y: pos.y,
                    bits: dm.dmBitsPerPel,
                });
            }
        }
        out
    }

    fn topology_file() -> Option<std::path::PathBuf> {
        Some(
            crate::system::state::state_dir()
                .ok()?
                .join("takeover_topology.txt"),
        )
    }

    fn serialize_topology(states: &[DisplayState]) -> String {
        states
            .iter()
            .map(|s| {
                format!(
                    "{}|{}|{}|{}|{}|{}|{}",
                    s.name, s.w, s.h, s.freq, s.x, s.y, s.bits
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn parse_topology(text: &str) -> Vec<DisplayState> {
        text.lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split('|').collect();
                if f.len() != 7 {
                    return None;
                }
                Some(DisplayState {
                    name: f[0].to_string(),
                    w: f[1].parse().ok()?,
                    h: f[2].parse().ok()?,
                    freq: f[3].parse().ok()?,
                    x: f[4].parse().ok()?,
                    y: f[5].parse().ok()?,
                    bits: f[6].parse().ok()?,
                })
            })
            .collect()
    }

    // --- CCD トポロジ (paths/modes) の保存/復元 ---
    //
    // 復元の本命。テイクオーバーは CCD (SetDisplayConfig) で全物理パスを非アクティブ化して仮想だけを
    // 残す。これを ChangeDisplaySettingsEx で戻そうとしても、ブートディスプレイを detach できない実機
    // で同様に「CCD が落としたパスの再アクティブ化」も効かず、全画面が暗転したまま固着する (再接続も
    // 不可)。さらに復元が中途半端だと fps (リフレッシュレート) と主ディスプレイ指定が物理へ戻らない。
    // そこでテイクオーバー直前のアクティブトポロジ (paths/modes) を丸ごと記録し、復元も CCD で行う。
    // SetDisplayConfig はアクティブ集合・位置・解像度・リフレッシュレート・主指定を原子的に戻すため、
    // 報告された両症状 (暗転固着 / fps・主が戻らない) を一括で解消する。

    /// CCD のバイナリ保存先 (raw bytes)。テキスト版 (`topology_file`) はフォールバック用に併存。
    fn ccd_topology_file() -> Option<std::path::PathBuf> {
        Some(
            crate::system::state::state_dir()
                .ok()?
                .join("takeover_ccd.bin"),
        )
    }

    /// 現在アクティブなトポロジ (paths/modes) を CCD で取得する。
    fn save_ccd_topology() -> Option<(
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO>,
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO>,
    )> {
        use windows::Win32::Devices::Display::{
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, GetDisplayConfigBufferSizes,
            QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
        };
        use windows::Win32::Foundation::ERROR_SUCCESS;

        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        // SAFETY: OUT パラメータ 2 つを埋める。
        let e = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes)
        };
        if e != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        // SAFETY: paths/modes は n_paths/n_modes 長で確保済み。
        let e = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut n_paths,
                paths.as_mut_ptr(),
                &mut n_modes,
                modes.as_mut_ptr(),
                None,
            )
        };
        if e != ERROR_SUCCESS {
            return None;
        }
        paths.truncate(n_paths as usize);
        modes.truncate(n_modes as usize);
        Some((paths, modes))
    }

    /// 記録した CCD トポロジ (paths/modes) を SetDisplayConfig で原子的に復元する。
    fn restore_ccd_topology(
        paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
        modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ) -> anyhow::Result<()> {
        use anyhow::anyhow;
        use windows::Win32::Devices::Display::{
            SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
            SetDisplayConfig,
        };
        if paths.is_empty() {
            return Err(anyhow!("empty CCD topology"));
        }
        // まず供給した config をそのまま適用 (解像度/リフレッシュ/位置/主を厳密復元)。
        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE;
        // SAFETY: paths/modes は QueryDisplayConfig が埋めた整合配列をそのまま供給する。
        let r = unsafe { SetDisplayConfig(Some(paths), Some(modes), flags) };
        if r == 0 {
            return Ok(());
        }
        tracing::warn!(
            code = r,
            "CCD restore strict failed; retrying with ALLOW_CHANGES"
        );
        // フォールバック: Windows にモード調整を許して再適用 (微妙な不整合でも戻す)。
        // SAFETY: 同上。
        let r2 = unsafe { SetDisplayConfig(Some(paths), Some(modes), flags | SDC_ALLOW_CHANGES) };
        if r2 != 0 {
            return Err(anyhow!("SetDisplayConfig(restore) failed ({r}/{r2})"));
        }
        Ok(())
    }

    /// CCD トポロジを raw bytes へ直列化する (n_paths/n_modes ヘッダ + POD 配列)。
    fn serialize_ccd(
        paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
        modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(modes.len() as u32).to_le_bytes());
        // SAFETY: DISPLAYCONFIG_* は #[repr(C)] でポインタを含まない POD。生バイト化は安全。
        let pb = unsafe {
            std::slice::from_raw_parts(paths.as_ptr() as *const u8, std::mem::size_of_val(paths))
        };
        // SAFETY: 同上 (modes も #[repr(C)] の POD)。ptr/len は size_of_val で整合。
        let mb = unsafe {
            std::slice::from_raw_parts(modes.as_ptr() as *const u8, std::mem::size_of_val(modes))
        };
        buf.extend_from_slice(pb);
        buf.extend_from_slice(mb);
        buf
    }

    /// raw bytes から CCD トポロジを復元する。サイズ不整合なら `None`。
    fn deserialize_ccd(
        bytes: &[u8],
    ) -> Option<(
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO>,
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO>,
    )> {
        use windows::Win32::Devices::Display::{DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO};
        let psz = std::mem::size_of::<DISPLAYCONFIG_PATH_INFO>();
        let msz = std::mem::size_of::<DISPLAYCONFIG_MODE_INFO>();
        if bytes.len() < 8 {
            return None;
        }
        let n_paths = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        let n_modes = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
        let need = 8usize
            .checked_add(n_paths.checked_mul(psz)?)?
            .checked_add(n_modes.checked_mul(msz)?)?;
        if bytes.len() < need {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes];
        // SAFETY: サイズ検証済み。raw bytes を POD 配列へコピーする。
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes[8..].as_ptr(),
                paths.as_mut_ptr() as *mut u8,
                n_paths * psz,
            );
            std::ptr::copy_nonoverlapping(
                bytes[8 + n_paths * psz..].as_ptr(),
                modes.as_mut_ptr() as *mut u8,
                n_modes * msz,
            );
        }
        Some((paths, modes))
    }

    /// 指定 GDI 名がデスクトップにアタッチ済みかを返す。
    fn is_attached(name: &str) -> bool {
        let mut i = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: NULL でアダプタを順に列挙。範囲外で FALSE。
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
            if !ok.as_bool() {
                break;
            }
            i += 1;
            if wstr(&dd.DeviceName) == name {
                return (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0;
            }
        }
        false
    }

    /// 保存済みトポロジへ全ディスプレイを復元する (CCD 優先、テキスト CDS フォールバック)。
    /// 復元を試みたら使用済みファイルを必ず削除する。何かを復元したら `true`。
    ///
    /// **安全網**: CCD 復元は `SetDisplayConfig` が成功 (Ok) を返しても、実機では物理 (特に
    /// DISPLAY1) が detached のまま戻らないことがある (2026-06-20 実機で物理黒固着を観測)。そこで
    /// 復元後に「テイクオーバー前にアタッチされていた保存済み物理」が実際に全て戻ったか検証し、
    /// 欠けていれば `ChangeDisplaySettingsEx` で強制再アタッチする (= 手動復旧で成功した手順)。
    /// これによりテイクオーバー/巻き戻しが何をしても切断時に物理黒固着を残さない。
    fn restore_all() -> bool {
        let mut ccd_ok = false;
        if let Some(path) = ccd_topology_file() {
            if let Ok(bytes) = std::fs::read(&path)
                && let Some((paths, modes)) = deserialize_ccd(&bytes)
            {
                match restore_ccd_topology(&paths, &modes) {
                    Ok(()) => {
                        ccd_ok = true;
                        tracing::info!("topology restored via CCD");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "CCD restore failed; trying CDS fallback");
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
        }
        // テイクオーバー前のアタッチ物理一覧 (テキスト topology)。CDS フォールバック + 検証の両方に使う。
        let states: Vec<DisplayState> = topology_file()
            .and_then(|path| {
                let text = std::fs::read_to_string(&path).ok();
                let _ = std::fs::remove_file(&path);
                text
            })
            .map(|t| parse_topology(&t))
            .unwrap_or_default();
        // CCD が失敗したときの一次フォールバック。
        if !ccd_ok
            && !states.is_empty()
            && let Err(e) = restore_topology(&states)
        {
            tracing::warn!(error = %e, "CDS fallback restore failed");
        }
        // ★安全網: 復元後も保存済み物理が 1 枚でも非アタッチなら強制再アタッチする。CCD restore が
        // Ok を返しても DISPLAY1 が戻らない事例の保険 (物理黒固着を絶対に残さない)。
        if !states.is_empty() {
            let missing = states.iter().filter(|s| !is_attached(&s.name)).count();
            if missing > 0 {
                tracing::warn!(
                    missing,
                    "restore left physical display(s) detached; forcing reattach (safety net)"
                );
                if let Err(e) = restore_topology(&states) {
                    tracing::warn!(error = %e, "forced reattach failed");
                } else if let Some(s) = states.iter().find(|s| !is_attached(&s.name)) {
                    tracing::error!(name = %s.name, "physical still detached after forced reattach");
                }
            }
        }
        ccd_ok
    }

    /// アダプタ (`\\.\DISPLAYn`) のモニタ hardware ID を調べ MttVDD かを判定する。
    /// モニタ子が取得できない場合は `None` (「VDD でない」と区別する。デタッチ済み VDD は
    /// モニタ情報が読めないことがあり、これを false 扱いすると誤検出につながるため)。
    fn monitor_is_vdd(adapter_name: &str) -> Option<bool> {
        let aw: Vec<u16> = adapter_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut mon = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: aw は NUL 終端の有効なアダプタ名。0 番モニタの情報を取る。
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR(aw.as_ptr()), 0, &mut mon, 0) };
        if !ok.as_bool() {
            return None;
        }
        Some(
            wstr(&mon.DeviceID).contains("MTT1337")
                || wstr(&mon.DeviceString).contains("VDD by MTT"),
        )
    }

    /// アダプタ (`\\.\DISPLAYn`) のモニタ hardware ID を調べ MttVDD かを判定する。
    ///
    /// phase1 後に Windows が GDI 名を再割当てする場合でも仮想ディスプレイを確実に識別するため。
    /// GDI 名のみで判定すると「古い名前が別の物理ディスプレイに付いてしまう」問題が起きうる。
    fn is_vdd_adapter(adapter_name: &str) -> bool {
        monitor_is_vdd(adapter_name).unwrap_or(false)
    }

    /// アタッチ済み全ディスプレイの右端 (max(x+width))。仮想を、既存デスクトップに
    /// 重ならない位置へ (再)配置するための原点に使う。
    fn rightmost_edge() -> i32 {
        let mut max_x = 0i32;
        let mut i = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: NULL でアダプタを順に列挙。範囲外で FALSE。
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
            if !ok.as_bool() {
                break;
            }
            i += 1;
            if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) == 0 {
                continue;
            }
            let name = wstr(&dd.DeviceName);
            let wide = to_wide(&name);
            if let Some(dm) = enum_settings(&wide, ENUM_CURRENT_SETTINGS) {
                // SAFETY: 表示用 DEVMODE の位置は dmPosition (POINTL)。
                let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
                max_x = max_x.max(pos.x + dm.dmPelsWidth as i32);
            }
        }
        max_x
    }

    /// 現在の VDD (MttVDD) の GDI 名 (`\\.\DISPLAYn`) を返す。CCD で仮想を唯一の表示にすると
    /// GDI 名が変わることがあるため、テイクオーバー後の解像度設定/キャプチャ対象の特定に使う。
    fn detect_vdd_name() -> Option<String> {
        let mut i = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: NULL でアダプタを順に列挙。範囲外で FALSE。
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
            if !ok.as_bool() {
                break;
            }
            i += 1;
            let name = wstr(&dd.DeviceName);
            if is_vdd_adapter(&name) {
                return Some(name);
            }
        }
        None
    }

    /// CCD (Connecting and Configuring Displays) API で「仮想ディスプレイ (source GDI 名 ==
    /// `virtual_name`) のパスだけをアクティブ、他を全て非アクティブ」にする。
    ///
    /// ChangeDisplaySettingsEx ではブートディスプレイ (DISPLAY1) を detach できなかった
    /// (DISP_CHANGE_FAILED) ため、トポロジ全体の切替を CCD に委ねる。SetDisplayConfig は
    /// 全パスを 1 回で原子的に適用するので、物理を全て落として仮想だけを唯一/主にできる。
    fn ccd_activate_only(virtual_name: &str) -> anyhow::Result<()> {
        use anyhow::anyhow;
        use windows::Win32::Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
            DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
            GetDisplayConfigBufferSizes, QDC_ALL_PATHS, QueryDisplayConfig, SDC_ALLOW_CHANGES,
            SDC_APPLY, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SetDisplayConfig,
        };
        use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};

        // DISPLAYCONFIG_PATH_ACTIVE (winuser.h)。crate に定数が無い場合に備えてローカル定義。
        const PATH_ACTIVE: u32 = 0x0000_0001;

        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        // SAFETY: OUT パラメータ 2 つを埋める。
        let e = unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut n_paths, &mut n_modes) };
        if e != ERROR_SUCCESS {
            return Err(anyhow!("GetDisplayConfigBufferSizes failed ({:?})", e));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        // SAFETY: paths/modes は n_paths/n_modes 長で確保済み。
        let e = unsafe {
            QueryDisplayConfig(
                QDC_ALL_PATHS,
                &mut n_paths,
                paths.as_mut_ptr(),
                &mut n_modes,
                modes.as_mut_ptr(),
                None,
            )
        };
        if e != ERROR_SUCCESS {
            return Err(anyhow!("QueryDisplayConfig failed ({:?})", e));
        }
        paths.truncate(n_paths as usize);
        modes.truncate(n_modes as usize);

        // 各 path の source GDI 名を引き、仮想に一致する path だけ active、他は非 active にする。
        let mut found = false;
        for p in paths.iter_mut() {
            let mut q = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            q.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            q.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            q.header.adapterId = p.sourceInfo.adapterId;
            q.header.id = p.sourceInfo.id;
            // SAFETY: header に type/size/adapterId/id を設定済み。viewGdiDeviceName を埋める。
            let r = unsafe { DisplayConfigGetDeviceInfo(&mut q.header) };
            let gdi = if r == 0 {
                wstr(&q.viewGdiDeviceName)
            } else {
                String::new()
            };
            if gdi == virtual_name {
                p.flags |= PATH_ACTIVE;
                found = true;
            } else {
                p.flags &= !PATH_ACTIVE;
            }
            // union への書き込みは safe。モード index を無効化し、モード決定を Windows に委ねる
            // (新規アクティブ化する仮想 path にモードが無くても SDC_ALLOW_CHANGES で生成される)。
            p.sourceInfo.Anonymous.modeInfoIdx = 0xffff_ffff;
            p.targetInfo.Anonymous.modeInfoIdx = 0xffff_ffff;
        }
        if !found {
            return Err(anyhow!(
                "virtual source path not found in CCD ({virtual_name})"
            ));
        }

        // **SDC_SAVE_TO_DATABASE は付けない (重要)**。テイクオーバーの「仮想のみ」構成を
        // ディスプレイ DB に永続保存すると、後で同じモニタ構成 (例: 物理3枚) を接続したときに
        // Windows がその壊れた構成を呼び戻し全画面が暗転する (実機で再現)。テイクオーバーは
        // セッション限りの適用 (SDC_APPLY) に留め、DB へ焼き付けるのは復元時の正常構成だけにする。
        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
        // SAFETY: paths は QueryDisplayConfig が埋めた有効配列 (active フラグ + modeInfoIdx 無効化のみ
        // 変更)。modes は供給せず (None)、Windows が ALLOW_CHANGES で計算する。
        let r = unsafe { SetDisplayConfig(Some(&paths), None, flags) };
        if r != WIN32_ERROR(0).0 as i32 {
            return Err(anyhow!(
                "SetDisplayConfig(activate-only-virtual) failed ({r})"
            ));
        }
        Ok(())
    }

    /// ChangeDisplaySettingsEx の薄いラッパー。DISP_CHANGE コード (i32) を返す。
    fn change_settings(name: &str, dm: &DEVMODEW, flags: CDS_TYPE) -> i32 {
        let wide = to_wide(name);
        // SAFETY: name 有効、dm 設定済み。hwnd=NULL、lparam 不使用。
        unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wide.as_ptr()),
                Some(dm as *const DEVMODEW),
                HWND::default(),
                flags,
                None,
            )
        }
        .0
    }

    /// 保留中 (CDS_NORESET) のディスプレイ変更を適用する (NULL/NULL/0)。
    fn commit_changes() -> i32 {
        // SAFETY: NULL デバイス + None devmode + flags 0 で保留設定を反映する。
        unsafe {
            ChangeDisplaySettingsExW(PCWSTR::null(), None, HWND::default(), CDS_TYPE(0), None)
        }
        .0
    }

    /// 仮想を主 (0,0) + 表示領域に最も近い解像度にし、物理を全てデタッチする。適用後の仮想解像度を返す。
    ///
    /// **2 段階**で行う。主切替とデタッチを 1 回の commit にまとめると、現主ディスプレイの
    /// デタッチと新主の設定が同一バッチで衝突し DISP_CHANGE_FAILED/BADDUALVIEW で失敗する実機が
    /// ある (= 暗転が一瞬で戻りウィンドウが集約されない症状)。そこで (1) まず仮想を主にして
    /// 即適用し、(2) その後で非主になった物理をデタッチして commit する。各 DISP_CHANGE コードは
    /// info で残し、失敗時にどの段で落ちたか実機ログから判る (headless 検証不可のため)。
    /// 指定ディスプレイ (`\\.\DISPLAYn`) の現在のジオメトリ (x,y,w,h) を読む。テイクオーバー後に
    /// 仮想が実際にどこに何の解像度で居るかを確定するために使う (stage2 の (0,0) 移動が -1 失敗で
    /// 右端に残ることがあり、入力座標系を実位置に合わせないとマウスがずれるため)。
    fn current_geom(name: &str) -> Option<(i32, i32, u32, u32)> {
        let wide = to_wide(name);
        let dm = enum_settings(&wide, ENUM_CURRENT_SETTINGS)?;
        // SAFETY: 表示用 DEVMODE の位置は Anonymous1.Anonymous2.dmPosition (POINTL)。
        let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
        Some((pos.x, pos.y, dm.dmPelsWidth, dm.dmPelsHeight))
    }

    fn apply_takeover(
        virtual_name: &str,
        w: u32,
        h: u32,
        pre_topo: &[DisplayState],
    ) -> anyhow::Result<(i32, i32, u32, u32)> {
        use anyhow::anyhow;
        let wide = to_wide(virtual_name);

        // 段階0: 仮想を一旦デスクトップ右端へ (再)配置する。**アタッチ済みでも毎回実行する**のが
        // 重要: set-primary は「アタッチ/再配置 + commit の直後」でないと成功せず、既にアタッチ済みの
        // 状態でいきなり set-primary すると DISP_CHANGE_FAILED(-1) になる実機を確認 (前回の
        // テイクオーバー後に detach_vdd が -1 で失敗し、仮想がアタッチ済みのまま固着するとこれに陥り、
        // 結果どの物理モニタも暗転しない)。右端への再配置→commit を必ず通してから主昇格することで、
        // 直前の状態に依らず set-primary を成功させる。
        {
            let pre_modes = list_modes(&wide);
            let (aw, ah) = pick_nearest(&pre_modes, w, h)
                .or_else(|| pre_modes.first().copied())
                .unwrap_or((800, 600));
            let x = rightmost_edge();
            let mut adm = devmode_init();
            adm.dmPelsWidth = aw;
            adm.dmPelsHeight = ah;
            // union への書き込みは safe。重ならない右端へ置く。
            adm.Anonymous1.Anonymous2.dmPosition = POINTL { x, y: 0 };
            adm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
            let ar = change_settings(virtual_name, &adm, CDS_UPDATEREGISTRY | CDS_NORESET);
            let ac = commit_changes();
            tracing::info!(
                aw,
                ah,
                x,
                attach = ar,
                commit = ac,
                "takeover: (re)attach virtual at right edge"
            );
            if ac != DISP_CHANGE_SUCCESSFUL.0 {
                return Err(anyhow!("attach virtual failed (DISP_CHANGE {ac})"));
            }
        }

        // 段階1: CCD で「仮想だけアクティブ = 全物理 (DISPLAY1 含む) を非アクティブ」にする。
        // ChangeDisplaySettingsEx ではブートディスプレイを detach できなかったため、トポロジ全体の
        // 切替は CCD (SetDisplayConfig) に委ねる。これで全物理モニタが暗転し仮想だけが残る。
        ccd_activate_only(virtual_name)?;
        tracing::info!(
            virtual_name,
            "takeover: CCD activated virtual-only topology"
        );

        // CCD 後は仮想が唯一の表示になり GDI 名が変わることがある。MTT1337 で再検出する。
        let cur_name = detect_vdd_name().unwrap_or_else(|| virtual_name.to_string());

        // 段階2 (バッチ): CCD は仮想を既定モード (800x600) に戻し、レジストリには物理アタッチが残る
        // (= 非永続のため数秒後に Windows が reconcile して物理を復活させる)。そこで **1 トランザクション
        // (CDS_NORESET 群 + 単一 commit)** で「① 仮想を目標解像度 + (0,0)」と「② 全物理の永続デタッチ
        // (方針 A: レジストリを"仮想のみ"へ同期し reconcile を止める)」をまとめて適用する。
        // 別々に commit すると、① の解像度変更が直前の detach commit と競合して DISP_CHANGE_FAILED(-1)
        // になり 800x600 に張り付く実機症状を確認したため、原子バッチにする (手動復旧で実証済みの手順)。
        // CCD の SDC_SAVE_TO_DATABASE は使わないので過去の「特定モニタ構成で毎回全黒固着」地雷とは別経路。
        let modes = list_modes(&to_wide(&cur_name));
        let (bw, bh) = pick_nearest(&modes, w, h).unwrap_or((w.max(2), h.max(2)));
        let mut vdm = devmode_init();
        vdm.dmPelsWidth = bw;
        vdm.dmPelsHeight = bh;
        // 表示用 DEVMODE の位置 (union フィールド) へ書く (union への書き込みは safe)。
        vdm.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y: 0 };
        vdm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
        let vr = change_settings(&cur_name, &vdm, CDS_UPDATEREGISTRY | CDS_NORESET);
        let mut nd = 0;
        for s in pre_topo {
            // 仮想 (テイクオーバー対象) と VDD アダプタは残す。is_vdd_adapter は MTT1337 モニタ ID で
            // 判定するため CCD 後に GDI 名が変わっても確実に VDD を除外できる。
            if s.name == cur_name || s.name == virtual_name || is_vdd_adapter(&s.name) {
                continue;
            }
            let mut dm = devmode_init();
            dm.dmPelsWidth = 0;
            dm.dmPelsHeight = 0;
            // union への書き込みは safe。
            dm.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y: 0 };
            dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
            let r = change_settings(&s.name, &dm, CDS_UPDATEREGISTRY | CDS_NORESET);
            tracing::info!(name = %s.name, code = r, "takeover: persist-detach physical (registry sync)");
            nd += 1;
        }
        let commit = commit_changes();
        tracing::info!(
            cur_name,
            bw,
            bh,
            set_res = vr,
            detached = nd,
            commit,
            "takeover: applied virtual resolution + physical persist-detach (batched)"
        );
        // 仮想の**実際の**位置/解像度を読み戻す。バッチが一部失敗しても入力座標系/解像度通知は
        // 実値でなければマウス・スケールがずれるため、ENUM_CURRENT_SETTINGS の値を信頼する。
        let (vx, vy, rw, rh) = current_geom(&cur_name).unwrap_or((0, 0, bw, bh));
        tracing::info!(
            cur_name,
            vx,
            vy,
            rw,
            rh,
            "takeover: virtual actual geometry"
        );
        Ok((vx, vy, rw, rh))
    }

    /// 保存したトポロジへ全ディスプレイを復元する。
    fn restore_topology(states: &[DisplayState]) -> anyhow::Result<()> {
        use anyhow::anyhow;
        for s in states {
            let mut dm = devmode_init();
            dm.dmPelsWidth = s.w;
            dm.dmPelsHeight = s.h;
            dm.dmDisplayFrequency = s.freq;
            dm.dmBitsPerPel = s.bits;
            // union への書き込みは safe。
            dm.Anonymous1.Anonymous2.dmPosition = POINTL { x: s.x, y: s.y };
            dm.dmFields =
                DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL | DM_POSITION;
            // 元の主ディスプレイ (原点) は CDS_SET_PRIMARY で戻す。デタッチを永続で行ったため
            // 再アタッチも CDS_UPDATEREGISTRY 永続でないと元に戻らない (再起動後も含め確実に戻す)。
            let primary = s.x == 0 && s.y == 0;
            let flags = if primary {
                CDS_UPDATEREGISTRY | CDS_SET_PRIMARY | CDS_NORESET
            } else {
                CDS_UPDATEREGISTRY | CDS_NORESET
            };
            let _ = change_settings(&s.name, &dm, flags);
        }
        let c = commit_changes();
        if c != DISP_CHANGE_SUCCESSFUL.0 {
            return Err(anyhow!("commit restore failed (DISP_CHANGE {c})"));
        }
        Ok(())
    }

    /// VDD がデスクトップにアタッチ済みかを StateFlags で確認してデタッチする。
    pub(super) fn detach_if_attached(name: &str) {
        // まず EnumDisplayDevices で DISPLAY_DEVICE_ATTACHED_TO_DESKTOP を確認する。
        let mut i = 0u32;
        let attached = loop {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: NULL でアダプタを順に列挙。範囲外で FALSE。
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
            if !ok.as_bool() {
                break false;
            }
            i += 1;
            if wstr(&dd.DeviceName) == name {
                break (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0;
            }
        };

        if !attached {
            tracing::debug!(name, "VDD already detached from desktop");
            return;
        }

        let mut dm = devmode_init();
        dm.dmPelsWidth = 0;
        dm.dmPelsHeight = 0;
        // union への書き込みは safe。
        dm.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y: 0 };
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
        // CDS_UPDATEREGISTRY で永続化する (再起動後も VDD がデスクトップに戻らないように)。
        let r = change_settings(name, &dm, CDS_UPDATEREGISTRY);
        if r == DISP_CHANGE_SUCCESSFUL.0 {
            tracing::info!(name, "VDD detached from desktop");
        } else {
            tracing::warn!(name, code = r, "VDD detach failed");
        }
    }

    pub(super) fn begin_takeover(
        virtual_name: &str,
        w: u32,
        h: u32,
    ) -> anyhow::Result<(i32, i32, u32, u32)> {
        // 最終防壁: detect() 側の GDI 名衝突ガードとは独立に、ここでも対象が本当に VDD か
        // 再確認する。誤検出で物理ディスプレイが VDD 扱いされてもここで弾き、以降のレジストリ
        // 永続書き換え (save_topology 等) が物理へ及ぶのを防ぐ。
        match monitor_is_vdd(virtual_name) {
            Some(false) => {
                return Err(anyhow::anyhow!(
                    "takeover target {virtual_name} is not the VDD monitor (MTT1337)"
                ));
            }
            None => {
                // デタッチ済み VDD はモニタ子が読めない場合がありうる。拒否すると正規の
                // テイクオーバーを壊すため、読めないだけなら続行する。
                tracing::debug!(
                    virtual_name,
                    "takeover target monitor unreadable; proceeding"
                );
            }
            Some(true) => {}
        }
        let topo = save_topology();
        // 先にディスクへ保存する (クラッシュしても次回起動で復元できるように)。
        // CCD バイナリが本命の復元元、テキストはフォールバック。両方を書く。
        if let Some((paths, modes)) = save_ccd_topology()
            && let Some(path) = ccd_topology_file()
        {
            let _ = std::fs::write(&path, serialize_ccd(&paths, &modes));
        }
        if let Some(path) = topology_file() {
            let _ = std::fs::write(&path, serialize_topology(&topo));
        }
        match apply_takeover(virtual_name, w, h, &topo) {
            Ok(dims) => {
                tracing::info!(
                    virtual_name,
                    x = dims.0,
                    y = dims.1,
                    w = dims.2,
                    h = dims.3,
                    "display takeover applied"
                );
                Ok(dims)
            }
            Err(e) => {
                // 失敗したら即座に元へ戻す (ベストエフォート)。apply が CCD で物理を落とした後に
                // 失敗した場合、ChangeDisplaySettingsEx では戻せないため CCD 復元を優先する。
                tracing::warn!(error = %e, "takeover failed; restoring topology");
                restore_all();
                Err(e)
            }
        }
    }

    pub(super) fn end_takeover() -> anyhow::Result<()> {
        restore_all();
        tracing::info!("display takeover ended; topology restored");
        Ok(())
    }

    pub(super) fn recover_takeover_if_any() {
        // 残骸ファイルがあるときだけ復元する (通常起動では何もしない)。
        let has_leftover = ccd_topology_file().map(|p| p.exists()).unwrap_or(false)
            || topology_file().map(|p| p.exists()).unwrap_or(false);
        if !has_leftover {
            return;
        }
        tracing::warn!("recovering display topology left by a previous takeover session");
        restore_all();
    }
}
