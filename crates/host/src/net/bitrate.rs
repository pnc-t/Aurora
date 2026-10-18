//! ビットレート適応制御と、制御ループ→encode/input スレッドへの通知型。
use super::*;
use crate::encode::{DEFAULT_BITRATE_BPS, MIN_BITRATE_BPS};

/// 制御ループ → encode スレッドへの通知。
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum EncoderCommand {
    RequestKeyframe,
    /// 適応制御による平均ビットレート変更 (bps)。
    SetBitrate(u32),
    /// ネゴシエーション結果の送出コーデック変更。encode スレッドがエンコーダを作り直し、
    /// 実効コーデック (要求どおりか、HW 非対応で H264 へフォールバックしたか) を
    /// `CodecStatus` で net へ報告する。
    SetCodec(VideoCodec),
}

/// 制御ループ → input スレッドへの通知。入力イベントとディスプレイ切替を 1 本にまとめる。
pub enum InputCmd {
    Event(InputEvent),
    /// キャプチャ対象ディスプレイ切替に伴う座標系更新。
    SetDisplay(DisplayGeom),
}

/// クライアント Stats を元にしたビットレート適応制御 (輻輳回避型)。
///
/// 単純な AIMD (+1Mbps/秒で上げ続け、ロスで ×0.7 急降下) は持続可能上限を毎回オーバーシュート
/// してクラッシュする「ノコギリ波」になり、体感が「軽い ↔ 重い+ブロックノイズ」で激しく振動する
/// (実測: 天井 ~20-25Mbps を +1Mbps/秒で突き抜けては急降下、15-20 秒周期)。
///
/// そこで直近で輻輳した rate を `ceiling_bps` (持続可能上限の推定) として記憶し、そこに**近づいたら
/// 増速を fast(+1Mbps) から slow(+250kbps) に落として張り付かせる** (TCP の輻輳回避相当)。これで
/// 持続可能なレートに安定収束し波が小さくなる。`ceiling_bps` は非輻輳時に緩やかに上方回復させ、
/// 条件が改善したら再び上を探る。
///
/// 制御は `MIN_BITRATE_BPS`..=`max_bps` にクランプする (上限は `--max-bitrate` で可変)。
pub(crate) struct BitrateController {
    current_bps: u32,
    /// 適応制御の上限 (bps)。`--max-bitrate` 由来。エンコーダのピーク上限と揃える。
    max_bps: u32,
    /// 運用者が `--max-bitrate` で決めた絶対上限。クライアントの SetVideoParams はこれを超えて
    /// 上限を引き上げられない (帯域/負荷の上限はホスト運用者の権限)。
    limit_bps: u32,
    /// 持続可能上限の推定 (直近の輻輳点)。増速を緩める閾値に使う。
    ceiling_bps: u32,
    /// 直前サンプルが輻輳だったか。輻輳の立ち上がりでのみ ceiling を記録するため。
    in_congestion: bool,
}

impl BitrateController {
    pub(crate) fn new(max_bps: u32) -> Self {
        // 上限が下限 (MIN_BITRATE_BPS) を下回ると on_stats の clamp が min>max で
        // panic するため、下限以上へ持ち上げる (無制限 = u32::MAX はそのまま)。
        let max_bps = max_bps.max(MIN_BITRATE_BPS);
        Self {
            current_bps: DEFAULT_BITRATE_BPS.clamp(MIN_BITRATE_BPS, max_bps),
            max_bps,
            limit_bps: max_bps,
            ceiling_bps: max_bps,
            in_congestion: false,
        }
    }

    /// クライアントからビットレート上限を明示変更する。上限を更新し、**現在値は据え置き**
    /// (新上限でクランプのみ)。旧実装は上限へ即ジャンプしていたが、それが接続直後に天井を
    /// 突き抜けてロス→急降下する初期の波の元だったため、下から `on_stats` で滑らかに上げる。
    /// 天井推定はリセットして新上限まで再探索させる。新しい現在値を返す。
    pub(crate) fn set_max(&mut self, max_bps: u32) -> u32 {
        self.max_bps = max_bps.min(self.limit_bps).max(MIN_BITRATE_BPS);
        self.current_bps = self.current_bps.clamp(MIN_BITRATE_BPS, self.max_bps);
        self.ceiling_bps = self.max_bps;
        self.in_congestion = false;
        self.current_bps
    }

