//! デバイスピン留め: 信頼するクライアント静的公開鍵の許可リストと、実行時の判定状態。
//!
//! 接続パスワード (PSK) は「知識要素」だが、漏れると任意端末から接続できてしまう。これを
//! 補う「所持要素」として、Noise XX が交換するクライアントの**永続静的公開鍵**を許可リストで
//! 照合する (= 2 要素目)。許可リストは公開鍵のみで非機密のため平文保存
//! (`%LOCALAPPDATA%\Aurora\trusted_devices.txt`、1 行 = `<hex公開鍵> <ラベル>`)。
//!
//! 既定は**無効** (opt-in)。有効時は許可リストに無い鍵を拒否し、登録は**ペアリング窓**で行う:
//! オーナーがトレイで「次の接続を登録」を押した 1 回だけ、次に PSK 認証を通した端末を信頼登録
//! する。パスワード漏洩時に攻撃者が勝手に登録される窓を、owner が能動的に開いた瞬間に限定する。
//!
//! 非 Windows は永続化未対応 (Windows 優先方針): 一覧は空・無効・保存は no-op。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 信頼登録済みクライアント (静的公開鍵 + 表示用ラベル)。
pub struct TrustedDevice {
    pub pubkey: Vec<u8>,
    pub label: String,
}

/// ピン留め判定の結果 (ログ・分岐用)。
pub enum Admit {
    /// ピン留め無効 (従来どおり PSK のみで許可)。
    Disabled,
    /// 許可リスト登録済みの端末。
    Trusted,
    /// ペアリング窓中の未知端末を新規登録して許可した。
    Enrolled,
    /// 未許可 (未知の鍵 / 鍵を観測できず)。拒否する。
    Rejected,
}

/// 実行時のピン留め状態。net (接続受理) と tray (操作) で `Arc` 共有する。
pub struct DeviceGuard {
    enabled: AtomicBool,
    pairing: AtomicBool,
    trusted: Mutex<Vec<TrustedDevice>>,
}

impl DeviceGuard {
    /// 永続状態 (有効フラグ・許可リスト) を読み込んで構築する。ペアリング窓は常に閉じた状態で開始。
    pub fn load() -> Self {
        Self {
            enabled: AtomicBool::new(load_enabled()),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(load_trusted()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// ピン留めの有効/無効を切り替えて永続化する。無効化時はペアリング窓も閉じる。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        let _ = save_enabled(on);
        if !on {
            self.pairing.store(false, Ordering::Relaxed);
        }
    }

    /// 次の 1 接続だけ未知端末の登録を許す「ペアリング窓」を開く。
    pub fn arm_pairing(&self) {
        self.pairing.store(true, Ordering::Relaxed);
    }

    pub fn pairing_armed(&self) -> bool {
        self.pairing.load(Ordering::Relaxed)
    }

    pub fn trusted_count(&self) -> usize {
        self.trusted.lock().map(|t| t.len()).unwrap_or(0)
    }

    /// 許可リストを全消去して永続化する (端末の総入れ替え用)。
    pub fn clear(&self) {
        if let Ok(mut t) = self.trusted.lock() {
            t.clear();
            let _ = save_trusted(&t);
        }
    }

    /// クライアント公開鍵 (ハンドシェイクの `remote_static`) を判定する。ペアリング窓が開いて
    /// いれば未知鍵をここで登録して消費する (1 回限り)。`Admit` で結果を返す。
    pub fn admit(&self, key: Option<&[u8]>) -> Admit {
        if !self.enabled.load(Ordering::Relaxed) {
            return Admit::Disabled;
        }
        let Some(key) = key else {
            return Admit::Rejected;
        };
        let Ok(mut trusted) = self.trusted.lock() else {
            return Admit::Rejected;
        };
        if trusted.iter().any(|d| d.pubkey == key) {
            return Admit::Trusted;
        }
        // ペアリング窓が開いていれば、この未知端末を 1 台だけ登録して窓を閉じる。
        if self.pairing.swap(false, Ordering::Relaxed) {
            trusted.push(TrustedDevice {
                label: default_label(key),
                pubkey: key.to_vec(),
            });
            let _ = save_trusted(&trusted);
            return Admit::Enrolled;
        }
        Admit::Rejected
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
    if s.len() % 2 != 0 {
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
    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("Aurora");
    p.push(file);
    Some(p)
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
    fn admit_disabled_passes_through() {
        let g = DeviceGuard {
            enabled: AtomicBool::new(false),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(Vec::new()),
        };
        assert!(matches!(g.admit(Some(&[1, 2, 3])), Admit::Disabled));
        assert!(matches!(g.admit(None), Admit::Disabled));
    }

    #[test]
    fn admit_enforces_and_pairs() {
        let g = DeviceGuard {
            enabled: AtomicBool::new(true),
            pairing: AtomicBool::new(false),
            trusted: Mutex::new(Vec::new()),
        };
        // 有効・未登録・ペアリング窓なし → 拒否。鍵なしも拒否。
        assert!(matches!(g.admit(Some(&[9, 9, 9, 9])), Admit::Rejected));
        assert!(matches!(g.admit(None), Admit::Rejected));

        // ペアリング窓を開くと次の 1 台を登録して許可。窓は消費される。
        g.arm_pairing();
        assert!(matches!(g.admit(Some(&[9, 9, 9, 9])), Admit::Enrolled));
        assert!(matches!(g.admit(Some(&[9, 9, 9, 9])), Admit::Trusted));
        assert!(!g.pairing_armed());

        // 別の未知端末は窓が閉じているので拒否。
        assert!(matches!(g.admit(Some(&[7, 7])), Admit::Rejected));
        assert_eq!(g.trusted_count(), 1);
    }
}
