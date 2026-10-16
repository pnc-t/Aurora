//! トレイ常駐 UI (Win32)。表示と入力だけを受け持ち、操作は [`TrayControl`] へ要求として投げる。
//!
//! 同じ UI が 2 つの形で動く:
//! - **同一プロセス** (開発実行・旧 ONLOGON 運用): host 本体のスレッドとして動き、
//!   [`super::LocalControl`] で直接操作する。「終了」は host ごと終了する。
//! - **単独プロセス** (`--tray`、SYSTEM サービス運用): ログオンユーザー権限で動き、名前付きパイプで
//!   本体に要求を送る。「トレイを閉じる」はトレイだけを閉じる (本体はサービスが維持する)。
//!   SYSTEM の本体が対話デスクトップへ UI を出さないための分離 (SYSTEM 所有のダイアログは
//!   ローカル利用者への権限昇格穴になる)。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontW, DeleteObject, GetSysColorBrush, HFONT, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::{TrayControl, TrayEvent, TrayReply, TrayRequest, TrayStatus};

/// トレイアイコンからの通知を受けるカスタムメッセージ。
const WM_TRAY: u32 = WM_APP + 1;
/// build.rs が exe へ埋め込む aurora アイコンのリソース ID (`set_icon_with_id` と対)。
const ICON_RESOURCE_ID: u16 = 1;

/// `MAKEINTRESOURCEW` 相当。Win32 は上位ワードが 0 のポインタを「文字列名ではなく数値 ID」と
/// 解釈するため、ID をそのままアドレス値として PCWSTR に載せる (逆参照はされない)。
///
/// `std::ptr::dangling::<u16>()` で代用してはいけない。あれは u16 のアラインメント値 = 2 を返す
/// ので、ID 1 のつもりが ID 2 を引いて `LoadIconW` が失敗する (実際にトレイが標準アイコンに
/// フォールバックしていた)。
fn int_resource(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}
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
/// 更新の適用要求メニュー項目 ID。
const ID_UPDATE: usize = 1009;
/// 登録端末リストの各項目 ID のベース (`ID_DEVICE_BASE + index`)。クリックでその端末を削除。
const ID_DEVICE_BASE: usize = 2000;
/// リスト表示する登録端末の上限 (ID 空間を有界にする。通常これを超える端末は持たない)。
const MAX_DEVICE_ITEMS: usize = 64;
/// 承認待ちデバイスリストの各項目 ID のベース (`ID_PENDING_BASE + index`)。クリックで承認する。
const ID_PENDING_BASE: usize = 3000;
/// リスト表示する承認待ちデバイスの上限 (devices 側の保留上限 MAX_PENDING と揃える必要はないが
/// ID 空間を有界にする)。
const MAX_PENDING_ITEMS: usize = 8;
/// 変更ダイアログ内コントロール ID。
const ID_EDIT_CUR: i32 = 100;
const ID_EDIT: i32 = 101;
const ID_EDIT2: i32 = 102;
/// ポート変更ダイアログの入力欄 ID。
const ID_EDIT_PORT: i32 = 110;

/// ステータス監視タイマの ID と間隔 (ms)。接続数の変化検知・通知バルーン・ツールチップ更新を
/// 1 秒周期で回す (常駐 UI のため軽量・低頻度で十分)。
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 1000;
/// 認証失敗バルーンの再通知抑止 (tick 数)。総当たり中に毎秒鳴らさないため。
const AUTH_BALLOON_COOLDOWN: u32 = 30;

/// UI がどの形で動いているか (モジュール doc 参照)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrayMode {
    /// host 本体と同一プロセス。「終了」で host ごと終了する。
    InProcess,
    /// `--tray` の単独プロセス。「トレイを閉じる」でトレイだけ閉じる。
    Standalone,
}

