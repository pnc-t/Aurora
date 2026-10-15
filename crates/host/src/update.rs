//! GitHub Releases 経由の host 自己更新。起動時 + 6h ごとにチェックし、新版があれば tray に
//! 通知する。ユーザーがトレイから更新要求したら、**接続中は保留し切断後 (アイドル) に**
//! setup.exe をサイレント実行して更新・再起動する (接続中の入れ替えは映像/操作の継続を壊すため)。
//!
//! client (tauri-plugin-updater) と同じ minisign 署名スキームでマニフェスト/setup.exe を検証する。
//! 生 exe を直接 rename する旧方式は廃止し、NSIS の `/S` サイレントインストールに一本化した
//! (差し替え + 再起動のトランザクション責任は NSIS 側に委ねる)。

use anyhow::{Context, Result};
use base64::Engine;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tray::SharedTrayBus;

const MANIFEST_URL: &str =
    "https://github.com/pnc-t/Aurora/releases/latest/download/latest-host.json";
/// manifest 本体の署名 (CI が setup.exe と同じ鍵で生成)。version と (url, signature) を
/// 暗号的に束ねるためのもので、これが無いと書き込み権限だけの攻撃者が version を騙って
/// 旧正規署名済み setup.exe へのダウングレードを誘導できる (J-2 対応)。
const MANIFEST_SIG_URL: &str =
    "https://github.com/pnc-t/Aurora/releases/latest/download/latest-host.json.sig";
const PLATFORM_KEY: &str = "windows-x86_64";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const APPLY_POLL: Duration = Duration::from_secs(3);
/// ダウンロードした setup.exe がこのサイズ未満なら破損/エラーページとみなし適用を中止する下限。
const MIN_VALID_EXE_BYTES: u64 = 200 * 1024;
/// latest-host.json / .sig は数百バイト程度。誤ってエラーページ等の巨大レスポンスを丸ごと
/// メモリに読み込まないための上限 (OOM 対策)。
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

// client (crates/client/src-tauri/tauri.conf.json の plugins.updater.pubkey) と同一の埋め込み
// 公開鍵。host/client を同じリリース鍵で署名するため値を揃える。
const HOST_UPDATE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDVCNDg5RUE5ODlBMUJEMjEKUldRaHZhR0pxWjVJVzJmd3I4TXVhamdIaDJKRWlISVJwRmFtbk82UjIzVHBlRWhqVjI5dFNqdy8K";

/// latest-host.json のスキーマ (client の latest.json と同形)。
#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    platforms: HashMap<String, ManifestPlatform>,
}

#[derive(serde::Deserialize)]
struct ManifestPlatform {
    signature: String,
    url: String,
}

/// 検知した新バージョンと setup.exe のダウンロード URL・署名。
#[derive(Clone)]
pub struct AvailableUpdate {
    pub version: String,
    pub download_url: String,
    /// minisign 署名ファイル全体を base64 化したもの (マニフェストの `signature` フィールドそのまま)。
    signature: String,
}

/// 更新の実行時状態。update スレッド (検知/適用) と tray (表示/要求) で共有する。
pub struct UpdateState {
    available: Mutex<Option<AvailableUpdate>>,
    apply_requested: AtomicBool,
}

/// tray/update スレッド間で共有するための別名。
pub type SharedUpdate = Arc<UpdateState>;

impl UpdateState {
    /// 空の状態を生成して共有ハンドルで返す。常に `Arc` 越しに使うため `Self` でなく共有別名を返す。
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> SharedUpdate {
        Arc::new(Self {
            available: Mutex::new(None),
            apply_requested: AtomicBool::new(false),
        })
    }

    /// 検知済みの新版 (あれば)。tray がメニュー表示に使う。
    pub fn available(&self) -> Option<AvailableUpdate> {
        self.available.lock().ok().and_then(|g| g.clone())
    }

    fn set_available(&self, u: Option<AvailableUpdate>) {
        if let Ok(mut g) = self.available.lock() {
            *g = u;
        }
    }

    /// ユーザーが更新適用を要求した (tray メニュークリック)。接続中なら保留される。
    pub fn request_apply(&self) {
        self.apply_requested.store(true, Ordering::Relaxed);
    }
}

