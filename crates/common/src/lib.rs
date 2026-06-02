//! 共有プロトコル・暗号化・フレーミング層。

pub mod clipboard;
pub mod framing;
pub mod noise;
pub mod proto;

pub use framing::{AudioPacket, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags};
pub use noise::{
    HandshakeState, NONCE_DOMAIN_AUDIO, NONCE_DOMAIN_CTRL, NONCE_DOMAIN_VIDEO, NoiseError,
    TransportSession, psk_from_password,
};
pub use proto::{ControlMsg, Display, InputEvent, MouseButton};
