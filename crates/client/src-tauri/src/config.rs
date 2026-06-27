//! クライアントのローカル設定永続化。
//!
//! 接続先 host (ラベル / Tailscale IP・hostname / ポート) の履歴と、クライアント
//! 設定 (既定フルスクリーン等) をアプリ設定ディレクトリの JSON に保存する。
//! WebView の localStorage は平文・キャッシュ消去で揮発・任意 JS から読める、
//! という理由で使わない。**この JSON にはパスワードを入れない**。パスワードは
//! 別途 `secret.rs` が DPAPI で暗号化して `passwords.bin` に保存する。

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

fn default_accent() -> String {
    "#3a82f7".to_string()
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_toolbar_position() -> String {
    "top".to_string()
}

/// クライアント側の永続設定。接続後の挙動と UI の既定値。
/// 各フィールドは `#[serde(default)]` で、項目を増やしても旧 settings.json を読める。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSettings {
    /// 接続成立時に自動でフルスクリーンにする。
    #[serde(default)]
    pub default_fullscreen: bool,
    /// 接続成立時に統計 HUD を表示する。
    #[serde(default)]
    pub show_stats: bool,
    /// 接続成立時に自動でマウスキャプチャ (相対マウス) を有効化する。
    #[serde(default)]
    pub default_capture: bool,
    /// ホスト追加ダイアログの port 欄の初期値。
    #[serde(default = "default_port")]
    pub default_port: u16,
    /// 接続後、ツールバーを最初からピン留め表示する。
    #[serde(default)]
    pub toolbar_pinned: bool,
    /// ツールバーの配置 ("top" | "bottom" | "left" | "right")。
    /// left/right ではピルが縦並びのサイドバーになる。CSS の位置クラスへ渡す。
    #[serde(default = "default_toolbar_position")]
    pub toolbar_position: String,
    /// タブストリップの配置 ("top" | "bottom" | "left" | "right")。
    /// left/right では縦並びのサイドタブになる。CSS の位置クラスへ渡す。
    #[serde(default = "default_toolbar_position")]
    pub tab_position: String,
    /// UI のアクセント色 (CSS 色)。
    #[serde(default = "default_accent")]
    pub accent_color: String,
    /// 配色テーマ ("auto" | "dark" | "light" | "legacy")。CSS の data-theme に渡す。
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 接続時にホストへ送る送出 fps 上限 (0 = 無制限)。
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// 接続時にホストへ送るビットレート上限 (Mbps)。
    #[serde(default = "default_max_mbps")]
    pub max_mbps: u32,
    /// 通信断などで切れたとき自動で再接続を試みる。
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    /// 接続時にホストの仮想ディスプレイ (IDD / MttVDD) をキャプチャ対象にする (RDP 風)。
    /// 既定 OFF (物理ディスプレイをキャプチャ)。ホストに仮想ディスプレイが無ければ無視される。
    #[serde(default)]
    pub use_virtual_display: bool,
    /// 接続時にこのPC (client) の This PC 全体 (全ドライブ) をホストの仮想ドライブへ公開する。
    /// 既定 OFF (共有しない)。ON でも、ホスト側に登録済みの端末でなければドライブは出ない
    /// (host net.rs の client_trusted ゲート)。中身は接続中のみ要求時に読まれる。
    #[serde(default)]
    pub share_files: bool,
}

fn default_fps() -> u32 {
    60
}

fn default_max_mbps() -> u32 {
    15
}

fn default_true() -> bool {
    true
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            default_fullscreen: false,
            show_stats: false,
            default_capture: false,
            default_port: DEFAULT_PORT,
            toolbar_pinned: false,
            toolbar_position: default_toolbar_position(),
            tab_position: default_toolbar_position(),
            accent_color: default_accent(),
            theme: default_theme(),
            fps: default_fps(),
            max_mbps: default_max_mbps(),
            auto_reconnect: true,
            use_virtual_display: false,
            share_files: false,
        }
    }
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

/// 接続先を保存する。手動並べ替えを尊重するため、既存 host (大文字小文字無視) は
/// **位置を保ったまま**内容を更新し、新規 host のみ先頭へ追加する。最大件数で切り詰める。
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
    match hosts
        .iter()
        .position(|h| h.host.eq_ignore_ascii_case(&entry.host))
    {
        Some(i) => hosts[i] = entry, // 既存は位置維持で更新
        None => hosts.insert(0, entry),
    }
    hosts.truncate(MAX_RECENT);
    write_hosts(&app, &hosts)
}

/// 履歴の並び順を丸ごと置き換える (ドラッグ並べ替え後にフロントから呼ぶ)。
#[tauri::command]
pub fn set_recent_hosts(app: AppHandle, hosts: Vec<HostEntry>) -> Result<(), String> {
    let mut hosts = hosts;
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
