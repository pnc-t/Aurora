//! クライアントのローカル設定永続化。
//!
//! 接続先 host (ラベル / Tailscale IP・hostname / ポート) の履歴と、クライアント
//! 設定 (既定フルスクリーン等) をアプリ設定ディレクトリの JSON に保存する。
//! WebView の localStorage は平文・キャッシュ消去で揮発・任意 JS から読める、
//! という理由で使わない。**パスワードは保存しない** (接続時に毎回入力)。

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 履歴の保持上限。最新が先頭。
const MAX_RECENT: usize = 8;
const HOSTS_FILE: &str = "recent_hosts.json";
const SETTINGS_FILE: &str = "settings.json";
/// host の既定ポート (host 側 --port の既定と揃える)。
const DEFAULT_PORT: u16 = 7000;

/// 保存する 1 件の接続先。`label` は一覧での表示名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEntry {
    /// 一覧での表示名 (空なら host を表示に使う)。
    #[serde(default)]
    pub label: String,
    /// Tailscale IP / hostname。
    pub host: String,
    /// 接続ポート (host 側 --port と一致させる)。
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// クライアント側の永続設定。接続後の挙動の既定値。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientSettings {
    /// 接続成立時に自動でフルスクリーンにする。
    #[serde(default)]
    pub default_fullscreen: bool,
    /// 接続成立時に統計 HUD を表示する。
    #[serde(default)]
    pub show_stats: bool,
}

fn config_path(app: &AppHandle, file: &str) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir 解決に失敗: {e}"))?;
    Ok(dir.join(file))
}

/// 接続先 host の履歴を読み込む (最新が先頭)。ファイルが無ければ空。
///
/// 旧形式 (`["100.x.x.x", ...]` の文字列配列) も読み、`label`=host・既定ポートへ
/// 変換して後方互換を保つ。
#[tauri::command]
pub fn load_recent_hosts(app: AppHandle) -> Result<Vec<HostEntry>, String> {
    let path = config_path(&app, HOSTS_FILE)?;
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("履歴の読み込みに失敗: {e}")),
    };
    // 新形式 (オブジェクト配列) を優先し、失敗したら旧形式 (文字列配列) を試す。
    if let Ok(entries) = serde_json::from_str::<Vec<HostEntry>>(&raw) {
        return Ok(entries);
    }
    let legacy: Vec<String> =
        serde_json::from_str(&raw).map_err(|e| format!("履歴の解析に失敗: {e}"))?;
    Ok(legacy
        .into_iter()
        .map(|host| HostEntry {
            label: host.clone(),
            host,
            port: DEFAULT_PORT,
        })
        .collect())
}

/// 接続先を履歴の先頭へ重複排除して追加し、最大件数で切り詰めて保存する。
/// 重複判定は host (大文字小文字無視) で行うため、同一 host の再保存は更新になる。
#[tauri::command]
pub fn save_recent_host(app: AppHandle, entry: HostEntry) -> Result<(), String> {
    let host = entry.host.trim().to_string();
    if host.is_empty() {
        return Ok(());
    }
    let entry = HostEntry {
        label: entry.label.trim().to_string(),
        host,
        port: entry.port,
    };

    let mut hosts = load_recent_hosts(app.clone())?;
    hosts.retain(|h| !h.host.eq_ignore_ascii_case(&entry.host));
    hosts.insert(0, entry);
    hosts.truncate(MAX_RECENT);
    write_hosts(&app, &hosts)
}

/// 指定 host の履歴を削除する。
#[tauri::command]
pub fn delete_recent_host(app: AppHandle, host: String) -> Result<(), String> {
    let mut hosts = load_recent_hosts(app.clone())?;
    hosts.retain(|h| !h.host.eq_ignore_ascii_case(host.trim()));
    write_hosts(&app, &hosts)
}

fn write_hosts(app: &AppHandle, hosts: &[HostEntry]) -> Result<(), String> {
    let path = config_path(app, HOSTS_FILE)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config dir 作成に失敗: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(hosts).map_err(|e| format!("履歴の直列化に失敗: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("履歴の書き込みに失敗: {e}"))
}

/// クライアント設定を読み込む。ファイルが無ければ既定値。
#[tauri::command]
pub fn load_settings(app: AppHandle) -> Result<ClientSettings, String> {
    let path = config_path(&app, SETTINGS_FILE)?;
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ClientSettings::default());
        }
        Err(e) => return Err(format!("設定の読み込みに失敗: {e}")),
    };
    serde_json::from_str(&raw).map_err(|e| format!("設定の解析に失敗: {e}"))
}

/// クライアント設定を保存する。
#[tauri::command]
pub fn save_settings(app: AppHandle, settings: ClientSettings) -> Result<(), String> {
    let path = config_path(&app, SETTINGS_FILE)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config dir 作成に失敗: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(&settings).map_err(|e| format!("設定の直列化に失敗: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("設定の書き込みに失敗: {e}"))
}
