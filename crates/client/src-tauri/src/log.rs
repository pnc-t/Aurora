//! フロント (WebView / TypeScript) のログを Rust 側の `tracing` に集約するブリッジ。
//!
//! release の WebView には開発者コンソールが無く、`console.*` も未捕捉例外も現地で消える。
//! フロントの `core/log.ts` がこのコマンド経由でログを送り、`client.<日付>.log` に
//! まとめて残す (target は `aurora::frontend`。既定フィルタ `aurora=info` に乗る)。
//!
//! 注意: 映像フレーム単位の高頻度ログをここへ流さないこと (JSON IPC のコスト。CLAUDE.md の
//! 「映像はバイナリ Channel」原則と同じ理由で、ログも低頻度イベントに限る)。

/// フロントから 1 行分のログを受け取り `tracing` へ転送する。`level` は
/// `error`/`warn`/`info`/`debug` のいずれか (未知は info 扱い)。
#[tauri::command]
pub fn frontend_log(level: String, message: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "aurora::frontend", "{message}"),
        "warn" => tracing::warn!(target: "aurora::frontend", "{message}"),
        "debug" => tracing::debug!(target: "aurora::frontend", "{message}"),
        _ => tracing::info!(target: "aurora::frontend", "{message}"),
    }
}

/// 計測ログ (`vstat` 等) を出すかどうか。環境変数 `AURORA_PERF` が空でない値で設定されて
/// いれば有効 (`0` / `false` は無効)。ホストの `--perf` 引数に対応するクライアント側スイッチ。
/// フロントは起動時に 1 回だけ問い合わせ、有効時のみ受信→提示の遅延集計をログへ流す。
#[tauri::command]
pub fn perf_enabled() -> bool {
    match std::env::var("AURORA_PERF") {
        Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// ログフォルダ (`%LOCALAPPDATA%\Aurora\logs`) をエクスプローラで開く。未作成なら作る。
#[tauri::command]
pub fn open_logs_folder() -> Result<(), String> {
    let dir =
        aurora_common::logging::default_log_dir().ok_or("ログフォルダの場所を特定できません")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    reveal(&dir)
}

/// ログ一式 (`logs\*.log`) を 1 つの ZIP にまとめて書き出し、生成先パスを返す。
/// 出力先は logs の親 (`%LOCALAPPDATA%\Aurora`) で、書き出し後にそのフォルダを開く。
/// クライアント機にあるのは client ログ (フロント転送含む) で、host ログは別マシンにある
/// (host 側はトレイ「ログフォルダを開く」で集める)。
#[tauri::command]
pub fn export_logs_zip() -> Result<String, String> {
    let dir =
        aurora_common::logging::default_log_dir().ok_or("ログフォルダの場所を特定できません")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out_dir = dir.parent().unwrap_or(dir.as_path()).to_path_buf();

    #[cfg(windows)]
    {
        // PowerShell の Compress-Archive で固める。ファイル名はタイムスタンプ付き。
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             $ts = Get-Date -Format 'yyyyMMdd-HHmmss'; \
             $dst = Join-Path '{out}' \"aurora-logs-$ts.zip\"; \
             Compress-Archive -Path (Join-Path '{logs}' '*') -DestinationPath $dst -Force; \
             Write-Output $dst",
            out = out_dir.display(),
            logs = dir.display(),
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ZIP 作成に失敗: {}", err.trim()));
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // 生成先フォルダを開いて見つけやすくする (失敗は致命的でないので無視)。
        let _ = reveal(&out_dir);
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        let _ = out_dir;
        Err("この OS では未対応です".into())
    }
}

/// 指定フォルダをファイラで開く。
fn reveal(dir: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Err("この OS では未対応です".into())
    }
}
