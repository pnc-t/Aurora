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
    AudioPacket, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
};
use aurora_common::noise::{
    HandshakeState, NONCE_DOMAIN_AUDIO, NONCE_DOMAIN_CTRL, NONCE_DOMAIN_MASK, NONCE_DOMAIN_VIDEO,
    TransportSession, psk_from_password,
};
use aurora_common::proto::{ControlMsg, Display, FsEntry, InputEvent};
use bincode::Options;
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

const MAX_TCP_FRAME: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// 接続確立 (TCP connect + Noise ハンドシェイク + 最初の HostInfo) の各 I/O 上限。
/// host 側 `HANDSHAKE_TIMEOUT` と対。無応答ホストでの無限ハングを防ぐ。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// ハンドシェイク I/O を `CONNECT_TIMEOUT` で包む。タイムアウト/失敗を String に正規化する。
async fn hs_io(fut: impl std::future::Future<Output = anyhow::Result<()>>) -> Result<(), String> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(err)
}

/// フロント (TS) に流す制御イベント。タグ付き enum。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientEvent {
    Connected,
    HostInfo {
        displays: Vec<DisplayDto>,
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
    Disconnected {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayDto {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
    pub is_virtual: bool,
}

impl From<Display> for DisplayDto {
    fn from(d: Display) -> Self {
        Self {
            id: d.id,
            width: d.width,
            height: d.height,
            name: d.name,
            is_virtual: d.is_virtual,
        }
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
        if let Ok(g) = self.event.lock() {
            if let Some(ch) = g.as_ref() {
                let _ = ch.send(ev);
            }
        }
    }
    /// 現在の表示先へ映像フレームを送る (未アタッチなら捨てる。次キーフレームで回復)。
    fn send_video(&self, body: InvokeResponseBody) {
        if let Ok(g) = self.video.lock() {
            if let Some(ch) = g.as_ref() {
                let _ = ch.send(body);
            }
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

/// capture ウィンドウを Rust 側で生成する (JS にウィンドウ生成権限を与えない)。
/// 接続資格情報 (password) は staging マップへ積み、URL には session/host/port/save のみ
/// 載せる (平文をウィンドウ URL に出さない)。生成した session_id を返す。
#[tauri::command]
pub async fn open_capture_window(
    app: tauri::AppHandle,
    host: String,
    port: u16,
    // タブの表示名 (空なら capture 側で host にフォールバック)。
    label: String,
    save: bool,
    // 手入力パスワード (今回だけ/保存) は Some、保存済み即接続/再接続は None。
    password: Option<String>,
    state: tauri::State<'_, SessionState>,
) -> Result<String, String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("s{n}");
    let win_label = format!("capture-{session_id}");
    // パスワードを staging へ積んでから window を作る (connect_to_host が pop する)。
    state
        .staging
        .lock()
        .await
        .insert(session_id.clone(), password);
    let url = format!(
        "capture.html?session={}&host={}&port={port}&label={}&save={}",
        q(&session_id),
        q(&host),
        q(&label),
        if save { 1 } else { 0 },
    );
    let build = WebviewWindowBuilder::new(&app, &win_label, WebviewUrl::App(url.into()))
        .title("Aurora")
        .inner_size(1280.0, 800.0)
        .resizable(true)
        .decorations(false)
        .build();
    if let Err(e) = build {
        // 生成に失敗したら積んだ資格情報を残さない。
        state.staging.lock().await.remove(&session_id);
        return Err(format!("open capture window: {e}"));
    }
    Ok(session_id)
}

/// 既存の capture ウィンドウ内で新しいタブ (セッション) を開く準備をする。ウィンドウは
/// 作らず、session_id を発番して接続資格情報 (password) を staging へ積むだけ。呼び出した
/// capture ウィンドウの JS が、返った id で `connect_to_host` を呼んで実接続する。
/// 平文パスワードを JS のウィンドウ間でやり取りしないための受け渡し (open_capture_window と対)。
#[tauri::command]
pub async fn stage_session(
    // 手入力パスワード (今回だけ/保存) は Some、保存済み即接続は None。
    password: Option<String>,
    state: tauri::State<'_, SessionState>,
) -> Result<String, String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("s{n}");
    state
        .staging
        .lock()
        .await
        .insert(session_id.clone(), password);
    Ok(session_id)
}

/// 既存セッションを別ウィンドウへ移譲するための空ウィンドウを開く (Phase 3 ドラッグ分割)。
/// networking は止めず、表示先 (sink) を即デタッチして所有ウィンドウを生成先の新ラベルへ
/// 付け替える。これを **ウィンドウ生成より前に** 行うことで、移譲元ウィンドウが直後に
/// (最後のタブが消えて) 閉じても、その Destroyed ハンドラが移譲中セッションを巻き込んで
/// 停止しない (window_label が既に新ラベルになっている)。`snapshot` は移譲先で `take_handoff`
/// が取り出す表示状態 JSON。`x`/`y` は新ウィンドウの配置 (カーソル位置、論理 px)。
#[tauri::command]
pub async fn open_session_window(
    app: tauri::AppHandle,
    session_id: String,
    snapshot: String,
    x: Option<f64>,
    y: Option<f64>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let label = format!("capture-move{n}");
    // 所有権 (window_label) を新ラベルへ移し、表示先を外す。移譲元が閉じても巻き込まれない。
    {
        let mut guard = state.inner.lock().await;
        let s = guard.get_mut(&session_id).ok_or("unknown session")?;
        s.window_label = label.clone();
        s.sink.clear();
    }
    state
        .adopt
        .lock()
        .await
        .insert(session_id.clone(), snapshot);
    let url = format!("capture.html?adopt={}", q(&session_id));
    let mut build = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title("Aurora")
        .inner_size(1280.0, 800.0)
        .resizable(true)
        .decorations(false);
    if let (Some(x), Some(y)) = (x, y) {
        build = build.position(x, y);
    }
    if let Err(e) = build.build() {
        // 生成失敗時は積んだ引き継ぎデータを残さない。セッションは sink デタッチ済みのまま
        // 残る (元ウィンドウは既にタブを外している可能性があるため、ここでは停止しない)。
        state.adopt.lock().await.remove(&session_id);
        return Err(format!("open session window: {e}"));
    }
    Ok(())
}

/// 移譲先ウィンドウが引き継ぎデータ (表示状態 JSON) を取り出す。1 回限り (pop)。
#[tauri::command]
pub async fn take_handoff(
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<Option<String>, String> {
    Ok(state.adopt.lock().await.remove(&session_id))
}

/// 既存セッションの表示先 (映像/イベント Channel) を、この呼び出し元 webview のものへ
/// 差し替える (Phase 3 移譲先がアタッチ)。所有ウィンドウもこの webview へ付け替える。
/// networking はそのままなので、移譲先は別途 `request_keyframe` で映像を復帰させる。
#[tauri::command]
pub async fn attach_session(
    window: WebviewWindow,
    session_id: String,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let mut guard = state.inner.lock().await;
    let s = guard.get_mut(&session_id).ok_or("unknown session")?;
    s.sink.set(event_channel, video_channel);
    s.window_label = window.label().to_string();
    Ok(())
}

/// このセッションのファイル共有を有効/無効にする。有効時は This PC 全体 (全ドライブ) を
/// ホストの仮想ドライブへ公開する。設定 (launcher) のトグルに対応し、接続成立時に session が呼ぶ。
#[tauri::command]
pub async fn set_file_sharing(
    session_id: String,
    enabled: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let scope = {
        let guard = state.inner.lock().await;
        let s = guard.get(&session_id).ok_or("unknown session")?;
        s.share_scope.clone()
    };
    *scope.lock().await = if enabled {
        ShareScope::WholePc
    } else {
        ShareScope::Disabled
    };
    tracing::info!(session = %session_id, enabled, "file sharing set");
    Ok(())
}

/// 接続中セッションの host を重複排除して返す。
fn distinct_hosts(map: &HashMap<String, SessionHandle>) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for h in map.values() {
        set.insert(h.host.clone());
    }
    set.into_iter().collect()
}

/// 現在接続中の host 一覧を全ウィンドウへ通知する (`aurora://connections`)。ランチャーが
/// これを購読して接続中表示・切断ボタンの出し分け・保存バッジの再読込を行う。セッションの
/// 追加 (connect) / 除去 (disconnect・ウィンドウ破棄) のたびに呼ぶ。
pub(crate) async fn emit_connections(app: &tauri::AppHandle, state: &SessionState) {
    let hosts = distinct_hosts(&*state.inner.lock().await);
    let _ = app.emit("aurora://connections", hosts);
}

/// ランチャー初期表示用: 現在接続中の host 一覧 (重複排除)。
#[tauri::command]
pub async fn connected_hosts(state: tauri::State<'_, SessionState>) -> Result<Vec<String>, String> {
    Ok(distinct_hosts(&*state.inner.lock().await))
}

/// 指定セッションの制御チャネルへ 1 メッセージ送る共通処理。未知 id は拒否する。
async fn send_ctrl(state: &SessionState, session_id: &str, msg: ControlMsg) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(session_id).ok_or("unknown session")?;
    s.ctrl_tx
        .send(msg)
        .map_err(|_| "ctrl channel closed".to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
// Tauri コマンドの引数 (state/channel) 注入で増える
// session_id を span で全ログへ付ける。host 側 conn_id と peer/時刻で突き合わせて 1 接続を
// 追跡できる。後段で spawn する I/O タスクにも同じ span を継承させる (Span::current)。
// 失敗時はここで 1 度だけ warn に残す。実体 (connect_inner) は各 `?` で String を早期 return
// するため、そのままでは接続失敗 (誤パスワード・タイムアウト等) が client.log に出ない。
#[tracing::instrument(skip_all, fields(session = %session_id, host = %host, port = port))]
pub async fn connect_to_host(
    app: tauri::AppHandle,
    window: WebviewWindow,
    session_id: String,
    host: String,
    port: u16,
    save: bool,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let r = connect_inner(
        app,
        window,
        session_id,
        host,
        port,
        save,
        event_channel,
        video_channel,
        state,
    )
    .await;
    if let Err(e) = &r {
        tracing::warn!(error = %e, "connect failed");
    }
    r
}

#[allow(clippy::too_many_arguments)]
async fn connect_inner(
    app: tauri::AppHandle,
    // このコマンドを呼んだ capture ウィンドウ。セッションを所有ウィンドウへ紐付け、
    // ウィンドウ破棄時にそのウィンドウの全タブをまとめて停止できるようにする。
    window: WebviewWindow,
    session_id: String,
    host: String,
    port: u16,
    // パスワードを DPAPI ストアへ保存してよいか (接続ダイアログの「保存する」由来)。
    // 手入力かつ true のときだけ保存し、「今回だけ接続」を可能にする。
    // 保存済み即接続/自動再接続では false 相当で、既存値に触れない。
    save: bool,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    // 接続資格情報は staging マップ (open_capture_window が積む) から取り出す。平文を
    // ウィンドウ URL に載せないための受け渡し。再接続は staging が空なので None 扱いになり、
    // 下の分岐で保存済みパスワード (DPAPI) を使う。
    let staged = state.staging.lock().await.remove(&session_id);
    let password = match staged.flatten() {
        Some(p) => p,
        None => crate::secret::get(&app, &host)
            .ok_or_else(|| "保存済みパスワードがありません".to_string())?,
    };
    // 同 id の既存セッションがあれば閉じる (再接続・置換)。
    {
        let mut guard = state.inner.lock().await;
        if let Some(s) = guard.remove(&session_id) {
            s.shutdown();
        }
    }

    // ホスト名 (Tailscale MagicDNS 名等) も解決する。SocketAddr::parse は数値 IP しか
    // 受け付けないため、lookup_host で名前解決し IPv4/IPv6 の最初のアドレスを採る。
    let target = format!("{host}:{port}");
    let tcp_addr: SocketAddr = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| format!("resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}"))?;
    let udp_addr = tcp_addr;

    tracing::info!(%tcp_addr, "connecting");
    let mut tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(tcp_addr))
        .await
        .map_err(|_| "TCP connect timed out".to_string())?
        .map_err(|e| format!("TCP connect: {e}"))?;
    tcp.set_nodelay(true).ok();

    let psk = psk_from_password(&password);

    // この端末の永続 ID 鍵を読み込む (無ければ生成保存)。host のデバイスピン留めで端末を
    // 識別できるよう、毎回同じ静的公開鍵で接続する。取得失敗時は None=従来の使い捨て鍵。
    let identity = crate::secret::load_or_create_identity(&app);
    let identity_priv = identity.as_ref().map(|id| id.private.as_slice());

    // --- Noise XX-PSK initiator ---
    // ハンドシェイク I/O には全てタイムアウトを設ける。これが無いと、ポートは開いているが
    // 応答しないホスト (別アプリがポートを掴んでいる等) に対して connect_to_host が永久に
    // 返らず、UI が「connecting…」のままキャンセル不能になる (host 側の HANDSHAKE_TIMEOUT と対)。
    let mut hs = HandshakeState::new_initiator(&psk, identity_priv).map_err(err)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();
    // -> e
    let mut out_msg = Vec::new();
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    hs_io(write_tcp_frame(&mut tcp, &out_msg)).await?;
    // <- e, ee, s, es
    hs_io(read_tcp_frame(&mut tcp, &mut buf)).await?;
    hs.read_msg(&buf, &mut tmp).map_err(err)?;
    // -> s, se, psk
    hs.write_msg(&[], &mut out_msg).map_err(err)?;
    hs_io(write_tcp_frame(&mut tcp, &out_msg)).await?;

    // snow の StatelessTransportState は encrypt/decrypt とも &self なので、
    // 複数タスク (writer/reader/udp) 間の共有は Arc だけで足り Mutex は不要。
    // 映像 UDP のパケット毎 decrypt が TCP 制御とロック競合しなくなる。
    let session = Arc::new(hs.into_transport().map_err(err)?);
    tracing::info!("noise handshake complete");

    // ハンドシェイク成功 = PSK (パスワード) 一致。ユーザーが「保存する」を選んだときだけ
    // DPAPI 暗号化保存する (無断永続化を避け「今回だけ接続」を可能にする)。失敗してもログ
    // のみで接続は続行する (保存は補助機能)。`save=false` (今回だけ/保存済み即接続/再接続) は
    // 既存の保存値に触れない (= 保存済みなら維持、未保存なら未保存のまま)。
    if save {
        if let Err(e) = crate::secret::put(&app, &host, &password) {
            tracing::warn!(error = %e, "failed to save host password");
        }
    }

    // 最初の制御メッセージ (HostInfo) を受け取る。CTRL ドメインの nonce 0 で復号し、
    // 以降の TCP 受信は reader ループが nonce 1 から続ける。
    hs_io(read_tcp_frame(&mut tcp, &mut buf)).await?;
    let mut plain = Vec::new();
    session
        .decrypt(NONCE_DOMAIN_CTRL, &buf, &mut plain)
        .map_err(err)?;
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    let host_info: ControlMsg = opts.deserialize(&plain).map_err(err)?;

    // 映像/イベントの表示先を差し替え可能 sink にまとめる。以降タスクへはこの sink を
    // clone して渡し、タブのウィンドウ移動 (attach/detach) で表示先だけ付け替えられるようにする。
    let sink = Sink::new(event_channel, video_channel);

    sink.send_event(ClientEvent::Connected);
    // HostInfo の中身を取り出すだけにし、フロントへの通知はセッションハンドルを管理状態へ
    // 格納した後に行う (関数末尾)。先に通知すると、フロントが host_info を受けて即
    // selectDisplay/setClientViewport を invoke した時にハンドル未格納 (再接続時は古い閉じた
    // ctrl_tx) で送信が失敗し、仮想ディスプレイのテイクオーバーが起動しないレースになる。
    let (host_displays, host_audio) = match host_info {
        ControlMsg::HostInfo { displays, audio } => (displays, audio),
        other => return Err(format!("expected HostInfo, got {other:?}")),
    };

    // UDP ソケットをローカル任意ポートで開き、サーバへ向けて "AURHELLO"。
    // 高 fps 時のバースト受信での取りこぼし (= 映像ノイズ) を抑えるため受信バッファを拡大する。
    let udp =
        Arc::new(make_udp_socket(udp_addr.is_ipv6()).map_err(|e| format!("UDP socket: {e}"))?);
    udp.connect(udp_addr)
        .await
        .map_err(|e| format!("UDP connect: {e}"))?;
    udp.send(b"AURHELLO")
        .await
        .map_err(|e| format!("UDP hello: {e}"))?;

    let (read_half, write_half) = tcp.into_split();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let (file_tx, file_rx) = mpsc::channel::<ControlMsg>(32);

    // クリップボード同期 (このセッション専用に起動、disconnect で shutdown)
    let (clip_handle, mut clip_change_rx) = clipboard::spawn();
    let clip_apply_tx = clip_handle.apply_tx.clone();

    // 受信品質メトリクス。Ping の基準時刻と共有して RTT を算出する。
    let stats = StatsCollector::new();
    let session_start = Instant::now();

    let sink_w = sink.clone();
    let sess_w = session.clone();
    let writer_task = tokio::spawn(
        async move {
            let _ = tcp_writer_loop(write_half, sess_w, ctrl_rx, file_rx).await;
            sink_w.send_event(ClientEvent::Disconnected {
                reason: "writer closed".into(),
            });
        }
        .instrument(tracing::Span::current()),
    );

    let sink_r = sink.clone();
    let sess_r = session.clone();
    let clip_apply_for_reader = clip_apply_tx.clone();
    let stats_for_reader = stats.clone();
    // FS RPC 応答は制御チャネル経由で writer ループへ流す (host の同期コールバックを起こす)。
    let fs_reply_tx = ctrl_tx.clone();
    // このセッションのファイル共有スコープ (既定 Disabled = 無効)。reader と SessionHandle で共有。
    let share_scope: Arc<Mutex<ShareScope>> = Arc::new(Mutex::new(ShareScope::Disabled));
    let share_scope_for_reader = share_scope.clone();
    // ダウンロード要求済み id の集合。request_file が追加し、reader が未要求の FileOffer を弾く。
    let pending_downloads = Arc::new(std::sync::Mutex::new(HashSet::<u32>::new()));
    let pending_for_reader = pending_downloads.clone();
    let reader_task = tokio::spawn(
        async move {
            let res = tcp_reader_loop(
                read_half,
                sess_r,
                sink_r.clone(),
                clip_apply_for_reader,
                stats_for_reader,
                session_start,
                fs_reply_tx,
                share_scope_for_reader,
                pending_for_reader,
            )
            .await;
            let reason = match res {
                Ok(()) => "server closed".to_string(),
                Err(e) => format!("read error: {e}"),
            };
            tracing::info!(%reason, "session closed (tcp reader)");
            sink_r.send_event(ClientEvent::Disconnected { reason });
        }
        .instrument(tracing::Span::current()),
    );

    // 音声再生 (ベストエフォート)。失敗しても映像のみで継続する。
    let audio_player = match audio::spawn() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "audio playback disabled");
            None
        }
    };
    let audio_tx = audio_player.as_ref().map(|p| p.sender());

    let sess_u = session.clone();
    let stats_for_udp = stats.clone();
    let sink_u = sink.clone();
    // ロス検出時にキーフレームを要求して映像を即時回復させるための送信口。
    let keyframe_tx = ctrl_tx.clone();
    let udp_task = tokio::spawn(
        async move {
            if let Err(e) =
                udp_reader_loop(udp, sess_u, sink_u, audio_tx, stats_for_udp, keyframe_tx).await
            {
                tracing::warn!(error = %e, "UDP reader ended");
            }
        }
        .instrument(tracing::Span::current()),
    );

    // クリップボード変化 → ControlMsg::Clipboard 送信
    let ctrl_for_clip = ctrl_tx.clone();
    let clip_forward_task = tokio::spawn(async move {
        while let Some(text) = clip_change_rx.recv().await {
            if ctrl_for_clip.send(ControlMsg::Clipboard { text }).is_err() {
                break;
            }
        }
    });

    // ping ループ (1 秒)。RTT を細かく取り、適応制御の応答性を上げる。
    let ctrl_for_ping = ctrl_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let ts_us = session_start.elapsed().as_micros() as u64;
            if ctrl_for_ping.send(ControlMsg::Ping { ts_us }).is_err() {
                break;
            }
        }
    });

    // 統計タスク (1 秒窓)。計測結果をホスト (適応制御用) とフロント (表示用) に送る。
    let ctrl_for_stats = ctrl_tx.clone();
    let sink_s = sink.clone();
    let stats_for_task = stats.clone();
    let stats_task = tokio::spawn(async move {
        let mut window = StatsWindow::new(&stats_for_task);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let s = window.sample(&stats_for_task);
            let _ = ctrl_for_stats.send(ControlMsg::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
            sink_s.send_event(ClientEvent::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
        }
    });

    let handle = SessionHandle {
        sink: sink.clone(),
        host: host.clone(),
        ctrl_tx,
        file_tx,
        tasks: vec![
            writer_task,
            reader_task,
            udp_task,
            ping_task,
            stats_task,
            clip_forward_task,
        ],
        clipboard: clip_handle,
        audio: audio_player,
        pending_downloads,
        share_scope,
        window_label: window.label().to_string(),
    };
    {
        let mut guard = state.inner.lock().await;
        // 所有ウィンドウがハンドシェイク中に閉じられていたら、孤児セッション (どの窓からも
        // 停止できず単一クライアントの host を占有し続け、以降の接続を全て塞ぐ) を作らず即停止
        // する。ウィンドウ破棄ハンドラと inner ロックで直列化し取りこぼしを防ぐ。
        if app.get_webview_window(&handle.window_label).is_none() {
            drop(guard);
            handle.shutdown();
            return Err("ウィンドウが閉じられました".to_string());
        }
        guard.insert(session_id.clone(), handle);
    }

    // ハンドル格納後に HostInfo を通知する (上部のレース回避)。これでフロントが host_info を
    // 受けて即 selectDisplay/setClientViewport を呼んでも ctrl_tx に確実に届く。
    sink.send_event(ClientEvent::HostInfo {
        displays: host_displays.into_iter().map(Into::into).collect(),
        audio: host_audio,
    });
    // ランチャーへ接続中 host 一覧を通知 (接続中表示・保存バッジ更新)。
    emit_connections(&app, &state).await;
    // 接続確立完了 (host 側「client connected」と対)。span に session/host/port が乗る。
    tracing::info!("connected");
    Ok(())
}

