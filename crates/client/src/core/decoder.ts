// WebCodecs VideoDecoder ラッパー (1 セッション = 1 インスタンス)。
// 複数タブ (セッション) が 1 枚の canvas を共有するため、描画先 canvas はインスタンス間で
// 同一でよいが、デコーダ状態 (configure / 参照フレーム) はセッションごとに独立させる。
// 非アクティブタブは Session 側で映像チャンクを渡さない (host も一時停止) ため、ここは
// 受け取ったチャンクを淡々とデコードして描画するだけでよい。
//
// 受信バイナリ形式 (net.rs udp_reader_loop):
//   [0]    flags (bit0=keyframe)
//   [1..3] frame_id (be u16)
//   [3..]  Annex-B H.264 (start code 00 00 00 01 区切りで複数 NALU を含む)

import { logger } from "./log";
import { isPerfEnabled } from "./ipc";

// --- ジッタバッファ (適応クッション) ---
// 旧実装はデコード完了フレームを onFrame で即 drawImage していた。WebCodecs の出力は
// ネットワーク到着ジッタ (パケットが束で来て→疎、を繰り返す) をそのまま引き継ぐため、
// 各フレームの表示時間が不均一になり「ヌルヌル動かずカクつく」体感の一因になっていた。
// クッションを持ち、到着間隔の推定 (EMA) で**均等間隔**に提示時刻をスケジュールして
// requestAnimationFrame で出すことで到着ジッタを吸収する。リフレッシュレート非依存。
//
// 旧実装の問題 (2026-06-27 計測で判明): クッションを固定 24ms にし、提示時刻を
// `nextPresentAt + 推定間隔` で**自走**させていたため、到着がバースト気味だと presentAt が
// 実時間より先行ドリフトし、上限 80ms に張り付いて hold が常時 ~56ms/p95 80ms まで肥大した
// (ジッタバッファ自身のバッファブロート)。本実装は (1) クッションを実測ジッタに比例させて
// 適応化し、(2) 提示時刻を「到着 + クッション」にアンカーしてドリフトをタイトにクランプする
// ことで、低ジッタ回線では hold をクッション (=数 ms) 付近まで縮める。

/// クッション = clamp(JITTER_K × 実測ジッタ, MIN, MAX)。ジッタ (到着間隔の平均絶対偏差) の
/// 何倍を先読みするか。大きいほど滑らかだが遅延が増える。
const JITTER_K = 2.5;
/// 適応クッションの下限 (ms)。低ジッタ回線 (Tailnet 直結) でもこの分は持ち、表示の取りこぼしを防ぐ。
/// 14ms ≈ 0.84 フレーム (@60Hz)。clean バーストで jitter が落ち cushion が 1 フレーム未満まで縮むと、
/// 数 ms 遅れた到着が rAF tick を 1 枚分逃して hitch になる。1 フレーム弱の余白を常に持たせ、遅れ
/// フレームでも提示 tick を逃さず提示尺を 1 vsync/フレームで安定させる (通常時は cushion>14 で非拘束=遅延無増)。
const MIN_CUSHION_MS = 14;
/// 適応クッションの上限 (ms)。ジッタが大きくてもこれ以上は先読みせず、操作遅延の暴走を防ぐ。
/// 40→30: spike 時の上振れを抑え、band を [MIN,MAX]=[14,30] に圧縮して cushion の whipsaw
/// (遅延と提示尺の揺れ) を steady にする。上振れ root は host encode p95 テールで本変更は client 側の
/// 対症療法。30 で underrun→drop が増えるなら 34 程度へ戻す。傾き (JITTER_K/EMA 係数) は触らない。
const MAX_CUSHION_MS = 30;
/// フレーム間隔の初期推定 (60fps)。実際の到着間隔の EMA で源 fps 変化に追従する。
const DEFAULT_INTERVAL_MS = 1000 / 60;
/// ジッタ推定の初期値 (ms)。実測の平均絶対偏差 EMA で更新する。
const DEFAULT_JITTER_MS = 4;
/// delay-line 提示バッファの「バースト吸収のりしろ」(フレーム数)。最大キュー深 =
/// round(cushion/間隔) + この値。デコード出力が束で出ても、ここまでは保持して 1 tick 1 枚ずつ
/// 提示し (落とさず後続 tick へ広げる)、これを超えた分だけ最古を間引いて遅延を有界化する。
/// 旧 self-clock + DRIFT_FRAMES 方式は自走クロックが ceiling に駐機し平時遅延が DRIFT に比例して
/// 増える欠点があった (2026-06-29 計測: DRIFT 1→3 で clean total 33→47ms・drop 15→7)。本方式は
/// cushion 待ち (eligibility) と「1 tick 1 枚」だけで提示し駐機しないので、バースト吸収 (落とさない)
/// と平時の低遅延を分離する。固着は「now が進めば必ず eligible + 上限で有界」なので構造的に無い。
const BURST_SLACK_FRAMES = 3;
/// 到着ジッタ EMA の非対称追従係数。RISE=上昇時は旧来どおり緩く (0.1) 追い、単発スパイクを
/// 追いかけて cushion を膨らませない (保護は対称 0.1 と同等)。FALL=下降時 (clean 復帰) だけ
/// 速くして (0.3) cushion=遅延を素早く回収する (2026-06-28 計測: RISE を速くする逆向きは悪化)。
const JITTER_EMA_RISE = 0.1;
const JITTER_EMA_FALL = 0.3;
/// eligibility 判定の早取り許容 (refresh の比率)。最古フレームの eligibility 時刻 (decodedAt+cushion)
/// が次 tick 直前に来たとき、その tick を取りこぼして 1 枚遅らせないよう、判定時刻 ts にこの幅だけ
/// 余裕を足す。refresh 未満なので cushion 待ちを丸々 1 tick 早めることはない。
const VSYNC_TOLERANCE_FRAC = 0.25;

