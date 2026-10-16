//! Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`) のバイト列ラッパー。
//!
//! host / client 双方の秘密ローカル保存 (接続パスワード・端末 ID 鍵) で共有し、**unsafe FFI を
//! 1 箇所に集約**する (以前は host/client の各 `secret.rs` に同一の unsafe が二重にあった)。
//! ユーザープロファイル鍵に紐づくため、復号できるのは暗号化したのと同じ Windows ユーザー
//! アカウントのみ (他ユーザー・他 PC では不可)。
//!
//! 非 Windows は未対応 (Windows 優先方針): [`protect`] / [`unprotect`] は常に
//! [`DpapiError::Unsupported`] を返す。保存形式 (ファイルパス・JSON マップ等) は呼び出し側の責務。

use thiserror::Error;

/// DPAPI 操作の失敗。
#[derive(Error, Debug)]
pub enum DpapiError {
    /// DPAPI API 呼び出しが失敗した (`op` は失敗した関数名)。復号失敗は「別ユーザーの鍵で
    /// 暗号化された / ストアが破損している」可能性を含む。
    #[cfg(windows)]
    #[error("{op} failed: {source}")]
    Api {
        op: &'static str,
        source: windows::core::Error,
    },
    /// 非 Windows で呼ばれた。
    #[error("DPAPI is only supported on Windows")]
    Unsupported,
}

#[cfg(windows)]
mod imp {
    use super::DpapiError;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
        CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    /// DPAPI 出力 BLOB。`pbData` は DPAPI が `LocalAlloc` した領域なので `Drop` で
    /// `LocalFree` する (CLAUDE.md: FFI 取得リソースは Drop で解放)。
    struct OutBlob(CRYPT_INTEGER_BLOB);

    impl Drop for OutBlob {
        fn drop(&mut self) {
            use windows::Win32::Foundation::{HLOCAL, LocalFree};
            if !self.0.pbData.is_null() {
                // SAFETY: pbData は DPAPI が LocalAlloc した領域。null 化して二重解放を防ぐ。
                unsafe {
                    let _ = LocalFree(HLOCAL(self.0.pbData as *mut core::ffi::c_void));
                }
                self.0.pbData = std::ptr::null_mut();
            }
        }
    }

    /// 平文を DPAPI (ユーザースコープ) で暗号化する。処理後は入力バッファ (ヒープ上の平文) を
    /// ゼロ埋めする (ベストエフォート)。
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, DpapiError> {
        protect_with(plain, CRYPTPROTECT_UI_FORBIDDEN)
    }

    /// 平文を DPAPI (マシンスコープ) で暗号化する。**同じ PC のどのアカウントでも復号できる**ので、
    /// 機密性は保存先ファイルの ACL で担保すること (ここが守るのはディスク持ち出し等のオフライン
    /// 読み取りだけ)。サインイン前でも SYSTEM が読める必要がある host の接続パスワード用。
    pub fn protect_machine(plain: &[u8]) -> Result<Vec<u8>, DpapiError> {
        protect_with(
            plain,
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
        )
    }

    fn protect_with(plain: &[u8], flags: u32) -> Result<Vec<u8>, DpapiError> {
        let mut input = plain.to_vec();
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB::default();

        // SAFETY: in_blob は input が指す cbData バイトの有効領域を指す。成功すると out は
        // DPAPI が LocalAlloc した領域を指すので、直後に OutBlob でラップして必ず解放する。
        let result = unsafe {
            CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, flags, &mut out)
        };
        let out = OutBlob(out);
        result.map_err(|source| DpapiError::Api {
            op: "CryptProtectData",
            source,
        })?;

        // SAFETY: 呼び出しが成功したので out.0 は cbData バイトの有効な暗号文を指す。
        let encrypted =
            unsafe { std::slice::from_raw_parts(out.0.pbData, out.0.cbData as usize) }.to_vec();

        // ヒープ上の平文を最小限消す (ベストエフォート)。
        for b in input.iter_mut() {
            *b = 0;
        }
        Ok(encrypted)
    }

    /// DPAPI で暗号化されたバイト列を復号する (ユーザー/マシンどちらのスコープの BLOB も可)。
    pub fn unprotect(enc: &[u8]) -> Result<Vec<u8>, DpapiError> {
        let mut input = enc.to_vec();
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB::default();

        // SAFETY: in_blob は input が指す cbData バイトの有効領域を指す。成功すると out は
        // DPAPI が LocalAlloc した領域を指すので OutBlob でラップして必ず解放する。
        let result = unsafe {
            CryptUnprotectData(
                &in_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        let out = OutBlob(out);
        result.map_err(|source| DpapiError::Api {
            op: "CryptUnprotectData",
            source,
        })?;

        // SAFETY: 呼び出しが成功したので out.0 は cbData バイトの有効な平文を指す。
        Ok(unsafe { std::slice::from_raw_parts(out.0.pbData, out.0.cbData as usize) }.to_vec())
    }
}

#[cfg(windows)]
pub use imp::{protect, protect_machine, unprotect};

/// 非 Windows では未対応。
#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, DpapiError> {
    Err(DpapiError::Unsupported)
}

/// 非 Windows では未対応。
#[cfg(not(windows))]
pub fn protect_machine(_plain: &[u8]) -> Result<Vec<u8>, DpapiError> {
    Err(DpapiError::Unsupported)
}

/// 非 Windows では未対応。
#[cfg(not(windows))]
pub fn unprotect(_enc: &[u8]) -> Result<Vec<u8>, DpapiError> {
    Err(DpapiError::Unsupported)
}
