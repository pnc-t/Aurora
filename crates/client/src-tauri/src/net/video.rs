//! UDP 映像受信: パケット並べ替え/ロス耐性の再構成とジッタ計測。
use super::*;

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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn udp_reader_loop(
    udp: Arc<UdpSocket>,
    session: Arc<TransportSession>,
    sink: Sink,
    audio_tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    stats: Arc<StatsCollector>,
    keyframe_tx: mpsc::UnboundedSender<ControlMsg>,
    session_start: Instant,
    udp_health: Arc<UdpHealth>,
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

        // UDP 自己修復ウォッチドッグの基準時刻を更新 (映像/音声どちらの受信でもよい)。
        udp_health.mark_recv(session_start.elapsed().as_millis() as u64);

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
pub(crate) fn make_udp_socket(ipv6: bool) -> anyhow::Result<UdpSocket> {
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

#[cfg(test)]
mod tests {
    use super::*;
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
