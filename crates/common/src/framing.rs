//! UDP映像フレーミング。1パケット = ヘッダ(7B) + payload。
//!
//! 実際に送出される UDP データグラムは、この framed バイト列に net 層で
//! `[平文 nonce 8B] + [ストリームタグ 1B] + framed + [Poly1305 タグ 16B]` が
//! 付く。`MAX_PAYLOAD` はこれらを `MAX_MTU` から差し引いて算出し、暗号化後の
//! データグラムが MTU (Tailscale 1280 を想定した安全側 1200) を超えないようにする。

use thiserror::Error;

pub const HEADER_LEN: usize = 7;
pub const MAX_MTU: usize = 1200;
/// net 層が 1 データグラムに足す固定オーバーヘッド: 先頭平文 nonce 8B +
/// ストリームタグ 1B + AEAD (Poly1305) タグ 16B。
pub const DATAGRAM_OVERHEAD: usize = 8 + 1 + 16;
/// 1 パケットの最大 payload。最大ストリームヘッダ (映像 `HEADER_LEN`) と
/// `DATAGRAM_OVERHEAD` を MTU から引く (音声ヘッダ 4B はこれより小さく安全側)。
pub const MAX_PAYLOAD: usize = MAX_MTU - DATAGRAM_OVERHEAD - HEADER_LEN;

/// UDP メディア多重化のストリーム種別タグ。暗号化前 plaintext の先頭 1B に付ける。
/// 映像と音声は同一 UDP ソケット・同一 Noise セッションに相乗りする。
pub const STREAM_VIDEO: u8 = 0;
pub const STREAM_AUDIO: u8 = 1;

/// 音声パケットヘッダ長 (4B seq)。
pub const AUDIO_HEADER_LEN: usize = 4;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("packet too short")]
    TooShort,
    #[error("payload too large: {0}")]
    TooLarge(usize),
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoPacketFlags: u8 {
        const KEYFRAME = 0b0000_0001;
        const FRAME_START = 0b0000_0010;
        const FRAME_END = 0b0000_0100;
    }
}

#[derive(Debug, Clone)]
pub struct VideoPacket<'a> {
    pub seq: u32,
    pub flags: VideoPacketFlags,
    pub frame_id: u16,
    pub payload: &'a [u8],
}

impl<'a> VideoPacket<'a> {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FramingError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(FramingError::TooLarge(self.payload.len()));
        }
        out.clear();
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.push(self.flags.bits());
        out.extend_from_slice(&self.frame_id.to_be_bytes());
        out.extend_from_slice(self.payload);
        Ok(())
    }

    pub fn decode(buf: &'a [u8]) -> Result<Self, FramingError> {
        if buf.len() < HEADER_LEN {
            return Err(FramingError::TooShort);
        }
        let seq = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let flags = VideoPacketFlags::from_bits_truncate(buf[4]);
        let frame_id = u16::from_be_bytes(buf[5..7].try_into().unwrap());
        Ok(Self {
            seq,
            flags,
            frame_id,
            payload: &buf[HEADER_LEN..],
        })
    }
}

/// 音声 (Opus) パケット。1 パケット = 1 Opus フレーム (20ms)。
/// Opus フレームは MTU 内に収まるため分割しない。`seq` は欠落検出用。
#[derive(Debug, Clone)]
pub struct AudioPacket<'a> {
    pub seq: u32,
    /// Opus エンコード済みフレーム。
    pub payload: &'a [u8],
}

impl<'a> AudioPacket<'a> {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FramingError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(FramingError::TooLarge(self.payload.len()));
        }
        out.clear();
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(self.payload);
        Ok(())
    }

    pub fn decode(buf: &'a [u8]) -> Result<Self, FramingError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(FramingError::TooShort);
        }
        let seq = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        Ok(Self {
            seq,
            payload: &buf[AUDIO_HEADER_LEN..],
        })
    }
}