/// 更新チェック/適用ループ本体。`std::thread` から呼ぶ想定 (ブロッキング HTTP/sleep を使うため
/// tokio タスクにしない。他の host サブシステムと同じ流儀)。
pub fn run(state: SharedUpdate, bus: SharedTrayBus, _port: Arc<std::sync::atomic::AtomicU16>) {
    // NSIS setup.exe がサイレント実行時にポート等の起動引数を持ち回らず、host は再起動後に
    // system::load_port() の保存値から実効ポートを復元する (main.rs 参照)。そのため待受ポートは
    // ここでは使わない (呼び出し元シグネチャ維持のため引数自体は残す)。

    // 前回の自己更新で残った中断ダウンロードを掃除する (アンインストーラの非再帰 RMDir が
    // INSTDIR を消せるように残骸を残さない)。失敗は無視 (掃除は必須ではない)。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let _ = std::fs::remove_file(dir.join("aurora-host.update-setup.exe"));
    }

    let current = env!("CARGO_PKG_VERSION");
    let mut last_check: Option<Instant> = None;
    // バルーン通知の重複防止 (同一バージョンを毎チェックのたび通知しない)。
    let mut announced: Option<String> = None;

    loop {
        let due = match last_check {
            None => true,
            Some(t) => t.elapsed() >= CHECK_INTERVAL,
        };
        if due {
            last_check = Some(Instant::now());
            match check_latest(current) {
                Ok(Some(av)) => {
                    if announced.as_deref() != Some(av.version.as_str()) {
                        bus.on_update_available(av.version.clone());
                        announced = Some(av.version.clone());
                    }
                    state.set_available(Some(av));
                }
                Ok(None) => state.set_available(None),
                Err(e) => tracing::warn!(error = %e, "update check failed"),
            }
        }

        if state.apply_requested.load(Ordering::Relaxed) {
            match state.available() {
                Some(av) if !crate::net::is_active() => {
                    // 成功時は戻らない (exit(0))。
                    if let Err(e) = apply(&av) {
                        tracing::error!(error = %e, "update apply failed");
                        // 無限リトライを防ぐ。ユーザーは再度メニューから要求できる。
                        state.apply_requested.store(false, Ordering::Relaxed);
                    }
                }
                Some(_) => {
                    // 接続中: 保留したまま次回ポーリングで再判定する (切断を待つ)。
                }
                None => {
                    // 新版情報が消えた (取り消された/古くなった) のに要求だけ残るのを防ぐ。
                    state.apply_requested.store(false, Ordering::Relaxed);
                }
            }
        }

        std::thread::sleep(APPLY_POLL);
    }
}

/// `latest-host.json` マニフェストを確認する。現行版より新しければ `Some` を返す。
///
/// fail-closed: manifest 本体の署名 (`latest-host.json.sig`) を取得・検証できない限り、
/// 更新ありなし判定を一切行わず `Err` を返す (`Ok(None)` にすると「取得できなかった」と
/// 「更新なし」を呼び出し側で混同してしまうため)。
fn check_latest(current: &str) -> Result<Option<AvailableUpdate>> {
    let manifest_bytes = fetch_capped(MANIFEST_URL).context("update manifest request failed")?;
    let sig_bytes =
        fetch_capped(MANIFEST_SIG_URL).context("update manifest signature request failed")?;
    let sig_text =
        String::from_utf8(sig_bytes).context("update manifest signature is not valid UTF-8")?;
    verify_signature(&manifest_bytes, &sig_text)
        .context("update manifest signature verification failed")?;

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid JSON in update manifest")?;

    let latest = semver::Version::parse(&manifest.version)
        .with_context(|| format!("invalid semver in manifest version '{}'", manifest.version))?;
    let current_ver =
        semver::Version::parse(current).with_context(|| format!("invalid semver '{current}'"))?;
    if latest <= current_ver {
        return Ok(None);
    }

    let platform = manifest
        .platforms
        .get(PLATFORM_KEY)
        .with_context(|| format!("update manifest has no platform entry '{PLATFORM_KEY}'"))?;

    Ok(Some(AvailableUpdate {
        version: manifest.version,
        download_url: platform.url.clone(),
        signature: platform.signature.clone(),
    }))
}

/// URL の内容をバイト列として取得する。manifest/署名は数百バイトのはずなので、エラーページ等の
/// 想定外に大きいレスポンスで OOM しないよう `MAX_MANIFEST_BYTES` で読み出し上限を設ける。
fn fetch_capped(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .set("User-Agent", "aurora-host-updater")
        .timeout(Duration::from_secs(15))
        .call()
        .context("request failed")?;
    let mut buf = Vec::new();
    let mut reader = resp.into_reader().take(MAX_MANIFEST_BYTES + 1);
    reader
        .read_to_end(&mut buf)
        .context("failed to read response body")?;
    if buf.len() as u64 > MAX_MANIFEST_BYTES {
        anyhow::bail!("response exceeds {MAX_MANIFEST_BYTES} byte limit");
    }
    Ok(buf)
}

