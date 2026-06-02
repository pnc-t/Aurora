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

/// ディレクトリ列挙の 1 エントリ (ProjFS ドライブリダイレクト用)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
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
    /// --- ProjFS ドライブリダイレクト (RDP 風) の RPC ---
    /// ホスト → クライアント。共有ルート相対 `path` のディレクトリを `offset` から列挙要求。
    FsList {
        req_id: u64,
        path: String,
        offset: u32,
    },
    /// クライアント → ホスト。`done=false` ならまだ続きがある (offset を進めて再要求)。
    FsListReply {
        req_id: u64,
        entries: Vec<FsEntry>,
        done: bool,
        error: Option<String>,
    },
    /// ホスト → クライアント。共有ルート相対 `path` のファイルを `offset` から最大 `length` 読み取り要求。
    FsRead {
        req_id: u64,
        path: String,
        offset: u64,
        length: u32,
    },
    /// クライアント → ホスト。`data` が空かつ `error=None` なら EOF。
    FsReadReply {
        req_id: u64,
        data: Vec<u8>,
        error: Option<String>,
    },
    /// --- ProjFS 書き戻し (ホスト Explorer → クライアント、一方向・応答なし) ---
    /// ファイル / ディレクトリを作成 (ファイルは 0 バイトに truncate)。
    FsCreate {
        path: String,
        is_dir: bool,
    },
    /// ファイルの `offset` 以降にバイトを書き込む。
    FsWrite {
        path: String,
        offset: u64,
        data: Vec<u8>,
    },
    /// ファイル / ディレクトリを削除する。
    FsDelete {
        path: String,
    },
    /// `from` を `to` にリネーム / 移動する。
    FsRename {
        from: String,
        to: String,
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