// --- TCP 制御チャネルのフレーミング (host / client 共通) ---
//
// 制御メッセージは Noise で暗号化した後、`[u16 BE 長さ] + [本体]` の 1 フレームとして TCP に
// 流す。長さが u16 のため 1 フレームは最大 65535B に自然に有界で、本体の意味的上限は復号後の
// bincode (`with_limit`) が守る。host/client の net 層に同一実装がコピーされていたのを集約した。

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// TCP から 1 フレーム (u16 BE 長さ prefix + 本体) を読み、本体を `out` に入れる。
///
/// **キャンセル安全ではない** (`read_exact` を 2 回行う複数段の await)。`select!` の枝に直接
/// 置かず、専用タスク内で回すこと (途中キャンセルでフレーム境界がズレる)。
pub async fn read_tcp_frame<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    out.resize(len, 0);
    stream.read_exact(out).await?;
    Ok(())
}

/// `data` を 1 フレーム (u16 BE 長さ prefix + 本体) として TCP に書く。u16 を超える長さは
/// フレーミングできないためエラー (呼び出し側で分割・上限管理する前提)。
pub async fn write_tcp_frame<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    data: &[u8],
) -> std::io::Result<()> {
    let len: u16 = data.len().try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("frame too large: {}", data.len()),
        )
    })?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(data).await?;
    stream.flush().await.ok();
    Ok(())
}

// --- 制御メッセージ (ControlMsg) の暗号化フレーム化 (host / client 共通) ---
//
// ワイヤ形式: bincode(ControlMsg) を CTRL ドメインの連番 nonce で Noise 暗号化し、上の
// `write_tcp_frame` で 1 フレームとして送る。nonce 採番・bincode 上限・暗号化→フレームの
// 順序はプロトコル不変条件なので、host/client にコピーを持たせず必ずここを通す
// (片側だけ変えると相手側とワイヤ互換が静かに壊れる)。

use crate::noise::{NONCE_DOMAIN_CTRL, NoiseError, TransportSession};
use crate::proto::ControlMsg;
use bincode::Options;

/// ControlMsg 1 件の最大シリアライズ長 (bincode `with_limit`)。u16 長さ prefix (65535B) と
/// 対の上限で、host/client は必ず同じ値で送受する (送信側だけ上げると大きいメッセージが
/// 相手側の deserialize でだけ失敗し、フレーキーな切断に見える)。
pub const MAX_CTRL_MSG: u64 = 64 * 1024;

