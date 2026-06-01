//! 共有プロトコル・暗号化・フレーミング層。

pub mod framing;
pub mod noise;
pub mod proto;

pub use framing::{VideoPacket, VideoPacketFlags};
pub use noise::{HandshakeState, NoiseError, TransportSession, psk_from_password};
pub use proto::{ControlMsg, Display, InputEvent, MouseButton};
