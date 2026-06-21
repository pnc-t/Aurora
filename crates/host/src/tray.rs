//! システムトレイ常駐 UI。アイコンの右クリックメニューから接続パスワードの設定・変更を
//! 行う。変更は DPAPI に保存し、共有 PSK ([`SharedPsk`]) を更新して **次の接続から** 反映する
//! (既存セッションは現行 PSK のまま継続)。
//!
//! Windows のみ実体を持つ。非 Windows では [`spawn`] は何もしない。

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32};
use std::sync::{Arc, Mutex};

/// host 全体で共有する接続 PSK。net 層が接続受理時に読み、tray がパスワード変更時に書く。
pub type SharedPsk = Arc<Mutex<[u8; 32]>>;

/// 実パスワードが設定済みかを共有するフラグ。設定済みなら変更時に現在パスワードを要求する。
pub type SharedHasPassword = Arc<AtomicBool>;

/// host 全体で共有する待受ポート。tray がポート変更で書き、net が reload 後に読み直す。
pub type SharedPort = Arc<AtomicU16>;

/// 認証失敗の累計カウンタ。net が接続受理で加算し、tray がメニューに表示する。
pub type SharedAuthFailures = Arc<AtomicU32>;

/// デバイスピン留めの実行時状態。net が照合し、tray が有効化/ペアリング/削除を操作する。
pub type SharedDevices = Arc<crate::devices::DeviceGuard>;

/// ポート変更を net の accept ループへ伝える反映トリガ (`notify_one` で TCP/UDP 貼り直し)。
pub type Reload = Arc<tokio::sync::Notify>;

