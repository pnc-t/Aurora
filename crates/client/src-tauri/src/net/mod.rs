//! クライアント側ネットワーク。
//! - TCP 制御チャネル: Noise XX-PSK initiator、ControlMsg を bincode で送受信
//! - UDP 映像チャネル: 暗号化 VideoPacket を受信、フレーム再構成して Channel<Response> でフロントへ
//!
//! セッションは `session_id` をキーに `SessionState` (managed state) の HashMap で多重保持する。
//! 1 capture ウィンドウ = 1 セッション。ウィンドウ生成 (`open_capture_window`) も Rust 側で行う。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aurora_common::clipboard::{self, ClipboardHandle};
use aurora_common::framing::{
    AudioPacket, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags, decode_ctrl_msg,
    read_tcp_frame, send_ctrl_msg, write_tcp_frame,
};
use aurora_common::noise::{
    HandshakeState, NONCE_DOMAIN_AUDIO, NONCE_DOMAIN_MASK, NONCE_DOMAIN_VIDEO, TransportSession,
    device_fingerprint, psk_from_password,
};
use aurora_common::proto::{ControlMsg, Display, FsEntry, InputEvent, VideoCodec};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::audio::{self, AudioPlayer};
use crate::stats::{StatsCollector, StatsWindow};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket, tcp::OwnedReadHalf, tcp::OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;

// クライアント net の実装を関心事ごとに分割したサブモジュール群。mod.rs は共有型
// (ClientEvent/Session*/Sink) を保持し、以下へ各関心事を委譲する (TCP 制御フレームの
// 送受信は host と共通の `aurora_common::framing`)。
mod commands;
mod connect;
mod fileshare;
mod probe;
mod video;
mod window;
mod wol;

// main.rs の invoke_handler が `net::<コマンド名>` で参照する tauri コマンドを再エクスポート。
pub use commands::*;
pub use connect::*;
pub use probe::*;
pub use window::*;
pub use wol::*;
// net モジュール内でサブモジュール間参照する型/関数 (`use super::*` で各ファイルへ供給)。
pub(crate) use connect::send_ctrl;
pub(crate) use fileshare::{
    DownloadState, ShareScope, fs_create, fs_delete, fs_list, fs_read, fs_rename, fs_write,
    open_download,
};
pub(crate) use video::{make_udp_socket, udp_reader_loop};
pub(crate) use window::emit_connections;

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// 接続確立 (TCP connect + Noise ハンドシェイク + 最初の HostInfo) の各 I/O 上限。
/// host 側 `HANDSHAKE_TIMEOUT` と対。無応答ホストでの無限ハングを防ぐ。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// フロント (TS) に流す制御イベント。タグ付き enum。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientEvent {
    Connected,
    HostInfo {
        // proto::Display をそのまま流す (フィールドが snake_case で TS 側の期待と一致するため
        // DTO への詰め替えは不要。詰め替えを挟むと proto 側のフィールド追加が黙って落ちる)。
        displays: Vec<Display>,
        audio: bool,
    },
    /// ホストが解像度を変更した (SetClientViewport / ディスプレイ切替の結果)。
    /// クライアントは入力スケールとデコーダを新解像度へ更新する。
    DisplayResized {
        id: u32,
        width: u32,
        height: u32,
    },
    Pong {
        rtt_us: i64,
    },
    Stats {
        bitrate_kbps: u32,
        fps: u8,
        rtt_us: u32,
        loss_permille: u16,
    },
    FileResult {
        id: u32,
        ok: bool,
        message: String,
    },
    /// ファイル転送 (アップロード/ダウンロード) の進捗。転送 id ごとに 100ms 以上間隔を空けて
    /// emit する (毎チャンク emit すると Tauri IPC が高頻度になり映像の遅延予算を食うため)。
    FileProgress {
        id: u32,
        done: u64,
        total: u64,
    },
    Disconnected {
        reason: String,
    },
    /// ホストが確定/変更した送出コーデック ("h264" | "hevc" | "av1")。
    VideoCodecChanged {
        codec: String,
    },
}

/// TS の `["h264","hevc","av1"]` のような小文字文字列を `VideoCodec` へ変換する
/// (未知の値は無視)。変換後に空 (無指定/空配列/全て未知) なら `[H264]` を既定にする
/// (host 側の「未申告 = H264」規約と揃える)。
pub(crate) fn parse_supported_codecs(codecs: &[String]) -> Vec<VideoCodec> {
    let mut out: Vec<VideoCodec> = codecs
        .iter()
        .filter_map(|s| match s.as_str() {
            "h264" => Some(VideoCodec::H264),
            "hevc" => Some(VideoCodec::Hevc),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        })
        .collect();
    if out.is_empty() {
        out.push(VideoCodec::H264);
    }
    out
}

