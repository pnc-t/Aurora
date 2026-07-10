//! 制御チャネルメッセージ定義。bincodeでシリアライズ。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// 仮想ディスプレイ (IDD / MttVDD) か。クライアントが「仮想ディスプレイを使う」設定の
    /// とき、接続時にこの id を自動選択してキャプチャ対象にする。
    pub is_virtual: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    MouseWheel {
        delta: i32,
        horizontal: bool,
    },
    Key {
        vk: u32,
        down: bool,
    },
    /// ポインタロック(キャプチャ)モードの相対移動量。クライアントの Pointer Lock API
    /// が返す `movementX/Y` をそのまま運ぶ。bincode タグ互換のため必ず末尾に置く。
    MouseMoveRelative {
        dx: i32,
        dy: i32,
    },
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
        /// ホストのプライマリ NIC の MAC アドレス。取得失敗時は `None`。クライアントは
        /// これを host エントリに保存し、WoL 中継 (aurora-wol) への wake 要求に使う。
        /// host/client 同時更新前提のため bincode タグ互換は割り切って末尾に追加する。
        mac: Option<[u8; 6]>,
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
    /// クライアントからの映像パラメータ変更 (実行時)。末尾に追加し既存タグを保持する。
    SetVideoParams {
        /// 送出フレームレート上限 (fps)。0 = 無制限。
        fps: u32,
        /// ビットレート上限 (bps)。適応制御の上限 + エンコーダの目標。
        max_bitrate_bps: u32,
    },
    /// クライアント → ホスト。音声送出のミュート切替。ミュート中はホストが音声 UDP を
    /// 送らない (音が止まるのに加え、わずかだが帯域も止まる)。末尾に追加しタグ互換を保つ。
    SetAudioMuted {
        muted: bool,
    },
    /// クライアント → ホスト。送出する映像の最大高さ (px)。0 = キャプチャ解像度のまま。
    /// ホストは GPU で縮小 (アスペクト比維持) してからエンコードし、帯域と負荷を下げる。
    SetResolution {
        max_height: u32,
    },
    /// クライアント → ホスト。クライアントの映像表示領域サイズ (物理 px)。ホストは
    /// キャプチャ対象ディスプレイの解像度を、これに最も近い対応モードへ変更する (RDP 風)。
    /// 黒帯を無くし 1:1 表示にするのが狙い。末尾に追加しタグ互換を保つ。
    SetClientViewport {
        width: u32,
        height: u32,
    },
    /// ホスト → クライアント。解像度変更 (SetClientViewport / ディスプレイ切替) の結果、
    /// アクティブなキャプチャ対象ディスプレイの実解像度が変わったことを通知する。
    /// クライアントは入力スケールとデコーダを更新する。末尾に追加しタグ互換を保つ。
    DisplayResized {
        id: u32,
        width: u32,
        height: u32,
    },
    /// クライアント → ホスト。映像送出の一時停止 / 再開。マルチタブで非アクティブな
    /// タブの帯域を止めるために使う。`paused=true` の間はホストが映像 UDP を送らない
    /// (フレームはドレインして捨てる)。再開時はクライアントが RequestKeyframe で映像を
    /// 復帰させる。末尾に追加しタグ互換を保つ。
    SetVideoPaused {
        paused: bool,
    },
    /// クライアント → ホスト。映像へのカーソル合成の ON/OFF。DXGI はカーソルを映像に含めない
    /// ため、ホストが別取得して焼き込む。クライアントは**マウスキャプチャ (ポインタロック) 中**
    /// だけ ON にする (絶対座標モードはローカルカーソルを使うため不要、二重表示も防ぐ)。
    /// 共有エンコーダなので、いずれかのクライアントが ON を要求していれば合成する。
    /// 末尾に追加しタグ互換を保つ。
    SetCursorOverlay {
        enabled: bool,
    },
}

/// WoL 中継デーモン (aurora-wol) との制御メッセージ。Noise ハンドシェイク後の TCP フレームで送受する。
/// 映像セッションの [`ControlMsg`] とは別チャネル・別プロトコルの独立したメッセージ (混同しないこと)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WolMsg {
    /// 対象 MAC へのマジックパケット送出要求。
    Wake { mac: [u8; 6] },
    /// 送出結果。
    Ack { ok: bool, message: String },
}
