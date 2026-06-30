//! TCP 制御 + UDP 映像のサーバ。単一クライアントモデル。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use aurora_common::framing::{
    AudioPacket, MAX_PAYLOAD, STREAM_AUDIO, STREAM_VIDEO, VideoPacket, VideoPacketFlags,
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

use crate::encode::{DEFAULT_BITRATE_BPS, MIN_BITRATE_BPS};

/// 接続ごとの相関 ID 採番カウンタ。ログの `conn` span に載せ、クライアント側の
/// session_id と peer/時刻で突き合わせて 1 接続を端から端まで追跡できるようにする。
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// 制御ループ → encode スレッドへの通知。
pub enum EncoderCommand {
    RequestKeyframe,
    /// 適応制御による平均ビットレート変更 (bps)。
    SetBitrate(u32),
}

/// 制御ループ → input スレッドへの通知。入力イベントとディスプレイ切替を 1 本にまとめる。
pub enum InputCmd {
    Event(InputEvent),
    /// キャプチャ対象ディスプレイ切替に伴う座標系更新。
    SetDisplay(DisplayGeom),
}

/// クライアント Stats を元にしたビットレート適応制御 (輻輳回避型)。
///
/// 単純な AIMD (+1Mbps/秒で上げ続け、ロスで ×0.7 急降下) は持続可能上限を毎回オーバーシュート
/// してクラッシュする「ノコギリ波」になり、体感が「軽い ↔ 重い+ブロックノイズ」で激しく振動する
/// (実測: 天井 ~20-25Mbps を +1Mbps/秒で突き抜けては急降下、15-20 秒周期)。
///
/// そこで直近で輻輳した rate を `ceiling_bps` (持続可能上限の推定) として記憶し、そこに**近づいたら
/// 増速を fast(+1Mbps) から slow(+250kbps) に落として張り付かせる** (TCP の輻輳回避相当)。これで
/// 持続可能なレートに安定収束し波が小さくなる。`ceiling_bps` は非輻輳時に緩やかに上方回復させ、
/// 条件が改善したら再び上を探る。
///
/// 制御は `MIN_BITRATE_BPS`..=`max_bps` にクランプする (上限は `--max-bitrate` で可変)。
struct BitrateController {
    current_bps: u32,
    /// 適応制御の上限 (bps)。`--max-bitrate` 由来。エンコーダのピーク上限と揃える。
    max_bps: u32,
    /// 持続可能上限の推定 (直近の輻輳点)。増速を緩める閾値に使う。
    ceiling_bps: u32,
    /// 直前サンプルが輻輳だったか。輻輳の立ち上がりでのみ ceiling を記録するため。
    in_congestion: bool,
}

impl BitrateController {
    fn new(max_bps: u32) -> Self {
        // 上限が下限 (MIN_BITRATE_BPS) を下回ると on_stats の clamp が min>max で
        // panic するため、下限以上へ持ち上げる (無制限 = u32::MAX はそのまま)。
        let max_bps = max_bps.max(MIN_BITRATE_BPS);
        Self {
            current_bps: DEFAULT_BITRATE_BPS.clamp(MIN_BITRATE_BPS, max_bps),
            max_bps,
            ceiling_bps: max_bps,
            in_congestion: false,
        }
    }

    /// クライアントからビットレート上限を明示変更する。上限を更新し、**現在値は据え置き**
    /// (新上限でクランプのみ)。旧実装は上限へ即ジャンプしていたが、それが接続直後に天井を
    /// 突き抜けてロス→急降下する初期の波の元だったため、下から `on_stats` で滑らかに上げる。
    /// 天井推定はリセットして新上限まで再探索させる。新しい現在値を返す。
    fn set_max(&mut self, max_bps: u32) -> u32 {
        self.max_bps = max_bps.max(MIN_BITRATE_BPS);
        self.current_bps = self.current_bps.clamp(MIN_BITRATE_BPS, self.max_bps);
        self.ceiling_bps = self.max_bps;
        self.in_congestion = false;
        self.current_bps
    }

    /// 新しい Stats を反映し、ビットレートを変更すべきなら新しい値 (bps) を返す。
    fn on_stats(&mut self, loss_permille: u16, rtt_us: u32) -> Option<u32> {
        const LOSS_HIGH: u16 = 50; // 5% 超で輻輳とみなす
        const LOSS_LOW: u16 = 10; // 1% 未満なら余裕あり
        const RTT_OK_US: u32 = 80_000; // 80ms 未満なら増速を許可
        const STEP_UP_FAST: u32 = 1_000_000; // ceiling から遠いとき
        const STEP_UP_SLOW: u32 = 250_000; // ceiling 付近 (輻輳回避でゆっくり張り付く)
        const DECREASE: f32 = 0.8;

        let congested = loss_permille > LOSS_HIGH;

        // 非輻輳時は ceiling をゆっくり上方回復させ、条件改善に追従する (低い値での固着防止)。
        if !congested {
            self.ceiling_bps = (self.ceiling_bps + self.ceiling_bps / 128).min(self.max_bps);
        }

        let next = if congested {
            // 輻輳の立ち上がりでのみ ceiling を記録する。持続中に連続記録すると、減速途中の
            // 低い rate を天井と誤認して過小評価してしまう。
            if !self.in_congestion {
                self.ceiling_bps = self.current_bps;
            }
            (self.current_bps as f32 * DECREASE) as u32
        } else if loss_permille < LOSS_LOW && rtt_us < RTT_OK_US {
            // ceiling に近いか遠いかで増速量を変える: 近ければ輻輳回避でゆっくり張り付く。
            let near = (self.current_bps as u64) * 100 >= (self.ceiling_bps as u64) * 85;
            let step = if near { STEP_UP_SLOW } else { STEP_UP_FAST };
            self.current_bps.saturating_add(step)
        } else {
            self.current_bps
        }
        .clamp(MIN_BITRATE_BPS, self.max_bps);

        self.in_congestion = congested;

        if next != self.current_bps {
            self.current_bps = next;
            Some(next)
        } else {
            None
        }
    }
}

/// 受信中のファイルアップロード状態 (クライアント → ホスト)。1 セッション 1 転送のみ。
struct UploadState {
    id: u32,
    file: tokio::fs::File,
    written: u64,
    total: u64,
    path: String,
}

/// アップロード受信ディレクトリ (`%USERPROFILE%\Aurora\received`)。
///
/// トレイ常駐 + ログオン時自動起動だと CWD が `System32` 等になり得るため、CWD 相対では
/// 保存先が予測不能になる。ユーザープロファイル配下の固定パスに絶対化して discoverable にする。
/// `USERPROFILE` (非 Windows では `HOME`) が無ければ最後の手段としてカレントディレクトリを使う。
fn upload_dir() -> std::path::PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("Aurora").join("received")
}

/// `%USERPROFILE%\Aurora\received` 配下に保存先ファイルを作る。
/// ファイル名はパス区切りを除去して安全化する。
async fn open_upload(name: &str) -> Result<(tokio::fs::File, String)> {
    let dir = upload_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .context("create aurora-received dir")?;
    // パス区切り・ADS (`:`) を中和し、ファイル名成分のみを採用する (client open_download と対称)。
    let safe = aurora_common::fsname::sanitize_filename(name, "upload.bin");
    let path = dir.join(&safe);
    let file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    Ok((file, path.display().to_string()))
}

