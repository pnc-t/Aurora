//! 送信元 IP 単位の認証失敗スロットル (ロックアウト無しの単一要素パスワードへのオンライン総当たりを減速)。
use super::*;

/// 送信元 IP ごとの認証失敗を追跡し、連続失敗で接続応答を指数的に遅らせるスロットル。
///
/// 本アプリの認証はロックアウトの無い単一要素 (パスワード→PSK) なので、オンライン総当たりの
/// レートを落とすのが狙い。**IP 単位**である点が要: グローバルなロックアウトにすると、攻撃者が
/// 失敗を撃ち込むだけで正規ユーザー (別 IP) を締め出せる自爆 DoS になる。TCP ハンドシェイクを
/// 完了した相手の IP しか記録しないため送信元詐称も効かない。正規ユーザーの成功で即リセットする。
pub(crate) struct AuthThrottle {
    /// 送信元 IP → (連続失敗回数, 最終失敗時刻)。
    inner: std::sync::Mutex<HashMap<IpAddr, FailRecord>>,
    /// tray 表示用の累計失敗カウンタ。
    total: Arc<AtomicU32>,
}

struct FailRecord {
    fails: u32,
    last: Instant,
}

impl AuthThrottle {
    /// 連続失敗のリセット猶予 兼 古いエントリの掃除しきい値。最後の失敗からこれを超えたら
    /// 連続失敗を 0 に戻す (誤入力した正規ユーザーを長く罰しない)。
    const RESET_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
    /// 初回失敗の基準遅延。以降 2 倍ずつ伸ばす。
    const BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
    /// 遅延の上限。
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

    pub(crate) fn new(total: Arc<AtomicU32>) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            total,
        }
    }

    /// 失敗を記録し、この接続を閉じる前に挟むべき遅延を返す。
    pub(crate) fn record_failure(&self, ip: IpAddr) -> std::time::Duration {
        self.total.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let Ok(mut map) = self.inner.lock() else {
            return Self::BASE_DELAY;
        };
        // 古いエントリを掃除し、マップを実際の送信元 IP 数に閉じ込める (メモリ上限)。
        map.retain(|_, r| now.duration_since(r.last) < Self::RESET_AFTER);
        let rec = map.entry(ip).or_insert(FailRecord {
            fails: 0,
            last: now,
        });
        rec.fails = rec.fails.saturating_add(1);
        rec.last = now;
        // 指数バックオフ: BASE * 2^(fails-1) を MAX で頭打ち。
        let shift = (rec.fails - 1).min(5);
        Self::BASE_DELAY
            .checked_mul(1u32 << shift)
            .unwrap_or(Self::MAX_DELAY)
            .min(Self::MAX_DELAY)
    }

    /// 認証成功時に当該 IP の連続失敗を消す (正規ユーザーは次回も遅延ゼロ)。
    pub(crate) fn record_success(&self, ip: IpAddr) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&ip);
        }
    }
}
