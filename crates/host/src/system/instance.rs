//! サーバ (常駐 host) の単一インスタンス保証。
//!
//! 二重起動すると、2 つ目はキャプチャ/エンコーダまで初期化して GPU を触ったうえで、待受ポートの
//! bind で失敗して黙って終わっていた (トレイの「起動」を誤って押す・インストーラ完了画面の
//! 「今すぐ起動」等で起きる)。名前付き Mutex の有無で、初期化に入る前に判定する。
//! (取り込みの固着からの回復は取り込みワーカーだけを作り直すので、本体が自分を起動し直して
//! この判定と食い合うことは無い。)

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    use windows::core::{HSTRING, w};

    const MUTEX_NAME: &str = "Local\\AuroraHostInstance";

    /// プロセス生存中ずっと保持する Mutex ハンドル。Drop (= プロセス終了時も OS が) で閉じる。
    pub struct InstanceGuard(HANDLE);

    // SAFETY: 保持するのは名前付き Mutex のカーネルハンドルのみで、存在確認にしか使わない
    // (所有権 = ロックは取らない)。どのスレッドから閉じても安全。
    unsafe impl Send for InstanceGuard {}

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            // SAFETY: try_create が返した有効なハンドルを 1 度だけ閉じる。
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// 他に host が居なければ `Some(guard)`。居れば `None` (呼び出し側は起動をやめる)。
    pub fn acquire() -> Option<InstanceGuard> {
        try_create(&HSTRING::from(MUTEX_NAME))
    }

    fn try_create(name: &HSTRING) -> Option<InstanceGuard> {
        // SAFETY: 名前付き Mutex を作成 (既存なら取得) するだけ。初期所有はしない (存在だけ使う)。
        let h = unsafe { CreateMutexW(None, false, name) }.ok()?;
        // SAFETY: 直前の CreateMutexW のエラー状態を読むだけ。
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // 自分のハンドルを残すと、先行プロセスが終わってもオブジェクトが消えず次の起動が弾かれる。
            // SAFETY: 直前に得た有効なハンドルを閉じる。
            let _ = unsafe { CloseHandle(h) };
            return None;
        }
        Some(InstanceGuard(h))
    }

    /// 既に起動していることを利用者に知らせる (GUI サブシステムでコンソールが無いため)。
    pub fn notify_already_running() {
        // SAFETY: 静的なワイド文字列でモーダルな情報ダイアログを出すだけ。
        unsafe {
            MessageBoxW(
                None,
                w!(
                    "Aurora Host は既に起動しています。\nタスクトレイのアイコンから操作してください。"
                ),
                w!("Aurora Host"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 実機の常駐 host (固定名) と衝突しないよう、プロセス固有の名前で試す。
        fn unique_name(tag: &str) -> String {
            format!("Local\\AuroraInstanceTest-{tag}-{}", std::process::id())
        }

        /// 2 つ目は弾かれ、先行が終われば取れる。
        #[test]
        fn second_instance_is_rejected_until_first_exits() {
            let name = HSTRING::from(unique_name("basic"));
            let first = try_create(&name).expect("first");
            assert!(try_create(&name).is_none());
            drop(first);
            assert!(try_create(&name).is_some());
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    /// 非 Windows は常駐形態 (トレイ・自動起動) が未整備のため判定しない。
    pub struct InstanceGuard;
    pub fn acquire() -> Option<InstanceGuard> {
        Some(InstanceGuard)
    }
    pub fn notify_already_running() {}
}

pub use imp::{acquire, notify_already_running};
