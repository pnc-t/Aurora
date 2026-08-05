//! デバイスピン留め: 信頼するクライアント静的公開鍵の許可リストと、実行時の判定状態。
//!
//! 接続パスワード (PSK) は「知識要素」だが、漏れると任意端末から接続できてしまう。これを
//! 補う「所持要素」として、Noise XX が交換するクライアントの**永続静的公開鍵**を許可リストで
//! 照合する (= 2 要素目)。許可リストは公開鍵のみで非機密のため平文保存
//! (`%LOCALAPPDATA%\Aurora\trusted_devices.txt`、1 行 = `<hex公開鍵> <ラベル>`)。
//!
//! 既定は**無効** (opt-in)。有効時は許可リストに無い鍵を拒否し、登録は**ペアリング窓 + オーナー
//! 承認**の 2 段階で行う: オーナーがトレイで窓を開くと、その間に PSK 認証を通った未知端末は
//! **自動登録されず承認待ちに入る**。オーナーはトレイでその端末のフィンガープリント
//! (`aurora_common::noise::device_fingerprint`) を、端末側に表示された値と目視照合した上で承認する。
//! PSK を持つ攻撃者が窓の開いた瞬間にレースで割り込んでも、フィンガープリント不一致でオーナーが
//! 弾ける (窓が開いた瞬間の自動登録だと、レースを制した攻撃者がそのまま恒久登録されてしまう)。
//!
//! 非 Windows は永続化未対応 (Windows 優先方針): 一覧は空・無効・保存は no-op。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 信頼登録済みクライアント (静的公開鍵 + 表示用ラベル)。
pub struct TrustedDevice {
    pub pubkey: Vec<u8>,
    pub label: String,
}

/// ペアリング窓中に現れた未知端末で、まだオーナー承認を得ていないもの。
struct PendingDevice {
    pubkey: Vec<u8>,
    label: String,
}

/// 承認待ちリストの上限。オーナーが放置している間に大量の接続試行で溢れないための保険。
const MAX_PENDING: usize = 8;

/// ピン留め判定の結果 (ログ・分岐用)。
pub enum Admit {
    /// ピン留め無効 (従来どおり PSK のみで許可)。
    Disabled,
    /// 許可リスト登録済みの端末。
    Trusted,
    /// 未許可 (未知の鍵 / 鍵を観測できず)。拒否する。
    Rejected,
    /// ペアリング窓中の未知端末を承認待ちに記録した。接続はこの回は拒否し、オーナーが
    /// フィンガープリント照合の上で承認したら次回接続で Trusted になる。
    PendingApproval,
}

/// 実行時のピン留め状態。net (接続受理) と tray (操作) で `Arc` 共有する。
pub struct DeviceGuard {
    enabled: AtomicBool,
    pairing: AtomicBool,
    trusted: Mutex<Vec<TrustedDevice>>,
    pending: Mutex<Vec<PendingDevice>>,
}

impl DeviceGuard {
    /// 永続状態 (有効フラグ・許可リスト) を読み込んで構築する。ペアリング窓・承認待ちリストは
    /// 常に空の状態で開始 (プロセスをまたいで持ち越さない)。
    pub fn load() -> Self {
        Self {
            enabled: AtomicBool::new(load_enabled()),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(load_trusted()),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// ピン留めの有効/無効を切り替えて永続化する。無効化時はペアリング窓・承認待ちも閉じる。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if let Err(e) = save_enabled(on) {
            tracing::warn!(error = %e, on, "failed to persist device pinning enabled flag");
        }
        if !on {
            self.pairing.store(false, Ordering::Relaxed);
            self.clear_pending();
        }
    }

    /// 次に PSK 認証を通る未知端末を承認待ちに記録できる「ペアリング窓」を開く。
    pub fn arm_pairing(&self) {
        self.pairing.store(true, Ordering::Relaxed);
    }

    /// 開いているペアリング窓を閉じる (承認せずに取り消す)。承認待ちに溜まっていた記録も破棄する
    /// (窓を閉じた = オーナーがこの回の登録意図を取り下げたとみなす)。
    pub fn disarm_pairing(&self) {
        self.pairing.store(false, Ordering::Relaxed);
        self.clear_pending();
    }

    pub fn pairing_armed(&self) -> bool {
        self.pairing.load(Ordering::Relaxed)
    }

    /// 登録済み端末のラベル一覧 (メニュー表示順 = 内部の保持順)。
    pub fn labels(&self) -> Vec<String> {
        self.trusted
            .lock()
            .map(|t| t.iter().map(|d| d.label.clone()).collect())
            .unwrap_or_default()
    }