#[tauri::command]
pub async fn disconnect(
    app: tauri::AppHandle,
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    {
        let mut guard = state.inner.lock().await;
        if let Some(s) = guard.remove(&session_id) {
            tracing::info!(session = %session_id, host = %s.host, "disconnected (by user)");
            // task abort だけでなくクリップボードスレッドにも shutdown を伝える
            // (connect_to_host のセッション置換経路と同じ後始末に統一)。
            s.shutdown();
        }
    }
    // ランチャーへ接続中 host 一覧を通知 (切断表示・接続中バッジ除去)。
    emit_connections(&app, &state).await;
    Ok(())
}

/// 指定 host の全セッションを停止する (ランチャーの切断ボタン)。session_id ではなく host で
/// 引くため、ウィンドウが既に閉じた後に残った孤児セッション (接続成立中にウィンドウを閉じる等)
/// もここで確実に止められる。capture ウィンドウの JS 経由 (`aurora://disconnect-host`) では
/// 窓が無い孤児に届かないため、Rust 側で直接掃く経路を用意する。
#[tauri::command]
pub async fn disconnect_host(
    app: tauri::AppHandle,
    host: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let target = host.trim().to_lowercase();
    {
        let mut guard = state.inner.lock().await;
        let ids: Vec<String> = guard
            .iter()
            .filter(|(_, h)| h.host.trim().to_lowercase() == target)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(h) = guard.remove(&id) {
                h.shutdown();
            }
        }
    }
    emit_connections(&app, &state).await;
    Ok(())
}

