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

/// 自前のコンソールを開かずに子プロセスを起動するフラグ (`CREATE_NO_WINDOW`)。GUI サブシステムの
/// トレイ常駐から `schtasks` を呼ぶと一瞬コンソール窓が点滅するため抑止する。CLI 実行時は既存の
/// 親コンソールへ出力されるため害はない。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `--install` / `--uninstall` 自体を除いた、現プロセスの起動引数を返す。登録するログオン時
/// タスクへ転送して、自動起動でも手動起動と同じ実効設定 (`--bind` / `--log-level` 等) で動かす。
/// CLI の `--install` 処理とトレイからの登録で同じ算出を使うため共通化する。
pub fn current_forward_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| a != "--install" && a != "--uninstall")
        .collect()
}

/// ログオン時自動起動タスクが現在登録されているかを返す。`schtasks /Query` の終了コードで判定し、
/// 出力は破棄する (トレイのメニュー表示用)。非 Windows や問い合わせ失敗時は `false`。
#[cfg(target_os = "windows")]
pub fn is_installed() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
pub fn is_installed() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn install(forward_args: &[String]) -> Result<()> {
    use anyhow::{Context, bail};
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let exe = std::env::current_exe().context("get current exe path")?;
    let exe = exe.to_string_lossy();

    // タスクが実行するコマンド行。exe を引用し、転送する起動引数 (--port 等) を後ろへ。
    // 空白を含む引数は引用してコマンド行が壊れないようにする (値はローカル CLI 由来)。
    let mut tr = format!("\"{exe}\"");
    for a in forward_args {
        tr.push(' ');
        if a.is_empty() || a.contains(' ') || a.contains('\t') {
            tr.push('"');
            tr.push_str(a);
            tr.push('"');
        } else {
            tr.push_str(a);
        }
    }

    // /SC ONLOGON: 現在ユーザのログオン時に起動 (対話セッションなので DXGI が動く)
    // /RL HIGHEST: 昇格して実行 (UAC 昇格ウィンドウへの入力注入を可能にする)
    // /F: 既存タスクを上書き
    let status = Command::new("schtasks")
        .args([
            "/Create", "/TN", TASK_NAME, "/TR", &tr, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("run schtasks /Create")?;
    if !status.success() {
        bail!("schtasks /Create failed (exit {:?})", status.code());
    }
    println!("registered scheduled task \"{TASK_NAME}\" (runs at logon)");
    println!("command: {tr}");
    println!(
        "note: set the connection password from the tray icon (right-click -> change password),"
    );
    println!(
        "      or run `aurora-host --set-password` once as this same user. Without one, a random"
    );
    println!("      password is generated each start until you set a fixed one.");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<()> {
    use anyhow::{Context, bail};
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
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