/// 隠しウィンドウに紐づける状態。`GWLP_USERDATA` にこの Box のポインタを格納する。
///
/// `Cell`/`RefCell` の項目は tray スレッド単独でのみ触る (本体の状態は `control` 越し)。
struct WndState {
    control: Box<dyn TrayControl>,
    mode: TrayMode,
    /// 直近に取得した本体の状態。本体に届かないとき (再起動中等) は `None`。
    status: RefCell<Option<TrayStatus>>,
    /// 前 tick の確立済みクライアント数 (変化時のみツールチップを更新)。`usize::MAX` は未取得。
    last_connected: Cell<usize>,
    /// 前 tick の累計認証失敗数 (増分でバルーン通知)。`None` は未取得 (初回は基準にするだけ)。
    last_auth: Cell<Option<u32>>,
    /// 認証失敗バルーンのクールダウン残 tick。
    auth_cooldown: Cell<u32>,
    /// 本体の状態を一度でも取得できたか。単独トレイはサービスが本体とほぼ同時に起動するため、
    /// 本体がパイプを開く (キャプチャ/エンコーダ初期化の後) まで数秒つながらない。初回パスワード
    /// ダイアログの要否は、起動時ではなく最初に取得できた時点で判定する。
    first_status_seen: Cell<bool>,
}

impl WndState {
    /// 状態を初めて取得できた時点で一度だけ評価し、パスワード未設定なら `true` (初回ダイアログを
    /// 出す)。まだ取得できていない・評価済み・設定済みはすべて `false`。
    fn take_first_status_prompt(&self) -> bool {
        if self.first_status_seen.get() {
            return false;
        }
        let Some(has_password) = self.status.borrow().as_ref().map(|s| s.has_password) else {
            return false;
        };
        self.first_status_seen.set(true);
        !has_password
    }

    /// 本体へ状態を問い合わせてキャッシュを更新し、溜まっていた通知イベントを返す。
    fn refresh(&self) -> Vec<TrayEvent> {
        match self.control.call(TrayRequest::Status) {
            TrayReply::Status(mut s) => {
                let events = std::mem::take(&mut s.events);
                *self.status.borrow_mut() = Some(*s);
                events
            }
            _ => {
                *self.status.borrow_mut() = None;
                Vec::new()
            }
        }
    }
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

/// トレイ UI を動かす (メッセージループが終わるまで戻らない)。本体にパスワードが未設定なら
/// 起動直後に設定ダイアログを出す (保存パスワードが無い初回向け)。
///
/// `InProcess` で「終了」が選ばれたときは host ごと終了する (戻らない)。`Standalone` では
/// トレイを閉じたら `Ok(())` で戻る。
pub fn run(control: Box<dyn TrayControl>, mode: TrayMode) -> windows::core::Result<()> {
    // SAFETY: トレイ用隠しウィンドウ生成一式。各 Win32 呼び出しの結果は ? で確認する。
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

        let state = WndState {
            control,
            mode,
            status: RefCell::new(None),
            last_connected: Cell::new(usize::MAX),
            last_auth: Cell::new(None),
            auth_cooldown: Cell::new(0),
            first_status_seen: Cell::new(false),
        };
        // 起動前に溜まっていた通知 (再起動前の接続など) はバルーンにしない。
        let _ = state.refresh();
        let prompt_now = state.take_first_status_prompt();
        // 共有状態をウィンドウに紐づける (WM_CREATE で GWLP_USERDATA へ格納)。
        let boxed: *mut WndState = Box::into_raw(Box::new(state));

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

        add_tray_icon(hwnd)?;
        // explorer 再起動後の再登録通知を受けられるよう、メッセージ ID を先に確定させる。
        let _ = taskbar_created_msg();

        // ステータス監視タイマを開始する。本体の状態をポーリングし、ツールチップ更新と
        // バルーン通知へ反映する。
        let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);

