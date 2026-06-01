//! 制御チャネルメッセージ定義。bincodeでシリアライズ。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, down: bool },
    MouseWheel { delta: i32, horizontal: bool },
    Key { vk: u32, down: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    HostInfo {
        displays: Vec<Display>,
        audio: bool,
    },
    SelectDisplay {
        id: u32,
    },
    Input(InputEvent),
    Clipboard {
        text: String,
    },
    FileRequest {
        id: u32,
        path: String,
    },
    /// 送信側 → 受信側。ファイル転送の開始を告知する (チャンク列の前に 1 回)。
    FileOffer {
        id: u32,
        name: String,
        size: u64,
    },
    FileChunk {
        id: u32,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    /// 受信側 → 送信側。転送完了 (ok=true) もしくはエラー (ok=false) を返す。
    FileResult {
        id: u32,
        ok: bool,
        message: String,
    },
    RequestKeyframe,
    Ping {
        ts_us: u64,
    },
    Pong {
        ts_us: u64,
    },
    Stats {
        bitrate_kbps: u32,
        fps: u8,
        rtt_us: u32,
        loss_permille: u16,
    },
}
