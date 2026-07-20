//! クライアントのローカル設定永続化。
//!
//! 接続先 host (ラベル / Tailscale IP・hostname / ポート) の履歴と、クライアント
//! 設定 (既定フルスクリーン等) をアプリ設定ディレクトリの JSON に保存する。
//! WebView の localStorage は平文・キャッシュ消去で揮発・任意 JS から読める、
//! という理由で使わない。**この JSON にはパスワードを入れない**。パスワードは
//! 別途 `secret.rs` が DPAPI で暗号化して `passwords.bin` に保存する。

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 履歴の保持上限。最新が先頭。
const MAX_RECENT: usize = 8;
const HOSTS_FILE: &str = "recent_hosts.json";
const SETTINGS_FILE: &str = "settings.json";
const WOL_TARGETS_FILE: &str = "wol_targets.json";
/// host の既定ポート (host 側 --port の既定と揃える)。
const DEFAULT_PORT: u16 = 7000;
/// WoL 中継デーモン (aurora-wol) の既定ポート (host 側 7000 と衝突しないよう別ポート)。
const DEFAULT_WOL_RELAY_PORT: u16 = 7100;

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
    /// カードのアバター画像 (data URL、空なら頭文字アバターにフォールバック)。
    /// `None` は「情報なし」(capture 側の接続成功時保存)。更新時は既存値を保持し、
    /// 明示値 (`Some`、空文字 = 解除) はそのまま採用する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// ホスト別の設定上書き (無ければグローバル設定に従う)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<HostOverrides>,
    /// host から HostInfo で通知されたプライマリ NIC の MAC アドレス
    /// (`"AA:BB:CC:DD:EE:FF"` 形式)。WoL 対象の自動登録に使う。`icon` と同じ
    /// 「`None`=情報なし (既存値保持)」規約: 接続成立時の自動保存だけがこれを更新する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// 最後に接続成功した日時 (unix 秒)。`icon`/`mac` と同じ「`None`=情報なし (既存値保持)」
    /// 規約: ハンドシェイク成功時 (HostInfo 受信) の自動更新だけがこれを書き換える。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<u64>,
}

/// ホスト別のクライアント設定上書き。未指定 (None) のフィールドはグローバル設定
/// (`ClientSettings`) に従う。launcher のホスト編集ダイアログで設定する。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostOverrides {
    /// 仮想ディスプレイを使うか (RDP 風)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_virtual_display: Option<bool>,
    /// 送出 fps 上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// ビットレート上限 (Mbps)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mbps: Option<u32>,
    /// 映像コーデック ("auto" | "av1" | "hevc" | "h264")。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