/// `VideoCodec` → フロントへ渡す小文字文字列。
pub(crate) fn codec_to_str(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

/// 差し替え可能な表示先 (sink)。セッションの networking (TCP/UDP/各タスク) はそのままに、
/// 映像/イベントをどの webview の `Channel` へ流すかを実行時に差し替えられるようにする。
/// これがタブのウィンドウ間移動 (Phase 3 ドラッグ分割) の土台:
/// 旧ウィンドウで `clear`、新ウィンドウで `set` することで、ネットワークを止めずに表示先だけ移す。
///
/// send は短時間で await を跨がないため同期 `std::sync::Mutex` で足りる (tokio タスクから
/// 呼ぶが、ロック保持中に await しない)。`Channel` 自体が Arc 内部の安価な複製。
#[derive(Clone, Default)]
pub struct Sink {
    event: Arc<std::sync::Mutex<Option<Channel<ClientEvent>>>>,
    video: Arc<std::sync::Mutex<Option<Channel<InvokeResponseBody>>>>,
}

impl Sink {
    fn new(event: Channel<ClientEvent>, video: Channel<InvokeResponseBody>) -> Self {
        Self {
            event: Arc::new(std::sync::Mutex::new(Some(event))),
            video: Arc::new(std::sync::Mutex::new(Some(video))),
        }
    }
    /// 現在の表示先へイベントを送る (未アタッチ = デタッチ中なら捨てる)。
    fn send_event(&self, ev: ClientEvent) {
        if let Ok(g) = self.event.lock()
            && let Some(ch) = g.as_ref()
        {
            let _ = ch.send(ev);
        }
    }
    /// 現在の表示先へ映像フレームを送る (未アタッチなら捨てる。次キーフレームで回復)。
    fn send_video(&self, body: InvokeResponseBody) {
        if let Ok(g) = self.video.lock()
            && let Some(ch) = g.as_ref()
        {
            let _ = ch.send(body);
        }
    }
    /// 表示先を差し替える (新ウィンドウのアタッチ)。
    fn set(&self, event: Channel<ClientEvent>, video: Channel<InvokeResponseBody>) {
        if let Ok(mut g) = self.event.lock() {
            *g = Some(event);
        }
        if let Ok(mut g) = self.video.lock() {
            *g = Some(video);
        }
    }
    /// 表示先を外す (旧ウィンドウのデタッチ)。ネットワークは継続する。
    fn clear(&self) {
        if let Ok(mut g) = self.event.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.video.lock() {
            *g = None;
        }
    }
}

/// セッションごとに保持するハンドル。
pub struct SessionHandle {
    /// 差し替え可能な表示先 (映像/イベント Channel)。タブのウィンドウ間移動で attach/detach する。
    pub sink: Sink,
    /// 制御メッセージを書き込み側タスクに投げる
    pub ctrl_tx: mpsc::UnboundedSender<ControlMsg>,
    /// ファイル転送チャンク専用の bounded チャネル。満杯時に送信が待つことで
    /// JS 側の読み込みにバックプレッシャがかかり、入力など他の制御を詰まらせない。
    pub file_tx: mpsc::Sender<ControlMsg>,
    /// ダウンロード要求済み (= ユーザーが明示的に取得を依頼した) 転送 id の集合。
    /// reader ループはこの集合にある id の FileOffer だけを受理し、未要求のオファー
    /// (悪意あるホストが押し付けるファイル) を拒否する。`request_file` で追加し、
    /// reader が転送の終端で除去する。同期 Mutex だが await を跨いで保持しない。
    pub pending_downloads: Arc<std::sync::Mutex<HashSet<u32>>>,
    /// 読み書き両タスクをまとめて止める
    pub tasks: Vec<JoinHandle<()>>,
    /// クリップボード同期ハンドル
    pub clipboard: ClipboardHandle,
    /// 音声再生 (ホストが音声非対応なら None)。保持目的は RAII: SessionHandle の drop で
    /// 再生スレッドへ EOF が伝わり停止する。直接読み出すことはない。
    #[allow(dead_code)]
    pub audio: Option<AudioPlayer>,
    /// このセッションのファイル共有スコープ。`set_file_sharing` で設定する。
    /// **既定は `Disabled` (共有無効)。** 有効時は This PC 全体 (全ドライブ) をホストの仮想
    /// ドライブへ投影する。reader タスクと共有するため Arc。セッション毎に独立させ、ある
    /// ウィンドウの共有設定が別ウィンドウの接続に漏れないようにする。
    pub share_scope: Arc<Mutex<ShareScope>>,
    /// 接続先 host (ランチャーの接続中表示 / 切断ボタンで host 単位に扱うために保持)。
    pub host: String,
    /// このセッションを表示している capture ウィンドウのラベル (`connect_to_host` を呼んだ
    /// webview)。1 ウィンドウに複数タブ (セッション) がぶら下がるため、ウィンドウ破棄時に
    /// このラベルを持つ全セッションをまとめて停止する (タブの取りこぼし防止)。
    pub window_label: String,
    /// UDP 映像/音声受信の自己修復 (途絶検知 + AURHELLO 再送) に使う共有状態。
    /// `set_video_paused` が pause フラグを更新し、udp watchdog タスクが無受信を見張る。
    pub udp_health: Arc<UdpHealth>,
}

/// UDP 映像/音声受信の自己修復に使う共有状態。TCP は生きている (Pong 到達中) のに UDP だけ
/// 途絶するケース (NAT の UDP マッピング切替等) を検知し、AURHELLO 再送 + キーフレーム要求で
/// 回復を試みる。非アクティブタブの意図的な一時停止 (`SetVideoPaused`) 中は無受信が正常なので、
/// `paused` が立っている間は途絶とみなさない。
pub(crate) struct UdpHealth {
    /// 最後に映像/音声パケットを受信した時刻 (session_start からの経過 ms)。
    last_recv_ms: AtomicU64,
    /// クライアントが映像を pause 中 (非アクティブタブ) か。
    paused: std::sync::atomic::AtomicBool,
}

impl UdpHealth {
    fn new() -> Self {
        Self {
            last_recv_ms: AtomicU64::new(0),
            paused: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// 映像/音声パケットを 1 個受信した (session_start からの経過 ms で記録)。
    pub(crate) fn mark_recv(&self, now_ms: u64) {
        self.last_recv_ms.store(now_ms, Ordering::Relaxed);
    }
    /// 最後の受信からの経過 (ms)。
    fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_recv_ms.load(Ordering::Relaxed))
    }
    pub(crate) fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl SessionHandle {
    /// セッションを停止する: クリップボードスレッドへ shutdown 通知 + tokio タスク abort。
    /// `audio` は self の drop で再生スレッドへ EOF が伝わり停止する。
    pub fn shutdown(mut self) {
        self.clipboard.shutdown();
        for t in std::mem::take(&mut self.tasks) {
            t.abort();
        }
    }
}

#[derive(Default)]
pub struct SessionState {
    /// session_id → 稼働中セッション。1 capture ウィンドウ = 1 セッション。
    pub inner: Mutex<HashMap<String, SessionHandle>>,
    /// `open_capture_window` が一時的に積む接続資格情報 (session_id → password)。
    /// `connect_to_host` が pop して使う。URL に平文パスワードを載せないための受け渡し。
    /// 値 `None` は「保存済みパスワード (DPAPI) を使う」を意味する。
    pub staging: Mutex<HashMap<String, Option<String>>>,
    /// タブのウィンドウ分割 (Phase 3) の引き継ぎデータ。`open_session_window` が移譲元の
    /// セッション表示状態 (JSON) を session_id をキーに積み、移譲先ウィンドウの JS が
    /// `take_handoff` で取り出して `Session` を復元する。networking は継続したまま表示先だけ移す。
    pub adopt: Mutex<HashMap<String, String>>,
    /// 最後にフォーカスされた capture ウィンドウのラベル。launcher からの接続を
    /// 既存窓のタブへルーティングするために使う (同期ハンドラから触るので std::sync::Mutex)。
    pub last_focused_capture: std::sync::Mutex<Option<String>>,
}

/// セッション id の発番。プロセス生存期間で一意ならよいので単調増加カウンタで足りる。
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// クエリ文字列値の最小 percent-encode (unreserved 以外を %XX に)。URL に host 等を
/// 載せるとき IPv6 のコロンや空白で壊れないようにする。JS 側は URLSearchParams が復号する。
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}
