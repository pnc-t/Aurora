//! host を Windows の「ログオン時」に自動起動するための登録 / 解除。
//!
//! 画面キャプチャ (DXGI Desktop Duplication) は対話ユーザセッションでしか動かない。
//! Session 0 で隔離実行される Windows サービスではキャプチャできないため、
//! タスクスケジューラの ONLOGON タスク (= ユーザセッションで実行) として登録する。
//!
//! パスワードはタスク定義へ埋め込まない (機密非保存方針)。同一ユーザーで一度
//! `aurora-host --set-password` を実行して DPAPI 暗号ファイルに保存しておけば、
//! 同じユーザーで走るログオン時タスクがそれを復号して読む。

use anyhow::Result;

/// 登録するタスク名。`--uninstall` で同名を削除する。
const TASK_NAME: &str = "Aurora Host";

#[cfg(target_os = "windows")]
pub fn install(forward_args: &[String]) -> Result<()> {
    use anyhow::{Context, bail};
    use std::process::Command;

    let exe = std::env::current_exe().context("get current exe path")?;
    let exe = exe.to_string_lossy();

    // タスクが実行するコマンド行。exe を引用し、転送する起動引数 (--port 等) を後ろへ。
    let mut tr = format!("\"{exe}\"");
    for a in forward_args {
        tr.push(' ');
        tr.push_str(a);
    }

    // /SC ONLOGON: 現在ユーザのログオン時に起動 (対話セッションなので DXGI が動く)
    // /RL HIGHEST: 昇格して実行 (UAC 昇格ウィンドウへの入力注入を可能にする)
    // /F: 既存タスクを上書き
    let status = Command::new("schtasks")
        .args([
            "/Create", "/TN", TASK_NAME, "/TR", &tr, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
        ])
        .status()
        .context("run schtasks /Create")?;
    if !status.success() {
        bail!("schtasks /Create failed (exit {:?})", status.code());
    }
    println!("registered scheduled task \"{TASK_NAME}\" (runs at logon)");
    println!("command: {tr}");
    println!(
        "note: run `aurora-host --set-password` once as this same user so the task can read the password"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<()> {
    use anyhow::{Context, bail};
    use std::process::Command;

    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .status()
        .context("run schtasks /Delete")?;
    if !status.success() {
        bail!("schtasks /Delete failed (exit {:?})", status.code());
    }
    println!("removed scheduled task \"{TASK_NAME}\"");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn install(_forward_args: &[String]) -> Result<()> {
    anyhow::bail!("--install is only supported on Windows");
}

#[cfg(not(target_os = "windows"))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("--uninstall is only supported on Windows");
}
