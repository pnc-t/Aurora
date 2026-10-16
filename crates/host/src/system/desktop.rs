//! スレッドを「入力デスクトップ」(= いま画面に出ていて入力を受けるデスクトップ) に追従させる。
//!
//! UAC 同意画面・ロック画面・サインイン画面は通常の `Default` ではなく `Winlogon` デスクトップに
//! 出る。DXGI Desktop Duplication の `DuplicateOutput` と `SendInput` は**呼び出しスレッドの
//! デスクトップ**に対して働くため、切替後もスレッドが `Default` に居ると、複製は ACCESS_LOST /
//! E_ACCESSDENIED を返し続け、入力はユーザーに見えない裏のデスクトップへ届く。
//!
//! Windows 固有の落とし穴:
//! - `Winlogon` を開けるのは SYSTEM だけ (サービス配下の本体)。昇格ユーザー (従来の ONLOGON
//!   運用・開発実行) では `OpenInputDesktop` が失敗するので、何もせず従来どおり `Default` に留まる。
//! - `SetThreadDesktop` はスレッドがウィンドウやフックを持っていると失敗する。呼ぶのは
//!   ウィンドウを持たない capture / input スレッドだけにする (トレイは `Default` に居続けるべき)。
//! - スレッドに割り当て中のデスクトップハンドルは `CloseDesktop` できない。乗り換えた後で旧ハンドルを閉じる。

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
        GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    };

    /// このスレッドを割り当てたデスクトップ。Drop で (割り当て解除後の) ハンドルを閉じる。
    struct Attached {
        handle: HDESK,
        name: String,
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            // SAFETY: OpenInputDesktop で得たハンドルを 1 度だけ閉じる。まだスレッドに割り当て中
            // (スレッド終了時) なら失敗するだけで、プロセス終了時に OS が回収する。
            let _ = unsafe { CloseDesktop(self.handle) };
        }
    }

    thread_local! {
        static ATTACHED: RefCell<Option<Attached>> = const { RefCell::new(None) };
        /// 直近に乗り換えに失敗したデスクトップ名。入力スレッドは毎イベント呼ぶので、同じ失敗を
        /// 何度も警告しない (別のデスクトップで失敗したら改めて出す)。
        static LAST_FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// デスクトップの名前 (`Default` / `Winlogon` 等)。取得できなければ `None`。
    pub fn desktop_name(desk: HDESK) -> Option<String> {
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        // SAFETY: desk は呼び出し側が保持する有効なハンドル。バッファ長 (バイト) を渡し UTF-16 名を受ける。
        unsafe {
            GetUserObjectInformationW(
                HANDLE(desk.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                (name.len() * 2) as u32,
                Some(&mut needed),
            )
        }
        .ok()?;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..len]))
    }

    /// 呼び出しスレッドを現在の入力デスクトップへ乗り換える。乗り換えたら `true`。
    /// 既に同じデスクトップ・開けない (非 SYSTEM で `Winlogon`)・割り当て失敗はいずれも `false`
    /// で、スレッドは元のデスクトップに留まる。
    pub fn follow_input_desktop() -> bool {
        // SAFETY: 入力デスクトップを開くだけ。失敗 (権限不足等) は乗り換えない。
        let desk = match unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        } {
            Ok(d) => d,
            Err(_) => return false,
        };
        let new = Attached {
            handle: desk,
            name: desktop_name(desk).unwrap_or_default(),
        };
        ATTACHED.with(|cell| {
            let mut cur = cell.borrow_mut();
            if cur.as_ref().is_some_and(|c| c.name == new.name) {
                return false; // new は Drop で閉じる
            }
            // SAFETY: new.handle は直前に開いた有効なデスクトップ。このスレッドはウィンドウを持たない前提。
            if let Err(e) = unsafe { SetThreadDesktop(new.handle) } {
                // UAC/ロック画面が操作できない原因を本番ログ (info 以上) から追えるよう warn にする。
                LAST_FAILED.with(|last| {
                    let mut last = last.borrow_mut();
                    if last.as_deref() != Some(new.name.as_str()) {
                        tracing::warn!(
                            error = %e,
                            desktop = %new.name,
                            thread = std::thread::current().name().unwrap_or("?"),
                            "SetThreadDesktop failed; staying on current desktop"
                        );
                        *last = Some(new.name.clone());
                    }
                });
                return false;
            }
            LAST_FAILED.with(|last| *last.borrow_mut() = None);
            tracing::info!(
                desktop = %new.name,
                thread = std::thread::current().name().unwrap_or("?"),
                "thread switched to input desktop"
            );
            // 旧ハンドルは割り当てが外れたのでここで閉じられる。
            *cur = Some(new);
            true
        })
    }

    /// `f` の間だけ呼び出しスレッドを入力デスクトップへ移し、終わったら元のデスクトップへ戻す。
    /// [`follow_input_desktop`] と違って乗り換えを残さないので、追従し続けない (以後の状態変化を
    /// 見ない) スレッド (起動時のアダプタ確認を行う main スレッド等) に使う。移せなければそのまま実行する。
    pub fn with_input_desktop<T>(f: impl FnOnce() -> T) -> T {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        // SAFETY: 自スレッドに割り当て中のデスクトップを得るだけ (このハンドルは閉じてはならない)。
        let Ok(original) = (unsafe { GetThreadDesktop(GetCurrentThreadId()) }) else {
            return f();
        };
        // SAFETY: 入力デスクトップを開くだけ。失敗 (権限不足等) はそのまま実行する。
        let Ok(input) = (unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        }) else {
            return f();
        };
        // SAFETY: input は直前に開いた有効なデスクトップ。このスレッドはウィンドウを持たない前提。
        let switched = unsafe { SetThreadDesktop(input) }.is_ok();
        let out = f();
        if switched {
            // SAFETY: original は自スレッドが元々割り当てられていた有効なデスクトップ。
            let _ = unsafe { SetThreadDesktop(original) };
        }
        // SAFETY: 割り当てを外した (または割り当てなかった) input を 1 度だけ閉じる。
        let _ = unsafe { CloseDesktop(input) };
        out
    }
}

#[cfg(target_os = "windows")]
pub use imp::{desktop_name, follow_input_desktop, with_input_desktop};

/// 非 Windows は入力デスクトップの概念が無い (常に乗り換えない)。
#[cfg(not(target_os = "windows"))]
pub fn follow_input_desktop() -> bool {
    false
}
