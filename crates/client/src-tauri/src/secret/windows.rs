//! Windows Adapter: DPAPI 呼び出しは [`aurora_common::dpapi`] に集約 (host/client 共通、
//! unsafe FFI は 1 箇所)。

pub(super) fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    aurora_common::dpapi::protect(plain).map_err(|e| format!("CryptProtectData failed: {e}"))
}

pub(super) fn unprotect(enc: &[u8]) -> Option<Vec<u8>> {
    aurora_common::dpapi::unprotect(enc).ok()
}
