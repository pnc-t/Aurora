//! TCP 制御 + UDP 映像のサーバ。共有 capture/encode の出力を最大 `MAX_CLIENTS` 接続へ
//! ファンアウトするマルチクライアントモデル (1 エンコーダを最悪リンクに合わせて全員へ配る)。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use aurora_common::framing::{
    AudioPacket, MAX_PAYLOAD, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
    read_tcp_frame, write_tcp_frame,
};
use aurora_common::noise::{
    HandshakeState, NONCE_DOMAIN_AUDIO, NONCE_DOMAIN_CTRL, NONCE_DOMAIN_VIDEO, TransportSession,
};
use aurora_common::proto::{ControlMsg, Display, InputEvent, MouseButton};
use bincode::Options;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};
use tracing::Instrument;

use crate::audio::EncodedAudio;
use crate::capture::DisplayInfo;
use crate::encode::EncodedNal;
use crate::input::DisplayGeom;
use crate::projfs::{self, FsBridge, FsReply};

// net サーバの実装を関心事ごとに分割したサブモジュール群。mod.rs はサーバ起動
// (bind/accept/共有状態/ファンアウト) を担い、以下へ各関心事を委譲する。
mod auth;
mod bitrate;
mod client;
mod files;
mod frame;
mod session;

// クレート内 (host の他モジュール) が使う公開 API。
pub(crate) use session::{ActiveGuard, is_active};
// net モジュール内で共有する型/関数。サブモジュール間参照は各ファイルの `use super::*` で解決する。
pub(crate) use auth::AuthThrottle;
pub(crate) use bitrate::BitrateController;
pub use bitrate::{EncoderCommand, InputCmd};
pub(crate) use client::handle_client;
pub(crate) use files::{UploadState, open_upload, stream_file_to_client};
pub(crate) use frame::{encrypt_udp, send_control_msg, wait_udp_hello};

/// 接続ごとの相関 ID 採番カウンタ。ログの `conn` span に載せ、クライアント側の
/// session_id と peer/時刻で突き合わせて 1 接続を端から端まで追跡できるようにする。
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// 待受アドレスの解決方法。`--bind` 明示なら `Fixed`、未指定なら `Auto`。
///
/// ホストは ONLOGON タスクで自動起動するため、`tailscaled` の Tailscale IP 割り当てより先に
/// 立ち上がるレースが常態化する。`Auto` を **bind 時に** 解決 (遅延解決) することで、起動直後に
/// IP が未確定でも bind ループ内で待ってから貼れる ([`resolve_bind_ip`])。`Fixed` は即値。
#[derive(Clone, Copy)]
pub enum BindSpec {
    /// Tailscale IP を bind ごとに解決する (起動が Tailscale より先でも待つ)。
    Auto,
    /// `--bind <ip>` で明示指定された固定アドレス。
    Fixed(std::net::IpAddr),
}

impl std::fmt::Display for BindSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindSpec::Auto => write!(f, "auto(Tailscale)"),
            BindSpec::Fixed(ip) => write!(f, "{ip}"),
        }
    }
}

pub struct ServerConfig {
    /// 待受アドレスの解決方法。既定 (`--bind` 未指定) は `Auto` = Tailscale IP を **bind 時に**
    /// 解決する (起動が Tailscale より先でも待てるよう遅延解決)。`--bind <ip>` で `Fixed`。
    /// Tailnet 外 (LAN/インターネット) への露出を絞り、総当たり・接続枠 DoS の面を減らす。
    pub bind: BindSpec,
    /// 待受ポート (TCP 制御 / UDP 映像で共用)。tray UI からのポート変更で書き換えられ、
    /// `reload` 通知で accept ループが TCP/UDP を貼り直す (再起動なしに反映)。
    pub port: std::sync::Arc<std::sync::atomic::AtomicU16>,
    /// ポート変更の反映トリガ。tray が `notify_one` し、accept ループが貼り直す。
    pub reload: std::sync::Arc<tokio::sync::Notify>,
    /// 接続 PSK。tray UI からのパスワード変更で書き換えられるため共有可変で持つ。
    /// 各接続は受理時にスナップショットを取り、その値でハンドシェイクする。
    pub psk: std::sync::Arc<std::sync::Mutex<[u8; 32]>>,
    pub displays: Vec<DisplayInfo>,
    pub audio: bool,
    /// 適応制御 / エンコーダのビットレート上限 (bps)。`--max-bitrate` 由来。
    pub max_bitrate_bps: u32,
    /// 認証失敗の累計カウンタ (起動以降)。net がハンドシェイク失敗で加算し、tray が
    /// メニューに表示する (総当たりに晒されているかをオーナーが気付けるようにする)。
    pub auth_failures: Arc<AtomicU32>,
    /// デバイスピン留め (クライアント静的鍵の許可リスト照合)。既定無効。tray が有効化/
    /// ペアリングを操作し、net が PSK 認証後に相手公開鍵を照合する (= 2 要素目)。
    pub devices: Arc<crate::security::DeviceGuard>,
    /// capture スレッドへ「現在の画面を 1 枚強制送出せよ」と促す共有フラグ。接続確立時と
    /// クライアントの RequestKeyframe で立て、capture が直近フレームを再投入する。画面静止中に
    /// 接続/再同期しても黒画面のまま固まらないようにするための仕組み (DXGI は変化フレームのみ返す)。
    pub force_frame: Arc<std::sync::atomic::AtomicBool>,
}