/// 値を [lo, hi] に収める。
function clamp(x: number, lo: number, hi: number): number {
  return Math.min(Math.max(x, lo), hi);
}

type DecoderOpts = {
  /** デコード不能 (設定前のキーフレーム待ち / エラー) で復帰を促すための要求口。 */
  requestKeyframe: () => void;
};

/// ソート済み配列の p95 (切り上げ index、空なら 0)。
function p95(sorted: number[]): number {
  const n = sorted.length;
  if (n === 0) return 0;
  const idx = Math.min(Math.max(Math.ceil(n * 0.95) - 1, 0), n - 1);
  return sorted[idx];
}

/**
 * 受信→提示の遅延を 1 秒窓で集計してログに出す計測器 (AURORA_PERF 有効時のみ稼働)。
 * - decode_ms : チャンク受信 (VideoFrame.timestamp) → デコード出力 (onFrame)
 * - hold_ms   : デコード出力 → 実提示 (drawImage)。ジッタバッファ待ち + vsync 待ち。
 * - total_ms  : 受信 → 実提示 (= クライアント内部の合計遅延)。
 * - sched_ms  : デコード出力 → eligibility 時刻 (eligibleAt-decodedAt = cushion)。スケジューラが
 *               意図的に積んだジッタバッファ滞留。
 * - late_ms   : eligibility → 実提示 (now-eligibleAt)。eligibility 超過 = rAF/vsync 量子化 + backlog。
 *               **hold_ms = sched_ms + late_ms** に厳密分解されるので、残遅延の主因が
 *               「バッファ待ち (sched)」か「present tick 待ち (late)」かを切り分けられる。
 * これらと dropped/qdepth/present_fps を `vstat` 行として `client.<日付>.log` に残す。
 */
class VstatMeter {
  private windowStart = performance.now();
  private decodeMs: number[] = [];
  private holdMs: number[] = [];
  private totalMs: number[] = [];
  // hold_ms の内訳 (sched + late = hold)。残遅延がバッファ待ちか present tick 待ちかの切り分け用。
  private schedMs: number[] = [];
  private lateMs: number[] = [];
  private presented = 0;
  private dropped = 0;
  private lastQdepth = 0;
  // 適応スケジューラの現在値 (直近値を記録)。クッションが上限に張り付いていないか等の診断用。
  // jitter = 到着ジッタ (cushion の基準)、outputJitter = デコード出力カデンスのジッタ (比較用)。
  private lastCushionMs = 0;
  private lastJitterMs = 0;
  private lastOutputJitterMs = 0;

