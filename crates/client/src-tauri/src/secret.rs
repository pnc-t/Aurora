//! 接続先 host ごとのパスワードのローカル暗号化保存。
//!
//! Windows: DPAPI (`CryptProtectData` / `CryptUnprotectData`) でログオンユーザーの鍵に
//! 紐づけて暗号化し、`app_config_dir/passwords.bin` に保存する。`recent_hosts.json`
//! (平文) には入れない。復号できるのは保存したのと同じ Windows ユーザーだけ。
//!
//! 中身は `{ host(小文字): password }` の JSON マップを丸ごと暗号化したもの。host を
//! キーにするのは `recent_hosts` の重複排除 (大文字小文字無視) と揃えるため。
//!
//! 非 Windows は未対応 (Windows 優先)。`get` は `None`、`put` はエラー、`delete` は no-op。

use std::collections::BTreeMap;
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

const FILE: &str = "passwords.bin";
const IDENTITY_FILE: &str = "identity.bin";

fn key(host: &str) -> String {
    host.trim().to_lowercase()
}

/// この端末の永続 ID 鍵 (Noise 静的鍵ペア) をシリアライズした中身。
#[derive(serde::Serialize, serde::Deserialize)]
struct Identity {
    private: Vec<u8>,
    public: Vec<u8>,
}

fn identity_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(IDENTITY_FILE))
}

/// プロセス内の ID 鍵キャッシュ。接続ごとに再生成せず、一度確定した鍵を使い回す。
///
/// これが無いと、自動再接続が重なって `load_or_create_identity` が並行に走ったとき、
/// `identity.bin` の読み書きが競合して毎回別の鍵を生成してしまい、host のデバイスピン留めが
/// 「登録した直後の接続が別の鍵で未登録扱い」になって永久に繋がらない。Mutex で生成を直列化し、
/// 一度確定した鍵をプロセス生存中ずっと返すことで端末の身元を安定させる。
static IDENTITY_CACHE: Mutex<Option<aurora_common::noise::StaticKeypair>> = Mutex::new(None);

/// この端末の永続 ID 鍵ペアを返す。プロセス内でキャッシュし、無ければ生成して DPAPI で暗号化保存する。
///
/// host のデバイスピン留め (許可リスト照合) が端末を識別するための、セッションを跨いで
/// 安定した公開鍵を提供する。秘密鍵は機密のため DPAPI でユーザー鍵に紐づけて保存する。
/// 取得・生成に失敗した場合 (非 Windows 等) は `None` を返し、接続は従来の使い捨て鍵に
/// フォールバックする (ピン留め未使用なら影響なし)。
pub fn load_or_create_identity(app: &AppHandle) -> Option<aurora_common::noise::StaticKeypair> {
    // 生成を直列化し、確定済みの鍵があればそれを返す (接続ごとの鍵ブレを防ぐ)。
    let mut cache = IDENTITY_CACHE.lock().ok()?;
    if let Some(id) = cache.as_ref() {
        return Some(id.clone());
    }
    let path = identity_path(app)?;
    // 保存済みを読む。復号/パースできればそれを採用。
    if let Ok(bytes) = std::fs::read(&path) {
        if let Some(id) = decrypt(&bytes).and_then(|p| serde_json::from_slice::<Identity>(&p).ok())
        {
            let kp = aurora_common::noise::StaticKeypair {
                private: id.private,
                public: id.public,
            };
            *cache = Some(kp.clone());
            return Some(kp);
        }
    }
    // 未保存 / 復号不能 → 新規生成して保存する。
    let kp = aurora_common::noise::generate_static_keypair().ok()?;
    let id = Identity {
        private: kp.private.clone(),
        public: kp.public.clone(),
    };
    if let Ok(plain) = serde_json::to_vec(&id) {
        if let Ok(enc) = encrypt(&plain) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 一時ファイルへ書いてから rename し、並行書き込みでの破損を避ける (atomic 置換)。
            let tmp = path.with_extension("bin.tmp");
            if std::fs::write(&tmp, &enc).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    *cache = Some(kp.clone());
    Some(kp)
}

fn store_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir 解決に失敗: {e}"))?;
    Ok(dir.join(FILE))
}

/// 暗号化マップを読み込む。ファイルが無ければ空。復号失敗時もエラーにせず空で返す
/// (別ユーザーのファイルが残っていても接続フローを止めないため)。
fn load_map(app: &AppHandle) -> BTreeMap<String, String> {
    let Ok(path) = store_path(app) else {
        return BTreeMap::new();
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return BTreeMap::new(),
    };
    decrypt(&bytes)
        .and_then(|plain| serde_json::from_slice(&plain).ok())
        .unwrap_or_default()
}

fn save_map(app: &AppHandle, map: &BTreeMap<String, String>) -> Result<(), String> {
    let path = store_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir 作成に失敗: {e}"))?;
    }
    let plain = serde_json::to_vec(map).map_err(|e| format!("直列化に失敗: {e}"))?;
    let enc = encrypt(&plain)?;
    std::fs::write(&path, enc).map_err(|e| format!("パスワード保存に失敗: {e}"))
}

/// 指定 host の保存済みパスワードを返す。無ければ `None`。
pub fn get(app: &AppHandle, host: &str) -> Option<String> {
    load_map(app).get(&key(host)).cloned()
}

/// 指定 host のパスワードを暗号化保存する (接続成功時に呼ぶ)。
pub fn put(app: &AppHandle, host: &str, password: &str) -> Result<(), String> {
    let mut map = load_map(app);
    map.insert(key(host), password.to_string());
    save_map(app, &map)
}

/// 一覧マーキング用: パスワードを保存済みの host(小文字) を返す。
#[tauri::command]
pub fn saved_password_hosts(app: AppHandle) -> Result<Vec<String>, String> {
    Ok(load_map(&app).into_keys().collect())
}

/// 指定 host の保存済みパスワードを削除する (「忘れる」ボタン)。
#[tauri::command]
pub fn delete_host_password(app: AppHandle, host: String) -> Result<(), String> {
    let mut map = load_map(&app);
    if map.remove(&key(&host)).is_some() {
        save_map(&app, &map)?;
    }
    Ok(())
}

// --- DPAPI バイト列の暗号化/復号 ---
//
// DPAPI 呼び出しは [`aurora_common::dpapi`] に集約 (host/client 共通、unsafe FFI は 1 箇所)。
// 非 Windows は `protect`/`unprotect` が `Unsupported` を返すので、従来どおり `encrypt` は
// Err、`decrypt` は None になる。

fn encrypt(plain: &[u8]) -> Result<Vec<u8>, String> {
    aurora_common::dpapi::protect(plain).map_err(|e| format!("CryptProtectData failed: {e}"))
}

fn decrypt(enc: &[u8]) -> Option<Vec<u8>> {
    aurora_common::dpapi::unprotect(enc).ok()
}
