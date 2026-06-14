//! TCP 制御 + UDP 映像のサーバ。単一クライアントモデル。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
use tokio::sync::mpsc;

use crate::audio::EncodedAudio;
use crate::capture::DisplayInfo;
use crate::encode::EncodedNal;
use crate::input::DisplayGeom;
use crate::projfs::{self, FsBridge, FsReply};

use crate::encode::{DEFAULT_BITRATE_BPS, MIN_BITRATE_BPS};

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

pub struct ServerConfig {
    /// 待受アドレス。既定は Tailscale IP (tailnet 限定公開)、`--bind` で上書き可能。
    /// Tailnet 外 (LAN/インターネット) への露出を絞り、総当たり・接続枠 DoS の面を減らす。
    pub bind_ip: std::net::IpAddr,
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
        if e.kind() != std::io::ErrorKind::AddrInUse || attempt >= ATTEMPTS {
            return Err(anyhow::Error::new(e).context(format!("bind {what} {addr}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    cfg: ServerConfig,
    mut nal_rx: mpsc::Receiver<EncodedNal>,
    mut audio_rx: mpsc::Receiver<EncodedAudio>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<u32>,
    fps_tx: std::sync::mpsc::Sender<u32>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    mut clip_change_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // 認証失敗スロットル (送信元 IP 単位の指数バックオフ)。accept ループ全体で 1 つ持ち、
    // セッションをまたいで失敗履歴を保つ。tray 表示用カウンタは cfg から共有する。
    let throttle = AuthThrottle::new(cfg.auth_failures.clone());

    // 外側ループ: 現在のポートで TCP/UDP を bind する。tray でポートが変わると reload 通知で
    // 内側の accept ループを抜け、ここへ戻って貼り直す (古いソケットは drop で解放)。
    loop {
        let port = cfg.port.load(Ordering::Relaxed);
        let addr = SocketAddr::new(cfg.bind_ip, port);
        let (tcp, udp) = bind_listeners(addr).await?;
        tracing::info!(tcp = %addr, udp = %addr, "host listening");

        // 内側ループ: 接続受理。reload が来たら break して外側で貼り直す。ポート変更は接続中
        // には tray 側で拒否されるため、reload はセッション間 (idle) でのみ届く。
        loop {
            let (stream, peer) = tokio::select! {
                accepted = tcp.accept() => accepted?,
                _ = cfg.reload.notified() => {
                    tracing::info!(new_port = cfg.port.load(Ordering::Relaxed), "port change requested; rebinding");
                    break;
                }
            };
            tracing::info!(peer = %peer, "TCP client connected");
            // 接続中であることを名前付きイベントで公開する。トレイ/CLI はこれを見て、接続中の
            // パスワード変更 (リモートからの締め出し/乗っ取り) を拒否する。セッション終了 (Drop)
            // で消える。単一クライアントモデルなのでガードは 1 つで足りる。
            let _active = crate::conn::ActiveGuard::new();
            // 接続ごとに現行 PSK のスナップショットを取る (tray でのパスワード変更を反映)。
            let psk = *cfg.psk.lock().expect("psk mutex poisoned");
            let displays = cfg.displays.clone();
            let audio = cfg.audio;
            let max_bitrate_bps = cfg.max_bitrate_bps;
            let udp = udp.clone();
            let enc_tx2 = enc_tx.clone();
            let input_tx2 = input_tx.clone();
            let cap_select_tx2 = cap_select_tx.clone();
            let fps_tx2 = fps_tx.clone();
            let clip_apply_tx2 = clip_apply_tx.clone();
            // nal_rx / audio_rx / clip_change_rx は単一所有のため、新規セッション開始時は既存
            // セッションを置き換える必要がある。MVP は単一クライアントのみ受けて完了まで占有する。
            let nal_rx_ref = &mut nal_rx;
            let audio_rx_ref = &mut audio_rx;
            let clip_change_rx_ref = &mut clip_change_rx;
            if let Err(e) = handle_client(
                stream,
                peer,
                psk,
                displays,
                audio,
                udp,
                enc_tx2,
                input_tx2,
                cap_select_tx2,
                fps_tx2,
                clip_apply_tx2,
                nal_rx_ref,
                audio_rx_ref,
                clip_change_rx_ref,
                max_bitrate_bps,
                &throttle,
                &cfg.devices,
            )
            .await
            {
                tracing::warn!(error = %e, "client session ended");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    psk: [u8; 32],
    displays: Vec<DisplayInfo>,
    audio: bool,
    udp: Arc<UdpSocket>,
    enc_tx: mpsc::Sender<EncoderCommand>,
    input_tx: mpsc::Sender<InputCmd>,
    cap_select_tx: std::sync::mpsc::Sender<u32>,
    fps_tx: std::sync::mpsc::Sender<u32>,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    nal_rx: &mut mpsc::Receiver<EncodedNal>,
    audio_rx: &mut mpsc::Receiver<EncodedAudio>,
    clip_change_rx: &mut mpsc::UnboundedReceiver<String>,
    max_bitrate_bps: u32,
    throttle: &AuthThrottle,
    devices: &Arc<crate::devices::DeviceGuard>,
) -> Result<()> {
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
    let (session, remote_key) = match async {
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
        // snow の StatelessTransportState は encrypt/decrypt とも &self なので Arc<Mutex> は
        // 不要 (映像 UDP のパケット毎ロックを排除)。
        let session = hs.into_transport().map_err(anyhow::Error::from)?;
        Ok::<_, anyhow::Error>((session, remote_key))
    }
    .await
    {
        Ok(pair) => pair,
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
    let admit = devices.admit(remote_key.as_deref());
    if let crate::devices::Admit::Rejected = &admit {
        let delay = throttle.record_failure(peer.ip());
        tracing::warn!(peer = %peer, delay_ms = delay.as_millis() as u64, "device not in allowlist; rejecting (device pinning)");
        tokio::time::sleep(delay).await;
        return Err(anyhow::anyhow!("device not in allowlist"));
    }
    // 認証成功 (PSK + デバイス)。この IP の連続失敗履歴を消す (誤入力後に正しく入れた正規
    // ユーザーを罰しない)。
    throttle.record_success(peer.ip());
    if let crate::devices::Admit::Enrolled = &admit {
        tracing::info!(peer = %peer, "device pinning: enrolled this device via pairing window");
    }
    tracing::info!(peer = %peer, "noise handshake complete");

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

    // HostInfo を即時送信 (全ディスプレイを提示)
    let host_info = ControlMsg::HostInfo {
        displays: displays.iter().map(Display::from).collect(),
        audio,
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

    let mut seq: u32 = 0;
    let bincode_opts = bincode::DefaultOptions::new().with_limit(64 * 1024);
    let mut upload: Option<UploadState> = None;
    let mut bitrate = BitrateController::new(max_bitrate_bps);
    // ダウンロード (ホスト → クライアント) のチャンクを select ループへ流す出口。
    // 送信タスクがここに積み、ループが TCP へ書く (stream への書き込みは単一箇所に集約)。
    let (out_tx, mut out_rx) = mpsc::channel::<ControlMsg>(64);

    // ProjFS ドライブリダイレクト。コールバック (同期) → ここへ FsList/FsRead 要求が流れる。
    // 開始失敗 (Client-ProjFS 無効など) はファイル投影を諦めてリモデス本体は継続する。
    // bounded: 書き戻し (FsWrite) の殺到で制御チャネルとメモリを溢れさせないようにする
    // (FsBridge が満杯時に blocking_send で ProjFS スレッドを待たせ、ディスク読みを止める)。
    let (fs_req_tx, mut fs_req_rx) = mpsc::channel::<ControlMsg>(64);
    let fs_bridge = Arc::new(FsBridge::new(fs_req_tx));
    // 投影ルートは CWD 相対ではなく固定の絶対パス (`%LOCALAPPDATA%\Aurora\remote`)。
    // セッション中だけ This PC にドライブとして現れ、切断 (Drop) で消える。
    let remote_root = projfs::remote_root();
    let _virt = match projfs::start(fs_bridge.clone(), &remote_root) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "ProjFS drive redirection disabled");
            None
        }
    };

    // セッション中に押下されたまま解放されていないキー/ボタンの追跡。切断時 (正常・異常
    // どちらでも) に解放イベントを注入し、ネットワーク断の瞬間に押していたキーがホスト側で
    // 押しっぱなしになる (stuck key) のを防ぐ。本体ループを async ブロックに包むのは、
    // ループ内の `?` による早期 return でもこの後始末を必ず通すため。
    let mut held = HeldInputs::default();
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
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::SetVideoParams { fps, max_bitrate_bps } => {
                        tracing::info!(fps, max_bitrate_bps, "client set video params");
                        // fps 上限を capture スレッドへ。
                        let _ = fps_tx.send(fps);
                        // ビットレート上限を更新し、新しい現在値を即エンコーダへ反映。
                        let new_bps = bitrate.set_max(max_bitrate_bps);
                        let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                    }
                    ControlMsg::Stats { bitrate_kbps, fps, rtt_us, loss_permille } => {
                        tracing::debug!(bitrate_kbps, fps, rtt_us, loss_permille, "client stats");
                        if let Some(new_bps) = bitrate.on_stats(loss_permille, rtt_us) {
                            tracing::info!(new_bps, loss_permille, rtt_us, "adapt bitrate");
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
                        match displays.iter().find(|d| d.id == id) {
                            Some(d) => {
                                tracing::info!(id, name = %d.name, "select display");
                                // capture スレッドへ切替指示。
                                let _ = cap_select_tx.send(id);
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
                            None => tracing::warn!(id, "SelectDisplay for unknown display id"),
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
            // ローカルクリップボード変化 → 制御チャネルで送信
            Some(text) = clip_change_rx.recv() => {
                send_control_msg(&mut write_half, &session, &ControlMsg::Clipboard { text }, &mut tcp_tx_nonce).await?;
            }
            // NAL 受信 → UDP 送信 (STREAM_VIDEO)
            Some(nal) = nal_rx.recv() => {
                let frame_id = nal.frame_id;
                let keyframe = nal.keyframe;
                let mut chunks = nal.data.chunks(MAX_PAYLOAD).peekable();
                let mut first = true;
                while let Some(chunk) = chunks.next() {
                    let is_last = chunks.peek().is_none();
                    let mut flags = VideoPacketFlags::empty();
                    if first { flags |= VideoPacketFlags::FRAME_START; }
                    if is_last { flags |= VideoPacketFlags::FRAME_END; }
                    if keyframe { flags |= VideoPacketFlags::KEYFRAME; }
                    let pkt = VideoPacket { seq, flags, frame_id, payload: chunk };
                    let mut framed = Vec::with_capacity(chunk.len() + 7);
                    pkt.encode(&mut framed).context("encode VideoPacket")?;
                    let nonce = NONCE_DOMAIN_VIDEO | seq as u64;
                    let dgram = encrypt_udp(&session, nonce, STREAM_VIDEO, &framed)?;
                    udp.send_to(&dgram, peer_udp).await.context("UDP send")?;
                    // 映像 nonce は seq (u32) を下位に使うため、seq 一周で nonce が再利用される。
                    // ChaChaPoly は nonce 再利用が機密性・完全性とも致命的なので、巻き戻る前に
                    // セッションを終了する。クライアントが再接続すると新しいハンドシェイクで
                    // 鍵が更新され nonce 空間がリセットされる (静かな nonce 再利用より安全)。
                    seq = match seq.checked_add(1) {
                        Some(s) => s,
                        None => {
                            return Err(anyhow!(
                                "video nonce space exhausted; disconnecting to force rekey"
                            ));
                        }
                    };
                    first = false;
                }
            }
            // 音声フレーム受信 → UDP 送信 (STREAM_AUDIO)
            Some(au) = audio_rx.recv() => {
                let pkt = AudioPacket { seq: au.seq, payload: &au.data };
                let mut framed = Vec::with_capacity(au.data.len() + 4);
                pkt.encode(&mut framed).context("encode AudioPacket")?;
                let nonce = NONCE_DOMAIN_AUDIO | au.seq as u64;
                let dgram = encrypt_udp(&session, nonce, STREAM_AUDIO, &framed)?;
                udp.send_to(&dgram, peer_udp).await.context("UDP send audio")?;
            }
        }
    }
    Ok(())
    }
    .await;

    // --- セッション後始末 (正常終了・エラーのどちらでも実行) ---
    // 1) stuck key 防止: 押下中のキー/マウスボタンへ解放イベントを注入する。
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
    // 2) ProjFS RPC を閉じる: 待機中のコールバックを即時エラーで起こし、以後の要求も
    //    即時失敗にする。`_virt` の Drop (PrjStopVirtualizing = コールバック完了待ち) が
    //    RPC_TIMEOUT まで引きずられ、次セッションの受理と Explorer が固まるのを防ぐ。
    fs_bridge.close();
    fs_req_rx.close();

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