  recordDrop(n = 1): void {
    this.dropped += n;
  }

  recordPresent(
    decodeMs: number,
    holdMs: number,
    totalMs: number,
    schedMs: number,
    lateMs: number,
    qdepth: number,
    cushionMs: number,
    jitterMs: number,
    outputJitterMs: number,
  ): void {
    this.presented++;
    this.decodeMs.push(decodeMs);
    this.holdMs.push(holdMs);
    this.totalMs.push(totalMs);
    this.schedMs.push(schedMs);
    this.lateMs.push(lateMs);
    this.lastQdepth = qdepth;
    this.lastCushionMs = cushionMs;
    this.lastJitterMs = jitterMs;
    this.lastOutputJitterMs = outputJitterMs;
    this.maybeFlush();
  }

  /** 1 秒窓が満了したら集計を 1 行ログに出してリセットする。 */
  maybeFlush(): void {
    const now = performance.now();
    const elapsed = now - this.windowStart;
    if (elapsed < 1000 || (this.presented === 0 && this.dropped === 0)) return;

    const stat = (xs: number[]): string => {
      if (xs.length === 0) return "0.0/0.0";
      const avg = xs.reduce((a, b) => a + b, 0) / xs.length;
      const sorted = [...xs].sort((a, b) => a - b);
      return `${avg.toFixed(1)}/${p95(sorted).toFixed(1)}`;
    };
    const fps = Math.round((this.presented / elapsed) * 1000);
    logger.info(
      `vstat decode_ms=${stat(this.decodeMs)} hold_ms=${stat(this.holdMs)} ` +
        `total_ms=${stat(this.totalMs)} sched_ms=${stat(this.schedMs)} late_ms=${stat(this.lateMs)} ` +
        `qdepth=${this.lastQdepth} ` +
        `present_fps=${fps} dropped=${this.dropped} ` +
        `cushion_ms=${this.lastCushionMs.toFixed(1)} jitter_ms=${this.lastJitterMs.toFixed(1)} ` +
        `ojitter_ms=${this.lastOutputJitterMs.toFixed(1)} (avg/p95)`,
    );

    this.windowStart = now;
    this.decodeMs = [];
    this.holdMs = [];
    this.totalMs = [];
    this.schedMs = [];
    this.lateMs = [];
    this.presented = 0;
    this.dropped = 0;
  }
}

export class Decoder {
  private decoder: VideoDecoder | null = null;
  private configured = false;
  private readonly ctx: CanvasRenderingContext2D | null;
  // デコードエラーログのレート制限。壊れたストリームではエラーがフレーム毎に出て IPC 転送が
  // 連発しうるため、最大 1 秒 1 回に間引き、その間に潰した件数を併記する。
  private lastErrLogMs = 0;
  private suppressedErrs = 0;

  // --- ジッタバッファ状態 ---
  // 提示待ちのデコード済みフレーム (到着順 = 提示順)。提示予定時刻と、遅延計測用の
  // デコード出力時刻 (decodedAt) を持たせる。
  private queue: { frame: VideoFrame; decodedAt: number; eligibleAt: number }[] = [];
  // 受信→提示の遅延計測 (AURORA_PERF 有効時のみログ出力)。フラグは起動時に確定済み。
  private readonly perf = isPerfEnabled();
  private readonly vstat = new VstatMeter();
  // 提示ループの requestAnimationFrame ハンドル (キューが空の間は止める)。
  private rafId: number | null = null;
  // デコード出力間隔の指数移動平均 (提示の均等間隔スケジューリングに使う源 fps 推定)。
  private emaIntervalMs = DEFAULT_INTERVAL_MS;
  // デコード出力間隔の平均絶対偏差の EMA (出力カデンスのジッタ)。計測比較用 (cushion には使わない)。
  private emaJitterMs = DEFAULT_JITTER_MS;
  // 直近に算出した適応クッション (ms)。vstat への記録用 (提示時に参照する)。
  private lastCushionMs = 0;
  // 直近のデコード出力時刻 (出力カデンス EMA 更新用)。0 = 未到着。
  private lastArrivalMs = 0;