/// 要求されたファイルを開き、FileOffer + FileChunk 列を `out_tx` 経由でクライアントへ送る。
/// 読み取りエラーは FileResult(ok=false) で通知する。成功完了は last=true のチャンクで示す
/// (受信側がローカル保存の成否を判断するため、成功時の FileResult はホストからは送らない)。
async fn stream_file_to_client(id: u32, path: &str, out_tx: mpsc::Sender<ControlMsg>) {
    const CHUNK: usize = 32 * 1024;

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            let _ = out_tx
                .send(ControlMsg::FileResult {
                    id,
                    ok: false,
                    message: format!("open failed: {e}"),
                })
                .await;
            return;
        }
    };
    // size=0 を誤って告知すると受信側が「告知サイズ超過」で初回チャンクから拒否して
    // しまうため、metadata 失敗は転送エラーとして通知する。
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = out_tx
                .send(ControlMsg::FileResult {
                    id,
                    ok: false,
                    message: format!("metadata failed: {e}"),
                })
                .await;
            return;
        }
    };
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin")
        .to_string();

    if out_tx
        .send(ControlMsg::FileOffer { id, name, size })
        .await
        .is_err()
    {
        return;
    }

    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                let _ = out_tx
                    .send(ControlMsg::FileResult {
                        id,
                        ok: false,
                        message: format!("read failed: {e}"),
                    })
                    .await;
                return;
            }
        };
        let last = n == 0 || offset + n as u64 >= size;
        if out_tx
            .send(ControlMsg::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
                last,
            })
            .await
            .is_err()
        {
            return;
        }
        offset += n as u64;
        if last {
            break;
        }
    }
    tracing::info!(id, size, "file download streamed");
}

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
    pub devices: Arc<crate::devices::DeviceGuard>,
    /// capture スレッドへ「現在の画面を 1 枚強制送出せよ」と促す共有フラグ。接続確立時と
    /// クライアントの RequestKeyframe で立て、capture が直近フレームを再投入する。画面静止中に
    /// 接続/再同期しても黒画面のまま固まらないようにするための仕組み (DXGI は変化フレームのみ返す)。
    pub force_frame: Arc<std::sync::atomic::AtomicBool>,
    /// net→tray の実行時ステータス/通知バス。接続確立/切断/ペアリング登録を記録し、tray が
    /// ツールチップ更新・バルーン通知に使う。
    pub tray_bus: Arc<crate::tray::TrayBus>,
}

/// 送信元 IP ごとの認証失敗を追跡し、連続失敗で接続応答を指数的に遅らせるスロットル。
///
/// 本アプリの認証はロックアウトの無い単一要素 (パスワード→PSK) なので、オンライン総当たりの
/// レートを落とすのが狙い。**IP 単位**である点が要: グローバルなロックアウトにすると、攻撃者が
/// 失敗を撃ち込むだけで正規ユーザー (別 IP) を締め出せる自爆 DoS になる。TCP ハンドシェイクを
/// 完了した相手の IP しか記録しないため送信元詐称も効かない。正規ユーザーの成功で即リセットする。
struct AuthThrottle {
    /// 送信元 IP → (連続失敗回数, 最終失敗時刻)。
    inner: std::sync::Mutex<HashMap<IpAddr, FailRecord>>,
    /// tray 表示用の累計失敗カウンタ。
    total: Arc<AtomicU32>,
}

struct FailRecord {
    fails: u32,
    last: Instant,
}

impl AuthThrottle {
    /// 連続失敗のリセット猶予 兼 古いエントリの掃除しきい値。最後の失敗からこれを超えたら
    /// 連続失敗を 0 に戻す (誤入力した正規ユーザーを長く罰しない)。
    const RESET_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
    /// 初回失敗の基準遅延。以降 2 倍ずつ伸ばす。
    const BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
    /// 遅延の上限。
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