    /// `index` 番目の登録端末を削除して永続化する。削除したラベルを返す (範囲外は `None`)。
    pub fn remove(&self, index: usize) -> Option<String> {
        let mut trusted = self.trusted.lock().ok()?;
        if index >= trusted.len() {
            return None;
        }
        let removed = trusted.remove(index);
        if let Err(e) = save_trusted(&trusted) {
            tracing::warn!(error = %e, "failed to persist device removal");
        }
        Some(removed.label)
    }

    /// 許可リストを全消去して永続化する (端末の総入れ替え用)。
    pub fn clear(&self) {
        if let Ok(mut t) = self.trusted.lock() {
            t.clear();
            if let Err(e) = save_trusted(&t) {
                tracing::warn!(error = %e, "failed to persist device allowlist clear");
            }
        }
    }

    /// 承認待ちの端末一覧を (公開鍵, フィンガープリント, ラベル) で返す (トレイ表示順 = 保持順)。
    /// 公開鍵も返すのは、承認時に index ではなく**鍵で対象を確定**するため
    /// ([`Self::approve_pending`] の TOCTOU 対策)。
    pub fn pending_devices(&self) -> Vec<(Vec<u8>, String, String)> {
        self.pending
            .lock()
            .map(|p| {
                p.iter()
                    .map(|d| {
                        (
                            d.pubkey.clone(),
                            aurora_common::noise::device_fingerprint(&d.pubkey),
                            d.label.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 指定した**公開鍵**の承認待ち端末を許可リストへ昇格して永続化する。承認は 1 回の判断につき
    /// 1 台に限定する意図で、承認待ちリスト全体を消去しペアリング窓も閉じる (他の保留分は
    /// オーナーが個別に再度ペアリング窓を開いて照合させる)。移した端末のラベルを返す
    /// (該当鍵が承認待ちに無ければ `None`)。
    ///
    /// **index ではなく鍵で対象を確定する**のは、トレイの承認ダイアログ (モーダルで tray スレッドを
    /// 数秒ブロックする) 表示中に、別接続 (tokio 側の `admit`) が承認待ちリストを変化させ得るため。
    /// index 指定だと、表示中に上限超過の evict でリストがずれ、オーナーが指紋照合したのとは別の
    /// (攻撃者の) 端末を承認してしまう TOCTOU が生じる。鍵で照合すれば、evict 済みなら `None` に
    /// fail-safe し、見た端末とは別物を登録しない。
    pub fn approve_pending(&self, pubkey: &[u8]) -> Option<String> {
        let mut pending = self.pending.lock().ok()?;
        let pos = pending.iter().position(|d| d.pubkey == pubkey)?;
        let approved = pending.remove(pos);
        pending.clear();
        drop(pending);
        self.pairing.store(false, Ordering::Relaxed);

        let label = approved.label.clone();
        let Ok(mut trusted) = self.trusted.lock() else {
            return None;
        };
        trusted.push(TrustedDevice {
            label: approved.label,
            pubkey: approved.pubkey,
        });
        if let Err(e) = save_trusted(&trusted) {
            // 承認は成立したがディスクに残らなかった = 再起動で信頼が消える。要警告。
            tracing::warn!(error = %e, "failed to persist approved device");
        }
        Some(label)
    }

    /// 承認待ちリストを空にする (窓を閉じる/無効化する経路から使う)。
    pub fn clear_pending(&self) {
        if let Ok(mut p) = self.pending.lock() {
            p.clear();
        }
    }

    /// 接続レベルのピン留め有効/無効に**関係なく**、鍵が許可リストに登録済みかを返す。
    /// 仮想ディスプレイ (RDP 風テイクオーバー) のような特権機能を「ホストに登録済みの端末だけ」に
    /// 限定するゲートに使う。未登録端末は通常接続 (物理キャプチャ) はできても仮想は使えない。
    pub fn is_trusted(&self, key: Option<&[u8]>) -> bool {
        let Some(key) = key else {
            return false;
        };
        self.trusted
            .lock()
            .map(|t| t.iter().any(|d| d.pubkey == key))
            .unwrap_or(false)
    }

    /// クライアント公開鍵 (ハンドシェイクの `remote_static`) を判定する。ペアリング窓が開いて
    /// いれば未知鍵を**自動登録せず**承認待ちリストに記録する (窓は消費しない。オーナーが
    /// [`Self::approve_pending_at`] で明示承認するまで登録されない)。`Admit` で結果を返す。
    ///
    /// `name` はクライアントが申告した端末名 (PC 名)。承認待ち表示・登録時の表示ラベルに使い、
    /// トレイの一覧で「どの PC か」を判別できるようにする。クライアント任意の値なのでサニタイズ・
    /// 長さ制限する (机上のラベル用途のみ。認証は公開鍵で行い名前は信頼しない)。空/無効なら
    /// 従来の `device-xxxx` (公開鍵 hex) にフォールバックする。
    pub fn admit(&self, key: Option<&[u8]>, name: Option<&str>) -> Admit {
        if !self.enabled.load(Ordering::Relaxed) {
            return Admit::Disabled;
        }
        let Some(key) = key else {
            return Admit::Rejected;
        };
        let Ok(trusted) = self.trusted.lock() else {
            return Admit::Rejected;
        };
        if trusted.iter().any(|d| d.pubkey == key) {
            return Admit::Trusted;
        }
        drop(trusted);
        // ペアリング窓が開いていれば、この未知端末を承認待ちに記録する。窓は消費しない
        // (オーナーが承認するまで開いたまま。攻撃者がレースで割り込んでも自動登録されない)。
        if self.pairing.load(Ordering::Relaxed) {
            let Ok(mut pending) = self.pending.lock() else {
                return Admit::Rejected;
            };
            let label = name
                .and_then(sanitize_label)
                .unwrap_or_else(|| default_label(key));
            if let Some(existing) = pending.iter_mut().find(|d| d.pubkey == key) {
                existing.label = label;
            } else {
                // 上限到達時に先頭を FIFO で捨てると、正規デバイスが承認待ちに
                // 入っている間に別の未知鍵で連続接続するだけでそれを押し出せてしまう
                // (承認妨害)。溢れた分は古いものを捨てずに新規追加を拒否する。
                if pending.len() >= MAX_PENDING {
                    tracing::warn!(
                        pending_count = pending.len(),
                        "pending device list full, rejecting new pairing request"
                    );
                    return Admit::Rejected;
                }
                pending.push(PendingDevice {
                    pubkey: key.to_vec(),
                    label,
                });
            }
            return Admit::PendingApproval;
        }
        Admit::Rejected
    }
}

/// クライアント申告の端末名を表示ラベルへ整える。クライアント任意の値なので、
/// (1) 制御文字を除去 (改行は `trusted_devices.txt` の `<hex> <label>` 行を壊すため特に重要、
/// トレイメニュー表示の崩れも防ぐ)、(2) 前後空白を除去、(3) 64 文字に制限する。
/// 結果が空なら `None` を返し、呼び出し側が既定ラベルへフォールバックする。
fn sanitize_label(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(64)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// 既定ラベル: 公開鍵の先頭 4 バイトの hex (`device-xxxxxxxx`)。一覧での識別用。
fn default_label(key: &[u8]) -> String {
    let mut s = String::from("device-");
    for b in key.iter().take(4) {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0xf));
    }
    s
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0xf));
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in b.chunks_exact(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Some(out)
}

// --- 永続化 (Windows) ---

#[cfg(target_os = "windows")]
fn store_path(file: &str) -> Option<std::path::PathBuf> {
    Some(aurora_common::paths::data_dir()?.join(file))
}

#[cfg(target_os = "windows")]
fn load_enabled() -> bool {
    store_path("pinning")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn save_enabled(on: bool) -> std::io::Result<()> {
    let Some(path) = store_path("pinning") else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, if on { "1" } else { "0" })
}

#[cfg(target_os = "windows")]
fn load_trusted() -> Vec<TrustedDevice> {
    let Some(path) = store_path("trusted_devices.txt") else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (hex, label) = match line.split_once(' ') {
                Some((h, l)) => (h, l.to_string()),
                None => (line, String::new()),
            };
            let pubkey = from_hex(hex)?;
            let label = if label.is_empty() {
                default_label(&pubkey)
            } else {
                label
            };
            Some(TrustedDevice { pubkey, label })
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn save_trusted(list: &[TrustedDevice]) -> std::io::Result<()> {
    let Some(path) = store_path("trusted_devices.txt") else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut body = String::new();
    for d in list {
        body.push_str(&to_hex(&d.pubkey));
        body.push(' ');
        body.push_str(&d.label);
        body.push('\n');
    }
    std::fs::write(path, body)
}

#[cfg(not(target_os = "windows"))]
fn load_enabled() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn save_enabled(_on: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn load_trusted() -> Vec<TrustedDevice> {
    Vec::new()
}

#[cfg(not(target_os = "windows"))]
fn save_trusted(_list: &[TrustedDevice]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff, 0x10];
        assert_eq!(to_hex(&bytes), "000fa5ff10");
        assert_eq!(from_hex("000fa5ff10").unwrap(), bytes);
        assert_eq!(from_hex("000FA5FF10").unwrap(), bytes); // 大文字も受ける
        assert!(from_hex("xyz").is_none());
        assert!(from_hex("abc").is_none()); // 奇数長
    }

    #[test]
    fn sanitize_label_cleans_and_caps() {
        // 通常の PC 名はそのまま (空白・ハイフンは保持)。
        assert_eq!(
            sanitize_label("DESKTOP-ABC 1").as_deref(),
            Some("DESKTOP-ABC 1")
        );
        // 制御文字 (改行など) を除去し前後空白を trim する。
        assert_eq!(sanitize_label("  pc\nname\t ").as_deref(), Some("pcname"));
        // 空 / 制御文字のみ → None (既定ラベルへフォールバックさせる)。
        assert_eq!(sanitize_label(""), None);
        assert_eq!(sanitize_label("\r\n\t"), None);
        // 64 文字に制限する。
        assert_eq!(
            sanitize_label(&"x".repeat(100)).unwrap().chars().count(),
            64
        );
    }

    #[test]
    fn pending_with_empty_name_falls_back_to_default_label() {
        let g = DeviceGuard {
            enabled: AtomicBool::new(true),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        };
        g.arm_pairing();
        // 旧クライアント (端末名なし) は既定ラベル device-xxxx に落ちる。承認待ちに記録されるのみで
        // まだ登録されない。
        assert!(matches!(
            g.admit(Some(&[0xab, 0xcd, 0xef, 0x01]), Some("")),
            Admit::PendingApproval
        ));
        assert!(g.labels().is_empty());
        let pending = g.pending_devices();
        assert_eq!(pending.len(), 1);
        // (公開鍵, フィンガープリント, ラベル)。
        assert_eq!(pending[0].0, vec![0xab, 0xcd, 0xef, 0x01]);
        assert_eq!(
            pending[0].1,
            aurora_common::noise::device_fingerprint(&[0xab, 0xcd, 0xef, 0x01])
        );
        assert_eq!(pending[0].2, "device-abcdef01");
    }

    #[test]
    fn admit_disabled_passes_through() {
        let g = DeviceGuard {
            enabled: AtomicBool::new(false),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        };
        assert!(matches!(g.admit(Some(&[1, 2, 3]), None), Admit::Disabled));
        assert!(matches!(g.admit(None, None), Admit::Disabled));
    }

    #[test]
    fn admit_enforces_pending_and_approval() {
        let g = DeviceGuard {
            enabled: AtomicBool::new(true),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        };
        // 有効・未登録・ペアリング窓なし → 拒否。鍵なしも拒否。
        assert!(matches!(
            g.admit(Some(&[9, 9, 9, 9]), None),
            Admit::Rejected
        ));
        assert!(matches!(g.admit(None, None), Admit::Rejected));

        // ペアリング窓を開くと、未知端末は自動登録されず承認待ちになる。窓は消費されない。
        // 申告された端末名がラベルになる (どの PC かを判別できる)。
        g.arm_pairing();
        assert!(matches!(
            g.admit(Some(&[9, 9, 9, 9]), Some("My-Laptop")),
            Admit::PendingApproval
        ));
        assert!(g.pairing_armed());
        assert!(g.labels().is_empty());
        let pending = g.pending_devices();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].2, "My-Laptop"); // (公開鍵, フィンガープリント, ラベル)

        // オーナーが承認するまでは Trusted にならない (再接続してもまだ PendingApproval)。
        assert!(matches!(
            g.admit(Some(&[9, 9, 9, 9]), Some("My-Laptop")),
            Admit::PendingApproval
        ));

        // オーナーが承認 (鍵で確定) すると許可リストへ昇格し、承認待ちは空になり、窓も閉じる。
        let approved = g.approve_pending(&[9, 9, 9, 9]);
        assert_eq!(approved, Some("My-Laptop".to_string()));
        assert!(!g.pairing_armed());
        assert!(g.pending_devices().is_empty());
        assert_eq!(g.labels(), vec!["My-Laptop".to_string()]);
        assert!(matches!(g.admit(Some(&[9, 9, 9, 9]), None), Admit::Trusted));

        // 窓が閉じている未知端末は拒否のまま。
        assert!(matches!(g.admit(Some(&[7, 7]), None), Admit::Rejected));
        assert_eq!(g.labels().len(), 1);

        // disarm でペアリング窓を閉じられる (承認待ちも破棄される)。
        g.arm_pairing();
        assert!(matches!(
            g.admit(Some(&[5, 5]), None),
            Admit::PendingApproval
        ));
        assert!(g.pairing_armed());
        g.disarm_pairing();
        assert!(!g.pairing_armed());
        assert!(g.pending_devices().is_empty());

        // approve_pending は承認待ちに無い鍵なら None (evict 済み等に fail-safe)。
        assert!(g.approve_pending(&[0xde, 0xad]).is_none());

        // remove で個別削除できる。
        assert!(g.remove(0).is_some());
        assert_eq!(g.labels().len(), 0);
        assert!(g.remove(0).is_none()); // 範囲外
    }
}