#[tauri::command]
pub async fn send_input(
    session_id: String,
    event: InputEvent,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::Input(event)).await
}

#[tauri::command]
pub async fn request_keyframe(
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::RequestKeyframe).await
}

/// キャプチャ対象ディスプレイの切替を要求する。
#[tauri::command]
pub async fn select_display(
    session_id: String,
    id: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::SelectDisplay { id }).await
}

/// 映像パラメータ (fps 上限 / ビットレート上限) を実行時にホストへ送る。
#[tauri::command]
pub async fn set_video_params(
    session_id: String,
    fps: u32,
    max_bitrate_bps: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetVideoParams {
            fps,
            max_bitrate_bps,
        },
    )
    .await
}

/// 音声ミュートの切替をホストへ送る (ミュート中はホストが音声を送らない)。
#[tauri::command]
pub async fn set_audio_muted(
    session_id: String,
    muted: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::SetAudioMuted { muted }).await
}

/// 映像送出の一時停止 / 再開をホストへ送る (非アクティブタブの帯域を止める)。
/// 再開時は呼び出し側が別途 `request_keyframe` で映像を復帰させる。
#[tauri::command]
pub async fn set_video_paused(
    session_id: String,
    paused: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::SetVideoPaused { paused }).await
}

/// 映像へのカーソル合成 ON/OFF をホストへ送る (マウスキャプチャ中だけ ON にする)。
#[tauri::command]
pub async fn set_cursor_overlay(
    session_id: String,
    enabled: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetCursorOverlay { enabled },
    )
    .await
}

/// 送出する映像の最大高さ (0 = キャプチャ解像度) をホストへ送る。
#[tauri::command]
pub async fn set_resolution(
    session_id: String,
    max_height: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetResolution { max_height },
    )
    .await
}

/// クライアントの映像表示領域サイズ (物理 px) をホストへ送る。ホストはこれに最も近い
/// 対応モードへキャプチャ対象ディスプレイの解像度を変更する (RDP 風・黒帯解消)。
#[tauri::command]
pub async fn set_client_viewport(
    session_id: String,
    width: u32,
    height: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetClientViewport { width, height },
    )
    .await
}

/// ホスト上のファイルのダウンロードを要求する。受信は reader ループが
/// `%USERPROFILE%\Aurora\downloads` に保存し、完了を FileResult イベントで通知する。
#[tauri::command]
pub async fn request_file(
    session_id: String,
    id: u32,
    path: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(&session_id).ok_or("unknown session")?;
    // この id を「要求済み」に登録してから送る。reader はこの集合にある id の FileOffer
    // だけを受理するため、登録は送信より先に行う (オファー受信との競合を避ける)。
    if let Ok(mut pending) = s.pending_downloads.lock() {
        pending.insert(id);
    }
    s.ctrl_tx
        .send(ControlMsg::FileRequest { id, path })
        .map_err(|_| "ctrl channel closed".to_string())
}

/// ローカルのファイルパス (キャプチャ画面への D&D 由来) を host へアップロードする。
/// ファイルは Rust 側で読み、既存のアップロードプロトコル (FileOffer + FileChunk) を
/// bounded file チャネル経由で流す。バイト列を WebView に載せず net 層を再利用する。
/// 完了 / 失敗は host からの FileResult イベントで通知される。
#[tauri::command]
pub async fn upload_file_from_path(
    session_id: String,
    id: u32,
    path: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    const CHUNK: usize = 32 * 1024;
    // file_tx を clone してロックは send の await を跨がない。
    let file_tx = {
        let guard = state.inner.lock().await;
        let s = guard.get(&session_id).ok_or("unknown session")?;
        s.file_tx.clone()
    };
    let name = Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload.bin")
        .to_string();
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("open {path}: {e}"))?;
    // size=0 を誤って告知すると host 側が「告知サイズ超過」で初回チャンクから拒否して
    // しまうため、metadata 失敗はここでエラーにする。
    let size = file
        .metadata()
        .await
        .map(|m| m.len())
        .map_err(|e| format!("metadata {path}: {e}"))?;
    file_tx
        .send(ControlMsg::FileOffer { id, name, size })
        .await
        .map_err(|_| "file channel closed".to_string())?;
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read: {e}"))?;
        let last = n == 0 || offset + n as u64 >= size;
        file_tx
            .send(ControlMsg::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
                last,
            })
            .await
            .map_err(|_| "file channel closed".to_string())?;
        offset += n as u64;
        if last {
            break;
        }
    }
    Ok(())
}

