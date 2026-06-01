//! 受信品質メトリクス収集 (ビットレート適応の計測側)。
//!
//! UDP 受信ループ・Pong ハンドラから atomic で更新し、定期タスクが 1 秒窓で
//! レート (ビットレート / FPS / 損失率) を算出する。算出結果は
//! - ホストへ `ControlMsg::Stats` として送り、エンコーダのビットレート適応に使う
//! - フロントへ `ClientEvent::Stats` として送り、ステータス表示に使う

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// 受信側カウンタ。各ループから共有して加算する。
#[derive(Default)]
pub struct StatsCollector {
    /// 受信した UDP データグラムの総バイト数 (映像 + 音声、暗号文サイズ)。
    bytes: AtomicU64,
    /// 受信した映像パケット数 (seq 連番ベース)。
    video_pkts: AtomicU64,
    /// seq ギャップから推定した損失パケット数。
    lost_pkts: AtomicU64,
    /// 組み立て完了した映像フレーム数。
    frames: AtomicU64,
    /// 直近の RTT (マイクロ秒)。Pong 受信で更新。
    rtt_us: AtomicU32,
}

impl StatsCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_video_pkt(&self) {
        self.video_pkts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_lost(&self, n: u64) {
        self.lost_pkts.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_rtt_us(&self, rtt_us: u32) {
        self.rtt_us.store(rtt_us, Ordering::Relaxed);
    }
}

/// `StatsCollector` の累積値を 1 回読み取ったスナップショット。
struct Snapshot {
    bytes: u64,
    video_pkts: u64,
    lost_pkts: u64,
    frames: u64,
    at: Instant,
}

impl Snapshot {
    fn take(c: &StatsCollector) -> Self {
        Self {
            bytes: c.bytes.load(Ordering::Relaxed),
            video_pkts: c.video_pkts.load(Ordering::Relaxed),
            lost_pkts: c.lost_pkts.load(Ordering::Relaxed),
            frames: c.frames.load(Ordering::Relaxed),
            at: Instant::now(),
        }
    }
}

/// 1 窓ぶんの計測結果。
pub struct StatsSample {
    pub bitrate_kbps: u32,
    pub fps: u8,
    pub rtt_us: u32,
    pub loss_permille: u16,
}

/// 累積カウンタの差分から窓ごとのレートを計算する状態を持つ。
pub struct StatsWindow {
    prev: Snapshot,
}

impl StatsWindow {
    pub fn new(c: &StatsCollector) -> Self {
        Self {
            prev: Snapshot::take(c),
        }
    }

    /// 前回呼び出しからの差分でレートを算出する。
    pub fn sample(&mut self, c: &StatsCollector) -> StatsSample {
        let now = Snapshot::take(c);
        let secs = now.at.duration_since(self.prev.at).as_secs_f64().max(1e-3);

        let bytes_d = now.bytes.saturating_sub(self.prev.bytes);
        let frames_d = now.frames.saturating_sub(self.prev.frames);
        let video_d = now.video_pkts.saturating_sub(self.prev.video_pkts);
        let lost_d = now.lost_pkts.saturating_sub(self.prev.lost_pkts);

        let bitrate_kbps = ((bytes_d as f64) * 8.0 / 1000.0 / secs) as u32;
        let fps = ((frames_d as f64) / secs).round().min(255.0) as u8;
        let expected = video_d + lost_d;
        let loss_permille = if expected > 0 {
            ((lost_d * 1000) / expected).min(1000) as u16
        } else {
            0
        };
        let rtt_us = c.rtt_us.load(Ordering::Relaxed);

        self.prev = now;
        StatsSample {
            bitrate_kbps,
            fps,
            rtt_us,
            loss_permille,
        }
    }
}