  // --- 到着 (チャンク受信) カデンス: 適応クッションの基準 ---
  // クッションは「伝送ジッタ」を吸収するためのもの。デコード出力カデンス (WebCodecs のバースト
  // を含み過大になりがち) ではなく、チャンク**到着**の間隔ジッタを基準にする。
  private lastChunkMs = 0;
  private emaArrivalIntervalMs = DEFAULT_INTERVAL_MS;
  private emaArrivalJitterMs = DEFAULT_JITTER_MS;

  // --- vsync 位相ロック (#3a) ---
  // rAF コールバックの ts から表示リフレッシュ間隔を実測し、提示予定を vsync グリッドへ整列させて
  // judder (提示尺の不均一) を消す。源 fps (emaIntervalMs) とは独立に持つ。
  // 実測リフレッシュ間隔 (ms)。連続 ts 差分の短窓 median。初期は 60Hz 仮定で実測まで暫定。
  private refreshMs = DEFAULT_INTERVAL_MS;
  // 直近の rAF ts (= 最後の vsync 時刻)。グリッドのアンカー兼 median の前回値。0 = 未取得。
  private lastVsyncTs = 0;
  // リフレッシュ実測の短窓サンプル (median 用、外れ値に強い)。
  private refreshSamples: number[] = [];

