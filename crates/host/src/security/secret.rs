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
    let dir = aurora_common::paths::data_dir().context("LOCALAPPDATA is not set")?;
    Ok(dir.join(FILE_NAME))
}

/// パスワードを DPAPI で暗号化してローカルに保存する。DPAPI 呼び出しは
/// [`aurora_common::dpapi`] に集約 (host/client 共通、unsafe FFI は 1 箇所)。
#[cfg(target_os = "windows")]
pub fn save_password(password: &str) -> Result<()> {
    use anyhow::Context;

    let path = store_path()?;
    let encrypted =
        aurora_common::dpapi::protect(password.as_bytes()).context("encrypt password")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create password store dir")?;
    }
    std::fs::write(&path, &encrypted).context("write password store")?;
    Ok(())
}

/// 保存済みパスワードを復号して返す。未保存なら `None`。
#[cfg(target_os = "windows")]
pub fn load_password() -> Result<Option<String>> {
    use anyhow::Context;

    let path = store_path()?;
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read password store"),
    };
    let bytes = aurora_common::dpapi::unprotect(&data)
        .context("decrypt password (different user or corrupted store?)")?;
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