/// 制御メッセージの送受信の失敗。
#[derive(Debug, Error)]
pub enum CtrlMsgError {
    #[error("encode ControlMsg: {0}")]
    Encode(#[source] bincode::Error),
    #[error("decode ControlMsg: {0}")]
    Decode(#[source] bincode::Error),
    #[error(transparent)]
    Crypto(#[from] NoiseError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn ctrl_bincode_opts() -> impl Options {
    bincode::DefaultOptions::new().with_limit(MAX_CTRL_MSG)
}

/// `msg` を bincode で直列化し、CTRL ドメインの連番 nonce (`tx_nonce` を採番して +1) で
/// 暗号化して 1 TCP フレームとして送る。
pub async fn send_ctrl_msg<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    session: &TransportSession,
    msg: &ControlMsg,
    tx_nonce: &mut u64,
) -> Result<(), CtrlMsgError> {
    let plain = ctrl_bincode_opts()
        .serialize(msg)
        .map_err(CtrlMsgError::Encode)?;
    let nonce = NONCE_DOMAIN_CTRL | *tx_nonce;
    *tx_nonce += 1;
    let mut cipher = Vec::new();
    session.encrypt(nonce, &plain, &mut cipher)?;
    write_tcp_frame(stream, &cipher).await?;
    Ok(())
}

/// 受信済みの 1 TCP フレーム (暗号文) を CTRL ドメインの連番 nonce (`rx_nonce` を採番して
/// +1) で復号し ControlMsg へ戻す。`plain` は復号バッファ (呼び出し側で再利用可)。
///
/// フレームの読み取り自体は含めない: host はフレーム読み取りを専用タスクへ隔離して
/// チャネル経由で受けるため (キャンセル安全性)、読み取りと復号は同じ場所にできない。
pub fn decode_ctrl_msg(
    session: &TransportSession,
    frame: &[u8],
    rx_nonce: &mut u64,
    plain: &mut Vec<u8>,
) -> Result<ControlMsg, CtrlMsgError> {
    plain.clear();
    let nonce = NONCE_DOMAIN_CTRL | *rx_nonce;
    *rx_nonce += 1;
    session.decrypt(nonce, frame, plain)?;
    ctrl_bincode_opts()
        .deserialize(plain)
        .map_err(CtrlMsgError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = vec![0x42; 800];
        let pkt = VideoPacket {
            seq: 0xdead_beef,
            flags: VideoPacketFlags::KEYFRAME | VideoPacketFlags::FRAME_START,
            frame_id: 0x1234,
            payload: &payload,
        };
        let mut buf = Vec::new();
        pkt.encode(&mut buf).unwrap();
        let dec = VideoPacket::decode(&buf).unwrap();
        assert_eq!(dec.seq, 0xdead_beef);
        assert!(dec.flags.contains(VideoPacketFlags::KEYFRAME));
        assert!(dec.flags.contains(VideoPacketFlags::FRAME_START));
        assert_eq!(dec.frame_id, 0x1234);
        assert_eq!(dec.payload, &payload[..]);
    }

    #[test]
    fn audio_roundtrip() {
        let payload = vec![0x7f; 240];
        let pkt = AudioPacket {
            seq: 0x0102_0304,
            payload: &payload,
        };
        let mut buf = Vec::new();
        pkt.encode(&mut buf).unwrap();
        let dec = AudioPacket::decode(&buf).unwrap();
        assert_eq!(dec.seq, 0x0102_0304);
        assert_eq!(dec.payload, &payload[..]);
    }

    #[test]
    fn too_large() {
        let big = vec![0u8; MAX_PAYLOAD + 1];
        let pkt = VideoPacket {
            seq: 0,
            flags: VideoPacketFlags::empty(),
            frame_id: 0,
            payload: &big,
        };
        let mut buf = Vec::new();
        assert!(pkt.encode(&mut buf).is_err());
    }

    /// send_ctrl_msg → (フレーム読み取り) → decode_ctrl_msg の往復。host/client が共有する
    /// ワイヤ形式 (nonce 採番・bincode 上限・暗号化→フレームの順序) の互換性をここで固定する。
    #[tokio::test]
    async fn ctrl_msg_roundtrip() {
        use crate::noise::{HandshakeState, psk_from_password};
        use crate::proto::ControlMsg;

        let psk = psk_from_password("hunter2");
        let mut ini = HandshakeState::new_initiator(&psk, None).unwrap();
        let mut res = HandshakeState::new_responder(&psk, None).unwrap();
        let mut buf = Vec::new();
        let mut tmp = Vec::new();
        ini.write_msg(&[], &mut buf).unwrap();
        res.read_msg(&buf, &mut tmp).unwrap();
        res.write_msg(&[], &mut buf).unwrap();
        ini.read_msg(&buf, &mut tmp).unwrap();
        ini.write_msg(&[], &mut buf).unwrap();
        res.read_msg(&buf, &mut tmp).unwrap();
        let ini_t = ini.into_transport().unwrap();
        let res_t = res.into_transport().unwrap();

        // 送信 (initiator) → TCP フレーム → 受信 (responder)。連番 nonce が両側で歩調を保つ。
        let mut wire = Vec::new();
        let mut tx_nonce: u64 = 0;
        for ts_us in [1u64, 2, 3] {
            send_ctrl_msg(
                &mut wire,
                &ini_t,
                &ControlMsg::Ping { ts_us },
                &mut tx_nonce,
            )
            .await
            .unwrap();
        }
        assert_eq!(tx_nonce, 3);

        let mut reader = std::io::Cursor::new(wire);
        let mut frame = Vec::new();
        let mut plain = Vec::new();
        let mut rx_nonce: u64 = 0;
        for expect in [1u64, 2, 3] {
            read_tcp_frame(&mut reader, &mut frame).await.unwrap();
            let msg = decode_ctrl_msg(&res_t, &frame, &mut rx_nonce, &mut plain).unwrap();
            match msg {
                ControlMsg::Ping { ts_us } => assert_eq!(ts_us, expect),
                other => panic!("unexpected msg: {other:?}"),
            }
        }
    }
}
