//! 性能計測 (`--perf` 時のみ有効)。
//!
//! エンコードループから純エンコード時間 (push_frame → poll_nal でNAL取得まで、
//! ネットワーク送信のバックプレッシャを含まない) とNALサイズを受け取り、
//! 1 秒窓で avg/min/max/p95 とソースビットレート・FPS を `info` ログに出す。
//! PLAN.md の目標 (エンコード遅延 < 10ms、動画時 5〜15Mbps) の実測に使う。
//!
//! 注意: NAL を drain するクライアントが接続していないと encode_loop が送信側で
//! ブロックし、計測も進まない。perf ログは実セッション中にのみ出力される。

use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(1);

/// 1 窓ぶんの集計結果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerfSample {
    pub fps: u32,
    pub avg_us: u32,
    pub p95_us: u32,
    pub min_us: u32,
    pub max_us: u32,
    pub src_bitrate_kbps: u32,
    /// capture→encode チャネルでフレームが待たされた時間の avg / p95 (マイクロ秒)。
    /// fps ペーシングの間引き間隔とチャネル滞留が出る。サンプルが無い窓では 0。
    pub cap_wait_avg_us: u32,
    pub cap_wait_p95_us: u32,
}

/// 1 秒窓のエンコード遅延・スループット集計器。
pub struct EncodeMeter {
    window_start: Instant,
    /// この窓で観測したエンコード所要時間 (マイクロ秒)。p95 算出のため保持。
    samples_us: Vec<u32>,
    /// この窓で観測した capture→encode 待ち時間 (マイクロ秒)。p95 算出のため保持。
    cap_wait_us: Vec<u32>,
    /// この窓で生成した NAL の総バイト数 (ソースビットレート算出用)。
    bytes: u64,
}

impl Default for EncodeMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl EncodeMeter {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            samples_us: Vec::with_capacity(120),
            cap_wait_us: Vec::with_capacity(120),
            bytes: 0,
        }
    }

    /// 1 フレーム分のエンコード結果を記録する。`cap_wait_us` は capture→encode チャネルで
    /// このフレームが待たされた時間 (push 直前に `captured_at.elapsed()` で測る)。
    pub fn record(&mut self, encode_us: u32, nal_bytes: usize, cap_wait_us: u32) {
        self.samples_us.push(encode_us);
        self.cap_wait_us.push(cap_wait_us);
        self.bytes += nal_bytes as u64;
    }

    /// 窓が満了していれば集計をログ出力してリセットする。各ループ反復で呼ぶ。
    pub fn maybe_flush(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < WINDOW || self.samples_us.is_empty() {
            return;
        }
        let sample = self.compute(elapsed.as_secs_f64());
        tracing::info!(
            fps = sample.fps,
            encode_avg_ms = format!("{:.2}", sample.avg_us as f64 / 1000.0),
            encode_p95_ms = format!("{:.2}", sample.p95_us as f64 / 1000.0),
            encode_min_ms = format!("{:.2}", sample.min_us as f64 / 1000.0),
            encode_max_ms = format!("{:.2}", sample.max_us as f64 / 1000.0),
            cap_wait_avg_ms = format!("{:.2}", sample.cap_wait_avg_us as f64 / 1000.0),
            cap_wait_p95_ms = format!("{:.2}", sample.cap_wait_p95_us as f64 / 1000.0),
            src_bitrate_kbps = sample.src_bitrate_kbps,
            "perf"
        );
        self.window_start = Instant::now();
        self.samples_us.clear();
        self.cap_wait_us.clear();
        self.bytes = 0;
    }

    /// 蓄積済みサンプルから集計値を算出する (`samples_us` をソートする副作用あり)。
    /// `secs` は窓の実経過秒。呼び出し側で空でないことを保証する。
    fn compute(&mut self, secs: f64) -> PerfSample {
        let n = self.samples_us.len();
        let sum: u64 = self.samples_us.iter().map(|&x| x as u64).sum();
        let avg_us = (sum / n as u64) as u32;
        let min_us = *self.samples_us.iter().min().unwrap_or(&0);
        let max_us = *self.samples_us.iter().max().unwrap_or(&0);

        self.samples_us.sort_unstable();
        let p95_us = p95(&self.samples_us);

        // capture→encode 待ちは encode 時間とは別系列なので独立に集計する (空窓は 0)。
        let cap_n = self.cap_wait_us.len();
        let cap_wait_avg_us = if cap_n == 0 {
            0
        } else {
            (self.cap_wait_us.iter().map(|&x| x as u64).sum::<u64>() / cap_n as u64) as u32
        };
        self.cap_wait_us.sort_unstable();
        let cap_wait_p95_us = p95(&self.cap_wait_us);

        PerfSample {
            fps: (n as f64 / secs).round() as u32,
            avg_us,
            p95_us,
            min_us,
            max_us,
            src_bitrate_kbps: ((self.bytes as f64) * 8.0 / 1000.0 / secs) as u32,
            cap_wait_avg_us,
            cap_wait_p95_us,
        }
    }
}

/// ソート済みスライスの p95 (切り上げ index、空なら 0)。
fn p95(sorted: &[u32]) -> u32 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    let idx = (((n as f64) * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_aggregates() {
        let mut m = EncodeMeter::new();
        // 10 フレーム: 1000,2000,...,10000 us。総 NAL 1,000,000 bytes を 1 秒で。
        // cap_wait は全フレーム一定 500us とする。
        for i in 1..=10u32 {
            m.record(i * 1000, 100_000, 500);
        }
        let s = m.compute(1.0);
        assert_eq!(s.fps, 10);
        assert_eq!(s.min_us, 1000);
        assert_eq!(s.max_us, 10000);
        assert_eq!(s.avg_us, 5500); // (1000+..+10000)/10
        // p95: ceil(10*0.95)=10 → idx 9 → 10000us
        assert_eq!(s.p95_us, 10000);
        // 1,000,000 bytes * 8 / 1000 / 1s = 8000 kbps
        assert_eq!(s.src_bitrate_kbps, 8000);
        assert_eq!(s.cap_wait_avg_us, 500);
        assert_eq!(s.cap_wait_p95_us, 500);
    }

    #[test]
    fn p95_excludes_single_outlier() {
        let mut m = EncodeMeter::new();
        // 100 サンプル: 0..=98 を 2000us、1 本だけ 50000us の外れ値。
        for _ in 0..99 {
            m.record(2000, 0, 0);
        }
        m.record(50000, 0, 0);
        let s = m.compute(1.0);
        // ceil(100*0.95)=95 → idx 94 → ソート後も 2000us (外れ値は p95 に出ない)
        assert_eq!(s.p95_us, 2000);
        assert_eq!(s.max_us, 50000);
    }
}
