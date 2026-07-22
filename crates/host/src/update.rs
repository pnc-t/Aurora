//! GitHub Releases 経由の host 自己更新。起動時 + 6h ごとにチェックし、新版があれば tray に
//! 通知する。ユーザーがトレイから更新要求したら、**接続中は保留し切断後 (アイドル) に** exe を
//! 差し替えて再起動する (接続中の入れ替えは映像/操作の継続を壊すため)。

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tray::SharedTrayBus;

const REPO_LATEST_API: &str = "https://api.github.com/repos/pnc-t/Aurora/releases/latest";
const HOST_ASSET_NAME: &str = "aurora-host.exe";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const APPLY_POLL: Duration = Duration::from_secs(3);
/// ダウンロードした exe がこのサイズ未満なら破損/エラーページとみなし適用を中止する下限。
const MIN_VALID_EXE_BYTES: u64 = 200 * 1024;

/// 検知した新バージョンと host exe のダウンロード URL。
#[derive(Clone)]
pub struct AvailableUpdate {
    pub version: String,
    pub download_url: String,
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
pub fn run(state: SharedUpdate, bus: SharedTrayBus, port: Arc<std::sync::atomic::AtomicU16>) {
    // 前回の自己更新で残った退避 exe / 中断ダウンロードを掃除する (アンインストーラの
    // 非再帰 RMDir が INSTDIR を消せるように残骸を残さない)。旧プロセスは新プロセス起動時点で
    // 既に exit 済みのため .old.exe も削除できる。失敗は無視 (掃除は必須ではない)。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let _ = std::fs::remove_file(dir.join("aurora-host.old.exe"));
        let _ = std::fs::remove_file(dir.join("aurora-host.update.tmp"));
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
                    if let Err(e) = apply(&av, port.load(Ordering::Relaxed)) {
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

/// GitHub Releases の最新版を確認する。現行版より新しければ `Some` を返す。
fn check_latest(current: &str) -> Result<Option<AvailableUpdate>> {
    let resp = ureq::get(REPO_LATEST_API)
        .set("User-Agent", "aurora-host-updater")
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .call()
        .context("GitHub releases API request failed")?;
    let json: serde_json::Value = resp.into_json().context("invalid JSON from releases API")?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("release response missing tag_name")?;
    let latest_str = tag.strip_prefix('v').unwrap_or(tag);
    let latest = semver::Version::parse(latest_str)
        .with_context(|| format!("invalid semver in tag_name '{tag}'"))?;
    let current_ver =
        semver::Version::parse(current).with_context(|| format!("invalid semver '{current}'"))?;
    if latest <= current_ver {
        return Ok(None);
    }

    let download_url = json
        .get("assets")
        .and_then(|v| v.as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(HOST_ASSET_NAME))
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|v| v.as_str())
        .with_context(|| format!("release {tag} has no asset named {HOST_ASSET_NAME}"))?;

    Ok(Some(AvailableUpdate {
        version: latest_str.to_string(),
        download_url: download_url.to_string(),
    }))
}

/// 新版 exe をダウンロードして自身と差し替え、再起動する。成功すると戻らない (`exit(0)`)。
fn apply(av: &AvailableUpdate, current_port: u16) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let dir = exe
        .parent()
        .context("current_exe has no parent directory")?;

    let tmp = dir.join("aurora-host.update.tmp");
    download_to(&av.download_url, &tmp).context("download failed")?;

    // 破損/エラーページ (HTML) 対策。実 exe は数 MB あるため、小さすぎれば適用を中止する。
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or_default();
    if size < MIN_VALID_EXE_BYTES {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("downloaded file too small ({size} bytes); aborting update");
    }

    let backup = dir.join("aurora-host.old.exe");
    let _ = std::fs::remove_file(&backup);
    // 実行中の exe は Windows でもリネームで退避できる (ハンドルはリネーム後のパスに追従する)。
    std::fs::rename(&exe, &backup).context("failed to move running exe to backup")?;

    if let Err(e) = std::fs::rename(&tmp, &exe) {
        // ロールバック: 新版を置けなかったので旧 exe を戻す (どちらも消えた状態にしない)。
        let _ = std::fs::rename(&backup, &exe);
        return Err(e).context("failed to install downloaded exe; rolled back");
    }

    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            let _ = it.next();
            continue;
        }
        args.push(a);
    }
    args.push("--port".into());
    args.push(current_port.to_string());

    let mut child = match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::rename(&backup, &exe);
            return Err(e).context("failed to spawn updated exe; rolled back");
        }
    };

    // 新プロセスが起動を確認できてから旧を捨てる (起動失敗で誰も残らない事態を防ぐ)。
    std::thread::sleep(Duration::from_millis(750));
    match child.try_wait() {
        Ok(Some(status)) => {
            let _ = std::fs::rename(&backup, &exe);
            anyhow::bail!("updated exe exited immediately (status: {status}); rolled back");
        }
        _ => {
            // プロセス終了 (exit) は Drop を走らせないため、投影ドライブを明示的に掃除する。
            crate::projfs::cleanup_drives();
            std::process::exit(0);
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