        // 初回 (保存パスワード無し) は起動直後に変更ダイアログを出す (本体にまだ届かなければ
        // on_timer が最初に届いた時点で出す)。
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
        // (初期化失敗時はループ前に return する)。
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        drop(Box::from_raw(boxed));
    }
    if mode == TrayMode::Standalone {
        tracing::info!("tray closed by user");
        return Ok(());
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

/// トレイアイコンを登録する。起動時と、explorer 再起動 (TaskbarCreated) 後の再登録で共用する。
/// build.rs で exe へ埋め込んだ aurora アイコン (リソース ID = 1) を使い、読めなければ標準
/// アプリアイコンへフォールバックする。ツールチップは現在の接続状態で組み立てる。
fn add_tray_icon(hwnd: HWND) -> windows::core::Result<()> {
    // SAFETY: 自プロセスのモジュールハンドルとアイコンリソースの読み出し、および自前で組み立てた
    // NOTIFYICONDATAW (szTip は NUL 終端で配列内に収める) を渡すだけ。hwnd は呼び出し元の生存窓。
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let hicon = LoadIconW(hinstance, int_resource(ICON_RESOURCE_ID))
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
        if let Some(state) = wnd_state(hwnd) {
            let text = tooltip_text(state.status.borrow().as_ref());
            let tip = to_wide_capped(&text, nid.szTip.len());
            nid.szTip[..tip.len()].copy_from_slice(&tip);
        }
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
    Ok(())
}

/// explorer (タスクバー) が起動し直したときに全トップレベル窓へ送られるメッセージの ID。
/// これを処理しないと explorer のクラッシュ/再起動でトレイアイコンが消え、コンソール無し常駐の
/// host が操作不能 (終了・パスワード変更・ペアリング承認ができない) になる。
fn taskbar_created_msg() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: 静的な NUL 終端ワイド文字列を渡すだけ。
    *ID.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

/// 隠しウィンドウのウィンドウプロシージャ。
extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: メッセージ処理中の Win32 呼び出し。hwnd は OS から渡る有効なウィンドウ。
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
            WM_TIMER => {
                if wparam.0 == TIMER_ID {
                    on_timer(hwnd);
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
                    ID_UPDATE => simple(hwnd, TrayRequest::RequestUpdate),
                    ID_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    // 登録端末リストの項目: その端末を削除する。
                    c if (ID_DEVICE_BASE..ID_DEVICE_BASE + MAX_DEVICE_ITEMS).contains(&c) => {
                        remove_device(hwnd, c - ID_DEVICE_BASE);
                    }
                    // 承認待ちデバイスリストの項目: その端末を承認する。
                    c if (ID_PENDING_BASE..ID_PENDING_BASE + MAX_PENDING_ITEMS).contains(&c) => {
                        approve_pending(hwnd, c - ID_PENDING_BASE);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(hwnd, TIMER_ID);
                PostQuitMessage(0);
                LRESULT(0)
            }
            // RegisterWindowMessageW の失敗時は 0 なので、通常メッセージ 0 と取り違えない。
            m if m != 0 && m == taskbar_created_msg() => {
                if let Err(e) = add_tray_icon(hwnd) {
                    tracing::warn!(error = %e, "failed to re-add tray icon after explorer restart");
                } else {
                    tracing::info!("tray icon re-added after explorer restart");
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 隠しウィンドウに紐づけた状態への参照を取り出す。
unsafe fn wnd_state(hwnd: HWND) -> Option<&'static WndState> {
    // SAFETY: hwnd は有効なウィンドウ。GWLP_USERDATA は run でセットしたポインタ値を返す。
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndState;
    if ptr.is_null() {
        None
    } else {
        // SAFETY: ポインタは run の Box::into_raw 由来で、ウィンドウ生存中は有効。
        Some(unsafe { &*ptr })
    }
}

/// ステータス監視タイマ (`WM_TIMER`)。本体の状態を取り直し、接続/切断/登録の通知バルーンを出し、
/// 接続数の変化でツールチップを更新し、認証失敗の増分を (クールダウン付きで) 警告通知する。
fn on_timer(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };

    let first_seen_before = state.first_status_seen.get();
    let mut events = state.refresh();
    if !first_seen_before {
        // 初めて本体に届いた: それまでに溜まった通知は (起動時と同じく) バルーンにしない。
        if state.take_first_status_prompt() {
            // SAFETY: hwnd は生存中のトレイ窓。メッセージを積むだけ (ダイアログはループ側で開く)。
            let _ = unsafe { PostMessageW(hwnd, WM_COMMAND, WPARAM(ID_CHANGE), LPARAM(0)) };
        }
        if state.first_status_seen.get() {
            events.clear();
        }
    }

    // 1) 通知イベント → バルーン。接続/切断/登録をオーナーへ能動通知する (無人ホストの気付き)。
    for ev in events {
        match ev {
            TrayEvent::Connected(label) => balloon(
                hwnd,
                "Aurora Host",
                &format!("端末「{label}」が接続しました"),
                false,
            ),
            TrayEvent::Disconnected(label) => balloon(
                hwnd,
                "Aurora Host",
                &format!("端末「{label}」が切断しました"),
                false,
            ),
            TrayEvent::PendingDevice { fingerprint, label } => balloon(
                hwnd,
                "Aurora Host",
                &format!(
                    "承認待ちのデバイスが接続しました\n端末: {label}\n端末ID: {fingerprint}\nトレイメニューで端末IDを照合して承認してください。"
                ),
                false,
            ),
            TrayEvent::UpdateAvailable(version) => balloon(
                hwnd,
                "Aurora Host",
                &format!(
                    "新しいバージョン v{version} が利用できます。\nトレイメニューの「更新」からインストールできます。"
                ),
                false,
            ),
            TrayEvent::EncodeStalled => balloon(
                hwnd,
                "Aurora Host — 映像配信が停止",
                "映像エンコーダの再構築に失敗し、映像配信が止まりました。\n接続は維持されますが映像は届きません。ホストの再起動を検討してください。",
                true,
            ),
        }
    }

    let status = state.status.borrow();
    // 2) 接続数が変わったら (本体に届かなくなったときも) ツールチップを更新する。
    let n = status.as_ref().map_or(usize::MAX - 1, |s| s.connected);
    if n != state.last_connected.get() {
        state.last_connected.set(n);
        update_tooltip(hwnd, &tooltip_text(status.as_ref()));
    }

    // 3) 認証失敗の増分を警告バルーンで通知する (クールダウンで連打を抑える)。
    let cd = state.auth_cooldown.get();
    if cd > 0 {
        state.auth_cooldown.set(cd - 1);
    }
    let Some(f) = status.as_ref().map(|s| s.auth_failures) else {
        return;
    };
    let prev = state.last_auth.replace(Some(f));
    // 初回取得 (トレイ起動・本体再起動) の値は基準にするだけで鳴らさない。
    if let Some(prev) = prev
        && f > prev
        && state.auth_cooldown.get() == 0
    {
        state.auth_cooldown.set(AUTH_BALLOON_COOLDOWN);
        balloon(
            hwnd,
            "Aurora Host — 認証失敗",
            &format!(
                "認証に失敗した接続があります (起動以降 合計 {f} 回)。\n\
                 心当たりが無ければパスワード変更を検討してください。"
            ),
            true,
        );
    }
}

/// ツールチップ本文を組み立てる。接続中は端末名、待受中は待受ポートを示す。
fn tooltip_text(status: Option<&TrayStatus>) -> String {
    let Some(s) = status else {
        return "Aurora Host — 本体に接続できません".to_string();
    };
    if s.connected == 0 {
        return format!("Aurora Host — 待受中 (ポート {})", s.port);
    }
    match &s.last_peer {
        Some(label) if s.connected == 1 => format!("Aurora Host — 接続中: {label}"),
        Some(label) => format!("Aurora Host — 接続中: {} 台 ({label} 他)", s.connected),
        None => format!("Aurora Host — 接続中: {} 台", s.connected),
    }
}

/// トレイアイコンのツールチップ (ホバー表示) を更新する。`szTip` は 128 wchar 固定長。
fn update_tooltip(hwnd: HWND, text: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    let tip = to_wide_capped(text, nid.szTip.len());
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    // SAFETY: nid は同じ hWnd+uID の既存アイコンを指し、szTip は NUL 終端済みで配列内に収まる。
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// バルーン通知 (トースト) を表示する。`warning` で警告アイコンにする。
fn balloon(hwnd: HWND, title: &str, text: &str, warning: bool) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: if warning { NIIF_WARNING } else { NIIF_INFO },
        ..Default::default()
    };
    let info = to_wide_capped(text, nid.szInfo.len());
    nid.szInfo[..info.len()].copy_from_slice(&info);
    let head = to_wide_capped(title, nid.szInfoTitle.len());
    nid.szInfoTitle[..head.len()].copy_from_slice(&head);
    // SAFETY: nid は既存アイコン (同 hWnd+uID) を指し、szInfo/szInfoTitle は NUL 終端で配列内に収まる。
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// UTF-16 へ変換し、NUL 終端込みで `cap` wchar に収める (固定長 `sz*` 配列へのコピー用)。
fn to_wide_capped(s: &str, cap: usize) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().take(cap.saturating_sub(1)).collect();
    v.push(0);
    v
}

/// トグル項目のラベル。現在状態 (オン/オフ) を名前直後に置き、操作は副次的に併記して
/// 「今どちらか」と「押すとどうなるか」を取り違えないようにする (チェックマークと併用)。
fn toggle_label(name: &str, on: bool) -> String {
    if on {
        format!("{name}：オン  ―  クリックでオフにする")
    } else {
        format!("{name}：オフ  ―  クリックでオンにする")
    }
}

/// トレイの右クリックメニューを表示する。開く直前に本体の状態を取り直す (表示と操作の食い違いを
/// 小さくする。index で指す項目は要求側でもラベル/鍵で照合する)。
unsafe fn show_menu(hwnd: HWND) {
    // SAFETY: メニュー構築一式の Win32 呼び出し。hwnd は有効で各 API の結果は確認する。
    unsafe {
        let Some(state) = wnd_state(hwnd) else {
            return;
        };
        let _ = state.refresh();
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        // `_keep` は AppendMenuW へ渡す UTF-16 バッファを TrackPopupMenu まで生かすための保持先
        // (AppendMenuW は文字列をコピーするが、サブメニュー構築をまたぐので明示的に束ねる)。
        let mut _keep: Vec<Vec<u16>> = Vec::new();
        // サブメニューは menu に MF_POPUP で挿入すると所有権が menu 側に移り、menu 破棄時に
        // 一緒に破棄される (個別破棄しない)。
        let mut _submenus: Vec<HMENU> = Vec::new();
        let status = state.status.borrow();

        // 先頭に現在のステータスを情報行 (操作不可) として出す。一目で「今だれか繋いでいるか」
        // 「待受ポート」「総当たりに晒されていないか」が分かるようにする。
        match status.as_ref() {
            Some(s) => {
                let conn = if s.connected == 0 {
                    "○ 接続なし (待受中)".to_string()
                } else {
                    match &s.last_peer {
                        Some(l) if s.connected == 1 => format!("● 接続中: {l}"),
                        Some(l) => format!("● 接続中: {} 台 ({l} 他)", s.connected),
                        None => format!("● 接続中: {} 台", s.connected),
                    }
                };
                let conn_w = to_wide(&conn);
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(conn_w.as_ptr()));
                _keep.push(conn_w);
                let port_w = to_wide(&format!("　待受ポート: {}", s.port));
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(port_w.as_ptr()));
                _keep.push(port_w);
                if s.auth_failures > 0 {
                    let fail_w = to_wide(&format!("　認証失敗: {} 回 (起動以降)", s.auth_failures));
                    let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(fail_w.as_ptr()));
                    _keep.push(fail_w);
                }
                if let Some(v) = &s.update_version {
                    let up = to_wide(&format!("★ 更新 (v{v}) をインストール..."));
                    let _ = AppendMenuW(menu, MF_STRING, ID_UPDATE, PCWSTR(up.as_ptr()));
                    _keep.push(up);
                }
            }
            None => {
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | MF_GRAYED,
                    0,
                    w!("× 本体に接続できません (再起動中の可能性)"),
                );
            }
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let reachable = status.is_some();
        let item = |enabled: bool| MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED };
        let _ = AppendMenuW(menu, item(reachable), ID_CHANGE, w!("パスワードを変更..."));
        let _ = AppendMenuW(menu, item(reachable), ID_PORT, w!("ポートを変更..."));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_LOGS, w!("ログフォルダを開く"));

        // ログオン時自動起動 (ONLOGON タスク) は同一プロセス運用だけの概念。サービス運用では
        // サービスが起動を担うので出さない。
        if state.mode == TrayMode::InProcess {
            let installed = crate::system::is_installed();
            let label = to_wide(&toggle_label("ログオン時に自動起動", installed));
            let flags = MF_STRING | if installed { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(menu, flags, ID_AUTOSTART, PCWSTR(label.as_ptr()));
            _keep.push(label);
        }

        // デバイスピン留め (2 要素目)。有効時のみペアリング/端末一覧を出す。
        if let Some(s) = status.as_ref() {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let toggle = to_wide(&toggle_label("デバイスピン留め", s.pinning_enabled));
            let toggle_flags = MF_STRING
                | if s.pinning_enabled {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
            let _ = AppendMenuW(menu, toggle_flags, ID_PIN_TOGGLE, PCWSTR(toggle.as_ptr()));
            _keep.push(toggle);
            if s.pinning_enabled {
                // ペアリング: 開いていれば「取消」、閉じていれば「次の接続を登録」。
                let pair = to_wide(if s.pairing_armed {
                    "ペアリング窓を閉じる (登録待機を取消)"
                } else {
                    "次の接続を登録 (ペアリング)..."
                });
                let _ = AppendMenuW(menu, MF_STRING, ID_PIN_PAIR, PCWSTR(pair.as_ptr()));
                _keep.push(pair);

                // ペアリング窓が開いている間、承認待ち端末をインライン項目として列挙する
                // (指紋を接続元端末の表示値と照合して承認する入口)。
                if s.pairing_armed && !s.pending.is_empty() {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                    for (i, p) in s.pending.iter().take(MAX_PENDING_ITEMS).enumerate() {
                        let item = to_wide(&format!("承認: {} ({})", p.fingerprint, p.label));
                        let _ = AppendMenuW(
                            menu,
                            MF_STRING,
                            ID_PENDING_BASE + i,
                            PCWSTR(item.as_ptr()),
                        );
                        _keep.push(item);
                    }
                }

                // 登録済みデバイスをサブメニューに列挙する (各項目クリックでその端末を削除)。
                if let Ok(sub) = CreatePopupMenu() {
                    if s.device_labels.is_empty() {
                        let none = to_wide("(登録なし)");
                        let _ = AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, PCWSTR(none.as_ptr()));
                        _keep.push(none);
                    } else {
                        for (i, label) in s.device_labels.iter().take(MAX_DEVICE_ITEMS).enumerate()
                        {
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
                        let _ = AppendMenuW(sub, MF_STRING, ID_PIN_CLEAR, PCWSTR(clear.as_ptr()));
                        _keep.push(clear);
                    }
                    let head = to_wide(&format!("登録済みデバイス ({} 台)", s.device_labels.len()));
                    let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(head.as_ptr()));
                    _keep.push(head);
                    _submenus.push(sub);
                }
            }
        }
        drop(status);

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let exit_label = match state.mode {
            TrayMode::InProcess => w!("終了"),
            TrayMode::Standalone => w!("トレイを閉じる (ホストは動作を続けます)"),
        };
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, exit_label);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // メニューが正しく閉じるよう自ウィンドウを前面化する (Win32 の定石)。
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// 要求を投げて結果をメッセージボックスで知らせる。成功なら `true`。
fn call_and_report(hwnd: HWND, state: &WndState, req: TrayRequest) -> bool {
    match state.control.call(req) {
        TrayReply::Done(msg) => {
            message_box(hwnd, &msg, "Aurora Host");
            true
        }
        TrayReply::Failed(msg) => {
            message_box(hwnd, &msg, "Aurora Host");
            false
        }
        TrayReply::Status(_) => false,
    }
}