/// 同一アドレスで TCP/UDP を bind する。キャプチャ固着回復の re-exec 直後は、旧プロセスが
/// まだポートを保持していてアドレス使用中 (AddrInUse) になることがあるため、その場合のみ
/// 数秒間リトライする (通常は旧プロセス終了で数十 ms 以内に空く)。それ以外のエラー
/// (--bind の IP がマシンに無い等の設定ミス) はリトライしても直らないので即時失敗させる。
/// Windows の UDP ソケットで `SIO_UDP_CONNRESET` を無効化する。
///
/// 既定では、過去に `send_to` した宛先ポートが閉じている (= 切断済みクライアント) と OS が
/// ICMP port-unreachable を受け、次の `recv_from`/`send_to` を `WSAECONNRESET` (10054) で
/// 失敗させる。映像を送り続けるホストでは再接続のたびにこれが起き、共有 UDP ソケットが
/// 毒されて以後の UDP hello 待ちが即死する。FALSE を設定してこの挙動を切る。失敗は無視
/// (致命ではなく、無効化できなくても従来どおり動くだけ)。
#[cfg(windows)]
fn disable_udp_conn_reset(udp: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, SOCKET, WSAIoctl};

    let socket = SOCKET(udp.as_raw_socket() as usize);
    let enable: u32 = 0; // FALSE
    let mut bytes_returned: u32 = 0;
    // SAFETY: socket は live な tokio UdpSocket の raw handle。WSAIoctl は in バッファ (u32 1 個)
    // を読み out は使わない。所有権は移らない。
    unsafe {
        let _ = WSAIoctl(
            socket,
            SIO_UDP_CONNRESET,
            Some(&enable as *const u32 as *const core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut bytes_returned,
            None,
            None,
        );
    }
}

#[cfg(not(windows))]
fn disable_udp_conn_reset(_udp: &UdpSocket) {}