  /** デコードパイプラインのエラーを 1 秒 1 回に間引いて記録する (抑制件数を併記)。 */
  private throttledError(msg: string, ...extra: unknown[]): void {
    const now = performance.now();
    if (now - this.lastErrLogMs < 1000) {
      this.suppressedErrs++;
      return;
    }
    if (this.suppressedErrs > 0) {
      logger.error(`${msg} (+${this.suppressedErrs} suppressed in last 1s)`, ...extra);
      this.suppressedErrs = 0;
    } else {
      logger.error(msg, ...extra);
    }
    this.lastErrLogMs = now;
  }

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly opts: DecoderOpts,
  ) {
    this.ctx = canvas.getContext("2d");
    this.reset();
  }

  /** デコーダを作り直す (接続/再接続/タブ復帰時)。次のキーフレームから再構成する。 */
  reset(): void {
    this.flushQueue();
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* noop */
      }
    }
    this.decoder = new VideoDecoder({
      output: (frame) => this.onFrame(frame),
      error: (e) => {
        this.throttledError("VideoDecoder error", e);
        this.configured = false;
        this.opts.requestKeyframe();
      },
    });
    this.configured = false;
  }

  /** セッション破棄時に閉じる。 */
  close(): void {
    this.flushQueue();
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* noop */
      }
    }
    this.decoder = null;
    this.configured = false;
  }

  /** WebCodecs のデコード出力。即描画せず、decode 時刻と eligibility (cushion 待ち) を付けて積む。 */
  private onFrame(frame: VideoFrame): void {
    if (!this.canvas || !this.ctx) {
      frame.close();
      return;
    }
    const now = performance.now();

    // 到着間隔の EMA とジッタ (平均絶対偏差) の EMA を更新 (源 fps 変化に追従)。大きな gap
    // (ロス回復・再開直後) は外れ値として無視し、推定が暴れないようにする。偏差は更新前の
    // 推定間隔を基準に測る。
    if (this.lastArrivalMs > 0) {
      const dt = now - this.lastArrivalMs;
      if (dt > 0 && dt < 250) {
        const dev = Math.abs(dt - this.emaIntervalMs);
        this.emaIntervalMs += 0.1 * (dt - this.emaIntervalMs);
        this.emaJitterMs += 0.1 * (dev - this.emaJitterMs);
      }
    }
    this.lastArrivalMs = now;

    // 適応クッション: チャンク**到着**ジッタ (handleChunk で計測) に比例させる。低ジッタ回線では
    // MIN まで縮んで低遅延、ジッタが大きければ膨らんで滑らかさを保つ。デコード出力カデンスではなく
    // 到着カデンスを基準にするのは、クッションの役割が伝送ジッタの吸収だから。
    const cushion = clamp(JITTER_K * this.emaArrivalJitterMs, MIN_CUSHION_MS, MAX_CUSHION_MS);
    this.lastCushionMs = cushion;

    // delay-line 方式: 提示予定の自走クロックは持たず、frame に decode 時刻と eligibility 時刻
    // (= decodedAt + cushion) だけを付けて到着順に積む。提示は presentLoop が 1 tick 1 枚ずつ最古を
    // 出す。これでバースト (デコード出力の束) は後続 tick へ広がって落ちず、かつ平時は cushion 待ち
    // だけで提示されるので駐機遅延が出ない (旧 self-clock+ceiling の DRIFT 比例遅延を解消)。
    this.queue.push({ frame, decodedAt: now, eligibleAt: now + cushion });

    // バックログ上限: cushion 相当 (round(cushion/間隔)) + バースト吸収のりしろ。これを超えるのは
    // 本当に追いつけない時 (源 fps > 表示 / 持続オーバラン) なので最古を間引いて遅延を有界化する。
    const targetDepth = Math.max(1, Math.round(cushion / this.emaIntervalMs));
    const maxDepth = targetDepth + BURST_SLACK_FRAMES;
    while (this.queue.length > maxDepth) {
      this.queue.shift()?.frame.close();
      if (this.perf) this.vstat.recordDrop();
    }

    this.ensureRafRunning();
  }

  /** 提示ループが止まっていれば起動する。 */
  private ensureRafRunning(): void {
    if (this.rafId === null) {
      this.rafId = requestAnimationFrame(this.presentLoop);
    }
  }

  /**
   * rAF の ts から表示リフレッシュ間隔を実測する (連続 ts 差分の短窓 median)。median は外れ値
   * (タブ復帰直後の巨大 gap 等) に強い。lastVsyncTs はグリッドのアンカーも兼ねる。
   */
  private updateRefresh(ts: number): void {
    if (this.lastVsyncTs > 0) {
      const dt = ts - this.lastVsyncTs;
      // 妥当な vsync 間隔のみ採用 (rAF 停止明けの巨大 gap・ゼロ/負を除外)。
      if (dt > 1 && dt < 100) {
        this.refreshSamples.push(dt);
        if (this.refreshSamples.length > 31) this.refreshSamples.shift();
        const sorted = [...this.refreshSamples].sort((a, b) => a - b);
        this.refreshMs = sorted[sorted.length >> 1];
      }
    }
    this.lastVsyncTs = ts;
  }

  /**
   * requestAnimationFrame 駆動の提示ループ (delay-line)。各 vsync tick で、最古フレームが
   * eligibility (decode から cushion 経過) を満たしていれば **1 枚だけ** 提示する。束で来たフレームは
   * 後続 tick へ自然に広がり、落とさず全部出る (drop は onFrame の backlog 上限のみ=追いつけない時だけ)。
   * tick で出すので提示は常に vsync に整列し (late≈0)、自走クロックが無いので駐機遅延も出ない。
   */
  private presentLoop = (ts: DOMHighResTimeStamp): void => {
    // rAF の ts からリフレッシュを実測 (eligibility の早取り許容に使う)。
    this.updateRefresh(ts);
    // キューに何か残っていればループ継続。空なら止め、次の onFrame で再起動する。
    this.rafId = this.queue.length > 0 ? requestAnimationFrame(this.presentLoop) : null;
    if (this.queue.length === 0) return;

    // 最古がまだ cushion 待ち (eligibility) を満たさなければ出さない (ジッタ吸収)。eligibility 直後の
    // tick を取りこぼさないよう、ts に refresh の一部を足して 1 tick 分だけ早取りを許容する。
    const head = this.queue[0];
    if (ts + this.refreshMs * VSYNC_TOLERANCE_FRAC < head.eligibleAt) return;

    // eligible: この tick で最古を 1 枚だけ提示する (追いつき drop はしない=束は次 tick へ広げる)。
    const item = this.queue.shift();
    if (!item) return;
    if (this.perf) {
      // 実描画時刻は実時計で測る。arrival は handleChunk で chunk.timestamp(us) に刻んだ受信時刻。
      const now = performance.now();
      const arrivalMs = item.frame.timestamp / 1000;
      this.vstat.recordPresent(
        item.decodedAt - arrivalMs, // decode_ms
        now - item.decodedAt, // hold_ms (eligibility 待ち + tick 待ち)
        now - arrivalMs, // total_ms (受信→提示)
        item.eligibleAt - item.decodedAt, // sched_ms (= cushion 意図滞留)
        now - item.eligibleAt, // late_ms (eligibility 超過 = tick 量子化 + backlog)
        this.queue.length,
        this.lastCushionMs, // 適応クッション (診断用)
        this.emaArrivalJitterMs, // 到着ジッタ (cushion の基準)
        this.emaJitterMs, // デコード出力カデンスのジッタ (比較用)
      );
    }
    this.drawFrame(item.frame);
  };

  /** 1 フレームを canvas へ描画して閉じる。 */
  private drawFrame(frame: VideoFrame): void {
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!canvas || !ctx) {
      frame.close();
      return;
    }
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
      // 解像度確定/変化時のみ (毎フレームではない)。表示サイズの追跡用に info で残す。
      logger.info(`video size ${frame.displayWidth}x${frame.displayHeight}`);
    }
    ctx.drawImage(frame, 0, 0);
    frame.close();
  }

  /** 提示キューを空にし (保持フレームを閉じ)、ループを止めてスケジューラ状態を初期化する。 */
  private flushQueue(): void {
    if (this.rafId !== null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    for (const item of this.queue) item.frame.close();
    this.queue = [];
    this.emaIntervalMs = DEFAULT_INTERVAL_MS;
    this.emaJitterMs = DEFAULT_JITTER_MS;
    this.lastArrivalMs = 0;
    this.lastChunkMs = 0;
    this.emaArrivalIntervalMs = DEFAULT_INTERVAL_MS;
    this.emaArrivalJitterMs = DEFAULT_JITTER_MS;
    // vsync グリッドのアンカー/サンプルはループ停止で無効になるので破棄。refreshMs は同一ディスプレイ
    // 前提で最後の実測値を保持する (再接続直後の再計測トランジェントを避ける)。
    this.lastVsyncTs = 0;
    this.refreshSamples = [];
  }

  /** バイナリチャンク 1 個を受け取って decoder に渡す。 */
  handleChunk(buf: ArrayBuffer): void {
    if (buf.byteLength < 3) return;
    const u8 = new Uint8Array(buf);
    const keyframe = (u8[0] & 1) === 1;
    // u8[1..3] = frame_id (将来 RTT 算出用、いまは未使用)
    const annexB = u8.subarray(3);

    const decoder = this.decoder;
    if (!decoder) return;

    if (!this.configured) {
      if (!keyframe) {
        // 設定前のキーフレームを待つ。届かなければ要求。
        this.opts.requestKeyframe();
        return;
      }
      // SPS を探して codec 文字列を作る
      const nalus = splitNalus(annexB);
      let sps: Uint8Array | null = null;
      for (const nal of nalus) {
        const nalType = nal[0] & 0x1f;
        if (nalType === 7) {
          sps = nal;
          break;
        }
      }
      if (!sps) {
        this.opts.requestKeyframe();
        return;
      }
      const codec = codecStringFromSps(sps);
      if (!codec) {
        this.opts.requestKeyframe();
        return;
      }
      try {
        decoder.configure({ codec, optimizeForLatency: true });
        this.configured = true;
        // 構成は 1 セッション 1 回 (キーフレーム待ち成立時) のみ。低頻度なので info で残す。
        logger.info(`decoder configured: ${codec}`);
        // ジッタバッファの設定値を 1 度だけ出す。実機計測で「どの変種が走っているか」を
        // ログだけで確定できるようにする (定数を変えてビルドし忘れる事故の検出用)。
        logger.info(
          `jitterbuf cfg: MODE=playout MIN_CUSHION=${MIN_CUSHION_MS} MAX_CUSHION=${MAX_CUSHION_MS} ` +
            `BURST_SLACK=${BURST_SLACK_FRAMES} JITTER_K=${JITTER_K} TOL=${VSYNC_TOLERANCE_FRAC}`,
        );
      } catch (e) {
        this.throttledError("configure failed", codec, e);
        return;
      }
    }

    // チャンク到着カデンスのジッタを更新 (適応クッションの基準)。大きな gap (ロス回復・再開
    // 直後) は外れ値として無視する。偏差は更新前の推定間隔を基準に測る。
    const arrivalNow = performance.now();
    if (this.lastChunkMs > 0) {
      const dt = arrivalNow - this.lastChunkMs;
      if (dt > 0 && dt < 250) {
        const dev = Math.abs(dt - this.emaArrivalIntervalMs);
        this.emaArrivalIntervalMs += 0.1 * (dt - this.emaArrivalIntervalMs);
        // 非対称追従 (2b): 上昇 (RISE) は緩く追い単発スパイクで cushion を膨らませない (保護は
        // 対称 0.1 と同等)。下降 (clean 復帰) だけ速く縮めて (FALL) cushion=遅延を素早く回収する。
        const jAlpha = dev > this.emaArrivalJitterMs ? JITTER_EMA_RISE : JITTER_EMA_FALL;
        this.emaArrivalJitterMs += jAlpha * (dev - this.emaArrivalJitterMs);
      }
    }
    this.lastChunkMs = arrivalNow;

    try {
      const chunk = new EncodedVideoChunk({
        type: keyframe ? "key" : "delta",
        timestamp: arrivalNow * 1000,
        data: annexB,
      });
      decoder.decode(chunk);
    } catch (e) {
      this.throttledError("decode failed", e);
      this.configured = false;
      this.opts.requestKeyframe();
    }
  }
}

