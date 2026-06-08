//! システムトレイ常駐 UI。アイコンの右クリックメニューから接続パスワードの設定・変更を
//! 行う。変更は DPAPI に保存し、共有 PSK ([`SharedPsk`]) を更新して **次の接続から** 反映する
//! (既存セッションは現行 PSK のまま継続)。
//!
//! Windows のみ実体を持つ。非 Windows では [`spawn`] は何もしない。

use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::{Arc, Mutex};

/// host 全体で共有する接続 PSK。net 層が接続受理時に読み、tray がパスワード変更時に書く。
pub type SharedPsk = Arc<Mutex<[u8; 32]>>;

/// 実パスワードが設定済みかを共有するフラグ。設定済みなら変更時に現在パスワードを要求する。
pub type SharedHasPassword = Arc<AtomicBool>;

/// host 全体で共有する待受ポート。tray がポート変更で書き、net が reload 後に読み直す。
pub type SharedPort = Arc<AtomicU16>;

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
    prompt_now: bool,
) {
    let _ = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = imp::run(psk, has_password, port, reload, prompt_now) {
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
    _prompt_now: bool,
) {
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{Reload, SharedHasPassword, SharedPort, SharedPsk};
    use aurora_common::noise::psk_from_password;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        COLOR_BTNFACE, DEFAULT_GUI_FONT, GetStockObject, GetSysColorBrush,
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
    }

    /// UTF-16 NUL 終端へ変換する (固定長配列へのコピー用)。
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run(
        psk: SharedPsk,
        has_password: SharedHasPassword,
        port: SharedPort,
        reload: Reload,
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
                    match wparam.0 & 0xffff {
                        ID_CHANGE => change_password(hwnd),
                        ID_PORT => change_port(hwnd),
                        ID_EXIT => {
                            // host プロセス全体を終了する。
                            let _ = DestroyWindow(hwnd);
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
            let _ = AppendMenuW(menu, MF_STRING, ID_CHANGE, w!("パスワードを変更..."));
            let _ = AppendMenuW(menu, MF_STRING, ID_PORT, w!("ポートを変更..."));
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

    /// 新ポートで TCP/UDP を実際に bind できるか事前確認する。確認後ソケットは drop で解放され、
    /// 直後に net 側が同ポートへ貼り直す (単一ユーザー host のため間の競合は実質起きない)。
    fn port_available(port: u16) -> std::result::Result<(), String> {
        use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
        let _tcp = TcpListener::bind(addr).map_err(|e| format!("TCP {e}"))?;
        let _udp = UdpSocket::bind(addr).map_err(|e| format!("UDP {e}"))?;
        Ok(())
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
                (340, 254)
            } else {
                (340, 200)
            };
            let dlg = match CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                class_name,
                w!("接続パスワードの設定"),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
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

            let font = GetStockObject(DEFAULT_GUI_FONT);
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
                    24,
                    dlg,
                    HMENU(id as *mut c_void),
                    hinstance,
                    None,
                )
                .ok()?;
                SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                Some(edit)
            };

            // 現在パスワードを要求する場合は先頭に 1 行追加し、以降を 54px ずつ下げる。
            let mut y = 12;
            if require_current {
                mk_label(y, w!("現在のパスワード:"));
                let _ = mk_edit(y + 20, ID_EDIT_CUR);
                y += 54;
            }
            mk_label(y, w!("新しいパスワード:"));
            let _ = mk_edit(y + 20, ID_EDIT);
            mk_label(y + 54, w!("確認のため再入力:"));
            let _ = mk_edit(y + 74, ID_EDIT2);
            let button_y = y + 116;

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
            if pw1.is_empty() {
                message_box(hwnd, "パスワードを入力してください。", "Aurora Host");
                return;
            }
            if pw1 != pw2 {
                message_box(hwnd, "確認用パスワードが一致しません。", "Aurora Host");
                return;
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
            let dlg = match CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                class_name,
                w!("待受ポートの変更"),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                340,
                184,
                owner,
                HMENU::default(),
                hinstance,
                Some(result_ptr as *const c_void),
            ) {
                Ok(h) => h,
                Err(_) => return None,
            };

            let font = GetStockObject(DEFAULT_GUI_FONT);
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
                38,
                300,
                24,
                dlg,
                HMENU(ID_EDIT_PORT as *mut c_void),
                hinstance,
                None,
            )
            .ok();
            if let Some(e) = edit {
                SendMessageW(e, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }

            let button_y = 96;
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
