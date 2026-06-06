//! 稼働中サーバが「クライアント接続中」であることを、トレイ UI と CLI サブコマンド
//! (`--set-password`) の双方から確認できるようにする仕組み。
//!
//! 接続中だけ生存する名前付きイベント (`Local\AuroraSessionActive`) を使う。サーバが
//! 接続を受理している間 [`ActiveGuard`] がイベントハンドルを保持し、切断 (Drop) で閉じる。
//! 名前付きカーネルオブジェクトはハンドルが全て閉じると破棄されるため、サーバがクラッシュ
//! してもロックが残らない。[`is_active`] は別プロセスからでも存在を確認できる。
//!
//! 目的: リモートクライアントは入力注入でホストのデスクトップを操作できる。接続中に
//! トレイや端末からパスワードを変更され、締め出し/乗っ取りされるのを防ぐ (接続中は拒否)。

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, OpenEventW};
    use windows::core::w;

    /// 接続中だけ生存する名前付きイベントを保持する RAII ガード。Drop でハンドルを閉じる。
    pub struct ActiveGuard {
        handle: Option<HANDLE>,
    }

    impl ActiveGuard {
        /// 名前付きイベントを作成/取得して接続中であることを示す。失敗しても無害 (None 保持)。
        pub fn new() -> Self {
            // SAFETY: 名前付きイベントを作成 (既存なら取得) するだけ。失敗は None で握り潰す。
            let handle =
                unsafe { CreateEventW(None, true, false, w!("Local\\AuroraSessionActive")) }.ok();
            Self { handle }
        }
    }

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                // SAFETY: new で取得した有効なハンドルのみ閉じる。
                let _ = unsafe { CloseHandle(h) };
            }
        }
    }

    /// 稼働中サーバがクライアント接続中なら `true`。別プロセスからも確認できる。
    pub fn is_active() -> bool {
        // SAFETY: 既存の名前付きイベントを開くだけ。成功したら即座に閉じる。
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\AuroraSessionActive")) } {
            Ok(h) => {
                // SAFETY: OpenEventW が返した有効なハンドルを閉じる。
                let _ = unsafe { CloseHandle(h) };
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    /// 非 Windows ではパスワード保存自体が未対応のため、ガードは何もしない。
    pub struct ActiveGuard;
    impl ActiveGuard {
        pub fn new() -> Self {
            Self
        }
    }
    pub fn is_active() -> bool {
        false
    }
}

pub use imp::{ActiveGuard, is_active};
