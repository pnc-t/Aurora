//! 更新マニフェスト (`latest.json`) 自体の署名検証。
//!
//! tauri-plugin-updater はダウンロードする setup.exe の署名だけを検証し、manifest の version と
//! (url, signature) の組は検証しない。そのため Release への書き込み権限だけを持つ攻撃者が、version を
//! 高く騙りつつ旧正規署名済み setup.exe を指す manifest に差し替えると、既知脆弱版へ戻せてしまう。
//! host の自己更新 (`latest-host.json.sig` を fail-closed で検証) と揃え、CI が同じ鍵で付ける
//! `latest.json.sig` を検証した manifest の内容と、プラグインが取得した更新内容の一致をフロントが
//! 確認してから適用する。

use std::io::Read;
use std::time::Duration;

use tauri::AppHandle;

const MANIFEST_URL: &str = "https://github.com/pnc-t/Aurora/releases/latest/download/latest.json";
const MANIFEST_SIG_URL: &str =
    "https://github.com/pnc-t/Aurora/releases/latest/download/latest.json.sig";
const PLATFORM_KEY: &str = "windows-x86_64";
/// manifest / .sig は数百バイト。エラーページ等の巨大レスポンスを丸ごと読まないための上限。
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// 署名検証済み manifest のうち、この端末向けの更新内容。
#[derive(serde::Serialize)]
pub struct VerifiedUpdate {
    version: String,
    url: String,
    signature: String,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    platforms: std::collections::HashMap<String, Platform>,
}

#[derive(serde::Deserialize)]
struct Platform {
    signature: String,
    url: String,
}

/// `latest.json` を `latest.json.sig` で検証し、この端末向けの (version, url, signature) を返す。
/// 取得・検証に失敗したら Err (fail-closed: 呼び出し側は更新なしとして扱う)。
#[tauri::command]
pub async fn verify_update_manifest(app: AppHandle) -> Result<VerifiedUpdate, String> {
    let pubkey = updater_pubkey(&app)?;
    // ureq は同期 I/O のため tokio ワーカーを塞がないよう spawn_blocking へ逃がす。
    tokio::task::spawn_blocking(move || {
        let manifest = fetch_capped(MANIFEST_URL)?;
        let sig = fetch_capped(MANIFEST_SIG_URL)?;
        let sig = String::from_utf8(sig).map_err(|e| format!("manifest signature: {e}"))?;
        aurora_common::update_sig::verify(&manifest, &pubkey, &sig)
            .map_err(|e| format!("update manifest: {e}"))?;
        let m: Manifest =
            serde_json::from_slice(&manifest).map_err(|e| format!("update manifest: {e}"))?;
        let p = m
            .platforms
            .get(PLATFORM_KEY)
            .ok_or_else(|| format!("update manifest has no {PLATFORM_KEY} entry"))?;
        Ok(VerifiedUpdate {
            version: m.version,
            url: p.url.clone(),
            signature: p.signature.clone(),
        })
    })
    .await
    .map_err(|e| format!("update manifest task failed: {e}"))?
}

/// updater プラグインの設定にある公開鍵 (setup.exe の検証と同じ鍵) を取り出す。二重定義すると
/// 鍵の更新時に片方だけ取り残されるため、tauri.conf.json を唯一の出典にする。
fn updater_pubkey(app: &AppHandle) -> Result<String, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .ok_or_else(|| "updater pubkey is not configured".to_string())
}

fn fetch_capped(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .set("User-Agent", "aurora-client-updater")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    if buf.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!("response exceeds {MAX_MANIFEST_BYTES} byte limit"));
    }
    Ok(buf)
}