/// `tailscale ip -4` を実行して tailnet IPv4 を取得する。CLI 不在・非ゼロ終了・パース失敗は
/// `None`。CGNAT 範囲 (100.64/10) のインターフェース誤検出を避けるため、権威ある CLI に問う。
/// bind ループ (async) から呼ぶため `tokio::process` で非同期に起動する (ランタイムを塞がない)。
async fn tailscale_ip() -> Option<std::net::IpAddr> {
    // GUI サブシステム (リリース) のトレイ常駐から console アプリを起動すると窓が点滅する。
    // Auto は起動レース中これを毎周回す可能性があるため CREATE_NO_WINDOW で抑止する
    // (autostart の schtasks 呼び出しと同じ理由)。
    let mut cmd = std::process::Command::new("tailscale");
    cmd.args(["ip", "-4"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = tokio::process::Command::from(cmd).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    // 複数アドレスが返りうる (1 行 1 つ)。先頭の有効な IPv4 を採る。
    text.lines()
        .map(str::trim)
        .find_map(|l| l.parse::<std::net::Ipv4Addr>().ok())
        .map(std::net::IpAddr::V4)
}

/// bind 時に実効待受 IP を解決する。`Fixed` は即値、`Auto` は Tailscale IP を待つ。
///
/// ホストは ONLOGON で `tailscaled` より先に起動しうるため、`Auto` は `tailscale ip -4` を
/// ポーリングで待ってから返す (一度きりの解決だと未確定時に loopback へ落ちて以後固着する)。
/// `TIMEOUT` まで取れなければ `127.0.0.1` に fail-close する (Tailscale 未導入の dev 用に
/// localhost 動作確認フローを残す。LAN/インターネットへは到達不能のまま)。bind ループは
/// ポート変更でここへ戻るため、loopback に倒れた後でも次の (再)bind で Tailscale を再探索する。
async fn resolve_bind_ip(spec: BindSpec) -> std::net::IpAddr {
    use std::net::{IpAddr, Ipv4Addr};
    /// Tailscale IP 待ちのポーリング間隔。
    const POLL: std::time::Duration = std::time::Duration::from_secs(2);
    /// Tailscale IP 待ちの上限。これを超えたら loopback に倒す (= Tailscale 未導入とみなす)。
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    // Fixed は即値で返す。Auto だけ Tailscale IP を待つ (以降のポーリングループ)。
    let start = match spec {
        BindSpec::Fixed(ip) => return ip,
        BindSpec::Auto => Instant::now(),
    };
    let mut waited = false;
    loop {
        if let Some(ip) = tailscale_ip().await {
            tracing::info!(%ip, waited, "binding to Tailscale interface (auto; override with --bind)");
            return ip;
        }
        if start.elapsed() >= TIMEOUT {
            tracing::warn!(
                timeout_s = TIMEOUT.as_secs(),
                "Tailscale IP not found; binding to 127.0.0.1 (loopback only). \
                 LAN/internet are NOT reachable. Install Tailscale, or pass \
                 --bind <ip> (e.g. a LAN IP, or 0.0.0.0 for all interfaces) to expose."
            );
            return IpAddr::V4(Ipv4Addr::LOCALHOST);
        }
        // 起動直後の Tailscale 待ちを 1 度だけ告知する (毎周ログを膨らませない)。
        if !waited {
            tracing::info!(
                "Tailscale IP not ready yet; waiting before bind (host started before tailscaled)"
            );
            waited = true;
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn bind_listeners(addr: SocketAddr) -> Result<(TcpListener, Arc<UdpSocket>)> {
    const ATTEMPTS: u32 = 25; // 25 × 200ms ≒ 5 秒
    let mut attempt = 0u32;
    loop {
        let (what, e) = match TcpListener::bind(addr).await {
            Ok(tcp) => match UdpSocket::bind(addr).await {
                Ok(udp) => {
                    // Windows: 切断済みクライアントへの UDP 送信で返る ICMP port-unreachable が
                    // 後続の recv_from を WSAECONNRESET で即失敗させる (SIO_UDP_CONNRESET の既定
                    // 挙動)。これを無効化しないと、数回の再接続でこの共有 UDP ソケットが毒され、
                    // 以後の wait_udp_hello が即 10054 になり全セッションが UDP hello 待ちで死ぬ。
                    disable_udp_conn_reset(&udp);
                    return Ok((tcp, Arc::new(udp)));
                }
                Err(e) => ("UDP", e),
            },
            Err(e) => ("TCP", e),
        };
        attempt += 1;
        // AddrInUse: re-exec 直後に旧プロセスがまだポートを保持しているケース。
        // AddrNotAvailable: `tailscale ip -4` が IP を返した直後でも OS のインターフェース整備が
        // 間に合わず bind が WSAEADDRNOTAVAIL (10049) で落ちるケース (起動レースの第二段)。どちらも
        // 数秒で解消するため短くリトライする。--bind の IP がマシンに無い設定ミスもここに来るが、
        // その場合は ATTEMPTS 回 (≒5 秒) 試して諦め、明示エラーで失敗させる。
        let retryable = matches!(
            e.kind(),
            std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AddrNotAvailable
        );
        if !retryable || attempt >= ATTEMPTS {
            return Err(anyhow::Error::new(e).context(format!("bind {what} {addr}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// 同時接続できるクライアントの上限。共有 capture/encode を N 人へファンアウトするため、
/// 帯域/CPU の暴走と接続枠 DoS を抑えるよう少数に固定する。超過接続は受理直後に丁寧に閉じる。
const MAX_CLIENTS: usize = 4;

/// 映像 broadcast のリング容量。遅いクライアントが溢れたら Lagged で検知してキーフレーム要求で
/// 復帰する (フレームは Arc 共有なのでバッファ自体は安価)。fan-out の drain 速度差を吸収できる
/// 程度に取る。
const VIDEO_RING: usize = 64;
/// 音声 broadcast のリング容量。
const AUDIO_RING: usize = 128;

/// host→各クライアントへ制御メッセージを push するための登録簿。値は各 [`handle_client`] の
/// 制御送出口 (out_tx) の clone。DisplayResized やクリップボード変化を全クライアントへ配る。
/// std Mutex なので await を跨がず try_send で配る (映像ホットパスとは無関係)。
type Registry = Arc<std::sync::Mutex<HashMap<u64, mpsc::Sender<ControlMsg>>>>;

/// 全クライアントへ制御メッセージを配信する (登録簿経由)。各クライアントの out_rx ループが
/// 自分の Noise セッションで暗号化して TCP へ書く。満杯/切断のクライアントは取りこぼすが、
/// 対象は低頻度の制御メッセージ (DisplayResized / Clipboard) なので許容する。
fn broadcast_ctrl(registry: &Registry, msg: &ControlMsg) {
    let Ok(map) = registry.lock() else { return };
    for (id, tx) in map.iter() {
        if let Err(e) = tx.try_send(msg.clone()) {
            tracing::warn!(conn_id = id, error = %e, "drop control msg to client (queue full/closed)");
        }
    }
}

/// 複数クライアントで共有する制御状態 (共有ミラー)。ディスプレイ・トポロジ、単一エンコーダの
/// ビットレート適応、各クライアントの直近 stats を 1 つにまとめ、tokio Mutex で直列化する。
/// 制御操作 (ディスプレイ切替・解像度・stats) は低頻度なので、ロックを await を跨いで保持しても
/// 映像/入力のホットパス (このロックを取らない) には影響しない。
struct SharedControl {
    /// 提示する全ディスプレイ。テイクオーバーで displays[active_idx] の name/座標/解像度が変わる。
    displays: Vec<DisplayInfo>,
    /// 現在キャプチャ中の displays 内 index。
    active_idx: usize,
    /// 仮想ディスプレイのテイクオーバー実施中か (物理オフ+仮想 primary)。最後の 1 本切断で復元する。
    taken_over: bool,
    /// 検出済み仮想ディスプレイの GDI 名 (テイクオーバー判定 / is_virtual 表示)。無ければ None。
    vdd_gdi_name: Option<String>,
    /// 単一エンコーダのビットレート適応制御。全クライアントで共有し、最悪リンクに合わせる。
    bitrate: BitrateController,
    /// クライアント毎の直近 stats (loss_permille, rtt_us)。最悪値集約に使う。
    client_stats: HashMap<u64, (u16, u32)>,
    /// カーソル合成を要求中のクライアント (マウスキャプチャ中)。単一エンコーダ = 共有映像なので、
    /// 1 つでも要求があれば合成する。切断時に除く。
    cursor_overlay: HashSet<u64>,
    /// ProjFS ドライブを所有する接続 (固定パスのため 1 本のみ mount 可)。None なら未所有。
    projfs_owner: Option<u64>,
    /// 確立済み (UDP hello 完了) のクライアント数。0 へ落ちた最後の 1 本がテイクオーバー等の
    /// グローバルな後始末を担う。
    active_conns: usize,
}

impl SharedControl {
    /// 全クライアントの最悪リンク (max loss, max rtt) を集約して返す。単一エンコーダは一番
    /// 苦しいクライアントに合わせる必要があるため、ロス/RTT とも最大値を採る。
    fn worst_link(&self) -> (u16, u32) {
        self.client_stats
            .values()
            .fold((0u16, 0u32), |(l, r), &(cl, cr)| (l.max(cl), r.max(cr)))
    }
}

/// 1 接続を処理する [`handle_client`] へ渡す依存一式。引数爆発を避けてまとめる。
/// broadcast 送信元・共有制御状態・登録簿は全接続で共有 (Arc/clone)。
pub(crate) struct ClientArgs {
    peer: SocketAddr,
    psk: [u8; 32],
    audio: bool,
    udp: Arc<UdpSocket>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<String>,
    fps_tx: std::sync::mpsc::Sender<u32>,
    scale_tx: std::sync::mpsc::Sender<u32>,
    cursor_tx: std::sync::mpsc::Sender<bool>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    /// 映像/音声 broadcast の送信元。接続確立後に subscribe して自鍵で暗号化し UDP へ送る。
    nal_btx: broadcast::Sender<Arc<EncodedNal>>,
    audio_btx: broadcast::Sender<Arc<EncodedAudio>>,
    throttle: Arc<AuthThrottle>,
    devices: Arc<crate::security::DeviceGuard>,
    shared: Arc<tokio::sync::Mutex<SharedControl>>,
    registry: Registry,
    conn_id: u64,
    /// 接続/キーフレーム要求時に capture へ現在の画面の強制送出を促す共有フラグ ([`ServerConfig`])。
    force_frame: Arc<std::sync::atomic::AtomicBool>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    cfg: ServerConfig,
    nal_rx: mpsc::Receiver<EncodedNal>,
    audio_rx: mpsc::Receiver<EncodedAudio>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<String>,
    fps_tx: std::sync::mpsc::Sender<u32>,
    scale_tx: std::sync::mpsc::Sender<u32>,
    cursor_tx: std::sync::mpsc::Sender<bool>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    clip_change_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // 認証失敗スロットル (送信元 IP 単位の指数バックオフ)。全接続で 1 つ共有し、セッションを
    // またいで失敗履歴を保つ。tray 表示用カウンタは cfg から共有する。
    let throttle = Arc::new(AuthThrottle::new(cfg.auth_failures.clone()));

    // --- 映像/音声ファンアウト ---
    // 単一所有の mpsc を専用フォワーダ task が drain し、Arc で包んで broadcast へ流す。各
    // クライアントは subscribe し、生 NAL を自分の Noise 鍵・seq で暗号化して自分の UDP へ送る
    // (生 NAL のみ共有、暗号化はクライアント毎)。フォワーダ/共有状態は rebind をまたいで生きる
    // ため、bind ループの外で 1 度だけ起動する。
    let (nal_btx, _) = broadcast::channel::<Arc<EncodedNal>>(VIDEO_RING);
    let (audio_btx, _) = broadcast::channel::<Arc<EncodedAudio>>(AUDIO_RING);
    {
        let nal_btx = nal_btx.clone();
        let mut nal_rx = nal_rx;
        tokio::spawn(async move {
            loop {
                // 受信者ゼロ (全員切断 = idle) の間は **drain しない**。NAL チャネルを満杯のまま
                // 残すことで encode→capture が backpressure で停止し、待機中の CPU/GPU を抑える
                // (常時 drain すると idle でもエンコードし続けてしまう)。クライアントが subscribe
                // したら次の周回で drain を再開する。
                if nal_btx.receiver_count() == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                match nal_rx.recv().await {
                    Some(nal) => {
                        let _ = nal_btx.send(Arc::new(nal));
                    }
                    None => break, // encode 側終了
                }
            }
        });
    }
    {
        let audio_btx = audio_btx.clone();
        let mut audio_rx = audio_rx;
        tokio::spawn(async move {
            loop {
                if audio_btx.receiver_count() == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                match audio_rx.recv().await {
                    Some(au) => {
                        let _ = audio_btx.send(Arc::new(au));
                    }
                    None => break,
                }
            }
        });
    }

    // --- 共有制御状態 ---
    // 仮想ディスプレイ (MttVDD) が検出されたら、未接続時はデスクトップ未アタッチで DXGI に
    // 出ないため、合成エントリ (番兵 id) を 1 度だけ足してクライアントが選べるようにする。
    let mut displays = cfg.displays.clone();
    let vdd_gdi_name = crate::display::detect().map(|v| {
        if !displays.iter().any(|d| d.name == v.gdi_name) {
            displays.push(DisplayInfo {
                id: VIRTUAL_SENTINEL_ID,
                name: v.gdi_name.clone(),
                width: v.width,
                height: v.height,
                x: 0,
                y: 0,
            });
        }
        v.gdi_name
    });
    tracing::info!(
        vdd = vdd_gdi_name.as_deref().unwrap_or("(none)"),
        names = ?displays.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        "virtual display detection (shared topology)"
    );
    let shared = Arc::new(tokio::sync::Mutex::new(SharedControl {
        displays,
        active_idx: 0,
        taken_over: false,
        vdd_gdi_name,
        bitrate: BitrateController::new(cfg.max_bitrate_bps),
        client_stats: HashMap::new(),
        cursor_overlay: HashSet::new(),
        projfs_owner: None,
        active_conns: 0,
    }));

    // host→全クライアント push 用の登録簿。
    let registry: Registry = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // ローカルのクリップボード変化を全クライアントへ配信するフォワーダ。
    {
        let registry = registry.clone();
        let mut clip_change_rx = clip_change_rx;
        tokio::spawn(async move {
            while let Some(text) = clip_change_rx.recv().await {
                broadcast_ctrl(&registry, &ControlMsg::Clipboard { text });
            }
        });
    }

    // 同時接続数を MAX_CLIENTS に制限するセマフォ。permit を各 task が握り、切断で解放する。
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CLIENTS));

    // 外側ループ: 現在のポートで TCP/UDP を bind する。tray でポートが変わると reload 通知で
    // 内側の accept ループを抜け、ここへ戻って貼り直す (古いソケットは drop で解放)。
    // 待受 IP は bind ごとに解決する: `Auto` は Tailscale IP を待つため、ホストが tailscaled より
    // 先に起動しても (起動レース) ここで待ってから貼れる。tray は既にこのループ前に起動済みなので、
    // 待っても tray の表示・操作は阻害しない。
    loop {
        let port = cfg.port.load(Ordering::Relaxed);
        let bind_ip = resolve_bind_ip(cfg.bind).await;
        let addr = SocketAddr::new(bind_ip, port);
        let (tcp, udp) = bind_listeners(addr).await?;
        tracing::info!(tcp = %addr, udp = %addr, max_clients = MAX_CLIENTS, "host listening");

        // 内側ループ: 接続受理。reload が来たら break して外側で貼り直す。ポート変更は接続中
        // (= 接続数 ≥ 1) には tray 側で拒否されるため、reload はセッション間 (idle) でのみ届く。
        loop {
            let (stream, peer) = tokio::select! {
                accepted = tcp.accept() => accepted?,
                _ = cfg.reload.notified() => {
                    tracing::info!(new_port = cfg.port.load(Ordering::Relaxed), "port change requested; rebinding");
                    break;
                }
            };
            // 接続上限。超過は受理直後に丁寧に閉じる (ハンドシェイク前)。permit は task の寿命と
            // ともに保持し、切断で解放される。
            let permit = match sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(%peer, max_clients = MAX_CLIENTS, "client limit reached; rejecting connection");
                    drop(stream);
                    continue;
                }
            };
            // 接続ごとに連番の conn_id を採番し、この接続に関する全ログへ span で付ける。
            // クライアント側ログ (session_id) と peer/時刻で突き合わせて 1 接続を追跡できる。
            let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
            let span = tracing::info_span!("conn", id = conn_id, peer = %peer);
            // 接続ごとに現行 PSK のスナップショットを取る (tray でのパスワード変更を反映)。
            let psk = *cfg.psk.lock().expect("psk mutex poisoned");
            let args = ClientArgs {
                peer,
                psk,
                audio: cfg.audio,
                udp: udp.clone(),
                enc_tx: enc_tx.clone(),
                input_tx: input_tx.clone(),
                cap_select_tx: cap_select_tx.clone(),
                fps_tx: fps_tx.clone(),
                scale_tx: scale_tx.clone(),
                cursor_tx: cursor_tx.clone(),
                clip_apply_tx: clip_apply_tx.clone(),
                nal_btx: nal_btx.clone(),
                audio_btx: audio_btx.clone(),
                throttle: throttle.clone(),
                devices: cfg.devices.clone(),
                shared: shared.clone(),
                registry: registry.clone(),
                conn_id,
                force_frame: cfg.force_frame.clone(),
            };
            // 各接続を独立 task で処理する (await で占有しない = 複数クライアント同時)。
            tokio::spawn(
                async move {
                    // permit を task 寿命まで保持 (drop で接続枠を解放)。
                    let _permit = permit;
                    tracing::info!("TCP client connected");
                    match handle_client(stream, args).await {
                        Ok(()) => tracing::info!("client disconnected"),
                        Err(e) => tracing::warn!(error = %e, "client session ended"),
                    }
                }
                .instrument(span),
            );
        }
    }
}

/// 合成した仮想ディスプレイ (デスクトップ非アタッチで DXGI に出ないもの) に付ける id。
/// 実 DXGI 出力 index (0..) とは衝突しない番兵。テイクオーバー成立時に実 index へ差し替える。
const VIRTUAL_SENTINEL_ID: u32 = u32::MAX;