    fn new(total: Arc<AtomicU32>) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            total,
        }
    }

    /// 失敗を記録し、この接続を閉じる前に挟むべき遅延を返す。
    fn record_failure(&self, ip: IpAddr) -> std::time::Duration {
        self.total.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let Ok(mut map) = self.inner.lock() else {
            return Self::BASE_DELAY;
        };
        // 古いエントリを掃除し、マップを実際の送信元 IP 数に閉じ込める (メモリ上限)。
        map.retain(|_, r| now.duration_since(r.last) < Self::RESET_AFTER);
        let rec = map.entry(ip).or_insert(FailRecord {
            fails: 0,
            last: now,
        });
        rec.fails = rec.fails.saturating_add(1);
        rec.last = now;
        // 指数バックオフ: BASE * 2^(fails-1) を MAX で頭打ち。
        let shift = (rec.fails - 1).min(5);
        Self::BASE_DELAY
            .checked_mul(1u32 << shift)
            .unwrap_or(Self::MAX_DELAY)
            .min(Self::MAX_DELAY)
    }

    /// 認証成功時に当該 IP の連続失敗を消す (正規ユーザーは次回も遅延ゼロ)。
    fn record_success(&self, ip: IpAddr) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&ip);
        }
    }
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
struct ClientArgs {
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
    devices: Arc<crate::devices::DeviceGuard>,
    shared: Arc<tokio::sync::Mutex<SharedControl>>,
    registry: Registry,
    conn_id: u64,
    /// 接続/キーフレーム要求時に capture へ現在の画面の強制送出を促す共有フラグ ([`ServerConfig`])。
    force_frame: Arc<std::sync::atomic::AtomicBool>,
    /// net→tray の実行時ステータス/通知バス ([`ServerConfig`])。
    tray_bus: Arc<crate::tray::TrayBus>,
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
    let vdd_gdi_name = crate::virtualdisplay::detect().map(|v| {
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
                tray_bus: cfg.tray_bus.clone(),
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

/// tray 表示 (ツールチップ/バルーン) 用の端末ラベルを作る。クライアント申告の端末名を
/// 制御文字除去・trim・長さ制限してそのまま使い、空なら peer IP へフォールバックする。
/// 申告名は任意値なので表示専用に整える (認証は公開鍵で行い、ここでは身元保証に使わない)。
fn display_label(device_name: &str, ip: IpAddr) -> String {
    let cleaned: String = device_name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(40)
        .collect();
    if cleaned.is_empty() {
        ip.to_string()
    } else {
        cleaned
    }
}

async fn handle_client(mut stream: TcpStream, args: ClientArgs) -> Result<()> {
    let ClientArgs {
        peer,
        psk,
        audio,
        udp,
        enc_tx,
        input_tx,
        cap_select_tx,
        fps_tx,
        scale_tx,
        cursor_tx,
        clip_apply_tx,
        nal_btx,
        audio_btx,
        throttle,
        devices,
        shared,
        registry,
        conn_id,
        force_frame,
        tray_bus,
    } = args;
    // 接続中であることを名前付きイベントで公開する。トレイ/CLI はこれを見て、接続中の
    // パスワード変更 (リモートからの締め出し/乗っ取り) を拒否する。セッション終了 (Drop) で
    // 当該ハンドルが閉じる。名前付きイベントは「ハンドルが 1 つでも開いていれば生存」する
    // ため、各接続が各々ガードを持つだけで「接続数 ≥ 1 の間 is_active=true」が成立する。
    let _active = crate::conn::ActiveGuard::new();

    // Noise XX-PSK レスポンダ。host 自身の静的鍵は使い捨て (None) でよい。デバイス識別は
    // 相手 (クライアント) の永続公開鍵 = remote_static で行う。
    let mut hs = HandshakeState::new_responder(&psk, None)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();

    // ハンドシェイクの各 I/O にタイムアウトを設ける。これが無いと、認証前のクライアントが
    // TCP 接続だけして何も送らないとここで永久にブロックし、単一クライアントモデルでは
    // accept ループが進まず正規ユーザーまで締め出される (認証前 DoS)。
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // ハンドシェイク 3 メッセージ + transport 確立を 1 ブロックにまとめ、失敗時に遅延を挟む。
    // 相手 (クライアント) の静的公開鍵は transport 確立で hs が消費される前に控える
    // (デバイスピン留めの許可リスト照合に使う)。
    let (session, remote_key, device_name) = match async {
        // <- e
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (e)")??;
        hs.read_msg(&buf, &mut tmp)?;
        // -> e, ee, s, es
        let mut out_msg = Vec::new();
        hs.write_msg(&[], &mut out_msg)?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, write_tcp_frame(&mut stream, &out_msg))
            .await
            .context("handshake timed out (e,ee,s,es)")??;
        // <- s, se, psk
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (s,se,psk)")??;
        hs.read_msg(&buf, &mut tmp)?;
        let remote_key = hs.remote_static();
        // 最終メッセージの (暗号化済み) payload にクライアントが載せた端末名。デバイスピン留めの
        // 登録ラベルに使い「どの PC を登録したか」を判別できるようにする (空 = 旧クライアントは
        // host 側で既定ラベル device-xxxx にフォールバック)。値はクライアント任意なので devices で
        // サニタイズ・長さ制限する。机上のラベル用途のみで認証には使わない。
        let device_name = String::from_utf8_lossy(&tmp).into_owned();
        // snow の StatelessTransportState は encrypt/decrypt とも &self なので Arc<Mutex> は
        // 不要 (映像 UDP のパケット毎ロックを排除)。
        let session = hs.into_transport().map_err(anyhow::Error::from)?;
        Ok::<_, anyhow::Error>((session, remote_key, device_name))
    }
    .await
    {
        Ok(triple) => triple,
        Err(e) => {
            // 認証/ハンドシェイク失敗。snow は PSK 不一致を最終メッセージの復号失敗として
            // 返すため、ここに来る = パスワード誤りかプロトコル不正。送信元 IP 単位の指数
            // バックオフ分だけ切断前に待たせ、ロックアウトの無い単一要素パスワードへの
            // オンライン総当たりレートを落とす (正規ユーザーの成功パスは遅延しない)。
            let delay = throttle.record_failure(peer.ip());
            tracing::warn!(peer = %peer, error = %e, delay_ms = delay.as_millis() as u64, "handshake failed; throttling source IP");
            tokio::time::sleep(delay).await;
            return Err(e);
        }
    };

    // デバイスピン留め (有効時のみ): PSK は正しくても、登録済み端末以外を拒否する 2 要素目。
    // PSK 漏洩でも「登録した端末の秘密鍵」を持たない相手を弾く。判定は record_success の前に
    // 行い、拒否はバックオフを伸ばす (= 履歴を消さない) ことで未許可端末の連投も減速させる。
    let admit = devices.admit(remote_key.as_deref(), Some(device_name.as_str()));
    if let crate::devices::Admit::Rejected = &admit {
        let delay = throttle.record_failure(peer.ip());
        tracing::warn!(peer = %peer, delay_ms = delay.as_millis() as u64, "device not in allowlist; rejecting (device pinning)");
        tokio::time::sleep(delay).await;
        return Err(anyhow::anyhow!("device not in allowlist"));
    }
    // 認証成功 (PSK + デバイス)。この IP の連続失敗履歴を消す (誤入力後に正しく入れた正規
    // ユーザーを罰しない)。
    throttle.record_success(peer.ip());
    // tray のツールチップ/バルーン通知に出す表示ラベル (端末名 or peer IP)。確立と切断で同じ
    // ラベルを使い、通知の対を揃える。
    let peer_label = display_label(&device_name, peer.ip());
    if let crate::devices::Admit::Enrolled = &admit {
        tracing::info!(peer = %peer, name = %device_name, "device pinning: enrolled this device via pairing window");
        tray_bus.on_enrolled(peer_label.clone());
    }
    tracing::info!(peer = %peer, "noise handshake complete");

    // 映像 UDP 送信を専用タスクへ分離するため session を Arc 共有する。StatelessTransportState は
    // encrypt/decrypt とも &self なので、制御ループと映像送信タスクが同じ session を並行参照できる
    // (nonce は呼び出し側が明示・ドメイン分離済みで衝突しない)。
    let session = std::sync::Arc::new(session);

    // TCP を読み書きの 2 ハーフに分割する。読み取りは専用タスクへ隔離し、本体の select
    // ループからは外す。理由: `read_tcp_frame` は `read_exact` を 2 回行う複数段の await で
    // **キャンセル安全ではない**。これを select の枝に直接置くと、NAL 送信や FS 要求など別の
    // 枝が ready になった瞬間に「長さは読んだが本体は途中」の future が drop され、TCP から
    // 消費済みのバイトが失われてフレーム境界がズレる (= decrypt error / read len で切断)。
    // 専用タスク内ならフレーム読み取りは決して途中キャンセルされない。読み取った生フレームは
    // bounded チャネルで本体ループへ渡し、select 側はキャンセル安全な `recv()` だけにする。
    let (mut read_half, mut write_half) = stream.into_split();
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    let reader_task = tokio::spawn(async move {
        let mut rbuf = Vec::new();
        loop {
            if read_tcp_frame(&mut read_half, &mut rbuf).await.is_err() {
                break; // EOF / 切断 / フレーム超過 → 読み取り終了 (frame_tx drop で本体へ伝播)
            }
            if frame_tx.send(std::mem::take(&mut rbuf)).await.is_err() {
                break; // 本体ループ終了
            }
        }
    });
    // 本体ループ終了時に読み取りタスクを確実に止める (セッション置換でのリーク防止)。
    let _reader_guard = AbortOnDrop(reader_task);

    // TCP 制御チャネルの nonce カウンタ (CTRL ドメイン)。送受で別カウンタを持ち、
    // TCP の順序保証により対向と歩調が合う。UDP メディアとは別ドメインなので衝突しない。
    let mut tcp_tx_nonce: u64 = 0;
    let mut tcp_rx_nonce: u64 = 0;

    // HostInfo を即時送信 (このクライアントへ全ディスプレイを提示)。トポロジは共有 (SharedControl)
    // で、capture が追従する単一の active_idx を全クライアントで共有する (共有ミラー)。
    //
    // **セキュリティゲート**: 仮想ディスプレイ (RDP 風テイクオーバー = 物理を暗転させる特権操作) は
    // **ホストに登録済み (許可リスト) の端末だけ**に許可する。未登録端末には HostInfo で仮想ディス
    // プレイ (合成エントリ) を一切見せず is_virtual も立てない (SelectDisplay/SetClientViewport の
    // テイクオーバー経路にも入れない)。接続レベルのピン留め (devices.admit) が無効でも独立に効く。
    let client_trusted = devices.is_trusted(remote_key.as_deref());
    let host_info = {
        let sc = shared.lock().await;
        let displays = sc
            .displays
            .iter()
            .filter_map(|d| {
                let is_virtual = sc.vdd_gdi_name.as_deref() == Some(d.name.as_str());
                // 未登録端末には仮想ディスプレイ (合成エントリ) を提示しない。
                if is_virtual && !client_trusted {
                    return None;
                }
                let mut pd = Display::from(d);
                pd.is_virtual = is_virtual;
                Some(pd)
            })
            .collect();
        ControlMsg::HostInfo { displays, audio }
    };
    send_control_msg(&mut write_half, &session, &host_info, &mut tcp_tx_nonce).await?;

    // UDP 対向アドレス確定待ち。クライアントは最初に 8B の "AURHELLO" を送る想定。
    // ハンドシェイクを完了した TCP ピアと同一 IP からの hello のみ受理する。これが無いと、
    // 任意の送信元が AURHELLO を送るだけで映像の宛先を奪える (正規クライアントを締め出す
    // ストリーム乗っ取り DoS)。さらに送信元 IP を詐称した単発 hello で、ホストが高ビットレート
    // 映像を被害者へ流し続けるリフレクション増幅にも悪用できる。bind が 0.0.0.0 で全 NIC
    // 公開のため Tailnet 外からも到達しうる。IP 一致の要求でこの両方を遮断する。
    let peer_udp = wait_udp_hello(&udp, peer.ip()).await?;
    tracing::info!(peer_udp = %peer_udp, "UDP peer registered");
    // ここまで来たら TCP 認証 + UDP 対向確定 = 接続確立。映像配信を開始する地点。
    tracing::info!("client connected");
    // tray へ確立を通知する (接続中バルーン + ツールチップ/メニューの「接続中」表示)。以降は
    // 早期 return せずセッション後始末を必ず通るため、ここの on_connected と後始末の
    // on_disconnected が必ず対になる (接続数のカウントがズレない)。
    tray_bus.on_connected(peer_label.clone());

    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let mut upload: Option<UploadState> = None;
    // ダウンロード (ホスト → クライアント) のチャンクと、host→クライアントへの制御 push
    // (DisplayResized / Clipboard 等、登録簿経由) を select ループへ流す出口。送信タスク/他接続が
    // ここに積み、ループが自分の Noise セッションで暗号化して TCP へ書く (書き込みは単一箇所に集約)。
    let (out_tx, mut out_rx) = mpsc::channel::<ControlMsg>(64);

    // 確立: 接続数を数え、登録簿へ自分の制御出口を登録する。ProjFS ドライブは固定パスのため
    // 最初に確立した 1 本だけが所有する (複数 mount は衝突する)。
    // さらに **ファイル共有ドライブは「ホストに登録済みの端末」だけに許可する** (仮想ディスプレイと
    // 同じ信頼ゲート client_trusted)。未登録端末は共有フォルダを設定していてもドライブを mount
    // しない。登録は devices のペアリング窓経由 (接続レベルのピン留め enforcement とは独立に効く)。
    let is_projfs_owner = {
        let mut sc = shared.lock().await;
        sc.active_conns += 1;
        if client_trusted && sc.projfs_owner.is_none() {
            sc.projfs_owner = Some(conn_id);
            true
        } else {
            false
        }
    };
    registry
        .lock()
        .expect("registry poisoned")
        .insert(conn_id, out_tx.clone());

    // ProjFS ドライブリダイレクト。コールバック (同期) → ここへ FsList/FsRead 要求が流れる。
    // 所有者でない接続は mount しない (要求も発生しない)。開始失敗 (Client-ProjFS 無効など) は
    // ファイル投影を諦めてリモデス本体は継続する。bounded: 書き戻し (FsWrite) の殺到で制御
    // チャネルとメモリを溢れさせないようにする (FsBridge が満杯時に blocking_send で ProjFS
    // スレッドを待たせ、ディスク読みを止める)。
    let (fs_req_tx, mut fs_req_rx) = mpsc::channel::<ControlMsg>(64);
    let fs_bridge = Arc::new(FsBridge::new(fs_req_tx));
    let _virt = if is_projfs_owner {
        // 投影ルートは CWD 相対ではなく固定の絶対パス (`%LOCALAPPDATA%\Aurora\remote`)。
        // セッション中だけ This PC にドライブとして現れ、切断 (Drop) で消える。
        let remote_root = projfs::remote_root();
        match projfs::start(fs_bridge.clone(), &remote_root) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "ProjFS drive redirection disabled");
                None
            }
        }
    } else if !client_trusted {
        tracing::info!("ProjFS drive redirection skipped (device not registered)");
        None
    } else {
        tracing::info!("ProjFS drive redirection skipped (another client owns it)");
        None
    };

    // 映像/音声 broadcast を subscribe する (確立後 = ハンドシェイク中のフレームを溜めない)。
    // nal_brx は下の専用映像送信タスクへ move する (そこで mut 受信)。
    let nal_brx = nal_btx.subscribe();
    let mut audio_brx = audio_btx.subscribe();

    // 接続直後に IDR を 1 枚要求し、最初のキーフレームを待つ黒画面時間を縮める。従来は
    // クライアントが delta を 1 枚受けてから RequestKeyframe を送り返す往復ぶん遅れていた。
    // 共有エンコーダだが、他クライアントには余分な keyframe が 1 枚増えるだけで無害。
    // あわせて capture に現在の画面の強制送出を促す。画面静止中は DXGI が新フレームを返さず
    // キーフレーム要求だけでは映像が生成されないため (接続直後ずっと黒くなる問題の本対策)。
    force_frame.store(true, std::sync::atomic::Ordering::Release);
    let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;

    // セッション中に押下されたまま解放されていないキー/ボタンの追跡。切断時 (正常・異常
    // どちらでも) に解放イベントを注入し、ネットワーク断の瞬間に押していたキーがホスト側で
    // 押しっぱなしになる (stuck key) のを防ぐ。本体ループを async ブロックに包むのは、
    // ループ内の `?` による早期 return でもこの後始末を必ず通すため。クライアント毎ローカル。
    let mut held = HeldInputs::default();
    // 音声ミュート状態 (クライアントの SetAudioMuted)。true の間は音声 UDP を送らない。
    // クライアント毎ローカル (各自が自分の送出のみ gate し、共有エンコーダは止めない)。
    let mut audio_muted = false;
    // 映像一時停止 (クライアントの SetVideoPaused)。true の間は映像 UDP を送らない
    // (マルチタブで非アクティブなタブの帯域を止める)。再開時はクライアントが RequestKeyframe を
    // 送って映像を復帰させる。これもクライアント毎ローカル。映像送信は専用タスクで走るため、
    // 制御ループ (ここ) とタスクで共有する atomic にする。
    let video_paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // --- 映像 UDP 送信タスク (制御/入力から分離) ---
    // 映像送信を制御ループの select から外し、専用タスクで回す。同じ select に置くと、ドラッグ中の
    // 大量の mousemove (TCP 制御、フレームの ~2 倍/秒) が `nal_brx.recv` と公平に競合し、フレーム送出
    // タイミングがジッタる (= クライアントの到着ジッタ増 → ジッタバッファ肥大 → 体感遅延)。別タスク化
    // するとマルチスレッドランタイムで送信が入力処理と並行に走り、送出が制御トラフィックから独立する。
    // session は &self (StatelessTransportState) なので Arc 共有で安全。seq は映像専用ローカル。
    let video_task = {
        let session = session.clone();
        let udp = udp.clone();
        let enc_tx = enc_tx.clone();
        let video_paused = video_paused.clone();
        let mut nal_brx = nal_brx;
        tokio::spawn(async move {
            let mut seq: u32 = 0;
            loop {
                let nal = match nal_brx.recv().await {
                    Ok(n) => n,
                    // 遅延でリングから溢れた: 取りこぼし、キーフレームで復帰させる (共有エンコーダ宛なので
                    // 全クライアントが次キーフレームを得る)。
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "video broadcast lagged; requesting keyframe");
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                        continue;
                    }
                    // 送信元 (encode → フォワーダ) 終了 = 映像供給停止。送信タスクを畳む。
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // 一時停止中はドレインするだけで送らない (非アクティブタブの帯域節約)。
                if video_paused.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let frame_id = nal.frame_id;
                let keyframe = nal.keyframe;
                let mut chunks = nal.data.chunks(MAX_PAYLOAD).peekable();
                let mut first = true;
                while let Some(chunk) = chunks.next() {
                    let is_last = chunks.peek().is_none();
                    let mut flags = VideoPacketFlags::empty();
                    if first {
                        flags |= VideoPacketFlags::FRAME_START;
                    }
                    if is_last {
                        flags |= VideoPacketFlags::FRAME_END;
                    }
                    if keyframe {
                        flags |= VideoPacketFlags::KEYFRAME;
                    }
                    let pkt = VideoPacket {
                        seq,
                        flags,
                        frame_id,
                        payload: chunk,
                    };
                    let mut framed = Vec::with_capacity(chunk.len() + 7);
                    if let Err(e) = pkt.encode(&mut framed) {
                        tracing::warn!(error = %e, "encode VideoPacket failed; ending video sender");
                        return;
                    }
                    let nonce = NONCE_DOMAIN_VIDEO | seq as u64;
                    let dgram = match encrypt_udp(&session, nonce, STREAM_VIDEO, &framed) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(error = %e, "encrypt video failed; ending video sender");
                            return;
                        }
                    };
                    if let Err(e) = udp.send_to(&dgram, peer_udp).await {
                        tracing::warn!(error = %e, "video UDP send failed; ending video sender");
                        return;
                    }
                    // 映像 nonce は seq (u32) を下位に使うため、一周で再利用される。ChaChaPoly は nonce
                    // 再利用が致命的なので巻き戻る前に送信を止める (クライアント再接続で鍵が更新される)。
                    seq = match seq.checked_add(1) {
                        Some(s) => s,
                        None => {
                            tracing::error!(
                                "video nonce space exhausted; ending video sender (reconnect to rekey)"
                            );
                            return;
                        }
                    };
                    first = false;
                }
            }
        })
    };
    // 本体終了 (切断・エラー) で映像送信タスクを確実に止める。
    let _video_guard = AbortOnDrop(video_task);

    let result: Result<()> = async {
    loop {
        tokio::select! {
            // 制御メッセージ受信 (専用リーダタスクが読んだ生フレームをキャンセル安全に受け取る)
            maybe_frame = frame_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    // リーダタスク終了 = 切断 / 読み取りエラー。セッションを正常終了する。
                    break;
                };
                let mut plain = Vec::new();
                let rx_nonce = NONCE_DOMAIN_CTRL | tcp_rx_nonce;
                tcp_rx_nonce += 1;
                session.decrypt(rx_nonce, &frame, &mut plain)?;
                let msg: ControlMsg = bincode_opts.deserialize(&plain)
                    .context("decode ControlMsg")?;
                match msg {
                    ControlMsg::Ping { ts_us } => {
                        send_control_msg(&mut write_half, &session, &ControlMsg::Pong { ts_us }, &mut tcp_tx_nonce).await?;
                    }
                    ControlMsg::RequestKeyframe => {
                        // 画面静止中の再同期でも現在の画面を 1 枚送れるよう capture を促す
                        // (UDP でキーフレームが欠けたまま静止すると、これが無いと回復しない)。
                        force_frame.store(true, std::sync::atomic::Ordering::Release);
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::SetVideoParams { fps, max_bitrate_bps } => {
                        tracing::info!(fps, max_bitrate_bps, "client set video params");
                        // fps 上限を capture スレッドへ (共有: last-writer-wins)。
                        let _ = fps_tx.send(fps);
                        // 共有エンコーダのビットレート上限を更新し、新しい現在値を即反映。
                        let new_bps = shared.lock().await.bitrate.set_max(max_bitrate_bps);
                        let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                    }
                    ControlMsg::SetAudioMuted { muted } => {
                        tracing::info!(muted, "client set audio muted");
                        audio_muted = muted;
                    }
                    ControlMsg::SetVideoPaused { paused } => {
                        tracing::info!(paused, "client set video paused");
                        video_paused.store(paused, std::sync::atomic::Ordering::Relaxed);
                    }
                    ControlMsg::SetCursorOverlay { enabled } => {
                        tracing::info!(enabled, "client set cursor overlay");
                        // 共有映像なので「1 つでも要求があれば合成」。集合を更新して capture へ。
                        let any = {
                            let mut sc = shared.lock().await;
                            if enabled {
                                sc.cursor_overlay.insert(conn_id);
                            } else {
                                sc.cursor_overlay.remove(&conn_id);
                            }
                            !sc.cursor_overlay.is_empty()
                        };
                        let _ = cursor_tx.send(any);
                    }
                    ControlMsg::SetResolution { max_height } => {
                        tracing::info!(max_height, "client set resolution");
                        // capture スレッドへ最大高さを伝える (GPU 縮小)。新キーフレームで即反映。
                        let _ = scale_tx.send(max_height);
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::Stats { bitrate_kbps, fps, rtt_us, loss_permille } => {
                        tracing::debug!(bitrate_kbps, fps, rtt_us, loss_permille, "client stats");
                        // 共有エンコーダは全クライアントの最悪リンクに合わせる。自分の stats を記録し、
                        // 全クライアントの max(loss)/max(rtt) を適応制御へ与える。
                        let new = {
                            let mut sc = shared.lock().await;
                            sc.client_stats.insert(conn_id, (loss_permille, rtt_us));
                            let (wl, wr) = sc.worst_link();
                            sc.bitrate.on_stats(wl, wr)
                        };
                        if let Some(new_bps) = new {
                            tracing::info!(new_bps, "adapt bitrate (worst-link)");
                            let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                        }
                    }
                    ControlMsg::Input(ev) => {
                        held.observe(&ev);
                        if input_tx.send(InputCmd::Event(ev)).await.is_err() {
                            tracing::warn!("input channel closed");
                        }
                    }
                    ControlMsg::SelectDisplay { id } => {
                        // 共有トポロジ (全クライアント共有)。last-writer-wins で active_idx を更新。
                        let mut sc = shared.lock().await;
                        match sc.displays.iter().position(|d| d.id == id) {
                            Some(idx) => {
                                if id == VIRTUAL_SENTINEL_ID && !client_trusted {
                                    // 未登録端末は仮想ディスプレイ (テイクオーバー) を選べない。
                                    tracing::warn!(id, "rejected virtual display select from untrusted client");
                                } else {
                                    // テイクオーバー中に物理ディスプレイへ切り替えるなら、先に
                                    // end_takeover で全トポロジを復元 (物理を再アタッチ) する。これを
                                    // 怠ると物理デタッチが残ったまま物理を選ぶことになり暗転が残る (穴③)。
                                    if sc.taken_over && id != VIRTUAL_SENTINEL_ID {
                                        let _ = tokio::task::spawn_blocking(
                                            crate::display::end_takeover,
                                        )
                                        .await;
                                        sc.taken_over = false;
                                        tracing::info!(
                                            "ended takeover: switching to physical display"
                                        );
                                    }
                                    // 物理ディスプレイの解像度は変更しないので、切替時の復元は不要。
                                    sc.active_idx = idx;
                                    let d = sc.displays[idx].clone();
                                    tracing::info!(id, name = %d.name, "select display");
                                    // 合成した仮想ディスプレイ (番兵 id) はまだ DXGI 出力が無い
                                    // (デタッチ中)。ここで cap_select すると EnumOutputs が外れて
                                    // 失敗するので送らない。直後の SetClientViewport で begin_takeover
                                    // がアタッチし、resolve_output が実 index で cap_select+座標系を
                                    // 張り替える。実 index を持つ通常ディスプレイのみ即切替する。
                                    if id != VIRTUAL_SENTINEL_ID {
                                        // capture スレッドへ切替指示 (GDI 名で。index はトポロジ変更で
                                        // 変わるため名前で引き直す)。
                                        let _ = cap_select_tx.send(d.name.clone());
                                        // input スレッドの座標系を新ディスプレイへ更新。
                                        let geom = DisplayGeom {
                                            x: d.x,
                                            y: d.y,
                                            width: d.width,
                                            height: d.height,
                                        };
                                        let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
                                        // 新キャプチャの最初の出力を keyframe にしてクライアント復帰を促す。
                                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                                    }
                                }
                            }
                            None => tracing::warn!(id, "SelectDisplay for unknown display id"),
                        }
                    }
                    ControlMsg::SetClientViewport { width, height } => {
                        // クライアントの表示領域にホスト解像度を合わせる。0 は無視。
                        // アクティブが仮想ディスプレイなら RDP 風テイクオーバー (物理オフ+仮想 primary)、
                        // そうでなければ物理ディスプレイの解像度変更 (方式B) を行う。
                        if width > 0 && height > 0 {
                            // 共有トポロジ (全クライアント共有)。ロックして適用する。
                            let mut sc = shared.lock().await;
                            let ai = sc.active_idx;
                            let active_name = sc.displays[ai].name.clone();
                            // 仮想ディスプレイのテイクオーバーは登録済み端末のみ (特権ゲート)。
                            let is_virtual = client_trusted
                                && sc.vdd_gdi_name.as_deref() == Some(active_name.as_str());
                            // 「リサイズで固まる」のライブ再現用 (RUST_LOG=aurora=debug)。どの viewport が
                            // いつ来てテイクオーバー/リサイズのどちらに入ったかを追える。
                            tracing::info!(
                                width,
                                height,
                                taken_over = sc.taken_over,
                                is_virtual,
                                active = %active_name,
                                "SetClientViewport"
                            );
                            if is_virtual {
                                // --- 仮想ディスプレイ: テイクオーバー ---
                                let applied = if !sc.taken_over {
                                    let vn = active_name.clone();
                                    let r = tokio::task::spawn_blocking(move || {
                                        crate::display::begin_takeover(&vn, width, height)
                                    })
                                    .await;
                                    match r {
                                        Ok(Ok((vx, vy, vw, vh))) => {
                                            sc.taken_over = true;
                                            // CCD で物理を落とすと仮想の GDI 名が変わることがあるため
                                            // MTT1337 で再検出し、capture 対象 (名前ベース) を更新する。
                                            let new_name = tokio::task::spawn_blocking(
                                                crate::virtualdisplay::detect,
                                            )
                                            .await
                                            .ok()
                                            .flatten()
                                            .map(|v| v.gdi_name)
                                            .unwrap_or_else(|| active_name.clone());
                                            sc.displays[ai].name = new_name.clone();
                                            // 仮想の**実位置**を入力座標系に使う。stage2 の (0,0) 移動が
                                            // 失敗すると仮想は右端 (例 5120,0) に残るため、決め打ち (0,0)
                                            // だとマウスがずれる (実機で確認)。begin_takeover が読み戻した
                                            // 実ジオメトリ (vx,vy,vw,vh) を信頼する。
                                            sc.displays[ai].x = vx;
                                            sc.displays[ai].y = vy;
                                            let _ = cap_select_tx.send(new_name);
                                            Some((vw, vh))
                                        }
                                        Ok(Err(e)) => {
                                            tracing::warn!(error = %e, "begin_takeover failed");
                                            None
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "begin_takeover task failed");
                                            None
                                        }
                                    }
                                } else {
                                    // テイクオーバー中のリサイズ追従 (仮想解像度の動的変更) は**行わない**。
                                    // アクティブな Desktop Duplication 中に仮想ディスプレイの解像度を
                                    // 変えると、duplication が回復不能の DXGI_ERROR_ACCESS_LOST に陥り
                                    // (同じ共有 D3D11 デバイス上では作り直しても復帰しない)、テイクオーバー中の
                                    // re-exec 抑止と相まって映像が永久固着する (実機ログ 2026-06-19 で確定)。
                                    // 解像度は接続時テイクオーバーで一度だけ確定し、以降の窓サイズ差は
                                    // クライアントの object-fit が吸収する。再フィットしたい場合は再接続する。
                                    tracing::debug!(
                                        width,
                                        height,
                                        "resize ignored during takeover (virtual resolution is fixed at connect to avoid capture ACCESS_LOST)"
                                    );
                                    None
                                };
                                if let Some((nw, nh)) = applied {
                                    sc.displays[ai].width = nw;
                                    sc.displays[ai].height = nh;
                                    let d = sc.displays[ai].clone();
                                    let geom = DisplayGeom {
                                        x: d.x,
                                        y: d.y,
                                        width: nw,
                                        height: nh,
                                    };
                                    let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
                                    // 解像度追従を全クライアントへ配信する (要求元以外の canvas も合わせる)。
                                    broadcast_ctrl(
                                        &registry,
                                        &ControlMsg::DisplayResized {
                                            id: d.id,
                                            width: nw,
                                            height: nh,
                                        },
                                    );
                                    let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                                    tracing::info!(id = d.id, nw, nh, "virtual display takeover/resize");
                                }
                            }
                            // 物理ディスプレイのキャプチャ時はホスト解像度を一切変更しない
                            // (はみ出しはクライアント側の object-fit / レターボックスで吸収する)。
                        }
                    }
                    ControlMsg::Clipboard { text } => {
                        if clip_apply_tx.send(text).is_err() {
                            tracing::warn!("clipboard apply channel closed");
                        }
                    }
                    ControlMsg::FileOffer { id, name, size } => {
                        match open_upload(&name).await {
                            Ok((file, path)) => {
                                tracing::info!(id, name = %name, size, path = %path, "file upload start");
                                upload = Some(UploadState { id, file, written: 0, total: size, path });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "open upload failed");
                                let msg = ControlMsg::FileResult { id, ok: false, message: e.to_string() };
                                send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                            }
                        }
                    }
                    ControlMsg::FileChunk { id, offset, data, last } => {
                        match upload.as_mut() {
                            Some(u) if u.id == id => {
                                if offset != u.written {
                                    // TCP は順序保証があるため offset 不一致は本来起きない。発生したら
                                    // データ欠落/重複の可能性が高く、書き込むと破損ファイルを残すため中断する。
                                    let message = format!("chunk offset mismatch: expected {}, got {offset}", u.written);
                                    tracing::warn!(id, expected = u.written, got = offset, "chunk offset mismatch, aborting upload");
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else if u.written.saturating_add(data.len() as u64) > u.total {
                                    // 告知サイズ (FileOffer.size) を超える書き込みは拒否する。これが無いと
                                    // size:0 を告知してから無限にチャンクを送り、ホストのディスクを埋められる
                                    // (total の検証が抜けており実質デッドだった)。
                                    let message = format!("upload exceeds declared size {}", u.total);
                                    tracing::warn!(id, total = u.total, written = u.written, got = data.len(), "upload exceeds declared size, aborting");
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else if let Err(e) = u.file.write_all(&data).await {
                                    let message = format!("write failed: {e}");
                                    tracing::warn!(id, error = %e, "file write failed");
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else {
                                    u.written += data.len() as u64;
                                    if last {
                                        let (written, total, path) = (u.written, u.total, u.path.clone());
                                        let res = u.file.flush().await;
                                        upload = None;
                                        let msg = match res {
                                            Ok(()) => {
                                                tracing::info!(id, written, total, path = %path, "file upload complete");
                                                ControlMsg::FileResult { id, ok: true, message: path }
                                            }
                                            Err(e) => ControlMsg::FileResult { id, ok: false, message: format!("flush failed: {e}") },
                                        };
                                        send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                    }
                                }
                            }
                            _ => {
                                tracing::warn!(id, "file chunk without matching offer, ignored");
                            }
                        }
                    }
                    ControlMsg::FileRequest { id, path } => {
                        tracing::info!(id, path = %path, "file download requested");
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            stream_file_to_client(id, &path, out_tx).await;
                        });
                    }
                    // ProjFS コールバックへの応答。相関 ID で待機中のコールバックを起こす。
                    ControlMsg::FsListReply { req_id, entries, done, error } => {
                        fs_bridge.complete(req_id, FsReply::List { entries, done, error });
                    }
                    ControlMsg::FsReadReply { req_id, data, error } => {
                        fs_bridge.complete(req_id, FsReply::Read { data, error });
                    }
                    other => {
                        tracing::debug!(?other, "received control msg (ignored in MVP)");
                    }
                }
            }
            // ダウンロード送信タスクからのメッセージ (FileOffer/FileChunk/FileResult)
            Some(msg) = out_rx.recv() => {
                send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
            }
            // ProjFS コールバックからの FS RPC 要求 (FsList/FsRead) を TCP へ送る
            Some(msg) = fs_req_rx.recv() => {
                send_control_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
            }
            // (映像 UDP 送信は専用タスク (上の video_task) へ分離済み。ここでは扱わない。)
            // 音声フレーム受信 (broadcast ファンアウト) → UDP 送信 (STREAM_AUDIO)
            au = audio_brx.recv() => {
                let au = match au {
                    Ok(a) => a,
                    // 音声は欠落しても次フレームで自然回復する。取りこぼしは debug ログのみ。
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "audio broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // ミュート中はチャネルをドレインするだけで送らない (帯域節約 + 詰まり防止)。
                // seq は送出時のみ進めればよいが、AudioPacket は au.seq をそのまま使い nonce も
                // au.seq 由来なので、送らない=その seq の nonce を消費しないだけで一貫性は保たれる。
                if !audio_muted {
                    let pkt = AudioPacket { seq: au.seq, payload: &au.data };
                    let mut framed = Vec::with_capacity(au.data.len() + 4);
                    pkt.encode(&mut framed).context("encode AudioPacket")?;
                    let nonce = NONCE_DOMAIN_AUDIO | au.seq as u64;
                    let dgram = encrypt_udp(&session, nonce, STREAM_AUDIO, &framed)?;
                    udp.send_to(&dgram, peer_udp).await.context("UDP send audio")?;
                }
            }
        }
    }
    Ok(())
    }
    .await;

    // --- セッション後始末 (正常終了・エラーのどちらでも実行) ---
    // 0) tray へ切断を通知する (切断バルーン + 接続数デクリメント)。確立時の on_connected と対。
    tray_bus.on_disconnected(peer_label);
    // 1) stuck key 防止: 押下中のキー/マウスボタンへ解放イベントを注入する (クライアント毎)。
    let releases = held.release_events();
    if !releases.is_empty() {
        tracing::info!(
            count = releases.len(),
            "releasing held inputs on disconnect"
        );
        for ev in releases {
            let _ = input_tx.send(InputCmd::Event(ev)).await;
        }
    }
    // 2) ProjFS RPC を閉じる (所有者でなければ no-op): 待機中のコールバックを即時エラーで起こし、
    //    以後の要求も即時失敗にする。`_virt` の Drop (PrjStopVirtualizing = コールバック完了待ち)
    //    が RPC_TIMEOUT まで引きずられ、次セッションの受理と Explorer が固まるのを防ぐ。
    fs_bridge.close();
    fs_req_rx.close();
    // 3) 登録解除 + 接続数減算 + ProjFS 所有解放。最後の 1 本かどうかとテイクオーバー復元の要否を
    //    ロック下で原子的に判定する (複数同時切断でも復元は 1 回だけ走る)。
    registry.lock().expect("registry poisoned").remove(&conn_id);
    let (is_last, restore_takeover) = {
        let mut sc = shared.lock().await;
        sc.active_conns = sc.active_conns.saturating_sub(1);
        sc.client_stats.remove(&conn_id);
        // カーソル合成要求を取り下げ、残りの要求有無を capture へ反映する (取りこぼし防止)。
        if sc.cursor_overlay.remove(&conn_id) {
            let _ = cursor_tx.send(!sc.cursor_overlay.is_empty());
        }
        if sc.projfs_owner == Some(conn_id) {
            sc.projfs_owner = None;
        }
        let is_last = sc.active_conns == 0;
        let restore = is_last && sc.taken_over;
        if restore {
            sc.taken_over = false;
        }
        (is_last, restore)
    };
    // 4) 最後の 1 本が切れたときだけグローバルな後始末を行う (他クライアントが視聴中なら維持)。
    //    テイクオーバー中なら全トポロジ (物理の再アタッチ含む) を復元する。物理解像度は変更しない
    //    方針なので、テイクオーバー以外の解像度復元は不要。
    if restore_takeover {
        let _ = tokio::task::spawn_blocking(crate::display::end_takeover).await;
    }
    // 5) VDD を再デタッチ: テイクオーバー復元後も VDD がデスクトップに残るため切り離す。
    //    未接続時にカーソルや Window が仮想ディスプレイへ迷い込む問題を防ぐ。最後の 1 本のみ。
    if is_last {
        if let Some(vdd) = crate::virtualdisplay::detect() {
            let vn = vdd.gdi_name;
            let _ = tokio::task::spawn_blocking(move || {
                crate::display::detach_vdd_if_attached(&vn);
            })
            .await;
        }
    }
    result
}