// -------- internal loops --------

async fn tcp_writer_loop(
    mut write_half: OwnedWriteHalf,
    session: Arc<TransportSession>,
    mut ctrl_rx: mpsc::UnboundedReceiver<ControlMsg>,
    mut file_rx: mpsc::Receiver<ControlMsg>,
) -> anyhow::Result<()> {
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    // クライアント → ホストの TCP 制御は CTRL ドメインの連番 nonce。ホスト側 rx と
    // TCP の順序保証により歩調が合う。
    let mut tcp_tx_nonce: u64 = 0;
    loop {
        // 制御チャネルとファイルチャネルの両方を待つ。どちらかが閉じたら終了。
        let msg = tokio::select! {
            m = ctrl_rx.recv() => m,
            m = file_rx.recv() => m,
        };
        let Some(msg) = msg else { break };
        let plain = opts.serialize(&msg)?;
        let nonce = NONCE_DOMAIN_CTRL | tcp_tx_nonce;
        tcp_tx_nonce += 1;
        let mut cipher = Vec::new();
        session.encrypt(nonce, &plain, &mut cipher)?;
        write_tcp_frame(&mut write_half, &cipher).await?;
    }
    Ok(())
}

/// ダウンロード受信状態 (ホスト → クライアント)。1 セッション 1 転送のみ。
struct DownloadState {
    id: u32,
    file: tokio::fs::File,
    written: u64,
    /// FileOffer で告知された総サイズ。これを超える書き込みは中断する
    /// (悪意あるホストが告知より多く送ってディスクを埋めるのを防ぐ)。
    total: u64,
    path: String,
}

/// ダウンロード保存ディレクトリ (`%USERPROFILE%\Aurora\downloads`)。
///
/// インストール版アプリはショートカット起動で CWD が不定 (書き込み不可の場所もある) の
/// ため、CWD 相対では保存に失敗したり場所が予測不能になる。host 側 `upload_dir` と対称に、
/// ユーザープロファイル配下の固定パスへ絶対化して discoverable にする。
/// `USERPROFILE` (非 Windows では `HOME`) が無ければ最後の手段としてカレントディレクトリ。
fn download_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Aurora").join("downloads")
}

/// `download_dir()` 配下に保存先ファイルを作る。ファイル名はパス区切りを除去して安全化する。
async fn open_download(name: &str) -> anyhow::Result<(tokio::fs::File, String)> {
    let dir = download_dir();
    tokio::fs::create_dir_all(&dir).await?;
    // パス区切り・ADS (`:`) を中和し、ファイル名成分のみを採用する (host open_upload と対称)。
    let safe = aurora_common::fsname::sanitize_filename(name, "download.bin");
    let path = dir.join(&safe);
    let file = tokio::fs::File::create(&path).await?;
    Ok((file, path.display().to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn tcp_reader_loop(
    mut read_half: OwnedReadHalf,
    session: Arc<TransportSession>,
    sink: Sink,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    stats: Arc<StatsCollector>,
    session_start: Instant,
    fs_reply_tx: mpsc::UnboundedSender<ControlMsg>,
    share_scope: Arc<Mutex<ShareScope>>,
    pending_downloads: Arc<std::sync::Mutex<HashSet<u32>>>,
) -> anyhow::Result<()> {
    let opts = bincode::DefaultOptions::new().with_limit(MAX_TCP_FRAME as u64);
    let mut buf = Vec::new();
    let mut plain = Vec::new();
    let mut download: Option<DownloadState> = None;
    // HostInfo (nonce 0) は connect_to_host で消費済みなので 1 から続ける。
    let mut tcp_rx_nonce: u64 = 1;
    loop {
        read_tcp_frame(&mut read_half, &mut buf).await?;
        plain.clear();
        let nonce = NONCE_DOMAIN_CTRL | tcp_rx_nonce;
        tcp_rx_nonce += 1;
        session.decrypt(nonce, &buf, &mut plain)?;
        let msg: ControlMsg = opts.deserialize(&plain)?;
        match msg {
            ControlMsg::Pong { ts_us } => {
                // ts_us は Ping 送信時の session_start からの経過。往復差で RTT を得る。
                let now_us = session_start.elapsed().as_micros() as u64;
                let rtt_us = now_us.saturating_sub(ts_us);
                stats.set_rtt_us(rtt_us.min(u32::MAX as u64) as u32);
                sink.send_event(ClientEvent::Pong {
                    rtt_us: rtt_us as i64,
                });
            }
            ControlMsg::HostInfo { displays, audio } => {
                sink.send_event(ClientEvent::HostInfo {
                    displays: displays.into_iter().map(Into::into).collect(),
                    audio,
                });
            }
            ControlMsg::DisplayResized { id, width, height } => {
                sink.send_event(ClientEvent::DisplayResized { id, width, height });
            }
            ControlMsg::Clipboard { text } => {
                if clip_apply_tx.send(text).is_err() {
                    tracing::debug!("clipboard apply channel closed");
                }
            }
            ControlMsg::FileResult { id, ok, message } => {
                sink.send_event(ClientEvent::FileResult { id, ok, message });
            }
            // --- ダウンロード受信 (ホスト → クライアント) ---
            // ユーザーが request_file で明示的に要求した id の FileOffer だけを受理する。
            // 未要求のオファーは、悪意あるホストがファイルを無断で押し付ける経路なので拒否する。
            ControlMsg::FileOffer { id, name, size } => {
                let solicited = pending_downloads
                    .lock()
                    .map(|s| s.contains(&id))
                    .unwrap_or(false);
                if !solicited {
                    tracing::warn!(id, name = %name, "unsolicited FileOffer ignored (no matching request)");
                } else {
                    match open_download(&name).await {
                        Ok((file, path)) => {
                            tracing::info!(id, name = %name, size, path = %path, "download start");
                            download = Some(DownloadState {
                                id,
                                file,
                                written: 0,
                                total: size,
                                path,
                            });
                        }
                        Err(e) => {
                            if let Ok(mut s) = pending_downloads.lock() {
                                s.remove(&id);
                            }
                            sink.send_event(ClientEvent::FileResult {
                                id,
                                ok: false,
                                message: format!("open failed: {e}"),
                            });
                        }
                    }
                }
            }
            ControlMsg::FileChunk {
                id,
                offset,
                data,
                last,
            } => match download.as_mut() {
                Some(d) if d.id == id => {
                    if offset != d.written {
                        // TCP は順序保証があるため offset 不一致は本来起きない。発生したら
                        // データ欠落/重複の可能性が高く、書き込むと破損ファイルを残すため
                        // 中断する (host 側アップロード受信と対称の防御)。
                        tracing::warn!(
                            id,
                            expected = d.written,
                            got = offset,
                            "download offset mismatch, aborting"
                        );
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message: format!(
                                "download offset mismatch: expected {}, got {offset}",
                                d.written
                            ),
                        });
                        download = None;
                    } else if d.written.saturating_add(data.len() as u64) > d.total {
                        // 告知サイズ超過は中断する。悪意あるホストが告知より多く送り、
                        // クライアントのディスクを埋めるのを防ぐ。
                        tracing::warn!(
                            id,
                            total = d.total,
                            "download exceeds declared size, aborting"
                        );
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message: "download exceeds declared size".into(),
                        });
                        download = None;
                    } else if let Err(e) = d.file.write_all(&data).await {
                        let message = format!("write failed: {e}");
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message,
                        });
                        download = None;
                    } else {
                        d.written += data.len() as u64;
                        if last {
                            let (written, path) = (d.written, d.path.clone());
                            let res = d.file.flush().await;
                            download = None;
                            if let Ok(mut s) = pending_downloads.lock() {
                                s.remove(&id);
                            }
                            let ev = match res {
                                Ok(()) => {
                                    tracing::info!(id, written, path = %path, "download complete");
                                    ClientEvent::FileResult {
                                        id,
                                        ok: true,
                                        message: path,
                                    }
                                }
                                Err(e) => ClientEvent::FileResult {
                                    id,
                                    ok: false,
                                    message: format!("flush failed: {e}"),
                                },
                            };
                            sink.send_event(ev);
                        }
                    }
                }
                _ => tracing::warn!(id, "download chunk without matching offer, ignored"),
            },
            // --- ProjFS ドライブリダイレクト: 自分のファイルをホストへ配信 ---
            // 共有スコープが Disabled なら resolve が error を返し、read は error 応答・破壊系は
            // ログのみで無視する (無断アクセス防止)。WholePc なら This PC 全体を投影する。
            ControlMsg::FsList {
                req_id,
                path,
                offset,
            } => {
                let scope = share_scope.lock().await.clone();
                let reply = fs_list(&scope, &path, offset).await;
                let _ = fs_reply_tx.send(reply.into_msg(req_id));
            }
            ControlMsg::FsRead {
                req_id,
                path,
                offset,
                length,
            } => {
                let scope = share_scope.lock().await.clone();
                let (data, error) = fs_read(&scope, &path, offset, length).await;
                let _ = fs_reply_tx.send(ControlMsg::FsReadReply {
                    req_id,
                    data,
                    error,
                });
            }
            // --- 書き戻し (ホスト Explorer → クライアント、一方向) ---
            ControlMsg::FsCreate { path, is_dir } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_create(&scope, &path, is_dir).await {
                    tracing::warn!(path = %path, error = %e, "FsCreate failed");
                }
            }
            ControlMsg::FsWrite { path, offset, data } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_write(&scope, &path, offset, &data).await {
                    tracing::warn!(path = %path, error = %e, "FsWrite failed");
                }
            }
            ControlMsg::FsDelete { path } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_delete(&scope, &path).await {
                    tracing::warn!(path = %path, error = %e, "FsDelete failed");
                }
            }
            ControlMsg::FsRename { from, to } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_rename(&scope, &from, &to).await {
                    tracing::warn!(from = %from, to = %to, error = %e, "FsRename failed");
                }
            }
            other => tracing::debug!(?other, "ignored control msg"),
        }
    }
}