/// 確認なしで 1 要求を投げるだけのメニュー項目。
fn simple(hwnd: HWND, req: TrayRequest) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    if let Some(state) = unsafe { wnd_state(hwnd) } {
        call_and_report(hwnd, state, req);
    }
}

/// パスワード変更ダイアログを表示し、本体へ変更を要求する。現在パスワードの照合は本体が行い、
/// 違っていたら入力し直せるようダイアログを開き直す。
fn change_password(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let _ = state.refresh();
    let Some(require_current) = state.status.borrow().as_ref().map(|s| s.has_password) else {
        message_box(hwnd, "本体に接続できません。", "Aurora Host");
        return;
    };
    while let Some((current, new)) = prompt_password(hwnd, require_current) {
        match state
            .control
            .call(TrayRequest::ChangePassword { current, new })
        {
            TrayReply::Done(msg) => {
                message_box(hwnd, &msg, "Aurora Host");
                return;
            }
            TrayReply::Failed(msg) => {
                message_box(hwnd, &msg, "Aurora Host");
                // 現在パスワード違い・長さ不足は入力し直させる。それ以外 (接続中・保存失敗) は諦める。
                if !msg.contains("パスワード") {
                    return;
                }
            }
            TrayReply::Status(_) => return,
        }
    }
}