/// セッション中に押下されたまま解放されていない入力の追跡 (切断時の stuck key 防止)。
/// クライアントから届いた `InputEvent` を観察して押下状態を保持し、切断時に
/// [`HeldInputs::release_events`] で解放イベント列を合成する。OS 非依存のここで
/// 追跡することで、各 OS の injector に状態を持たせず host/client の対称性も保つ。
#[derive(Default)]
struct HeldInputs {
    keys: std::collections::HashSet<u32>,
    left: bool,
    right: bool,
    middle: bool,
}

impl HeldInputs {
    fn observe(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { vk, down } => {
                if down {
                    self.keys.insert(vk);
                } else {
                    self.keys.remove(&vk);
                }
            }
            InputEvent::MouseButton { button, down } => match button {
                MouseButton::Left => self.left = down,
                MouseButton::Right => self.right = down,
                MouseButton::Middle => self.middle = down,
            },
            _ => {}
        }
    }

    /// 押下中の全ボタン/キーに対する解放イベント列を返す。ボタンを先に上げる
    /// (ドラッグ中の切断で「修飾キーだけ先に上がる」中途半端な状態を避ける)。
    fn release_events(&self) -> Vec<InputEvent> {
        let mut evs = Vec::new();
        for (down, button) in [
            (self.left, MouseButton::Left),
            (self.right, MouseButton::Right),
            (self.middle, MouseButton::Middle),
        ] {
            if down {
                evs.push(InputEvent::MouseButton {
                    button,
                    down: false,
                });
            }
        }
        evs.extend(
            self.keys
                .iter()
                .map(|&vk| InputEvent::Key { vk, down: false }),
        );
        evs
    }
}

