//! ウィンドウモード中のキーボード占有 (Win キー / Alt+Tab 等をクライアント OS に奪わせない)。
//! マウスキャプチャ (ポインタロック) と連動し、フロントが ON/OFF を切り替える。
//! Windows のみ実装。非 Windows は no-op (呼び出し側は cfg 分岐せず同じコマンドを叩ける)。

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, Once};

    use serde::Serialize;
    use tauri::Emitter;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
        MSG, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
        WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    const VK_LWIN: u16 = 0x5b;
    const VK_RWIN: u16 = 0x5c;
    const VK_TAB: u16 = 0x09;
    const VK_ESCAPE: u16 = 0x1b;
    const VK_F4: u16 = 0x73;

    // フックはプロセス生存中ずっと張ったままにし (張り直しコスト回避)、この 3 つのグローバル状態
    // だけで ON/OFF・転送先・残留防止を制御する。フックスレッド起動は初回 ON 時の 1 回のみ。
    static ENABLED: AtomicBool = AtomicBool::new(false);
    static TARGET: Mutex<Option<tauri::Window>> = Mutex::new(None);
    // swallow して down を転送済みの vk 一覧 (up も揃えて転送するため)。同時押下数は少ないので Vec で十分。
    static SWALLOWED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    static HOOK_STARTED: Once = Once::new();

    #[derive(Serialize, Clone)]
    struct GrabbedKey {
        vk: u16,
        down: bool,
    }

    /// キーボード占有の ON/OFF。呼び出し元 window を転送先として記録する。
    // #[tauri::command] マクロが同じモジュールに生成する隠し items は `pub use` 経由の再エクスポート
    // では見えない (tauri::generate_handler! が解決できない) ため、コマンドの実体はここに置かず
    // 素の関数として実装し、外側の `set_keyboard_grab` (#[tauri::command] 本体) から呼ぶ。
    pub fn set_keyboard_grab_impl(window: tauri::Window, on: bool) {
        if on {
            *TARGET.lock().expect("keyboard grab target mutex poisoned") = Some(window);
            ensure_hook_thread();
            ENABLED.store(true, Ordering::Relaxed);
            tracing::info!("keyboard grab enabled");
        } else {
            release_for_window(window.label());
        }
    }

    /// `label` の窓が転送先のときだけ占有を解除する。窓 A の blur 由来の OFF (async invoke) が
    /// 窓 B の ON より後に届くと B の占有を勝手に切ってしまうため、他窓からの OFF は無視する。
    pub fn release_for_window(label: &str) {
        {
            let target = TARGET.lock().expect("keyboard grab target mutex poisoned");
            match target.as_ref() {
                Some(t) if t.label() == label => {}
                _ => return,
            }
        }
        ENABLED.store(false, Ordering::Relaxed);
        // down だけ奪って up を転送し損ねるとリモート側にキー残留するため、
        // swallow 中だった分は OFF 時にまとめて up を転送する。
        let downs: Vec<u16> = std::mem::take(
            &mut *SWALLOWED
                .lock()
                .expect("keyboard grab swallowed mutex poisoned"),
        );
        for vk in downs {
            forward(vk, false);
        }
        tracing::info!("keyboard grab disabled");
    }

    fn ensure_hook_thread() {
        HOOK_STARTED.call_once(|| {
            // 起動時初期化に準ずる: フックスレッドを立てられないのは致命的な環境異常のため main.rs
            // 同様に expect で許容する。
            std::thread::Builder::new()
                .name("keyboard-grab-hook".into())
                .spawn(hook_thread_main)
                .expect("spawn keyboard grab hook thread");
        });
    }

    fn hook_thread_main() {
        // SAFETY: WH_KEYBOARD_LL はこのスレッドのメッセージループが生きている間だけ有効。
        // hhook はプロセス終了までスレッドを保持し続けるため明示的な UnhookWindowsHookEx は
        // ループ終了時 (実運用では起きない) にのみ行う。
        let hhook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
        let hhook = match hhook {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "SetWindowsHookExW failed");
                return;
            }
        };
        tracing::info!("keyboard grab hook installed");
        let mut msg = MSG::default();
        // SAFETY: 標準的な Win32 メッセージループ。フックコールバックはこの GetMessageW 呼び出し
        // の中で同期的に呼ばれる (別スレッドから割り込まない)。
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(hhook);
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && ENABLED.load(Ordering::Relaxed) {
            // SAFETY: WH_KEYBOARD_LL の契約上 lparam は KBDLLHOOKSTRUCT を指す。
            let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
            // 自己注入 (SendInput 等) はループ防止のため素通しする。
            if info.flags.0 & LLKHF_INJECTED.0 == 0 {
                let msg = wparam.0 as u32;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                if down || up {
                    let vk = info.vkCode as u16;
                    if should_swallow(vk, down) {
                        forward(vk, down);
                        return LRESULT(1);
                    }
                }
            }
        }
        // SAFETY: 次のフックへ渡すだけの委譲呼び出し。
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// 修飾キー自体は奪わない (ブラウザ経路で転送済み)。奪うのはコンボの「本体キー」のみ。
    fn should_swallow(vk: u16, down: bool) -> bool {
        let mut set = SWALLOWED
            .lock()
            .expect("keyboard grab swallowed mutex poisoned");
        if !down {
            // down 時に swallow した vk の keyup だけ揃えて swallow する (片方だけだと残留する)。
            if let Some(pos) = set.iter().position(|&v| v == vk) {
                set.remove(pos);
                return true;
            }
            return false;
        }
        // Ctrl+Alt+Shift 同時押下はローカル UI 予約 (Ctrl+Alt+Shift+M 等) のため奪わない。
        if key_down(VK_CONTROL.0.into())
            && key_down(VK_MENU.0.into())
            && key_down(VK_SHIFT.0.into())
        {
            return false;
        }
        let alt = key_down(VK_MENU.0.into());
        let ctrl = key_down(VK_CONTROL.0.into());
        let swallow = matches!(vk, VK_LWIN | VK_RWIN)
            || (alt && matches!(vk, VK_TAB | VK_ESCAPE | VK_F4))
            || (ctrl && vk == VK_ESCAPE);
        if swallow && !set.contains(&vk) {
            set.push(vk);
        }
        swallow
    }

    fn key_down(vk: i32) -> bool {
        // SAFETY: 現在の物理キー状態を読むだけの単純な照会 API。
        (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
    }

    fn forward(vk: u16, down: bool) {
        let guard = TARGET.lock().expect("keyboard grab target mutex poisoned");
        if let Some(win) = guard.as_ref() {
            // emit は全ウィンドウへのブロードキャストのため、マルチウィンドウだと他窓の
            // アクティブセッション (= 別ホスト) にも二重注入されてしまう。転送先の窓に限定する。
            let target = tauri::EventTarget::labeled(win.label());
            if let Err(e) = win.emit_to(target, "grabbed-key", GrabbedKey { vk, down }) {
                tracing::warn!(error = %e, "grabbed-key emit failed");
            }
        }
    }
}

/// キーボード占有の ON/OFF (フロントが呼ぶ tauri コマンド)。実装は Windows のみ、非 Windows は no-op
/// (呼び出し側は cfg 分岐せず同じコマンド名を叩ける)。マルチウィンドウ前提の機能でモバイルには
/// 存在しないため、invoke_handler への登録ごとデスクトップ限定にする。
#[cfg(desktop)]
#[tauri::command]
pub async fn set_keyboard_grab(window: tauri::Window, on: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    imp::set_keyboard_grab_impl(window, on);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, on);
    }
    Ok(())
}

/// 窓破棄時の後始末。JS の後始末 (タブの teardown) を経ずに窓が閉じても、その窓が転送先なら
/// 占有を解除する (残すとローカル PC の Win / Alt+Tab がシステム全体で握り潰されたままになる)。
pub(crate) fn release_for_window(label: &str) {
    #[cfg(target_os = "windows")]
    imp::release_for_window(label);
    #[cfg(not(target_os = "windows"))]
    let _ = label;
}