/** Annex-B から NALU 単位に分割。start code (3 or 4 byte) をスキップ。 */
function splitNalus(buf: Uint8Array): Uint8Array[] {
  const out: Uint8Array[] = [];
  const n = buf.length;
  let i = 0;
  let nalStart = -1;
  while (i < n) {
    // 0 0 0 1 もしくは 0 0 1 を探す
    if (i + 2 < n && buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 1) {
      if (nalStart >= 0) out.push(buf.subarray(nalStart, i));
      i += 3;
      nalStart = i;
    } else if (
      i + 3 < n &&
      buf[i] === 0 &&
      buf[i + 1] === 0 &&
      buf[i + 2] === 0 &&
      buf[i + 3] === 1
    ) {
      if (nalStart >= 0) out.push(buf.subarray(nalStart, i));
      i += 4;
      nalStart = i;
    } else {
      i += 1;
    }
  }
  if (nalStart >= 0 && nalStart < n) out.push(buf.subarray(nalStart, n));
  return out;
}

/** SPS の先頭 4 バイトから avc1.* codec 文字列を構築 (profile/constraint/level)。 */
function codecStringFromSps(sps: Uint8Array): string | null {
  // sps[0] = nal_unit_header, sps[1]=profile_idc, sps[2]=constraint_set_flags, sps[3]=level_idc
  if (sps.length < 4) return null;
  const profile = sps[1].toString(16).padStart(2, "0");
  const constraints = sps[2].toString(16).padStart(2, "0");
  const level = sps[3].toString(16).padStart(2, "0");
  // fourCC "avc1" は小文字必須 (Chromium の codec パーサは大小区別する)。
  // 16 進部分は大小どちらも可だが慣例の小文字に揃える。
  return `avc1.${profile}${constraints}${level}`.toLowerCase();
}