/// drop でラップした tokio タスクを abort するガード。handle_client 終了時に
/// 読み取りタスクを確実に停止させ、セッション置換でのリークを防ぐ。
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// UDP メディアデータグラムを構築する。
///
/// plaintext は `[stream tag(1B)] + framed` を `nonce` で暗号化し、受信側が復号前に
/// nonce を知れるよう **暗号文の前に nonce を 8B (BE) 平文で付与**する。
/// 出力: `[nonce(8B BE)] + [ciphertext]`。
fn encrypt_udp(
    session: &TransportSession,
    nonce: u64,
    stream: u8,
    framed: &[u8],
) -> Result<Vec<u8>> {
    let mut plain = Vec::with_capacity(1 + framed.len());
    plain.push(stream);
    plain.extend_from_slice(framed);
    let mut dgram = Vec::with_capacity(8 + plain.len() + 16);
    dgram.extend_from_slice(&nonce.to_be_bytes());
    session.encrypt(nonce, &plain, &mut dgram)?;
    Ok(dgram)
}

/// クライアントの UDP "AURHELLO" を待ち、対向アドレスを確定する。
///
/// hello は単発 UDP のためロスしうる。クライアントは映像受信が始まるまで再送するので、
/// ここでは各受信に上限を設けるだけにし、長時間 hello が来なければ Err を返して
/// セッションを諦める (無限ブロックで accept ループまで止めない安全弁)。
///
/// `expected_ip` はハンドシェイクを完了した TCP ピアの IP。これと一致する送信元からの
/// hello のみ受理し、別 IP からの hello はストリーム乗っ取り/増幅の試みとして捨てる。
/// Tailnet / LAN / localhost いずれも client の TCP と UDP は同一インターフェースから
/// 出るため送信元 IP は一致する (ポートは異なってよい)。
async fn wait_udp_hello(udp: &UdpSocket, expected_ip: std::net::IpAddr) -> Result<SocketAddr> {
    use std::time::Duration;
    let mut buf = [0u8; 64];
    loop {
        let (n, src) = tokio::time::timeout(Duration::from_secs(30), udp.recv_from(&mut buf))
            .await
            .context("timed out waiting for UDP hello")??;
        if &buf[..n] == b"AURHELLO" {
            if src.ip() == expected_ip {
                return Ok(src);
            }
            // TCP ピア以外からの hello は乗っ取り/増幅の疑い。受理せず待機を続ける。
            tracing::warn!(?src, expected = %expected_ip, "rejected UDP hello from unexpected source");
            continue;
        }
        tracing::debug!(?src, n, "unexpected UDP probe");
    }
}