/// ポート変更ダイアログを表示し、本体へ変更を要求する (本体が bind 確認・保存・貼り直しを行う)。
fn change_port(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let _ = state.refresh();
    let Some(current) = state.status.borrow().as_ref().map(|s| s.port) else {
        message_box(hwnd, "本体に接続できません。", "Aurora Host");
        return;
    };
    if let Some(new_port) = prompt_port(hwnd, current)
        && new_port != current
    {
        call_and_report(hwnd, state, TrayRequest::ChangePort(new_port));
    }
}

/// ログフォルダ (`%ProgramData%\Aurora\host\logs`) をエクスプローラで開く。
fn open_logs_folder(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    // 単独トレイ (一般ユーザー権限) は保護済みフォルダを作成・ACL 設定できないので、
    // 本体が用意した場所を開くだけにする。
    let dir = match state.mode {
        TrayMode::InProcess => crate::system::state::log_dir().ok(),
        TrayMode::Standalone => crate::system::state::log_dir_path(),
    };
    let Some(dir) = dir else {
        message_box(
            hwnd,
            "ログフォルダの場所を特定できませんでした。",
            "Aurora Host",
        );
        return;
    };
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

/// ログオン時自動起動 (タスクスケジューラの ONLOGON タスク) の有効/無効をトグルする
/// (同一プロセス運用のみ)。`schtasks /Create` は `/RL HIGHEST` で登録するため管理者権限を
/// 要する場合があり、失敗時はその旨をメッセージで案内する。接続中でも安全な操作 (映像/入力経路に
/// 影響しない) のため接続中ガードは設けない。
fn toggle_autostart(hwnd: HWND) {
    if crate::system::is_installed() {
        match crate::system::uninstall() {
            Ok(()) => message_box(
                hwnd,
                "ログオン時の自動起動を無効にしました。",
                "Aurora Host",
            ),
            Err(e) => message_box(
                hwnd,
                &format!("自動起動の解除に失敗しました: {e}\n管理者権限が必要な場合があります。"),
                "Aurora Host",
            ),
        }
    } else {
        let args = crate::system::current_forward_args();
        match crate::system::install(&args) {
            Ok(()) => message_box(
                hwnd,
                "ログオン時に自動起動するようにしました。",
                "Aurora Host",
            ),
            Err(e) => message_box(
                hwnd,
                &format!("自動起動の登録に失敗しました: {e}\n管理者権限が必要な場合があります。"),
                "Aurora Host",
            ),
        }
    }
}

/// デバイスピン留めの有効/無効を切り替える (無効化の接続中拒否は本体側)。
fn toggle_pinning(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let Some(on) = state.status.borrow().as_ref().map(|s| s.pinning_enabled) else {
        return;
    };
    call_and_report(hwnd, state, TrayRequest::SetPinning(!on));
}

/// ペアリング窓をトグルする。開いていれば取り消し、閉じていれば開く (開く操作の接続中拒否は本体側)。
fn toggle_pairing(hwnd: HWND) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let Some(armed) = state.status.borrow().as_ref().map(|s| s.pairing_armed) else {
        return;
    };
    let req = if armed {
        TrayRequest::DisarmPairing
    } else {
        TrayRequest::ArmPairing
    };
    call_and_report(hwnd, state, req);
}

