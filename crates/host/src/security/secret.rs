//! host 接続パスワードのローカル保存。
//!
//! Windows: DPAPI (`CryptProtectData` / `CryptUnprotectData`) でユーザープロファイル鍵に
//! 紐づけて暗号化し `%LOCALAPPDATA%\Aurora\password.bin` に保存する。平文の環境変数や
//! レジストリへ秘密を置かないための置き換え。復号できるのは保存したのと同じユーザー
//! アカウントのみ (他ユーザー・他PCでは不可) なので、同一ユーザーで走るログオン時タスク
//! からは無人で読めるが、それ以外からは読めない。
//!
//! 非 Windows は未対応 (Windows 優先方針)。`load_password` は `None` を返し、
//! `save_password` / `clear_password` はエラーにする。

use anyhow::Result;

/// 保存ファイル名 (`%LOCALAPPDATA%\Aurora\` 配下)。
#[cfg(target_os = "windows")]
const FILE_NAME: &str = "password.bin";

#[cfg(target_os = "windows")]
fn store_path() -> Result<std::path::PathBuf> {
    use anyhow::Context;
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("Aurora");
    p.push(FILE_NAME);
    Ok(p)
}

/// DPAPI 出力 BLOB。`pbData` は DPAPI が `LocalAlloc` した領域なので `Drop` で
/// `LocalFree` する (CLAUDE.md: FFI 取得リソースは Drop で解放)。
#[cfg(target_os = "windows")]
struct OutBlob(windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB);

#[cfg(target_os = "windows")]
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

/// パスワードを DPAPI で暗号化してローカルに保存する。
#[cfg(target_os = "windows")]
pub fn save_password(password: &str) -> Result<()> {
    use anyhow::Context;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    use windows::core::PCWSTR;

    let path = store_path()?;
    let mut input = password.as_bytes().to_vec();
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();

    // SAFETY: in_blob は input が指す cbData バイトの有効領域を指す。成功すると out は
    // DPAPI が LocalAlloc した領域を指すので、直後に OutBlob でラップして必ず解放する。
    let result = unsafe {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    let out = OutBlob(out);
    result.context("CryptProtectData failed")?;

    // SAFETY: 呼び出しが成功したので out.0 は cbData バイトの有効な暗号文を指す。
    let encrypted =
        unsafe { std::slice::from_raw_parts(out.0.pbData, out.0.cbData as usize) }.to_vec();

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create password store dir")?;
    }
    std::fs::write(&path, &encrypted).context("write password store")?;

    // ヒープ上の平文を最小限消す (ベストエフォート)。
    for b in input.iter_mut() {
        *b = 0;
    }
    Ok(())
}

/// 保存済みパスワードを復号して返す。未保存なら `None`。
#[cfg(target_os = "windows")]
pub fn load_password() -> Result<Option<String>> {
    use anyhow::Context;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let path = store_path()?;
    let mut data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read password store"),
    };
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();

    // SAFETY: in_blob は data が指す cbData バイトの有効領域を指す。成功すると out は
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
    result.context("CryptUnprotectData failed (different user or corrupted store?)")?;

    // SAFETY: 呼び出しが成功したので out.0 は cbData バイトの有効な平文を指す。
    let bytes = unsafe { std::slice::from_raw_parts(out.0.pbData, out.0.cbData as usize) }.to_vec();
    let s = String::from_utf8(bytes).context("stored password is not valid UTF-8")?;
    Ok(Some(s))
}

/// 保存済みパスワードを削除する。未保存でも成功扱い。
#[cfg(target_os = "windows")]
pub fn clear_password() -> Result<()> {
    use anyhow::Context;
    let path = store_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove password store"),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn save_password(_password: &str) -> Result<()> {
    anyhow::bail!("password storage is only supported on Windows")
}

#[cfg(not(target_os = "windows"))]
pub fn load_password() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(target_os = "windows"))]
pub fn clear_password() -> Result<()> {
    anyhow::bail!("password storage is only supported on Windows")
}
