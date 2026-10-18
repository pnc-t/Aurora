//! 映像コーデックネゴシエーション: host 能力と全クライアント申告の共通集合から
//! 優先順位 (AV1 > HEVC > H264) で最良コーデックを選ぶ純関数。
//!
//! エンコーダは全クライアント共有 (net/client.rs の SetCursorOverlay と同じ理由: 単一
//! エンコーダを最悪リンク/最小公倍数の設定に合わせる) なので、1 クライアントでも
//! 対応しないコーデックは選べない。
use super::*;

/// 優先順位: AV1 > HEVC > H264。
const PRIORITY: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];

/// host 能力 (`host_codecs`) と、接続中の全クライアントの申告集合 (`client_sets`。各要素が
/// 1 クライアントぶんの `SetSupportedCodecs` 内容。未申告クライアントは呼び出し側が
/// `[H264]` を補って渡す) から、両者の共通集合の最良コーデックを返す。
///
/// クライアントが 1 つも無ければ host 能力の最良を返す。共通集合が空になることは無い
/// (H264 は host/client 双方が必ず対応する前提: host は SW フォールバックを持ち、client の
/// 未申告時の既定も H264)。
pub(crate) fn negotiate_codec(
    host_codecs: &[VideoCodec],
    client_sets: &[Vec<VideoCodec>],
) -> VideoCodec {
    for &c in &PRIORITY {
        if !host_codecs.contains(&c) {
            continue;
        }
        if client_sets.iter().all(|set| set.contains(&c)) {
            return c;
        }
    }
    VideoCodec::H264
}

/// encode スレッド → 非同期ランタイムへの実効コーデック報告。エンコーダの (再) 作成は
/// std::thread (encode_loop) 側で行われるため、その結果 (要求どおりだったか/フォールバック
/// したか) を非同期側 (net セッション) へ伝え、`VideoCodecChanged` の配信とネゴシエーション
/// 状態の是正 (host_codecs から使えないコーデックを外す) を行わせる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CodecStatus {
    /// (再) 作成が完了し、この実効コーデックで送出中 (要求どおり)。
    Active(VideoCodec),
    /// 要求コーデック `failed` での (再) 作成 / 送出に失敗し、H.264 へ動的フォールバックした。
    Fallback { failed: VideoCodec },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_clients_uses_host_best() {
        let host = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        assert_eq!(negotiate_codec(&host, &[]), VideoCodec::Av1);
    }

    #[test]
    fn common_subset_wins_over_host_best() {
        let host = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        // 1 クライアントは AV1 非対応。共有エンコーダなので AV1 は選べず HEVC に落ちる。
        let clients = vec![
            vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
            vec![VideoCodec::Hevc, VideoCodec::H264],
        ];
        assert_eq!(negotiate_codec(&host, &clients), VideoCodec::Hevc);
    }

    #[test]
    fn undeclared_client_defaults_to_h264_only() {
        let host = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        // 未申告クライアントは呼び出し側が [H264] を補って渡す前提。
        let clients = vec![vec![VideoCodec::H264]];
        assert_eq!(negotiate_codec(&host, &clients), VideoCodec::H264);
    }

    #[test]
    fn host_missing_codec_is_never_selected_even_if_all_clients_support_it() {
        // host が HEVC 非対応 (probe で除外済み) なら、クライアント全員が対応していても選ばない。
        let host = [VideoCodec::Av1, VideoCodec::H264];
        let clients = vec![vec![VideoCodec::Hevc, VideoCodec::H264]];
        assert_eq!(negotiate_codec(&host, &clients), VideoCodec::H264);
    }

    #[test]
    fn recompute_after_disconnect_can_upgrade() {
        // 2 クライアント中 1 つが H264 のみ → H264。切断後に残りが AV1 対応なら AV1 に上がる。
        let host = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        let with_both = vec![
            vec![VideoCodec::H264],
            vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
        ];
        assert_eq!(negotiate_codec(&host, &with_both), VideoCodec::H264);
        let after_disconnect = vec![vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264]];
        assert_eq!(negotiate_codec(&host, &after_disconnect), VideoCodec::Av1);
    }

    #[test]
    fn runtime_fallback_removes_codec_from_host_capability() {
        // AV1 の実エンコードが失敗した想定: host_codecs から外すと次点の HEVC が選ばれる。
        let mut host = vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
        let clients = vec![vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264]];
        assert_eq!(negotiate_codec(&host, &clients), VideoCodec::Av1);
        host.retain(|&c| c != VideoCodec::Av1);
        assert_eq!(negotiate_codec(&host, &clients), VideoCodec::Hevc);
    }
}