/// はい/いいえの確認ダイアログ。はいなら `true`。
fn confirm(hwnd: HWND, text: &str) -> bool {
    let text = to_wide(text);
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
    r == IDYES
}

/// 登録端末リストの `index` 番目を削除する (確認付き)。表示していたラベルも送り、一覧が
/// 変わっていたら本体が削除を拒む。
fn remove_device(hwnd: HWND, index: usize) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let Some(label) = state
        .status
        .borrow()
        .as_ref()
        .and_then(|s| s.device_labels.get(index).cloned())
    else {
        return;
    };
    if confirm(
        hwnd,
        &format!(
            "登録端末「{label}」を削除しますか?\nこの端末はピン留め有効中に接続できなくなります (再ペアリングが必要)。"
        ),
    ) {
        call_and_report(hwnd, state, TrayRequest::RemoveDevice { index, label });
    }
}

/// 承認待ちリストの `index` 番目を信頼登録する (確認付き)。クリック時点のスナップショットから
/// 対象の**公開鍵**を控えて承認を確定する (確認表示中に一覧が変わっても、照合した端末とは
/// 別物を登録しない TOCTOU 対策)。
fn approve_pending(hwnd: HWND, index: usize) {
    // SAFETY: wnd_state は run が Box::into_raw でセットした生存中の WndState を返す。
    let Some(state) = (unsafe { wnd_state(hwnd) }) else {
        return;
    };
    let Some(p) = state
        .status
        .borrow()
        .as_ref()
        .and_then(|s| s.pending.get(index).cloned())
    else {
        message_box(
            hwnd,
            "承認できませんでした (一覧が変わった可能性)。",
            "Aurora Host",
        );
        return;
    };
    if confirm(
        hwnd,
        &format!(
            "この端末を信頼登録しますか?\n\n端末名: {}\n端末ID: {}\n\n登録したい端末の画面に表示された端末IDとこの値が一致することを必ず確認してください。一致しない場合は承認しないでください (別端末が割り込んでいる可能性があります)。",
            p.label, p.fingerprint
        ),
    ) {
        call_and_report(
            hwnd,
            state,
            TrayRequest::ApprovePending { pubkey: p.pubkey },
        );
    }
}

