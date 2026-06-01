//! UDP映像フレーミング。1パケット = ヘッダ(7B) + payload。

use thiserror::Error;

pub const HEADER_LEN: usize = 7;
pub const MAX_MTU: usize = 1200;
pub const MAX_PAYLOAD: usize = MAX_MTU - HEADER_LEN;

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