impl HostOverrides {
    /// 全フィールド未指定 (= 編集ダイアログの「既定に従う」への明示リセット) か。
    fn is_empty(&self) -> bool {
        self.use_virtual_display.is_none()
            && self.fps.is_none()
            && self.max_mbps.is_none()
            && self.codec.is_none()
    }
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

fn default_stats_position() -> String {
    "tr".to_string()
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
    /// 接続時にホストへ申告する映像コーデック ("auto" | "av1" | "hevc" | "h264")。
    /// "auto" は host/client 双方が対応する最良 (優先順位 AV1>HEVC>H264) を host が選ぶ。
    /// 特定値を選ぶと `SetSupportedCodecs` をその 1 つだけに絞って送り実質固定する。
    #[serde(default = "default_codec")]
    pub codec: String,
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
    /// 接続時に音声をミュートで開始する。ツールバーからいつでも解除できる。
    #[serde(default)]
    pub audio_muted: bool,
    /// クライアント再生音量 (0.0〜1.0)。既定 1.0。ツールバーのスライダーで変更・永続化される。
    #[serde(default = "default_audio_volume")]
    pub audio_volume: f32,
    /// マウスキャプチャ中にキーボード占有 (WH_KEYBOARD_LL) を有効にするか。既定 ON (現状踏襲):
    /// Win キーや Alt+Tab もリモートへ送る。OFF にすると手元の Windows に効く。
    #[serde(default = "default_true")]
    pub grab_keyboard: bool,
    /// 統計 HUD の表示位置 ("tl" | "tr" | "bl" | "br")。
    #[serde(default = "default_stats_position")]
    pub stats_position: String,
    /// ローカル UI ショートカットのユーザー割当。キーは `LocalHotkey` id (TS 側
    /// `core/shortcuts.ts` の `SHORTCUTS`)、値は `"Ctrl+Alt+KeyF"` 形式のバインディング文字列
    /// (修飾は Ctrl/Alt/Shift/Meta の固定順 + `KeyboardEvent.code`)。未登録の id は既定
    /// (Ctrl+Alt+Shift+<code>) を使う。空マップが既定 (= 全操作が既定バインディング)。
    #[serde(default)]
    pub shortcuts: HashMap<String, String>,
    /// クリップボード同期を行うか。既定 ON。接続時にセッションのゲート初期値として使われ、
    /// 以後はツールバーのトグル (`set_clipboard_sync`) がセッション単位で切り替える。
    #[serde(default = "default_true")]
    pub clipboard_sync: bool,
}

fn default_fps() -> u32 {
    60
}

fn default_codec() -> String {
    "auto".to_string()
}

fn default_max_mbps() -> u32 {
    // 標準プリセット (video-presets.ts) と一致させる。15M 以上は 1440p HW エンコードの
    // 持続可能上限を超えバッファブロートで遅延・カクつきが悪化する実測 (2026-07-02)。
    8
}

fn default_true() -> bool {
    true
}

fn default_audio_volume() -> f32 {
    1.0
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
            codec: default_codec(),
            auto_reconnect: true,
            use_virtual_display: false,
            share_files: false,
            audio_muted: false,
            audio_volume: 1.0,
            grab_keyboard: true,
            stats_position: default_stats_position(),
            shortcuts: HashMap::new(),
            clipboard_sync: true,
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
            icon: None,
            overrides: None,
            mac: None,
            last_connected: None,
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
    let mut entry = HostEntry {
        label: entry.label.trim().to_string(),
        host,
        port: entry.port,
        icon: entry.icon,
        overrides: entry.overrides,
        mac: entry.mac,
        last_connected: entry.last_connected,
    };

    let mut hosts = load_recent_hosts(app.clone())?;
    match hosts
        .iter()
        .position(|h| h.host.eq_ignore_ascii_case(&entry.host))
    {
        Some(i) => {
            // capture 側の接続成功時保存は icon/overrides/mac/last_connected を持たない
            // (None = 情報なし) ため既存値を残す (毎接続でアバター画像・ホスト別設定・MAC・
            // 最終接続日時が消えるのを防ぐ)。編集ダイアログは常に明示値を送る: icon の空文字 = 解除、
            // overrides の空オブジェクト = 「既定に従う」へのリセット (None へ正規化)。
            if entry.icon.is_none() {
                entry.icon = hosts[i].icon.clone();
            }
            if entry.mac.is_none() {
                entry.mac = hosts[i].mac.clone();
            }
            if entry.last_connected.is_none() {
                entry.last_connected = hosts[i].last_connected;
            }
            match entry.overrides.as_ref() {
                None => entry.overrides = hosts[i].overrides.clone(),
                Some(o) if o.is_empty() => entry.overrides = None,
                Some(_) => {}
            }
            hosts[i] = entry; // 既存は位置維持で更新
        }
        None => {
            // 新規でも空オブジェクトの overrides は保存形を None に正規化する。
            if entry.overrides.as_ref().is_some_and(|o| o.is_empty()) {
                entry.overrides = None;
            }
            hosts.insert(0, entry);
        }
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

/// WoL (Wake-on-LAN) 中継先の 1 件。`wake_host` (net/wol.rs) が `relay_host:relay_port`
/// (aurora-wol) へ接続し、`mac` 宛のマジックパケット送出を依頼する。`host`/`port` は
/// 紐付く登録済みホスト (プローブ/自動接続で使う。任意)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WolTarget {
    /// 一覧・削除・更新のキー。
    pub id: String,
    /// 一覧での表示名。
    #[serde(default)]
    pub label: String,
    /// 対象 PC の MAC アドレス (`"AA:BB:CC:DD:EE:FF"` 形式)。
    pub mac: String,
    /// WoL 中継デーモン (aurora-wol) の Tailscale IP / hostname。
    pub relay_host: String,
    /// aurora-wol の待受ポート。
    #[serde(default = "default_wol_relay_port")]
    pub relay_port: u16,
    /// 紐付く登録済みホスト (プローブ/自動接続用、任意)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// カードのアバター画像 (data URL、任意)。未設定なら頭文字アバターを描く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

fn default_wol_relay_port() -> u16 {
    DEFAULT_WOL_RELAY_PORT
}

fn wol_targets_path(app: &AppHandle) -> Result<PathBuf, String> {
    config_path(app, WOL_TARGETS_FILE)
}

/// WoL 対象の一覧を読み込む。ファイルが無ければ空。
#[tauri::command]
pub fn list_wol_targets(app: AppHandle) -> Result<Vec<WolTarget>, String> {
    let path = wol_targets_path(&app)?;
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("WoL 対象の読み込みに失敗: {e}")),
    };
    serde_json::from_str(&raw).map_err(|e| format!("WoL 対象の解析に失敗: {e}"))
}

/// WoL 対象を保存する。`id` が一致する既存エントリは置換し、無ければ追加する。
#[tauri::command]
pub fn save_wol_target(app: AppHandle, target: WolTarget) -> Result<(), String> {
    let mut targets = list_wol_targets(app.clone())?;
    match targets.iter().position(|t| t.id == target.id) {
        Some(i) => targets[i] = target,
        None => targets.push(target),
    }
    write_wol_targets(&app, &targets)
}

/// WoL 対象一覧を丸ごと差し替える (並べ替えの保存用)。
#[tauri::command]
pub fn set_wol_targets(app: AppHandle, targets: Vec<WolTarget>) -> Result<(), String> {
    write_wol_targets(&app, &targets)
}

/// 指定 id の WoL 対象を削除する。
#[tauri::command]
pub fn delete_wol_target(app: AppHandle, id: String) -> Result<(), String> {
    let mut targets = list_wol_targets(app.clone())?;
    targets.retain(|t| t.id != id);
    write_wol_targets(&app, &targets)
}

fn write_wol_targets(app: &AppHandle, targets: &[WolTarget]) -> Result<(), String> {
    let path = wol_targets_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config dir 作成に失敗: {e}"))?;
    }
    let json = serde_json::to_string_pretty(targets)
        .map_err(|e| format!("WoL 対象の直列化に失敗: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("WoL 対象の書き込みに失敗: {e}"))
}