/// 登録済み信頼端末をすべて削除する (確認付き)。
fn clear_devices(hwnd: HWND) {
    if confirm(
        hwnd,
        "登録済みの信頼端末をすべて削除しますか?\nピン留め有効中はどの端末も接続できなくなります (再ペアリングが必要)。",
    ) {
        simple(hwnd, TrayRequest::ClearDevices);
    }
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
    // SAFETY: t/c は NUL 終端 UTF-16 で呼び出し中は生存。owner は有効なウィンドウ。
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

/// ダイアログの入力結果。WndProc から参照/書き戻すため `GWLP_USERDATA` に置く。
struct DlgResult {
    /// OK が押され検証を通った (現在のパスワード, 新パスワード)。
    value: Option<(Option<String>, String)>,
    /// 「現在のパスワード」欄を出しているか。
    require_current: bool,
}

/// パスワード設定/変更ダイアログを表示する。`require_current` のときは「現在のパスワード」欄を
/// 追加する。照合は本体側 (`control::handle`) で行う (トレイに PSK を持たせないため)。
fn prompt_password(owner: HWND, require_current: bool) -> Option<(Option<String>, String)> {
    // SAFETY: パスワードダイアログ生成一式。各 Win32 呼び出しの結果は ? で確認する。
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
            require_current,
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
    // SAFETY: メッセージ処理中の Win32 呼び出し。hwnd は OS から渡る有効なウィンドウ。
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

/// OK 押下: 新パスワード 2 欄を検証して、問題なければ結果へ格納しダイアログを閉じる。
/// 検証に失敗したときはダイアログを開いたままにする (現在パスワードの照合は本体側)。
unsafe fn on_ok(hwnd: HWND) {
    // SAFETY: hwnd は有効なダイアログ。GWLP_USERDATA は WM_CREATE で置いた DlgResult ポインタ。
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DlgResult;
        let require_current = !ptr.is_null() && (*ptr).require_current;
        let current = if require_current {
            match GetDlgItem(hwnd, ID_EDIT_CUR) {
                Ok(h) => Some(get_text(h)),
                Err(_) => return,
            }
        } else {
            None
        };

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
            (*ptr).value = Some((current, pw1));
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// EDIT コントロールの現在テキストを取得する。
unsafe fn get_text(edit: HWND) -> String {
    // SAFETY: edit は有効な EDIT コントロール。テキスト長で確保した buf に書き込む。
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
    // SAFETY: ポートダイアログ生成一式。各 Win32 呼び出しの結果は ? で確認する。
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
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL) as u32),
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
extern "system" fn port_dlg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: メッセージ処理中の Win32 呼び出し。hwnd は OS から渡る有効なウィンドウ。
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
    // SAFETY: hwnd は有効なダイアログ。GetDlgItem/get_text で入力欄のテキストを読む。
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
