//! host 接続パスワードのローカル保存。
//!
//! Windows: DPAPI (`CryptProtectData`) の**マシンスコープ**で暗号化し
//! `%ProgramData%\Aurora\host\password.bin` に保存する。平文の環境変数やレジストリへ秘密を
//! 置かないための置き換え。host は SYSTEM サービスとしてサインイン前にも読む必要があるため
//! ユーザースコープは使えない。マシンスコープは同じ PC の誰でも復号できるので、読み取りを
//! SYSTEM / Administrators に限るのは保存先ディレクトリの ACL ([`crate::system::state`]) の役目。
//!
//! 旧版 (ユーザースコープ・`%LOCALAPPDATA%`) からの移行は `--migrate-user-state` が行う
//! (旧 BLOB はそのユーザーでしか復号できないため、インストーラが利用者として実行する)。
//!
//! 非 Windows は未対応 (Windows 優先方針)。`load_password` は `None` を返し、
//! `save_password` / `clear_password` はエラーにする。

use anyhow::Result;

/// 保存ファイル名 (状態ディレクトリ配下。旧版の `%LOCALAPPDATA%\Aurora\` でも同名)。
#[cfg(target_os = "windows")]
pub(crate) const FILE_NAME: &str = "password.bin";

#[cfg(target_os = "windows")]
fn store_path() -> Result<std::path::PathBuf> {
    use anyhow::Context;
    let dir = crate::system::state::state_dir().context("resolve host state dir")?;
    Ok(dir.join(FILE_NAME))
}

/// パスワードを DPAPI (マシンスコープ) で暗号化してローカルに保存する。DPAPI 呼び出しは
/// [`aurora_common::dpapi`] に集約 (host/client 共通、unsafe FFI は 1 箇所)。
#[cfg(target_os = "windows")]
pub fn save_password(password: &str) -> Result<()> {
    use anyhow::Context;

    let path = store_path()?;
    let encrypted =
        aurora_common::dpapi::protect_machine(password.as_bytes()).context("encrypt password")?;
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
    let bytes =
        aurora_common::dpapi::unprotect(&data).context("decrypt password (corrupted store?)")?;
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