/// マニフェストの `signature` (minisign 署名ファイル全体を base64 化したもの) と埋め込み公開鍵で
/// ダウンロード済みデータを検証する。tauri updater と同じスキームに揃え、改竄/破損した setup.exe
/// を適用しないため。
fn verify_signature(data: &[u8], signature_b64: &str) -> Result<()> {
    let pubkey_bytes = base64::engine::general_purpose::STANDARD
        .decode(HOST_UPDATE_PUBKEY)
        .context("failed to decode embedded update public key")?;
    let pubkey_text =
        String::from_utf8(pubkey_bytes).context("embedded update public key is not valid UTF-8")?;
    let public_key = minisign_verify::PublicKey::decode(&pubkey_text)
        .context("invalid embedded minisign public key")?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .context("failed to decode update signature")?;
    let sig_text = String::from_utf8(sig_bytes).context("update signature is not valid UTF-8")?;
    let signature =
        minisign_verify::Signature::decode(&sig_text).context("invalid update signature format")?;

    // tauri updater は prehashed (非 legacy) 署名を生成するため allow_legacy=false で厳密照合する。
    public_key
        .verify(data, &signature, false)
        .context("update signature verification failed")?;
    Ok(())
}

/// setup.exe をダウンロード・検証してサイレントインストールを起動する。差し替え + 再起動は
/// NSIS 側のトランザクション責任 (別タスク)。成功すると戻らない (`exit(0)`)。
fn apply(av: &AvailableUpdate) -> Result<()> {
    // インストール先は管理者専用の %ProgramFiles% で、setup も requireAdministrator。非昇格
    // (スタートメニューからの手動起動等) では setup を置けず、起動しても UAC が要りサイレント
    // 更新の契約を守れないため、ダウンロード前に理由が分かる形で中止する。
    // setup を一時ディレクトリへ逃がす回避はしない: ユーザー書込可の場所に置くと検証→実行の
    // 間に差し替えられ、昇格実行される (インストール先を per-machine にした理由と同じ)。
    if !crate::display::process_is_elevated() {
        anyhow::bail!(
            "host is not elevated; update requires the logon task (or run as administrator)"
        );
    }
    let exe = std::env::current_exe().context("current_exe failed")?;
    let dir = exe
        .parent()
        .context("current_exe has no parent directory")?;

    let setup = dir.join("aurora-host.update-setup.exe");
    download_to(&av.download_url, &setup).context("setup download failed")?;

    // 破損/エラーページ (HTML) 対策。実 setup は数 MB あるため、小さすぎれば適用を中止する。
    let size = std::fs::metadata(&setup)
        .map(|m| m.len())
        .unwrap_or_default();
    if size < MIN_VALID_EXE_BYTES {
        let _ = std::fs::remove_file(&setup);
        anyhow::bail!("downloaded setup too small ({size} bytes); aborting update");
    }

    let data = match std::fs::read(&setup) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(&setup);
            return Err(e).context("failed to read downloaded setup for verification");
        }
    };
    if let Err(e) = verify_signature(&data, &av.signature) {
        let _ = std::fs::remove_file(&setup);
        return Err(e).context("update signature verification failed; aborting update");
    }
    drop(data);

    // 実行中の自 exe は Windows 上でロックされ setup から上書きできないため、spawn できたら
    // 待たずに (detached) 自プロセスを速やかに終了する。setup 起動直後の失敗は検知できないが、
    // トランザクションの成否は NSIS 側の責任とする (別タスク)。
    match std::process::Command::new(&setup).arg("/S").spawn() {
        Ok(_child) => {
            // プロセス終了 (exit) は Drop を走らせないため、投影ドライブを明示的に掃除する。
            crate::projfs::cleanup_drives();
            std::process::exit(0);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&setup);
            Err(e).context("failed to spawn update setup")
        }
    }
}

/// URL からファイルへダウンロードする。
fn download_to(url: &str, dest: &Path) -> Result<()> {
    let resp = ureq::get(url)
        .set("User-Agent", "aurora-host-updater")
        .timeout(Duration::from_secs(180))
        .call()
        .context("asset download request failed")?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).context("failed to create temp file")?;
    std::io::copy(&mut reader, &mut file).context("failed to write downloaded data")?;
    file.sync_all().context("failed to flush downloaded file")?;
    Ok(())
}