/// このセッションのファイル共有スコープ。
#[derive(Clone, Default)]
pub enum ShareScope {
    /// 共有無効 (既定)。すべての FS 要求を拒否する。
    #[default]
    Disabled,
    /// This PC 全体。先頭コンポーネントをドライブ文字とみなし、全ドライブを 1 つの合成ルートに
    /// 公開する。ホスト側は登録端末のときだけ実際にドライブを mount する (net.rs の client_trusted)。
    WholePc,
}

/// 1 ページの一覧結果。
struct ListPage {
    entries: Vec<FsEntry>,
    done: bool,
    error: Option<String>,
}

impl ListPage {
    fn err(msg: String) -> Self {
        Self {
            entries: Vec::new(),
            done: true,
            error: Some(msg),
        }
    }
    fn into_msg(self, req_id: u64) -> ControlMsg {
        ControlMsg::FsListReply {
            req_id,
            entries: self.entries,
            done: self.done,
            error: self.error,
        }
    }
}

/// 制御フレーム 64KB 上限に収めるための 1 ページあたりエントリ数上限。
const LIST_PAGE: usize = 128;
/// FsRead 1 回の上限。host は 32KB ずつ要求するが、悪意あるホストが大きい length を
/// 送ってきても `FsReadReply` (data + bincode + AEAD タグ) が TCP フレーム上限
/// (`u16::MAX`) を超えて自己切断しないよう、オーバーヘッド分を残してクランプする。
const MAX_READ: u32 = 60 * 1024;

/// 読み取り系 (一覧/読込) のパス解決。`Ok(None)` は合成ルート (= ドライブ一覧を出す) を表す。
fn resolve_read(scope: &ShareScope, rel: &str) -> Result<Option<PathBuf>, String> {
    match scope {
        ShareScope::Disabled => Err("sharing disabled".into()),
        ShareScope::WholePc => whole_pc_resolve(rel),
    }
}

/// 書き戻し系 (作成/書込/削除/改名) のパス解決。ドライブ/合成ルート自身への作用は拒否する。
fn resolve_write(scope: &ShareScope, rel: &str) -> Result<PathBuf, String> {
    match scope {
        ShareScope::Disabled => Err("sharing disabled".into()),
        ShareScope::WholePc => whole_pc_resolve_write(rel),
    }
}

/// 全ドライブ合成ルート: ホスト相対パス (`/` 区切り) を実パスへ写す。
/// 先頭コンポーネント = ドライブ文字 (例 "C") → `C:\`、以降は通常の子コンポーネント。
/// `Ok(None)` は合成ルート自身 (= ドライブ一覧)。`..` / 区切り混入などの危険成分は拒否する。
/// This PC 全体を公開する仕様上「ルート外脱出」概念は無く、全ドライブが共有対象なので
/// canonicalize での封じ込めは行わない (脱出先も同じく共有範囲内)。
fn whole_pc_resolve(rel: &str) -> Result<Option<PathBuf>, String> {
    let mut comps = rel.split('/').filter(|c| !c.is_empty() && *c != ".");
    let Some(first) = comps.next() else {
        return Ok(None); // 合成ルート
    };
    // ドライブ文字は 1 文字の英字のみ。
    let mut chars = first.chars();
    let letter = chars.next();
    if first.len() != 1
        || chars.next().is_some()
        || !letter.is_some_and(|c| c.is_ascii_alphabetic())
    {
        return Err(format!("invalid drive: {first}"));
    }
    let mut p = PathBuf::from(format!("{}:\\", letter.unwrap().to_ascii_uppercase()));
    for comp in comps {
        if comp == ".." || comp.contains('\\') || comp.contains(':') {
            return Err(format!("unsafe path component: {comp}"));
        }
        p.push(comp);
    }
    Ok(Some(p))
}

/// 書き戻し用の解決。合成ルート (0 成分) とドライブのルート (1 成分) への破壊的操作を拒否する
/// (悪意あるホストが `FsDelete{path:"C"}` 1 通でドライブごと消す/改名する事故を防ぐ)。
fn whole_pc_resolve_write(rel: &str) -> Result<PathBuf, String> {
    let n = rel
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .count();
    if n < 2 {
        return Err("refusing operation on drive or synthetic root".into());
    }
    whole_pc_resolve(rel)?.ok_or_else(|| "refusing operation on synthetic root".into())
}

/// 存在する論理ドライブを `FsEntry` (ディレクトリ扱い) で返す。合成ルートの一覧に使う。
/// ドライブ名は `:` を含むとホストのファイル名として不正になるため、裸の文字 ("C") にする。
#[cfg(windows)]
fn list_drives() -> Vec<FsEntry> {
    // SAFETY: GetLogicalDrives は引数を取らず、存在ドライブのビットマスクを返すだけの純粋関数。
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| FsEntry {
            name: ((b'A' + i as u8) as char).to_string(),
            is_dir: true,
            size: 0,
        })
        .collect()
}
#[cfg(not(windows))]
fn list_drives() -> Vec<FsEntry> {
    Vec::new()
}

/// `all` を名前順に整列し `offset` から 1 ページ (最大 `LIST_PAGE` 件) 切り出す。
fn paginate(mut all: Vec<FsEntry>, offset: u32) -> ListPage {
    // ページ境界の安定のため名前で整列 (host 側でも ProjFS 規約で再整列する)。
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let total = all.len();
    let start = (offset as usize).min(total);
    let entries: Vec<FsEntry> = all.into_iter().skip(start).take(LIST_PAGE).collect();
    let done = start + entries.len() >= total;
    ListPage {
        entries,
        done,
        error: None,
    }
}

/// スコープに従いディレクトリ (または合成ルート) を `offset` から最大 `LIST_PAGE` 件列挙する。
async fn fs_list(scope: &ShareScope, rel: &str, offset: u32) -> ListPage {
    let dir = match resolve_read(scope, rel) {
        Ok(Some(p)) => p,
        Ok(None) => return paginate(list_drives(), offset), // 合成ルート = ドライブ一覧
        Err(e) => return ListPage::err(e),
    };
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return ListPage::err(format!("read_dir: {e}")),
    };
    let mut all: Vec<FsEntry> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(ent)) => {
                let name = ent.file_name().to_string_lossy().to_string();
                let (is_dir, size) = match ent.metadata().await {
                    Ok(m) => (m.is_dir(), m.len()),
                    Err(_) => continue, // メタ取得不能なエントリはスキップ
                };
                all.push(FsEntry { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => return ListPage::err(format!("read entry: {e}")),
        }
    }
    paginate(all, offset)
}

/// スコープに従いファイルを `offset` から最大 `length` バイト読む。
/// 戻り (data, error)。空 data かつ error=None は EOF。
async fn fs_read(
    scope: &ShareScope,
    rel: &str,
    offset: u64,
    length: u32,
) -> (Vec<u8>, Option<String>) {
    let path = match resolve_read(scope, rel) {
        Ok(Some(p)) => p,
        Ok(None) => return (Vec::new(), Some("is a directory".into())),
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return (Vec::new(), Some(format!("open: {e}"))),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        return (Vec::new(), Some(format!("seek: {e}")));
    }
    let want = length.min(MAX_READ) as usize;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    // 要求長を満たすか EOF まで読む (短い read を埋める)。
    while filled < want {
        match file.read(&mut buf[filled..]).await {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) => return (Vec::new(), Some(format!("read: {e}"))),
        }
    }
    buf.truncate(filled);
    (buf, None)
}

/// ファイル / ディレクトリを作成する (ファイルは 0 バイトに truncate)。
async fn fs_create(scope: &ShareScope, rel: &str, is_dir: bool) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    if is_dir {
        tokio::fs::create_dir_all(&p)
            .await
            .map_err(|e| format!("create_dir: {e}"))
    } else {
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::File::create(&p)
            .await
            .map(|_| ())
            .map_err(|e| format!("create_file: {e}"))
    }
}

/// ファイルの `offset` 以降にバイトを書き込む (なければ作成、truncate はしない)。
async fn fs_write(scope: &ShareScope, rel: &str, offset: u64, data: &[u8]) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .await
        .map_err(|e| format!("open: {e}"))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("seek: {e}"))?;
    file.write_all(data)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// ファイル / ディレクトリを削除する。
async fn fs_delete(scope: &ShareScope, rel: &str) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    match tokio::fs::metadata(&p).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(&p)
            .await
            .map_err(|e| format!("remove_dir: {e}")),
        Ok(_) => tokio::fs::remove_file(&p)
            .await
            .map_err(|e| format!("remove_file: {e}")),
        Err(_) => Ok(()), // 既に無いなら成功扱い
    }
}

