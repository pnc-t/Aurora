//! 共有プロトコル・暗号化・フレーミング層。

pub mod framing;
pub mod noise;
pub mod proto;

pub use framing::{VideoPacket, VideoPacketFlags};
pub use proto::{ControlMsg, Display, InputEvent, MouseButton};