    /// 新しい Stats を反映し、ビットレートを変更すべきなら新しい値 (bps) を返す。
    pub(crate) fn on_stats(&mut self, loss_permille: u16, rtt_us: u32) -> Option<u32> {
        const LOSS_HIGH: u16 = 50; // 5% 超で輻輳とみなす
        const LOSS_LOW: u16 = 10; // 1% 未満なら余裕あり
        const RTT_OK_US: u32 = 80_000; // 80ms 未満なら増速を許可
        const STEP_UP_FAST: u32 = 1_000_000; // ceiling から遠いとき
        const STEP_UP_SLOW: u32 = 250_000; // ceiling 付近 (輻輳回避でゆっくり張り付く)
        const DECREASE: f32 = 0.8;

        let congested = loss_permille > LOSS_HIGH;

        // 非輻輳時は ceiling をゆっくり上方回復させ、条件改善に追従する (低い値での固着防止)。
        if !congested {
            // 無制限 (u32::MAX) だと加算があふれるため飽和させる。
            self.ceiling_bps = self
                .ceiling_bps
                .saturating_add(self.ceiling_bps / 128)
                .min(self.max_bps);
        }

        let next = if congested {
            // 輻輳の立ち上がりでのみ ceiling を記録する。持続中に連続記録すると、減速途中の
            // 低い rate を天井と誤認して過小評価してしまう。
            if !self.in_congestion {
                self.ceiling_bps = self.current_bps;
            }
            (self.current_bps as f32 * DECREASE) as u32
        } else if loss_permille < LOSS_LOW && rtt_us < RTT_OK_US {
            // ceiling に近いか遠いかで増速量を変える: 近ければ輻輳回避でゆっくり張り付く。
            let near = (self.current_bps as u64) * 100 >= (self.ceiling_bps as u64) * 85;
            let step = if near { STEP_UP_SLOW } else { STEP_UP_FAST };
            self.current_bps.saturating_add(step)
        } else {
            self.current_bps
        }
        .clamp(MIN_BITRATE_BPS, self.max_bps);

        self.in_congestion = congested;

        if next != self.current_bps {
            self.current_bps = next;
            Some(next)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ceiling から遠いときは fast(+1Mbps)、近い (>=85%) ときは slow(+250kbps) で張り付く。
    /// 単純 AIMD のノコギリ波 (毎回 +1Mbps で天井突き抜け) を防ぐ核心。
    #[test]
    fn ramps_fast_far_below_ceiling_then_crawls_near_it() {
        let mut bc = BitrateController::new(30_000_000);
        bc.ceiling_bps = 20_000_000;
        bc.current_bps = 10_000_000; // 天井の 50% → fast
        assert_eq!(bc.on_stats(0, 1000), Some(11_000_000));
        bc.current_bps = 18_000_000; // 天井の 90% → slow
        assert_eq!(bc.on_stats(0, 1000), Some(18_250_000));
    }

    /// 輻輳では ×0.8 減速し、ceiling は**立ち上がりで 1 度だけ**記録する (持続中の連続記録で
    /// 減速途中の低い rate を天井と誤認しない)。
    #[test]
    fn congestion_decreases_and_records_ceiling_once() {
        let mut bc = BitrateController::new(30_000_000);
        bc.current_bps = 25_000_000;
        bc.ceiling_bps = 30_000_000;
        // 立ち上がり: ceiling=25M を記録、25M→20M。
        assert_eq!(bc.on_stats(200, 1000), Some(20_000_000));
        assert_eq!(bc.ceiling_bps, 25_000_000);
        // 持続: ceiling 据え置き、20M→16M。
        assert_eq!(bc.on_stats(200, 1000), Some(16_000_000));
        assert_eq!(bc.ceiling_bps, 25_000_000);
    }

    /// 無制限 (u32::MAX) でも ceiling の上方回復があふれない。
    #[test]
    fn unlimited_ceiling_recovery_does_not_overflow() {
        let mut bc = BitrateController::new(u32::MAX);
        bc.current_bps = 10_000_000;
        let _ = bc.on_stats(0, 1000);
        assert_eq!(bc.ceiling_bps, u32::MAX);
    }

    /// クライアントの set_max は運用者の上限を超えられない (下げるのは可)。
    #[test]
    fn client_cannot_raise_above_operator_limit() {
        let mut bc = BitrateController::new(20_000_000);
        bc.set_max(100_000_000);
        assert_eq!(bc.max_bps, 20_000_000);
        bc.set_max(8_000_000);
        assert_eq!(bc.max_bps, 8_000_000);
        bc.set_max(15_000_000);
        assert_eq!(bc.max_bps, 15_000_000);
    }
}