async fn send_control_msg<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    session: &TransportSession,
    msg: &ControlMsg,
    tcp_tx_nonce: &mut u64,
) -> Result<()> {
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let plain = bincode_opts.serialize(msg).context("encode ControlMsg")?;
    let nonce = NONCE_DOMAIN_CTRL | *tcp_tx_nonce;
    *tcp_tx_nonce += 1;
    let mut cipher = Vec::new();
    session.encrypt(nonce, &plain, &mut cipher)?;
    write_tcp_frame(stream, &cipher).await?;
    Ok(())
}

async fn read_tcp_frame<R: AsyncReadExt + Unpin>(stream: &mut R, out: &mut Vec<u8>) -> Result<()> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await.context("read len")?;
    // 長さは u16 なので最大 65535。受信サイズはこれで自然に有界で、メッセージ本体の
    // 上限は復号後の bincode (with_limit 64KiB) が守る。
    let len = u16::from_be_bytes(len_buf) as usize;
    out.resize(len, 0);
    stream.read_exact(out).await.context("read frame body")?;
    Ok(())
}

async fn write_tcp_frame<W: AsyncWriteExt + Unpin>(stream: &mut W, data: &[u8]) -> Result<()> {
    if data.len() > u16::MAX as usize {
        return Err(anyhow!("frame too large: {}", data.len()));
    }
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await.context("write len")?;
    stream.write_all(data).await.context("write body")?;
    stream.flush().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ceiling から遠いときは fast(+1Mbps)、近い (>=85%) ときは slow(+250kbps) で張り付く。
    /// 単純 AIMD のノコギリ波 (毎回 +1Mbps で天井突き抜け) を防ぐ核心。
    #[test]
    fn ramps_fast_far_below_ceiling_then_crawls_near_it() {
        let mut bc = BitrateController::new(30_000_000);
        bc.ceiling_bps = 20_000_000;
        bc.current_bps = 10_000_000; // 天井の 50% → fast
        assert_eq!(bc.on_stats(0, 1000), Some(11_000_000));
        bc.current_bps = 18_000_000; // 天井の 90% → slow
        assert_eq!(bc.on_stats(0, 1000), Some(18_250_000));
    }

    /// 輻輳では ×0.8 減速し、ceiling は**立ち上がりで 1 度だけ**記録する (持続中の連続記録で
    /// 減速途中の低い rate を天井と誤認しない)。
    #[test]
    fn congestion_decreases_and_records_ceiling_once() {
        let mut bc = BitrateController::new(30_000_000);
        bc.current_bps = 25_000_000;
        bc.ceiling_bps = 30_000_000;
        // 立ち上がり: ceiling=25M を記録、25M→20M。
        assert_eq!(bc.on_stats(200, 1000), Some(20_000_000));
        assert_eq!(bc.ceiling_bps, 25_000_000);
        // 持続: ceiling 据え置き、20M→16M。
        assert_eq!(bc.on_stats(200, 1000), Some(16_000_000));
        assert_eq!(bc.ceiling_bps, 25_000_000);
    }
}
