//! 稼働中サーバが「クライアント接続中」であることを、トレイ UI と CLI サブコマンド
//! (`--set-password` / `--clear-password`) の双方から確認できるようにする仕組み。
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
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
    use windows::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, OpenEventW};
    use windows::core::{PCWSTR, w};

    const EVENT_NAME: PCWSTR = w!("Local\\AuroraSessionActive");

    /// 接続中だけ生存する名前付きイベントを保持する RAII ガード。Drop でハンドルを閉じる。
    pub struct ActiveGuard {
        handle: Option<HANDLE>,
    }

    impl ActiveGuard {
        /// 名前付きイベントを作成/取得して接続中であることを示す。失敗しても無害 (None 保持)。
        pub fn new() -> Self {
            // SAFETY: 名前付きイベントを作成 (既存なら取得) するだけ。失敗は None で握り潰す。
            let handle = unsafe { CreateEventW(None, true, false, EVENT_NAME) }.ok();
            Self { handle }
        }
    }

    // SAFETY: 保持するのは名前付きイベントのカーネルハンドルのみ。Windows のハンドルはスレッド
    // 非依存で、別スレッドからの CloseHandle も安全。マルチクライアント化で handle_client が
    // tokio::spawn される (= 接続ごとに本ガードを await を跨いで保持する) ため Send が要る。
    unsafe impl Send for ActiveGuard {}

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
        event_exists(EVENT_NAME)
    }

    fn event_exists(name: PCWSTR) -> bool {
        // SAFETY: 既存の名前付きイベントを開くだけ。成功したら即座に閉じる。
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, name) } {
            Ok(h) => {
                // SAFETY: OpenEventW が返した有効なハンドルを閉じる。
                let _ = unsafe { CloseHandle(h) };
                true
            }
            // 存在するが開けない = 接続中。自動起動タスク (昇格) の host が作ったイベントは
            // High 整合性ラベル付きで、非昇格の CLI からは書込系の権限で開けない。ここを
            // 「無い」扱いにすると接続中のパスワード変更ガードが素通りになる (fail-open)。
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::Security::{
            ACL, ACL_REVISION, InitializeAcl, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
        };
        /// `SECURITY_DESCRIPTOR_REVISION` (= 1)。テストのためだけに windows crate の機能を増やさない。
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

        /// 昇格 host が作ったイベントを非昇格 CLI が書込権限で開けない状況を、空 DACL (誰にも
        /// アクセスを許さない) のイベントで再現する。存在するのに開けない = 接続中とみなすべき。
        #[test]
        fn existing_but_inaccessible_event_counts_as_active() {
            let name: Vec<u16> = format!("Local\\AuroraTestNoAccess-{}\0", std::process::id())
                .encode_utf16()
                .collect();
            let name = PCWSTR(name.as_ptr());
            let mut acl = ACL::default();
            let mut sd = SECURITY_DESCRIPTOR::default();
            let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut _);
            // SAFETY: スタック上の ACL / SD を初期化し、空 DACL (ACE 0 個 = 全拒否) を設定する。
            unsafe {
                InitializeAcl(&mut acl, std::mem::size_of::<ACL>() as u32, ACL_REVISION).unwrap();
                InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION).unwrap();
                SetSecurityDescriptorDacl(psd, true, Some(&acl), false).unwrap();
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd.0,
                bInheritHandle: false.into(),
            };
            // SAFETY: 上で構築した SA でイベントを作るだけ。ハンドルはテスト末尾で閉じる。
            let h = unsafe { CreateEventW(Some(&sa), true, false, name) }.unwrap();
            assert!(event_exists(name));
            // SAFETY: 直前に作成した有効なハンドルを閉じる。
            let _ = unsafe { CloseHandle(h) };
            assert!(!event_exists(name));
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
