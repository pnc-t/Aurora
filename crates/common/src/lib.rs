//! 共有プロトコル・暗号化・フレーミング層。

// 新たに追加する unsafe ブロックには直上に `// SAFETY:` を必須化する (規約の機械強制)。
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod clipboard;
pub mod dpapi;
pub mod framing;
pub mod fsname;
pub mod logging;
pub mod noise;
pub mod paths;
pub mod preauth;
pub mod proto;

pub use framing::{AudioPacket, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags};
pub use noise::{
    HandshakeState, NONCE_DOMAIN_AUDIO, NONCE_DOMAIN_CTRL, NONCE_DOMAIN_VIDEO, NoiseError,
    TransportSession, psk_from_password,
};
pub use proto::{ControlMsg, Display, InputEvent, MouseButton};
