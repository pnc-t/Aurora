//! 本体 ⇔ 取り込みワーカー間のメッセージと、パイプ上の枠付け (`u32 LE 長さ + bincode`)。
//!
//! ワーカーの標準入力 = 本体からの指示 ([`ToWorker`])、標準出力 = ワーカーからの出力
//! ([`FromWorker`])。どちらも同じ PC 内の親子プロセス間だけで使う (外部入力ではない)。

use std::io::{Read, Write};

use aurora_common::proto::VideoCodec;
use serde::{Deserialize, Serialize};

use crate::encode::EncodedNal;
use crate::net::{CodecStatus, EncoderCommand};

/// 1 メッセージの上限。4K のキーフレームでも数 MB に収まる (壊れた長さでの巨大確保を防ぐ)。
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// 本体 → ワーカー。
#[derive(Serialize, Deserialize)]
pub enum ToWorker {
    /// 起動直後に 1 度だけ送る設定。
    Init {
        /// 最初に取り込むディスプレイ (GDI 名)。
        display: String,
        /// 運用者が決めた送出 fps 上限 (0 = 無制限)。
        fps_cap: u32,
        /// ビットレート上限 (bps、`u32::MAX` = 無制限)。
        max_bitrate_bps: u32,
        /// エンコード GPU の指定 (解決済み)。
        encode_gpu: Option<String>,
        /// エンコード遅延等の計測ログ (`--perf`)。
        perf: bool,
    },
    Encoder(EncoderCommand),
    /// 取り込むディスプレイを切り替える。
    SelectDisplay(String),
    /// クライアント要求の fps 上限。
    Fps(u32),
    /// 送出する映像の最大高さ (0 = 取り込み解像度のまま)。
    Scale(u32),
    /// 映像へのカーソル合成。
    Cursor(bool),
    /// 現在の画面を 1 枚送らせる (新規接続・キーフレーム要求時)。
    ForceFrame,
    /// 本体側の仮想ディスプレイのテイクオーバー状態 (ワーカーの固着判定に使う)。
    Takeover(bool),
}

/// ワーカー → 本体。
#[derive(Serialize, Deserialize)]
pub enum FromWorker {
    /// 取り込みとエンコーダの準備ができた。`resolution` は最初のフレームの寸法 (取れなければ `None`)。
    Ready {
        resolution: Option<(u32, u32)>,
        host_codecs: Vec<VideoCodec>,
    },
    Nal(EncodedNal),
    Codec(CodecStatus),
    /// エンコーダの再構築が尽き、映像配信が止まった (トレイへ知らせる)。
    EncodeStalled,
    /// テイクオーバー中に取り込みが固着し続けた。物理ディスプレイの復元 (end_takeover) は
    /// テイクオーバーを始めた本体でしか行えないので依頼する。
    RestoreTakeover,
}

/// 1 メッセージを書く (呼び出し側で flush する)。
pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> anyhow::Result<()> {
    let bytes = bincode::serialize(msg)?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    Ok(())
}

/// 1 メッセージを読む。相手が閉じていたら `Ok(None)`。
pub fn read_msg<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> anyhow::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_MESSAGE, "worker message too large ({len} bytes)");
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(bincode::deserialize(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_messages() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &ToWorker::SelectDisplay(r"\\.\DISPLAY2".into())).unwrap();
        write_msg(
            &mut buf,
            &FromWorker::Nal(EncodedNal {
                data: vec![0, 0, 0, 1, 0x65],
                keyframe: true,
                frame_id: 7,
            }),
        )
        .unwrap();
        let mut r = buf.as_slice();
        match read_msg::<ToWorker>(&mut r).unwrap() {
            Some(ToWorker::SelectDisplay(name)) => assert_eq!(name, r"\\.\DISPLAY2"),
            _ => panic!("unexpected message"),
        }
        match read_msg::<FromWorker>(&mut r).unwrap() {
            Some(FromWorker::Nal(n)) => {
                assert_eq!(n.data, vec![0, 0, 0, 1, 0x65]);
                assert!(n.keyframe);
                assert_eq!(n.frame_id, 7);
            }
            _ => panic!("unexpected message"),
        }
        assert!(read_msg::<ToWorker>(&mut r).unwrap().is_none());
    }
}