/// トレイ常駐 UI を別スレッドで起動する。`prompt_now` が真なら起動直後に変更ダイアログを
/// 自動表示する (保存パスワードが無い初回向け)。
#[cfg(target_os = "windows")]
pub fn spawn(
    psk: SharedPsk,
    has_password: SharedHasPassword,
    port: SharedPort,
    reload: Reload,
    auth_failures: SharedAuthFailures,
    devices: SharedDevices,
    prompt_now: bool,
) {
    let _ = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = imp::run(
                psk,
                has_password,
                port,
                reload,
                auth_failures,
                devices,
                prompt_now,
            ) {
                tracing::warn!(error = %e, "tray UI disabled");
            }
        });
}

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _psk: SharedPsk,
    _has_password: SharedHasPassword,
    _port: SharedPort,
    _reload: Reload,
    _auth_failures: SharedAuthFailures,
    _devices: SharedDevices,
    _prompt_now: bool,
) {
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{
        Reload, SharedAuthFailures, SharedDevices, SharedHasPassword, SharedPort, SharedPsk,
    };
    use aurora_common::noise::psk_from_password;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        COLOR_BTNFACE, CreateFontW, DeleteObject, GetSysColorBrush, HFONT, HGDIOBJ,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{PCWSTR, w};

    /// トレイアイコンからの通知を受けるカスタムメッセージ。
    const WM_TRAY: u32 = WM_APP + 1;
    /// メニュー項目 ID。
    const ID_CHANGE: usize = 1001;
    const ID_EXIT: usize = 1002;
    const ID_PORT: usize = 1003;
    /// ログフォルダをエクスプローラで開くメニュー項目 ID。
    const ID_OPEN_LOGS: usize = 1007;
    /// ログオン時自動起動の有効/無効をトグルするメニュー項目 ID。
    const ID_AUTOSTART: usize = 1008;
    /// デバイスピン留めのメニュー項目 ID。
    const ID_PIN_TOGGLE: usize = 1004;
    const ID_PIN_PAIR: usize = 1005;
    const ID_PIN_CLEAR: usize = 1006;
    /// 登録端末リストの各項目 ID のベース (`ID_DEVICE_BASE + index`)。クリックでその端末を削除。
    const ID_DEVICE_BASE: usize = 2000;
    /// リスト表示する登録端末の上限 (ID 空間を有界にする。通常これを超える端末は持たない)。
    const MAX_DEVICE_ITEMS: usize = 64;
    /// 変更ダイアログ内コントロール ID。
    const ID_EDIT_CUR: i32 = 100;
    const ID_EDIT: i32 = 101;
    const ID_EDIT2: i32 = 102;
    /// ポート変更ダイアログの入力欄 ID。
    const ID_EDIT_PORT: i32 = 110;

    /// 隠しウィンドウに紐づける共有状態。パスワード変更で PSK を書き換え、設定済みフラグを
    /// 立てる。`GWLP_USERDATA` にこの Box のポインタを格納する。
    struct WndState {
        psk: SharedPsk,
        has_password: SharedHasPassword,
        port: SharedPort,
        reload: Reload,
        auth_failures: SharedAuthFailures,
        devices: SharedDevices,
    }

    /// UTF-16 NUL 終端へ変換する (固定長配列へのコピー用)。
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// ダイアログ用の現代的なフォント (Segoe UI 9pt, ClearType) を生成する。既定の
    /// `DEFAULT_GUI_FONT` は古い Tahoma 系で見た目が古臭いため置き換える。返した `HFONT` は
    /// 呼び出し側がダイアログ破棄後に `DeleteObject` で解放すること。
    fn ui_font() -> HFONT {
        let face = to_wide("Segoe UI");
        // SAFETY: face は NUL 終端の UTF-16。返る HFONT の所有権は呼び出し側 (DeleteObject)。
        // 負の高さ = キャラクタ高さ指定 (96dpi で 9pt 相当)。weight 400=Normal, charset 1=DEFAULT、
        // precision 0=OUT/CLIP_DEFAULT_PRECIS, quality 5=CLEARTYPE_QUALITY, pitch 0=DEFAULT。
        unsafe {
            CreateFontW(
                -12,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                PCWSTR(face.as_ptr()),
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run(
        psk: SharedPsk,
        has_password: SharedHasPassword,
        port: SharedPort,
        reload: Reload,
        auth_failures: SharedAuthFailures,
        devices: SharedDevices,
        prompt_now: bool,
    ) -> windows::core::Result<()> {
        unsafe {
            let hmod = GetModuleHandleW(None)?;
            let hinstance = HINSTANCE(hmod.0);
            let class_name = w!("AuroraTrayWnd");

            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: class_name,
                hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);

            // 共有状態をウィンドウに紐づける (WM_CREATE で GWLP_USERDATA へ格納)。
            let boxed: *mut WndState = Box::into_raw(Box::new(WndState {
                psk,
                has_password,
                port,
                reload,
                auth_failures,
                devices,
            }));

            // メッセージ受信専用の不可視ウィンドウ。表示しないので位置/サイズは既定でよい。
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("Aurora Host"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                HWND::default(),
                HMENU::default(),
                hinstance,
                Some(boxed as *const c_void),
            ) {
                Ok(h) => h,
                Err(e) => {
                    // WM_CREATE が来ず GWLP_USERDATA に保存されないため、ここで Box を回収する。
                    // SAFETY: boxed は直前の Box::into_raw 由来で、まだどこにも所有が移っていない。
                    drop(Box::from_raw(boxed));
                    return Err(e);
                }
            };

            // トレイアイコン登録。build.rs で exe へ埋め込んだ aurora アイコン (リソース
            // ID = 1) を使う。万一読めなければ標準アプリアイコンへフォールバックする。
            let hicon = LoadIconW(hinstance, PCWSTR(1 as *const u16))
                .or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION))?;
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: hicon,
                ..Default::default()
            };
            let tip = to_wide("Aurora Host");
            nid.szTip[..tip.len()].copy_from_slice(&tip);
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);

            // 初回 (保存パスワード無し) は起動直後に変更ダイアログを出す。
            if prompt_now {
                let _ = PostMessageW(hwnd, WM_COMMAND, WPARAM(ID_CHANGE), LPARAM(0));
            }

            // メッセージループ。
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // ループを抜けるのは「終了」メニューでウィンドウを破棄したときだけ
            // (初期化失敗時はループ前に return する)。アイコンを消して host 全体を終了する。
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            drop(Box::from_raw(boxed));
        }
        // process::exit は Drop を走らせないため、投影ドライブ (subst) を明示的に掃除する
        // (これが無いと This PC に古い Aurora ドライブが残る)。
        crate::projfs::cleanup_drives();
        // テイクオーバー中にトレイから終了されると net の cleanup (end_takeover) を経ずに死に、
        // 物理が暗転したまま残る (穴①)。残骸トポロジがあれば復元してから終了する。
        crate::display::recover_takeover_if_any();
        tracing::info!("tray exit requested; shutting down host");
        std::process::exit(0);
    }

    /// 隠しウィンドウのウィンドウプロシージャ。
    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_CREATE => {
                    // CreateWindowEx の lpCreateParams (=Box<WndState> ポインタ) を保存。
                    let cs = lparam.0 as *const CREATESTRUCTW;
                    if !cs.is_null() {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                    }
                    LRESULT(0)
                }
                WM_TRAY => {
                    // 右クリック (とアイコンの WM_CONTEXTMENU) でメニュー表示。
                    let ev = (lparam.0 as u32) & 0xffff;
                    if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU || ev == WM_LBUTTONUP {
                        show_menu(hwnd);
                    }
                    LRESULT(0)
                }
                WM_COMMAND => {
                    let cmd = wparam.0 & 0xffff;
                    match cmd {
                        ID_CHANGE => change_password(hwnd),
                        ID_PORT => change_port(hwnd),
                        ID_OPEN_LOGS => open_logs_folder(hwnd),
                        ID_AUTOSTART => toggle_autostart(hwnd),
                        ID_PIN_TOGGLE => toggle_pinning(hwnd),
                        ID_PIN_PAIR => toggle_pairing(hwnd),
                        ID_PIN_CLEAR => clear_devices(hwnd),
                        ID_EXIT => {
                            // host プロセス全体を終了する。
                            let _ = DestroyWindow(hwnd);
                        }
                        // 登録端末リストの項目: その端末を削除する。
                        c if (ID_DEVICE_BASE..ID_DEVICE_BASE + MAX_DEVICE_ITEMS).contains(&c) => {
                            remove_device(hwnd, c - ID_DEVICE_BASE);
                        }
                        _ => {}
                    }
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// 隠しウィンドウに紐づけた共有状態 (PSK + 設定済みフラグ) への参照を取り出す。
    unsafe fn wnd_state(hwnd: HWND) -> Option<&'static WndState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndState;
        if ptr.is_null() {
            None
        } else {
            // SAFETY: ポインタは run の Box::into_raw 由来で、ウィンドウ生存中は有効。
            Some(unsafe { &*ptr })
        }
    }

    /// トレイの右クリックメニューを表示する。
    unsafe fn show_menu(hwnd: HWND) {
        unsafe {
            let menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(_) => return,
            };
            // 認証失敗の累計を情報行 (操作不可) として先頭に出す。0 のときは出さない。
            // 総当たりに晒されていればここで数が伸びるので、オーナーが気付ける。
            if let Some(state) = wnd_state(hwnd) {
                let n = state.auth_failures.load(Ordering::Relaxed);
                if n > 0 {
                    let label = to_wide(&format!("認証失敗: {n} 回 (起動以降)"));
                    // AppendMenuW は文字列を内部コピーするので label は直後に drop してよい。
                    let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(label.as_ptr()));
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
            }
            let _ = AppendMenuW(menu, MF_STRING, ID_CHANGE, w!("パスワードを変更..."));
            let _ = AppendMenuW(menu, MF_STRING, ID_PORT, w!("ポートを変更..."));
            let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_LOGS, w!("ログフォルダを開く"));

            // ログオン時自動起動のトグル。現在の登録状態をチェックマークで示す。`_autostart`
            // はラベルの UTF-16 バッファを TrackPopupMenu まで生かすための保持先 (AppendMenuW は
            // 内部コピーするが、後続のサブメニュー構築をまたぐため明示的に束ねる)。
            let installed = crate::autostart::is_installed();
            let autostart_label = to_wide(if installed {
                "ログオン時に自動起動: 有効 (クリックで無効化)"
            } else {
                "ログオン時に自動起動: 無効 (クリックで有効化)"
            });
            let autostart_flags = MF_STRING | if installed { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                menu,
                autostart_flags,
                ID_AUTOSTART,
                PCWSTR(autostart_label.as_ptr()),
            );
            let _autostart = autostart_label;

            // デバイスピン留め (2 要素目)。有効時のみペアリング/端末一覧を出す。
            // `_keep` は AppendMenuW へ渡す UTF-16 バッファを TrackPopupMenu まで生かすための保持先
            // (AppendMenuW は文字列をコピーするが、サブメニュー構築をまたぐので明示的に束ねる)。
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let mut _keep: Vec<Vec<u16>> = Vec::new();
            // サブメニューは menu に MF_POPUP で挿入すると所有権が menu 側に移り、menu 破棄時に
            // 一緒に破棄される (個別破棄しない)。
            let mut _submenus: Vec<HMENU> = Vec::new();
            if let Some(state) = wnd_state(hwnd) {
                let enabled = state.devices.enabled();
                let toggle = to_wide(if enabled {
                    "デバイスピン留め: 有効 (クリックで無効化)"
                } else {
                    "デバイスピン留め: 無効 (クリックで有効化)"
                });
                let toggle_flags = MF_STRING | if enabled { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(menu, toggle_flags, ID_PIN_TOGGLE, PCWSTR(toggle.as_ptr()));
                _keep.push(toggle);
                if enabled {
                    // ペアリング: 開いていれば「取消」、閉じていれば「次の接続を登録」。
                    let armed = state.devices.pairing_armed();
                    let pair = to_wide(if armed {
                        "ペアリング窓を閉じる (登録待機を取消)"
                    } else {
                        "次の接続を登録 (ペアリング)..."
                    });
                    let _ = AppendMenuW(menu, MF_STRING, ID_PIN_PAIR, PCWSTR(pair.as_ptr()));
                    _keep.push(pair);

                    // 登録済みデバイスをサブメニューに列挙する (各項目クリックでその端末を削除)。
                    let labels = state.devices.labels();
                    if let Ok(sub) = CreatePopupMenu() {
                        if labels.is_empty() {
                            let none = to_wide("(登録なし)");
                            let _ =
                                AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, PCWSTR(none.as_ptr()));
                            _keep.push(none);
                        } else {
                            for (i, label) in labels.iter().take(MAX_DEVICE_ITEMS).enumerate() {
                                let item = to_wide(&format!("{label}  (クリックで削除)"));
                                let _ = AppendMenuW(
                                    sub,
                                    MF_STRING,
                                    ID_DEVICE_BASE + i,
                                    PCWSTR(item.as_ptr()),
                                );
                                _keep.push(item);
                            }
                            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                            let clear = to_wide("すべて削除...");
                            let _ =
                                AppendMenuW(sub, MF_STRING, ID_PIN_CLEAR, PCWSTR(clear.as_ptr()));
                            _keep.push(clear);
                        }
                        let head = to_wide(&format!("登録済みデバイス ({} 台)", labels.len()));
                        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(head.as_ptr()));
                        _keep.push(head);
                        _submenus.push(sub);
                    }
                }
            }

            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("終了"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // メニューが正しく閉じるよう自ウィンドウを前面化する (Win32 の定石)。
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
            let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }

    /// パスワード変更ダイアログを表示し、入力されたら DPAPI 保存 + 共有 PSK を更新する。
    fn change_password(hwnd: HWND) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        // 接続中はリモート操作者による締め出し/乗っ取りを防ぐため変更を拒否する。
        if crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中はパスワードを変更できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        // 既にパスワードが設定済みなら現在のパスワードを要求して照合する。未設定 (初回/
        // ランダム) のときは現在パスワード欄を出さない。
        let expect_current = if state.has_password.load(Ordering::Relaxed) {
            state.psk.lock().ok().map(|g| *g)
        } else {
            None
        };
        if let Some(pw) = prompt_password(hwnd, expect_current) {
            if let Err(e) = crate::secret::save_password(&pw) {
                message_box(hwnd, &format!("保存に失敗しました: {e}"), "Aurora Host");
                return;
            }
            if let Ok(mut g) = state.psk.lock() {
                *g = psk_from_password(&pw);
            }
            state.has_password.store(true, Ordering::Relaxed);
            message_box(
                hwnd,
                "パスワードを変更しました (次の接続から有効)。",
                "Aurora Host",
            );
        }
    }

    /// ポート変更ダイアログを表示し、入力されたら保存 + 共有ポートを更新して reload を通知する
    /// (net の accept ループが TCP/UDP を貼り直して再起動なしに反映)。
    fn change_port(hwnd: HWND) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        // 接続中はソケット貼り直しで相手を切ってしまうため拒否する (パスワード変更と同じ方針)。
        if crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中はポートを変更できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        let current = state.port.load(Ordering::Relaxed);
        if let Some(new_port) = prompt_port(hwnd, current) {
            if new_port == current {
                return; // 変更なし
            }
            // 新ポートで実際に bind できるか事前確認する。net 側の貼り直しが失敗すると host
            // ごと終了してしまうため、使用中ポート等は保存前にここで弾く。
            if let Err(e) = port_available(new_port) {
                message_box(
                    hwnd,
                    &format!("ポート {new_port} は使用できません: {e}"),
                    "Aurora Host",
                );
                return;
            }
            if let Err(e) = crate::settings::save_port(new_port) {
                message_box(hwnd, &format!("保存に失敗しました: {e}"), "Aurora Host");
                return;
            }
            state.port.store(new_port, Ordering::Relaxed);
            // accept ループを起こして TCP/UDP を新ポートで貼り直させる。
            state.reload.notify_one();
            message_box(
                hwnd,
                &format!("待受ポートを {new_port} に変更しました。"),
                "Aurora Host",
            );
        }
    }

    /// ログフォルダ (`%LOCALAPPDATA%\Aurora\logs`) をエクスプローラで開く。未作成なら作る
    /// (起動直後でまだ書き込みが無い場合に空フォルダを開けるように)。
    fn open_logs_folder(hwnd: HWND) {
        let Some(dir) = aurora_common::logging::default_log_dir() else {
            message_box(
                hwnd,
                "ログフォルダの場所を特定できませんでした。",
                "Aurora Host",
            );
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        // explorer で開く。GUI サブシステムのため標準の関連付け起動でよい。
        if std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .is_err()
        {
            message_box(
                hwnd,
                &format!("ログフォルダ:\n{}", dir.display()),
                "Aurora Host",
            );
        }
    }

    /// ログオン時自動起動 (タスクスケジューラの ONLOGON タスク) の有効/無効をトグルする。
    /// 現在登録されていれば解除し、未登録なら現プロセスの起動引数を転送して登録する。
    /// `schtasks /Create` は `/RL HIGHEST` で登録するため管理者権限を要する場合があり、失敗時は
    /// その旨をメッセージで案内する。接続中でも安全な操作 (映像/入力経路に影響しない) のため
    /// 接続中ガードは設けない。
    fn toggle_autostart(hwnd: HWND) {
        if crate::autostart::is_installed() {
            match crate::autostart::uninstall() {
                Ok(()) => message_box(
                    hwnd,
                    "ログオン時の自動起動を無効にしました。",
                    "Aurora Host",
                ),
                Err(e) => message_box(
                    hwnd,
                    &format!(
                        "自動起動の解除に失敗しました: {e}\n管理者権限が必要な場合があります。"
                    ),
                    "Aurora Host",
                ),
            }
        } else {
            let args = crate::autostart::current_forward_args();
            match crate::autostart::install(&args) {
                Ok(()) => message_box(
                    hwnd,
                    "ログオン時に自動起動するようにしました。",
                    "Aurora Host",
                ),
                Err(e) => message_box(
                    hwnd,
                    &format!(
                        "自動起動の登録に失敗しました: {e}\n管理者権限が必要な場合があります。"
                    ),
                    "Aurora Host",
                ),
            }
        }
    }

    /// デバイスピン留めの有効/無効を切り替える。**無効化は接続中拒否**: 接続中の操作者が
    /// 入力注入でトレイを操作してピン留めを外す (= 2 要素目の無効化) のを防ぐ (パスワード/
    /// ポート変更と同じ方針)。有効化は保護を強める方向なので接続中でも許可する。
    fn toggle_pinning(hwnd: HWND) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        let currently_on = state.devices.enabled();
        if currently_on && crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中はデバイスピン留めを無効化できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        state.devices.set_enabled(!currently_on);
        let msg = if currently_on {
            "デバイスピン留めを無効化しました (パスワードのみで接続可)。"
        } else {
            "デバイスピン留めを有効化しました。\n登録済みの端末のみ接続できます。\n新しい端末は「次の接続を登録」で追加してください。"
        };
        message_box(hwnd, msg, "Aurora Host");
    }

    /// ペアリング窓をトグルする。開いていれば取り消し、閉じていれば開く。**開く操作は接続中
    /// 拒否**: 接続中の操作者が自分の別端末を信頼登録して永続アクセスを得るのを防ぐ (取消は
    /// 保護を弱めないので接続中でも許可)。窓は開いたままだと「次の認証成功端末を登録」状態が
    /// 続くため、誤って開けたままにしたときに閉じられるよう取消を用意する。
    fn toggle_pairing(hwnd: HWND) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        if state.devices.pairing_armed() {
            state.devices.disarm_pairing();
            message_box(
                hwnd,
                "ペアリング窓を閉じました (登録待機を取り消しました)。",
                "Aurora Host",
            );
            return;
        }
        if crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中は新しい端末を登録できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        state.devices.arm_pairing();
        message_box(
            hwnd,
            "ペアリング窓を開きました。\n登録したい端末から今すぐ 1 回接続してください\n(次に認証を通した 1 台が信頼登録され、窓は閉じます)。\n登録しない場合はメニューから取り消せます。",
            "Aurora Host",
        );
    }

    /// 登録端末リストの `index` 番目を削除する (確認付き)。**接続中拒否** + 破壊的操作のため確認。
    fn remove_device(hwnd: HWND, index: usize) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        if crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中は登録端末を削除できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        // 表示時のラベルで確認する (index は表示順と一致)。
        let label = state
            .devices
            .labels()
            .get(index)
            .cloned()
            .unwrap_or_default();
        let text = to_wide(&format!(
            "登録端末「{label}」を削除しますか?\nこの端末はピン留め有効中に接続できなくなります (再ペアリングが必要)。"
        ));
        let caption = to_wide("Aurora Host");
        // SAFETY: text/caption は NUL 終端の UTF-16 で、呼び出し中は生存している。
        let r = unsafe {
            MessageBoxW(
                hwnd,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_YESNO | MB_ICONWARNING,
            )
        };
        if r != IDYES {
            return;
        }
        match state.devices.remove(index) {
            Some(removed) => message_box(
                hwnd,
                &format!("「{removed}」を削除しました。"),
                "Aurora Host",
            ),
            None => message_box(
                hwnd,
                "削除できませんでした (一覧が変わった可能性)。",
                "Aurora Host",
            ),
        }
    }

    /// 登録済み信頼端末をすべて削除する (確認付き)。**接続中拒否** + 破壊的操作のため確認。
    fn clear_devices(hwnd: HWND) {
        let Some(state) = (unsafe { wnd_state(hwnd) }) else {
            return;
        };
        if crate::conn::is_active() {
            message_box(
                hwnd,
                "クライアント接続中は登録端末を削除できません。\n切断してから操作してください。",
                "Aurora Host",
            );
            return;
        }
        let text = to_wide(
            "登録済みの信頼端末をすべて削除しますか?\nピン留め有効中はどの端末も接続できなくなります (再ペアリングが必要)。",
        );
        let caption = to_wide("Aurora Host");
        // SAFETY: text/caption は NUL 終端の UTF-16 で、呼び出し中は生存している。
        let r = unsafe {
            MessageBoxW(
                hwnd,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_YESNO | MB_ICONWARNING,
            )
        };
        if r != IDYES {
            return;
        }
        state.devices.clear();
        message_box(hwnd, "登録端末をすべて削除しました。", "Aurora Host");
    }

    /// 新ポートで TCP/UDP を実際に bind できるか事前確認する。確認後ソケットは drop で解放され、
    /// 直後に net 側が同ポートへ貼り直す (単一ユーザー host のため間の競合は実質起きない)。
    fn port_available(port: u16) -> std::result::Result<(), String> {
        use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
        let _tcp = TcpListener::bind(addr).map_err(|e| format!("TCP {e}"))?;
        let _udp = UdpSocket::bind(addr).map_err(|e| format!("UDP {e}"))?;
        Ok(())
    }

    /// 指定サイズのウィンドウを画面中央に置くための左上座標を返す。プログラム生成ダイアログは
    /// `CW_USEDEFAULT` だと画面左上に寄って使いにくいため、これで中央寄せする。取得失敗時は
    /// `CW_USEDEFAULT` を返してフォールバックする。
    fn centered_pos(w: i32, h: i32) -> (i32, i32) {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        // SAFETY: スクリーンメトリクスを読むだけ。
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if sw > 0 && sh > 0 {
            (((sw - w) / 2).max(0), ((sh - h) / 2).max(0))
        } else {
            (CW_USEDEFAULT, CW_USEDEFAULT)
        }
    }

    /// 簡易メッセージボックス。
    fn message_box(owner: HWND, text: &str, caption: &str) {
        let t = to_wide(text);
        let c = to_wide(caption);
        unsafe {
            MessageBoxW(
                owner,
                PCWSTR(t.as_ptr()),
                PCWSTR(c.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    // --- パスワード入力ダイアログ (プログラム生成、リソース不要) ---

    /// ダイアログの入力結果と検証材料。WndProc から参照/書き戻すため `GWLP_USERDATA` に置く。
    struct DlgResult {
        /// OK が押され検証を通った新パスワード。
        value: Option<String>,
        /// `Some` のとき「現在のパスワード」欄を検証する (期待する現行 PSK)。
        expect_current: Option<[u8; 32]>,
    }

    /// パスワード設定/変更ダイアログを表示する。`expect_current` が `Some` のときは「現在の
    /// パスワード」欄を追加し、入力が現行 PSK と一致しないと OK を受け付けない。
    fn prompt_password(owner: HWND, expect_current: Option<[u8; 32]>) -> Option<String> {
        let require_current = expect_current.is_some();
        unsafe {
            let hmod = GetModuleHandleW(None).ok()?;
            let hinstance = HINSTANCE(hmod.0);
            let class_name = w!("AuroraPwDlg");

            // 専用クラスを (毎回) 登録する。RegisterClassW は二重登録でも害がない。
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dlg_proc),
                hInstance: hinstance,
                lpszClassName: class_name,
                hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).ok()?,
                hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
                ..Default::default()
            };
            RegisterClassW(&wc);

            let mut result = Box::new(DlgResult {
                value: None,
                expect_current,
            });
            let result_ptr: *mut DlgResult = result.as_mut();

            let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
            // 現在パスワード欄の有無で 1 行ぶん高さを変える。
            let (w, h) = if require_current {
                (340, 280)
            } else {
                (340, 216)
            };
            let (x, y) = centered_pos(w, h);
            let dlg = match CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                class_name,
                w!("接続パスワードの設定"),
                style,
                x,
                y,
                w,
                h,
                owner,
                HMENU::default(),
                hinstance,
                Some(result_ptr as *const c_void),
            ) {
                Ok(h) => h,
                Err(_) => return None,
            };

            let font = ui_font();
            let mk_label = |y: i32, text: PCWSTR| {
                let h = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    text,
                    WS_CHILD | WS_VISIBLE,
                    16,
                    y,
                    300,
                    18,
                    dlg,
                    HMENU::default(),
                    hinstance,
                    None,
                )
                .ok();
                if let Some(h) = h {
                    SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                }
            };
            let mk_edit = |y: i32, id: i32| -> Option<HWND> {
                let edit = CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | WINDOW_STYLE((ES_PASSWORD | ES_AUTOHSCROLL) as u32),
                    16,
                    y,
                    300,
                    26,
                    dlg,
                    HMENU(id as *mut c_void),
                    hinstance,
                    None,
                )
                .ok()?;
                SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                Some(edit)
            };

            // 現在パスワードを要求する場合は先頭に 1 行追加し、以降を 58px ずつ下げる。
            let mut y = 16;
            if require_current {
                mk_label(y, w!("現在のパスワード"));
                let _ = mk_edit(y + 20, ID_EDIT_CUR);
                y += 58;
            }
            mk_label(y, w!("新しいパスワード"));
            let _ = mk_edit(y + 20, ID_EDIT);
            mk_label(y + 58, w!("新しいパスワード (確認)"));
            let _ = mk_edit(y + 78, ID_EDIT2);
            let button_y = y + 120;

            // OK / キャンセルボタン。
            let mk_button = |x: i32, id: usize, text: PCWSTR, default: bool| {
                let extra = if default { BS_DEFPUSHBUTTON as u32 } else { 0 };
                let b = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    text,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(extra),
                    x,
                    button_y,
                    96,
                    28,
                    dlg,
                    HMENU(id as *mut c_void),
                    hinstance,
                    None,
                )
                .ok();
                if let Some(b) = b {
                    SendMessageW(b, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                }
            };
            mk_button(120, IDOK.0 as usize, w!("OK"), true);
            mk_button(224, IDCANCEL.0 as usize, w!("キャンセル"), false);

            // モーダル化: オーナーを無効化し、破棄まで自前のメッセージループを回す。
            let _ = EnableWindow(owner, BOOL::from(false));
            // ダイアログ自体は WS_VISIBLE 無しで生成しているので明示的に表示する
            // (これが無いと子コントロールはあっても親が不可視のまま何も出ない)。
            let _ = ShowWindow(dlg, SW_SHOW);
            let _ = SetForegroundWindow(dlg);
            // 先頭欄にフォーカス (現在パスワード欄があればそちらを優先)。
            let first_id = if require_current {
                ID_EDIT_CUR
            } else {
                ID_EDIT
            };
            if let Ok(first) = GetDlgItem(dlg, first_id) {
                let _ = SetFocus(first);
            }

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                if !IsWindow(dlg).as_bool() {
                    break;
                }
                if !IsDialogMessageW(dlg, &msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            let _ = EnableWindow(owner, BOOL::from(true));
            let _ = SetForegroundWindow(owner);
            // 生成したフォントを解放する (ダイアログ破棄後)。
            let _ = DeleteObject(HGDIOBJ(font.0));

            result.value.take()
        }
    }

    /// ダイアログのウィンドウプロシージャ。
    extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_CREATE => {
                    let cs = lparam.0 as *const CREATESTRUCTW;
                    if !cs.is_null() {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                    }
                    LRESULT(0)
                }
                WM_COMMAND => {
                    let id = (wparam.0 & 0xffff) as i32;
                    if id == IDOK.0 {
                        on_ok(hwnd);
                    } else if id == IDCANCEL.0 {
                        let _ = DestroyWindow(hwnd);
                    }
                    LRESULT(0)
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// OK 押下: (現在パスワードが要求されていれば照合し) 新旧 2 欄を検証して、問題なければ
    /// 結果へ格納しダイアログを閉じる。検証に失敗したときはダイアログを開いたままにする。
    unsafe fn on_ok(hwnd: HWND) {
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DlgResult;
            let expect_current = if ptr.is_null() {
                None
            } else {
                (*ptr).expect_current
            };

            // 現在パスワードの照合 (要求時のみ)。一致しなければ閉じない。
            if let Some(expected) = expect_current {
                let cur = match GetDlgItem(hwnd, ID_EDIT_CUR) {
                    Ok(h) => get_text(h),
                    Err(_) => return,
                };
                if psk_from_password(&cur) != expected {
                    message_box(hwnd, "現在のパスワードが違います。", "Aurora Host");
                    return;
                }
            }

            let pw1 = match GetDlgItem(hwnd, ID_EDIT) {
                Ok(h) => get_text(h),
                Err(_) => return,
            };
            let pw2 = match GetDlgItem(hwnd, ID_EDIT2) {
                Ok(h) => get_text(h),
                Err(_) => return,
            };
            if pw1.chars().count() < aurora_common::noise::MIN_PASSWORD_LEN {
                let msg = format!(
                    "パスワードは{}文字以上で入力してください。",
                    aurora_common::noise::MIN_PASSWORD_LEN
                );
                message_box(hwnd, &msg, "Aurora Host");
                return;
            }
            if pw1 != pw2 {
                message_box(hwnd, "確認用パスワードが一致しません。", "Aurora Host");
                return;
            }
            // 最低長は満たすが弱い兆候があればソフト警告 (はい=続行 / いいえ=再考)。CLI の
            // --set-password と同じ判定を使い、設定経路の防御を対称に保つ。
            if let Some(reason) = aurora_common::noise::weak_password_reason(&pw1) {
                let text = to_wide(&format!(
                    "{reason}。\nより強いパスワードを推奨します (例: {})。\n\nこのまま設定しますか?",
                    crate::suggest_passphrase()
                ));
                let caption = to_wide("Aurora Host");
                let r = MessageBoxW(
                    hwnd,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(caption.as_ptr()),
                    MB_YESNO | MB_ICONWARNING,
                );
                if r != IDYES {
                    return; // ダイアログは開いたまま (パスワードを選び直せる)
                }
            }
            if !ptr.is_null() {
                (*ptr).value = Some(pw1);
            }
            let _ = DestroyWindow(hwnd);
        }
    }

    /// EDIT コントロールの現在テキストを取得する。
    unsafe fn get_text(edit: HWND) -> String {
        unsafe {
            let len = GetWindowTextLengthW(edit);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(edit, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    // --- ポート入力ダイアログ (プログラム生成、リソース不要) ---

    /// ポート変更ダイアログの入力結果。`GWLP_USERDATA` 経由で WndProc と受け渡しする。
    struct PortDlgResult {
        value: Option<u16>,
    }

    /// 待受ポート変更ダイアログを表示する。`current` を初期値に 1 欄だけ出し、1..=65535 を
    /// 検証する。OK で有効値なら `Some(port)` を返す。
    fn prompt_port(owner: HWND, current: u16) -> Option<u16> {
        unsafe {
            let hmod = GetModuleHandleW(None).ok()?;
            let hinstance = HINSTANCE(hmod.0);
            let class_name = w!("AuroraPortDlg");

            let wc = WNDCLASSW {
                lpfnWndProc: Some(port_dlg_proc),
                hInstance: hinstance,
                lpszClassName: class_name,
                hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).ok()?,
                hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
                ..Default::default()
            };
            RegisterClassW(&wc);

            let mut result = Box::new(PortDlgResult { value: None });
            let result_ptr: *mut PortDlgResult = result.as_mut();

            let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
            let (dlg_w, dlg_h) = (340, 192);
            let (x, y) = centered_pos(dlg_w, dlg_h);
            let dlg = match CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                class_name,
                w!("待受ポートの変更"),
                style,
                x,
                y,
                dlg_w,
                dlg_h,
                owner,
                HMENU::default(),
                hinstance,
                Some(result_ptr as *const c_void),
            ) {
                Ok(h) => h,
                Err(_) => return None,
            };

            let font = ui_font();
            let label = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("新しいポート (1-65535):"),
                WS_CHILD | WS_VISIBLE,
                16,
                16,
                300,
                18,
                dlg,
                HMENU::default(),
                hinstance,
                None,
            )
            .ok();
            if let Some(h) = label {
                SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }

            // 数値専用 (ES_NUMBER) の入力欄。現在のポートを初期表示する。
            let init = to_wide(&current.to_string());
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                PCWSTR(init.as_ptr()),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL) as u32),
                16,
                40,
                300,
                26,
                dlg,
                HMENU(ID_EDIT_PORT as *mut c_void),
                hinstance,
                None,
            )
            .ok();
            if let Some(e) = edit {
                SendMessageW(e, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }

            let button_y = 102;
            let mk_button = |x: i32, id: usize, text: PCWSTR, default: bool| {
                let extra = if default { BS_DEFPUSHBUTTON as u32 } else { 0 };
                let b = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    text,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(extra),
                    x,
                    button_y,
                    96,
                    28,
                    dlg,
                    HMENU(id as *mut c_void),
                    hinstance,
                    None,
                )
                .ok();
                if let Some(b) = b {
                    SendMessageW(b, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                }
            };
            mk_button(120, IDOK.0 as usize, w!("OK"), true);
            mk_button(224, IDCANCEL.0 as usize, w!("キャンセル"), false);

            // モーダル化: オーナーを無効化し、破棄まで自前のメッセージループを回す。
            let _ = EnableWindow(owner, BOOL::from(false));
            let _ = ShowWindow(dlg, SW_SHOW);
            let _ = SetForegroundWindow(dlg);
            if let Ok(first) = GetDlgItem(dlg, ID_EDIT_PORT) {
                let _ = SetFocus(first);
            }

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                if !IsWindow(dlg).as_bool() {
                    break;
                }
                if !IsDialogMessageW(dlg, &msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            let _ = EnableWindow(owner, BOOL::from(true));
            let _ = SetForegroundWindow(owner);
            // 生成したフォントを解放する (ダイアログ破棄後)。
            let _ = DeleteObject(HGDIOBJ(font.0));

            result.value.take()
        }
    }

    /// ポート変更ダイアログのウィンドウプロシージャ。
    extern "system" fn port_dlg_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_CREATE => {
                    let cs = lparam.0 as *const CREATESTRUCTW;
                    if !cs.is_null() {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                    }
                    LRESULT(0)
                }
                WM_COMMAND => {
                    let id = (wparam.0 & 0xffff) as i32;
                    if id == IDOK.0 {
                        on_port_ok(hwnd);
                    } else if id == IDCANCEL.0 {
                        let _ = DestroyWindow(hwnd);
                    }
                    LRESULT(0)
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// OK 押下: 入力を 1..=65535 で検証し、問題なければ結果へ格納して閉じる。不正値は
    /// ダイアログを開いたままにする。
    unsafe fn on_port_ok(hwnd: HWND) {
        unsafe {
            let text = match GetDlgItem(hwnd, ID_EDIT_PORT) {
                Ok(h) => get_text(h),
                Err(_) => return,
            };
            let port: u16 = match text.trim().parse() {
                Ok(p) if p != 0 => p,
                _ => {
                    message_box(
                        hwnd,
                        "1〜65535 のポート番号を入力してください。",
                        "Aurora Host",
                    );
                    return;
                }
            };
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PortDlgResult;
            if !ptr.is_null() {
                (*ptr).value = Some(port);
            }
            let _ = DestroyWindow(hwnd);
        }
    }
}
