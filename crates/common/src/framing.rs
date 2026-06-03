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
}