/// `from` を `to` にリネーム / 移動する。
async fn fs_rename(scope: &ShareScope, from: &str, to: &str) -> Result<(), String> {
    let src = resolve_write(scope, from)?;
    let dst = resolve_write(scope, to)?;
    if let Some(parent) = dst.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::rename(&src, &dst)
        .await
        .map_err(|e| format!("rename: {e}"))
}

/// 並べ替え許容パケットを保持するバッファの上限 (パケット数)。実ネットワーク (Wi-Fi /
/// Tailscale リレー) の並べ替え距離は通常数パケットなので、これより先まで埋まらない gap は
/// 真のロスとみなす。メモリ上限も兼ねる (~64 * 1.2KB ≒ 80KB)。
const REORDER_WINDOW: usize = 64;
/// gap (= 欠番) が埋まるのを待つ最大時間。これを超えたら真のロスと判断して resync する。
/// 並べ替えで遅れたパケットは通常数 ms 以内に届くため、ここを短く保てば定常遅延に影響しない
/// 一方、真のロス時はこの時間だけ表示が止まってから次フレームへ resync する (フリーズしない)。
const REORDER_TIMEOUT: Duration = Duration::from_millis(15);
/// キーフレーム要求の再送間隔。ロス回復のため要求を出した後、回復 (キーフレーム到達) するまで
/// は再要求しない。万一その要求/応答自体がロスしても止まり続けないよう、この間隔で 1 回だけ
/// 再アームする。旧実装の「ロスのたびに 200ms 間隔で要求」を撤廃し、IDR ストームを断つ。
const KEYFRAME_REREQUEST: Duration = Duration::from_millis(500);

/// 再構成器が吐く出来事。`udp_reader_loop` 側で送信/統計/キーフレーム要求へ振り分ける。
enum ReasmEvent {
    /// 1 フレーム完成 (フロントへ送る Annex-B)。
    Frame {
        keyframe: bool,
        frame_id: u16,
        data: Vec<u8>,
    },
    /// 真のロスとして計上すべき欠番パケット数 (並べ替えは含めない)。
    Lost(u32),
    /// 参照フレーム破損からの回復のためキーフレームを要求する。
    RequestKeyframe,
}

/// 並べ替え許容の映像フレーム再構成器。
///
/// 旧実装は `seq` が 1 つでも飛んだら即「ロス扱い・現フレーム破棄・キーフレーム要求」していた
/// ため、(1) UDP の並べ替えをロスと誤認し、(2) ロスのたびに IDR を要求して帯域スパイク→さらに
/// ロス、の破綻ループ (IDR ストーム) を起こしていた。これがリモート操作のカクつきの主因。
///
/// ここでは `seq` をキーに小さな再整列バッファ (`REORDER_WINDOW` / `REORDER_TIMEOUT`) を持ち、
/// 並べ替えで遅れたパケットは順序を直して取り込む (フレームを落とさない)。窓も時間も尽きた gap
/// だけを真のロスとして resync し、その時に限りキーフレームを要求する。要求は回復するまで
/// 抑制し (`awaiting_keyframe`)、ストームを防ぐ。
struct VideoReassembler {
    /// 次に順序通り処理すべき seq。`None` は初回パケット待ち。`seq` は u32 巡回。
    next_seq: Option<u32>,
    /// 先行して届いた (順序待ち) パケット。`seq` -> (flags, frame_id, payload)。
    buffered: HashMap<u32, OwnedPkt>,
    /// gap が初めて生じた時刻。`REORDER_TIMEOUT` 判定に使う。埋まれば `None` に戻す。
    gap_since: Option<Instant>,
    // --- 現在組み立て中のフレーム ---
    cur_frame_id: Option<u16>,
    cur_keyframe: bool,
    cur_payload: Vec<u8>,
    /// 現フレームを諦めて次の FRAME_START まで読み飛ばす。
    dropping: bool,
    /// キーフレーム要求中 (回復待ち)。回復するまで再要求を抑制する。
    awaiting_keyframe: bool,
    /// 直近のキーフレーム要求時刻 (`KEYFRAME_REREQUEST` の再アーム判定)。
    last_keyframe_req: Instant,
}

/// 順序待ちバッファに置く所有パケット。
struct OwnedPkt {
    flags: VideoPacketFlags,
    frame_id: u16,
    payload: Vec<u8>,
}

impl VideoReassembler {
    fn new(now: Instant) -> Self {
        Self {
            next_seq: None,
            buffered: HashMap::new(),
            gap_since: None,
            cur_frame_id: None,
            cur_keyframe: false,
            cur_payload: Vec::new(),
            dropping: false,
            awaiting_keyframe: false,
            // 過去側に倒し、最初のロスで即要求できるようにする。
            last_keyframe_req: now.checked_sub(KEYFRAME_REREQUEST).unwrap_or(now),
        }
    }

    /// パケット 1 個を取り込む。完成フレーム等の出来事を `out` へ push する。
    fn push(&mut self, seq: u32, pkt: OwnedPkt, now: Instant, out: &mut Vec<ReasmEvent>) {
        match self.next_seq {
            None => {
                // 初回。この seq から順序付けを開始する。
                self.next_seq = Some(seq.wrapping_add(1));
                self.ingest(pkt, out);
                self.drain(out);
            }
            Some(ns) => {
                let diff = seq.wrapping_sub(ns) as i32;
                if diff < 0 {
                    // 既に通過済みの seq = 並べ替えで遅れて届いた or 重複。ロスに数えず捨てる。
                } else if diff == 0 {
                    self.next_seq = Some(ns.wrapping_add(1));
                    self.ingest(pkt, out);
                    self.drain(out);
                } else {
                    // 先行パケット。順序待ちバッファへ積む。
                    self.buffered.insert(seq, pkt);
                    if self.gap_since.is_none() {
                        self.gap_since = Some(now);
                    }
                    self.maybe_force_skip(now, out);
                }
            }
        }
    }

    /// バッファ先頭が `next_seq` と連続する限り取り込む (並べ替えの追いつき)。
    fn drain(&mut self, out: &mut Vec<ReasmEvent>) {
        while let Some(ns) = self.next_seq {
            let Some(pkt) = self.buffered.remove(&ns) else {
                break;
            };
            self.next_seq = Some(ns.wrapping_add(1));
            self.ingest(pkt, out);
        }
        if self.buffered.is_empty() {
            self.gap_since = None;
        }
    }

    /// 窓 or 時間切れの gap を真のロスとして飛ばし、最小の先行 seq へ resync する。
    fn maybe_force_skip(&mut self, now: Instant, out: &mut Vec<ReasmEvent>) {
        let over_window = self.buffered.len() >= REORDER_WINDOW;
        let over_time = self
            .gap_since
            .is_some_and(|t| now.duration_since(t) >= REORDER_TIMEOUT);
        if !(over_window || over_time) {
            return;
        }
        let Some(ns) = self.next_seq else { return };
        // 先行バッファの中で「最も近い (巡回前方距離が最小の)」seq まで飛ぶ。
        let Some(target) = self
            .buffered
            .keys()
            .copied()
            .min_by_key(|&k| k.wrapping_sub(ns))
        else {
            return;
        };
        let lost = target.wrapping_sub(ns);
        out.push(ReasmEvent::Lost(lost));
        // 現フレームは欠落で復号不能。破棄して次の FRAME_START まで読み飛ばす。
        self.cur_payload.clear();
        self.cur_frame_id = None;
        self.dropping = true;
        // 参照フレームが壊れたので回復のためキーフレームを要求する (抑制付き)。
        self.request_keyframe(now, out);
        self.next_seq = Some(target);
        self.gap_since = None;
        self.drain(out);
    }

    /// 順序通りの 1 パケットをフレーム組み立てに反映する。
    fn ingest(&mut self, pkt: OwnedPkt, out: &mut Vec<ReasmEvent>) {
        if pkt.flags.contains(VideoPacketFlags::FRAME_START) {
            self.cur_payload.clear();
            self.cur_frame_id = Some(pkt.frame_id);
            self.cur_keyframe = pkt.flags.contains(VideoPacketFlags::KEYFRAME);
            self.dropping = false;
        }
        if self.dropping || self.cur_frame_id != Some(pkt.frame_id) {
            // フレーム境界が合わない (読み飛ばし中 or 取りこぼした FRAME_START)。
            return;
        }
        if self.cur_payload.len() + pkt.payload.len() > MAX_FRAME_BYTES {
            tracing::warn!("frame too large, dropping");
            self.cur_payload.clear();
            self.cur_frame_id = None;
            self.dropping = true;
            return;
        }
        self.cur_payload.extend_from_slice(&pkt.payload);
        if pkt.flags.contains(VideoPacketFlags::FRAME_END) {
            let keyframe = self.cur_keyframe;
            // キーフレームが完成 = 参照フレーム回復。以降のキーフレーム要求抑制を解除する。
            if keyframe {
                self.awaiting_keyframe = false;
            }
            out.push(ReasmEvent::Frame {
                keyframe,
                frame_id: self.cur_frame_id.take().unwrap_or(pkt.frame_id),
                data: std::mem::take(&mut self.cur_payload),
            });
        }
    }

