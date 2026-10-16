//! host の非機密ローカル設定 (待受ポート・エンコード GPU 指定) の保存。
//!
//! パスワード ([`crate::security::secret`], DPAPI 暗号化) と異なりこれらは機密ではないので、
//! 平文テキストで保存する。設定ごとに 1 ファイル (`%ProgramData%\Aurora\host\<名前>`、ACL で
//! SYSTEM/Administrators のみ = 一般ユーザーは改ざんできない) に分ける。
//! - `port`: トレイの「ポートを変更」で書き込み、起動時に読む。`--port` 引数が明示された
//!   場合はそちらが優先される (CLI > 保存値 > 既定、解決は `main` で行う)。
//! - `encode-gpu`: `--encode-gpu` はトレイ UI が無く CLI が唯一の入口のため、`--port` と異なり
//!   明示指定時にここへ永続化する (CLI 明示 > 保存値 > 既定、解決は `main` で行う)。
//!
//! 非 Windows は未対応 (Windows 優先方針)。`load_*` は `None`、`save_*`/`clear_*` は Err。

use anyhow::Result;

#[cfg(target_os = "windows")]
fn store_path(file_name: &str) -> Result<std::path::PathBuf> {
    use anyhow::Context;
    let dir = crate::system::state::state_dir().context("resolve host state dir")?;
    Ok(dir.join(file_name))
}

/// 保存済み待受ポートを読む。未保存・不正値 (0 や非数値)・読めない場合は `None`。
#[cfg(target_os = "windows")]
pub fn load_port() -> Option<u16> {
    let path = store_path("port").ok()?;
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
    let path = store_path("port")?;
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

/// 保存済みのエンコード GPU 指定 (名前部分一致文字列) を読む。未保存・空なら None。
#[cfg(target_os = "windows")]
pub fn load_encode_gpu() -> Option<String> {
    let path = store_path("encode-gpu").ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// エンコード GPU 指定を保存する。トレイ UI が無く CLI が唯一の入口のため、明示指定を永続化する。
#[cfg(target_os = "windows")]
pub fn save_encode_gpu(name: &str) -> Result<()> {
    use anyhow::Context;
    let path = store_path("encode-gpu")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create Aurora settings dir")?;
    }
    std::fs::write(&path, name).context("write encode-gpu file")?;
    Ok(())
}

/// エンコード GPU 指定を削除する (auto へ戻す)。ファイルが無ければ成功扱い。
#[cfg(target_os = "windows")]
pub fn clear_encode_gpu() -> Result<()> {
    use anyhow::Context;
    let path = store_path("encode-gpu")?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove encode-gpu file"),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn load_encode_gpu() -> Option<String> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn save_encode_gpu(_name: &str) -> Result<()> {
    anyhow::bail!("encode GPU persistence is only supported on Windows")
}

#[cfg(not(target_os = "windows"))]
pub fn clear_encode_gpu() -> Result<()> {
    anyhow::bail!("encode GPU persistence is only supported on Windows")
}
