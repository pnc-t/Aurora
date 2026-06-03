//! クライアントのローカル設定永続化。
//!
//! 接続先 host (Tailscale IP / hostname) の履歴のみをアプリ設定ディレクトリの
//! JSON ファイルに保存する。WebView の localStorage は平文・キャッシュ消去で揮発・
//! 任意 JS から読める、という理由で使わない。パスワードは保存しない (毎回入力)。

use std::fs;
use std::path::PathBuf;

use tauri::{AppHandle, Manager};

/// 履歴の保持上限。最新が先頭。
const MAX_RECENT: usize = 8;
const FILE_NAME: &str = "recent_hosts.json";

/// 履歴ファイルのパス (`app_config_dir/recent_hosts.json`)。
fn recent_hosts_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir 解決に失敗: {e}"))?;
    Ok(dir.join(FILE_NAME))
}

/// 接続先 host の履歴を読み込む (最新が先頭)。ファイルが無ければ空。
#[tauri::command]
pub fn load_recent_hosts(app: AppHandle) -> Result<Vec<String>, String> {
    let path = recent_hosts_path(&app)?;
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        // 未作成は正常 (初回起動)。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("履歴の読み込みに失敗: {e}")),
    };
    serde_json::from_str(&raw).map_err(|e| format!("履歴の解析に失敗: {e}"))
}

/// 接続成功した host を履歴の先頭へ重複排除して追加し、最大件数で切り詰めて保存する。
#[tauri::command]
pub fn save_recent_host(app: AppHandle, host: String) -> Result<(), String> {
    let host = host.trim().to_string();
    if host.is_empty() {
        return Ok(());
    }
    let path = recent_hosts_path(&app)?;

    let mut hosts = load_recent_hosts(app)?;
    // 既存の同一 host を除去してから先頭へ (大文字小文字は区別しない)。
    hosts.retain(|h| !h.eq_ignore_ascii_case(&host));
    hosts.insert(0, host);
    hosts.truncate(MAX_RECENT);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config dir 作成に失敗: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(&hosts).map_err(|e| format!("履歴の直列化に失敗: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("履歴の書き込みに失敗: {e}"))
}