    /// キーフレームを要求する。回復待ち中は再アーム間隔を超えるまで抑制する (IDR ストーム防止)。
    fn request_keyframe(&mut self, now: Instant, out: &mut Vec<ReasmEvent>) {
        if self.awaiting_keyframe && now.duration_since(self.last_keyframe_req) < KEYFRAME_REREQUEST
        {
            return;
        }
        self.awaiting_keyframe = true;
        self.last_keyframe_req = now;
        out.push(ReasmEvent::RequestKeyframe);
    }
}

/// 受信したパケットからフレームを再構成し、Channel<InvokeResponseBody> でフロントへ。
/// フロント宛バイナリの先頭に小さなヘッダを付ける:
///   [0]   flags (bit0=keyframe)
///   [1..3] frame_id (be u16)
///   [3..]  Annex-B H.264 NALU
/// 受信側 (Rust) のフレーム完成カデンスのジッタ計測 (`AURORA_PERF` 有効時のみ)。webview の Channel
/// へ渡す**前** (= ネット受信 + 再組立直後) の到着間隔ジッタを測る。JS 側 `vstat` の jitter_ms と
/// 比較し、残ジッタが「ネット/再組立由来」(Rust ≈ JS) か「webview IPC/JS スケジューリング由来」
/// (Rust ≪ JS) かを切り分ける診断用。`rstat` 行として client.log に 1 秒窓で出す。
struct RecvJitterMeter {
    enabled: bool,
    last: Option<Instant>,
    ema_interval_ms: f64,
    ema_jitter_ms: f64,
    window_start: Instant,
    frames: u32,
}

impl RecvJitterMeter {
    fn new() -> Self {
        // ホストの --perf に対応するクライアント側スイッチ (log.rs の perf_enabled と同条件)。
        let enabled = std::env::var("AURORA_PERF")
            .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        Self {
            enabled,
            last: None,
            ema_interval_ms: 1000.0 / 60.0,
            ema_jitter_ms: 4.0,
            window_start: Instant::now(),
            frames: 0,
        }
    }

    /// フレーム完成時刻を 1 つ記録し、1 秒窓ごとにジッタを `rstat` ログへ出す。
    fn record(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        if let Some(last) = self.last {
            let dt = now.duration_since(last).as_secs_f64() * 1000.0;
            // 大きな gap (ロス回復・再開直後) は外れ値として無視 (推定を暴れさせない)。
            if dt > 0.0 && dt < 250.0 {
                let dev = (dt - self.ema_interval_ms).abs();
                self.ema_interval_ms += 0.1 * (dt - self.ema_interval_ms);
                self.ema_jitter_ms += 0.1 * (dev - self.ema_jitter_ms);
            }
        }
        self.last = Some(now);
        self.frames += 1;
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            tracing::info!(
                target: "aurora::frontend",
                "rstat recv_jitter_ms={:.1} recv_interval_ms={:.1} frames={}",
                self.ema_jitter_ms,
                self.ema_interval_ms,
                self.frames,
            );
            self.window_start = now;
            self.frames = 0;
        }
    }
}

async fn udp_reader_loop(
    udp: Arc<UdpSocket>,
    session: Arc<TransportSession>,
    sink: Sink,
    audio_tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    stats: Arc<StatsCollector>,
    keyframe_tx: mpsc::UnboundedSender<ControlMsg>,
) -> anyhow::Result<()> {
    let mut recv_buf = vec![0u8; 2048];
    let mut plain = Vec::new();

    // ホストの UDP hello は単発でロスしうる。最初のデータグラムを受信するまで
    // 250ms ごとに hello を再送し、ホストの peer_udp 登録を確実にする (登録前は
    // ホストが映像を送れず、こちらも何も受け取れないため再送がないと無音になる)。
    let mut established = false;
    let mut hello_interval = tokio::time::interval(Duration::from_millis(250));
    hello_interval.tick().await; // 初回の即時 tick を捨てる

    // 映像フレーム組立状態 (STREAM_VIDEO のみで使う)。並べ替え許容の再構成器に集約する。
    let mut reasm = VideoReassembler::new(Instant::now());
    // push が吐く出来事を毎パケット受ける一時バッファ (再利用してアロケーションを抑える)。
    let mut events: Vec<ReasmEvent> = Vec::new();
    // 受信側ジッタ計測 (AURORA_PERF 有効時のみ)。webview へ渡す前のフレーム完成カデンスを測る。
    let mut recv_jitter = RecvJitterMeter::new();

    loop {
        // recv エラーでループを終了させない。Windows の connected UDP ソケットは
        // ICMP Port Unreachable (対向ポートの一時不達等) を WSAECONNRESET として recv
        // エラーで返すため、`?` で抜けると映像タスクだけが静かに死ぬ (TCP セッションは
        // 生きているので Disconnected も自動再接続も発火しない)。捨てて受信を続ける。
        let n = if established {
            match udp.recv(&mut recv_buf).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!(error = %e, "UDP recv error ignored");
                    continue;
                }
            }
        } else {
            tokio::select! {
                r = udp.recv(&mut recv_buf) => {
                    match r {
                        Ok(n) => {
                            established = true;
                            n
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "UDP recv error ignored");
                            continue;
                        }
                    }
                }
                _ = hello_interval.tick() => {
                    let _ = udp.send(b"AURHELLO").await;
                    continue;
                }
            }
        };
        // 受信スループット (映像 + 音声、データグラム全体) を計測。
        stats.add_bytes(n as u64);
        // 先頭 8B は平文の nonce (BE)。これで復号する。
        if n < 8 {
            continue;
        }
        let nonce = u64::from_be_bytes(recv_buf[..8].try_into().unwrap());
        // UDP は映像/音声ドメインのみ。TCP 制御ドメイン等の暗号文 (同一鍵) を UDP へ注入
        // されても、復号前にドメインで弾く (多重防御)。AEAD 偽造不可 + Tailscale 下層暗号と
        // 合わせ、経路上のクロスチャネル注入をここで遮断する。
        let domain = nonce & NONCE_DOMAIN_MASK;
        if domain != NONCE_DOMAIN_VIDEO && domain != NONCE_DOMAIN_AUDIO {
            continue;
        }
        plain.clear();
        if session.decrypt(nonce, &recv_buf[8..n], &mut plain).is_err() {
            // 復号失敗パケットは捨てる
            continue;
        }

        // 先頭 1B のストリームタグで映像 / 音声を振り分ける
        let (&tag, body) = match plain.split_first() {
            Some(x) => x,
            None => continue,
        };

        if tag == STREAM_AUDIO {
            if let Some(tx) = &audio_tx {
                if let Ok(pkt) = AudioPacket::decode(body) {
                    // 再生スレッドへ Opus フレームを渡す (seq は MVP では未使用)
                    let _ = tx.send(pkt.payload.to_vec());
                }
            }
            continue;
        }
        if tag != STREAM_VIDEO {
            continue;
        }

        let pkt = match VideoPacket::decode(body) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // 損失率算出のため受信した映像パケットを計上。
        stats.add_video_pkt();

        // 並べ替え許容の再構成器へ渡す。完成フレーム・真のロス・キーフレーム要求が出来事として返る。
        let now = Instant::now();
        events.clear();
        reasm.push(
            pkt.seq,
            OwnedPkt {
                flags: pkt.flags,
                frame_id: pkt.frame_id,
                payload: pkt.payload.to_vec(),
            },
            now,
            &mut events,
        );
        for ev in events.drain(..) {
            match ev {
                ReasmEvent::Frame {
                    keyframe,
                    frame_id,
                    data,
                } => {
                    // webview へ渡す前にフレーム完成カデンスのジッタを記録 (診断)。`now` は
                    // 完成パケット (FRAME_END) の受信時刻 ≒ フレーム完成時刻。
                    recv_jitter.record(now);
                    let mut out = Vec::with_capacity(3 + data.len());
                    out.push(if keyframe { 1u8 } else { 0u8 });
                    out.extend_from_slice(&frame_id.to_be_bytes());
                    out.extend_from_slice(&data);
                    // バイナリで現在の表示先へ送信 (デタッチ中なら捨てられ、次キーフレームで回復)
                    sink.send_video(InvokeResponseBody::Raw(out));
                    stats.add_frame();
                }
                ReasmEvent::Lost(n) => stats.add_lost(n as u64),
                ReasmEvent::RequestKeyframe => {
                    let _ = keyframe_tx.send(ControlMsg::RequestKeyframe);
                }
            }
        }
    }
}

/// 受信バッファを拡大した UDP ソケットを 0.0.0.0:任意ポート で開く。
///
/// 既定の受信バッファ (数百 KB) は 1440p/60fps のバースト時に溢れてパケットロス
/// (映像ノイズの主因) を招くため、4MB に広げてバーストを吸収する。
fn make_udp_socket(ipv6: bool) -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Socket, Type};
    // 対向 (解決済みアドレス) と同じファミリでソケットを開く。IPv4 固定だと IPv6
    // Tailscale アドレスへの connect が失敗するため、解決結果に合わせる。
    let domain = if ipv6 { Domain::IPV6 } else { Domain::IPV4 };
    let sock = Socket::new(domain, Type::DGRAM, None)?;
    // 失敗しても致命的ではない (既定値で続行)。
    let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);
    sock.set_nonblocking(true)?;
    let bind: SocketAddr = if ipv6 {
        "[::]:0".parse()
    } else {
        "0.0.0.0:0".parse()
    }
    .unwrap();
    sock.bind(&bind.into())?;
    let std_udp: std::net::UdpSocket = sock.into();
    Ok(UdpSocket::from_std(std_udp)?)
}

