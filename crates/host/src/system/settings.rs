//! host の非機密ローカル設定 (待受ポート) の保存。
//!
//! パスワード ([`crate::security::secret`], DPAPI 暗号化) と異なりポート番号は機密ではないので、
//! 平文テキストで保存する。Windows: `%LOCALAPPDATA%\Aurora\port`。トレイの「ポートを
//! 変更」で書き込み、起動時に読む。`--port` 引数が明示された場合はそちらが優先される
//! (CLI > 保存値 > 既定、解決は `main` で行う)。
//!
//! 非 Windows は未対応 (Windows 優先方針)。`load_port` は `None`、`save_port` は Err。

use anyhow::Result;

/// 保存ファイル名 (`%LOCALAPPDATA%\Aurora\` 配下)。
#[cfg(target_os = "windows")]
const FILE_NAME: &str = "port";

#[cfg(target_os = "windows")]
fn store_path() -> Result<std::path::PathBuf> {
    use anyhow::Context;
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("Aurora");
    p.push(FILE_NAME);
    Ok(p)
}

/// 保存済み待受ポートを読む。未保存・不正値 (0 や非数値)・読めない場合は `None`。
#[cfg(target_os = "windows")]
pub fn load_port() -> Option<u16> {
    let path = store_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    match raw.trim().parse::<u16>() {
        Ok(p) if p != 0 => Some(p),
        _ => None,
    }
}

/// 待受ポートを保存する。
#[cfg(target_os = "windows")]
pub fn save_port(port: u16) -> Result<()> {
    use anyhow::Context;
    let path = store_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create Aurora settings dir")?;
    }
    std::fs::write(&path, port.to_string()).context("write port file")?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn load_port() -> Option<u16> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn save_port(_port: u16) -> Result<()> {
    anyhow::bail!("port persistence is only supported on Windows")
}