// -------- TCP framing util (host net.rs と同形) --------

async fn read_tcp_frame<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    // 長さは u16 なので最大 65535。受信サイズはこれで自然に有界で、メッセージ本体の
    // 上限は復号後の bincode (with_limit MAX_TCP_FRAME) が守る。
    let len = u16::from_be_bytes(len_buf) as usize;
    out.resize(len, 0);
    stream.read_exact(out).await?;
    Ok(())
}

async fn write_tcp_frame<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    data: &[u8],
) -> anyhow::Result<()> {
    if data.len() > u16::MAX as usize {
        anyhow::bail!("frame too large: {}", data.len());
    }
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    stream.flush().await.ok();
    Ok(())
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成ルート (0 成分) とドライブのルート (1 成分) への破壊系操作は拒否する
    /// (ドライブごと remove_dir_all / rename される回帰を防ぐ)。
    #[test]
    fn whole_pc_write_rejects_drive_and_synthetic_root() {
        for rel in ["", ".", "/", "C", "C/", "/C/.", "//"] {
            assert!(
                whole_pc_resolve_write(rel).is_err(),
                "expected rejection for {rel:?}"
            );
        }
    }

    /// `..` / 区切り混入 / 不正なドライブ文字はコンポーネント単位で拒否する。
    #[test]
    fn whole_pc_resolve_rejects_traversal_and_bad_drive() {
        for rel in ["C/..", "C/a/../b", "C/a\\b", "C/x:y", "CC/x", "1/x"] {
            assert!(whole_pc_resolve(rel).is_err(), "should reject {rel:?}");
        }
    }

    /// 合成ルートは `None`、ドライブ配下は実パスへ写す (ドライブ文字は大文字化)。
    #[cfg(windows)]
    #[test]
    fn whole_pc_resolve_maps_paths() {
        assert!(matches!(whole_pc_resolve(""), Ok(None)));
        assert!(matches!(whole_pc_resolve("."), Ok(None)));
        let p = whole_pc_resolve("c/Users/name").unwrap().unwrap();
        assert_eq!(p, std::path::PathBuf::from("C:\\Users\\name"));
    }

    // ---- VideoReassembler (P1: 並べ替え/ロス耐性) ----

    /// 単一パケットフレーム (START|END[|KEYFRAME]) を組み立てるテスト用ヘルパ。
    fn single_frame_pkt(frame_id: u16, keyframe: bool) -> OwnedPkt {
        let mut flags = VideoPacketFlags::FRAME_START | VideoPacketFlags::FRAME_END;
        if keyframe {
            flags |= VideoPacketFlags::KEYFRAME;
        }
        OwnedPkt {
            flags,
            frame_id,
            payload: vec![frame_id as u8],
        }
    }

    fn frame_ids(events: &[ReasmEvent]) -> Vec<u16> {
        events
            .iter()
            .filter_map(|e| match e {
                ReasmEvent::Frame { frame_id, .. } => Some(*frame_id),
                _ => None,
            })
            .collect()
    }

    fn count_keyframe_reqs(events: &[ReasmEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, ReasmEvent::RequestKeyframe))
            .count()
    }

    fn total_lost(events: &[ReasmEvent]) -> u32 {
        events
            .iter()
            .map(|e| match e {
                ReasmEvent::Lost(n) => *n,
                _ => 0,
            })
            .sum()
    }

    /// 順序通りに届けば各フレームが完成し、ロスもキーフレーム要求も出ない。
    #[tokio::test]
    async fn reasm_in_order() {
        let now = Instant::now();
        let mut r = VideoReassembler::new(now);
        let mut out = Vec::new();
        for (seq, fid) in [(0u32, 0u16), (1, 1), (2, 2)] {
            r.push(seq, single_frame_pkt(fid, fid == 0), now, &mut out);
        }
        assert_eq!(frame_ids(&out), vec![0, 1, 2]);
        assert_eq!(total_lost(&out), 0);
        assert_eq!(count_keyframe_reqs(&out), 0);
    }

    /// 窓内の並べ替え (後続パケットが先に届く) はフレームを落とさずに吸収する。
    #[tokio::test]
    async fn reasm_reorder_within_window() {
        let now = Instant::now();
        let mut r = VideoReassembler::new(now);
        let mut out = Vec::new();
        // frame0: seq0 単発。frame1: seq1=START, seq2=END。seq2 を seq1 より先に届ける。
        r.push(0, single_frame_pkt(0, true), now, &mut out);
        r.push(
            2,
            OwnedPkt {
                flags: VideoPacketFlags::FRAME_END,
                frame_id: 1,
                payload: vec![0xBB],
            },
            now,
            &mut out,
        );
        r.push(
            1,
            OwnedPkt {
                flags: VideoPacketFlags::FRAME_START,
                frame_id: 1,
                payload: vec![0xAA],
            },
            now,
            &mut out,
        );
        assert_eq!(frame_ids(&out), vec![0, 1]);
        assert_eq!(total_lost(&out), 0, "reorder must not count as loss");
        assert_eq!(count_keyframe_reqs(&out), 0);
    }

    /// 遅れて届いた (通過済み seq) パケットは重複/並べ替えとして無視し、ロスに数えない。
    #[tokio::test]
    async fn reasm_late_packet_ignored() {
        let now = Instant::now();
        let mut r = VideoReassembler::new(now);
        let mut out = Vec::new();
        r.push(0, single_frame_pkt(0, true), now, &mut out);
        r.push(1, single_frame_pkt(1, false), now, &mut out);
        out.clear();
        // 既に通過した seq0 が再度届く (重複)。
        r.push(0, single_frame_pkt(0, true), now, &mut out);
        assert!(out.is_empty(), "late/duplicate packet must produce nothing");
    }

    /// タイムアウトを超えても埋まらない gap は真のロスとして resync し、キーフレームを 1 回要求する。
    #[tokio::test]
    async fn reasm_real_loss_resyncs() {
        let base = Instant::now();
        let mut r = VideoReassembler::new(base);
        let mut out = Vec::new();
        r.push(0, single_frame_pkt(0, true), base, &mut out);
        out.clear();
        // seq1 がロス。seq2 が届くが gap は未タイムアウト → まだ何も出ない。
        r.push(2, single_frame_pkt(2, false), base, &mut out);
        assert!(out.is_empty(), "before timeout: hold, no events");
        // タイムアウト後に次パケットが来ると force-skip して resync する。
        let later = base + REORDER_TIMEOUT + Duration::from_millis(1);
        r.push(3, single_frame_pkt(3, false), later, &mut out);
        assert_eq!(
            total_lost(&out),
            1,
            "exactly the missing seq1 counts as lost"
        );
        assert_eq!(count_keyframe_reqs(&out), 1);
        // resync 後、バッファに溜まっていた frame2/frame3 は届く。
        assert_eq!(frame_ids(&out), vec![2, 3]);
    }

    /// 回復前の連続ロスではキーフレーム要求を抑制し (ストーム防止)、キーフレーム完成で再アームする。
    #[tokio::test]
    async fn reasm_keyframe_request_suppressed_until_recovery() {
        let mut t = Instant::now();
        let mut r = VideoReassembler::new(t);
        let mut out = Vec::new();
        let after_timeout = REORDER_TIMEOUT + Duration::from_millis(1);

        // 「1 個だけロスし、続く 1 フレームが届いて resync する」を起こすクロージャ。
        // gap パケットを積み、タイムアウト後にその次の seq を届けて force-skip → 連続 drain させる。
        // (lost_seq を欠番にし、lost_seq+1 と lost_seq+2 のフレームを届ける)
        let clean_loss = |r: &mut VideoReassembler,
                          t: &mut Instant,
                          lost_seq: u32,
                          out: &mut Vec<ReasmEvent>| {
            r.push(
                lost_seq + 1,
                single_frame_pkt((lost_seq + 1) as u16, false),
                *t,
                out,
            );
            *t += after_timeout;
            r.push(
                lost_seq + 2,
                single_frame_pkt((lost_seq + 2) as u16, false),
                *t,
                out,
            );
            *t += Duration::from_millis(10);
        };

        // seq0: 起点フレーム。next_seq=1 になる。
        r.push(0, single_frame_pkt(0, true), t, &mut out);
        t += Duration::from_millis(10);

        // 1 回目のロス (seq1 欠番) → キーフレーム要求 1 回。
        out.clear();
        clean_loss(&mut r, &mut t, 1, &mut out);
        assert_eq!(count_keyframe_reqs(&out), 1, "first loss requests once");

        // 回復前 (まだキーフレーム未到達) の 2 回目のロス (seq4 欠番) → 抑制され要求は出ない。
        out.clear();
        clean_loss(&mut r, &mut t, 4, &mut out);
        assert_eq!(
            count_keyframe_reqs(&out),
            0,
            "subsequent loss before recovery is suppressed"
        );

        // キーフレーム (seq7) が順序通り完成 = 回復 → 抑制解除。
        out.clear();
        r.push(7, single_frame_pkt(7, true), t, &mut out);
        t += Duration::from_millis(10);
        assert_eq!(frame_ids(&out), vec![7], "keyframe completes in order");

        // 回復後の 3 回目のロス (seq8 欠番) → 再びキーフレームを要求する。
        out.clear();
        clean_loss(&mut r, &mut t, 8, &mut out);
        assert_eq!(
            count_keyframe_reqs(&out),
            1,
            "after keyframe recovery, loss requests again"
        );
    }
}
